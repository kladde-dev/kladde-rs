//! [`UnjournaledBackend`]: the simplest concrete backend -- a
//! [`Composed`]`(Storage, RelocatableHeap, id pool)` with no journal, applying
//! every operation immediately.
//!
//! It presents the `&self` write facade by wrapping the `Composed` core in a
//! `RefCell`; the `ReadBackend` methods take `&mut self` and reach the core via
//! `RefCell::get_mut`, so the real seekable cursor comes straight out. The
//! backend owns the id pool (over the pointer width `W`); the heap owns the
//! geometry and the `id -> address` table.

use std::cell::RefCell;
use std::io::{Read, Seek};

use crate::backend::{Backend, BackendError, ReadBackend, WriteBackend};
use crate::composed::Composed;
use crate::heap::{CompactionProgress, IncrementallyCompactableHeap, RelocatableHeap};
use crate::pointer::{Pointer, ResolvedPointer, UniquePointerFixedSize, UniquePointerResizable};
use crate::storage::Storage;
use crate::word::Word;

/// A non-journaled backend composing a `Storage`, a `RelocatableHeap`, and the
/// id pool (over pointer width `W`, defaulted to `u32`).
pub struct UnjournaledBackend<S, H: RelocatableHeap, W: Word = u32> {
    inner: RefCell<Composed<S, H, W>>,
}

impl<S: Storage, H: RelocatableHeap<Id = Pointer<W>>, W: Word> UnjournaledBackend<S, H, W> {
    pub fn new(storage: S, heap: H) -> Self {
        Self {
            inner: RefCell::new(Composed::new(storage, heap)),
        }
    }

    /// Number of live (not-yet-freed) allocations -- useful for leak checks.
    pub fn live_count(&self) -> usize {
        self.inner.borrow().live_count()
    }

    /// One past the highest live byte: what the store can be truncated to.
    pub fn len(&self) -> H::Address {
        self.inner.borrow().heap.len()
    }

    /// Whether nothing is allocated.
    pub fn is_empty(&self) -> bool {
        self.inner.borrow().heap.is_empty()
    }

    /// Consume the backend, returning the inner storage and heap.
    pub fn into_parts(self) -> (S, H) {
        let composed = self.inner.into_inner();
        (composed.storage, composed.heap)
    }
}

/// Compaction controls, present only when the heap actually compacts -- a
/// backend over a non-compacting heap simply has no such method.
impl<S: Storage, H: IncrementallyCompactableHeap<Id = Pointer<W>>, W: Word>
    UnjournaledBackend<S, H, W>
{
    /// Run compaction steps until the heap quiesces or `budget` bytes have been
    /// copied, then truncate the store. Named for the *incremental* algorithm;
    /// an optimized whole-heap compaction would be a separate entry point.
    pub fn compact_incrementally(&self, budget: H::Address) -> CompactionProgress {
        self.inner.borrow_mut().compact_incrementally(budget)
    }
}

impl<S: Storage, H: RelocatableHeap<Id = Pointer<W>>, W: Word> Backend
    for UnjournaledBackend<S, H, W>
{
    type Pointer = Pointer<W>;
    type Size = H::Size;

    fn size(&self, p: Self::Pointer) -> Result<Self::Size, BackendError> {
        self.inner.borrow().size(p)
    }
    fn resolve(&self, p: Self::Pointer) -> Result<ResolvedPointer<Self::Pointer>, BackendError> {
        self.inner.borrow().resolve(p)
    }
}

