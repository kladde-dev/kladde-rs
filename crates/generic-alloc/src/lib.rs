//! Throwaway prototype for the `generic-allocator.md` redesign.
//!
//! Goal: check that the proposed trait hierarchy actually *composes and
//! compiles* — the generic integer associated types with arithmetic, the
//! associated pointer types, the reserve/claim/convert/lookup surface, the
//! `Storage`/`WriteBackend`/`Backend` layering, and the RPITIT `-> impl
//! Write`/`-> impl Read + Seek` returns. It is not a working allocator.
//!
//! Findings are written back into `generic-allocator.md` (§ "Problems and
//! regressions"). Nothing here is meant to be kept.

use std::collections::HashMap;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::num::NonZeroU32;

// ---------------------------------------------------------------------------
// A minimal "unsigned word" bound so the allocator can do arithmetic over its
// generic `Address`/`Size` associated types. Rust std has no such trait, so
// *some* helper like this is unavoidable once those become type parameters.
// ---------------------------------------------------------------------------

pub trait Word: Copy + Ord + std::fmt::Debug {
    const ZERO: Self;
    fn from_usize(n: usize) -> Self;
    fn to_usize(self) -> usize;
    fn checked_add(self, rhs: Self) -> Option<Self>;
}

macro_rules! impl_word {
    ($($t:ty),*) => {$(
        impl Word for $t {
            const ZERO: Self = 0;
            fn from_usize(n: usize) -> Self { n as $t }
            fn to_usize(self) -> usize { self as usize }
            fn checked_add(self, rhs: Self) -> Option<Self> { <$t>::checked_add(self, rhs) }
        }
    )*};
}
impl_word!(u32, u64);

/// Whether an allocation may be resized.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sizedness {
    Fixed,
    Resizable,
}

/// Everything the allocator knows about a single allocation — *no address*,
/// which only `TransparentAllocator` exposes.
#[derive(Clone, Debug)]
pub struct Allocation<A: Allocator + ?Sized> {
    pub id: A::Id,
    pub size: A::Size,
    pub sizedness: Sizedness,
    pub meta: A::Meta,
}

// ---------------------------------------------------------------------------
// Layer 0: Allocator (no memory access, no addresses exposed).
// ---------------------------------------------------------------------------

pub trait Allocator {
    /// Stable, serializable identity of an allocation (`Index` in
    /// allocator-spec.md). Exposed to user types.
    type Id: Copy + Eq + std::hash::Hash;
    /// Internal memory address — never exposed above `TransparentAllocator`.
    type Address: Word;
    /// Allocation size, exposed to user types.
    type Size: Word + Into<Self::Address>;
    /// Per-allocation metadata kept in the allocator's own table.
    type Meta: Default + Clone;

    /// Owned handle to a resizable region. Impl-chosen representation.
    type ResizablePointer;
    /// Owned handle to a fixed-size region.
    type FixedPointer;
    /// `Copy` identity handle: no size, no sizedness.
    type RawPointer: Copy;

    // --- lifecycle (no read/write) ---
    fn alloc_resizable(&self, size: Self::Size) -> Self::ResizablePointer;
    fn alloc_fixed(&self, size: Self::Size) -> Self::FixedPointer;
    fn free_resizable(&self, p: Self::ResizablePointer);
    fn free_fixed(&self, p: Self::FixedPointer);
    fn resize(&self, p: &Self::ResizablePointer, new_size: Self::Size);

    // --- reserve an id now, assign an address later (for journaling) ---
    fn reserve_resizable(&self, size: Self::Size) -> Self::ResizablePointer {
        self.alloc_resizable(size)
    }
    fn reserve_fixed(&self, size: Self::Size) -> Self::FixedPointer {
        self.alloc_fixed(size)
    }
    fn claim_resizable(&self, _p: &Self::ResizablePointer) {}
    fn claim_fixed(&self, _p: &Self::FixedPointer) {}

    // --- convert between kinds; may mint a new id, must not move memory ---
    fn make_resizable(&self, p: Self::FixedPointer) -> Self::ResizablePointer;
    fn make_fixed(&self, p: Self::ResizablePointer) -> Self::FixedPointer;

    // --- erase to a Copy identity / recover the serializable id ---
    fn raw_resizable(&self, p: &Self::ResizablePointer) -> Self::RawPointer;
    fn raw_fixed(&self, p: &Self::FixedPointer) -> Self::RawPointer;
    fn id(&self, raw: Self::RawPointer) -> Self::Id;

    // --- reconstruct the single owner from a just-deserialized id, learning
    //     its sizedness in the process (used on `load`). ---
    fn resolve_owned(&self, id: Self::Id) -> Option<Owned<Self>>;

