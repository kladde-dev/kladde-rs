//! [`Persistable`]: a type that knows how to encode itself, in either of its
//! two encodings, and how to decode itself again.
//!
//! Parametric over the *pointer type* `P` (default `Pointer`), **not** over the
//! size type: allocation sizes belong to the store (a container queries
//! `backend.size(ptr)` rather than storing a size inline), and offsets are
//! transient (computed at the moment of a read or write, never stored). So `P`
//! is the only width a `Persistable` type is pinned to, and `Size` flows from
//! the backend as `B::Size`.
//!
//! ## Encodings
//!
//! Every method that touches bytes takes the encoding as a type parameter
//! `E`: [`Slotted`](crate::Slotted) for a type's fixed encoding,
//! [`Packed`](crate::Packed) for its packed one. A value is written by first
//! [preparing](Persistable::prepare) it -- creating the allocations it owns
//! but has not got yet, which settles every pointer -- and then
//! [encoding](Persistable::encode) it into one buffer, written with one
//! record.
//!
//! ## The `&self` / `&mut self` asymmetry
//!
//! [`guard`](Persistable::guard) takes a shared `&B`, so a parent guard can
//! hand the same backend to every field guard by reborrow.
//! [`decode`](Persistable::decode) takes `&mut B`, because a load is
//! *sequential* -- one field or element after another -- and reads the
//! allocations it follows through a real cursor. As a free side effect the
//! borrow checker forbids loading while any guard is alive.

use kladde_schema::{Fingerprint, TypeDescriptor, TypeRef, TypeTable};
use kladde_store::{Error, Pointer, PointerRepr, ReadBackend, Word, WriteBackend};

use crate::encoding::Encoding;
use crate::guard::Guard;
use crate::input::{read_allocation, Input};
use crate::location::Location;
use crate::place::{write_encoded, Place};
use crate::schema::SchemaBuilder;

