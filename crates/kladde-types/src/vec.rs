//! [`PersistableVec`] -- the backed variant of `Vec<T>`.
//!
//! Layout: the content allocation's pointer inline (all-zero while there is
//! none), and the content allocation holding the elements back to back in
//! fixed-size slots, element `i` at `i · T::INLINE_SIZE`.
//!
//! **The length is not stored.** It is the content allocation's size divided
//! by the element size, since the store knows every allocation's size. That
//! halves the inline footprint and removes a field that could drift out of
//! step with the allocation. The price is that a zero-sized element type is
//! unrepresentable, and `PersistableVec` refuses one.

use kladde_persist::{
    replace, Error, Guard, Location, Persistable, Pointer, PointerRepr, ReadBackend, UniquePointer,
    Word, WriteBackend,
};
use std::ops::Deref;

use crate::slot::{read_slot, size, write_slot};

/// A growable array whose contents are persisted.
///
/// Reads go through `Deref<Target = [T]>`, so everything a slice offers --
/// `len`, indexing, `iter`, `first`, ... -- works on the in-memory copy at the
/// cost of a plain memory access. Mutation goes through a
/// [`PersistableVecGuard`], which records each change and then applies it.
/// `T` needs only [`Persistable`]: no `Clone`, no `Serialize`.
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::PersistableVec;
///
/// let mut numbers = Kladde::new(PersistableVec::<u32>::new());
/// numbers.guard().push(10)?;
/// numbers.guard().push(20)?;
/// assert_eq!(numbers.get().len(), 2);
/// assert_eq!(numbers.get()[1], 20);
/// # Ok::<(), kladde::Error>(())
/// ```
#[derive(Debug, PartialEq)]
pub struct PersistableVec<T, P = Pointer> {
    data: Vec<T>,
    /// The content allocation, `None` until something needs one. Its size is
    /// this vec's length times the element size.
    pointer: Option<UniquePointer<P>>,
}

/// The element stride, refusing a zero-sized element type, whose length the
/// layout could not recover.
#[inline]
fn stride<T: Persistable<P>, P: PointerRepr>() -> usize {
    let elem = <T as Persistable<P>>::INLINE_SIZE;
    assert!(
        elem > 0,
        "PersistableVec cannot hold a zero-sized element type: its length is \
         recovered from the content allocation's size",
    );
    elem
}

impl<T, P> PersistableVec<T, P> {
    /// An empty vec, holding no allocation until something is pushed.
    ///
    /// ```
    /// use kladde_types::PersistableVec;
    ///
    /// let v = PersistableVec::<u8>::new();
    /// assert!(v.is_empty());
    /// ```
    pub fn new() -> Self {
        PersistableVec {
            data: Vec::new(),
            pointer: None,
        }
    }

    /// The elements, as a slice. The same as dereferencing.
    ///
    /// ```
    /// use kladde_types::PersistableVec;
    ///
    /// let v: PersistableVec<u8> = [1, 2].into_iter().collect();
    /// assert_eq!(v.as_slice(), &[1, 2]);
    /// ```
    pub fn as_slice(&self) -> &[T] {
        &self.data
    }

    pub(crate) fn into_data(self) -> Vec<T> {
        self.data
    }
}

impl<T, P> Deref for PersistableVec<T, P> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.data
    }
}

impl<P: PointerRepr> PersistableVec<u8, P> {
    /// `store` for bytes: a first content allocation is filled with one write
    /// rather than one per element, which is what `PersistableString` and
    /// `PersistableBlob` store through.
    pub(crate) fn store_bytes<B: WriteBackend<Pointer = P>>(
        &mut self,
        backend: &B,
        location: Location<P, B::Size>,
    ) -> Result<(), Error> {
        if self.pointer.is_none() && !self.data.is_empty() {
            let pointer = backend.alloc(size(0)?)?;
            backend.write(pointer.raw(), size(0)?, &self.data)?;
            self.pointer = Some(pointer);
        }
        write_slot(backend, location, self.pointer.as_ref().map(|p| p.raw()))
    }
}

impl<T, P> Default for PersistableVec<T, P> {
    fn default() -> Self {
        Self::new()
    }
}

/// Collects into a vec that holds no allocation yet: storing it, as part of a
/// value that is itself stored, creates its content allocation.
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::PersistableVec;
///
/// let squares: PersistableVec<u32> = (1..4).map(|i| i * i).collect();
/// let db = Kladde::new(squares);
/// assert_eq!(db.get().as_slice(), &[1, 4, 9]);
/// ```
impl<T, P> FromIterator<T> for PersistableVec<T, P> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        PersistableVec {
            data: Vec::from_iter(iter),
            pointer: None,
        }
    }
}

