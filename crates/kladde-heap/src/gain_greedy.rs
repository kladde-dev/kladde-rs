//! [`GainGreedyHeap`]: the concrete [`RelocatableHeap`] of
//! `incremental-compaction.md` §4 -- one address-keyed map of allocations, three
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
//! `free_by_size` and `live_by_size` are derived indexes maintained by the same
//! two primitives every mutation goes through (`insert_raw`/`remove_raw`), which
//! is what keeps the "which move is best" question answerable by a query rather
//! than a scan.
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
//! Two candidate shapes are generated (§4):
//!
//! - **Evacuation**, one per fixed-size class: its highest-addressed member
//!   jumps into the lowest gap that fits it.
//! - **Slide**, one overall: the maximal contiguous *run* above the largest gap
//!   shifts down into it. This is what moves resizable allocations, which are
//!   deliberately not indexed as movers.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::heap::{
    AllocationId, HeapError, IncrementallyCompactableHeap, RelocatableHeap, Relocation, Step,
};

/// One row of the address-keyed table.
#[derive(Clone, Copy)]
struct Entry<Id> {
    len: u32,
    id: Id,
}

/// A relocatable heap over a `u64` address space with `u32` allocation sizes,
/// compacting by gain-greedy incremental steps. See the module docs.
pub struct GainGreedyHeap<Id> {
    /// Start address -> allocation. Gaps are the space between consecutive
    /// entries; `end` is the last entry's end.
    allocations: BTreeMap<u64, Entry<Id>>,
    /// The id table's address column (P1: held once, here).
    by_id: HashMap<Id, u64>,
    /// Gap length -> the start addresses of gaps that long.
    free_by_size: BTreeMap<u64, BTreeSet<u64>>,
    /// Allocation size -> addresses, **fixed-size allocations only**. Resizable
    /// ones would scatter one-per-class and would only have to move again on the
    /// next growth; they stay movable by slides.
    live_by_size: BTreeMap<u32, BTreeSet<u64>>,
    /// One past the highest live byte.
    end: u64,
    /// Sum of all live allocation sizes.
    live_bytes: u64,
}

