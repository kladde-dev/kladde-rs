//! [`PackedPersistableVec`] -- a vector whose elements are laid out packed.
//!
//! Layout: the content allocation's pointer inline (null while there is
//! none), and the content allocation holding the elements back to back in
//! their packed encodings, each as long as its value needs: an enum takes
//! only its current variant, an integer is a varint. Nothing else is stored,
//! no count and no offsets; a load decodes elements until the allocation
//! ends, and keeps each element's offset in memory.
//!
//! A guard of an element finds itself through those offsets, and an element
//! whose encoding changes size splices it in and has the vector shift the
//! offsets behind it.

use std::cell::Cell;
use std::ops::Deref;

use kladde_persist::{
    read_allocation, replace, Encoding, Error, Guard, Input, Link, Location, Node, Packed,
    Persistable, Place, Pointer, PointerRepr, ReadBackend, Slotted, UniquePointer, Word,
    WriteBackend,
};

use crate::slot::{decode_pointer, encode_pointer, pointer_size, publish_pointer, size};

/// A growable array whose contents are persisted packed: each element takes
/// as many bytes as its value needs.
///
/// It reads like a [`PersistableVec`](crate::PersistableVec), through
/// `Deref<Target = [T]>`, and mutates through a
/// [`PackedPersistableVecGuard`] with the same methods. What differs is the
/// file: an enum element takes its current variant rather than its largest,
/// integers are varints, and a `char` is UTF-8. The price is that an element
/// whose encoding changes size moves every element behind it, a splice
/// where a slotted vector writes in place, and that the vector keeps each
/// element's offset in memory.
///
/// ```
/// use kladde::{Kladde, Persistable};
/// use kladde_types::PackedPersistableVec;
///
/// #[derive(Persistable)]
/// enum Step {
///     Stop,
///     Go { x: f32, y: f32 },
/// }
///
/// let mut path = Kladde::new(PackedPersistableVec::<Step>::new());
/// path.guard().push(Step::Go { x: 1.0, y: 2.0 })?;
/// path.guard().push(Step::Stop)?;
/// path.guard().get_mut(1).unwrap().set(Step::Go { x: 3.0, y: 4.0 })?;
/// assert!(matches!(path.get()[1], Step::Go { x: 3.0, .. }));
/// # Ok::<(), kladde::Error>(())
/// ```
pub struct PackedPersistableVec<T, P = Pointer> {
    data: Vec<T>,
    seq: Sequence<P>,
}

/// What a packed vector keeps beside its elements: its allocation, and where
/// each element starts in it. It is the [`Node`] its elements' guards link
/// to, kept apart from the elements so that a guard can borrow one element
/// mutably and this immutably.
pub(crate) struct Sequence<P> {
    /// The content allocation, `None` until something needs one.
    pointer: Option<UniquePointer<P>>,
    /// Where each element starts, and after the last, where the content
    /// ends: one more entry than there are elements, once the vector is
    /// stored. Empty while it is not.
    offsets: Vec<Cell<usize>>,
}

impl<P> Sequence<P> {
    fn new() -> Self {
        Sequence {
            pointer: None,
            offsets: Vec::new(),
        }
    }

    fn offset(&self, index: usize) -> usize {
        self.offsets[index].get()
    }

    /// The content's size.
    fn end(&self) -> usize {
        self.offsets.last().map_or(0, Cell::get)
    }

    /// Records an element of `len` bytes inserted at `index`.
    fn insert(&mut self, index: usize, len: usize) {
        if self.offsets.is_empty() {
            self.offsets.push(Cell::new(0));
        }
        let at = self.offset(index);
        self.offsets.insert(index, Cell::new(at));
        self.shift(index + 1, len as isize);
    }

    /// Records the removal of element `index`.
    fn remove(&mut self, index: usize) {
        let len = self.offset(index + 1) - self.offset(index);
        self.offsets.remove(index);
        self.shift(index, -(len as isize));
    }

