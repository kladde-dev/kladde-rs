//! [`PersistableString`] -- the backed variant of `String`, built as a thin
//! wrapper around [`PersistableVec<u8>`] rather than its own from-scratch
//! `Persistable` implementation. Unlike `PersistableBlob<T>` (which needs
//! `serde`/`postcard` and is gated behind this crate's `serde` feature),
//! `PersistableString` needs neither -- `u8` is already `Persistable` via
//! the scalar blanket impls in `kladde-traits`, so wrapping
//! `PersistableVec<u8>` is enough. It also carries none of `PersistableBlob<T>`'s
//! "always has real content" leak risk: an empty `PersistableString` is
//! exactly an empty `PersistableVec<u8>`, which is already the safe, lazy,
//! no-allocation-yet state `PersistableVec::new()` relies on.
//!
//! `PartialEq`/`Eq`/`Hash`/`Ord` are hand-written rather than derived
//! through `PersistableVec`'s own (pointer-inclusive) derive: two
//! `PersistableString`s with equal text but different allocation states
//! (e.g. one freshly constructed, one just loaded from disk) should
//! compare equal -- string *content* is the only thing that should ever
//! matter here, which is also what makes it safe to use as a
//! `PersistableHashMap` key.

use crate::vec::PersistableVec;
use kladde_traits::{Backend, Guard, Location, Persistable};
use std::cmp::Ordering;
use std::hash::{Hash, Hasher};
use std::ops::Deref;

/// `String` itself deliberately has no `Persistable` impl at all (see
/// `kladde-traits/src/scalar.rs`) -- it has no room to cache an
/// allocation pointer, so every `store` would have to allocate fresh and
/// leak the previous one. This is what enforces using `PersistableString`
/// for backed text fields: a struct field still typed as plain `String`
/// simply fails to compile under `#[derive(Persistable)]`.
///
/// ```compile_fail
/// use kladde_types::Persistable;
///
/// #[derive(Persistable)]
/// struct Contact {
///     name: String, // error[E0277]: the trait bound `String: Persistable` is not satisfied
/// }
/// ```
///
/// Swapping in `PersistableString` fixes the exact same struct:
///
/// ```
/// use kladde_types::{Persistable, PersistableString};
///
/// #[derive(Persistable)]
/// struct Contact {
///     name: PersistableString,
/// }
/// ```
#[derive(Debug, Default)]
pub struct PersistableString(PersistableVec<u8>);

impl PersistableString {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Deref for PersistableString {
    type Target = str;
    fn deref(&self) -> &str {
        std::str::from_utf8(self.0.as_slice())
            .expect("PersistableString should always hold valid UTF-8")
    }
}

impl std::fmt::Display for PersistableString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&**self, f)
    }
}

impl From<&str> for PersistableString {
    fn from(s: &str) -> Self {
        PersistableString(PersistableVec::from_iter(s.bytes()))
    }
}

impl From<String> for PersistableString {
    fn from(s: String) -> Self {
        PersistableString::from(s.as_str())
    }
}

impl From<PersistableString> for String {
    fn from(s: PersistableString) -> Self {
        String::from_utf8(s.0.into_data())
            .expect("PersistableString should always hold valid UTF-8")
    }
}

impl PartialEq for PersistableString {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}
impl Eq for PersistableString {}

impl Hash for PersistableString {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (**self).hash(state)
    }
}

impl PartialOrd for PersistableString {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for PersistableString {
    fn cmp(&self, other: &Self) -> Ordering {
        (**self).cmp(&**other)
    }
}

impl PartialEq<str> for PersistableString {
    fn eq(&self, other: &str) -> bool {
        &**self == other
    }
}
impl PartialEq<PersistableString> for str {
    fn eq(&self, other: &PersistableString) -> bool {
        self == &**other
    }
}
impl PartialEq<&str> for PersistableString {
    fn eq(&self, other: &&str) -> bool {
        &**self == *other
    }
}
impl PartialEq<PersistableString> for &str {
    fn eq(&self, other: &PersistableString) -> bool {
        *self == &**other
    }
}

impl Persistable for PersistableString {
    const INLINE_SIZE: usize = <PersistableVec<u8> as Persistable>::INLINE_SIZE;

    type Guard<'s, B: Backend>
        = PersistableStringGuard<'s, B>
    where
        Self: 's,
        B: 's;

    fn guard<'s, B: Backend>(
        &'s mut self,
        backend: &'s B,
        location: Location,
    ) -> Self::Guard<'s, B> {
        PersistableStringGuard {
            inner: self,
            backend,
            location,
        }
    }

    fn store<B: Backend>(&mut self, backend: &B, location: Location) {
        self.0.store(backend, location);
    }

    fn load<B: Backend>(backend: &B, location: Location) -> Self {
        PersistableString(PersistableVec::load(backend, location))
    }

    fn describe_local(
        _builder: &mut kladde_traits::SchemaBuilder,
    ) -> kladde_traits::TypeDescriptor {
        kladde_traits::TypeDescriptor::Opaque {
            library_name: "kladde-types".into(),
            type_name: "PersistableString".into(),
            version: crate::library_version(),
            inline_size: 8,
            parameters: vec![],
        }
    }
}

