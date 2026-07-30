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
mod tuple;
pub use scalar::*;
pub use schema::SchemaBuilder;
pub use tuple::TupleGuard;

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

/// An owning handle to a variable-capacity region in the backed heap.
///
/// The persistent analog of `Box<[u8]>`: a single owner of a region whose
/// byte capacity is chosen at runtime and may change. Obtain one from
/// [`Allocator::alloc_resizable`] (or the typed
/// [`AllocatorExt::alloc_array`]), grow or shrink it with
/// [`resize`](Allocator::resize) / [`splice`](Allocator::splice), read its
/// current byte capacity with [`capacity`](Allocator::capacity), and release
/// it with [`Allocator::free_resizable`].
///
/// The allocator owns and remembers the region's capacity — it is not
/// recoverable any other way — so a value backed by one need only track its
/// own logical element count. The handle is a stable identity (it keeps
/// naming the same region even as compaction relocates the bytes) and is
/// neither `Clone` nor `Copy`, so single ownership — hence freedom from
/// double-frees — is enforced at compile time. Use [`raw`](Self::raw) to
/// read or write the region's bytes.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct UniquePointerResizable {
    index: NonZeroU32,
}

impl UniquePointerResizable {
    /// Rebuilds a handle from a raw allocation identity.
    ///
    /// Intended for [`Allocator`] implementations, which mint identities
    /// from their own counter; ordinary code obtains handles from
    /// [`Allocator::alloc_resizable`].
    pub fn from_index(index: NonZeroU32) -> Self {
        UniquePointerResizable { index }
    }

    /// Returns this allocation's stable raw identity.
    pub fn index(&self) -> NonZeroU32 {
        self.index
    }

    /// Returns a type-erased [`RawPointer`] naming the same region, for use
    /// as a [`Location`] anchor or a
    /// [`read`](Allocator::read)/[`write`](Allocator::write)/[`copy`](Allocator::copy)
    /// target.
    pub fn raw(&self) -> RawPointer {
        RawPointer(self.index)
    }
}

/// An owning handle to a fixed-size, type-erased region in the backed heap.
///
/// Like [`UniquePointerResizable`], a single-owner handle — but for a region
/// whose size is fixed for the lifetime of the file (up to schema evolution)
/// and therefore need not be stored per block: it is recoverable from the
/// static type of what lives there, or from metadata the owning container
/// keeps once (e.g. a chunked container storing one chunk size for all its
/// chunks). It carries no static type; for a boxed single value with a known
/// `T`, prefer the typed [`UniquePointer`].
///
/// Deliberately exposes **no** `resize`/`splice`: a fixed region never
/// changes size, and the type system enforces that (those operations accept
/// only [`UniquePointerResizable`]). Obtain one from
/// [`Allocator::alloc_fixed`] and release it with
/// [`Allocator::free_fixed`]. Not `Clone`/`Copy`; use [`raw`](Self::raw) for
/// byte access.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct UniquePointerFixedSize {
    index: NonZeroU32,
}

impl UniquePointerFixedSize {
    /// Rebuilds a handle from a raw allocation identity.
    ///
    /// Intended for [`Allocator`] implementations; ordinary code obtains
    /// handles from [`Allocator::alloc_fixed`].
    pub fn from_index(index: NonZeroU32) -> Self {
        UniquePointerFixedSize { index }
    }

    /// Returns this allocation's stable raw identity.
    pub fn index(&self) -> NonZeroU32 {
        self.index
    }

    /// Returns a type-erased [`RawPointer`] naming the same region.
    pub fn raw(&self) -> RawPointer {
        RawPointer(self.index)
    }
}

