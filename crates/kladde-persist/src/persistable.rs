//! [`Persistable`]: a type that knows how to store itself into, and load itself
//! from, a backend allocation at a given [`Location`].
//!
//! Parametric over the *pointer type* `P` (default `Pointer`), **not** over the
//! size type: allocation sizes belong to the store (a container queries
//! `backend.size(ptr)` rather than storing a size inline), and offsets are
//! transient (computed at the moment of a read or write, never stored). So `P`
//! is the only width a `Persistable` type is pinned to, and `Size` flows from
//! the backend as `B::Size`.
//!
//! ## The `&self` / `&mut self` asymmetry
//!
//! [`store`](Persistable::store) and [`guard`](Persistable::guard) take a shared
//! `&B`, so a parent guard can hand the same backend to every field guard by
//! reborrow. [`load`](Persistable::load) takes `&mut B`, because a load is
//! *sequential* -- one field or element after another -- and `ReadBackend`
//! hands out a real seekable cursor rather than a copied buffer. As a free side
//! effect the borrow checker forbids loading while any guard is alive.

use kladde_schema::{Fingerprint, TypeDescriptor, TypeRef, TypeTable};
use kladde_store::{Error, Pointer, PointerRepr, ReadBackend, WriteBackend};

use crate::guard::Guard;
use crate::location::Location;
use crate::schema::SchemaBuilder;

