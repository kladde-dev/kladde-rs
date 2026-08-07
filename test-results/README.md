# Compaction search: measurements

How much work does `GainGreedyHeap::propose_compaction_step` do to *decide* a
compaction step? Compaction bounds the bytes it copies by construction (the
budget), but nothing bounds the **candidate search**, so this is where an
"incremental" compactor can quietly stop being incremental.

Two independent measurements, recorded below for each of two search designs:

- **Counters** — a `#[cfg(test)]` probe counting candidates examined per call
  over a churny alloc/free/resize workload. Says *how many* items the search
  touches and what fraction of the available space that is.
- **Benchmarks** — criterion timings of `propose_compaction_step` alone, with no
  byte copying and no commit. Says what that costs in wall-clock.

Environment for every run: rustc 1.97.1, 11th Gen Intel i7-1165G7 @ 2.80 GHz,
8 cores. Counters come from an **unoptimized** (`cargo test`) build; benchmarks
from an optimized one, so the two are not comparable to each other — only across
designs.

The two designs live on branches, so either can be re-measured:

| branch | search |
|---|---|
| `search-movers` | enumerates **movers** — one candidate per fixed-size class plus one per resizable allocation, descending address |
| `search-destinations` | enumerates **destinations** — gaps ascending, one `MoverTree` descent per gap |

## The schedule being measured

Compaction runs in **bursts**, which is what a backend flush does:
`compact_incrementally(budget)` loops internally until the budget is spent, so
one pause performs many consecutive steps with no mutation in between.

The workload pauses every **32 operations** and spends a budget of **2048
bytes**. Calibrated, not guessed — measured over the 4 000-round workload:

| interval | budget | steps/burst | bursts reaching quiescence | residual fragmentation |
|---|---|---|---|---|
| 32 | 512 | 6.0 | 1 / 125 | 1.1% |
| 32 | 1024 | 7.1 | 3 / 125 | 0.8% |
| **32** | **2048** | **7.7** | **5 / 125** | **0.8%** |
| 32 | 4096 | 9.0 | 5 / 125 | 0.6% |
| 128 | 2048 | 11.7 | 1 / 32 | 0.8% |
| 128 | 8192 | 15.3 | 2 / 32 | 0.7% |

`(32, 2048)` gives several steps per burst while only ~4% of bursts run out of
work, so the incremental behaviour is genuinely exercised rather than degrading
into a full compaction.

Two phases are still reported separately: **bursts** (compaction interleaved with
churn, as above) and **quiescing** (no churn, budget 4096, run until gapless).
Quiescing is not a schedule any caller uses; it is retained as a stress case and
because it is where the search was previously worst. The call counts are **not
weights** — quiescing contributes more calls only because it is run to
completion by choice.

```sh
cargo test -p kladde-heap --lib candidate_search_cost -- --ignored --nocapture
cargo bench -p kladde-heap
```

---

## Counters

Mean items examined per `propose_compaction_step` call. `examined` = visited ÷
available, i.e. how much of the candidate space the pruning failed to skip.

| workload | live allocs | phase | movers | destinations |
|---|---|---|---|---|
| 400 rounds | 118 | bursts | 13.01 | **4.54** |
| | | quiescing | 25.25 | **3.08** |
| 4 000 rounds | 1 397 | bursts | 113.15 | **57.50** |
| | | quiescing | 347.48 | **52.29** |
| 40 000 rounds | 13 963 | bursts | 1 021.22 | **530.94** |
| | | quiescing | 3 498.09 | **579.23** |

Full detail, 40 000 rounds:

```
movers        bursts     calls  12054  visited  12 309 754  max 3510  examined 57.1%
              quiescing  calls  60758  visited 212 536 974  max 3511  examined 99.6%
destinations  bursts     calls  12054  visited   6 400 009  max 1198  examined 86.5%
              quiescing  calls  60758  visited  35 192 768  max 1198  examined 100.0%
```

By item count destination-first is the clear winner: **~2× fewer items during
bursts, ~6× fewer while quiescing.** Both designs still grow roughly linearly in
heap size, so neither changes the asymptotics.

