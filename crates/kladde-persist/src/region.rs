//! [`Region`]: the substring of the sequence that holds one value, and the
//! links through which a value that changes size tells the values around
//! it.
//!
//! A `Mut` holds its value's region. A region knows its length, and how to
//! find where it starts: a `Mut` made on its own holds its start, relative
//! to the nearest `Mut` above it that was made together with its siblings
//! (or to the root), since nothing between the two can change size while it
//! lives; a `Mut` made together with its siblings by `parts` asks its
//! parent's [`Node`] for its start at every write, since a sibling before it
//! may have changed size meanwhile.
//!
//! When a value changes size, its region tells its parent's region, and the
//! parent's [`Node`], which keeps where the parent's children start; the
//! parent's region tells its own parent in turn, up to the root. These
//! reports change memory only, after the store recorded the change.

use std::cell::Cell;

use kladde_store::{CutId, Error, WriteBackend};

use crate::durable::Durable;

/// What a `Mut` records through: a [`WriteBackend`] that is cheap to copy,
/// such as `&Store`, so that `Mut`s can hand it to their children and to
/// what outlives them, such as a [`Cut`](crate::Cut).
pub trait Backend: WriteBackend + Copy {}

impl<B: WriteBackend + Copy> Backend for B {}

/// What a value whose children may change size keeps about them, in cells,
/// so that a child's report can update it while sibling `Mut`s hold shared
/// references to it.
///
/// A vector of variable-size elements keeps a tree of their sizes; a `Mut`
/// that hands out all of a struct's fields at once keeps their sizes in
/// [`FieldSizes`].
///
/// ```
/// use kladde_persist::{FieldSizes, Node};
///
/// let mut fields = FieldSizes::new();
/// fields.fill(1, &[None, None], &[4, 2]); // after a one-byte discriminant
/// assert_eq!(fields.offset_of(1), 5);
/// fields.resized(0, 4, 6);
/// assert_eq!(fields.offset_of(1), 7);
/// ```
pub trait Node {
    /// Where child `index` starts, relative to the start of the value that
    /// keeps this node.
    fn offset_of(&self, index: usize) -> u64;

    /// Records that child `index` changed its encoded size from `old` to
    /// `new` bytes. Called after the store recorded the change.
    fn resized(&self, index: usize, old: u64, new: u64);
}

/// The encoded sizes of a struct's fields, or of an enum variant's, kept by
/// a `Mut` that hands out `Mut`s for all of them at once, so that each
/// finds its start however its siblings change size.
///
/// See [`Node`] for an example.
#[derive(Default)]
pub struct FieldSizes {
    /// Where the first field starts within the value.
    base: Cell<u64>,
    sizes: Vec<Cell<u64>>,
    /// How many fields, from the first, start at a constant offset: those
    /// with fixed-size fields only before them.
    constant: Cell<usize>,
}

impl FieldSizes {
    /// No fields yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records fields of the encoded sizes `sizes`, laid out back to back
    /// from `base` bytes into the value, whose types have the fixed sizes
    /// `fixed`. See [`Node`] for an example.
    pub fn fill(&mut self, base: u64, fixed: &[Option<usize>], sizes: &[usize]) {
        self.base.set(base);
        self.sizes.clear();
        self.sizes
            .extend(sizes.iter().map(|&size| Cell::new(size as u64)));
        let constant = fixed
            .iter()
            .position(Option::is_none)
            .map_or(fixed.len(), |i| i + 1);
        self.constant.set(constant);
    }

    /// Whether field `index` starts at a constant offset.
    pub fn is_constant(&self, index: usize) -> bool {
        index < self.constant.get()
    }

    /// The encoded size of field `index`.
    pub fn size(&self, index: usize) -> u64 {
        self.sizes[index].get()
    }
}

impl Node for FieldSizes {
    fn offset_of(&self, index: usize) -> u64 {
        self.base.get() + self.sizes[..index].iter().map(Cell::get).sum::<u64>()
    }

