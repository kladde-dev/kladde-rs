//! [`Allocator`]: pure management of a dynamic set of non-overlapping address
//! ranges keyed by stable [`Pointer`] ids, plus [`SimpleAllocator`], a simple
//! functional (unoptimized) in-memory implementation.
//!
//! The allocator knows nothing about the *bytes* stored at those ranges (that is
//! the backend's job) and nothing about *types* (that is `Persistable`'s job, a
//! layer above). Addresses **are** core to the contract, but a backend hides
//! them from everything above it. Mutating methods take a plain `&mut self`; the
//! `&self` interior-mutability facade the guard model needs lives only in the
//! backend adapter, never here.
//!
//! ## Fallible vs. infallible surface
//!
//! Methods that take an **owned handle** (`resize`, `make_*`, `free_*`) cannot
//! receive a dangling id through normal use -- the handle itself proves the
//! allocation is live -- so a dangling handle is a *logic bug* and they
//! **panic** rather than return an error. `alloc_*` is likewise infallible
//! (it panics on exhaustion, like the standard global allocator). Only the
//! **queries over a raw `Pointer`** (`address`, `size`, `lookup`, `resolve`, …)
//! are fallible: an id decoded from a possibly-corrupt file during `load` can be
//! dangling, which is a recoverable [`AllocError`], not a panic.

use std::collections::HashMap;
use std::hash::Hash;
use std::num::NonZeroU32;

use crate::pointer::{Pointer, ResolvedPointer, UniquePointerFixedSize, UniquePointerResizable};
use crate::word::Word;

/// A query error over a raw, possibly-untrusted [`Pointer`].
///
/// Kept minimal and Kladde-agnostic. `DanglingPointer`: the id isn't a live
/// allocation (freed, never existed, or decoded from corrupt bytes).
/// `WrongSizedness`: a `*_fixed_size`/`*_resizable` query found a live
/// allocation of the *other* sizedness (so the specialized call sites get a
/// single error path instead of matching on [`ResolvedPointer`]).
///
/// There is deliberately no `Exhausted` variant yet: `alloc_*`/`resize` are
/// infallible in this crate (panic on out-of-space, like `std`'s allocator), so
/// nothing produces exhaustion. A future *bounded*, file-backed allocator that
/// can genuinely run out of space would add it (and make those methods
/// fallible) at that point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocError {
    DanglingPointer,
    WrongSizedness,
}

/// What a resize (or a `make_*` conversion) did to an allocation's address.
///
/// The 3-way split (rather than a bare `Option<(old, new)>`) both distinguishes
/// the two "no move" cases a backend must treat differently and documents which
/// address is old and which is new.
///
/// `Unclaimed` is retained for the raw-`Allocator` contract (resizing a
/// reserved-but-not-yet-claimed allocation, which has no address yet); the
/// backends route around it in practice, so an in-memory allocator like
/// [`SimpleAllocator`] never produces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relocation<Addr> {
    /// The bytes must move from `old` to `new`.
    Relocated { old: Addr, new: Addr },
    /// The allocation stayed at `addr` (shrink, or an in-place grow).
    InPlace { addr: Addr },
    /// The allocation has no address yet (a reservation); nothing to move.
    Unclaimed,
}

/// Everything the table knows about one allocation, borrowing the allocator for
/// `'a`. Addresses are deliberately *not* here -- they are queried separately
/// (`address`) and never surfaced above the backend.
pub struct Allocation<'a, A: Allocator + ?Sized> {
    pub pointer: ResolvedPointer<A::Pointer>,
    pub size: A::Size,
    pub meta: &'a A::Meta,
}

/// Like [`Allocation`] but with a mutable borrow of the per-allocation metadata.
pub struct AllocationMut<'a, A: Allocator + ?Sized> {
    pub pointer: ResolvedPointer<A::Pointer>,
    pub size: A::Size,
    pub meta: &'a mut A::Meta,
}

/// Pure address-range management over stable, `Copy` [`Pointer`] ids.
///
/// Implementors provide the lifecycle methods, `address`, and `lookup`/
/// `lookup_mut`; the query conveniences (`resolve`/`size`/`meta` and their
/// sizedness-specialized `*_fixed_size`/`*_resizable` forms) are all defaulted.
/// The specialized forms return the typed handle directly so a `load` that knows
/// the sizedness in advance skips the [`ResolvedPointer`] match and has one error
/// path (see `generic-allocator.md`, Rob's response on the query split).
pub trait Allocator {
    /// The concrete, serialized id (e.g. `Pointer<u32>`). `Eq + Hash` for the
    /// table; **not** a serialization bound -- byte encoding is the backend's job.
    type Pointer: Copy + Eq + Hash;
    /// Internal memory address; core to the allocator, hidden above the backend.
    type Address: Word;
    /// Allocation sizes and offsets. `Into<Address>` because a size added to an
    /// address must land in the address space.
    type Size: Word + Into<Self::Address>;
    /// Per-allocation metadata kept in the allocator's own table (backends use
    /// it to track where allocator state lives). `()` if unused.
    type Meta: Default;

