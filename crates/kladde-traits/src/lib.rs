//! Core vocabulary shared by every other `kladde-*` crate: the traits that
//! define what it means for a type to be persistable, the guard pattern
//! used for mutation, and the pointer types used to reference allocated
//! regions in the backed heap.
//!
//! See `spec.md` (in the repository root) for the full design rationale.

use std::marker::PhantomData;
use std::num::NonZeroU32;

mod scalar;
pub use scalar::*;

/// The offset of a pointer's own serialized bytes within the file.
/// Type alias rather than a bare integer so widening it later (to `u64`,
/// or to a variable-length encoding) only touches one definition.
pub type Position = NonZeroU32;

/// The offset of the region a pointer refers to.
pub type Target = NonZeroU32;

/// The size, in bytes, of an allocated region.
pub type Size = NonZeroU32;

/// A stable, in-memory-only handle to an allocated region, assigned by an
/// [`Allocator`]. Deliberately small and opaque: it does *not* hold a
/// reference to any [`Backend`], so it can't free itself in `Drop` -- see
/// the "Freeing" section of `spec.md` for why that's fine (generated
/// [`Guard`] wrapper types are responsible for freeing the
/// `UniquePointer`s they own).
///
/// `index` is a stable identity assigned once by `Allocator` and never
/// changes for the lifetime of this pointer -- even though the region it
/// (eventually) refers to may move around during compaction or a
/// [`Allocator::resize`] relocation, and even though this pointer may not
/// have been flushed to the snapshot at all yet.
///
/// Deliberately *not* `Clone`/`Copy` (so double-freeing is a compile-time
/// impossibility, matching the single-owner design) and *not*
/// `Serialize`/`Deserialize` (`index` is meaningless outside the process
/// that assigned it) -- see [`ResolvedPointer`] for how a `UniquePointer`
/// embedded in some other value actually gets written out.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct UniquePointer<T> {
    index: NonZeroU32,
    _marker: PhantomData<*const T>,
}

impl<T> UniquePointer<T> {
    /// Only [`Allocator`] implementations are expected to call this --
    /// `index` should always come from an allocator's own counter.
    pub fn from_index(index: NonZeroU32) -> Self {
        UniquePointer {
            index,
            _marker: PhantomData,
        }
    }

    pub fn index(&self) -> NonZeroU32 {
        self.index
    }

    /// Erases `T`, for use as the `anchor` of a [`Location`] or as the
    /// target of [`Allocator::write`]/`copy`/`read` -- a deeply nested
    /// leaf writing into some ancestor's allocation doesn't know or care
    /// what concrete type that ancestor's own pointer was created as.
    pub fn raw(&self) -> RawPointer {
        RawPointer(self.index)
    }
}

/// A type-erased [`UniquePointer`] index -- see [`UniquePointer::raw`].
/// `Copy`, since (unlike `UniquePointer`) there's no single-owner
/// invariant to protect here: a `RawPointer` is just an address to write
/// at or read from, not something that owns or frees the region it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RawPointer(NonZeroU32);

impl RawPointer {
    pub fn from_index(index: NonZeroU32) -> Self {
        RawPointer(index)
    }

    pub fn index(&self) -> NonZeroU32 {
        self.0
    }
}

/// Where a [`Persistable`] value's fixed-size inline representation lives:
/// the nearest ancestor allocation that owns real storage (`anchor`),
/// plus a byte `offset` within it. Threaded down through
/// [`Persistable::guard`]/[`store`](Persistable::store)/[`load`](Persistable::load)
/// so even a deeply nested leaf (a struct field, a container element)
/// knows where to write. A value that owns its own allocation hands its
/// children a *fresh* `Location` (its own pointer, offset `0`); an inline
/// value just extends the one it was given by its own static offset. See
/// `spec.md`'s "The Trait Layer" for the full rationale.
#[derive(Debug, Clone, Copy)]
pub struct Location {
    pub anchor: RawPointer,
    pub offset: u32,
}

/// Writes an 8-byte `{ target: u32, len: u32 }` header at `location` --
/// the fixed-size inline representation every "owning" [`Persistable`]
/// type (one with a separate content allocation: `PersistedVec`,
/// `PersistedHashMap`, `PersistedString`, `Persisted<T>`) uses. `target`
/// is `0` to mean "no allocation yet", the same convention the pointer
/// registry uses (see `spec.md`'s Pointers and Memory Management).
pub fn write_header<B: Backend>(backend: &B, location: Location, index: NonZeroU32, len: u32) {
    let mut bytes = [0u8; 8];
    bytes[0..4].copy_from_slice(&index.get().to_le_bytes());
    bytes[4..8].copy_from_slice(&len.to_le_bytes());
    backend.write(location.anchor, location.offset, &bytes);
}