    fn resized(&self, index: usize, _old: u64, new: u64) {
        self.sizes[index].set(new);
    }
}

/// Where a region starts: the root's start, a constant offset from an
/// anchor whose start may move, or wherever its parent's node says.
enum Start<'a> {
    Root,
    Offset { anchor: &'a Link<'a>, offset: u64 },
    Indexed,
}

/// Whom a region tells when it changes size: its parent's region, and the
/// node through which the parent keeps where its children start.
struct Parent<'a> {
    node: Option<&'a (dyn Node + 'a)>,
    owner: &'a Link<'a>,
    index: usize,
}

/// The untyped part of a region: its length, where it starts, and whom it
/// tells.
struct Link<'a> {
    len: Cell<u64>,
    start: Start<'a>,
    parent: Option<Parent<'a>>,
}

impl Link<'_> {
    fn start(&self) -> u64 {
        match &self.start {
            Start::Root => 0,
            Start::Offset { anchor, offset } => anchor.start() + offset,
            Start::Indexed => {
                let p = self
                    .parent
                    .as_ref()
                    .expect("an indexed region has a parent");
                let node = p.node.expect("an indexed region has a node");
                p.owner.start() + node.offset_of(p.index)
            }
        }
    }

    fn resized(&self, old: u64, new: u64) {
        if old == new {
            return;
        }
        self.len.set(new);
        if let Some(p) = &self.parent {
            if let Some(node) = p.node {
                node.resized(p.index, old, new);
            }
            let before = p.owner.len.get();
            p.owner.resized(before, before - old + new);
        }
    }
}

/// The substring of the sequence that holds one value's encoding: what a
/// value's `Mut` writes through.
///
/// Every offset is relative to the region's start, and every method refuses
/// one outside the region with [`Error::OutOfBounds`], recording nothing. A
/// method that changes the region's size reports the change to the regions
/// around it once the store has recorded it; a `Mut` updates its in-memory
/// value after its records succeeded, so that a failure leaves the value as
/// it was.
///
/// ```
/// use kladde_persist::Region;
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let root = Region::root(&store);
/// root.splice(0, 0, b"hello")?;
/// {
///     let child = root.child(1, 3, None, 0); // "ell"
///     child.splice(1, 1, b"EE")?;
///     assert_eq!(child.len(), 4);
/// }
/// assert_eq!(root.len(), 6);
/// store.flush()?;
/// assert_eq!(store.read_all()?, b"heEElo");
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub struct Region<'r, B> {
    backend: B,
    link: Link<'r>,
}

impl<B: Backend> Region<'static, B> {
    /// The region of the whole sequence: the root value's. See [`Region`]
    /// for an example.
    pub fn root(backend: B) -> Self {
        Region {
            link: Link {
                len: Cell::new(backend.len()),
                start: Start::Root,
                parent: None,
            },
            backend,
        }
    }
}

impl<'r, B: Backend> Region<'r, B> {
    /// The backend the region records through.
    pub fn backend(&self) -> B {
        self.backend
    }

    /// The region's length in bytes: the value's encoded size.
    pub fn len(&self) -> u64 {
        self.link.len.get()
    }

    /// Whether the region is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Where the region starts in the sequence, now.
    ///
    /// ```
    /// use kladde_persist::Region;
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// store.splice(0, 0, b"abc")?;
    /// let root = Region::root(&store);
    /// assert_eq!(root.child(2, 1, None, 0).start(), 2);
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn start(&self) -> u64 {
        self.link.start()
    }

    fn check(&self, at: u64, len: u64) -> Result<(), Error> {
        match at.checked_add(len) {
            Some(end) if end <= self.len() => Ok(()),
            _ => Err(Error::OutOfBounds),
        }
    }

