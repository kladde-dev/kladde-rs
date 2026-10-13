//! [`DurableString`]: text, encoded as UTF-8 and the byte `0xFF`.

use std::hash::{Hash, Hasher};
use std::ops::{Deref, Range};

use kladde_persist::{
    Backend, Decoder, Durable, Encoder, Error, Guard, Region, SchemaBuilder, TypeDescriptor,
};

/// Text: UTF-8, then the byte `0xFF`, which UTF-8 never contains. In memory,
/// a `String`.
///
/// Text states no count, so an edit of text is one splice of the file's
/// sequence, wherever it lands and however long the text is.
///
/// Read it as a `str`, through `Deref`; change it through a
/// [`DurableStringGuard`].
///
/// ```
/// use kladde::{Kladde, Durable};
/// use kladde_types::DurableString;
///
/// let s = DurableString::from("Map");
/// assert_eq!(s.to_bytes(), [b'M', b'a', b'p', 0xff]);
/// let mut title = Kladde::new(s);
/// title.guard().insert_str(0, "World ")?;
/// assert_eq!(title.get(), "World Map");
/// # Ok::<(), kladde::Error>(())
/// ```
#[derive(Default, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DurableString(String);

impl DurableString {
    /// An empty string.
    ///
    /// ```
    /// assert!(kladde_types::DurableString::new().is_empty());
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// The text, as a `String`.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Debug for DurableString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::fmt::Display for DurableString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl Deref for DurableString {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl From<&str> for DurableString {
    fn from(s: &str) -> Self {
        DurableString(s.to_owned())
    }
}

impl From<String> for DurableString {
    fn from(s: String) -> Self {
        DurableString(s)
    }
}

impl From<DurableString> for String {
    fn from(s: DurableString) -> Self {
        s.0
    }
}

impl Hash for DurableString {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state)
    }
}

impl PartialEq<str> for DurableString {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for DurableString {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<DurableString> for str {
    fn eq(&self, other: &DurableString) -> bool {
        self == other.0
    }
}

impl PartialEq<DurableString> for &str {
    fn eq(&self, other: &DurableString) -> bool {
        *self == other.0
    }
}

impl Durable for DurableString {
    const FIXED_SIZE: Option<usize> = None;

    fn encoded_len(&self) -> usize {
        self.0.len() + 1
    }

    fn encode(&self, out: &mut Encoder<'_>) {
        out.text(&self.0);
    }

    fn decode(input: &mut Decoder<'_>) -> Result<Self, Error> {
        input.text().map(DurableString)
    }

    type Guard<'r, B: Backend + 'r> = DurableStringGuard<'r, B>;

    fn guard<'r, B: Backend + 'r>(&'r mut self, region: Region<'r, B>) -> Self::Guard<'r, B> {
        DurableStringGuard {
            inner: self,
            region,
        }
    }

    fn describe_local(_builder: &mut SchemaBuilder) -> TypeDescriptor {
        TypeDescriptor::Text
    }
}

/// The [`Guard`] of a [`DurableString`]: every edit is one splice of
/// the text's bytes, and the terminator stays where the text ends.
///
/// Byte positions must lie on `char` boundaries; one that does not fails
/// with [`Error::OutOfBounds`], recording nothing.
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::DurableString;
///
/// let mut s = Kladde::new(DurableString::from("kladde"));
/// let mut guard = s.guard();
/// guard.push_str(" file")?;
/// guard.replace_range(0..6, "Kladde")?;
/// guard.truncate(6)?;
/// assert_eq!(s.get(), "Kladde");
/// # Ok::<(), kladde::Error>(())
/// ```
pub struct DurableStringGuard<'r, B> {
    inner: &'r mut DurableString,
    region: Region<'r, B>,
}

impl<B: Backend> DurableStringGuard<'_, B> {
    /// Replaces the bytes in `range` with `text`. See
    /// [`DurableStringGuard`] for an example.
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

    /// Appends `text`. See [`DurableStringGuard`] for an example.
    pub fn push_str(&mut self, text: &str) -> Result<(), Error> {
        let end = self.inner.0.len();
        self.replace_range(end..end, text)
    }

    /// Inserts `text` at byte `at`.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::DurableString;
    ///
    /// let mut s = Kladde::new(DurableString::from("ac"));
    /// s.guard().insert_str(1, "b")?;
    /// assert_eq!(s.get(), "abc");
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn insert_str(&mut self, at: usize, text: &str) -> Result<(), Error> {
        self.replace_range(at..at, text)
    }

    /// Removes everything from byte `len` on. See
    /// [`DurableStringGuard`] for an example.
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
    /// use kladde_types::DurableString;
    ///
    /// let mut s = Kladde::new(DurableString::from("draft"));
    /// s.guard().set("final")?;
    /// assert_eq!(s.get(), "final");
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn set(&mut self, text: impl Into<String>) -> Result<(), Error> {
        let text = text.into();
        let end = self.inner.0.len();
        self.region.splice(0, end as u64, text.as_bytes())?;
        self.inner.0 = text;
        Ok(())
    }
}

impl<B> Guard for DurableStringGuard<'_, B> {
    type Durable = DurableString;

    fn as_durable(&self) -> &DurableString {
        self.inner
    }
}

impl<B> Deref for DurableStringGuard<'_, B> {
    type Target = DurableString;

    fn deref(&self) -> &DurableString {
        self.inner
    }
}
