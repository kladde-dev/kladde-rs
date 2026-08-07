# Incremental compaction in `GainGreedyHeap`

How `crates/kladde-heap` squeezes the free space out of a file a little at a time,
without ever stopping the world. This document is self-contained: it assumes no
familiarity with the rest of the project.

## 1. The problem

A **relocatable heap** hands out regions of a byte-addressed store (in practice, a
file) and remembers where each one lives:

- An **allocation** is a contiguous byte range, named by a stable **id** that its
  owner keeps. `alloc(id, size)` places one, `free(id)` releases one,
  `resize(id, new_size)` changes one's length.
- The heap owns the `id → address` table. Everything that refers to an allocation
  refers to it *by id*, and resolves through that table.
- An allocation is either **fixed-size** — its length is settled when it is
  created and `resize` is never called on it — or **resizable**. Which one it is
  is fixed when the id is minted and readable from the id itself, so the heap can
  tell the two apart without consulting anything. §4 explains why it cares.
- A **gap** is free space between two allocations.
- **`end`** is one past the highest live byte — the length the store must have.
  **`live_bytes`** is the total size of all allocations.

The heap is **compact** when `end == live_bytes`, i.e. when there are no gaps.

The single fact that makes compaction cheap here is the id indirection: because
nothing holds a raw address, **moving an allocation's bytes is a one-row table
update**. There are no references to find and rewrite. So the entire problem
reduces to deciding *which bytes to move where*, and doing it in bounded
increments.

**The goal.** Provide an operation

```
propose_compaction_step(budget) -> Option<Step>
commit_compaction_step(step)
```

such that:

1. each call copies roughly `budget` bytes (§3.1 is precise about "roughly") and
   spends bounded CPU *deciding* — which rules out scanning the heap to find a
   move;
2. repeated calls converge to a compact heap;
3. there is no phase that must run to completion. A full compaction is just this
   step in a loop, and the loop may be abandoned at any point. Whatever work was
   done is kept.

A `Step` is a contiguous byte-range move:

```rust
struct Step { from: Address, to: Address, len: Address }   // always to < from
```

The heap only decides and bookkeeps; the caller performs the copy. That split
exists because the copy is I/O, and the caller may want to chunk it, defer it, or
record it in a write-ahead log first so that a crash mid-move is recoverable.
`commit_compaction_step` then re-keys every allocation in `[from, from + len)` by
`to − from`. Note that a step's `len` can span *several* allocations — see §4.

## 2. Measuring progress: the potential

"How compact is the heap?" needs a number that (a) is minimal exactly when the
heap is compact, and (b) improves a little on every useful move, so that greedy
choices provably converge. `end` alone is a poor choice for (b): most useful
moves do not change it at all.

The measure used is a **potential over live bytes**:

```
Φ  =  Σ  address(b)
     b live
```

the sum of the addresses of every live byte. Equivalently, per allocation:

```
Φ  =  Σ  s_a · ( a + (s_a − 1)/2 )
    alloc a at address a with size s_a
```

Three properties make it the right measure:

- **Its minimum is exactly compactness.** With `L` live bytes, `Φ ≥ L(L−1)/2`,
  with equality for *every* gapless layout, whatever order the allocations sit
  in. The excess `Φ − L(L−1)/2` counts precisely the pairs (free byte, live byte)
  where the free byte sits *below* the live byte — the heap's "inversions". So
  `Φ` is minimal ⟺ there are no gaps, and the excess doubles as a
  fragmentation-debt number.
- **Every downward move improves it.** Moving `s` bytes down by distance `d`
  reduces `Φ` by exactly `s·d`, wherever in the file the move happens. Moves that
  do not immediately shorten the file are still credited.
- **It does not care how bytes are grouped.** `Φ` sums over bytes, not
  allocations, so moving one 100-byte allocation down by `d` and moving four
  25-byte allocations down by `d` score identically. This is what lets a single
  step move a whole run of neighbouring allocations (§4) and still be compared
  against single-allocation moves on the same scale.

### 2.1 The fragmentation variant

`Φ` prices only *how far bytes travel*. It is indifferent to the **shape** of the
free space: filling a gap exactly and splitting a large gap into a useless sliver
score the same. An optional term fixes that. With `G` the number of gaps and a
weight `α ≥ 0`:

