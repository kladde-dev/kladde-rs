//! [`PersistableVec`]: a deliberately "prematurely optimized" chunked vector, to
//! validate that the redesigned trait surface supports the layouts kladde may
//! eventually want. See `generic-allocator.md`, "Test case: chunked PersistableVec".
//!
//! ## On-disk layouts and how they're told apart
//!
//! The inline slot in the parent allocation holds just a nullable pointer id
//! (the head). **Which layout it points at is discovered by querying the
//! allocator's sizedness** -- no bit is stolen from the pointer:
//!
//! - **Empty** -> the inline pointer is null (`None`).
//! - **Small** (`len <= CHUNK_LEN`) -> the head is a **resizable** allocation
//!   holding exactly `len * elem` bytes of data, no inline length or capacity
//!   (the length is `alloc_size / elem`). Grows/shrinks by resizing.
//! - **Linked** (`len > CHUNK_LEN`) -> the head is a **fixed-size** allocation:
//!   the first chunk of a linked list. Chunks are held in memory as a `Vec` of
//!   pointers for random access.
//!
//! Chunk layout keeps the data **first** so converting between Small and Linked
//! never moves element bytes (only trailer bytes are added/removed):
//!
//! - First chunk: `[ data: CHUNK_LEN elems ][ vec_len: u32 ][ next: pointer ]`.
//! - Other chunks: `[ data: CHUNK_LEN elems ][ next: pointer ]`.
//!
//! Small -> Linked promotes the resizable head to a fixed first chunk via
//! `make_fixed_size` (data preserved); Linked -> Small demotes the first chunk
//! via `make_resizable` and frees the rest. Both exercise the backend's
//! `make_*`, and the "no data move" property the doc calls for.
//!
//! ## Scope / simplifications (see implementation-notes.md)
//!
//! - Elements are held in memory (`Vec<T>`); `store` re-lays the whole vector
//!   (reusing//resizing existing allocations rather than leaking them). Truly
//!   incremental guard-based mutation is future work.
//! - The "compact" mode (a resizable *last* chunk) is not built here; the
//!   fixed-chunk Linked mode plus Small mode already exercise the sizedness
//!   discrimination and no-move conversion the design turns on.
//! - `CHUNK_LEN` is a small constant (good for tests); a real vector would tune it.

use kladde_heap::{
    Pointer, ReadBackend, ResolvedPointer, UniquePointerFixedSize, UniquePointerResizable, Word,
    WriteBackend,
};
use std::io::Read;

use crate::location::Location;
use crate::persistable::Persistable;
use crate::repr::{decode_option_slice, encode_option, PointerRepr};

/// Elements per fixed-size chunk in Linked mode (and the Small/Linked threshold).
const CHUNK_LEN: usize = 4;
/// Bytes of the `vec_len` field stored in the first chunk.
const LEN_FIELD: usize = 4;

/// The current on-disk representation, tracked so `store` can transition between
/// layouts with minimal work.
enum Repr<P> {
    Empty,
    Small(UniquePointerResizable<P>),
    Linked(Vec<UniquePointerFixedSize<P>>),
}

/// A chunked, backend-persisted vector. Elements live in memory as a `Vec<T>`;
/// `store`/`load` project that to/from the chunked on-disk layout.
pub struct PersistableVec<T, P = Pointer> {
    data: Vec<T>,
    repr: Repr<P>,
}

/// Byte layout of the chunk trailers, derived from the element and pointer widths.
struct ChunkLayout {
    elem: usize,
    first_size: usize,
    other_size: usize,
    len_off: usize,
    first_next_off: usize,
    other_next_off: usize,
}

impl ChunkLayout {
    fn of<T: Persistable<P>, P: PointerRepr>() -> Self {
        let elem = T::INLINE_SIZE;
        let ptr_w = P::BYTE_LEN;
        let chunk_data = CHUNK_LEN * elem;
        ChunkLayout {
            elem,
            first_size: chunk_data + LEN_FIELD + ptr_w,
            other_size: chunk_data + ptr_w,
            len_off: chunk_data,
            first_next_off: chunk_data + LEN_FIELD,
            other_next_off: chunk_data,
        }
    }
    fn next_off(&self, chunk_index: usize) -> usize {
        if chunk_index == 0 {
            self.first_next_off
        } else {
            self.other_next_off
        }
    }
}

impl<T, P> PersistableVec<T, P> {
    pub fn new() -> Self {
        Self {
            data: Vec::new(),
            repr: Repr::Empty,
        }
    }