    /// Moves every offset from `from` on by `delta` bytes.
    fn shift(&self, from: usize, delta: isize) {
        for cell in &self.offsets[from..] {
            cell.set(cell.get().wrapping_add_signed(delta));
        }
    }
}

impl<P: PointerRepr, B: WriteBackend<Pointer = P>> Node<B> for Sequence<P> {
    fn location_of(&self, _owner: &Link<'_, B>, index: usize) -> Location<P, B::Size> {
        let pointer = self
            .pointer
            .as_ref()
            .expect("a vec with elements has content");
        Location::new(
            pointer.raw(),
            <B::Size as Word>::from_usize(self.offset(index)),
        )
    }

    /// The element has spliced its new encoding into the content allocation,
    /// whose size the store tracks; the vector's own encoding, its pointer,
    /// stays as it is.
    fn resized(
        &self,
        _owner: &Link<'_, B>,
        _backend: &B,
        index: usize,
        old: usize,
        new: usize,
    ) -> Result<(), Error> {
        self.shift(index + 1, new as isize - old as isize);
        Ok(())
    }
}

/// The packed size of an element, refusing a zero-sized element type, whose
/// count the layout could not recover.
fn packed_size<T: Persistable<P>, P: PointerRepr>(item: &T) -> usize {
    let size = item.encoded_size::<Packed>();
    assert!(
        T::PACKED_SIZE != Some(0),
        "PackedPersistableVec cannot hold a zero-sized element type: its length is \
         recovered from the content allocation's size",
    );
    size
}

impl<T, P> PackedPersistableVec<T, P> {
    /// An empty vec, holding no allocation until something is pushed.
    ///
    /// ```
    /// use kladde_types::PackedPersistableVec;
    ///
    /// let v = PackedPersistableVec::<u32>::new();
    /// assert!(v.is_empty());
    /// ```
    pub fn new() -> Self {
        PackedPersistableVec {
            data: Vec::new(),
            seq: Sequence::new(),
        }
    }

    /// The elements, as a slice. The same as dereferencing.
    ///
    /// ```
    /// use kladde_types::PackedPersistableVec;
    ///
    /// let v: PackedPersistableVec<u16> = [1, 2].into_iter().collect();
    /// assert_eq!(v.as_slice(), &[1, 2]);
    /// ```
    pub fn as_slice(&self) -> &[T] {
        &self.data
    }

    /// How many bytes the elements take in the content allocation, as of the
    /// last time the vector was stored or changed through a guard.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PackedPersistableVec;
    ///
    /// let v: PackedPersistableVec<u32> = [1, 300, 70_000].into_iter().collect();
    /// let db = Kladde::new(v);
    /// assert_eq!(db.get().content_size(), 1 + 2 + 3);
    /// ```
    pub fn content_size(&self) -> usize {
        self.seq.end()
    }
}

impl<T, P: Copy> PackedPersistableVec<T, P> {
    fn raw_pointer(&self) -> Option<P> {
        self.seq.pointer.as_ref().map(|p| p.raw())
    }
}

impl<T: std::fmt::Debug, P> std::fmt::Debug for PackedPersistableVec<T, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PackedPersistableVec")
            .field(&self.data)
            .finish()
    }
}

/// Compares the elements only, not where they are stored.
impl<T: PartialEq, P> PartialEq for PackedPersistableVec<T, P> {
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data
    }
}

impl<T, P> Deref for PackedPersistableVec<T, P> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.data
    }
}

impl<T, P> Default for PackedPersistableVec<T, P> {
    fn default() -> Self {
        Self::new()
    }
}

/// Collects into a vec that holds no allocation yet: storing it, as part of a
/// value that is itself stored, creates its content allocation.
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::PackedPersistableVec;
///
/// let squares: PackedPersistableVec<u32> = (1..4).map(|i| i * i).collect();
/// let db = Kladde::new(squares);
/// assert_eq!(db.get().as_slice(), &[1, 4, 9]);
/// ```
impl<T, P> FromIterator<T> for PackedPersistableVec<T, P> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        PackedPersistableVec {
            data: Vec::from_iter(iter),
            seq: Sequence::new(),
        }
    }
}

