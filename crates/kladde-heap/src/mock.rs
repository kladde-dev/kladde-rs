//! [`MockBackend`]: a public, self-contained, in-memory backend for testing
//! `Persistable` types (and containers) in *other* crates without wiring up a
//! real `Storage` or `Allocator`.
//!
//! It is a newtype over `UnjournaledBackend<InMemoryStorage, GainGreedyHeap>`:
//! the storage stays private (per `generic-allocator.md`, the public vehicle is
//! a `MockBackend` that hides addresses, not a public `MockStorage`), and the
//! backend traits are re-exposed by delegation. Not `#[cfg(test)]`-gated, so
//! downstream crates can use it in their own tests.

use std::io::{Read, Seek};

use crate::backend::{Backend, BackendError, ReadBackend, WriteBackend};
use crate::gain_greedy::GainGreedyHeap;
use crate::pointer::{Pointer, ResolvedPointer, UniquePointerFixedSize, UniquePointerResizable};
use crate::storage::InMemoryStorage;
use crate::unjournaled::UnjournaledBackend;

/// An in-memory [`WriteBackend`] + [`ReadBackend`] with `Pointer = Pointer<u32>`
/// and `Size = u32`, for exercising persistable types in tests.
pub struct MockBackend(UnjournaledBackend<InMemoryStorage, GainGreedyHeap<Pointer<u32>>>);

impl MockBackend {
    pub fn new() -> Self {
        Self(UnjournaledBackend::new(
            InMemoryStorage::default(),
            GainGreedyHeap::new(),
        ))
    }

    /// Number of live (not-yet-freed) allocations -- useful for leak checks in
    /// downstream tests.
    pub fn live_count(&self) -> usize {
        self.0.live_count()
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for MockBackend {
    type Pointer = Pointer;
    type Size = u32;

    fn size(&self, p: Self::Pointer) -> Result<Self::Size, BackendError> {
        self.0.size(p)
    }
    fn resolve(&self, p: Self::Pointer) -> Result<ResolvedPointer<Self::Pointer>, BackendError> {
        self.0.resolve(p)
    }
}

impl WriteBackend for MockBackend {
    fn alloc_resizable(&self, size: Self::Size) -> UniquePointerResizable<Self::Pointer> {
        self.0.alloc_resizable(size)
    }
    fn alloc_fixed_size(&self, size: Self::Size) -> UniquePointerFixedSize<Self::Pointer> {
        self.0.alloc_fixed_size(size)
    }
    fn free_resizable(&self, p: UniquePointerResizable<Self::Pointer>) {
        self.0.free_resizable(p);
    }
    fn free_fixed_size(&self, p: UniquePointerFixedSize<Self::Pointer>) {
        self.0.free_fixed_size(p);
    }
    fn resize(
        &self,
        p: &UniquePointerResizable<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<(), BackendError> {
        self.0.resize(p, new_size)
    }
    fn make_resizable(
        &self,
        p: UniquePointerFixedSize<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<UniquePointerResizable<Self::Pointer>, BackendError> {
        self.0.make_resizable(p, new_size)
    }
    fn make_fixed_size(
        &self,
        p: UniquePointerResizable<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<UniquePointerFixedSize<Self::Pointer>, BackendError> {
        self.0.make_fixed_size(p, new_size)
    }
    fn write(&self, anchor: Self::Pointer, offset: Self::Size, bytes: &[u8]) {
        self.0.write(anchor, offset, bytes);
    }
    fn splice(
        &self,
        p: &UniquePointerResizable<Self::Pointer>,
        offset: Self::Size,
        old_len: Self::Size,
        new: &[u8],
    ) {
        self.0.splice(p, offset, old_len, new);
    }
}

impl ReadBackend for MockBackend {
    fn read_at(&mut self, anchor: Self::Pointer, offset: Self::Size) -> impl Read + Seek + '_ {
        self.0.read_at(anchor, offset)
    }
}

impl crate::CompactingBackend for MockBackend {
    fn compact_incrementally(&self, budget: usize) -> crate::CompactionProgress {
        self.0.compact_incrementally(budget as u64)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_backend_round_trips_and_counts_allocations() {
        let mut b = MockBackend::new();
        let p = b.alloc_fixed_size(4);
        assert_eq!(b.live_count(), 1);
        b.write(p.raw(), 0, &[10, 20, 30, 40]);

        let mut buf = [0u8; 4];
        {
            let mut cursor = b.read_at(p.raw(), 0);
            cursor.read_exact(&mut buf).unwrap();
        }
        assert_eq!(buf, [10, 20, 30, 40]);

        b.free_fixed_size(p);
        assert_eq!(b.live_count(), 0);
    }
}
