//! [`PersistableString`] -- the backed variant of `String`, built as a thin
//! wrapper around [`PersistableVec<u8>`] rather than its own from-scratch
//! `Persistable` implementation. Unlike `PersistableBlob<T>` (which needs
//! `serde`/`postcard` and is gated behind this crate's `serde` feature),
//! `PersistableString` needs neither -- `u8` is already `Persistable` via
//! the scalar blanket impls in `kladde-persist`, so wrapping
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
use kladde_persist::{
    Guard, Location, Persistable, Pointer, PointerRepr, ReadBackend, WriteBackend,
};
use std::cmp::Ordering;
use std::hash::{Hash, Hasher};
use std::ops::Deref;

/// `String` itself deliberately has no `Persistable` impl at all (see
/// `kladde-persist/src/scalar.rs`) -- it has no room to cache an
/// allocation pointer, so every `store` would have to allocate fresh and
/// leak the previous one. This is what enforces using `PersistableString`
/// for backed text fields: a struct field still typed as plain `String`
/// simply fails to compile under `#[derive(Persistable)]`.
///
/// ```compile_fail
/// use kladde::Persistable;
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
/// use kladde::Persistable;
/// use kladde_types::PersistableString;
///
/// #[derive(Persistable)]
/// struct Contact {
///     name: PersistableString,
/// }
/// ```
pub struct PersistableString<P = Pointer>(PersistableVec<u8, P>);

// Hand-written rather than derived: `#[derive]` would attach a spurious
// `P: Debug`/`P: Default` bound to the *pointer* parameter, which is a phantom
// as far as the text content goes.
impl<P> std::fmt::Debug for PersistableString<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PersistableString").field(&&**self).finish()
    }
}

impl<P> Default for PersistableString<P> {
    fn default() -> Self {
        PersistableString(PersistableVec::new())
    }
}

impl<P> PersistableString<P> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<P> Deref for PersistableString<P> {
    type Target = str;
    fn deref(&self) -> &str {
        std::str::from_utf8(self.0.as_slice())
            .expect("PersistableString should always hold valid UTF-8")
    }
}

impl<P> std::fmt::Display for PersistableString<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&**self, f)
    }
}

impl<P> From<&str> for PersistableString<P> {
    fn from(s: &str) -> Self {
        PersistableString(PersistableVec::from_iter(s.bytes()))
    }
}

impl<P> From<String> for PersistableString<P> {
    fn from(s: String) -> Self {
        PersistableString::from(s.as_str())
    }
}

impl<P> From<PersistableString<P>> for String {
    fn from(s: PersistableString<P>) -> Self {
        String::from_utf8(s.0.into_data())
            .expect("PersistableString should always hold valid UTF-8")
    }
}

impl<P> PartialEq for PersistableString<P> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}
impl<P> Eq for PersistableString<P> {}

impl<P> Hash for PersistableString<P> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (**self).hash(state)
    }
}

impl<P> PartialOrd for PersistableString<P> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl<P> Ord for PersistableString<P> {
    fn cmp(&self, other: &Self) -> Ordering {
        (**self).cmp(&**other)
    }
}

impl<P> PartialEq<str> for PersistableString<P> {
    fn eq(&self, other: &str) -> bool {
        &**self == other
    }
}
impl<P> PartialEq<PersistableString<P>> for str {
    fn eq(&self, other: &PersistableString<P>) -> bool {
        self == &**other
    }
}
impl<P> PartialEq<&str> for PersistableString<P> {
    fn eq(&self, other: &&str) -> bool {
        &**self == *other
    }
}
impl<P> PartialEq<PersistableString<P>> for &str {
    fn eq(&self, other: &PersistableString<P>) -> bool {
        *self == &**other
    }
}

impl<P: PointerRepr> Persistable<P> for PersistableString<P> {
    const INLINE_SIZE: usize = <PersistableVec<u8, P> as Persistable<P>>::INLINE_SIZE;

    type Guard<'s, B: WriteBackend<Pointer = P>>
        = PersistableStringGuard<'s, B>
    where
        Self: 's,
        B: 's;

    #[inline]
    fn guard<'s, B: WriteBackend<Pointer = P>>(
        &'s mut self,
        backend: &'s B,
        location: Location<P, B::Size>,
    ) -> Self::Guard<'s, B> {
        PersistableStringGuard {
            inner: self,
            backend,
            location,
        }
    }

    fn store<B: WriteBackend<Pointer = P>>(&mut self, backend: &B, location: Location<P, B::Size>) {
        self.0.store(backend, location);
    }

    fn load<B: ReadBackend<Pointer = P>>(backend: &mut B, location: Location<P, B::Size>) -> Self {
        PersistableString(PersistableVec::load(backend, location))
    }

    fn describe_local(
        _builder: &mut kladde_persist::SchemaBuilder,
    ) -> kladde_persist::TypeDescriptor {
        kladde_persist::TypeDescriptor::Opaque {
            library_name: "kladde-types".into(),
            type_name: "PersistableString".into(),
            version: crate::library_version(),
            inline_size: P::BYTE_LEN as u64,
            parameters: vec![],
        }
    }
}