/// An owning handle to a fixed-size `T` in the backed heap — the persistent
/// `Box<T>`.
///
/// A typed wrapper around [`UniquePointerFixedSize`]: it names one region
/// holding a single `T` whose size is the statically known `T::INLINE_SIZE`,
/// so nothing about its size is persisted (the reader re-derives it from the
/// layout at that location). Obtain one from
/// [`AllocatorExt::alloc_boxed`] and release it with
/// [`AllocatorExt::free_boxed`]. Like the erased fixed handle it exposes no
/// `resize`/`splice`.
///
/// `PhantomData<*const T>` (not `PhantomData<T>`) gives covariance in `T` —
/// sound because mutation only ever happens through an exclusive `Guard`,
/// never a shared reference — without the "may drop a `T`" drop-check
/// obligation, which this handle never discharges (it holds no backend and
/// never runs `T::drop`; freeing is the owning type's job). Not
/// `Clone`/`Copy`.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct UniquePointer<T> {
    inner: UniquePointerFixedSize,
    _marker: PhantomData<*const T>,
}

impl<T> UniquePointer<T> {
    /// Attaches a static type to an erased fixed handle.
    pub fn from_fixed(inner: UniquePointerFixedSize) -> Self {
        UniquePointer {
            inner,
            _marker: PhantomData,
        }
    }

    /// Discards the static type, yielding the erased fixed handle.
    pub fn into_fixed(self) -> UniquePointerFixedSize {
        self.inner
    }

    /// Returns this allocation's stable raw identity.
    pub fn index(&self) -> NonZeroU32 {
        self.inner.index()
    }

    /// Returns a type-erased [`RawPointer`] naming the same region, for use
    /// as a [`Location`] anchor or a read/write/copy target, where the
    /// concrete `T` is irrelevant.
    pub fn raw(&self) -> RawPointer {
        self.inner.raw()
    }
}

/// A type- and size-erased, `Copy` address into the backed heap.
///
/// Produced from any owned handle by its `.raw()` method. `Copy`, since
/// (unlike the owned handles) there's no single-owner invariant to protect:
/// a `RawPointer` is just an address to read at, write at, or resolve, never
/// something that owns, frees, or resizes the region it names. It carries no
/// size — every operation through it supplies its own byte range.
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

impl std::ops::Add<u32> for Location {
    type Output = Location;

    /// Advances the location by `offset` bytes within the same anchor --
    /// how an inline value reaches a field/element at a static offset
    /// (`location + field_offset`).
    fn add(self, offset: u32) -> Location {
        Location {
            anchor: self.anchor,
            offset: self.offset + offset,
        }
    }
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

/// The on-disk-facing result of [`Allocator::resolve`]. Holds a region's
/// current on-disk `target` -- the offset of the region an owning pointer
/// refers *at*, which is what a pointer's written-out representation actually
/// is (as opposed to `position`, the offset of the pointer's *own*
/// serialized bytes, which is separate bookkeeping an `Allocator` tracks
/// internally -- not something `ResolvedPointer` knows).
///
/// Type-erased, matching [`RawPointer`] (the input to `resolve`): a resolved
/// target is a raw file offset, independent of what type lives there.
/// Borrowing the allocator for `'a` is deliberate: it prevents, at compile
/// time, any allocator operation that could invalidate this snapshot (most
/// importantly, compaction moving the target) for as long as a
/// `ResolvedPointer` derived from it is still alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedPointer<'a> {
    target: NonZeroU32,
    _allocator: PhantomData<&'a ()>,
}

impl<'a> ResolvedPointer<'a> {
    pub fn from_target(target: NonZeroU32) -> Self {
        ResolvedPointer {
            target,
            _allocator: PhantomData,
        }
    }

    pub fn target(&self) -> NonZeroU32 {
        self.target
    }
}

/// The low-level, type-agnostic byte storage interface behind the backed heap.
///
/// Implement this to store the persistent types on a storage medium of your
/// own; most application code uses those types (and
/// [`kladde::Kladde`](../kladde/struct.Kladde.html)) and never calls these
/// methods directly. Every method works on plain byte ranges within regions
/// named by owned handles ([`UniquePointerResizable`] /
/// [`UniquePointerFixedSize`]) or the [`Copy`] address [`RawPointer`].
///
/// The *required* methods deal only in **resizable**, type-erased regions —
/// the weakest assumption an allocator can be asked to support. Fixed-size
/// regions are an optional refinement: [`alloc_fixed`](Allocator::alloc_fixed)
/// / [`free_fixed`](Allocator::free_fixed) come with default implementations
/// that treat them like ordinary resizable regions, which an allocator may
/// override to exploit their fixed size. Typed convenience
/// ([`alloc_boxed`](AllocatorExt::alloc_boxed),
/// [`alloc_array`](AllocatorExt::alloc_array)) lives in the [`AllocatorExt`]
/// extension trait.
///
/// Mutating methods are recorded and become visible to the reading methods
/// (`read`, `resolve`, `capacity`) only after the next flush.
pub trait Allocator {
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

