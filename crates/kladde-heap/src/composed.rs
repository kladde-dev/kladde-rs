//! [`Composed`]: the shared `(Storage, RelocatableHeap, id pool)` core holding
//! every **immediate** backend operation as a plain `&mut self` method.
//!
//! The split is the one `incremental-compaction.md` §5 argues for: the heap owns
//! the geometry *and* the `id -> address` table (it is the component choosing
//! moves, so it needs index-grade access to both sides), while the backend keeps
//! what persistence actually couples to -- **minting and recycling ids**,
//! **storage I/O**, and the journal. There is no second in-memory id table here.
//!
//! Ids are `Pointer<W>`, whose low bit carries sizedness (see
//! [`Pointer::from_parts`]). That is why nothing in this module stores sizedness
//! separately: it is a property of the id, so `resolve` can answer it without
//! consulting the heap for anything but liveness, and a sizedness *conversion*
//! necessarily mints a new id rather than re-tagging the old one.
//!
//! Both concrete backends build on this type: `UnjournaledBackend` calls these
//! methods directly, and `JournaledBackend` calls them during journal replay.

use std::io::{self, SeekFrom};

use crate::backend::BackendError;
use crate::heap::{CompactionProgress, RelocatableHeap, Relocation};
use crate::pointer::{
    Pointer, ResolvedPointer, Sizedness, UniquePointerFixedSize, UniquePointerResizable,
};
use crate::storage::Storage;
use crate::word::Word;

/// Storage `S`, relocatable heap `H`, and the backend-owned id pool over the
/// pointer width `W`.
pub(crate) struct Composed<S, H: RelocatableHeap, W: Word = u32> {
    pub storage: S,
    pub heap: H,
    /// Highest counter handed out so far. Shared across both sizednesses, so
    /// `Pointer::counter` stays unique.
    next_counter: W,
    /// Counters of freed ids, reusable for either sizedness. LIFO keeps ids
    /// dense at no cost; the on-file table layout is free to want something
    /// else, which is exactly why this lives in the backend and not the heap.
    free_counters: Vec<W>,
}

impl<S: Storage, H: RelocatableHeap<Id = Pointer<W>>, W: Word> Composed<S, H, W> {
    pub(crate) fn new(storage: S, heap: H) -> Self {
        Self {
            storage,
            heap,
            next_counter: W::zero(),
            free_counters: Vec::new(),
        }
    }

    /// Number of live allocations (for leak checks).
    pub(crate) fn live_count(&self) -> usize {
        self.heap.live_count()
    }

    // ---- the id pool ----

    /// Mint a fresh id of the given sizedness, without reserving any space for
    /// it. The journaled backend uses this on its own to hand out a stable id
    /// early and defer [`claim`](Composed::claim) to flush.
    pub(crate) fn mint(&mut self, sizedness: Sizedness) -> Pointer<W> {
        let counter = self.free_counters.pop().unwrap_or_else(|| {
            self.next_counter += W::from_usize(1);
            self.next_counter
        });
        Pointer::from_parts(counter, sizedness).expect("id counter exhausted")
    }

    fn recycle(&mut self, id: Pointer<W>) {
        self.free_counters.push(id.counter());
    }

    /// Reserve space for an already-minted `id`.
    pub(crate) fn claim(&mut self, id: Pointer<W>, size: H::Size) {
        let address = self
            .heap
            .alloc(id, size)
            .expect("in-memory heap never runs out of address space");
        self.cover(address, size)
            .expect("extend storage for new allocation");
    }

    // ---- storage helpers ----

    fn address_of(&self, id: Pointer<W>) -> H::Address {
        self.heap
            .lookup(id)
            .expect("operation on a dangling pointer")
            .0
    }

    fn seek_to(&mut self, id: Pointer<W>, offset: H::Size) -> io::Result<()> {
        let pos = (self.address_of(id).to_usize() + offset.to_usize()) as u64;
        self.storage.seek(SeekFrom::Start(pos))?;
        Ok(())
    }

    /// Ensure the store covers `[address, address + size)`.
    fn cover(&mut self, address: H::Address, size: H::Size) -> io::Result<()> {
        let end = (address.to_usize() + size.to_usize()) as u64;
        if self.storage.len()? < end {
            self.storage.resize(end)?;
        }
        Ok(())
    }