impl<'a, T, P> IntoIterator for &'a PackedPersistableVec<T, P> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.data.iter()
    }
}

impl<T: Persistable<P>, P: PointerRepr> Persistable<P> for PackedPersistableVec<T, P> {
    /// Just the content allocation's pointer, as for a slotted vector.
    const SLOTTED_SIZE: Option<usize> = Some(P::BYTE_LEN);
    const PACKED_SIZE: Option<usize> = Some(P::BYTE_LEN);

    type Guard<'s, B: WriteBackend<Pointer = P>, E: Encoding>
        = PackedPersistableVecGuard<'s, T, B, E>
    where
        Self: 's,
        B: 's;

    #[inline]
    fn guard<'s, B: WriteBackend<Pointer = P>, E: Encoding>(
        &'s mut self,
        backend: &'s B,
        place: Place<'s, B, E>,
    ) -> Self::Guard<'s, B, E> {
        PackedPersistableVecGuard {
            inner: self,
            backend,
            place,
        }
    }

    fn encoded_size<E: Encoding>(&self) -> usize {
        pointer_size::<P, E>(self.raw_pointer())
    }

    /// The content pointer. A vec with elements but no allocation, collected
    /// from an iterator, must be [prepared](Persistable::prepare) first.
    fn encode<E: Encoding>(&self, out: &mut Vec<u8>) {
        debug_assert!(self.seq.pointer.is_some() || self.data.is_empty());
        encode_pointer::<P, E>(self.raw_pointer(), out);
    }

    fn decode<B: ReadBackend<Pointer = P>, E: Encoding>(
        backend: &mut B,
        input: &mut Input<'_>,
    ) -> Result<Self, Error> {
        let Some(target) = decode_pointer::<P, E>(input)? else {
            return Ok(PackedPersistableVec::new());
        };
        let bytes = read_allocation(backend, target)?;
        let mut content = Input::new(&bytes);
        let mut data = Vec::new();
        let mut offsets = vec![Cell::new(0)];
        while !content.is_empty() {
            data.push(T::decode::<B, Packed>(backend, &mut content)?);
            offsets.push(Cell::new(content.position()));
        }
        Ok(PackedPersistableVec {
            data,
            seq: Sequence {
                pointer: Some(UniquePointer::from_pointer(target)),
                offsets,
            },
        })
    }

    /// A vec that already has an allocation keeps it, since guards keep its
    /// content current; one with elements but no allocation, collected from
    /// an iterator, gets one here, filled with one write.
    fn prepare<B: WriteBackend<Pointer = P>>(&mut self, backend: &B) -> Result<(), Error> {
        if self.seq.pointer.is_none() && !self.data.is_empty() {
            let pointer = backend.alloc(size(0)?)?;
            let mut bytes = Vec::new();
            let mut offsets = vec![Cell::new(0)];
            for item in &mut self.data {
                item.prepare(backend)?;
                packed_size(item);
                item.encode::<Packed>(&mut bytes);
                offsets.push(Cell::new(bytes.len()));
            }
            backend.write(pointer.raw(), size(0)?, &bytes)?;
            self.seq = Sequence {
                pointer: Some(pointer),
                offsets,
            };
        }
        Ok(())
    }

    /// Frees every element, then the content allocation.
    fn free<B: WriteBackend<Pointer = P>>(&mut self, backend: &B) -> Result<(), Error> {
        for item in &mut self.data {
            item.free(backend)?;
        }
        if let Some(pointer) = self.seq.pointer.take() {
            backend.free(pointer)?;
        }
        Ok(())
    }

    fn describe_local(builder: &mut kladde_persist::SchemaBuilder) -> kladde_persist::TypeDescriptor
    where
        Self: 'static,
    {
        kladde_persist::TypeDescriptor::Opaque {
            library_name: "kladde-types".into(),
            type_name: "PackedPersistableVec".into(),
            version: crate::library_version(),
            inline_size: P::BYTE_LEN as u64,
            parameters: vec![<T as Persistable<P>>::describe(builder)],
        }
    }
}