/// Reads back a header written by [`write_header`]. `None` for `target`
/// means "no allocation yet" -- either this value has never been stored
/// (freshly `None`-headed) or nothing has been flushed yet.
pub fn read_header<B: Backend>(backend: &B, location: Location) -> (Option<NonZeroU32>, u32) {
    let bytes = backend.read(location.anchor, location.offset, 8);
    let target = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    let len = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    (NonZeroU32::new(target), len)
}

/// The on-disk-facing counterpart of [`UniquePointer`], produced by
/// [`Allocator::resolve`]. Holds the pointer's current on-disk `target` --
/// the offset of the region it points *at*, which is what a pointer's
/// written-out representation actually is (as opposed to `position`, the
/// offset of the pointer's *own* serialized bytes, which is separate
/// bookkeeping an `Allocator` tracks internally once this value has
/// actually been written out somewhere -- not something `ResolvedPointer`
/// itself knows).
///
/// Borrowing the allocator for `'a` is deliberate: it prevents, at compile
/// time, any allocator operation that could invalidate this snapshot
/// (most importantly, compaction moving the target) for as long as a
/// `ResolvedPointer` derived from it is still alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedPointer<'a, T> {
    target: NonZeroU32,
    _allocator: PhantomData<&'a ()>,
    _marker: PhantomData<*const T>,
}

impl<'a, T> ResolvedPointer<'a, T> {
    pub fn from_target(target: NonZeroU32) -> Self {
        ResolvedPointer {
            target,
            _allocator: PhantomData,
            _marker: PhantomData,
        }
    }

    pub fn target(&self) -> NonZeroU32 {
        self.target
    }
}

/// Allocates, frees, resizes, and reads/writes bytes in the backed heap.
/// This is the *only* thing a [`Guard`] ever calls to make a mutation
/// durable: every recorded change is one of these type-agnostic
/// primitives, so there's no separate journal/recording abstraction and
/// nothing type-specific ever reaches the file. See `spec.md`'s "The
/// Trait Layer" for the full rationale.
pub trait Allocator {
    /// Allocates a fresh region of `size` bytes, returning a pointer that
    /// uniquely identifies it. The index is assigned immediately; the
    /// underlying bytes aren't necessarily materialized until the next
    /// flush -- see `spec.md`'s Pointers and Memory Management section.
    fn alloc<T>(&self, size: usize) -> UniquePointer<T>;

    /// Frees the region `pointer` identifies. See the "Freeing" section
    /// of `spec.md`: this doesn't necessarily touch live allocator state
    /// synchronously -- a real, file-backed `Allocator` would journal the
    /// free and apply it at the next flush.
    fn free<T>(&self, pointer: UniquePointer<T>);

    /// `None` if `pointer` hasn't been flushed yet (no target exists to
    /// resolve to).
    fn resolve<'a, T>(&'a self, pointer: &UniquePointer<T>) -> Option<ResolvedPointer<'a, T>>;

    /// Reads `len` bytes starting at `offset` within the region `target`
    /// identifies. Reflects only already-flushed (materialized) state --
    /// unlike `write`/`copy`/`resize`/`alloc`/`free`, this isn't itself a
    /// journaled mutation, just a query.
    fn read(&self, target: RawPointer, offset: u32, len: u32) -> Vec<u8>;

    /// Overwrites the span `offset..offset + bytes.len()` within the
    /// region `target` identifies with `bytes`.
    fn write(&self, target: RawPointer, offset: u32, bytes: &[u8]);

    /// Copies `len` bytes from `src_offset` within `src` to `dst_offset`
    /// within `dst` (`src` and `dst` may be the same region, for an
    /// in-place shift -- e.g. `PersistedVec::remove`'s tail memmove).
    fn copy(&self, src: RawPointer, src_offset: u32, len: u32, dst: RawPointer, dst_offset: u32);

    /// Changes an existing allocation's size, in place if it still fits
    /// at its current position, or by relocating (copying over
    /// `min(old_size, new_size)` bytes and freeing the old region)
    /// otherwise. The pointer's `index` never changes either way -- only
    /// `alloc`/`free` mint or retire an index; `resize` never does.
    fn resize<T>(&self, pointer: &UniquePointer<T>, new_size: usize);
}

