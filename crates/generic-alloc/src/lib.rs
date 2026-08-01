//! Throwaway prototype for the *concrete-pointer* design decided in
//! `generic-allocator.md`, now folding in the "New idea after the feedback"
//! restructure. Validates that these compile and compose:
//!
//! - `Pointer<W = NonZeroU32>(W)` — the only type parameterized over the raw
//!   width integer `W`; owned handles parameterize over the *pointer type* `P`.
//! - `Allocator` is a *normal `&mut self`* mutating API and addresses are
//!   *core* to it (no `TransparentAllocator`); `resize` reports a relocation as
//!   `Option<(old_addr, new_addr)>`.
//! - `Backend` is *composed of* (not an extension of) an `Allocator`, split
//!   into `ReadBackend` (read = `&mut self`) and `WriteBackend` (write =
//!   `&self`), with a convenience `Backend<P>: ReadBackend<Pointer = P> +
//!   WriteBackend<Pointer = P>`.
//! - the `&self` write facade over a `&mut self` `Allocator` lives entirely in
//!   the backend adapter (a `RefCell` around the composed inner state); the
//!   read path uses `&mut self` and needs no `RefCell`, so it can hand out the
//!   real seekable cursor (`read_at -> impl Read + Seek`).
//! - `Persistable<P = Pointer>`: `store` takes `&impl WriteBackend`, `load`
//!   takes `&mut impl ReadBackend` — the natural asymmetry.
//! - the three implementor styles and the parameter-free default-`P` case.

use std::io::{self, Read, Seek, Write};
use std::marker::PhantomData;
use std::num::NonZeroU32;

#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::io::SeekFrom;

// ============================ pointer types ============================

/// A `Copy`, type- and size-erased identity: the serialized/at-rest form of a
/// pointer and the `anchor` of a [`Location`]. The *only* type parameterized
/// over the raw width integer `W` (default `NonZeroU32`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Pointer<W = NonZeroU32>(pub W);

/// Owned, single-owner handle to a resizable region. Parameterized over the
/// *pointer type* `P` (default `Pointer`), not over `W`.
#[derive(PartialEq, Eq, Debug)]
pub struct UniquePointerResizable<P = Pointer>(P);

/// Owned, single-owner handle to a fixed-size region.
#[derive(PartialEq, Eq, Debug)]
pub struct UniquePointerFixedSize<P = Pointer>(P);

/// The typed `Box<T>`: a fixed-size handle plus a phantom `T`.
pub struct UniquePointer<T, P = Pointer> {
    inner: UniquePointerFixedSize<P>,
    _marker: PhantomData<*const T>,
}

impl<P: Copy> UniquePointerResizable<P> {
    pub fn from_pointer(p: P) -> Self {
        Self(p)
    }
    /// Inherent `.raw()` — available because pointers are concrete.
    pub fn raw(&self) -> P {
        self.0
    }
}
impl<P: Copy> UniquePointerFixedSize<P> {
    pub fn from_pointer(p: P) -> Self {
        Self(p)
    }
    pub fn raw(&self) -> P {
        self.0
    }
}
impl<T, P: Copy> UniquePointer<T, P> {
    pub fn from_fixed(inner: UniquePointerFixedSize<P>) -> Self {
        Self {
            inner,
            _marker: PhantomData,
        }
    }
    pub fn into_fixed(self) -> UniquePointerFixedSize<P> {
        self.inner
    }
    pub fn raw(&self) -> P {
        self.inner.raw()
    }
}

// =============================== location ===============================

#[derive(Clone, Copy)]
pub struct Location<P = Pointer> {
    pub anchor: P,
    pub offset: u32,
}

// ============================== allocator ==============================

/// Pure address-range management: a *normal* `&mut self` mutating API, with no
/// knowledge of the bytes stored at those ranges. Addresses are **core** to the
/// `Allocator` (there is no separate `TransparentAllocator`); a `Backend` is
/// what *hides* them from user types. (Sizes are `usize` here for brevity; the
/// real trait is generic over `Address`/`Size` — see the `Word`-bound problem.)
pub trait Allocator {
    type Pointer: Copy;

    fn alloc_resizable(&mut self, size: usize) -> UniquePointerResizable<Self::Pointer>;
    fn alloc_fixed(&mut self, size: usize) -> UniquePointerFixedSize<Self::Pointer>;
    fn free_resizable(&mut self, p: UniquePointerResizable<Self::Pointer>);
    fn free_fixed(&mut self, p: UniquePointerFixedSize<Self::Pointer>);

