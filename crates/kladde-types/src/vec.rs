//! [`PersistableVec`] -- the backed variant of `Vec<T>`, named distinctly from
//! `std::vec::Vec` rather than shadowing it.
//!
//! Snapshot layout: a `P::BYTE_LEN`-byte inline pointer id (`None` encoded as
//! all-zero, the on-file null niche) plus a separate content allocation holding
//! the elements back to back in fixed-size (`T::INLINE_SIZE`) slots, analogous
//! to how `std::vec::Vec` is laid out in memory. No slack/amortized growth yet
//! -- every push/remove resizes the content allocation to exactly fit. Per
//! `later.md`, this straightforward layout is meant to be replaced with a
//! chunked-list representation eventually (`kladde_persist::ChunkedVec` is the
//! prototype of that).
//!
//! ## The length is not stored
//!
//! The old layout carried an inline `{ target, len }` header. It no longer does:
//! the heap already owns each allocation's size, so `len` is recovered as
//! `backend.size(pointer) / T::INLINE_SIZE`. That halves the inline footprint
//! and removes a field that could drift out of step with the allocation.
//!
//! Two consequences worth naming:
//!
//! - **Zero-sized elements are unrepresentable.** A `T` with
//!   `INLINE_SIZE == 0` leaves no way to recover a length from a size, so
//!   `PersistableVec<T>` panics on such a `T` rather than silently reporting
//!   an empty vector. (Nothing in kladde has a zero-sized `Persistable` today
//!   except `()` itself.)
//! - **"Publish the length last" is no longer expressible.** The old `push`
//!   grew the allocation, wrote the element, *then* published the new length,
//!   so a torn journal prefix left the previous, still-valid length. With the
//!   length being the allocation size, growing the allocation *is* publishing
//!   the length. This costs nothing today (the backend applies every operation
//!   immediately -- there is no journal to tear), but it is a property that has
//!   to be re-established when the write-ahead log arrives.

use kladde_persist::{
    Guard, Location, Persistable, Pointer, PointerRepr, ReadBackend, UniquePointerResizable, Word,
    WriteBackend, WriteBackendExt,
};
use std::io::Read;
use std::ops::{Deref, DerefMut};

/// A growable array whose contents are persisted to the backing store.
///
/// Behaves like `std::vec::Vec<T>` for reads (`len`, `get`, `iter`, ...), which
/// touch only the in-memory copy. Mutation goes through a
/// [`PersistableVecGuard`] obtained from [`Persistable::guard`] (or a derived
/// parent's `_mut()` accessor): `push`/`remove`/`get_mut` each update memory
/// *and* record the change to the backend in one step, so persistence is never a
/// separate, forgettable action. The element type `T` only needs to implement
/// [`Persistable`] -- any scalar, container, `PersistableString`, or
/// `#[derive(Persistable)]` type.
#[derive(Debug, PartialEq)]
pub struct PersistableVec<T, P = Pointer> {
    data: Vec<T>,
    /// The variable-capacity content allocation holding this vec's elements --
    /// `None` until the first `push` (or a `store` of a `from_iter`-built vec)
    /// ever needs one. Lazily created rather than eager, since
    /// `PersistableVec::new()` takes no backend to create one with. Its byte
    /// capacity is owned by the allocator, and *is* this vec's length.
    pointer: Option<UniquePointerResizable<P>>,
}

/// The element stride, rejecting the zero-sized case the pointer-only layout
/// cannot represent (see the module docs).
#[inline]
fn stride<T: Persistable<P>, P: PointerRepr>() -> usize {
    let elem = <T as Persistable<P>>::INLINE_SIZE;
    assert!(
        elem > 0,
        "PersistableVec cannot hold a zero-sized element type: its length is \
         recovered from the content allocation's size, which would be 0 for any \
         number of elements",
    );
    elem
}

impl<T, P> PersistableVec<T, P> {
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