/// A type that can round-trip through a backend, in a slotted or a packed
/// place.
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
/// use kladde_persist::{Location, Persistable, Slotted};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let mut store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let size = <(u16, bool) as Persistable>::SLOTTED_SIZE.unwrap();
/// let p = store.alloc(size as u32)?;
/// let at = Location::new(p.raw(), 0);
/// let mut value = (7u16, true);
/// value.store::<_, Slotted>(&store, at)?;
/// store.flush()?;
/// assert_eq!(<(u16, bool)>::load::<_, Slotted>(&mut store, at)?, (7, true));
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub trait Persistable<P: PointerRepr = Pointer>: Sized {
    /// How many bytes this type's fixed encoding takes -- the slot a slotted
    /// place reserves for it -- or `None` if it has no fixed encoding and can
    /// stand only in packed places.
    ///
    /// For a scalar it is the value's own width; for a derived struct, the
    /// sum of its fields'; for a derived enum, its discriminant plus its
    /// largest variant; and for an owning type with a separate content
    /// allocation (`PersistableVec`, `PersistableString`, ...), just
    /// `P::BYTE_LEN`: the pointer alone, since the store knows the
    /// allocation's size. A constant slot size is what makes the offsets of
    /// slotted fields static.
    ///
    /// ```
    /// use kladde_persist::Persistable;
    ///
    /// assert_eq!(<u32 as Persistable>::SLOTTED_SIZE, Some(4));
    /// ```
    const SLOTTED_SIZE: Option<usize>;

    /// How many bytes this type's packed encoding takes, if that is the same
    /// for every value: `Some` exactly for a *fixed-size* type, whose two
    /// encodings are one.
    ///
    /// ```
    /// use kladde_persist::Persistable;
    ///
    /// assert_eq!(<f32 as Persistable>::PACKED_SIZE, Some(4));
    /// assert_eq!(<u32 as Persistable>::PACKED_SIZE, None); // a varint
    /// ```
    const PACKED_SIZE: Option<usize>;

    /// The mutation-capable view onto this type, for a value in a place of
    /// encoding `E`. See [`Guard`].
    type Guard<'s, B: WriteBackend<Pointer = P>, E: Encoding>: Guard<
        Persistable = Self,
        Backend = B,
    >
    where
        Self: 's,
        B: 's;

    /// Borrows both `self` and a backend for `'s`, producing a [`Guard`]
    /// through which mutations are recorded and applied. `place` is where
    /// *this* value's encoding lives, and which encoding it is.
    ///
    /// ```
    /// use kladde_persist::{Location, Persistable, Slotted};
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// let p = store.alloc(4)?;
    /// let mut count = 0u32;
    /// count.guard(&store, Slotted::at(Location::new(p.raw(), 0))).set(5)?;
    /// assert_eq!(count, 5);
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    fn guard<'s, B: WriteBackend<Pointer = P>, E: Encoding>(
        &'s mut self,
        backend: &'s B,
        place: Place<'s, B, E>,
    ) -> Self::Guard<'s, B, E>;

    /// How many bytes [`encode`](Persistable::encode) writes for this value
    /// in encoding `E`: [`SLOTTED_SIZE`](Persistable::SLOTTED_SIZE) for
    /// [`Slotted`](crate::Slotted), and the value's own count for
    /// [`Packed`](crate::Packed).
    ///
    /// A value that owns an allocation counts its pointer as it stands, so
    /// [`prepare`](Persistable::prepare) it first if it may not have its
    /// allocation yet.
    ///
    /// ```
    /// use kladde_persist::{Packed, Persistable, Slotted};
    ///
    /// assert_eq!(<u32 as Persistable>::encoded_size::<Slotted>(&300), 4);
    /// assert_eq!(<u32 as Persistable>::encoded_size::<Packed>(&300), 2);
    /// ```
    fn encoded_size<E: Encoding>(&self) -> usize;

    /// Appends this value's encoding `E` to `out`.
    ///
    /// Writes pointers as they stand: [`prepare`](Persistable::prepare) a
    /// value first if it may own content that has no allocation yet.
    ///
    /// ```
    /// use kladde_persist::{Packed, Persistable};
    ///
    /// let mut bytes = Vec::new();
    /// <u32 as Persistable>::encode::<Packed>(&300, &mut bytes);
    /// assert_eq!(bytes, [0xac, 0x02]);
    /// ```
    fn encode<E: Encoding>(&self, out: &mut Vec<u8>);

    /// Reconstructs a value from its encoding `E` at the front of `input`,
    /// advancing past it, and following pointers into other allocations
    /// through `backend`, as of its last flush.
    ///
    /// Fails with [`Error::Corrupt`] on bytes that no value encodes to,
    /// including a packed encoding that is not canonical.
    ///
    /// ```
    /// use kladde_persist::{Input, Packed, Persistable};
    /// use kladde_store::{MemoryStorage, Store};
    ///
    /// let mut store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// let mut input = Input::new(&[0xac, 0x02]);
    /// assert_eq!(<u32 as Persistable>::decode::<_, Packed>(&mut store, &mut input)?, 300);
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    fn decode<B: ReadBackend<Pointer = P>, E: Encoding>(
        backend: &mut B,
        input: &mut Input<'_>,
    ) -> Result<Self, Error>;

    /// Creates and fills every allocation this value owns but does not have
    /// yet, recursively, so that its encoding is settled.
    ///
    /// A value built with `from_iter` holds real content but no pointer until
    /// it is first stored, and its pointer's packed encoding depends on the id
    /// its allocation gets. A value that owns nothing, or already has every
    /// allocation it owns, keeps the default, which does nothing.
    ///
    /// ```
    /// use kladde_persist::Persistable;
    /// use kladde_store::{MemoryStorage, Store};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// let mut scalar = 3i64;
    /// scalar.prepare(&store)?; // owns nothing: nothing to do
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    fn prepare<B: WriteBackend<Pointer = P>>(&mut self, backend: &B) -> Result<(), Error> {
        let _ = backend;
        Ok(())
    }

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

    /// Writes this value in encoding `E` at `location` with one write, after
    /// [preparing](Persistable::prepare) it, so that the inline bytes,
    /// written last, publish whatever content it owns.
    ///
    /// Takes `&mut self`, not `&self`: a value holding its own allocation
    /// pointer may need to *learn* that pointer here. Storing a value that
    /// already owns allocations writes only its pointers, so moving a value
    /// into a container copies none of its content. See [`Persistable`] for
    /// an example.
    fn store<B: WriteBackend<Pointer = P>, E: Encoding>(
        &mut self,
        backend: &B,
        location: Location<P, B::Size>,
    ) -> Result<(), Error> {
        self.prepare(backend)?;
        let bytes = self.to_bytes::<E>();
        backend.write(location.anchor, location.offset, &bytes)
    }

    /// Reconstructs a value from its encoding `E` at `location`, as of the
    /// backend's last flush. See [`Persistable`] for an example.
    fn load<B: ReadBackend<Pointer = P>, E: Encoding>(
        backend: &mut B,
        location: Location<P, B::Size>,
    ) -> Result<Self, Error> {
        let bytes = read_allocation(backend, location.anchor)?;
        let mut input = Input::new(&bytes);
        input.take(location.offset.to_usize())?;
        Self::decode::<B, E>(backend, &mut input)
    }

    /// This value's encoding `E`, as a fresh buffer.
    ///
    /// ```
    /// use kladde_persist::{Persistable, Slotted};
    ///
    /// assert_eq!(<u16 as Persistable>::to_bytes::<Slotted>(&258), [2, 1]);
    /// ```
    fn to_bytes<E: Encoding>(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_size::<E>());
        self.encode::<E>(&mut out);
        out
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

/// The slot size of `T`, for code that has made sure `T` stands in a slotted
/// place only if it has one. Panics for a type without a fixed encoding.
///
/// ```
/// assert_eq!(kladde_persist::slot_size::<u64, kladde_persist::Pointer>(), 8);
/// ```
pub fn slot_size<T: Persistable<P>, P: PointerRepr>() -> usize {
    match T::SLOTTED_SIZE {
        Some(size) => size,
        None => panic!(
            "{} has no fixed encoding, so it cannot stand in a slotted place",
            std::any::type_name::<T>()
        ),
    }
}

/// Replaces `*current` with `new`: prepares `new`, writes its encoding over
/// the current one at `place` -- in place if the size stays, as a splice its
/// ancestors hear about if not -- then frees what the old value owned, all in
/// one transaction, and only then updates `*current`.
///
/// This is the whole-value `set` of every generated guard and of most
/// hand-written ones. If anything fails, `*current` is left as it was.
///
/// ```
/// use kladde_persist::{replace, Location, Slotted};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let p = store.alloc(8)?;
/// let mut pair = (1u32, 2u32);
/// replace(&mut pair, (3, 4), &store, &Slotted::at(Location::new(p.raw(), 0)))?;
/// assert_eq!(pair, (3, 4));
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub fn replace<P, T, B, E>(
    current: &mut T,
    mut new: T,
    backend: &B,
    place: &Place<'_, B, E>,
) -> Result<(), Error>
where
    P: PointerRepr,
    T: Persistable<P>,
    B: WriteBackend<Pointer = P>,
    E: Encoding,
{
    backend.atomically(|| {
        new.prepare(backend)?;
        let bytes = new.to_bytes::<E>();
        write_encoded(backend, place, current.encoded_size::<E>(), &bytes)?;
        current.free(backend)
    })?;
    *current = new;
    Ok(())
}