```
Φ_α  =  Φ  +  α · G
```

Every gapless layout still has `G = 0`, so the minimum — and therefore what
"compact" means — is unchanged for any `α`. What changes is the ranking of moves
that are otherwise comparable. Two effects fall out:

- Vacating an allocation whose neighbours on *both* sides are free merges two
  gaps into one. Vacating one walled in by live neighbours mints a new gap. The
  term prefers the former.
- Landing in a gap of exactly the mover's size erases that gap; landing in a
  larger one leaves a remainder. The term prefers the former.

`α` is a field on the heap with a getter and setter. It **ships at `0`**, which
makes `Φ_α = Φ`; tuning it is deferred (§6).

## 3. Cost, and the gain of a move

The cost of a move is **the number of bytes copied**, which for a step is its
`len`.

Combining that with the potential gives a single figure of merit. A move of `s`
bytes down by distance `d` that changes the gap count by `r` reduces `Φ_α` by
`s·d + α·r`, at a cost of `s`. So its **gain**, per byte copied, is

```
gain  =  (s·d + α·r) / s  =  d + α·r/s
```

Reading this off:

- With `α = 0`, **gain is simply the travel distance `d`**. Cost and benefit both
  scale with size, so size cancels out and only depth matters.
- The structural bonus is a per-*move* prize, so per byte it scales as `1/s`.
  That is correct accounting: the same `+1` gap costs 10 copied bytes when won by
  moving a 10-byte allocation and 1000 when won by moving a 1000-byte one.
- `r = r_src + r_dest ∈ {−1, 0, +1, +2}`: `r_src ∈ {+1, 0, −1}` for a source with
  two, one, or no free neighbours, and `r_dest ∈ {+1, 0}` for an exact or
  inexact destination.

Gains are rationals with differing denominators, so candidates are compared by
cross-multiplication rather than by evaluating a quotient.

### 3.1 The budget

`propose_compaction_step` takes a byte `budget`. It is a **ranking input, not a
hard cap**: among candidates, one that fits the budget is always preferred to one
that does not, however much less it gains — but if *no* worthwhile candidate fits,
the best oversized one is returned anyway.

The alternative would be to report "nothing to do", which is wrong: a heap whose
only remaining useful move is one large slide would claim to be compact and never
make progress again. Since the caller can read the returned step's `len`, it is
better placed to decide whether to run it, chunk it, or skip it. The heap's
contract is "prefer to stay within `budget`", not "never exceed it".

## 4. The greedy policy

Each step picks the single highest-gain move available and does it. Two shapes of
move are considered.

**Evacuation.** A single allocation jumps from high in the file into a gap far
below it. Its gain is its full travel distance, which can be far larger than any
gap is wide. The extreme case is a handful of small allocations stranded above a
large free region: copying a few bytes releases a large suffix.

**Every** allocation is a possible evacuation mover, but the two sizednesses are
handled differently. Fixed-size allocations are minted in bulk at a handful of
distinct sizes, so they form a few densely populated size classes — and freeing
one mints a gap that is a plug-compatible slot for every other member of its
class. Resizable allocations have one-off sizes, so grouping them into classes
would produce singletons; each is instead considered on its own.

The one substantive difference is that a resizable mover earns **no exact-fit
bonus** (`r_dest = 0`). Filling a gap exactly really does erase it, but a
resizable allocation that fits snugly has to relocate again the moment it grows,
re-opening the gap and wasting both copies — so the reward would be luring the
policy into a round trip. The *source* term still applies in full: vacating a
plug between two gaps merges them whatever moved out. A useful side effect is
that with `r_dest` pinned to zero, only one destination can win for a resizable
mover — the lowest gap that fits.

**Slide.** The maximal run of *contiguous* allocations sitting directly above a
gap shifts down into it. Every byte in the run travels the gap's width, so the
gain is the gap size. The slide is what guarantees progress when nothing fits
anywhere (§4.2), and it is the only move that shifts allocations too large for
any gap. The gap chosen is the largest one, which maximizes the distance
travelled.

A slide is where a step spans several allocations. That needs no extra machinery:
the run is found by walking forward from the allocation just above the gap while
each one starts exactly where the previous ended, which costs one step per
allocation actually moved. If the run exceeds the budget, a **prefix** of it is
taken — itself a perfectly legal move, since the gap simply reopens above the
prefix.