    /// Overwrites `bytes.len()` bytes at `at`, and returns the cut of the
    /// overwritten bytes, as [`WriteBackend::write`] does.
    pub fn write(&self, at: u64, bytes: &[u8]) -> Result<Option<CutId>, Error> {
        self.check(at, bytes.len() as u64)?;
        self.backend.write(self.start() + at, bytes)
    }

    /// Replaces the `removed` bytes at `at` with `bytes`, shifting what
    /// follows, or overwriting them if the lengths agree, and returns the cut
    /// of the removed bytes, as [`WriteBackend::splice`] does.
    ///
    /// ```
    /// use kladde_persist::Region;
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// store.splice(0, 0, b"abc")?;
    /// let root = Region::root(&store);
    /// root.splice(1, 1, b"B")?; // a write
    /// root.splice(2, 1, b"CC")?; // grows the region
    /// assert_eq!(root.len(), 4);
    /// store.flush()?;
    /// assert_eq!(store.read_all()?, b"aBCC");
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn splice(&self, at: u64, removed: u64, bytes: &[u8]) -> Result<Option<CutId>, Error> {
        self.check(at, removed)?;
        let cut = self.backend.splice(self.start() + at, removed, bytes)?;
        let len = self.len();
        self.link.resized(len, len - removed + bytes.len() as u64);
        Ok(cut)
    }

    /// Replaces the `removed` bytes at `at` with `value`'s encoding, and
    /// returns its length.
    ///
    /// ```
    /// use kladde_persist::Region;
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// let root = Region::root(&store);
    /// assert_eq!(root.splice_value(0, 0, &300u32)?, 2);
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn splice_value<T: Durable>(
        &self,
        at: u64,
        removed: u64,
        value: &T,
    ) -> Result<usize, Error> {
        let bytes = value.to_bytes();
        self.splice(at, removed, &bytes)?;
        Ok(bytes.len())
    }

    /// Moves the `len` bytes at `from` to `to`, a position in the region
    /// without them, writing none of them.
    pub fn move_bytes(&self, from: u64, len: u64, to: u64) -> Result<(), Error> {
        self.check(from, len)?;
        self.check(to, len)?;
        let start = self.start();
        self.backend.move_range(start + from, len, start + to)
    }

    /// Replaces the `removed` bytes at `at` with the bytes of `cut`, whose
    /// encoding `bytes` is, as [`WriteBackend::paste`] does: writing none of
    /// them while the cut's journal lasts, and `bytes` once it has been
    /// folded.
    pub fn paste(&self, at: u64, removed: u64, cut: CutId, bytes: &[u8]) -> Result<(), Error> {
        self.check(at, removed)?;
        let added = cut.len();
        self.backend.paste(self.start() + at, removed, cut, bytes)?;
        let len = self.len();
        self.link.resized(len, len - removed + added);
        Ok(())
    }

    /// Runs `f` as one transaction of the store.
    ///
    /// If `f` fails, the store discards what it recorded, and is poisoned,
    /// since the regions around may have heard of size changes by then: a
    /// `Mut` checks what can fail before it opens a transaction, so that
    /// only a broken invariant or the store's own failure gets here.
    pub fn transaction<R>(&self, f: impl FnOnce(&Self) -> Result<R, Error>) -> Result<R, Error> {
        let r = self.backend.atomically(|| f(self));
        if r.is_err() {
            self.backend.poison();
        }
        r
    }

