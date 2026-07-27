//! [`PersistableVec`] -- the backed variant of `Vec<T>`, named distinctly
//! from `std::vec::Vec` rather than shadowing it.
//!
//! Snapshot layout: a small, fixed 8-byte inline header (`target`, `len`
//! -- see `kladde_traits::write_header`) plus a separate content
//! allocation holding `len` fixed-size (`T::INLINE_SIZE`) element slots,
//! analogous to how `std::vec::Vec` is laid out in memory. No
//! slack/amortized growth yet -- every push/remove resizes the content
//! allocation to exactly fit. Per `later.md`, this straightforward layout
//! is meant to be replaced with a chunked-list representation eventually.

use kladde_traits::{
    read_header, write_header, Backend, Guard, Location, Persistable, RawPointer,
    UniqueArrayPointer,
};
use std::ops::{Deref, DerefMut};

/// A growable array whose contents are persisted to the backing store.
///
/// Behaves like `std::vec::Vec<T>` for reads (`len`, `get`, `iter`, ...),
/// which touch only the in-memory copy. Mutation goes through a
/// [`PersistableVecGuard`] obtained from [`Persistable::guard`] (or a
/// derived parent's `_mut()` accessor): `push`/`remove`/`get_mut` each
/// update memory *and* record the change to the backend in one step, so
/// persistence is never a separate, forgettable action. The element type
/// `T` only needs to implement [`Persistable`] -- any scalar, container,
/// `PersistableString`, or `#[derive(Persistable)]` type.
#[derive(Debug, PartialEq)]
pub struct PersistableVec<T> {
    data: Vec<T>,
    /// The variable-capacity content allocation holding this vec's elements
    /// -- `None` until the first `push` (or a `store` of a
    /// `from_iter`-built vec) ever needs one. Lazily created rather than
    /// eager, since `PersistableVec::new()` takes no `Backend` to create
    /// one with. A [`UniqueArrayPointer`] (the `Box<[T]>` analog): its byte
    /// capacity is owned by the allocator, while this vec keeps only the
    /// logical element count (in `data`, published as the header `len`).
    pointer: Option<UniqueArrayPointer<PersistableVec<T>>>,
}

impl<T> PersistableVec<T> {
    pub fn new() -> Self {
        PersistableVec {
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

    /// Crate-internal escape hatch for [`crate::PersistableString`], the
    /// only thing that needs a raw `&[T]`/`Vec<T>` view rather than going
    /// through `get`/`iter`/`push`/`remove` one element at a time.
    pub(crate) fn as_slice(&self) -> &[T] {
        &self.data
    }

    pub(crate) fn into_data(self) -> Vec<T> {
        self.data
    }
}

impl<T> Default for PersistableVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Backend-free, like `new()` -- but unlike `new()`, the result may hold
/// real content with `pointer` still `None` if `iter` isn't empty (e.g.
/// `PersistableString::from("hello")` goes through this). `store`'s `None`
/// branch below handles that: it's not the same "genuinely never
/// touched" case `new()`/`load` produce, so it can't just assume there's
/// nothing to allocate.
impl<T> FromIterator<T> for PersistableVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        PersistableVec {
            data: Vec::from_iter(iter),
            pointer: None,
        }
    }
}

impl<'a, T> IntoIterator for &'a PersistableVec<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.data.iter()
    }
}

impl<T: Persistable> Persistable for PersistableVec<T> {
    /// A fixed 8-byte `{ target, len }` header -- the content allocation
    /// itself (`len * T::INLINE_SIZE` bytes) is separate.
    const INLINE_SIZE: usize = 8;

    type Guard<'s, B: Backend>
        = PersistableVecGuard<'s, T, B>
    where
        Self: 's,
        B: 's;