**Choosing the destination.** For a fixed-size mover of size `s`, the best
inexact destination is the **lowest-addressed gap of width at least `s`** (lowest
maximizes `d`), and the best exact one is the lowest gap of width exactly `s`.
Those two provably suffice: among gaps that leave a remainder the lowest wins,
and among exact ones likewise, so no third candidate can beat both. With `α = 0`
the exact one only ever wins by also being lower. A resizable mover has no
exact-fit bonus, so for it the lowest fitting gap is the only candidate.

**Choosing the mover.** Within a size class, all members share the same
destination, so the highest-addressed member maximizes `d`. When `α > 0` the best
member instead maximizes `a + α·r_src/s`, which the highest member need not do —
so the class contributes three candidate movers, the highest one in each of the
three neighbour categories. Resizable allocations are not grouped, so each is its
own candidate.

**Searching them.** The search enumerates **destinations**, not movers. For a
fixed gap the best mover is whichever fits and sits highest, and one index
descent answers that — so a single visit weighs every allocation in the heap at
once, where visiting a mover would weigh one allocation against every gap. It
also front-loads the value: the lowest gap is the most valuable destination there
is, and it yields a candidate whenever *any* allocation is small enough to fit,
whereas the topmost mover frequently has nowhere to go at all.

Gaps are examined in **increasing** address, which makes the stopping rule
monotone. With `T` the topmost allocation's address, a move into the gap at
`dest` gains at most `T − dest` (plus at most `2α` from the fragmentation term,
at `s = 1`), and every unexamined gap sits at least as high — so once that
ceiling can no longer beat the best gain found, nothing deeper can. §5 gives the
loop.

This is exact at `α = 0`. Above it there is a bounded approximation: for a given
gap the search takes the highest-addressed allocation that fits, but the gain
also carries `α·r_src/s`, and neither `r_src` nor `s` is constant across the
allocations that fit — so the highest-addressed one need not be the best. The
shortfall is at most `2α` per byte, the same constant that widens the stopping
rule. Exact-fit destinations are unaffected, since they pin `s` to the gap's
width and are indexed per neighbour category.

Larger-than-needed gaps are simply **split**: the mover takes the bottom, and the
remainder becomes an ordinary gap available to the next move. This makes
"combination fits" fall out for free — a gap of width `3s` absorbs three
`s`-sized movers one after another.

### 4.1 Placement is the free version of the same move

Compaction is not the only thing that moves the potential. Every `alloc` chooses
an address, and a new allocation of size `s` at address `a` adds
`s·(a + (s−1)/2)` to `Φ`. So placement is scored against the same potential —
but **without a cost term**, because the bytes are written wherever they go. The
same `Φ` reduction that costs compaction a full copy is free at allocation time.
Placement therefore strictly dominates compaction wherever both could act, and
the right rule is simply *the lowest address that fits*.

`alloc` weighs the same two candidates the compactor weighs for a destination:
the lowest gap wide enough, and — for a fixed-size allocation only — the lowest
gap of exactly the right width, carrying the `α` bonus for erasing a gap
outright. The sizedness condition is the same one as in §4, and for the same
reason: a resizable allocation parked in a snug gap has to move again the moment
it grows, so the bonus would be luring it into a round trip. At `α = 0` the lower
address always wins either way; a large enough `α` buys a fixed-size allocation
the exact fit. Only when no gap fits at all does the heap extend past `end`.

`resize` is the exception, and deliberately: it keeps the current address when it
can — always on a shrink, and on a growth that fits the space immediately above.
Relocating would cost a copy, which puts it back in gain-versus-cost territory
rather than the free-win territory above. Nothing is lost permanently: a shrink
leaves a gap, and the compactor will find whatever move that opened up. When it
*must* relocate it places the allocation like any other, which — resizable
allocations earning no exact-fit bonus — means the lowest gap that fits the new
size.

### 4.2 Why it terminates

While any gap exists, a positive-gain candidate exists: the maximal run above the
largest gap is by definition flanked by free space above (another gap, or the top
of the heap), so sliding it either merges two gaps or lets `end` retreat —
`r ≥ +1` — and its distance term is the gap width, which is positive. So the gain
is positive for every `α`.

