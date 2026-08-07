# Compaction search: measurements

How much work does `GainGreedyHeap::propose_compaction_step` do to *decide* a
compaction step? Compaction bounds the bytes it copies by construction (the
budget), but nothing bounds the **candidate search**, so this is where an
"incremental" compactor can quietly stop being incremental.

Two independent measurements, recorded for each of two search designs:

- **Counters** — a `#[cfg(test)]` probe counting candidates examined per call
  over a churny alloc/free/resize workload, plus a table of the heap's shape
  sampled at ~10 points along the run.
- **Benchmarks** — criterion timings, with no byte copying.

Environment: rustc 1.97.1, 11th Gen Intel i7-1165G7 @ 2.80 GHz, 8 cores. Both
measurements are **optimized** builds, which cuts the counter run from minutes to
seconds.

| branch | tip | report | search |
|---|---|---|---|
| `search-movers` | `1855d45` | [search-movers/report/](search-movers/report/index.html) | enumerates **movers** — one candidate per fixed-size class plus one per resizable allocation, descending address |
| `search-destinations` | this branch | [search-destinations/report/](search-destinations/report/index.html) | enumerates **destinations** — gaps ascending, one `MoverTree` descent per gap |

Both carry the same placement fix, the same `α = 0` short-circuit, and the same
measurement harness, so the comparison is like-for-like.

```sh
cargo test --release -p kladde-heap --lib candidate_search_cost -- --ignored --nocapture
cargo bench -p kladde-heap --bench propose_compaction_step
```

Both branches ran the benchmark from an empty `target/criterion`, so the
"change" plots in the committed reports compare only against the run's own
warm-up baseline, not against an earlier round.

## What is measured

Compaction runs in **bursts**, which is what a backend flush does:
`compact_incrementally(budget)` loops internally until the budget is spent, so
one pause performs many consecutive steps with no mutation in between. The
workload churns for **25 operations**, then spends **2048 bytes**, which yields
~7 steps per burst with only a handful of bursts running out of work.

Afterwards a **catch-up** phase compacts with no churn competing, until `end` is
within **1%** of `live_bytes`. It deliberately does *not* run to a gapless heap:
that endgame is narrow gaps bubbling to the top one run at a time, is quadratic
in heap size, and is work no budget would ever buy.

The burst fires at the *end* of each 25-round interval, and 25 divides every
round count measured, so every run stops immediately after a burst. That matters
for comparability: with the burst at the *start* of the interval, a run ending at
round `n − 1` would carry `(n − 1) mod interval` rounds of un-compacted churn,
which differed per size and biased the larger runs.

## The headline: the burst schedule already stays under 1%

| workload | catch-up steps | overhead before → after |
|---|---|---|
| 400 rounds | **0** | 0.42% |
| 4 000 rounds | **0** | 0.91% |
| 40 000 rounds | **0** | 0.82% |

Sampling the heap along the 40 000-round run shows why — overhead never drifts:

| round | allocations | live_bytes | end | gaps | overhead |
|---|---|---|---|---|---|
| 3 999 | 1 397 | 100 176 | 101 091 | 128 | 0.91% |
| 7 999 | 2 746 | 203 775 | 205 234 | 227 | 0.72% |
| 11 999 | 4 147 | 311 890 | 314 698 | 359 | 0.90% |
| 15 999 | 5 550 | 420 447 | 423 547 | 472 | 0.74% |
| 19 999 | 6 920 | 524 851 | 529 219 | 614 | 0.83% |
| 23 999 | 8 280 | 618 469 | 622 856 | 711 | 0.71% |
| 27 999 | 9 757 | 727 923 | 732 740 | 840 | 0.66% |
| 31 999 | 11 152 | 843 233 | 849 764 | 969 | 0.77% |
| 35 999 | 12 529 | 955 759 | 963 669 | 1 103 | 0.83% |
| 39 999 | 13 963 | 1 068 537 | 1 077 320 | 1 218 | 0.82% |

**Overhead is flat at ~0.7–0.9% across a 30× growth in heap size**, and the gap
count grows linearly with the allocation count (~1 gap per 11.5 allocations). A
fixed 2048-byte budget every 25 operations keeps up indefinitely at these
proportions. The catch-up phase now takes **zero** steps at every size: the
schedule never even reaches the 1% threshold from the wrong side.

That retires the "quiescing is the expensive regime" framing from earlier rounds.
It was expensive, but it was an artefact of the *target*: those runs measured the
cost of reaching a gapless heap, which the schedule never approaches and no
caller wants.

The mover-first branch produces the same trajectory to within tie-breaking — its
40 000-round run ends at `end = 1 077 196` and 1 198 gaps, against 1 077 320 and
1 218 here, a 0.01% difference in file size. The two agree exactly for the first
~2 000 rounds and then drift apart by a hair, which is what "exact greedy at
`α = 0`, differing only in ties" predicts. (An earlier round found them
byte-identical; that was the same phenomenon at a schedule where no tie happened
to break differently, not a stronger property.)

---

## Counters: items examined per call

| workload | live allocs | movers | destinations |
|---|---|---|---|
| 400 rounds | 118 | 12.48 | **4.45** |
| 4 000 rounds | 1 397 | 116.32 | **62.16** |
| 40 000 rounds | 13 963 | 1 088.29 | **552.30** |

```
movers        calls  12745  visited  13 870 207  max 3511  examined 63.0%
destinations  calls  12793  visited   7 065 630  max 1218  examined 92.2%
```

Destination-first examines **~2× fewer items** at every scale. Both still grow
roughly linearly in heap size, so neither changes the asymptotics — consistent
with the gap count growing linearly above.

Note the `examined` columns: destination-first walks 92% of its (smaller)
candidate set, mover-first 63% of its larger one. Neither prune is doing much;
the difference is the size of the set being enumerated.

