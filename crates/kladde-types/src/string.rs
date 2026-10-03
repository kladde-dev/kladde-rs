//! [`PersistableString`] -- the backed variant of `String`: a thin wrapper
//! around [`PersistableVec<u8>`] that keeps its bytes valid UTF-8.
//!
//! `PartialEq`, `Eq`, `Hash`, and `Ord` compare the text only, not the
//! allocation, so a string just loaded equals one freshly constructed with the
//! same text -- which is also what makes it usable as a `PersistableHashMap`
//! key.

use crate::vec::PersistableVec;
use kladde_persist::{
    replace, Encoding, Error, Guard, Input, Persistable, Place, Pointer, PointerRepr, ReadBackend,
    Slotted, WriteBackend,
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
    const SLOTTED_SIZE: Option<usize> = <PersistableVec<u8, P> as Persistable<P>>::SLOTTED_SIZE;
    const PACKED_SIZE: Option<usize> = <PersistableVec<u8, P> as Persistable<P>>::PACKED_SIZE;

    type Guard<'s, B: WriteBackend<Pointer = P>, E: Encoding>
        = PersistableStringGuard<'s, B, E>
    where
        Self: 's,
        B: 's;

    #[inline]
    fn guard<'s, B: WriteBackend<Pointer = P>, E: Encoding>(
        &'s mut self,
        backend: &'s B,
        place: Place<'s, B, E>,
    ) -> Self::Guard<'s, B, E> {
        PersistableStringGuard {
            inner: self,
            backend,
            place,
        }
    }

    fn encoded_size<E: Encoding>(&self) -> usize {
        self.0.encoded_size::<E>()
    }

    fn encode<E: Encoding>(&self, out: &mut Vec<u8>) {
        self.0.encode::<E>(out)
    }

    fn decode<B: ReadBackend<Pointer = P>, E: Encoding>(
        backend: &mut B,
        input: &mut Input<'_>,
    ) -> Result<Self, Error> {
        let bytes = PersistableVec::decode::<B, E>(backend, input)?;
        if std::str::from_utf8(bytes.as_slice()).is_err() {
            return Err(Error::Corrupt(
                "a PersistableString holds invalid UTF-8".into(),
            ));
        }
        Ok(PersistableString(bytes))
    }

    fn prepare<B: WriteBackend<Pointer = P>>(&mut self, backend: &B) -> Result<(), Error> {
        self.0.prepare_bytes(backend)
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
pub struct PersistableStringGuard<'s, B: WriteBackend, E: Encoding = Slotted> {
    inner: &'s mut PersistableString<B::Pointer>,
    backend: &'s B,
    place: Place<'s, B, E>,
}

impl<'s, B: WriteBackend, E: Encoding> PersistableStringGuard<'s, B, E> {
    /// Appends `s` in one write. See [`PersistableStringGuard`] for an
    /// example.
    pub fn push_str(&mut self, s: &str) -> Result<(), Error> {
        self.inner
            .0
            .guard(self.backend, self.place)
            .extend_from_slice(s.as_bytes())
    }

    /// Replaces the text with `new` in one write, whatever the current
    /// length. An owned `String` is moved in without copying. See
    /// [`PersistableStringGuard`] for an example.
    pub fn set(&mut self, new: impl Into<String>) -> Result<(), Error> {
        self.inner
            .0
            .guard(self.backend, self.place)
            .set_bytes(new.into().into_bytes())
    }

    /// Replaces the text in byte range `range` with `with`, in one splice,
    /// as [`String::replace_range`] does. Panics if the range does not lie on
    /// `char` boundaries.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableString;
    ///
    /// let mut name = Kladde::new(PersistableString::from("ada"));
    /// name.guard().replace_range(1..2, "nn")?;
    /// assert_eq!(name.get(), "anna");
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn replace_range(
        &mut self,
        range: std::ops::Range<usize>,
        with: &str,
    ) -> Result<(), Error> {
        let text: &str = self.inner;
        assert!(
            text.is_char_boundary(range.start) && text.is_char_boundary(range.end),
            "PersistableString::replace_range: {range:?} does not lie on char boundaries"
        );
        self.inner.0.guard(self.backend, self.place).splice_bytes(
            range.start,
            range.end - range.start,
            with.as_bytes(),
        )
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
        replace(self.inner, value, self.backend, &self.place)
    }
}

impl<'s, B: WriteBackend, E: Encoding> Guard for PersistableStringGuard<'s, B, E> {
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

impl<'s, B: WriteBackend, E: Encoding> Deref for PersistableStringGuard<'s, B, E> {
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
        let mut f = Fixture::for_type::<PersistableString>();
        let mut value = PersistableString::new();
        let mut guard = value.guard(&f.store, f.place());
        guard.push_str("hello").unwrap();
        guard.push_str(" world").unwrap();
        assert_eq!(&*value, "hello world");
        let reloaded: PersistableString = f.reload();
        assert_eq!(reloaded, value);
        value.guard(&f.store, f.place()).set("short").unwrap();
        let reloaded: PersistableString = f.reload();
        assert_eq!(reloaded, "short");
        value
            .guard(&f.store, f.place())
            .replace_range(0..1, "S")
            .unwrap();
        let reloaded: PersistableString = f.reload();
        assert_eq!(reloaded, "Short");
    }

    #[test]
    fn equality_ignores_allocation_state() {
        let mut f = Fixture::for_type::<PersistableString>();
        let mut value = PersistableString::new();
        value.guard(&f.store, f.place()).push_str("same").unwrap();
        let reloaded: PersistableString = f.reload();
        assert_eq!(value, reloaded);
        assert_eq!(value, PersistableString::from("same"));
        assert_eq!("same", value);
    }
}
