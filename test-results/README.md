# Compaction search: measurements

How much work does `GainGreedyHeap::propose_compaction_step` do to *decide* a
compaction step? Compaction bounds the bytes it copies by construction (the
budget), but nothing bounds the **candidate search**, so this is where an
"incremental" compactor can quietly stop being incremental.

Two independent measurements, recorded for each of two search designs:

- **Counters** — a `#[cfg(test)]` probe counting candidates examined per call
  over a churny alloc/free/resize workload, plus a table of the heap's shape
  sampled at ~10 points along the run.
- **Benchmarks** — criterion timings of `propose_compaction_step` alone, with no
  byte copying and no commit.

Environment: rustc 1.97.1, 11th Gen Intel i7-1165G7 @ 2.80 GHz, 8 cores. Both
measurements are now **optimized** builds (`cargo test --release`), which cuts
the counter run from minutes to seconds.

| branch | tip | search |
|---|---|---|
| `search-movers` | `e227357` | enumerates **movers** — one candidate per fixed-size class plus one per resizable allocation, descending address |
| `search-destinations` | `ef4f6ec` | enumerates **destinations** — gaps ascending, one `MoverTree` descent per gap |

Both carry the same placement fix and the same `α = 0` short-circuit, so the
comparison is like-for-like.

```sh
cargo test --release -p kladde-heap --lib candidate_search_cost -- --ignored --nocapture
cargo bench -p kladde-heap --bench propose_compaction_step
```

## What is measured

Compaction runs in **bursts**, which is what a backend flush does:
`compact_incrementally(budget)` loops internally until the budget is spent, so
one pause performs many consecutive steps with no mutation in between. The
workload pauses every **32 operations** and spends **2048 bytes**, which yields
~7.6 steps per burst with only ~4% of bursts running out of work.

Afterwards a **catch-up** phase compacts with no churn competing, until `end` is
within **1%** of `live_bytes`. It deliberately does *not* run to a gapless heap:
that endgame is narrow gaps bubbling to the top one run at a time, is quadratic
in heap size, and is work no budget would ever buy. (Earlier rounds did drive to
zero, so their counter totals are not comparable to these.)

## The headline: the burst schedule already stays under 1%

| workload | catch-up steps | overhead before → after |
|---|---|---|
| 400 rounds | 1 | 1.16% → 0.47% |
| 4 000 rounds | **0** | 0.85% |
| 40 000 rounds | **0** | 0.89% |

Sampling the heap along the 40 000-round run shows why — overhead never drifts:

| round | allocations | live_bytes | end | gaps | overhead |
|---|---|---|---|---|---|
| 3 968 | 1 381 | 98 992 | 99 695 | 114 | 0.71% |
| 7 968 | 2 736 | 202 206 | 203 980 | 222 | 0.88% |
| 11 968 | 4 135 | 311 705 | 314 370 | 345 | 0.85% |
| 15 968 | 5 536 | 418 547 | 421 938 | 465 | 0.81% |
| 19 968 | 6 912 | 523 606 | 528 664 | 619 | 0.97% |
| 23 968 | 8 272 | 618 239 | 623 691 | 727 | 0.88% |
| 27 968 | 9 746 | 727 619 | 733 457 | 847 | 0.80% |
| 31 968 | 11 135 | 842 208 | 849 025 | 949 | 0.81% |
| 35 968 | 12 514 | 954 687 | 963 400 | 1 090 | 0.91% |
| 39 968 | 13 955 | 1 067 468 | 1 076 970 | 1 205 | 0.89% |

**Overhead is flat at ~0.8–0.9% across a 30× growth in heap size**, and the gap
count grows linearly with the allocation count (~1 gap per 11.6 allocations). A
fixed 2048-byte budget every 32 operations keeps up indefinitely at these
proportions.

That retires the "quiescing is the expensive regime" framing from earlier rounds.
It was expensive, but it was an artefact of the *target*: those runs measured the
cost of reaching a gapless heap, which the schedule never approaches and no
caller wants. The state the schedule actually maintains needs **zero** catch-up
work.

It also revises the earlier steps-to-quiescence observation (0.57 → 5.09 steps
per allocation from 4 000 to 40 000 rounds). That superlinearity is real, but it
lives entirely in the sub-1% endgame — the part now excluded.

**The tables above are byte-identical on both branches**, which is the third
independent confirmation that the two designs follow the same trajectory: both
are exact greedy at `α = 0`, so they can differ only in tie-breaking, and
empirically they do not differ even there.

