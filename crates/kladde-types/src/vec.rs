//! [`PersistableVec<T>`]: a vector, encoded as its count and its elements.

use std::ops::Deref;

use kladde_persist::{
    varint_len, Backend, Cut, Decoder, Encoder, Error, Guard, Node, Persistable, Region,
    SchemaBuilder, TypeDescriptor,
};

use crate::index::Sizes;

/// A vector of persistable values: the number of elements as a varint, then
/// the elements.
///
/// In memory, a `Vec<T>`, and for a `T` whose size depends on its value a
/// Fenwick tree of the elements' encoded sizes, four bytes per element, so
/// that the guard of element `i` finds where it starts in `O(log m)`. For a
/// fixed-size `T` the element starts at `i` times its size, and the vector
/// keeps nothing more.
///
/// Read it as a slice, through `Deref<Target = [T]>`; change it through a
/// [`PersistableVecGuard`].
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::{PersistableString, PersistableVec};
///
/// let mut lines = Kladde::new(PersistableVec::<PersistableString>::new());
/// lines.guard().push("hello".into())?;
/// lines.guard().get_mut(0).unwrap().push_str(", world")?;
/// assert_eq!(lines.get()[0], "hello, world");
/// # Ok::<(), kladde::Error>(())
/// ```
pub struct PersistableVec<T> {
    items: Vec<T>,
    /// The elements' sizes, for a `T` whose size depends on its value;
    /// empty otherwise.
    sizes: Sizes,
}

impl<T: Persistable> PersistableVec<T> {
    /// An empty vector.
    ///
    /// ```
    /// let v = kladde_types::PersistableVec::<u32>::new();
    /// assert!(v.is_empty());
    /// ```
    pub fn new() -> Self {
        Self::from_items(Vec::new())
    }

    fn from_items(items: Vec<T>) -> Self {
        let sizes = match T::FIXED_SIZE {
            Some(_) => Sizes::default(),
            None => Sizes::build(
                items.iter().map(T::encoded_len),
                varint_len(items.len() as u64) as u64,
            ),
        };
        PersistableVec { items, sizes }
    }

    /// The elements, as a slice.
    pub fn as_slice(&self) -> &[T] {
        &self.items
    }

    /// The elements, as a `Vec`.
    ///
    /// ```
    /// let v: kladde_types::PersistableVec<u8> = vec![1, 2].into();
    /// assert_eq!(v.into_vec(), [1, 2]);
    /// ```
    pub fn into_vec(self) -> Vec<T> {
        self.items
    }

    /// The bytes the elements take, without the count.
    fn content_len(&self) -> u64 {
        match T::FIXED_SIZE {
            Some(size) => (size * self.items.len()) as u64,
            None => self.sizes.total(),
        }
    }

    /// Where element `i` starts within the vector's encoding; `i` may be the
    /// length, for where an element pushed would start.
    fn offset_of(&self, i: usize) -> u64 {
        let count = varint_len(self.items.len() as u64) as u64;
        count
            + match T::FIXED_SIZE {
                Some(size) => (size * i) as u64,
                None => self.sizes.prefix(i),
            }
    }

    /// Rebuilds the sizes after an insertion, a removal or a reordering.
    fn reindex(&mut self) {
        if T::FIXED_SIZE.is_none() {
            self.sizes = Sizes::build(
                self.items.iter().map(T::encoded_len),
                varint_len(self.items.len() as u64) as u64,
            );
        }
    }

    /// Checks that the sizes the vector keeps match its elements, panicking
    /// otherwise: for tests of guards that change elements' sizes.
    ///
    /// ```
    /// let v: kladde_types::PersistableVec<u32> = vec![1, 300].into();
    /// v.check();
    /// ```
    pub fn check(&self) {
        if T::FIXED_SIZE.is_none() {
            assert_eq!(self.sizes.len(), self.items.len(), "one size per element");
            let mut sum = 0u64;
            for (i, item) in self.items.iter().enumerate() {
                assert_eq!(self.sizes.prefix(i), sum, "where element {i} starts");
                sum += item.encoded_len() as u64;
            }
            assert_eq!(self.sizes.total(), sum, "the elements' total size");
            assert_eq!(
                self.sizes.offset_of(0),
                varint_len(self.items.len() as u64) as u64
            );
        }
    }
}

