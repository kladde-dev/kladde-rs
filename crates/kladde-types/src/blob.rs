//! [`PersistableBlob<T>`] -- persist any `serde`-serializable value as an
//! opaque blob.
//!
//! The escape hatch for a foreign type that can get neither a derive nor a
//! hand-written `Persistable`: the value is serialized with `postcard` into a
//! byte vec of its own, and rewritten in full on every change. Available only
//! with this crate's `serde` feature.

use crate::vec::PersistableVec;
use kladde_persist::{
    replace, Error, Guard, Location, Persistable, Pointer, PointerRepr, ReadBackend, WriteBackend,
};
use std::ops::Deref;

/// Persists an arbitrary `serde`-serializable `T` as an opaque blob.
///
/// Read the value through `Deref<Target = T>`; change it through a
/// [`PersistableBlobGuard`], with [`set`](PersistableBlobGuard::set) or
/// [`update`](PersistableBlobGuard::update). `T` must implement
/// `Serialize + DeserializeOwned + Default`; a blob made with `default()`
/// stores no bytes at all until it is changed.
///
/// It is deliberately the least attractive option: rewritten in full on every
/// change, and opaque to tooling -- every `PersistableBlob<_>` has the same
/// schema descriptor, whatever `T` is.
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::PersistableBlob;
///
/// #[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug)]
/// struct Config {
///     retries: u32,
///     name: String,
/// }
///
/// let mut db = Kladde::new(PersistableBlob::new(Config::default()));
/// db.guard().set(Config { retries: 3, name: "primary".into() })?;
/// assert_eq!(db.get().retries, 3);
/// # Ok::<(), kladde::Error>(())
/// ```
#[derive(Debug, PartialEq)]
pub struct PersistableBlob<T, P = Pointer> {
    /// The value, for reading.
    value: T,
    /// Its `postcard` serialization, which is what is persisted; empty for a
    /// blob made with `default()`, which stores no bytes at all.
    serialized: PersistableVec<u8, P>,
}

fn serialize<T: serde::Serialize>(value: &T) -> Vec<u8> {
    postcard::to_allocvec(value).expect("postcard serializes every in-memory value")
}

impl<T: serde::Serialize, P> PersistableBlob<T, P> {
    /// Wraps `value`. Allocates nothing until the blob is stored.
    ///
    /// ```
    /// use kladde_types::PersistableBlob;
    ///
    /// let blob: PersistableBlob<Vec<u8>> = PersistableBlob::new(vec![1, 2, 3]);
    /// assert_eq!(*blob, vec![1, 2, 3]);
    /// ```
    pub fn new(value: T) -> Self {
        let serialized = serialize(&value).into_iter().collect();
        PersistableBlob { value, serialized }
    }
}

impl<T: Default, P> Default for PersistableBlob<T, P> {
    fn default() -> Self {
        PersistableBlob {
            value: T::default(),
            serialized: PersistableVec::new(),
        }
    }
}

impl<T, P> Deref for PersistableBlob<T, P> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T, P: PointerRepr> Persistable<P> for PersistableBlob<T, P>
where
    T: serde::Serialize + serde::de::DeserializeOwned + Default,
{
    /// The byte vec's pointer: a blob *is* that vec, representationally.
    const INLINE_SIZE: usize = <PersistableVec<u8, P> as Persistable<P>>::INLINE_SIZE;

    type Guard<'s, B: WriteBackend<Pointer = P>>
        = PersistableBlobGuard<'s, T, B>
    where
        Self: 's,
        B: 's;

    #[inline]
    fn guard<'s, B: WriteBackend<Pointer = P>>(
        &'s mut self,
        backend: &'s B,
        location: Location<P, B::Size>,
    ) -> Self::Guard<'s, B> {
        PersistableBlobGuard {
            inner: self,
            backend,
            location,
        }
    }

    fn store<B: WriteBackend<Pointer = P>>(
        &mut self,
        backend: &B,
        location: Location<P, B::Size>,
    ) -> Result<(), Error> {
        self.serialized.store_bytes(backend, location)
    }

    fn load<B: ReadBackend<Pointer = P>>(
        backend: &mut B,
        location: Location<P, B::Size>,
    ) -> Result<Self, Error> {
        let serialized = <PersistableVec<u8, P> as Persistable<P>>::load(backend, location)?;
        let value = if serialized.is_empty() {
            T::default()
        } else {
            postcard::from_bytes(serialized.as_slice()).map_err(|e| {
                Error::Corrupt(format!("a PersistableBlob does not deserialize: {e}"))
            })?
        };
        Ok(PersistableBlob { value, serialized })
    }

    fn free<B: WriteBackend<Pointer = P>>(&mut self, backend: &B) -> Result<(), Error> {
        self.serialized.free(backend)
    }

    // `T` is a foreign type that is not itself `Persistable`, so its structure
    // cannot be described: every `PersistableBlob<_>` shares one descriptor.
    fn describe_local(
        _builder: &mut kladde_persist::SchemaBuilder,
    ) -> kladde_persist::TypeDescriptor
    where
        Self: 'static,
    {
        kladde_persist::TypeDescriptor::Opaque {
            library_name: "kladde-types".into(),
            type_name: "PersistableBlob".into(),
            version: crate::library_version(),
            inline_size: P::BYTE_LEN as u64,
            parameters: vec![],
        }
    }
}

