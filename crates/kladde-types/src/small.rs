//! [`SmallPersistableString`] and [`SmallPersistableVec`]: containers that
//! keep short content inline, in the value that holds them, and move it to an
//! allocation of their own once it grows.
//!
//! Layout, a `Small(C, T)` value: a one-byte size tag, then
//!
//! - for a tag of 0 to 254, the content itself, in that many bytes: a
//!   string's text in UTF-8, or a vector's elements in their packed
//!   encodings, back to back;
//! - for the tag 255, the content's allocation as a varint pointer: the
//!   allocation a [`PersistableString`](crate::PersistableString) or a
//!   [`PackedPersistableVec`](crate::PackedPersistableVec) would hold, with
//!   the same bytes.
//!
//! A small value has only a packed encoding, since a fixed one would reserve
//! its whole inline capacity in every value, so it stands only in packed
//! places: an element of a packed vector, a field of a struct inside one, or a
//! packed root. The policy of when content moves is this crate's, and the
//! tag says which form a value takes, so a reader needs neither threshold.

use std::cell::Cell;
use std::cmp::Ordering;
use std::hash::{Hash, Hasher};
use std::ops::{Deref, Range};

use kladde_persist::{
    read_allocation, replace, slot_size, splice_at, varint_len, write_encoded, write_varint,
    Encoding, Error, Guard, Input, Link, Location, Node, Packed, Persistable, Place, Pointer,
    PointerRepr, ReadBackend, TypeDescriptor, UniquePointer, Word, WriteBackend,
};

use crate::offsets::Offsets;
use crate::packed::{decode_elements, encode_elements, packed_size, PackedPersistableVec};
use crate::slot::size;
use crate::PersistableString;

/// Content of more than this many bytes moves to an allocation of its own.
///
/// A new small value is inline if its content takes at most this many bytes.
/// Between [`FOLD_BELOW`] and this, a value keeps the form it has, so that
/// edits around one threshold do not allocate and free at every crossing.
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::{SmallPersistableString, SPILL_ABOVE};
///
/// let long = "x".repeat(SPILL_ABOVE + 1);
/// let db = Kladde::new(SmallPersistableString::from(long.as_str()));
/// assert!(!db.get().is_inline());
/// ```
pub const SPILL_ABOVE: usize = 128;

/// Spilled content of fewer than this many bytes moves back inline. See
/// [`SPILL_ABOVE`].
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::{SmallPersistableString, FOLD_BELOW, SPILL_ABOVE};
///
/// let mut db = Kladde::new(SmallPersistableString::from("x".repeat(200).as_str()));
/// db.guard().set("x".repeat(FOLD_BELOW))?; // still spilled
/// assert!(!db.get().is_inline());
/// db.guard().set("x".repeat(FOLD_BELOW - 1))?;
/// assert!(db.get().is_inline());
/// # assert!(FOLD_BELOW < SPILL_ABOVE);
/// # Ok::<(), kladde::Error>(())
/// ```
pub const FOLD_BELOW: usize = 64;

/// The tag of a spilled value.
const SPILLED: u8 = 255;

/// The encoding of a value whose `content` is inline.
fn inline_encoding(content: &[u8]) -> Vec<u8> {
    debug_assert!(content.len() < SPILLED as usize);
    let mut out = Vec::with_capacity(1 + content.len());
    out.push(content.len() as u8);
    out.extend_from_slice(content);
    out
}

/// The encoding of a value whose content spilled into allocation `pointer`.
fn spilled_encoding<P: PointerRepr>(pointer: P) -> Vec<u8> {
    let mut out = vec![SPILLED];
    write_varint(pointer.to_u32() as u64, &mut out);
    out
}

/// How many bytes a small value takes: its tag, then its inline content of
/// `content` bytes or the pointer to its spilled content.
fn small_size<P: PointerRepr>(spilled: Option<P>, content: usize) -> usize {
    1 + match spilled {
        Some(p) => varint_len(p.to_u32() as u64),
        None => content,
    }
}

/// Reads a small value's tag and the content it gives: the bytes inline, or
/// those of the allocation it spilled into, with that allocation.
fn decode_content<P: PointerRepr, B: ReadBackend<Pointer = P>>(
    backend: &mut B,
    input: &mut Input<'_>,
) -> Result<(Vec<u8>, Option<P>), Error> {
    let tag = input.byte()?;
    if tag != SPILLED {
        return Ok((input.take(tag as usize)?.to_vec(), None));
    }
    let id = input.varint()?;
    let pointer = match u32::try_from(id) {
        Ok(id) if id > 0 => P::from_u32(id),
        _ => {
            return Err(Error::Corrupt(format!(
                "a spilled small value points to {id}, which is no allocation id"
            )))
        }
    };
    Ok((read_allocation(backend, pointer)?, Some(pointer)))
}

/// The refusal of a small value in a slotted place, which no valid schema
/// has.
fn not_slotted<T>() -> Result<T, Error> {
    Err(Error::Corrupt(
        "a small value has no fixed encoding to decode".into(),
    ))
}

