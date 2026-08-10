//! How long does it take to *decide* the next compaction step?
//!
//! Compaction promises bounded work per step. The bytes copied are bounded by
//! construction (the budget); the part that is not obviously bounded is
//! **finding** the move. This benchmark times the search -- no byte copying --
//! across heap shapes chosen to span the range of its pruning.
//!
//! Three shapes, chosen when the decision was a branch-and-bound walk over
//! candidates and kept because they still span the interesting range -- whether
//! a *high-gain move exists at all*, and how much free space is reachable:
//!
//! - `roomy` -- a wide gap low down that the topmost allocation fits.
//! - `slivers` -- every gap narrower than every allocation, so nothing can
//!   evacuate anywhere and only the slide has anything to offer.
//! - `churned` -- alloc/free/resize churn with compaction bursts interleaved,
//!   which is the state a real heap spends most of its time in.
//!
//! **The two shapes measure different quantities.** `roomy` and `slivers` are
//! static states, so they time a single `propose_compaction_step` call, repeated
//! on an unchanging heap. `churned` instead times a whole
//! `compact_incrementally` **burst** -- propose *and* commit until the budget is
//! spent -- from a saved pre-burst state, restored by cloning before each
//! iteration. That is what a backend flush actually pays, and unlike the
//! repeated-single-call form it also covers the later calls of a burst, which
//! run on a progressively more compacted heap than the first one does.

use std::hint::black_box;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};

use kladde_heap::{GainGreedyHeap, Pointer, RelocatableHeap, Sizedness};

type Heap = GainGreedyHeap<Pointer<u32>>;

fn id(counter: u32, fixed: bool) -> Pointer<u32> {
    let sizedness = if fixed {
        Sizedness::Fixed
    } else {
        Sizedness::Resizable
    };
    Pointer::from_parts(counter, sizedness).expect("valid id")
}

/// A deterministic xorshift, so every shape is reproducible run to run.
fn rng(seed: u64) -> impl FnMut() -> u64 {
    let mut state = seed;
    move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    }
}

/// `n` resizable allocations of assorted sizes, each separated by an 8-byte
/// spacer that is then freed. Every gap is 8 bytes and every allocation is far
/// larger, so nothing can evacuate anywhere: no bound is ever established and
/// the search runs to the end of its candidate list.
fn slivers(n: usize) -> Heap {
    let mut rand = rng(0x5117_E123);
    let mut h = Heap::new();
    let mut counter = 1u32;
    let mut spacers = Vec::with_capacity(n);
    for _ in 0..n {
        let size = 200 + (rand() % 400) as u32;
        h.alloc(id(counter, false), size).expect("alloc");
        counter += 1;
        let spacer = id(counter, true);
        counter += 1;
        h.alloc(spacer, 8).expect("alloc");
        spacers.push(spacer);
    }
    for spacer in spacers {
        h.free(spacer).expect("free");
    }
    h
}

/// The same population, but with a wide gap at the bottom that the topmost
/// allocation fits into -- so the very first candidate examined wins.
fn roomy(n: usize) -> Heap {
    let mut rand = rng(0xA11C_0DE1);
    let mut h = Heap::new();
    let mut counter = 1u32;
    let mut head = Vec::new();
    // A run of allocations at the bottom, freed to leave one wide gap.
    for _ in 0..16 {
        let this = id(counter, false);
        counter += 1;
        h.alloc(this, 1024).expect("alloc");
        head.push(this);
    }
    for _ in 0..n {
        let size = 200 + (rand() % 400) as u32;
        h.alloc(id(counter, false), size).expect("alloc");
        counter += 1;
    }
    for this in head {
        h.free(this).expect("free");
    }
    h
}

/// How often the churn below pauses to compact, and how much it may move when
/// it does. Matches the instrumented test in `gain_greedy.rs`.
///
/// The interval divides every `n` measured here, and a burst fires at the *end*
/// of each interval, so `churned(n)` stops exactly where a burst is due. That is
/// what lets the benchmark measure a burst from a state the schedule really
/// produces, and it keeps all three sizes at the same phase of the burst cycle.
const COMPACTION_INTERVAL: usize = 25;
const COMPACTION_BUDGET: u64 = 2048;

/// Stage 2's size reward, measured both ways. It ships off, and the case for
/// turning it on is entirely empirical -- it trades exactness in `Φ` for a
/// preference the potential does not express -- so both settings are an axis
/// here rather than a settled default. Timing is only half the answer: the
/// fragmentation it leaves behind is measured by the workload tests in
/// `gain_greedy.rs`.
const LAMBDAS: [(&str, bool); 2] = [("plain", false), ("reward", true)];

