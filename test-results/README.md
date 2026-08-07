# Compaction search: measurements

How much work does `GainGreedyHeap::propose_compaction_step` do to *decide* a
compaction step? Compaction bounds the bytes it copies by construction (the
budget), but nothing bounds the **candidate search**, so this is where an
"incremental" compactor can quietly stop being incremental.

Two independent measurements, both recorded below for each implementation:

- **Counters** — a `#[cfg(test)]` probe counting candidates examined per call
  over a churny alloc/free/resize workload. Says *how many* items the search
  touches and what fraction of the available space that is.
- **Benchmarks** — criterion timings of `propose_compaction_step` alone, with no
  byte copying and no commit. Says what that costs in wall-clock.

Environment for every run below: rustc 1.97.1, 11th Gen Intel i7-1165G7 @
2.80 GHz, 8 cores. Counters come from an **unoptimized** (`cargo test`) build;
benchmarks from an optimized one, so the two are not directly comparable to each
other — only across implementations.

---

## Run 1 — mover-first search

The search enumerates **movers**: one candidate per fixed-size class (at the
class's highest member) plus one per resizable allocation, walked in descending
address, pruning when `address + 2α` can no longer beat the best gain found.

| | |
|---|---|
| Heap implementation | [`ae461b4`](../../../commit/ae461b4) — *Instrument the candidate search (test-only)* |
| Last change to what is searched | [`478c249`](../../../commit/478c249) — *Consider resizable allocations for evacuation, not just for slides* |
| Reports | [`search-movers/report/index.html`](search-movers/report/index.html) |

```sh
# counters
cargo test -p kladde-heap --lib candidate_search_cost -- --ignored --nocapture
# benchmarks (all three groups)
cargo bench -p kladde-heap
```

### Counters

`interleaved` = a compaction step every 7 workload ops, budget 128.
`quiescing` = no churn, budget 4096, run until the heap is gapless.
`examined` = visited ÷ available, i.e. how much of the candidate space the
pruning failed to skip.

| workload | live allocs | phase | calls | visited | mean/call | max | examined |
|---|---|---|---|---|---|---|---|
| 400 rounds | 118 | interleaved | 58 | 437 | 7.53 | 18 | 45.3% |
| | | quiescing | 18 | 504 | 28.00 | 31 | 90.3% |
| 4 000 rounds | 1 397 | interleaved | 572 | 30 581 | 53.46 | 338 | 29.6% |
| | | quiescing | 802 | 275 699 | 343.76 | 356 | 96.3% |
| 40 000 rounds | 13 963 | interleaved | 5 715 | 2 223 363 | 389.04 | 2 789 | 22.3% |
| | | quiescing | 71 608 | 250 638 886 | 3 500.15 | 3 510 | **99.7%** |

Per-call distribution, 40 000-round workload:

```
interleaved   0:4  1:4  2-3:4  4-7:28  8-15:91  16-31:207  32-63:405  64-127:682  128+:4290
quiescing     0:1  128+:71607
```

**The pruning is close to inert in the regime that matters.** Mean candidates
per call grows ~10× for every 10× of heap size — 7.5 → 53 → 389 interleaved,
28 → 344 → 3 500 while quiescing — so the search is *linear in heap size* in
practice, not `O(log n)`. Driving 14 k allocations to quiescence examined 250
million candidates across 71 607 steps, skipping 0.3% of the space.

The reason the pruning fails is structural, not accidental: it can only stop
early once a *high-gain* move has been found, and the entire point of driving
toward quiescence is that the high-gain moves are gone. The distribution shows
it starkly — while quiescing, every single call but one lands in the `128+`
bucket.

Note also what sets the scale. A fixed-size class contributes **one** candidate
however many members it has; a resizable allocation contributes **one each**. So
the walk length tracks the resizable population specifically, which is largely a
cost of `478c249`.

### Benchmark: `propose compaction step`

Three shapes spanning the pruning's range — `roomy` has a wide low gap the
topmost allocation fits (bound set immediately), `slivers` has every gap
narrower than every allocation (no bound ever set), `churned` is workload output
with compaction run partway.

| shape | 1 000 | 10 000 | 100 000 |
|---|---|---|---|
| `roomy` | 270 ns | 306 ns | 332 ns |
| `churned` | 24.1 µs | 438 µs | **6.72 ms** |
| `slivers` | 107 µs | 1.00 ms | **11.99 ms** |

`roomy` is flat in heap size — that is the pruning working, and it confirms the
mechanism is sound when a good move exists. The other two grow ~10× per 10×,
matching the counters. **12 ms to decide one step** on a 100 k-allocation heap
is the headline: at that point the deciding costs far more than the copying.

### Benchmark: `lowest fitting gap` (unchanged, context only)

The augmented `GapTree` versus scanning `free_by_size` upward. Not affected by
this work; included because the reports are committed whole.

| distinct gap widths | scan @100k gaps | augmented tree @100k gaps |
|---|---|---|
| 4 | 7.25 µs | 7.68 µs |
| 64 | 70.1 µs | 8.81 µs |
| 1024 | 1.11 ms | 8.67 µs |

Per 256 queries. The tree stays in a 6–9 µs band across every configuration
while the scan degrades with the number of distinct widths.

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
