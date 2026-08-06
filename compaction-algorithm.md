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

Only **fixed-size** allocations are considered as evacuation movers.
Fixed-size allocations are minted in bulk at a handful of distinct sizes, so they
form a few densely populated size classes — and freeing one mints a gap that is a
plug-compatible slot for every other member of its class. Resizable allocations
would instead scatter one per class, and parking one in a snug gap only
guarantees it must move again the moment it grows.

**Slide.** The maximal run of *contiguous* allocations sitting directly above a
gap shifts down into it. Every byte in the run travels the gap's width, so the
gain is the gap size. The slide is what covers allocations that generate no
evacuation candidate of their own — resizable ones included — and it is what
guarantees progress when nothing fits anywhere (§4.1). The gap chosen is the
largest one, which maximizes the distance travelled.

A slide is where a step spans several allocations. That needs no extra machinery:
the run is found by walking forward from the allocation just above the gap while
each one starts exactly where the previous ended, which costs one step per
allocation actually moved. If the run exceeds the budget, a **prefix** of it is
taken — itself a perfectly legal move, since the gap simply reopens above the
prefix.

**Choosing the destination.** For a mover of size `s`, the best inexact
destination is the **lowest-addressed gap of width at least `s`** (lowest
maximizes `d`), and the best exact one is the lowest gap of width exactly `s`.
Those two provably suffice: among gaps that leave a remainder the lowest wins,
and among exact ones likewise, so no third candidate can beat both. With `α = 0`
the exact one only ever wins by also being lower.

**Choosing the mover.** Within a size class, all members share the same
destination, so the highest-addressed member maximizes `d`. When `α > 0` the best
member instead maximizes `a + α·r_src/s`, which the highest member need not do —
so the class contributes three candidate movers, the highest one in each of the
three neighbour categories.

Larger-than-needed gaps are simply **split**: the mover takes the bottom, and the
remainder becomes an ordinary gap available to the next move. This makes
"combination fits" fall out for free — a gap of width `3s` absorbs three
`s`-sized movers one after another.

### 4.1 Why it terminates

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

### 4.2 A worked example

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

    # ---- candidate 2..n: one evacuation per fixed-size class ----
    for each fixed-size class s, in descending order of the class's top address:
        if best.within_budget exists and top(s) + 2*alpha <= gain(best.within_budget):
            break                         # see below: no later class can win

        destinations = [ lowest_gap_of_width_at_least(s),   # r_dest = +1 iff exact
                         lowest_gap_of_width_exactly(s) ]   # r_dest = +1
        for mover in [ top member of s with 2 free neighbours,   # r_src = +1
                       top member of s with 1 free neighbour,    # r_src =  0
                       top member of s with 0 free neighbours ]: # r_src = -1
            for (dest, r_dest) in destinations:
                if dest < mover:
                    offer(best, gain(d = mover - dest, s, r_src + r_dest),
                                Step(from = mover, to = dest, len = s),
                                budget)

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

The `break` deserves a word, since it is what keeps the search sub-linear in the
number of size classes. A mover's gain is `d + α·r/s`, and `d = mover − dest ≤
mover ≤ top(s)` because destinations are non-negative, so a class's gain can
never exceed `top(s) + 2α`. Since classes are visited in descending `top`, once
the best gain found already matches that bound, no unvisited class can beat it.
The bound compared against is deliberately the *within-budget* best: it is no
larger than the overall best, so pruning on it is sound for both tracks. Before
any within-budget candidate turns up, the bound is 0 and nothing is pruned, which
is no worse than visiting every class.

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
- **Bounding the class walk near quiescence.** The `break` above prunes hard while
  a good move exists high in the file, and degrades toward one visit per class
  only when every gain is small — i.e. near quiescence, where there is little left
  to do. A visit cap with a resumable cursor would bound even that.
- **Rewarding destination gaps that are an integer multiple of the mover.** A gap
  of width `k·s` absorbs an `s`-sized mover and leaves a `(k−1)·s` remainder that
  is *itself* an exact fit for the same class, so no sliver is stranded. Worth
  scoring explicitly rather than leaving to chance.
- **A dedicated full-compaction routine.** Today a full compaction is this step in
  a loop. An algorithm that may run to completion could plan the whole permutation
  at once and beat the incremental one on total bytes copied.
- **Smarter scheduling.** Steps currently run up to a fixed budget whenever the
  caller flushes its buffered writes. A schedule aware of what is in that buffer
  could fold compaction into it — for instance placing a newly created allocation
  directly at the address compaction would have moved it to, so the bytes are
  written once instead of written and then moved.

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
| `live_by_size` | `BTreeMap<u32, [BTreeSet<u64>; 3]>`, fixed-size allocations only | each class's highest member, per neighbour category |
| `tops` | `BTreeMap<u64, u32>` | classes in descending order of top address, for the pruned walk |

Two of these deserve a note.

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
remove un-indexes its neighbours, mutates, and re-indexes them.

Costs: every foreground operation (`alloc`, `free`, `resize`) is `O(log n)` and
touches `O(1)` size classes. `propose_compaction_step` is `O(log n)` per size
class visited, with the walk usually stopping after one or two. Committing a step
is `O(k log n)` for the `k` allocations it moves — proportional to the work being
done. All indexes together hold `O(live + gaps)` entries, a constant factor on the
id table that has to exist anyway.