    // --- query the table ---
    fn lookup(&self, raw: Self::RawPointer) -> Option<Allocation<Self>>;
    fn size(&self, raw: Self::RawPointer) -> Option<Self::Size> {
        self.lookup(raw).map(|a| a.size)
    }
    fn meta(&self, raw: Self::RawPointer) -> Option<Self::Meta> {
        self.lookup(raw).map(|a| a.meta)
    }
}

/// A single-owner handle recovered from a persisted id, of the kind the
/// allocation actually has.
pub enum Owned<A: Allocator + ?Sized> {
    Resizable(A::ResizablePointer),
    Fixed(A::FixedPointer),
}

// ---------------------------------------------------------------------------
// Layer 0b: TransparentAllocator (exposes addresses; only Backends use it).
// ---------------------------------------------------------------------------

pub trait TransparentAllocator: Allocator {
    /// `None` if `raw` was only reserved, never claimed.
    fn address(&self, raw: Self::RawPointer) -> Option<Self::Address>;

    /// Like [`Allocator::resize`], but reports a relocation as
    /// `Some((old, new))` so the backend can move the bytes in `Storage`.
    fn resize_transparently(
        &self,
        p: &Self::ResizablePointer,
        new_size: Self::Size,
    ) -> Option<(Self::Address, Self::Address)>;
}

// ---------------------------------------------------------------------------
// Storage: raw byte access, orthogonal to Allocator.
// ---------------------------------------------------------------------------

pub trait Storage: Read + Write + Seek {
    /// Grows or shrinks the backing store to `new_len` bytes.
    fn resize(&mut self, new_len: u64) -> std::io::Result<()>;
    /// Current length in bytes.
    fn len(&self) -> std::io::Result<u64>;
    fn is_empty(&self) -> std::io::Result<bool> {
        Ok(self.len()? == 0)
    }
}

// ---------------------------------------------------------------------------
// WriteBackend / Backend: Allocator + memory access.
// ---------------------------------------------------------------------------

pub trait WriteBackend: Allocator {
    /// Hand back a writer positioned at `raw`, promising exactly `size` bytes.
    /// (See problems section: for a journaled backend, under-writing the
    /// promised span corrupts the op frame.)
    fn write_at(&mut self, raw: Self::RawPointer, size: Self::Size) -> impl Write + '_;

    /// Atomic resize + tail-shift + content overwrite of one region.
    fn splice(
        &mut self,
        p: &Self::ResizablePointer,
        offset: Self::Size,
        old_len: Self::Size,
        new: &[u8],
    );
}

pub trait Backend: WriteBackend {
    /// Hand back a reader positioned at `raw + offset`.
    fn read_at(&self, raw: Self::RawPointer, offset: Self::Size) -> impl Read + Seek + '_;
}

// ===========================================================================
// A tiny in-memory mock `Allocator`, purely to prove the trait composes with
// concrete associated types and the `Word` arithmetic.
// ===========================================================================

#[derive(Debug, Default, Clone)]
pub struct NoMeta;

pub struct Resizable(NonZeroU32);
pub struct Fixed(NonZeroU32);
#[derive(Clone, Copy)]
pub struct Raw(NonZeroU32);

#[derive(Default)]
struct Slot {
    address: Option<u64>, // None = reserved, not yet claimed
    size: u32,
    sizedness_fixed: bool,
}

#[derive(Default)]
pub struct MockAllocator {
    table: std::cell::RefCell<HashMap<NonZeroU32, Slot>>,
    next_id: std::cell::Cell<u32>,
    bump: std::cell::Cell<u64>,
}

impl MockAllocator {
    fn fresh(&self, size: u32, fixed: bool, claim: bool) -> NonZeroU32 {
        let raw = self.next_id.get() + 1;
        self.next_id.set(raw);
        let id = NonZeroU32::new(raw).unwrap();
        let address = claim.then(|| {
            let a = self.bump.get();
            self.bump.set(a + u64::from(size));
            a
        });
        self.table.borrow_mut().insert(
            id,
            Slot {
                address,
                size,
                sizedness_fixed: fixed,
            },
        );
        id
    }
}

impl Allocator for MockAllocator {
    type Id = NonZeroU32;
    type Address = u64;
    type Size = u32;
    type Meta = NoMeta;
    type ResizablePointer = Resizable;
    type FixedPointer = Fixed;
    type RawPointer = Raw;