impl<'a, T, P> IntoIterator for &'a PersistableVec<T, P> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.data.iter()
    }
}

impl<T: Persistable<P>, P: PointerRepr> Persistable<P> for PersistableVec<T, P> {
    /// Just the content allocation's pointer: the length lives with the store.
    const INLINE_SIZE: usize = P::BYTE_LEN;

    type Guard<'s, B: WriteBackend<Pointer = P>>
        = PersistableVecGuard<'s, T, B>
    where
        Self: 's,
        B: 's;

    #[inline]
    fn guard<'s, B: WriteBackend<Pointer = P>>(
        &'s mut self,
        backend: &'s B,
        location: Location<P, B::Size>,
    ) -> Self::Guard<'s, B> {
        PersistableVecGuard {
            inner: self,
            backend,
            location,
        }
    }

    /// Publishes this vec's content pointer at `location`. A vec that already
    /// has an allocation keeps it, since guards keep its content current; one
    /// with elements but no allocation, collected from an iterator, gets one
    /// here, filled before the pointer publishes it.
    fn store<B: WriteBackend<Pointer = P>>(
        &mut self,
        backend: &B,
        location: Location<P, B::Size>,
    ) -> Result<(), Error> {
        if self.pointer.is_none() && !self.data.is_empty() {
            let elem = stride::<T, P>();
            let pointer = backend.alloc(size(self.data.len() * elem)?)?;
            for (i, item) in self.data.iter_mut().enumerate() {
                item.store(backend, Location::new(pointer.raw(), size(i * elem)?))?;
            }
            self.pointer = Some(pointer);
        }
        write_slot(backend, location, self.pointer.as_ref().map(|p| p.raw()))
    }

    fn load<B: ReadBackend<Pointer = P>>(
        backend: &mut B,
        location: Location<P, B::Size>,
    ) -> Result<Self, Error> {
        let Some(target) = read_slot::<P, B>(backend, location)? else {
            return Ok(PersistableVec::new());
        };
        let elem = stride::<T, P>();
        let len = backend.read_size(target)?.to_usize() / elem;
        let mut data = Vec::with_capacity(len);
        for i in 0..len {
            data.push(T::load(backend, Location::new(target, size(i * elem)?))?);
        }
        Ok(PersistableVec {
            data,
            pointer: Some(UniquePointer::from_pointer(target)),
        })
    }

    /// Frees every element, then the content allocation.
    fn free<B: WriteBackend<Pointer = P>>(&mut self, backend: &B) -> Result<(), Error> {
        for item in &mut self.data {
            item.free(backend)?;
        }
        if let Some(pointer) = self.pointer.take() {
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
            type_name: "PersistableVec".into(),
            version: crate::library_version(),
            inline_size: P::BYTE_LEN as u64,
            parameters: vec![<T as Persistable<P>>::describe(builder)],
        }
    }
}

/// The mutation-capable view onto a [`PersistableVec`].
///
/// Each method records its change -- as one transaction, so a crash keeps
/// all of it or none -- and then applies it to the in-memory vec. Removal
/// comes in two flavours: [`remove`](Self::remove) and [`pop`](Self::pop)
/// hand the element back with everything it owns, so it can be stored
/// elsewhere without copying; [`delete`](Self::delete) and
/// [`clear`](Self::clear) free it.
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::{PersistableString, PersistableVec};
///
/// let mut db = Kladde::new(PersistableVec::<PersistableString>::new());
/// let mut v = db.guard();
/// v.push(PersistableString::from("a"))?;
/// v.push(PersistableString::from("b"))?;
/// let first = v.remove(0)?; // yours now, allocation and all
/// v.push(first)?; // moved back without copying its bytes
/// v.delete(0)?; // removed and freed
/// assert_eq!(db.get()[0], "a");
/// # Ok::<(), kladde::Error>(())
/// ```
pub struct PersistableVecGuard<'s, T, B: WriteBackend> {
    inner: &'s mut PersistableVec<T, B::Pointer>,
    backend: &'s B,
    location: Location<B::Pointer, B::Size>,
}

