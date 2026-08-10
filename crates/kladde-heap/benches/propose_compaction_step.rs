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
//! - `shrinking` -- the same, continued with allocate and free swapped so the
//!   heap nets *smaller*. A different regime rather than a different size: free
//!   space arrives faster than the allocator consumes it, so there are more
//!   gaps and far more coalescing.
//!
//! **The shapes measure different quantities.** `roomy` and `slivers` are static
//! states, so they time a single `propose_compaction_step` call, repeated on an
//! unchanging heap. `churned` and `shrinking` instead time a whole
//! `compact_incrementally` **burst** -- propose *and* commit until the budget is
//! spent -- from a saved pre-burst state, restored by cloning before each
//! iteration. That is what a backend flush actually pays, and unlike the
//! repeated-single-call form it also covers the later calls of a burst, which
//! run on a progressively more compacted heap than the first one does.
//!
//! Timing is only half of what decides a policy; the *fragmentation* each one
//! leaves behind is measured by the workload tests in `gain_greedy.rs`, which
//! report overhead per burst over the same two workloads.

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

/// The policy settings measured, as `(label, λ, (μ₁, μₖ))`.
///
/// All of them ship off, and the case for turning any of them on is entirely
/// empirical -- they trade exactness in `Φ` for preferences the potential does
/// not express -- so they are an axis here rather than settled defaults. Timing
/// is only half the answer: the fragmentation they leave behind is what the
/// workload measurements in `gain_greedy.rs` report.
///
/// `μ` is expected to cost a little *time* (a scan over the live size classes
/// per proposal, plus divisibility tests per gap event) in exchange for a better
/// *shape*, so the two numbers have to be read together.
const POLICIES: [(&str, bool, u64, (u64, u64)); 4] = [
    ("plain", false, 0, (0, 0)),
    ("reward", true, 0, (0, 0)),
    ("gapcount", false, 64, (0, 0)),
    ("tiling", false, 0, (4096, 512)),
];

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
fn churned(n: usize, lambda: bool, alpha: u64, mu: (u64, u64)) -> Heap {
    churn(n, lambda, alpha, mu, false).0
}

/// The growing workload. `run_last_burst` says whether the burst due on the
/// final round is executed: the benchmark wants it left un-run so it can time
/// it, and [`shrinking`] wants it run so its own schedule starts from a settled
/// state.
fn churn(
    n: usize,
    lambda: bool,
    alpha: u64,
    mu: (u64, u64),
    run_last_burst: bool,
) -> (Heap, Vec<Pointer<u32>>, u32) {
    assert!(
        n.is_multiple_of(COMPACTION_INTERVAL),
        "the run must stop exactly where a burst is due"
    );
    let mut rand = rng(0x2545_F491);
    let mut h = Heap::new();
    h.set_lambda(lambda);
    h.set_alpha(alpha);
    h.set_mu(mu.0, mu.1);
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

        // A compaction burst every so often, exactly as a flush would do it.
        if (round + 1) % COMPACTION_INTERVAL == 0 && (run_last_burst || round + 1 < n) {
            h.compact_incrementally(COMPACTION_BUDGET);
        }
    }
    (h, live, counter)
}

