//! Core vocabulary shared by every other `kladde-*` crate: the traits that
//! define what it means for a type to be persistable, the guard pattern
//! used for mutation, and the pointer types used to reference allocated
//! regions in the backed heap.
//!
//! See `spec.md` (in the repository root) for the full design rationale.

use std::marker::PhantomData;
use std::num::NonZeroU32;

mod scalar;
mod schema;
pub use scalar::*;
pub use schema::SchemaBuilder;

// Re-exported so `#[derive(Persistable)]` output and hand-written
// `describe` impls can name every schema type through `kladde_traits`
// alone, without a separate `kladde-schema` dependency.
pub use kladde_schema::{
    Field, Fingerprint, Primitive, TypeDescriptor, TypeRef, TypeTable, Variant, Version,
};

/// The offset of a pointer's own serialized bytes within the file.
/// Type alias rather than a bare integer so widening it later (to `u64`,
/// or to a variable-length encoding) only touches one definition.
pub type Position = NonZeroU32;

/// The offset of the region a pointer refers to.
pub type Target = NonZeroU32;

/// The size, in bytes, of an allocated region.
pub type Size = NonZeroU32;

/// An owning handle to a fixed-size allocation in the backed heap.
///
/// The persistent analog of `Box<T>`: it uniquely owns one region holding
/// a single `T` whose size is fixed and known from `T` alone. Obtain one
/// from [`Allocator::alloc`] and release it with [`Allocator::free`]; for a
/// variable-length run of values, reach for [`UniqueArrayPointer`] instead.
///
/// The handle is a stable identity — it keeps naming the same logical
/// allocation even if the underlying bytes are relocated later, and even
/// before anything has been flushed. It is neither `Clone` nor `Copy`, so
/// the single-owner invariant (and freedom from double-frees) is enforced
/// at compile time. To write into the region or hand its address to a
/// nested value, use [`raw`](Self::raw).
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct UniquePointer<T> {
    index: NonZeroU32,
    _marker: PhantomData<*const T>,
}

impl<T> UniquePointer<T> {
    /// Rebuilds a handle from a raw allocation identity.
    ///
    /// Intended for [`Allocator`] implementations, which mint identities
    /// from their own counter; ordinary code obtains handles from
    /// [`Allocator::alloc`] rather than calling this.
    pub fn from_index(index: NonZeroU32) -> Self {
        UniquePointer {
            index,
            _marker: PhantomData,
        }
    }

    /// Returns this allocation's stable raw identity.
    pub fn index(&self) -> NonZeroU32 {
        self.index
    }

    /// Returns a type-erased [`RawPointer`] naming the same region.
    ///
    /// Use it as the `anchor` of a [`Location`] or as the target of
    /// [`Allocator::read`]/[`write`](Allocator::write)/[`copy`](Allocator::copy),
    /// where the concrete `T` is irrelevant.
    pub fn raw(&self) -> RawPointer {
        RawPointer(self.index)
    }
}

/// An owning handle to a variable-capacity array allocation in the backed heap.
///
/// The persistent analog of `Box<[T]>`: it uniquely owns a contiguous run
/// of `T`-sized slots whose count is chosen at runtime. Obtain one from
/// [`Allocator::alloc_array`], grow or shrink it with
/// [`resize_array`](Allocator::resize_array) or
/// [`splice`](Allocator::splice), query its current byte capacity with
/// [`array_capacity`](Allocator::array_capacity), and release it with
/// [`Allocator::free_array`]. For a single fixed-size value, use
/// [`UniquePointer`] instead.
///
/// The allocator owns and remembers the region's capacity, so a value
/// backed by one need only track its own logical element count. Like
/// [`UniquePointer`], the handle is a stable identity and is neither
/// `Clone` nor `Copy`, enforcing single ownership at compile time. Use
/// [`raw`](Self::raw) to read or write the region's bytes.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct UniqueArrayPointer<T> {
    index: NonZeroU32,
    _marker: PhantomData<*const T>,
}

impl<T> UniqueArrayPointer<T> {
    /// Rebuilds a handle from a raw allocation identity.
    ///
    /// Intended for [`Allocator`] implementations; ordinary code obtains
    /// handles from [`Allocator::alloc_array`].
    pub fn from_index(index: NonZeroU32) -> Self {
        UniqueArrayPointer {
            index,
            _marker: PhantomData,
        }
    }

    /// Returns this allocation's stable raw identity.
    pub fn index(&self) -> NonZeroU32 {
        self.index
    }