    /// Resize a region. Returns `Some((old_addr, new_addr))` iff the bytes must
    /// move (so the enclosing `Backend` can copy them); `None` means no move —
    /// either an in-place resize or an unclaimed (address-less) reservation.
    fn resize(
        &mut self,
        p: &UniquePointerResizable<Self::Pointer>,
        new_size: usize,
    ) -> Option<(usize, usize)>;

    /// `None` if `p` was only reserved, never claimed.
    fn address(&self, p: Self::Pointer) -> Option<usize>;
    fn size(&self, p: Self::Pointer) -> Option<usize>;
}

// ============================ backend split ============================

/// Read access. `read` is `&mut self`: `load` is *sequential* (one field/element
/// at a time), so a `&mut` reborrowed down the recursion never needs two live
/// borrows — which means the read path can hand out the real seekable cursor
/// with no `RefCell` (see `UnjournaledBackend::read_at`). Querying allocator
/// state (`size`) stays available; only reads of *stored bytes* are the point.
pub trait ReadBackend {
    type Pointer: Copy;
    fn read(&mut self, anchor: Self::Pointer, offset: u32, len: u32) -> Vec<u8>;
    fn size(&self, p: Self::Pointer) -> Option<usize>;
}

/// Write access. `write`/`alloc_*`/`resize` are `&self`: the guard model hands
/// each nested field guard the *same* `&B` by reborrow, so mutation must go
/// through interior mutability inside the backend. Addresses never surface here
/// (`resize` returns nothing — contrast `Allocator::resize`).
pub trait WriteBackend {
    type Pointer: Copy;
    fn alloc_resizable(&self, size: usize) -> UniquePointerResizable<Self::Pointer>;
    fn alloc_fixed(&self, size: usize) -> UniquePointerFixedSize<Self::Pointer>;
    fn free_resizable(&self, p: UniquePointerResizable<Self::Pointer>);
    fn free_fixed(&self, p: UniquePointerFixedSize<Self::Pointer>);
    fn resize(&self, p: &UniquePointerResizable<Self::Pointer>, new_size: usize);
    fn write(&self, anchor: Self::Pointer, offset: u32, bytes: &[u8]);
}

/// Convenience name for "both halves, over the same pointer type". Carries `P`
/// so the two associated `Pointer`s are pinned equal; blanket-impl'd.
pub trait Backend<P>: ReadBackend<Pointer = P> + WriteBackend<Pointer = P> {}
impl<P, B: ReadBackend<Pointer = P> + WriteBackend<Pointer = P>> Backend<P> for B {}

// ============================== persistable ==============================

/// Parameterized over the *pointer type* `P` (default `Pointer`). `store` gets a
/// shared `&WriteBackend` (guard reborrow model); `load` gets an exclusive
/// `&mut ReadBackend` (sequential reads). Both pin the backend to `P`.
pub trait Persistable<P = Pointer>: Sized {
    const INLINE_SIZE: usize;
    fn store<B: WriteBackend<Pointer = P>>(&mut self, backend: &B, location: Location<P>);
    fn load<B: ReadBackend<Pointer = P>>(backend: &mut B, location: Location<P>) -> Self;
}

// Style (1) from the doc: a type that stores no pointers can be `Persistable`
// for *every* `P` -- works with any pointer width.
impl<P> Persistable<P> for i32 {
    const INLINE_SIZE: usize = 4;
    fn store<B: WriteBackend<Pointer = P>>(&mut self, backend: &B, location: Location<P>) {
        backend.write(location.anchor, location.offset, &self.to_le_bytes());
    }
    fn load<B: ReadBackend<Pointer = P>>(backend: &mut B, location: Location<P>) -> Self {
        let bytes = backend.read(location.anchor, location.offset, 4);
        i32::from_le_bytes(bytes.try_into().unwrap())
    }
}

// Style (2): a container that *does* store pointers is generic over `P`
// (default `Pointer`), and stores `P`-typed owned handles.
pub struct PersistableVec<T, P = Pointer> {
    data: Vec<T>,
    pointer: Option<UniquePointerResizable<P>>,
}

impl<T, P: Copy> PersistableVec<T, P> {
    pub fn new() -> Self {
        Self {
            data: Vec::new(),
            pointer: None,
        }
    }
    pub fn len(&self) -> usize {
        self.data.len()
    }
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
    pub fn push_in_memory(&mut self, value: T) {
        self.data.push(value);
    }
    pub fn get(&self, i: usize) -> Option<&T> {
        self.data.get(i)
    }
}

