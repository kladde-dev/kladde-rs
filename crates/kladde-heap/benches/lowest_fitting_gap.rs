//! "Which gap should this mover land in?" -- the one query the compactor asks on
//! every candidate, benchmarked two ways.
//!
//! The answer wanted is **the lowest-addressed gap at least `n` bytes wide**,
//! because among gaps that fit, the lowest maximizes travel distance and travel
//! distance *is* the per-byte gain (`incremental-compaction.md` §1).
//!
//! - **scan**: `free_by_size: BTreeMap<len, BTreeSet<start>>`, walked from `n`
//!   upward taking the minimum first-address. Costs one probe per distinct gap
//!   *length* at least `n` -- so it is fast exactly when gaps cluster on a few
//!   lengths, and degrades as they spread out. This is the structure the heap
//!   used to keep and no longer does; it survives here as the thing to beat.
//! - **tree**: [`EvacuationIndex`], keyed by size, where the answer is
//!   `min_gap_pos` aggregated over a key suffix -- one `O(log n)` descent
//!   regardless of spread.
//!
//! The interesting axis is therefore not the gap *count* but the number of
//! distinct gap *lengths*, so both are varied. kladde's design bet -- many
//! allocations at few fixed sizes -- predicts the clustered end; one-off
//! resizable allocations push toward the spread end.
//!
//! Note that the index is carrying live allocations too in the real heap, so its
//! tree is deeper there than here. That makes this a *lower* bound on its cost
//! and an exact one on the scan's, which is the conservative direction.

use std::collections::{BTreeMap, BTreeSet};
use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

use kladde_heap::bench_support::{EvacuationIndex, Key};

/// The size-class scan the augmented tree replaced, kept here verbatim as the
/// thing being measured against.
fn lowest_fitting_by_scan(
    free_by_size: &BTreeMap<u64, BTreeSet<u64>>,
    min_len: u64,
) -> Option<u64> {
    free_by_size
        .range(min_len..)
        .filter_map(|(_, starts)| starts.first().copied())
        .min()
}

/// `count` non-overlapping gaps whose lengths are drawn from `distinct_lengths`
/// different values.
fn make_gaps(count: usize, distinct_lengths: usize, rng: &mut StdRng) -> Vec<(u64, u64)> {
    let lengths: Vec<u64> = (0..distinct_lengths).map(|i| 8 + (i as u64) * 8).collect();
    let mut gaps = Vec::with_capacity(count);
    let mut start = 0u64;
    for _ in 0..count {
        let len = lengths[rng.random_range(0..lengths.len())];
        gaps.push((start, len));
        // Leave a live allocation between consecutive gaps.
        start += len + 1 + rng.random_range(0..64);
    }
    gaps
}

fn bench_lowest_fitting_gap(c: &mut Criterion) {
    let mut group = c.benchmark_group("lowest fitting gap");
    let mut rng = StdRng::seed_from_u64(0x0DDB_A11D_EADB_EEF1);

    const GAP_COUNTS: &[usize] = &[100, 1_000, 10_000, 100_000];
    // "few" is kladde's design bet (fixed-size classes); "many" is what one-off
    // resizable allocations produce.
    const SPREADS: &[(&str, usize)] =
        &[("4 lengths", 4), ("64 lengths", 64), ("1024 lengths", 1024)];
    const QUERIES: usize = 256;

    for &(spread_name, distinct) in SPREADS {
        for &count in GAP_COUNTS {
            let gaps = make_gaps(count, distinct, &mut rng);
            let max_len = 8 + (distinct as u64) * 8;

            let mut free_by_size: BTreeMap<u64, BTreeSet<u64>> = BTreeMap::new();
            let mut tree = EvacuationIndex::default();
            for &(start, len) in &gaps {
                free_by_size.entry(len).or_default().insert(start);
                tree.insert(Key::gap(start, len));
            }

            // Ask for sizes spread across the whole range, since a query near the
            // top of the range is the scan's worst case and one near the bottom
            // is its best.
            let queries: Vec<u64> = (0..QUERIES)
                .map(|_| rng.random_range(1..=max_len))
                .collect();

            // Sanity: the two must actually answer the same thing.
            for &q in &queries {
                assert_eq!(
                    tree.lowest_gap_fitting(q),
                    lowest_fitting_by_scan(&free_by_size, q),
                    "the two implementations disagree at min_len={q}"
                );
            }

            let id = format!("{spread_name}/{count}");
            group.bench_with_input(
                BenchmarkId::new("scan (free_by_size)", &id),
                &count,
                |b, _| {
                    b.iter(|| {
                        for &q in &queries {
                            black_box(lowest_fitting_by_scan(&free_by_size, black_box(q)));
                        }
                    });
                },
            );
            group.bench_with_input(BenchmarkId::new("augmented tree", &id), &count, |b, _| {
                b.iter(|| {
                    for &q in &queries {
                        black_box(tree.lowest_gap_fitting(black_box(q)));
                    }
                });
            });
        }
    }

    group.finish();
}

criterion_group!(benches, bench_lowest_fitting_gap);
criterion_main!(benches);
