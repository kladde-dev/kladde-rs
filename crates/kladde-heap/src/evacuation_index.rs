//! [`EvacuationIndex`]: one augmented B+ tree over allocation and gap **sizes**
//! that keeps the best evacuation at its root, instead of searching for it.
//!
//! See `augmented-segment-tree.md`. The query it answers is
//!
//! > among all pairs `(A, G)` with `G.width >= A.size`, maximize
//! > `score(A) − G.pos`
//!
//! The index does not know what a score *is*; it maximizes whatever it is handed
//! in the key, and the caller decides. With `score(A) = A.pos` that is the best
//! evacuation under the potential `Φ = Σ_{live bytes} address` with cost measured
//! in bytes copied, where the size cancels and the per-byte gain is simply the
//! distance travelled. Richer scores (the size reward of stage 2, the gap-count
//! term of stage 3) are additive in `A` alone, which is exactly the condition for
//! leaving the merge below untouched -- see [`GainGreedyHeap::lambda`].
//!
//! [`GainGreedyHeap::lambda`]: crate::GainGreedyHeap::lambda
//!
//! # Why a size-keyed tree answers it in `O(1)`
//!
//! Sort every gap and every allocation onto one axis **by size** and split it
//! anywhere. Every gap above the split is wide enough for every allocation below
//! it -- the constraint holds across the entire cross product, for free, without
//! looking at an individual size. So the best pair drawn across a split is
//! obtained by maximizing the two sides *independently*:
//!
//! ```text
//! best crossing pair = (highest-addressed allocation below the split)
//!                    − (lowest-addressed gap above the split)
//! ```
//!
//! That is an `O(1)` combination of two aggregates, which is exactly what a
//! node of an augmented tree can maintain over its children.
//!
//! # The key, and why `is_gap` is a bit of it
//!
//! ```text
//! key = ( (size << 1) | is_gap ,  score ,  address )
//! ```
//!
//! **Ordering.** An allocation and a gap of the *same* size form a valid pair
//! (`G.width >= A.size` holds with equality), but under `(size, address)` alone
//! the gap could sort before the allocation and a merge that pairs allocations
//! with gaps to their right would never see it. With `is_gap` in the low bit,
//! allocations precede gaps at equal size and
//!
//! > `key(G) > key(A)` **iff** `G.width >= A.size`.
//!
//! The key order *is* the validity relation.
//!
//! **Identification.** [`Argument::from_leaf`] is handed the keys and nothing
//! else, and the merge treats the two kinds completely differently -- one feeds
//! `min_gap_pos`, the other `max_alloc_score`. So the key has to say which kind
//! an entry is. Inflating gap widths by one would buy the ordering and lose
//! this, since `width + 1` has arbitrary parity; against this interface that
//! alternative is not merely less exact but unimplementable.
//!
//! The size field is `u64` even though allocation sizes are `u32`, because gap
//! widths already are -- so the shifted flag costs nothing real. The formal
//! ceiling moves from `2^64 − 1` to `2^63 − 1` on *gap width*, an eight-exabyte
//! gap; allocation sizes are untouched.

use sweep_bptree::argument::Argument;
use sweep_bptree::tree::visit::{DescendVisit, DescendVisitResult};
use sweep_bptree::{BPlusTree, NodeStoreVec};

use crate::heap::Step;

/// The concrete tree behind [`EvacuationIndex`].
type Store = NodeStoreVec<Key, (), Aggregate>;

/// One entry: an allocation, or a gap.
///
/// Ordered lexicographically by `(sized, score, addr)`, which is the whole point
/// -- see the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    /// `(size << 1) | is_gap`.
    sized: u64,
    /// What the merge maximizes over allocations: `A.pos` at this stage, plus the
    /// reward and gap-count terms of later stages. For a gap, `G.pos`.
    score: u64,
    /// Start address. Last, so that entries sharing a size and a score are
    /// address-ordered -- which is what makes "the lowest gap of width `L`" a
    /// contiguous run rather than a scattered set.
    addr: u64,
}

impl Key {
    /// An allocation of `size` bytes at `addr`, scoring `score`.
    pub fn alloc(addr: u64, size: u32, score: u64) -> Self {
        Self {
            sized: u64::from(size) << 1,
            score,
            addr,
        }
    }