/// Text that keeps up to [`SPILL_ABOVE`] bytes inline, in the value that
/// holds it, and moves to an allocation of its own once it grows: for the
/// many short strings of a document, an `id` or a class name, which would
/// otherwise each own an allocation.
///
/// It reads like a [`PersistableString`], through `Deref<Target = str>`, and
/// mutates through a [`SmallPersistableStringGuard`]. It has no fixed
/// encoding, so it stands only in packed places: inside an element of a
/// [`PackedPersistableVec`] or a [`SmallPersistableVec`], or as a packed root.
/// A struct that holds one is packed-only too, and is marked
/// `#[kladde(packed_only)]`.
///
/// ```
/// use kladde::{Kladde, Persistable};
/// use kladde_types::{PackedPersistableVec, SmallPersistableString};
///
/// #[derive(Persistable)]
/// #[kladde(packed_only)]
/// struct Tag {
///     name: SmallPersistableString,
///     weight: u8,
/// }
///
/// let mut tags = Kladde::new(PackedPersistableVec::<Tag>::new());
/// tags.guard().push(Tag { name: "rust".into(), weight: 3 })?;
/// tags.guard().get_mut(0).unwrap().name_mut().push_str("acean")?;
/// assert_eq!(tags.get()[0].name, "rustacean");
/// assert!(tags.get()[0].name.is_inline());
/// # Ok::<(), kladde::Error>(())
/// ```
///
/// A struct holding one and not marked fails to compile:
///
/// ```compile_fail
/// use kladde::Persistable;
/// use kladde_types::SmallPersistableString;
///
/// #[derive(Persistable)]
/// struct Tag {
///     name: SmallPersistableString, // error[E0277]: `SmallPersistableString` has no fixed encoding
/// }
/// ```
pub struct SmallPersistableString<P = Pointer> {
    text: String,
    /// The allocation the text spilled into, if it did.
    spilled: Option<UniquePointer<P>>,
}

impl<P> SmallPersistableString<P> {
    /// An empty string, inline.
    ///
    /// ```
    /// use kladde_types::SmallPersistableString;
    ///
    /// assert_eq!(SmallPersistableString::<kladde::Pointer>::new(), "");
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the text is inline, rather than in an allocation of its own.
    /// See [`SmallPersistableString`] for an example.
    pub fn is_inline(&self) -> bool {
        self.spilled.is_none()
    }
}

impl<P: Copy> SmallPersistableString<P> {
    fn raw_spilled(&self) -> Option<P> {
        self.spilled.as_ref().map(|p| p.raw())
    }
}

impl<P> Default for SmallPersistableString<P> {
    fn default() -> Self {
        SmallPersistableString {
            text: String::new(),
            spilled: None,
        }
    }
}

impl<P> std::fmt::Debug for SmallPersistableString<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SmallPersistableString")
            .field(&self.text)
            .finish()
    }
}

impl<P> Deref for SmallPersistableString<P> {
    type Target = str;
    fn deref(&self) -> &str {
        &self.text
    }
}

impl<P> std::fmt::Display for SmallPersistableString<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.text, f)
    }
}

impl<P> From<&str> for SmallPersistableString<P> {
    fn from(s: &str) -> Self {
        Self::from(s.to_string())
    }
}

impl<P> From<String> for SmallPersistableString<P> {
    fn from(text: String) -> Self {
        SmallPersistableString {
            text,
            spilled: None,
        }
    }
}

impl<P> From<SmallPersistableString<P>> for String {
    fn from(s: SmallPersistableString<P>) -> Self {
        s.text
    }
}

impl<P> PartialEq for SmallPersistableString<P> {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
    }
}
impl<P> Eq for SmallPersistableString<P> {}

impl<P> Hash for SmallPersistableString<P> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.text.hash(state)
    }
}

impl<P> PartialOrd for SmallPersistableString<P> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl<P> Ord for SmallPersistableString<P> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.text.cmp(&other.text)
    }
}

impl<P> PartialEq<str> for SmallPersistableString<P> {
    fn eq(&self, other: &str) -> bool {
        self.text == other
    }
}
impl<P> PartialEq<&str> for SmallPersistableString<P> {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}
impl<P> PartialEq<SmallPersistableString<P>> for str {
    fn eq(&self, other: &SmallPersistableString<P>) -> bool {
        self == other.text
    }
}
impl<P> PartialEq<SmallPersistableString<P>> for &str {
    fn eq(&self, other: &SmallPersistableString<P>) -> bool {
        *self == other.text
    }
}

impl<P: PointerRepr> Persistable<P> for SmallPersistableString<P> {
    const SLOTTED_SIZE: Option<usize> = None;
    const PACKED_SIZE: Option<usize> = None;

    type RootEncoding = Packed;

    type Guard<'s, B: WriteBackend<Pointer = P>, E: Encoding>
        = SmallPersistableStringGuard<'s, B, E>
    where
        Self: 's,
        B: 's;