Every executed move reduces `Φ_α` by `s·d + α·r`, which is a positive integer and
so at least 1, and `Φ_α` is bounded below. Repeated steps therefore cannot
continue indefinitely, and the only state in which no step is proposed is one
with no gaps — which is exactly `end == live_bytes`. (A caller that stops early
because its budget ran out simply keeps the progress made; nothing is left in an
intermediate state.)

### 4.3 A worked example

Five allocations, with four gaps between them:

```
0        1000  1020        2000     2100  2110   2200  2210     2310      2420
|   A    |·····|     B     |········|  C  |······|  D  |········|    E    |
            20              100              90            100
end = 2420,  live = 2110  →  310 bytes of gaps
```

Running the loop to quiescence (all sizes fixed, `α = 0`, unbounded budget):

| step | move | gain (`d`) | bytes | result |
|---|---|---|---|---|
| 1 | `D`: `[2200, 2210)` → `1000` | 1200 | 10 | its slot merges with the gaps on both sides into one 200-byte gap at 2110 |
| 2 | `C`: `[2100, 2110)` → `1010` | 1090 | 10 | its slot merges everything above 2000 into one 310-byte gap |
| 3 | `E`: `[2310, 2420)` → `2000` | 310 | 110 | the coalesced gap now fits the tail; `end` drops to 2110 |

Fully compact after copying **130 bytes** to recover 310 bytes of free space. The
two cheap moves went first not because they were cheap but because they were the
deepest: `D` and `C` had further to fall than the 110-byte tail did.

Nothing here required lookahead. Steps 1 and 2 were not chosen *because* they
would enable step 3; they were chosen because they had the highest per-byte gain
at the time. The coalescing that made step 3 possible was a side effect. This is
the practical payoff of scoring against `Φ` rather than against `end`: an interior
move that shortens the file only indirectly is still credited immediately, so the
greedy policy takes it.

Greedy is still greedy — the choice of which gap to split is a bin-packing
decision in disguise, so there is no claim of global optimality. But no move is
ever wasted: each one strictly reduces the potential.

## 5. Pseudocode

Written against abstract queries; §7 says how each is answered efficiently.

```
propose_step(budget):
    if there are no gaps:
        return None                       # already compact

    best = { within_budget: none, overall: none }

    # ---- candidate 1: slide the run above the largest gap ----
    (gap_start, gap_len) = largest_gap()
    (run_len, truncated) = contiguous_run_above(gap_start + gap_len, limit = budget)
    if run_len > 0:
        # A whole run is flanked by free space above, so sliding it removes a
        # gap; a budget-truncated prefix merely relocates one.
        r = 0 if truncated else 1
        offer(best, gain(d = gap_len, s = run_len, r),
                    Step(from = gap_start + gap_len, to = gap_start, len = run_len),
                    budget)

    # ---- candidate 2..n: evacuations, one visit per destination ----
    # Gaps in *increasing* address, so the ceiling below only ever falls.
    T = address of the topmost allocation
    for (dest, width) in gaps_ascending():
        if best.within_budget exists and (T - dest) + 2*alpha <= gain(best.within_budget):
            break                         # see below: nothing deeper can win

        # Inexact: for a fixed destination the gain rises with the mover's
        # address, so the highest-addressed allocation that fits is the only
        # one worth trying. One index descent, whatever the heap size.
        (from, s) = highest_allocation_of_size_at_most(width)
        if from > dest:
            offer(best, gain(d = from - dest, s, r_src of from),
                        Step(from, to = dest, len = s), budget)

        # Exact: only fixed-size allocations earn the +1, and they are indexed
        # by size *and* neighbour category, so all three sub-maxima are weighed.
        for (mover, r_src) in fixed_class_sub_maxima(size = width):
            if mover > dest:
                offer(best, gain(d = mover - dest, s = width, r_src + 1),
                            Step(mover, to = dest, len = width), budget)

    return best.within_budget ?? best.overall     # prefer to fit; else the best move


offer(best, gain, step, budget):        # two tracks; an absent best counts as -inf
    if gain <= 0: return
    if step.len <= budget and gain > gain(best.within_budget): best.within_budget = step
    if gain > gain(best.overall):                              best.overall      = step


commit_step(step):
    movers = all allocations starting in [step.from, step.from + step.len)
    remove every mover                    # the whole span becomes one gap
    for m in movers, in ascending address order:
        reinsert m at (m.address - (step.from - step.to))
```