    /// Append an element in memory. It is written to the backend on the next
    /// [`store`](Persistable::store).
    pub fn push(&mut self, value: T) {
        self.data.push(value);
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
    pub fn get(&self, i: usize) -> Option<&T> {
        self.data.get(i)
    }
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.data.iter()
    }
}

impl<T, P> Default for PersistableVec<T, P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, P: PointerRepr> PersistableVec<T, P> {
    /// Free every backend allocation this vector owns (its handles don't free on
    /// drop -- freeing needs the backend). Leaves the vector empty in memory.
    pub fn free<B: WriteBackend<Pointer = P>>(&mut self, backend: &B) {
        match std::mem::replace(&mut self.repr, Repr::Empty) {
            Repr::Empty => {}
            Repr::Small(p) => backend.free_resizable(p),
            Repr::Linked(chunks) => {
                for c in chunks {
                    backend.free_fixed_size(c);
                }
            }
        }
        self.data.clear();
    }
}

impl<T: Persistable<P>, P: PointerRepr> PersistableVec<T, P> {
    /// Transition `self.repr` to fit `self.data.len()`, returning the head id (or
    /// `None` when empty). Reuses existing allocations where it can.
    fn sync<B: WriteBackend<Pointer = P>>(&mut self, backend: &B) -> Option<P> {
        let layout = ChunkLayout::of::<T, P>();
        let len = self.data.len();

        if len == 0 {
            self.free(backend);
            return None;
        }

        if len <= CHUNK_LEN {
            let head = self.ensure_small(backend, len * layout.elem);
            self.write_small(backend, head.raw(), &layout);
            let id = head.raw();
            self.repr = Repr::Small(head);
            Some(id)
        } else {
            let chunks = self.ensure_linked(backend, len, &layout);
            self.write_linked(backend, &chunks, len, &layout);
            let id = chunks[0].raw();
            self.repr = Repr::Linked(chunks);
            Some(id)
        }
    }

    fn ensure_small<B: WriteBackend<Pointer = P>>(
        &mut self,
        backend: &B,
        size_bytes: usize,
    ) -> UniquePointerResizable<P> {
        let size: B::Size = Word::from_usize(size_bytes);
        match std::mem::replace(&mut self.repr, Repr::Empty) {
            Repr::Empty => backend.alloc_resizable(size),
            Repr::Small(p) => {
                backend.resize(&p, size).expect("resize small allocation");
                p
            }
            Repr::Linked(mut chunks) => {
                // Demote the first chunk to resizable (head data preserved), drop rest.
                let first = chunks.remove(0);
                for c in chunks {
                    backend.free_fixed_size(c);
                }
                backend
                    .make_resizable(first, size)
                    .expect("demote first chunk to resizable")
            }
        }
    }

    fn ensure_linked<B: WriteBackend<Pointer = P>>(
        &mut self,
        backend: &B,
        len: usize,
        layout: &ChunkLayout,
    ) -> Vec<UniquePointerFixedSize<P>> {
        let needed = len.div_ceil(CHUNK_LEN);
        let first_size: B::Size = Word::from_usize(layout.first_size);
        let other_size: B::Size = Word::from_usize(layout.other_size);

        let mut chunks = match std::mem::replace(&mut self.repr, Repr::Empty) {
            Repr::Empty => Vec::with_capacity(needed),
            Repr::Small(p) => {
                // Promote the resizable head to a fixed first chunk (data preserved).
                vec![backend
                    .make_fixed_size(p, first_size)
                    .expect("promote small allocation to first chunk")]
            }
            Repr::Linked(chunks) => chunks,
        };

        // Grow or shrink the chunk list to `needed`.
        if chunks.is_empty() {
            chunks.push(backend.alloc_fixed_size(first_size));
        }
        while chunks.len() < needed {
            chunks.push(backend.alloc_fixed_size(other_size));
        }
        while chunks.len() > needed {
            let extra = chunks.pop().expect("more than `needed` chunks");
            backend.free_fixed_size(extra);
        }
        chunks
    }

    fn write_small<B: WriteBackend<Pointer = P>>(
        &mut self,
        backend: &B,
        head: P,
        layout: &ChunkLayout,
    ) {
        for (i, item) in self.data.iter_mut().enumerate() {
            item.store(
                backend,
                Location::new(head, Word::from_usize(i * layout.elem)),
            );
        }
    }

