# Compaction search: measurements

How much work does `GainGreedyHeap::propose_compaction_step` do to *decide* a
compaction step? Compaction bounds the bytes it copies by construction (the
budget), but nothing bounds the **candidate search**, so this is where an
"incremental" compactor can quietly stop being incremental.

Two independent measurements, recorded for each of two search designs:

- **Counters** — a `#[cfg(test)]` probe counting candidates examined per call
  over a churny alloc/free/resize workload. Says *how many* items the search
  touches and what fraction of the available space that is.
- **Benchmarks** — criterion timings of `propose_compaction_step` alone, with no
  byte copying and no commit. Says what that costs in wall-clock.

Environment: rustc 1.97.1, 11th Gen Intel i7-1165G7 @ 2.80 GHz, 8 cores.
Counters come from an **unoptimized** (`cargo test`) build; benchmarks from an
optimized one, so the two are not comparable to each other — only across designs.

The two designs live on branches, so either can be re-measured:

| branch | tip | search |
|---|---|---|
| `search-movers` | `63af1af` | enumerates **movers** — one candidate per fixed-size class plus one per resizable allocation, descending address |
| `search-destinations` | `7a2abee` | enumerates **destinations** — gaps ascending, one `MoverTree` descent per gap |

Both branches carry the same placement fix (*Score placement against the
potential, and claim largest-first*) and the same `α = 0` short-circuit, so the
comparison below is like-for-like.

```sh
cargo test -p kladde-heap --lib candidate_search_cost -- --ignored --nocapture
cargo bench -p kladde-heap
```

## The schedule being measured

Compaction runs in **bursts**, which is what a backend flush does:
`compact_incrementally(budget)` loops internally until the budget is spent, so
one pause performs many consecutive steps with no mutation in between.

The workload pauses every **32 operations** and spends a budget of **2048
bytes**. Calibrated, not guessed — measured over the 4 000-round workload:

| interval | budget | steps/burst | bursts reaching quiescence | residual gap bytes |
|---|---|---|---|---|
| 32 | 512 | 6.0 | 1 / 125 | 1.1% |
| 32 | 1024 | 7.1 | 3 / 125 | 0.8% |
| **32** | **2048** | **7.7** | **5 / 125** | **0.8%** |
| 32 | 4096 | 9.0 | 5 / 125 | 0.6% |
| 128 | 2048 | 11.7 | 1 / 32 | 0.8% |
| 128 | 8192 | 15.3 | 2 / 32 | 0.7% |

`(32, 2048)` gives several steps per burst while only ~4% of bursts run out of
work, so incremental behaviour is genuinely exercised rather than degrading into
a full compaction.

Two caveats on that table, both mine to own:

- It was measured on **`search-destinations` only**, before the branches were
  split, and carried across unchanged. That turned out to be safe — both branches
  report identical steps per burst (5.0 / 7.6 / 7.6) and identical
  bursts-reaching-quiescence (5 / 5 / 5) at every scale — but it was not checked
  in advance.
- The "residual gap bytes" column is a **4 000-round** figure and does not hold
  at larger scale. It is `end − live_bytes` at the end of the run, i.e.
  `end ≈ 1.008 × live_bytes` there. But steps-to-quiescence per live allocation
  rises from 791/1397 = 0.57 at 4 000 rounds to 71058/13963 = 5.09 at 40 000 —
  nine times the backlog per allocation — so a fixed 2048-byte budget stops
  keeping up as the heap grows. Note also that a move usually *relocates* a gap
  upward rather than removing it; only `end` retreating reduces total free bytes,
  which is why ~1% fragmentation and tens of thousands of quiescing steps are
  consistent rather than contradictory.

Two phases are reported separately: **bursts** (as above) and **quiescing** (no
churn, budget 4096, run until gapless). Quiescing is not a schedule any caller
uses; it is retained as a stress case. The call counts are **not weights** —
quiescing contributes more calls only because it is run to completion by choice.

## The two designs follow the same trajectory

Both searches are **exact greedy at `α = 0`**: for a fixed mover the best
destination is the lowest gap that fits, and for a fixed gap the best mover is
the highest-addressed one that fits — the same maximum of `addr − dest`, reached
from opposite ends. The only freedom is which of several equally-scoring steps is
returned, since `Best::offer` keeps strictly-greater and the two iterate in
opposite orders.

Empirically even that does not bite. Across all three workloads the branches
agree exactly on steps to quiescence (12 / 791 / 71 058), steps per burst
(5.0 / 7.6 / 7.6) and bursts reaching quiescence (5 / 5 / 5). Three exact
coincidences at that granularity is not luck: **the trajectories coincide**, and
the two designs are measured on the same heaps.

An earlier version of this file claimed the designs "pick different moves" and
listed design-dependent benchmark state as a caveat. That was asserted rather
than checked, and the evidence above says it was wrong.

---

## Counters

Mean items examined per `propose_compaction_step` call. `examined` = visited ÷
available, i.e. how much of the candidate space the pruning failed to skip.

| workload | live allocs | phase | movers | destinations |
|---|---|---|---|---|
| 400 rounds | 118 | bursts | 12.99 | **5.45** |
| | | quiescing | 27.23 | **3.92** |
| 4 000 rounds | 1 397 | bursts | 117.94 | **60.07** |
| | | quiescing | 344.80 | **48.95** |
| 40 000 rounds | 13 963 | bursts | 1 094.13 | **549.77** |
| | | quiescing | 3 500.56 | **540.24** |

```
movers        bursts     calls  10713  visited  11 721 422  max 3510  examined 62.9%
              quiescing  calls  71059  visited 248 746 245  max 3511  examined 99.7%
destinations  bursts     calls  10713  visited   5 889 710  max 1206  examined 92.2%
              quiescing  calls  71059  visited  38 389 249  max 1206  examined 100.0%
```

