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
//! - **Evacuation**: an allocation jumps into a gap below it. Fixed-size ones
//!   are grouped into size classes and contribute their highest member (per
//!   neighbour category); resizable ones have one-off sizes, so each is its own
//!   candidate and earns no exact-fit bonus -- a snug fit would only re-open on
//!   its next growth.
//! - **Slide**, one overall: the maximal contiguous *run* above the largest gap
//!   shifts down into it. This is what guarantees progress when nothing fits.
//!
//! The best candidate is found by a bounded walk rather than a full scan -- see
//! [`GainGreedyHeap::propose_compaction_step`] for why the search key is an
//! address and not a gain.
//!
//! [`GainGreedyHeap::alpha`] optionally adds a fragmentation term, which is the
//! only thing that prices the *shape* of the free space rather than just how far
//! bytes travel. It ships at zero.

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

/// Which of an allocation's neighbours are free -- the only thing that decides
/// what vacating it does to the *gap count*, and so the only source triage the
/// `α` term of [`GainGreedyHeap::alpha`] needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FreeNeighbours {
    /// Free on both sides (or free below and at the top of the heap): vacating
    /// merges two gaps into one, or lets `end` retreat. The "plug" case.
    Both,
    /// Free on exactly one side: the gap just extends.
    One,
    /// Live on both sides: vacating mints a brand new gap.
    Neither,
}

impl FreeNeighbours {
    const COUNT: usize = 3;

    /// The change in the number of gaps when this allocation is vacated.
    fn r_src(self) -> i128 {
        match self {
            FreeNeighbours::Both => 1,
            FreeNeighbours::One => 0,
            FreeNeighbours::Neither => -1,
        }
    }

    fn index(self) -> usize {
        match self {
            FreeNeighbours::Both => 0,
            FreeNeighbours::One => 1,
            FreeNeighbours::Neither => 2,
        }
    }
}

/// A candidate's per-byte gain, as the exact rational `num / den`.
///
/// With `α = 0` this is just the travel distance, but the fragmentation term
/// makes it `d + α·r/s`, which is not an integer -- and the denominators differ
/// between candidates, so comparison has to cross-multiply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Gain {
    num: i128,
    den: i128,
}

impl Gain {
    /// Per-byte gain of moving `size` bytes down by `distance`, where the move
    /// changes the gap count by `r`.
    fn new(distance: u64, size: u64, r: i128, alpha: u64) -> Self {
        Self {
            num: (distance as i128) * (size as i128) + (alpha as i128) * r,
            den: size as i128,
        }
    }

    fn is_positive(self) -> bool {
        self.num > 0
    }
}

