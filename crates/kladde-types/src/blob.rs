//! [`PersistableBlob<T>`] -- persist any `serde`-serializable value as an
//! opaque blob.
//!
//! Use it to store a value whose type isn't itself one of the persistent
//! containers -- a plain enum, a `serde`-derived struct, a `Vec`, and so
//! on. The value is serialized (with `postcard`) on write and deserialized
//! on read; `T` must implement `serde::Serialize +
//! serde::de::DeserializeOwned + Default`.
//!
//! Available only when this crate's `serde` feature is enabled.
//!
//! ```
//! use kladde::Kladde;
//! use kladde_types::PersistableBlob;
//!
//! #[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug)]
//! struct Config {
//!     retries: u32,
//!     name: String,
//! }
//!
//! let mut db = Kladde::new(PersistableBlob::new(Config::default()));
//! db.guard().set(Config {
//!     retries: 3,
//!     name: "primary".into(),
//! });
//! assert_eq!(db.get().retries, 3); // read through the blob's Deref
//! ```

use crate::vec::PersistableVec;
use kladde_traits::{Backend, Guard, Location, Persistable};
use std::ops::{Deref, DerefMut};

/// Persists an arbitrary `serde`-serializable `T` as an opaque blob.
///
/// Read the wrapped value through the blob's [`Deref`] (`&*blob`, or just
/// `blob.field` / `blob.method()`); change it through a
/// [`PersistableBlobGuard`], via [`set`](PersistableBlobGuard::set) or
/// [`edit`](PersistableBlobGuard::edit). Construct one with
/// [`new`](Self::new).
///
/// `T` must implement `serde::Serialize + serde::de::DeserializeOwned +
/// Default`. Because the contents are opaque, every `PersistableBlob<_>`
/// shares one schema fingerprint regardless of `T`; prefer a purpose-built
/// persistent type when the schema needs to tell them apart.
///
/// ```
/// use kladde_types::PersistableBlob;
///
/// let blob = PersistableBlob::new(vec![1u8, 2, 3]);
/// assert_eq!(*blob, vec![1, 2, 3]); // Deref to the wrapped value
/// ```
#[derive(Debug, PartialEq)]
pub struct PersistableBlob<T> {
    /// In-memory cache of the wrapped value, for cheap `Deref`/`edit`
    /// reads. The persisted source of truth is `serialized`'s bytes.
    value: T,
    /// The wrapped value's postcard serialization, held as a backed byte
    /// vec. Empty exactly when `value == T::default()` (see the module
    /// doc comment); non-empty vecs own a content allocation, which
    /// `PersistableVec` creates/reuses/frees crash-safely.
    serialized: PersistableVec<u8>,
}

impl<T: serde::Serialize> PersistableBlob<T> {
    /// Wraps `value`, ready to be stored.
    ///
    /// Needs no backend and allocates nothing in the backing store: that
    /// happens lazily the first time the blob is written. So a blob that
    /// is only ever read costs nothing on disk.
    ///
    /// ```
    /// use kladde_types::PersistableBlob;
    ///
    /// let blob = PersistableBlob::new(42u32);
    /// assert_eq!(*blob, 42);
    /// ```
    pub fn new(value: T) -> Self {
        let bytes = postcard::to_allocvec(&value)
            .expect("postcard serialization of an in-memory value should not fail");
        PersistableBlob {
            value,
            serialized: PersistableVec::from_iter(bytes),
        }
    }
}

impl<T: Default> Default for PersistableBlob<T> {
    fn default() -> Self {
        PersistableBlob {
            value: T::default(),
            serialized: PersistableVec::new(),
        }
    }
}