    /// Reserves a variable-capacity region of `byte_size` bytes and returns
    /// an owning [`UniquePointerResizable`] to it.
    ///
    /// The allocator remembers the region's capacity; change it later with
    /// [`resize`](Allocator::resize) or [`splice`](Allocator::splice), and
    /// read it back with [`capacity`](Allocator::capacity). The handle is
    /// usable immediately; the bytes become readable after the next flush.
    fn alloc_resizable(&self, byte_size: usize) -> UniquePointerResizable;

    /// Releases a resizable region, consuming its handle.
    ///
    /// Takes effect at the next flush.
    fn free_resizable(&self, pointer: UniquePointerResizable);

    /// Changes a resizable region's byte capacity to `new_byte_size`,
    /// preserving the bytes the old and new sizes share.
    ///
    /// The handle keeps its identity whether the region is resized in place
    /// or relocated. Growing zero-fills the new tail; shrinking drops the
    /// excess. Takes effect at the next flush.
    fn resize(&self, pointer: &UniquePointerResizable, new_byte_size: usize);

    /// Replaces the `old_byte_len` bytes at `byte_offset` in a resizable
    /// region with `new`, shifting the trailing bytes and adjusting the
    /// region's capacity, as a single atomic operation.
    ///
    /// All three of `byte_offset`, `old_byte_len`, and `new` are measured in
    /// bytes. Because the entire shift is one recorded operation, a crash can
    /// never leave the region half-updated: on recovery it is either fully
    /// spliced or untouched. Common shapes:
    ///
    /// - `splice(p, off, k, &[])` deletes `k` bytes at `off`;
    /// - `splice(p, off, 0, ins)` inserts `ins` at `off`;
    /// - `splice(p, 0, old, new)` replaces the entire content.
    ///
    /// Takes effect at the next flush.
    fn splice(
        &self,
        pointer: &UniquePointerResizable,
        byte_offset: u32,
        old_byte_len: u32,
        new: &[u8],
    );

    /// Returns a resizable region's current byte capacity, or `None` if it
    /// has not been flushed yet.
    ///
    /// Like [`read`](Allocator::read), this reflects only already-flushed
    /// state.
    fn capacity(&self, pointer: &UniquePointerResizable) -> Option<usize>;

    /// Resolves a region's address to its current on-disk `target`, or
    /// `None` if it has not been flushed yet.
    ///
    /// Takes a [`RawPointer`] (obtain one from any owned handle via
    /// `.raw()`), since resolution is a size-agnostic query. Reflects only
    /// already-flushed state.
    fn resolve(&self, pointer: RawPointer) -> Option<ResolvedPointer<'_>>;

    /// Reserves a **fixed-size** region of `byte_size` bytes and returns an
    /// owning [`UniquePointerFixedSize`] to it.
    ///
    /// A fixed region never resizes (the type system enforces it —
    /// [`resize`](Allocator::resize)/[`splice`](Allocator::splice) accept
    /// only [`UniquePointerResizable`]), which lets an allocator manage it
    /// more tightly (size-class bins, no per-block size metadata). The
    /// default implementation treats it exactly like an ordinary resizable
    /// region; override this (and [`free_fixed`](Allocator::free_fixed)) to
    /// exploit the fixed size.
    fn alloc_fixed(&self, byte_size: usize) -> UniquePointerFixedSize {
        UniquePointerFixedSize::from_index(self.alloc_resizable(byte_size).index())
    }