impl Ord for Gain {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Denominators are allocation sizes, hence strictly positive, so the
        // cross-multiplied comparison keeps its direction. `saturating_mul`
        // only ever bites at heap sizes far past anything realistic, and never
        // at the shipped `α = 0` (where this reduces to comparing distances).
        self.num
            .saturating_mul(other.den)
            .cmp(&other.num.saturating_mul(self.den))
    }
}
impl PartialOrd for Gain {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The best candidate seen so far, on two tracks.
///
/// `budget` is a ranking input rather than a cap, so a step that exceeds it is
/// still worth remembering: without it a heap whose only useful move is one
/// oversized slide would report quiescence and never compact. But a step that
/// fits is always preferred, however much less it gains.
#[derive(Default, Clone, Copy)]
struct Best {
    within: Option<(Gain, Step<u64>)>,
    overall: Option<(Gain, Step<u64>)>,
}

impl Best {
    fn offer(&mut self, gain: Gain, step: Step<u64>, budget: u64) {
        if !gain.is_positive() {
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
    /// which is no worse than an exhaustive scan.
    fn bound(&self) -> Option<Gain> {
        self.within.map(|(gain, _)| gain)
    }

    fn pick(self) -> Option<Step<u64>> {
        self.within.or(self.overall).map(|(_, step)| step)
    }
}

/// Counters for how much work the candidate search does.
///
/// Test-only instrumentation: outside `cfg(test)` none of this exists, `Probe`
/// is a zero-sized type whose methods are empty, and the heap carries no extra
/// field. See `test-results/` for measurements.
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SearchStats {
    /// Calls to `propose_compaction_step`.
    pub calls: u64,
    /// Calls that returned a step (the rest found the heap already compact).
    pub proposals: u64,
    /// Search items examined, summed over all calls.
    pub visited: u64,
    /// Most items examined in any single call.
    pub max_visited: u64,
    /// Items the search *could* have examined, summed over all calls -- the
    /// denominator that says how much the pruning actually saved.
    pub available: u64,
    /// Items examined per call, bucketed by `1, 2, 4, 8, ... , 128+`.
    pub buckets: [u64; 9],
}

#[cfg(test)]
impl SearchStats {
    fn record(&mut self, visited: u64, available: u64, proposed: bool) {
        self.calls += 1;
        self.proposals += u64::from(proposed);
        self.visited += visited;
        self.max_visited = self.max_visited.max(visited);
        self.available += available;
        let bucket = (u64::BITS - visited.leading_zeros()) as usize;
        self.buckets[bucket.min(self.buckets.len() - 1)] += 1;
    }

    /// Mean items examined per call.
    pub fn mean_visited(&self) -> f64 {
        if self.calls == 0 {
            0.0
        } else {
            self.visited as f64 / self.calls as f64
        }
    }

    /// Fraction of the available search space actually examined.
    pub fn examined_fraction(&self) -> f64 {
        if self.available == 0 {
            0.0
        } else {
            self.visited as f64 / self.available as f64
        }
    }
}

/// Counts items examined by one candidate search. A ZST with empty methods
/// outside tests, so the instrumentation costs real workloads nothing.
#[derive(Default)]
struct Probe {
    #[cfg(test)]
    visited: u64,
}

impl Probe {
    #[inline(always)]
    fn visit(&mut self) {
        #[cfg(test)]
        {
            self.visited += 1;
        }
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
    /// Gap length -> the start addresses of gaps that long. Answers *exact*-fit
    /// lookups and "the largest gap"; the by-address view lives in `gaps`.
    free_by_size: BTreeMap<u64, BTreeSet<u64>>,
    /// The same gaps, address-ordered and augmented with each subtree's longest
    /// gap, which is what makes "the lowest gap that fits" a single descent
    /// instead of a scan over size classes. See [`GapTree`].
    gaps: GapTree,
    /// Allocation size -> addresses, **fixed-size allocations only**. Resizable
    /// ones would scatter one-per-class and would only have to move again on the
    /// next growth; they stay movable by slides.
    ///
    /// Split by [`FreeNeighbours`], because the `α` term prices *where a move takes
    /// from*: with it, the best mover in a class is no longer simply its highest
    /// member, so one sub-maximum per neighbour category is needed.
    live_by_size: BTreeMap<u32, [BTreeSet<u64>; FreeNeighbours::COUNT]>,
    /// Addresses of the **resizable** allocations, which are movers too but do
    /// not group usefully by size: their sizes are one-off, so size classes
    /// would degenerate to singletons. Each is instead evaluated individually
    /// when the candidate walk reaches it, with its neighbour category computed
    /// on the spot rather than indexed.
    resizable_by_address: BTreeSet<u64>,
    /// Each class's highest member address -> its size class. The search key for
    /// the branch-and-bound candidate walk: unlike a gain, a class's top changes
    /// only when that class's own members do, never when a `free` elsewhere
    /// opens a deep gap. See [`GainGreedyHeap::propose_compaction_step`].
    tops: BTreeMap<u64, u32>,
    /// One past the highest live byte.
    end: u64,
    /// Sum of all live allocation sizes.
    live_bytes: u64,
    /// Weight of the fragmentation term. See [`GainGreedyHeap::alpha`].
    alpha: u64,
    /// Test-only search instrumentation; absent from real builds.
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
            live_by_size: BTreeMap::new(),
            resizable_by_address: BTreeSet::new(),
            tops: BTreeMap::new(),
            end: 0,
            live_bytes: 0,
            alpha: 0,
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

    /// Weight of the fragmentation term in the compaction potential.
    ///
    /// The base potential `Φ = Σ_{live bytes} address` is blind to how the free
    /// space is *shaped*: it prices a move purely by how far its bytes travel, so
    /// filling a gap exactly and splitting a large gap into a useless sliver
    /// score the same. Adding `α·G`, with `G` the number of gaps, makes a move's
    /// per-byte gain `d + α·r/s`, where `r` is the move's net effect on the gap
    /// count. That prices two things the base potential cannot: extracting a
    /// "plug" between two gaps (which merges them) now beats carving a hole out
    /// of a solid run at equal distance, and an exactly-fitting destination beats
    /// one that leaves a remainder.
    ///
    /// `α` is a policy knob of *this* implementation, so it is a field with an
    /// accessor rather than an argument of any trait method -- the trait models
    /// the capability to compact, not the policy behind it. Ships at `0`, which
    /// makes the gain exactly the travel distance again; raise it when traces
    /// show large gaps being squandered on small far-travelling movers.
    ///
    /// Every gapless layout has `G = 0`, so no value of `α` moves the optimum,
    /// and a full-run slide always has `r >= +1`, so no value of `α` can stall
    /// compaction short of compactness.
    pub fn alpha(&self) -> u64 {
        self.alpha
    }

    /// Set the fragmentation weight. See [`alpha`](Self::alpha).
    pub fn set_alpha(&mut self, alpha: u64) {
        self.alpha = alpha;
    }

    /// Fold one search's [`Probe`] into the counters. A no-op outside tests.
    #[cfg(test)]
    fn record_search(&self, probe: Probe, proposed: bool) {
        let available = (self.tops.len() + self.resizable_by_address.len()) as u64;
        let mut stats = self.stats.get();
        stats.record(probe.visited, available, proposed);
        self.stats.set(stats);
    }
    #[cfg(not(test))]
    #[inline(always)]
    fn record_search(&self, _probe: Probe, _proposed: bool) {}

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

    // ---- the mover index, split by neighbour category ----

    /// How `addr` sits between its neighbours, derived from the current map.
    /// Being at the top of the heap counts as free above: vacating there lets
    /// `end` retreat rather than minting a gap.
    fn neighbours_of(&self, addr: u64, len: u32) -> FreeNeighbours {
        let below_free = self.prev_end(addr) < addr;
        let above = addr + len as u64;
        let above_free = self.next_start(above).is_none_or(|next| next > above);
        match (below_free, above_free) {
            (true, true) => FreeNeighbours::Both,
            (false, false) => FreeNeighbours::Neither,
            _ => FreeNeighbours::One,
        }
    }

    /// Drop `addr` from the mover index, using the category it currently has.
    /// Must be called *before* the map change that would alter that category.
    fn unindex(&mut self, addr: Option<u64>) {
        let Some(addr) = addr else { return };
        let Some(&e) = self.allocations.get(&addr) else {
            return;
        };
        if !e.id.is_fixed_size() {
            return;
        }
        let slot = self.neighbours_of(addr, e.len).index();
        let was = self.class_top(e.len);
        if let Some(sets) = self.live_by_size.get_mut(&e.len) {
            sets[slot].remove(&addr);
            if sets.iter().all(BTreeSet::is_empty) {
                self.live_by_size.remove(&e.len);
            }
        }
        self.refresh_top(e.len, was);
    }

    /// Add `addr` back to the mover index under its (re-derived) category.
    fn reindex(&mut self, addr: Option<u64>) {
        let Some(addr) = addr else { return };
        let Some(&e) = self.allocations.get(&addr) else {
            return;
        };
        if !e.id.is_fixed_size() {
            return;
        }
        let slot = self.neighbours_of(addr, e.len).index();
        let was = self.class_top(e.len);
        self.live_by_size.entry(e.len).or_default()[slot].insert(addr);
        self.refresh_top(e.len, was);
    }

    /// The neighbours of `addr`, whose categories an insert or remove at `addr`
    /// can change. Nothing further away is affected, since a category depends
    /// only on the immediately adjacent space.
    fn neighbour_addrs(&self, addr: u64, len: u32) -> (Option<u64>, Option<u64>) {
        let prev = self.allocations.range(..addr).next_back().map(|(&a, _)| a);
        let next = self.next_start(addr + len as u64);
        (prev, next)
    }

    // ---- class-top index (the branch-and-bound key) ----

    /// The highest-addressed member of size class `size`, if any.
    fn class_top(&self, size: u32) -> Option<u64> {
        self.live_by_size
            .get(&size)?
            .iter()
            .filter_map(|set| set.last().copied())
            .max()
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
        // The neighbours' categories change as soon as this lands next to them,
        // so pull them out of the mover index first and put them back after.
        let (prev, next) = self.neighbour_addrs(addr, len);
        self.unindex(prev);
        self.unindex(next);

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
        if !id.is_fixed_size() {
            self.resizable_by_address.insert(addr);
        }

        self.reindex(prev);
        self.reindex(next);
        self.reindex(Some(addr));
    }

    /// Drop the allocation at `addr`, coalescing its range into the neighbouring
    /// gaps (or retreating `end` if it was the topmost).
    fn remove_raw(&mut self, addr: u64) -> Entry<Id> {
        let len = self
            .allocations
            .get(&addr)
            .expect("remove_raw on an address with no allocation")
            .len;
        // Same dance as `insert_raw`, and for the same reason -- but this one
        // must also unindex the departing allocation itself.
        let (prev, next) = self.neighbour_addrs(addr, len);
        self.unindex(prev);
        self.unindex(next);
        self.unindex(Some(addr));

        let e = self.allocations.remove(&addr).expect("checked above");
        self.by_id.remove(&e.id);
        self.live_bytes -= e.len as u64;
        if !e.id.is_fixed_size() {
            self.resizable_by_address.remove(&addr);
        }

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

        self.reindex(prev);
        self.reindex(next);
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

    /// The best evacuation candidate for size class `size`.
    ///
    /// With `α = 0` this is simply the class's highest member jumping into the
    /// lowest gap that fits. The `α` term widens the search on both sides, but
    /// only to a fixed handful of possibilities: **three movers**, since the best
    /// member now maximizes `a + α·r_src/s` and so depends on neighbour category
    /// (hence the split index), and **two destinations**, since among gaps that
    /// leave a remainder the lowest maximizes `d`, and among exactly-fitting ones
    /// likewise -- no third can win.
    fn evacuation_candidate(&self, size: u32) -> Option<(Gain, Step<u64>)> {
        let s = size as u64;
        let sets = self.live_by_size.get(&size)?;
        let lowest_fitting = self.lowest_gap_fitting(s);
        let lowest_exact = self
            .free_by_size
            .get(&s)
            .and_then(|starts| starts.first().copied());

        let mut best: Option<(Gain, Step<u64>)> = None;
        for category in [
            FreeNeighbours::Both,
            FreeNeighbours::One,
            FreeNeighbours::Neither,
        ] {
            let Some(&from) = sets[category.index()].last() else {
                continue;
            };
            for (to, r_dest) in [
                lowest_fitting.map(|(to, len)| (to, i128::from(len == s))),
                lowest_exact.map(|to| (to, 1)),
            ]
            .into_iter()
            .flatten()
            {
                if to >= from {
                    continue;
                }
                let gain = Gain::new(from - to, s, category.r_src() + r_dest, self.alpha);
                if best.is_none_or(|(g, _)| gain > g) {
                    best = Some((gain, Step { from, to, len: s }));
                }
            }
        }
        best
    }

    /// The evacuation candidate for the resizable allocation at `addr`.
    ///
    /// Simpler than its fixed-size counterpart in two ways, both following from
    /// `r_dest = 0`. There is no exact-fit reward: a resizable allocation that
    /// snugly fills a gap has to relocate again the moment it grows, so the gap
    /// it erased comes straight back, and the reward would be luring the policy
    /// into a round trip. With `r_dest` pinned, only one destination can win --
    /// the lowest gap that fits, which maximizes the travel distance. The source
    /// term still applies in full: vacating a plug between two gaps really does
    /// merge them, whatever the mover's sizedness.
    fn resizable_candidate(&self, addr: u64) -> Option<(Gain, Step<u64>)> {
        let len = self.allocations.get(&addr)?.len;
        let s = len as u64;
        let (to, _) = self.lowest_gap_fitting(s)?;
        (to < addr).then(|| {
            let r_src = self.neighbours_of(addr, len).r_src();
            (
                Gain::new(addr - to, s, r_src, self.alpha),
                Step {
                    from: addr,
                    to,
                    len: s,
                },
            )
        })
    }

    /// The slide candidate: the run above the largest gap, shifting down into it.
    /// The only candidate shape that moves resizable allocations.
    ///
    /// A *maximal* run is flanked by free space above (a gap, or the top of the
    /// heap), so sliding it merges that with the range it vacates, or lets `end`
    /// retreat: `r = +1` either way. That is what guarantees a positive-gain move
    /// always exists while any gap does, for every `α`. A budget-truncated prefix
    /// instead leaves the rest of the run above it, so the gap is merely
    /// relocated: `r = 0`.
    fn slide_candidate(&self, budget: u64) -> Option<(Gain, Step<u64>)> {
        let (&gap_len, starts) = self.free_by_size.last_key_value()?;
        let to = *starts.first()?;
        let from = to + gap_len;
        let (len, truncated) = self.run_len_from(from, budget);
        if len == 0 {
            return None;
        }
        let r = if truncated { 0 } else { 1 };
        Some((
            Gain::new(gap_len, len, r, self.alpha),
            Step { from, to, len },
        ))
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

    /// Pick the highest-gain move **without** evaluating every candidate.
    ///
    /// The search key is an address, not a gain. Keying on gains would be subtly
    /// wrong: when a `free` opens a deep gap, the true gains of every mover small
    /// enough to fit it rise at once, so any cached gain ordering goes stale in
    /// the direction that matters and can sit on the globally best move
    /// indefinitely. An address does not have that problem -- it changes only
    /// when that allocation itself moves.
    ///
    /// And an address bounds a gain: `d = mover - dest <= mover`, since
    /// `dest >= 0`. So walking candidates in descending address, the walk can
    /// stop the moment the best gain so far reaches the next candidate's
    /// address: nothing below it can beat that. (The `2α` widening covers the
    /// fragmentation term's largest possible per-byte contribution, at `s = 1`.
    /// The per-class `2α/s` would be tighter but is not monotone in the address,
    /// so it is not a sound stopping rule for a descending walk.)
    ///
    /// Two streams are merged, both descending: one entry per fixed-size class
    /// (at the class's highest member) and one per resizable allocation. The
    /// slide is evaluated first, outside the walk, because it is a single
    /// O(log n) lookup and usually seeds a bound straight away.
    fn propose_compaction_step(&self, budget: u64) -> Option<Step<u64>> {
        if self.free_by_size.is_empty() {
            self.record_search(Probe::default(), false);
            return None; // gapless: compact
        }
        let mut best = Best::default();
        if let Some((gain, step)) = self.slide_candidate(budget) {
            best.offer(gain, step, budget);
        }

        let mut probe = Probe::default();
        let mut classes = self.tops.iter().rev().peekable();
        let mut resizables = self.resizable_by_address.iter().rev().peekable();
        loop {
            // `Option`'s ordering puts `None` first, so `max` picks whichever
            // stream still has the higher address. The two never collide: an
            // address holds one allocation, and it is either fixed or resizable.
            let class_at = classes.peek().map(|(&addr, _)| addr);
            let resizable_at = resizables.peek().map(|&&addr| addr);
            let Some(next) = class_at.max(resizable_at) else {
                break;
            };
            if let Some(bound) = best.bound() {
                if Gain::new(next.saturating_add(2 * self.alpha), 1, 0, 0) <= bound {
                    break;
                }
            }
            probe.visit();
            let candidate = if class_at == Some(next) {
                let (_, &size) = classes.next().expect("peeked");
                self.evacuation_candidate(size)
            } else {
                let &addr = resizables.next().expect("peeked");
                self.resizable_candidate(addr)
            };
            if let Some((gain, step)) = candidate {
                best.offer(gain, step, budget);
            }
        }
        let chosen = best.pick();
        self.record_search(probe, chosen.is_some());
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
        for &size in self.live_by_size.keys() {
            if let Some((gain, step)) = self.evacuation_candidate(size) {
                best.offer(gain, step, budget);
            }
        }
        for &addr in &self.resizable_by_address {
            if let Some((gain, step)) = self.resizable_candidate(addr) {
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

        let mut expected_live: BTreeMap<u32, [BTreeSet<u64>; FreeNeighbours::COUNT]> =
            BTreeMap::new();
        for (&addr, e) in &self.allocations {
            if e.id.is_fixed_size() {
                let slot = self.neighbours_of(addr, e.len).index();
                expected_live.entry(e.len).or_default()[slot].insert(addr);
            }
        }
        assert_eq!(self.live_by_size, expected_live, "live_by_size drifted");

        let expected_resizable: BTreeSet<u64> = self
            .allocations
            .iter()
            .filter(|(_, e)| !e.id.is_fixed_size())
            .map(|(&addr, _)| addr)
            .collect();
        assert_eq!(
            self.resizable_by_address, expected_resizable,
            "resizable_by_address drifted"
        );

        let expected_tops: BTreeMap<u64, u32> = expected_live
            .iter()
            .map(|(&size, sets)| {
                let top = sets
                    .iter()
                    .filter_map(|s| s.last().copied())
                    .max()
                    .expect("no empty classes");
                (top, size)
            })
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

    #[test]
    fn a_resizable_tail_is_evacuated_rather_than_sliding_the_whole_heap() {
        // One small gap at the bottom, a densely packed middle, and a small
        // resizable allocation at the top. Sliding is available but absurd: it
        // would copy the whole heap to close a 100-byte gap, where evacuating
        // the tail copies 50 bytes and travels 10100.
        let mut h = Heap::new();
        h.alloc(fixed(1), 100).unwrap(); // 0..100, freed below
        for i in 0..10 {
            h.alloc(fixed(10 + i), 1000).unwrap(); // 100..10100
        }
        h.alloc(resizable(99), 50).unwrap(); // 10100..10150
        h.free(fixed(1)).unwrap(); // gap [0, 100)

        let step = h.propose_compaction_step(UNBOUNDED).unwrap();
        assert_eq!(
            step,
            Step {
                from: 10100,
                to: 0,
                len: 50
            },
            "the resizable tail is the deepest mover, so it must be considered"
        );
        h.commit_compaction_step(step);
        assert_eq!(h.len(), 10100, "the tail came off for 50 bytes of copying");
        h.assert_invariants();
    }

    #[test]
    fn a_resizable_mover_earns_no_exact_fit_bonus() {
        // One mover, two destinations: a deeper gap that leaves a sliver, and a
        // shallower one that fits exactly. The only difference between the two
        // heaps below is the *sizedness* of the mover, so any difference in the
        // chosen destination is the exact-fit bonus and nothing else.
        let layout = |mover| {
            [
                (fixed(1), 30, true),  // gap    0..30   <- deeper, leaves a sliver
                (fixed(2), 70, false), //        30..100
                (fixed(3), 10, true),  // gap  100..110  <- exact fit
                (fixed(4), 90, false), //       110..200
                (mover, 10, false),    //       200..210 <- the mover
            ]
        };
        let mut fixed_mover = heap_of(&layout(fixed(5)));
        let mut resizable_mover = heap_of(&layout(resizable(5)));

        // Travelling 100 bytes less has to be bought back by erasing a gap, so
        // with alpha = 0 neither mover takes the exact fit.
        for h in [&mut fixed_mover, &mut resizable_mover] {
            assert_eq!(
                h.propose_compaction_step(UNBOUNDED).unwrap().to,
                0,
                "without the fragmentation term, depth decides"
            );
        }

        // Turn it on, and only the fixed mover is rerouted.
        for h in [&mut fixed_mover, &mut resizable_mover] {
            h.set_alpha(2000);
        }
        assert_eq!(
            fixed_mover.propose_compaction_step(UNBOUNDED).unwrap().to,
            100,
            "a fixed mover should take the exact fit"
        );
        assert_eq!(
            resizable_mover
                .propose_compaction_step(UNBOUNDED)
                .unwrap()
                .to,
            0,
            "a resizable mover should not: it re-opens that gap on its next growth"
        );
    }

    #[test]
    fn a_resizable_mover_still_earns_the_plug_bonus() {
        // The *source* half of the fragmentation term applies regardless of
        // sizedness: vacating an allocation flanked by two gaps really does merge
        // them, whatever it was that moved out.
        let plan = [
            (fixed(1), 10, true),      // gap  0..10   <- destination
            (fixed(2), 20, false),     //      10..30
            (fixed(3), 10, true),      // gap 30..40
            (resizable(4), 10, false), //      40..50  <- the plug
            (fixed(5), 10, true),      // gap 50..60
            (fixed(6), 20, false),     //      60..80
            (resizable(7), 10, false), //      80..90  <- walled in by live neighbours
            (fixed(8), 20, false),     //      90..110
        ];
        let mut h = heap_of(&plan);

        assert_eq!(
            h.propose_compaction_step(UNBOUNDED).unwrap(),
            Step {
                from: 80,
                to: 0,
                len: 10
            },
            "with alpha = 0 the deeper mover wins"
        );

        h.set_alpha(300);
        assert_eq!(
            h.propose_compaction_step(UNBOUNDED).unwrap(),
            Step {
                from: 40,
                to: 0,
                len: 10
            },
            "the gap-merging source outranks the deeper one once alpha pays for it"
        );
    }

    #[test]
    fn alpha_prefers_extracting_a_plug_over_a_deeper_mover_that_carves_a_new_gap() {
        // Same size class, same destination, so the only difference is what each
        // move does to the *gap count*: the lower mover sits between two gaps
        // (vacating merges them), the higher one between two live neighbours
        // (vacating mints a gap).
        let plan = [
            (1, 10, true),  // gap  0..10   <- the destination (an exact fit)
            (2, 20, false), //      10..30
            (3, 10, true),  // gap  30..40
            (4, 10, false), //      40..50  <- the plug
            (5, 10, true),  // gap  50..60
            (6, 20, false), //      60..80
            (7, 10, false), //      80..90  <- walled in by live neighbours
            (8, 20, false), //      90..110
        ];

        let mut h = heap_with_layout(&plan);
        assert_eq!(h.alpha(), 0, "the fragmentation term ships off");
        assert_eq!(
            h.propose_compaction_step(UNBOUNDED).unwrap(),
            Step {
                from: 80,
                to: 0,
                len: 10
            },
            "distance alone picks the higher mover"
        );

        // The plug is 40 bytes lower, so it needs 2*alpha > 40*10 to win.
        h.set_alpha(300);
        assert_eq!(
            h.propose_compaction_step(UNBOUNDED).unwrap(),
            Step {
                from: 40,
                to: 0,
                len: 10
            },
            "alpha should buy the gap-merging move"
        );
    }

    #[test]
    fn alpha_prefers_an_exact_fit_over_squandering_a_larger_gap_further_down() {
        // One mover, two destinations: a deep gap three times too big, and a
        // shallower one that fits exactly. Splitting the big gap leaves a sliver.
        let plan = [
            (1, 30, true),  // gap    0..30   <- deep, but leaves a 20-byte sliver
            (2, 70, false), //        30..100
            (3, 10, true),  // gap  100..110  <- exact fit
            (4, 90, false), //       110..200
            (5, 10, false), //       200..210 <- the mover
        ];

        let mut h = heap_with_layout(&plan);
        assert_eq!(
            h.propose_compaction_step(UNBOUNDED).unwrap(),
            Step {
                from: 200,
                to: 0,
                len: 10
            },
            "distance alone takes the deeper gap and splits it"
        );

        // Travelling 100 bytes less must be bought back by erasing a gap.
        h.set_alpha(2000);
        assert_eq!(
            h.propose_compaction_step(UNBOUNDED).unwrap(),
            Step {
                from: 200,
                to: 100,
                len: 10
            },
            "alpha should reroute the mover to the exact fit"
        );
    }

    #[test]
    fn compaction_still_converges_under_a_large_alpha() {
        // Termination does not depend on alpha: a maximal run is always flanked
        // by free space, so sliding it always merges two gaps (r >= +1) and its
        // gain stays positive however the term is weighted.
        for alpha in [0u64, 1, 1000, 1_000_000] {
            let plan: Vec<(u32, u32, bool)> =
                (1..=21).map(|i| (i, 8 + (i % 5) * 7, i % 3 == 0)).collect();
            let mut h = heap_with_layout(&plan);
            h.set_alpha(alpha);
            assert!(h.len() > h.live_bytes(), "the layout should start gappy");

            compact_fully(&mut h, UNBOUNDED);
            assert_eq!(h.len(), h.live_bytes(), "alpha={alpha} failed to compact");
        }
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

            // Also across alphas: the walk's stop condition widens by 2*alpha,
            // and that widening has to be enough to stay exact.
            for alpha in [0u64, 7, 5000] {
                h.set_alpha(alpha);
                for budget in [1u64, 16, 64, u64::MAX] {
                    assert_eq!(
                        h.propose_compaction_step(budget),
                        h.propose_by_exhaustive_scan(budget),
                        "bounded walk disagreed with the scan at alpha={alpha} budget={budget}"
                    );
                }
            }
            h.set_alpha(0);
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

    /// The churny workload the soak test uses, factored out so the
    /// instrumentation run and the convergence run exercise exactly the same
    /// sequence of operations.
    fn run_churny_workload(h: &mut Heap, rounds: usize) -> Vec<Pointer<u32>> {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut rand = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut live: Vec<Pointer<u32>> = Vec::new();
        let mut next_counter = 1u32;

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

            // Interleave bounded compaction with the workload, as a backend would.
            if round % 7 == 0 {
                if let Some(step) = h.propose_compaction_step(128) {
                    h.commit_compaction_step(step);
                }
            }
        }
        live
    }

    fn report_search_stats(label: &str, s: SearchStats) {
        println!(
            "  {label:<24} calls {:>7}  visited {:>12}  mean {:>9.2}  max {:>6}  examined {:>6.2}%",
            s.calls,
            s.visited,
            s.mean_visited(),
            s.max_visited,
            s.examined_fraction() * 100.0
        );
        let labels = [
            "0", "1", "2-3", "4-7", "8-15", "16-31", "32-63", "64-127", "128+",
        ];
        let hist: Vec<String> = labels
            .iter()
            .zip(s.buckets)
            .filter(|(_, n)| *n > 0)
            .map(|(l, n)| format!("{l}:{n}"))
            .collect();
        println!("  {:<24} {}", "", hist.join("  "));
    }

    /// Not an assertion of behaviour -- a **measurement**, of how much work the
    /// candidate search does over a realistic churn. That is what decides
    /// whether the search needs bounding at all, and the answer is recorded in
    /// `test-results/`.
    ///
    /// Two regimes are reported separately, because they behave very
    /// differently: compaction interleaved with churn (a step every 7 ops, small
    /// budget), and compaction driven to quiescence (no churn, larger budget).
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
            let mut h = Heap::new();
            run_churny_workload(&mut h, rounds);
            let interleaved = h.search_stats();

            // Then drive to quiescence: the regime where the search is expected
            // to be worst, since every remaining gain is small.
            h.reset_search_stats();
            let mut steps = 0u64;
            while let Some(step) = h.propose_compaction_step(4096) {
                h.commit_compaction_step(step);
                steps += 1;
            }
            let quiescing = h.search_stats();

            println!(
                "\n=== {rounds} rounds -> {} live allocations; {steps} steps to quiescence ===",
                h.live_count()
            );
            report_search_stats("interleaved w/ churn", interleaved);
            report_search_stats("driven to quiescence", quiescing);
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
