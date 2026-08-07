//! [`GapTree`]: an address-ordered index of the heap's gaps, augmented with each
//! subtree's **maximum gap length**, so that "the lowest-addressed gap at least
//! `n` bytes wide" is one `O(log n)` descent rather than a scan.
//!
//! That query is what the gain analysis of `incremental-compaction.md` §1 asks
//! for on every candidate: among gaps big enough to take a mover, the *lowest*
//! one maximizes the travel distance, and travel distance is the per-byte gain.
//! It is a 2-D dominance query -- minimize address subject to a size bound --
//! which neither an address-keyed nor a size-keyed map answers on its own.
//!
//! # Why gaps are entries here, not the space between them
//!
//! The obvious alternative is to augment the *allocation* map and leave gaps
//! implicit, which stores nothing new. It is a much sharper knife:
//!
//! - A gap can straddle the boundary between two children, so it belongs to no
//!   child's subtree. The augmentation then has to carry `(min_start, max_end)`
//!   alongside the maximum, and the descent has to thread the left-hand boundary
//!   down through the query to reconstruct those crossing gaps.
//! - The leading gap (from address 0 to the first allocation) has no left
//!   neighbour at all and needs handling outside the tree.
//! - Empty subtrees have to be distinguished from zero-length ones, or the
//!   crossing-gap subtraction underflows.
//!
//! Materializing gaps as entries makes every one of those disappear: the
//! augmentation is a plain bottom-up maximum of a value each entry already
//! carries, and the "lowest qualifying" descent is "first child whose maximum
//! qualifies". The cost is one index, which the heap already paid anyway -- it
//! keeps `free_by_size` for exact-fit lookups regardless.
//!
//! # Why the length lives in the key
//!
//! [`SearchArgument::locate_in_leaf`] receives only the leaf's **keys**, never
//! its values, so anything the leaf-level search must read has to be part of the
//! key. Hence the internal `Gap` key carries `len` but orders and compares
//! purely by `start`.

use sweep_bptree::argument::{Argument, SearchArgument};
use sweep_bptree::BPlusTreeMap;

/// A gap in the address space: free bytes `[start, start + len)`.
///
/// Ordered **by `start` alone** -- `len` rides along only so the leaf-level
/// search can see it (see the module docs). That is also what makes
/// `Borrow<u64>` sound: the borrowed ordering agrees with the full one.
#[derive(Clone, Copy, Debug)]
struct Gap {
    start: u64,
    len: u64,
}

impl PartialEq for Gap {
    fn eq(&self, other: &Self) -> bool {
        self.start == other.start
    }
}
impl Eq for Gap {}
impl PartialOrd for Gap {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Gap {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.start.cmp(&other.start)
    }
}
impl std::borrow::Borrow<u64> for Gap {
    fn borrow(&self) -> &u64 {
        &self.start
    }
}

/// The longest gap anywhere in a subtree. `0` for an empty subtree, which is
/// indistinguishable from "no gap qualifies" -- sound because a recorded gap is
/// never zero-length and every query asks for at least one byte.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MaxGapLen(u64);

impl Argument<Gap> for MaxGapLen {
    fn from_leaf(keys: &[Gap]) -> Self {
        Self(keys.iter().map(|gap| gap.len).max().unwrap_or(0))
    }

    fn from_inner(_keys: &[Gap], arguments: &[Self]) -> Self {
        Self(arguments.iter().map(|a| a.0).max().unwrap_or(0))
    }
}

impl SearchArgument<Gap> for MaxGapLen {
    /// The minimum acceptable gap length. Callers must pass at least 1; see
    /// [`GapTree::lowest_fitting`], which enforces it.
    type Query = u64;

    fn locate_in_leaf(min_len: u64, keys: &[Gap]) -> Option<usize> {
        keys.iter().position(|gap| gap.len >= min_len)
    }

    fn locate_in_inner(
        min_len: u64,
        _keys: &[Gap],
        arguments: &[Self],
    ) -> Option<(usize, Self::Query)> {
        // Children are in ascending address order, so the *first* whose subtree
        // contains a long-enough gap is the one holding the lowest such gap.
        // Descending into it can never dead-end: its maximum is a real entry's
        // length, and `locate_in_leaf` accepts on the same predicate.
        arguments
            .iter()
            .position(|a| a.0 >= min_len)
            .map(|child| (child, min_len))
    }
}

/// The gap index. See the module docs.
pub struct GapTree {
    tree: BPlusTreeMap<Gap, (), MaxGapLen>,
}

impl Default for GapTree {
    fn default() -> Self {
        Self {
            tree: BPlusTreeMap::new(),
        }
    }
}

