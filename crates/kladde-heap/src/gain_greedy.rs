//! [`GainGreedyHeap`]: the concrete [`RelocatableHeap`] of
//! `incremental-compaction.md` §4 -- one address-keyed map of allocations, its
//! derived indexes, and a gain-greedy choice of compaction step.
//!
//! Named for the policy rather than the structure, since a sibling
//! implementation would differ exactly there.
//!
//! # The structure
//!
//! The single source of truth is `allocations`, keyed by start address. **Gaps
//! are not stored**: the gap preceding the entry at `a` runs from the previous
//! entry's end to `a`, and `end` is the last entry's end -- so there is never a
//! trailing gap, and freeing the topmost allocation truncates for free.
//!
//! The derived indexes are maintained by the same two primitives every mutation
//! goes through (`insert_raw`/`remove_raw`), which is what keeps the "which move
//! is best" question answerable by a query rather than a scan.
//!
//! # The policy
//!
//! Every move relocates live bytes *downward*, and one lens prices all of them:
//! moving `s` bytes down by `d` costs `s` and buys `s·d` of progress against the
//! potential `Φ = Σ_{live bytes} address`, so the **per-byte gain is the travel
//! distance `d`**. Greedily maximizing `d` needs no lookahead: the cheap interior
//! moves that merely *enable* a later truncation are themselves credited
//! immediately, because `Φ` falls the moment bytes move down.
//!
//! # Candidate shapes
//!
//! - **Slide**: the maximal contiguous *run* above the largest gap shifts down
//!   into it. It does not require the moved bytes to *fit* -- `run.size >
//!   gap.width` is the normal case and the move is a partial overlapping shift
//!   -- which is what guarantees progress while any gap exists.
//! - **Evacuation**: an allocation jumps down into a gap that fits it. **Not
//!   implemented here yet.** The address-ordered branch-and-bound search this
//!   module used to carry has been removed wholesale, to be replaced by the
//!   augmented size-keyed index of `augmented-segment-tree.md`, which keeps the
//!   best evacuation at a tree root instead of searching for it. Until then the
//!   slide is the only candidate, so compaction still converges -- just more
//!   expensively, since a slide copies a whole run to close one gap.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::gap_tree::GapTree;
use crate::heap::{
    AllocationId, HeapError, IncrementallyCompactableHeap, RelocatableHeap, Relocation, Step,
};

/// One row of the address-keyed table.
#[derive(Clone, Copy)]
struct Entry<Id> {
    len: u32,
    id: Id,
}

/// Counters for what compaction and placement actually did.
///
/// Test-only instrumentation: outside `cfg(test)` none of this exists and the
/// heap carries no extra field. See `test-results/` for measurements.
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SearchStats {
    /// Calls to `propose_compaction_step`.
    pub calls: u64,
    /// Calls that returned a step (the rest found the heap already compact).
    pub proposals: u64,
    /// Committed steps that were the slide candidate, and that were an
    /// evacuation. Which shape wins is what decides whether a burst shrinks the
    /// file or merely rearranges it -- see `truncated_by_*`.
    pub slides: u64,
    pub evacuations: u64,
    /// Bytes those steps moved.
    pub slide_bytes: u64,
    pub evac_bytes: u64,
    /// Bytes `end` fell by as a result. **This is the only way free space
    /// leaves the file**: every other move conserves it, taking `len` free bytes
    /// at the destination and giving `len` back at the source.
    pub truncated_by_slides: u64,
    pub truncated_by_evacuations: u64,
    /// Placements (`alloc`, and the relocating half of `resize`) that found a
    /// gap to sit in, and those that had to extend `end` because nothing fitted.
    ///
    /// The other half of the free-space budget: free bytes are created by
    /// freeing something mid-heap and destroyed either by truncation at the top
    /// or by a placement landing in a gap. A heap whose gaps are *usable* runs
    /// leaner without compaction doing anything more.
    pub placed_in_gap: u64,
    pub placed_at_end: u64,
    pub placed_in_gap_bytes: u64,
    pub placed_at_end_bytes: u64,
}

#[cfg(test)]
impl SearchStats {
    fn record(&mut self, proposed: bool) {
        self.calls += 1;
        self.proposals += u64::from(proposed);
    }

    fn record_step(&mut self, is_slide: bool, len: u64, truncated: u64) {
        if is_slide {
            self.slides += 1;
            self.slide_bytes += len;
            self.truncated_by_slides += truncated;
        } else {
            self.evacuations += 1;
            self.evac_bytes += len;
            self.truncated_by_evacuations += truncated;
        }
    }
}

/// A relocatable heap over a `u64` address space with `u32` allocation sizes,
/// compacting by gain-greedy incremental steps. See the module docs.
///
/// `Clone` is provided so that a benchmark can measure repeated compaction from
/// one fixed starting state; it is not cheap (see [`GapTree`]'s `Clone`).
#[derive(Clone)]
pub struct GainGreedyHeap<Id> {
    /// Start address -> allocation. Gaps are the space between consecutive
    /// entries; `end` is the last entry's end.
    allocations: BTreeMap<u64, Entry<Id>>,
    /// The id table's address column (P1: held once, here).
    by_id: HashMap<Id, u64>,
    /// Gap length -> the start addresses of gaps that long. Answers "the largest
    /// gap", which is the slide's destination; the by-address view lives in
    /// `gaps`.
    free_by_size: BTreeMap<u64, BTreeSet<u64>>,
    /// The same gaps, address-ordered and augmented with each subtree's longest
    /// gap, which is what makes "the lowest gap that fits" a single descent
    /// instead of a scan over size classes. See [`GapTree`].
    gaps: GapTree,
    /// One past the highest live byte.
    end: u64,
    /// Sum of all live allocation sizes.
    live_bytes: u64,
    /// Test-only instrumentation; absent from real builds.
    #[cfg(test)]
    stats: std::cell::Cell<SearchStats>,
}

