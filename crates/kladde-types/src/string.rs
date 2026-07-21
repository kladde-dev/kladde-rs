//! [`PersistedString`] -- the backed variant of `String`, built as a thin
//! wrapper around [`PersistedVec<u8>`] rather than its own from-scratch
//! `Persistable` implementation. Unlike `Persisted<T>` (which needs
//! `serde`/`postcard` and is gated behind this crate's `serde` feature),
//! `PersistedString` needs neither -- `u8` is already `Persistable` via
//! the scalar blanket impls in `kladde-traits`, so wrapping
//! `PersistedVec<u8>` is enough. It also carries none of `Persisted<T>`'s
//! "always has real content" leak risk: an empty `PersistedString` is
//! exactly an empty `PersistedVec<u8>`, which is already the safe, lazy,
//! no-allocation-yet state `PersistedVec::new()` relies on.
//!
//! `PartialEq`/`Eq`/`Hash`/`Ord` are hand-written rather than derived
//! through `PersistedVec`'s own (pointer-inclusive) derive: two
//! `PersistedString`s with equal text but different allocation states
//! (e.g. one freshly constructed, one just loaded from disk) should
//! compare equal -- string *content* is the only thing that should ever
//! matter here, which is also what makes it safe to use as a
//! `PersistedHashMap` key.

use crate::vec::PersistedVec;
use kladde_traits::{Backend, Guard, Location, Persistable};
use std::cmp::Ordering;
use std::hash::{Hash, Hasher};
use std::ops::Deref;

#[derive(Debug, Default)]
pub struct PersistedString(PersistedVec<u8>);

impl PersistedString {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Deref for PersistedString {
    type Target = str;
    fn deref(&self) -> &str {
        std::str::from_utf8(self.0.as_slice())
            .expect("PersistedString should always hold valid UTF-8")
    }
}

impl std::fmt::Display for PersistedString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&**self, f)
    }
}

impl From<&str> for PersistedString {
    fn from(s: &str) -> Self {
        PersistedString(PersistedVec::from_iter(s.bytes()))
    }
}

impl From<String> for PersistedString {
    fn from(s: String) -> Self {
        PersistedString::from(s.as_str())
    }
}

impl From<PersistedString> for String {
    fn from(s: PersistedString) -> Self {
        String::from_utf8(s.0.into_data()).expect("PersistedString should always hold valid UTF-8")
    }
}

impl PartialEq for PersistedString {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}
impl Eq for PersistedString {}

impl Hash for PersistedString {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (**self).hash(state)
    }
}

impl PartialOrd for PersistedString {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for PersistedString {
    fn cmp(&self, other: &Self) -> Ordering {
        (**self).cmp(&**other)
    }
}

impl PartialEq<str> for PersistedString {
    fn eq(&self, other: &str) -> bool {
        &**self == other
    }
}
impl PartialEq<PersistedString> for str {
    fn eq(&self, other: &PersistedString) -> bool {
        self == &**other
    }
}
impl PartialEq<&str> for PersistedString {
    fn eq(&self, other: &&str) -> bool {
        &**self == *other
    }
}
impl PartialEq<PersistedString> for &str {
    fn eq(&self, other: &PersistedString) -> bool {
        *self == &**other
    }
}

impl Persistable for PersistedString {
    const INLINE_SIZE: usize = <PersistedVec<u8> as Persistable>::INLINE_SIZE;

    type Guard<'s, B: Backend>
        = PersistedStringGuard<'s, B>
    where
        Self: 's,
        B: 's;

    fn guard<'s, B: Backend>(
        &'s mut self,
        backend: &'s B,
        location: Location,
    ) -> Self::Guard<'s, B> {
        PersistedStringGuard {
            inner: self,
            backend,
            location,
        }
    }

    fn store<B: Backend>(&mut self, backend: &B, location: Location) {
        self.0.store(backend, location);
    }

    fn load<B: Backend>(backend: &B, location: Location) -> Self {
        PersistedString(PersistedVec::load(backend, location))
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
pub struct PersistedStringGuard<'s, B = kladde::DefaultBackend> {
    inner: &'s mut PersistedString,
    backend: &'s B,
    location: Location,
}