The `break` deserves a word, since it is what keeps the search from examining
every gap. A move's gain is `d + α·r/s`, and a move into the gap at `dest` has
`d = mover − dest ≤ T − dest` where `T` is the topmost allocation. So its gain
cannot exceed `(T − dest) + 2α` — the `2α` being the term's largest possible
per-byte value, at `s = 1`; the tighter `2α/s` is not monotone and so would not
be a sound stopping rule. Since gaps are examined in *increasing* address, that
ceiling only ever falls, and once it no longer beats the best gain found, nothing
deeper can.

The bound compared against is deliberately the *within-budget* best: it is no
larger than the overall best, so pruning on it is sound for both tracks. Before
any within-budget candidate turns up the bound is 0 and nothing is pruned. That
happens when no gap fits anything at all, which is a real state and the one
remaining case where the search reaches the last gap (§6).

The caller drives it:

```
compact_incrementally(budget):
    moved = 0
    loop:
        step = propose_step(budget - moved)
        if step is None: break                      # quiesced: compact
        if step.len > budget - moved and moved > 0: break   # save it for next time
        copy step.len bytes from step.from to step.to       # I/O; may be logged first
        commit_step(step)
        moved += step.len
        if moved >= budget: break
    truncate the store to `end`
```

Two details. The copy must tolerate **overlapping** ranges — a slide's
destination overlaps its source whenever the gap is narrower than the run — and
because the move is always downward, a front-to-back copy is safe. And an
over-budget step is executed only when nothing has been moved yet, so a single
oversized allocation can neither be starved forever nor blow the budget on top of
work already done.

## 6. Deferred

Discussed, not implemented:

- **A more realistic cost model.** Cost is currently "bytes copied". On real
  hardware a contiguous transfer costs roughly `c₀ + c₁·s`: a fixed per-operation
  term (syscall, page-cache work, 4 KiB page granularity — a small write to a cold
  page is a read-modify-write of the whole page) plus a throughput term. Under
  `gain/cost = (s·d + α·r)/(c₀ + c₁·s)`, bulk moves are unaffected while tiny ones
  are discounted by their fixed overhead. This would not change the machinery: the
  cost stays a function of `s` alone and stays independent of the destination,
  which is what the "three movers, two destinations" argument relies on. Only the
  class-visiting order changes.
- **Tuning `α`.** Ships at 0. There is no measurement yet indicating a good value,
  or that a nonzero one pays for itself.
- **Bounding the number of gaps examined.** The ceiling rule stops the search
  early whenever a good move exists, but a heap whose gaps are all too narrow for
  anything to fit establishes no bound at all and the search reaches the last
  gap. A cap would bound that, and it would be principled rather than arbitrary:
  gaps are examined deepest-first, so capping at `K` yields the best move among
  the `K` most valuable destinations.
- **Rewarding destination gaps that are an integer multiple of the mover.** A gap
  of width `k·s` absorbs an `s`-sized mover and leaves a `(k−1)·s` remainder that
  is *itself* an exact fit for the same class, so no sliver is stranded. Worth
  scoring explicitly rather than leaving to chance.
- **A dedicated full-compaction routine.** Today a full compaction is this step in
  a loop. An algorithm that may run to completion could plan the whole permutation
  at once and beat the incremental one on total bytes copied.
- **Redirecting writes that have not landed yet.** Steps currently run up to a
  fixed budget whenever the caller flushes its buffered writes, and the compaction
  loop copies bytes that are already in the store. But an allocation created
  during the same transaction has its bytes *still in the buffer* when compaction
  runs. Moving one of those should not be a copy at all — the pending write can
  simply be re-addressed, saving both a read and a write. This is the part §4.1's
  placement rule cannot reach, since the move decision comes after placement.
  (Note that the groundwork is already there: a journaling caller mints ids
  immediately but assigns addresses at flush, so placement already sees the whole
  transaction's frees before choosing anywhere to put anything, and claims the
  largest pending allocation first so the big gaps are still intact when it is
  served.)

## 7. The data structures

Everything above is stated in terms of a handful of queries. This section is how
they are answered without scanning, which is what makes "bounded CPU per step"
true rather than aspirational. Addresses are `u64`, allocation sizes `u32`.