impl<T, P: Copy> Default for PersistableVec<T, P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Persistable<P>, P: Copy> Persistable<P> for PersistableVec<T, P> {
    // Just the pointer id -- the length/size is owned by the allocator (query
    // `size`), not stored inline. Empty is the null pointer, free via
    // `Option<P>`'s niche, so this is `size_of::<P>()` for a NonZero id.
    const INLINE_SIZE: usize = std::mem::size_of::<P>();

    fn store<B: WriteBackend<Pointer = P>>(&mut self, backend: &B, location: Location<P>) {
        let byte_size = self.data.len() * T::INLINE_SIZE;
        // `alloc_*`/`write` are `&self` -- nested field guards share this `&B`.
        let pointer = self
            .pointer
            .take()
            .unwrap_or_else(|| backend.alloc_resizable(byte_size));
        for (i, item) in self.data.iter_mut().enumerate() {
            item.store(
                backend,
                Location {
                    anchor: pointer.raw(),
                    offset: (i * T::INLINE_SIZE) as u32,
                },
            );
        }
        // inline header: just the target pointer id (INLINE_SIZE bytes), no len
        backend.write(
            location.anchor,
            location.offset,
            &vec![0u8; Self::INLINE_SIZE],
        );
        self.pointer = Some(pointer);
    }

    fn load<B: ReadBackend<Pointer = P>>(backend: &mut B, location: Location<P>) -> Self {
        // Read the inline pointer id (reconstruction of the owned handle + the
        // `len = size / elem_size` query elided). The loop shows that the
        // sequential `&mut` read path reborrows cleanly element by element.
        let _id_bytes = backend.read(location.anchor, location.offset, Self::INLINE_SIZE as u32);
        // (in reality `n = allocator.size(target) / T::INLINE_SIZE`, from the
        // reconstructed target pointer; here just derived from a live query so
        // the sequential `&mut` reborrow loop is genuinely type-checked)
        let n = backend.size(location.anchor).unwrap_or(0) / T::INLINE_SIZE.max(1);
        let mut data = Vec::new();
        for i in 0..n {
            let item = T::load(
                backend,
                Location {
                    anchor: location.anchor,
                    offset: (i * T::INLINE_SIZE) as u32,
                },
            );
            data.push(item);
        }
        Self {
            data,
            pointer: None,
        }
    }
}

// Style (3): a type that doesn't care about non-default widths implements
// `Persistable` only for the default `P = Pointer`, with no `P` generic.
#[allow(dead_code)] // compile-check only
struct DefaultOnly(i32);
impl Persistable for DefaultOnly {
    const INLINE_SIZE: usize = 4;
    fn store<B: WriteBackend<Pointer = Pointer>>(&mut self, backend: &B, location: Location) {
        self.0.store(backend, location);
    }
    fn load<B: ReadBackend<Pointer = Pointer>>(backend: &mut B, location: Location) -> Self {
        DefaultOnly(i32::load(backend, location))
    }
}

// ============================ storage (mock) ============================

/// Unstructured byte store: `Read + Write + Seek` plus resize/len. The concrete
/// backends compose one of these with an `Allocator`.
pub trait Storage: Read + Write + Seek {
    fn resize(&mut self, new_len: u64) -> io::Result<()>;
    fn len(&self) -> io::Result<u64>;
    fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }
}

// test-only, in-memory: a Cursor gives it the Seek that a bare Vec<u8> lacks.
#[cfg(test)]
#[derive(Default)]
struct MockStorage(std::io::Cursor<Vec<u8>>);
#[cfg(test)]
impl Read for MockStorage {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}
#[cfg(test)]
impl Write for MockStorage {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
#[cfg(test)]
impl Seek for MockStorage {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.seek(pos)
    }
}
#[cfg(test)]
impl Storage for MockStorage {
    fn resize(&mut self, new_len: u64) -> io::Result<()> {
        self.0.get_mut().resize(new_len as usize, 0);
        Ok(())
    }
    fn len(&self) -> io::Result<u64> {
        Ok(self.0.get_ref().len() as u64)
    }
}

// ========================= composed backend (mock) =========================

