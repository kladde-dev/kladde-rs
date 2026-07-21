//! [`PersistedVec`] -- the backed variant of `Vec<T>`. Named distinctly
//! from `std::vec::Vec` (rather than shadowing it) per `V1_QUESTIONS.md`
//! question 10.
//!
//! Snapshot layout (per `FLUSHING_QUESTIONS.md` question 3): a small,
//! fixed 8-byte inline header (`target`, `len` -- see
//! `kladde_traits::write_header`) plus a separate content allocation
//! holding `len` fixed-size (`T::INLINE_SIZE`) element slots, analogous
//! to how `std::vec::Vec` is laid out in memory. No slack/amortized
//! growth yet -- every push/remove resizes the content allocation to
//! exactly fit. Per `later.md`, this straightforward layout is meant to
//! be replaced with a chunked-list representation once this version is
//! tested and committed.

use kladde_traits::{
    read_header, write_header, Backend, Guard, Location, Persistable, RawPointer, UniquePointer,
};
use std::ops::{Deref, DerefMut};

#[derive(Debug, PartialEq)]
pub struct PersistedVec<T> {
    data: Vec<T>,
    /// The content allocation holding this vec's elements -- `None`
    /// until the first `push` ever needs one. Lazily created rather than
    /// eager, since `PersistedVec::new()` takes no `Backend` to create
    /// one with; see `FLUSHING_QUESTIONS.md` question 2's "which
    /// instance" resolution.
    pointer: Option<UniquePointer<PersistedVec<T>>>,
}

impl<T> PersistedVec<T> {
    pub fn new() -> Self {
        PersistedVec {
            data: Vec::new(),
            pointer: None,
        }
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

    /// Crate-internal escape hatch for [`crate::PersistedString`], the
    /// only thing that needs a raw `&[T]`/`Vec<T>` view rather than going
    /// through `get`/`iter`/`push`/`remove` one element at a time.
    pub(crate) fn as_slice(&self) -> &[T] {
        &self.data
    }

    pub(crate) fn into_data(self) -> Vec<T> {
        self.data
    }
}

impl<T> Default for PersistedVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Backend-free, like `new()` -- but unlike `new()`, the result may hold
/// real content with `pointer` still `None` if `iter` isn't empty (e.g.
/// `PersistedString::from("hello")` goes through this). `store`'s `None`
/// branch below handles that: it's not the same "genuinely never
/// touched" case `new()`/`load` produce, so it can't just assume there's
/// nothing to allocate.
impl<T> FromIterator<T> for PersistedVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        PersistedVec {
            data: Vec::from_iter(iter),
            pointer: None,
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

impl<T: Persistable> Persistable for PersistedVec<T> {
    /// A fixed 8-byte `{ target, len }` header -- the content allocation
    /// itself (`len * T::INLINE_SIZE` bytes) is separate, per Question 1
    /// of `FLUSHING_QUESTIONS.md`.
    const INLINE_SIZE: usize = 8;

    type Guard<'s, B: Backend>
        = PersistedVecGuard<'s, T, B>
    where
        Self: 's,
        B: 's;

    fn guard<'s, B: Backend>(
        &'s mut self,
        backend: &'s B,
        location: Location,
    ) -> Self::Guard<'s, B> {
        PersistedVecGuard {
            inner: self,
            backend,
            location,
        }
    }