impl<T: Persistable> Default for PersistableVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Persistable> From<Vec<T>> for PersistableVec<T> {
    fn from(items: Vec<T>) -> Self {
        Self::from_items(items)
    }
}

impl<T: Persistable> FromIterator<T> for PersistableVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self::from_items(iter.into_iter().collect())
    }
}

impl<T> Deref for PersistableVec<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.items
    }
}

impl<'a, T> IntoIterator for &'a PersistableVec<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}

impl<T: Clone> Clone for PersistableVec<T> {
    fn clone(&self) -> Self {
        PersistableVec {
            items: self.items.clone(),
            sizes: self.sizes.clone(),
        }
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for PersistableVec<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.items.fmt(f)
    }
}

impl<T: PartialEq> PartialEq for PersistableVec<T> {
    fn eq(&self, other: &Self) -> bool {
        self.items == other.items
    }
}

impl<T: Eq> Eq for PersistableVec<T> {}

/// At most this many elements are reserved before they are decoded, so that
/// a corrupt count cannot make a decode allocate without bound.
const RESERVE_AT_MOST: usize = 1 << 16;

impl<T: Persistable> Persistable for PersistableVec<T> {
    const FIXED_SIZE: Option<usize> = None;

    fn encoded_len(&self) -> usize {
        varint_len(self.items.len() as u64) + self.content_len() as usize
    }

    fn encode(&self, out: &mut Encoder<'_>) {
        out.varint(self.items.len() as u64);
        for item in &self.items {
            item.encode(out);
        }
    }

    fn decode(input: &mut Decoder<'_>) -> Result<Self, Error> {
        let count = input.varint_as::<usize>()?;
        let mut items = Vec::with_capacity(count.min(RESERVE_AT_MOST));
        for _ in 0..count {
            items.push(T::decode(input)?);
        }
        Ok(Self::from_items(items))
    }

    type Guard<'r, B: Backend + 'r>
        = PersistableVecGuard<'r, T, B>
    where
        Self: 'r;

    fn guard<'r, B: Backend + 'r>(&'r mut self, region: Region<'r, B>) -> Self::Guard<'r, B> {
        PersistableVecGuard {
            inner: self,
            region,
        }
    }

    fn describe_local(builder: &mut SchemaBuilder) -> TypeDescriptor
    where
        Self: 'static,
    {
        TypeDescriptor::Sequence(T::describe(builder))
    }
}

/// Writes the count `new` over the count `old` at the start of `region`.
pub(crate) fn write_count<B: Backend>(
    region: &Region<'_, B>,
    old: usize,
    new: usize,
) -> Result<(), Error> {
    let mut bytes = Vec::with_capacity(10);
    Encoder::new(&mut bytes).varint(new as u64);
    region.splice(0, varint_len(old as u64) as u64, &bytes)?;
    Ok(())
}

/// The [`Guard`] of a [`PersistableVec`]: insert, remove, reorder and cut
/// elements, and reach each element's own guard.
///
/// An insertion or a removal is one transaction of two records, the element
/// and the count; a reordering is one `Move`, which writes none of the
/// element's bytes. Each changes memory only after the store recorded it.
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::PersistableVec;
///
/// let mut v = Kladde::new(PersistableVec::<u32>::from(vec![1, 2, 3]));
/// let mut guard = v.guard();
/// guard.push(4)?;
/// guard.move_element(3, 0)?;
/// assert_eq!(guard.remove(1)?, 1);
/// guard.get_mut(0).unwrap().set(400)?;
/// assert_eq!(v.get().as_slice(), [400, 2, 3]);
/// # Ok::<(), kladde::Error>(())
/// ```
pub struct PersistableVecGuard<'r, T, B> {
    inner: &'r mut PersistableVec<T>,
    region: Region<'r, B>,
}

impl<'r, T: Persistable, B: Backend> PersistableVecGuard<'r, T, B> {
    fn check_index(&self, index: usize, len: usize) -> Result<(), Error> {
        if index < len {
            Ok(())
        } else {
            Err(Error::OutOfBounds)
        }
    }