/// The mutation-capable view onto a [`PersistableBlob`]: replace the value
/// with [`set`](Self::set), or change it in place with
/// [`update`](Self::update). Either rewrites the whole serialization in one
/// write.
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::PersistableBlob;
///
/// let mut db = Kladde::new(PersistableBlob::new(1u32));
/// db.guard().set(2)?;
/// db.guard().update(|n| *n += 1)?;
/// assert_eq!(**db.get(), 3);
/// # Ok::<(), kladde::Error>(())
/// ```
pub struct PersistableBlobGuard<'s, T, B: WriteBackend> {
    inner: &'s mut PersistableBlob<T, B::Pointer>,
    backend: &'s B,
    location: Location<B::Pointer, B::Size>,
}

impl<'s, T, B> PersistableBlobGuard<'s, T, B>
where
    T: serde::Serialize + serde::de::DeserializeOwned + Default,
    B: WriteBackend,
{
    /// Replaces the value with `value`. See [`PersistableBlobGuard`] for an
    /// example.
    pub fn set(&mut self, value: T) -> Result<(), Error> {
        let bytes = serialize(&value);
        self.inner
            .serialized
            .guard(self.backend, self.location)
            .set_bytes(bytes)?;
        self.inner.value = value;
        Ok(())
    }

    /// Changes the value in place with `f`, then persists it; if persisting
    /// fails, the value is restored. See [`PersistableBlobGuard`] for an
    /// example.
    pub fn update(&mut self, f: impl FnOnce(&mut T)) -> Result<(), Error> {
        f(&mut self.inner.value);
        let bytes = serialize(&self.inner.value);
        let written = self
            .inner
            .serialized
            .guard(self.backend, self.location)
            .set_bytes(bytes);
        if written.is_err() {
            // The old serialization is still in memory: recover from it.
            let old = self.inner.serialized.as_slice();
            self.inner.value = if old.is_empty() {
                T::default()
            } else {
                postcard::from_bytes(old).unwrap_or_default()
            };
        }
        written
    }

    /// Replaces the whole blob: stores `value`, then frees the old one, in one
    /// transaction.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableBlob;
    ///
    /// let mut db = Kladde::new(PersistableBlob::new(1u8));
    /// db.guard().replace(PersistableBlob::new(9))?;
    /// assert_eq!(**db.get(), 9);
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn replace(&mut self, value: PersistableBlob<T, B::Pointer>) -> Result<(), Error> {
        replace(self.inner, value, self.backend, self.location)
    }
}

impl<'s, T, B: WriteBackend> Guard for PersistableBlobGuard<'s, T, B> {
    type Persistable = PersistableBlob<T, B::Pointer>;
    type Backend = B;

    fn as_persistable(&self) -> &Self::Persistable {
        self.inner
    }
    fn as_persistable_mut(&mut self) -> &mut Self::Persistable {
        self.inner
    }
    fn backend(&self) -> &B {
        self.backend
    }
}

impl<'s, T, B: WriteBackend> Deref for PersistableBlobGuard<'s, T, B> {
    type Target = PersistableBlob<T, B::Pointer>;
    fn deref(&self) -> &Self::Target {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Fixture;

    #[derive(serde::Serialize, serde::Deserialize, Default, Debug, PartialEq)]
    struct Rec {
        a: i32,
        b: String,
    }

    #[test]
    fn values_round_trip_and_reuse_one_allocation() {
        let mut f = Fixture::new(<PersistableBlob<String> as Persistable>::INLINE_SIZE);
        let mut value = PersistableBlob::new(String::from("hi"));
        value.store(&f.store, f.location).unwrap();
        for text in ["a much longer string than before", "x", "medium"] {
            value.guard(&f.store, f.location).set(text.into()).unwrap();
            let reloaded: PersistableBlob<String> = f.reload();
            assert_eq!(&*reloaded, text);
            assert_eq!(f.store.allocations().len(), 2);
        }
    }

    #[test]
    fn the_default_value_is_stored_as_nothing() {
        let mut f = Fixture::new(<PersistableBlob<i32> as Persistable>::INLINE_SIZE);
        let mut value = PersistableBlob::<i32>::default();
        value.store(&f.store, f.location).unwrap();
        let reloaded: PersistableBlob<i32> = f.reload();
        assert_eq!(*reloaded, 0);
        assert_eq!(f.store.allocations().len(), 1, "just the root");
    }

    #[test]
    fn update_changes_one_field() {
        let mut f = Fixture::new(<PersistableBlob<Rec> as Persistable>::INLINE_SIZE);
        let mut value = PersistableBlob::new(Rec {
            a: 1,
            b: "one".into(),
        });
        value.store(&f.store, f.location).unwrap();
        value
            .guard(&f.store, f.location)
            .update(|r| r.a = 2)
            .unwrap();
        let reloaded: PersistableBlob<Rec> = f.reload();
        assert_eq!(reloaded.a, 2);
        assert_eq!(reloaded.b, "one");
    }
}
