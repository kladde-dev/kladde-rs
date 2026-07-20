//! [`PersistedVec`] -- the backed variant of `Vec<T>`. Named distinctly
//! from `std::vec::Vec` (rather than shadowing it) per `V1_QUESTIONS.md`
//! question 10.

use kladde_traits::{Backend, Guard, Persistable};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::ops::{Deref, DerefMut};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PersistedVecOp<T> {
    Push(T),
    Remove(usize),
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersistedVec<T> {
    data: Vec<T>,
}

impl<T> PersistedVec<T> {
    pub fn new() -> Self {
        PersistedVec { data: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        self.data.get(index)
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.data.iter()
    }
}

impl<T> Default for PersistedVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> FromIterator<T> for PersistedVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        PersistedVec {
            data: Vec::from_iter(iter),
        }
    }
}

impl<'a, T> IntoIterator for &'a PersistedVec<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.data.iter()
    }
}

impl<T: Serialize + DeserializeOwned> Persistable for PersistedVec<T> {
    type Op = PersistedVecOp<T>;
    type Guard<'s, B: Backend>
        = PersistedVecGuard<'s, T, B>
    where
        Self: 's,
        B: 's;

    fn guard<'s, B: Backend>(&'s mut self, backend: &'s B) -> Self::Guard<'s, B> {
        PersistedVecGuard {
            inner: self,
            backend,
        }
    }
}

/// `B` defaults to [`kladde::DefaultBackend`](../../kladde/struct.DefaultBackend.html)
/// so application code that only ever uses the default backend never has
/// to name it.
pub struct PersistedVecGuard<'s, T, B = kladde::DefaultBackend> {
    inner: &'s mut PersistedVec<T>,
    backend: &'s B,
}

// `get_mut` doesn't need `T: Clone` -- kept in its own impl block, as in
// `sketch.rs`, so it stays available for non-`Clone` element types.
impl<'s, T: Persistable, B: Backend> PersistedVecGuard<'s, T, B> {
    pub fn get_mut(&mut self, index: usize) -> Option<T::Guard<'_, B>> {
        self.inner
            .data
            .get_mut(index)
            .map(|item| item.guard(self.backend))
    }
}

impl<'s, T: Clone + Serialize + DeserializeOwned, B: Backend> PersistedVecGuard<'s, T, B> {
    /// Pushes `value`, recording a `Push` op. Requires `T: Clone` --
    /// recording and pushing both need their own copy of `value` since
    /// `Op` is owned rather than borrowed for v1 (see `spec.md`'s Future
    /// Work).
    pub fn push(&mut self, value: T) {
        self.backend
            .record::<PersistedVec<T>>(&PersistedVecOp::Push(value.clone()));
        self.inner.data.push(value);
    }
}

impl<'s, T: Serialize + DeserializeOwned, B: Backend> PersistedVecGuard<'s, T, B> {
    /// Removes and returns the element at `index`, recording a `Remove`
    /// op. Panics if `index` is out of bounds (matches `Vec::remove`).
    pub fn remove(&mut self, index: usize) -> T {
        self.backend
            .record::<PersistedVec<T>>(&PersistedVecOp::Remove(index));
        self.inner.data.remove(index)
    }
}

impl<'s, T: Serialize + DeserializeOwned, B: Backend> Guard for PersistedVecGuard<'s, T, B> {
    type Persistable = PersistedVec<T>;
    type Backend = B;

    fn as_persistable(&self) -> &PersistedVec<T> {
        self.inner
    }
    fn as_persistable_mut(&mut self) -> &mut PersistedVec<T> {
        self.inner
    }
    fn backend(&self) -> &B {
        self.backend
    }
}

impl<'s, T, B> Deref for PersistedVecGuard<'s, T, B> {
    type Target = PersistedVec<T>;
    fn deref(&self) -> &PersistedVec<T> {
        self.inner
    }
}

impl<'s, T, B> DerefMut for PersistedVecGuard<'s, T, B> {
    fn deref_mut(&mut self) -> &mut PersistedVec<T> {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockBackend;

    #[test]
    fn push_appends_and_records_one_op() {
        let backend = MockBackend::default();
        let mut vec = PersistedVec::<i32>::new();

        let mut guard = vec.guard(&backend);
        guard.push(1);
        guard.push(2);

        assert_eq!(vec.len(), 2);
        assert_eq!(vec.get(0), Some(&1));
        assert_eq!(vec.get(1), Some(&2));
        assert_eq!(backend.record_count(), 2);
    }

    #[test]
    fn get_mut_returns_a_nested_guard_for_persistable_elements() {
        let backend = MockBackend::default();
        let mut vec = PersistedVec::<i32>::new();
        {
            let mut guard = vec.guard(&backend);
            guard.push(10);
        }

        let mut guard = vec.guard(&backend);
        guard.get_mut(0).unwrap().set(99);

        assert_eq!(vec.get(0), Some(&99));
        assert_eq!(backend.record_count(), 2); // one push, one nested set
    }

    #[test]
    fn remove_shrinks_the_vec_and_records_one_op() {
        let backend = MockBackend::default();
        let mut vec = PersistedVec::<i32>::new();
        {
            let mut guard = vec.guard(&backend);
            guard.push(1);
            guard.push(2);
        }

        let removed = vec.guard(&backend).remove(0);

        assert_eq!(removed, 1);
        assert_eq!(vec.len(), 1);
        assert_eq!(vec.get(0), Some(&2));
        assert_eq!(backend.record_count(), 3);
    }
}