/// A concrete `&mut self` bump `Allocator` over an in-memory table.
#[cfg(test)]
#[derive(Default)]
struct MockAllocator {
    table: HashMap<NonZeroU32, Row>,
    next_id: u32,
    bump: usize,
}
#[cfg(test)]
#[derive(Clone, Copy)]
struct Row {
    address: usize,
    size: usize,
}
#[cfg(test)]
impl MockAllocator {
    fn fresh(&mut self, size: usize) -> Pointer {
        self.next_id += 1;
        let id = NonZeroU32::new(self.next_id).unwrap();
        let address = self.bump;
        self.bump += size;
        self.table.insert(id, Row { address, size });
        Pointer(id)
    }
}
#[cfg(test)]
impl Allocator for MockAllocator {
    type Pointer = Pointer;
    fn alloc_resizable(&mut self, size: usize) -> UniquePointerResizable<Pointer> {
        UniquePointerResizable::from_pointer(self.fresh(size))
    }
    fn alloc_fixed(&mut self, size: usize) -> UniquePointerFixedSize<Pointer> {
        UniquePointerFixedSize::from_pointer(self.fresh(size))
    }
    fn free_resizable(&mut self, p: UniquePointerResizable<Pointer>) {
        self.table.remove(&p.raw().0);
    }
    fn free_fixed(&mut self, p: UniquePointerFixedSize<Pointer>) {
        self.table.remove(&p.raw().0);
    }
    fn resize(
        &mut self,
        p: &UniquePointerResizable<Pointer>,
        new_size: usize,
    ) -> Option<(usize, usize)> {
        let row = *self.table.get(&p.raw().0)?;
        if new_size <= row.size {
            self.table.get_mut(&p.raw().0).unwrap().size = new_size;
            None // shrink in place -- no move
        } else {
            let new_addr = self.bump; // bump allocator can't grow in place
            self.bump += new_size;
            let e = self.table.get_mut(&p.raw().0).unwrap();
            e.address = new_addr;
            e.size = new_size;
            Some((row.address, new_addr))
        }
    }
    fn address(&self, p: Pointer) -> Option<usize> {
        self.table.get(&p.0).map(|r| r.address)
    }
    fn size(&self, p: Pointer) -> Option<usize> {
        self.table.get(&p.0).map(|r| r.size)
    }
}

/// Composition, not extension: holds a `Storage` and an `Allocator`. The
/// `RefCell` is the entire `&self`-write-facade cost, contained here and never
/// touching the reusable `Allocator`.
#[cfg(test)]
struct UnjournaledBackend<S, A> {
    inner: RefCell<(S, A)>,
}
#[cfg(test)]
impl<S: Storage, A: Allocator<Pointer = Pointer>> UnjournaledBackend<S, A> {
    fn new(storage: S, alloc: A) -> Self {
        Self {
            inner: RefCell::new((storage, alloc)),
        }
    }
    /// The read path is `&mut self`, so it can hand out the real seekable
    /// cursor (no `RefCell` juggling, no `Seek`-takes-`&mut` snag).
    fn read_at(&mut self, anchor: Pointer, offset: u32) -> &mut S {
        let (storage, alloc) = self.inner.get_mut();
        let addr = alloc.address(anchor).unwrap();
        storage
            .seek(SeekFrom::Start((addr + offset as usize) as u64))
            .unwrap();
        storage
    }
}
#[cfg(test)]
impl<S: Storage, A: Allocator<Pointer = Pointer>> WriteBackend for UnjournaledBackend<S, A> {
    type Pointer = Pointer;
    fn alloc_resizable(&self, size: usize) -> UniquePointerResizable<Pointer> {
        let mut g = self.inner.borrow_mut();
        let (storage, alloc) = &mut *g;
        let p = alloc.alloc_resizable(size);
        let end = (alloc.address(p.raw()).unwrap() + size) as u64;
        if storage.len().unwrap() < end {
            storage.resize(end).unwrap();
        }
        p
    }
    fn alloc_fixed(&self, size: usize) -> UniquePointerFixedSize<Pointer> {
        let mut g = self.inner.borrow_mut();
        let (storage, alloc) = &mut *g;
        let p = alloc.alloc_fixed(size);
        let end = (alloc.address(p.raw()).unwrap() + size) as u64;
        if storage.len().unwrap() < end {
            storage.resize(end).unwrap();
        }
        p
    }
    fn free_resizable(&self, p: UniquePointerResizable<Pointer>) {
        self.inner.borrow_mut().1.free_resizable(p);
    }
    fn free_fixed(&self, p: UniquePointerFixedSize<Pointer>) {
        self.inner.borrow_mut().1.free_fixed(p);
    }
    fn resize(&self, p: &UniquePointerResizable<Pointer>, new_size: usize) {
        let mut g = self.inner.borrow_mut();
        let (storage, alloc) = &mut *g;
        let old_size = alloc.size(p.raw()).unwrap_or(0);
        // Consume the allocator's address-level relocation report and translate
        // it into a `Storage` byte move; users of the backend never see addrs.
        if let Some((old, new)) = alloc.resize(p, new_size) {
            let end = (new + new_size) as u64;
            if storage.len().unwrap() < end {
                storage.resize(end).unwrap();
            }
            let mut buf = vec![0u8; old_size.min(new_size)];
            storage.seek(SeekFrom::Start(old as u64)).unwrap();
            storage.read_exact(&mut buf).unwrap();
            storage.seek(SeekFrom::Start(new as u64)).unwrap();
            storage.write_all(&buf).unwrap();
        }
    }
    fn write(&self, anchor: Pointer, offset: u32, bytes: &[u8]) {
        let mut g = self.inner.borrow_mut();
        let (storage, alloc) = &mut *g;
        let addr = alloc.address(anchor).unwrap();
        storage
            .seek(SeekFrom::Start((addr + offset as usize) as u64))
            .unwrap();
        storage.write_all(bytes).unwrap();
    }
}
#[cfg(test)]
impl<S: Storage, A: Allocator<Pointer = Pointer>> ReadBackend for UnjournaledBackend<S, A> {
    type Pointer = Pointer;
    fn read(&mut self, anchor: Pointer, offset: u32, len: u32) -> Vec<u8> {
        let (storage, alloc) = self.inner.get_mut();
        let addr = alloc.address(anchor).unwrap();
        storage
            .seek(SeekFrom::Start((addr + offset as usize) as u64))
            .unwrap();
        let mut buf = vec![0u8; len as usize];
        storage.read_exact(&mut buf).unwrap();
        buf
    }
    fn size(&self, p: Pointer) -> Option<usize> {
        self.inner.borrow().1.size(p)
    }
}