    /// Crate-internal escape hatch for [`crate::PersistableString`], the only
    /// thing that needs a raw `&[T]`/`Vec<T>` view rather than going through
    /// `get`/`iter`/`push`/`remove` one element at a time.
    pub(crate) fn as_slice(&self) -> &[T] {
        &self.data
    }

    pub(crate) fn into_data(self) -> Vec<T> {
        self.data
    }
}

impl<T, P> Default for PersistableVec<T, P> {
    fn default() -> Self {
        Self::new()
    }
}

/// Backend-free, like `new()` -- but unlike `new()`, the result may hold real
/// content with `pointer` still `None` if `iter` isn't empty (e.g.
/// `PersistableString::from("hello")` goes through this). `store`'s `None`
/// branch handles that: it's not the same "genuinely never touched" case
/// `new()`/`load` produce, so it can't just assume there's nothing to allocate.
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
    /// Just the content allocation's id -- the length lives with the allocator.
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

    /// Publishes this vec's content pointer at `location` -- used when a whole
    /// `PersistableVec` is being written as a brand-new value somewhere (e.g. a
    /// struct field being assembled) rather than via incremental `push`/`remove`.
    ///
    /// If a pointer already exists, its content is already correct
    /// (`push`/`remove`/`get_mut().set(..)` keep it in sync incrementally) --
    /// `store` only needs to point a new slot at it. The one case that *does*
    /// need real work: `data` non-empty despite `pointer` being `None`, which
    /// happens for a value built via `PersistableVec::from_iter` that has never
    /// been pushed/set through a `Guard`, so there's been no chance yet to
    /// allocate. This is why `store` takes `&mut self` (see the trait doc
    /// comment): it allocates *and* remembers the new pointer in `self`, so a
    /// second `store` reuses it instead of leaking, and any `Guard` obtained
    /// from `self` afterward sees consistent bookkeeping. Each element gets the
    /// same treatment recursively, in case it is itself an "owning" type.
    fn store<B: WriteBackend<Pointer = P>>(&mut self, backend: &B, location: Location<P, B::Size>) {
        if self.pointer.is_none() && !self.data.is_empty() {
            let elem = stride::<T, P>();
            let pointer = backend.alloc_resizable_array::<T>(self.data.len());
            for (i, item) in self.data.iter_mut().enumerate() {
                item.store(
                    backend,
                    Location::new(pointer.raw(), Word::from_usize(i * elem)),
                );
            }
            self.pointer = Some(pointer);
        }
        write_slot(backend, location, self.pointer.as_ref().map(|p| p.raw()));
    }

    fn load<B: ReadBackend<Pointer = P>>(backend: &mut B, location: Location<P, B::Size>) -> Self {
        let Some(target) = read_slot::<P, B>(backend, location) else {
            return PersistableVec::new();
        };
        let elem = stride::<T, P>();
        let len = backend
            .size(target)
            .expect("PersistableVec content allocation is live")
            .to_usize()
            / elem;
        let mut data = Vec::with_capacity(len);
        for i in 0..len {
            data.push(T::load(
                backend,
                Location::new(target, Word::from_usize(i * elem)),
            ));
        }
        PersistableVec {
            data,
            pointer: Some(UniquePointerResizable::from_pointer(target)),
        }
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

/// Writes the inline pointer slot (`None` as the all-zero null niche).
pub(crate) fn write_slot<P: PointerRepr, B: WriteBackend<Pointer = P>>(
    backend: &B,
    location: Location<P, B::Size>,
    pointer: Option<P>,
) {
    backend.write(
        location.anchor,
        location.offset,
        kladde_persist::encode_option(pointer).as_ref(),
    );
}

/// Reads back a slot written by [`write_slot`].
pub(crate) fn read_slot<P: PointerRepr, B: ReadBackend<Pointer = P>>(
    backend: &mut B,
    location: Location<P, B::Size>,
) -> Option<P> {
    let mut bytes = vec![0u8; P::BYTE_LEN];
    backend
        .read_at(location.anchor, location.offset)
        .read_exact(&mut bytes)
        .expect("read inline content pointer");
    kladde_persist::decode_option_slice(&bytes)
}

/// The mutation-capable view onto a [`PersistableVec`].
pub struct PersistableVecGuard<'s, T, B: WriteBackend> {
    inner: &'s mut PersistableVec<T, B::Pointer>,
    backend: &'s B,
    location: Location<B::Pointer, B::Size>,
}