    /// Publishes a header at `location` pointing at this vec's content --
    /// used when a whole `PersistedVec` is being written as a brand-new
    /// value somewhere (e.g. a struct field being assembled) rather than
    /// via incremental `push`/`remove`.
    ///
    /// Deliberately does *not* rewrite any elements: `push`/`remove`/
    /// `get_mut().set(...)` already keep the on-disk content at
    /// `self.pointer` exactly in sync with `self.data` incrementally, so
    /// if a pointer already exists, its content is already correct --
    /// `store` only needs to point a new header at it, not rewrite
    /// anything. (Rewriting unconditionally, the way an earlier version
    /// of this method did, wasn't just redundant -- for `PersistedVec` it
    /// happened to still be *correct*, since elements are always packed
    /// at `0..data.len()` with no equivalent to `PersistedHashMap`'s
    /// tombstone/capacity bookkeeping to fall out of sync, but it was
    /// still doing real work for no reason. Not touching existing content
    /// at all is both simpler and avoids relying on that coincidence.)
    fn store<B: Backend>(&self, backend: &B, location: Location) {
        match &self.pointer {
            Some(existing) => {
                write_header(backend, location, existing.index(), self.data.len() as u32);
            }
            None if self.data.is_empty() => {
                // The genuinely-never-touched case (`new`/`load`, or an
                // empty `from_iter`) -- nothing to allocate, just record
                // "no allocation yet" directly (`write_header` requires a
                // real index, so this can't go through it).
                backend.write(location.anchor, location.offset, &[0u8; 8]);
            }
            None => {
                // `data` is non-empty despite `pointer` being `None`: a
                // `from_iter`-constructed value (see its doc comment)
                // that's never been pushed/set through a `Guard`, so
                // there's been no chance yet to remember an allocation.
                // Allocating here is correct, not a leak, for the same
                // reason `push`'s very first call is: there's nothing to
                // reuse yet. The one residual caveat: calling `store`
                // *again* on this exact in-memory value while its own
                // `pointer` field stays `None` in the caller's copy
                // (rather than going through a `Guard`, which does update
                // it) would allocate a second time -- treat a
                // `from_iter`-constructed value as something to push/set
                // exactly once, not to `store` repeatedly.
                let elem_size = T::INLINE_SIZE as u32;
                let byte_size = self.data.len() * T::INLINE_SIZE;
                let pointer = backend.alloc::<PersistedVec<T>>(byte_size);
                for (i, item) in self.data.iter().enumerate() {
                    item.store(
                        backend,
                        Location {
                            anchor: pointer.raw(),
                            offset: i as u32 * elem_size,
                        },
                    );
                }
                write_header(backend, location, pointer.index(), self.data.len() as u32);
            }
        }
    }

    fn load<B: Backend>(backend: &B, location: Location) -> Self {
        let (target, len) = read_header(backend, location);
        let pointer = target.map(UniquePointer::from_index);
        let mut data = Vec::with_capacity(len as usize);
        if let Some(target) = target {
            let anchor = RawPointer::from_index(target);
            let elem_size = T::INLINE_SIZE as u32;
            for i in 0..len {
                data.push(T::load(
                    backend,
                    Location {
                        anchor,
                        offset: i * elem_size,
                    },
                ));
            }
        }
        PersistedVec { data, pointer }
    }
}

/// `B` defaults to [`kladde::DefaultBackend`](../../kladde/struct.DefaultBackend.html)
/// so application code that only ever uses the default backend never has
/// to name it.
pub struct PersistedVecGuard<'s, T, B = kladde::DefaultBackend> {
    inner: &'s mut PersistedVec<T>,
    backend: &'s B,
    location: Location,
}

// `get_mut` doesn't need any extra bounds beyond `T: Persistable` --
// kept in its own impl block, as before, so it stays available
// regardless of what other bounds `push`/`remove` need.
impl<'s, T: Persistable, B: Backend> PersistedVecGuard<'s, T, B> {
    pub fn get_mut(&mut self, index: usize) -> Option<T::Guard<'_, B>> {
        let elem_size = T::INLINE_SIZE as u32;
        let pointer = self.inner.pointer.as_ref()?;
        let location = Location {
            anchor: pointer.raw(),
            offset: index as u32 * elem_size,
        };
        self.inner
            .data
            .get_mut(index)
            .map(|item| item.guard(self.backend, location))
    }
}

impl<'s, T: Persistable, B: Backend> PersistedVecGuard<'s, T, B> {
    /// Appends `value`: grows (or creates) the content allocation to fit
    /// one more element, writes the new element into the freshly-grown
    /// slot, then publishes the updated header -- in that order, so any
    /// crash/torn-journal prefix of this sequence leaves the previous,
    /// still-valid state (the header update is always last -- see
    /// `spec.md`'s Crash Consistency section).
    pub fn push(&mut self, value: T) {
        let elem_size = T::INLINE_SIZE as u32;
        let old_len = self.inner.data.len();
        let new_len = old_len + 1;
        let new_byte_size = new_len * T::INLINE_SIZE;

        match &self.inner.pointer {
            Some(pointer) => self.backend.resize(pointer, new_byte_size),
            None => self.inner.pointer = Some(self.backend.alloc::<PersistedVec<T>>(new_byte_size)),
        }
        let pointer = self.inner.pointer.as_ref().unwrap();

        value.store(
            self.backend,
            Location {
                anchor: pointer.raw(),
                offset: old_len as u32 * elem_size,
            },
        );
        write_header(self.backend, self.location, pointer.index(), new_len as u32);

        self.inner.data.push(value);
    }