    /// A gap of `width` bytes at `pos`. Its score is its position: a gap is only
    /// ever a destination, and the merge subtracts it.
    pub fn gap(pos: u64, width: u64) -> Self {
        Self {
            sized: (width << 1) | 1,
            score: pos,
            addr: pos,
        }
    }

    fn is_gap(self) -> bool {
        self.sized & 1 == 1
    }

    fn size(self) -> u64 {
        self.sized >> 1
    }
}

/// What every subtree summarizes about itself.
///
/// All of it is `u64` and every difference is formed with `saturating_sub`,
/// which is not a detail: plain subtraction would wrap an *upward* pair into a
/// huge positive number and poison the maximum. With saturation,
/// `0.saturating_sub(x) == 0` and `x.saturating_sub(u64::MAX) == 0`, so the
/// identities below behave and every non-beneficial pair collapses to `0`.
///
/// Using `0` rather than `−∞` as the identity for `max_alloc_score` conflates
/// "no allocation here" with "an allocation scoring 0", which is exact rather
/// than merely tolerable: an allocation at address 0 with no bonus cannot travel
/// down at all. And `best == 0` is unambiguously "nothing beneficial", because a
/// gain of exactly zero is impossible -- a gap and an allocation cannot start at
/// the same address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Aggregate {
    /// Lowest `G.pos` over gaps in the subtree.
    ///
    /// At the root this is the **compaction frontier**: everything below it is
    /// at its final address, since the gapless layout moves every byte above the
    /// lowest gap down and every byte below it not at all. That is what
    /// `lowest_gap()` reads, and it is the slide's destination -- see
    /// [`GainGreedyHeap::slide_candidate`].
    ///
    /// It doubles as the "does this subtree hold a gap at all" predicate
    /// (`!= u64::MAX`), which is what [`WidestGap`]'s descent steers by.
    ///
    /// [`GainGreedyHeap::slide_candidate`]: crate::GainGreedyHeap
    min_gap_pos: u64,
    /// Highest `G.pos` over gaps in the subtree. At the root, the gap the top
    /// run sits on -- the only gap whose closure retires `end`, which is the
    /// quantity the potential is blind to. See
    /// [`GainGreedyHeap::end_slide_candidate`].
    ///
    /// Its identity is `0`, which is *not* a usable "no gap here" sentinel,
    /// because a gap at address 0 is perfectly ordinary. `min_gap_pos` carries
    /// the emptiness predicate for both, and `highest_gap()` reads this field
    /// only once that says a gap exists.
    ///
    /// [`GainGreedyHeap::end_slide_candidate`]: crate::GainGreedyHeap
    max_gap_pos: u64,
    /// Highest `score` over allocations in the subtree, and the allocation
    /// achieving it. The witness rides along because the merge has to be able to
    /// name the mover of a crossing pair, and `from_inner` never sees keys of
    /// the entries beneath it -- only its children's aggregates.
    max_alloc_score: u64,
    max_alloc_addr: u64,
    max_alloc_size: u32,
    /// Best `score(A) − G.pos` over valid pairs lying *entirely inside* this
    /// subtree, and the pair achieving it.
    best: u64,
    best_from: u64,
    best_to: u64,
    best_len: u32,
}

impl Default for Aggregate {
    fn default() -> Self {
        Self {
            min_gap_pos: u64::MAX,
            max_gap_pos: 0,
            max_alloc_score: 0,
            max_alloc_addr: 0,
            max_alloc_size: 0,
            best: 0,
            best_from: 0,
            best_to: 0,
            best_len: 0,
        }
    }
}

impl Aggregate {
    /// The aggregate of a subtree holding one entry.
    fn of(key: Key) -> Self {
        let mut a = Self::default();
        if key.is_gap() {
            a.min_gap_pos = key.score;
            a.max_gap_pos = key.score;
        } else {
            a.max_alloc_score = key.score;
            a.max_alloc_addr = key.addr;
            a.max_alloc_size = key.size() as u32;
        }
        a
    }