/// The bound every [`Persistable`]/[`Guard`] is generic over. A thin,
/// blanket-implemented marker so application and derive-generated code
/// only ever needs to name `Backend`, not `Allocator` directly.
pub trait Backend: Allocator {}
impl<B: Allocator> Backend for B {}

/// A plain, in-memory value type that *could* be persisted -- it has the
/// right shape (a fixed-size inline representation, a paired [`Guard`]
/// view) -- but isn't yet tied to any backend. Only ever accessed
/// read-only; see `Guard` for mutation.
pub trait Persistable: Sized {
    /// The size, in bytes, of this type's fixed-size inline
    /// representation -- what a containing struct reserves for it inline,
    /// regardless of how much variable-length content it may own
    /// elsewhere. For a scalar it's the value's own bytes; for a derived
    /// struct, the sum of its fields'; for a derived enum, a 4-byte
    /// discriminant plus its largest variant; and for an "owning" type
    /// with a separate content allocation (`PersistedVec`,
    /// `PersistedString`, ...), a fixed 8-byte `{ target, len }` header.
    /// Having *some* fixed inline size is what makes sibling fields'
    /// offsets within a containing struct statically computable.
    const INLINE_SIZE: usize;

    type Guard<'s, B: Backend>: Guard<Persistable = Self, Backend = B>
    where
        Self: 's,
        B: 's;

    /// Borrows both `self` and a `Backend` for `'s`, producing a `Guard`
    /// through which mutations are recorded and applied. `location` is
    /// where *this* value's own inline representation lives (or will
    /// live, once first written) -- see [`Location`].
    fn guard<'s, B: Backend>(
        &'s mut self,
        backend: &'s B,
        location: Location,
    ) -> Self::Guard<'s, B>;

    /// Writes `self`'s current value as its fixed-size inline
    /// representation at `location`, creating/growing/writing whatever
    /// separate content allocation it needs along the way. Used both
    /// internally by `Guard::set`-style methods and by container types
    /// writing a brand-new element/entry that didn't exist at `location`
    /// before.
    ///
    /// Takes `&mut self`, not `&self`: a type with its own cached
    /// allocation pointer (`PersistedVec`, `PersistedString`, ...) may
    /// need to *learn* that pointer for the first time here -- e.g. a
    /// value built via `PersistedVec::from_iter`/`PersistedString::from`
    /// can hold real content while its pointer is still `None` (nothing
    /// has allocated for it yet), and `store` is exactly the place that
    /// allocation happens the first time such a value is written
    /// somewhere. If `store` only had `&self`, it could still allocate
    /// and write the content correctly, but the caller's own copy would
    /// stay stuck believing it has no allocation -- breaking any `Guard`
    /// obtained from it afterward (e.g. `get_mut`). Types with nothing of
    /// their own to fix up (scalars, derived structs, whose fields just
    /// forward this same call recursively) simply never need the
    /// mutability.
    fn store<B: Backend>(&mut self, backend: &B, location: Location);

    /// Reconstructs a fresh value purely from what's stored at
    /// `location` -- the read-side counterpart of `store`, used by the
    /// round-trip test and (eventually) by opening a file.
    fn load<B: Backend>(backend: &B, location: Location) -> Self;
}

/// A live, mutation-capable, RAII-style view onto a [`Persistable`]
/// value, tied to a [`Backend`] for its lifetime -- the write-side
/// counterpart of `Persistable`, in the same relationship `MutexGuard`
/// has to `Mutex`. Rarely implemented manually; usually generated by
/// `#[derive(Persistable)]`.
pub trait Guard {
    type Persistable: Persistable;
    type Backend: Backend;

    fn as_persistable(&self) -> &Self::Persistable;
    fn as_persistable_mut(&mut self) -> &mut Self::Persistable;
    fn backend(&self) -> &Self::Backend;
}