impl<Id> Default for GainGreedyHeap<Id> {
    fn default() -> Self {
        Self {
            allocations: BTreeMap::new(),
            by_id: HashMap::new(),
            free_by_size: BTreeMap::new(),
            gaps: GapTree::default(),
            end: 0,
            live_bytes: 0,
            #[cfg(test)]
            stats: std::cell::Cell::new(SearchStats::default()),
        }
    }
}

impl<Id: AllocationId> GainGreedyHeap<Id> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of live allocations.
    pub fn live_count(&self) -> usize {
        self.allocations.len()
    }

    /// Spend up to `budget` bytes on consecutive compaction steps, returning the
    /// steps taken and whether the heap ran out of work before the budget did.
    ///
    /// This is the caller-side loop of `compaction-algorithm.md` §5, minus the
    /// byte copying, which needs a store the bare heap does not have.
    pub fn compact_incrementally(&mut self, budget: u64) -> (u64, bool) {
        let mut moved = 0u64;
        let mut steps = 0u64;
        loop {
            let Some(step) = self.propose_compaction_step(budget - moved) else {
                return (steps, true); // ran out of work
            };
            // An oversized step is worth taking on its own, but not on top of
            // steps already paid for: leave it for the next burst.
            if step.len > budget - moved && steps > 0 {
                return (steps, false);
            }
            // Which candidate shape won, and what it did to `end`. Recomputing
            // the slide to identify it is redundant work, which is why it is
            // test-only; `Step` is small and equality is exact.
            #[cfg(test)]
            let provenance = (self.slide_candidate(budget - moved) == Some(step), self.end);
            self.commit_compaction_step(step);
            #[cfg(test)]
            {
                let (is_slide, before_end) = provenance;
                let mut stats = self.stats.get();
                stats.record_step(is_slide, step.len, before_end - self.end);
                self.stats.set(stats);
            }
            moved += step.len;
            steps += 1;
            if moved >= budget {
                return (steps, false);
            }
        }
    }

    /// Note that a proposal was made. A no-op outside tests.
    #[cfg(test)]
    fn record_search(&self, proposed: bool) {
        let mut stats = self.stats.get();
        stats.record(proposed);
        self.stats.set(stats);
    }
    #[cfg(not(test))]
    #[inline(always)]
    fn record_search(&self, _proposed: bool) {}

    /// Note where a placement landed. A no-op outside tests.
    #[cfg(test)]
    fn record_placement(&self, in_gap: bool, bytes: u64) {
        let mut stats = self.stats.get();
        if in_gap {
            stats.placed_in_gap += 1;
            stats.placed_in_gap_bytes += bytes;
        } else {
            stats.placed_at_end += 1;
            stats.placed_at_end_bytes += bytes;
        }
        self.stats.set(stats);
    }
    #[cfg(not(test))]
    #[inline(always)]
    fn record_placement(&self, _in_gap: bool, _bytes: u64) {}

    /// The search counters accumulated so far.
    #[cfg(test)]
    pub(crate) fn search_stats(&self) -> SearchStats {
        self.stats.get()
    }

    /// Zero the search counters, e.g. to measure one phase of a workload.
    #[cfg(test)]
    pub(crate) fn reset_search_stats(&self) {
        self.stats.set(SearchStats::default());
    }

    // ---- gap index maintenance ----

    fn gap_record(&mut self, start: u64, len: u64) {
        if len > 0 {
            self.free_by_size.entry(len).or_default().insert(start);
            self.gaps.insert(start, len);
        }
    }

    fn gap_forget(&mut self, start: u64, len: u64) {
        if len == 0 {
            return;
        }
        if let Some(set) = self.free_by_size.get_mut(&len) {
            set.remove(&start);
            if set.is_empty() {
                self.free_by_size.remove(&len);
            }
        }
        self.gaps.remove(start);
    }

    /// End of the allocation immediately below `addr` (0 if there is none).
    fn prev_end(&self, addr: u64) -> u64 {
        self.allocations
            .range(..addr)
            .next_back()
            .map_or(0, |(&a, e)| a + e.len as u64)
    }

    /// Start of the first allocation at or above `addr`.
    fn next_start(&self, addr: u64) -> Option<u64> {
        self.allocations.range(addr..).next().map(|(&a, _)| a)
    }

    // ---- the two primitives every mutation goes through ----

    /// Record an allocation at `addr`, which must be free and `len` bytes wide.
    fn insert_raw(&mut self, addr: u64, len: u32, id: Id) {
        let prev_end = self.prev_end(addr);
        match self.next_start(addr) {
            Some(next) => {
                // Split the enclosing gap around the new allocation.
                self.gap_forget(prev_end, next - prev_end);
                self.gap_record(prev_end, addr - prev_end);
                self.gap_record(addr + len as u64, next - (addr + len as u64));
            }
            None => {
                // Nothing above, so `end == prev_end` and we are extending it.
                self.gap_record(prev_end, addr - prev_end);
                self.end = addr + len as u64;
            }
        }
        self.allocations.insert(addr, Entry { len, id });
        self.by_id.insert(id, addr);
        self.live_bytes += len as u64;
    }

    /// Drop the allocation at `addr`, coalescing its range into the neighbouring
    /// gaps (or retreating `end` if it was the topmost).
    fn remove_raw(&mut self, addr: u64) -> Entry<Id> {
        let e = self
            .allocations
            .remove(&addr)
            .expect("remove_raw on an address with no allocation");
        self.by_id.remove(&e.id);
        self.live_bytes -= e.len as u64;

        let prev_end = self.prev_end(addr);
        let above = addr + e.len as u64;
        self.gap_forget(prev_end, addr - prev_end);
        match self.next_start(addr) {
            Some(next) => {
                self.gap_forget(above, next - above);
                self.gap_record(prev_end, next - prev_end);
            }
            None => self.end = prev_end,
        }
        e
    }

    // ---- placement ----

    /// The lowest-addressed gap at least `min_len` bytes wide -- the destination
    /// that maximizes travel distance, and so per-byte gain, for a `min_len`-byte
    /// mover.
    ///
    /// One `O(log n)` descent of the augmented [`GapTree`]. The obvious
    /// alternative, scanning `free_by_size.range(min_len..)` for the minimum
    /// address, costs one probe per distinct gap *size* -- fine when gaps cluster
    /// on a few sizes, but unbounded when they do not. `benches/lowest_fitting_gap.rs`
    /// measures the difference.
    fn lowest_gap_fitting(&self, min_len: u64) -> Option<(u64, u64)> {
        self.gaps.lowest_fitting(min_len)
    }

    /// Where to put a new `size`-byte allocation: the lowest gap that fits, else
    /// the top.
    ///
    /// Placement is scored against the same potential compaction is, but
    /// *without* a cost term: the bytes are written wherever they go, so a lower
    /// address here is free where compaction would pay a full copy for it. An
    /// allocation of size `s` at `a` adds `s·(a + (s−1)/2)` to `Φ`, and the
    /// constant drops out of a comparison -- so with nothing else in the
    /// objective, the lowest fitting gap simply wins.
    ///
    /// Nothing here yet prices the *shape* of what is left behind: an exact fit
    /// erases a gap outright and a near-fit leaves a sliver, and this cannot tell
    /// them apart. That is stage 4 of `augmented-segment-tree.md`, whose μ
    /// weights placement and compaction are meant to share.
    fn place(&self, size: u32) -> Result<u64, HeapError> {
        let want = size as u64;
        if let Some((addr, _)) = self.lowest_gap_fitting(want) {
            self.record_placement(true, want);
            return Ok(addr);
        }

        // Nothing fits: extend. `end` is above every gap, so this is only ever
        // reached when no gap could have taken the allocation at all.
        self.record_placement(false, want);
        self.end
            .checked_add(want)
            .map(|_| self.end)
            .ok_or(HeapError::OutOfMemory)
    }

    // ---- compaction candidates ----

    /// The maximal run of contiguous allocations starting at `start`, truncated
    /// to a prefix costing at most `budget` (but always at least one
    /// allocation). Returns the byte length and whether the budget cut it short.
    fn run_len_from(&self, start: u64, budget: u64) -> (u64, bool) {
        let mut cursor = start;
        let mut taken = 0u64;
        for (&addr, e) in self.allocations.range(start..) {
            if addr != cursor {
                return (taken, false); // hit a gap: the run genuinely ends here
            }
            let next = taken + e.len as u64;
            if taken > 0 && next > budget {
                return (taken, true); // stay within budget, having taken one
            }
            taken = next;
            cursor = addr + e.len as u64;
        }
        (taken, false) // ran off the top of the heap
    }

    /// The slide candidate: the run above the largest gap, shifting down into it.
    ///
    /// Currently the *only* candidate shape, so it is also what guarantees
    /// progress: it does not require the run to fit in the gap, and a maximal run
    /// is flanked by free space above (a gap, or the top of the heap), so sliding
    /// it either merges that free space with the range it vacates or lets `end`
    /// retreat. Every byte in the run travels exactly `gap_len` down, so the
    /// per-byte gain is `gap_len` -- positive while any gap exists.
    fn slide_candidate(&self, budget: u64) -> Option<Step<u64>> {
        let (&gap_len, starts) = self.free_by_size.last_key_value()?;
        let to = *starts.first()?;
        let from = to + gap_len;
        let (len, _truncated) = self.run_len_from(from, budget);
        if len == 0 {
            return None;
        }
        Some(Step { from, to, len })
    }
}

