//! [`Place`]: where a guard's value lives, and whom the value tells when its
//! encoded size changes.
//!
//! A value in a slotted place never changes size, so its guard needs only a
//! [`Location`]. A value in a packed place can: setting a `u32` from 100 to
//! 200 makes its varint a byte longer, and switching an enum's variant changes
//! how many bytes it takes. Its guard then splices the new encoding in and
//! reports the change to its parent, which may have to act on it -- a small
//! value rewrites the length its tag states, a vector shifts the offsets it
//! keeps -- and passes it on to its own parent while its own size changes too.
//!
//! The parent is reached through a [`Link`]: either a fixed [`Location`] with
//! nobody to tell, or a reference to the parent's [`Node`] and the child's
//! index among its children. Locations of linked values are computed when
//! they are needed, by asking up the chain, so a sibling that changes size,
//! or an ancestor whose content moves to another allocation, never leaves a
//! guard writing at a stale offset.

use std::cell::Cell;
use std::marker::PhantomData;

use kladde_store::{Error, UniquePointer, Word, WriteBackend};

use crate::encoding::{Encoding, Packed, Slotted};
use crate::location::Location;

/// What a value whose children may change size keeps about them: where each
/// child starts, and what a child's change of size requires.
///
/// A composite guard implements it over the offsets of its fields
/// ([`FieldOffsets`]), a packed vector over the offsets of its elements, a
/// small value over its tag. Its methods take the owner's [`Link`] -- where
/// the value implementing it sits -- so that the node itself can live in the
/// value rather than in its guard.
///
/// ```
/// use kladde_persist::{FieldOffsets, Link, Node};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let p = store.alloc(8)?;
/// let owner = Link::<Store>::at(kladde_persist::Location::new(p.raw(), 2));
/// let fields = FieldOffsets::<3>::new();
/// fields.fill(0, &[1, 4]); // two fields, of one and four bytes
/// assert_eq!(fields.location_of(&owner, 1).offset, 3);
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub trait Node<B: WriteBackend> {
    /// Where child `index` of the value at `owner` starts now.
    fn location_of(&self, owner: &Link<'_, B>, index: usize) -> Location<B::Pointer, B::Size>;

    /// Records that child `index` changed its encoded size from `old` to
    /// `new` bytes, after it spliced its new encoding in.
    ///
    /// Runs inside the child's transaction: it records whatever else the
    /// change requires and reports the owner's own change of size through
    /// `owner`, and only once all of that succeeded updates what the node
    /// keeps in memory, so that a failure leaves the node matching the
    /// unchanged value.
    fn resized(
        &self,
        owner: &Link<'_, B>,
        backend: &B,
        index: usize,
        old: usize,
        new: usize,
    ) -> Result<(), Error>;
}

/// The untyped part of a [`Place`]: a fixed location, or a child of a
/// [`Node`].
///
/// ```
/// use kladde_persist::{Link, Location};
/// use kladde_store::{Pointer, Store};
///
/// let at = Location::new(Pointer::<u32>::from_raw(1).unwrap(), 4u32);
/// let link = Link::<Store>::at(at);
/// assert_eq!(link.location(), at);
/// assert!(link.is_fixed());
/// ```
pub struct Link<'a, B: WriteBackend>(Kind<'a, B>);

enum Kind<'a, B: WriteBackend> {
    /// A location nobody needs to hear about: the value's size never
    /// changes, or the store tracks the allocation it fills.
    At(Location<B::Pointer, B::Size>),
    /// Child `index` of the value at `owner`, whose node is `node`.
    In {
        node: &'a (dyn Node<B> + 'a),
        owner: &'a Link<'a, B>,
        index: usize,
    },
}

impl<B: WriteBackend> Clone for Kind<'_, B> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<B: WriteBackend> Copy for Kind<'_, B> {}

impl<B: WriteBackend> Clone for Link<'_, B> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<B: WriteBackend> Copy for Link<'_, B> {}

impl<'a, B: WriteBackend> Link<'a, B> {
    /// A fixed location with nobody to tell. See [`Link`] for an example.
    pub fn at(location: Location<B::Pointer, B::Size>) -> Self {
        Link(Kind::At(location))
    }

    /// Where the value starts now. See [`Link`] for an example.
    pub fn location(&self) -> Location<B::Pointer, B::Size> {
        match self.0 {
            Kind::At(location) => location,
            Kind::In { node, owner, index } => node.location_of(owner, index),
        }
    }

    /// Reports that the value's encoded size changed from `old` to `new`
    /// bytes, after it spliced its new encoding in, to whoever needs to know.
    pub fn resized(&self, backend: &B, old: usize, new: usize) -> Result<(), Error> {
        match self.0 {
            _ if old == new => Ok(()),
            Kind::At(_) => Ok(()),
            Kind::In { node, owner, index } => node.resized(owner, backend, index, old, new),
        }
    }

    /// Whether the location is fixed, with nobody to tell: no ancestor keeps
    /// track of this value's size, and no sibling can move it.
    pub fn is_fixed(&self) -> bool {
        matches!(self.0, Kind::At(_))
    }
}

