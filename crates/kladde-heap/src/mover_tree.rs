//! [`MoverTree`]: an address-ordered index of the live allocations, augmented
//! with each subtree's **smallest** allocation, so that "the highest-addressed
//! allocation that fits in `w` bytes" is one `O(log n)` descent.
//!
//! That is the query the destination-first candidate search asks once per gap:
//! for a fixed destination, the best mover is whichever one travels furthest,
//! i.e. sits highest, among those small enough to land there. Answering it by
//! walking allocations instead costs one visit per allocation, which measurement
//! showed to be the whole cost of the search (see `test-results/`).
//!
//! It is the mirror image of [`GapTree`](crate::gap_tree): that one minimizes an
//! address subject to a *lower* bound on length and descends leftmost-first;
//! this one maximizes an address subject to an *upper* bound on size and
//! descends rightmost-first. Both are 2-D dominance queries that neither an
//! address-ordered nor a size-ordered map answers alone.
//!
//! As there, the size has to live in the key: `SearchArgument::locate_in_leaf`
//! sees only the leaf's keys, never its values.

use sweep_bptree::argument::{Argument, SearchArgument};
use sweep_bptree::{BPlusTree, NodeStoreVec};

/// The concrete tree behind [`MoverTree`]. The raw `BPlusTree` rather than
/// `BPlusTreeMap` for the same reason as [`GapTree`](crate::gap_tree)'s: only it
/// exposes `bulk_load`, which is what makes `Clone` linear.
type Store = NodeStoreVec<Mover, (), MinSize>;

/// A live allocation, as this index sees it: where it starts and how big it is.
///
/// Ordered **by `addr` alone**, which is what makes the `Borrow<u64>` impl sound
/// -- the borrowed ordering agrees with the full one.
#[derive(Clone, Copy, Debug)]
struct Mover {
    addr: u64,
    size: u32,
}

impl PartialEq for Mover {
    fn eq(&self, other: &Self) -> bool {
        self.addr == other.addr
    }
}
impl Eq for Mover {}
impl PartialOrd for Mover {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Mover {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.addr.cmp(&other.addr)
    }
}
impl std::borrow::Borrow<u64> for Mover {
    fn borrow(&self) -> &u64 {
        &self.addr
    }
}

/// The smallest allocation anywhere in a subtree.
///
/// `u32::MAX` for an empty subtree -- deliberately *not* the derived `Default` of
/// zero, which would claim an empty subtree can accommodate any request and let
/// a descent dead-end in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MinSize(u32);

impl Default for MinSize {
    fn default() -> Self {
        Self(u32::MAX)
    }
}

impl Argument<Mover> for MinSize {
    fn from_leaf(keys: &[Mover]) -> Self {
        Self(keys.iter().map(|m| m.size).min().unwrap_or(u32::MAX))
    }

    fn from_inner(_keys: &[Mover], arguments: &[Self]) -> Self {
        Self(arguments.iter().map(|a| a.0).min().unwrap_or(u32::MAX))
    }
}

impl SearchArgument<Mover> for MinSize {
    /// The largest size that fits the destination.
    type Query = u32;

    fn locate_in_leaf(max_size: u32, keys: &[Mover]) -> Option<usize> {
        keys.iter().rposition(|m| m.size <= max_size)
    }

    fn locate_in_inner(
        max_size: u32,
        _keys: &[Mover],
        arguments: &[Self],
    ) -> Option<(usize, Self::Query)> {
        // Children are in ascending address order, so the *last* whose subtree
        // holds a small-enough allocation is the one holding the highest such
        // allocation. Descending into it cannot dead-end: a non-empty subtree's
        // minimum is a real entry's size, and `locate_in_leaf` accepts on the
        // same predicate.
        arguments
            .iter()
            .rposition(|a| a.0 <= max_size)
            .map(|child| (child, max_size))
    }
}

/// The mover index. See the module docs.
pub struct MoverTree {
    tree: BPlusTree<Store>,
}

impl Default for MoverTree {
    fn default() -> Self {
        Self {
            tree: BPlusTree::new(Store::default()),
        }
    }
}

impl Clone for MoverTree {
    /// Rebuilt from already-sorted data, for the same reason as
    /// [`GapTree`](crate::gap_tree)'s: `bulk_load` fills leaves linearly, with
    /// no comparisons and no node splits.
    fn clone(&self) -> Self {
        let data: Vec<(Mover, ())> = self.tree.iter().map(|(&m, &())| (m, ())).collect();
        Self {
            tree: BPlusTree::bulk_load(data),
        }
    }
}

