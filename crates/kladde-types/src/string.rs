//! [`PersistableString`] -- the backed variant of `String`: a thin wrapper
//! around [`PersistableVec<u8>`] that keeps its bytes valid UTF-8.
//!
//! `PartialEq`, `Eq`, `Hash`, and `Ord` compare the text only, not the
//! allocation, so a string just loaded equals one freshly constructed with the
//! same text -- which is also what makes it usable as a `PersistableHashMap`
//! key.

use crate::vec::PersistableVec;
use kladde_persist::{
    replace, Error, Guard, Location, Persistable, Pointer, PointerRepr, ReadBackend, WriteBackend,
};
use std::cmp::Ordering;
use std::hash::{Hash, Hasher};
use std::ops::Deref;

/// Growable text whose contents are persisted.
///
/// Reads go through `Deref<Target = str>`. Mutation goes through a
/// [`PersistableStringGuard`], which offers [`set`](PersistableStringGuard::set)
/// and [`push_str`](PersistableStringGuard::push_str), each one write.
///
/// Plain `String` deliberately does not implement `Persistable`: it has
/// nowhere to remember which allocation holds its bytes, so a struct field
/// typed `String` fails to compile rather than silently persisting nothing.
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
/// ```
/// use kladde::{Kladde, Persistable};
/// use kladde_types::PersistableString;
///
/// #[derive(Persistable)]
/// struct Contact {
///     name: PersistableString,
/// }
///
/// let mut contact = Kladde::new(Contact { name: PersistableString::from("ada") });
/// contact.guard().name_mut().push_str(" lovelace")?;
/// assert_eq!(contact.get().name, "ada lovelace");
/// # Ok::<(), kladde::Error>(())
/// ```
pub struct PersistableString<P = Pointer>(PersistableVec<u8, P>);

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
    /// An empty string, holding no allocation.
    ///
    /// ```
    /// use kladde_types::PersistableString;
    ///
    /// assert_eq!(PersistableString::<kladde::Pointer>::new(), "");
    /// ```
    pub fn new() -> Self {
        Self::default()
    }
}

impl<P> Deref for PersistableString<P> {
    type Target = str;
    fn deref(&self) -> &str {
        std::str::from_utf8(self.0.as_slice()).expect("a PersistableString holds UTF-8")
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
        PersistableString(PersistableVec::from_iter(s.into_bytes()))
    }
}

impl<P> From<PersistableString<P>> for String {
    fn from(s: PersistableString<P>) -> Self {
        String::from_utf8(s.0.into_data()).expect("a PersistableString holds UTF-8")
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

    fn store<B: WriteBackend<Pointer = P>>(
        &mut self,
        backend: &B,
        location: Location<P, B::Size>,
    ) -> Result<(), Error> {
        self.0.store(backend, location)
    }

    fn load<B: ReadBackend<Pointer = P>>(
        backend: &mut B,
        location: Location<P, B::Size>,
    ) -> Result<Self, Error> {
        let bytes = PersistableVec::load(backend, location)?;
        if std::str::from_utf8(bytes.as_slice()).is_err() {
            return Err(Error::Corrupt(
                "a PersistableString holds invalid UTF-8".into(),
            ));
        }
        Ok(PersistableString(bytes))
    }

    fn free<B: WriteBackend<Pointer = P>>(&mut self, backend: &B) -> Result<(), Error> {
        self.0.free(backend)
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

/// The mutation-capable view onto a [`PersistableString`]: whole-value
/// [`set`](Self::set) and appending [`push_str`](Self::push_str), each one
/// write.
///
/// Reads go through `Deref<Target = str>`. There is no mutable access to the
/// bytes, which could break the UTF-8 the string maintains.
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::PersistableString;
///
/// let mut name = Kladde::new(PersistableString::new());
/// let mut guard = name.guard();
/// guard.set("ada")?;
/// guard.push_str(" lovelace")?;
/// assert_eq!(&*guard, "ada lovelace");
/// # Ok::<(), kladde::Error>(())
/// ```
pub struct PersistableStringGuard<'s, B: WriteBackend> {
    inner: &'s mut PersistableString<B::Pointer>,
    backend: &'s B,
    location: Location<B::Pointer, B::Size>,
}

impl<'s, B: WriteBackend> PersistableStringGuard<'s, B> {
    /// Appends `s` in one write. See [`PersistableStringGuard`] for an
    /// example.
    pub fn push_str(&mut self, s: &str) -> Result<(), Error> {
        self.inner
            .0
            .guard(self.backend, self.location)
            .extend_from_slice(s.as_bytes())
    }

    /// Replaces the text with `new` in one write, whatever the current
    /// length. An owned `String` is moved in without copying. See
    /// [`PersistableStringGuard`] for an example.
    pub fn set(&mut self, new: impl Into<String>) -> Result<(), Error> {
        self.inner
            .0
            .guard(self.backend, self.location)
            .set_bytes(new.into().into_bytes())
    }

    /// Replaces the whole value: stores `value`, then frees the old one, in
    /// one transaction. Unlike [`set`](Self::set), it can move in a string
    /// that already has an allocation without copying its bytes.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableString;
    ///
    /// let mut name = Kladde::new(PersistableString::from("a"));
    /// name.guard().replace(PersistableString::from("b"))?;
    /// assert_eq!(name.get(), "b");
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn replace(&mut self, value: PersistableString<B::Pointer>) -> Result<(), Error> {
        replace(self.inner, value, self.backend, self.location)
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
    use crate::test_support::Fixture;

    #[test]
    fn conversions_round_trip_through_str_and_string() {
        let s = PersistableString::<Pointer>::from("hello");
        assert_eq!(&*s, "hello");
        assert_eq!(String::from(s), "hello".to_string());
    }

    #[test]
    fn push_str_and_set_round_trip() {
        let mut f = Fixture::new(<PersistableString as Persistable>::INLINE_SIZE);
        let mut value = PersistableString::new();
        let mut guard = value.guard(&f.store, f.location);
        guard.push_str("hello").unwrap();
        guard.push_str(" world").unwrap();
        assert_eq!(&*value, "hello world");
        let reloaded: PersistableString = f.reload();
        assert_eq!(reloaded, value);
        value.guard(&f.store, f.location).set("short").unwrap();
        let reloaded: PersistableString = f.reload();
        assert_eq!(reloaded, "short");
    }

    #[test]
    fn equality_ignores_allocation_state() {
        let mut f = Fixture::new(<PersistableString as Persistable>::INLINE_SIZE);
        let mut value = PersistableString::new();
        value.guard(&f.store, f.location).push_str("same").unwrap();
        let reloaded: PersistableString = f.reload();
        assert_eq!(value, reloaded);
        assert_eq!(value, PersistableString::from("same"));
        assert_eq!("same", value);
    }
}