/// Where a guard's value lives, which encoding the place holds, and whom to
/// tell when the value's size changes.
///
/// A slotted value at a fixed location is just a [`Location`]; build one with
/// [`Slotted::at`] or [`Packed::at`], which hand it to a
/// [`guard`](crate::Persistable::guard). Composite guards build the places of
/// their children with [`field`](Place::field) and [`child`](Place::child).
///
/// ```
/// use kladde_persist::{Location, Persistable, Slotted};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let p = store.alloc(4)?;
/// let mut count = 0u32;
/// count.guard(&store, Slotted::at(Location::new(p.raw(), 0))).set(5)?;
/// assert_eq!(count, 5);
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub struct Place<'a, B: WriteBackend, E: Encoding> {
    link: Link<'a, B>,
    encoding: PhantomData<E>,
}

impl<B: WriteBackend, E: Encoding> Clone for Place<'_, B, E> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<B: WriteBackend, E: Encoding> Copy for Place<'_, B, E> {}

impl<'a, B: WriteBackend, E: Encoding> Place<'a, B, E> {
    /// A fixed location with nobody to tell. See [`Place`] for an example.
    pub fn at(location: Location<B::Pointer, B::Size>) -> Self {
        Place::from_link(Link::at(location))
    }

    /// The place `link` names, holding encoding `E`.
    ///
    /// ```
    /// use kladde_persist::{Link, Location, Packed, Place};
    /// use kladde_store::{Pointer, Store};
    ///
    /// let at = Location::new(Pointer::<u32>::from_raw(1).unwrap(), 0u32);
    /// let place = Place::<Store, Packed>::from_link(Link::at(at));
    /// assert_eq!(place.location(), at);
    /// ```
    pub fn from_link(link: Link<'a, B>) -> Self {
        Place {
            link,
            encoding: PhantomData,
        }
    }

    /// The untyped link to the place's parent.
    pub fn link(&self) -> &Link<'a, B> {
        &self.link
    }

    /// The same place, read as holding encoding `C`: for a value that lays
    /// itself out exactly as something it wraps.
    pub fn cast<C: Encoding>(self) -> Place<'a, B, C> {
        Place::from_link(self.link)
    }

    /// Where the value starts now. See [`Place`] for an example.
    pub fn location(&self) -> Location<B::Pointer, B::Size> {
        self.link.location()
    }

    /// Whether the location is fixed, with nobody to tell.
    pub fn is_fixed(&self) -> bool {
        self.link.is_fixed()
    }

    /// Reports a change of the value's encoded size from `old` to `new`
    /// bytes, after it spliced its new encoding in. See [`Link::resized`].
    pub fn resized(&self, backend: &B, old: usize, new: usize) -> Result<(), Error> {
        self.link.resized(backend, old, new)
    }

    /// The place of child `index` of this value, whose node is `node`,
    /// holding encoding `C`.
    pub fn child<'b, C: Encoding>(
        &'b self,
        node: &'b (dyn Node<B> + 'b),
        index: usize,
    ) -> Place<'b, B, C> {
        Place::from_link(Link(Kind::In {
            node,
            owner: &self.link,
            index,
        }))
    }

    /// The place of field `index` of a composite value at this place, which
    /// starts `fixed_offset` bytes into the value's fixed encoding.
    ///
    /// A slotted value at a fixed location hands its fields fixed locations
    /// at their static offsets, which is all that today's layout needs. Any
    /// other value links its fields to `offsets`, so that each field finds
    /// itself however its siblings and ancestors change size.
    pub fn field<'b, C: Encoding, const N: usize>(
        &'b self,
        offsets: &'b FieldOffsets<N>,
        index: usize,
        fixed_offset: usize,
    ) -> Place<'b, B, C> {
        match self.link.0 {
            Kind::At(location) if !E::PACKED => {
                Place::at(location + <B::Size as Word>::from_usize(fixed_offset))
            }
            _ => self.child(offsets, index),
        }
    }

    /// Whether a composite value at this place links its fields to its
    /// [`FieldOffsets`], which it must then [`fill`](FieldOffsets::fill).
    pub fn links_fields(&self) -> bool {
        E::PACKED || !self.is_fixed()
    }
}

impl Slotted {
    /// A slotted place at a fixed `location`. See [`Place`] for an example.
    pub fn at<'a, B: WriteBackend>(
        location: Location<B::Pointer, B::Size>,
    ) -> Place<'a, B, Slotted> {
        Place::at(location)
    }
}

impl Packed {
    /// A packed place at a fixed `location`, with nobody to tell when the
    /// value changes size: the start of an allocation that holds nothing
    /// else.
    ///
    /// ```
    /// use kladde_persist::{Location, Packed, Persistable};
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let mut store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// let p = store.alloc(1)?;
    /// let mut count = 5u32;
    /// count.guard(&store, Packed::at(Location::new(p.raw(), 0))).set(500)?;
    /// store.flush()?;
    /// assert_eq!(store.read_all(p.raw())?, [0xf4, 0x03]); // the varint grew
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn at<'a, B: WriteBackend>(
        location: Location<B::Pointer, B::Size>,
    ) -> Place<'a, B, Packed> {
        Place::at(location)
    }
}

