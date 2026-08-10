//! [`SizeClasses`]: for each live **fixed** allocation size `s`, the
//! highest-addressed allocation of that size and the lowest-addressed gaps whose
//! width is a multiple of it.
//!
//! This is stage 4 of `augmented-segment-tree.md`, the destination-side half of
//! the objective. Free-space accounting on this project shows that most free
//! space is destroyed not by compaction truncating the top of the file but by
//! **new allocations landing in existing gaps** -- around three quarters of it.
//! Compaction's contribution to file size is therefore mostly indirect: it
//! decides what *shape* the free space is in when the allocator next needs some.
//!
//! That reframes what a good destination is. A gap whose width is an exact
//! multiple of a commonly-minted size can be consumed with no residue; a gap one
//! byte wider than a size class leaves a sliver nothing will ever use. So:
//!
//! - `k = 1` -- an **exact fit** erases the gap outright. Worth `μ₁`.
//! - `k > 1` -- a **proper multiple** leaves `(k−1)·s`, still exactly tileable by
//!   the same class, so the property is preserved rather than consumed. Worth
//!   `μₖ`, and `μₖ < μ₁`, because this one is *speculative*: it only pays if the
//!   remaining `k−1` allocations of that class actually arrive and actually get
//!   placed there.
//!
//! # Why fixed-size only
//!
//! Not a simplification -- the correct rule. A *resizable* allocation parked in a
//! snug gap has to move again the moment it grows, re-opening the gap and paying
//! for two copies, so rewarding its snug fit would be luring it into a round
//! trip. Resizable allocations earn no destination-side reward at any `k`, and do
//! not appear here at all.
//!
//! # Why this cannot live in the evacuation index
//!
//! That merge works because `>=` on sizes is **monotone**: everything in a
//! high-size subtree beats everything in a low-size subtree, so a node needs no
//! knowledge of the individual sizes it contains. Divisibility has no such
//! structure -- a 4-byte gap is a multiple of a 2-byte allocation, a 5-byte gap is
//! not -- so to resolve a crossing pair a node would have to remember every
//! distinct size beneath it, blowing its state up from `O(1)` to `O(subtree
//! span)`. Hence a separate small index, combined with the main one at the top.
//!
//! # Why testing `|C|` sizes rather than enumerating divisors
//!
//! The textbook "gap of width `G` pairs with any divisor of `G`" problem is
//! solved by enumerating all `O(√G)` divisors. Here that is wasted work: only the
//! sizes in `C` can ever be the size of a candidate allocation, so testing those
//! for divisibility is both sufficient and cheaper -- `|C|` is a handful where
//! `√G` for a kilobyte gap is 32. It is also pessimistic as written, since a
//! width is divisible by `s` for only about `1/s` of widths, so the typical gap
//! event does `|C|` modulo tests and then touches no set at all.

use std::collections::{BTreeMap, BTreeSet};

/// One live fixed-size class.
#[derive(Default, Clone)]
struct Class {
    /// Addresses of live allocations of exactly this size. Only the maximum is
    /// ever read, but a *set* is needed rather than a scalar because the maximum
    /// has to be **restored** when it is freed.
    allocs: BTreeSet<u64>,
    /// Positions of gaps whose width is exactly `s`. Feeds `μ₁`.
    exact: BTreeSet<u64>,
    /// Positions of gaps whose width is `k·s` for `k >= 2` -- disjoint from
    /// `exact`, so that a candidate's weight follows from which set it came out
    /// of. Feeds `μₖ`.
    multiple: BTreeSet<u64>,
}

