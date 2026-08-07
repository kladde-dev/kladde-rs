//! [`JournaledWriteBackend`] / [`JournaledReadBackend`]: an in-memory-journal
//! backend on the relocatable-heap model.
//!
//! It defers *everything* to flush: `alloc` mints an id immediately (so a `store`
//! can serialize it) but **records only a pending size** -- reserving the address
//! range is deferred to [`flush`](JournaledWriteBackend::flush), the `claim`.
//! Sizedness needs no pending state at all, since it rides on the id. Writes are
//! buffered. `size`/`resolve` answer from the pending map during the write phase.
//!
//! On flush, every still-live id is claimed, the buffered writes are replayed at
//! the now-known addresses, and then a bounded round of **incremental
//! compaction** runs -- the per-flush schedule of `incremental-compaction.md`
//! §5.1. That call is unconditional: a backend over a heap that does not compact
//! gets a no-op out of it, which is exactly why the compaction methods are
//! defaulted on `RelocatableHeap` rather than living on the marker subtrait.
//!
//! The read/write phase split is by ownership: `JournaledWriteBackend` implements
//! only `WriteBackend`; `flush(self)` consumes it and returns a
//! `JournaledReadBackend` that implements only `ReadBackend`.
//!
//! Still an in-memory-journal *mock*: the journal + id pool live in memory only,
//! so [`JournaledWriteBackend::open`] (recovery from `Storage`) is a `todo!()`.
//! `splice` is likewise `todo!()`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Seek};

use crate::backend::{Backend, BackendError, ReadBackend, WriteBackend};
use crate::composed::{resolved, Composed};
use crate::heap::{CompactionProgress, IncrementallyCompactableHeap, RelocatableHeap};
use crate::pointer::{
    Pointer, ResolvedPointer, Sizedness, UniquePointerFixedSize, UniquePointerResizable,
};
use crate::storage::Storage;
use crate::word::Word;

/// Bytes of compaction work attempted per flush when nothing else is configured.
pub const DEFAULT_COMPACTION_BUDGET: usize = 64 * 1024;

struct JournaledInner<S, H: RelocatableHeap, W: Word = u32> {
    composed: Composed<S, H, W>,
    /// Minted-but-not-yet-claimed allocations and their pending sizes. The
    /// deferred heap state, mutated by `alloc`/`resize`/`free`/`make_*`.
    pending: HashMap<Pointer<W>, H::Size>,
    /// Buffered writes `(id, offset, bytes)`, replayed after claim.
    journal: Vec<(Pointer<W>, H::Size, Vec<u8>)>,
}

/// The write half of a journaled transaction.
pub struct JournaledWriteBackend<S, H: RelocatableHeap, W: Word = u32> {
    inner: RefCell<JournaledInner<S, H, W>>,
    compaction_budget: usize,
}

impl<S: Storage, H: RelocatableHeap<Id = Pointer<W>>, W: Word> JournaledWriteBackend<S, H, W> {
    pub fn new(storage: S, heap: H) -> Self {
        Self {
            inner: RefCell::new(JournaledInner {
                composed: Composed::new(storage, heap),
                pending: HashMap::new(),
                journal: Vec::new(),
            }),
            compaction_budget: DEFAULT_COMPACTION_BUDGET,
        }
    }

    /// How many bytes of compaction work [`flush`](Self::flush) will attempt.
    /// Zero disables per-flush compaction.
    pub fn compaction_budget(&self) -> usize {
        self.compaction_budget
    }

    /// Set the per-flush compaction budget. A *policy* knob on the backend, not
    /// a parameter of the heap trait, which models only the capability.
    pub fn set_compaction_budget(&mut self, budget: usize) {
        self.compaction_budget = budget;
    }

    /// Reopen a journaled file, reconstructing state persisted in `storage`.
    /// Blocked on the self-hosting bootstrap (deferred): the journal + id pool
    /// live in memory only, so there is nothing in `storage` to recover yet.
    pub fn open(_storage: S) -> Self {
        todo!("self-hosting bootstrap: recover the id pool + journal from Storage (deferred)")
    }