The **single source of truth** is one address-keyed map of allocations:

```rust
allocations: BTreeMap<u64, Entry { len: u32, id: Id }>
```

Gaps are *not* stored here — the gap preceding the entry at `a` runs from the
previous entry's end to `a`, and `end` is the last entry's end. So there is never
a trailing gap, and freeing the topmost allocation shortens the file for free.
Everything else is a derived index, maintained by the two primitives every
mutation funnels through (insert one allocation, remove one allocation):

| index | shape | answers |
|---|---|---|
| `by_id` | `HashMap<Id, u64>` | `lookup(id)` — this *is* the id table's address column |
| `free_by_size` | `BTreeMap<u64, BTreeSet<u64>>` | the *largest* gap; the lowest gap of a width *exactly* `s` |
| `gaps` | B+ tree of gaps keyed by address, augmented with each subtree's longest gap | the lowest gap of width *at least* `s` |
| `live_by_size` | `BTreeMap<u32, [BTreeSet<u64>; 3]>`, fixed-size allocations only | each class's highest member, per neighbour category, for *exact*-fit destinations |
| `movers` | B+ tree of allocations keyed by address, augmented with each subtree's smallest | the highest-addressed allocation of size *at most* `w` |

`movers` and `gaps` are mirror images, and between them they are the search: the
loop walks `gaps` ascending and asks `movers` one question per gap. `gaps`
minimizes an address subject to a *lower* bound on length and descends
leftmost-first; `movers` maximizes an address subject to an *upper* bound on size
and descends rightmost-first. Both are 2-D dominance queries that neither an
address-ordered nor a size-ordered map answers alone, and in both the length has
to sit in the key because the crate's leaf-level search sees only keys.

One of the indexes deserves a longer note.

**`gaps` is an augmented tree because "lowest gap of width ≥ `s`" is a 2-D
dominance query** — minimize address subject to a size bound — which neither an
address-ordered nor a size-ordered map answers alone. Augmenting each subtree with
the longest gap it contains turns it into a descent: take the first child whose
maximum is at least `s`, recurse, and the first qualifying leaf entry is the
answer. `std` has no augmented `BTreeMap`, so this uses the `sweep-bptree` crate.
Gaps are materialized as tree *entries* (with the length in the key, since the
crate's leaf-level search sees only keys), which keeps the augmentation a plain
bottom-up maximum.

A whole augmented tree is a lot of machinery for one query, so it is worth saying
why the obvious cheaper answer is not enough. `free_by_size` can answer the same
question by walking upward from `s` and taking the lowest address across the
classes it visits — one probe per distinct gap *width* at least `s`. The tests use
exactly that as the reference the descent is checked against. But its cost is
governed by how many distinct widths exist, and `benches/lowest_fitting_gap.rs`
measures the consequence: with widths clustered on four values the scan is
marginally the faster of the two, at 64 distinct widths the descent is 7–9×
faster, and at 1024 it is over 100× (976 µs against 8.3 µs per 256 queries on
100k gaps), while the descent stays within a 5–8 µs band throughout. Since
splitting a gap leaves a remainder of arbitrary width, and resizable allocations
are of arbitrary width to begin with, the widths spread out no matter how
disciplined the fixed-size classes are — so the flat profile is worth the constant
factor in the clustered case.

**`live_by_size` is split three ways** by whether an allocation has two, one, or
no free neighbours, because that is what determines `r_src`. A move can change the
category of the (at most two) allocations adjacent to it, so each insert and
remove un-indexes its neighbours, mutates, and re-indexes them. It holds only
fixed-size allocations, because only they earn the exact-fit bonus that this
index serves.

Costs: every foreground operation (`alloc`, `free`, `resize`) is `O(log n)` and
touches `O(1)` size classes. `propose_compaction_step` is `O(log n)` per *gap*
examined, and the ceiling rule usually stops it after one or two. Committing a
step is `O(k log n)` for the `k` allocations it moves — proportional to the work
being done. All indexes together hold `O(live + gaps)` entries, a constant factor
on the id table that has to exist anyway.

Measurements of the search's actual cost, and of what an earlier mover-enumerating
version cost, are in [`test-results/`](test-results/README.md).