/// Deliberately implements a much smaller surface than `std::String`'s
/// own API -- just enough to be useful (append, whole-value replace).
/// Read access goes through `Deref<Target = str>`, but only ever
/// non-`mut`: giving out `&mut str` here would let callers poke
/// individual bytes and break the UTF-8 invariant `push_str`/`set`
/// maintain, so there's no `DerefMut`.
pub struct PersistableStringGuard<'s, B: WriteBackend> {
    inner: &'s mut PersistableString<B::Pointer>,
    backend: &'s B,
    location: Location<B::Pointer, B::Size>,
}

impl<'s, B: WriteBackend> PersistableStringGuard<'s, B> {
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
        fn inner<B: WriteBackend>(guard: &mut PersistableStringGuard<'_, B>, new: String) {
            guard
                .inner
                .0
                .guard(guard.backend, guard.location)
                .set(new.into_bytes());
        }
        inner(self, new.into())
    }
}

impl<'s, B: WriteBackend> Guard for PersistableStringGuard<'s, B> {
    type Persistable = PersistableString<B::Pointer>;
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

impl<'s, B: WriteBackend> Deref for PersistableStringGuard<'s, B> {
    type Target = str;
    fn deref(&self) -> &str {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{root_location, MockBackend};
    use crate::{PersistableHashMap, PersistableVec};

    fn root(backend: &MockBackend) -> Location<Pointer, u32> {
        root_location(backend, <PersistableString as Persistable>::INLINE_SIZE)
    }

    #[test]
    fn conversions_round_trip_through_str_and_string() {
        let s = PersistableString::<Pointer>::from("hello");
        assert_eq!(&*s, "hello");
        assert_eq!(String::from(s), "hello".to_string());

        let owned = "world".to_string();
        let s = PersistableString::<Pointer>::from(owned);
        assert_eq!(&*s, "world");
    }

    #[test]
    fn push_str_appends() {
        let backend = MockBackend::default();
        let location = root(&backend);
        let mut value = PersistableString::new();

        let mut guard = value.guard(&backend, location);
        guard.push_str("hello");
        guard.push_str(" world");

        assert_eq!(&*value, "hello world");
    }

    #[test]
    fn set_replaces_the_whole_content() {
        let backend = MockBackend::default();
        let location = root(&backend);
        let mut value = PersistableString::new();

        let mut guard = value.guard(&backend, location);
        guard.push_str("a longer string than what follows");
        guard.set("short");

        assert_eq!(&*value, "short");
    }

    #[test]
    fn reloading_round_trips_the_content() {
        let mut backend = MockBackend::default();
        let location = root(&backend);
        let mut value = PersistableString::new();
        value.guard(&backend, location).push_str("persisted");

        let reloaded = <PersistableString as Persistable>::load(&mut backend, location);
        assert_eq!(reloaded, value);
    }

    #[test]
    fn content_equality_ignores_allocation_state() {
        let mut backend = MockBackend::default();
        let location = root(&backend);
        let mut value = PersistableString::new();
        value.guard(&backend, location).push_str("same text");

        let reloaded = <PersistableString as Persistable>::load(&mut backend, location);
        let fresh = PersistableString::from("same text");

        assert_eq!(value, reloaded);
        assert_eq!(value, fresh);
        assert_eq!(value, "same text");
        assert_eq!("same text", value);
    }

    #[test]
    fn works_as_an_element_and_as_a_hash_map_key() {
        let backend = MockBackend::default();

        let vec_location = root_location(
            &backend,
            <PersistableVec<PersistableString> as Persistable>::INLINE_SIZE,
        );
        let mut names = PersistableVec::<PersistableString>::new();
        names
            .guard(&backend, vec_location)
            .push(PersistableString::from("ada"));
        assert_eq!(names.get(0).unwrap(), "ada");

        let map_location = root_location(
            &backend,
            <PersistableHashMap<PersistableString, i32> as Persistable>::INLINE_SIZE,
        );
        let mut ages = PersistableHashMap::<PersistableString, i32>::new();
        ages.guard(&backend, map_location)
            .insert(PersistableString::from("ada"), 36);

        assert_eq!(ages.get(&PersistableString::from("ada")), Some(&36));
    }
}