    /// Returns a type-erased [`RawPointer`] naming the same region.
    ///
    /// Use it as the `anchor` of a [`Location`] (array elements live inside
    /// this region) or as a
    /// [`read`](Allocator::read)/[`write`](Allocator::write)/[`copy`](Allocator::copy)
    /// target.
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
/// type (one with a separate content allocation: `PersistableVec`,
/// `PersistableHashMap`, `PersistableString`, `PersistableBlob<T>`) uses. `target`
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

/// The low-level, type-agnostic byte storage interface behind the backed heap.
///
/// Implement this to store the persistent types on a storage medium of
/// your own; most application code uses those types (and
/// [`kladde::Kladde`](../kladde/struct.Kladde.html)) and never calls these
/// methods directly. Every method works on plain byte ranges within
/// allocations named by [`UniquePointer`]/[`UniqueArrayPointer`].
///
/// Mutating methods (`alloc`, `free`, `alloc_array`, `free_array`,
/// `write`, `copy`, `resize_array`, `splice`) are recorded and become
/// visible to the reading methods (`read`, `resolve`, `array_capacity`)
/// only after the next flush.
pub trait Allocator {
    /// Reserves a fixed-size region of `size` bytes and returns an owning
    /// [`UniquePointer`] to it.
    ///
    /// The handle is usable immediately; the region's bytes become
    /// readable only after the next flush.
    fn alloc<T>(&self, size: usize) -> UniquePointer<T>;

    /// Releases a fixed-size region, consuming its handle.
    ///
    /// Takes effect at the next flush.
    fn free<T>(&self, pointer: UniquePointer<T>);

    /// Reserves a variable-capacity array region of `byte_size` bytes and
    /// returns an owning [`UniqueArrayPointer`] to it.
    ///
    /// The allocator remembers the region's capacity; change it later with
    /// [`resize_array`](Allocator::resize_array) or
    /// [`splice`](Allocator::splice), and read it back with
    /// [`array_capacity`](Allocator::array_capacity). The handle is usable
    /// immediately; the bytes become readable after the next flush.
    fn alloc_array<T>(&self, byte_size: usize) -> UniqueArrayPointer<T>;

    /// Releases an array region, consuming its handle.
    ///
    /// Takes effect at the next flush.
    fn free_array<T>(&self, pointer: UniqueArrayPointer<T>);

    /// Resolves a handle to its current on-disk location, or `None` if it
    /// has not been flushed yet.
    fn resolve<'a, T>(&'a self, pointer: &UniquePointer<T>) -> Option<ResolvedPointer<'a, T>>;

    /// Reads `len` bytes starting at `offset` within the region `target`
    /// names.
    ///
    /// Reflects only already-flushed state; changes recorded since the
    /// last flush are not visible here.
    fn read(&self, target: RawPointer, offset: u32, len: u32) -> Vec<u8>;

    /// Overwrites the span `offset..offset + bytes.len()` within the region
    /// `target` names with `bytes`.
    ///
    /// Takes effect at the next flush.
    fn write(&self, target: RawPointer, offset: u32, bytes: &[u8]);

    /// Copies `len` bytes from `src_offset` within `src` to `dst_offset`
    /// within `dst`.
    ///
    /// `src` and `dst` may name the same region, in which case the source
    /// and destination spans may overlap (an in-place shift). Takes effect
    /// at the next flush.
    fn copy(&self, src: RawPointer, src_offset: u32, len: u32, dst: RawPointer, dst_offset: u32);

    /// Changes an array region's byte capacity to `new_byte_size`,
    /// preserving the bytes the old and new sizes share.
    ///
    /// The handle keeps its identity whether the region is resized in place
    /// or relocated. Growing zero-fills the new tail; shrinking drops the
    /// excess. Takes effect at the next flush.
    fn resize_array<T>(&self, pointer: &UniqueArrayPointer<T>, new_byte_size: usize);

    /// Returns an array region's current byte capacity, or `None` if it has
    /// not been flushed yet.
    ///
    /// Like [`read`](Allocator::read), this reflects only already-flushed
    /// state.
    fn array_capacity<T>(&self, pointer: &UniqueArrayPointer<T>) -> Option<usize>;

    /// Replaces the `old_len` bytes at `offset` in an array region with
    /// `new`, shifting the trailing bytes and adjusting the region's
    /// capacity, as a single atomic operation.
    ///
    /// `offset`, `old_len`, and `new` are all measured in bytes. Because
    /// the entire shift is one recorded operation, a crash can never leave
    /// the region half-updated: on recovery it is either fully spliced or
    /// untouched. Common shapes:
    ///
    /// - `splice(p, off, k, &[])` deletes `k` bytes at `off`;
    /// - `splice(p, off, 0, ins)` inserts `ins` at `off`;
    /// - `splice(p, 0, old, new)` replaces the entire content.
    ///
    /// Takes effect at the next flush.
    fn splice<T>(&self, pointer: &UniqueArrayPointer<T>, offset: u32, old_len: u32, new: &[u8]);
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
    /// with a separate content allocation (`PersistableVec`,
    /// `PersistableString`, ...), a fixed 8-byte `{ target, len }` header.
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
    /// allocation pointer (`PersistableVec`, `PersistableString`, ...) may
    /// need to *learn* that pointer for the first time here -- e.g. a
    /// value built via `PersistableVec::from_iter`/`PersistableString::from`
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