    /// Copy `len` bytes `src -> dst`. Safe for overlapping ranges: the source is
    /// buffered in full before anything is written, which is what a compaction
    /// slide needs (its destination overlaps its source whenever the gap it
    /// closes is narrower than the run it moves).
    fn copy_bytes(&mut self, src: H::Address, dst: H::Address, len: usize) -> io::Result<()> {
        if len == 0 || src == dst {
            return Ok(());
        }
        let mut buf = vec![0u8; len];
        self.storage.seek(SeekFrom::Start(src.to_usize() as u64))?;
        self.storage.read_exact(&mut buf)?;
        self.storage.seek(SeekFrom::Start(dst.to_usize() as u64))?;
        self.storage.write_all(&buf)?;
        Ok(())
    }

    // ---- immediate operations ----

    fn alloc(&mut self, size: H::Size, sizedness: Sizedness) -> Pointer<W> {
        let id = self.mint(sizedness);
        self.claim(id, size);
        id
    }

    pub(crate) fn alloc_resizable(&mut self, size: H::Size) -> UniquePointerResizable<Pointer<W>> {
        UniquePointerResizable::from_pointer(self.alloc(size, Sizedness::Resizable))
    }
    pub(crate) fn alloc_fixed_size(&mut self, size: H::Size) -> UniquePointerFixedSize<Pointer<W>> {
        UniquePointerFixedSize::from_pointer(self.alloc(size, Sizedness::Fixed))
    }

    pub(crate) fn free(&mut self, id: Pointer<W>) {
        self.heap.free(id).expect("free of a dangling handle");
        self.recycle(id);
    }

    /// Release `id`, whether or not it ever reached the heap.
    ///
    /// The deferred path needs this: an allocation minted and freed inside one
    /// transaction is never claimed, so there is nothing for the heap to release
    /// -- but its counter must still come back, or the id pool leaks. See design
    /// note §3, which is why `free` of a `New` entry lands on `Freed` rather than
    /// on absent.
    pub(crate) fn release(&mut self, id: Pointer<W>) {
        if self.heap.lookup(id).is_some() {
            self.heap.free(id).expect("release of a live id");
        }
        self.recycle(id);
    }

    pub(crate) fn free_resizable(&mut self, p: UniquePointerResizable<Pointer<W>>) {
        self.free(p.raw());
    }
    pub(crate) fn free_fixed_size(&mut self, p: UniquePointerFixedSize<Pointer<W>>) {
        self.free(p.raw());
    }

    pub(crate) fn resize(
        &mut self,
        p: &UniquePointerResizable<Pointer<W>>,
        new_size: H::Size,
    ) -> Result<(), BackendError> {
        self.resize_by_id(p.raw(), new_size)
    }

    /// [`resize`](Self::resize) addressed by id rather than by owned handle, for
    /// the journal replayer -- which holds ids, never handles.
    pub(crate) fn resize_by_id(
        &mut self,
        id: Pointer<W>,
        new_size: H::Size,
    ) -> Result<(), BackendError> {
        let (_, old_size) = self.heap.lookup(id).ok_or(BackendError::DanglingPointer)?;
        let moved = self
            .heap
            .resize(id, new_size)
            .expect("resize of a dangling handle");
        debug_assert!(
            non_overlapping(&moved, old_size.to_usize().min(new_size.to_usize())),
            "a heap-initiated move must not overlap: replaying a partially applied \
             overlapping copy reads bytes the partial run already clobbered, which \
             is what makes compaction slides non-restartable (see later.md)",
        );
        match moved {
            Relocation::Single { old, new } => {
                let copy_len = old_size.to_usize().min(new_size.to_usize());
                self.cover(new, new_size)?;
                self.copy_bytes(old, new, copy_len)?;
            }
            // A lift: the mover goes up and a replacement comes down into the
            // address it vacated. The order is not ours to choose -- the second
            // destination *is* the first source.
            Relocation::Double {
                first,
                then,
                then_len,
            } => {
                let copy_len = old_size.to_usize().min(new_size.to_usize());
                self.cover(first.1, new_size)?;
                self.copy_bytes(first.0, first.1, copy_len)?;
                self.cover(then.1, then_len)?;
                self.copy_bytes(then.0, then.1, then_len.to_usize())?;
            }
            Relocation::None => self.cover(self.address_of(id), new_size)?,
        }
        Ok(())
    }

    /// Convert sizedness. Because sizedness lives in the *id*, this necessarily
    /// mints a **new** id: it allocates a fresh range of the new sizedness,
    /// copies the data across, and frees the old one. The single owner is handed
    /// the new id back, so the `UniquePointer*` discipline keeps this sound.
    fn convert(
        &mut self,
        old_id: Pointer<W>,
        new_size: H::Size,
        new_sizedness: Sizedness,
    ) -> Result<Pointer<W>, BackendError> {
        let new_id = self.mint(new_sizedness);
        self.convert_to(old_id, new_id, new_size)?;
        Ok(new_id)
    }

