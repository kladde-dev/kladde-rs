//! [`JournaledWriteBackend`] / [`JournaledReadBackend`]: a backend that records
//! every mutation in an ordered log and applies the log at flush.
//!
//! See `journal-semantics.md`. The model in one line: **the log is the sole
//! authority**, and the [`Deltas`] map is a derived cache that answers
//! `size`/`resolve` during the write phase without consulting it.
//!
//! `alloc` mints an id immediately (so a `store` can serialize it) but reserves no
//! address range; sizedness needs no pending state at all, since it rides on the
//! id. Everything else is appended to the log. On flush the log is applied in
//! order and a bounded round of **incremental compaction** runs -- the per-flush
//! schedule of `incremental-compaction.md` §5.1. That call is unconditional: a
//! backend over a heap that does not compact gets a no-op out of it, which is
//! exactly why the compaction methods are defaulted on `RelocatableHeap` rather
//! than living on the marker subtrait.
//!
//! The read/write phase split is by ownership: `JournaledWriteBackend` implements
//! only `WriteBackend`; `flush(self)` consumes it and returns a
//! `JournaledReadBackend` that implements only `ReadBackend`.
//!
//! Still an in-memory-journal *mock*: the log lives in memory only, so
//! [`JournaledWriteBackend::open`] (recovery from `Storage`) is a `todo!()`.
//! Making the log durable is §9 of the design note and is not built here.

use std::cell::RefCell;
use std::io::{Read, Seek};

use crate::backend::{Backend, BackendError, ReadBackend, WriteBackend};
use crate::composed::{resolved, Composed};
use crate::heap::{CompactionProgress, IncrementallyCompactableHeap, RelocatableHeap};
use crate::fold::{fold, literal, Action, Source};
use crate::journal::{Deltas, Geometry, Log, Op};
use crate::pointer::{
    Pointer, ResolvedPointer, Sizedness, UniquePointerFixedSize, UniquePointerResizable,
};
use crate::storage::Storage;
use crate::word::Word;

/// Bytes of compaction work attempted per flush when nothing else is configured.
pub const DEFAULT_COMPACTION_BUDGET: usize = 64 * 1024;

/// How many bytes one gathered destination write may assemble before it is split.
///
/// Design note §4.1.1: a destination run can name arbitrarily many scattered
/// sources, and assembling it means staging them somewhere. Capping keeps that
/// staging bounded; splitting a run only costs an extra write, so this is a pure
/// throughput knob.
const GATHER_CAP: usize = 8 * 1024;

pub(crate) struct JournaledInner<S, H: RelocatableHeap, W: Word = u32> {
    pub(crate) composed: Composed<S, H, W>,
    /// The authoritative record of this transaction.
    pub(crate) log: Log<W, H::Size>,
    /// Derived from `log`; discarded at checkpoint. Exists so `size`/`resolve` are
    /// O(1) instead of a fold per query.
    pub(crate) deltas: Deltas<W, H::Size>,
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
                log: Log::default(),
                deltas: Deltas::default(),
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

    /// Bytes of payload currently buffered. The quantity an auto-checkpoint would
    /// watch (`journal-semantics.md` §9.1); nothing triggers on it yet.
    pub fn buffered_bytes(&self) -> usize {
        self.inner.borrow().log.payload_len()
    }

    /// Reopen a journaled file, reconstructing state persisted in `storage`.
    /// Blocked on the self-hosting bootstrap (deferred): the log + id pool live
    /// in memory only, so there is nothing in `storage` to recover yet.
    pub fn open(_storage: S) -> Self {
        todo!("self-hosting bootstrap: recover the id pool + log from Storage (deferred)")
    }

    /// End the write transaction: apply the log, compact within the budget, and
    /// hand back a read-only view.
    pub fn flush(self) -> JournaledReadBackend<S, H, W> {
        self.finish(apply_folded)
    }

    /// [`flush`](Self::flush) via the naive in-order replayer instead of the fold.
    ///
    /// Kept as the reference implementation §8 calls for: it performs one
    /// `Composed` call per logged op, with no folding and no reordering, so it is
    /// the simplest thing that can be correct. The differential oracle runs both.
    #[cfg(test)]
    pub(crate) fn flush_naively(self) -> JournaledReadBackend<S, H, W> {
        self.finish(apply)
    }

    fn finish(
        self,
        how: impl FnOnce(&mut JournaledInner<S, H, W>),
    ) -> JournaledReadBackend<S, H, W> {
        let budget = self.compaction_budget;
        let mut inner = self.inner.into_inner();
        how(&mut inner);
        // Unconditional: a non-compacting heap proposes nothing and this is free.
        let progress = inner
            .composed
            .compact_incrementally(Word::from_usize(budget));
        JournaledReadBackend { inner, progress }
    }

    /// The current size of `id`, from the deltas if this transaction has an
    /// opinion and from the heap otherwise.
    fn current_size(&self, id: Pointer<W>) -> Result<H::Size, BackendError> {
        let inner = self.inner.borrow();
        match inner.deltas.geometry(id) {
            Geometry::Known(s) => Ok(s),
            Geometry::Dead => Err(BackendError::DanglingPointer),
            Geometry::AskHeap => inner.composed.size(id),
        }
    }