impl<'s, T: Persistable<B::Pointer>, B: WriteBackend> PersistableVecGuard<'s, T, B> {
    /// The guard of element `index`, or `None` if there is none.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableVec;
    ///
    /// let mut db = Kladde::new(PersistableVec::<i32>::new());
    /// db.guard().push(1)?;
    /// db.guard().get_mut(0).unwrap().set(99)?;
    /// assert_eq!(db.get()[0], 99);
    /// # Ok::<(), kladde::Error>(())
    /// ```
    #[inline]
    pub fn get_mut(
        &mut self,
        index: usize,
    ) -> Option<<T as Persistable<B::Pointer>>::Guard<'_, B>> {
        let elem = stride::<T, B::Pointer>();
        let target = self.inner.pointer.as_ref()?.raw();
        let location = Location::new(target, size(index * elem).ok()?);
        self.inner
            .data
            .get_mut(index)
            .map(|item| item.guard(self.backend, location))
    }

    /// Appends `value`: one transaction that grows the content allocation,
    /// creating it for the first element, and stores the element into the new
    /// slot.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableVec;
    ///
    /// let mut db = Kladde::new(PersistableVec::<u16>::new());
    /// db.guard().push(7)?;
    /// assert_eq!(db.get().as_slice(), &[7]);
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn push(&mut self, value: T) -> Result<(), Error> {
        let len = self.inner.data.len();
        self.insert(len, value)
    }

    /// Inserts `value` at `index`, shifting every later element up by one.
    /// Panics if `index > len`, as [`Vec::insert`] does.
    ///
    /// Runs in `O(n)` in the elements after `index`, which the flush rewrites.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableVec;
    ///
    /// let mut db = Kladde::new(PersistableVec::<u8>::new());
    /// db.guard().push(3)?;
    /// db.guard().insert(0, 1)?;
    /// assert_eq!(db.get().as_slice(), &[1, 3]);
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn insert(&mut self, index: usize, mut value: T) -> Result<(), Error> {
        let len = self.inner.data.len();
        assert!(
            index <= len,
            "PersistableVec::insert: index {index} out of bounds"
        );
        debug_assert!(
            self.inner.pointer.is_some() || len == 0,
            "a stored vec with elements has a content allocation"
        );
        let elem = stride::<T, B::Pointer>();
        let (backend, location) = (self.backend, self.location);
        let existing = &self.inner.pointer;
        let fresh = backend.atomically(|| {
            let fresh = match existing {
                Some(_) => None,
                None => Some(backend.alloc(size(0)?)?),
            };
            let pointer = fresh.as_ref().or(existing.as_ref()).unwrap();
            if index == len {
                backend.resize(pointer, size((len + 1) * elem)?)?;
            } else {
                backend.splice(pointer, size(index * elem)?, size(0)?, &vec![0u8; elem])?;
            }
            value.store(backend, Location::new(pointer.raw(), size(index * elem)?))?;
            if fresh.is_some() {
                write_slot(backend, location, Some(pointer.raw()))?;
            }
            Ok(fresh)
        })?;
        if fresh.is_some() {
            self.inner.pointer = fresh;
        }
        self.inner.data.insert(index, value);
        Ok(())
    }

    /// Removes the last element and returns it, allocations and all, or
    /// `None` if the vec is empty.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableVec;
    ///
    /// let mut db = Kladde::new(PersistableVec::<u8>::new());
    /// db.guard().push(4)?;
    /// assert_eq!(db.guard().pop()?, Some(4));
    /// assert_eq!(db.guard().pop()?, None);
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn pop(&mut self) -> Result<Option<T>, Error> {
        let len = self.inner.data.len();
        let Some(pointer) = self.inner.pointer.as_ref().filter(|_| len > 0) else {
            return Ok(None);
        };
        let elem = stride::<T, B::Pointer>();
        self.backend.resize(pointer, size((len - 1) * elem)?)?;
        Ok(self.inner.data.pop())
    }

    /// Removes element `index` and returns it, allocations and all, shifting
    /// every later element down by one. Panics if `index` is out of bounds,
    /// as [`Vec::remove`] does.
    ///
    /// The element is yours: store it elsewhere to move it without copying
    /// its content, or free it -- dropped instead, it leaks its allocations in
    /// the file. [`delete`](Self::delete) removes and frees.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableVec;
    ///
    /// let mut db = Kladde::new(PersistableVec::<i32>::new());
    /// for x in [10, 20, 30] {
    ///     db.guard().push(x)?;
    /// }
    /// assert_eq!(db.guard().remove(0)?, 10);
    /// assert_eq!(db.get().as_slice(), &[20, 30]);
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn remove(&mut self, index: usize) -> Result<T, Error> {
        let len = self.inner.data.len();
        assert!(
            index < len,
            "PersistableVec::remove: index {index} out of bounds"
        );
        let elem = stride::<T, B::Pointer>();
        let pointer = self
            .inner
            .pointer
            .as_ref()
            .expect("a vec with elements has content");
        self.backend
            .splice(pointer, size(index * elem)?, size(elem)?, &[])?;
        Ok(self.inner.data.remove(index))
    }

    /// Removes element `index` and frees everything it owns, in one
    /// transaction. Panics if `index` is out of bounds.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::{PersistableString, PersistableVec};
    ///
    /// let mut db = Kladde::new(PersistableVec::<PersistableString>::new());
    /// db.guard().push(PersistableString::from("gone"))?;
    /// db.guard().delete(0)?;
    /// assert!(db.get().is_empty());
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn delete(&mut self, index: usize) -> Result<(), Error> {
        let len = self.inner.data.len();
        assert!(
            index < len,
            "PersistableVec::delete: index {index} out of bounds"
        );
        let elem = stride::<T, B::Pointer>();
        let backend = self.backend;
        let PersistableVec { data, pointer } = &mut *self.inner;
        let pointer = pointer.as_ref().expect("a vec with elements has content");
        backend.atomically(|| {
            backend.splice(pointer, size(index * elem)?, size(elem)?, &[])?;
            data[index].free(backend)
        })?;
        data.remove(index);
        Ok(())
    }

    /// Removes every element and frees everything they own, in one
    /// transaction. The content allocation stays, empty.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableVec;
    ///
    /// let mut db = Kladde::new(PersistableVec::<u8>::new());
    /// db.guard().push(1)?;
    /// db.guard().clear()?;
    /// assert!(db.get().is_empty());
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn clear(&mut self) -> Result<(), Error> {
        let backend = self.backend;
        let PersistableVec { data, pointer } = &mut *self.inner;
        let Some(pointer) = pointer.as_ref() else {
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
        Ok(())
    }

    /// Replaces the whole vec: stores `value`, which publishes it, then frees
    /// the old elements and content, in one transaction.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableVec;
    ///
    /// let mut db = Kladde::new(PersistableVec::<u8>::new());
    /// db.guard().set([5, 6].into_iter().collect())?;
    /// assert_eq!(db.get().as_slice(), &[5, 6]);
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn set(&mut self, value: PersistableVec<T, B::Pointer>) -> Result<(), Error> {
        replace(self.inner, value, self.backend, self.location)
    }
}