impl<T> Deref for PersistableBlob<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> Persistable for PersistableBlob<T>
where
    T: serde::Serialize + serde::de::DeserializeOwned + Default,
{
    /// Delegated straight to the wrapped `PersistableVec<u8>`'s 8-byte
    /// `{ target, len }` header -- a blob *is* that vec, representationally.
    const INLINE_SIZE: usize = <PersistableVec<u8> as Persistable>::INLINE_SIZE;

    type Guard<'s, B: Backend>
        = PersistableBlobGuard<'s, T, B>
    where
        Self: 's,
        B: 's;

    fn guard<'s, B: Backend>(
        &'s mut self,
        backend: &'s B,
        location: Location,
    ) -> Self::Guard<'s, B> {
        PersistableBlobGuard {
            inner: self,
            backend,
            location,
        }
    }

    fn store<B: Backend>(&mut self, backend: &B, location: Location) {
        self.serialized.store(backend, location);
    }

    fn load<B: Backend>(backend: &B, location: Location) -> Self {
        let serialized = PersistableVec::<u8>::load(backend, location);
        let value = if serialized.is_empty() {
            // Empty content is the canonical on-disk encoding of the
            // default value -- see the module doc comment's `empty <=>
            // default` note.
            T::default()
        } else {
            postcard::from_bytes(serialized.as_slice()).expect("corrupt persisted value bytes")
        };
        PersistableBlob { value, serialized }
    }

    // `T` here is a foreign, `serde`-serialized type that is *not* itself
    // `Persistable`, so its inner structure cannot be described -- a
    // `PersistableBlob<T>` is genuinely an opaque `postcard` blob behind an 8-byte
    // header. That means every `PersistableBlob<_>` shares one fingerprint,
    // regardless of `T`; the schema cannot tell `PersistableBlob<Foo>` from
    // `PersistableBlob<Bar>`. This is a known limitation of the `serde` escape
    // hatch (and a reason to prefer a real `Persistable` type where the
    // distinction matters).
    fn describe_local(_builder: &mut kladde_traits::SchemaBuilder) -> kladde_traits::TypeDescriptor
    where
        Self: 'static,
    {
        kladde_traits::TypeDescriptor::Opaque {
            library_name: "kladde-types".into(),
            type_name: "PersistableBlob".into(),
            version: crate::library_version(),
            inline_size: 8,
            parameters: vec![],
        }
    }
}

/// A handle for reading and modifying a [`PersistableBlob`]'s value.
///
/// Read the current value through [`Deref`] to the blob; replace it
/// wholesale with [`set`](Self::set), change it in place with
/// [`edit`](Self::edit), or reset it to `T::default()` with
/// [`set_to_default`](Self::set_to_default). `B` defaults to
/// [`kladde::DefaultBackend`](../../kladde/struct.DefaultBackend.html), so
/// code using the default backend never has to name it.
pub struct PersistableBlobGuard<'s, T, B = kladde::DefaultBackend> {
    inner: &'s mut PersistableBlob<T>,
    backend: &'s B,
    location: Location,
}

impl<'s, T: serde::Serialize, B: Backend> PersistableBlobGuard<'s, T, B> {
    /// Replaces the wrapped value with `value` and persists it.
    ///
    /// To change only part of a large value, prefer [`edit`](Self::edit) so
    /// the whole `T` needn't be rebuilt.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableBlob;
    ///
    /// let mut db = Kladde::new(PersistableBlob::new(1u32));
    /// db.guard().set(2);
    /// assert_eq!(**db.get(), 2);
    /// ```
    pub fn set(&mut self, value: T) {
        self.inner.value = value;
        self.persist();
    }

    /// Returns a handle for modifying the wrapped value in place.
    ///
    /// The returned [`PersistableBlobEdit`] derefs (mutably) to `T`; mutate
    /// it and the change is persisted when that handle is dropped or
    /// [`commit`](PersistableBlobEdit::commit)ted. Use this to tweak one
    /// field of a large value without rebuilding it for [`set`](Self::set).
    /// The value is re-serialized on drop whether or not it actually
    /// changed, so use the blob's own `Deref` for read-only access.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableBlob;
    ///
    /// #[derive(serde::Serialize, serde::Deserialize, Default)]
    /// struct Point {
    ///     x: i32,
    ///     y: i32,
    /// }
    ///
    /// let mut db = Kladde::new(PersistableBlob::new(Point::default()));
    /// {
    ///     let mut guard = db.guard();
    ///     let mut edit = guard.edit();
    ///     edit.x = 5; // change one field
    /// } // persisted when `edit` drops
    /// assert_eq!(db.get().x, 5);
    /// ```
    pub fn edit(&mut self) -> PersistableBlobEdit<'_, 's, T, B> {
        PersistableBlobEdit { guard: self }
    }

    /// Serializes the current in-memory `value` and writes it into the
    /// wrapped byte vec via its crash-safe bulk `set` -- which handles the
    /// grow/shrink/reuse/free ordering once, for every owning type, rather
    /// than blob re-deriving it.
    fn persist(&mut self) {
        let bytes = postcard::to_allocvec(&self.inner.value)
            .expect("postcard serialization of an in-memory value should not fail");
        self.inner
            .serialized
            .guard(self.backend, self.location)
            .set(bytes);
    }
}