/// The mirror image of [`churned`]: allocate and free swapped, so the heap nets
/// *smaller*, started from the state the growing workload leaves behind.
///
/// Worth measuring separately because it is a different regime for the
/// compactor, not just a different size. While shrinking, free space appears
/// faster than the allocator consumes it, so there are more gaps, the widest is
/// wider, and far more of the work is coalescing and retreating `end`. It is
/// also where `α`'s neighbour re-keying runs hardest, since gaps are being
/// created and merged constantly.
///
/// Allocation does not stop -- 25% of rounds still allocate, mirroring the 25%
/// of the growing workload that frees -- because a pure drain would be a much
/// easier and less representative problem: nothing would ever be placed into a
/// gap, and placement is where most of this heap's free space is destroyed.
///
/// Like [`churned`], the returned heap sits **just before** a due burst.
fn shrinking(n: usize, lambda: bool, alpha: u64, mu: (u64, u64)) -> Heap {
    let (mut h, mut live, mut counter) = churn(n, lambda, alpha, mu, true);
    // Enough to be unambiguously in the shrinking regime -- the mix nets about
    // 0.35 removals per round, so this retires roughly a sixth of the heap --
    // while leaving it comparable in size to what `churned` measures at the
    // same `n`.
    let rounds = (live.len() / 2).next_multiple_of(COMPACTION_INTERVAL) + COMPACTION_INTERVAL;
    let mut rand = rng(0x9E37_79B9);
    for round in 0..rounds {
        let roll = rand() % 100;
        if roll < 60 && !live.is_empty() {
            let victim = live.swap_remove((rand() % live.len() as u64) as usize);
            h.free(victim).expect("free");
        } else if roll < 85 || live.is_empty() {
            let size = [8u32, 16, 16, 64, 250][(rand() % 5) as usize];
            let this = id(counter, !rand().is_multiple_of(4));
            counter += 1;
            h.alloc(this, size).expect("alloc");
            live.push(this);
        } else {
            let i = (rand() % live.len() as u64) as usize;
            let new_size = 1 + (rand() % 300) as u32;
            if h.lookup(live[i]).is_some() {
                let _ = h.resize(live[i], new_size);
            }
        }

        if (round + 1) % COMPACTION_INTERVAL == 0 && round + 1 < rounds {
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
        for (label, lambda, alpha, mu) in POLICIES {
            // Static shapes: one `propose_compaction_step` call, repeated.
            // Nothing commits, so the heap and the decision are identical on
            // every iteration.
            let mut shapes: [(&str, Heap); 2] = [("roomy", roomy(n)), ("slivers", slivers(n))];
            for (shape, heap) in &mut shapes {
                heap.set_lambda(lambda);
                heap.set_alpha(alpha);
                heap.set_mu(mu.0, mu.1);
                let id = BenchmarkId::new(format!("{shape}-{label}"), n);
                group.bench_with_input(id, &n, |b, _| {
                    b.iter(|| black_box(heap.propose_compaction_step(black_box(4096))));
                });
            }

            // The realistic shapes: one whole burst, from the state this policy's
            // own schedule leaves just before it -- once while the heap is
            // growing and once while it is draining, which are different regimes
            // for the compactor and not merely different sizes. The burst mutates
            // the heap, so each iteration starts from a fresh clone of that saved
            // state, restored in `setup`, which criterion excludes from the
            // timing. `PerIteration` keeps one clone alive at a time; a batched
            // size would hold hundreds.
            //
            // A burst is far cheaper than the clone that restores its input, so
            // criterion's default measurement time would spend minutes cloning
            // per benchmark. Ten samples of a shorter run say the same thing
            // about a routine this repeatable.
            let states: [(&str, Heap); 2] = [
                ("churned", churned(n, lambda, alpha, mu)),
                ("shrinking", shrinking(n, lambda, alpha, mu)),
            ];
            group.measurement_time(Duration::from_millis(750));
            group.warm_up_time(Duration::from_millis(250));
            for (shape, pre_burst) in &states {
                let id = BenchmarkId::new(format!("{shape}-{label}"), n);
                group.bench_with_input(id, &n, |b, _| {
                    b.iter_batched_ref(
                        || pre_burst.clone(),
                        |h| black_box(h.compact_incrementally(black_box(COMPACTION_BUDGET))),
                        BatchSize::PerIteration,
                    );
                });
            }
            group.measurement_time(Duration::from_secs(5));
            group.warm_up_time(Duration::from_secs(3));
        }
    }

    group.finish();
}

criterion_group!(benches, bench_propose);
criterion_main!(benches);