// Note: there is deliberately no blanket `impl<T: Guard> Deref for T`
// here. `Guard` is a foreign trait from the point of view of any
// downstream crate, so `Self` in a blanket impl over it would be a fully
// generic, uncovered type parameter -- exactly what Rust's orphan rules
// forbid (E0210), regardless of the trait bound on it. Instead, each
// concrete guard type gets its own `Deref`/`DerefMut` impl generated
// alongside it, since those are local types in whatever crate the macro
// (or hand-written container implementation) expands in.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_pointer_round_trips_its_index() {
        let index = NonZeroU32::new(7).unwrap();
        let pointer = UniquePointer::<()>::from_index(index);
        assert_eq!(pointer.index(), index);
    }

    // A minimal, self-contained mock of a `Persistable`/`Guard`/`Backend`
    // triple, exercised only to prove the trait definitions actually
    // compose the way real implementations (kladde-alloc, kladde-types)
    // are expected to use them -- not a real container.
    mod mock_usage {
        use super::*;
        use std::cell::RefCell;
        use std::collections::HashMap;

        struct Counter(u32);

        impl Persistable for Counter {
            const INLINE_SIZE: usize = 4;

            type Guard<'s, B: Backend>
                = CounterGuard<'s, B>
            where
                Self: 's,
                B: 's;

            fn guard<'s, B: Backend>(
                &'s mut self,
                backend: &'s B,
                location: Location,
            ) -> Self::Guard<'s, B> {
                CounterGuard {
                    inner: self,
                    backend,
                    location,
                }
            }

            fn store<B: Backend>(&mut self, backend: &B, location: Location) {
                backend.write(location.anchor, location.offset, &self.0.to_le_bytes());
            }

            fn load<B: Backend>(backend: &B, location: Location) -> Self {
                let bytes = backend.read(location.anchor, location.offset, 4);
                Counter(u32::from_le_bytes(bytes.try_into().unwrap()))
            }
        }

        struct CounterGuard<'s, B> {
            inner: &'s mut Counter,
            backend: &'s B,
            location: Location,
        }

        impl<'s, B: Backend> CounterGuard<'s, B> {
            fn set(&mut self, value: u32) {
                self.backend.write(
                    self.location.anchor,
                    self.location.offset,
                    &value.to_le_bytes(),
                );
                self.inner.0 = value;
            }
        }

        impl<'s, B: Backend> Guard for CounterGuard<'s, B> {
            type Persistable = Counter;
            type Backend = B;

            fn as_persistable(&self) -> &Counter {
                self.inner
            }
            fn as_persistable_mut(&mut self) -> &mut Counter {
                self.inner
            }
            fn backend(&self) -> &B {
                self.backend
            }
        }

        #[derive(Default)]
        struct MockBackend {
            regions: RefCell<HashMap<NonZeroU32, Vec<u8>>>,
            next_index: std::cell::Cell<u32>,
        }

        impl Allocator for MockBackend {
            fn alloc<T>(&self, size: usize) -> UniquePointer<T> {
                let raw = self.next_index.get() + 1;
                self.next_index.set(raw);
                let index = NonZeroU32::new(raw).unwrap();
                self.regions.borrow_mut().insert(index, vec![0u8; size]);
                UniquePointer::from_index(index)
            }
            fn free<T>(&self, pointer: UniquePointer<T>) {
                self.regions.borrow_mut().remove(&pointer.index());
            }
            fn resolve<'a, T>(
                &'a self,
                pointer: &UniquePointer<T>,
            ) -> Option<ResolvedPointer<'a, T>> {
                self.regions
                    .borrow()
                    .contains_key(&pointer.index())
                    .then(|| ResolvedPointer::from_target(pointer.index()))
            }
            fn read(&self, target: RawPointer, offset: u32, len: u32) -> Vec<u8> {
                let regions = self.regions.borrow();
                let region = &regions[&target.index()];
                region[offset as usize..(offset + len) as usize].to_vec()
            }
            fn write(&self, target: RawPointer, offset: u32, bytes: &[u8]) {
                let mut regions = self.regions.borrow_mut();
                let region = regions.get_mut(&target.index()).unwrap();
                let start = offset as usize;
                region[start..start + bytes.len()].copy_from_slice(bytes);
            }
            fn copy(
                &self,
                src: RawPointer,
                src_offset: u32,
                len: u32,
                dst: RawPointer,
                dst_offset: u32,
            ) {
                let bytes = self.read(src, src_offset, len);
                self.write(dst, dst_offset, &bytes);
            }
            fn resize<T>(&self, pointer: &UniquePointer<T>, new_size: usize) {
                let mut regions = self.regions.borrow_mut();
                let region = regions.get_mut(&pointer.index()).unwrap();
                region.resize(new_size, 0);
            }
        }

        #[test]
        fn traits_compose_end_to_end() {
            let backend = MockBackend::default();
            let root_pointer = backend.alloc::<Counter>(Counter::INLINE_SIZE);
            let location = Location {
                anchor: root_pointer.raw(),
                offset: 0,
            };
            let mut counter = Counter(0);

            let mut guard = counter.guard(&backend, location);
            guard.set(5);
            assert_eq!(guard.as_persistable().0, 5);

            let reloaded = Counter::load(&backend, location);
            assert_eq!(reloaded.0, 5);
        }
    }
}