    /// Fold in a sibling whose keys are all **strictly smaller** than every key
    /// already folded in. This is the whole merge, used at every level: over a
    /// leaf's entries, over an inner node's children, and over the canonical
    /// subtrees a budgeted query collects.
    ///
    /// A pair inside the combined range either lives entirely in one side or
    /// crosses it -- allocation on the low-size side, gap on the high-size side
    /// -- and every crossing pair is valid by the key-order property, so the
    /// crossing case is `lower.max_alloc_score − self.min_gap_pos` and nothing
    /// else needs checking.
    fn extend_left(&mut self, lower: &Aggregate) {
        let crossing = lower.max_alloc_score.saturating_sub(self.min_gap_pos);
        if crossing > self.best {
            self.best = crossing;
            self.best_from = lower.max_alloc_addr;
            self.best_len = lower.max_alloc_size;
            self.best_to = self.min_gap_pos;
        }
        if lower.best > self.best {
            self.best = lower.best;
            self.best_from = lower.best_from;
            self.best_len = lower.best_len;
            self.best_to = lower.best_to;
        }

        if lower.min_gap_pos < self.min_gap_pos {
            self.min_gap_pos = lower.min_gap_pos;
        }
        if lower.max_gap_pos > self.max_gap_pos {
            self.max_gap_pos = lower.max_gap_pos;
        }
        if lower.max_alloc_score > self.max_alloc_score {
            self.max_alloc_score = lower.max_alloc_score;
            self.max_alloc_addr = lower.max_alloc_addr;
            self.max_alloc_size = lower.max_alloc_size;
        }
    }

    /// The step this aggregate's `best` describes, with the objective value it
    /// scored -- which is *not* the travel distance once the score carries more
    /// than the address, so the caller must not re-derive it from the step.
    fn scored_step(&self) -> Option<(u64, Step<u64>)> {
        (self.best > 0).then(|| {
            (
                self.best,
                Step {
                    from: self.best_from,
                    to: self.best_to,
                    len: u64::from(self.best_len),
                },
            )
        })
    }

    /// Fold a run of siblings in descending key order -- the right-to-left sweep.
    fn sweep<'a>(seed: Aggregate, descending: impl Iterator<Item = &'a Aggregate>) -> Aggregate {
        let mut acc = seed;
        for lower in descending {
            acc.extend_left(lower);
        }
        acc
    }
}

impl Argument<Key> for Aggregate {
    fn from_leaf(keys: &[Key]) -> Self {
        let singles: Vec<Aggregate> = keys.iter().map(|&k| Aggregate::of(k)).collect();
        Aggregate::sweep(Aggregate::default(), singles.iter().rev())
    }

    fn from_inner(_keys: &[Key], arguments: &[Self]) -> Self {
        Aggregate::sweep(Aggregate::default(), arguments.iter().rev())
    }
}

/// The evacuation index. See the module docs.
pub struct EvacuationIndex {
    tree: BPlusTree<Store>,
}

impl Default for EvacuationIndex {
    fn default() -> Self {
        Self {
            tree: BPlusTree::new(Store::default()),
        }
    }
}

impl Clone for EvacuationIndex {
    /// `sweep-bptree`'s node store is not `Clone`, so the tree is rebuilt -- but
    /// from already-sorted data, which `bulk_load` turns into a linear fill with
    /// no comparisons and no node splits. Only the benchmark harness clones a
    /// heap, and it does so outside the timed region.
    fn clone(&self) -> Self {
        let data: Vec<(Key, ())> = self.tree.iter().map(|(&k, _)| (k, ())).collect();
        Self {
            tree: BPlusTree::bulk_load(data),
        }
    }
}

impl EvacuationIndex {
    pub fn insert(&mut self, key: Key) {
        self.tree.insert(key, ());
    }

    pub fn remove(&mut self, key: Key) {
        let gone = self.tree.remove(&key);
        debug_assert!(gone.is_some(), "removing an entry the index never held");
    }

