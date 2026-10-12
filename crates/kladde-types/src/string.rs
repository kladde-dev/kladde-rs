//! [`PersistableString`]: text, encoded as UTF-8 and the byte `0xFF`.

use std::hash::{Hash, Hasher};
use std::ops::{Deref, Range};

use kladde_persist::{
    Backend, Decoder, Encoder, Error, Guard, Persistable, Region, SchemaBuilder, TypeDescriptor,
};

/// Text: UTF-8, then the byte `0xFF`, which UTF-8 never contains. In memory,
/// a `String`.
///
/// Text states no count, so an edit of text is one splice of the file's
/// sequence, wherever it lands and however long the text is.
///
/// Read it as a `str`, through `Deref`; change it through a
/// [`PersistableStringGuard`].
///
/// ```
/// use kladde::{Kladde, Persistable};
/// use kladde_types::PersistableString;
///
/// let s = PersistableString::from("Map");
/// assert_eq!(s.to_bytes(), [b'M', b'a', b'p', 0xff]);
/// let mut title = Kladde::new(s);
/// title.guard().insert_str(0, "World ")?;
/// assert_eq!(title.get(), "World Map");
/// # Ok::<(), kladde::Error>(())
/// ```
#[derive(Default, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PersistableString(String);

impl PersistableString {
    /// An empty string.
    ///
    /// ```
    /// assert!(kladde_types::PersistableString::new().is_empty());
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// The text, as a `String`.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Debug for PersistableString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::fmt::Display for PersistableString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl Deref for PersistableString {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl From<&str> for PersistableString {
    fn from(s: &str) -> Self {
        PersistableString(s.to_owned())
    }
}

impl From<String> for PersistableString {
    fn from(s: String) -> Self {
        PersistableString(s)
    }
}

impl From<PersistableString> for String {
    fn from(s: PersistableString) -> Self {
        s.0
    }
}

impl Hash for PersistableString {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state)
    }
}

impl PartialEq<str> for PersistableString {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for PersistableString {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<PersistableString> for str {
    fn eq(&self, other: &PersistableString) -> bool {
        self == other.0
    }
}

impl PartialEq<PersistableString> for &str {
    fn eq(&self, other: &PersistableString) -> bool {
        *self == other.0
    }
}

impl Persistable for PersistableString {
    const FIXED_SIZE: Option<usize> = None;

    fn encoded_len(&self) -> usize {
        self.0.len() + 1
    }

    fn encode(&self, out: &mut Encoder<'_>) {
        out.text(&self.0);
    }

    fn decode(input: &mut Decoder<'_>) -> Result<Self, Error> {
        input.text().map(PersistableString)
    }

    type Guard<'r, B: Backend + 'r> = PersistableStringGuard<'r, B>;

    fn guard<'r, B: Backend + 'r>(&'r mut self, region: Region<'r, B>) -> Self::Guard<'r, B> {
        PersistableStringGuard {
            inner: self,
            region,
        }
    }

    fn describe_local(_builder: &mut SchemaBuilder) -> TypeDescriptor {
        TypeDescriptor::Text
    }
}

/// The [`Guard`] of a [`PersistableString`]: every edit is one splice of
/// the text's bytes, and the terminator stays where the text ends.
///
/// Byte positions must lie on `char` boundaries; one that does not fails
/// with [`Error::OutOfBounds`], recording nothing.
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::PersistableString;
///
/// let mut s = Kladde::new(PersistableString::from("kladde"));
/// let mut guard = s.guard();
/// guard.push_str(" file")?;
/// guard.replace_range(0..6, "Kladde")?;
/// guard.truncate(6)?;
/// assert_eq!(s.get(), "Kladde");
/// # Ok::<(), kladde::Error>(())
/// ```
pub struct PersistableStringGuard<'r, B> {
    inner: &'r mut PersistableString,
    region: Region<'r, B>,
}

impl<B: Backend> PersistableStringGuard<'_, B> {
    /// Replaces the bytes in `range` with `text`. See
    /// [`PersistableStringGuard`] for an example.
    pub fn replace_range(&mut self, range: Range<usize>, text: &str) -> Result<(), Error> {
        let s = &self.inner.0;
        if range.start > range.end
            || range.end > s.len()
            || !s.is_char_boundary(range.start)
            || !s.is_char_boundary(range.end)
        {
            return Err(Error::OutOfBounds);
        }
        self.region.splice(
            range.start as u64,
            (range.end - range.start) as u64,
            text.as_bytes(),
        )?;
        self.inner.0.replace_range(range, text);
        Ok(())
    }

    /// Appends `text`. See [`PersistableStringGuard`] for an example.
    pub fn push_str(&mut self, text: &str) -> Result<(), Error> {
        let end = self.inner.0.len();
        self.replace_range(end..end, text)
    }

    /// Inserts `text` at byte `at`.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableString;
    ///
    /// let mut s = Kladde::new(PersistableString::from("ac"));
    /// s.guard().insert_str(1, "b")?;
    /// assert_eq!(s.get(), "abc");
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn insert_str(&mut self, at: usize, text: &str) -> Result<(), Error> {
        self.replace_range(at..at, text)
    }

    /// Removes everything from byte `len` on. See
    /// [`PersistableStringGuard`] for an example.
    pub fn truncate(&mut self, len: usize) -> Result<(), Error> {
        let end = self.inner.0.len();
        if len >= end {
            return Ok(());
        }
        self.replace_range(len..end, "")
    }

    /// Replaces the whole text.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableString;
    ///
    /// let mut s = Kladde::new(PersistableString::from("draft"));
    /// s.guard().set("final")?;
    /// assert_eq!(s.get(), "final");
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn set(&mut self, text: impl Into<String>) -> Result<(), Error> {
        let text = text.into();
        let end = self.inner.0.len();
        self.region.replace_at(0, end as u64, text.as_bytes())?;
        self.inner.0 = text;
        Ok(())
    }
}

impl<B> Guard for PersistableStringGuard<'_, B> {
    type Persistable = PersistableString;

    fn as_persistable(&self) -> &PersistableString {
        self.inner
    }
}

impl<B> Deref for PersistableStringGuard<'_, B> {
    type Target = PersistableString;

    fn deref(&self) -> &PersistableString {
        self.inner
    }
}