// ================================ tests ================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;

    fn backend() -> UnjournaledBackend<MockStorage, MockAllocator> {
        UnjournaledBackend::new(MockStorage::default(), MockAllocator::default())
    }

    #[test]
    fn default_pointer_common_case_is_parameter_free() {
        let backend = backend();
        let root = backend.alloc_fixed(PersistableVec::<i32>::INLINE_SIZE);

        // `PersistableVec::<i32>` — `P` defaults to `Pointer`; no width in sight.
        let mut v = PersistableVec::<i32>::new();
        v.push_in_memory(10);
        v.push_in_memory(20);
        v.store(
            &backend,
            Location {
                anchor: root.raw(),
                offset: 0,
            },
        );
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn store_via_shared_ref_then_load_via_mut_ref() {
        let mut backend = backend();
        let root = backend.alloc_fixed(4);

        // write path: `store` takes `&backend` (write = &self)
        let mut x = 7i32;
        x.store(
            &backend,
            Location {
                anchor: root.raw(),
                offset: 0,
            },
        );

        // read path: `load` takes `&mut backend` (read = &mut self)
        let y = i32::load(
            &mut backend,
            Location {
                anchor: root.raw(),
                offset: 0,
            },
        );
        assert_eq!(y, 7);
    }

    #[test]
    fn read_hands_out_a_real_seekable_cursor_via_mut_self() {
        let mut backend = backend();
        let root = backend.alloc_fixed(8);
        backend.write(root.raw(), 0, &[1, 2, 3, 4, 5, 6, 7, 8]);

        // `read_at` returns the real cursor because it borrows `&mut self`.
        let cursor = backend.read_at(root.raw(), 2);
        let mut buf = [0u8; 2];
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [3, 4]); // read from offset 2..4
                                 // and it genuinely seeks (the whole point of `read = &mut self`) --
                                 // rewind by 1 and re-read to prove `Seek` works on the handed-out cursor
        cursor.seek(SeekFrom::Current(-1)).unwrap();
        let mut one = [0u8; 1];
        cursor.read_exact(&mut one).unwrap();
        assert_eq!(one, [4]); // we're now at offset 3
    }

    #[test]
    fn resize_relocation_moves_bytes_in_storage() {
        let backend = backend();
        let p = backend.alloc_resizable(4);
        backend.write(p.raw(), 0, &[9, 8, 7, 6]);
        // grow -> the bump allocator relocates -> WriteBackend copies the bytes
        backend.resize(&p, 8);
        // read back through a fresh &mut borrow
        let mut backend = backend;
        let got = backend.read(p.raw(), 0, 4);
        assert_eq!(got, vec![9, 8, 7, 6]);
    }

    #[test]
    fn inline_size_is_just_the_pointer_width() {
        // The inline representation is just the pointer id -- the length/size
        // lives in the allocator, not inline. Default `Pointer<NonZeroU32>` is
        // 4 bytes; a wider `NonZeroU64` id is 8. (Per-`P` via `size_of::<P>()`.)
        assert_eq!(<PersistableVec<i32> as Persistable>::INLINE_SIZE, 4);
        assert_eq!(
            <PersistableVec<i32, Pointer<NonZeroU64>> as Persistable<Pointer<NonZeroU64>>>::INLINE_SIZE,
            8
        );
    }
}