    /// End the write transaction: claim every still-live id (reserving address
    /// ranges), replay the buffered writes, compact within the budget, and hand
    /// back a read-only view.
    pub fn flush(self) -> JournaledReadBackend<S, H, W> {
        let budget = self.compaction_budget;
        let mut inner = self.inner.into_inner();
        let mut pending: Vec<(Pointer<W>, H::Size)> = inner.pending.drain().collect();
        // First-fit-decreasing: serve the large allocations while the large gaps
        // are still intact, so the placement each one gets is the best available
        // rather than whatever a smaller one left behind. Sorting also makes the
        // resulting layout independent of hash iteration order, which would
        // otherwise leave it -- and how much work compaction then has to do --
        // unreproducible from one run to the next.
        pending.sort_unstable_by(|(id_a, size_a), (id_b, size_b)| {
            size_b.cmp(size_a).then_with(|| id_a.raw().cmp(&id_b.raw()))
        });
        for (id, size) in pending {
            inner.composed.claim(id, size);
        }
        for (id, offset, bytes) in std::mem::take(&mut inner.journal) {
            inner.composed.write(id, offset, &bytes);
        }
        // Unconditional: a non-compacting heap proposes nothing and this is free.
        let progress = inner
            .composed
            .compact_incrementally(Word::from_usize(budget));
        JournaledReadBackend { inner, progress }
    }
}

impl<S: Storage, H: RelocatableHeap<Id = Pointer<W>>, W: Word> Backend
    for JournaledWriteBackend<S, H, W>
{
    type Pointer = Pointer<W>;
    type Size = H::Size;

    fn size(&self, p: Self::Pointer) -> Result<Self::Size, BackendError> {
        self.inner
            .borrow()
            .pending
            .get(&p)
            .copied()
            .ok_or(BackendError::DanglingPointer)
    }
    fn resolve(&self, p: Self::Pointer) -> Result<ResolvedPointer<Self::Pointer>, BackendError> {
        if !self.inner.borrow().pending.contains_key(&p) {
            return Err(BackendError::DanglingPointer);
        }
        Ok(resolved(p))
    }
}

impl<S: Storage, H: RelocatableHeap<Id = Pointer<W>>, W: Word> JournaledWriteBackend<S, H, W> {
    /// Mint an id of `sizedness` and record its pending size.
    fn mint_pending(&self, size: H::Size, sizedness: Sizedness) -> Pointer<W> {
        let mut inner = self.inner.borrow_mut();
        let id = inner.composed.mint(sizedness);
        inner.pending.insert(id, size);
        id
    }

    /// Re-mint `old` with the other sizedness (sizedness lives in the id, so a
    /// conversion cannot re-tag in place), carrying the pending entry over and
    /// re-anchoring any writes already buffered against the old id.
    fn remint(&self, old: Pointer<W>, new_size: H::Size, sizedness: Sizedness) -> Pointer<W> {
        let mut inner = self.inner.borrow_mut();
        inner.pending.remove(&old);
        let new = inner.composed.mint(sizedness);
        inner.pending.insert(new, new_size);
        for (anchor, _, _) in &mut inner.journal {
            if *anchor == old {
                *anchor = new;
            }
        }
        new
    }
}