Note what the burst schedule did to the mover-first numbers. Under the earlier
single-step schedule its bursts phase averaged 389 items per call; under bursts
it averages 1 021. That is the expected direction: after the first step of a
burst there is no intervening mutation, so the remaining steps of the burst face
the same exhausted-high-gain state that made quiescing expensive. **A burst looks
like quiescing from the second step onward**, which is exactly why the earlier
"interleaved" phase was a poor model.

## Benchmark: `propose compaction step`

| shape | n | movers | destinations |
|---|---|---|---|
| `roomy` | 1 000 | **372 ns** | 301 ns |
| | 10 000 | 422 ns | **315 ns** |
| | 100 000 | 459 ns | **364 ns** |
| `churned` | 1 000 | **11.0 µs** | 20.1 µs |
| | 10 000 | **49.9 µs** | 138.6 µs |
| | 100 000 | **60.1 µs** | 1 209 µs |
| `slivers` | 1 000 | **130 µs** | 141 µs |
| | 10 000 | 1.33 ms | **599 µs** |
| | 100 000 | 15.57 ms | **4.70 ms** |

## The benchmark contradicts the counters, and the benchmark is the one to trust

Destination-first examines ~2× fewer items during bursts but is **1.8–2.8×
slower per call** on `churned` at 1 000 and 10 000 allocations, and 20× slower at
100 000. The counters and the clock disagree, so the per-item costs must differ —
and they do. Per gap, destination-first does a `MoverTree` descent *plus*
`neighbours_of`, which is two more `BTreeMap` range queries, to recover `r_src`.
Mover-first's per-item work is one map lookup plus a `GapTree` descent. Examining
half as many items at more than twice the price is a net loss.

`slivers` still favours destination-first (2.2× and 3.3× at 10 k and 100 k),
which is consistent: that shape is constructed so *nothing* fits any gap, so the
`MoverTree` descent fails at the root and the expensive `neighbours_of` never
runs.

**This reverses the earlier conclusion.** Under the old single-step schedule the
same benchmark had destination-first 2× *faster* on `churned` at 100 k (3.28 ms
against 6.72 ms). The schedule changed the heap state, and the state decides
which design wins:

- **Badly fragmented heap** (old schedule: compaction never kept up, huge
  backlog) — many useless movers, so enumerating destinations wins.
- **Reasonably maintained heap** (burst schedule: residual fragmentation ~1%) —
  few useless movers and a lot of small gaps, so enumerating movers wins.

Since the burst schedule is the realistic one, the honest reading is that
**destination-first is a regression for the workload the backend will actually
run**, and its advantage is confined to pathological fragmentation.

### Caveats on the comparison

- **The two heaps are not identical.** Compaction runs *during* the setup, and
  the two designs choose different moves, so their trajectories diverge and the
  benchmark's input state is design-dependent. A fair per-call comparison would
  build the state with churn only, no compaction, and measure both designs on
  that. The `churned`/100 000 gap of 20× is the least trustworthy number here for
  exactly this reason, and it is also the case where the setup stops compacting
  at round 64 000 and then churns for another 236 000 rounds — a state the burst
  schedule would never produce.
- **`roomy` is flat** at ~300–460 ns for both, across all three sizes, as it has
  been in every run. When a good move exists the pruning works and the design
  barely matters. Nothing about the easy case needed fixing.
- Counters are measured on each design's *own* trajectory, which is the right
  thing for "what does this design cost in the state it creates" but means the
  two columns are not measured on identical heaps either.

### What this suggests

The per-item cost, not the item count, is what needs attention. Destination-first
would need `r_src` available without two range queries — cached on the
allocation, or the neighbour-category split extended to all allocations rather
than only fixed-size ones — before its lower item count turns into lower
wall-clock on a maintained heap. Until then the mover-first design is the better
default, and the deferred cap on items examined (see `compaction-algorithm.md`
§6) applies to either.

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

Measurements under the earlier single-step schedule (one step every 7 operations,
budget 128) are in git history at commit `686912b`, together with the reasoning
about why that schedule modelled the wrong thing.