/// The offsets of a composite value's fields within its encoding, kept by its
/// guard so that each field's guard finds itself after a sibling changes
/// size, and so that a field's change of size becomes the value's own.
///
/// `N` is one more than the most fields the value can have.
///
/// ```
/// use kladde_persist::FieldOffsets;
///
/// let offsets = FieldOffsets::<4>::new();
/// offsets.fill(1, &[2, 3, 4]); // after a one-byte discriminant
/// assert_eq!(offsets.offset(2), 6);
/// assert_eq!(offsets.end(), 10);
/// ```
pub struct FieldOffsets<const N: usize> {
    offsets: [Cell<usize>; N],
    len: Cell<usize>,
}

impl<const N: usize> Default for FieldOffsets<N> {
    fn default() -> Self {
        FieldOffsets {
            offsets: [const { Cell::new(0) }; N],
            len: Cell::new(1),
        }
    }
}

impl<const N: usize> FieldOffsets<N> {
    /// No fields yet. See [`FieldOffsets`] for an example.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records fields of encoded sizes `sizes`, laid out back to back from
    /// `start` bytes into the value. See [`FieldOffsets`] for an example.
    pub fn fill(&self, start: usize, sizes: &[usize]) {
        assert!(sizes.len() < N, "FieldOffsets<{N}> holds {} fields", N - 1);
        let mut at = start;
        self.offsets[0].set(at);
        for (i, size) in sizes.iter().enumerate() {
            at += size;
            self.offsets[i + 1].set(at);
        }
        self.len.set(sizes.len() + 1);
    }

    /// Where field `index` starts within the value.
    pub fn offset(&self, index: usize) -> usize {
        self.offsets[index].get()
    }

    /// Where the last field ends: the value's encoded size.
    pub fn end(&self) -> usize {
        self.offsets[self.len.get() - 1].get()
    }
}

impl<B: WriteBackend, const N: usize> Node<B> for FieldOffsets<N> {
    fn location_of(&self, owner: &Link<'_, B>, index: usize) -> Location<B::Pointer, B::Size> {
        owner.location() + <B::Size as Word>::from_usize(self.offset(index))
    }

    fn resized(
        &self,
        owner: &Link<'_, B>,
        backend: &B,
        index: usize,
        old: usize,
        new: usize,
    ) -> Result<(), Error> {
        // The owner records first, so that a failure leaves the offsets as
        // they were, matching the value, which the child changes only on
        // success.
        let before = self.end();
        owner.resized(backend, before, before + new - old)?;
        for cell in &self.offsets[index + 1..self.len.get()] {
            cell.set(cell.get() + new - old);
        }
        Ok(())
    }
}

/// `n` as the backend's size type, or [`Error::OutOfBounds`] if it does not
/// fit.
pub(crate) fn size<S: Word>(n: usize) -> Result<S, Error> {
    S::try_from_usize(n).ok_or(Error::OutOfBounds)
}

/// Replaces the `old_len` bytes at `location` with `bytes`, shifting what
/// follows: how a value in a packed place takes its new size.
///
/// ```
/// use kladde_persist::{splice_at, Location};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let mut store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let p = store.alloc(0)?;
/// store.write(p.raw(), 0, b"abc")?;
/// splice_at(&store, Location::new(p.raw(), 1), 1, b"xyz")?;
/// store.flush()?;
/// assert_eq!(store.read_all(p.raw())?, b"axyzc");
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub fn splice_at<B: WriteBackend>(
    backend: &B,
    location: Location<B::Pointer, B::Size>,
    old_len: usize,
    bytes: &[u8],
) -> Result<(), Error> {
    // A splice names the allocation by its owner's handle; the value being
    // spliced is part of that allocation's content, which its owner reaches
    // only through this value's guard, so borrowing the handle is sound.
    let anchor = UniquePointer::from_pointer(location.anchor);
    backend.splice(&anchor, location.offset, size(old_len)?, bytes)
}

/// Writes `bytes`, the new encoding of the value at `place` whose current
/// encoding takes `old_size` bytes: in place if the size stays, otherwise as
/// a splice that the place's ancestors hear about, in one transaction.
///
/// This is how a guard of a value in a packed place writes it; in a slotted
/// place, the size never changes.
///
/// ```
/// use kladde_persist::{write_encoded, Location, Packed};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let mut store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let p = store.alloc(2)?;
/// let place = Packed::at(Location::new(p.raw(), 0));
/// write_encoded(&store, &place, 2, b"xyz")?;
/// store.flush()?;
/// assert_eq!(store.read_all(p.raw())?, b"xyz");
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub fn write_encoded<B: WriteBackend, E: Encoding>(
    backend: &B,
    place: &Place<'_, B, E>,
    old_size: usize,
    bytes: &[u8],
) -> Result<(), Error> {
    let location = place.location();
    if old_size == bytes.len() {
        return backend.write(location.anchor, location.offset, bytes);
    }
    backend.atomically(|| {
        splice_at(backend, location, old_size, bytes)?;
        place.resized(backend, old_size, bytes.len())
    })
}