    /// The region of a child made on its own: the `len` bytes at `at`,
    /// child number `index` of this value, whose `node`, if it keeps one,
    /// hears when the child changes size.
    ///
    /// The child borrows this region, so this value cannot write while the
    /// child exists, and the child's start stays constant relative to the
    /// nearest region above made by [`together`](Region::together).
    pub fn child<'b>(
        &'b self,
        at: u64,
        len: u64,
        node: Option<&'b (dyn Node + 'b)>,
        index: usize,
    ) -> Region<'b, B> {
        debug_assert!(at + len <= self.len(), "a child inside its parent");
        let start = match self.link.start {
            Start::Offset { anchor, offset } => Start::Offset {
                anchor,
                offset: offset + at,
            },
            Start::Root | Start::Indexed => Start::Offset {
                anchor: &self.link,
                offset: at,
            },
        };
        Region {
            backend: self.backend,
            link: Link {
                len: Cell::new(len),
                start,
                parent: Some(Parent {
                    node,
                    owner: &self.link,
                    index,
                }),
            },
        }
    }

    /// The region of child number `index`, made together with its siblings:
    /// `node` says where it starts whenever it writes, so a sibling before
    /// it may change size meanwhile.
    pub fn together<'b>(
        &'b self,
        len: u64,
        node: &'b (dyn Node + 'b),
        index: usize,
    ) -> Region<'b, B> {
        Region {
            backend: self.backend,
            link: Link {
                len: Cell::new(len),
                start: Start::Indexed,
                parent: Some(Parent {
                    node: Some(node),
                    owner: &self.link,
                    index,
                }),
            },
        }
    }

    /// The region of field `index` of the fields `fields` holds, made
    /// together with its siblings: at a constant offset if only fixed-size
    /// fields precede it, and indexed otherwise.
    pub fn field<'b>(&'b self, fields: &'b FieldSizes, index: usize) -> Region<'b, B> {
        if fields.is_constant(index) {
            self.child(
                fields.offset_of(index),
                fields.size(index),
                Some(fields),
                index,
            )
        } else {
            self.together(fields.size(index), fields, index)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kladde_store::{MemoryStorage, Store};

    fn store(bytes: &[u8]) -> Store {
        let store = Store::create(Box::new(MemoryStorage::new()), Default::default()).unwrap();
        store.splice(0, 0, bytes).unwrap();
        store
    }

    #[test]
    fn regions_refuse_what_lies_outside() {
        let store = store(b"abcdef");
        let root = Region::root(&store);
        let child = root.child(2, 2, None, 0);
        assert!(child.write(1, b"xy").is_err());
        assert!(child.splice(3, 0, b"x").is_err());
        assert!(child.move_bytes(0, 1, 2).is_err());
        child.write(0, b"CD").unwrap();
        store.flush().unwrap();
        assert_eq!(store.read_all().unwrap(), b"abCDef");
    }

    #[test]
    fn siblings_made_together_find_their_starts_after_each_other_grows() {
        let store = store(b"[aa|bb]");
        let root = Region::root(&store);
        let mut fields = FieldSizes::new();
        fields.fill(1, &[None, None], &[3, 2]);
        let outer = root.child(0, 7, None, 0);
        let a = outer.together(3, &fields, 0);
        let b = outer.together(2, &fields, 1);
        a.splice(0, 0, b"AA").unwrap();
        assert_eq!(fields.offset_of(1), 6);
        assert_eq!(outer.len(), 9);
        assert_eq!(root.len(), 9);
        b.write(0, b"BB").unwrap();
        store.flush().unwrap();
        assert_eq!(store.read_all().unwrap(), b"[AAaa|BB]");
    }

    #[test]
    fn a_child_below_a_region_made_together_follows_it() {
        let store = store(b"xxabcd");
        let root = Region::root(&store);
        let mut fields = FieldSizes::new();
        fields.fill(0, &[None, None], &[2, 4]);
        let first = root.together(2, &fields, 0);
        let second = root.together(4, &fields, 1);
        let inner = second.child(1, 2, None, 0); // "bc"
        let deeper = inner.child(1, 1, None, 0); // "c"
        first.splice(0, 2, b"").unwrap();
        deeper.write(0, b"C").unwrap();
        deeper.splice(1, 0, b"!").unwrap();
        assert_eq!(fields.size(1), 5);
        assert_eq!(root.len(), 5);
        store.flush().unwrap();
        assert_eq!(store.read_all().unwrap(), b"abC!d");
    }
}