    // --- lifecycle (infallible; panic on exhaustion) ---
    fn alloc_resizable(&mut self, size: Self::Size) -> UniquePointerResizable<Self::Pointer>;
    fn alloc_fixed_size(&mut self, size: Self::Size) -> UniquePointerFixedSize<Self::Pointer>;
    fn free_resizable(&mut self, p: UniquePointerResizable<Self::Pointer>);
    fn free_fixed_size(&mut self, p: UniquePointerFixedSize<Self::Pointer>);

    /// Resize a resizable allocation to `new_size`, reporting whether the bytes
    /// must move. Infallible: the owned handle proves the id is live, so a
    /// dangling handle is a bug and panics. (`FixedSize` has no `resize` -- that
    /// is the sizedness gate.)
    fn resize(
        &mut self,
        p: &UniquePointerResizable<Self::Pointer>,
        new_size: Self::Size,
    ) -> Relocation<Self::Address>;

    // --- addresses are core (no TransparentAllocator) ---
    /// The address of a *claimed* allocation. `Err(DanglingPointer)` for a
    /// non-live id. A reserved-but-unclaimed pointer is never a valid argument
    /// (during journal replay `claim` always precedes any `address` call).
    fn address(&self, p: Self::Pointer) -> Result<Self::Address, AllocError>;

    // --- reserve an id now, assign an address later (journaling) ---
    // Defaults allocate immediately; a journaled allocator overrides these to
    // defer address assignment to `claim_*` during replay.
    fn reserve_resizable(&mut self, size: Self::Size) -> UniquePointerResizable<Self::Pointer> {
        self.alloc_resizable(size)
    }
    fn reserve_fixed_size(&mut self, size: Self::Size) -> UniquePointerFixedSize<Self::Pointer> {
        self.alloc_fixed_size(size)
    }
    fn claim_resizable(&mut self, _p: &UniquePointerResizable<Self::Pointer>) {}
    fn claim_fixed_size(&mut self, _p: &UniquePointerFixedSize<Self::Pointer>) {}

    // --- convert sizedness, bundling a resize (the realistic use case) ---
    /// Consume a fixed-size handle, return a resizable one for the same data,
    /// resized to `new_size`. May mint a new id (some allocators pack a
    /// sizedness bit into the id); the caller must rewrite any serialized copy.
    /// Tries to keep the data in place at the old size but does not guarantee it.
    fn make_resizable(
        &mut self,
        p: UniquePointerFixedSize<Self::Pointer>,
        new_size: Self::Size,
    ) -> (
        UniquePointerResizable<Self::Pointer>,
        Relocation<Self::Address>,
    );
    /// The reverse of [`Allocator::make_resizable`].
    fn make_fixed_size(
        &mut self,
        p: UniquePointerResizable<Self::Pointer>,
        new_size: Self::Size,
    ) -> (
        UniquePointerFixedSize<Self::Pointer>,
        Relocation<Self::Address>,
    );

    // --- query the table (raw Pointer -> fallible) ---
    fn lookup(&self, p: Self::Pointer) -> Result<Allocation<'_, Self>, AllocError>;
    /// `&mut self` because only the `meta` part is handed out mutably.
    fn lookup_mut(&mut self, p: Self::Pointer) -> Result<AllocationMut<'_, Self>, AllocError>;

    fn resolve(&self, p: Self::Pointer) -> Result<ResolvedPointer<Self::Pointer>, AllocError> {
        self.lookup(p).map(|a| a.pointer)
    }
    fn size(&self, p: Self::Pointer) -> Result<Self::Size, AllocError> {
        self.lookup(p).map(|a| a.size)
    }
    fn meta(&self, p: Self::Pointer) -> Result<&Self::Meta, AllocError> {
        self.lookup(p).map(|a| a.meta)
    }
    fn meta_mut(&mut self, p: Self::Pointer) -> Result<&mut Self::Meta, AllocError> {
        self.lookup_mut(p).map(|a| a.meta)
    }