    #[inline]
    fn guard<'s, B: WriteBackend<Pointer = P>, E: Encoding>(
        &'s mut self,
        backend: &'s B,
        place: Place<'s, B, E>,
    ) -> Self::Guard<'s, B, E> {
        SmallPersistableStringGuard {
            inner: self,
            backend,
            place,
        }
    }

    fn encoded_size<E: Encoding>(&self) -> usize {
        if !E::PACKED {
            return slot_size::<Self, P>();
        }
        small_size(self.raw_spilled(), self.text.len())
    }

    /// The tag and the text, or the tag 255 and the pointer to it. A new
    /// string whose text is longer than [`SPILL_ABOVE`] must be
    /// [prepared](Persistable::prepare) first.
    fn encode<E: Encoding>(&self, out: &mut Vec<u8>) {
        debug_assert!(E::PACKED, "a small string stands only in packed places");
        match self.raw_spilled() {
            Some(p) => out.extend_from_slice(&spilled_encoding(p)),
            None => out.extend_from_slice(&inline_encoding(self.text.as_bytes())),
        }
    }

    fn decode<B: ReadBackend<Pointer = P>, E: Encoding>(
        backend: &mut B,
        input: &mut Input<'_>,
    ) -> Result<Self, Error> {
        if !E::PACKED {
            return not_slotted();
        }
        let (bytes, spilled) = decode_content(backend, input)?;
        let text = String::from_utf8(bytes)
            .map_err(|_| Error::Corrupt("a SmallPersistableString holds invalid UTF-8".into()))?;
        Ok(SmallPersistableString {
            text,
            spilled: spilled.map(UniquePointer::from_pointer),
        })
    }

    /// A new string longer than [`SPILL_ABOVE`] spills here, into an
    /// allocation filled with one write.
    fn prepare<B: WriteBackend<Pointer = P>>(&mut self, backend: &B) -> Result<(), Error> {
        if self.spilled.is_none() && self.text.len() > SPILL_ABOVE {
            let pointer = backend.alloc(size(0)?)?;
            backend.write(pointer.raw(), size(0)?, self.text.as_bytes())?;
            self.spilled = Some(pointer);
        }
        Ok(())
    }

    fn free<B: WriteBackend<Pointer = P>>(&mut self, backend: &B) -> Result<(), Error> {
        if let Some(pointer) = self.spilled.take() {
            backend.free(pointer)?;
        }
        Ok(())
    }

    /// `Small(Sequence(char), Pointer(Packed(Sequence(char))))`: the text
    /// inline, or a [`PersistableString`].
    fn describe_local(builder: &mut kladde_persist::SchemaBuilder) -> TypeDescriptor
    where
        Self: 'static,
    {
        TypeDescriptor::Small {
            content: builder.sequence::<P, char>(),
            spilled: <PersistableString<P> as Persistable<P>>::describe(builder),
        }
    }
}

/// The mutation-capable view onto a [`SmallPersistableString`].
///
/// Each method is one transaction. An edit that keeps the text inline
/// rewrites the tag and splices the text in place; one that keeps it spilled
/// splices its allocation; one that crosses a threshold moves the text
/// between the two forms.
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::SmallPersistableString;
///
/// let mut name = Kladde::new(SmallPersistableString::from("ada"));
/// let mut guard = name.guard();
/// guard.push_str(" lovelace")?;
/// guard.replace_range(0..1, "A")?;
/// assert_eq!(&*guard, "Ada lovelace");
/// # Ok::<(), kladde::Error>(())
/// ```
pub struct SmallPersistableStringGuard<'s, B: WriteBackend, E: Encoding = Packed> {
    inner: &'s mut SmallPersistableString<B::Pointer>,
    backend: &'s B,
    place: Place<'s, B, E>,
}

impl<'s, B: WriteBackend, E: Encoding> SmallPersistableStringGuard<'s, B, E> {
    /// Replaces the text with `new`. See [`SmallPersistableStringGuard`] for
    /// an example.
    pub fn set(&mut self, new: impl Into<String>) -> Result<(), Error> {
        let new = new.into();
        let len = self.inner.text.len();
        self.edit(0..len, &new)
    }

    /// Appends `s`. See [`SmallPersistableStringGuard`] for an example.
    pub fn push_str(&mut self, s: &str) -> Result<(), Error> {
        let len = self.inner.text.len();
        self.edit(len..len, s)
    }

    /// Replaces the text in byte range `range` with `with`, as
    /// [`String::replace_range`] does. Panics if the range does not lie on
    /// `char` boundaries. See [`SmallPersistableStringGuard`] for an example.
    pub fn replace_range(&mut self, range: Range<usize>, with: &str) -> Result<(), Error> {
        let text = &self.inner.text;
        assert!(
            text.is_char_boundary(range.start) && text.is_char_boundary(range.end),
            "SmallPersistableString::replace_range: {range:?} does not lie on char boundaries"
        );
        self.edit(range, with)
    }