    fn write_linked<B: WriteBackend<Pointer = P>>(
        &mut self,
        backend: &B,
        chunks: &[UniquePointerFixedSize<P>],
        len: usize,
        layout: &ChunkLayout,
    ) {
        // Elements, each into its chunk at the within-chunk offset.
        for (gi, item) in self.data.iter_mut().enumerate() {
            let ci = gi / CHUNK_LEN;
            let within = gi % CHUNK_LEN;
            item.store(
                backend,
                Location::new(chunks[ci].raw(), Word::from_usize(within * layout.elem)),
            );
        }
        // `next` pointers linking the chunks (None terminates the last).
        for i in 0..chunks.len() {
            let next: Option<P> = chunks.get(i + 1).map(|c| c.raw());
            backend.write(
                chunks[i].raw(),
                Word::from_usize(layout.next_off(i)),
                encode_option(next).as_ref(),
            );
        }
        // Total length lives in the first chunk's trailer.
        backend.write(
            chunks[0].raw(),
            Word::from_usize(layout.len_off),
            &(len as u32).to_le_bytes(),
        );
    }
}

/// Read exactly `n` bytes at `anchor + offset` from a read backend.
fn read_bytes<P: PointerRepr, B: ReadBackend<Pointer = P>>(
    backend: &mut B,
    anchor: P,
    offset: usize,
    n: usize,
) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    let mut cursor = backend.read_at(anchor, Word::from_usize(offset));
    cursor.read_exact(&mut buf).expect("read chunk bytes");
    buf
}

impl<T: Persistable<P>, P: PointerRepr> Persistable<P> for PersistableVec<T, P> {
    // Just the head pointer id inline; the length/size lives with the allocator
    // (Small) or one indirection away (Linked).
    const INLINE_SIZE: usize = P::BYTE_LEN;

    fn store<B: WriteBackend<Pointer = P>>(&mut self, backend: &B, location: Location<P, B::Size>) {
        let head = self.sync(backend);
        backend.write(
            location.anchor,
            location.offset,
            encode_option(head).as_ref(),
        );
    }

