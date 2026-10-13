//! [`DurableBlob<T>`] -- persist any `serde`-serializable value as an
//! opaque blob.
//!
//! The escape hatch for a foreign type that can get neither a derive nor a
//! hand-written `Durable`: the value is serialized with `postcard`, and
//! written again in full on every change. Available only with this crate's
//! `serde` feature.

use kladde_persist::{
    varint_len, Backend, Decoder, Durable, Encoder, Error, Guard, Region, SchemaBuilder,
    TypeDescriptor,
};
use std::ops::Deref;

/// Persists an arbitrary `serde`-serializable `T` as an opaque blob: its
/// `postcard` serialization, prefixed with its length.
///
/// Read the value through `Deref<Target = T>`; change it through a
/// [`DurableBlobGuard`], with [`set`](DurableBlobGuard::set) or
/// [`update`](DurableBlobGuard::update).
///
/// It is deliberately the least attractive option: written again in full on
/// every change, and opaque to tooling -- every `DurableBlob<_>` has the
/// same schema descriptor, whatever `T` is.
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::DurableBlob;
///
/// #[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug)]
/// struct Config {
///     retries: u32,
///     name: String,
/// }
///
/// let mut db = Kladde::new(DurableBlob::new(Config::default()));
/// db.guard().set(Config { retries: 3, name: "primary".into() })?;
/// assert_eq!(db.get().retries, 3);
/// # Ok::<(), kladde::Error>(())
/// ```
#[derive(Debug, PartialEq)]
pub struct DurableBlob<T> {
    /// The value, for reading.
    value: T,
    /// Its `postcard` serialization, which is what is persisted.
    serialized: Vec<u8>,
}

fn serialize<T: serde::Serialize>(value: &T) -> Vec<u8> {
    postcard::to_allocvec(value).expect("postcard serializes every in-memory value")
}

impl<T: serde::Serialize> DurableBlob<T> {
    /// Wraps `value`.
    ///
    /// ```
    /// use kladde_types::DurableBlob;
    ///
    /// let blob = DurableBlob::new(vec![1u8, 2, 3]);
    /// assert_eq!(blob.len(), 3);
    /// ```
    pub fn new(value: T) -> Self {
        let serialized = serialize(&value);
        DurableBlob { value, serialized }
    }
}

impl<T: serde::Serialize + Default> Default for DurableBlob<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> Deref for DurableBlob<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> Durable for DurableBlob<T>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    const FIXED_SIZE: Option<usize> = None;

    fn encoded_len(&self) -> usize {
        varint_len(self.serialized.len() as u64) + self.serialized.len()
    }

    fn encode(&self, out: &mut Encoder<'_>) {
        out.varint(self.serialized.len() as u64);
        out.bytes(&self.serialized);
    }

    fn decode(input: &mut Decoder<'_>) -> Result<Self, Error> {
        let len = input.varint_as::<usize>()?;
        let mut serialized = Vec::new();
        input.bytes_into(len, &mut serialized)?;
        let value = postcard::from_bytes(&serialized)
            .map_err(|e| input.corrupt(format!("a blob that does not deserialize: {e}")))?;
        Ok(DurableBlob { value, serialized })
    }

    type Guard<'r, B: Backend + 'r>
        = DurableBlobGuard<'r, T, B>
    where
        Self: 'r;

    fn guard<'r, B: Backend + 'r>(&'r mut self, region: Region<'r, B>) -> Self::Guard<'r, B> {
        DurableBlobGuard {
            inner: self,
            region,
        }
    }

    // `T` is a foreign type that is not itself `Durable`, so its structure
    // cannot be described: every `DurableBlob<_>` shares one descriptor.
    fn describe_local(_builder: &mut SchemaBuilder) -> TypeDescriptor
    where
        Self: 'static,
    {
        TypeDescriptor::Opaque {
            library_name: "kladde-types".into(),
            type_name: "DurableBlob".into(),
            version: crate::library_version(),
            fixed_size: None,
            parameters: vec![],
        }
    }
}

/// The mutation-capable view onto a [`DurableBlob`]: replace the value
/// with [`set`](Self::set), or change it in place with
/// [`update`](Self::update). Either writes the whole serialization again.
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::DurableBlob;
///
/// let mut db = Kladde::new(DurableBlob::new(1u32));
/// db.guard().set(2)?;
/// db.guard().update(|n| *n += 1)?;
/// assert_eq!(**db.get(), 3);
/// # Ok::<(), kladde::Error>(())
/// ```
pub struct DurableBlobGuard<'r, T, B> {
    inner: &'r mut DurableBlob<T>,
    region: Region<'r, B>,
}

impl<T, B> DurableBlobGuard<'_, T, B>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
    B: Backend,
{
    /// Replaces the value with `value`. See [`DurableBlobGuard`] for an
    /// example.
    pub fn set(&mut self, value: T) -> Result<(), Error> {
        kladde_persist::replace(self.inner, DurableBlob::new(value), &self.region)
    }

    /// Changes the value with `f`, then persists it; if persisting fails,
    /// the value is restored from its old serialization. See
    /// [`DurableBlobGuard`] for an example.
    pub fn update(&mut self, f: impl FnOnce(&mut T)) -> Result<(), Error> {
        f(&mut self.inner.value);
        let new = serialize(&self.inner.value);
        let mut bytes = Vec::with_capacity(new.len() + 10);
        let mut out = Encoder::new(&mut bytes);
        out.varint(new.len() as u64);
        out.bytes(&new);
        match self.region.splice(0, self.region.len(), &bytes) {
            Ok(_) => {
                self.inner.serialized = new;
                Ok(())
            }
            Err(e) => {
                self.inner.value = postcard::from_bytes(&self.inner.serialized)
                    .expect("the old serialization deserializes");
                Err(e)
            }
        }
    }
}

impl<T, B> Guard for DurableBlobGuard<'_, T, B> {
    type Durable = DurableBlob<T>;

    fn as_durable(&self) -> &DurableBlob<T> {
        self.inner
    }
}

impl<T, B> Deref for DurableBlobGuard<'_, T, B> {
    type Target = DurableBlob<T>;

    fn deref(&self) -> &DurableBlob<T> {
        self.inner
    }
}