    fn mint(&self, size: H::Size, sizedness: Sizedness) -> Pointer<W> {
        let mut inner = self.inner.borrow_mut();
        let id = inner.composed.mint(sizedness);
        inner.log.push(Op::Alloc { id, size });
        inner.deltas.allocated(id, size);
        id
    }

    /// Log a `Resize` and fold it into the deltas. Separated out because both
    /// `resize` and the two halves of a sizedness conversion need it.
    fn log_resize(&self, id: Pointer<W>, size: H::Size) -> Result<(), BackendError> {
        let mut inner = self.inner.borrow_mut();
        let live = inner.composed.heap.lookup(id).is_some();
        inner
            .deltas
            .resized(id, size, live)
            .map_err(|()| BackendError::DanglingPointer)?;
        inner.log.push(Op::Resize { id, size });
        Ok(())
    }

    /// Log a `ChangeSizedness` and fold it into the deltas.
    fn log_conversion(&self, old: Pointer<W>, new: Pointer<W>, size: H::Size) {
        let mut inner = self.inner.borrow_mut();
        let live = inner.composed.heap.lookup(old).is_some();
        inner.deltas.sizedness_changed(old, new, size, live);
        inner.log.push(Op::ChangeSizedness { old, new });
    }
}

/// Apply the log in order, then clear it.
///
/// This is the **naive in-order replayer**: one `Composed` call per op, no
/// folding, no reordering. It is deliberately the simplest thing that can be
/// correct, because `journal-semantics.md` §8 keeps it as the reference
/// implementation the optimized path is differentially tested against.
#[cfg(test)]
pub(crate) fn apply<S: Storage, H: RelocatableHeap<Id = Pointer<W>>, W: Word>(
    inner: &mut JournaledInner<S, H, W>,
) {
    // Moved out so the payload arena can be read while `composed` is borrowed
    // mutably; the log is cleared by this operation either way.
    let log = std::mem::take(&mut inner.log);
    for op in log.ops() {
        match *op {
            Op::Alloc { id, size } => inner.composed.claim(id, size),
            Op::Free { id } => inner.composed.free(id),
            Op::Resize { id, size } => inner
                .composed
                .resize_by_id(id, size)
                .expect("replayed resize of a live id"),
            Op::ChangeSizedness { old, new } => {
                // Size-preserving by construction: `make_*` logs the size change
                // as its own `Resize`, before or after this op as the sizedness
                // requires (§4.3).
                let (_, size) = inner
                    .composed
                    .heap
                    .lookup(old)
                    .expect("replayed conversion of a live id");
                inner
                    .composed
                    .convert_to(old, new, size)
                    .expect("replayed conversion of a live id");
            }
            Op::Write {
                id,
                offset,
                payload,
            } => inner.composed.write(id, offset, log.payload(payload)),
            Op::Splice {
                id,
                offset,
                old_len,
                payload,
            } => inner
                .composed
                .splice_by_id(id, offset, old_len, log.payload(payload)),
            Op::Copy {
                src,
                src_offset,
                len,
                dst,
                dst_offset,
            } => inner
                .composed
                .copy_between(src, src_offset, len, dst, dst_offset),
        }
    }
    inner.deltas.clear();
}

/// Apply the log through the fold (design note §4, §5 and §6).
///
/// Phase order is `hoist -> releases -> relabels -> claims -> reshapes -> writes`.
///
/// Releases run **first**, which is what gives the placement pass the freed space
/// to work with. That is only sound because hoisting has already resolved every
/// read of an allocation this flush disturbs; without it a released allocation's
/// bytes could be handed to a claim before a piece that still names them was
/// read, and releases would have to run last instead.
pub(crate) fn apply_folded<S: Storage, H: RelocatableHeap<Id = Pointer<W>>, W: Word>(
    inner: &mut JournaledInner<S, H, W>,
) {
    let mut log = std::mem::take(&mut inner.log);
    let mut folded = {
        let heap = &inner.composed.heap;
        fold(&log, |id| heap.lookup(id).map(|(_, size)| size))
    };
    let survivors = folded.survivors();

    // The content-blind fast path (§7): with no cross-id read there is nothing to
    // hoist and nothing to order, so the overwhelming majority of transactions
    // skip this entirely. Only `Copy` can set the flag.
    if folded.has_cross_id_reads(&survivors) {
        for (id, start, len, src, src_off) in folded.hoistable(&survivors) {
            let bytes = inner.composed.read_bytes(src, src_off, len);
            let span = log.intern(&bytes);
            folded.resolve(id, start, len, span.start);
        }
    }

    debug_assert!(
        inner.deltas.agrees_with(|id| folded.geometry_of(id)),
        "fold agreement: the geometry derived from the log must equal what the \
         write phase recorded incrementally",
    );

    for id in folded.releases() {
        inner.composed.release(id);
    }

    for action in folded.before_writes() {
        match action {
            Action::Claim(id, size) => inner.composed.claim(id, size),
            Action::Reshape(id, size) => inner
                .composed
                .resize_by_id(id, size)
                .expect("reshape of a live id"),
            // No bytes move: the allocation keeps its address and its size, and
            // only its key changes. The pending size, if any, is a separate
            // `Resize` that folded into the same entry.
            Action::Relabel { from, to, size } => {
                inner.composed.relabel(from, to).expect("relabel of a live id");
                inner
                    .composed
                    .resize_by_id(to, size)
                    .expect("reshape of a live id");
            }
        }
    }

    for (id, size) in survivors {
        for (start, pieces) in folded.runs(id, size.to_usize()) {
            emit(inner, &log, id, start, &pieces);
        }
    }

    inner.deltas.clear();
}