    /// Builds this type's own descriptor node — the common-case schema
    /// hook. `#[derive(Persistable)]` generates this for you; a hand-written
    /// impl returns a fresh [`TypeDescriptor`], obtaining references to its
    /// field/element/parameter types by calling
    /// [`describe`](Persistable::describe) on each of them (which honors
    /// whatever registration policy *that* type has).
    ///
    /// The default [`describe`](Persistable::describe) registers whatever
    /// this returns under `Self`'s own `TypeId`, deduplicated and
    /// cycle-safe. A type that instead wants to be **schema-transparent** —
    /// reusing another type's descriptor rather than owning one — overrides
    /// [`describe`](Persistable::describe) directly and leaves this method
    /// unimplemented (it is then never called). Implementing *neither*
    /// panics: every `Persistable` must supply one or the other.
    fn describe_local(builder: &mut SchemaBuilder) -> TypeDescriptor
    where
        Self: 'static,
    {
        let _ = builder;
        panic!(
            "{}: implement `describe_local` (the usual case) or override \
             `describe` (for a schema-transparent type)",
            std::any::type_name::<Self>(),
        )
    }

    /// Records this type's representation into `builder`, returning a
    /// reference to its descriptor. You usually call the higher-level
    /// [`schema`] or [`fingerprint`] instead of this directly.
    ///
    /// The default registers a node built from
    /// [`describe_local`](Persistable::describe_local), deduplicated by
    /// `Self`'s `TypeId` and reserving the slot before recursing so cyclic
    /// types terminate — this is what almost every type (all derived ones)
    /// uses. Override it only to be **schema-transparent**, reusing another
    /// type's descriptor: return e.g. `builder.describe::<Inner>()` (or
    /// `<Inner as Persistable>::describe(builder)`) and skip
    /// [`describe_local`](Persistable::describe_local) entirely.
    ///
    /// [`schema`]: Persistable::schema
    /// [`fingerprint`]: Persistable::fingerprint
    fn describe(builder: &mut SchemaBuilder) -> TypeRef
    where
        Self: 'static,
    {
        builder.describe::<Self>()
    }

    /// This type's full descriptor table (its schema) -- a language-neutral
    /// description of how it lays out and interprets its bytes, rooted at
    /// index 0.
    fn schema() -> TypeTable
    where
        Self: 'static,
    {
        let mut builder = SchemaBuilder::new();
        let root = Self::describe(&mut builder);
        builder.finish(root)
    }

    /// This type's 128-bit [schema fingerprint](Fingerprint): a compact,
    /// reproducible identity for its representation, suitable for detecting
    /// at load time whether a stored file was written with a compatible
    /// layout.
    fn fingerprint() -> Fingerprint
    where
        Self: 'static,
    {
        Self::schema().fingerprint()
    }
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

            fn describe_local(_builder: &mut SchemaBuilder) -> TypeDescriptor {
                TypeDescriptor::Primitive(Primitive::U32)
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
            fn alloc_array<T>(&self, byte_size: usize) -> UniqueArrayPointer<T> {
                let raw = self.next_index.get() + 1;
                self.next_index.set(raw);
                let index = NonZeroU32::new(raw).unwrap();
                self.regions
                    .borrow_mut()
                    .insert(index, vec![0u8; byte_size]);
                UniqueArrayPointer::from_index(index)
            }
            fn free_array<T>(&self, pointer: UniqueArrayPointer<T>) {
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
            fn resize_array<T>(&self, pointer: &UniqueArrayPointer<T>, new_byte_size: usize) {
                let mut regions = self.regions.borrow_mut();
                let region = regions.get_mut(&pointer.index()).unwrap();
                region.resize(new_byte_size, 0);
            }
            fn array_capacity<T>(&self, pointer: &UniqueArrayPointer<T>) -> Option<usize> {
                self.regions.borrow().get(&pointer.index()).map(Vec::len)
            }
            fn splice<T>(
                &self,
                pointer: &UniqueArrayPointer<T>,
                offset: u32,
                old_len: u32,
                new: &[u8],
            ) {
                let mut regions = self.regions.borrow_mut();
                let region = regions.get_mut(&pointer.index()).unwrap();
                let start = offset as usize;
                region.splice(start..start + old_len as usize, new.iter().copied());
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