    /// Every entry, in key order.
    pub fn iter(&self) -> impl Iterator<Item = Key> + '_ {
        self.tree.iter().map(|(&k, _)| k)
    }

    /// The best evacuation in the whole heap, as `(objective value, step)`. One
    /// field read at the root.
    pub fn best_evacuation(&self) -> Option<(u64, Step<u64>)> {
        self.tree.root_argument().scored_step()
    }

    /// The **compaction frontier**: the lowest-addressed gap in the heap, or
    /// `None` when the heap is gapless. A root read.
    ///
    /// This is the slide's destination. Everything below it is already at its
    /// final address, so a slide into it settles the bytes it moves for good --
    /// which is the whole reason compaction terminates in a linear number of
    /// copied bytes rather than a quadratic one. See
    /// [`GainGreedyHeap::slide_candidate`].
    ///
    /// [`GainGreedyHeap::slide_candidate`]: crate::GainGreedyHeap
    pub fn lowest_gap(&self) -> Option<u64> {
        let pos = self.tree.root_argument().min_gap_pos;
        (pos != u64::MAX).then_some(pos)
    }

    /// The highest-addressed gap, or `None` when the heap is gapless. A root
    /// read.
    ///
    /// This is the only gap the top run sits on, so it is the only one whose
    /// closure lets `end` retreat -- the destination of
    /// [`GainGreedyHeap::end_slide_candidate`].
    ///
    /// [`GainGreedyHeap::end_slide_candidate`]: crate::GainGreedyHeap
    pub fn highest_gap(&self) -> Option<u64> {
        let a = self.tree.root_argument();
        (a.min_gap_pos != u64::MAX).then_some(a.max_gap_pos)
    }

    /// The widest gap, as `(pos, width)`, taking the **highest**-addressed one
    /// when several are equally wide.
    ///
    /// Nothing in the compactor needs this any more -- it is a diagnostic, read
    /// by the measurement harness and by `assert_consistent`. So it is a descent
    /// (`O(B log_B n)`) rather than the two extra `Aggregate` fields it used to
    /// be, which were paid for on every merge at every level of every update.
    /// Cold-path cost in exchange for a smaller hot path.
    pub fn widest_gap(&self) -> Option<(u64, u64)> {
        self.tree.descend_visit(WidestGap)
    }

    /// The lowest-addressed gap at least `min_len` bytes wide.
    ///
    /// A suffix aggregate: every gap with `width >= min_len` sits at or above the
    /// key `((min_len << 1) | 1, 0, 0)`, so this is `min_gap_pos` over that
    /// suffix -- one descent, collecting the children that lie wholly to the
    /// right of the path.
    pub fn lowest_gap_fitting(&self, min_len: u64) -> Option<u64> {
        let found = self
            .tree
            .descend_visit(SuffixMinGapPos {
                boundary: Key::gap(0, min_len),
                acc: u64::MAX,
            })
            .unwrap_or(u64::MAX);
        (found != u64::MAX).then_some(found)
    }

    /// The best evacuation whose mover is at most `budget` bytes -- **exactly**,
    /// not as a heuristic.
    ///
    /// Because the tree is keyed by size, the budget is a prefix of the key
    /// order. One descent along the boundary splits every node it passes into
    /// children wholly below it, children wholly above it, and the one the path
    /// continues into; the union of the "wholly below" children over all levels
    /// is exactly the prefix, and of the "wholly above" ones exactly the suffix.
    ///
    /// The whole suffix then collapses to a **single scalar**: every allocation
    /// in the prefix has `A.size <= budget` and every gap in the suffix has
    /// `G.width > budget`, so every prefix-allocation/suffix-gap pair is valid
    /// without further checking and only the suffix's lowest gap can matter.
    /// Seeding the ordinary right-to-left sweep of the prefix with that scalar is
    /// what offers each prefix allocation both the gaps above it *within* the
    /// prefix and the best gap in the entire suffix.
    pub fn best_evacuation_within(&self, budget: u64) -> Option<(u64, Step<u64>)> {
        // Sizes are `u32`, so a budget at or above that ceiling constrains
        // nothing -- and taking the root read here also keeps `budget + 1` from
        // overflowing the shifted key below.
        if budget >= u64::from(u32::MAX) {
            return self.best_evacuation();
        }
        let mut visit = BudgetedBest {
            // The first key of any entry whose size exceeds the budget.
            boundary: Key::alloc(0, 0, 0).with_sized((budget + 1) << 1),
            prefix: Vec::new(),
            suffix_min_gap_pos: u64::MAX,
        };
        self.tree.descend_visit(&mut visit);
        let seed = Aggregate {
            min_gap_pos: visit.suffix_min_gap_pos,
            ..Aggregate::default()
        };
        Aggregate::sweep(seed, visit.prefix.iter().rev()).scored_step()
    }
}

impl Key {
    fn with_sized(mut self, sized: u64) -> Self {
        self.sized = sized;
        self
    }
}

