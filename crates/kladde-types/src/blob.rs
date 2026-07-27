//! [`PersistableBlob<T>`] -- a generic escape hatch for wrapping a plain, foreign
//! `T` (one with no room of its own for a cached `pointer` field, so it
//! can't safely implement [`Persistable`] directly without leaking on
//! every `store` -- see `spec.md`'s notes on `String`/derived `enum`s)
//! so it can be persisted anyway, by treating its postcard-serialized
//! bytes as an opaque content allocation.
//!
//! Gated behind this crate's `serde` Cargo feature -- this is the only
//! thing in the whole `kladde` workspace that needs `serde`/`postcard`.
//!
//! Built as a thin wrapper around [`PersistableVec<u8>`]: the wrapped
//! `value`'s postcard bytes *are* the vec's content. That vec already
//! hand-rolls (leak-free, crash-safely) every piece a blob needs -- the
//! `{ target, len }` header, the lazy-until-first-write allocation, the
//! grow/shrink/reuse ordering -- so `PersistableBlob` no longer carries any
//! of that itself. `value: T` is kept alongside purely as an in-memory
//! cache for cheap [`Deref`]/[`edit`](PersistableBlobGuard::edit) reads;
//! the vec's own bytes are the source of truth on disk.
//!
//! **Empty content <=> default value.** A never-mutated
//! `PersistableBlob::default()` holds an *empty* `serialized` vec (no
//! allocation, exactly `PersistableVec`'s lazy state), and `load` maps an
//! empty vec back to `T::default()`. This relies on the assumption that no
//! non-default value serializes to an empty byte string -- true for
//! postcard, which gives any information-carrying value at least one byte,
//! so an empty encoding implies a single-inhabitant (ZST-like) type whose
//! one value is its `Default`. It would break only for a pathological
//! hand-written `Serialize` mapping several distinct values to `[]`. This
//! is the same assumption `PersistableVec`/`PersistableHashMap` already
//! make for their own `empty <=> no allocation` states.
//!
//! Note the `T: Default` bound on `Persistable`'s own impl below (not
//! just on the separate `std::default::Default` impl): `load` has to be
//! able to reconstruct the empty case as *some* value, and
//! `Persistable::load`'s signature can't be conditional -- so every `T`
//! used with `PersistableBlob<T>` needs to be `Default`.

use crate::vec::PersistableVec;
use kladde_traits::{Backend, Guard, Location, Persistable};
use std::ops::{Deref, DerefMut};

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
    /// Wraps `value`, serializing it immediately into an (as-yet
    /// unallocated) backed byte vec. Backend-free, like
    /// `PersistableString::from`: the actual allocation happens lazily the
    /// first time this blob is `store`d or mutated through a guard.
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

/// `B` defaults to [`kladde::DefaultBackend`](../../kladde/struct.DefaultBackend.html)
/// so application code that only ever uses the default backend never has
/// to name it.
pub struct PersistableBlobGuard<'s, T, B = kladde::DefaultBackend> {
    inner: &'s mut PersistableBlob<T>,
    backend: &'s B,
    location: Location,
}

impl<'s, T: serde::Serialize, B: Backend> PersistableBlobGuard<'s, T, B> {
    /// Replaces the whole value and re-persists it. Prefer [`edit`] when
    /// changing only part of a large value.
    ///
    /// [`edit`]: Self::edit
    pub fn set(&mut self, value: T) {
        self.inner.value = value;
        self.persist();
    }

    /// Returns a short-lived handle for mutating the wrapped value *in
    /// place* (through `DerefMut<Target = T>`), re-persisting it when the
    /// handle is dropped or [`commit`](PersistableBlobEdit::commit)ted --
    /// so tweaking one nested field doesn't mean rebuilding the whole `T`
    /// to hand to [`set`](Self::set). Read-only access should go through
    /// the guard's own `Deref` instead: an edit always re-serializes on
    /// drop, whether or not anything actually changed.
    pub fn edit(&mut self) -> PersistableBlobEdit<'_, 's, T, B> {
        PersistableBlobEdit { guard: self }
    }

    /// Serializes the current in-memory `value` and writes it into the
    /// wrapped byte vec via its crash-safe bulk
    /// [`set_content`](crate::PersistableVec) -- which handles the
    /// grow/shrink/reuse/free ordering (and its residual window, closed by
    /// Step 4's `splice`) once, for every owning type, rather than blob
    /// re-deriving it.
    fn persist(&mut self) {
        let bytes = postcard::to_allocvec(&self.inner.value)
            .expect("postcard serialization of an in-memory value should not fail");
        self.inner
            .serialized
            .guard(self.backend, self.location)
            .set_content(&bytes);
    }
}

impl<'s, T: serde::Serialize + Default, B: Backend> PersistableBlobGuard<'s, T, B> {
    /// Resets the value to `T::default()`, freeing the content allocation
    /// and returning to the lazy empty state. Crash-safe (empty header
    /// published before the free) courtesy of `set_content(b"")`.
    pub fn set_to_default(&mut self) {
        self.inner.value = T::default();
        self.inner
            .serialized
            .guard(self.backend, self.location)
            .set_content(b"");
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

/// An in-place editing handle for a [`PersistableBlob`]'s wrapped value,
/// obtained from [`PersistableBlobGuard::edit`]. Deref-mutates the value
/// directly (`edit.some_field = x`); the change is re-serialized and
/// persisted when the handle is dropped or [`commit`](Self::commit)ted.
///
/// Deliberately a separate, short-lived type rather than folding the
/// behavior into the guard: the guard is often held only for reads (via
/// its `Deref`), and shouldn't re-serialize-and-write on every drop.
#[must_use = "an edit persists on drop; bind it or call .commit()"]
pub struct PersistableBlobEdit<'g, 's, T: serde::Serialize, B: Backend = kladde::DefaultBackend> {
    guard: &'g mut PersistableBlobGuard<'s, T, B>,
}

impl<'g, 's, T: serde::Serialize, B: Backend> PersistableBlobEdit<'g, 's, T, B> {
    /// Persists the edited value and consumes the handle. Equivalent to
    /// letting it drop; kept as an explicit method both to make the commit
    /// point obvious at a call site and because it will grow a `Result`
    /// return once error handling lands (today it cannot fail).
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
        // wrapped vec's `set_content`.
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
        // Now that ordering lives in `PersistableVec::set_content` and no
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