/// Assemble one destination run and put it down with as few writes as the cap
/// allows.
fn emit<S: Storage, H: RelocatableHeap<Id = Pointer<W>>, W: Word>(
    inner: &mut JournaledInner<S, H, W>,
    log: &Log<W, H::Size>,
    id: Pointer<W>,
    start: usize,
    pieces: &[(usize, Source<W>)],
) {
    // A run that reads *its own* allocation is a memmove (splice shifts a tail
    // that way). Splitting it would let the first write clobber bytes a later
    // piece still has to read, so such a run is staged whole -- bounded by the
    // allocation's own size either way.
    let self_referential = pieces
        .iter()
        .any(|(_, src)| matches!(src, Source::Storage(other, _) if *other == id));
    let cap = if self_referential { usize::MAX } else { GATHER_CAP };

    let mut buf: Vec<u8> = Vec::new();
    let mut offset = start;
    let put = |inner: &mut JournaledInner<S, H, W>, buf: &mut Vec<u8>, offset: &mut usize| {
        if !buf.is_empty() {
            inner.composed.write(id, Word::from_usize(*offset), buf);
            *offset += buf.len();
            buf.clear();
        }
    };

    for &(len, src) in pieces {
        let mut done = 0;
        while done < len {
            let take = (len - done).min(cap - buf.len());
            match src {
                Source::Literal(pos) => buf.extend_from_slice(literal(log, pos + done, take)),
                Source::Storage(other, off) => {
                    let bytes = inner.composed.read_bytes(other, off + done, take);
                    buf.extend_from_slice(&bytes);
                }
                Source::Undefined => unreachable!("undefined segments are not emitted"),
            }
            done += take;
            if buf.len() >= cap {
                put(inner, &mut buf, &mut offset);
            }
        }
    }
    put(inner, &mut buf, &mut offset);
}

impl<S: Storage, H: RelocatableHeap<Id = Pointer<W>>, W: Word> Backend
    for JournaledWriteBackend<S, H, W>
{
    type Pointer = Pointer<W>;
    type Size = H::Size;

    fn size(&self, p: Self::Pointer) -> Result<Self::Size, BackendError> {
        self.current_size(p)
    }

    fn resolve(&self, p: Self::Pointer) -> Result<ResolvedPointer<Self::Pointer>, BackendError> {
        let inner = self.inner.borrow();
        match inner.deltas.geometry(p) {
            Geometry::Known(_) => Ok(resolved(p)),
            Geometry::Dead => Err(BackendError::DanglingPointer),
            Geometry::AskHeap => inner.composed.resolve(p),
        }
    }
}