/// See the module docs.
///
/// A `BTreeMap` rather than the `HashMap` the design note suggests: at `|C| ≈ 5`
/// the lookup cost is a wash, and a deterministic iteration order keeps ties
/// between classes from depending on hash seeding, which would make a proposed
/// step irreproducible run to run.
///
/// **TODO: cap the number of tracked classes.** `|C|` multiplies every gap
/// creation and destruction, and entries are deliberately retained when a class
/// empties (see [`SizeClasses::remove_alloc`]), so nothing currently stops an
/// application that mints fixed-size allocations at hundreds of distinct sizes
/// from making this unbounded. Eviction is benign -- the whole mechanism is a
/// bonus term, so an untracked class simply earns no destination-side reward, in
/// placement as well as in compaction, and every move it makes stays valid.
#[derive(Default, Clone)]
pub struct SizeClasses {
    classes: BTreeMap<u32, Class>,
}

impl SizeClasses {
    /// Record a live fixed-size allocation. Callers must not pass resizable ones.
    ///
    /// A novel size **enters** here, with empty gap sets that are deliberately
    /// **not backfilled**: scanning the existing gaps to populate them would be
    /// `O(#gaps)`, and the payoff is small, because this is a bonus term and not
    /// a correctness requirement. A new class simply sees only the gaps created
    /// after it entered, and converges as the heap churns.
    pub fn add_alloc(&mut self, size: u32, addr: u64) {
        self.classes.entry(size).or_default().allocs.insert(addr);
    }

    /// Forget a live fixed-size allocation.
    ///
    /// The class entry is **retained** even when its last allocation goes, so
    /// that a class which re-enters finds its gaps already tracked instead of
    /// starting blind -- which is worth more here than the memory, given that
    /// entering classes are not backfilled. See the `TODO` on [`SizeClasses`].
    pub fn remove_alloc(&mut self, size: u32, addr: u64) {
        if let Some(class) = self.classes.get_mut(&size) {
            class.allocs.remove(&addr);
        }
    }

    /// Record a gap, filing it under every class whose size divides its width.
    pub fn add_gap(&mut self, pos: u64, width: u64) {
        for (&size, class) in &mut self.classes {
            let size = u64::from(size);
            if width == size {
                class.exact.insert(pos);
            } else if width.is_multiple_of(size) {
                class.multiple.insert(pos);
            }
        }
    }

    /// Forget a gap. Mirrors [`add_gap`](Self::add_gap) exactly, so a class that
    /// entered after the gap did -- and therefore never filed it -- removes
    /// nothing, which is correct rather than merely harmless.
    pub fn remove_gap(&mut self, pos: u64, width: u64) {
        for (&size, class) in &mut self.classes {
            let size = u64::from(size);
            if width == size {
                class.exact.remove(&pos);
            } else if width.is_multiple_of(size) {
                class.multiple.remove(&pos);
            }
        }
    }

    /// The lowest-addressed gap of width exactly `size`, if that class is
    /// tracked. Feeds `μ₁` at placement time.
    pub fn lowest_exact_gap(&self, size: u32) -> Option<u64> {
        self.classes.get(&size)?.exact.first().copied()
    }

    /// The lowest-addressed gap whose width is a *proper* multiple of `size`.
    /// Feeds `μₖ` at placement time.
    pub fn lowest_multiple_gap(&self, size: u32) -> Option<u64> {
        self.classes.get(&size)?.multiple.first().copied()
    }