/// `B` defaults to [`kladde::DefaultBackend`](../../kladde/struct.DefaultBackend.html)
/// so application code that only ever uses the default backend never has
/// to name it.
///
/// Deliberately implements a much smaller surface than `std::String`'s
/// own API -- just enough to be useful (append, whole-value replace).
/// Read access goes through `Deref<Target = str>`, but only ever
/// non-`mut`: giving out `&mut str` here would let callers poke
/// individual bytes and break the UTF-8 invariant `push_str`/`set`
/// maintain, so there's no `DerefMut`.
pub struct PersistableStringGuard<'s, B = kladde::DefaultBackend> {
    inner: &'s mut PersistableString,
    backend: &'s B,
    location: Location,
}

impl<'s, B: Backend> PersistableStringGuard<'s, B> {
    /// Appends `s`, one byte at a time -- see this crate's `later.md` for
    /// a bulk, single-write append/replace primitive as a possible future
    /// optimization.
    pub fn push_str(&mut self, s: &str) {
        let mut guard = self.inner.0.guard(self.backend, self.location);
        for byte in s.bytes() {
            guard.push(byte);
        }
    }

    /// Replaces the entire string with `new` in a single bulk update.
    ///
    /// Runs in `O(n)` in the new length, independent of the current length.
    /// Accepts anything convertible into a `String`; an owned `String` is
    /// consumed without copying, while a `&str` is copied.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableString;
    ///
    /// let mut db = Kladde::new(PersistableString::new());
    /// db.guard().set("hello"); // a &str is accepted directly
    /// assert_eq!(db.get().to_string(), "hello");
    ///
    /// db.guard().set(String::from("hi")); // an owned String is moved in, no copy
    /// assert_eq!(db.get().to_string(), "hi");
    /// ```
    pub fn set(&mut self, new: impl Into<String>) {
        // Non-generic inner fn: the real body compiles once per backend,
        // rather than being re-monomorphized for every `Into` argument type.
        fn inner<B: Backend>(guard: &mut PersistableStringGuard<'_, B>, new: String) {
            guard
                .inner
                .0
                .guard(guard.backend, guard.location)
                .set(new.into_bytes());
        }
        inner(self, new.into())
    }
}

impl<'s, B: Backend> Guard for PersistableStringGuard<'s, B> {
    type Persistable = PersistableString;
    type Backend = B;

    fn as_persistable(&self) -> &PersistableString {
        self.inner
    }
    fn as_persistable_mut(&mut self) -> &mut PersistableString {
        self.inner
    }
    fn backend(&self) -> &B {
        self.backend
    }
}

impl<'s, B> Deref for PersistableStringGuard<'s, B> {
    type Target = str;
    fn deref(&self) -> &str {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockBackend;
    use crate::{PersistableHashMap, PersistableVec};
    use kladde_traits::Allocator;

    fn root_location(backend: &MockBackend) -> Location {
        let pointer = backend.alloc::<()>(PersistableString::INLINE_SIZE);
        Location {
            anchor: pointer.raw(),
            offset: 0,
        }
    }

    #[test]
    fn conversions_round_trip_through_str_and_string() {
        let s = PersistableString::from("hello");
        assert_eq!(&*s, "hello");
        assert_eq!(String::from(s), "hello".to_string());

        let owned = "world".to_string();
        let s = PersistableString::from(owned);
        assert_eq!(&*s, "world");
    }

    #[test]
    fn push_str_appends() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut value = PersistableString::new();

        let mut guard = value.guard(&backend, location);
        guard.push_str("hello");
        guard.push_str(" world");

        assert_eq!(&*value, "hello world");
    }

    #[test]
    fn set_replaces_the_whole_content() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut value = PersistableString::new();

        let mut guard = value.guard(&backend, location);
        guard.push_str("a longer string than what follows");
        guard.set("short");

        assert_eq!(&*value, "short");
    }

    #[test]
    fn flushing_and_reloading_round_trips_the_content() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut value = PersistableString::new();
        value.guard(&backend, location).push_str("persisted");

        backend.flush();

        let reloaded = PersistableString::load(&backend, location);
        assert_eq!(reloaded, value);
    }

    #[test]
    fn content_equality_ignores_allocation_state() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut value = PersistableString::new();
        value.guard(&backend, location).push_str("same text");
        backend.flush();

        let reloaded = PersistableString::load(&backend, location);
        let fresh = PersistableString::from("same text");

        assert_eq!(value, reloaded);
        assert_eq!(value, fresh);
        assert_eq!(value, "same text");
        assert_eq!("same text", value);
    }

    #[test]
    fn works_as_an_element_and_as_a_hash_map_key() {
        let backend = MockBackend::default();

        let vec_location = root_location(&backend);
        let mut names = PersistableVec::<PersistableString>::new();
        names
            .guard(&backend, vec_location)
            .push(PersistableString::from("ada"));
        assert_eq!(names.get(0).unwrap(), "ada");

        let map_pointer =
            backend.alloc::<()>(PersistableHashMap::<PersistableString, i32>::INLINE_SIZE);
        let map_location = Location {
            anchor: map_pointer.raw(),
            offset: 0,
        };
        let mut ages = PersistableHashMap::<PersistableString, i32>::new();
        ages.guard(&backend, map_location)
            .insert(PersistableString::from("ada"), 36);

        assert_eq!(ages.get(&PersistableString::from("ada")), Some(&36));
    }
}