impl<'s, T: serde::Serialize + Default, B: Backend> PersistableBlobGuard<'s, T, B> {
    /// Resets the wrapped value to `T::default()`, releasing its backing
    /// allocation.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableBlob;
    ///
    /// let mut db = Kladde::new(PersistableBlob::new(99u32));
    /// db.guard().set_to_default();
    /// assert_eq!(**db.get(), 0);
    /// ```
    pub fn set_to_default(&mut self) {
        self.inner.value = T::default();
        self.inner
            .serialized
            .guard(self.backend, self.location)
            .set(Vec::new());
    }
}

impl<'s, T, B: Backend> Guard for PersistableBlobGuard<'s, T, B>
where
    T: serde::Serialize + serde::de::DeserializeOwned + Default,
{
    type Persistable = PersistableBlob<T>;
    type Backend = B;

    fn as_persistable(&self) -> &PersistableBlob<T> {
        self.inner
    }
    fn as_persistable_mut(&mut self) -> &mut PersistableBlob<T> {
        self.inner
    }
    fn backend(&self) -> &B {
        self.backend
    }
}

impl<'s, T, B> Deref for PersistableBlobGuard<'s, T, B> {
    type Target = PersistableBlob<T>;
    fn deref(&self) -> &PersistableBlob<T> {
        self.inner
    }
}

impl<'s, T, B> DerefMut for PersistableBlobGuard<'s, T, B> {
    fn deref_mut(&mut self) -> &mut PersistableBlob<T> {
        self.inner
    }
}

/// An in-place editing handle for a [`PersistableBlob`]'s value, returned
/// by [`PersistableBlobGuard::edit`].
///
/// Derefs (mutably) to the wrapped `T`, so you can mutate the value
/// directly (`edit.some_field = x`). The edited value is persisted when the
/// handle is dropped or [`commit`](Self::commit)ted -- always re-serialized
/// on drop, whether or not it changed, so reach for it only when you intend
/// to write.
#[must_use = "an edit persists on drop; bind it or call .commit()"]
pub struct PersistableBlobEdit<'g, 's, T: serde::Serialize, B: Backend = kladde::DefaultBackend> {
    guard: &'g mut PersistableBlobGuard<'s, T, B>,
}

impl<'g, 's, T: serde::Serialize, B: Backend> PersistableBlobEdit<'g, 's, T, B> {
    /// Persists the edited value and consumes the handle.
    ///
    /// Equivalent to letting the handle drop; call it to make the commit
    /// point explicit at a call site.
    pub fn commit(self) {
        // The `Drop` impl below does the persisting.
    }
}

impl<'g, 's, T: serde::Serialize, B: Backend> Deref for PersistableBlobEdit<'g, 's, T, B> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard.inner.value
    }
}

impl<'g, 's, T: serde::Serialize, B: Backend> DerefMut for PersistableBlobEdit<'g, 's, T, B> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard.inner.value
    }
}

impl<'g, 's, T: serde::Serialize, B: Backend> Drop for PersistableBlobEdit<'g, 's, T, B> {
    fn drop(&mut self) {
        self.guard.persist();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockBackend;
    use kladde_traits::Allocator;

    fn root_location(backend: &MockBackend) -> Location {
        let pointer = backend.alloc::<()>(PersistableBlob::<i32>::INLINE_SIZE);
        Location {
            anchor: pointer.raw(),
            offset: 0,
        }
    }

    #[test]
    fn new_round_trips_through_store_and_load() {
        let backend = MockBackend::default();
        let location = root_location(&backend);

        let mut value = PersistableBlob::new(42i32);
        value.store(&backend, location);
        backend.flush();

        let reloaded = PersistableBlob::<i32>::load(&backend, location);
        assert_eq!(*reloaded, 42);
    }

    #[test]
    fn default_is_lazy_and_round_trips_as_the_default_value() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        backend.flush(); // materialize the root anchor itself, unrelated to `PersistableBlob<T>`
        let live_before = backend.live_count();

        let mut value = PersistableBlob::<i32>::default();
        value.store(&backend, location);
        backend.flush();

        assert_eq!(
            backend.live_count(),
            live_before,
            "default() shouldn't allocate anything"
        );

        let reloaded = PersistableBlob::<i32>::load(&backend, location);
        assert_eq!(*reloaded, 0);
    }

    #[test]
    fn set_reuses_an_existing_allocation_instead_of_leaking_it() {
        let backend = MockBackend::default();
        let location = root_location(&backend);

        let mut value = PersistableBlob::new(1i32);
        value.store(&backend, location);
        backend.flush();
        let live_before = backend.live_count();

        value.guard(&backend, location).set(2);
        backend.flush();

        assert_eq!(
            backend.live_count(),
            live_before,
            "set() should reuse the existing allocation, not leak a second one"
        );
        assert_eq!(*value, 2);

        let reloaded = PersistableBlob::<i32>::load(&backend, location);
        assert_eq!(*reloaded, 2);
    }

