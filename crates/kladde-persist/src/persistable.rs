//! [`Persistable`]: a type that encodes itself into the sequence, decodes
//! itself from it, and hands out a [`Guard`] that mutates its region.

use kladde_schema::{Fingerprint, TypeDescriptor, TypeRef, TypeTable};
use kladde_store::Error;

use crate::codec::{Decoder, Encoder};
use crate::region::{Backend, Region};
use crate::schema::SchemaBuilder;

/// A type that can live in a kladde file: it encodes itself into one
/// buffer, decodes itself from the sequence, and mutates its region through
/// its [`Guard`]. It never allocates, frees, or knows where it lies.
///
/// Scalars, tuples, [`Option`], [`Box`], [`Fixed`](crate::Fixed), the
/// containers of `kladde-types`, and every `#[derive(Persistable)]` type
/// implement it.
///
/// A hand-written implementation owes four things: `FIXED_SIZE` and
/// [`encoded_len`](Persistable::encoded_len) match what
/// [`encode`](Persistable::encode) writes; `encode` and
/// [`decode`](Persistable::decode) are inverses; every guard method leaves
/// its region holding the value's encoding when its transaction ends; and
/// the descriptor matches the bytes.
///
/// ```
/// use kladde_persist::Persistable;
///
/// let bytes = (300u32, true).to_bytes();
/// assert_eq!(bytes, [0xac, 0x02, 1]);
/// assert_eq!(<(u32, bool)>::from_bytes(&bytes)?, (300, true));
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub trait Persistable: Sized {
    /// `Some(n)` if every value encodes to `n` bytes: then the type's
    /// containers find their elements by multiplication and keep no index,
    /// and writing a value is always an overwrite in place.
    ///
    /// ```
    /// use kladde_persist::Persistable;
    ///
    /// assert_eq!(<f32 as Persistable>::FIXED_SIZE, Some(4));
    /// assert_eq!(<u32 as Persistable>::FIXED_SIZE, None); // a varint
    /// ```
    const FIXED_SIZE: Option<usize>;

    /// The size of the type's fixed form, which
    /// [`Fixed<Self>`](crate::Fixed) encodes it in, or `None` if it has
    /// none: a type holding text, a sequence, an opaque value without a
    /// fixed size, or itself.
    ///
    /// ```
    /// use kladde_persist::Persistable;
    ///
    /// assert_eq!(<u32 as Persistable>::FIXED_FORM_SIZE, Some(4));
    /// assert_eq!(<Option<u32> as Persistable>::FIXED_FORM_SIZE, Some(5));
    /// ```
    const FIXED_FORM_SIZE: Option<usize> = None;

    /// How many bytes [`encode`](Persistable::encode) writes for this
    /// value.
    ///
    /// Takes time independent of the value's size for every type of this
    /// workspace: containers keep their total.
    ///
    /// ```
    /// use kladde_persist::Persistable;
    ///
    /// assert_eq!(300u32.encoded_len(), 2);
    /// ```
    fn encoded_len(&self) -> usize;

    /// Appends this value's encoding to `out`.
    ///
    /// ```
    /// use kladde_persist::{Encoder, Persistable};
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
    /// use kladde_persist::{Decoder, Persistable};
    ///
    /// let mut input = Decoder::new(&[0xac, 0x02]);
    /// assert_eq!(u32::decode(&mut input)?, 300);
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    fn decode(input: &mut Decoder<'_>) -> Result<Self, Error>;

    /// Appends this value's fixed form, of
    /// [`FIXED_FORM_SIZE`](Persistable::FIXED_FORM_SIZE) bytes. Called only
    /// for a type that has one; the default panics.
    ///
    /// ```
    /// use kladde_persist::{Encoder, Persistable};
    ///
    /// let mut bytes = Vec::new();
    /// 300u32.encode_fixed(&mut Encoder::new(&mut bytes));
    /// assert_eq!(bytes, [0x2c, 0x01, 0, 0]);
    /// ```
    fn encode_fixed(&self, out: &mut Encoder<'_>) {
        let _ = out;
        panic!("{} has no fixed form", std::any::type_name::<Self>())
    }

    /// Reads this type's fixed form from the front of `input`. Called only
    /// for a type that has one; the default fails.
    fn decode_fixed(input: &mut Decoder<'_>) -> Result<Self, Error> {
        Err(input.corrupt(format!(
            "{} has no fixed form",
            std::any::type_name::<Self>()
        )))
    }

    /// The mutation-capable view onto this type, over a value whose region
    /// records through `B`. See [`Guard`].
    ///
    /// A type persisted as another, such as `Box<T>`, may use the other's
    /// guard, so the guard's [`Guard::Persistable`] need not be `Self`.
    type Guard<'r, B: Backend + 'r>: Guard
    where
        Self: 'r;

    /// Borrows `self` with its `region`, the substring of the sequence
    /// that holds its encoding, producing a [`Guard`] through which
    /// mutations are recorded and applied.
    ///
    /// ```
    /// use kladde_persist::{Persistable, Region};
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// let mut count = 5u32;
    /// store.splice(0, 0, &count.to_bytes())?;
    /// count.guard(Region::root(&store)).set(500)?;
    /// store.flush()?;
    /// assert_eq!(store.read_all()?, [0xf4, 0x03]); // the varint grew
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    fn guard<'r, B: Backend + 'r>(&'r mut self, region: Region<'r, B>) -> Self::Guard<'r, B>;

    /// This value's encoding, as a fresh buffer.
    ///
    /// ```
    /// use kladde_persist::Persistable;
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
    /// use kladde_persist::Persistable;
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

    /// Records this type's encoding into `builder`, returning a reference
    /// to its descriptor. You usually call [`schema`](Persistable::schema)
    /// or [`fingerprint`](Persistable::fingerprint) instead.
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
        builder.describe::<Self>()
    }

    /// This type's full descriptor table (its schema): a language-neutral
    /// description of its encoding, rooted at index 0.
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
        let root = <Self as Persistable>::describe(&mut builder);
        builder.finish(root)
    }

    /// This type's 128-bit [schema fingerprint](Fingerprint): a compact,
    /// reproducible identity for its encoding, which a file records for its
    /// root type so that opening it as another type is refused.
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
        <Self as Persistable>::schema().fingerprint()
    }
}