    fn guard<'s, B: Backend>(
        &'s mut self,
        backend: &'s B,
        location: Location,
    ) -> Self::Guard<'s, B> {
        PersistableVecGuard {
            inner: self,
            backend,
            location,
        }
    }

    /// Publishes a header at `location` pointing at this vec's content --
    /// used when a whole `PersistableVec` is being written as a brand-new
    /// value somewhere (e.g. a struct field being assembled) rather than
    /// via incremental `push`/`remove`.
    ///
    /// If a pointer already exists, its content is already correct
    /// (`push`/`remove`/`get_mut().set(...)` keep it in sync
    /// incrementally) -- `store` only needs to point a new header at it,
    /// not rewrite anything. The one case that *does* need real work:
    /// `data` non-empty despite `pointer` being `None`, which happens for
    /// a value built via `PersistableVec::from_iter` (see its doc comment)
    /// that's never been pushed/set through a `Guard`, so there's been no
    /// chance yet to allocate. This is why `store` takes `&mut self`, not
    /// `&self` (see the trait doc comment): it allocates *and* remembers
    /// the new pointer in `self`, so a second `store` call later reuses
    /// it instead of allocating (and leaking) again, and so any `Guard`
    /// obtained from `self` afterward (e.g. via a container's `get_mut`)
    /// sees consistent bookkeeping. Each element gets the same treatment
    /// recursively, in case it's itself an "owning" type with the same
    /// possible gap (e.g. a `PersistableString`).
    fn store<B: Backend>(&mut self, backend: &B, location: Location) {
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
                let elem_size = T::INLINE_SIZE as u32;
                let byte_size = self.data.len() * T::INLINE_SIZE;
                let pointer = backend.alloc_array::<PersistableVec<T>>(byte_size);
                for (i, item) in self.data.iter_mut().enumerate() {
                    item.store(
                        backend,
                        Location {
                            anchor: pointer.raw(),
                            offset: i as u32 * elem_size,
                        },
                    );
                }
                write_header(backend, location, pointer.index(), self.data.len() as u32);
                self.pointer = Some(pointer);
            }
        }
    }

    fn load<B: Backend>(backend: &B, location: Location) -> Self {
        let (target, len) = read_header(backend, location);
        let pointer = target.map(UniqueArrayPointer::from_index);
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
        PersistableVec { data, pointer }
    }

    fn describe_local(builder: &mut kladde_traits::SchemaBuilder) -> kladde_traits::TypeDescriptor
    where
        Self: 'static,
    {
        kladde_traits::TypeDescriptor::Opaque {
            library_name: "kladde-types".into(),
            type_name: "PersistableVec".into(),
            version: crate::library_version(),
            inline_size: 8,
            parameters: vec![<T as Persistable>::describe(builder)],
        }
    }
}

/// `B` defaults to [`kladde::DefaultBackend`](../../kladde/struct.DefaultBackend.html)
/// so application code that only ever uses the default backend never has
/// to name it.
pub struct PersistableVecGuard<'s, T, B = kladde::DefaultBackend> {
    inner: &'s mut PersistableVec<T>,
    backend: &'s B,
    location: Location,
}