// `get_mut` doesn't need any extra bounds beyond `T: Persistable` -- kept in its
// own impl block so it stays available regardless of what other bounds
// `push`/`remove` need.
impl<'s, T: Persistable<B::Pointer>, B: WriteBackend> PersistableVecGuard<'s, T, B> {
    #[inline]
    pub fn get_mut(
        &mut self,
        index: usize,
    ) -> Option<<T as Persistable<B::Pointer>>::Guard<'_, B>> {
        let elem = stride::<T, B::Pointer>();
        let pointer = self.inner.pointer.as_ref()?;
        let location = Location::new(pointer.raw(), Word::from_usize(index * elem));
        self.inner
            .data
            .get_mut(index)
            .map(|item| item.guard(self.backend, location))
    }
}

impl<'s, T: Persistable<B::Pointer>, B: WriteBackend> PersistableVecGuard<'s, T, B> {
    /// Appends `value`: grows (or creates) the content allocation to fit one
    /// more element, then writes the new element into the freshly-grown slot.
    ///
    /// Growing the allocation *is* publishing the new length (see the module
    /// docs), so unlike the old `{ target, len }` layout there is no separate
    /// header write to order last.
    pub fn push(&mut self, mut value: T) {
        let elem = stride::<T, B::Pointer>();
        let old_len = self.inner.data.len();
        let new_byte_size = Word::from_usize((old_len + 1) * elem);

        match &self.inner.pointer {
            Some(pointer) => self
                .backend
                .resize(pointer, new_byte_size)
                .expect("grow PersistableVec content"),
            None => {
                let pointer = self.backend.alloc_resizable_array::<T>(old_len + 1);
                write_slot(self.backend, self.location, Some(pointer.raw()));
                self.inner.pointer = Some(pointer);
            }
        }
        let pointer = self.inner.pointer.as_ref().unwrap();

        value.store(
            self.backend,
            Location::new(pointer.raw(), Word::from_usize(old_len * elem)),
        );

        self.inner.data.push(value);
    }

    /// Removes and returns the element at `index`, shifting every later element
    /// down to close the gap.
    ///
    /// Runs in `O(n)` in the number of elements after `index`. Panics if `index`
    /// is out of bounds, matching [`Vec::remove`].
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableVec;
    ///
    /// let mut db = Kladde::new(PersistableVec::<i32>::new());
    /// db.guard().push(10);
    /// db.guard().push(20);
    /// db.guard().push(30);
    ///
    /// assert_eq!(db.guard().remove(0), 10);
    /// assert_eq!(db.get().len(), 2);
    /// assert_eq!(db.get().get(0), Some(&20));
    /// ```
    pub fn remove(&mut self, index: usize) -> T {
        let elem = stride::<T, B::Pointer>();
        assert!(
            index < self.inner.data.len(),
            "PersistableVec::remove: index out of bounds"
        );
        let pointer = self
            .inner
            .pointer
            .as_ref()
            .expect("PersistableVec::remove called but no content allocation exists");

        // The splice both shifts the tail down and shrinks the allocation, which
        // is the length update.
        self.backend.splice(
            pointer,
            Word::from_usize(index * elem),
            Word::from_usize(elem),
            &[],
        );

        self.inner.data.remove(index)
    }
}