---

## Counters: items examined per call

| workload | live allocs | movers | destinations |
|---|---|---|---|
| 400 rounds | 118 | 12.99 | **5.45** |
| 4 000 rounds | 1 397 | 117.94 | **60.07** |
| 40 000 rounds | 13 963 | 1 094.13 | **549.77** |

```
movers        calls  10713  visited  11 721 422  max 3510  examined 62.9%
destinations  calls  10713  visited   5 889 710  max 1206  examined 92.2%
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
| `roomy` | 1 000 | 238 ns | **181 ns** |
| | 10 000 | 240 ns | **218 ns** |
| | 100 000 | 261 ns | **220 ns** |
| `churned` | 1 000 | 7.40 µs | **5.58 µs** |
| | 10 000 | 122 µs | **114 µs** |
| | 100 000 | **670 µs** | 1 063 µs |
| `slivers` | 1 000 | **106 µs** | 131 µs |
| | 10 000 | 1.566 ms | **586 µs** |
| | 100 000 | 14.58 ms | **4.49 ms** |

## Reading it

**Removing the compaction pause changed the verdict on `churned`.** The previous
round had destination-first losing at every size (1.9× / 2.9× / 10.6×), but that
shape stopped compacting at round 64 000 and then churned for another 236 000 —
a backlog no caller can produce. With compaction running throughout,
destination-first now **wins** at 1 000 rounds and roughly ties at 10 000. It
still loses at 100 000, by 1.6×.

So the honest summary is much closer than before, and shape-dependent:

- **`roomy`** — destination-first wins everywhere, and both stay flat in heap
  size. When a good move exists both prunes fire immediately.
- **`slivers`** — destination-first wins by 2.7× and 3.2× at the larger sizes.
  Nothing fits any gap there, so its `MoverTree` descent fails at the root and
  its per-gap work collapses.
- **`churned`** — the realistic shape, and the two are within ~1.3× of each other
  up to 10 000 rounds. The 1.6× loss at 100 000 rounds is the one clear
  regression left.

**Why the residual loss at scale.** Extrapolating the counters, at 100 000 rounds
destination-first walks ~2 800 gaps where mover-first walks ~5 500 movers — half
the items, yet 1.6× slower, so its per-item cost is roughly 3× higher. The
`α = 0` short-circuit removed one source of that (two range queries for a
discarded `r_src`); the remaining suspect is the **exact-fit branch**, which runs
for every gap: a `live_by_size` lookup plus up to three `BTreeSet::last` calls,
all of it dead work at `α = 0`, since `highest_fitting(w)` already returns the
highest allocation of size ≤ `w` and the `r_dest = +1` bonus is multiplied by
zero. That is the obvious next thing to try.

**Recommendation.** Mover-first remains the safer default — it has no shape where
it loses badly — but the case against destination-first is now much weaker than
the previous round suggested, and one identified optimization is still untried.

### Still open

- **Skip the exact-fit branch at `α = 0`** (above). Not yet done or measured.
- **The benchmark shapes are not instrumented**, so the per-item cost argument is
  inferred by extrapolating counters from a different workload rather than
  measured directly. Adding the probe to the bench would settle it.
- **`α > 0` is unmeasured entirely.** Every number here is at the shipped
  `α = 0`, where both designs are exact. The `2α` approximation destination-first
  makes above zero has been tested for correctness but never for cost.

---

## Notes on the artifacts

Only `propose_compaction_step` is run now; `lowest_fitting_gap` and
`btree_point_lookup` are unaffected by this work and were dominating the wall
clock at criterion's default 100 samples. Their last results are in git history
at `5ac45eb`.

The criterion reports are committed whole, with every `.svg` gzipped to `.svgz`
and the HTML references rewritten (`test-results/svgz.sh`) — 2.1 MB down to
1.1 MB per report.

**Caveat:** browsers decompress `.svgz` over `file://` inconsistently — Firefox
does, Chrome generally expects a `Content-Encoding: gzip` header and will show
broken images. If the plots do not render, serve the directory over HTTP or
reverse the compression:

```sh
find test-results -name '*.svgz' -exec sh -c 'gunzip -c "$1" > "${1%z}" && rm "$1"' _ {} \;
find test-results -name '*.html' -exec sed -i -E 's/\.svgz(["'"'"')])/.svg\1/g' {} +
```

Earlier rounds in git history: the single-step schedule at `686912b`, the first
burst-schedule round at `0ff1153`, and the run-to-gapless round at `5ac45eb`.