impl MoverTree {
    /// Record a live allocation.
    pub fn insert(&mut self, addr: u64, size: u32) {
        self.tree.insert(Mover { addr, size }, ());
    }

    /// Forget the allocation starting at `addr`, if any.
    pub fn remove(&mut self, addr: u64) {
        self.tree.remove(&addr);
    }

    /// The highest-addressed allocation of at most `max_size` bytes, as
    /// `(address, size)`. One descent, `O(log n)`.
    ///
    /// `max_size` is a `u64` because it comes from a gap width, which can exceed
    /// what any single allocation could be; it is clamped, which is exact since
    /// no allocation is larger than `u32::MAX` anyway.
    pub fn highest_fitting(&self, max_size: u64) -> Option<(u64, u32)> {
        let clamped = u32::try_from(max_size).unwrap_or(u32::MAX);
        self.tree
            .get_by_argument(clamped)
            .map(|(mover, ())| (mover.addr, mover.size))
    }

    /// Number of allocations indexed.
    pub fn len(&self) -> usize {
        self.tree.len()
    }

    /// Whether nothing is indexed.
    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    /// Every allocation, in ascending address order.
    pub fn iter(&self) -> impl Iterator<Item = (u64, u32)> + '_ {
        self.tree.iter().map(|(mover, ())| (mover.addr, mover.size))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference the descent must agree with.
    fn highest_fitting_by_scan(movers: &[(u64, u32)], max_size: u64) -> Option<(u64, u32)> {
        movers
            .iter()
            .copied()
            .filter(|&(_, size)| u64::from(size) <= max_size)
            .max_by_key(|&(addr, _)| addr)
    }

    #[test]
    fn an_empty_tree_answers_nothing() {
        let tree = MoverTree::default();
        assert_eq!(tree.highest_fitting(0), None);
        assert_eq!(tree.highest_fitting(u64::MAX), None);
    }

    #[test]
    fn it_finds_the_highest_fitting_mover_not_the_last_or_the_smallest() {
        let mut tree = MoverTree::default();
        for (addr, size) in [(0u64, 8u32), (100, 64), (200, 8), (300, 512)] {
            tree.insert(addr, size);
        }
        // The topmost allocation is 512 bytes, so a small destination skips it.
        assert_eq!(tree.highest_fitting(8), Some((200, 8)));
        assert_eq!(tree.highest_fitting(63), Some((200, 8)));
        // Widen the destination and a higher mover becomes reachable.
        assert_eq!(tree.highest_fitting(64), Some((200, 8)));
        assert_eq!(tree.highest_fitting(512), Some((300, 512)));
        assert_eq!(tree.highest_fitting(u64::MAX), Some((300, 512)));
        // Nothing is small enough.
        assert_eq!(tree.highest_fitting(7), None);
    }

    #[test]
    fn removal_is_by_address_alone() {
        let mut tree = MoverTree::default();
        tree.insert(10, 5);
        tree.insert(20, 5);
        tree.remove(10);
        assert_eq!(tree.iter().collect::<Vec<_>>(), vec![(20, 5)]);
        tree.remove(999); // absent: a no-op, not a panic
        assert_eq!(tree.len(), 1);
    }

    /// Deep enough to have real inner nodes, so the descent actually branches.
    #[test]
    fn the_descent_agrees_with_a_scan_across_a_deep_tree() {
        let mut state = 0x1357_9BDF_2468_ACE0u64;
        let mut rand = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        let mut tree = MoverTree::default();
        let mut reference: Vec<(u64, u32)> = Vec::new();
        let mut addr = 0u64;
        for _ in 0..5_000 {
            let size = 1 + (rand() % 500) as u32;
            tree.insert(addr, size);
            reference.push((addr, size));
            addr += u64::from(size) + 1 + rand() % 50;
        }
        assert_eq!(tree.len(), reference.len());

        for max_size in (0..=520).chain([1000, u64::MAX]) {
            assert_eq!(
                tree.highest_fitting(max_size),
                highest_fitting_by_scan(&reference, max_size),
                "descent disagreed with the scan at max_size={max_size}"
            );
        }

        // Removing the current answers must promote the next one correctly --
        // where a stale augmentation would show up.
        for _ in 0..2_000 {
            let max_size = rand() % 520;
            let found = tree.highest_fitting(max_size);
            assert_eq!(found, highest_fitting_by_scan(&reference, max_size));
            if let Some((addr, _)) = found {
                tree.remove(addr);
                reference.retain(|&(a, _)| a != addr);
            }
        }
    }
}
