//! [`PersistableBlob<T>`] -- a generic escape hatch for wrapping a plain, foreign
//! `T` (one with no room of its own for a cached `pointer` field, so it
//! can't safely implement [`Persistable`] directly without leaking on
//! every `store` -- see `spec.md`'s notes on `String`/derived `enum`s)
//! so it can be persisted anyway, by treating its postcard-serialized
//! bytes as an opaque content allocation and keeping the allocation
//! identity here instead.
//!
//! Gated behind this crate's `serde` Cargo feature -- this is the only
//! thing in the whole `kladde` workspace that needs `serde`/`postcard`.
//!
//! Two ways to construct one:
//! - [`PersistableBlob::new`] -- eager: always allocates immediately, so
//!   `pointer` is always `Some`. The general-purpose constructor.
//! - `PersistableBlob::default()`, via `impl<T: Default> Default for
//!   PersistableBlob<T>` -- lazy: `pointer` starts `None`, paired with the
//!   invariant `value == T::default()`. This carries no leak risk, for
//!   the same reason `PersistableVec::new()`/`PersistableHashMap::new()`
//!   don't: a freshly-defaulted, never-mutated value legitimately has
//!   nothing to allocate yet. [`PersistableBlobGuard::set`] promotes
//!   `None -> Some` the first time it's actually mutated;
//!   [`PersistableBlobGuard::set_to_default`] explicitly frees whatever
//!   allocation existed and resets back to the lazy `None` state.
//!
//! Note the `T: Default` bound on `Persistable`'s own impl below (not
//! just on the separate `std::default::Default` impl): `load` has to be
//! able to reconstruct the lazy (`pointer: None`) case somehow, and
//! `Persistable::load`'s signature can't be conditional on which
//! constructor originally produced the value -- so every `T` used with
//! `PersistableBlob<T>` needs to be `Default`, even if an application only
//! ever calls `new()` and never touches the lazy path.

use kladde_traits::{
    read_header, write_header, Backend, Guard, Location, Persistable, UniquePointer,
};
use std::ops::{Deref, DerefMut};

#[derive(Debug, PartialEq)]
pub struct PersistableBlob<T> {
    value: T,
    /// The content allocation holding `value`'s postcard-serialized bytes
    /// -- `None` only while `value` is still exactly `T::default()`,
    /// courtesy of the `Default` impl below. See the module doc comment.
    pointer: Option<UniquePointer<PersistableBlob<T>>>,
}

impl<T: serde::Serialize> PersistableBlob<T> {
    /// Allocates immediately: writes `value`'s postcard-serialized bytes
    /// to a fresh allocation and remembers the pointer, so a later
    /// `store` can reuse (not leak) it -- see `vec.rs`'s identical
    /// reasoning for `PersistableVec`.
    pub fn new<B: Backend>(value: T, backend: &B) -> Self {
        let bytes = postcard::to_allocvec(&value)
            .expect("postcard serialization of an in-memory value should not fail");
        let pointer = backend.alloc::<PersistableBlob<T>>(bytes.len());
        backend.write(pointer.raw(), 0, &bytes);
        PersistableBlob {
            value,
            pointer: Some(pointer),
        }
    }
}

impl<T: Default> Default for PersistableBlob<T> {
    fn default() -> Self {
        PersistableBlob {
            value: T::default(),
            pointer: None,
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
    /// A fixed 8-byte `{ target, len }` header -- see `String`/
    /// `PersistableVec`'s identical layout note. `len` here is the
    /// postcard-serialized content's byte length (there's no static
    /// per-element size to derive it from, unlike `PersistableVec`).
    const INLINE_SIZE: usize = 8;

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

    /// Deliberately doesn't rewrite content when `pointer` is already
    /// `Some`: `PersistableBlobGuard::set` always writes a value's postcard
    /// bytes to its allocation immediately, so by the time `store` runs
    /// separately (e.g. assembling a struct field from an
    /// already-complete `PersistableBlob<T>`) the existing allocation's
    /// content is already correct -- only a fresh header needs
    /// publishing, not new content. Re-serializing here is just to learn
    /// the byte length cheaply, not to write it anywhere.
    fn store<B: Backend>(&mut self, backend: &B, location: Location) {
        match &self.pointer {
            Some(existing) => {
                let bytes = postcard::to_allocvec(&self.value)
                    .expect("postcard serialization of an in-memory value should not fail");
                write_header(backend, location, existing.index(), bytes.len() as u32);
            }
            None => {
                // `pointer` is only ever `None` when this value was
                // constructed via `Default` and never mutated -- `value`
                // is exactly `T::default()` by construction, the same
                // "no allocation yet" state `PersistableVec`/
                // `PersistableHashMap` use for their own legitimately-empty
                // case.
                backend.write(location.anchor, location.offset, &[0u8; 8]);
            }
        }
    }

    fn load<B: Backend>(backend: &B, location: Location) -> Self {
        let (target, len) = read_header(backend, location);
        match target {
            Some(target) => {
                let pointer = UniquePointer::from_index(target);
                let bytes = backend.read(pointer.raw(), 0, len);
                let value = postcard::from_bytes(&bytes).expect("corrupt persisted value bytes");
                PersistableBlob {
                    value,
                    pointer: Some(pointer),
                }
            }
            None => PersistableBlob {
                value: T::default(),
                pointer: None,
            },
        }
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
    /// Replaces the whole value: allocates (first time) or resizes
    /// (reusing the existing pointer, never leaking it -- see
    /// `PersistableVec::push`'s identical pattern) the content allocation
    /// to fit, writes the new postcard bytes, then publishes the updated
    /// header. Promotes `pointer` from `None` to `Some` the first time
    /// this is called on a `Default`-constructed value.
    pub fn set(&mut self, value: T) {
        let bytes = postcard::to_allocvec(&value)
            .expect("postcard serialization of an in-memory value should not fail");
        match &self.inner.pointer {
            Some(existing) => self.backend.resize(existing, bytes.len()),
            None => {
                self.inner.pointer = Some(self.backend.alloc::<PersistableBlob<T>>(bytes.len()))
            }
        }
        let pointer = self.inner.pointer.as_ref().unwrap();
        self.backend.write(pointer.raw(), 0, &bytes);
        write_header(
            self.backend,
            self.location,
            pointer.index(),
            bytes.len() as u32,
        );
        self.inner.value = value;
    }
}

impl<'s, T: Default, B: Backend> PersistableBlobGuard<'s, T, B> {
    /// Frees the existing content allocation (if any) and resets back to
    /// the lazy `None`/`T::default()` state, publishing the empty header
    /// immediately -- the explicit counterpart to `Default`'s implicit
    /// lazy construction.
    pub fn set_to_default(&mut self) {
        if let Some(existing) = self.inner.pointer.take() {
            self.backend.free(existing);
        }
        self.inner.value = T::default();
        self.backend
            .write(self.location.anchor, self.location.offset, &[0u8; 8]);
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

        let mut value = PersistableBlob::new(42i32, &backend);
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

        let mut value = PersistableBlob::new(1i32, &backend);
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

        let mut value = PersistableBlob::new(5i32, &backend);
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
}