    #[test]
    fn set_to_default_frees_the_allocation_and_goes_back_to_lazy() {
        let backend = MockBackend::default();
        let location = root_location(&backend);

        let mut value = PersistableBlob::new(5i32);
        value.store(&backend, location);
        backend.flush();
        let live_before = backend.live_count();
        assert!(live_before > 0);

        value.guard(&backend, location).set_to_default();
        backend.flush();

        assert_eq!(backend.live_count(), live_before - 1);
        assert_eq!(*value, 0);

        let reloaded = PersistableBlob::<i32>::load(&backend, location);
        assert_eq!(*reloaded, 0);
    }

    #[test]
    fn set_grows_and_shrinks_the_content_and_reuses_the_allocation() {
        // A `String` payload whose serialization length actually changes,
        // exercising the grow / shrink / same-length branches of the
        // wrapped vec's `set`.
        let backend = MockBackend::default();
        let location = root_location(&backend);

        let mut value = PersistableBlob::new(String::from("hi"));
        value.store(&backend, location);
        backend.flush();
        let live = backend.live_count();

        for text in ["a much longer string than before", "x", "medium length"] {
            value.guard(&backend, location).set(String::from(text));
            backend.flush();
            assert_eq!(&*value, text);
            assert_eq!(
                backend.live_count(),
                live,
                "grow/shrink must reuse the one allocation, not leak"
            );
            let reloaded = PersistableBlob::<String>::load(&backend, location);
            assert_eq!(&*reloaded, text);
        }
    }

    #[derive(serde::Serialize, serde::Deserialize, Default, Debug, PartialEq)]
    struct Rec {
        a: i32,
        b: String,
    }

    #[test]
    fn edit_persists_in_place_field_changes() {
        let backend = MockBackend::default();
        let location = root_location(&backend);

        let mut value = PersistableBlob::new(Rec {
            a: 1,
            b: "one".into(),
        });
        value.store(&backend, location);
        backend.flush();

        {
            let mut guard = value.guard(&backend, location);
            let mut edit = guard.edit();
            edit.a = 2; // mutate a single field in place, no whole-value rebuild
            edit.commit();
        }
        backend.flush();

        let reloaded = PersistableBlob::<Rec>::load(&backend, location);
        assert_eq!(reloaded.a, 2);
        assert_eq!(reloaded.b, "one");
    }

    #[test]
    fn edit_persists_on_drop_without_explicit_commit() {
        let backend = MockBackend::default();
        let location = root_location(&backend);

        let mut value = PersistableBlob::new(Rec {
            a: 1,
            b: "one".into(),
        });
        value.store(&backend, location);
        backend.flush();

        {
            let mut guard = value.guard(&backend, location);
            guard.edit().b = "two".into(); // dropped at the end of the statement
        }
        backend.flush();

        let reloaded = PersistableBlob::<Rec>::load(&backend, location);
        assert_eq!(reloaded.b, "two");
        assert_eq!(reloaded.a, 1);
    }

    #[test]
    fn setting_to_an_equal_value_is_a_no_op_but_still_correct() {
        let backend = MockBackend::default();
        let location = root_location(&backend);

        let mut value = PersistableBlob::new(String::from("stable"));
        value.store(&backend, location);
        backend.flush();
        let live = backend.live_count();

        value.guard(&backend, location).set(String::from("stable"));
        backend.flush();

        assert_eq!(backend.live_count(), live);
        let reloaded = PersistableBlob::<String>::load(&backend, location);
        assert_eq!(&*reloaded, "stable");
    }

    #[test]
    fn postcard_from_bytes_ignores_trailing_bytes() {
        // Historically the crash-safe grow/shrink ordering relied on this
        // (a mid-mutation reader over-reading a complete value plus slack).
        // Now that ordering lives in `PersistableVec`'s bulk `set` and no
        // longer leans on trailing tolerance, but the property is still
        // worth pinning: `load` reads exactly the vec's `len` bytes, so a
        // format that rejected trailing bytes would still be fine here --
        // this documents the postcard behavior either way.
        let value = Rec {
            a: 7,
            b: "hi".into(),
        };
        let mut bytes = postcard::to_allocvec(&value).unwrap();
        bytes.extend_from_slice(&[0xff, 0x00, 0x42, 0x99]); // trailing garbage
        let decoded: Rec = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, value);
    }
}