    /// [`convert`](Self::convert) with the new id supplied rather than minted.
    ///
    /// The journal replayer needs this: the log already recorded which id the
    /// conversion produced, so replay must reuse it rather than mint a second one.
    pub(crate) fn convert_to(
        &mut self,
        old_id: Pointer<W>,
        new_id: Pointer<W>,
        new_size: H::Size,
    ) -> Result<(), BackendError> {
        self.relabel(old_id, new_id)?;
        self.resize_by_id(new_id, new_size)
    }

    /// Rekey an allocation in place, recycling the old counter.
    ///
    /// This is what makes a sizedness conversion cheap: **no bytes move**. The
    /// previous implementation allocated a fresh range, copied across and freed
    /// the old one, so every conversion relocated the allocation even when its
    /// size was unchanged -- ruinous for a chunked container oscillating across a
    /// chunk boundary. See `journal-semantics.md` §4.3.
    pub(crate) fn relabel(
        &mut self,
        from: Pointer<W>,
        to: Pointer<W>,
    ) -> Result<(), BackendError> {
        self.heap
            .relabel(from, to)
            .map_err(|_| BackendError::DanglingPointer)?;
        self.recycle(from);
        Ok(())
    }

    pub(crate) fn make_resizable(
        &mut self,
        p: UniquePointerFixedSize<Pointer<W>>,
        new_size: H::Size,
    ) -> Result<UniquePointerResizable<Pointer<W>>, BackendError> {
        let id = self.convert(p.raw(), new_size, Sizedness::Resizable)?;
        Ok(UniquePointerResizable::from_pointer(id))
    }
    pub(crate) fn make_fixed_size(
        &mut self,
        p: UniquePointerResizable<Pointer<W>>,
        new_size: H::Size,
    ) -> Result<UniquePointerFixedSize<Pointer<W>>, BackendError> {
        let id = self.convert(p.raw(), new_size, Sizedness::Fixed)?;
        Ok(UniquePointerFixedSize::from_pointer(id))
    }

    pub(crate) fn write(&mut self, anchor: Pointer<W>, offset: H::Size, bytes: &[u8]) {
        self.seek_to(anchor, offset).expect("seek for write");
        self.storage.write_all(bytes).expect("write bytes");
    }

    pub(crate) fn splice(
        &mut self,
        p: &UniquePointerResizable<Pointer<W>>,
        offset: H::Size,
        old_len: H::Size,
        new: &[u8],
    ) {
        self.splice_by_id(p.raw(), offset, old_len, new)
    }

    /// [`splice`](Self::splice) addressed by id, for the journal replayer.
    pub(crate) fn splice_by_id(
        &mut self,
        id: Pointer<W>,
        offset: H::Size,
        old_len: H::Size,
        new: &[u8],
    ) {
        let (_, size) = self.heap.lookup(id).expect("splice of a dangling handle");
        let old_size = size.to_usize();
        let off = offset.to_usize();
        let tail_start = off + old_len.to_usize();
        assert!(tail_start <= old_size, "splice range out of bounds");
        let tail_len = old_size - tail_start;

        // Save the trailing bytes (at the current address) before any relocation.
        let mut tail = vec![0u8; tail_len];
        self.seek_to(id, Word::from_usize(tail_start))
            .expect("seek to tail");
        self.storage.read_exact(&mut tail).expect("read tail");

        // Resize to fit `new` in place of the spliced-out range.
        let new_size: H::Size = Word::from_usize(off + new.len() + tail_len);
        self.resize_by_id(id, new_size).expect("splice resize");

        // Lay down `new`, then the saved tail immediately after it.
        self.seek_to(id, offset).expect("seek for splice write");
        self.storage.write_all(new).expect("write new");
        self.storage.write_all(&tail).expect("write tail");
    }

    /// Copy `len` bytes between two allocations (or within one).
    ///
    /// `copy_bytes` stages the source in full before writing, so an overlapping
    /// range within a single allocation moves correctly.
    pub(crate) fn copy_between(
        &mut self,
        src: Pointer<W>,
        src_offset: H::Size,
        len: H::Size,
        dst: Pointer<W>,
        dst_offset: H::Size,
    ) {
        let from = self.address_of(src).to_usize() + src_offset.to_usize();
        let to = self.address_of(dst).to_usize() + dst_offset.to_usize();
        self.copy_bytes(Word::from_usize(from), Word::from_usize(to), len.to_usize())
            .expect("copy between allocations");
    }

    /// Read `len` bytes from `id`'s address plus `offset`.
    ///
    /// The flush emitter's gather step: a destination run is assembled from
    /// however many scattered sources it names before a single write puts it down.
    pub(crate) fn read_bytes(&mut self, id: Pointer<W>, offset: usize, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        self.seek_to(id, Word::from_usize(offset))
            .expect("seek for gather");
        self.storage.read_exact(&mut buf).expect("gather bytes");
        buf
    }

