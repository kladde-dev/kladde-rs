//! [`UnjournaledBackend`]: the simplest concrete backend -- a
//! [`Composed`]`(Storage, Allocator)` with no journal, applying every operation
//! immediately.
//!
//! It is a *composition*, not an extension of `Allocator`, which is exactly why
//! it can present the `&self` write facade: the `Composed` core is wrapped in a
//! `RefCell`, so `WriteBackend`'s `&self` methods borrow it mutably per call
//! while the reusable `Allocator` keeps its clean `&mut self` API. The
//! `ReadBackend` methods take `&mut self` and reach the core via
//! `RefCell::get_mut` -- no runtime borrow, and the real seekable cursor comes
//! straight out. Addresses never leave the backend.
//!
//! Probably not directly useful for Kladde (which wants journaling), but a good
//! versatility test and the foundation the journaled backend reuses.

use std::io::{Read, Seek};

use crate::allocator::{AllocError, Allocator, SimpleAllocator};
use crate::backend::{Backend, BackendError, ReadBackend, WriteBackend};
use crate::composed::Composed;
use crate::pointer::{ResolvedPointer, UniquePointerFixedSize, UniquePointerResizable};
use crate::storage::{InMemoryStorage, Storage};
use std::cell::RefCell;

/// A non-journaled backend composing a `Storage` and an `Allocator`.
pub struct UnjournaledBackend<S, A> {
    inner: RefCell<Composed<S, A>>,
}

impl<S: Storage, A: Allocator> UnjournaledBackend<S, A> {
    pub fn new(storage: S, alloc: A) -> Self {
        Self {
            inner: RefCell::new(Composed::new(storage, alloc)),
        }
    }

    /// Consume the backend, returning the inner storage and allocator (handy for
    /// tests that want to re-open or inspect the raw state).
    pub fn into_parts(self) -> (S, A) {
        let composed = self.inner.into_inner();
        (composed.storage, composed.alloc)
    }
}

impl UnjournaledBackend<InMemoryStorage, SimpleAllocator> {
    /// Number of live allocations. Concrete because `live_count` is a
    /// `SimpleAllocator` inherent, not part of the `Allocator` trait; used by
    /// `MockBackend` for downstream leak checks.
    pub fn live_count(&self) -> usize {
        self.inner.borrow().alloc.live_count()
    }
}

impl<S: Storage, A: Allocator> Backend for UnjournaledBackend<S, A> {
    type Pointer = A::Pointer;
    type Size = A::Size;

    fn size(&self, p: Self::Pointer) -> Result<Self::Size, AllocError> {
        self.inner.borrow().alloc.size(p)
    }
    fn resolve(&self, p: Self::Pointer) -> Result<ResolvedPointer<Self::Pointer>, AllocError> {
        self.inner.borrow().alloc.resolve(p)
    }
}