By item count destination-first wins: **~2× fewer during bursts, ~6.5× fewer
while quiescing.** Both still grow roughly linearly in heap size, so neither
changes the asymptotics.

Note what the burst schedule does to mover-first. Under the earlier single-step
schedule its churn phase averaged 389 items per call; under bursts it averages
1 094. That is the expected direction — after the first step of a burst there is
no intervening mutation, so the rest of the burst faces the same exhausted state
that makes quiescing expensive. **A burst looks like quiescing from the second
step onward**, which is why the single-step phase was a poor model.

## Benchmark: `propose compaction step`

| shape | n | movers | destinations |
|---|---|---|---|
| `roomy` | 1 000 | 280 ns | **187 ns** |
| | 10 000 | 271 ns | **206 ns** |
| | 100 000 | 293 ns | **249 ns** |
| `churned` | 1 000 | **9.70 µs** | 18.4 µs |
| | 10 000 | **71.4 µs** | 204 µs |
| | 100 000 | **117 µs** | 1 241 µs |
| `slivers` | 1 000 | **115 µs** | 128 µs |
| | 10 000 | 1.215 ms | **580 µs** |
| | 100 000 | 14.34 ms | **4.51 ms** |

## Reading it

**The counters and the clock still disagree, and the `α = 0` short-circuit did
not reconcile them.** Removing the two `allocations` range queries that computed
an `r_src` destined to be multiplied by zero was expected to close the per-item
gap. It did not: destination-first remains 1.9×, 2.9× and 10.6× slower on
`churned`. Per-item cost was not the whole story, and the earlier note
attributing the regression to that lookup was too confident.

What is left is the **bound**, and it is structural rather than a constant
factor. Both prunes are monotone but anchored differently:

- **Mover-first** stops when `candidate_address ≤ best`, walking *down*. If the
  topmost allocation has a destination anywhere below, its gain is ≈ `T`, so
  `best` jumps to ≈ `T` after one item and the next candidate is pruned.
- **Destination-first** stops when `T − dest ≤ best`, walking *up*. Its ceiling
  starts at ≈ `T` and falls only as `dest` climbs, so it walks until
  `dest ≥ T − best`, where `best` comes from the first gap's mover at
  `f₀ − dest₀`. If `f₀ ≪ T` it walks a number of gaps proportional to `T − f₀`,
  which **grows with the heap**.

The asymmetry is in which condition is easy to satisfy. Mover-first needs *the
topmost allocation to have some destination* — common, since any wide-enough gap
below will do. Destination-first needs *the lowest gap to accommodate a very
high-addressed allocation* — much rarer, since a narrow low gap admits only small
allocations, which may sit anywhere. This is the flaw in the original argument
for destination-first: the lowest gap does almost always yield *a* candidate, but
yielding a candidate is not enough — it must yield a *high* one to tighten the
bound.

`slivers` is the shape where destination-first wins (2.1× and 3.2× at 10 k and
100 k), and consistently so: nothing fits any gap there, so the `MoverTree`
descent fails at the root and the per-gap work is minimal. `roomy` now favours
destination-first slightly and stays flat in heap size for both — when a good
move exists, both prunes work and the design barely matters.

**Conclusion, unchanged: mover-first is the better default.** Destination-first's
lower item count does not survive contact with the clock on the shape that models
a live heap, and its advantage is confined to pathological fragmentation.

### Still open

- **The `α = 0` exact branch is dead work too.** For a gap of width `w`,
  `highest_fitting(w)` already returns the highest allocation of size ≤ `w`,
  which includes every exact-fit mover, so the exact branch can only find a
  lower-addressed one with the same destination — and its `r_dest = +1` is
  multiplied by zero. Skipping it at `α = 0` would remove a `BTreeMap` lookup and
  three `BTreeSet::last` calls per gap. Not yet done or measured.
- **`churned` at 100 000 is the least trustworthy number.** Its setup stops
  compacting at round 64 000 then churns for another 236 000, producing a backlog
  the burst schedule would never leave. The 1 000 → 10 000 comparison is the
  trustworthy one and shows the same direction more modestly.
- **Item counts and timings come from different workloads** — the counters from
  the burst workload, the benchmark from its own three shapes — so "fewer items
  yet slower" is not strictly a contradiction. Instrumenting the benchmark shapes
  with the same probe would settle whether destination-first really walks more
  gaps there than mover-first walks movers.
- **This round changed two things at once.** The placement fix landed alongside
  the `α = 0` short-circuit, so comparing against the previous run is confounded;
  only the cross-branch comparison within this run is clean.

---

## Notes on the artifacts

The criterion reports are committed whole, with every `.svg` gzipped to `.svgz`
and the HTML references rewritten (`test-results/svgz.sh`). That takes each
report directory from ~13 MB to ~5.8 MB.

**Caveat:** browsers decompress `.svgz` over `file://` inconsistently — Firefox
does, Chrome generally expects a `Content-Encoding: gzip` header and will show
broken images. If the plots do not render, either serve the directory over HTTP
or reverse the compression:

```sh
find test-results -name '*.svgz' -exec sh -c 'gunzip -c "$1" > "${1%z}" && rm "$1"' _ {} \;
find test-results -name '*.html' -exec sed -i -E 's/\.svgz(["'"'"')])/.svg\1/g' {} +
```

Earlier rounds are in git history: the single-step schedule and the reasoning
about why it modelled the wrong thing at `686912b`, and the first burst-schedule
round — before the placement fix and the `α = 0` short-circuit — at `0ff1153`.