impl<S: Storage, H: RelocatableHeap<Id = Pointer<W>>, W: Word> WriteBackend
    for JournaledWriteBackend<S, H, W>
{
    fn alloc_resizable(&self, size: Self::Size) -> UniquePointerResizable<Self::Pointer> {
        UniquePointerResizable::from_pointer(self.mint(size, Sizedness::Resizable))
    }
    fn alloc_fixed_size(&self, size: Self::Size) -> UniquePointerFixedSize<Self::Pointer> {
        UniquePointerFixedSize::from_pointer(self.mint(size, Sizedness::Fixed))
    }
    fn free_resizable(&self, p: UniquePointerResizable<Self::Pointer>) {
        let mut inner = self.inner.borrow_mut();
        inner.log.push(Op::Free { id: p.raw() });
        inner.deltas.freed(p.raw());
    }
    fn free_fixed_size(&self, p: UniquePointerFixedSize<Self::Pointer>) {
        let mut inner = self.inner.borrow_mut();
        inner.log.push(Op::Free { id: p.raw() });
        inner.deltas.freed(p.raw());
    }

    fn resize(
        &self,
        p: &UniquePointerResizable<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<(), BackendError> {
        self.log_resize(p.raw(), new_size)
    }

    /// `ChangeSizedness` first, then `Resize`: the allocation is fixed-size on
    /// entry, and a `Resize` is only defined on a resizable one (§4.3).
    fn make_resizable(
        &self,
        p: UniquePointerFixedSize<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<UniquePointerResizable<Self::Pointer>, BackendError> {
        let old = p.raw();
        let size = self.current_size(old)?;
        let new = self.inner.borrow_mut().composed.mint(Sizedness::Resizable);
        self.log_conversion(old, new, size);
        self.log_resize(new, new_size)?;
        Ok(UniquePointerResizable::from_pointer(new))
    }

    /// `Resize` first, then `ChangeSizedness`: the mirror of
    /// [`make_resizable`](Self::make_resizable), for the same reason.
    fn make_fixed_size(
        &self,
        p: UniquePointerResizable<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<UniquePointerFixedSize<Self::Pointer>, BackendError> {
        let old = p.raw();
        self.log_resize(old, new_size)?;
        let new = self.inner.borrow_mut().composed.mint(Sizedness::Fixed);
        self.log_conversion(old, new, new_size);
        Ok(UniquePointerFixedSize::from_pointer(new))
    }

    fn write(&self, anchor: Self::Pointer, offset: Self::Size, bytes: &[u8]) {
        let mut inner = self.inner.borrow_mut();
        let payload = inner.log.intern(bytes);
        inner.log.push(Op::Write {
            id: anchor,
            offset,
            payload,
        });
    }

    fn copy(
        &self,
        src: Self::Pointer,
        src_offset: Self::Size,
        len: Self::Size,
        dst: Self::Pointer,
        dst_offset: Self::Size,
    ) {
        self.inner.borrow_mut().log.push(Op::Copy {
            src,
            src_offset,
            len,
            dst,
            dst_offset,
        });
    }

    fn splice(
        &self,
        p: &UniquePointerResizable<Self::Pointer>,
        offset: Self::Size,
        old_len: Self::Size,
        new: &[u8],
    ) {
        let id = p.raw();
        let size = self.current_size(id).expect("splice of a live id").to_usize();
        let tail_start = offset.to_usize() + old_len.to_usize();
        assert!(tail_start <= size, "splice range out of bounds");
        let new_size: H::Size =
            Word::from_usize(offset.to_usize() + new.len() + (size - tail_start));

        let mut inner = self.inner.borrow_mut();
        let live = inner.composed.heap.lookup(id).is_some();
        inner
            .deltas
            .resized(id, new_size, live)
            .expect("splice of a live id");
        let payload = inner.log.intern(new);
        inner.log.push(Op::Splice {
            id,
            offset,
            old_len,
            payload,
        });
    }
}

/// The read-only view produced by [`JournaledWriteBackend::flush`].
pub struct JournaledReadBackend<S, H: RelocatableHeap, W: Word = u32> {
    pub(crate) inner: JournaledInner<S, H, W>,
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

    /// Begin a new write transaction over the same storage, heap **and id pool**.
    ///
    /// Preferable to `into_parts` + `new` for a second transaction, which would
    /// reset the id pool and hand out counters that are still live.
    pub fn reopen(self) -> JournaledWriteBackend<S, H, W> {
        JournaledWriteBackend {
            inner: RefCell::new(self.inner),
            compaction_budget: DEFAULT_COMPACTION_BUDGET,
        }
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
    use std::cell::Cell;
    use std::io::Read;
    use std::rc::Rc;

    type Wb = JournaledWriteBackend<InMemoryStorage, GainGreedyHeap<Pointer<u32>>>;

    /// Counts writes reaching the store, so the fold's claims about *how much*
    /// I/O it removes can be asserted rather than asserted-in-prose.
    struct Counting {
        inner: InMemoryStorage,
        writes: Rc<Cell<usize>>,
        bytes: Rc<Cell<usize>>,
    }

    impl std::io::Read for Counting {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.inner.read(buf)
        }
    }
    impl std::io::Write for Counting {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.writes.set(self.writes.get() + 1);
            self.bytes.set(self.bytes.get() + buf.len());
            self.inner.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }
    impl std::io::Seek for Counting {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }
    impl crate::storage::Storage for Counting {
        fn resize(&mut self, new_len: u64) -> std::io::Result<()> {
            self.inner.resize(new_len)
        }
        fn len(&self) -> std::io::Result<u64> {
            self.inner.len()
        }
    }

    type CountingWb = JournaledWriteBackend<Counting, GainGreedyHeap<Pointer<u32>>>;

    /// A backend whose store counts writes, plus the two counters.
    fn counting() -> (CountingWb, Rc<Cell<usize>>, Rc<Cell<usize>>) {
        let writes = Rc::new(Cell::new(0));
        let bytes = Rc::new(Cell::new(0));
        let storage = Counting {
            inner: InMemoryStorage::default(),
            writes: Rc::clone(&writes),
            bytes: Rc::clone(&bytes),
        };
        (
            JournaledWriteBackend::new(storage, GainGreedyHeap::new()),
            writes,
            bytes,
        )
    }

    fn write_backend() -> Wb {
        JournaledWriteBackend::new(InMemoryStorage::default(), GainGreedyHeap::new())
    }

    fn read_back<St: Storage, const N: usize>(
        rb: &mut JournaledReadBackend<St, GainGreedyHeap<Pointer<u32>>>,
        id: Pointer<u32>,
    ) -> [u8; N] {
        let mut buf = [0u8; N];
        rb.read_at(id, 0).read_exact(&mut buf).unwrap();
        buf
    }

    #[test]
    fn writes_are_buffered_until_flush_then_visible() {
        let wb = write_backend();
        let p = wb.alloc_fixed_size(4);
        wb.write(p.raw(), 0, &[1, 2, 3, 4]);
        let id = p.raw();

        let mut rb = wb.flush();
        assert_eq!(read_back::<_, 4>(&mut rb, id), [1, 2, 3, 4]);
    }

    #[test]
    fn resize_is_deferred_and_size_reflects_the_pending_state() {
        let wb = write_backend();
        let p = wb.alloc_resizable(4);
        assert_eq!(wb.size(p.raw()).unwrap(), 4);
        wb.resize(&p, 8).unwrap();
        assert_eq!(wb.size(p.raw()).unwrap(), 8);

        let id = p.raw();
        let rb = wb.flush();
        assert_eq!(rb.size(id).unwrap(), 8);
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
        assert_eq!(read_back::<_, 8>(&mut rb, id), [1, 2, 3, 4, 5, 6, 7, 8]);
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
        wb.write(p.raw(), 0, &[1, 2, 3, 4]);
        let old = p.raw();

        let q = wb.make_resizable(p, 4).unwrap();
        assert_ne!(q.raw(), old, "sizedness lives in the id, so it must re-mint");
        assert_eq!(q.raw().sizedness(), Sizedness::Resizable);
        assert!(matches!(wb.size(old), Err(BackendError::DanglingPointer)));

        let id = q.raw();
        let mut rb = wb.flush();
        assert_eq!(read_back::<_, 4>(&mut rb, id), [1, 2, 3, 4]);
    }

    // ---- the three bugs from `journal-semantics.md` §1 ----

    #[test]
    fn alloc_write_free_in_one_transaction_is_not_a_panic() {
        // §1 bug 1. The predecessor annihilated the alloc and the free inside its
        // `pending` map, so the id was never claimed, while the buffered write
        // survived in a second structure and replayed against a dangling pointer.
        let wb = write_backend();
        let p = wb.alloc_resizable(4);
        wb.write(p.raw(), 0, &[1, 2, 3, 4]);
        wb.free_resizable(p);

        let rb = wb.flush();
        assert_eq!(rb.live_count(), 0);
        assert!(rb.is_empty());
    }

    #[test]
    fn a_write_made_before_a_shrink_cannot_clobber_a_neighbour() {
        // §1 bug 2. The predecessor deferred the write past the shrink and past
        // B's claim, so it replayed at an offset that by then lay outside A --
        // landing inside B, which read back as `[0xAA; 8]` instead of its own
        // content.
        //
        // Note what is *not* asserted: that B reads zeros. B's bytes are
        // uninitialized until written, and §2.1 explicitly permits them to be
        // whatever happened to be at that address before -- including A's
        // pre-shrink write. The contract is that B's *own* content survives.
        let wb = write_backend();
        let a = wb.alloc_resizable(128);
        wb.write(a.raw(), 64, &[0xAA; 8]);
        wb.resize(&a, 64).unwrap();
        wb.write(a.raw(), 0, &[1; 64]);
        let b = wb.alloc_resizable(8);
        wb.write(b.raw(), 0, &[2; 8]);
        let (a_id, b_id) = (a.raw(), b.raw());

        let mut rb = wb.flush();
        assert_eq!(read_back::<_, 8>(&mut rb, b_id), [2; 8], "B keeps its own bytes");
        assert_eq!(read_back::<_, 64>(&mut rb, a_id), [1; 64], "and A keeps its own");
    }

    #[test]
    fn a_sizedness_conversion_of_a_claimed_allocation_keeps_its_content() {
        // §1 bug 3. The predecessor performed only the re-mint, so the content of
        // an allocation claimed by an earlier flush was silently dropped.
        let wb = write_backend();
        let p = wb.alloc_fixed_size(16);
        wb.write(p.raw(), 0, &[9; 16]);
        let rb = wb.flush();

        let wb = rb.reopen();
        let q = wb.make_resizable(p, 16).unwrap();
        let id = q.raw();
        let mut rb = wb.flush();
        assert_eq!(read_back::<_, 16>(&mut rb, id), [9; 16]);
    }

    // ---- the `pending`-as-a-delta fixes ----

    #[test]
    fn geometry_queries_see_allocations_claimed_by_an_earlier_flush() {
        // The predecessor's map was exhaustive rather than a delta, so every id
        // from an earlier transaction reported `DanglingPointer`.
        let wb = write_backend();
        let p = wb.alloc_fixed_size(12);
        let id = p.raw();
        let wb = wb.flush().reopen();

        assert_eq!(wb.size(id).unwrap(), 12);
        assert!(wb.resolve(id).is_ok());
    }

    #[test]
    fn resizing_an_allocation_from_an_earlier_flush_works() {
        let wb = write_backend();
        let p = wb.alloc_resizable(4);
        wb.write(p.raw(), 0, &[1, 2, 3, 4]);
        let wb = wb.flush().reopen();

        wb.resize(&p, 8).unwrap();
        assert_eq!(wb.size(p.raw()).unwrap(), 8);
        let id = p.raw();
        let mut rb = wb.flush();
        assert_eq!(rb.size(id).unwrap(), 8);
        assert_eq!(read_back::<_, 4>(&mut rb, id), [1, 2, 3, 4]);
    }

    #[test]
    fn freeing_an_allocation_from_an_earlier_flush_releases_it() {
        // The predecessor's `free` only removed a `pending` entry, so freeing an
        // already-claimed id was a silent no-op -- a leak with no error.
        let wb = write_backend();
        let p = wb.alloc_fixed_size(64);
        let q = wb.alloc_fixed_size(64);
        let wb = wb.flush().reopen();
        assert_eq!(wb.inner.borrow().composed.live_count(), 2);

        wb.free_fixed_size(p);
        let rb = wb.flush();
        assert_eq!(rb.live_count(), 1);
        assert!(rb.size(q.raw()).is_ok());
    }

    #[test]
    fn a_transient_allocations_counter_is_recycled_at_flush() {
        // `free` of a `New` entry lands on `Freed`, not on absent, precisely so
        // the counter still comes back. Absent would leak it.
        let wb = write_backend();
        let a = wb.alloc_fixed_size(8);
        let counter = a.raw().counter();
        wb.free_fixed_size(a);
        let wb = wb.flush().reopen();

        let b = wb.alloc_fixed_size(8);
        assert_eq!(b.raw().counter(), counter, "the counter must be reusable");
    }

    // ---- pre-existing behaviour that must survive ----

    #[test]
    #[ignore = "FFD claim ordering returns with the scheduler (design note §5.2); \
                the naive replayer claims in log order"]
    fn flush_claims_the_largest_pending_allocations_first() {
        let wb = write_backend();
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
    fn the_same_log_always_produces_the_same_layout() {
        // Determinism is not merely nice here: design note §9.4 makes it
        // load-bearing, because recovery re-derives each claim's address rather
        // than reading it from the log. A `HashMap` iteration order leaking into
        // the flush would be silent until a crash, so it is asserted from the
        // start even though nothing depends on it yet.
        let layout_of = |sizes: &[u32]| {
            let wb = write_backend();
            let ids: Vec<_> = sizes
                .iter()
                .map(|&s| wb.alloc_fixed_size(s).raw())
                .collect();
            let rb = wb.flush();
            ids.iter()
                .map(|&id| rb.inner.composed.heap.lookup(id).unwrap())
                .collect::<Vec<(u64, u32)>>()
        };
        let sizes = [7, 64, 7, 200, 31, 5, 90];
        assert_eq!(layout_of(&sizes), layout_of(&sizes));
    }

    #[test]
    fn flush_compacts_within_its_budget_and_truncates_the_store() {
        let wb = write_backend();
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
        let b_id = b.raw();
        let rb = wb.flush();
        assert_eq!(rb.len(), 128);

        let wb = rb.reopen();
        wb.free_fixed_size(a);

        let mut rb = wb.flush();
        assert_eq!(rb.len(), 64, "the survivor slid down into the freed range");
        assert_eq!(
            read_back::<_, 64>(&mut rb, b_id),
            [7; 64],
            "compaction moved the bytes, not just the entry",
        );
    }

    // ---- what the fold buys (design note §4.2) ----

    #[test]
    fn repeated_writes_to_one_range_collapse_to_a_single_write() {
        // The piece table holds exactly one source per byte, so this is a
        // consequence of the representation rather than a special case.
        let (wb, writes, bytes) = counting();
        let p = wb.alloc_fixed_size(8);
        for i in 0..100u8 {
            wb.write(p.raw(), 0, &[i; 8]);
        }
        let id = p.raw();
        let mut rb = wb.flush();

        assert_eq!(writes.get(), 1, "one write, not a hundred");
        assert_eq!(bytes.get(), 8);
        assert_eq!(read_back::<_, 8>(&mut rb, id), [99; 8], "and the last one wins");
    }

    #[test]
    fn a_transient_allocation_never_touches_storage() {
        // Fresh and freed inside one transaction: its content is fully symbolic,
        // so nothing about it ever needs to reach the file.
        let (wb, writes, _) = counting();
        let p = wb.alloc_resizable(64);
        wb.write(p.raw(), 0, &[7; 64]);
        wb.write(p.raw(), 32, &[8; 32]);
        wb.free_resizable(p);

        let rb = wb.flush();
        assert_eq!(writes.get(), 0);
        assert!(rb.is_empty());
    }

    #[test]
    fn splice_on_a_fresh_allocation_is_a_table_edit() {
        // The shift never touches bytes: only the spliced *result* is written,
        // once. `PersistableString`/`PersistableVec` splice in loops, so this is
        // the largest single win the fold offers.
        let (wb, writes, bytes) = counting();
        let p = wb.alloc_resizable(6);
        wb.write(p.raw(), 0, &[1, 2, 3, 4, 5, 6]);
        wb.splice(&p, 1, 2, &[9, 9, 9]);
        wb.splice(&p, 0, 0, &[0]);
        let id = p.raw();

        let mut rb = wb.flush();
        assert_eq!(writes.get(), 1, "one write for the whole result");
        assert_eq!(bytes.get(), 8);
        assert_eq!(read_back::<_, 8>(&mut rb, id), [0, 1, 9, 9, 9, 4, 5, 6]);
    }

    #[test]
    fn an_untouched_persistent_allocation_is_not_rewritten() {
        // Its whole table is one identity piece, which emits nothing. Without
        // that rule every flush would copy every live allocation onto itself.
        let (wb, writes, _) = counting();
        let p = wb.alloc_fixed_size(64);
        wb.write(p.raw(), 0, &[3; 64]);
        let q = wb.alloc_fixed_size(64);
        wb.write(q.raw(), 0, &[4; 64]);
        let rb = wb.flush();
        let before = writes.get();

        // A second transaction that touches only `q`.
        let wb = rb.reopen();
        wb.write(q.raw(), 0, &[5; 64]);
        let (p_id, q_id) = (p.raw(), q.raw());
        let mut rb = wb.flush();

        assert_eq!(writes.get() - before, 1, "only `q` is written");
        assert_eq!(read_back::<_, 64>(&mut rb, p_id), [3; 64]);
        assert_eq!(read_back::<_, 64>(&mut rb, q_id), [5; 64]);
    }

    #[test]
    fn writing_a_range_twice_across_a_resize_writes_only_what_survives() {
        // The clip is what makes this safe: the first write is bounded by the
        // size at the time it was made, and the surviving prefix is emitted once.
        let (wb, _, bytes) = counting();
        let p = wb.alloc_resizable(64);
        wb.write(p.raw(), 0, &[1; 64]);
        wb.resize(&p, 8).unwrap();
        let id = p.raw();

        let mut rb = wb.flush();
        assert_eq!(bytes.get(), 8, "only the 8 surviving bytes are written");
        assert_eq!(read_back::<_, 8>(&mut rb, id), [1; 8]);
    }

    // ---- sizedness conversions move nothing (design note §4.3) ----

    #[test]
    fn a_sizedness_conversion_keeps_the_address_and_writes_nothing() {
        let (wb, writes, _) = counting();
        let p = wb.alloc_fixed_size(64);
        wb.write(p.raw(), 0, &[3; 64]);
        let filler = wb.alloc_fixed_size(64);
        wb.write(filler.raw(), 0, &[4; 64]);
        let rb = wb.flush();
        let before_addr = rb.inner.composed.heap.lookup(p.raw()).unwrap().0;
        let before_writes = writes.get();

        let wb = rb.reopen();
        let q = wb.make_resizable(p, 64).unwrap();
        let id = q.raw();
        let mut rb = wb.flush();

        assert_eq!(
            writes.get(),
            before_writes,
            "a size-preserving conversion is a relabel: no byte moves",
        );
        assert_eq!(
            rb.inner.composed.heap.lookup(id).unwrap().0,
            before_addr,
            "and the allocation keeps its address",
        );
        assert_eq!(read_back::<_, 64>(&mut rb, id), [3; 64]);
        assert_eq!(id.sizedness(), Sizedness::Resizable);
    }

    #[test]
    fn oscillating_a_chunk_across_a_sizedness_boundary_copies_nothing() {
        // The motivating case: a chunked vector whose length oscillates across a
        // chunk boundary converts its last chunk on every push/pop pair. The
        // allocate-copy-free form copied the whole chunk each time.
        let (wb, _, bytes) = counting();
        let chunk = wb.alloc_fixed_size(256);
        wb.write(chunk.raw(), 0, &[1; 256]);
        let neighbour = wb.alloc_fixed_size(256);
        wb.write(neighbour.raw(), 0, &[2; 256]);
        let rb = wb.flush();
        let addr = rb.inner.composed.heap.lookup(chunk.raw()).unwrap().0;
        let baseline = bytes.get();

        let mut owned = Owned::Fixed(chunk);
        let mut rb = rb;
        for _ in 0..8 {
            let wb = rb.reopen();
            owned = match owned {
                Owned::Fixed(f) => Owned::Resizable(wb.make_resizable(f, 256).unwrap()),
                Owned::Resizable(r) => Owned::Fixed(wb.make_fixed_size(r, 256).unwrap()),
            };
            rb = wb.flush();
        }

        assert_eq!(bytes.get(), baseline, "sixteen conversions, zero bytes copied");
        let id = match &owned {
            Owned::Fixed(p) => p.raw(),
            Owned::Resizable(p) => p.raw(),
        };
        assert_eq!(rb.inner.composed.heap.lookup(id).unwrap().0, addr);
        assert_eq!(read_back::<_, 256>(&mut rb, id), [1; 256]);
    }

    enum Owned {
        Fixed(UniquePointerFixedSize<Pointer<u32>>),
        Resizable(UniquePointerResizable<Pointer<u32>>),
    }

    #[test]
    fn a_growing_conversion_still_preserves_the_leading_bytes() {
        let (wb, _, _) = counting();
        let p = wb.alloc_fixed_size(16);
        wb.write(p.raw(), 0, &[5; 16]);
        let rb = wb.flush();

        let wb = rb.reopen();
        let q = wb.make_resizable(p, 32).unwrap();
        let id = q.raw();
        let mut rb = wb.flush();

        assert_eq!(rb.size(id).unwrap(), 32);
        assert_eq!(read_back::<_, 16>(&mut rb, id), [5; 16]);
    }

    // ---- hoisting, and what frees-first buys (design note §6) ----

    #[test]
    fn a_copy_out_of_an_allocation_that_is_then_freed_still_reads_it() {
        // The §6.1 scenario. Without hoisting the read has to happen before the
        // release, which forces releases to the end of the schedule.
        let wb = write_backend();
        let a1 = wb.alloc_fixed_size(256);
        wb.write(a1.raw(), 0, &[0xAB; 256]);
        let a2 = wb.alloc_fixed_size(128);
        wb.write(a2.raw(), 0, &[0; 128]);
        let rb = wb.flush();

        let wb = rb.reopen();
        wb.copy(a1.raw(), 0, 64, a2.raw(), 32);
        wb.free_fixed_size(a1);
        let a3 = wb.alloc_fixed_size(200);
        wb.write(a3.raw(), 0, &[7; 200]);
        let (a2_id, a3_id) = (a2.raw(), a3.raw());

        let mut rb = wb.flush();
        let a2_bytes = read_back::<_, 128>(&mut rb, a2_id);
        assert_eq!(&a2_bytes[..32], &[0; 32], "before the copied range");
        assert_eq!(&a2_bytes[32..96], &[0xAB; 64], "the copied range");
        assert_eq!(&a2_bytes[96..], &[0; 32], "after it");
        assert_eq!(read_back::<_, 200>(&mut rb, a3_id), [7; 200]);
    }

    #[test]
    fn hoisting_lets_a_new_allocation_reuse_the_freed_range() {
        // The payoff §6.3 and §6.5 are about: because the read of A1 is resolved
        // up front, `Release(A1)` has nothing waiting on it and can run before the
        // claim -- so A3 lands in A1's space instead of extending the file.
        let wb = write_backend();
        let a1 = wb.alloc_fixed_size(256);
        wb.write(a1.raw(), 0, &[0xAB; 256]);
        let a2 = wb.alloc_fixed_size(128);
        let rb = wb.flush();
        let a1_addr = rb.inner.composed.heap.lookup(a1.raw()).unwrap().0;
        let len_before = rb.len();

        let wb = rb.reopen();
        wb.copy(a1.raw(), 0, 64, a2.raw(), 32);
        wb.free_fixed_size(a1);
        let a3 = wb.alloc_fixed_size(200);
        let a3_id = a3.raw();
        let rb = wb.flush();

        assert_eq!(
            rb.inner.composed.heap.lookup(a3_id).unwrap().0,
            a1_addr,
            "A3 reuses the range A1 vacated",
        );
        assert!(rb.len() <= len_before, "and the file did not grow");
    }

    #[test]
    fn a_copy_out_of_a_transient_allocation_never_materializes_it() {
        // The copy takes *pieces*, not bytes, so it resolves to the literals
        // already in the log and the source never has to reach storage (§4.2).
        let (wb, writes, bytes) = counting();
        let dst = wb.alloc_fixed_size(32);
        let scratch = wb.alloc_resizable(32);
        wb.write(scratch.raw(), 0, &[9; 32]);
        wb.copy(scratch.raw(), 0, 32, dst.raw(), 0);
        wb.free_resizable(scratch);
        let id = dst.raw();

        let mut rb = wb.flush();
        assert_eq!(writes.get(), 1, "one write, for the destination only");
        assert_eq!(bytes.get(), 32);
        assert_eq!(read_back::<_, 32>(&mut rb, id), [9; 32]);
        assert_eq!(rb.live_count(), 1);
    }

    #[test]
    fn a_copy_within_one_allocation_behaves_like_memmove() {
        let wb = write_backend();
        let p = wb.alloc_fixed_size(8);
        wb.write(p.raw(), 0, &[1, 2, 3, 4, 5, 6, 7, 8]);
        wb.copy(p.raw(), 0, 6, p.raw(), 2); // overlapping, shifting up
        let id = p.raw();

        let mut rb = wb.flush();
        assert_eq!(read_back::<_, 8>(&mut rb, id), [1, 2, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn splice_replaces_a_range_and_shifts_the_tail() {
        let wb = write_backend();
        let p = wb.alloc_resizable(6);
        wb.write(p.raw(), 0, &[1, 2, 3, 4, 5, 6]);
        wb.splice(&p, 1, 2, &[9, 9, 9]);
        let id = p.raw();

        let mut rb = wb.flush();
        assert_eq!(rb.size(id).unwrap(), 7);
        assert_eq!(read_back::<_, 7>(&mut rb, id), [1, 9, 9, 9, 4, 5, 6]);
    }
}