impl<S: Storage, H: RelocatableHeap<Id = Pointer<W>>, W: Word> WriteBackend
    for UnjournaledBackend<S, H, W>
{
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
    fn copy(
        &self,
        src: Self::Pointer,
        src_offset: Self::Size,
        len: Self::Size,
        dst: Self::Pointer,
        dst_offset: Self::Size,
    ) {
        self.inner
            .borrow_mut()
            .copy_between(src, src_offset, len, dst, dst_offset)
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

impl<S: Storage, H: RelocatableHeap<Id = Pointer<W>>, W: Word> ReadBackend
    for UnjournaledBackend<S, H, W>
{
    fn read_at(&mut self, anchor: Self::Pointer, offset: Self::Size) -> impl Read + Seek + '_ {
        self.inner.get_mut().read_at(anchor, offset)
    }
}

impl<S: Storage, H: IncrementallyCompactableHeap<Id = Pointer<W>>, W: Word> crate::CompactingBackend
    for UnjournaledBackend<S, H, W>
{
    fn compact_incrementally(&self, budget: usize) -> CompactionProgress {
        UnjournaledBackend::compact_incrementally(self, Word::from_usize(budget))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::InMemoryStorage;
    use crate::GainGreedyHeap;
    use std::io::Read;

    fn backend() -> UnjournaledBackend<InMemoryStorage, GainGreedyHeap<Pointer<u32>>> {
        UnjournaledBackend::new(InMemoryStorage::default(), GainGreedyHeap::new())
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
        cursor.seek(std::io::SeekFrom::Current(-1)).unwrap();
        cursor.read_exact(&mut one).unwrap();
        assert_eq!(one, [3]);
    }

    #[test]
    fn resize_relocation_moves_bytes_in_storage() {
        let mut b = backend();
        let p = b.alloc_resizable(4);
        b.write(p.raw(), 0, &[9, 8, 7, 6]);
        // grow past the following allocation -> relocates -> backend copies bytes
        let _blocker = b.alloc_fixed_size(4);
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
        assert!(matches!(b.size(bogus), Err(BackendError::DanglingPointer)));
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
        b.splice(&p, 1, 2, &[9, 9, 9]); // grows to 7
        assert_eq!(b.size(p.raw()).unwrap(), 7);
        let mut cursor = b.read_at(p.raw(), 0);
        let mut buf = [0u8; 7];
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [1, 9, 9, 9, 4, 5, 6]);
    }

    #[test]
    fn compaction_moves_the_bytes_and_shrinks_the_store() {
        let mut b = backend();
        let low = b.alloc_fixed_size(8);
        let high = b.alloc_fixed_size(8);
        b.write(low.raw(), 0, &[1; 8]);
        b.write(high.raw(), 0, &[2; 8]);
        b.free_fixed_size(low); // gap at 0..8
        assert_eq!(b.len(), 16, "the survivor still pins the end");

        let progress = b.compact_incrementally(1024);
        assert_eq!(progress.steps, 1);
        assert_eq!(progress.bytes_moved, 8);
        assert!(progress.quiesced);
        assert_eq!(b.len(), 8, "the survivor slid down into the gap");

        // The data must have moved with the entry, not just the bookkeeping.
        let mut buf = [0u8; 8];
        {
            let mut cursor = b.read_at(high.raw(), 0);
            cursor.read_exact(&mut buf).unwrap();
        }
        assert_eq!(buf, [2; 8]);

        let (storage, _) = b.into_parts();
        assert_eq!(storage.len().unwrap(), 8, "store truncated to the heap");
    }

    #[test]
    fn compaction_respects_its_budget_and_reports_not_having_quiesced() {
        let b = backend();
        let mut live = Vec::new();
        for _ in 0..6 {
            live.push(b.alloc_fixed_size(8));
        }
        // Free every other one, leaving three 8-byte gaps to close.
        for p in live
            .drain(..)
            .enumerate()
            .filter_map(|(i, p)| (i % 2 == 0).then_some(p))
        {
            b.free_fixed_size(p);
        }

        let progress = b.compact_incrementally(8);
        assert!(!progress.quiesced, "the budget, not the heap, stopped it");
        assert!(progress.bytes_moved <= 8);
        assert_eq!(progress.steps, 1);

        // Further rounds finish the job.
        while !b.compact_incrementally(8).quiesced {}
        assert_eq!(b.len(), 24, "three 8-byte survivors, gaplessly packed");
    }

    #[test]
    fn free_reduces_live_count() {
        let b = backend();
        assert_eq!(b.live_count(), 0);
        let p = b.alloc_fixed_size(4);
        assert_eq!(b.live_count(), 1);
        b.free_fixed_size(p);
        assert_eq!(b.live_count(), 0);
    }
}