    /// Replaces the whole value: stores `value`, then frees the old one, in
    /// one transaction.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::SmallPersistableString;
    ///
    /// let mut name = Kladde::new(SmallPersistableString::from("a"));
    /// name.guard().replace(SmallPersistableString::from("b"))?;
    /// assert_eq!(name.get(), "b");
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn replace(&mut self, value: SmallPersistableString<B::Pointer>) -> Result<(), Error> {
        replace(self.inner, value, self.backend, &self.place)
    }

    /// Replaces the bytes in `range` with `with`, moving the text between
    /// its forms where the new length crosses a threshold.
    fn edit(&mut self, range: Range<usize>, with: &str) -> Result<(), Error> {
        let (backend, place) = (self.backend, &self.place);
        let old_len = self.inner.text.len();
        let new_len = old_len - range.len() + with.len();
        let new_text = || {
            let mut text = self.inner.text.clone();
            text.replace_range(range.clone(), with);
            text
        };
        let spilled = match self.inner.raw_spilled() {
            None if new_len <= SPILL_ABOVE => {
                backend.atomically(|| {
                    let at = place.location();
                    if new_len != old_len {
                        backend.write(at.anchor, at.offset, &[new_len as u8])?;
                    }
                    let content = at + <B::Size as Word>::from_usize(1 + range.start);
                    if range.len() == with.len() {
                        backend.write(content.anchor, content.offset, with.as_bytes())?;
                    } else {
                        splice_at(backend, content, range.len(), with.as_bytes())?;
                    }
                    place.resized(backend, 1 + old_len, 1 + new_len)
                })?;
                None
            }
            None => {
                let text = new_text();
                let pointer = backend.atomically(|| {
                    let pointer = backend.alloc(size(0)?)?;
                    backend.write(pointer.raw(), size(0)?, text.as_bytes())?;
                    write_encoded(
                        backend,
                        place,
                        1 + old_len,
                        &spilled_encoding(pointer.raw()),
                    )?;
                    Ok(pointer)
                })?;
                Some(pointer)
            }
            Some(p) if new_len >= FOLD_BELOW => {
                let owned = UniquePointer::from_pointer(p);
                if range.len() == with.len() {
                    backend.write(p, size(range.start)?, with.as_bytes())?;
                } else {
                    backend.splice(
                        &owned,
                        size(range.start)?,
                        size(range.len())?,
                        with.as_bytes(),
                    )?;
                }
                Some(owned)
            }
            Some(p) => {
                let text = new_text();
                backend.atomically(|| {
                    write_encoded(
                        backend,
                        place,
                        small_size(Some(p), old_len),
                        &inline_encoding(text.as_bytes()),
                    )?;
                    backend.free(UniquePointer::from_pointer(p))
                })?;
                None
            }
        };
        self.inner.text.replace_range(range, with);
        self.inner.spilled = spilled;
        Ok(())
    }
}

impl<'s, B: WriteBackend, E: Encoding> Guard for SmallPersistableStringGuard<'s, B, E> {
    type Persistable = SmallPersistableString<B::Pointer>;
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

impl<'s, B: WriteBackend, E: Encoding> Deref for SmallPersistableStringGuard<'s, B, E> {
    type Target = str;
    fn deref(&self) -> &str {
        &self.inner.text
    }
}

/// A vector that keeps up to [`SPILL_ABOVE`] bytes of elements inline, in the
/// value that holds it, and moves them to an allocation of its own once they
/// grow: for the many short lists of a document, such as an element's
/// attributes, which would otherwise each own an allocation.
///
/// Its elements are packed, as a [`PackedPersistableVec`]'s are, and laid out
/// identically inline and spilled, so it holds any `T`, small strings
/// included. It reads through `Deref<Target = [T]>` and mutates through a
/// [`SmallPersistableVecGuard`]. It has no fixed encoding, so it stands only
/// in packed places.
///
/// ```
/// use kladde::{Kladde, Persistable};
/// use kladde_types::{PackedPersistableVec, SmallPersistableString, SmallPersistableVec};
///
/// #[derive(Persistable)]
/// #[kladde(packed_only)]
/// struct Node {
///     tags: SmallPersistableVec<SmallPersistableString>,
/// }
///
/// let mut nodes = Kladde::new(PackedPersistableVec::<Node>::new());
/// nodes.guard().push(Node { tags: SmallPersistableVec::new() })?;
/// {
///     let mut guard = nodes.guard();
///     let mut node = guard.get_mut(0).unwrap();
///     let mut tags = node.tags_mut();
///     tags.push("a".into())?;
///     tags.push("b".into())?;
///     tags.get_mut(0).unwrap().push_str("lpha")?;
/// }
/// assert_eq!(nodes.get()[0].tags[0], "alpha");
/// assert!(nodes.get()[0].tags.is_inline());
/// # Ok::<(), kladde::Error>(())
/// ```
pub struct SmallPersistableVec<T, P = Pointer> {
    data: Vec<T>,
    seq: SmallSequence<P>,
}

/// What a small vector keeps beside its elements: where its content is, and
/// where each element starts in it. The [`Node`] its elements' guards link
/// to, which rewrites the tag when an element changes size inline, and moves
/// the content when it crosses a threshold.
pub(crate) struct SmallSequence<P> {
    /// The allocation the content spilled into, if it did; owned.
    spilled: Cell<Option<P>>,
    /// Where each element starts in the content, inline or spilled alike.
    offsets: Offsets,
}

impl<P: PointerRepr> SmallSequence<P> {
    /// Where the content starts, for a vector at `owner`.
    fn content_start<B: WriteBackend<Pointer = P>>(
        &self,
        owner: &Link<'_, B>,
    ) -> Location<P, B::Size> {
        match self.spilled.get() {
            Some(p) => Location::new(p, <B::Size as Word>::zero()),
            None => owner.location() + <B::Size as Word>::from_usize(1),
        }
    }
}

impl<P: PointerRepr, B: WriteBackend<Pointer = P>> Node<B> for SmallSequence<P> {
    fn location_of(&self, owner: &Link<'_, B>, index: usize) -> Location<P, B::Size> {
        self.content_start(owner) + <B::Size as Word>::from_usize(self.offsets.offset(index))
    }

