# Bounded compaction search, mode (a): restart every call

`SearchCap::Restart(k)` examines at most `k` candidates and **always begins at
the extreme end** of the walk — the lowest gap for destination-first, the highest
mover for mover-first. Every call re-walks the same prefix; nothing past it is
ever seen.

This is the companion to [`capped-resume.md`](capped-resume.md), which measures
the same two designs under a cursor that carries across the calls of one burst.
The uncapped baseline both are compared against is [`README.md`](README.md).

**Chosen `k` = 16**, for both search designs. See [Choosing `k`](#choosing-k) —
the ~1% fragmentation target does not constrain the choice at all, which is the
first surprise in this round.

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

Both branches ran the benchmark from an empty `target/criterion`, and each
report contains all three cap settings, so `capped-movers` and
`capped-destinations` are also the source for `capped-resume.md`.

## What is measured

The schedule is unchanged from [`README.md`](README.md): churn for **25
operations**, then a **burst** — `compact_incrementally(2048)` — which loops
internally until the budget is spent. The burst fires at the *end* of each
interval and 25 divides every round count, so every run stops immediately after
one.

What changed is that `compact_incrementally` is now a method on the heap rather
than a loop in the harness. Under mode (a) that is not strictly necessary, but it
is what gives mode (b)'s cursor a scope, and keeping one implementation means the
two modes differ only in the cap.

Three quantities are reported, and they answer different questions:

- **Overhead** (`(end − live_bytes) / live_bytes`) — does the bound cost
  fragmentation? This is the constraint being held.
- **µs/burst**, sampled at ten points *inside* the simulation — does the bound
  make the cost flat in file size? This is the point of the exercise, and it is
  the measurement that matters most, because it gives a scaling curve rather than
  three points.
- **Criterion timings** — the same burst cost measured properly, on three heap
  shapes.

## The headline: the bound holds cost flat, at no cost to fragmentation

| workload | catch-up steps | overhead, uncapped | overhead, `Restart(16)` |
|---|---|---|---|
| 400 rounds | **0** | 0.42% | 0.42% |
| 4 000 rounds | **0** | 0.91% / 0.95% | 0.61% / 0.55% |
| 40 000 rounds | **0** | 0.82% / 0.81% | **0.44% / 0.51%** |

(destination-first / mover-first, where they differ.)

Sampling along the 40 000-round run is where the result actually lives.
**Destination-first:**

| round | allocations | end | gaps | overhead | visited/call | µs/burst |
|---|---|---|---|---|---|---|
| 3 999 | 1 397 | 100 789 | 66 | 0.61% | 13.3 | 103.0 |
| 7 999 | 2 746 | 204 970 | 121 | 0.59% | 16.0 | 139.6 |
| 11 999 | 4 147 | 313 678 | 194 | 0.57% | 16.0 | 132.3 |
| 15 999 | 5 550 | 422 524 | 244 | 0.49% | 16.0 | 109.8 |
| 19 999 | 6 920 | 527 563 | 323 | 0.52% | 16.0 | 117.9 |
| 23 999 | 8 280 | 620 970 | 366 | 0.40% | 16.0 | 116.3 |
| 27 999 | 9 757 | 730 745 | 424 | 0.39% | 16.0 | 143.3 |
| 31 999 | 11 152 | 846 308 | 492 | 0.36% | 16.0 | 134.4 |
| 35 999 | 12 529 | 959 668 | 544 | 0.41% | 16.0 | 142.0 |
| 39 999 | 13 963 | 1 073 285 | 604 | 0.44% | 16.0 | **132.8** |

**Mover-first:**

| round | allocations | end | gaps | overhead | visited/call | µs/burst |
|---|---|---|---|---|---|---|
| 3 999 | 1 397 | 100 722 | 115 | 0.55% | 15.0 | 87.6 |
| 7 999 | 2 746 | 204 821 | 178 | 0.51% | 16.0 | 91.3 |
| 11 999 | 4 147 | 313 932 | 287 | 0.65% | 16.0 | 79.2 |
| 15 999 | 5 550 | 422 448 | 356 | 0.48% | 16.0 | 82.4 |
| 19 999 | 6 920 | 527 313 | 474 | 0.47% | 16.0 | 86.0 |
| 23 999 | 8 280 | 620 934 | 507 | 0.40% | 16.0 | 99.0 |
| 27 999 | 9 757 | 730 951 | 614 | 0.42% | 16.0 | 95.5 |
| 31 999 | 11 152 | 847 026 | 736 | 0.45% | 16.0 | 92.7 |
| 35 999 | 12 529 | 960 508 | 865 | 0.50% | 16.0 | 93.9 |
| 39 999 | 13 963 | 1 073 951 | 964 | 0.51% | 16.0 | **100.3** |

Against the uncapped baseline over the same 10× growth in live allocations:

| | 1 397 allocs | 13 963 allocs | growth |
|---|---|---|---|
| destinations, uncapped | 226.6 µs | 1 807.5 µs | **8.0×** |
| destinations, `Restart(16)` | 103.0 µs | 132.8 µs | **1.29×** |
| movers, uncapped | 118.0 µs | 3 768.4 µs | **31.9×** |
| movers, `Restart(16)` | 87.6 µs | 100.3 µs | **1.15×** |

**That is the result.** A 10× heap costs the bounded search 1.15–1.29× more per
burst, against 8× and 32× unbounded. `visited/call` pins at exactly 16.0 from the
second sample onward, so the cap — not the prune — is what stops every walk, and
the residual growth is not search at all: it is `commit_compaction_step` moving
runs, plus `slide_candidate`'s budget-bounded walk.

Mover-first's uncapped 31.9× over a 10× heap is worth noting on its own. It is
*super*linear, because its walk length tracks the resizable population while its
per-item work also grows.

## Bounding the search does not cost fragmentation — it slightly improves it

Overhead does not merely survive the bound: it falls, 0.82% → 0.44% on
destination-first and 0.81% → 0.51% on mover-first, with the gap count following
(1 218 → 604 and 1 198 → 964).

**In absolute terms this is a much smaller effect than "halves" suggests**, and
the free-space accounting is the way to see it. Every compaction step *conserves*
free space: it consumes `len` free bytes at the destination and releases `len`
where the mover was. Free space is created only by freeing something mid-heap,
and destroyed only two ways — a placement landing in a gap, or a move that
vacates the top of the heap so the freed bytes end up above `end`. Over the
40 000-round run:

| bytes, 40 000 rounds | dest. uncapped | dest. `Restart(16)` | movers uncapped | movers `Restart(16)` |
|---|---|---|---|---|
| destroyed by placement into a gap | 624 375 | 826 952 | 628 625 | 627 939 |
| destroyed by truncation at the top | 207 537 | 8 939 | 203 605 | 207 312 |
| **total destroyed** | **831 912** | **835 891** | **832 230** | **835 251** |
| standing free space at the end | 8 783 | 4 748 | 8 659 | 5 414 |

All four configurations destroy the same amount of free space to within **0.5%**.
Standing free space is the small residual between creation and destruction, so a
sub-percent shift in that balance moves it by a factor of two. The bounded search
is not removing twice as much free space; it is running a near-identical flow
about half a percent leaner.

**And the route differs by branch, which rules out a single mechanism.**
Destination-first's bounded run shifts massively from truncation to placement
absorption — it truncates 23× *less* and absorbs 202 KB more into gaps.
Mover-first's bounded run does neither: its placement and truncation figures are
within 1% of its own unbounded run, and it still ends leaner. Whatever produces
the residual, it is not "the bounded search truncates more" and it is not "the
bounded search leaves more usable gaps" — both are measured and both fail on one
branch or the other.

The widest gap at the end of the run was the obvious candidate for "leaves more
usable gaps" and does not support it either: 62 bytes uncapped vs 122 capped on
destination-first, 64 vs 62 on mover-first. No consistent signal.

**So: the effect is real and persistent across all ten snapshots of both runs,
its size is a ~0.5% shift in the free-space balance, and its cause is not
established.** See [Still open](#still-open).

What *is* established, and is worth keeping in view: at `α = 0` the search does
not price the gap count at all. `Gain::new(d, s, r, 0)` discards `r`, and
`offer_evacuations_into` does not even compute it (`if self.alpha == 0 { 0 }`).
So whatever an evacuation does to the gap count — `FreeNeighbours::Both` merges
two gaps and scores `r_src = +1`, `Neither` mints one and scores `−1` — is an
unpriced side effect of the distance-maximizing choice, not something selected
for. That is what `α > 0` exists to fix, and it remains untested.

It also means the `k` tuning the task called for has no tension to resolve: there
is no `k` in the range measured at which fragmentation approaches 1%.

## Choosing `k`

`search_cap_sweep` over the 40 000-round workload, `peak%` being the worst
overhead over the second half of the run (i.e. after burn-in):

**Destination-first:**

| cap | peak% | final% | gaps | visited/call | µs/burst |
|---|---|---|---|---|---|
| `Unbounded` | 0.83% | 0.82% | 1 218 | 1 075.3 | 1 871.6 |
| `Restart(4)` | 0.44% | 0.44% | 613 | 4.0 | 115.7 |
| `Restart(8)` | 0.44% | 0.44% | 601 | 8.0 | 107.8 |
| **`Restart(16)`** | **0.44%** | **0.44%** | **604** | **16.0** | **105.5** |
| `Restart(32)` | 0.46% | 0.46% | 623 | 32.0 | 120.2 |
| `Restart(64)` | 0.51% | 0.50% | 671 | 63.9 | 134.6 |
| `Restart(128)` | 0.52% | 0.52% | 690 | 127.7 | 172.3 |
| `Restart(256)` | 0.77% | 0.60% | 805 | 255.1 | 277.5 |
| `Restart(512)` | 0.79% | 0.65% | 918 | 506.3 | 477.9 |

**Mover-first:**

| cap | peak% | final% | gaps | visited/call | µs/burst |
|---|---|---|---|---|---|
| `Unbounded` | 0.82% | 0.81% | 1 198 | 2 124.1 | 3 759.2 |
| `Restart(4)` | 0.40% | 0.40% | 716 | 4.0 | 99.9 |
| `Restart(8)` | 0.45% | 0.45% | 835 | 8.0 | 99.7 |
| **`Restart(16)`** | **0.51%** | **0.51%** | **964** | **16.0** | **94.2** |
| `Restart(32)` | 0.60% | 0.60% | 1 095 | 32.0 | 106.2 |
| `Restart(64)` | 0.53% | 0.53% | 1 086 | 63.8 | 104.6 |
| `Restart(128)` | 0.60% | 0.60% | 1 157 | 127.2 | 144.6 |
| `Restart(256)` | 0.60% | 0.60% | 1 193 | 252.3 | 232.5 |
| `Restart(512)` | 0.67% | 0.67% | 1 209 | 494.9 | 553.8 |

Overhead is **monotonically worse in `k`** on both designs, and cost is monotone
too below ~16, so the criterion the task set — hold fragmentation near 1% — is
satisfied everywhere and selects nothing. `k = 16` is chosen instead as the
smallest value at which the search is still doing recognizable work: below it the
walk is short enough that the result is essentially "always slide", and the
comparison between the two search designs would stop meaning anything. The same
`k` on both branches also keeps the four experiments directly comparable.

Note that `µs/burst` bottoms out around 95–110 µs and will not go lower however
small `k` gets. That floor is commit and slide cost, not search.

---

## Counters: items examined per call

40 000-round workload, during bursts:

| | mean visited | max | examined | calls |
|---|---|---|---|---|
| movers, uncapped | 1 088.29 | 3 511 | 63.0% | 12 745 |
| movers, `Restart(16)` | **15.87** | 16 | 0.94% | 7 564 |
| destinations, uncapped | 552.30 | 1 218 | 92.2% | 12 793 |
| destinations, `Restart(16)` | **15.57** | 16 | 5.44% | 4 789 |

`max` equals `k` exactly, which is the bound doing its job. The `examined`
column — the fraction of the *whole* candidate space each call touches — now
falls as the heap grows, which is the asymptotic statement: a fixed 16 candidates
out of a linearly growing set.

The call count drops too (12 793 → 4 789 on destination-first) because the bursts
need fewer, larger steps.

## Benchmark: `propose compaction step`

`roomy` and `slivers` are sized in allocations; `churned` is sized in **rounds**,
and with 60% allocate / 25% free its live count settles near a third of that.

| shape | n | movers, uncapped | movers, `restart` | destinations, uncapped | destinations, `restart` |
|---|---|---|---|---|---|
| `roomy` | 1 000 | 245 ns | 243 ns | 224 ns | 229 ns |
| | 10 000 | 255 ns | 243 ns | 252 ns | 247 ns |
| | 100 000 | 287 ns | 285 ns | 260 ns | 265 ns |
| `churned` | 1 000 | 342 µs | **54.6 µs** | 208 µs | **98.2 µs** |
| | 10 000 | 1.284 ms | **63.3 µs** | 1.387 ms | **76.1 µs** |
| | 100 000 | 12.12 ms | **140.6 µs** | 10.09 ms | **119.2 µs** |
| `slivers` | 1 000 | 112 µs | **1.60 µs** | 147 µs | **2.59 µs** |
| | 10 000 | 1.184 ms | **1.47 µs** | 703 µs | **1.51 µs** |
| | 100 000 | 14.20 ms | **1.44 µs** | 6.94 ms | **1.37 µs** |

`roomy` is unchanged, as it must be: a good move exists immediately, the prune
fires on the first candidate, and the cap never binds.

`slivers` is the pathological shape — every gap narrower than every allocation,
so no evacuation exists and no bound is ever established. Uncapped, the walk runs
to the end of a linearly growing list: 14.2 ms at n = 100 000. Capped, it is
**1.4 µs and completely flat**, a 10 000× reduction at the largest size. This is
the clearest demonstration that the bound is doing what it claims.

`churned` — the realistic shape — grows 2.6× (movers) and 1.2× (destinations)
across a 100× growth in rounds, against 35× and 49× uncapped.

### How `churned` is measured

`churned(n, cap)` runs `n` rounds with a burst at the end of every 25 — all but
the last, which is what the benchmark times. The state is saved and restored by
cloning in `iter_batched_ref`'s `setup`, which criterion excludes from the
timing; `BatchSize::PerIteration` keeps one clone alive at a time. So a `churned`
figure is **the cost of one flush's worth of compaction**, not of one decision.

Two details matter for reading it:

- **The cap applies to the setup too.** A bounded search leaves a measurably
  different heap behind — half the gaps — so timing a bounded burst on a state an
  unbounded search produced would time a state no caller can reach.
- **Measurement time is reduced for this shape** (750 ms, from 5 s). A bounded
  burst is ~50× cheaper than the clone that restores its input, so criterion's
  default would spend minutes cloning per benchmark. With `sample_size(10)` the
  estimator is unchanged; only the iteration count falls.

Both trees clone in linear time via `bulk_load`, which is why the clone is
affordable at all.

## Reading it

**The bound achieves what it was built for, on both designs, with no cost to
fragmentation.** Per-burst cost grows 1.15–1.29× across a 10× heap, against 8×
and 32× unbounded; the pathological shape flattens completely.

**Mover-first is now slightly cheaper than destination-first on the realistic
shape** — 100.3 vs 132.8 µs/burst in-simulation, though criterion has them the
other way round at n = 100 000 (140.6 vs 119.2 µs), so the two are within noise
of each other. That is expected: with the walk clipped to 16 items, neither
design's enumeration cost dominates any more, and what is left is the shared
commit-and-slide work. **The bound largely erases the difference between the two
designs**, which is itself an argument for the simpler one.

**Destination-first keeps a real edge on fragmentation**: 0.44% and 604 gaps
against 0.51% and 964. Its bounded prefix — the 16 lowest gaps — is a better
16 candidates than mover-first's 16 highest movers, because a low gap is a
destination for *any* allocation above it, whereas a high mover has to find a
destination.

### Still open

- **`k` is tuned on one workload.** The sweep covers a single churn mix
  (60/25/15 alloc/free/resize, five size classes) at one budget and interval. A
  workload with a different gap-size distribution could plausibly want a
  different `k`, and nothing here would have caught that.
- **The floor is commit, not search.** ~95–110 µs/burst remains at any `k`, and
  it still grows slowly with the heap (1.15–1.29×). If the burst cost has to be
  genuinely `O(log n)`, the next thing to bound is the *slide*: `run_len_from`
  walks a budget's worth of allocations, and `commit_compaction_step` re-inserts
  every allocation in the moved run.
- **The bias is never corrected.** Mode (a) re-walks the same prefix on every
  call, so a candidate outside it is invisible forever, at every scale. That it
  does no harm here is an empirical fact about this workload, not a property.
  Mode (b) is the attempt to fix it — see [`capped-resume.md`](capped-resume.md).
- **`α > 0` is unmeasured.** Every number here is at the shipped `α = 0`, where
  the gap count is not priced at all. Since the fragmentation differences above
  are unpriced side effects, `α > 0` is the obvious next experiment and the one
  most likely to make them controllable rather than incidental.
- **Why the bounded search runs leaner is not established.** The free-space
  decomposition above rules out the two mechanisms that looked plausible —
  truncating more, and leaving wider gaps — because each fails on one of the two
  branches. The remaining candidates, none tested: the *order* in which free
  space becomes available to placements (the counters are run totals and would
  hide a timing effect); the interaction between compaction and `place`'s own
  lowest-address-first policy; or simply a small persistent bias from the
  different step-size distribution. A per-round free-space time series would
  probably settle it, and the instrumentation to produce one is already in place.
- **An earlier version of this document asserted a mechanism here and was wrong
  twice.** The first claim — that evacuations split gaps — is contradicted by the
  code: `offer_evacuations_into` emits `to: dest`, the gap's low end, so an
  evacuation shrinks a gap from below and never splits it. The second — that
  slides shrink the file by truncating — is contradicted by the table above.
  Both are recorded here because the underlying question is still open and these
  are the answers already ruled out.

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
