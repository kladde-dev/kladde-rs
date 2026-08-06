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

/// The best candidate seen so far, on two tracks.
///
/// `budget` is a ranking input rather than a cap, so a step that exceeds it is
/// still worth remembering: without it a heap whose only useful move is one
/// oversized slide would report quiescence and never compact. But a step that
/// fits is always preferred, however much less it gains.
#[derive(Default, Clone, Copy)]
struct Best {
    within: Option<(u64, Step<u64>)>,
    overall: Option<(u64, Step<u64>)>,
}

impl Best {
    fn offer(&mut self, gain: u64, step: Step<u64>, budget: u64) {
        if gain == 0 {
            return;
        }
        if step.len <= budget && self.within.is_none_or(|(g, _)| gain > g) {
            self.within = Some((gain, step));
        }
        if self.overall.is_none_or(|(g, _)| gain > g) {
            self.overall = Some((gain, step));
        }
    }

    /// The gain a further candidate must beat to change the outcome.
    ///
    /// This is the *within-budget* gain, not the overall one, and deliberately
    /// so: a class that cannot beat the within-budget best cannot beat the
    /// overall best either (`within <= overall`), so pruning on it is sound for
    /// both tracks -- whereas pruning on the overall gain could discard a
    /// cheaper candidate that would actually have been chosen. Before any
    /// within-budget candidate is found the bound is 0 and nothing is pruned,
    /// which is no worse than the exhaustive scan this replaces.
    fn bound(&self) -> u64 {
        self.within.map_or(0, |(gain, _)| gain)
    }

    fn pick(self) -> Option<Step<u64>> {
        self.within.or(self.overall).map(|(_, step)| step)
    }
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
    /// Each class's highest member address -> its size class. The search key for
    /// the branch-and-bound candidate walk: unlike a gain, a class's top changes
    /// only when that class's own members do, never when a `free` elsewhere
    /// opens a deep gap. See [`GainGreedyHeap::propose_compaction_step`].
    tops: BTreeMap<u64, u32>,
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
            tops: BTreeMap::new(),
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

    // ---- class-top index (the branch-and-bound key) ----

    /// The highest-addressed member of size class `size`, if any.
    fn class_top(&self, size: u32) -> Option<u64> {
        self.live_by_size.get(&size)?.last().copied()
    }

    /// Re-key `tops` for `size` after its membership changed. `was` is the
    /// class's top *before* the change.
    fn refresh_top(&mut self, size: u32, was: Option<u64>) {
        if let Some(old) = was {
            self.tops.remove(&old);
        }
        if let Some(now) = self.class_top(size) {
            self.tops.insert(now, size);
        }
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
            let was = self.class_top(len);
            self.live_by_size.entry(len).or_default().insert(addr);
            self.refresh_top(len, was);
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
            let was = self.class_top(e.len);
            if let Some(set) = self.live_by_size.get_mut(&e.len) {
                set.remove(&addr);
                if set.is_empty() {
                    self.live_by_size.remove(&e.len);
                }
            }
            self.refresh_top(e.len, was);
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

    /// The evacuation candidate for size class `size`: its highest member
    /// jumping into the lowest gap that fits.
    fn evacuation_candidate(&self, size: u32, from: u64) -> Option<(u64, Step<u64>)> {
        let (to, _) = self.lowest_gap_fitting(size as u64)?;
        // `then`, not `then_some`: the latter would evaluate `from - to` even
        // when the gap sits *above* the mover, underflowing.
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

    /// Pick the highest-gain move, **without** evaluating every size class.
    ///
    /// A naive priority queue over classes keyed by last-computed gain would be
    /// subtly wrong: when a `free` opens a deep gap, the true gains of every
    /// class small enough to fit it rise at once, and a max-queue can sit on a
    /// stale-low key forever. The fix is to key by something *class-locally*
    /// maintainable. A mover's gain is `top - dest <= top`, since `dest >= 0`,
    /// and `top(s)` changes only when class `s`'s own members change -- never
    /// through gap events. So walking `tops` downward, the walk can stop the
    /// moment the best gain so far reaches the next class's top: no unvisited
    /// class can beat it.
    ///
    /// The slide is evaluated first, outside the walk, because it is a single
    /// O(log n) lookup and usually seeds a bound straight away.
    fn propose_compaction_step(&self, budget: u64) -> Option<Step<u64>> {
        if self.free_by_size.is_empty() {
            return None; // gapless: compact
        }
        let mut best = Best::default();
        if let Some((gain, step)) = self.slide_candidate(budget) {
            best.offer(gain, step, budget);
        }
        for (&top, &size) in self.tops.iter().rev() {
            if top <= best.bound() {
                break; // gain <= top, so nothing further down can win
            }
            if let Some((gain, step)) = self.evacuation_candidate(size, top) {
                best.offer(gain, step, budget);
            }
        }
        best.pick()
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

    /// The reference implementation the branch-and-bound walk must agree with:
    /// evaluate every size class, prune nothing.
    fn propose_by_exhaustive_scan(&self, budget: u64) -> Option<Step<u64>> {
        if self.free_by_size.is_empty() {
            return None;
        }
        let mut best = Best::default();
        if let Some((gain, step)) = self.slide_candidate(budget) {
            best.offer(gain, step, budget);
        }
        for (&size, members) in &self.live_by_size {
            let top = *members.last().expect("no empty classes");
            if let Some((gain, step)) = self.evacuation_candidate(size, top) {
                best.offer(gain, step, budget);
            }
        }
        best.pick()
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

        let expected_tops: BTreeMap<u64, u32> = expected_live
            .iter()
            .map(|(&size, members)| (*members.last().expect("no empty classes"), size))
            .collect();
        assert_eq!(self.tops, expected_tops, "tops drifted");
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

    #[test]
    fn the_bounded_walk_agrees_with_an_exhaustive_scan() {
        // The branch-and-bound is meant to be a pure optimization, so check it
        // against the scan it replaces over a churny workload.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut rand = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut h = Heap::new();
        let mut live: Vec<Pointer<u32>> = Vec::new();
        let mut counter = 1u32;

        for _ in 0..600 {
            if rand() % 100 < 55 || live.is_empty() {
                let size = [4u32, 12, 12, 40, 96, 300][(rand() % 6) as usize];
                let id = if rand() % 5 == 0 {
                    resizable(counter)
                } else {
                    fixed(counter)
                };
                counter += 1;
                h.alloc(id, size).unwrap();
                live.push(id);
            } else {
                let victim = live.swap_remove((rand() % live.len() as u64) as usize);
                h.free(victim).unwrap();
            }

            for budget in [1u64, 16, 64, u64::MAX] {
                assert_eq!(
                    h.propose_compaction_step(budget),
                    h.propose_by_exhaustive_scan(budget),
                    "bounded walk disagreed with the exhaustive scan at budget {budget}"
                );
            }
            if let Some(step) = h.propose_compaction_step(64) {
                h.commit_compaction_step(step);
                h.assert_invariants();
            }
        }
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