    /// The element has spliced its new encoding into the content. Inline,
    /// the tag must state the new length, and content that grows past
    /// [`SPILL_ABOVE`] moves to an allocation; spilled, content that shrinks
    /// below [`FOLD_BELOW`] moves back inline. Either way the move copies
    /// the content's bytes where they are, since the element being changed
    /// is borrowed and cannot be encoded again.
    fn resized(
        &self,
        owner: &Link<'_, B>,
        backend: &B,
        index: usize,
        old: usize,
        new: usize,
    ) -> Result<(), Error> {
        let before = self.offsets.end();
        let after = before + new - old;
        let one = <B::Size as Word>::from_usize(1);
        let spilled = match self.spilled.get() {
            None if after <= SPILL_ABOVE => {
                let at = owner.location();
                backend.write(at.anchor, at.offset, &[after as u8])?;
                owner.resized(backend, 1 + before, 1 + after)?;
                None
            }
            None => {
                let at = owner.location();
                let pointer = backend.alloc(size(0)?)?;
                backend.copy(
                    at.anchor,
                    at.offset + one,
                    size(after)?,
                    pointer.raw(),
                    size(0)?,
                )?;
                let encoding = spilled_encoding(pointer.raw());
                splice_at(backend, at, 1 + after, &encoding)?;
                owner.resized(backend, 1 + before, encoding.len())?;
                Some(pointer.raw())
            }
            Some(p) if after >= FOLD_BELOW => Some(p),
            Some(p) => {
                let at = owner.location();
                let mut encoding = vec![0u8; 1 + after];
                encoding[0] = after as u8;
                let old_size = small_size(Some(p), before);
                splice_at(backend, at, old_size, &encoding)?;
                backend.copy(p, size(0)?, size(after)?, at.anchor, at.offset + one)?;
                owner.resized(backend, old_size, 1 + after)?;
                backend.free(UniquePointer::from_pointer(p))?;
                None
            }
        };
        self.spilled.set(spilled);
        self.offsets.shift(index + 1, new as isize - old as isize);
        Ok(())
    }
}

impl<T, P> SmallPersistableVec<T, P> {
    /// An empty vector, inline.
    ///
    /// ```
    /// use kladde_types::SmallPersistableVec;
    ///
    /// assert!(SmallPersistableVec::<u32>::new().is_empty());
    /// ```
    pub fn new() -> Self {
        SmallPersistableVec {
            data: Vec::new(),
            seq: SmallSequence {
                spilled: Cell::new(None),
                offsets: Offsets::new(),
            },
        }
    }

    /// The elements, as a slice. The same as dereferencing.
    ///
    /// ```
    /// use kladde_types::SmallPersistableVec;
    ///
    /// let v: SmallPersistableVec<u16> = [1, 2].into_iter().collect();
    /// assert_eq!(v.as_slice(), &[1, 2]);
    /// ```
    pub fn as_slice(&self) -> &[T] {
        &self.data
    }

    /// How many bytes the elements take, inline or spilled, as of the last
    /// time the vector was stored or changed through a guard.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::SmallPersistableVec;
    ///
    /// let db = Kladde::new([1u32, 300].into_iter().collect::<SmallPersistableVec<u32>>());
    /// assert_eq!(db.get().content_size(), 1 + 2);
    /// ```
    pub fn content_size(&self) -> usize {
        self.seq.offsets.end()
    }
}

impl<T, P: Copy> SmallPersistableVec<T, P> {
    /// Whether the elements are inline, rather than in an allocation of
    /// their own. See [`SmallPersistableVec`] for an example.
    pub fn is_inline(&self) -> bool {
        self.seq.spilled.get().is_none()
    }
}

impl<T: std::fmt::Debug, P> std::fmt::Debug for SmallPersistableVec<T, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SmallPersistableVec")
            .field(&self.data)
            .finish()
    }
}

/// Compares the elements only, not where they are stored.
impl<T: PartialEq, P> PartialEq for SmallPersistableVec<T, P> {
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data
    }
}

impl<T, P> Deref for SmallPersistableVec<T, P> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.data
    }
}