impl<'s, B: WriteBackend> PersistableVecGuard<'s, u8, B> {
    /// Replaces the entire contents with `new` in a single bulk update.
    ///
    /// Runs in `O(n)` in the new length, regardless of the current length, so
    /// replacing a whole value is far cheaper than clearing and re-pushing one
    /// byte at a time. Accepts anything convertible into a `Vec<u8>`; an owned
    /// `Vec<u8>` is moved in without copying, while a slice or byte-string
    /// literal is copied. Passing an empty value clears the contents and
    /// releases the backing allocation.
    ///
    /// Only available for byte vectors (`PersistableVec<u8>`), for which a
    /// whole-buffer replacement is a single contiguous write.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableVec;
    ///
    /// let mut db = Kladde::new(PersistableVec::<u8>::new());
    /// db.guard().set(b"hello"); // a literal is accepted directly
    /// assert_eq!(db.get().len(), 5);
    ///
    /// db.guard().set(vec![b'h', b'i']); // an owned Vec is moved in, no copy
    /// assert_eq!(db.get().len(), 2);
    /// assert_eq!(db.get().get(0), Some(&b'h'));
    /// ```
    pub fn set(&mut self, new: impl Into<Vec<u8>>) {
        // Non-generic inner fn: the real body compiles once per backend, rather
        // than being re-monomorphized for every `Into` argument type.
        fn inner<B: WriteBackend>(guard: &mut PersistableVecGuard<'_, u8, B>, new: Vec<u8>) {
            let old_len = guard.inner.data.len();

            // Empty content returns to the lazy no-allocation state (empty <=>
            // no allocation, the invariant `PersistableBlob` relies on), freeing
            // any existing region. The null slot is published *before* the free,
            // so a torn prefix leaves the value already empty with the old
            // region merely unreferenced, never dangling.
            if new.is_empty() {
                if let Some(pointer) = guard.inner.pointer.take() {
                    write_slot(guard.backend, guard.location, None);
                    guard.backend.free_resizable(pointer);
                }
                guard.inner.data.clear();
                return;
            }

            match &guard.inner.pointer {
                None => {
                    // No allocation yet -- alloc, fill, then publish.
                    let pointer = guard.backend.alloc_resizable_array::<u8>(new.len());
                    guard.backend.write(pointer.raw(), Word::zero(), &new);
                    write_slot(guard.backend, guard.location, Some(pointer.raw()));
                    guard.inner.pointer = Some(pointer);
                }
                Some(pointer) => {
                    // Replace the entire old content in one atomic op; the
                    // splice's resize is the length update.
                    guard
                        .backend
                        .splice(pointer, Word::zero(), Word::from_usize(old_len), &new);
                }
            }
            guard.inner.data = new;
        }

        inner(self, new.into())
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

impl<'s, T, B: WriteBackend> DerefMut for PersistableVecGuard<'s, T, B> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{root_location, MockBackend};

    fn root(backend: &MockBackend) -> Location<Pointer, u32> {
        root_location(backend, <PersistableVec<i32> as Persistable>::INLINE_SIZE)
    }

    #[test]
    fn push_appends_in_memory() {
        let backend = MockBackend::default();
        let location = root(&backend);
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
        let location = root(&backend);
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
        let location = root(&backend);
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
    fn reloading_round_trips_the_content() {
        let mut backend = MockBackend::default();
        let location = root(&backend);
        let mut vec = PersistableVec::<i32>::new();
        {
            let mut guard = vec.guard(&backend, location);
            guard.push(10);
            guard.push(20);
            guard.push(30);
        }
        vec.guard(&backend, location).remove(1);

        let reloaded = <PersistableVec<i32> as Persistable>::load(&mut backend, location);
        assert_eq!(reloaded.as_slice(), &[10, 30]);
        assert_eq!(reloaded, vec);
    }

    #[test]
    fn the_length_comes_back_from_the_allocation_size() {
        let mut backend = MockBackend::default();
        let location = root(&backend);
        let mut vec = PersistableVec::<i32>::new();
        {
            let mut guard = vec.guard(&backend, location);
            for i in 0..7 {
                guard.push(i);
            }
        }
        // Nothing wrote a length anywhere: the inline slot is just a pointer.
        assert_eq!(<PersistableVec<i32> as Persistable>::INLINE_SIZE, 4);
        assert_eq!(
            <PersistableVec<i32> as Persistable>::load(&mut backend, location).len(),
            7
        );
    }

    #[test]
    fn set_grows_and_shrinks_reusing_one_allocation() {
        let mut backend = MockBackend::default();
        let location = root_location(&backend, <PersistableVec<u8> as Persistable>::INLINE_SIZE);
        let mut vec = PersistableVec::<u8>::new();

        for content in [b"hello".as_slice(), b"hi".as_slice(), b"hello there"] {
            vec.guard(&backend, location).set(content);
            assert_eq!(vec.as_slice(), content);
            assert_eq!(backend.live_count(), 2, "one root + one content allocation");
            let reloaded = <PersistableVec<u8> as Persistable>::load(&mut backend, location);
            assert_eq!(reloaded.as_slice(), content);
        }
    }

    #[test]
    fn set_to_empty_frees_and_returns_to_lazy() {
        let mut backend = MockBackend::default();
        let location = root_location(&backend, <PersistableVec<u8> as Persistable>::INLINE_SIZE);
        let mut vec = PersistableVec::<u8>::new();

        vec.guard(&backend, location).set(b"content");
        assert_eq!(backend.live_count(), 2);

        vec.guard(&backend, location).set(b"");
        assert!(vec.is_empty());
        assert_eq!(backend.live_count(), 1, "content allocation was released");
        assert!(<PersistableVec<u8> as Persistable>::load(&mut backend, location).is_empty());

        // ...and it can be filled again afterwards.
        vec.guard(&backend, location).set(b"again");
        assert_eq!(vec.as_slice(), b"again");
        assert_eq!(backend.live_count(), 2);
    }

    #[test]
    fn store_allocates_content_for_a_from_iter_constructed_vec() {
        let mut backend = MockBackend::default();
        let location = root(&backend);
        let mut vec = PersistableVec::from_iter([1, 2, 3]);

        vec.store(&backend, location);

        let reloaded = <PersistableVec<i32> as Persistable>::load(&mut backend, location);
        assert_eq!(reloaded.as_slice(), &[1, 2, 3]);
    }

    #[test]
    fn store_reuses_an_existing_allocation_instead_of_leaking_it() {
        let mut backend = MockBackend::default();
        let location = root(&backend);
        let mut vec = PersistableVec::from_iter([1, 2, 3]);

        vec.store(&backend, location);
        let after_first = backend.live_count();
        vec.store(&backend, location);

        assert_eq!(backend.live_count(), after_first, "no second allocation");
        assert_eq!(
            <PersistableVec<i32> as Persistable>::load(&mut backend, location).as_slice(),
            &[1, 2, 3]
        );
    }

    #[test]
    fn store_on_a_from_iter_constructed_vec_remembers_its_own_pointer() {
        let mut backend = MockBackend::default();
        let location = root(&backend);
        let mut vec = PersistableVec::from_iter([1, 2, 3]);
        vec.store(&backend, location);

        // The pointer learned during `store` is what a later `push` grows.
        vec.guard(&backend, location).push(4);

        assert_eq!(vec.get(3), Some(&4));
        assert_eq!(
            <PersistableVec<i32> as Persistable>::load(&mut backend, location).as_slice(),
            &[1, 2, 3, 4]
        );
    }

    #[test]
    fn an_empty_vec_stores_as_a_null_pointer_and_allocates_nothing() {
        let mut backend = MockBackend::default();
        let location = root(&backend);
        let mut vec = PersistableVec::<i32>::new();

        vec.store(&backend, location);

        assert_eq!(backend.live_count(), 1, "just the root allocation");
        assert!(<PersistableVec<i32> as Persistable>::load(&mut backend, location).is_empty());
    }
}