impl<S: Storage, H: RelocatableHeap<Id = Pointer<W>>, W: Word> WriteBackend
    for JournaledWriteBackend<S, H, W>
{
    fn alloc_resizable(&self, size: Self::Size) -> UniquePointerResizable<Self::Pointer> {
        UniquePointerResizable::from_pointer(self.mint_pending(size, Sizedness::Resizable))
    }
    fn alloc_fixed_size(&self, size: Self::Size) -> UniquePointerFixedSize<Self::Pointer> {
        UniquePointerFixedSize::from_pointer(self.mint_pending(size, Sizedness::Fixed))
    }
    fn free_resizable(&self, p: UniquePointerResizable<Self::Pointer>) {
        self.inner.borrow_mut().pending.remove(&p.raw());
    }
    fn free_fixed_size(&self, p: UniquePointerFixedSize<Self::Pointer>) {
        self.inner.borrow_mut().pending.remove(&p.raw());
    }
    fn resize(
        &self,
        p: &UniquePointerResizable<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<(), BackendError> {
        match self.inner.borrow_mut().pending.get_mut(&p.raw()) {
            Some(size) => {
                *size = new_size;
                Ok(())
            }
            None => Err(BackendError::DanglingPointer),
        }
    }
    fn make_resizable(
        &self,
        p: UniquePointerFixedSize<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<UniquePointerResizable<Self::Pointer>, BackendError> {
        let id = self.remint(p.raw(), new_size, Sizedness::Resizable);
        Ok(UniquePointerResizable::from_pointer(id))
    }
    fn make_fixed_size(
        &self,
        p: UniquePointerResizable<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<UniquePointerFixedSize<Self::Pointer>, BackendError> {
        let id = self.remint(p.raw(), new_size, Sizedness::Fixed);
        Ok(UniquePointerFixedSize::from_pointer(id))
    }
    fn write(&self, anchor: Self::Pointer, offset: Self::Size, bytes: &[u8]) {
        self.inner
            .borrow_mut()
            .journal
            .push((anchor, offset, bytes.to_vec()));
    }
    fn splice(
        &self,
        _p: &UniquePointerResizable<Self::Pointer>,
        _offset: Self::Size,
        _old_len: Self::Size,
        _new: &[u8],
    ) {
        todo!(
            "journaled splice: deferring a splice interacts with the pending map + buffered writes"
        )
    }
}

/// The read-only view produced by [`JournaledWriteBackend::flush`].
pub struct JournaledReadBackend<S, H: RelocatableHeap, W: Word = u32> {
    inner: JournaledInner<S, H, W>,
    progress: CompactionProgress,
}

impl<S: Storage, H: RelocatableHeap<Id = Pointer<W>>, W: Word> JournaledReadBackend<S, H, W> {
    /// What the compaction round at the end of the flush achieved.
    pub fn compaction_progress(&self) -> CompactionProgress {
        self.progress
    }

    /// Number of live allocations.
    pub fn live_count(&self) -> usize {
        self.inner.composed.live_count()
    }

    /// One past the highest live byte.
    pub fn len(&self) -> H::Address {
        self.inner.composed.heap.len()
    }

    /// Whether nothing is allocated.
    pub fn is_empty(&self) -> bool {
        self.inner.composed.heap.is_empty()
    }

    /// Consume the view, returning the raw storage and heap.
    pub fn into_parts(self) -> (S, H) {
        (self.inner.composed.storage, self.inner.composed.heap)
    }
}

/// Compaction controls, present only when the heap actually compacts.
impl<S: Storage, H: IncrementallyCompactableHeap<Id = Pointer<W>>, W: Word>
    JournaledReadBackend<S, H, W>
{
    /// Run another bounded round of compaction outside the flush schedule.
    pub fn compact_incrementally(&mut self, budget: H::Address) -> CompactionProgress {
        self.inner.composed.compact_incrementally(budget)
    }
}

impl<S: Storage, H: RelocatableHeap<Id = Pointer<W>>, W: Word> Backend
    for JournaledReadBackend<S, H, W>
{
    type Pointer = Pointer<W>;
    type Size = H::Size;

    fn size(&self, p: Self::Pointer) -> Result<Self::Size, BackendError> {
        self.inner.composed.size(p)
    }
    fn resolve(&self, p: Self::Pointer) -> Result<ResolvedPointer<Self::Pointer>, BackendError> {
        self.inner.composed.resolve(p)
    }
}

impl<S: Storage, H: RelocatableHeap<Id = Pointer<W>>, W: Word> ReadBackend
    for JournaledReadBackend<S, H, W>
{
    fn read_at(&mut self, anchor: Self::Pointer, offset: Self::Size) -> impl Read + Seek + '_ {
        self.inner.composed.read_at(anchor, offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::InMemoryStorage;
    use crate::GainGreedyHeap;
    use std::io::Read;

    type Wb = JournaledWriteBackend<InMemoryStorage, GainGreedyHeap<Pointer<u32>>>;

    fn write_backend() -> Wb {
        JournaledWriteBackend::new(InMemoryStorage::default(), GainGreedyHeap::new())
    }

    #[test]
    fn writes_are_buffered_until_flush_then_visible() {
        let wb = write_backend();
        let p = wb.alloc_fixed_size(4);
        wb.write(p.raw(), 0, &[1, 2, 3, 4]);
        let id = p.raw();

        let mut rb = wb.flush();
        let mut cursor = rb.read_at(id, 0);
        let mut buf = [0u8; 4];
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);
    }

    #[test]
    fn resize_is_deferred_and_size_reflects_the_pending_state() {
        let wb = write_backend();
        let p = wb.alloc_resizable(4);
        assert_eq!(wb.size(p.raw()).unwrap(), 4);
        wb.resize(&p, 8).unwrap();
        assert_eq!(wb.size(p.raw()).unwrap(), 8); // pending reflects the new size

        let id = p.raw();
        let rb = wb.flush();
        assert_eq!(rb.size(id).unwrap(), 8); // claimed at the final size
    }

    #[test]
    fn resize_then_write_into_the_grown_region_replays_in_order() {
        let wb = write_backend();
        let p = wb.alloc_resizable(4);
        wb.write(p.raw(), 0, &[1, 2, 3, 4]);
        wb.resize(&p, 8).unwrap();
        wb.write(p.raw(), 4, &[5, 6, 7, 8]);
        let id = p.raw();

        let mut rb = wb.flush();
        let mut cursor = rb.read_at(id, 0);
        let mut buf = [0u8; 8];
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn free_drops_the_pending_allocation() {
        let wb = write_backend();
        let p = wb.alloc_resizable(4);
        wb.resize(&p, 8).unwrap();
        assert_eq!(wb.size(p.raw()).unwrap(), 8);
        let id = p.raw();
        wb.free_resizable(p);
        assert!(matches!(wb.size(id), Err(BackendError::DanglingPointer)));
    }

    #[test]
    fn a_sizedness_conversion_re_anchors_writes_buffered_against_the_old_id() {
        let wb = write_backend();
        let p = wb.alloc_fixed_size(4);
        wb.write(p.raw(), 0, &[1, 2, 3, 4]); // buffered against the *old* id
        let old = p.raw();

        let q = wb.make_resizable(p, 4).unwrap();
        assert_ne!(
            q.raw(),
            old,
            "sizedness lives in the id, so it must re-mint"
        );
        assert_eq!(q.raw().sizedness(), Sizedness::Resizable);
        assert!(matches!(wb.size(old), Err(BackendError::DanglingPointer)));

        // The buffered write must follow the id, or flush would replay it
        // against an id that no longer exists.
        let id = q.raw();
        let mut rb = wb.flush();
        let mut cursor = rb.read_at(id, 0);
        let mut buf = [0u8; 4];
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);
    }

    #[test]
    fn flush_claims_the_largest_pending_allocations_first() {
        let wb = write_backend();
        // Minted small-first, so hash order alone would not produce this layout.
        let small = wb.alloc_fixed_size(10);
        let large = wb.alloc_fixed_size(100);
        let medium = wb.alloc_fixed_size(50);
        let (s, l, m) = (small.raw(), large.raw(), medium.raw());

        let rb = wb.flush();
        let addresses = |id| rb.inner.composed.heap.lookup(id).unwrap().0;
        assert_eq!(addresses(l), 0, "the largest is served first");
        assert_eq!(addresses(m), 100);
        assert_eq!(addresses(s), 150);
        assert_eq!(rb.len(), 160, "and the result is gapless");
    }

    #[test]
    fn flush_order_does_not_depend_on_hash_iteration_order() {
        // Same multiset of sizes, minted in two different orders: the claimed
        // layout must come out identical.
        let layout_of = |sizes: &[u32]| {
            let wb = write_backend();
            let ids: Vec<_> = sizes
                .iter()
                .map(|&s| wb.alloc_fixed_size(s).raw())
                .collect();
            let rb = wb.flush();
            let mut spans: Vec<(u64, u32)> = ids
                .iter()
                .map(|&id| rb.inner.composed.heap.lookup(id).unwrap())
                .collect();
            spans.sort_unstable();
            spans
        };
        assert_eq!(
            layout_of(&[7, 64, 7, 200, 31]),
            layout_of(&[200, 31, 7, 7, 64]),
        );
    }

    #[test]
    fn flush_compacts_within_its_budget_and_truncates_the_store() {
        let wb = write_backend();
        // Three allocations, the middle one freed before flush -- so the claim
        // order leaves no gap at all and there is nothing to compact.
        let a = wb.alloc_fixed_size(64);
        let b = wb.alloc_fixed_size(64);
        let c = wb.alloc_fixed_size(64);
        wb.write(a.raw(), 0, &[1; 64]);
        wb.write(c.raw(), 0, &[3; 64]);
        wb.free_fixed_size(b);

        let rb = wb.flush();
        assert!(rb.compaction_progress().quiesced);
        assert_eq!(rb.len(), 128, "two 64-byte allocations, gaplessly claimed");
        let (storage, _) = rb.into_parts();
        assert_eq!(storage.len().unwrap(), 128, "store truncated to the heap");
    }

    #[test]
    fn a_gap_opened_after_a_flush_is_closed_by_the_next_one() {
        let wb = write_backend();
        let a = wb.alloc_fixed_size(64);
        let b = wb.alloc_fixed_size(64);
        wb.write(b.raw(), 0, &[7; 64]);
        let (a_id, b_id) = (a.raw(), b.raw());
        let rb = wb.flush();
        assert_eq!(rb.len(), 128);

        // Reopen a write phase over the same heap, drop the low allocation, and
        // flush again: the survivor should slide down and the file halve.
        let (storage, heap) = rb.into_parts();
        let wb: Wb = JournaledWriteBackend::new(storage, heap);
        wb.inner.borrow_mut().composed.free(a_id);

        let mut rb = wb.flush();
        assert_eq!(rb.len(), 64, "the survivor slid down into the freed range");
        let mut cursor = rb.read_at(b_id, 0);
        let mut buf = [0u8; 64];
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(
            buf, [7; 64],
            "compaction moved the bytes, not just the entry"
        );
    }
}
