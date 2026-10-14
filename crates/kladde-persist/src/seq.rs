//! [`Durable`] for [`Vec<T>`] of fixed-size elements, and its [`VecMut`].
//!
//! A vector of fixed-size elements finds element `i` by multiplication, so
//! a plain `Vec` holds everything it needs. A vector of elements whose size
//! depends on their value also needs an index of where they start, which a
//! `Vec` has no room for: that is `DurableVec` of `kladde-types`, and using
//! a `Vec` for such elements fails to compile.

use std::ops::Deref;

use kladde_schema::TypeDescriptor;
use kladde_store::Error;

use crate::codec::{varint_len, Decoder, Encoder};
use crate::durable::{replace, Durable, Mut};
use crate::region::{Backend, Region};
use crate::schema::SchemaBuilder;

/// The size of every element of a `Vec<T>`, or a compile error where `T`
/// is not fixed-size.
const fn element_size<T: Durable>() -> usize {
    match T::FIXED_SIZE {
        Some(n) => n,
        None => panic!(
            "a `Vec<T>` is durable only for a fixed-size `T`; \
             use `DurableVec<T>` for elements whose size depends on their value"
        ),
    }
}

/// A vector of fixed-size elements: their number as a varint, then the
/// elements back to back.
///
/// Only a `T` whose every value encodes to the same number of bytes is
/// allowed, so that element `i` lies at `i` times that size; for any other
/// `T`, using `Vec<T>` as a durable value fails to compile, and
/// `DurableVec<T>` of `kladde-types` is the type to use.
///
/// ```
/// use kladde_persist::{Durable, Region};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let mut points: Vec<(f32, f32)> = vec![(0.0, 0.0)];
/// store.splice(0, 0, &points.to_bytes())?;
/// let mut list = points.mutate(Region::root(&store));
/// list.push((1.0, 2.0))?;
/// list.get_mut(0).unwrap().set((3.0, 4.0))?;
/// assert_eq!(points, [(3.0, 4.0), (1.0, 2.0)]);
/// store.flush()?;
/// assert_eq!(store.read_all()?, points.to_bytes());
/// # Ok::<(), kladde_store::Error>(())
/// ```
///
/// Elements whose size depends on their value, such as text, are refused
/// when the code that uses the vector is compiled:
///
/// ```compile_fail
/// use kladde_persist::Durable;
///
/// let names: Vec<String> = vec!["Ada".into()];
/// names.to_bytes(); // error: a `Vec<T>` is durable only for a fixed-size `T`
/// ```
impl<T: Durable> Durable for Vec<T> {
    const FIXED_SIZE: Option<usize> = None;

    fn encoded_len(&self) -> usize {
        varint_len(self.len() as u64) + self.len() * const { element_size::<T>() }
    }

    fn encode(&self, out: &mut Encoder<'_>) {
        const { element_size::<T>() };
        out.varint(self.len() as u64);
        for item in self {
            item.encode(out);
        }
    }

    fn decode(input: &mut Decoder<'_>) -> Result<Self, Error> {
        let size = const { element_size::<T>() };
        let n: usize = input.varint_as()?;
        let mut items = Vec::with_capacity(n.min(1 << 20) / size.max(1));
        for _ in 0..n {
            items.push(T::decode(input)?);
        }
        Ok(items)
    }

    type Mut<'r, B: Backend + 'r>
        = VecMut<'r, T, B>
    where
        T: 'r;

    fn mutate<'r, B: Backend + 'r>(&'r mut self, region: Region<'r, B>) -> Self::Mut<'r, B> {
        const { element_size::<T>() };
        VecMut {
            inner: self,
            region,
        }
    }

    fn describe_local(builder: &mut SchemaBuilder) -> TypeDescriptor
    where
        Self: 'static,
    {
        const { element_size::<T>() };
        TypeDescriptor::Sequence(T::describe(builder))
    }
}

/// The [`Mut`] of a [`Vec`] of fixed-size elements: insert and remove
/// elements, and reach each element's own `Mut`.
///
/// Every insertion and removal is a splice of the element and a rewrite of
/// the count, in one transaction. See [`Vec`]'s `Durable` implementation for
/// an example.
pub struct VecMut<'r, T, B> {
    inner: &'r mut Vec<T>,
    region: Region<'r, B>,
}

impl<'r, T: Durable, B: Backend> VecMut<'r, T, B> {
    /// Where element `index` starts, relative to the vector.
    fn offset_of(&self, index: usize) -> u64 {
        (varint_len(self.inner.len() as u64) + index * element_size::<T>()) as u64
    }

    /// Records the change of the count from `old` to `new`.
    fn write_count(region: &Region<'_, B>, old: usize, new: usize) -> Result<(), Error> {
        let mut bytes = Vec::with_capacity(10);
        Encoder::new(&mut bytes).varint(new as u64);
        region.splice(0, varint_len(old as u64) as u64, &bytes)?;
        Ok(())
    }

    /// Replaces the whole vector.
    pub fn set(&mut self, value: Vec<T>) -> Result<(), Error> {
        replace(self.inner, value, &self.region)
    }

    /// Inserts `value` before element `index`, which may be the length.
    /// Fails with [`Error::OutOfBounds`] past it, recording nothing.
    pub fn insert(&mut self, index: usize, value: T) -> Result<(), Error> {
        let n = self.inner.len();
        if index > n {
            return Err(Error::OutOfBounds);
        }
        let at = self.offset_of(index);
        self.inner.reserve(1);
        self.region.transaction(|r| {
            r.splice_value(at, 0, &value)?;
            Self::write_count(r, n, n + 1)
        })?;
        self.inner.insert(index, value);
        Ok(())
    }

    /// Appends `value`.
    pub fn push(&mut self, value: T) -> Result<(), Error> {
        let n = self.inner.len();
        self.insert(n, value)
    }

    /// Removes and returns element `index`. Fails with
    /// [`Error::OutOfBounds`] if there is none, recording nothing.
    pub fn remove(&mut self, index: usize) -> Result<T, Error> {
        let n = self.inner.len();
        if index >= n {
            return Err(Error::OutOfBounds);
        }
        let at = self.offset_of(index);
        let size = element_size::<T>() as u64;
        self.region.transaction(|r| {
            r.splice(at, size, &[])?;
            Self::write_count(r, n, n - 1)
        })?;
        Ok(self.inner.remove(index))
    }

    /// Removes and returns the last element, if there is one.
    pub fn pop(&mut self) -> Result<Option<T>, Error> {
        match self.inner.len() {
            0 => Ok(None),
            n => self.remove(n - 1).map(Some),
        }
    }

    /// Removes every element.
    pub fn clear(&mut self) -> Result<(), Error> {
        self.set(Vec::new())
    }

    /// The `Mut` of element `index`, if there is one.
    pub fn get_mut(&mut self, index: usize) -> Option<T::Mut<'_, B>> {
        if index >= self.inner.len() {
            return None;
        }
        let at = self.offset_of(index);
        let size = element_size::<T>() as u64;
        let Self { inner, region } = self;
        Some(inner[index].mutate(region.child(at, size, None, index)))
    }
}

impl<T, B> Mut for VecMut<'_, T, B> {
    type Durable = Vec<T>;

    fn as_durable(&self) -> &Vec<T> {
        self.inner
    }
}

impl<T, B> Deref for VecMut<'_, T, B> {
    type Target = Vec<T>;

    fn deref(&self) -> &Vec<T> {
        self.inner
    }
}
