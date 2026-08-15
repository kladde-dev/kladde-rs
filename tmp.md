null
Everything green: 82 heap tests, zero clippy warnings, formatted, docs build clean.

## What was built

Seven commits, following the Recommendation order:

| Commit | Content |
|---|---|
| `5179fb2` | Clean slate — removed `MoverTree`, `live_by_size`, `SearchCap`, per-byte `Gain`, `alpha` |
| `14d47e6` | Stage 1 — [evacuation_index.rs](crates/kladde-heap/src/evacuation_index.rs), the augmented size-keyed tree |
| `1cc2e52` | Stage 2 — the size reward, λ as a bool |
| `802ed04` | Stage 5 (minimal) — opportunistic run extension |
| `de7838a` | Stage 4 — [size_classes.rs](crates/kladde-heap/src/size_classes.rs), μ₁/μₖ |
| `55af0e9` | Stage 3 — α and neighbour re-keying |
| `dd13fc4` | The shrinking test + benchmark |

`GapTree`, `MoverTree` and `free_by_size` are all gone — the index answers `widest_gap` as a root read and `lowest_gap_fitting` as a suffix aggregate. The budget query is exact, cross-checked against brute force on every `assert_invariants` call.

## Three things I found that the document doesn't say

**α cannot express the case it was introduced for.** A plug can never outrank a walled allocation above it into the same destination: making the plug's top neighbour free requires a gap above it, and the allocation just above that gap has free space below, so it scores un-penalised at `≥ plug.pos + plug.size + 1` — already more than `plug.pos + α_eff`. Forcing intermediates to be too large to fit the destination only pushes the walled one out of α's reach. The two must aim at *different* destinations, which took a ten-allocation layout to construct. α is a near-tie-breaker, not a policy lever, and the sign bound is what makes it one. The doc's own "dual aggregates with fallback" is the documented escape hatch; I did not adopt it unasked.

**λ and α are mutually exclusive.** The bound gives an allocation `A.size` of headroom and `reward(s) = s` spends all of it. I also applied the bound *per allocation* rather than globally — the sign argument is per-pair, so this is equally safe and avoids one 1-byte allocation making α inert heap-wide.

**Stage 5's upward extension is dead code today,** because any absorbable upper neighbour is itself a strictly better candidate. I kept the loop and recorded the argument, since stage 4 breaks its premise (a tiling candidate chosen by `score + μ` can be a *lower* allocation).

## One thing I deliberately left out

The de-duplicating `(class, nc)` bucketing. Its benefit is update traffic, not correctness, and by the document's own accounting it's a wash for alloc/free and pays only for α's re-keying — while requiring the index to hold one entry per bucket rather than per allocation. That's a substantial change justified only by an unmeasured claim; the benchmark's `gapcount` axis is what would settle it. Everything else in the document is implemented.

First numbers from the new shrinking measurement, at the shipped defaults: overhead climbs from ~1% while growing to a 6% mean / 10% peak while draining, while burst cost stays flat at ~160 µs for 1400 allocations. The drain costs shape, not time — which is exactly what stages 3 and 4 price, and now there's a workload that exercises them.
1. I don't understand your paragraph "α cannot express the case it was introduced for". What exactly can't alpha resolve? Give a simple example of where one would have expected alpha to help but it turns out it doesn't.
2. What do you mean with "overhead climbs from ~1% while growing to a 6% mean / 10% peak while draining"? Is this literally just "while draining", i.e., before the compaction, or does compaction reduce the overhead?
