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
//! - **Evacuation**: one allocation jumps down into a gap below it that fits it.
//!   This is not *searched* for: [`EvacuationIndex`] keeps the best one at its
//!   root, so proposing it is a field read, and constraining it to a budget is
//!   one descent. See `augmented-segment-tree.md`.
//! - **Slide**: the maximal contiguous *run* above the widest gap shifts down
//!   into it. It does not require the moved bytes to *fit* -- `run.size >
//!   gap.width` is the normal case and the move is a partial overlapping shift
//!   -- which is what guarantees progress while any gap exists, including on a
//!   heap whose every gap is too narrow for anything.
//!
//! Both are offered and the better one wins. They are comparable because both
//! are scored by the same quantity, the per-byte gain against `Φ`: for an
//! evacuation the distance the mover travels, for a slide the width of the gap
//! it closes, since every byte of the run travels exactly that far.

use std::collections::{BTreeMap, HashMap};

use crate::evacuation_index::{EvacuationIndex, Key};
use crate::heap::{
    AllocationId, HeapError, IncrementallyCompactableHeap, RelocatableHeap, Relocation, Step,
};
use crate::size_classes::SizeClasses;

/// One row of the address-keyed table.
#[derive(Clone, Copy)]
struct Entry<Id> {
    len: u32,
    id: Id,
}

/// Which of an allocation's neighbours are free -- the only thing that decides
/// what *vacating* it does to the number of gaps, and so the only input the `α`
/// term of [`GainGreedyHeap::alpha`] needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FreeNeighbours {
    /// Free on both sides (or free below and at the top of the heap): vacating
    /// merges the two gaps and the vacated span into one, or lets `end` retreat.
    /// The "plug" case, `r = +1`.
    Both,
    /// Free on exactly one side: the adjacent gap simply extends. `r = 0`.
    One,
    /// Live on both sides: vacating mints a brand-new gap. `r = −1`.
    Neither,
}

/// The weights that turn an allocation into the number the index maximizes.
///
/// A separate type, rather than methods on the heap, because rebuilding the
/// index needs the scoring function while iterating `allocations` -- which
/// already borrows `self`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Weights {
    lambda: bool,
    alpha: u64,
}