impl<T, P> Default for SmallPersistableVec<T, P> {
    fn default() -> Self {
        Self::new()
    }
}

/// Collects into a vector that is not laid out yet: storing it, as part of a
/// value that is itself stored, lays it out inline, or spills it if its
/// elements take more than [`SPILL_ABOVE`] bytes.
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::SmallPersistableVec;
///
/// let small: SmallPersistableVec<u32> = (1..4).collect();
/// let large: SmallPersistableVec<u32> = (1..200).collect();
/// assert!(Kladde::new(small).get().is_inline());
/// assert!(!Kladde::new(large).get().is_inline());
/// ```
impl<T, P> FromIterator<T> for SmallPersistableVec<T, P> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        SmallPersistableVec {
            data: Vec::from_iter(iter),
            ..Self::new()
        }
    }
}

impl<'a, T, P> IntoIterator for &'a SmallPersistableVec<T, P> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.data.iter()
    }
}

impl<T: Persistable<P>, P: PointerRepr> SmallPersistableVec<T, P> {
    /// The content's size, laid out or not.
    fn content_len(&self) -> usize {
        if self.seq.offsets.laid_out() {
            self.seq.offsets.end()
        } else {
            Offsets::of::<T, P>(&self.data).end()
        }
    }

    /// The elements' packed encodings, back to back.
    fn content(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.content_len());
        for item in &self.data {
            item.encode::<Packed>(&mut out);
        }
        out
    }
}

impl<T: Persistable<P>, P: PointerRepr> Persistable<P> for SmallPersistableVec<T, P> {
    const SLOTTED_SIZE: Option<usize> = None;
    const PACKED_SIZE: Option<usize> = None;

    type RootEncoding = Packed;

    type Guard<'s, B: WriteBackend<Pointer = P>, E: Encoding>
        = SmallPersistableVecGuard<'s, T, B, E>
    where
        Self: 's,
        B: 's;

    #[inline]
    fn guard<'s, B: WriteBackend<Pointer = P>, E: Encoding>(
        &'s mut self,
        backend: &'s B,
        place: Place<'s, B, E>,
    ) -> Self::Guard<'s, B, E> {
        SmallPersistableVecGuard {
            inner: self,
            backend,
            place,
        }
    }

    fn encoded_size<E: Encoding>(&self) -> usize {
        if !E::PACKED {
            return slot_size::<Self, P>();
        }
        small_size(self.seq.spilled.get(), self.content_len())
    }

    /// The tag and the elements, or the tag 255 and the pointer to them. A
    /// new vector must be [prepared](Persistable::prepare) first, which
    /// decides its form.
    fn encode<E: Encoding>(&self, out: &mut Vec<u8>) {
        debug_assert!(E::PACKED, "a small vector stands only in packed places");
        match self.seq.spilled.get() {
            Some(p) => out.extend_from_slice(&spilled_encoding(p)),
            None => out.extend_from_slice(&inline_encoding(&self.content())),
        }
    }

    fn decode<B: ReadBackend<Pointer = P>, E: Encoding>(
        backend: &mut B,
        input: &mut Input<'_>,
    ) -> Result<Self, Error> {
        if !E::PACKED {
            return not_slotted();
        }
        let (bytes, spilled) = decode_content(backend, input)?;
        let (data, offsets) = decode_elements::<T, P, B>(backend, &mut Input::new(&bytes))?;
        Ok(SmallPersistableVec {
            data,
            seq: SmallSequence {
                spilled: Cell::new(spilled),
                offsets,
            },
        })
    }

    /// A new vector is laid out here: inline if its elements take at most
    /// [`SPILL_ABOVE`] bytes, and otherwise spilled into an allocation
    /// filled with one write.
    fn prepare<B: WriteBackend<Pointer = P>>(&mut self, backend: &B) -> Result<(), Error> {
        if !self.seq.offsets.laid_out() {
            let (bytes, offsets) = encode_elements(&mut self.data, backend)?;
            if bytes.len() > SPILL_ABOVE {
                let pointer = backend.alloc(size(0)?)?;
                backend.write(pointer.raw(), size(0)?, &bytes)?;
                self.seq.spilled.set(Some(pointer.raw()));
            }
            self.seq.offsets = offsets;
        }
        Ok(())
    }

    /// Frees every element, then the allocation the content spilled into.
    fn free<B: WriteBackend<Pointer = P>>(&mut self, backend: &B) -> Result<(), Error> {
        for item in &mut self.data {
            item.free(backend)?;
        }
        if let Some(pointer) = self.seq.spilled.take() {
            backend.free(UniquePointer::from_pointer(pointer))?;
        }
        Ok(())
    }

    /// `Small(Sequence(T), Pointer(Packed(Sequence(T))))`: the elements
    /// inline, or a [`PackedPersistableVec`].
    fn describe_local(builder: &mut kladde_persist::SchemaBuilder) -> TypeDescriptor
    where
        Self: 'static,
    {
        TypeDescriptor::Small {
            content: builder.sequence::<P, T>(),
            spilled: <PackedPersistableVec<T, P> as Persistable<P>>::describe(builder),
        }
    }
}