    /// Releases a fixed-size region, consuming its handle.
    ///
    /// The default reclaims it exactly like a resizable region; override it
    /// alongside [`alloc_fixed`](Allocator::alloc_fixed) to return the region
    /// to a fixed-size pool.
    fn free_fixed(&self, pointer: UniquePointerFixedSize) {
        self.free_resizable(UniquePointerResizable::from_index(pointer.index()))
    }
}

/// Typed and boxed conveniences over [`Allocator`], blanket-implemented for
/// every allocator so the core trait stays purely byte-oriented while call
/// sites can work in `Persistable` units.
///
/// The `T` here is always the *boxed value* or the *array element* — the type
/// whose `T::INLINE_SIZE` is the real stride — never a container type (whose
/// `INLINE_SIZE` is only its inline header).
pub trait AllocatorExt: Allocator {
    /// Boxes a single fixed-size value: reserves `T::INLINE_SIZE` bytes and
    /// returns a typed [`UniquePointer<T>`] — the persistent `Box<T>`.
    fn alloc_boxed<T: Persistable>(&self) -> UniquePointer<T> {
        UniquePointer::from_fixed(self.alloc_fixed(T::INLINE_SIZE))
    }

    /// Releases a boxed value, consuming its typed handle.
    fn free_boxed<T>(&self, pointer: UniquePointer<T>) {
        self.free_fixed(pointer.into_fixed())
    }

    /// Reserves a resizable run of `len` elements of `T`, sized from
    /// `T::INLINE_SIZE`.
    ///
    /// Returns the *erased* [`UniquePointerResizable`]: `T` is consumed only
    /// to compute the byte size, and the caller addresses elements itself via
    /// `.raw()` and byte offsets.
    fn alloc_array<T: Persistable>(&self, len: usize) -> UniquePointerResizable {
        self.alloc_resizable(len * T::INLINE_SIZE)
    }
}

impl<A: Allocator + ?Sized> AllocatorExt for A {}

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

        assert_eq!(UniquePointerResizable::from_index(index).index(), index);
        assert_eq!(UniquePointerFixedSize::from_index(index).index(), index);

        // The typed handle wraps the erased fixed one; the index survives the
        // round trip through both, as does `into_fixed`.
        let boxed = UniquePointer::<i32>::from_fixed(UniquePointerFixedSize::from_index(index));
        assert_eq!(boxed.index(), index);
        assert_eq!(boxed.into_fixed().index(), index);
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
            fn alloc_resizable(&self, byte_size: usize) -> UniquePointerResizable {
                let raw = self.next_index.get() + 1;
                self.next_index.set(raw);
                let index = NonZeroU32::new(raw).unwrap();
                self.regions
                    .borrow_mut()
                    .insert(index, vec![0u8; byte_size]);
                UniquePointerResizable::from_index(index)
            }
            fn free_resizable(&self, pointer: UniquePointerResizable) {
                self.regions.borrow_mut().remove(&pointer.index());
            }
            fn resize(&self, pointer: &UniquePointerResizable, new_byte_size: usize) {
                let mut regions = self.regions.borrow_mut();
                let region = regions.get_mut(&pointer.index()).unwrap();
                region.resize(new_byte_size, 0);
            }
            fn splice(
                &self,
                pointer: &UniquePointerResizable,
                byte_offset: u32,
                old_byte_len: u32,
                new: &[u8],
            ) {
                let mut regions = self.regions.borrow_mut();
                let region = regions.get_mut(&pointer.index()).unwrap();
                let start = byte_offset as usize;
                region.splice(start..start + old_byte_len as usize, new.iter().copied());
            }
            fn capacity(&self, pointer: &UniquePointerResizable) -> Option<usize> {
                self.regions.borrow().get(&pointer.index()).map(Vec::len)
            }
            fn resolve(&self, pointer: RawPointer) -> Option<ResolvedPointer<'_>> {
                self.regions
                    .borrow()
                    .contains_key(&pointer.index())
                    .then(|| ResolvedPointer::from_target(pointer.index()))
            }
        }

        #[test]
        fn traits_compose_end_to_end() {
            let backend = MockBackend::default();
            let root_pointer = backend.alloc_boxed::<Counter>();
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