/// Descends to the last gap in key order, which -- since the key leads with
/// `(width << 1) | 1` -- is the widest one.
///
/// The steering predicate is `min_gap_pos != u64::MAX`, "this subtree holds a
/// gap": entering the *rightmost* child that holds one and repeating is exactly
/// a descent to the last gap. Allocations are not in the way even though a large
/// allocation outranks a small gap, because a child holding only allocations is
/// skipped outright.
struct WidestGap;

impl DescendVisit<Key, (), Aggregate> for WidestGap {
    type Result = (u64, u64);

    fn visit_inner(
        &mut self,
        _keys: &[Key],
        arguments: &[Aggregate],
    ) -> DescendVisitResult<Self::Result> {
        match arguments.iter().rposition(|a| a.min_gap_pos != u64::MAX) {
            Some(child) => DescendVisitResult::GoDown(child),
            None => DescendVisitResult::Cancel,
        }
    }

    fn visit_leaf(&mut self, keys: &[Key], _values: &[()]) -> Option<Self::Result> {
        keys.iter()
            .rfind(|k| k.is_gap())
            .map(|k| (k.score, k.size()))
    }
}

/// Collects `min_gap_pos` over the key suffix at or above `boundary`.
struct SuffixMinGapPos {
    boundary: Key,
    acc: u64,
}

/// The child index the descent continues into: every key at or above `boundary`
/// lives in that child or to its right.
///
/// `sweep-bptree` sends an exact separator match to the *right* child, so the
/// path is the number of separators less than **or equal to** the boundary --
/// not `partition_point(< boundary)`, which would step left of the match and
/// miss the whole suffix.
fn path_to(keys: &[Key], boundary: Key) -> usize {
    keys.partition_point(|k| *k <= boundary)
}

impl DescendVisit<Key, (), Aggregate> for SuffixMinGapPos {
    type Result = u64;

    fn visit_inner(&mut self, keys: &[Key], arguments: &[Aggregate]) -> DescendVisitResult<u64> {
        let path = path_to(keys, self.boundary);
        for a in &arguments[path + 1..] {
            self.acc = self.acc.min(a.min_gap_pos);
        }
        DescendVisitResult::GoDown(path)
    }

    fn visit_leaf(&mut self, keys: &[Key], _values: &[()]) -> Option<u64> {
        let cut = keys.partition_point(|k| *k < self.boundary);
        for k in &keys[cut..] {
            if k.is_gap() {
                self.acc = self.acc.min(k.score);
            }
        }
        Some(self.acc)
    }
}

/// Collects the canonical prefix subtrees and the suffix's lowest gap.
struct BudgetedBest {
    boundary: Key,
    /// The canonical subtrees covering `[0, boundary)`, in **ascending** key
    /// order. A descent visits shallower levels first, and everything collected
    /// at a shallower level sits to the left of everything collected below it,
    /// so appending in descent order already gives that order.
    prefix: Vec<Aggregate>,
    suffix_min_gap_pos: u64,
}

impl DescendVisit<Key, (), Aggregate> for &mut BudgetedBest {
    type Result = ();

    fn visit_inner(&mut self, keys: &[Key], arguments: &[Aggregate]) -> DescendVisitResult<()> {
        let path = path_to(keys, self.boundary);
        self.prefix.extend_from_slice(&arguments[..path]);
        for a in &arguments[path + 1..] {
            self.suffix_min_gap_pos = self.suffix_min_gap_pos.min(a.min_gap_pos);
        }
        DescendVisitResult::GoDown(path)
    }