impl<'s, B: WriteBackend> PersistableVecGuard<'s, u8, B> {
    /// Replaces the contents with `new` in one write, whatever the current
    /// length. An owned `Vec<u8>` is moved in without copying.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableVec;
    ///
    /// let mut db = Kladde::new(PersistableVec::<u8>::new());
    /// db.guard().set_bytes(b"hello")?;
    /// db.guard().set_bytes(vec![b'h', b'i'])?;
    /// assert_eq!(db.get().as_slice(), b"hi");
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn set_bytes(&mut self, new: impl Into<Vec<u8>>) -> Result<(), Error> {
        let new = new.into();
        let old_len = self.inner.data.len();
        match &self.inner.pointer {
            Some(pointer) => self
                .backend
                .splice(pointer, size(0)?, size(old_len)?, &new)?,
            None if new.is_empty() => {}
            None => {
                let fresh = self.fresh_content(&new)?;
                self.inner.pointer = Some(fresh);
            }
        }
        self.inner.data = new;
        Ok(())
    }

    /// Appends `bytes` in one write.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableVec;
    ///
    /// let mut db = Kladde::new(PersistableVec::<u8>::new());
    /// db.guard().extend_from_slice(b"ab")?;
    /// db.guard().extend_from_slice(b"cd")?;
    /// assert_eq!(db.get().as_slice(), b"abcd");
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn extend_from_slice(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let len = self.inner.data.len();
        match &self.inner.pointer {
            Some(pointer) => self.backend.write(pointer.raw(), size(len)?, bytes)?,
            None if bytes.is_empty() => {}
            None => {
                let fresh = self.fresh_content(bytes)?;
                self.inner.pointer = Some(fresh);
            }
        }
        self.inner.data.extend_from_slice(bytes);
        Ok(())
    }

    /// A content allocation holding `bytes`, published at this guard's
    /// location, in one transaction.
    fn fresh_content(&mut self, bytes: &[u8]) -> Result<UniquePointer<B::Pointer>, Error> {
        let (backend, location) = (self.backend, self.location);
        backend.atomically(|| {
            let pointer = backend.alloc(size(0)?)?;
            backend.write(pointer.raw(), size(0)?, bytes)?;
            write_slot(backend, location, Some(pointer.raw()))?;
            Ok(pointer)
        })
    }
}