    /// The best tiling evacuation, as `(objective value, from, to, size)`.
    ///
    /// `score` is the caller's score for an allocation of this size at an
    /// address -- the same one the evacuation index is keyed by -- so that the
    /// value returned is directly comparable with the other candidate shapes.
    ///
    /// Two candidates per class, not one. `min_multiple` and `min_exact` are
    /// separate sets precisely because `μ₁ > μₖ`: an exact-fit gap at a *higher*
    /// address can beat a lower proper multiple, so testing only the lowest
    /// tileable gap and asking whether it happens to be exact would miss it.
    ///
    /// Each side is maximized independently, which is exact here for the same
    /// reason it is in the index: if the lowest tracked gap of a class sits above
    /// that class's highest allocation, then *every* such gap does, so no
    /// downward pair exists at all. That check is what stops `μ` from rescuing an
    /// upward pair, exactly as stage 2's bound stops the size reward from doing
    /// so.
    pub fn best_tiling_evacuation(
        &self,
        budget: u64,
        mu_exact: u64,
        mu_multiple: u64,
        score: impl Fn(u64, u32) -> u64,
    ) -> Option<(u64, u64, u64, u32)> {
        let mut best: Option<(u64, u64, u64, u32)> = None;
        for (&size, class) in &self.classes {
            if u64::from(size) > budget {
                continue;
            }
            let Some(&from) = class.allocs.last() else {
                continue;
            };
            let scored = score(from, size);
            for (gap, mu) in [
                (class.exact.first(), mu_exact),
                (class.multiple.first(), mu_multiple),
            ] {
                let Some(&to) = gap else { continue };
                if to >= from {
                    continue; // every gap of this kind is above every mover
                }
                let gain = scored - to + mu;
                if best.is_none_or(|(incumbent, ..)| gain > incumbent) {
                    best = Some((gain, from, to, size));
                }
            }
        }
        best
    }
}