/// A live, mutation-capable view onto a persistable value and its region.
///
/// Every mutating method of a guard records the change in the store's
/// journal and then mutates the in-memory value, in that order, and returns
/// a `Result`: a failed recording leaves the value as it was. Guards also
/// dereference to the value, so read-only methods stay available.
///
/// ```
/// use kladde_persist::{Guard, Persistable, Region};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let mut flag = false;
/// store.splice(0, 0, &flag.to_bytes())?;
/// let mut guard = flag.guard(Region::root(&store));
/// guard.set(true)?;
/// assert!(*guard.as_persistable());
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub trait Guard {
    /// The value this guard mutates.
    type Persistable;

    /// The value, for reading.
    fn as_persistable(&self) -> &Self::Persistable;
}

/// The guard of a value that changes only as a whole: a scalar, a
/// [`Fixed`](crate::Fixed) value, or anything else whose
/// [`Persistable::Guard`] offers nothing but [`set`](SetGuard::set).
///
/// ```
/// use kladde_persist::{Persistable, Region};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let mut c = 'a';
/// store.splice(0, 0, &c.to_bytes())?;
/// c.guard(Region::root(&store)).set('€')?;
/// assert_eq!(c, '€');
/// store.flush()?;
/// assert_eq!(store.read_all()?, "€".as_bytes());
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub struct SetGuard<'r, T, B> {
    value: &'r mut T,
    region: Region<'r, B>,
}

impl<'r, T: Persistable, B: Backend> SetGuard<'r, T, B> {
    /// The guard of `value` at `region`.
    pub fn new(value: &'r mut T, region: Region<'r, B>) -> Self {
        SetGuard { value, region }
    }

    /// Replaces the value: an overwrite in place if the encoding keeps its
    /// size, otherwise a splice that the values around hear about. See
    /// [`SetGuard`] for an example.
    pub fn set(&mut self, value: T) -> Result<(), Error> {
        replace(self.value, value, &self.region)
    }
}

impl<T, B> Guard for SetGuard<'_, T, B> {
    type Persistable = T;

    fn as_persistable(&self) -> &T {
        self.value
    }
}

impl<T, B> std::ops::Deref for SetGuard<'_, T, B> {
    type Target = T;

    fn deref(&self) -> &T {
        self.value
    }
}

/// Replaces `*current`, whose encoding fills `region`, with `new`: writes
/// `new`'s encoding over the old one, in place if the size stays, and only
/// then updates `*current`.
///
/// This is the whole-value `set` of every generated guard and of most
/// hand-written ones. If the write fails, `*current` is left as it was.
///
/// ```
/// use kladde_persist::{replace, Persistable, Region};
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
pub fn replace<T: Persistable, B: Backend>(
    current: &mut T,
    new: T,
    region: &Region<'_, B>,
) -> Result<(), Error> {
    debug_assert_eq!(current.encoded_len() as u64, region.len());
    region.splice(0, region.len(), &new.to_bytes())?;
    *current = new;
    Ok(())
}