/// The mutation-capable view onto a [`PackedPersistableVec`]: the methods of
/// a [`PersistableVecGuard`](crate::PersistableVecGuard), each one
/// transaction.
///
/// An insertion, removal, or change of an element's size in the middle is
/// one splice that moves every element behind it; at the end, it is one
/// write or one resize, as for a slotted vector.
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::PackedPersistableVec;
///
/// let mut db = Kladde::new(PackedPersistableVec::<u32>::new());
/// let mut v = db.guard();
/// v.push(1)?;
/// v.push(2)?;
/// v.get_mut(0).unwrap().set(1_000_000)?; // three bytes longer now
/// v.insert(1, 7)?;
/// assert_eq!(v.remove(2)?, 2);
/// assert_eq!(db.get().as_slice(), &[1_000_000, 7]);
/// # Ok::<(), kladde::Error>(())
/// ```
pub struct PackedPersistableVecGuard<'s, T, B: WriteBackend, E: Encoding = Slotted> {
    inner: &'s mut PackedPersistableVec<T, B::Pointer>,
    backend: &'s B,
    place: Place<'s, B, E>,
}

impl<'s, T: Persistable<B::Pointer>, B: WriteBackend, E: Encoding>
    PackedPersistableVecGuard<'s, T, B, E>
{
    /// The guard of element `index`, in a packed place, or `None` if there is
    /// none. See [`PackedPersistableVecGuard`] for an example.
    #[inline]
    pub fn get_mut(
        &mut self,
        index: usize,
    ) -> Option<<T as Persistable<B::Pointer>>::Guard<'_, B, Packed>> {
        let PackedPersistableVec { data, seq } = &mut *self.inner;
        let item = data.get_mut(index)?;
        let place = self.place.child::<Packed>(&*seq, index);
        Some(item.guard(self.backend, place))
    }

    /// Appends `value`: one transaction that writes its packed encoding past
    /// the end, creating the content allocation for the first element.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PackedPersistableVec;
    ///
    /// let mut db = Kladde::new(PackedPersistableVec::<i64>::new());
    /// db.guard().push(-1)?;
    /// assert_eq!(db.get().as_slice(), &[-1]);
    /// assert_eq!(db.get().content_size(), 1);
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn push(&mut self, value: T) -> Result<(), Error> {
        let len = self.inner.data.len();
        self.insert(len, value)
    }

    /// Inserts `value` at `index`, shifting every later element. Panics if
    /// `index > len`, as [`Vec::insert`] does.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PackedPersistableVec;
    ///
    /// let mut db = Kladde::new(PackedPersistableVec::<u8>::new());
    /// db.guard().push(3)?;
    /// db.guard().insert(0, 1)?;
    /// assert_eq!(db.get().as_slice(), &[1, 3]);
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn insert(&mut self, index: usize, mut value: T) -> Result<(), Error> {
        let len = self.inner.data.len();
        assert!(
            index <= len,
            "PackedPersistableVec::insert: index {index} out of bounds"
        );
        debug_assert!(
            self.inner.seq.pointer.is_some() || len == 0,
            "a stored vec with elements has a content allocation"
        );
        let (backend, place) = (self.backend, &self.place);
        let seq = &self.inner.seq;
        let (fresh, bytes) = backend.atomically(|| {
            let fresh = match &seq.pointer {
                Some(_) => None,
                None => Some(backend.alloc(size(0)?)?),
            };
            let pointer = fresh.as_ref().or(seq.pointer.as_ref()).unwrap();
            value.prepare(backend)?;
            packed_size(&value);
            let bytes = value.to_bytes::<Packed>();
            let at = if len == 0 { 0 } else { seq.offset(index) };
            if index == len {
                backend.write(pointer.raw(), size(at)?, &bytes)?;
            } else {
                backend.splice(pointer, size(at)?, size(0)?, &bytes)?;
            }
            if fresh.is_some() {
                publish_pointer(backend, place, None, Some(pointer.raw()))?;
            }
            Ok((fresh, bytes.len()))
        })?;
        if fresh.is_some() {
            self.inner.seq.pointer = fresh;
        }
        self.inner.seq.insert(index, bytes);
        self.inner.data.insert(index, value);
        Ok(())
    }

    /// Removes the last element and returns it, allocations and all, or
    /// `None` if the vec is empty.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PackedPersistableVec;
    ///
    /// let mut db = Kladde::new(PackedPersistableVec::<u8>::new());
    /// db.guard().push(4)?;
    /// assert_eq!(db.guard().pop()?, Some(4));
    /// assert_eq!(db.guard().pop()?, None);
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn pop(&mut self) -> Result<Option<T>, Error> {
        let len = self.inner.data.len();
        let Some(pointer) = self.inner.seq.pointer.as_ref().filter(|_| len > 0) else {
            return Ok(None);
        };
        self.backend
            .resize(pointer, size(self.inner.seq.offset(len - 1))?)?;
        self.inner.seq.remove(len - 1);
        Ok(self.inner.data.pop())
    }

    /// Removes element `index` and returns it, allocations and all, shifting
    /// every later element. Panics if `index` is out of bounds, as
    /// [`Vec::remove`] does. See [`PackedPersistableVecGuard`] for an
    /// example.
    pub fn remove(&mut self, index: usize) -> Result<T, Error> {
        let len = self.inner.data.len();
        assert!(
            index < len,
            "PackedPersistableVec::remove: index {index} out of bounds"
        );
        let seq = &self.inner.seq;
        let pointer = seq
            .pointer
            .as_ref()
            .expect("a vec with elements has content");
        let (at, end) = (seq.offset(index), seq.offset(index + 1));
        self.backend
            .splice(pointer, size(at)?, size(end - at)?, &[])?;
        self.inner.seq.remove(index);
        Ok(self.inner.data.remove(index))
    }

    /// Removes element `index` and frees everything it owns, in one
    /// transaction. Panics if `index` is out of bounds.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::{PackedPersistableVec, PersistableString};
    ///
    /// let mut db = Kladde::new(PackedPersistableVec::<PersistableString>::new());
    /// db.guard().push(PersistableString::from("gone"))?;
    /// db.guard().delete(0)?;
    /// assert!(db.get().is_empty());
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn delete(&mut self, index: usize) -> Result<(), Error> {
        let len = self.inner.data.len();
        assert!(
            index < len,
            "PackedPersistableVec::delete: index {index} out of bounds"
        );
        let backend = self.backend;
        let PackedPersistableVec { data, seq } = &mut *self.inner;
        let pointer = seq
            .pointer
            .as_ref()
            .expect("a vec with elements has content");
        let (at, end) = (seq.offset(index), seq.offset(index + 1));
        backend.atomically(|| {
            backend.splice(pointer, size(at)?, size(end - at)?, &[])?;
            data[index].free(backend)
        })?;
        seq.remove(index);
        data.remove(index);
        Ok(())
    }

    /// Removes every element and frees everything they own, in one
    /// transaction. The content allocation stays, empty.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PackedPersistableVec;
    ///
    /// let mut db = Kladde::new(PackedPersistableVec::<u8>::new());
    /// db.guard().push(1)?;
    /// db.guard().clear()?;
    /// assert!(db.get().is_empty());
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn clear(&mut self) -> Result<(), Error> {
        let backend = self.backend;
        let PackedPersistableVec { data, seq } = &mut *self.inner;
        let Some(pointer) = seq.pointer.as_ref() else {
            return Ok(());
        };
        backend.atomically(|| {
            backend.resize(pointer, size(0)?)?;
            for item in data.iter_mut() {
                item.free(backend)?;
            }
            Ok(())
        })?;
        data.clear();
        seq.offsets = vec![Cell::new(0)];
        Ok(())
    }

    /// Replaces the whole vec: stores `value`, which publishes it, then frees
    /// the old elements and content, in one transaction.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PackedPersistableVec;
    ///
    /// let mut db = Kladde::new(PackedPersistableVec::<u8>::new());
    /// db.guard().set([5, 6].into_iter().collect())?;
    /// assert_eq!(db.get().as_slice(), &[5, 6]);
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn set(&mut self, value: PackedPersistableVec<T, B::Pointer>) -> Result<(), Error> {
        replace(self.inner, value, self.backend, &self.place)
    }
}

