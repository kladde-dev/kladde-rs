//! [`Durable`] for [`String`]: text, encoded as UTF-8 and the byte `0xFF`,
//! and its [`StringMut`], every edit of which is one splice.

use std::ops::{Deref, Range};

use kladde_schema::TypeDescriptor;
use kladde_store::Error;

use crate::codec::{Decoder, Encoder};
use crate::durable::{Durable, Mut};
use crate::region::{Backend, Region};
use crate::schema::SchemaBuilder;

/// Text: UTF-8, then the byte `0xFF`, which UTF-8 never contains.
///
/// Text states no count, so an edit of text is one splice of the file's
/// sequence, wherever it lands and however long the text is.
///
/// ```
/// use kladde_persist::{Durable, Region};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let mut title = String::from("Map");
/// assert_eq!(title.to_bytes(), [b'M', b'a', b'p', 0xff]);
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// store.splice(0, 0, &title.to_bytes())?;
/// title.mutate(Region::root(&store)).insert_str(0, "World ")?;
/// assert_eq!(title, "World Map");
/// store.flush()?;
/// assert_eq!(store.read_all()?, b"World Map\xff");
/// # Ok::<(), kladde_store::Error>(())
/// ```
impl Durable for String {
    const FIXED_SIZE: Option<usize> = None;

    fn encoded_len(&self) -> usize {
        self.len() + 1
    }

    fn encode(&self, out: &mut Encoder<'_>) {
        out.text(self);
    }

    fn decode(input: &mut Decoder<'_>) -> Result<Self, Error> {
        input.text()
    }

    type Mut<'r, B: Backend + 'r> = StringMut<'r, B>;

    fn mutate<'r, B: Backend + 'r>(&'r mut self, region: Region<'r, B>) -> Self::Mut<'r, B> {
        StringMut {
            inner: self,
            region,
        }
    }

    fn describe_local(_builder: &mut SchemaBuilder) -> TypeDescriptor {
        TypeDescriptor::Text
    }
}

/// The [`Mut`] of a [`String`]: every edit is one splice of the text's
/// bytes, and the terminator stays where the text ends.
///
/// Byte positions must lie on `char` boundaries; one that does not fails
/// with [`Error::OutOfBounds`], recording nothing.
///
/// ```
/// use kladde_persist::{Durable, Region};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let mut s = String::from("kladde");
/// store.splice(0, 0, &s.to_bytes())?;
/// let mut text = s.mutate(Region::root(&store));
/// text.push_str(" file")?;
/// text.replace_range(0..6, "Kladde")?;
/// text.truncate(6)?;
/// assert_eq!(s, "Kladde");
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub struct StringMut<'r, B> {
    inner: &'r mut String,
    region: Region<'r, B>,
}

impl<B: Backend> StringMut<'_, B> {
    /// Replaces the bytes in `range` with `text`. See [`StringMut`] for an
    /// example.
    pub fn replace_range(&mut self, range: Range<usize>, text: &str) -> Result<(), Error> {
        let s = &*self.inner;
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
        self.inner.replace_range(range, text);
        Ok(())
    }

    /// Appends `text`. See [`StringMut`] for an example.
    pub fn push_str(&mut self, text: &str) -> Result<(), Error> {
        let end = self.inner.len();
        self.replace_range(end..end, text)
    }

    /// Inserts `text` at byte `at`.
    ///
    /// ```
    /// use kladde_persist::{Durable, Region};
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// let mut s = String::from("ac");
    /// store.splice(0, 0, &s.to_bytes())?;
    /// s.mutate(Region::root(&store)).insert_str(1, "b")?;
    /// assert_eq!(s, "abc");
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn insert_str(&mut self, at: usize, text: &str) -> Result<(), Error> {
        self.replace_range(at..at, text)
    }

    /// Removes everything from byte `len` on. See [`StringMut`] for an
    /// example.
    pub fn truncate(&mut self, len: usize) -> Result<(), Error> {
        let end = self.inner.len();
        if len >= end {
            return Ok(());
        }
        self.replace_range(len..end, "")
    }

    /// Replaces the whole text, recording only what changed: one splice of
    /// the bytes between the longest prefix and the longest suffix the old
    /// and the new text share, or nothing if they are equal. Comparing the
    /// two costs a pass over both.
    ///
    /// So code that sets a whole text at every change, a path's data
    /// rewritten for every step of a drag, say, records a few bytes rather
    /// than the whole text.
    ///
    /// ```
    /// use kladde_persist::{Durable, Region};
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// let mut d = String::from("M 0 0 L 10 10 L 20 0");
    /// store.splice(0, 0, &d.to_bytes())?;
    /// let before = store.stats().journal_bytes;
    /// d.mutate(Region::root(&store)).set("M 0 0 L 12 10 L 20 0")?;
    /// assert!(store.stats().journal_bytes - before < 12); // "2" over "0"
    /// assert_eq!(d, "M 0 0 L 12 10 L 20 0");
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn set(&mut self, text: impl Into<String>) -> Result<(), Error> {
        let text = text.into();
        splice_changed(&self.region, 0, self.inner.as_bytes(), text.as_bytes())?;
        *self.inner = text;
        Ok(())
    }
}

/// Replaces `old`, the bytes at `at` in `region`, with `new` by one splice
/// of the range between their longest common prefix and their longest
/// common suffix, or by nothing if they are equal: the least that turns the
/// one into the other with one record.
///
/// ```
/// use kladde_persist::{splice_changed, Region};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// store.splice(0, 0, b"abcdef")?;
/// splice_changed(&Region::root(&store), 0, b"abcdef", b"abXYZef")?;
/// store.flush()?;
/// assert_eq!(store.read_all()?, b"abXYZef");
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub fn splice_changed<B: Backend>(
    region: &Region<'_, B>,
    at: u64,
    old: &[u8],
    new: &[u8],
) -> Result<(), Error> {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let room = old.len().min(new.len()) - prefix;
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take(room)
        .take_while(|(a, b)| a == b)
        .count();
    let removed = old.len() - prefix - suffix;
    let inserted = &new[prefix..new.len() - suffix];
    if removed == 0 && inserted.is_empty() {
        return Ok(());
    }
    region.splice(at + prefix as u64, removed as u64, inserted)?;
    Ok(())
}

impl<B> Mut for StringMut<'_, B> {
    type Durable = String;

    fn as_durable(&self) -> &String {
        self.inner
    }
}

impl<B> Deref for StringMut<'_, B> {
    type Target = String;

    fn deref(&self) -> &String {
        self.inner
    }
}
