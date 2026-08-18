//! [`Persistable`]: a type that knows how to store itself into, and load itself
//! from, a backend allocation at a given [`Location`].
//!
//! Parametric over the *pointer type* `P` (default `Pointer`), **not** over the
//! size type: allocation sizes belong to the allocator (a container queries
//! `backend.size(ptr)` rather than storing a size inline), and offsets are
//! transient (computed at the moment of a read/write, never stored). So `P` is
//! the only width a `Persistable` type is pinned to, and `Size` flows from the
//! backend as `B::Size`.
//!
//! ## The `&self` / `&mut self` asymmetry
//!
//! [`store`](Persistable::store) and [`guard`](Persistable::guard) take a shared
//! `&B`, so a parent guard can hand the same backend to every field guard by
//! reborrow. [`load`](Persistable::load) takes `&mut B`, because a load is
//! *sequential* -- one field or element after another -- and `ReadBackend`
//! hands out a real seekable cursor rather than a copied buffer. As a free side
//! effect the borrow checker forbids loading while any guard is alive.

use kladde_heap::{ReadBackend, WriteBackend};
use kladde_schema::{Fingerprint, TypeDescriptor, TypeRef, TypeTable};

use crate::guard::Guard;
use crate::location::Location;
use crate::schema::SchemaBuilder;
use kladde_heap::PointerRepr;

/// A type with a fixed inline byte size that can round-trip through a backend.
///
/// The `P: PointerRepr` bound is what lets pointer-holding implementors
/// serialize `Option<P>` with the on-file null niche; a pointer-free type simply
/// ignores it and works at every width.
pub trait Persistable<P: PointerRepr = kladde_heap::Pointer>: Sized {
    /// The number of bytes this value occupies *inline* in its parent allocation.
    ///
    /// For a scalar it is the value's own bytes; for a derived struct, the sum of
    /// its fields'; for a derived enum, a 4-byte discriminant plus its largest
    /// variant; and for an "owning" type with a separate content allocation
    /// (`PersistableVec`, `PersistableString`, ...), just `P::BYTE_LEN` -- the
    /// pointer id alone, since the heap already owns the allocation's size.
    /// Having *some* fixed inline size is what makes sibling fields' offsets
    /// within a containing struct statically computable.
    const INLINE_SIZE: usize;

    /// The mutation-capable view onto this type. See [`Guard`].
    type Guard<'s, B: WriteBackend<Pointer = P>>: Guard<Persistable = Self, Backend = B>
    where
        Self: 's,
        B: 's;

    /// Borrows both `self` and a backend for `'s`, producing a [`Guard`] through
    /// which mutations are recorded and applied. `location` is where *this*
    /// value's own inline representation lives (or will live, once first
    /// written).
    fn guard<'s, B: WriteBackend<Pointer = P>>(
        &'s mut self,
        backend: &'s B,
        location: Location<P, B::Size>,
    ) -> Self::Guard<'s, B>;

    /// Writes `self`'s current value as its fixed-size inline representation at
    /// `location`, creating/growing/writing whatever separate content allocation
    /// it needs along the way.
    ///
    /// Takes `&mut self`, not `&self`: a type with its own cached allocation
    /// pointer (`PersistableVec`, `PersistableString`, ...) may need to *learn*
    /// that pointer for the first time here -- e.g. a value built via
    /// `from_iter` can hold real content while its pointer is still `None`, and
    /// `store` is exactly the place that allocation happens the first time such
    /// a value is written somewhere. With only `&self` it could still allocate
    /// and write correctly, but the caller's own copy would stay stuck believing
    /// it has no allocation, breaking any `Guard` obtained from it afterward.
    fn store<B: WriteBackend<Pointer = P>>(&mut self, backend: &B, location: Location<P, B::Size>);

    /// Reconstructs a fresh value purely from what is stored at `location` --
    /// the read-side counterpart of [`store`](Persistable::store).
    fn load<B: ReadBackend<Pointer = P>>(backend: &mut B, location: Location<P, B::Size>) -> Self;

    /// Builds this type's own descriptor node -- the common-case schema hook.
    /// `#[derive(Persistable)]` generates this for you; a hand-written impl
    /// returns a fresh [`TypeDescriptor`], obtaining references to its
    /// field/element/parameter types by calling
    /// [`describe`](Persistable::describe) on each of them.
    ///
    /// A type that instead wants to be **schema-transparent** -- reusing another
    /// type's descriptor rather than owning one -- overrides
    /// [`describe`](Persistable::describe) directly and leaves this method
    /// unimplemented (it is then never called). Implementing *neither* panics.
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

    /// Records this type's representation into `builder`, returning a reference
    /// to its descriptor. You usually call [`schema`](Persistable::schema) or
    /// [`fingerprint`](Persistable::fingerprint) instead.
    ///
    /// The default registers a node built from
    /// [`describe_local`](Persistable::describe_local), deduplicated by `Self`'s
    /// `TypeId` and reserving the slot before recursing so cyclic types
    /// terminate. Override it only to be schema-transparent.
    fn describe(builder: &mut SchemaBuilder) -> TypeRef
    where
        Self: 'static,
    {
        builder.describe::<P, Self>()
    }

    /// This type's full descriptor table (its schema) -- a language-neutral
    /// description of how it lays out and interprets its bytes, rooted at index 0.
    fn schema() -> TypeTable
    where
        Self: 'static,
    {
        let mut builder = SchemaBuilder::new();
        let root = <Self as Persistable<P>>::describe(&mut builder);
        builder.finish(root)
    }

    /// This type's 128-bit [schema fingerprint](Fingerprint): a compact,
    /// reproducible identity for its representation, suitable for detecting at
    /// load time whether a stored file was written with a compatible layout.
    fn fingerprint() -> Fingerprint
    where
        Self: 'static,
    {
        <Self as Persistable<P>>::schema().fingerprint()
    }
}