    fn visit_leaf(&mut self, keys: &[Key], _values: &[()]) -> Option<()> {
        let cut = keys.partition_point(|k| *k < self.boundary);
        self.prefix
            .extend(keys[..cut].iter().map(|&k| Aggregate::of(k)));
        for k in &keys[cut..] {
            if k.is_gap() {
                self.suffix_min_gap_pos = self.suffix_min_gap_pos.min(k.score);
            }
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A heap layout as `(address, size)` allocations and `(pos, width)` gaps.
    #[derive(Default, Clone)]
    struct Layout {
        allocs: Vec<(u64, u32)>,
        gaps: Vec<(u64, u64)>,
        /// Stage 2's size reward, as the bool the heap actually ships. The index
        /// itself knows nothing about it -- it maximizes whatever score it is
        /// handed -- so this rides along only to build keys and to tell the
        /// brute-force reference what it is checking against.
        lambda: bool,
    }

    impl Layout {
        fn score(&self, addr: u64, size: u32) -> u64 {
            if self.lambda {
                addr + u64::from(size)
            } else {
                addr
            }
        }

        fn index(&self) -> EvacuationIndex {
            let mut ix = EvacuationIndex::default();
            for &(addr, size) in &self.allocs {
                ix.insert(Key::alloc(addr, size, self.score(addr, size)));
            }
            for &(pos, width) in &self.gaps {
                ix.insert(Key::gap(pos, width));
            }
            ix
        }

        /// The reference every query below is checked against: every
        /// (allocation, gap) pair, no index and no pruning.
        fn best_by_brute_force(&self, budget: u64) -> Option<u64> {
            let mut best = 0u64;
            for &(from, size) in &self.allocs {
                if u64::from(size) > budget {
                    continue;
                }
                for &(pos, width) in &self.gaps {
                    if width >= u64::from(size) && pos < from {
                        best = best.max(self.score(from, size) - pos);
                    }
                }
            }
            (best > 0).then_some(best)
        }

        fn lowest_gap_fitting_by_scan(&self, min_len: u64) -> Option<u64> {
            self.gaps
                .iter()
                .filter(|&&(_, width)| width >= min_len)
                .map(|&(pos, _)| pos)
                .min()
        }

        fn widest_gap_by_scan(&self) -> Option<(u64, u64)> {
            self.gaps.iter().copied().max_by_key(|&(pos, w)| (w, pos))
        }

        fn lowest_gap_by_scan(&self) -> Option<u64> {
            self.gaps.iter().map(|&(pos, _)| pos).min()
        }

        fn highest_gap_by_scan(&self) -> Option<u64> {
            self.gaps.iter().map(|&(pos, _)| pos).max()
        }
    }

    /// A deterministic xorshift, so every case is reproducible run to run.
    fn rng(seed: u64) -> impl FnMut() -> u64 {
        let mut state = seed;
        move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        }
    }

    /// A plausible heap: allocations and gaps alternating up the address space,
    /// with sizes skewed to a few classes the way kladde's would be.
    fn random_layout(rand: &mut impl FnMut() -> u64, n: usize) -> Layout {
        let mut layout = Layout::default();
        let mut cursor = 0u64;
        for _ in 0..n {
            let size = [8u32, 16, 16, 64, 250][(rand() % 5) as usize];
            layout.allocs.push((cursor, size));
            cursor += u64::from(size);
            if rand().is_multiple_of(3) {
                let width = 1 + rand() % 300;
                layout.gaps.push((cursor, width));
                cursor += width;
            }
        }
        layout
    }

    #[test]
    fn an_empty_index_answers_nothing() {
        let ix = EvacuationIndex::default();
        assert_eq!(ix.best_evacuation(), None);
        assert_eq!(ix.best_evacuation_within(4096), None);
        assert_eq!(ix.widest_gap(), None);
        assert_eq!(ix.lowest_gap_fitting(1), None);
    }

    #[test]
    fn a_gap_exactly_as_wide_as_the_allocation_is_a_valid_destination() {
        // The case the `is_gap` bit exists for: at equal size the allocation must
        // sort first, or the merge never sees the pair.
        let layout = Layout {
            allocs: vec![(100, 10)],
            gaps: vec![(0, 10)],
            lambda: false,
        };
        assert_eq!(
            layout.index().best_evacuation().map(|(_, s)| s),
            Some(Step {
                from: 100,
                to: 0,
                len: 10
            })
        );
    }

    #[test]
    fn a_gap_one_byte_too_narrow_is_not() {
        let layout = Layout {
            allocs: vec![(100, 10)],
            gaps: vec![(0, 9)],
            lambda: false,
        };
        assert_eq!(layout.index().best_evacuation(), None);
    }

    #[test]
    fn an_upward_pair_is_never_proposed() {
        // The only gap sits above the only allocation, so the sole candidate
        // would raise `Φ`. Saturation is what collapses it to zero.
        let layout = Layout {
            allocs: vec![(0, 10)],
            gaps: vec![(50, 100)],
            lambda: false,
        };
        assert_eq!(layout.index().best_evacuation(), None);
    }

    #[test]
    fn the_furthest_travelling_pair_wins_not_the_largest_or_the_lowest() {
        let layout = Layout {
            allocs: vec![(10, 8), (500, 4), (900, 100)],
            gaps: vec![(0, 4), (200, 100)],
            lambda: false,
        };
        // 900 -> 200 travels 700; 500 -> 0 travels 500; 900 -> 0 is invalid
        // (a 100-byte allocation does not fit a 4-byte gap).
        assert_eq!(
            layout.index().best_evacuation().map(|(_, s)| s),
            Some(Step {
                from: 900,
                to: 200,
                len: 100
            })
        );
    }

    #[test]
    fn the_widest_gap_query_finds_the_widest_past_larger_allocations() {
        let layout = Layout {
            // The 4096-byte allocation outranks every gap in key order, so a
            // descent that merely walked right would land on it.
            allocs: vec![(10, 8), (2000, 4096)],
            gaps: vec![(300, 64), (100, 64), (500, 8)],
            lambda: false,
        };
        // Widest, and among equally wide the highest-addressed.
        assert_eq!(layout.index().widest_gap(), Some((300, 64)));
    }

    #[test]
    fn the_frontier_is_the_lowest_gap_whatever_its_width() {
        let layout = Layout {
            allocs: vec![(10, 8)],
            // The lowest gap is the narrowest one: width must not enter into it.
            gaps: vec![(300, 64), (100, 1), (500, 8)],
            lambda: false,
        };
        assert_eq!(layout.index().lowest_gap(), Some(100));
        assert_eq!(layout.index().highest_gap(), Some(500));
        assert_eq!(EvacuationIndex::default().lowest_gap(), None);
        assert_eq!(EvacuationIndex::default().highest_gap(), None);
    }

    #[test]
    fn the_budget_excludes_movers_that_are_too_large() {
        let layout = Layout {
            allocs: vec![(900, 100), (500, 4)],
            gaps: vec![(0, 4), (200, 100)],
            lambda: false,
        };
        let ix = layout.index();
        // Unconstrained, the 100-byte allocation travelling 700 wins.
        assert_eq!(
            ix.best_evacuation_within(4096).map(|(_, s)| s.from),
            Some(900)
        );
        // At a budget of 99 it is out of reach, and the 4-byte one is all that
        // is left -- into the *lowest* gap that fits it, not the nearest.
        assert_eq!(
            ix.best_evacuation_within(99).map(|(_, s)| s),
            Some(Step {
                from: 500,
                to: 0,
                len: 4
            })
        );
        assert_eq!(ix.best_evacuation_within(3), None);
    }

    /// The budgeted descent is the part that could silently lose a candidate:
    /// it reassembles the answer from canonical subtrees collected at every
    /// level, and a single off-by-one in the prefix/suffix split would drop a
    /// whole subtree without any other symptom.
    #[test]
    fn every_query_agrees_with_brute_force_across_deep_trees() {
        let mut rand = rng(0x51E7_E123_4F6C_DD1D);
        for lambda in [false, true] {
            for n in [1usize, 2, 5, 60, 65, 200, 2_000] {
                let mut layout = random_layout(&mut rand, n);
                layout.lambda = lambda;
                let ix = layout.index();

                assert_eq!(
                    ix.best_evacuation().map(|(gain, _)| gain),
                    layout.best_by_brute_force(u64::MAX),
                    "lambda={lambda} n={n}: the root disagreed with brute force"
                );
                assert_eq!(
                    ix.widest_gap(),
                    layout.widest_gap_by_scan(),
                    "lambda={lambda} n={n}: widest_gap disagreed with a scan"
                );
                assert_eq!(
                    ix.lowest_gap(),
                    layout.lowest_gap_by_scan(),
                    "lambda={lambda} n={n}: lowest_gap disagreed with a scan"
                );
                assert_eq!(
                    ix.highest_gap(),
                    layout.highest_gap_by_scan(),
                    "lambda={lambda} n={n}: highest_gap disagreed with a scan"
                );

                for budget in [0u64, 1, 8, 15, 16, 63, 64, 249, 250, 251, 10_000] {
                    assert_eq!(
                        ix.best_evacuation_within(budget).map(|(gain, _)| gain),
                        layout.best_by_brute_force(budget),
                        "lambda={lambda} n={n} budget={budget}: the budgeted descent disagreed"
                    );
                }
                for min_len in [1u64, 2, 8, 16, 64, 250, 300, 301, 1_000] {
                    assert_eq!(
                        ix.lowest_gap_fitting(min_len),
                        layout.lowest_gap_fitting_by_scan(min_len),
                        "lambda={lambda} n={n} min_len={min_len}: lowest_gap_fitting disagreed"
                    );
                }
            }
        }
    }

    /// The sign test has to stay exact once the score carries the reward: an
    /// *upward* pair now earns `+A.size` and could in principle come out
    /// positive, which would propose a move that raises `Φ`. The bound
    /// `lambda*reward(s) <= s` is what forbids it, and `reward(s) = s` with
    /// `lambda` a bool sits exactly at its edge.
    #[test]
    fn the_reward_can_never_rescue_an_upward_pair() {
        let mut rand = rng(0xD15E_A5E1_0BAD_C0DE);
        for _ in 0..20 {
            let mut layout = random_layout(&mut rand, 200);
            layout.lambda = true;
            // Keep only the gaps above every allocation, so *no* downward pair
            // exists at all and any proposal is a bug.
            let top = layout.allocs.iter().map(|&(a, _)| a).max().unwrap();
            layout.gaps.retain(|&(pos, _)| pos > top);
            if layout.gaps.is_empty() {
                continue;
            }
            assert_eq!(
                layout.index().best_evacuation(),
                None,
                "an upward pair was proposed"
            );
        }
    }

    /// A returned step must be one the caller can actually commit: downward,
    /// within budget, and landing in a gap that really fits it.
    #[test]
    fn every_proposed_step_is_valid_and_within_budget() {
        let mut rand = rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..40 {
            let layout = random_layout(&mut rand, 300);
            let ix = layout.index();
            for budget in [4u64, 16, 64, 250, 4_096] {
                let Some((_, step)) = ix.best_evacuation_within(budget) else {
                    continue;
                };
                assert!(step.to < step.from, "{step:?} is not downward");
                assert!(step.len <= budget, "{step:?} exceeds budget {budget}");
                assert!(
                    layout.allocs.contains(&(step.from, step.len as u32)),
                    "{step:?} moves something that is not there"
                );
                assert!(
                    layout
                        .gaps
                        .iter()
                        .any(|&(pos, width)| pos == step.to && width >= step.len),
                    "{step:?} lands in no gap wide enough"
                );
            }
        }
    }

    /// Removal has to promote the next-best answer, which is where a stale
    /// augmentation shows up rather than a stale entry.
    #[test]
    fn repeated_removal_keeps_the_root_honest() {
        let mut rand = rng(0x2545_F491_4F6C_DD1D);
        let mut layout = random_layout(&mut rand, 500);
        let mut ix = layout.index();

        while let Some((gain, step)) = ix.best_evacuation() {
            assert_eq!(
                Some(gain),
                layout.best_by_brute_force(u64::MAX),
                "the root drifted from brute force after removals"
            );
            // Retire the winning mover from both the index and the reference.
            let size = step.len as u32;
            ix.remove(Key::alloc(step.from, size, layout.score(step.from, size)));
            layout.allocs.retain(|&(a, _)| a != step.from);
        }
        assert_eq!(layout.best_by_brute_force(u64::MAX), None);
    }

    #[test]
    fn clone_rebuilds_the_augmentation_not_just_the_entries() {
        let mut rand = rng(0x0BAD_C0DE_D15E_A5E1);
        let layout = random_layout(&mut rand, 1_000);
        let ix = layout.index();
        let cloned = ix.clone();

        assert_eq!(
            cloned.iter().collect::<Vec<_>>(),
            ix.iter().collect::<Vec<_>>()
        );
        // `bulk_load` recomputes the augmentation, so query it rather than
        // trusting that equal entries imply equal aggregates.
        assert_eq!(cloned.best_evacuation(), ix.best_evacuation());
        assert_eq!(cloned.widest_gap(), ix.widest_gap());
        for budget in [4u64, 64, 250, 4_096] {
            assert_eq!(
                cloned.best_evacuation_within(budget),
                ix.best_evacuation_within(budget)
            );
        }
    }
}