    fn load<B: ReadBackend<Pointer = P>>(backend: &mut B, location: Location<P, B::Size>) -> Self {
        let layout = ChunkLayout::of::<T, P>();

        // Read the inline head pointer.
        let head_bytes = {
            let mut buf = vec![0u8; Self::INLINE_SIZE];
            let mut cursor = backend.read_at(location.anchor, location.offset);
            cursor
                .read_exact(&mut buf)
                .expect("read inline head pointer");
            buf
        };
        let head: Option<P> = decode_option_slice(&head_bytes);

        let Some(head) = head else {
            return Self::new();
        };

        match backend.resolve(head).expect("resolve head pointer") {
            ResolvedPointer::Resizable(handle) => {
                // Small mode: length is the allocation size / element size.
                let size = backend
                    .size(head)
                    .expect("size of small allocation")
                    .to_usize();
                let len = size / layout.elem;
                let mut data = Vec::with_capacity(len);
                for i in 0..len {
                    data.push(T::load(
                        backend,
                        Location::new(head, Word::from_usize(i * layout.elem)),
                    ));
                }
                Self {
                    data,
                    repr: Repr::Small(handle),
                }
            }
            ResolvedPointer::Fixed(first) => {
                // Linked mode: read the length, then walk the `next` chain.
                let len = {
                    let b = read_bytes(backend, head, layout.len_off, LEN_FIELD);
                    u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize
                };

                let mut chunks = vec![first];
                let mut cur = head;
                let mut index = 0usize;
                loop {
                    let nb = read_bytes(backend, cur, layout.next_off(index), P::BYTE_LEN);
                    match decode_option_slice::<P>(&nb) {
                        Some(next) => {
                            chunks.push(
                                backend
                                    .resolve_fixed_size(next)
                                    .expect("resolve linked chunk"),
                            );
                            cur = next;
                            index += 1;
                        }
                        None => break,
                    }
                }

                let mut data = Vec::with_capacity(len);
                for gi in 0..len {
                    let ci = gi / CHUNK_LEN;
                    let within = gi % CHUNK_LEN;
                    data.push(T::load(
                        backend,
                        Location::new(chunks[ci].raw(), Word::from_usize(within * layout.elem)),
                    ));
                }
                Self {
                    data,
                    repr: Repr::Linked(chunks),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kladde_heap::{Backend, MockBackend};

    fn as_vec(v: &PersistableVec<u32>) -> Vec<u32> {
        v.iter().copied().collect()
    }

    /// Store `v` at a fresh root, then load it back through the same backend.
    fn round_trip(b: &mut MockBackend, v: &mut PersistableVec<u32>) -> PersistableVec<u32> {
        let root = b.alloc_fixed_size(<PersistableVec<u32> as Persistable>::INLINE_SIZE as u32);
        v.store(b, Location::new(root.raw(), 0));
        PersistableVec::<u32>::load(b, Location::new(root.raw(), 0))
    }

    #[test]
    fn empty_vec_round_trips_as_a_null_pointer() {
        let mut b = MockBackend::new();
        let mut v = PersistableVec::<u32>::new();
        let loaded = round_trip(&mut b, &mut v);
        assert!(loaded.is_empty());
    }

    #[test]
    fn small_vec_round_trips() {
        let mut b = MockBackend::new();
        let mut v = PersistableVec::<u32>::new();
        for x in [10, 20, 30] {
            v.push(x);
        }
        let loaded = round_trip(&mut b, &mut v);
        assert_eq!(as_vec(&loaded), vec![10, 20, 30]);
    }

    #[test]
    fn large_vec_round_trips_through_the_linked_layout() {
        let mut b = MockBackend::new();
        let mut v = PersistableVec::<u32>::new();
        let expected: Vec<u32> = (0..10).collect(); // > CHUNK_LEN (4) -> linked, 3 chunks
        for &x in &expected {
            v.push(x);
        }
        let loaded = round_trip(&mut b, &mut v);
        assert_eq!(as_vec(&loaded), expected);
    }

    #[test]
    fn small_is_resizable_and_large_is_fixed_on_disk() {
        let mut b = MockBackend::new();

        let mut small = PersistableVec::<u32>::new();
        small.push(1);
        let root = b.alloc_fixed_size(4);
        small.store(&b, Location::new(root.raw(), 0));
        // head is resizable
        let head = decode_option_slice::<Pointer>(&{
            let mut buf = vec![0u8; 4];
            let mut c = b.read_at(root.raw(), 0);
            c.read_exact(&mut buf).unwrap();
            buf
        })
        .unwrap();
        assert!(matches!(
            b.resolve(head).unwrap(),
            ResolvedPointer::Resizable(_)
        ));
    }

    #[test]
    fn growing_across_the_threshold_then_reloading_preserves_data() {
        let mut b = MockBackend::new();
        let mut v = PersistableVec::<u32>::new();
        let root = b.alloc_fixed_size(4);

        // Start small (3 elems), store.
        for x in [1, 2, 3] {
            v.push(x);
        }
        v.store(&b, Location::new(root.raw(), 0));

        // Grow past the threshold (to 9 elems) and store again -> Small becomes
        // Linked in place (first chunk keeps its data via make_fixed_size).
        for x in [4, 5, 6, 7, 8, 9] {
            v.push(x);
        }
        v.store(&b, Location::new(root.raw(), 0));

        let loaded = PersistableVec::<u32>::load(&mut b, Location::new(root.raw(), 0));
        assert_eq!(as_vec(&loaded), (1..=9).collect::<Vec<_>>());
    }

    #[test]
    fn shrinking_back_below_the_threshold_preserves_data() {
        let mut b = MockBackend::new();
        let mut v = PersistableVec::<u32>::new();
        let root = b.alloc_fixed_size(4);

        for x in 0..8 {
            v.push(x);
        }
        v.store(&b, Location::new(root.raw(), 0)); // linked
        v.data.truncate(2); // now small-sized
        v.store(&b, Location::new(root.raw(), 0)); // Linked -> Small

        let loaded = PersistableVec::<u32>::load(&mut b, Location::new(root.raw(), 0));
        assert_eq!(as_vec(&loaded), vec![0, 1]);
        // head is resizable again
        let head = decode_option_slice::<Pointer>(&{
            let mut buf = vec![0u8; 4];
            let mut c = b.read_at(root.raw(), 0);
            c.read_exact(&mut buf).unwrap();
            buf
        })
        .unwrap();
        assert!(matches!(
            b.resolve(head).unwrap(),
            ResolvedPointer::Resizable(_)
        ));
    }

    #[test]
    fn store_reuses_allocations_rather_than_leaking() {
        let b = MockBackend::new();
        let mut v = PersistableVec::<u32>::new();
        let root = b.alloc_fixed_size(4);

        for x in [1, 2, 3] {
            v.push(x);
        }
        v.store(&b, Location::new(root.raw(), 0));
        let after_first = b.live_count();
        // storing again with the same small size must not allocate anew
        v.store(&b, Location::new(root.raw(), 0));
        assert_eq!(b.live_count(), after_first);

        v.free(&b);
    }
}