    // --- sizedness-specialized queries (return the typed handle directly) ---
    fn resolve_fixed_size(
        &self,
        p: Self::Pointer,
    ) -> Result<UniquePointerFixedSize<Self::Pointer>, AllocError> {
        match self.resolve(p)? {
            ResolvedPointer::Fixed(h) => Ok(h),
            ResolvedPointer::Resizable(_) => Err(AllocError::WrongSizedness),
        }
    }
    fn resolve_resizable(
        &self,
        p: Self::Pointer,
    ) -> Result<UniquePointerResizable<Self::Pointer>, AllocError> {
        match self.resolve(p)? {
            ResolvedPointer::Resizable(h) => Ok(h),
            ResolvedPointer::Fixed(_) => Err(AllocError::WrongSizedness),
        }
    }
    #[allow(clippy::type_complexity)]
    fn lookup_fixed_size(
        &self,
        p: Self::Pointer,
    ) -> Result<
        (
            UniquePointerFixedSize<Self::Pointer>,
            Self::Size,
            &Self::Meta,
        ),
        AllocError,
    > {
        let a = self.lookup(p)?;
        match a.pointer {
            ResolvedPointer::Fixed(h) => Ok((h, a.size, a.meta)),
            ResolvedPointer::Resizable(_) => Err(AllocError::WrongSizedness),
        }
    }
    #[allow(clippy::type_complexity)]
    fn lookup_resizable(
        &self,
        p: Self::Pointer,
    ) -> Result<
        (
            UniquePointerResizable<Self::Pointer>,
            Self::Size,
            &Self::Meta,
        ),
        AllocError,
    > {
        let a = self.lookup(p)?;
        match a.pointer {
            ResolvedPointer::Resizable(h) => Ok((h, a.size, a.meta)),
            ResolvedPointer::Fixed(_) => Err(AllocError::WrongSizedness),
        }
    }
    #[allow(clippy::type_complexity)]
    fn lookup_mut_fixed_size(
        &mut self,
        p: Self::Pointer,
    ) -> Result<
        (
            UniquePointerFixedSize<Self::Pointer>,
            Self::Size,
            &mut Self::Meta,
        ),
        AllocError,
    > {
        let a = self.lookup_mut(p)?;
        match a.pointer {
            ResolvedPointer::Fixed(h) => Ok((h, a.size, a.meta)),
            ResolvedPointer::Resizable(_) => Err(AllocError::WrongSizedness),
        }
    }
    #[allow(clippy::type_complexity)]
    fn lookup_mut_resizable(
        &mut self,
        p: Self::Pointer,
    ) -> Result<
        (
            UniquePointerResizable<Self::Pointer>,
            Self::Size,
            &mut Self::Meta,
        ),
        AllocError,
    > {
        let a = self.lookup_mut(p)?;
        match a.pointer {
            ResolvedPointer::Resizable(h) => Ok((h, a.size, a.meta)),
            ResolvedPointer::Fixed(_) => Err(AllocError::WrongSizedness),
        }
    }
}

// ============================ SimpleAllocator ============================

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sizedness {
    Resizable,
    Fixed,
}

struct Row<M> {
    address: u64,
    size: u32,
    sizedness: Sizedness,
    meta: M,
}

/// A simple, functional, **unoptimized** in-memory allocator: a `HashMap` from
/// id to `(address, size, sizedness, meta)`, with a monotonically increasing
/// address bump and id counter. It never reuses addresses or ids and never
/// compacts -- it exists to exercise the trait surface and back the tests /
/// `MockBackend`, not to be efficient.
///
/// Concrete widths: `Pointer = Pointer<u32>`, `Address = u64`, `Size = u32`.
/// Generic only over the metadata type `M` (default `()`).
pub struct SimpleAllocator<M = ()> {
    table: HashMap<NonZeroU32, Row<M>>,
    next_id: u32,
    bump: u64,
}

impl<M> Default for SimpleAllocator<M> {
    fn default() -> Self {
        Self {
            table: HashMap::new(),
            next_id: 0,
            bump: 0,
        }
    }
}

impl<M> SimpleAllocator<M> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of live (not-yet-freed) allocations. Handy for leak checks.
    pub fn live_count(&self) -> usize {
        self.table.len()
    }
}

