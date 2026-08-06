use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use std::borrow::Borrow;
use std::collections::BTreeSet;
use std::hint::black_box;
use std::num::NonZeroU32;

// Import sweep_bptree types
use sweep_bptree::argument::Argument;
use sweep_bptree::{BPlusTreeMap, BPlusTreeSet};

#[derive(Debug, Clone, Copy)]
struct Entry {
    pos: u64,
    len: NonZeroU32,
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.pos.partial_cmp(&other.pos)
    }
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.pos.cmp(&other.pos)
    }
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.pos == other.pos
    }
}

impl Eq for Entry {}

impl Borrow<u64> for Entry {
    fn borrow(&self) -> &u64 {
        &self.pos
    }
}

/// Structure representing the maximum gap augmentation (`None` only for empty subtree).
///
/// TODO: The implementation of `Argument<Entry>` for this struct is ad-hoc and untested.
/// It is only used to benchmark the impact that maintaining an augmentation has on the
/// run time of tasks that don't even need the augmentation. In particular, the current
/// implementation doesn't consider the gap before the very first entry in the tree. That
/// gap has to be treated specially because it is not bounded by any entry on the left
/// and thus doesn't belong to any subtree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct MaxGapAugmentation {
    /// `pos` of the first entry. Zero if the subtree is empty (which may only happen for an empty tree at the root).
    left: u64,
    /// `pos + len` of the last entry. Zero if the subtree is empty (which may only happen for an empty tree at the root).
    right: u64,
    /// Maximum gap between consecutive entries within the subtree. Zero if the subtree is empty (which may only happen for an empty tree at the root).
    max_gap: u64,
}

impl Argument<Entry> for MaxGapAugmentation {
    /// Creates an augmentation for a leaf node from its keys and values.
    fn from_leaf(keys: &[Entry]) -> Self {
        let mut iter = keys.iter();
        let Some(&first_entry) = iter.next() else {
            return Self::default();
        };

        let (max_gap, last_right) = iter.fold(
            (0, first_entry.pos + first_entry.len.get() as u64),
            |(max_gap, last_right), entry| {
                let gap_size = entry.pos - last_right;
                (max_gap.max(gap_size), entry.pos + entry.len.get() as u64)
            },
        );
        Self {
            left: first_entry.pos,
            right: last_right,
            max_gap,
        }
    }

    /// Creates an augmentation for an inner node from its separator keys and child augmentations.
    fn from_inner(_keys: &[Entry], arguments: &[Self]) -> Self {
        let mut iter = arguments.iter();
        let Some(first_arg) = iter.next() else {
            return Self::default();
        };

        let (max_gap, max_end_pos) = arguments[1..].iter().fold(
            (first_arg.max_gap, first_arg.right),
            |(max_gap, last_right), arg| {
                let gap_size = arg.left - last_right;
                (max_gap.max(arg.max_gap).max(gap_size), arg.right)
            },
        );

        Self {
            left: first_arg.left,
            right: max_end_pos,
            max_gap,
        }
    }
}

/// Helper to generate random non-overlapping entries satisfying:
/// pos >= prev.pos + prev.len.get()
fn generate_entries(count: usize, rng: &mut StdRng) -> Vec<Entry> {
    let mut entries = Vec::with_capacity(count);
    let mut current_pos: u64 = rng.random_range(0..100);

    for _ in 0..count {
        let len_val = rng.random_range(1..=500);
        let len = NonZeroU32::new(len_val).unwrap();

        entries.push(Entry {
            pos: current_pos,
            len,
        });

        // Ensure next pos >= pos + len + gap
        let gap: u64 = rng.random_range(0..=1000);
        current_pos += len.get() as u64 + gap;
    }

    entries
}

/// Helper to generate lookup target keys (mix of existing hit keys and random missing keys)
fn generate_lookup_keys(entries: &[Entry], num_lookups: usize, rng: &mut StdRng) -> Vec<u64> {
    let max_pos = entries
        .last()
        .map(|e| e.pos + e.len.get() as u64)
        .unwrap_or(1000);
    let mut keys = Vec::with_capacity(num_lookups);

    for i in 0..num_lookups {
        if i % 2 == 0 {
            // Pick a key guaranteed to exist
            let idx = rng.random_range(0..entries.len());
            keys.push(entries[idx].pos);
        } else {
            // Pick a random position in range
            keys.push(rng.random_range(0..max_pos));
        }
    }

    keys
}

pub fn bench_random_point_lookups(c: &mut Criterion) {
    let mut group = c.benchmark_group("Random Point Lookups");
    let mut rng = StdRng::seed_from_u64(0xDEADBEEF);

    const ENTRY_COUNTS: &[usize] = &[100, 1_000, 10_000, 100_000, 1_000_000];
    const NUM_LOOKUPS: usize = 1_000;

    for &size in ENTRY_COUNTS {
        let entries = generate_entries(size, &mut rng);
        let lookup_keys = generate_lookup_keys(&entries, NUM_LOOKUPS, &mut rng);

        // 1. std::collections::BTreeSet
        let std_btree: BTreeSet<Entry> = entries.iter().copied().collect();

        group.bench_with_input(
            BenchmarkId::new("std::collections::BTreeSet", size),
            &size,
            |b, _| {
                b.iter(|| {
                    for &key in &lookup_keys {
                        black_box(std_btree.get(black_box(&key)));
                    }
                });
            },
        );

        // 2. sweep_bptree::BPlusTreeSet (unaugmented)
        let unaugmented_tree: BPlusTreeSet<Entry> = entries.iter().copied().collect();

        group.bench_with_input(
            BenchmarkId::new("sweep_bptree::BPlusTreeSet (Unaugmented)", size),
            &size,
            |b, _| {
                b.iter(|| {
                    for &key in &lookup_keys {
                        black_box(unaugmented_tree.contains(black_box(&key)));
                    }
                });
            },
        );

        // 3. sweep_bptree::BPlusTreeSet (unaugmented)
        let augmented_tree: BPlusTreeSet<Entry> = entries.iter().copied().collect();

        group.bench_with_input(
            BenchmarkId::new("sweep_bptree::BPlusTreeMap (Unaugmented)", size),
            &size,
            |b, _| {
                b.iter(|| {
                    for &key in &lookup_keys {
                        black_box(augmented_tree.contains(black_box(&key)));
                    }
                });
            },
        );

        // 4. sweep_bptree::BPlusTreeMap (augmented)
        let augmented_tree: BPlusTreeMap<Entry, (), MaxGapAugmentation> =
            entries.iter().map(|&e| (e, ())).collect();

        group.bench_with_input(
            BenchmarkId::new("sweep_bptree::BPlusTreeMap (Augmented)", size),
            &size,
            |b, _| {
                b.iter(|| {
                    for &key in &lookup_keys {
                        black_box(augmented_tree.get(black_box(&key)));
                    }
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_random_point_lookups);
criterion_main!(benches);