impl<'s, T, B: WriteBackend> Guard for PersistableVecGuard<'s, T, B> {
    type Persistable = PersistableVec<T, B::Pointer>;
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

impl<'s, T, B: WriteBackend> Deref for PersistableVecGuard<'s, T, B> {
    type Target = PersistableVec<T, B::Pointer>;
    fn deref(&self) -> &Self::Target {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Fixture;
    use crate::PersistableString;

    #[test]
    fn push_and_remove_round_trip() {
        let mut f = Fixture::new(<PersistableVec<i32> as Persistable>::INLINE_SIZE);
        let mut vec = PersistableVec::<i32>::new();
        {
            let mut guard = vec.guard(&f.store, f.location);
            for x in [10, 20, 30] {
                guard.push(x).unwrap();
            }
            assert_eq!(guard.remove(1).unwrap(), 20);
        }
        let reloaded: PersistableVec<i32> = f.reload();
        assert_eq!(reloaded.as_slice(), &[10, 30]);
        assert_eq!(reloaded, vec);
    }

    #[test]
    fn the_length_comes_back_from_the_allocation_size() {
        let mut f = Fixture::new(<PersistableVec<i32> as Persistable>::INLINE_SIZE);
        let mut vec = PersistableVec::<i32>::new();
        {
            let mut guard = vec.guard(&f.store, f.location);
            for i in 0..7 {
                guard.push(i).unwrap();
            }
            assert_eq!(guard.pop().unwrap(), Some(6));
            guard.insert(0, -1).unwrap();
        }
        assert_eq!(<PersistableVec<i32> as Persistable>::INLINE_SIZE, 4);
        let reloaded: PersistableVec<i32> = f.reload();
        assert_eq!(reloaded.as_slice(), &[-1, 0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn delete_frees_what_the_element_owns() {
        let mut f = Fixture::new(<PersistableVec<PersistableString> as Persistable>::INLINE_SIZE);
        let mut vec = PersistableVec::<PersistableString>::new();
        let mut guard = vec.guard(&f.store, f.location);
        guard.push(PersistableString::from("kept")).unwrap();
        guard.push(PersistableString::from("deleted")).unwrap();
        f.store.flush().unwrap();
        let before = f.store.allocations().len();
        vec.guard(&f.store, f.location).delete(1).unwrap();
        f.store.flush().unwrap();
        assert_eq!(f.store.allocations().len(), before - 1);
        let reloaded: PersistableVec<PersistableString> = f.reload();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0], "kept");
    }

    #[test]
    fn store_allocates_content_for_a_collected_vec_once() {
        let mut f = Fixture::new(<PersistableVec<i32> as Persistable>::INLINE_SIZE);
        let mut vec: PersistableVec<i32> = [1, 2, 3].into_iter().collect();
        vec.store(&f.store, f.location).unwrap();
        f.store.flush().unwrap();
        let after_first = f.store.allocations().len();
        vec.store(&f.store, f.location).unwrap();
        vec.guard(&f.store, f.location).push(4).unwrap();
        f.store.flush().unwrap();
        assert_eq!(
            f.store.allocations().len(),
            after_first,
            "no second allocation"
        );
        let reloaded: PersistableVec<i32> = f.reload();
        assert_eq!(reloaded.as_slice(), &[1, 2, 3, 4]);
    }

    #[test]
    fn an_empty_vec_stores_as_a_null_pointer() {
        let mut f = Fixture::new(<PersistableVec<i32> as Persistable>::INLINE_SIZE);
        let mut vec = PersistableVec::<i32>::new();
        vec.store(&f.store, f.location).unwrap();
        let reloaded: PersistableVec<i32> = f.reload();
        assert!(reloaded.is_empty());
        assert_eq!(f.store.allocations().len(), 1, "just the root allocation");
    }

    #[test]
    fn set_bytes_grows_and_shrinks_one_allocation() {
        let mut f = Fixture::new(<PersistableVec<u8> as Persistable>::INLINE_SIZE);
        let mut vec = PersistableVec::<u8>::new();
        for content in [b"hello".as_slice(), b"hi", b"hello there", b""] {
            vec.guard(&f.store, f.location).set_bytes(content).unwrap();
            let reloaded: PersistableVec<u8> = f.reload();
            assert_eq!(reloaded.as_slice(), content);
            assert_eq!(f.store.allocations().len(), 2);
        }
    }
}