/// The mutation-capable view onto a [`SmallPersistableVec`]: the methods of a
/// [`PackedPersistableVecGuard`](crate::PackedPersistableVecGuard), each one
/// transaction.
///
/// Inline, a change of the content's length also rewrites the tag; content
/// that grows past [`SPILL_ABOVE`] moves to an allocation of its own, and
/// spilled content that shrinks below [`FOLD_BELOW`] moves back inline. An
/// element's guard does the same when the element changes size.
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::SmallPersistableVec;
///
/// let mut db = Kladde::new(SmallPersistableVec::<u64>::new());
/// let mut v = db.guard();
/// for i in 0..20 {
///     v.push(i)?;
/// }
/// assert!(v.is_inline());
/// v.get_mut(0).unwrap().set(u64::MAX)?; // nine bytes longer, still inline
/// for _ in 0..12 {
///     v.push(u64::MAX)?;
/// }
/// assert!(!v.is_inline()); // more than 128 bytes now
/// v.clear()?;
/// assert!(db.get().is_inline());
/// # Ok::<(), kladde::Error>(())
/// ```
pub struct SmallPersistableVecGuard<'s, T, B: WriteBackend, E: Encoding = Packed> {
    inner: &'s mut SmallPersistableVec<T, B::Pointer>,
    backend: &'s B,
    place: Place<'s, B, E>,
}