#[cfg(test)]
impl SizeClasses {
    /// Check this against the layout it is derived from.
    ///
    /// The allocation side is checked for **equality** -- every live fixed-size
    /// allocation must be filed, and nothing else. The gap side is checked only
    /// for *soundness*: every recorded gap must really exist and really be tiled
    /// by its class. It deliberately cannot be checked for completeness, because
    /// a class that entered after a gap did never filed that gap and never will
    /// (see [`SizeClasses::add_alloc`]) -- which is a bonus foregone, where a
    /// *stale* entry would be a step proposed into free space that is not there.
    pub(crate) fn assert_consistent(&self, gaps: &[(u64, u64)], fixed: &[(u64, u32)]) {
        for (&size, class) in &self.classes {
            let expected: BTreeSet<u64> = fixed
                .iter()
                .filter(|&&(_, s)| s == size)
                .map(|&(addr, _)| addr)
                .collect();
            assert_eq!(class.allocs, expected, "class {size}'s allocations drifted");

            let width_at = |pos: u64| gaps.iter().find(|&&(p, _)| p == pos).map(|&(_, w)| w);
            for &pos in &class.exact {
                assert_eq!(
                    width_at(pos),
                    Some(u64::from(size)),
                    "class {size} holds a stale exact gap at {pos}"
                );
            }
            for &pos in &class.multiple {
                let width = width_at(pos)
                    .unwrap_or_else(|| panic!("class {size} holds a stale gap at {pos}"));
                assert!(
                    width > u64::from(size) && width.is_multiple_of(u64::from(size)),
                    "class {size} holds a gap of width {width} at {pos}, not a proper multiple"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Score with `λ = 0`, so the value is the plain travel distance plus `μ`.
    fn plain(addr: u64, _size: u32) -> u64 {
        addr
    }

    #[test]
    fn a_gap_is_filed_under_every_class_whose_size_divides_it() {
        let mut sc = SizeClasses::default();
        for size in [8u32, 16, 250] {
            sc.add_alloc(size, 1000);
        }
        sc.add_gap(500, 16); // exact for 16, a proper multiple of 8

        assert_eq!(sc.lowest_exact_gap(16), Some(500));
        assert_eq!(sc.lowest_multiple_gap(16), None);
        assert_eq!(sc.lowest_exact_gap(8), None);
        assert_eq!(sc.lowest_multiple_gap(8), Some(500));
        assert_eq!(sc.lowest_exact_gap(250), None);
        assert_eq!(sc.lowest_multiple_gap(250), None);

        sc.remove_gap(500, 16);
        assert_eq!(sc.lowest_exact_gap(16), None);
        assert_eq!(sc.lowest_multiple_gap(8), None);
    }

    #[test]
    fn an_untracked_class_earns_nothing() {
        let mut sc = SizeClasses::default();
        sc.add_gap(500, 64); // no classes yet: filed nowhere
        sc.add_alloc(64, 1000);
        assert_eq!(
            sc.lowest_exact_gap(64),
            None,
            "a class entering must not be backfilled"
        );

        // ...but it picks up gaps created from now on.
        sc.add_gap(600, 64);
        assert_eq!(sc.lowest_exact_gap(64), Some(600));
    }

    #[test]
    fn a_class_entry_survives_its_last_allocation() {
        let mut sc = SizeClasses::default();
        sc.add_alloc(64, 1000);
        sc.add_gap(500, 128);
        sc.remove_alloc(64, 1000);

        // No mover, so no candidate...
        assert_eq!(sc.best_tiling_evacuation(u64::MAX, 100, 10, plain), None);
        // ...but the class kept tracking gaps, so re-entry starts informed.
        sc.add_alloc(64, 2000);
        assert_eq!(
            sc.best_tiling_evacuation(u64::MAX, 100, 10, plain),
            Some((2000 - 500 + 10, 2000, 500, 64))
        );
    }

    /// `μ₁ > μₖ`, so an exact fit can win from a *higher* address than a proper
    /// multiple. Keeping the two sets separate is what makes that expressible.
    #[test]
    fn an_exact_fit_can_beat_a_lower_proper_multiple() {
        let mut sc = SizeClasses::default();
        sc.add_alloc(64, 10_000);
        sc.add_gap(100, 128); // proper multiple, 60 lower
        sc.add_gap(160, 64); // exact fit

        // With the weights equal, depth decides.
        assert_eq!(
            sc.best_tiling_evacuation(u64::MAX, 10, 10, plain),
            Some((10_000 - 100 + 10, 10_000, 100, 64))
        );
        // Price gap *erasure* above mere tileability and the exact fit wins,
        // even though it is 60 bytes shallower.
        assert_eq!(
            sc.best_tiling_evacuation(u64::MAX, 100, 10, plain),
            Some((10_000 - 160 + 100, 10_000, 160, 64))
        );
    }

    #[test]
    fn mu_cannot_rescue_an_upward_pair() {
        let mut sc = SizeClasses::default();
        sc.add_alloc(64, 100);
        sc.add_gap(500, 64); // an exact fit, but *above* the only mover
        assert_eq!(
            sc.best_tiling_evacuation(u64::MAX, 1_000_000, 1_000_000, plain),
            None,
            "no weight may buy a move that raises the potential"
        );
    }

    #[test]
    fn the_budget_excludes_whole_classes() {
        let mut sc = SizeClasses::default();
        sc.add_alloc(8, 900);
        sc.add_alloc(250, 1000);
        sc.add_gap(0, 250);
        sc.add_gap(500, 8);

        // Unconstrained, the 250-byte class travels 1000 into the gap at 0.
        assert_eq!(
            sc.best_tiling_evacuation(u64::MAX, 100, 10, plain),
            Some((1000 + 100, 1000, 0, 250))
        );
        // A budget of 8 leaves only the 8-byte class, whose exact gap is at 500.
        assert_eq!(
            sc.best_tiling_evacuation(8, 100, 10, plain),
            Some((900 - 500 + 100, 900, 500, 8))
        );
    }

    /// The maximum has to be *restored* when the top allocation of a class is
    /// freed, which is the whole reason `allocs` is an ordered set and not a
    /// running scalar.
    #[test]
    fn freeing_the_top_of_a_class_promotes_the_next_one_down() {
        let mut sc = SizeClasses::default();
        sc.add_alloc(64, 1_000);
        sc.add_alloc(64, 5_000);
        sc.add_gap(0, 64);

        assert_eq!(
            sc.best_tiling_evacuation(u64::MAX, 0, 0, plain)
                .map(|c| c.1),
            Some(5_000)
        );
        sc.remove_alloc(64, 5_000);
        assert_eq!(
            sc.best_tiling_evacuation(u64::MAX, 0, 0, plain)
                .map(|c| c.1),
            Some(1_000),
            "the class maximum must fall back, not vanish"
        );
    }
}