    /// Position the cursor and hand out the store as a seekable reader.
    pub(crate) fn read_at(&mut self, anchor: Pointer<W>, offset: H::Size) -> &mut S {
        self.seek_to(anchor, offset).expect("seek for read");
        &mut self.storage
    }

    // ---- compaction ----

    /// Execute compaction steps until the heap quiesces or `budget` bytes have
    /// been copied, then truncate the store to the heap's new length.
    ///
    /// Deliberately **not** gated on `IncrementallyCompactableHeap`: this is the
    /// shared path a flush calls unconditionally, and a heap that does not
    /// compact simply proposes nothing and makes this a no-op. The gated,
    /// user-facing entry points sit on the concrete backends.
    ///
    /// A step exceeding the remaining budget is executed only when nothing has
    /// moved yet, so a single oversized move can never be starved forever but
    /// also cannot blow the budget on top of work already done.
    pub(crate) fn compact_incrementally(&mut self, budget: H::Address) -> CompactionProgress {
        let mut progress = CompactionProgress::default();
        loop {
            let remaining = budget
                .to_usize()
                .saturating_sub(progress.bytes_moved as usize);
            let Some(step) = self
                .heap
                .propose_compaction_step(Word::from_usize(remaining))
            else {
                progress.quiesced = true;
                break;
            };
            let cost = step.len.to_usize();
            if cost > remaining && progress.steps > 0 {
                break;
            }
            self.copy_bytes(step.from, step.to, cost)
                .expect("copy bytes for a compaction step");
            self.heap.commit_compaction_step(step);
            progress.steps += 1;
            progress.bytes_moved += cost as u64;
            if progress.bytes_moved as usize >= budget.to_usize() {
                break;
            }
        }
        self.truncate_to_heap().expect("truncate after compaction");
        progress
    }

    /// Shrink the store to the heap's current length -- the payoff compaction is
    /// working toward. Safe at any time: the heap's `len` is one past its
    /// highest live byte.
    fn truncate_to_heap(&mut self) -> io::Result<()> {
        let wanted = self.heap.len().to_usize() as u64;
        if self.storage.len()? > wanted {
            self.storage.resize(wanted)?;
        }
        Ok(())
    }

    // ---- queries ----

    pub(crate) fn size(&self, id: Pointer<W>) -> Result<H::Size, BackendError> {
        self.heap
            .lookup(id)
            .map(|(_, size)| size)
            .ok_or(BackendError::DanglingPointer)
    }

    /// Rebuild the owned handle for `id`. The sizedness comes from the id itself;
    /// the heap is consulted only to confirm the id is still live.
    pub(crate) fn resolve(
        &self,
        id: Pointer<W>,
    ) -> Result<ResolvedPointer<Pointer<W>>, BackendError> {
        if self.heap.lookup(id).is_none() {
            return Err(BackendError::DanglingPointer);
        }
        Ok(resolved(id))
    }
}

/// Whether a relocation's source and destination ranges are disjoint.
///
/// `GainGreedyHeap` finds a resize's new home *before* releasing the old one, and
/// draws the lift's replacement from above the vacated gap, so this holds today
/// -- but by an argument that is structural and implicit rather than stated. A
/// placement policy that let an allocation grow *downward* into the gap below it
/// would break it silently, and the consequence only shows up after a crash: a
/// partially applied overlapping copy has already clobbered bytes that re-running
/// it from the start would read. See `later.md`.
fn non_overlapping<A: Word, Sz: Word>(moved: &Relocation<A, Sz>, copy_len: usize) -> bool {
    let disjoint = |a: A, b: A, len: usize| {
        let (a, b) = (a.to_usize(), b.to_usize());
        len == 0 || a + len <= b || b + len <= a
    };
    match *moved {
        Relocation::None => true,
        Relocation::Single { old, new } => disjoint(old, new, copy_len),
        Relocation::Double {
            first,
            then,
            then_len,
        } => disjoint(first.0, first.1, copy_len) && disjoint(then.0, then.1, then_len.to_usize()),
    }
}

/// The owned handle `id`'s own sizedness bit calls for.
pub(crate) fn resolved<W: Word>(id: Pointer<W>) -> ResolvedPointer<Pointer<W>> {
    match id.sizedness() {
        Sizedness::Resizable => {
            ResolvedPointer::Resizable(UniquePointerResizable::from_pointer(id))
        }
        Sizedness::Fixed => ResolvedPointer::Fixed(UniquePointerFixedSize::from_pointer(id)),
    }
}