impl<'s, T, B: WriteBackend, E: Encoding> Guard for PackedPersistableVecGuard<'s, T, B, E> {
    type Persistable = PackedPersistableVec<T, B::Pointer>;
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

impl<'s, T, B: WriteBackend, E: Encoding> Deref for PackedPersistableVecGuard<'s, T, B, E> {
    type Target = PackedPersistableVec<T, B::Pointer>;
    fn deref(&self) -> &Self::Target {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Fixture;
    use crate::PersistableString;

    type Vec32 = PackedPersistableVec<u32>;

    #[test]
    fn elements_take_their_packed_size() {
        let mut f = Fixture::for_type::<Vec32>();
        let mut vec = Vec32::new();
        {
            let mut guard = vec.guard(&f.store, f.place());
            for x in [1, 300, 70_000, 5] {
                guard.push(x).unwrap();
            }
        }
        assert_eq!(vec.content_size(), 1 + 2 + 3 + 1);
        let reloaded: Vec32 = f.reload();
        assert_eq!(reloaded.as_slice(), &[1, 300, 70_000, 5]);
        assert_eq!(reloaded.content_size(), 7);
    }

    #[test]
    fn an_element_that_grows_moves_the_ones_behind_it() {
        let mut f = Fixture::for_type::<Vec32>();
        let mut vec: Vec32 = [1, 2, 3].into_iter().collect();
        vec.store::<_, Slotted>(&f.store, f.location).unwrap();
        {
            let mut guard = vec.guard(&f.store, f.place());
            guard.get_mut(0).unwrap().set(1 << 20).unwrap();
            guard.get_mut(2).unwrap().set(1 << 10).unwrap();
            guard.get_mut(1).unwrap().set(9).unwrap();
            guard.get_mut(0).unwrap().set(4).unwrap();
        }
        let reloaded: Vec32 = f.reload();
        assert_eq!(reloaded.as_slice(), &[4, 9, 1 << 10]);
        assert_eq!(reloaded.content_size(), 1 + 1 + 2);
    }

    #[test]
    fn insert_remove_pop_and_clear_keep_the_offsets() {
        let mut f = Fixture::for_type::<Vec32>();
        let mut vec = Vec32::new();
        {
            let mut guard = vec.guard(&f.store, f.place());
            guard.push(500).unwrap();
            guard.insert(0, 1).unwrap();
            guard.insert(1, 100_000).unwrap();
            guard.push(2).unwrap();
            assert_eq!(guard.remove(1).unwrap(), 100_000);
            assert_eq!(guard.pop().unwrap(), Some(2));
            guard.get_mut(1).unwrap().set(3).unwrap();
        }
        let reloaded: Vec32 = f.reload();
        assert_eq!(reloaded.as_slice(), &[1, 3]);
        vec.guard(&f.store, f.place()).clear().unwrap();
        vec.guard(&f.store, f.place()).push(7).unwrap();
        let reloaded: Vec32 = f.reload();
        assert_eq!(reloaded.as_slice(), &[7]);
    }

    #[test]
    fn elements_that_own_allocations_are_freed() {
        let mut f = Fixture::for_type::<PackedPersistableVec<PersistableString>>();
        let mut vec = PackedPersistableVec::<PersistableString>::new();
        let mut guard = vec.guard(&f.store, f.place());
        guard.push(PersistableString::from("kept")).unwrap();
        guard.push(PersistableString::from("deleted")).unwrap();
        f.store.flush().unwrap();
        let before = f.store.allocations().len();
        vec.guard(&f.store, f.place()).delete(1).unwrap();
        f.store.flush().unwrap();
        assert_eq!(f.store.allocations().len(), before - 1);
        let reloaded: PackedPersistableVec<PersistableString> = f.reload();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0], "kept");
    }
}
