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
    /// A cache of the postcard serialization currently written to that
    /// allocation (empty exactly when `pointer` is `None`). Kept in sync by
    /// `new`/`load` and every guard mutation. It earns its extra memory
    /// three ways: `store` reads the content length off it instead of
    /// re-serializing; a mutation compares the new serialization's length
    /// against it to learn whether the content grew or shrank (which
    /// decides the crash-safe op order in `PersistableBlobGuard::persist`);
    /// and an unchanged serialization is detected and skips the write
    /// entirely. See that method and `later.md` for the diffing this cache
    /// is also the groundwork for.
    bytes: Vec<u8>,
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
            bytes,
        }
    }
}

impl<T: Default> Default for PersistableBlob<T> {
    fn default() -> Self {
        PersistableBlob {
            value: T::default(),
            pointer: None,
            bytes: Vec::new(),
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
    /// `Some`: `PersistableBlobGuard`'s mutators always write a value's
    /// postcard bytes to its allocation immediately, so by the time `store`
    /// runs separately (e.g. assembling a struct field from an
    /// already-complete `PersistableBlob<T>`) the existing allocation's
    /// content is already correct -- only a fresh header needs publishing.
    /// The content length comes off the cached `bytes`, so `store` never
    /// re-serializes.
    fn store<B: Backend>(&mut self, backend: &B, location: Location) {
        match &self.pointer {
            Some(existing) => {
                write_header(backend, location, existing.index(), self.bytes.len() as u32);
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
                    bytes,
                }
            }
            None => PersistableBlob {
                value: T::default(),
                pointer: None,
                bytes: Vec::new(),
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

    /// Serializes the current in-memory `value` and writes it to the
    /// content allocation, then publishes the header -- crash-safely, and
    /// skipping the write entirely if the serialization is unchanged.
    ///
    /// The op order keeps *any* torn-journal prefix readable as either the
    /// old or the new value (spec.md's Crash Consistency), and does so
    /// relying on one property of postcard: `from_bytes` reads exactly a
    /// value's bytes and ignores any trailing slack. That lets a reader
    /// mid-mutation over-read a stale-but-complete value:
    ///
    /// - **grow**: enlarge, publish the new (longer) length while the
    ///   content is still the old (shorter) value -- an over-read there
    ///   deserializes to the *old* value -- then overwrite with the new
    ///   content (the single atomic commit point);
    /// - **shrink**: overwrite the content first (a reader still on the old
    ///   longer length over-reads past the new value into stale tail bytes,
    ///   yielding the *new* value -- this write is the commit), then publish
    ///   the shorter length, then shrink;
    /// - **same length**: a single content write is the atomic commit.
    ///
    /// (If the serialization format were ever changed to one that rejects
    /// trailing bytes, the grow/shrink cases would need a `Transaction`
    /// bracket or per-`set` copy-on-write instead -- see `later.md`.)
    /// Reuses the existing allocation rather than leaking it, exactly like
    /// `PersistableVec`.
    fn persist(&mut self) {
        let new_bytes = postcard::to_allocvec(&self.inner.value)
            .expect("postcard serialization of an in-memory value should not fail");
        match &self.inner.pointer {
            Some(existing) => {
                if new_bytes == self.inner.bytes {
                    return; // content already on disk; nothing to write
                }
                let old_len = self.inner.bytes.len();
                let new_len = new_bytes.len();
                match new_len.cmp(&old_len) {
                    std::cmp::Ordering::Greater => {
                        self.backend.resize(existing, new_len);
                        write_header(
                            self.backend,
                            self.location,
                            existing.index(),
                            new_len as u32,
                        );
                        self.backend.write(existing.raw(), 0, &new_bytes);
                    }
                    std::cmp::Ordering::Less => {
                        self.backend.write(existing.raw(), 0, &new_bytes);
                        write_header(
                            self.backend,
                            self.location,
                            existing.index(),
                            new_len as u32,
                        );
                        self.backend.resize(existing, new_len);
                    }
                    std::cmp::Ordering::Equal => {
                        self.backend.write(existing.raw(), 0, &new_bytes);
                    }
                }
            }
            None => {
                // Promoting the lazy `None`/default state: allocate, fill
                // the (still unreferenced) region, then publish the header
                // last -- the same append-then-publish shape as
                // `PersistableVec::push`.
                let pointer = self.backend.alloc::<PersistableBlob<T>>(new_bytes.len());
                self.backend.write(pointer.raw(), 0, &new_bytes);
                write_header(
                    self.backend,
                    self.location,
                    pointer.index(),
                    new_bytes.len() as u32,
                );
                self.inner.pointer = Some(pointer);
            }
        }
        self.inner.bytes = new_bytes;
    }
}

impl<'s, T: Default, B: Backend> PersistableBlobGuard<'s, T, B> {
    /// Frees the existing content allocation (if any) and resets back to
    /// the lazy `None`/`T::default()` state. Publishes the empty header
    /// *before* freeing, so a torn-journal prefix leaves the value already
    /// at its default with the old region merely unreferenced (reclaimed at
    /// the next flush), never dangling.
    pub fn set_to_default(&mut self) {
        self.backend
            .write(self.location.anchor, self.location.offset, &[0u8; 8]);
        if let Some(existing) = self.inner.pointer.take() {
            self.backend.free(existing);
        }
        self.inner.value = T::default();
        self.inner.bytes.clear();
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

    #[test]
    fn set_grows_and_shrinks_the_content_and_reuses_the_allocation() {
        // A `String` payload whose serialization length actually changes,
        // exercising the grow / shrink / same-length branches of `persist`.
        let backend = MockBackend::default();
        let location = root_location(&backend);

        let mut value = PersistableBlob::new(String::from("hi"), &backend);
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

        let mut value = PersistableBlob::new(
            Rec {
                a: 1,
                b: "one".into(),
            },
            &backend,
        );
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

        let mut value = PersistableBlob::new(
            Rec {
                a: 1,
                b: "one".into(),
            },
            &backend,
        );
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

        let mut value = PersistableBlob::new(String::from("stable"), &backend);
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
        // The crash-safe op order in `persist` relies on this: a reader
        // mid-mutation over-reads a complete value plus some slack, and
        // must still deserialize to that value. If this ever fails (a
        // serialization-format change), `persist`'s grow/shrink ordering is
        // no longer safe and needs a Transaction/CoW instead.
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