// `get_mut` doesn't need any extra bounds beyond `T: Persistable` --
// kept in its own impl block, as before, so it stays available
// regardless of what other bounds `push`/`remove` need.
impl<'s, T: Persistable, B: Backend> PersistableVecGuard<'s, T, B> {
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

impl<'s, T: Persistable, B: Backend> PersistableVecGuard<'s, T, B> {
    /// Appends `value`: grows (or creates) the content allocation to fit
    /// one more element, writes the new element into the freshly-grown
    /// slot, then publishes the updated header -- in that order, so any
    /// crash/torn-journal prefix of this sequence leaves the previous,
    /// still-valid state (the header update is always last -- see
    /// `spec.md`'s Crash Consistency section).
    pub fn push(&mut self, mut value: T) {
        let elem_size = T::INLINE_SIZE as u32;
        let old_len = self.inner.data.len();
        let new_len = old_len + 1;
        let new_byte_size = new_len * T::INLINE_SIZE;

        match &self.inner.pointer {
            Some(pointer) => self.backend.resize_array(pointer, new_byte_size),
            None => {
                self.inner.pointer =
                    Some(self.backend.alloc_array::<PersistableVec<T>>(new_byte_size))
            }
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

    /// Removes and returns the element at `index`, deleting its slot's bytes
    /// with a single [`splice`](kladde_traits::Allocator::splice) -- which
    /// shifts every later element down and shrinks the allocation as one
    /// atomic op -- then publishing the new (shorter) header. Panics if
    /// `index` is out of bounds (matches `Vec::remove`).
    ///
    /// The `splice` collapses the old `copy`-then-`resize` pair (whose
    /// intermediate state duplicated the last element) into one entry, so
    /// the content allocation is never observed mid-shift. What `splice`
    /// alone can't close is the header: it lives in a *separate* region
    /// (the parent anchor), so a torn journal that applied the `splice` but
    /// not the header write would see the shorter content under the old,
    /// longer count. Fully closing that needs the length to come from the
    /// allocator's capacity rather than the header (plan decision 4, still
    /// deferred -- see `array-pointer-problems.md`); until then this is the
    /// tightest ordering.
    pub fn remove(&mut self, index: usize) -> T {
        let elem_size = T::INLINE_SIZE as u32;
        let old_len = self.inner.data.len();
        assert!(
            index < old_len,
            "PersistableVec::remove: index out of bounds"
        );
        let new_len = old_len - 1;
        let pointer = self
            .inner
            .pointer
            .as_ref()
            .expect("PersistableVec::remove called but no content allocation exists");

        self.backend
            .splice(pointer, index as u32 * elem_size, elem_size, &[]);
        write_header(self.backend, self.location, pointer.index(), new_len as u32);

        self.inner.data.remove(index)
    }
}

impl<'s, B: Backend> PersistableVecGuard<'s, u8, B> {
    /// Replaces the entire byte content with `new` in one bulk write,
    /// instead of the `O(n^2)` pop-then-repush a whole-value replace would
    /// otherwise be (see [`crate::PersistableStringGuard::set`], its first
    /// consumer). Specialized to `u8` so the content is a single `write`
    /// rather than a per-element `store`; the generic whole-array replace
    /// is deferred (freeing owning elements' sub-allocations needs a trait
    /// hook that doesn't exist yet -- see `array-pointer-problems.md`).
    ///
    /// Replaces the whole content with one atomic
    /// [`splice`](kladde_traits::Allocator::splice) (resize + overwrite in a
    /// single journal entry), so the content allocation is never observed
    /// half-updated -- no more grow/shrink ordering split or
    /// trailing-slack-tolerance requirement. Every owning consumer
    /// (`PersistableString`, `PersistableBlob`) inherits this by routing
    /// through here rather than re-deriving its own ordering.
    ///
    /// **Residual window.** The `splice` makes the *content* atomic, but the
    /// length still lives in a separate header region, published by a
    /// distinct write: a torn journal that applied the `splice` but not the
    /// header would see new content under the old length. Closing that last
    /// gap needs the length to come from the allocator's capacity instead of
    /// the header (plan decision 4, deferred -- see
    /// `array-pointer-problems.md`).
    pub fn set_content(&mut self, new: &[u8]) {
        let old_len = self.inner.data.len();
        let new_len = new.len();

        // Empty content returns to the lazy no-allocation state (empty
        // <=> no allocation, the invariant `PersistableBlob` relies on),
        // freeing any existing region. The empty header is published
        // *before* the free, so a torn-journal prefix leaves the value
        // already empty with the old region merely unreferenced (reclaimed
        // at the next flush), never dangling -- the same shape blob's old
        // `set_to_default` used.
        if new_len == 0 {
            if let Some(pointer) = self.inner.pointer.take() {
                self.backend
                    .write(self.location.anchor, self.location.offset, &[0u8; 8]);
                self.backend.free_array(pointer);
            }
            self.inner.data.clear();
            return;
        }

        match &self.inner.pointer {
            None => {
                // No allocation yet -- alloc then publish (append-then-
                // publish; there's no old content to splice against).
                let pointer = self.backend.alloc_array::<PersistableVec<u8>>(new_len);
                self.backend.write(pointer.raw(), 0, new);
                write_header(self.backend, self.location, pointer.index(), new_len as u32);
                self.inner.pointer = Some(pointer);
            }
            Some(pointer) => {
                // Replace the entire old content in one atomic op, then
                // republish the (possibly changed) length.
                self.backend.splice(pointer, 0, old_len as u32, new);
                write_header(self.backend, self.location, pointer.index(), new_len as u32);
            }
        }
        self.inner.data = new.to_vec();
    }
}

impl<'s, T: Persistable, B: Backend> Guard for PersistableVecGuard<'s, T, B> {
    type Persistable = PersistableVec<T>;
    type Backend = B;

    fn as_persistable(&self) -> &PersistableVec<T> {
        self.inner
    }
    fn as_persistable_mut(&mut self) -> &mut PersistableVec<T> {
        self.inner
    }
    fn backend(&self) -> &B {
        self.backend
    }
}

impl<'s, T, B> Deref for PersistableVecGuard<'s, T, B> {
    type Target = PersistableVec<T>;
    fn deref(&self) -> &PersistableVec<T> {
        self.inner
    }
}

impl<'s, T, B> DerefMut for PersistableVecGuard<'s, T, B> {
    fn deref_mut(&mut self) -> &mut PersistableVec<T> {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockBackend;
    use kladde_traits::Allocator;

    fn root_location(backend: &MockBackend) -> Location {
        let pointer = backend.alloc::<()>(PersistableVec::<i32>::INLINE_SIZE);
        Location {
            anchor: pointer.raw(),
            offset: 0,
        }
    }

    #[test]
    fn push_appends_in_memory() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut vec = PersistableVec::<i32>::new();

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
        let mut vec = PersistableVec::<i32>::new();
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
        let mut vec = PersistableVec::<i32>::new();
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
        let mut vec = PersistableVec::<i32>::new();
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

        let reloaded = PersistableVec::<i32>::load(&backend, location);
        assert_eq!(reloaded.data, vec![10, 30]);
        assert_eq!(reloaded, vec);
    }

    #[test]
    fn set_content_grows_and_shrinks_reusing_one_allocation() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut vec = PersistableVec::<u8>::new();

        vec.guard(&backend, location).set_content(b"start");
        backend.flush();
        let live = backend.live_count();

        for content in [
            &b"a much longer byte string"[..],
            &b"x"[..],
            &b"equalize!"[..],
            &b"back again"[..],
        ] {
            vec.guard(&backend, location).set_content(content);
            backend.flush();
            assert_eq!(vec.as_slice(), content);
            assert_eq!(
                backend.live_count(),
                live,
                "set_content must reuse the one allocation, not leak"
            );
            let reloaded = PersistableVec::<u8>::load(&backend, location);
            assert_eq!(reloaded.as_slice(), content);
        }
    }

    #[test]
    fn set_content_to_empty_frees_and_returns_to_lazy() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut vec = PersistableVec::<u8>::new();

        vec.guard(&backend, location).set_content(b"some content");
        backend.flush();
        let live = backend.live_count();
        assert!(live > 0);

        vec.guard(&backend, location).set_content(b"");
        backend.flush();
        assert_eq!(
            backend.live_count(),
            live - 1,
            "emptying should free the content allocation"
        );
        assert!(vec.is_empty());
        assert!(PersistableVec::<u8>::load(&backend, location).is_empty());

        // Re-populating after emptying allocates a fresh region and works.
        vec.guard(&backend, location).set_content(b"again");
        backend.flush();
        assert_eq!(vec.as_slice(), b"again");
        assert_eq!(
            PersistableVec::<u8>::load(&backend, location).as_slice(),
            b"again"
        );
    }