/// Churn with compaction bursts interleaved throughout: the state a live heap is
/// usually in, under the schedule a backend actually uses.
///
/// `n` counts *rounds*, not allocations -- with 60% allocate and 25% free, the
/// live count settles around a third of it. Compaction never pauses: a state
/// reached by churning for a long stretch with the compactor switched off is one
/// no caller can produce, so measuring on it would measure nothing real.
///
/// The returned heap is the state **just before** the burst that round `n - 1`
/// is due to trigger. That final burst is what the benchmark measures, so it is
/// deliberately left un-run.
///
/// The bursts that *build* the state run under the same `lambda` as the one
/// being measured -- a different policy leaves a measurably different heap
/// behind, so timing a burst on a state some other policy produced would time a
/// state no caller can reach.
fn churned(n: usize, lambda: bool) -> Heap {
    assert!(
        n.is_multiple_of(COMPACTION_INTERVAL),
        "the run must stop exactly where a burst is due"
    );
    let mut rand = rng(0x2545_F491);
    let mut h = Heap::new();
    h.set_lambda(lambda);
    let mut live: Vec<Pointer<u32>> = Vec::new();
    let mut counter = 1u32;
    for round in 0..n {
        let roll = rand() % 100;
        if roll < 60 || live.is_empty() {
            let size = [8u32, 16, 16, 64, 250][(rand() % 5) as usize];
            let this = id(counter, !rand().is_multiple_of(4));
            counter += 1;
            h.alloc(this, size).expect("alloc");
            live.push(this);
        } else if roll < 85 {
            let victim = live.swap_remove((rand() % live.len() as u64) as usize);
            h.free(victim).expect("free");
        } else {
            let i = (rand() % live.len() as u64) as usize;
            let new_size = 1 + (rand() % 300) as u32;
            if h.lookup(live[i]).is_some() {
                let _ = h.resize(live[i], new_size);
            }
        }

        // A compaction burst every so often, exactly as a flush would do it --
        // all but the last, which is the benchmark's routine.
        if (round + 1) % COMPACTION_INTERVAL == 0 && round + 1 < n {
            h.compact_incrementally(COMPACTION_BUDGET);
        }
    }
    h
}

fn bench_propose(c: &mut Criterion) {
    let mut group = c.benchmark_group("propose compaction step");
    // Light sampling on purpose: the worst shapes take milliseconds per call at
    // 100k allocations, and criterion's defaults would run for hours.
    group.sample_size(10);

    for &n in &[1_000usize, 10_000, 100_000] {
        for (label, lambda) in LAMBDAS {
            // Static shapes: one `propose_compaction_step` call, repeated.
            // Nothing commits, so the heap and the decision are identical on
            // every iteration.
            let mut shapes: [(&str, Heap); 2] = [("roomy", roomy(n)), ("slivers", slivers(n))];
            for (shape, heap) in &mut shapes {
                heap.set_lambda(lambda);
                let id = BenchmarkId::new(format!("{shape}-{label}"), n);
                group.bench_with_input(id, &n, |b, _| {
                    b.iter(|| black_box(heap.propose_compaction_step(black_box(4096))));
                });
            }

            // The realistic shape: one whole burst, from the state this policy's
            // own schedule leaves just before it. The burst mutates the heap, so
            // each iteration starts from a fresh clone of that saved state --
            // restored in `setup`, which criterion excludes from the timing.
            // `PerIteration` keeps one clone alive at a time; a batched size
            // would hold hundreds.
            let pre_burst = churned(n, lambda);
            // A burst is far cheaper than the clone that restores its input, so
            // criterion's default measurement time would spend minutes cloning
            // per benchmark. Ten samples of a shorter run say the same thing
            // about a routine this repeatable.
            group.measurement_time(Duration::from_millis(750));
            group.warm_up_time(Duration::from_millis(250));
            let id = BenchmarkId::new(format!("churned-{label}"), n);
            group.bench_with_input(id, &n, |b, _| {
                b.iter_batched_ref(
                    || pre_burst.clone(),
                    |h| black_box(h.compact_incrementally(black_box(COMPACTION_BUDGET))),
                    BatchSize::PerIteration,
                );
            });
            group.measurement_time(Duration::from_secs(5));
            group.warm_up_time(Duration::from_secs(3));
        }
    }

    group.finish();
}

criterion_group!(benches, bench_propose);
criterion_main!(benches);