impl<M: Default> SimpleAllocator<M> {
    fn fresh(&mut self, size: u32, sizedness: Sizedness) -> Pointer {
        self.next_id += 1;
        let id = NonZeroU32::new(self.next_id).expect("id counter overflowed u32");
        let address = self.bump;
        self.bump += u64::from(size);
        self.table.insert(
            id,
            Row {
                address,
                size,
                sizedness,
                meta: M::default(),
            },
        );
        Pointer::from_nonzero(id)
    }

    /// Shared resize core for `resize`/`make_*`. Panics on a dangling id.
    fn resize_row(&mut self, key: NonZeroU32, new_size: u32) -> Relocation<u64> {
        let row = self.table.get(&key).expect("resize of a dangling pointer");
        let old_addr = row.address;
        if new_size <= row.size {
            // shrink or same size: stays put (a real allocator might not, but a
            // bump allocator trivially can leave the head where it is)
            self.table.get_mut(&key).unwrap().size = new_size;
            Relocation::InPlace { addr: old_addr }
        } else {
            let new_addr = self.bump;
            self.bump += u64::from(new_size);
            let row = self.table.get_mut(&key).unwrap();
            row.address = new_addr;
            row.size = new_size;
            Relocation::Relocated {
                old: old_addr,
                new: new_addr,
            }
        }
    }
}

impl<M: Default> Allocator for SimpleAllocator<M> {
    type Pointer = Pointer;
    type Address = u64;
    type Size = u32;
    type Meta = M;

    fn alloc_resizable(&mut self, size: u32) -> UniquePointerResizable<Pointer> {
        UniquePointerResizable::from_pointer(self.fresh(size, Sizedness::Resizable))
    }
    fn alloc_fixed_size(&mut self, size: u32) -> UniquePointerFixedSize<Pointer> {
        UniquePointerFixedSize::from_pointer(self.fresh(size, Sizedness::Fixed))
    }
    fn free_resizable(&mut self, p: UniquePointerResizable<Pointer>) {
        let existed = self.table.remove(&p.raw().nonzero()).is_some();
        assert!(
            existed,
            "free of a dangling/already-freed resizable pointer"
        );
    }
    fn free_fixed_size(&mut self, p: UniquePointerFixedSize<Pointer>) {
        let existed = self.table.remove(&p.raw().nonzero()).is_some();
        assert!(
            existed,
            "free of a dangling/already-freed fixed-size pointer"
        );
    }

    fn resize(&mut self, p: &UniquePointerResizable<Pointer>, new_size: u32) -> Relocation<u64> {
        self.resize_row(p.raw().nonzero(), new_size)
    }

    fn address(&self, p: Pointer) -> Result<u64, AllocError> {
        self.table
            .get(&p.nonzero())
            .map(|r| r.address)
            .ok_or(AllocError::DanglingPointer)
    }

    fn make_resizable(
        &mut self,
        p: UniquePointerFixedSize<Pointer>,
        new_size: u32,
    ) -> (UniquePointerResizable<Pointer>, Relocation<u64>) {
        let key = p.raw().nonzero();
        self.table
            .get_mut(&key)
            .expect("make_resizable of a dangling pointer")
            .sizedness = Sizedness::Resizable;
        let reloc = self.resize_row(key, new_size);
        (UniquePointerResizable::from_pointer(p.raw()), reloc)
    }
    fn make_fixed_size(
        &mut self,
        p: UniquePointerResizable<Pointer>,
        new_size: u32,
    ) -> (UniquePointerFixedSize<Pointer>, Relocation<u64>) {
        let key = p.raw().nonzero();
        self.table
            .get_mut(&key)
            .expect("make_fixed_size of a dangling pointer")
            .sizedness = Sizedness::Fixed;
        let reloc = self.resize_row(key, new_size);
        (UniquePointerFixedSize::from_pointer(p.raw()), reloc)
    }