impl<S: Storage, A: Allocator> WriteBackend for UnjournaledBackend<S, A> {
    fn alloc_resizable(&self, size: Self::Size) -> UniquePointerResizable<Self::Pointer> {
        self.inner.borrow_mut().alloc_resizable(size)
    }
    fn alloc_fixed_size(&self, size: Self::Size) -> UniquePointerFixedSize<Self::Pointer> {
        self.inner.borrow_mut().alloc_fixed_size(size)
    }
    fn free_resizable(&self, p: UniquePointerResizable<Self::Pointer>) {
        self.inner.borrow_mut().free_resizable(p);
    }
    fn free_fixed_size(&self, p: UniquePointerFixedSize<Self::Pointer>) {
        self.inner.borrow_mut().free_fixed_size(p);
    }
    fn resize(
        &self,
        p: &UniquePointerResizable<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<(), BackendError> {
        self.inner.borrow_mut().resize(p, new_size)
    }
    fn make_resizable(
        &self,
        p: UniquePointerFixedSize<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<UniquePointerResizable<Self::Pointer>, BackendError> {
        self.inner.borrow_mut().make_resizable(p, new_size)
    }
    fn make_fixed_size(
        &self,
        p: UniquePointerResizable<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<UniquePointerFixedSize<Self::Pointer>, BackendError> {
        self.inner.borrow_mut().make_fixed_size(p, new_size)
    }
    fn write(&self, anchor: Self::Pointer, offset: Self::Size, bytes: &[u8]) {
        self.inner.borrow_mut().write(anchor, offset, bytes);
    }
    fn splice(
        &self,
        p: &UniquePointerResizable<Self::Pointer>,
        offset: Self::Size,
        old_len: Self::Size,
        new: &[u8],
    ) {
        self.inner.borrow_mut().splice(p, offset, old_len, new);
    }
}

impl<S: Storage, A: Allocator> ReadBackend for UnjournaledBackend<S, A> {
    fn read_at(&mut self, anchor: Self::Pointer, offset: Self::Size) -> impl Read + Seek + '_ {
        self.inner.get_mut().read_at(anchor, offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pointer::Pointer;
    use crate::storage::InMemoryStorage;
    use crate::SimpleAllocator;
    use std::io::Read;

    fn backend() -> UnjournaledBackend<InMemoryStorage, SimpleAllocator> {
        UnjournaledBackend::new(InMemoryStorage::default(), SimpleAllocator::new())
    }

    #[test]
    fn alloc_write_read_round_trip() {
        let mut b = backend();
        let p = b.alloc_fixed_size(4);
        b.write(p.raw(), 0, &[1, 2, 3, 4]);

        let mut cursor = b.read_at(p.raw(), 1);
        let mut buf = [0u8; 2];
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [2, 3]);
    }

    #[test]
    fn read_hands_out_a_genuinely_seekable_cursor() {
        let mut b = backend();
        let p = b.alloc_fixed_size(8);
        b.write(p.raw(), 0, &[1, 2, 3, 4, 5, 6, 7, 8]);

        let mut cursor = b.read_at(p.raw(), 2);
        let mut one = [0u8; 1];
        cursor.read_exact(&mut one).unwrap();
        assert_eq!(one, [3]);
        // rewind on the handed-out cursor to prove Seek works
        cursor.seek(std::io::SeekFrom::Current(-1)).unwrap();
        cursor.read_exact(&mut one).unwrap();
        assert_eq!(one, [3]);
    }

    #[test]
    fn resize_relocation_moves_bytes_in_storage() {
        let mut b = backend();
        let p = b.alloc_resizable(4);
        b.write(p.raw(), 0, &[9, 8, 7, 6]);
        // grow -> the bump allocator relocates -> the backend copies the bytes
        b.resize(&p, 8).unwrap();
        let mut cursor = b.read_at(p.raw(), 0);
        let mut buf = [0u8; 4];
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [9, 8, 7, 6]);
    }

    #[test]
    fn size_of_a_dangling_id_is_an_error() {
        let b = backend();
        let bogus = Pointer::from_raw(999).unwrap();
        assert_eq!(b.size(bogus), Err(AllocError::DanglingPointer));
    }

    #[test]
    fn make_resizable_then_grow_preserves_data() {
        let mut b = backend();
        let fixed = b.alloc_fixed_size(4);
        b.write(fixed.raw(), 0, &[1, 2, 3, 4]);
        let resizable = b.make_resizable(fixed, 4).unwrap();
        b.resize(&resizable, 8).unwrap();
        let mut cursor = b.read_at(resizable.raw(), 0);
        let mut buf = [0u8; 4];
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);
    }

    #[test]
    fn splice_replaces_a_range_and_shifts_the_tail() {
        let mut b = backend();
        let p = b.alloc_resizable(6);
        b.write(p.raw(), 0, &[1, 2, 3, 4, 5, 6]);
        // replace the 2 bytes at offset 1 with 3 bytes -> size grows to 7
        b.splice(&p, 1, 2, &[9, 9, 9]);
        assert_eq!(b.size(p.raw()), Ok(7));
        let mut cursor = b.read_at(p.raw(), 0);
        let mut buf = [0u8; 7];
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [1, 9, 9, 9, 4, 5, 6]);
    }

    #[test]
    fn free_reduces_live_count() {
        let b = backend();
        let p = b.alloc_fixed_size(4);
        b.free_fixed_size(p);
        let (_s, a) = b.into_parts();
        assert_eq!(a.live_count(), 0);
    }
}
