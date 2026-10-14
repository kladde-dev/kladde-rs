//! [`Durable`]: a type that encodes itself into the sequence, decodes
//! itself from it, and hands out a [`Mut`] that mutates its region.

use kladde_schema::{Fingerprint, TypeDescriptor, TypeRef, TypeTable};
use kladde_store::Error;

use crate::codec::{Decoder, Encoder};
use crate::region::{Backend, Region};
use crate::schema::SchemaBuilder;

/// A type that can live in a kladde file: it encodes itself into one
/// buffer, decodes itself from the sequence, and mutates its region through
/// its [`Mut`]. It never allocates, frees, or knows where it lies.
///
/// Scalars, tuples, [`Option`], [`Box`], [`Fixed`](crate::Fixed), the
/// containers of `kladde-types`, and every `#[derive(Durable)]` type
/// implement it. A type that also has a fixed form implements [`Bounded`].
///
/// A hand-written implementation owes four things: `FIXED_SIZE` and
/// [`encoded_len`](Durable::encoded_len) match what
/// [`encode`](Durable::encode) writes; `encode` and
/// [`decode`](Durable::decode) are inverses; every [`Mut`] method leaves
/// its region holding the value's encoding when its transaction ends; and
/// the descriptor matches the bytes.
///
/// ```
/// use kladde_persist::Durable;
///
/// let bytes = (300u32, true).to_bytes();
/// assert_eq!(bytes, [0xac, 0x02, 1]);
/// assert_eq!(<(u32, bool)>::from_bytes(&bytes)?, (300, true));
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub trait Durable: Sized {
    /// `Some(n)` if every value encodes to `n` bytes: then the type's
    /// containers find their elements by multiplication and keep no index,
    /// and writing a value is always an overwrite in place.
    ///
    /// ```
    /// use kladde_persist::Durable;
    ///
    /// assert_eq!(<f32 as Durable>::FIXED_SIZE, Some(4));
    /// assert_eq!(<u32 as Durable>::FIXED_SIZE, None); // a varint
    /// ```
    const FIXED_SIZE: Option<usize>;

    /// How many bytes [`encode`](Durable::encode) writes for this
    /// value.
    ///
    /// Takes time independent of the value's size for every type of this
    /// workspace: containers keep their total.
    ///
    /// ```
    /// use kladde_persist::Durable;
    ///
    /// assert_eq!(300u32.encoded_len(), 2);
    /// ```
    fn encoded_len(&self) -> usize;

    /// Appends this value's encoding to `out`.
    ///
    /// ```
    /// use kladde_persist::{Encoder, Durable};
    ///
    /// let mut bytes = Vec::new();
    /// (-1i64).encode(&mut Encoder::new(&mut bytes));
    /// assert_eq!(bytes, [1]); // zigzag
    /// ```
    fn encode(&self, out: &mut Encoder<'_>);

    /// Reads a value from the front of `input`, advancing past it.
    ///
    /// Fails with [`Error::Corrupt`] on bytes that no value encodes to,
    /// including an encoding that is not canonical.
    ///
    /// ```
    /// use kladde_persist::{Decoder, Durable};
    ///
    /// let mut input = Decoder::new(&[0xac, 0x02]);
    /// assert_eq!(u32::decode(&mut input)?, 300);
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    fn decode(input: &mut Decoder<'_>) -> Result<Self, Error>;

    /// The mutation-capable view onto this type, over a value whose region
    /// records through `B`. See [`Mut`].
    ///
    /// A type persisted as another, such as `Box<T>`, may use the other's
    /// `Mut`, so the `Mut`'s [`Mut::Durable`] need not be `Self`.
    type Mut<'r, B: Backend + 'r>: Mut
    where
        Self: 'r;

    /// Borrows `self` with its `region`, the substring of the sequence
    /// that holds its encoding, producing a [`Mut`] through which
    /// mutations are recorded and applied.
    ///
    /// ```
    /// use kladde_persist::{Durable, Region};
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// let mut count = 5u32;
    /// store.splice(0, 0, &count.to_bytes())?;
    /// count.mutate(Region::root(&store)).set(500)?;
    /// store.flush()?;
    /// assert_eq!(store.read_all()?, [0xf4, 0x03]); // the varint grew
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    fn mutate<'r, B: Backend + 'r>(&'r mut self, region: Region<'r, B>) -> Self::Mut<'r, B>;

    /// This value's encoding, as a fresh buffer.
    ///
    /// ```
    /// use kladde_persist::Durable;
    ///
    /// assert_eq!(258u16.to_bytes(), [0x82, 0x02]);
    /// ```
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        self.encode(&mut Encoder::new(&mut out));
        out
    }

    /// The value `bytes` encodes, which must hold nothing else.
    ///
    /// ```
    /// use kladde_persist::Durable;
    ///
    /// assert_eq!(u16::from_bytes(&[0x82, 0x02])?, 258);
    /// assert!(u16::from_bytes(&[0x82, 0x02, 0]).is_err());
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let mut input = Decoder::new(bytes);
        let value = Self::decode(&mut input)?;
        if !input.is_at_end() {
            return Err(input.corrupt("bytes after the value"));
        }
        Ok(value)
    }

    /// Builds this type's own descriptor node -- the common-case schema hook.
    /// `#[derive(Durable)]` generates it; a hand-written impl returns a
    /// fresh [`TypeDescriptor`] describing the bytes it actually reads and
    /// writes, obtaining references to its field, element, and parameter types
    /// by calling [`describe`](Durable::describe) on each of them.
    ///
    /// A type that instead wants to be **schema-transparent** -- reusing
    /// another type's descriptor rather than owning one -- overrides
    /// [`describe`](Durable::describe) directly and leaves this method
    /// unimplemented (it is then never called). Implementing *neither* panics.
    ///
    /// ```
    /// use kladde_persist::{Durable, Primitive, SchemaBuilder, TypeDescriptor};
    ///
    /// let mut builder = SchemaBuilder::new();
    /// let d = <u32 as Durable>::describe_local(&mut builder);
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

    /// Records this type's encoding into `builder`, returning a reference
    /// to its descriptor. You usually call [`schema`](Durable::schema)
    /// or [`fingerprint`](Durable::fingerprint) instead.
    ///
    /// The default registers a node built from
    /// [`describe_local`](Durable::describe_local), deduplicated by
    /// `Self`'s `TypeId` and reserving the slot before recursing so cyclic
    /// types terminate. Override it only to be schema-transparent.
    ///
    /// ```
    /// use kladde_persist::{Durable, SchemaBuilder};
    ///
    /// let mut builder = SchemaBuilder::new();
    /// let a = <u8 as Durable>::describe(&mut builder);
    /// let b = <u8 as Durable>::describe(&mut builder);
    /// assert_eq!(a, b); // one table entry per type
    /// ```
    fn describe(builder: &mut SchemaBuilder) -> TypeRef
    where
        Self: 'static,
    {
        builder.describe::<Self>()
    }

    /// This type's full descriptor table (its schema): a language-neutral
    /// description of its encoding, rooted at index 0.
    ///
    /// ```
    /// use kladde_persist::Durable;
    ///
    /// let table = <(u8, bool) as Durable>::schema();
    /// assert_eq!(table.descriptors().len(), 3); // the tuple, `u8`, and `bool`
    /// ```
    fn schema() -> TypeTable
    where
        Self: 'static,
    {
        let mut builder = SchemaBuilder::new();
        let root = <Self as Durable>::describe(&mut builder);
        builder.finish(root)
    }

    /// This type's 128-bit [schema fingerprint](Fingerprint): a compact,
    /// reproducible identity for its encoding, which a file records for its
    /// root type so that opening it as another type is refused.
    ///
    /// ```
    /// use kladde_persist::Durable;
    ///
    /// assert_eq!(<u32 as Durable>::fingerprint(), <u32 as Durable>::fingerprint());
    /// assert_ne!(<u32 as Durable>::fingerprint(), <i32 as Durable>::fingerprint());
    /// ```
    fn fingerprint() -> Fingerprint
    where
        Self: 'static,
    {
        <Self as Durable>::schema().fingerprint()
    }
}