impl<Id: AllocationId> RelocatableHeap for GainGreedyHeap<Id> {
    type Id = Id;
    type Address = u64;
    type Size = u32;

    fn alloc(&mut self, id: Id, size: u32) -> Result<u64, HeapError> {
        if self.by_id.contains_key(&id) {
            return Err(HeapError::DuplicateId);
        }
        let addr = self.place(size)?;
        self.insert_raw(addr, size, id);
        Ok(addr)
    }

    fn free(&mut self, id: Id) -> Result<(), HeapError> {
        let addr = *self.by_id.get(&id).ok_or(HeapError::UnknownId)?;
        self.remove_raw(addr);
        Ok(())
    }

    fn resize(&mut self, id: Id, new_size: u32) -> Result<Relocation<u64>, HeapError> {
        let addr = *self.by_id.get(&id).ok_or(HeapError::UnknownId)?;
        let old_len = self.allocations[&addr].len;
        if new_size == old_len {
            return Ok(None);
        }

        // Shrinking, or growing into the gap immediately above, keeps the
        // address: drop the entry and re-place it at the same spot.
        let fits_in_place = if new_size < old_len {
            true
        } else {
            let above = addr + old_len as u64;
            let room = self.next_start(above).map_or(u64::MAX, |next| next - above);
            (new_size - old_len) as u64 <= room
        };
        if fits_in_place {
            let e = self.remove_raw(addr);
            self.insert_raw(addr, new_size, e.id);
            return Ok(None);
        }

        // Otherwise find the new home *before* releasing the old one, so the two
        // ranges cannot overlap and the caller's copy is unambiguous.
        let dest = self.place(new_size)?;
        let e = self.remove_raw(addr);
        self.insert_raw(dest, new_size, e.id);
        Ok(Some((addr, dest)))
    }

    fn lookup(&self, id: Id) -> Option<(u64, u32)> {
        let &addr = self.by_id.get(&id)?;
        Some((addr, self.allocations[&addr].len))
    }

    fn len(&self) -> u64 {
        self.end
    }

    fn live_bytes(&self) -> u64 {
        self.live_bytes
    }

    fn live_count(&self) -> usize {
        self.allocations.len()
    }