    fn lookup(&self, p: Pointer) -> Result<Allocation<'_, Self>, AllocError> {
        let row = self
            .table
            .get(&p.nonzero())
            .ok_or(AllocError::DanglingPointer)?;
        let pointer = match row.sizedness {
            Sizedness::Resizable => {
                ResolvedPointer::Resizable(UniquePointerResizable::from_pointer(p))
            }
            Sizedness::Fixed => ResolvedPointer::Fixed(UniquePointerFixedSize::from_pointer(p)),
        };
        Ok(Allocation {
            pointer,
            size: row.size,
            meta: &row.meta,
        })
    }
    fn lookup_mut(&mut self, p: Pointer) -> Result<AllocationMut<'_, Self>, AllocError> {
        let row = self
            .table
            .get_mut(&p.nonzero())
            .ok_or(AllocError::DanglingPointer)?;
        let pointer = match row.sizedness {
            Sizedness::Resizable => {
                ResolvedPointer::Resizable(UniquePointerResizable::from_pointer(p))
            }
            Sizedness::Fixed => ResolvedPointer::Fixed(UniquePointerFixedSize::from_pointer(p)),
        };
        Ok(AllocationMut {
            pointer,
            size: row.size,
            meta: &mut row.meta,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dangling() -> Pointer {
        Pointer::from_raw(9999).unwrap()
    }

    #[test]
    fn alloc_records_size_address_and_sizedness() {
        let mut a = SimpleAllocator::<()>::new();
        let p = a.alloc_fixed_size(8);
        assert_eq!(a.size(p.raw()), Ok(8));
        assert_eq!(a.address(p.raw()), Ok(0));
        let q = a.alloc_resizable(4);
        assert_eq!(a.address(q.raw()), Ok(8)); // bumped past the first
        assert_eq!(a.live_count(), 2);
    }

    #[test]
    fn queries_on_a_dangling_id_are_errors() {
        let a = SimpleAllocator::<()>::new();
        assert_eq!(a.size(dangling()), Err(AllocError::DanglingPointer));
        assert_eq!(a.address(dangling()), Err(AllocError::DanglingPointer));
        assert!(a.lookup(dangling()).is_err());
    }

    #[test]
    fn resolve_recovers_the_right_sizedness() {
        let mut a = SimpleAllocator::<()>::new();
        let fixed = a.alloc_fixed_size(4);
        let resizable = a.alloc_resizable(4);
        assert!(a.resolve_fixed_size(fixed.raw()).is_ok());
        assert_eq!(
            a.resolve_fixed_size(resizable.raw()),
            Err(AllocError::WrongSizedness)
        );
        assert!(a.resolve_resizable(resizable.raw()).is_ok());
        assert_eq!(
            a.resolve_resizable(fixed.raw()),
            Err(AllocError::WrongSizedness)
        );
    }

    #[test]
    fn resize_reports_in_place_on_shrink_and_relocation_on_grow() {
        let mut a = SimpleAllocator::<()>::new();
        let p = a.alloc_resizable(8);
        let addr0 = a.address(p.raw()).unwrap();
        assert_eq!(a.resize(&p, 4), Relocation::InPlace { addr: addr0 });
        // grow past capacity -> bump allocator relocates
        match a.resize(&p, 16) {
            Relocation::Relocated { old, new } => {
                assert_eq!(old, addr0);
                assert_ne!(new, addr0);
                assert_eq!(a.address(p.raw()), Ok(new));
            }
            other => panic!("expected relocation, got {other:?}"),
        }
        assert_eq!(a.size(p.raw()), Ok(16));
    }

    #[test]
    fn make_resizable_flips_sizedness_keeping_the_id() {
        let mut a = SimpleAllocator::<()>::new();
        let fixed = a.alloc_fixed_size(4);
        let id = fixed.raw();
        let (resizable, _reloc) = a.make_resizable(fixed, 4);
        assert_eq!(resizable.raw(), id); // same id
        assert!(a.resolve_resizable(id).is_ok());
        assert_eq!(a.resolve_fixed_size(id), Err(AllocError::WrongSizedness));
    }

    #[test]
    fn lookup_fixed_size_hands_back_a_typed_tuple() {
        let mut a = SimpleAllocator::<()>::new();
        let p = a.alloc_fixed_size(12);
        let (handle, size, _meta) = a.lookup_fixed_size(p.raw()).unwrap();
        assert_eq!(handle.raw(), p.raw());
        assert_eq!(size, 12);
        assert_eq!(
            a.lookup_resizable(p.raw()).map(|_| ()),
            Err(AllocError::WrongSizedness)
        );
    }

    #[test]
    fn free_removes_the_allocation() {
        let mut a = SimpleAllocator::<()>::new();
        let p = a.alloc_fixed_size(4);
        a.free_fixed_size(p);
        assert_eq!(a.live_count(), 0);
        assert_eq!(a.size(dangling()), Err(AllocError::DanglingPointer));
    }

    #[test]
    fn metadata_is_readable_and_mutable() {
        let mut a = SimpleAllocator::<u32>::new();
        let p = a.alloc_fixed_size(4);
        assert_eq!(a.meta(p.raw()), Ok(&0));
        *a.meta_mut(p.raw()).unwrap() = 7;
        assert_eq!(a.meta(p.raw()), Ok(&7));
    }
}