    #[test]
    fn set_content_on_a_fresh_empty_vec_stays_lazy() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        backend.flush(); // materialize the root anchor itself
        let live_before = backend.live_count();

        let mut vec = PersistableVec::<u8>::new();
        vec.guard(&backend, location).set_content(b"");
        backend.flush();

        assert_eq!(
            backend.live_count(),
            live_before,
            "replacing an empty vec with empty content shouldn't allocate"
        );
    }

    #[test]
    fn store_reuses_an_existing_allocation_instead_of_leaking_it() {
        let backend = MockBackend::default();
        let location_a = root_location(&backend);
        let location_b = root_location(&backend);

        let mut vec = PersistableVec::<i32>::new();
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

        let reloaded = PersistableVec::<i32>::load(&backend, location_b);
        assert_eq!(reloaded, vec);
    }

    #[test]
    fn store_allocates_content_for_a_from_iter_constructed_vec() {
        let backend = MockBackend::default();
        let location = root_location(&backend);

        let mut vec: PersistableVec<i32> = [1, 2, 3].into_iter().collect();
        vec.store(&backend, location);
        backend.flush();

        let reloaded = PersistableVec::<i32>::load(&backend, location);
        assert_eq!(reloaded.data, vec![1, 2, 3]);
    }

    #[test]
    fn store_on_a_from_iter_constructed_vec_remembers_its_own_pointer() {
        let backend = MockBackend::default();
        let location = root_location(&backend);

        // Constructed without a backend, so `pointer` starts `None` even
        // though `data` is non-empty -- `store`'s first call has to
        // allocate. It must also remember that allocation in `self`, or
        // a later `push` (via `get_mut`-adjacent bookkeeping) would
        // either panic or allocate (and leak) a second time.
        let mut vec: PersistableVec<i32> = [1, 2, 3].into_iter().collect();
        vec.store(&backend, location);
        backend.flush();
        let live_before = backend.live_count();

        vec.guard(&backend, location).push(4);
        backend.flush();

        assert_eq!(
            backend.live_count(),
            live_before,
            "push() should reuse the allocation store() already made, not leak a second one"
        );
        assert_eq!(vec.get(3), Some(&4));

        let reloaded = PersistableVec::<i32>::load(&backend, location);
        assert_eq!(reloaded.data, vec![1, 2, 3, 4]);
    }
}