    fn iter(&self) -> impl Iterator<Item = (Id, u64, u32)> + '_ {
        self.allocations
            .iter()
            .map(|(&addr, e)| (e.id, addr, e.len))
    }

    /// The next compaction step: currently the slide, and nothing else.
    ///
    /// The evacuation candidate -- an allocation jumping down into a gap that
    /// fits it -- is not offered here at all; see the module docs. Losing it
    /// costs *efficiency*, not correctness: the slide already guarantees a
    /// positive-gain move exists while any gap does, so compaction still
    /// converges to a gapless heap, but it pays a whole run's worth of copying
    /// where a well-chosen evacuation would have paid one allocation's.
    ///
    /// `budget` is a ranking input, not a cap: a truncated slide is offered when
    /// the run is too long for it, and an untruncated one is offered whole even
    /// if that exceeds the budget, since reporting quiescence would strand the
    /// gap forever.
    fn propose_compaction_step(&self, budget: u64) -> Option<Step<u64>> {
        if self.free_by_size.is_empty() {
            self.record_search(false);
            return None; // gapless: compact
        }
        let chosen = self.slide_candidate(budget);
        self.record_search(chosen.is_some());
        chosen
    }
    fn commit_compaction_step(&mut self, step: Step<u64>) {
        let Step { from, to, len } = step;
        assert!(to < from, "a compaction step must move bytes downward");
        let addrs: Vec<u64> = self
            .allocations
            .range(from..from + len)
            .map(|(&addr, _)| addr)
            .collect();
        let delta = from - to;
        // Drop the whole run first, so `[to, from + len)` is one gap, then lay it
        // back down in ascending order -- each insert splits that gap correctly.
        let moved: Vec<Entry<Id>> = addrs.iter().map(|&addr| self.remove_raw(addr)).collect();
        for (addr, e) in addrs.into_iter().zip(moved) {
            self.insert_raw(addr - delta, e.len, e.id);
        }
    }
}

impl<Id: AllocationId> IncrementallyCompactableHeap for GainGreedyHeap<Id> {}

#[cfg(test)]
impl<Id: AllocationId> GainGreedyHeap<Id> {
    /// Every gap implied by `allocations`, as `(start, len)` in address order.
    fn implied_gaps(&self) -> Vec<(u64, u64)> {
        let mut gaps = Vec::new();
        let mut cursor = 0u64;
        for (&addr, e) in &self.allocations {
            if addr > cursor {
                gaps.push((cursor, addr - cursor));
            }
            cursor = addr + e.len as u64;
        }
        gaps
    }

    /// The reference `lowest_gap_fitting` must agree with: the size-class scan
    /// the augmented tree replaced.
    fn lowest_gap_fitting_by_scan(&self, min_len: u64) -> Option<(u64, u64)> {
        self.free_by_size
            .range(min_len..)
            .filter_map(|(&len, set)| set.first().map(|&start| (start, len)))
            .min_by_key(|&(start, _)| start)
    }