impl<Id> Default for GainGreedyHeap<Id> {
    fn default() -> Self {
        Self {
            allocations: BTreeMap::new(),
            by_id: HashMap::new(),
            free_by_size: BTreeMap::new(),
            live_by_size: BTreeMap::new(),
            end: 0,
            live_bytes: 0,
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

    // ---- gap index maintenance ----

    fn gap_record(&mut self, start: u64, len: u64) {
        if len > 0 {
            self.free_by_size.entry(len).or_default().insert(start);
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
        if id.is_fixed_size() {
            self.live_by_size.entry(len).or_default().insert(addr);
        }
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
        if e.id.is_fixed_size() {
            if let Some(set) = self.live_by_size.get_mut(&e.len) {
                set.remove(&addr);
                if set.is_empty() {
                    self.live_by_size.remove(&e.len);
                }
            }
        }
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

    /// The lowest-addressed gap at least `min_len` bytes wide.
    fn lowest_gap_fitting(&self, min_len: u64) -> Option<(u64, u64)> {
        self.free_by_size
            .range(min_len..)
            .filter_map(|(&len, set)| set.first().map(|&start| (start, len)))
            .min_by_key(|&(start, _)| start)
    }

    /// Where to put a new `size`-byte allocation: an exactly-fitting gap if one
    /// exists (it leaves no sliver), else the lowest gap that fits, else the top.
    fn place(&self, size: u32) -> Result<u64, HeapError> {
        let want = size as u64;
        if let Some(set) = self.free_by_size.get(&want) {
            if let Some(&start) = set.first() {
                return Ok(start);
            }
        }
        if let Some((start, _)) = self.lowest_gap_fitting(want) {
            return Ok(start);
        }
        self.end
            .checked_add(want)
            .map(|_| self.end)
            .ok_or(HeapError::OutOfMemory)
    }

    // ---- compaction candidates ----

    /// The maximal run of contiguous allocations starting at `start`, truncated
    /// to a prefix costing at most `budget` (but always at least one
    /// allocation). Returns the total byte length.
    fn run_len_from(&self, start: u64, budget: u64) -> u64 {
        let mut cursor = start;
        let mut taken = 0u64;
        for (&addr, e) in self.allocations.range(start..) {
            if addr != cursor {
                break; // hit a gap: the run ends here
            }
            let next = taken + e.len as u64;
            if taken > 0 && next > budget {
                break; // stay within budget, having taken at least one
            }
            taken = next;
            cursor = addr + e.len as u64;
        }
        taken
    }

    /// Evacuation candidates: one per fixed-size class, its highest member
    /// jumping into the lowest gap that fits.
    fn evacuation_candidates(&self) -> impl Iterator<Item = (u64, Step<u64>)> + '_ {
        self.live_by_size.iter().filter_map(|(&size, members)| {
            let from = *members.last()?;
            let (to, _) = self.lowest_gap_fitting(size as u64)?;
            // `then`, not `then_some`: the latter would evaluate `from - to`
            // even when the gap sits *above* the mover, underflowing.
            (to < from).then(|| {
                (
                    from - to,
                    Step {
                        from,
                        to,
                        len: size as u64,
                    },
                )
            })
        })
    }

    /// The slide candidate: the run above the largest gap, shifting down into it.
    /// The only candidate shape that moves resizable allocations.
    fn slide_candidate(&self, budget: u64) -> Option<(u64, Step<u64>)> {
        let (&gap_len, starts) = self.free_by_size.last_key_value()?;
        let to = *starts.first()?;
        let from = to + gap_len;
        let len = self.run_len_from(from, budget);
        (len > 0).then_some((gap_len, Step { from, to, len }))
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

    fn propose_compaction_step(&self, budget: u64) -> Option<Step<u64>> {
        if self.free_by_size.is_empty() {
            return None; // gapless: compact
        }
        let candidates = self
            .evacuation_candidates()
            .chain(self.slide_candidate(budget));

        // Prefer a step within budget; fall back to the best over-budget one
        // rather than claiming quiescence with work still to do.
        let mut best_within: Option<(u64, Step<u64>)> = None;
        let mut best_overall: Option<(u64, Step<u64>)> = None;
        for (gain, step) in candidates {
            if gain == 0 {
                continue;
            }
            if step.len <= budget && best_within.is_none_or(|(g, _)| gain > g) {
                best_within = Some((gain, step));
            }
            if best_overall.is_none_or(|(g, _)| gain > g) {
                best_overall = Some((gain, step));
            }
        }
        best_within.or(best_overall).map(|(_, step)| step)
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

        let mut expected_free: BTreeMap<u64, BTreeSet<u64>> = BTreeMap::new();
        for (start, len) in self.implied_gaps() {
            expected_free.entry(len).or_default().insert(start);
        }
        assert_eq!(self.free_by_size, expected_free, "free_by_size drifted");

        let mut expected_live: BTreeMap<u32, BTreeSet<u64>> = BTreeMap::new();
        for (&addr, e) in &self.allocations {
            if e.id.is_fixed_size() {
                expected_live.entry(e.len).or_default().insert(addr);
            }
        }
        assert_eq!(self.live_by_size, expected_live, "live_by_size drifted");
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
    fn a_gapless_heap_proposes_nothing() {
        let mut h = Heap::new();
        h.alloc(fixed(1), 10).unwrap();
        h.alloc(fixed(2), 10).unwrap();
        assert_eq!(h.propose_compaction_step(UNBOUNDED), None);
        assert_eq!(h.len(), h.live_bytes());
    }

    #[test]
    fn evacuation_beats_sliding_when_a_high_allocation_can_jump_far_down() {
        let mut h = Heap::new();
        h.alloc(fixed(1), 10).unwrap(); // 0..10
        h.alloc(fixed(2), 10).unwrap(); // 10..20  (freed below)
        h.alloc(resizable(3), 100).unwrap(); // 20..120
        h.alloc(fixed(4), 10).unwrap(); // 120..130
        h.free(fixed(2)).unwrap(); // gap 10..20

        // The 10-byte class's highest member is at 120; the lowest gap that fits
        // it is at 10, so the gain is 110 -- far better than sliding the run
        // above the gap down by 10.
        let step = h.propose_compaction_step(UNBOUNDED).unwrap();
        assert_eq!(
            step,
            Step {
                from: 120,
                to: 10,
                len: 10
            }
        );
        h.commit_compaction_step(step);
        assert_eq!(h.lookup(fixed(4)), Some((10, 10)));
        assert_eq!(h.len(), 120);
        h.assert_invariants();
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

    /// The worked example of `incremental-compaction.md` §4: distance-greed
    /// finds the cheap interior moves that a truncation-greedy policy misses,
    /// compacting the file in 130 bytes where pure sliding would copy 1460.
    #[test]
    fn the_worked_example_compacts_without_lookahead() {
        let mut h = Heap::new();
        // Lay out E1..E5 with gaps of 20, 90, 90, 100 between them by allocating
        // wall-to-wall and then freeing the spacers.
        let plan: [(u32, u32, bool); 9] = [
            (1, 1000, false), // E1  0..1000
            (2, 20, true),    // gap 1000..1020
            (3, 980, false),  // E2  1020..2000
            (4, 100, true),   // gap 2000..2100
            (5, 10, false),   // E3  2100..2110
            (6, 90, true),    // gap 2110..2200
            (7, 10, false),   // E4  2200..2210
            (8, 100, true),   // gap 2210..2310
            (9, 110, false),  // E5  2310..2420
        ];
        for (id, size, _) in plan {
            h.alloc(fixed(id), size).unwrap();
        }
        for (id, _, spacer) in plan {
            if spacer {
                h.free(fixed(id)).unwrap();
            }
        }
        assert_eq!(h.len(), 2420);
        assert_eq!(h.live_bytes(), 2110);
        h.assert_invariants();

        let (_, bytes) = compact_fully(&mut h, UNBOUNDED);
        assert_eq!(h.len(), 2110, "fully compact");
        assert_eq!(h.len(), h.live_bytes());
        assert!(
            bytes <= 200,
            "distance-greed should move ~130 bytes, not slide 1460; moved {bytes}"
        );
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