/// A [`Durable`] type with a fixed form: an encoding of the same size for
/// every value, which [`Fixed<Self>`](crate::Fixed) stores.
///
/// In the fixed form every integer takes its full width, every `char` four
/// bytes, and every enum its discriminant at a fixed width and its variant
/// padded with zeros to the largest, so that every field lies at a constant
/// offset whatever the value.
///
/// The scalars, [`Fixed`](crate::Fixed), and the [`Option`]s, [`Box`]es and
/// tuples of `Bounded` types implement it, and so does a type derived with
/// `#[kladde(bounded)]`, which every one of its fields' types must implement
/// too. Text, containers, and opaque values without a fixed size have no
/// fixed form, and neither has a type that holds itself.
///
/// A hand-written implementation owes that
/// [`encode_fixed`](Bounded::encode_fixed) writes exactly
/// [`FIXED_FORM_SIZE`](Bounded::FIXED_FORM_SIZE) bytes, that
/// [`decode_fixed`](Bounded::decode_fixed) is its inverse, and that its
/// descriptor's fixed form is these bytes.
///
/// ```
/// use kladde_persist::Bounded;
///
/// assert_eq!(<u32 as Bounded>::FIXED_FORM_SIZE, 4);
/// assert_eq!(<Option<u32> as Bounded>::FIXED_FORM_SIZE, 5);
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` has no fixed form",
    label = "`Fixed` and `#[kladde(bounded)]` need a fixed form here",
    note = "the scalars, `Fixed<T>`, and the `Option`s, `Box`es and tuples of types with a fixed form \
            have one, and so has a derived type marked `#[kladde(bounded)]`, whose fields' \
            types must all have one"
)]
pub trait Bounded: Durable {
    /// The size of every value's fixed form.
    ///
    /// See [`Bounded`] for an example.
    const FIXED_FORM_SIZE: usize;