    /// Check that the derived indexes still agree with the source of truth.
    fn assert_invariants(&self) {
        let mut cursor = 0u64;
        let mut live = 0u64;
        for (&addr, e) in &self.allocations {
            assert!(addr >= cursor, "allocations overlap at {addr}");
            assert!(e.len > 0, "zero-length allocation at {addr}");
            assert_eq!(self.by_id.get(&e.id), Some(&addr), "by_id disagrees");
            cursor = addr + e.len as u64;
            live += e.len as u64;
        }
        assert_eq!(self.end, cursor, "end is not the top allocation's end");
        assert_eq!(self.live_bytes, live, "live_bytes drifted");
        assert_eq!(self.by_id.len(), self.allocations.len(), "stale by_id rows");

        let gaps = self.implied_gaps();
        let mut expected_free: BTreeMap<u64, BTreeSet<u64>> = BTreeMap::new();
        for &(start, len) in &gaps {
            expected_free.entry(len).or_default().insert(start);
        }
        assert_eq!(self.free_by_size, expected_free, "free_by_size drifted");

        // The augmented tree must hold exactly the same gaps...
        assert_eq!(self.gaps.iter().collect::<Vec<_>>(), gaps, "gaps drifted");
        // ...and its descent must still agree with the scan it replaced, which
        // is what would catch a stale augmentation rather than a stale entry.
        for min_len in gaps
            .iter()
            .flat_map(|&(_, len)| [len.saturating_sub(1), len, len + 1])
            .chain([1])
        {
            assert_eq!(
                self.lowest_gap_fitting(min_len),
                self.lowest_gap_fitting_by_scan(min_len.max(1)),
                "the gap tree disagreed with the scan at min_len={min_len}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pointer::{Pointer, Sizedness};

    const UNBOUNDED: u64 = u64::MAX;

    type Heap = GainGreedyHeap<Pointer<u32>>;

    fn fixed(counter: u32) -> Pointer<u32> {
        Pointer::from_parts(counter, Sizedness::Fixed).unwrap()
    }
    fn resizable(counter: u32) -> Pointer<u32> {
        Pointer::from_parts(counter, Sizedness::Resizable).unwrap()
    }

    /// Run compaction to quiescence, returning the number of steps and bytes moved.
    fn compact_fully(heap: &mut Heap, budget: u64) -> (usize, u64) {
        let (mut steps, mut bytes) = (0, 0);
        while let Some(step) = heap.propose_compaction_step(budget) {
            bytes += step.len;
            heap.commit_compaction_step(step);
            heap.assert_invariants();
            steps += 1;
            assert!(steps < 10_000, "compaction is not converging");
        }
        (steps, bytes)
    }

    #[test]
    fn allocations_bump_upward_then_reuse_the_gap_a_free_leaves() {
        let mut h = Heap::new();
        assert_eq!(h.alloc(fixed(1), 10).unwrap(), 0);
        assert_eq!(h.alloc(fixed(2), 20).unwrap(), 10);
        assert_eq!(h.alloc(fixed(3), 10).unwrap(), 30);
        assert_eq!(h.len(), 40);
        assert_eq!(h.live_bytes(), 40);
        h.assert_invariants();

        h.free(fixed(2)).unwrap();
        assert_eq!(h.implied_gaps(), vec![(10, 20)]);
        assert_eq!(h.len(), 40); // the top allocation still pins `end`
        assert_eq!(h.live_bytes(), 20);
        h.assert_invariants();

        // A 20-byte request takes the exact fit rather than the top.
        assert_eq!(h.alloc(fixed(4), 20).unwrap(), 10);
        assert!(h.implied_gaps().is_empty());
        h.assert_invariants();
    }

    #[test]
    fn freeing_the_top_allocation_retreats_end() {
        let mut h = Heap::new();
        h.alloc(fixed(1), 10).unwrap();
        h.alloc(fixed(2), 10).unwrap();
        h.free(fixed(2)).unwrap();
        assert_eq!(h.len(), 10, "end should follow the highest allocation down");
        assert!(h.implied_gaps().is_empty());
        h.assert_invariants();
    }

    #[test]
    fn freeing_between_two_gaps_coalesces_all_three() {
        let mut h = Heap::new();
        for i in 1..=5 {
            h.alloc(fixed(i), 10).unwrap();
        }
        h.free(fixed(2)).unwrap(); // gap 10..20
        h.free(fixed(4)).unwrap(); // gap 30..40
        assert_eq!(h.implied_gaps(), vec![(10, 10), (30, 10)]);
        h.free(fixed(3)).unwrap(); // the plug between them
        assert_eq!(h.implied_gaps(), vec![(10, 30)]);
        h.assert_invariants();
    }

    #[test]
    fn unknown_and_duplicate_ids_are_rejected() {
        let mut h = Heap::new();
        h.alloc(fixed(1), 4).unwrap();
        assert_eq!(h.alloc(fixed(1), 4), Err(HeapError::DuplicateId));
        assert_eq!(h.free(fixed(9)), Err(HeapError::UnknownId));
        assert_eq!(h.resize(fixed(9), 8), Err(HeapError::UnknownId));
    }

    #[test]
    fn resize_grows_in_place_when_the_space_above_is_free() {
        let mut h = Heap::new();
        let p = resizable(1);
        h.alloc(p, 10).unwrap();
        h.alloc(fixed(2), 10).unwrap();
        h.free(fixed(2)).unwrap();

        assert_eq!(h.resize(p, 30).unwrap(), None, "should grow in place");
        assert_eq!(h.lookup(p), Some((0, 30)));
        h.assert_invariants();
    }

    #[test]
    fn resize_relocates_when_blocked_and_reports_both_addresses() {
        let mut h = Heap::new();
        let p = resizable(1);
        h.alloc(p, 10).unwrap();
        h.alloc(fixed(2), 10).unwrap(); // blocks growth at 10

        let moved = h.resize(p, 30).unwrap();
        assert_eq!(moved, Some((0, 20)), "must move above the blocker");
        assert_eq!(h.lookup(p), Some((20, 30)));
        assert_eq!(h.implied_gaps(), vec![(0, 10)]);
        h.assert_invariants();
    }

    #[test]
    fn resize_shrinking_keeps_the_address_and_frees_the_tail() {
        let mut h = Heap::new();
        let p = resizable(1);
        h.alloc(p, 30).unwrap();
        h.alloc(fixed(2), 10).unwrap();

        assert_eq!(h.resize(p, 10).unwrap(), None);
        assert_eq!(h.lookup(p), Some((0, 10)));
        assert_eq!(h.implied_gaps(), vec![(10, 20)]);
        h.assert_invariants();
    }

    #[test]
    fn placement_takes_the_lowest_fitting_gap_over_a_higher_exact_one() {
        // A double-width gap low down and an exact-width gap far above it.
        let plan = [
            (1, 20, true),  // gap    0..20   <- wide, but 20 bytes lower
            (2, 80, false), //        20..100
            (3, 10, true),  // gap  100..110  <- an exact fit for a 10-byte request
            (4, 80, false), //       110..190
        ];
        let mut h = heap_with_layout(&plan);

        // Only the potential counts, and 0 is lower than 100. (Preferring the
        // exact fit is what stage 4's μ₁ will buy; nothing prices it yet.)
        assert_eq!(h.alloc(fixed(5), 10).unwrap(), 0);
        h.assert_invariants();
    }

    #[test]
    fn a_relocating_resize_lands_in_the_lowest_fitting_gap() {
        // `resize` only ever runs on resizable allocations, so a relocation must
        // never be diverted into a higher exact fit.
        let mut h = Heap::new();
        let p = resizable(5);
        h.alloc(fixed(1), 20).unwrap(); //     0..20
        h.alloc(fixed(2), 80).unwrap(); //    20..100
        h.alloc(fixed(3), 15).unwrap(); //   100..115
        h.alloc(fixed(4), 80).unwrap(); //   115..195
        h.alloc(p, 5).unwrap(); //           195..200
        h.alloc(fixed(6), 80).unwrap(); //   200..280, walls `p` in from above
        h.free(fixed(1)).unwrap(); // gap      0..20  <- lowest that fits 15
        h.free(fixed(3)).unwrap(); // gap    100..115 <- an exact fit for 15
        h.assert_invariants();

        let moved = h.resize(p, 15).unwrap();
        assert_eq!(
            moved,
            Some((195, 0)),
            "must fall to the lowest fitting gap, not rise into the exact one"
        );
        h.assert_invariants();
    }

    #[test]
    fn placement_extends_the_heap_only_when_no_gap_fits() {
        let plan = [
            (1, 8, true),   // gap  0..8
            (2, 16, false), //      8..24
        ];
        let mut h = heap_with_layout(&plan);
        assert_eq!(h.alloc(fixed(3), 9).unwrap(), 24, "9 does not fit in 8");
        assert_eq!(h.alloc(fixed(4), 8).unwrap(), 0, "8 does");
        h.assert_invariants();
    }

    #[test]
    fn a_gapless_heap_proposes_nothing() {
        let mut h = Heap::new();
        h.alloc(fixed(1), 10).unwrap();
        h.alloc(fixed(2), 10).unwrap();
        assert_eq!(h.propose_compaction_step(UNBOUNDED), None);
        assert_eq!(h.len(), h.live_bytes());
    }

    #[test]
    fn a_slide_moves_a_whole_contiguous_run_as_one_step() {
        let mut h = Heap::new();
        h.alloc(resizable(1), 10).unwrap(); // 0..10, freed below
        h.alloc(resizable(2), 20).unwrap(); // 10..30
        h.alloc(resizable(3), 30).unwrap(); // 30..60
        h.free(resizable(1)).unwrap(); // gap 0..10

        // Resizable allocations generate no evacuation candidates, so the only
        // candidate is the slide -- and it takes both of them at once.
        let step = h.propose_compaction_step(UNBOUNDED).unwrap();
        assert_eq!(
            step,
            Step {
                from: 10,
                to: 0,
                len: 50
            }
        );
        h.commit_compaction_step(step);
        assert_eq!(h.lookup(resizable(2)), Some((0, 20)));
        assert_eq!(h.lookup(resizable(3)), Some((20, 30)));
        assert_eq!(h.len(), 50);
        assert_eq!(h.len(), h.live_bytes(), "one slide compacted it fully");
        h.assert_invariants();
    }

    #[test]
    fn a_slide_stops_at_the_gap_that_ends_the_run() {
        let mut h = Heap::new();
        h.alloc(resizable(1), 10).unwrap(); // 0..10, freed
        h.alloc(resizable(2), 20).unwrap(); // 10..30
        h.alloc(resizable(3), 10).unwrap(); // 30..40, freed
        h.alloc(resizable(4), 20).unwrap(); // 40..60
        h.free(resizable(1)).unwrap();
        h.free(resizable(3)).unwrap();

        // Both gaps are 10 wide; the run above the chosen one is just allocation 2.
        let step = h.propose_compaction_step(UNBOUNDED).unwrap();
        assert_eq!(
            step,
            Step {
                from: 10,
                to: 0,
                len: 20
            },
            "must not span the gap at 30"
        );
        h.commit_compaction_step(step);
        h.assert_invariants();
    }

    #[test]
    fn a_budget_takes_a_prefix_of_the_run_and_the_rest_follows_next_step() {
        let mut h = Heap::new();
        h.alloc(resizable(1), 10).unwrap(); // 0..10, freed
        h.alloc(resizable(2), 20).unwrap(); // 10..30
        h.alloc(resizable(3), 20).unwrap(); // 30..50
        h.free(resizable(1)).unwrap();

        let step = h.propose_compaction_step(25).unwrap();
        assert_eq!(
            step,
            Step {
                from: 10,
                to: 0,
                len: 20
            },
            "budget stops after one"
        );
        h.commit_compaction_step(step);
        h.assert_invariants();

        // The gap simply moved up; the next step carries the rest.
        let step = h.propose_compaction_step(25).unwrap();
        assert_eq!(
            step,
            Step {
                from: 30,
                to: 20,
                len: 20
            }
        );
        h.commit_compaction_step(step);
        assert_eq!(h.len(), h.live_bytes());
        h.assert_invariants();
    }

    #[test]
    fn an_over_budget_step_is_still_proposed_when_nothing_cheaper_is_worth_doing() {
        let mut h = Heap::new();
        h.alloc(resizable(1), 10).unwrap(); // 0..10, freed
        h.alloc(resizable(2), 100).unwrap(); // 10..110
        h.free(resizable(1)).unwrap();

        // Budget 5 cannot cover the 100-byte allocation, but reporting quiescence
        // would strand the gap forever, so the step comes back anyway.
        let step = h.propose_compaction_step(5).unwrap();
        assert_eq!(
            step,
            Step {
                from: 10,
                to: 0,
                len: 100
            }
        );
        assert!(step.len > 5, "the caller is the one who gets to say no");
    }

    /// Lay out an exact address map by allocating wall-to-wall, then freeing the
    /// entries marked as spacers. `(id, size, is_spacer)`.
    fn heap_of(plan: &[(Pointer<u32>, u32, bool)]) -> Heap {
        let mut h = Heap::new();
        for &(id, size, _) in plan {
            h.alloc(id, size).unwrap();
        }
        for &(id, _, spacer) in plan {
            if spacer {
                h.free(id).unwrap();
            }
        }
        h.assert_invariants();
        h
    }

    /// [`heap_of`] with every allocation fixed-size. `(counter, size, is_spacer)`.
    fn heap_with_layout(plan: &[(u32, u32, bool)]) -> Heap {
        let owned: Vec<_> = plan
            .iter()
            .map(|&(id, size, spacer)| (fixed(id), size, spacer))
            .collect();
        heap_of(&owned)
    }

    /// How often the workload below pauses to compact, and how much it is
    /// allowed to move when it does.
    ///
    /// Chosen so that a burst does *several* steps but does not reach
    /// quiescence: a single step per pause would not exercise the search in the
    /// state a burst actually leaves behind, and a burst that compacts fully
    /// would make the "incremental" in the algorithm moot. See
    /// `test-results/README.md` for the measured steps-per-burst this yields.
    ///
    /// The interval divides every round count measured here, and a burst fires
    /// at the *end* of each interval, so a run always stops immediately after
    /// one. That keeps every measurement at the same phase of the burst cycle --
    /// otherwise the larger runs would be sampled with more un-compacted churn
    /// on them than the smaller ones, purely as an artefact of where the loop
    /// happened to end.
    const COMPACTION_INTERVAL: usize = 25;
    const COMPACTION_BUDGET: u64 = 2048;

    /// How much overhead over the live bytes counts as "compact enough".
    ///
    /// Driving to a literally gapless heap is not a target any caller has: the
    /// last fraction of a percent is dominated by narrow gaps bubbling to the
    /// top one run at a time, which is quadratic in heap size and is exactly the
    /// work a budget would never buy. Measuring to a threshold keeps the figure
    /// about the healthy state rather than about the endgame.
    const COMPACT_ENOUGH_PERCENT: u64 = 1;

    /// The heap's shape at one point in the simulation, plus what the bursts
    /// since the previous snapshot cost.
    #[derive(Clone, Copy, Default)]
    struct Snapshot {
        round: usize,
        allocations: usize,
        live_bytes: u64,
        end: u64,
        gaps: usize,
        /// The widest gap. A heap with a few wide gaps absorbs new allocations
        /// without extending `end`; one with many slivers cannot.
        widest_gap: u64,
        /// Mean wall-clock of one burst since the previous snapshot, in
        /// microseconds. Plotted against `allocations` down the table, this is
        /// the scaling curve the compactor is judged on.
        micros_per_burst: f64,
    }

    impl Snapshot {
        fn take(h: &Heap, round: usize) -> Self {
            Self {
                round,
                allocations: h.live_count(),
                live_bytes: h.live_bytes(),
                end: h.len(),
                gaps: h.gaps.len(),
                widest_gap: h.free_by_size.last_key_value().map_or(0, |(&len, _)| len),
                micros_per_burst: 0.0,
            }
        }

        /// Free bytes as a percentage of live bytes.
        fn overhead_percent(&self) -> f64 {
            if self.live_bytes == 0 {
                0.0
            } else {
                100.0 * (self.end - self.live_bytes) as f64 / self.live_bytes as f64
            }
        }
    }

    /// What one burst achieved.
    #[derive(Default, Clone, Copy)]
    struct BurstStats {
        bursts: u64,
        steps: u64,
        quiesced: u64,
    }

    /// The churny workload the measurement below drives, with compaction
    /// bursts interleaved the way a backend flush does it. Returns the live ids
    /// and what the bursts achieved.
    fn run_churny_workload_tracked(
        h: &mut Heap,
        rounds: usize,
    ) -> (Vec<Pointer<u32>>, BurstStats, Vec<Snapshot>) {
        assert!(
            rounds.is_multiple_of(COMPACTION_INTERVAL),
            "the run must end on a burst; see COMPACTION_INTERVAL"
        );
        // Sample the heap shape at ~10 points, always immediately after a burst,
        // so the table shows the state the schedule actually leaves behind.
        let snapshot_every = (rounds / COMPACTION_INTERVAL / 10).max(1);
        let mut snapshots = Vec::new();
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut rand = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut live: Vec<Pointer<u32>> = Vec::new();
        let mut next_counter = 1u32;
        let mut bursts = BurstStats::default();
        // Reset at each snapshot, so a row reports the interval it ends, not the
        // run so far -- otherwise growth would be smeared across the table.
        let mut window = std::time::Duration::ZERO;
        let mut window_bursts = 0u64;

        for round in 0..rounds {
            let roll = rand() % 100;
            if roll < 60 || live.is_empty() {
                // Skewed to a few sizes, as kladde's fixed-size classes would be.
                let size = [8u32, 16, 16, 64, 250][(rand() % 5) as usize];
                let id = if rand() % 4 == 0 {
                    resizable(next_counter)
                } else {
                    fixed(next_counter)
                };
                next_counter += 1;
                h.alloc(id, size).unwrap();
                live.push(id);
            } else if roll < 85 {
                let victim = live.swap_remove((rand() % live.len() as u64) as usize);
                h.free(victim).unwrap();
            } else {
                let i = (rand() % live.len() as u64) as usize;
                if !live[i].is_fixed_size() {
                    let new_size = 1 + (rand() % 300) as u32;
                    h.resize(live[i], new_size).unwrap();
                }
            }

            // Compact the way a backend does: not one step, but a *burst* --
            // `compact_incrementally` spends a whole budget in one call, so a
            // pause does many consecutive steps with no mutation in between.
            if (round + 1) % COMPACTION_INTERVAL == 0 {
                let t0 = std::time::Instant::now();
                let (steps, quiesced) = h.compact_incrementally(COMPACTION_BUDGET);
                window += t0.elapsed();
                window_bursts += 1;
                bursts.bursts += 1;
                bursts.steps += steps;
                bursts.quiesced += u64::from(quiesced);
                if bursts.bursts % snapshot_every as u64 == 0 {
                    let mut snapshot = Snapshot::take(h, round);
                    snapshot.micros_per_burst =
                        window.as_secs_f64() * 1e6 / window_bursts.max(1) as f64;
                    snapshots.push(snapshot);
                    window = std::time::Duration::ZERO;
                    window_bursts = 0;
                }
            }
        }
        (live, bursts, snapshots)
    }

    fn report_search_stats(label: &str, s: SearchStats) {
        println!(
            "  {label:<24} calls {:>7}  proposals {:>7}",
            s.calls, s.proposals,
        );
    }

    /// Which candidate shape the bursts actually committed, and -- the column
    /// that matters -- how many bytes of `end` each one bought.
    ///
    /// Every compaction step *conserves* free space: it takes `len` free bytes
    /// at the destination and gives `len` back where the mover was. The only
    /// exception is a move that vacates the top of the heap, where the freed
    /// bytes end up above `end` and stop counting. So the whole of the
    /// fragmentation result is in `truncated`, and nowhere else.
    fn report_step_shapes(s: SearchStats) {
        let row = |label: &str, n: u64, bytes: u64, truncated: u64| {
            println!(
                "  {label:<24} steps {:>7}  bytes {:>10}  truncated {:>10}  ({:>5.1}% of bytes moved)",
                n,
                bytes,
                truncated,
                if bytes == 0 {
                    0.0
                } else {
                    100.0 * truncated as f64 / bytes as f64
                },
            );
        };
        row("slides", s.slides, s.slide_bytes, s.truncated_by_slides);
        row(
            "evacuations",
            s.evacuations,
            s.evac_bytes,
            s.truncated_by_evacuations,
        );
        let placements = s.placed_in_gap + s.placed_at_end;
        println!(
            "  {:<24} in a gap {:>7} ({:>5.1}%, {:>9} bytes)   extending end {:>7} ({:>9} bytes)",
            "placements",
            s.placed_in_gap,
            if placements == 0 {
                0.0
            } else {
                100.0 * s.placed_in_gap as f64 / placements as f64
            },
            s.placed_in_gap_bytes,
            s.placed_at_end,
            s.placed_at_end_bytes,
        );
    }

    /// Not an assertion of behaviour -- a **measurement**, of how much work the
    /// candidate search does over a realistic churn. That is what decides
    /// whether the search needs bounding at all, and the answer is recorded in
    /// `test-results/`.
    ///
    /// Two regimes are reported separately, because they behave very
    /// differently: compaction in **bursts** interleaved with churn (what a
    /// backend does on each flush), and compaction driven to quiescence.
    ///
    /// `#[ignore]`d because the largest case takes minutes -- it is a
    /// measurement, not part of the suite. Run with:
    ///
    /// ```text
    /// cargo test -p kladde-heap --lib candidate_search_cost -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "measurement, not a behavioural test; takes minutes"]
    fn candidate_search_cost_over_a_churny_workload() {
        for &rounds in &[400usize, 4_000, 40_000] {
            measure_one(rounds);
        }
    }

    fn measure_one(rounds: usize) {
        let mut h = Heap::new();
        let (_, bursts, snapshots) = run_churny_workload_tracked(&mut h, rounds);
        let during_bursts = h.search_stats();

        // Then catch up: compaction with no churn competing, until the file is
        // within `COMPACT_ENOUGH_PERCENT` of the live bytes. Not to a gapless
        // heap -- see the constant. Still one burst at a time, so the schedule
        // matches production's.
        h.reset_search_stats();
        let target = h.live_bytes() + h.live_bytes() * COMPACT_ENOUGH_PERCENT / 100;
        let before = Snapshot::take(&h, rounds);
        let mut steps = 0u64;
        while h.len() > target {
            let (took, quiesced) = h.compact_incrementally(4096);
            steps += took;
            if quiesced || took == 0 {
                break;
            }
        }
        let catching_up = h.search_stats();
        let after = Snapshot::take(&h, rounds);

        println!(
            "\n=== {rounds} rounds -> {} live allocations ===",
            after.allocations
        );
        println!(
            "  bursts: {} of budget {COMPACTION_BUDGET} every {COMPACTION_INTERVAL} ops; \
             {:.1} steps each; {} of them ran out of work",
            bursts.bursts,
            bursts.steps as f64 / bursts.bursts.max(1) as f64,
            bursts.quiesced,
        );
        println!(
            "  catch-up to <={COMPACT_ENOUGH_PERCENT}% overhead: {steps} steps, \
             {:.2}% -> {:.2}% overhead, {} -> {} gaps ({:.4} steps per allocation)",
            before.overhead_percent(),
            after.overhead_percent(),
            before.gaps,
            after.gaps,
            steps as f64 / after.allocations.max(1) as f64,
        );
        report_search_stats("during bursts", during_bursts);
        report_search_stats("catching up", catching_up);
        report_step_shapes(during_bursts);

        println!(
            "  {:>8}  {:>12}  {:>12}  {:>12}  {:>8}  {:>8}  {:>9}  {:>11}",
            "round", "allocations", "live_bytes", "end", "gaps", "widest", "overhead", "us/burst"
        );
        for s in &snapshots {
            println!(
                "  {:>8}  {:>12}  {:>12}  {:>12}  {:>8}  {:>8}  {:>8.2}%  {:>11.1}",
                s.round,
                s.allocations,
                s.live_bytes,
                s.end,
                s.gaps,
                s.widest_gap,
                s.overhead_percent(),
                s.micros_per_burst,
            );
        }
    }

    #[test]
    fn compaction_converges_from_a_randomized_workload() {
        // A tiny xorshift keeps this deterministic without a dev-dependency.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut rand = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        let mut h = Heap::new();
        let mut live: Vec<Pointer<u32>> = Vec::new();
        let mut next_counter = 1u32;

        for round in 0..400 {
            let roll = rand() % 100;
            if roll < 60 || live.is_empty() {
                // Skewed to a few sizes, as kladde's fixed-size classes would be.
                let size = [8u32, 16, 16, 64, 250][(rand() % 5) as usize];
                let id = if rand() % 4 == 0 {
                    resizable(next_counter)
                } else {
                    fixed(next_counter)
                };
                next_counter += 1;
                h.alloc(id, size).unwrap();
                live.push(id);
            } else if roll < 85 {
                let victim = live.swap_remove((rand() % live.len() as u64) as usize);
                h.free(victim).unwrap();
            } else {
                let i = (rand() % live.len() as u64) as usize;
                if !live[i].is_fixed_size() {
                    let new_size = 1 + (rand() % 300) as u32;
                    h.resize(live[i], new_size).unwrap();
                }
            }
            h.assert_invariants();

            // Interleave bounded compaction with the workload, as a backend would.
            if round % 7 == 0 {
                if let Some(step) = h.propose_compaction_step(128) {
                    h.commit_compaction_step(step);
                    h.assert_invariants();
                }
            }
        }

        compact_fully(&mut h, 128);
        assert_eq!(h.len(), h.live_bytes(), "quiescence must mean gapless");
        assert_eq!(h.live_count(), live.len());
        for id in live {
            assert!(h.lookup(id).is_some(), "an allocation went missing");
        }
    }
}