impl Weights {
    /// `score(A) = A.pos + λ·A.size + α_eff·(nc − 1)`.
    ///
    /// **`α_eff` is capped per allocation**, at whatever `λ` has left of its own
    /// size, and that is a deliberate departure from the design note. The sign
    /// test needs `λ·reward(s) + α <= s` for every live size `s`; the note
    /// applies that as one *global* bound, which therefore binds at the
    /// **smallest** live allocation -- so a single 1-byte allocation anywhere in
    /// the heap would make `α` inert for every other. Capping each allocation's
    /// own term at its own size gives exactly the same guarantee, because the
    /// sign argument is per-pair and only ever needed to hold for the pair being
    /// scored, while letting large allocations carry the full weight.
    ///
    /// The price is that the gap-count term is no longer uniform across sizes:
    /// small allocations get proportionally less credit for what they do to the
    /// gap count. Note too that `λ` and `α` are mutually exclusive under the
    /// bound -- at `λ = 1` the size reward already saturates it -- so setting both
    /// leaves `α` with nothing to spend.
    fn score(self, addr: u64, len: u32, nc: FreeNeighbours) -> u64 {
        let size = u64::from(len);
        let base = if self.lambda { addr + size } else { addr };
        let headroom = if self.lambda { 0 } else { size };
        let alpha = self.alpha.min(headroom);
        match nc {
            FreeNeighbours::Both => base.saturating_add(alpha),
            FreeNeighbours::One => base,
            // An allocation at an address below `α` cannot travel far anyway, so
            // the saturation costs nothing real.
            FreeNeighbours::Neither => base.saturating_sub(alpha),
        }
    }
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
/// one fixed starting state; it is not cheap (see [`EvacuationIndex`]'s
/// `Clone`).
#[derive(Clone)]
pub struct GainGreedyHeap<Id> {
    /// Start address -> allocation. Gaps are the space between consecutive
    /// entries; `end` is the last entry's end.
    allocations: BTreeMap<u64, Entry<Id>>,
    /// The id table's address column (P1: held once, here).
    by_id: HashMap<Id, u64>,
    /// Every gap **and** every live allocation, keyed by size, augmented so that
    /// the best evacuation sits at the root. It also answers the two free-space
    /// questions the rest of the heap asks -- the widest gap (for the slide) and
    /// the lowest gap that fits (for placement) -- which is why there is no
    /// separate free-space directory any more. See [`EvacuationIndex`].
    index: EvacuationIndex,
    /// Per fixed size: its live allocations, and the gaps its size tiles. This
    /// is what prices a destination by what it leaves behind, which the
    /// size-monotone merge in `index` structurally cannot. See [`SizeClasses`].
    classes: SizeClasses,
    /// One past the highest live byte.
    end: u64,
    /// Sum of all live allocation sizes.
    live_bytes: u64,
    /// The source-side weights: stage 2's size reward and stage 3's gap-count
    /// term. See [`GainGreedyHeap::lambda`] and [`GainGreedyHeap::alpha`].
    weights: Weights,
    /// Stage 4's destination-side weights. See [`GainGreedyHeap::mu`].
    mu_exact: u64,
    mu_multiple: u64,
    /// Test-only instrumentation; absent from real builds.
    #[cfg(test)]
    stats: std::cell::Cell<SearchStats>,
}

impl<Id> Default for GainGreedyHeap<Id> {
    fn default() -> Self {
        Self {
            allocations: BTreeMap::new(),
            by_id: HashMap::new(),
            index: EvacuationIndex::default(),
            classes: SizeClasses::default(),
            end: 0,
            live_bytes: 0,
            weights: Weights::default(),
            mu_exact: 0,
            mu_multiple: 0,
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

    /// Whether the compactor rewards moving *large* allocations.
    ///
    /// "Cost = bytes copied" models the copying well and the *step* badly: a step
    /// also costs a proposal, a commit, and -- once a store is attached -- an I/O
    /// boundary and a journal record. Under a realistic `cost = c₀ + A.size`, ten
    /// 100-byte moves are strictly worse than one 1000-byte move of equal total
    /// distance.
    ///
    /// The exact objective for that cost is `w(A.size)·(A.pos − G.pos)`, which
    /// breaks the index: the coefficient on `G.pos` would depend on which
    /// allocation is chosen, and the merge's `O(1)` crossing case needs the two
    /// sides to be independent. So the preference is expressed *additively*
    /// instead, as `f(A, G) = (A.pos − G.pos) + λ·reward(A.size)`, which keeps
    /// the form `U(A) − V(G)` and so keeps the merge.
    ///
    /// `reward(s) = s` and `λ` a bool is the whole of it. The sign test needs
    /// `λ·reward(s) <= s` -- otherwise an allocation whose only size-valid gaps
    /// lie *above* it could win outright and propose a move that raises `Φ` --
    /// and `λ·s <= s` sits exactly at that edge, so this is the strongest reward
    /// the bound permits, and it needs no multiplication:
    ///
    /// ```text
    /// score(A) = A.pos + A.size   if λ,   else   A.pos
    /// ```
    ///
    /// Two consequences worth stating plainly. The bound binds at the *smallest*
    /// live allocation, not a typical one, so 1-byte allocations would degenerate
    /// it to almost no reward at all. And it caps the reward at the allocation's
    /// own size, so `f` lies between `d` and `2d`: on a heap where distances run
    /// to `10^5` and sizes to `10^2`, this can only ever reorder near-ties.
    ///
    /// Ships **off**, because stage 2's justification is entirely empirical -- it
    /// trades exactness in `Φ` for a preference the potential does not express --
    /// and both settings are a benchmark axis rather than a settled default.
    pub fn lambda(&self) -> bool {
        self.weights.lambda
    }

    /// The weight on what a move does to the **number of gaps at its source**.
    ///
    /// Removing an allocation does one of three things to the gap count, decided
    /// entirely by its immediate neighbours: with both free the two gaps and the
    /// vacated span merge into one (`r = +1`); with one free the adjacent gap
    /// simply extends (`r = 0`); with neither, a brand-new gap is minted
    /// (`r = −1`). The top of the heap counts as free, since vacating the topmost
    /// allocation lets `end` retreat rather than leaving a trailing gap.
    ///
    /// That is a property of the mover **alone**, so it decouples and goes
    /// straight into the score as `α·(nc − 1)`, leaving the merge, the key order
    /// and the validity argument all unchanged. The destination side cannot join
    /// it -- whether a move *closes* a gap depends on `G.width == A.size`, which
    /// couples the two sides -- and that is what [`mu`](Self::mu) is for.
    ///
    /// Note this is an **absolute** term, not the per-byte `α·r/s` an earlier
    /// design used: the tuning does not transfer between them.
    ///
    /// Ships at `0`. It is the term with a cost the others do not have: an
    /// allocation's score now depends on its neighbours, so every mutation
    /// re-keys up to two *other* entries (see
    /// [`set_alpha`](Self::set_alpha)). It should be measured against `α = 0`
    /// rather than adopted on principle.
    pub fn alpha(&self) -> u64 {
        self.weights.alpha
    }

    /// The destination-side weights `(μ₁, μₖ)`: what a move is worth for
    /// *erasing* a gap, and for leaving one still exactly tileable by its own
    /// size class.
    ///
    /// These price the shape of the free space rather than how far bytes travel,
    /// and they are the one term the measurements this project already has argue
    /// for most directly: around three quarters of the free space that leaves
    /// this heap is consumed by new allocations landing in gaps, not by
    /// truncation, so what compaction mostly decides is what *shape* the gaps are
    /// in when the allocator next needs one.
    ///
    /// `μ₁` prices gap erasure, a countable event. `μₖ` prices tileability, a
    /// fragmentation property that only pays if the rest of the class actually
    /// arrives, so it should be weighted well below `μ₁`.
    ///
    /// Both ship at `0`, which makes the whole mechanism inert: the index and the
    /// tiling candidate are still maintained, but a tiling candidate can then
    /// never outscore what the index already found. Like `λ` they are a benchmark
    /// axis, not a settled default -- every fragmentation number this project has
    /// was produced under distance-greed, so the effect has to be measured.
    pub fn mu(&self) -> (u64, u64) {
        (self.mu_exact, self.mu_multiple)
    }

    /// Set the destination-side weights. See [`mu`](Self::mu).
    ///
    /// Unlike [`set_lambda`](Self::set_lambda) this needs no rebuild: `μ` is
    /// applied when a candidate is *scored*, not stored in any key.
    pub fn set_mu(&mut self, mu_exact: u64, mu_multiple: u64) {
        self.mu_exact = mu_exact;
        self.mu_multiple = mu_multiple;
    }

    /// Turn the size reward on or off. See [`lambda`](Self::lambda).
    pub fn set_lambda(&mut self, lambda: bool) {
        self.set_weights(Weights {
            lambda,
            ..self.weights
        });
    }

    /// Set the gap-count weight. See [`alpha`](Self::alpha).
    ///
    /// Turning this on is what makes an allocation's key depend on its
    /// *neighbours*, so from here on freeing or allocating next to `A` re-keys
    /// `A` even though `A` itself did not change -- a delete-and-insert in the
    /// index for each, up to two per mutation on top of the mutation's own work.
    /// That price is charged on the mutation path, which runs several times as
    /// often as the proposal path, which is exactly why `α = 0` is the default
    /// and why the two should be measured against each other.
    pub fn set_alpha(&mut self, alpha: u64) {
        self.set_weights(Weights {
            alpha,
            ..self.weights
        });
    }

    /// Adopt new source-side weights, rebuilding the index.
    ///
    /// The score is part of the index key, so this cannot mutate in place:
    /// changing scores under the existing entries would leave every one of them
    /// unremovable. `O(n log n)`, which is fine for what it is -- a policy knob
    /// set once before the heap is used, or swept by a benchmark.
    fn set_weights(&mut self, weights: Weights) {
        if weights == self.weights {
            return;
        }
        self.weights = weights;
        self.index = EvacuationIndex::default();
        // Walk the layout once, minting both kinds of entry in address order.
        // `nc` is read off this walk rather than re-derived per allocation:
        // `prev_free` is the gap that has just been emitted, and `next_free`
        // needs only one lookahead.
        let mut cursor = 0u64;
        let mut entries = self.allocations.iter().peekable();
        while let Some((&addr, e)) = entries.next() {
            let below_free = addr > cursor;
            if below_free {
                self.index.insert(Key::gap(cursor, addr - cursor));
            }
            cursor = addr + u64::from(e.len);
            // The top of the heap counts as free.
            let above_free = entries.peek().is_none_or(|(&next, _)| next > cursor);
            let nc = match (below_free, above_free) {
                (true, true) => FreeNeighbours::Both,
                (false, false) => FreeNeighbours::Neither,
                _ => FreeNeighbours::One,
            };
            self.index
                .insert(Key::alloc(addr, e.len, weights.score(addr, e.len, nc)));
        }
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
            let provenance = (
                self.slide_candidate(budget - moved).map(|(_, s)| s) == Some(step),
                self.end,
            );
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

    // ---- index maintenance ----
    //
    // Gaps and allocations are entries of the *same* tree, so both kinds are
    // recorded and forgotten here. A zero-length gap is not a gap and is never
    // recorded, which is what lets these be called unconditionally.

    fn gap_record(&mut self, start: u64, len: u64) {
        if len > 0 {
            self.index.insert(Key::gap(start, len));
            self.classes.add_gap(start, len);
        }
    }

    fn gap_forget(&mut self, start: u64, len: u64) {
        if len > 0 {
            self.index.remove(Key::gap(start, len));
            self.classes.remove_gap(start, len);
        }
    }

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

    /// The index key of the allocation at `addr`.
    ///
    /// Both removal and insertion go through this, so a remove can never compute
    /// a different key than the matching insert did -- which matters more than it
    /// looks once the score depends on the *neighbours*, since the map has to be
    /// in the same state both times. That is what the `unindex`/`reindex` dance
    /// below is for.
    fn alloc_key(&self, addr: u64, len: u32) -> Key {
        let nc = self.neighbours_of(addr, len);
        Key::alloc(addr, len, self.weights.score(addr, len, nc))
    }

    /// The addresses whose neighbour category an insert or removal at `addr` can
    /// change. Nothing further away is affected, since the category depends only
    /// on the immediately adjacent space.
    fn neighbour_addrs(&self, addr: u64, len: u32) -> (Option<u64>, Option<u64>) {
        let prev = self.allocations.range(..addr).next_back().map(|(&a, _)| a);
        let next = self.next_start(addr + len as u64);
        (prev, next)
    }

    /// Drop `addr` from the index under the category it *currently* has. Must be
    /// called before the map change that would alter that category.
    fn unindex(&mut self, addr: Option<u64>) {
        let Some(addr) = addr else { return };
        let Some(&e) = self.allocations.get(&addr) else {
            return;
        };
        self.index.remove(self.alloc_key(addr, e.len));
    }

    /// Put `addr` back under its re-derived category.
    fn reindex(&mut self, addr: Option<u64>) {
        let Some(addr) = addr else { return };
        let Some(&e) = self.allocations.get(&addr) else {
            return;
        };
        self.index.insert(self.alloc_key(addr, e.len));
    }

    /// Record an allocation in the index, and -- if it is fixed-size -- in its
    /// size class. Resizable ones are deliberately absent from `classes`: a snug
    /// fit would only re-open on their next growth, so they earn no
    /// destination-side reward at all.
    fn alloc_record(&mut self, addr: u64, len: u32, id: Id) {
        let key = self.alloc_key(addr, len);
        self.index.insert(key);
        if id.is_fixed_size() {
            self.classes.add_alloc(len, addr);
        }
    }

    fn alloc_forget(&mut self, addr: u64, len: u32, id: Id) {
        let key = self.alloc_key(addr, len);
        self.index.remove(key);
        if id.is_fixed_size() {
            self.classes.remove_alloc(len, addr);
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
        // The neighbours' categories change the moment this lands next to them,
        // and their keys carry those categories -- so pull them out of the index
        // first, while their old keys are still computable, and put them back
        // after. At `α = 0` the score ignores the category and both calls are
        // pure overhead, which is precisely the cost `α` is measured against.
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
        self.alloc_record(addr, len, id);

        self.reindex(prev);
        self.reindex(next);
    }

    /// Drop the allocation at `addr`, coalescing its range into the neighbouring
    /// gaps (or retreating `end` if it was the topmost).
    fn remove_raw(&mut self, addr: u64) -> Entry<Id> {
        let len = self
            .allocations
            .get(&addr)
            .expect("remove_raw on an address with no allocation")
            .len;
        // Same dance as `insert_raw`, and for the same reason -- but this one has
        // to unindex the departing allocation itself too, and all three before
        // the map changes underneath their keys.
        let (prev, next) = self.neighbour_addrs(addr, len);
        self.unindex(prev);
        self.unindex(next);
        let e = self.allocations[&addr];
        self.alloc_forget(addr, e.len, e.id);

        self.allocations.remove(&addr);
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

        self.reindex(prev);
        self.reindex(next);
        e
    }

    // ---- placement ----

    /// The lowest-addressed gap at least `min_len` bytes wide -- the destination
    /// that maximizes travel distance, and so per-byte gain, for a `min_len`-byte
    /// mover.
    ///
    /// One `O(log n)` descent of the index, aggregating `min_gap_pos` over the
    /// key suffix. The obvious alternative, scanning a size-keyed map upward for
    /// the minimum address, costs one probe per distinct gap *size* -- fine when
    /// gaps cluster on a few sizes, but unbounded when they do not.
    /// `benches/lowest_fitting_gap.rs` measures the difference.
    fn lowest_gap_fitting(&self, min_len: u64) -> Option<u64> {
        self.index.lowest_gap_fitting(min_len)
    }

    /// Where to put a new `size`-byte allocation.
    ///
    /// Placement is scored against the same potential compaction is, but
    /// *without* a cost term: the bytes are written wherever they go, so a lower
    /// address here is free where compaction would pay a full copy for it. An
    /// allocation of size `s` at `a` adds `s·(a + (s−1)/2)` to `Φ`, and the
    /// constant drops out of a comparison.
    ///
    /// Consuming a gap cleanly is worth the same `μ` weights the compactor uses,
    /// so placement and compaction agree about what a good destination is --
    /// which matters here more than it does there, since it is *placement* that
    /// destroys most of this heap's free space.
    ///
    /// Three candidates suffice: `cost` is otherwise monotone in the address, so
    /// the only gaps that can beat the lowest fitting one are the two that carry
    /// a bonus. Only a fixed-size allocation may claim either, for the reason
    /// given in [`SizeClasses`].
    fn place(&self, size: u32, is_fixed_size: bool) -> Result<u64, HeapError> {
        let want = size as u64;
        let s = i128::from(size);
        let cost = |addr: u64, bonus: u64| s * i128::from(addr) - i128::from(bonus);

        let mut best = self
            .lowest_gap_fitting(want)
            .map(|addr| (cost(addr, 0), addr));
        if is_fixed_size {
            for (gap, mu) in [
                (self.classes.lowest_exact_gap(size), self.mu_exact),
                (self.classes.lowest_multiple_gap(size), self.mu_multiple),
            ] {
                let Some(addr) = gap else { continue };
                let scored = (cost(addr, mu), addr);
                if best.is_none_or(|incumbent| scored < incumbent) {
                    best = Some(scored);
                }
            }
        }
        if let Some((_, addr)) = best {
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

    /// Grow a chosen step into a **run**, in both directions, as far as the
    /// destination gap and the budget allow -- stage 5's minimal version.
    ///
    /// Full run support would mean *indexing* runs, which costs `O(k_max²)`
    /// point updates per allocation inserted or removed and multiplies the tree
    /// by `k_max`, all charged to the mutation path. This gets a cheap fraction
    /// of the benefit for nothing: it runs once per proposal, after the winner is
    /// already known, and needs no new entries at all.
    ///
    /// It is safe and weakly profitable in **every** case, which follows from the
    /// distance bound. Let the step move `[from, from+len)` into a gap at `to` of
    /// width `w`, and let `d = from − to`; a gap and an allocation never overlap,
    /// so `d >= w` always.
    ///
    /// - Absorbing the neighbour *above* leaves `from` alone and adds `c` bytes
    ///   that each travel the full `d`, so the potential drops by a further `c·d`.
    /// - Absorbing the neighbour *below* lowers `from` by `b` and changes the drop
    ///   by `b·(d − S)`, where `S` is the new total size. That is non-negative
    ///   because the run must still fit the gap (`S <= w`) and must still sit
    ///   above it (`d >= w`), so `d >= S` throughout.
    ///
    /// Capping at `min(w, budget)` is therefore the whole rule. Note that the
    /// downward extension degenerates gracefully: if it reaches the allocation
    /// immediately above the gap, the step has quietly become a slide.
    ///
    /// Applied to *every* chosen step, because it is a no-op on a slide -- that
    /// run is already maximal up to the budget, and nothing ends exactly at its
    /// `from`, which is the far side of the gap.
    fn extend_into_run(&self, mut step: Step<u64>, budget: u64) -> Step<u64> {
        // The gap the step lands in runs from `to` to the next allocation.
        let Some(gap_end) = self.next_start(step.to) else {
            return step; // no allocation above it at all: nothing to absorb
        };
        let cap = (gap_end - step.to).min(budget);

        // Upward: absorb the allocation starting exactly where the run ends.
        //
        // This cannot currently fire on an evacuation, and the reason is worth
        // recording. Absorbing `N` requires `step.len + N.size <= cap <= w`, so
        // `N` fits the gap on its own; and `N` sits above the run, so it scores
        // strictly higher than the mover under any score of the form
        // `A.pos + f(A.size)` -- meaning the index would have returned `N` in the
        // first place. Nor does the budget separate them, since `N.size <= cap
        // <= budget`. On a slide it is equally inert: that run is already
        // maximal up to the budget.
        //
        // It is kept because stage 4 breaks the premise. A tiling candidate is
        // chosen by `score + mu`, so a *lower* allocation with an exact fit can
        // win, and then the allocation above it is both absorbable and not the
        // one that was chosen.
        while let Some(next) = self.allocations.get(&(step.from + step.len)) {
            if step.len + u64::from(next.len) > cap {
                break;
            }
            step.len += u64::from(next.len);
        }

        // Downward: absorb the allocation ending exactly where the run starts.
        while let Some((&addr, e)) = self.allocations.range(..step.from).next_back() {
            if addr + u64::from(e.len) != step.from || addr <= step.to {
                break;
            }
            if step.len + u64::from(e.len) > cap {
                break;
            }
            step.len += u64::from(e.len);
            step.from = addr;
        }

        step
    }

    /// The slide candidate: the run above the widest gap, shifting down into it,
    /// paired with its per-byte gain.
    ///
    /// This is the candidate that guarantees progress. It does not require the
    /// run to fit in the gap, and a maximal run is flanked by free space above (a
    /// gap, or the top of the heap), so sliding it either merges that free space
    /// with the range it vacates or lets `end` retreat. Every byte in the run
    /// travels exactly `gap_len` down, so its per-byte gain is `gap_len` --
    /// positive while any gap exists, which is what the evacuation index cannot
    /// promise on a heap whose every gap is too narrow for anything.
    ///
    /// Under `λ` it earns the same size reward an evacuation does, **capped at
    /// the distance travelled**. That cap is not an extra rule but the invariant
    /// the sign bound already forces on evacuations: there `d >= s >= λ·reward(s)`,
    /// so `f` lies in `[d, 2d]`. Applying it here keeps the two shapes on one
    /// scale. Uncapped it would not: a slide's run is budget-sized (thousands of
    /// bytes) against an evacuation's single allocation (hundreds), so `λ·len`
    /// alone would usually exceed any evacuation's entire score and the reward
    /// would stop being a tie-breaker and become a standing preference for
    /// sliding.
    fn slide_candidate(&self, budget: u64) -> Option<(u64, Step<u64>)> {
        let (to, gap_len) = self.index.widest_gap()?;
        let from = to + gap_len;
        let (len, _truncated) = self.run_len_from(from, budget);
        if len == 0 {
            return None;
        }
        let reward = if self.weights.lambda {
            len.min(gap_len)
        } else {
            0
        };
        Some((gap_len + reward, Step { from, to, len }))
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
        let addr = self.place(size, id.is_fixed_size())?;
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
        // ranges cannot overlap and the caller's copy is unambiguous. A relocating
        // `resize` only ever runs on a resizable allocation, which claims no fit
        // bonus -- it would only re-open the gap on its next growth.
        let dest = self.place(new_size, false)?;
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

    /// The best of the two candidate shapes, both scored by the same objective.
    ///
    /// No walk: the slide is a root read plus a bounded run scan, and the
    /// evacuation is the index's budgeted descent. Ties go to the slide, which
    /// is the shape that can also retire free space at the top of the heap.
    ///
    /// `budget` is a ranking input, not a cap. Both candidates respect it where
    /// they can -- the evacuation exactly, the slide by taking a prefix of the
    /// run -- but a slide of a single oversized allocation is still offered
    /// whole, since reporting quiescence would strand its gap forever.
    fn propose_compaction_step(&self, budget: u64) -> Option<Step<u64>> {
        let mut best: Option<(u64, Step<u64>)> = self.slide_candidate(budget);

        // The index maximizes `score(A) − G.pos` directly and reports it, which
        // matters once `λ` is on: the objective is then no longer the travel
        // distance, so re-deriving it from the step would silently compare the
        // wrong quantity against the slide's.
        if let Some((gain, step)) = self.index.best_evacuation_within(budget) {
            if best.is_none_or(|(incumbent, _)| gain > incumbent) {
                best = Some((gain, step));
            }
        }

        // The tiling candidate re-ranks evacuations the index already scored --
        // it never contributes a move the index could not see, since a tiling
        // evacuation *is* an ordinary evacuation and `μ` changes only its rank.
        // So if `best` is still empty there is no tiling candidate either, and
        // the guard is what makes that reasoning load-bearing rather than
        // incidental.
        if best.is_some() {
            // The mover a class nominates is scored exactly as the index would
            // score it -- neighbour category included -- so that `+ μ` is the only
            // difference between the two candidates and the values stay
            // comparable.
            let tiling = self.classes.best_tiling_evacuation(
                budget,
                self.mu_exact,
                self.mu_multiple,
                |addr, size| {
                    self.weights
                        .score(addr, size, self.neighbours_of(addr, size))
                },
            );
            if let Some((gain, from, to, size)) = tiling {
                if best.is_none_or(|(incumbent, _)| gain > incumbent) {
                    best = Some((
                        gain,
                        Step {
                            from,
                            to,
                            len: u64::from(size),
                        },
                    ));
                }
            }
        }

        // Defensive, and in practice unreachable: `slide_candidate` yields
        // something whenever a gap exists, because a gap always has an
        // allocation above it (free space at the top is not a gap, it is `end`
        // retreating). Reading the unbudgeted root is `O(1)`, so keeping the
        // fallback costs nothing next to making the caller re-enter.
        let chosen = best.map(|(_, step)| step).or_else(|| {
            debug_assert!(self.index.widest_gap().is_none(), "a gap with no slide");
            self.index.best_evacuation().map(|(_, step)| step)
        });
        // Opportunistic, after the winner is known: never changes *which* move is
        // taken, only how much of the neighbourhood rides along with it.
        let chosen = chosen.map(|step| self.extend_into_run(step, budget));
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

    /// The reference `lowest_gap_fitting` must agree with: a scan of the gaps
    /// the allocation map implies.
    fn lowest_gap_fitting_by_scan(&self, min_len: u64) -> Option<u64> {
        self.implied_gaps()
            .into_iter()
            .filter(|&(_, len)| len >= min_len)
            .map(|(start, _)| start)
            .min()
    }

    /// The reference the index's root must agree with: every (mover, gap) pair,
    /// no index and no pruning at all. Returns the winning objective value, which
    /// carries the full score -- so this checks `λ` and `α` as well as the search.
    fn best_evacuation_by_brute_force(&self, budget: u64) -> Option<u64> {
        let gaps = self.implied_gaps();
        let mut best = 0u64;
        for (&from, e) in &self.allocations {
            if u64::from(e.len) > budget {
                continue;
            }
            let score = self
                .weights
                .score(from, e.len, self.neighbours_of(from, e.len));
            for &(pos, width) in &gaps {
                if width >= u64::from(e.len) && pos < from {
                    best = best.max(score.saturating_sub(pos));
                }
            }
        }
        (best > 0).then_some(best)
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

        // The index must hold exactly one entry per gap and one per allocation.
        let gaps = self.implied_gaps();
        let mut expected: Vec<Key> = gaps
            .iter()
            .map(|&(start, len)| Key::gap(start, len))
            .chain(
                self.allocations
                    .iter()
                    .map(|(&addr, e)| self.alloc_key(addr, e.len)),
            )
            .collect();
        expected.sort();
        assert_eq!(
            self.index.iter().collect::<Vec<_>>(),
            expected,
            "the evacuation index drifted from the allocation map"
        );

        // A stale *augmentation* would survive that check, so every query the
        // index answers is also checked against a scan.
        assert_eq!(
            self.index.widest_gap(),
            gaps.iter().copied().max_by_key(|&(pos, len)| {
                // Widest, and among equally wide the lowest-addressed.
                (len, std::cmp::Reverse(pos))
            }),
            "widest_gap disagreed with a scan"
        );
        for min_len in gaps
            .iter()
            .flat_map(|&(_, len)| [len.saturating_sub(1), len, len + 1])
            .chain([1])
        {
            assert_eq!(
                self.lowest_gap_fitting(min_len),
                self.lowest_gap_fitting_by_scan(min_len.max(1)),
                "lowest_gap_fitting disagreed with a scan at min_len={min_len}"
            );
        }
        let fixed: Vec<(u64, u32)> = self
            .allocations
            .iter()
            .filter(|(_, e)| e.id.is_fixed_size())
            .map(|(&addr, e)| (addr, e.len))
            .collect();
        self.classes.assert_consistent(&gaps, &fixed);

        for budget in self
            .allocations
            .values()
            .flat_map(|e| [u64::from(e.len).saturating_sub(1), u64::from(e.len)])
            .chain([0, u64::MAX])
        {
            assert_eq!(
                self.index
                    .best_evacuation_within(budget)
                    .map(|(gain, _)| gain),
                self.best_evacuation_by_brute_force(budget),
                "the budgeted descent disagreed with brute force at budget={budget}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pointer::{Pointer, Sizedness};

    const UNBOUNDED: u64 = u64::MAX;

    /// The policy settings the randomized tests sweep: `(λ, α, (μ₁, μₖ))`.
    ///
    /// Every one of them must converge to a gapless heap and must never propose
    /// an invalid step -- none of these knobs is allowed to buy correctness with
    /// tuning. The shipped default is first; the last two are deliberately
    /// extreme, to check that weights large enough to dominate the distance term
    /// still cannot stall compaction or produce an upward move.
    ///
    /// `λ` and `α` never appear together with both non-zero, because they cannot
    /// both bite: the sign bound gives an allocation `A.size` to spend and `λ`
    /// spends all of it. See [`Weights::score`].
    const POLICIES: [(bool, u64, (u64, u64)); 7] = [
        (false, 0, (0, 0)),
        (true, 0, (0, 0)),
        (false, 64, (0, 0)),
        (false, 0, (4096, 512)),
        (true, 0, (4096, 512)),
        (false, 64, (4096, 512)),
        (false, u64::MAX, (1 << 40, 1 << 36)),
    ];

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
    fn evacuation_beats_sliding_when_a_high_allocation_can_jump_far_down() {
        let mut h = Heap::new();
        h.alloc(fixed(1), 10).unwrap(); // 0..10
        h.alloc(fixed(2), 10).unwrap(); // 10..20  (freed below)
        h.alloc(resizable(3), 100).unwrap(); // 20..120
        h.alloc(fixed(4), 10).unwrap(); // 120..130
        h.free(fixed(2)).unwrap(); // gap 10..20

        // The highest 10-byte allocation is at 120 and the lowest gap that fits
        // it is at 10, so the evacuation gains 110 -- far better than sliding the
        // run above the gap down by 10.
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

    /// Resizable allocations are ordinary entries of the index, with no special
    /// handling of any kind. The old design excluded them from its size-class
    /// structure -- they scatter one per class -- and reached them only through a
    /// second, address-keyed index; keying by size makes the distinction moot.
    #[test]
    fn a_resizable_allocation_is_an_ordinary_evacuation_candidate() {
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

    /// The budget is a **prefix of the key order**, so constraining the search by
    /// it is exact rather than the two-track heuristic it replaced: the best
    /// affordable move is found even when a far better unaffordable one exists.
    #[test]
    fn a_budget_yields_the_best_affordable_evacuation_not_a_near_miss() {
        // The neighbours are deliberately too large to be absorbed at this
        // budget, so `extend_into_run` is a no-op and this stays a test about
        // *choosing* a mover rather than about growing one.
        let plan = [
            (1, 100, true),  // gap    0..100  <- fits either mover
            (2, 50, false),  //      100..150
            (3, 8, true),    // gap  150..158  <- fits only the small mover
            (4, 92, false),  //      158..250
            (5, 8, false),   //      250..258  <- the small mover
            (6, 100, false), //      258..358  <- the large mover, travels further
        ];
        let h = heap_with_layout(&plan);

        // Unconstrained: the 100-byte mover travels 258, beating the 8-byte
        // mover's 250.
        assert_eq!(
            h.propose_compaction_step(UNBOUNDED).unwrap(),
            Step {
                from: 258,
                to: 0,
                len: 100
            }
        );
        // Priced out of it, the 8-byte mover takes the *lowest* gap that fits
        // it -- not the nearer one at 150.
        assert_eq!(
            h.propose_compaction_step(99).unwrap(),
            Step {
                from: 250,
                to: 0,
                len: 8
            }
        );
        h.assert_invariants();
    }

    /// `α` prices what *vacating* a mover does to the gap count: extracting a
    /// "plug" between two gaps merges them, where carving a mover out of a solid
    /// run mints a new one.
    ///
    /// The layout below is elaborate, and that is the finding rather than an
    /// accident of test-writing. Under the sign bound, `α_eff <= A.size`, so the
    /// term can only reorder candidates whose scores are already within
    /// `α_plug + α_walled` of each other -- and the geometry fights back hard:
    ///
    /// - A plug can **never** outrank a walled allocation above it *into the same
    ///   destination*. Making the plug's top neighbour free requires a gap above
    ///   it, and the allocation just above that gap has free space below it, so
    ///   it scores un-penalised at `>= plug.pos + plug.size + 1` -- already more
    ///   than `plug.pos + α_eff`. Every intermediate allocation must therefore be
    ///   too large to fit the destination at all, which pushes the walled one
    ///   further away than `α` can reach.
    /// - So the two must aim at **different** destinations, which is what this
    ///   layout arranges: `Z` and `W` fit no gap in the heap, so they are not
    ///   candidates and merely serve to wall `B` in.
    ///
    /// The conclusion worth carrying: `α` is a near-tie-breaker, not a policy
    /// lever, and the sign bound is what makes it one.
    #[test]
    fn alpha_prefers_extracting_a_plug_over_carving_a_new_gap() {
        let plan = [
            (1, 250, true),   // gap    0..250   <- A's destination
            (2, 250, false),  // X    250..500
            (3, 300, true),   // gap  500..800   <- B's destination
            (4, 200, false),  // Y    800..1000
            (5, 10, true),    // gap 1000..1010
            (6, 250, false),  // A   1010..1260  <- the plug: free on both sides
            (7, 10, true),    // gap 1260..1270
            (8, 400, false),  // Z   1270..1670  <- fits no gap; walls B from below
            (9, 300, false),  // B   1670..1970  <- walled in: live on both sides
            (10, 400, false), // W   1970..2370  <- fits no gap; walls B from above
        ];
        let mut h = heap_with_layout(&plan);
        assert_eq!(h.alpha(), 0, "the gap-count term ships off");

        // Distance alone: B travels 1670 − 500 = 1170, beating A's 1010.
        assert_eq!(
            h.propose_compaction_step(UNBOUNDED).unwrap(),
            Step {
                from: 1670,
                to: 500,
                len: 300
            },
            "distance alone takes the move that carves a new gap"
        );

        // With α, A gains its own size and B loses B's: 1260 against 920.
        h.set_alpha(250);
        assert_eq!(
            h.propose_compaction_step(UNBOUNDED).unwrap(),
            Step {
                from: 1010,
                to: 0,
                len: 250
            },
            "α should buy the gap-merging move"
        );
        h.assert_invariants();
    }

    /// `α` makes an allocation's key depend on its *neighbours*, so a mutation
    /// next to `A` has to re-key `A` even though `A` did not change. If that
    /// bookkeeping were wrong the index would hold entries under keys nobody can
    /// recompute, which `assert_invariants` catches on the very next call.
    #[test]
    fn alpha_rekeys_the_neighbours_of_every_mutation() {
        let mut h = Heap::new();
        h.set_alpha(64);

        // Build a run, then punch holes in it: every free changes the category of
        // both survivors beside it.
        for i in 1..=9u32 {
            h.alloc(fixed(i), 64).unwrap();
            h.assert_invariants();
        }
        for i in [2u32, 5, 8, 4] {
            h.free(fixed(i)).unwrap();
            h.assert_invariants();
        }
        // Re-filling those holes flips the categories back.
        for i in 20..=23u32 {
            h.alloc(fixed(i), 64).unwrap();
            h.assert_invariants();
        }
        compact_fully(&mut h, UNBOUNDED);
        assert_eq!(h.len(), h.live_bytes());
    }

    #[test]
    fn toggling_alpha_rebuilds_the_index_consistently() {
        let plan: Vec<(u32, u32, bool)> = (1..=41)
            .map(|i| (i, 8 + (i % 7) * 11, i % 3 == 0))
            .collect();
        let mut h = heap_with_layout(&plan);
        for alpha in [64u64, 0, 1_000_000] {
            h.set_alpha(alpha);
            assert_eq!(h.alpha(), alpha);
            h.assert_invariants();
        }
        compact_fully(&mut h, UNBOUNDED);
        assert_eq!(h.len(), h.live_bytes(), "α must not stall compaction");
    }

    /// `μ₁` should reroute a mover into a gap it fills exactly, giving up some
    /// travel distance to erase a gap outright rather than leave a sliver.
    #[test]
    fn mu_reroutes_a_fixed_size_mover_into_an_exact_fit() {
        let plan = [
            (1, 30, true),  // gap    0..30   <- deeper, but leaves a 20-byte sliver
            (2, 70, false), //       30..100
            (3, 10, true),  // gap  100..110  <- an exact fit for the mover
            (4, 90, false), //      110..200
            (5, 10, false), //      200..210  <- the mover
        ];
        let mut h = heap_with_layout(&plan);
        assert_eq!(h.mu(), (0, 0), "the destination weights ship off");

        // Off, depth decides and the 30-byte gap is carved up.
        assert_eq!(
            h.propose_compaction_step(UNBOUNDED).unwrap().to,
            0,
            "with no destination weight, depth decides"
        );

        // Travelling 100 bytes less has to be bought back by erasing a gap.
        h.set_mu(2000, 0);
        let step = h.propose_compaction_step(UNBOUNDED).unwrap();
        assert_eq!(
            step,
            Step {
                from: 200,
                to: 100,
                len: 10
            },
            "μ₁ should reroute the mover into the exact fit"
        );
        h.commit_compaction_step(step);
        h.assert_invariants();
    }

    /// A resizable allocation earns no destination-side reward at any `k`: parked
    /// in a snug gap it would have to move again the moment it grows, re-opening
    /// the gap and paying for two copies.
    #[test]
    fn a_resizable_mover_earns_no_fit_bonus() {
        // The two heaps differ *only* in the sizedness of the mover, so any
        // difference in the destination chosen is the fit bonus and nothing else.
        let layout = |mover| {
            [
                (fixed(1), 30, true),  // gap    0..30   <- deeper, leaves a sliver
                (fixed(2), 70, false), //       30..100
                (fixed(3), 10, true),  // gap  100..110  <- exact fit
                (fixed(4), 90, false), //      110..200
                (mover, 10, false),    //      200..210  <- the mover
            ]
        };
        let mut fixed_mover = heap_of(&layout(fixed(5)));
        let mut resizable_mover = heap_of(&layout(resizable(5)));

        for h in [&mut fixed_mover, &mut resizable_mover] {
            h.set_mu(2000, 0);
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

    /// Placement and compaction share the `μ` weights, which matters more at
    /// placement time than at compaction time: most of this heap's free space is
    /// destroyed by new allocations landing in gaps, not by truncation.
    #[test]
    fn mu_buys_the_exact_fit_at_placement_time_too() {
        let plan = [
            (1, 20, true),  // gap    0..20
            (2, 80, false), //       20..100
            (3, 10, true),  // gap  100..110  <- exact for a 10-byte request
            (4, 80, false), //      110..190
        ];
        let mut h = heap_with_layout(&plan);
        // Travelling 100 higher costs 10·100 of potential, so erasing a gap has
        // to be worth more than that before the exact fit wins.
        h.set_mu(999, 0);
        assert_eq!(h.alloc(fixed(5), 10).unwrap(), 0, "μ₁ too small to matter");
        h.free(fixed(5)).unwrap();

        h.set_mu(1001, 0);
        assert_eq!(h.alloc(fixed(6), 10).unwrap(), 100, "μ₁ now pays for it");
        h.assert_invariants();

        // ...and a resizable request still takes the lowest fitting gap.
        h.free(fixed(6)).unwrap();
        assert_eq!(h.alloc(resizable(7), 10).unwrap(), 0);
        h.assert_invariants();
    }

    /// `μₖ` is speculative and priced below `μ₁`, so a gap that is merely
    /// *tileable* must not outrank one that is erased outright.
    #[test]
    fn a_proper_multiple_is_worth_less_than_an_exact_fit() {
        let plan = [
            (1, 20, true),  // gap    0..20   <- a proper multiple of 10
            (2, 80, false), //       20..100
            (3, 10, true),  // gap  100..110  <- an exact fit
            (4, 80, false), //      110..190
            (5, 10, false), //      190..200  <- the mover
        ];
        let mut h = heap_with_layout(&plan);

        // Weighted equally, the deeper (merely tileable) gap wins on distance.
        h.set_mu(2000, 2000);
        assert_eq!(h.propose_compaction_step(UNBOUNDED).unwrap().to, 0);
        // Priced properly -- erasure above tileability -- the exact fit wins.
        h.set_mu(2000, 10);
        assert_eq!(h.propose_compaction_step(UNBOUNDED).unwrap().to, 100);
    }

    /// The chosen move drags its lower neighbours along when they fit -- stage
    /// 5's opportunistic run extension.
    ///
    /// Only *downward* is exercised, because the upward direction provably
    /// cannot fire on a step this proposer chooses; see `extend_into_run`.
    ///
    /// The layout below is arranged so that the top allocation fits no gap at
    /// all, which is what stops it from being the mover and leaves the mover
    /// with room beneath it.
    #[test]
    fn a_chosen_evacuation_absorbs_neighbours_that_fit_the_gap() {
        let plan = [
            (1, 100, true),  // gap    0..100  <- 100 bytes of room
            (2, 30, false),  //      100..130
            (3, 8, true),    // gap  130..138
            (4, 20, false),  //      138..158  <- absorbed second
            (5, 10, false),  //      158..168  <- absorbed first
            (6, 20, false),  //      168..188  <- the mover: travels 168, the most
            (7, 200, false), //     188..388  <- fits no gap, so never the mover
        ];
        let mut h = heap_with_layout(&plan);

        let step = h.propose_compaction_step(UNBOUNDED).unwrap();
        assert_eq!(
            step,
            Step {
                from: 138,
                to: 0,
                len: 50
            },
            "the run should grow downward to 20 + 10 + 20 = 50 bytes"
        );
        h.commit_compaction_step(step);
        // All three landed, in order, at the bottom.
        assert_eq!(h.lookup(fixed(4)), Some((0, 20)));
        assert_eq!(h.lookup(fixed(5)), Some((20, 10)));
        assert_eq!(h.lookup(fixed(6)), Some((30, 20)));
        h.assert_invariants();
    }

    #[test]
    fn run_extension_respects_the_budget_as_well_as_the_gap() {
        let plan = [
            (1, 100, true),  // gap    0..100
            (2, 30, false),  //      100..130
            (3, 8, true),    // gap  130..138
            (4, 20, false),  //      138..158
            (5, 10, false),  //      158..168
            (6, 20, false),  //      168..188  <- the mover
            (7, 200, false), //     188..388
        ];
        let h = heap_with_layout(&plan);

        // The gap would take all 50 bytes; a 35-byte budget stops after 30.
        let step = h.propose_compaction_step(35).unwrap();
        assert_eq!(
            step,
            Step {
                from: 158,
                to: 0,
                len: 30
            },
            "extension must stop at the budget, not at the gap's width"
        );
        assert!(step.len <= 35, "{step:?} exceeded the budget");
    }

    /// Extension must never turn a profitable move into an unprofitable one, and
    /// must never produce a step that spans a gap -- `commit` would then move
    /// bytes it does not re-key. It is applied to every chosen step, including
    /// slides, where it should simply do nothing.
    #[test]
    fn run_extension_never_produces_an_invalid_step() {
        let mut state = 0x0BAD_C0DE_D15E_A5E1u64;
        let mut rand = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for (lambda, alpha, mu) in POLICIES {
            let mut h = Heap::new();
            h.set_lambda(lambda);
            h.set_alpha(alpha);
            h.set_mu(mu.0, mu.1);
            let mut live: Vec<Pointer<u32>> = Vec::new();
            let mut counter = 1u32;

            for round in 0..800 {
                if rand() % 100 < 60 || live.is_empty() {
                    let size = [8u32, 16, 16, 64, 250][(rand() % 5) as usize];
                    let id = if rand() % 4 == 0 {
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

                if round % 5 == 0 {
                    let Some(step) = h.propose_compaction_step(128) else {
                        continue;
                    };
                    assert!(step.to < step.from, "{step:?} is not downward");
                    let (run, _) = h.run_len_from(step.from, u64::MAX);
                    assert!(
                        run >= step.len,
                        "{step:?} spans a gap: the run from {} is only {run}",
                        step.from
                    );
                    h.commit_compaction_step(step);
                    h.assert_invariants();
                }
            }
            compact_fully(&mut h, 128);
            assert_eq!(
                h.len(),
                h.live_bytes(),
                "lambda={lambda} alpha={alpha} mu={mu:?}: gaps left over"
            );
        }
    }

    /// The worked example of `incremental-compaction.md` §4: distance-greed
    /// finds the cheap interior moves that a truncation-greedy policy misses,
    /// compacting the file in ~130 bytes where pure sliding would copy 1460.
    #[test]
    fn the_worked_example_compacts_without_lookahead() {
        // Lay out E1..E5 with gaps of 20, 100, 90, 100 between them by allocating
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
        let mut h = heap_with_layout(&plan);
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

    /// `λ` ships off, and turning it on rebuilds the index rather than mutating
    /// it -- the score is part of the key, so a half-converted index would hold
    /// entries nobody could remove. This checks the rebuild lands somewhere the
    /// invariants still hold, in both directions and with a non-trivial heap.
    #[test]
    fn toggling_lambda_rebuilds_the_index_consistently() {
        let plan: Vec<(u32, u32, bool)> = (1..=41)
            .map(|i| (i, 8 + (i % 7) * 11, i % 3 == 0))
            .collect();
        let mut h = heap_with_layout(&plan);
        assert!(!h.lambda(), "the size reward ships off");

        for lambda in [true, false, true] {
            h.set_lambda(lambda);
            assert_eq!(h.lambda(), lambda);
            h.assert_invariants();
        }
        // And it still compacts from there.
        h.set_lambda(true);
        compact_fully(&mut h, UNBOUNDED);
        assert_eq!(h.len(), h.live_bytes(), "λ must not stall compaction");
    }

    /// What the reward is *for*: at equal travel distance, prefer to move the
    /// larger allocation, because a step costs more than the bytes it copies.
    ///
    /// Constructing a genuine tie takes a little care, because the low wide gap
    /// that serves a large mover serves a small one too -- so the small mover's
    /// best distance is never *less* than the large one's simply by being
    /// higher. The two therefore need different destinations: a narrow gap at
    /// the bottom that only the small mover fits, and a wide one above it.
    #[test]
    fn lambda_breaks_a_tie_towards_the_larger_mover() {
        let plan = [
            (1, 8, true),   // gap    0..8    <- only the 8-byte mover fits
            (2, 42, false), //        8..50
            (3, 64, true),  // gap   50..114  <- the lowest gap fitting 64 bytes
            (4, 86, false), //      114..200
            (5, 8, false),  //      200..208  <- small mover: 200 - 0   = 200
            (6, 42, false), //      208..250
            (7, 64, false), //      250..314  <- large mover: 250 - 50  = 200
        ];
        let mut h = heap_with_layout(&plan);

        // Off, the two are genuinely indistinguishable and either is correct.
        let plain = h.propose_compaction_step(UNBOUNDED).unwrap();
        assert_eq!(plain.from - plain.to, 200, "both movers travel 200");
        assert!(
            plain.from == 200 || plain.from == 250,
            "expected one of the two tied movers, got {plain:?}"
        );

        // On, the reward `+A.size` separates them: 200 + 64 beats 200 + 8.
        h.set_lambda(true);
        assert_eq!(
            h.propose_compaction_step(UNBOUNDED).unwrap(),
            Step {
                from: 250,
                to: 50,
                len: 64
            },
            "the reward should break the tie towards the larger mover"
        );
        h.assert_invariants();
    }

    /// The bound `λ·reward(s) <= s` exists so that `f > 0` still implies a
    /// *downward* move. Without it an allocation whose only size-valid gaps sit
    /// above it could win outright and propose a step that raises `Φ` -- which
    /// `commit_compaction_step` asserts against, so this would be a panic rather
    /// than a silent regression.
    #[test]
    fn lambda_never_proposes_an_upward_move() {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut rand = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut h = Heap::new();
        h.set_lambda(true);
        let mut live: Vec<Pointer<u32>> = Vec::new();
        let mut counter = 1u32;

        for round in 0..600 {
            if rand() % 100 < 60 || live.is_empty() {
                let size = [1u32, 2, 8, 16, 250][(rand() % 5) as usize];
                let id = if rand() % 4 == 0 {
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
            // Sizes down to 1 byte are deliberate: the bound binds at the
            // *smallest* live allocation, so this is where it is tightest.
            if round % 5 == 0 {
                if let Some(step) = h.propose_compaction_step(128) {
                    assert!(step.to < step.from, "{step:?} raises the potential");
                    h.commit_compaction_step(step);
                    h.assert_invariants();
                }
            }
        }
    }

    #[test]
    fn a_slide_moves_a_whole_contiguous_run_as_one_step() {
        let mut h = Heap::new();
        h.alloc(resizable(1), 10).unwrap(); // 0..10, freed below
        h.alloc(resizable(2), 20).unwrap(); // 10..30
        h.alloc(resizable(3), 30).unwrap(); // 30..60
        h.free(resizable(1)).unwrap(); // gap 0..10

        // No allocation fits the 10-byte gap, so there is no evacuation at all
        // and the slide is the only candidate -- and it takes both at once.
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
                gaps: h.implied_gaps().len(),
                widest_gap: h.index.widest_gap().map_or(0, |(_, width)| width),
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

    /// Not an assertion of behaviour -- a **measurement**, of what compaction
    /// costs and what it achieves over a realistic churn, recorded in
    /// `test-results/`.
    ///
    /// Two regimes are reported separately, because they behave very
    /// differently: compaction in **bursts** interleaved with churn (what a
    /// backend does on each flush), and compaction driven to quiescence.
    ///
    /// Swept across `λ`, because that is the only way to settle stage 2: the
    /// reward buys a preference the potential does not express, so its effect on
    /// fragmentation has to be *measured* rather than argued for. The columns
    /// that decide it are `overhead` down the table and the `truncated` share of
    /// the bytes moved -- truncation is the only way free space leaves the file.
    ///
    /// `#[ignore]`d because the largest case takes minutes -- it is a
    /// measurement, not part of the suite. Run with:
    ///
    /// ```text
    /// cargo test --release -p kladde-heap --lib candidate_search_cost -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "measurement, not a behavioural test; takes minutes"]
    fn candidate_search_cost_over_a_churny_workload() {
        for (lambda, alpha, mu) in POLICIES {
            println!("\n\n########## lambda = {lambda}, alpha = {alpha}, mu = {mu:?} ##########");
            for &rounds in &[400usize, 4_000, 40_000] {
                measure_one(rounds, lambda, alpha, mu);
            }
        }
    }

    fn measure_one(rounds: usize, lambda: bool, alpha: u64, mu: (u64, u64)) {
        let mut h = Heap::new();
        h.set_lambda(lambda);
        h.set_alpha(alpha);
        h.set_mu(mu.0, mu.1);
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
        for (lambda, alpha, mu) in POLICIES {
            converges_under(lambda, alpha, mu);
        }
    }

    fn converges_under(lambda: bool, alpha: u64, mu: (u64, u64)) {
        // A tiny xorshift keeps this deterministic without a dev-dependency.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut rand = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        let mut h = Heap::new();
        h.set_lambda(lambda);
        h.set_alpha(alpha);
        h.set_mu(mu.0, mu.1);
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