    /// Appends this value's fixed form, of
    /// [`FIXED_FORM_SIZE`](Bounded::FIXED_FORM_SIZE) bytes.
    ///
    /// ```
    /// use kladde_persist::{Bounded, Encoder};
    ///
    /// let mut bytes = Vec::new();
    /// 300u32.encode_fixed(&mut Encoder::new(&mut bytes));
    /// assert_eq!(bytes, [0x2c, 0x01, 0, 0]);
    /// ```
    fn encode_fixed(&self, out: &mut Encoder<'_>);

    /// Reads a value's fixed form from the front of `input`, advancing past
    /// it.
    ///
    /// Fails with [`Error::Corrupt`] on bytes that no value's fixed form is,
    /// including padding that is not zero.
    ///
    /// ```
    /// use kladde_persist::{Bounded, Decoder};
    ///
    /// let mut input = Decoder::new(&[0x2c, 0x01, 0, 0]);
    /// assert_eq!(u32::decode_fixed(&mut input)?, 300);
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    fn decode_fixed(input: &mut Decoder<'_>) -> Result<Self, Error>;
}

/// A live, mutation-capable view onto a durable value and its region.
///
/// Every mutating method of a `Mut` records the change in the store's
/// journal and then mutates the in-memory value, in that order, and returns
/// a `Result`: a failed recording leaves the value as it was. A `Mut` also
/// dereferences to the value, so read-only methods stay available.
///
/// ```
/// use kladde_persist::{Mut, Durable, Region};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let mut flag = false;
/// store.splice(0, 0, &flag.to_bytes())?;
/// let mut flag_mut = flag.mutate(Region::root(&store));
/// flag_mut.set(true)?;
/// assert!(*flag_mut.as_durable());
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub trait Mut {
    /// The value this `Mut` mutates.
    type Durable;

    /// The value, for reading.
    fn as_durable(&self) -> &Self::Durable;
}

/// The `Mut` of a value that changes only as a whole: a scalar, a
/// [`Fixed`](crate::Fixed) value, or anything else whose
/// [`Durable::Mut`] offers nothing but [`set`](SetMut::set).
///
/// ```
/// use kladde_persist::{Durable, Region};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let mut c = 'a';
/// store.splice(0, 0, &c.to_bytes())?;
/// c.mutate(Region::root(&store)).set('€')?;
/// assert_eq!(c, '€');
/// store.flush()?;
/// assert_eq!(store.read_all()?, "€".as_bytes());
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub struct SetMut<'r, T, B> {
    value: &'r mut T,
    region: Region<'r, B>,
}

impl<'r, T: Durable, B: Backend> SetMut<'r, T, B> {
    /// The `Mut` of `value` at `region`.
    pub fn new(value: &'r mut T, region: Region<'r, B>) -> Self {
        SetMut { value, region }
    }

    /// Replaces the value: an overwrite in place if the encoding keeps its
    /// size, otherwise a splice that the values around hear about. See
    /// [`SetMut`] for an example.
    pub fn set(&mut self, value: T) -> Result<(), Error> {
        replace(self.value, value, &self.region)
    }
}

impl<T, B> Mut for SetMut<'_, T, B> {
    type Durable = T;

    fn as_durable(&self) -> &T {
        self.value
    }
}

impl<T, B> std::ops::Deref for SetMut<'_, T, B> {
    type Target = T;

    fn deref(&self) -> &T {
        self.value
    }
}

/// Replaces `*current`, whose encoding fills `region`, with `new`: writes
/// `new`'s encoding over the old one, in place if the size stays, and only
/// then updates `*current`.
///
/// This is the whole-value `set` of every generated `Mut` and of most
/// hand-written ones. If the write fails, `*current` is left as it was.
///
/// ```
/// use kladde_persist::{replace, Durable, Region};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let mut pair = (1u32, 2u32);
/// store.splice(0, 0, &pair.to_bytes())?;
/// replace(&mut pair, (3, 400), &Region::root(&store))?;
/// assert_eq!(pair, (3, 400));
/// assert_eq!(store.len(), 3);
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub fn replace<T: Durable, B: Backend>(
    current: &mut T,
    new: T,
    region: &Region<'_, B>,
) -> Result<(), Error> {
    debug_assert_eq!(current.encoded_len() as u64, region.len());
    region.splice(0, region.len(), &new.to_bytes())?;
    *current = new;
    Ok(())
}