## Benchmark: `propose compaction step`

`roomy` and `slivers` are sized in allocations; `churned` is sized in **rounds**,
and with 60% allocate / 25% free its live count settles near a third of that.

| shape | n | movers | destinations |
|---|---|---|---|
| `roomy` | 1 000 | 257 ns | **180 ns** |
| | 10 000 | 251 ns | **200 ns** |
| | 100 000 | 275 ns | **210 ns** |
| `churned` | 1 000 | 231 µs | **113 µs** |
| | 10 000 | 1.220 ms | **902 µs** |
| | 100 000 | 10.43 ms | **7.98 ms** |
| `slivers` | 1 000 | **120 µs** | 125 µs |
| | 10 000 | 1.161 ms | **553 µs** |
| | 100 000 | 13.84 ms | **4.29 ms** |

### How `churned` is measured

**The `churned` row measures a different quantity from the other two, and it is
not comparable to the `churned` figures in earlier rounds of this file.**

`roomy` and `slivers` are static shapes. Their benchmark builds the heap once,
outside the timed region, then times a single `propose_compaction_step(4096)`
call. Because `propose_compaction_step` takes `&self` and nothing commits, the
heap is identical on every iteration — criterion simply repeats the same decision
and reports its mean.

That form was wrong for `churned`, for two reasons. It measured only the *first*
`propose` call of a burst, never the later ones, which run on a progressively
more compacted heap; and it sampled the three sizes at different points in the
burst cycle, so the largest one carried the most un-compacted churn purely by
accident of where the loop ended.

`churned` now measures **one whole burst**:

1. `churned(n)` runs `n` rounds of alloc/free/resize with a
   `compact_incrementally(2048)` burst at the end of every 25 rounds — all but
   the last. `n` is a multiple of 25, so it stops exactly where a burst is due,
   and the heap it returns is the **pre-burst state** the schedule really
   produces at round `n`.
2. That state is saved once and cloned in `iter_batched_ref`'s `setup`, which
   criterion excludes from the timing. `BatchSize::PerIteration` keeps one clone
   alive at a time.
3. The timed routine is `compact_incrementally(h, 2048)` — propose *and* commit
   until the same 2048-byte budget is spent, with the same budget the setup used.

So a `churned` figure is **the wall-clock cost of one flush's worth of
compaction**, ~7 steps, not the cost of one decision. Cloning is what makes it
repeatable: the burst mutates the heap, so each iteration has to start from the
same saved state.

Cloning a heap costs `O(n log n)`, because `sweep-bptree`'s node store is not
itself `Clone` and both augmented trees are rebuilt entry by entry. It is
excluded from the timing but not from the wall clock, which is why this shape
dominates the benchmark's runtime.

## Reading it

**Measuring the burst reversed the verdict on `churned`.** The previous round had
destination-first losing by 1.6× at 100 000 rounds, which read as the one clear
regression left. It was an artefact: measuring only the first call of a burst, on
a state 31 rounds past its last compaction. Timing the burst itself,
destination-first wins at all three sizes — 2.0× / 1.35× / 1.31×.

The two independent measurements now agree in direction at every scale, which
they did not before:

- **`roomy`** — destination-first wins everywhere, and both stay flat in heap
  size. When a good move exists both prunes fire immediately.
- **`slivers`** — a tie at 1 000, then destination-first by 2.1× and 3.2×.
  Nothing fits any gap there, so its `MoverTree` descent fails at the root and
  its per-gap work collapses.
- **`churned`** — the realistic shape: destination-first by 1.3–2.0×.

**Per-item cost is still the weak spot.** At 40 000 rounds destination-first
examines 2.0× fewer items but is only 1.3× faster on the comparable `churned`
size, so its per-item cost is roughly 1.5× mover-first's. The `α = 0`
short-circuit removed one source of that (two range queries for a discarded
`r_src`); the remaining suspect is the **exact-fit branch**, which runs for every
gap: a `live_by_size` lookup plus up to three `BTreeSet::last` calls, all of it
dead work at `α = 0`, since `highest_fitting(w)` already returns the highest
allocation of size ≤ `w` and the `r_dest = +1` bonus is multiplied by zero. That
is the obvious next thing to try.

**Recommendation.** Destination-first is now the better default: it wins on every
shape at every size but one near-tie, it examines half as many candidates, and
its one identified inefficiency is still untried. The `2α` approximation it makes
above `α = 0` remains the reason to keep the mover-first branch around.

### Still open

- **Skip the exact-fit branch at `α = 0`** (above). Not yet done or measured.
- **The benchmark shapes are not instrumented**, so the per-item cost argument is
  inferred by comparing counters from one workload against timings from another
  rather than measured directly. Adding the probe to the bench would settle it.
- **`α > 0` is unmeasured entirely.** Every number here is at the shipped
  `α = 0`, where both designs are exact. The `2α` approximation destination-first
  makes above zero has been tested for correctness but never for cost.
- **Both searches are still linear in heap size.** Nothing here bounds the
  candidate walk; the burst cost grows with the heap, and at 33 000 allocations a
  flush already costs ~8 ms of pure decision-making. Keeping the sub-1%
  fragmentation at lower cost is the open problem.

---

## Notes on the artifacts

Only `propose_compaction_step` is run now; `lowest_fitting_gap` and
`btree_point_lookup` are unaffected by this work and were dominating the wall
clock at criterion's default 100 samples. Their last results are in git history
at `5ac45eb`.

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

Earlier rounds in git history: the single-step schedule at `686912b`, the first
burst-schedule round at `0ff1153`, the run-to-gapless round at `5ac45eb`, and the
1%-target round that still timed a single `propose` call at `3cff4de`.
