# Bounded compaction search, mode (b): resume where the last call stopped

`SearchCap::Resume(k)` examines at most `k` candidates and **continues from where
the previous call stopped**, sweeping a moving window across the candidate space
instead of re-walking a fixed prefix. The cursor spans exactly one burst:
`compact_incrementally` resets it on entry, as does the walk running out or the
prune firing — both of which mean the search was exhaustive-equivalent, so there
is nothing left to resume.

This is the companion to [`capped-restart.md`](capped-restart.md), which measures
the same two designs under a cap that restarts every call. The uncapped baseline
both are compared against is [`README.md`](README.md).

**Chosen `k` = 16**, for both search designs. See [Choosing `k`](#choosing-k) —
as in mode (a), the ~1% fragmentation target does not constrain the choice.

Environment: rustc 1.97.1, 11th Gen Intel i7-1165G7 @ 2.80 GHz, 8 cores. All
measurements are **optimized** builds.

| branch | tip | report | search |
|---|---|---|---|
| `search-movers` | `2dc0b56` | [capped-movers/report/](capped-movers/report/index.html) | enumerates **movers** — one candidate per fixed-size class plus one per resizable allocation, descending address |
| `search-destinations` | this branch | [capped-destinations/report/](capped-destinations/report/index.html) | enumerates **destinations** — gaps ascending, one `MoverTree` descent per gap |

```sh
cargo test --release -p kladde-heap --lib candidate_search_cost -- --ignored --nocapture
cargo test --release -p kladde-heap --lib search_cap_sweep      -- --ignored --nocapture
cargo bench -p kladde-heap --bench propose_compaction_step
```

Both branches ran the benchmark from an empty `target/criterion`, and each report
contains all three cap settings, so these are the same two reports
`capped-restart.md` links to.

## What is measured

The schedule is unchanged from [`README.md`](README.md): churn for **25
operations**, then a **burst** — `compact_incrementally(2048)` — which loops
internally until the budget is spent. The burst fires at the *end* of each
interval and 25 divides every round count, so every run stops immediately after
one.

### How the cursor survives across calls

`propose_compaction_step` takes `&self` and each committed step mutates the tree
the walk iterates, so a live iterator cannot be held across a step. What is held
instead is a **resume key** — the address of the first candidate the previous
call did *not* examine — in a `Cell` on the heap. That costs one seek per
`propose` call, not one per candidate visited, which is the property the whole
exercise depends on: a re-seek per item would replace a linear walk with a
linear-times-`log` one and measure nothing.

The two branches pay for that seek differently:

- **Mover-first** walks `std` collections, which already range-seek. The resume
  key becomes a `Bound::Included` upper bound on both streams' `range(..)`. No
  new machinery.
- **Destination-first** walks the augmented `GapTree`, and `sweep-bptree`'s
  `BPlusTreeMap` exposes only whole-tree iteration. `GapTree` therefore moves to
  the raw `BPlusTree`, which does expose cursors, and gains `iter_from`: one
  descent to seed, then leaf-to-leaf steps following the leaves' own `next`
  pointers. This is the cursor support `later.md` anticipated needing, arriving
  earlier than expected and for a different reason.

  One trap worth recording: `Cursor` remembers the key it was **asked for**, not
  the key it found, so a cursor seeded by `get_cursor` reports the probe's
  length — zero — rather than the real one. `GapTree` now stores the length as
  the value as well as in the key, and `iter_from` reads it from there.

`compact_incrementally` moved onto the heap for this mode: the cursor needs a
scope, and "one burst" is it. A caller-side loop could not express that.

## The headline: the bound holds cost flat *and* fragmentation improves

| workload | catch-up steps | overhead, uncapped | overhead, `Resume(16)` |
|---|---|---|---|
| 400 rounds | **0** | 0.42% | 0.42% / 0.38% |
| 4 000 rounds | **0** | 0.91% / 0.95% | 0.75% / 0.75% |
| 40 000 rounds | **0** | 0.82% / 0.81% | **0.43% / 0.69%** |

(destination-first / mover-first, where they differ.)

Sampling along the 40 000-round run. **Destination-first:**

| round | allocations | end | gaps | overhead | visited/call | µs/burst |
|---|---|---|---|---|---|---|
| 3 999 | 1 397 | 100 928 | 91 | 0.75% | 12.6 | 93.4 |
| 7 999 | 2 746 | 205 180 | 159 | 0.69% | 15.7 | 109.5 |
| 11 999 | 4 147 | 313 990 | 210 | 0.67% | 15.8 | 101.5 |
| 15 999 | 5 550 | 423 244 | 288 | 0.67% | 16.0 | 93.1 |
| 19 999 | 6 920 | 528 446 | 360 | 0.68% | 16.0 | 100.4 |
| 23 999 | 8 280 | 621 317 | 380 | 0.46% | 16.0 | 109.7 |
| 27 999 | 9 757 | 731 204 | 430 | 0.45% | 16.0 | 108.2 |
| 31 999 | 11 152 | 846 812 | 492 | 0.42% | 16.0 | 110.4 |
| 35 999 | 12 529 | 959 963 | 535 | 0.44% | 16.0 | 121.9 |
| 39 999 | 13 963 | 1 073 161 | 606 | 0.43% | 16.0 | **106.6** |

**Mover-first:**

| round | allocations | end | gaps | overhead | visited/call | µs/burst |
|---|---|---|---|---|---|---|
| 3 999 | 1 397 | 100 924 | 113 | 0.75% | 14.6 | 105.7 |
| 7 999 | 2 746 | 205 297 | 229 | 0.75% | 15.9 | 124.8 |
| 11 999 | 4 147 | 314 662 | 371 | 0.89% | 16.0 | 128.3 |
| 15 999 | 5 550 | 423 257 | 462 | 0.67% | 16.0 | 132.9 |
| 19 999 | 6 920 | 528 290 | 594 | 0.66% | 16.0 | 143.0 |
| 23 999 | 8 280 | 622 149 | 692 | 0.60% | 16.0 | 156.7 |
| 27 999 | 9 757 | 732 341 | 815 | 0.61% | 16.0 | 168.9 |
| 31 999 | 11 152 | 848 640 | 918 | 0.64% | 16.0 | 167.7 |
| 35 999 | 12 529 | 962 690 | 1 055 | 0.73% | 16.0 | 138.4 |
| 39 999 | 13 963 | 1 075 892 | 1 140 | 0.69% | 16.0 | **126.7** |

Against the uncapped baseline over the same 10× growth in live allocations:

| | 1 397 allocs | 13 963 allocs | growth |
|---|---|---|---|
| destinations, uncapped | 226.6 µs | 1 807.5 µs | **8.0×** |
| destinations, `Resume(16)` | 93.4 µs | 106.6 µs | **1.14×** |
| movers, uncapped | 118.0 µs | 3 768.4 µs | **31.9×** |
| movers, `Resume(16)` | 105.7 µs | 126.7 µs | **1.20×** |

**That is the result.** A 10× heap costs the bounded search 1.14–1.20× more per
burst, against 8× and 32× unbounded. `visited/call` pins at exactly 16.0, so the
cap — not the prune — stops every walk, and the residual growth is not search: it
is `commit_compaction_step` moving runs, plus `slide_candidate`'s
budget-bounded walk.

## The surprise: bounding the search *reduces* fragmentation

Overhead roughly halves on destination-first (0.82% → 0.43%) and improves on
mover-first (0.81% → 0.69%), with the gap count falling 1 218 → 606 and
1 198 → 1 140.

The mechanism shows up in the steps-per-burst figure, which falls from **7.0 to
2.5** (destination-first) and **7.0 to 3.4** (mover-first). The bounded search
finds fewer good evacuations, so the **slide** candidate — offered
unconditionally, outside the capped walk — wins far more often. A slide moves a
whole contiguous run into the largest gap, closing it entirely; an evacuation
moves one allocation and generally splits a gap in two.

The two policies optimize different things, and the one we measure is not the one
the greedy search maximizes. The search is greedy in the potential
`Φ = Σ address`, and the best `Φ` move is often a small allocation travelling a
long way — excellent for `Φ`, neutral for the gap count and for `end`.
Fragmentation is a different objective; clipping the search happens to bias it
toward the moves that serve *that* one. This is a finding about the cost
function, not about the cap.

## Choosing `k`

`search_cap_sweep` over the 40 000-round workload, `peak%` being the worst
overhead over the second half of the run (i.e. after burn-in):

**Destination-first:**

| cap | peak% | final% | gaps | visited/call | µs/burst |
|---|---|---|---|---|---|
| `Unbounded` | 0.83% | 0.82% | 1 218 | 1 075.3 | 1 871.6 |
| `Resume(4)` | 0.46% | 0.43% | 596 | 4.0 | 112.1 |
| `Resume(8)` | 0.50% | 0.50% | 623 | 8.0 | 130.8 |
| **`Resume(16)`** | **0.46%** | **0.43%** | **606** | **16.0** | **104.9** |
| `Resume(32)` | 0.51% | 0.51% | 661 | 31.9 | 116.1 |
| `Resume(64)` | 0.61% | 0.57% | 763 | 63.4 | 187.6 |
| `Resume(128)` | 0.72% | 0.70% | 873 | 124.5 | 220.7 |
| `Resume(256)` | 0.82% | 0.82% | 1 050 | 238.9 | 313.6 |
| `Resume(512)` | 0.91% | 0.91% | 1 163 | 414.8 | 535.7 |

**Mover-first:**

| cap | peak% | final% | gaps | visited/call | µs/burst |
|---|---|---|---|---|---|
| `Unbounded` | 0.82% | 0.81% | 1 198 | 2 124.1 | 3 759.2 |
| `Resume(4)` | 0.62% | 0.60% | 965 | 4.0 | 85.3 |
| `Resume(8)` | 0.65% | 0.65% | 1 059 | 8.0 | 96.9 |
| **`Resume(16)`** | **0.73%** | **0.69%** | **1 140** | **16.0** | **89.2** |
| `Resume(32)` | 0.72% | 0.67% | 1 201 | 31.9 | 93.0 |
| `Resume(64)` | 0.80% | 0.73% | 1 245 | 63.6 | 143.4 |
| `Resume(128)` | 0.81% | 0.79% | 1 228 | 126.9 | 234.5 |
| `Resume(256)` | 0.77% | 0.77% | 1 143 | 247.7 | 437.2 |
| `Resume(512)` | 0.80% | 0.78% | 1 146 | 466.8 | 640.4 |

Overhead is broadly **worse in `k`** on both designs — the same inversion as in
mode (a) — so the criterion the task set is satisfied everywhere and selects
nothing. `k = 16` is chosen as the smallest value at which the search is still
doing recognizable work, and the same value is used on both branches and both
modes so the four experiments stay directly comparable.

`µs/burst` bottoms out around 85–110 µs and will not go lower however small `k`
gets. That floor is commit and slide cost, not search.

---

## Counters: items examined per call

40 000-round workload, during bursts:

| | mean visited | max | examined | calls |
|---|---|---|---|---|
| movers, uncapped | 1 088.29 | 3 511 | 63.0% | 12 745 |
| movers, `Resume(16)` | **15.77** | 16 | 0.98% | 7 029 |
| destinations, uncapped | 552.30 | 1 218 | 92.2% | 12 793 |
| destinations, `Resume(16)` | **15.31** | 16 | 5.36% | 5 575 |

`max` equals `k` exactly. The `examined` column — the fraction of the *whole*
candidate space each call touches — now falls as the heap grows, which is the
asymptotic statement: a fixed 16 candidates out of a linearly growing set.

On destination-first, `Resume` makes **more** calls per run than `Restart`
(5 575 vs 4 789) — its bursts take more, smaller steps (2.5 vs 2.0). That reads
like the sweep working as intended: a window further up the address space finds a
better evacuation than the fixed prefix does, so the slide wins slightly less
often. On mover-first it goes the other way (7 029 vs 7 564), so this is not a
property of resuming, only an observation about one design.

## Benchmark: `propose compaction step`

`roomy` and `slivers` are sized in allocations; `churned` is sized in **rounds**,
and with 60% allocate / 25% free its live count settles near a third of that.

| shape | n | movers, uncapped | movers, `resume` | destinations, uncapped | destinations, `resume` |
|---|---|---|---|---|---|
| `roomy` | 1 000 | 245 ns | 243 ns | 224 ns | 227 ns |
| | 10 000 | 255 ns | 246 ns | 252 ns | 256 ns |
| | 100 000 | 287 ns | 296 ns | 260 ns | 264 ns |
| `churned` | 1 000 | 342 µs | **86.1 µs** | 208 µs | **74.0 µs** |
| | 10 000 | 1.284 ms | **55.1 µs** | 1.387 ms | **135.7 µs** |
| | 100 000 | 12.12 ms | **112.1 µs** | 10.09 ms | **138.7 µs** |
| `slivers` | 1 000 | 112 µs | **1.86 µs** | 147 µs | **2.62 µs** |
| | 10 000 | 1.184 ms | **2.28 µs** | 703 µs | **1.42 µs** |
| | 100 000 | 14.20 ms | **2.47 µs** | 6.94 ms | **1.39 µs** |

`roomy` is unchanged, as it must be: a good move exists immediately, the prune
fires on the first candidate, and the cap never binds.

`slivers` is the pathological shape — every gap narrower than every allocation,
so no evacuation exists and no bound is ever established. Uncapped, the walk runs
to the end of a linearly growing list: 14.2 ms at n = 100 000. Capped, it is
**1.4–2.5 µs and completely flat**, a ~5 000× reduction at the largest size.

`churned` grows 1.3× (movers) and 1.9× (destinations) across a 100× growth in
rounds, against 35× and 49× uncapped.

### How `churned` is measured

`churned(n, cap)` runs `n` rounds with a burst at the end of every 25 — all but
the last, which is what the benchmark times. The state is saved and restored by
cloning in `iter_batched_ref`'s `setup`, which criterion excludes from the
timing; `BatchSize::PerIteration` keeps one clone alive at a time. So a `churned`
figure is **the cost of one flush's worth of compaction**, not of one decision —
which for this mode is also the only honest way to measure it, since the cursor
only means anything across the steps of a burst.

Two details matter for reading it:

- **The cap applies to the setup too.** A bounded search leaves a measurably
  different heap behind — half the gaps — so timing a bounded burst on a state an
  unbounded search produced would time a state no caller can reach.
- **Measurement time is reduced for this shape** (750 ms, from 5 s). A bounded
  burst is ~50× cheaper than the clone that restores its input, so criterion's
  default would spend minutes cloning per benchmark. With `sample_size(10)` the
  estimator is unchanged; only the iteration count falls.

For `roomy` and `slivers` under this mode there is a caveat the other modes do
not have: those benchmarks repeat a single `propose_compaction_step` call on an
unchanging heap, and under `Resume` the cursor advances between iterations. So
they measure a *sweep* across the candidate space rather than one repeated
decision. That is what happens inside a burst too, so the figure is meaningful —
but it is not the same quantity as the `uncapped` and `restart` columns beside
it.

## Reading it

**The bound achieves what it was built for, on both designs, with no cost to
fragmentation.** Per-burst cost grows 1.14–1.20× across a 10× heap, against 8×
and 32× unbounded; the pathological shape flattens completely.

**Resuming is not measurably better than restarting.** Against
[mode (a)](capped-restart.md) at the same `k`:

| | mode (a) `Restart(16)` | mode (b) `Resume(16)` |
|---|---|---|
| destinations, overhead | **0.44%** | 0.43% |
| destinations, gaps | 604 | 606 |
| destinations, µs/burst | 132.8 | **106.6** |
| movers, overhead | **0.51%** | 0.69% |
| movers, gaps | **964** | 1 140 |
| movers, µs/burst | **100.3** | 126.7 |

Destination-first is a tie on fragmentation and mode (b) is somewhat cheaper;
mover-first is *worse* under mode (b) on every axis. Neither difference is large,
and the cheaper-per-burst figures partly reflect the different step counts rather
than a faster search.

The honest conclusion is that **the extra machinery does not pay for itself
here** — and the reason is the same finding as above. The bias mode (b) was built
to correct is bias in the *evacuation* search, but once the search is bounded the
policy is mostly sliding anyway, and the slide is chosen outside the capped walk.
Correcting which 16 evacuations get considered matters less when evacuations
account for a smaller share of the work.

That is a statement about this workload at this `k`, not a refutation of the
idea. Mode (b) would be expected to matter where evacuations dominate — a larger
`k`, a lower budget, or a workload with many well-fitting gaps.

**The cursor work was still worth doing**: `GapTree::iter_from` is the seek-once
resumable walk `later.md` calls for, and it is what an `α > 0` implementation
needs regardless of whether the cap uses it.

### Still open

- **`k` is tuned on one workload.** The sweep covers a single churn mix
  (60/25/15 alloc/free/resize, five size classes) at one budget and interval.
- **The floor is commit, not search.** ~85–110 µs/burst remains at any `k`, and
  it still grows slowly with the heap (1.14–1.20×). If the burst cost has to be
  genuinely `O(log n)`, the next thing to bound is the *slide*: `run_len_from`
  walks a budget's worth of allocations, and `commit_compaction_step` re-inserts
  every allocation in the moved run.
- **The cursor is reset more often than it needs to be.** It resets whenever the
  prune fires, which on this workload is most calls at small heaps. A design that
  distinguished "pruned" from "cut" more finely would sweep further per burst;
  whether that helps is untested.
- **`α > 0` is unmeasured.** Every number here is at the shipped `α = 0`.
- **Fragmentation is not what the search optimizes.** The halving above says the
  greedy-in-`Φ` policy is not aligned with the metric that matters. Pricing gaps
  directly — which is what `α` is for — is the principled fix, and it is
  untested.

---

## Notes on the artifacts

The criterion reports are committed whole, with every `.svg` gzipped to `.svgz`
and the HTML references rewritten (`test-results/svgz.sh`).

**Caveat:** browsers decompress `.svgz` over `file://` inconsistently — Firefox
does, Chrome generally expects a `Content-Encoding: gzip` header and will show
broken images. If the plots do not render, serve the directory over HTTP or
reverse the compression:

```sh
find test-results -name '*.svgz' -exec sh -c 'gunzip -c "$1" > "${1%z}" && rm "$1"' _ {} \;
find test-results -name '*.html' -exec sed -i -E 's/\.svgz(["'"'"')])/.svg\1/g' {} +
```