    fn alloc_resizable(&self, size: u32) -> Resizable {
        Resizable(self.fresh(size, false, true))
    }
    fn alloc_fixed(&self, size: u32) -> Fixed {
        Fixed(self.fresh(size, true, true))
    }
    fn free_resizable(&self, p: Resizable) {
        self.table.borrow_mut().remove(&p.0);
    }
    fn free_fixed(&self, p: Fixed) {
        self.table.borrow_mut().remove(&p.0);
    }
    fn resize(&self, p: &Resizable, new_size: u32) {
        if let Some(slot) = self.table.borrow_mut().get_mut(&p.0) {
            slot.size = new_size;
        }
    }
    fn reserve_resizable(&self, size: u32) -> Resizable {
        Resizable(self.fresh(size, false, false))
    }
    fn reserve_fixed(&self, size: u32) -> Fixed {
        Fixed(self.fresh(size, true, false))
    }
    fn claim_resizable(&self, p: &Resizable) {
        if let Some(slot) = self.table.borrow_mut().get_mut(&p.0) {
            if slot.address.is_none() {
                let a = self.bump.get();
                self.bump.set(a + u64::from(slot.size));
                slot.address = Some(a);
            }
        }
    }
    fn claim_fixed(&self, p: &Fixed) {
        self.claim_resizable(&Resizable(p.0));
    }
    fn make_resizable(&self, p: Fixed) -> Resizable {
        if let Some(slot) = self.table.borrow_mut().get_mut(&p.0) {
            slot.sizedness_fixed = false;
        }
        Resizable(p.0)
    }
    fn make_fixed(&self, p: Resizable) -> Fixed {
        if let Some(slot) = self.table.borrow_mut().get_mut(&p.0) {
            slot.sizedness_fixed = true;
        }
        Fixed(p.0)
    }
    fn raw_resizable(&self, p: &Resizable) -> Raw {
        Raw(p.0)
    }
    fn raw_fixed(&self, p: &Fixed) -> Raw {
        Raw(p.0)
    }
    fn id(&self, raw: Raw) -> NonZeroU32 {
        raw.0
    }
    fn resolve_owned(&self, id: NonZeroU32) -> Option<Owned<Self>> {
        let fixed = self.table.borrow().get(&id)?.sizedness_fixed;
        Some(if fixed {
            Owned::Fixed(Fixed(id))
        } else {
            Owned::Resizable(Resizable(id))
        })
    }
    fn lookup(&self, raw: Raw) -> Option<Allocation<Self>> {
        let table = self.table.borrow();
        let slot = table.get(&raw.0)?;
        Some(Allocation {
            id: raw.0,
            size: slot.size,
            sizedness: if slot.sizedness_fixed {
                Sizedness::Fixed
            } else {
                Sizedness::Resizable
            },
            meta: NoMeta,
        })
    }
}

impl TransparentAllocator for MockAllocator {
    fn address(&self, raw: Raw) -> Option<u64> {
        self.table.borrow().get(&raw.0)?.address
    }
    fn resize_transparently(&self, p: &Resizable, new_size: u32) -> Option<(u64, u64)> {
        // The mock always "relocates" to keep the sketch honest about the
        // return shape.
        let old = self.address(self.raw_resizable(p))?;
        self.resize(p, new_size);
        let new = self.bump.get();
        self.bump.set(new + u64::from(new_size));
        if let Some(slot) = self.table.borrow_mut().get_mut(&p.0) {
            slot.address = Some(new);
        }
        Some((old, new))
    }
}

// A `Cursor<Vec<u8>>`-backed `Storage`, proving the `Read+Write+Seek+resize`
// shape works.
#[allow(dead_code)] // constructed only in `#[cfg(test)]`
struct MockStorage(Cursor<Vec<u8>>);

impl Read for MockStorage {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}
impl Write for MockStorage {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}
impl Seek for MockStorage {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.0.seek(pos)
    }
}
impl Storage for MockStorage {
    fn resize(&mut self, new_len: u64) -> std::io::Result<()> {
        self.0.get_mut().resize(new_len as usize, 0);
        Ok(())
    }
    fn len(&self) -> std::io::Result<u64> {
        Ok(self.0.get_ref().len() as u64)
    }
}

// A minimal `Backend` composed of a `Storage` + a `TransparentAllocator`,
// proving the layering (Allocator -> WriteBackend -> Backend) composes and
// that the RPITIT `-> impl Write` / `-> impl Read + Seek` returns work when a
// backend hands out its inner `&mut Storage`/`&Storage`.
pub struct UnjournaledBackend<S: Storage, A: TransparentAllocator> {
    storage: S,
    alloc: A,
}

impl<S: Storage, A: TransparentAllocator> Allocator for UnjournaledBackend<S, A> {
    type Id = A::Id;
    type Address = A::Address;
    type Size = A::Size;
    type Meta = A::Meta;
    type ResizablePointer = A::ResizablePointer;
    type FixedPointer = A::FixedPointer;
    type RawPointer = A::RawPointer;