impl<'s, T: Persistable<B::Pointer>, B: WriteBackend, E: Encoding>
    SmallPersistableVecGuard<'s, T, B, E>
{
    /// The guard of element `index`, in a packed place, or `None` if there is
    /// none. See [`SmallPersistableVecGuard`] for an example.
    #[inline]
    pub fn get_mut(
        &mut self,
        index: usize,
    ) -> Option<<T as Persistable<B::Pointer>>::Guard<'_, B, Packed>> {
        let SmallPersistableVec { data, seq } = &mut *self.inner;
        let item = data.get_mut(index)?;
        let place = self.place.child::<Packed>(&*seq, index);
        Some(item.guard(self.backend, place))
    }

    /// Appends `value`. See [`SmallPersistableVecGuard`] for an example.
    pub fn push(&mut self, value: T) -> Result<(), Error> {
        let len = self.inner.data.len();
        self.insert(len, value)
    }

    /// Inserts `value` at `index`, shifting every later element. Panics if
    /// `index > len`, as [`Vec::insert`] does.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::SmallPersistableVec;
    ///
    /// let mut db = Kladde::new(SmallPersistableVec::<u8>::new());
    /// db.guard().push(3)?;
    /// db.guard().insert(0, 1)?;
    /// assert_eq!(db.get().as_slice(), &[1, 3]);
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn insert(&mut self, index: usize, mut value: T) -> Result<(), Error> {
        let len = self.inner.data.len();
        assert!(
            index <= len,
            "SmallPersistableVec::insert: index {index} out of bounds"
        );
        debug_assert!(
            self.inner.seq.offsets.laid_out(),
            "a stored vector is laid out"
        );
        let (backend, place) = (self.backend, &self.place);
        let inner = &*self.inner;
        let before = inner.seq.offsets.end();
        let at = if len == 0 {
            0
        } else {
            inner.seq.offsets.offset(index)
        };
        let (bytes, spilled) = backend.atomically(|| {
            value.prepare(backend)?;
            packed_size(&value);
            let bytes = value.to_bytes::<Packed>();
            let after = before + bytes.len();
            let spilled = match inner.seq.spilled.get() {
                None if after <= SPILL_ABOVE => {
                    let tag = place.location();
                    backend.write(tag.anchor, tag.offset, &[after as u8])?;
                    let content = tag + <B::Size as Word>::from_usize(1 + at);
                    splice_at(backend, content, 0, &bytes)?;
                    place.resized(backend, 1 + before, 1 + after)?;
                    None
                }
                None => {
                    let mut content = inner.content();
                    content.splice(at..at, bytes.iter().copied());
                    let pointer = backend.alloc(size(0)?)?;
                    backend.write(pointer.raw(), size(0)?, &content)?;
                    write_encoded(backend, place, 1 + before, &spilled_encoding(pointer.raw()))?;
                    Some(pointer.raw())
                }
                Some(p) => {
                    backend.splice(&UniquePointer::from_pointer(p), size(at)?, size(0)?, &bytes)?;
                    Some(p)
                }
            };
            Ok((bytes.len(), spilled))
        })?;
        self.inner.seq.spilled.set(spilled);
        self.inner.seq.offsets.insert(index, bytes);
        self.inner.data.insert(index, value);
        Ok(())
    }

    /// Removes the last element and returns it, allocations and all, or
    /// `None` if the vector is empty.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::SmallPersistableVec;
    ///
    /// let mut db = Kladde::new(SmallPersistableVec::<u8>::new());
    /// db.guard().push(4)?;
    /// assert_eq!(db.guard().pop()?, Some(4));
    /// assert_eq!(db.guard().pop()?, None);
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn pop(&mut self) -> Result<Option<T>, Error> {
        match self.inner.data.len() {
            0 => Ok(None),
            len => self.take(len - 1, false).map(Some),
        }
    }

    /// Removes element `index` and returns it, allocations and all, shifting
    /// every later element. Panics if `index` is out of bounds.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::SmallPersistableVec;
    ///
    /// let mut db = Kladde::new(SmallPersistableVec::<u8>::new());
    /// db.guard().push(4)?;
    /// db.guard().push(5)?;
    /// assert_eq!(db.guard().remove(0)?, 4);
    /// assert_eq!(db.get().as_slice(), &[5]);
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn remove(&mut self, index: usize) -> Result<T, Error> {
        self.take(index, false)
    }

    /// Removes element `index` and frees everything it owns, in one
    /// transaction. Panics if `index` is out of bounds.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::{SmallPersistableString, SmallPersistableVec};
    ///
    /// let mut db = Kladde::new(SmallPersistableVec::<SmallPersistableString>::new());
    /// db.guard().push("gone".into())?;
    /// db.guard().delete(0)?;
    /// assert!(db.get().is_empty());
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn delete(&mut self, index: usize) -> Result<(), Error> {
        self.take(index, true).map(drop)
    }

    /// Takes element `index` out, freeing what it owns if `free`, and moves
    /// spilled content that shrinks below [`FOLD_BELOW`] back inline.
    fn take(&mut self, index: usize, free: bool) -> Result<T, Error> {
        let len = self.inner.data.len();
        assert!(
            index < len,
            "SmallPersistableVec: index {index} out of bounds"
        );
        let (backend, place) = (self.backend, &self.place);
        let SmallPersistableVec { data, seq } = &mut *self.inner;
        let before = seq.offsets.end();
        let (at, gone) = (seq.offsets.offset(index), seq.offsets.len_of(index));
        let after = before - gone;
        let spilled = backend.atomically(|| {
            let spilled = match seq.spilled.get() {
                None => {
                    let tag = place.location();
                    backend.write(tag.anchor, tag.offset, &[after as u8])?;
                    let content = tag + <B::Size as Word>::from_usize(1 + at);
                    splice_at(backend, content, gone, &[])?;
                    place.resized(backend, 1 + before, 1 + after)?;
                    None
                }
                Some(p) if after >= FOLD_BELOW => {
                    backend.splice(&UniquePointer::from_pointer(p), size(at)?, size(gone)?, &[])?;
                    Some(p)
                }
                Some(p) => {
                    let mut content = Vec::with_capacity(after);
                    for (i, item) in data.iter().enumerate() {
                        if i != index {
                            item.encode::<Packed>(&mut content);
                        }
                    }
                    write_encoded(
                        backend,
                        place,
                        small_size(Some(p), before),
                        &inline_encoding(&content),
                    )?;
                    backend.free(UniquePointer::from_pointer(p))?;
                    None
                }
            };
            if free {
                data[index].free(backend)?;
            }
            Ok(spilled)
        })?;
        seq.spilled.set(spilled);
        seq.offsets.remove(index);
        Ok(data.remove(index))
    }

    /// Removes every element and frees everything they own, in one
    /// transaction; the content is inline and empty afterwards. See
    /// [`SmallPersistableVecGuard`] for an example.
    pub fn clear(&mut self) -> Result<(), Error> {
        let (backend, place) = (self.backend, &self.place);
        let SmallPersistableVec { data, seq } = &mut *self.inner;
        let before = seq.offsets.end();
        let spilled = seq.spilled.get();
        backend.atomically(|| {
            write_encoded(backend, place, small_size(spilled, before), &[0])?;
            if let Some(p) = spilled {
                backend.free(UniquePointer::from_pointer(p))?;
            }
            for item in data.iter_mut() {
                item.free(backend)?;
            }
            Ok(())
        })?;
        seq.spilled.set(None);
        seq.offsets.clear();
        data.clear();
        Ok(())
    }

    /// Replaces the whole vector: stores `value`, which publishes it, then
    /// frees the old elements and content, in one transaction.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::SmallPersistableVec;
    ///
    /// let mut db = Kladde::new(SmallPersistableVec::<u8>::new());
    /// db.guard().set([5, 6].into_iter().collect())?;
    /// assert_eq!(db.get().as_slice(), &[5, 6]);
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn set(&mut self, value: SmallPersistableVec<T, B::Pointer>) -> Result<(), Error> {
        replace(self.inner, value, self.backend, &self.place)
    }
}

impl<'s, T, B: WriteBackend, E: Encoding> Guard for SmallPersistableVecGuard<'s, T, B, E> {
    type Persistable = SmallPersistableVec<T, B::Pointer>;
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

impl<'s, T, B: WriteBackend, E: Encoding> Deref for SmallPersistableVecGuard<'s, T, B, E> {
    type Target = SmallPersistableVec<T, B::Pointer>;
    fn deref(&self) -> &Self::Target {
        self.inner
    }
}