impl<'s, B: Backend> PersistedStringGuard<'s, B> {
    /// Appends `s`, one byte at a time -- see this crate's `later.md` for
    /// a bulk, single-write append/replace primitive as a possible future
    /// optimization.
    pub fn push_str(&mut self, s: &str) {
        let mut guard = self.inner.0.guard(self.backend, self.location);
        for byte in s.bytes() {
            guard.push(byte);
        }
    }

    /// Replaces the whole content with `new`: clears the existing bytes
    /// (popping from the end, which -- unlike popping from the front --
    /// never triggers `PersistedVec::remove`'s tail-shift, so this is
    /// `O(old_len)`, not `O(old_len^2)`) then appends `new`. Not a single
    /// atomic write -- see `push_str`'s note.
    pub fn set(&mut self, new: impl AsRef<str>) {
        {
            let mut guard = self.inner.0.guard(self.backend, self.location);
            while !guard.is_empty() {
                let last = guard.len() - 1;
                guard.remove(last);
            }
        }
        self.push_str(new.as_ref());
    }
}

impl<'s, B: Backend> Guard for PersistedStringGuard<'s, B> {
    type Persistable = PersistedString;
    type Backend = B;

    fn as_persistable(&self) -> &PersistedString {
        self.inner
    }
    fn as_persistable_mut(&mut self) -> &mut PersistedString {
        self.inner
    }
    fn backend(&self) -> &B {
        self.backend
    }
}

impl<'s, B> Deref for PersistedStringGuard<'s, B> {
    type Target = str;
    fn deref(&self) -> &str {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockBackend;
    use crate::{PersistedHashMap, PersistedVec};
    use kladde_traits::Allocator;

    fn root_location(backend: &MockBackend) -> Location {
        let pointer = backend.alloc::<()>(PersistedString::INLINE_SIZE);
        Location {
            anchor: pointer.raw(),
            offset: 0,
        }
    }

    #[test]
    fn conversions_round_trip_through_str_and_string() {
        let s = PersistedString::from("hello");
        assert_eq!(&*s, "hello");
        assert_eq!(String::from(s), "hello".to_string());

        let owned = "world".to_string();
        let s = PersistedString::from(owned);
        assert_eq!(&*s, "world");
    }

    #[test]
    fn push_str_appends() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut value = PersistedString::new();

        let mut guard = value.guard(&backend, location);
        guard.push_str("hello");
        guard.push_str(" world");

        assert_eq!(&*value, "hello world");
    }

    #[test]
    fn set_replaces_the_whole_content() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut value = PersistedString::new();

        let mut guard = value.guard(&backend, location);
        guard.push_str("a longer string than what follows");
        guard.set("short");

        assert_eq!(&*value, "short");
    }

    #[test]
    fn flushing_and_reloading_round_trips_the_content() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut value = PersistedString::new();
        value.guard(&backend, location).push_str("persisted");

        backend.flush();

        let reloaded = PersistedString::load(&backend, location);
        assert_eq!(reloaded, value);
    }

    #[test]
    fn content_equality_ignores_allocation_state() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut value = PersistedString::new();
        value.guard(&backend, location).push_str("same text");
        backend.flush();

        let reloaded = PersistedString::load(&backend, location);
        let fresh = PersistedString::from("same text");

        assert_eq!(value, reloaded);
        assert_eq!(value, fresh);
        assert_eq!(value, "same text");
        assert_eq!("same text", value);
    }

    #[test]
    fn works_as_an_element_and_as_a_hash_map_key() {
        let backend = MockBackend::default();

        let vec_location = root_location(&backend);
        let mut names = PersistedVec::<PersistedString>::new();
        names
            .guard(&backend, vec_location)
            .push(PersistedString::from("ada"));
        assert_eq!(names.get(0).unwrap(), "ada");

        let map_pointer =
            backend.alloc::<()>(PersistedHashMap::<PersistedString, i32>::INLINE_SIZE);
        let map_location = Location {
            anchor: map_pointer.raw(),
            offset: 0,
        };
        let mut ages = PersistedHashMap::<PersistedString, i32>::new();
        ages.guard(&backend, map_location)
            .insert(PersistedString::from("ada"), 36);

        assert_eq!(ages.get(&PersistedString::from("ada")), Some(&36));
    }
}