/// A type with a fixed inline byte size that can round-trip through a backend.
///
/// A `Persistable` is a plain in-memory value; reading it is ordinary reading.
/// Mutating it in a way that is recorded goes through its [`Guard`], which
/// [`guard`](Persistable::guard) hands out. Scalars, tuples, the containers of
/// `kladde-types`, and every `#[derive(Persistable)]` type implement it.
///
/// The `P: PointerRepr` bound is what lets pointer-holding implementors
/// serialize `Option<P>` with the on-file null niche; a pointer-free type
/// simply ignores it and works at every width.
///
/// ```
/// use kladde_persist::{Location, Persistable};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let mut store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let p = store.alloc(<(u16, bool) as Persistable>::INLINE_SIZE as u32)?;
/// let at = Location::new(p.raw(), 0);
/// let mut value = (7u16, true);
/// value.store(&store, at)?;
/// store.flush()?;
/// assert_eq!(<(u16, bool)>::load(&mut store, at)?, (7, true));
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub trait Persistable<P: PointerRepr = Pointer>: Sized {
    /// The number of bytes this value occupies *inline* in its parent
    /// allocation.
    ///
    /// For a scalar it is the value's own bytes; for a derived struct, the sum
    /// of its fields'; for a derived enum, a 4-byte discriminant plus its
    /// largest variant; and for an owning type with a separate content
    /// allocation (`PersistableVec`, `PersistableString`, ...), just
    /// `P::BYTE_LEN`: the pointer alone, since the store knows the
    /// allocation's size. Having *some* fixed inline size is what makes
    /// sibling fields' offsets statically computable.
    ///
    /// [`store`](Persistable::store) writes exactly this many bytes, and
    /// [`load`](Persistable::load) reads them.
    const INLINE_SIZE: usize;

    /// The mutation-capable view onto this type. See [`Guard`].
    type Guard<'s, B: WriteBackend<Pointer = P>>: Guard<Persistable = Self, Backend = B>
    where
        Self: 's,
        B: 's;

    /// Borrows both `self` and a backend for `'s`, producing a [`Guard`]
    /// through which mutations are recorded and applied. `location` is where
    /// *this* value's own inline representation lives.
    ///
    /// ```
    /// use kladde_persist::{Location, Persistable};
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// let p = store.alloc(4)?;
    /// let mut count = 0u32;
    /// count.guard(&store, Location::new(p.raw(), 0)).set(5)?;
    /// assert_eq!(count, 5);
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    fn guard<'s, B: WriteBackend<Pointer = P>>(
        &'s mut self,
        backend: &'s B,
        location: Location<P, B::Size>,
    ) -> Self::Guard<'s, B>;

    /// Writes `self`'s current value as its inline representation at
    /// `location`, first creating and writing whatever separate content
    /// allocation it needs, so that the inline bytes, written last, publish
    /// it.
    ///
    /// Takes `&mut self`, not `&self`: a type holding its own allocation
    /// pointer may need to *learn* that pointer here -- a value built with
    /// `from_iter` holds real content but no pointer until it is first
    /// stored. With only `&self` it could still allocate and write correctly,
    /// but the caller's copy would stay stuck believing it has no allocation,
    /// breaking any guard obtained from it afterwards.
    ///
    /// Storing a value that already owns allocations writes only its
    /// pointers, so moving a value into a container copies none of its
    /// content. See [`Persistable`] for an example.
    fn store<B: WriteBackend<Pointer = P>>(
        &mut self,
        backend: &B,
        location: Location<P, B::Size>,
    ) -> Result<(), Error>;

    /// Reconstructs a value purely from what is stored at `location`, as of
    /// the backend's last flush. See [`Persistable`] for an example.
    fn load<B: ReadBackend<Pointer = P>>(
        backend: &mut B,
        location: Location<P, B::Size>,
    ) -> Result<Self, Error>;

    /// Releases every allocation this value owns, recursively. A type that
    /// owns nothing keeps the default, which does nothing.
    ///
    /// Guards call it where ownership ends: after a `set` has published the
    /// value replacing this one, or when a container deletes an element. The
    /// value must not be stored again afterwards.
    ///
    /// ```
    /// use kladde_persist::Persistable;
    /// use kladde_store::{MemoryStorage, Store};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// let mut scalar = 3i64;
    /// scalar.free(&store)?; // owns nothing: nothing to do
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    fn free<B: WriteBackend<Pointer = P>>(&mut self, backend: &B) -> Result<(), Error> {
        let _ = backend;
        Ok(())
    }

    /// Builds this type's own descriptor node -- the common-case schema hook.
    /// `#[derive(Persistable)]` generates it; a hand-written impl returns a
    /// fresh [`TypeDescriptor`] describing the bytes it actually reads and
    /// writes, obtaining references to its field, element, and parameter types
    /// by calling [`describe`](Persistable::describe) on each of them.
    ///
    /// A type that instead wants to be **schema-transparent** -- reusing
    /// another type's descriptor rather than owning one -- overrides
    /// [`describe`](Persistable::describe) directly and leaves this method
    /// unimplemented (it is then never called). Implementing *neither* panics.
    ///
    /// ```
    /// use kladde_persist::{Persistable, Primitive, SchemaBuilder, TypeDescriptor};
    ///
    /// let mut builder = SchemaBuilder::new();
    /// let d = <u32 as Persistable>::describe_local(&mut builder);
    /// assert_eq!(d, TypeDescriptor::Primitive(Primitive::U32));
    /// ```
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
    /// reference to its descriptor. You usually call
    /// [`schema`](Persistable::schema) or
    /// [`fingerprint`](Persistable::fingerprint) instead.
    ///
    /// The default registers a node built from
    /// [`describe_local`](Persistable::describe_local), deduplicated by
    /// `Self`'s `TypeId` and reserving the slot before recursing so cyclic
    /// types terminate. Override it only to be schema-transparent.
    ///
    /// ```
    /// use kladde_persist::{Persistable, SchemaBuilder};
    ///
    /// let mut builder = SchemaBuilder::new();
    /// let a = <u8 as Persistable>::describe(&mut builder);
    /// let b = <u8 as Persistable>::describe(&mut builder);
    /// assert_eq!(a, b); // one table entry per type
    /// ```
    fn describe(builder: &mut SchemaBuilder) -> TypeRef
    where
        Self: 'static,
    {
        builder.describe::<P, Self>()
    }

    /// This type's full descriptor table (its schema) -- a language-neutral
    /// description of how it lays out and interprets its bytes, rooted at
    /// index 0.
    ///
    /// ```
    /// use kladde_persist::Persistable;
    ///
    /// let table = <(u8, bool) as Persistable>::schema();
    /// assert_eq!(table.descriptors().len(), 3); // the tuple, `u8`, and `bool`
    /// ```
    fn schema() -> TypeTable
    where
        Self: 'static,
    {
        let mut builder = SchemaBuilder::new();
        let root = <Self as Persistable<P>>::describe(&mut builder);
        builder.finish(root)
    }

    /// This type's 128-bit [schema fingerprint](Fingerprint): a compact,
    /// reproducible identity for its representation, which a file records for
    /// its root type so that opening it with a different layout is refused.
    ///
    /// ```
    /// use kladde_persist::Persistable;
    ///
    /// assert_eq!(<u32 as Persistable>::fingerprint(), <u32 as Persistable>::fingerprint());
    /// assert_ne!(<u32 as Persistable>::fingerprint(), <i32 as Persistable>::fingerprint());
    /// ```
    fn fingerprint() -> Fingerprint
    where
        Self: 'static,
    {
        <Self as Persistable<P>>::schema().fingerprint()
    }
}

/// Replaces `*current` with `new`: stores `new` at `location`, which
/// publishes it, then frees what the old value owned, all in one transaction,
/// and only then updates `*current`.
///
/// This is the whole-value `set` of every generated guard and of most
/// hand-written ones. If anything fails, `*current` is left as it was.
///
/// ```
/// use kladde_persist::{replace, Location};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let p = store.alloc(8)?;
/// let mut pair = (1u32, 2u32);
/// replace(&mut pair, (3, 4), &store, Location::new(p.raw(), 0))?;
/// assert_eq!(pair, (3, 4));
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub fn replace<P, T, B>(
    current: &mut T,
    mut new: T,
    backend: &B,
    location: Location<P, B::Size>,
) -> Result<(), Error>
where
    P: PointerRepr,
    T: Persistable<P>,
    B: WriteBackend<Pointer = P>,
{
    backend.atomically(|| {
        new.store(backend, location)?;
        current.free(backend)
    })?;
    *current = new;
    Ok(())
}