impl Clone for GapTree {
    /// Rebuilt entry by entry: `sweep-bptree`'s node store is not itself
    /// `Clone`, so this is `O(n log n)` rather than a copy of the arena. Only
    /// the benchmark harness clones a heap, and it does so outside the timed
    /// region.
    fn clone(&self) -> Self {
        let mut cloned = Self::default();
        for (start, len) in self.iter() {
            cloned.insert(start, len);
        }
        cloned
    }
}

impl GapTree {
    /// Record a gap. Zero-length gaps are not gaps and are ignored.
    pub fn insert(&mut self, start: u64, len: u64) {
        if len > 0 {
            self.tree.insert(Gap { start, len }, ());
        }
    }

    /// Forget the gap starting at `start`, if any.
    pub fn remove(&mut self, start: u64) {
        self.tree.remove(&start);
    }

    /// The lowest-addressed gap at least `min_len` bytes wide, as
    /// `(start, len)`. One descent, `O(log n)`.
    pub fn lowest_fitting(&self, min_len: u64) -> Option<(u64, u64)> {
        // A query of 0 would match empty subtrees (whose maximum is also 0) and
        // could dead-end in one; every real request is for at least one byte.
        self.tree
            .get_by_argument(min_len.max(1))
            .map(|(gap, ())| (gap.start, gap.len))
    }

    /// Number of gaps.
    pub fn len(&self) -> usize {
        self.tree.len()
    }

    /// Whether there are no gaps at all.
    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    /// Every gap, in ascending address order.
    pub fn iter(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.tree.iter().map(|(gap, ())| (gap.start, gap.len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference the descent must agree with.
    fn lowest_fitting_by_scan(gaps: &[(u64, u64)], min_len: u64) -> Option<(u64, u64)> {
        gaps.iter()
            .copied()
            .filter(|&(_, len)| len >= min_len)
            .min_by_key(|&(start, _)| start)
    }

    #[test]
    fn an_empty_tree_answers_nothing() {
        let tree = GapTree::default();
        assert_eq!(tree.lowest_fitting(1), None);
        assert_eq!(tree.lowest_fitting(0), None);
    }

    #[test]
    fn it_finds_the_lowest_fitting_gap_not_the_first_or_the_biggest() {
        let mut tree = GapTree::default();
        for (start, len) in [(0u64, 4u64), (100, 64), (200, 8), (300, 32)] {
            tree.insert(start, len);
        }
        // Lowest overall.
        assert_eq!(tree.lowest_fitting(1), Some((0, 4)));
        // The 4-byte gap at 0 is too small, so the answer skips past it.
        assert_eq!(tree.lowest_fitting(8), Some((100, 64)));
        // ...and keeps skipping.
        assert_eq!(tree.lowest_fitting(32), Some((100, 64)));
        // Only one gap is big enough.
        assert_eq!(tree.lowest_fitting(64), Some((100, 64)));
        // None are.
        assert_eq!(tree.lowest_fitting(65), None);
    }

    #[test]
    fn zero_length_gaps_are_never_recorded() {
        let mut tree = GapTree::default();
        tree.insert(10, 0);
        assert_eq!(tree.len(), 0);
        assert_eq!(tree.lowest_fitting(1), None);
    }

    #[test]
    fn removal_is_by_start_address_alone() {
        let mut tree = GapTree::default();
        tree.insert(10, 5);
        tree.insert(20, 5);
        tree.remove(10);
        assert_eq!(tree.iter().collect::<Vec<_>>(), vec![(20, 5)]);
        tree.remove(999); // absent: a no-op, not a panic
        assert_eq!(tree.len(), 1);
    }

    /// Deep enough to have real inner nodes, so the descent actually branches
    /// rather than reading a single leaf.
    #[test]
    fn the_descent_agrees_with_a_scan_across_a_deep_tree() {
        let mut state = 0x243F_6A88_85A3_08D3u64;
        let mut rand = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        let mut tree = GapTree::default();
        let mut reference: Vec<(u64, u64)> = Vec::new();
        let mut start = 0u64;
        for _ in 0..5_000 {
            let len = 1 + rand() % 500;
            tree.insert(start, len);
            reference.push((start, len));
            start += len + 1 + rand() % 50;
        }
        assert_eq!(tree.len(), reference.len());

        for min_len in (1..=520).chain([1000, u64::MAX]) {
            assert_eq!(
                tree.lowest_fitting(min_len),
                lowest_fitting_by_scan(&reference, min_len),
                "descent disagreed with the scan at min_len={min_len}"
            );
        }

        // Removing the current answers must promote the next one correctly --
        // this is where a stale augmentation would show up.
        for _ in 0..2_000 {
            let min_len = 1 + rand() % 520;
            let found = tree.lowest_fitting(min_len);
            assert_eq!(found, lowest_fitting_by_scan(&reference, min_len));
            if let Some((start, _)) = found {
                tree.remove(start);
                reference.retain(|&(s, _)| s != start);
            }
        }
    }
}
