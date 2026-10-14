//! [`DurableBlob<T>`] -- persist any `serde`-serializable value as an
//! opaque blob.
//!
//! The escape hatch for a foreign type that can get neither a derive nor a
//! hand-written `Durable`: the value is serialized with `postcard` again in
//! full on every change, and the bytes that changed are recorded. Available
//! only with this crate's `serde` feature.

use kladde_persist::{
    splice_changed, varint_len, Backend, Decoder, Durable, Encoder, Error, Mut, Region,
    SchemaBuilder, TypeDescriptor,
};
use std::ops::Deref;

/// Persists an arbitrary `serde`-serializable `T` as an opaque blob: its
/// `postcard` serialization, prefixed with its length.
///
/// Read the value through `Deref<Target = T>`; change it through a
/// [`DurableBlobMut`], with [`set`](DurableBlobMut::set) or
/// [`update`](DurableBlobMut::update).
///
/// It is deliberately the least attractive option: serialized again in full
/// on every change, and opaque to tooling -- every `DurableBlob<_>` has the
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
/// db.get_mut().set(Config { retries: 3, name: "primary".into() })?;
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

    type Mut<'r, B: Backend + 'r>
        = DurableBlobMut<'r, T, B>
    where
        Self: 'r;

    fn mutate<'r, B: Backend + 'r>(&'r mut self, region: Region<'r, B>) -> Self::Mut<'r, B> {
        DurableBlobMut {
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
/// [`update`](Self::update). Either serializes the whole value again, and
/// records only the bytes of the serialization that changed.
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::DurableBlob;
///
/// let mut db = Kladde::new(DurableBlob::new(1u32));
/// db.get_mut().set(2)?;
/// db.get_mut().update(|n| *n += 1)?;
/// assert_eq!(**db.get(), 3);
/// # Ok::<(), kladde::Error>(())
/// ```
pub struct DurableBlobMut<'r, T, B> {
    inner: &'r mut DurableBlob<T>,
    region: Region<'r, B>,
}

impl<T, B> DurableBlobMut<'_, T, B>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
    B: Backend,
{
    /// Replaces the value with `value`, recording only the bytes of its
    /// serialization that changed, and its length if that changed. See
    /// [`DurableBlobMut`] for an example.
    pub fn set(&mut self, value: T) -> Result<(), Error> {
        let new = serialize(&value);
        self.write_serialized(&new)?;
        *self.inner = DurableBlob {
            value,
            serialized: new,
        };
        Ok(())
    }

    /// Changes the value with `f`, then persists it, recording only the
    /// bytes of its serialization that changed; if persisting fails, the
    /// value is restored from its old serialization. See [`DurableBlobMut`]
    /// for an example.
    pub fn update(&mut self, f: impl FnOnce(&mut T)) -> Result<(), Error> {
        f(&mut self.inner.value);
        let new = serialize(&self.inner.value);
        match self.write_serialized(&new) {
            Ok(()) => {
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

    /// Records the change of the serialization from the one in memory to
    /// `new`: the length, if it changed, and one splice of the range between
    /// the two serializations' common prefix and suffix, in one transaction.
    fn write_serialized(&self, new: &[u8]) -> Result<(), Error> {
        let old = &self.inner.serialized;
        let old_prefix = varint_len(old.len() as u64);
        if old.len() == new.len() {
            return splice_changed(&self.region, old_prefix as u64, old, new);
        }
        let mut len = Vec::with_capacity(10);
        Encoder::new(&mut len).varint(new.len() as u64);
        self.region.transaction(|r| {
            r.splice(0, old_prefix as u64, &len)?;
            splice_changed(r, len.len() as u64, old, new)
        })
    }
}

impl<T, B> Mut for DurableBlobMut<'_, T, B> {
    type Durable = DurableBlob<T>;

    fn as_durable(&self) -> &DurableBlob<T> {
        self.inner
    }
}

impl<T, B> Deref for DurableBlobMut<'_, T, B> {
    type Target = DurableBlob<T>;

    fn deref(&self) -> &DurableBlob<T> {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use kladde::Kladde;

    use super::DurableBlob;

    /// Journal bytes that `f` appends to `k`.
    fn appended<T>(k: &mut Kladde<T>, f: impl FnOnce(&mut Kladde<T>)) -> u64
    where
        T: kladde::Durable,
    {
        let before = k.stats().journal_bytes;
        f(k);
        k.stats().journal_bytes - before
    }

    #[test]
    fn a_change_of_a_few_bytes_records_a_few_bytes() {
        let big: Vec<u8> = (0..20_000).map(|i| i as u8).collect();
        let mut k = Kladde::new(DurableBlob::new(big.clone()));
        let same_len = appended(&mut k, |k| {
            k.get_mut().update(|v| v[10_000] ^= 0xff).unwrap();
        });
        assert!(same_len < 64, "{same_len} bytes for one changed byte");
        // The length changes, and the blob's length prefix with it: still a
        // few bytes. (An appended byte would not do: `postcard` puts the
        // vector's own length first, so the one range that holds every
        // changed byte would reach from it to the end.)
        let longer = appended(&mut k, |k| k.get_mut().update(|v| v.insert(0, 7)).unwrap());
        assert!(longer < 64, "{longer} bytes for one inserted byte");
        let unchanged = appended(&mut k, |k| {
            let v = (**k.get()).clone();
            k.get_mut().set(v).unwrap();
        });
        assert_eq!(unchanged, 0);
        let mut expected = big;
        expected[10_000] ^= 0xff;
        expected.insert(0, 7);
        assert_eq!(**k.get(), expected);
        k.flush().unwrap();
        let bytes = k.store().read_all().unwrap();
        assert_eq!(
            <DurableBlob<Vec<u8>> as kladde::Durable>::from_bytes(&bytes).unwrap(),
            *k.get()
        );
    }
}