    /// The guard of element `index`, or `None` past the end. Finds where the
    /// element starts in `O(log m)`, or by a multiplication for a fixed-size
    /// `T`. See [`PersistableVecGuard`] for an example.
    pub fn get_mut(&mut self, index: usize) -> Option<T::Guard<'_, B>> {
        if index >= self.inner.items.len() {
            return None;
        }
        let offset = self.inner.offset_of(index);
        let PersistableVec { items, sizes } = &mut *self.inner;
        let item = &mut items[index];
        let len = item.encoded_len() as u64;
        let node: Option<&dyn Node> = match T::FIXED_SIZE {
            Some(_) => None,
            None => Some(&*sizes),
        };
        Some(item.guard(self.region.child(offset, len, node, index)))
    }

    /// Appends `value`. See [`PersistableVecGuard`] for an example.
    pub fn push(&mut self, value: T) -> Result<(), Error> {
        let n = self.inner.items.len();
        self.insert(n, value)
    }

    /// Inserts `value` before element `index`, shifting the elements after
    /// it, in `O(m)` time in memory and two records in the file.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableVec;
    ///
    /// let mut v = Kladde::new(PersistableVec::<i32>::from(vec![1, 3]));
    /// v.guard().insert(1, 2)?;
    /// assert_eq!(v.get().as_slice(), [1, 2, 3]);
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn insert(&mut self, index: usize, value: T) -> Result<(), Error> {
        let n = self.inner.items.len();
        self.check_index(index, n + 1)?;
        let at = self.inner.offset_of(index);
        let bytes = value.to_bytes();
        self.region.transaction(|r| {
            r.splice(at, 0, &bytes)?;
            write_count(r, n, n + 1)
        })?;
        let v = &mut *self.inner;
        if index == n && T::FIXED_SIZE.is_none() {
            v.sizes.push(bytes.len());
            v.sizes.set_base(varint_len(n as u64 + 1) as u64);
            v.items.push(value);
        } else {
            v.items.insert(index, value);
            v.reindex();
        }
        Ok(())
    }

    /// Removes and returns the last element, or `None` if there is none.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableVec;
    ///
    /// let mut v = Kladde::new(PersistableVec::<u8>::from(vec![1, 2]));
    /// assert_eq!(v.guard().pop()?, Some(2));
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn pop(&mut self) -> Result<Option<T>, Error> {
        match self.inner.items.len() {
            0 => Ok(None),
            n => self.remove(n - 1).map(Some),
        }
    }

    /// Removes element `index` and returns it, shifting the elements after
    /// it. See [`PersistableVecGuard`] for an example.
    pub fn remove(&mut self, index: usize) -> Result<T, Error> {
        let n = self.inner.items.len();
        self.check_index(index, n)?;
        let at = self.inner.offset_of(index);
        let len = self.inner.items[index].encoded_len() as u64;
        self.region.transaction(|r| {
            r.splice(at, len, &[])?;
            write_count(r, n, n - 1)
        })?;
        Ok(self.take(index))
    }

    /// Removes element `index` from memory, after its bytes were spliced out.
    fn take(&mut self, index: usize) -> T {
        let v = &mut *self.inner;
        if index + 1 == v.items.len() && T::FIXED_SIZE.is_none() {
            let value = v.items.pop().unwrap();
            v.sizes.pop(value.encoded_len());
            v.sizes.set_base(varint_len(v.items.len() as u64) as u64);
            value
        } else {
            let value = v.items.remove(index);
            v.reindex();
            value
        }
    }

    /// Removes element `index` and drops it.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableVec;
    ///
    /// let mut v = Kladde::new(PersistableVec::<u8>::from(vec![1, 2]));
    /// v.guard().delete(0)?;
    /// assert_eq!(v.get().as_slice(), [2]);
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn delete(&mut self, index: usize) -> Result<(), Error> {
        self.remove(index).map(drop)
    }

    /// Moves element `from` to position `to`, shifting the elements between:
    /// one `Move` in the file, which writes none of the element's bytes,
    /// however large it is. See [`PersistableVecGuard`] for an example.
    pub fn move_element(&mut self, from: usize, to: usize) -> Result<(), Error> {
        let n = self.inner.items.len();
        self.check_index(from, n)?;
        self.check_index(to, n)?;
        if from == to {
            return Ok(());
        }
        let start = self.inner.offset_of(from);
        let len = self.inner.items[from].encoded_len() as u64;
        // Where the element goes, in the vector without it.
        let dest = if to < from {
            self.inner.offset_of(to)
        } else {
            self.inner.offset_of(to + 1) - len
        };
        self.region.move_bytes(start, len, dest)?;
        let v = &mut *self.inner;
        let value = v.items.remove(from);
        v.items.insert(to, value);
        v.reindex();
        Ok(())
    }

    /// Removes every element.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableVec;
    ///
    /// let mut v = Kladde::new(PersistableVec::<u8>::from(vec![1, 2]));
    /// v.guard().clear()?;
    /// assert!(v.get().is_empty());
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn clear(&mut self) -> Result<(), Error> {
        self.set(PersistableVec::new())
    }

    /// Replaces the whole vector.
    pub fn set(&mut self, value: PersistableVec<T>) -> Result<(), Error> {
        kladde_persist::replace(self.inner, value, &self.region)
    }

    /// Cuts element `index` out, for a paste elsewhere in the same
    /// operation without encoding it again: see [`Cut`]. The cut holds a
    /// transaction open until it is pasted, committed or dropped.
    ///
    /// ```
    /// use kladde::{Kladde, Persistable};
    /// use kladde_types::{PersistableString, PersistableVec};
    ///
    /// #[derive(Persistable)]
    /// struct Groups {
    ///     a: PersistableVec<PersistableString>,
    ///     b: PersistableVec<PersistableString>,
    /// }
    ///
    /// let mut groups = Kladde::new(Groups {
    ///     a: vec!["circle".into(), "square".into()].into(),
    ///     b: PersistableVec::new(),
    /// });
    /// let mut guard = groups.guard();
    /// let cut = guard.a_mut().cut(1)?;
    /// assert_eq!(*cut, "square");
    /// guard.b_mut().push_cut(cut)?;
    /// assert_eq!(groups.get().b[0], "square");
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn cut(&mut self, index: usize) -> Result<Cut<T, B>, Error> {
        let n = self.inner.items.len();
        self.check_index(index, n)?;
        let at = self.inner.offset_of(index);
        let len = self.inner.items[index].encoded_len() as u64;
        let backend = self.region.backend();
        backend.begin_transaction()?;
        let id = self.region.transaction(|r| {
            let id = r.splice(at, len, &[])?;
            write_count(r, n, n - 1)?;
            Ok(id)
        });
        match id {
            Ok(id) => Ok(Cut::new(self.take(index), id, backend)),
            Err(e) => {
                let _ = backend.end_transaction();
                Err(e)
            }
        }
    }

    /// Inserts the value `cut` holds before element `index`, pasting its
    /// bytes rather than encoding it again, and ends the transaction the
    /// cut held open. See [`cut`](Self::cut) for an example.
    pub fn insert_cut(&mut self, index: usize, cut: Cut<T, B>) -> Result<(), Error> {
        let n = self.inner.items.len();
        self.check_index(index, n + 1)?;
        let at = self.inner.offset_of(index);
        let region = &self.region;
        let value = cut.paste_with(|id| {
            region.transaction(|r| {
                if let Some(id) = id {
                    r.paste(at, 0, id)?;
                }
                write_count(r, n, n + 1)
            })
        })?;
        let v = &mut *self.inner;
        v.items.insert(index, value);
        v.reindex();
        Ok(())
    }

    /// Appends the value `cut` holds. See [`cut`](Self::cut) for an example.
    pub fn push_cut(&mut self, cut: Cut<T, B>) -> Result<(), Error> {
        let n = self.inner.items.len();
        self.insert_cut(n, cut)
    }
}

impl<T, B> Guard for PersistableVecGuard<'_, T, B> {
    type Persistable = PersistableVec<T>;

    fn as_persistable(&self) -> &PersistableVec<T> {
        self.inner
    }
}

impl<T, B> Deref for PersistableVecGuard<'_, T, B> {
    type Target = PersistableVec<T>;

    fn deref(&self) -> &PersistableVec<T> {
        self.inner
    }
}