    /// Removes and returns the element at `index`, shifting every later
    /// element down by one slot (`copy`) *before* shrinking the
    /// allocation (`resize`) and publishing the new header -- shrinking
    /// first would truncate live elements before they've been moved out
    /// of the way. Panics if `index` is out of bounds (matches
    /// `Vec::remove`).
    pub fn remove(&mut self, index: usize) -> T {
        let elem_size = T::INLINE_SIZE as u32;
        let old_len = self.inner.data.len();
        assert!(index < old_len, "PersistedVec::remove: index out of bounds");
        let new_len = old_len - 1;
        let pointer = self
            .inner
            .pointer
            .as_ref()
            .expect("PersistedVec::remove called but no content allocation exists");

        if index < new_len {
            self.backend.copy(
                pointer.raw(),
                (index as u32 + 1) * elem_size,
                (new_len - index) as u32 * elem_size,
                pointer.raw(),
                index as u32 * elem_size,
            );
        }
        self.backend.resize(pointer, new_len * T::INLINE_SIZE);
        write_header(self.backend, self.location, pointer.index(), new_len as u32);

        self.inner.data.remove(index)
    }
}

impl<'s, T: Persistable, B: Backend> Guard for PersistedVecGuard<'s, T, B> {
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
    use kladde_traits::Allocator;

    fn root_location(backend: &MockBackend) -> Location {
        let pointer = backend.alloc::<()>(PersistedVec::<i32>::INLINE_SIZE);
        Location {
            anchor: pointer.raw(),
            offset: 0,
        }
    }

    #[test]
    fn push_appends_in_memory() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut vec = PersistedVec::<i32>::new();

        let mut guard = vec.guard(&backend, location);
        guard.push(1);
        guard.push(2);

        assert_eq!(vec.len(), 2);
        assert_eq!(vec.get(0), Some(&1));
        assert_eq!(vec.get(1), Some(&2));
    }

    #[test]
    fn get_mut_returns_a_nested_guard_for_persistable_elements() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut vec = PersistedVec::<i32>::new();
        {
            let mut guard = vec.guard(&backend, location);
            guard.push(10);
        }

        let mut guard = vec.guard(&backend, location);
        guard.get_mut(0).unwrap().set(99);

        assert_eq!(vec.get(0), Some(&99));
    }

    #[test]
    fn remove_shrinks_the_vec() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut vec = PersistedVec::<i32>::new();
        {
            let mut guard = vec.guard(&backend, location);
            guard.push(1);
            guard.push(2);
            guard.push(3);
        }

        let removed = vec.guard(&backend, location).remove(0);

        assert_eq!(removed, 1);
        assert_eq!(vec.len(), 2);
        assert_eq!(vec.get(0), Some(&2));
        assert_eq!(vec.get(1), Some(&3));
    }

    #[test]
    fn flushing_and_reloading_round_trips_the_content() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut vec = PersistedVec::<i32>::new();
        {
            let mut guard = vec.guard(&backend, location);
            guard.push(10);
            guard.push(20);
            guard.push(30);
        }
        {
            let mut guard = vec.guard(&backend, location);
            guard.remove(1); // remove the middle element
        }

        backend.flush();

        let reloaded = PersistedVec::<i32>::load(&backend, location);
        assert_eq!(reloaded.data, vec![10, 30]);
        assert_eq!(reloaded, vec);
    }

    #[test]
    fn store_reuses_an_existing_allocation_instead_of_leaking_it() {
        let backend = MockBackend::default();
        let location_a = root_location(&backend);
        let location_b = root_location(&backend);

        let mut vec = PersistedVec::<i32>::new();
        {
            let mut guard = vec.guard(&backend, location_a);
            guard.push(1);
            guard.push(2);
        }
        backend.flush();
        let live_before = backend.live_count();

        // `vec` already owns a live allocation from the pushes above --
        // `store` writing it somewhere new (e.g. as part of assembling a
        // struct field, without ever resetting `vec`'s own pointer)
        // should reuse that allocation rather than leaking it.
        vec.store(&backend, location_b);
        backend.flush();

        assert_eq!(
            backend.live_count(),
            live_before,
            "store() should reuse the existing allocation, not leak a second one"
        );

        let reloaded = PersistedVec::<i32>::load(&backend, location_b);
        assert_eq!(reloaded, vec);
    }

    #[test]
    fn store_allocates_content_for_a_from_iter_constructed_vec() {
        let backend = MockBackend::default();
        let location = root_location(&backend);

        let vec: PersistedVec<i32> = [1, 2, 3].into_iter().collect();
        vec.store(&backend, location);
        backend.flush();

        let reloaded = PersistedVec::<i32>::load(&backend, location);
        assert_eq!(reloaded.data, vec![1, 2, 3]);
    }
}