    fn alloc_resizable(&self, size: A::Size) -> A::ResizablePointer {
        self.alloc.alloc_resizable(size)
    }
    fn alloc_fixed(&self, size: A::Size) -> A::FixedPointer {
        self.alloc.alloc_fixed(size)
    }
    fn free_resizable(&self, p: A::ResizablePointer) {
        self.alloc.free_resizable(p)
    }
    fn free_fixed(&self, p: A::FixedPointer) {
        self.alloc.free_fixed(p)
    }
    fn resize(&self, p: &A::ResizablePointer, new_size: A::Size) {
        self.alloc.resize(p, new_size)
    }
    fn make_resizable(&self, p: A::FixedPointer) -> A::ResizablePointer {
        self.alloc.make_resizable(p)
    }
    fn make_fixed(&self, p: A::ResizablePointer) -> A::FixedPointer {
        self.alloc.make_fixed(p)
    }
    fn raw_resizable(&self, p: &A::ResizablePointer) -> A::RawPointer {
        self.alloc.raw_resizable(p)
    }
    fn raw_fixed(&self, p: &A::FixedPointer) -> A::RawPointer {
        self.alloc.raw_fixed(p)
    }
    fn id(&self, raw: A::RawPointer) -> A::Id {
        self.alloc.id(raw)
    }
    fn resolve_owned(&self, id: A::Id) -> Option<Owned<Self>> {
        Some(match self.alloc.resolve_owned(id)? {
            Owned::Resizable(p) => Owned::Resizable(p),
            Owned::Fixed(p) => Owned::Fixed(p),
        })
    }
    fn lookup(&self, raw: A::RawPointer) -> Option<Allocation<Self>> {
        let a = self.alloc.lookup(raw)?;
        Some(Allocation {
            id: a.id,
            size: a.size,
            sizedness: a.sizedness,
            meta: a.meta,
        })
    }
}

impl<S: Storage, A: TransparentAllocator> WriteBackend for UnjournaledBackend<S, A> {
    fn write_at(&mut self, raw: A::RawPointer, _size: A::Size) -> impl Write + '_ {
        // Resolve to an address, seek, and hand out the storage as the writer.
        let addr = self.alloc.address(raw).expect("write to unclaimed pointer");
        self.storage
            .seek(SeekFrom::Start(addr.to_usize() as u64))
            .expect("seek");
        &mut self.storage
    }
    fn splice(&mut self, _p: &A::ResizablePointer, _offset: A::Size, _old_len: A::Size, _new: &[u8]) {
        // (elided in the prototype)
    }
}

impl<S: Storage, A: TransparentAllocator> Backend for UnjournaledBackend<S, A> {
    fn read_at(&self, raw: A::RawPointer, offset: A::Size) -> impl Read + Seek + '_ {
        let addr = self.alloc.address(raw).expect("read from unclaimed pointer");
        let start = addr.to_usize() + offset.to_usize();
        // The prototype reads a fresh cursor over a copy; a real backend would
        // hand out `&mut Storage` after seeking (needs `&mut self`, see the
        // problems section on read/write both wanting the single cursor).
        let bytes = {
            // We can't easily reuse the single `Storage` cursor from `&self`,
            // so this stand-in just proves the return type composes.
            let _ = start;
            Vec::<u8>::new()
        };
        Cursor::new(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocator_composes_with_generic_associated_types() {
        let a = MockAllocator::default();

        let r = a.alloc_resizable(16);
        let raw = a.raw_resizable(&r);
        assert_eq!(a.size(raw), Some(16));
        assert_eq!(a.lookup(raw).unwrap().sizedness, Sizedness::Resizable);

        // reserve (no address) -> claim (address assigned): the journaling path
        let f = a.reserve_fixed(8);
        let fraw = a.raw_fixed(&f);
        assert_eq!(a.address(fraw), None);
        a.claim_fixed(&f);
        assert!(a.address(fraw).is_some());

        // serialize the id, later reconstruct the correct owned handle
        let id = a.id(raw);
        assert!(matches!(a.resolve_owned(id), Some(Owned::Resizable(_))));

        // fixed <-> resizable conversion keeps the id in the mock
        let promoted = a.make_resizable(f);
        assert_eq!(a.lookup(a.raw_resizable(&promoted)).unwrap().sizedness, Sizedness::Resizable);

        a.free_resizable(r);
        a.free_resizable(promoted);
    }

    #[test]
    fn backend_layering_composes() {
        let mut backend = UnjournaledBackend {
            storage: MockStorage(Cursor::new(vec![0u8; 64])),
            alloc: MockAllocator::default(),
        };
        let p = backend.alloc_resizable(4);
        let raw = backend.raw_resizable(&p);
        {
            let mut w = backend.write_at(raw, 4);
            w.write_all(&[1, 2, 3, 4]).unwrap();
        }
        let mut r = backend.read_at(raw, 0);
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).unwrap();
        backend.free_resizable(p);
    }
}
