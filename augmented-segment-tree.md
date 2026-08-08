# Finding the best evacuation in `O(1)`

An augmented B+ tree over allocation and gap *sizes* that keeps the best
compaction move at its root, instead of searching for it.

## The problem

The heap partitions an address space `[0, end)` into **allocations** — live byte
ranges, each with a stable id — and **gaps**, the free space between them.
Because ids are stable, moving an allocation's bytes is a table update and not a
pointer fixup, so the compactor is free to move anything anywhere.

Compaction proceeds one bounded **step** at a time. The step shape this document
is about is the **evacuation**: an allocation `A` jumps down into a gap `G` that
sits below it and is wide enough to take it,

```
G.width >= A.size        and        G.pos < A.pos
```

landing at `G.pos`, the gap's low end. (Landing anywhere else would travel less
far for the same cost, so the low end is always at least as good.)

Choosing *which* evacuation to perform is the expensive part. The obvious
formulations enumerate one side and query the other: walk allocations from the
top down and ask for the lowest gap that fits each, or walk gaps from the bottom
up and ask for the highest allocation that fits in each. Both admit a
branch-and-bound prune, and both have been implemented and measured on this
project. Both are, in practice, **linear in the heap size** — the prune skips
37% of the candidate space in one design and 8% in the other, and the mean number
of candidates examined per call grows in proportion to the number of
allocations.

This document develops a different answer. Rather than search for the best pair,
maintain a data structure whose root *is* the best pair, updated in `O(log n)`
as the heap changes. The core observation is that the constraint
`G.width >= A.size` is **monotone in size**, which is exactly the property an
augmented search tree keyed by size can exploit.

The development is in five stages, each one adding something the previous stage
could not express:

1. The simplest objective — potential, and cost measured in bytes moved.
2. An objective that prefers moving *large* allocations, for cost models with a
   fixed per-step overhead.
3. Pricing what a move does to the gap count **at the source**.
4. Pricing what it does **at the destination**, for fixed-size allocations, by
   preferring gaps whose width is an integer multiple of the allocation's size.
5. Moving **runs** of adjacent allocations together — deferred, except for a
   cheap opportunistic version adopted now.

Three sections follow: alternatives considered and rejected, the algorithms and
the data structures that implement them, and an assessment against what is
currently implemented.

---

## 1. The simplest setting

### The objective

Take the potential

```
Φ = Σ over live bytes b of address(b)
```

which is minimized, for a given number of live bytes, exactly when the heap is
gapless. Moving an allocation `A` of size `A.size` down into a gap at `G.pos`
changes it by

```
ΔΦ = − A.size · (A.pos − G.pos)
```

— size times distance travelled. Take the cost of a step to be the number of
bytes copied, `A.size`. Then

```
gain / cost  =  A.size · (A.pos − G.pos) / A.size  =  A.pos − G.pos
```

The size cancels. **The best move under this pairing of objective and cost is
simply the one that travels furthest**, and the query we need is:

> Among all pairs `(A, G)` with `G.width >= A.size`, maximize `A.pos − G.pos`.

The constraint `G.pos < A.pos` needs no separate handling: any pair violating it
scores `<= 0`, so it can only win when no beneficial move exists at all, which is
detectable from the sign of the answer. (Stages 2 and 3 add terms that can mask
that sign, and each states the bound that restores it.)

### Two facts about distance, used throughout

A gap and an allocation never overlap, so if `G.pos < A.pos` then
`G.pos + G.width <= A.pos`, and therefore

```
valid downward pair:   A.pos − G.pos  >=  G.width  >=  A.size
```

Symmetrically, if `G.pos > A.pos` then `A.pos + A.size <= G.pos`, so

```
upward pair:           A.pos − G.pos  <=  −A.size
```

Every sign argument below rests on this pair of bounds.

### The structural insight

Sort every gap and every allocation onto a single axis **by size**. Now split
that axis anywhere. Every gap on the high-size side is wide enough for every
allocation on the low-size side — the constraint is satisfied for the entire
cross product, for free, without looking at any individual size.

So for a pair drawn across the split, the best one is obtained by maximizing the
two sides independently:

```
best cross pair  =  (highest-addressed allocation below the split)
                    −
                    (lowest-addressed gap above the split)
```

That is an `O(1)` combination of two aggregates, and it is what makes a tree over
sizes work. A subtree can summarize everything it contains in three numbers, and
a parent can combine its children's summaries without descending into them.

### One tree, not a tree plus per-size heaps

Everything — gaps, fixed-size allocations, resizable allocations — goes into a
single B+ tree. There is **no per-size-class heap and no separate structure for
resizable allocations**; the key does that work.

The key packs an `is_gap` flag into the low bit of a widened size field:

```
key = ( (size << 1) | is_gap ,  score ,  address )
```

where `size` is `A.size` for an allocation and `G.width` for a gap, `is_gap` is
`false` for allocations and `true` for gaps, and `score` is `A.pos` / `G.pos` at
this stage (stages 2 and 3 enrich it). The size field is `u64` even though
allocation sizes are `u32`, because gap widths are already `u64` — so the shifted
flag costs nothing real. The formal ceiling moves from `2^64 − 1` to `2^63 − 1`
on *gap width*, which is an eight-exabyte gap; allocation sizes are untouched.

The flag earns its bit twice over.

**First, ordering.** Consider `(size, address)` alone, with an allocation `A` and
a gap `G` of the *same* size and `G.pos < A.pos`. That pair is valid —
`G.width = A.size` satisfies `>=` — and may well be the best available. But `G`
sorts *before* `A`, so a merge rule that pairs allocations with gaps to their
right would never see it. With `is_gap` in the low bit of the size field,
allocations precede gaps at equal size, and:

> `key(G) > key(A)` **if and only if** `G.width >= A.size`.

If `G.width > A.size` the size field decides; if `G.width = A.size` the flag
decides, and the pair is valid; if `G.width < A.size` then `key(G) < key(A)` and
the pair is invalid. The key order *is* the validity relation.

**Second, and decisively, identification.** The merge treats the two kinds
completely differently — one feeds `min_gap_pos`, the other `max_alloc_score` —
and in `sweep-bptree` the augmentation is computed by `from_leaf(keys)`, which is
handed the **keys and nothing else**. So the key must say which kind an entry is.
The tempting alternative of inflating gap sizes by one to get the ordering
(`key = (width + 1, ...)` for gaps) achieves the ordering but destroys this: a
key `(5, …)` is then either an allocation of size 5 or a gap of width 4, with
nothing to distinguish them, and parity does not help because `width + 1` has
arbitrary parity. That alternative is not merely less exact — it is
unimplementable against this interface.

Because the address is the last key component, no auxiliary per-size heaps are
needed either. A "min-heap of gap start addresses for all gaps of size `L`" is
precisely the address-ordered run of entries with key prefix `(L, gap)`, and its
minimum is the aggregate of the subtrees covering that run.

### The augmentation

Every subtree — every B+ tree node, and every leaf — carries:

| field | meaning | identity |
|---|---|---|
| `min_gap_pos` | lowest `G.pos` over gaps in the subtree | `u64::MAX` |
| `max_alloc_score` | highest `score` over allocations in the subtree | `0` |
| `best` | max of `score(A) − G.pos` over valid pairs *entirely inside* the subtree | `0` |
| `best_pair` | the `(A, G)` achieving `best` | — |

The first two are plain minima and maxima. `best` is what makes the root
answer the query in `O(1)`.

**On representation.** Everything is `u64` and the differences are computed with
`saturating_sub`, which is not a detail — plain `u64` subtraction wraps an upward
pair into a huge positive number and poisons the maximum. With saturation,
`0.saturating_sub(x) == 0` and `x.saturating_sub(u64::MAX) == 0`, so the
identities above behave correctly and every non-beneficial pair collapses to `0`.
Using `0` rather than `−∞` as the identity for `max_alloc_score` conflates "no
allocation here" with "an allocation scoring 0", which is exact rather than
merely tolerable: an allocation at address 0 with no bonus cannot move down at
all, so its true contribution is `0` either way. And `best == 0` is unambiguously
"nothing beneficial", because a genuine gain of exactly zero is impossible — a
gap and an allocation cannot start at the same address.

### The merge

A node's state is computed from its children's, which are in ascending key order
`c[0] .. c[k−1]`. A pair inside this node either lives entirely inside one child,
or crosses — allocation in `c[i]`, gap in `c[j]`, `i < j` — and every crossing
pair is valid by the key-order property above.

A single right-to-left sweep finds the best crossing pair in `O(k)`:

```
best             = max over children of child.best
min_gap_pos      = u64::MAX
max_alloc_score  = 0

for c in children, from the largest sizes down to the smallest:
    # every gap seen so far is at a size >= anything in c
    best            = max(best, c.max_alloc_score.saturating_sub(min_gap_pos))
    min_gap_pos     = min(min_gap_pos, c.min_gap_pos)
    max_alloc_score = max(max_alloc_score, c.max_alloc_score)
```

The same sweep computes a **leaf's** state, treating each stored entry as a
degenerate child: an allocation contributes `max_alloc_score = score` and
`min_gap_pos = u64::MAX`, a gap the reverse, and both contribute `best = 0`. So
there is one merge rule, used at every level.

Tracking `best_pair` alongside `best` is bookkeeping: record which of the three
sources — a child's own `best`, or the crossing pair `(c.max_alloc, running min
gap)` — supplied the winning value, and carry the witnessing keys up with it.
Because size, score and address all live in the key, the witnesses are
recoverable from keys alone, with no side table and no second lookup.

### Operations

**Query.** Read the root's `best` and `best_pair`. `O(1)`. A zero `best` means no
evacuation improves the objective.

**Update.** Each mutation touches a bounded number of entries:

- *Allocate* — remove the gap that was consumed, insert the new allocation,
  insert the leftover gap if any. Three entries.
- *Free* — remove the allocation, remove up to two adjacent gaps, insert the
  merged gap. Four entries.
- *Commit an evacuation* — remove the allocation and reinsert it at its new
  address (its size is unchanged, so this is a key change, not just a value
  change), plus the gap splits and merges at both ends. A handful of entries.

Each insert or removal recomputes the augmentation along one root-to-leaf path:
`O(log_B n)` levels, `O(B)` work each, so `O(B log_B n)` per entry.

### Why a B+ tree and not a binary segment tree

The classical presentation of this structure is a binary segment tree indexed by
size, laid out as a flat array over the whole size domain. That is attractive
when the domain is small — no pointers, parent is `i >> 1`, everything contiguous.

It is the wrong shape here. Sizes are `u32` (and the key field `u64`), so a flat
array over the domain is out of the question, and a dynamic pointer-based binary
tree over a sparse domain means ~32 dependent pointer dereferences per update,
each a likely cache miss. A B+ tree with `B` around 16–32 compresses that to 3–5
levels, and the `O(B)` sweep inside a node runs over a contiguous array that
arrives in one or two cache lines. The theoretical cost goes from `O(log₂ n)` to
`O(B log_B n)`, which is larger on paper and considerably smaller in practice.

There is also a practical reason specific to this project: it already depends on
`sweep-bptree`, and the augmentation above is exactly its `Argument` trait —
`from_leaf(keys)` and `from_inner(keys, arguments)` correspond one-for-one to the
two uses of the sweep, `root_argument()` is the `O(1)` query, and both existing
indexes (`GapTree`, `MoverTree`) are already built this way.

### The budget, for free

`propose_compaction_step` takes a budget and prefers steps that fit inside it.
The current implementation handles this with a two-track selection: keep the best
candidate that fits, and separately the best overall, and prefer the former.

Because the tree is *keyed by size*, the budget is a prefix of the key order, and
the constrained query

> best pair with `A.size <= budget`

is answerable exactly, in `O(B log_B n)`. Descend once along the boundary
`budget`. As the descent proceeds, each node it passes through splits its
children into those wholly below the boundary, those wholly above, and the one
the path continues into. Collecting the "wholly below" children at every level
gives `O(log_B n)` **canonical subtrees** whose union is exactly the prefix
`[0, budget]`, with no overlap and nothing missed; collecting the "wholly above"
children the same way covers the suffix. Then:

1. **Take `min_gap_pos` over the suffix subtrees — a single scalar.** Every
   allocation in the prefix has `A.size <= budget`, and every gap in the suffix
   has `G.width > budget >= A.size`, so *every* prefix-allocation/suffix-gap pair
   is valid without further checking. The only thing that matters about the whole
   suffix, therefore, is its lowest gap; nothing else about it can affect the
   answer.

2. **Sweep the prefix subtrees right to left, seeded with that scalar.** This is
   the node merge verbatim, with the canonical subtrees playing the role of
   children. Seeding `min_gap_pos` with the suffix minimum is what folds case (1)
   in: an allocation in the prefix is offered both the gaps above it *within* the
   prefix (accumulated by the sweep) and the best gap in the entire suffix
   (present from the start). Each subtree's own `best` is folded in too, so pairs
   living inside a single canonical subtree are not lost.

The result is the true best budget-respecting pair, not a heuristic. This is a
genuine advantage of keying by size rather than by address, and it is not
available to either of the address-ordered searches currently implemented.

(In `sweep-bptree` this needs a custom descent — the crate exposes
`descend_visit` for that purpose — and I have not verified that its visitor
interface can express a two-sided collection of canonical subtrees.)

---

## 2. Rewarding large moves

### Why the simple cost model is not the real one

"Cost = bytes copied" is a good model of the *copying*, and a poor model of the
*step*. A step also costs a proposal, a commit, and — once a store is attached —
an I/O boundary and a journal record. Real cost looks more like

```
cost = c₀ + A.size
```

with a fixed overhead `c₀`. Under that model, ten 100-byte moves are strictly
worse than one 1000-byte move of equal total distance, and the compactor should
prefer to move fewer, larger things.

### Why the natural objective breaks the structure

Substituting the realistic cost gives

```
gain / cost = A.size · (A.pos − G.pos) / (c₀ + A.size)
            = w(A.size) · (A.pos − G.pos)          where w(s) = s / (c₀ + s)
```

`w` is increasing in `s`, which is the desired behaviour. But expand it:

```
f(A, G) = w(A.size) · A.pos  −  w(A.size) · G.pos
```

The coefficient on `G.pos` now **depends on which allocation is chosen**. The
merge's `O(1)` cross-case relied on the two sides being independent: the low-size
side could hand up its single best allocation without knowing anything about the
gap it would be paired with. That is no longer true — a large-`w` allocation is
preferable when `G.pos` is small, a small-`w` one when `G.pos` is large.

Recovering exactness would mean each node maintaining the upper envelope of a set
of linear functions in `G.pos` — a Li Chao tree or dynamic convex hull inside
every node — under continuous insertion and deletion. That is `O(log² n)` or
worse per update, with a large constant and a lot of machinery. Not worth it.

### Additive decoupling

Any objective of the form

```
f(A, G) = U(A) − V(G)
```

keeps the `O(1)` merge, because the two sides can still be maximized
independently. So express the preference for large moves as an additive reward
rather than a multiplicative weight:

```
f(A, G) = (A.pos − G.pos) + λ · reward(A.size)
```

where `reward` is any monotonically increasing function of size and `λ` sets the
exchange rate between *distance travelled* and *size moved*. The score becomes

```
score(A) = A.pos + λ · reward(A.size)
```

and nothing else changes: `min_gap_pos` is untouched, the merge is still
`c.max_alloc_score − min_gap_pos`, the constraint and the key order are
unaffected, and both `A.pos` and `A.size` remain in the key so the score is
computable from keys alone.

### The bound that keeps the sign test exact

The reward can be positive while the distance is negative, so `f > 0` no longer
implies a downward move — and an allocation with a large reward whose only
size-valid gaps lie *above* it could win the global maximum outright, proposing a
move that increases `Φ`.

The fix follows from the two distance bounds established in stage 1. An upward
pair has `A.pos − G.pos <= −A.size`, so requiring

```
λ · reward(s)  <=  s        for every live size s
```

makes every upward pair score `<= 0` and every downward pair score
`>= s + λ·reward(s) > 0`. The sign test is exact again.

This is adopted. Two consequences worth stating plainly:

- **The bound binds at the smallest live allocation, not a typical one.** With
  1-byte allocations in play it degenerates to almost no reward at all. It is
  tolerable here on the expectation that small-vector inlining will keep tiny
  allocations out of the heap; if that expectation fails, the reward becomes
  inert and one of the alternatives below is needed instead.
- **It caps the reward at the allocation's own size**, so with `reward(s) = s`
  it means `λ <= 1`, and `f` lies between `d` and `2d`. On a heap where distances
  run to `10^5` and sizes to `10^2`, the reward can only ever reorder near-ties.

Two alternatives that avoid the bound entirely are recorded under [Alternatives
considered](#alternatives-considered).

### What it costs conceptually

`f` is no longer `ΔΦ / cost` for any cost model — it is a stated objective in its
own right, of which the potential's per-byte term is one component. The greedy
policy is therefore no longer greedy *in the potential*, and any argument that
rested on that (for instance, that a step's gain is credited immediately and
needs no lookahead) has to be restated in terms of `f`.

This is less of a loss than it sounds. Measurements on this project have already
established that greed in `Φ` is not aligned with the quantity actually read off
the heap — file size, and free bytes below `end` — so `Φ`-exactness is not a
property worth protecting for its own sake. But it does mean `λ` and `reward` are
tuning parameters whose effect on fragmentation has to be *measured*, not
assumed.

---

## 3. Pricing the gap count at the source

### What vacating an allocation does

Removing an allocation does one of three things to the number of gaps, decided
entirely by its immediate neighbours:

| free neighbours `nc` | effect | `r_src = nc − 1` |
|---|---|---|
| both | the two gaps and the vacated span merge into one | `+1` |
| one | the adjacent gap simply extends | `0` |
| neither | a brand-new gap is minted | `−1` |

The top of the heap counts as free: vacating the topmost allocation lets `end`
retreat rather than leaving a trailing gap.

This is a property of `A` alone, so it decouples, and it can go straight into the
score:

```
score(A) = A.pos + λ · reward(A.size) + α · (nc − 1)
```

with `α` the weight on gap count. The merge, the key order and the validity
argument are all unchanged. Note that this is an **absolute** term, not the
per-byte `α·r/s` the current implementation uses; the tuning does not transfer.

**The destination side cannot join it.** Whether a move *closes* a gap depends on
whether `G.width == A.size`, which couples the two sides — exactly what the merge
cannot express. That is stage 4's job, and it is deliberately restricted to
fixed-size allocations.

### The sign bound, extended

The worst case is now `nc = 2`, so the bound of stage 2 becomes

```
λ · reward(s)  +  α  <=  s        for every live size s
```

Under it, an upward pair scores at most `−s + λ·reward(s) + α <= 0` and is
rejected; a downward pair scores at least `s + λ·reward(s) − α >= 0`. The one
soft edge is that a downward pair can now score exactly `0` when `α = s` and
`λ·reward(s) = 0`, and would be rejected — a lost marginal move, never a bad one
taken.

Scores are `u64`, and the `nc = 0` case subtracts `α`, so the score must be
formed with `saturating_sub`. An allocation at an address below `α` cannot travel
far anyway, so the saturation costs nothing real.

### Bucketing fixed-size classes

Within one fixed-size class every member shares `size`, hence `λ·reward(size)`.
If they also share `nc`, they share everything but `A.pos` — so the best member
of such a group is simply the **highest-addressed** one. That makes bucketing a
class by neighbour count both natural and necessary, and it is the moment to
collapse each bucket into a single tree entry.

The main tree then holds:

- **one entry per gap**;
- **one entry per resizable allocation**, carrying its own size and its own `nc`
  in its score (no bucketing — each is its own class of one);
- **one entry per non-empty `(fixed class, nc)` bucket**, carrying that bucket's
  highest-addressed member.

Sized on this project's measured workload — 13 963 allocations, 25% resizable,
five fixed classes, 1 218 gaps — that is roughly `3 490 + 15 + 1 218 ≈ 4 700`
entries instead of `15 200`. Three-fold fewer entries is only about 0.4 of a
level at `B = 32`, so the height saving is negligible; the real gain is that a
fixed-size allocate or free touches the main tree **only when its bucket's
maximum changes**, which placement's preference for low addresses makes rare on
allocation and roughly `1/m` on free.

### What it costs: re-bucketing

An allocation's score now depends on its *neighbours*, so freeing or allocating
next to `A` changes `A`'s key even though `A` itself did not change. Each
mutation therefore re-keys up to two other entries — a delete-and-insert each, or
for a bucketed class, a move between buckets that may change two buckets' maxima.
Budget up to four extra main-tree updates per mutation.

This is the price of `α > 0`, and it is worth measuring against `α = 0` before
committing to it. At `α = 0` the bucketing collapses to one entry per class and
the neighbour tracking disappears entirely.

---

## 4. Pricing the gap count at the destination

### Why this is worth wanting

Free-space accounting on this project shows that most free space is destroyed not
by compaction truncating the top of the file, but by **new allocations landing in
existing gaps** — around three quarters of it, and more under some policies.
Compaction's contribution to file size is therefore mostly indirect: it decides
what *shape* the free space is in when the allocator next needs some.

That reframes what a good destination is. A gap whose width is an exact multiple
of a commonly-minted allocation size can be consumed with no residue. A gap one
byte wider than a size class leaves a sliver that nothing will ever use.

So: prefer evacuating a **fixed-size** allocation of size `s` into a gap of width
`k · s`. For `k = 1` this erases the gap outright. For `k > 1` it leaves
`(k−1)·s`, which is still exactly tileable by the same class — the property is
preserved rather than consumed.

Restricting this to fixed-size allocations is not a simplification, it is the
correct rule, and this project already applies it elsewhere: a *resizable*
allocation parked in a snug gap has to move again the moment it grows, re-opening
the gap and paying for two copies, so rewarding its snug fit would be luring it
into a round trip. **Resizable allocations therefore earn no destination-side
reward at all**, at any `k`.

### Two weights, not one

For `k = 1` the reward is unambiguous: the gap is gone, now. For `k > 1` it is
**speculative** — filling `s` of a `k·s` gap is only valuable if the remaining
`k−1` allocations of that class actually arrive and actually get placed there. If
the workload's mix shifts, the reward was paid for a benefit never realized.

So there are two tuning parameters, not one:

```
μ₁   reward for an exact fit          (k = 1)
μₖ   reward for a proper multiple     (k > 1),   with μₖ < μ₁
```

`μ₁` prices gap *erasure*, a countable event. `μₖ` prices *tileability*, a
fragmentation property, and should be weighted well below it.

### Why it cannot live in the main tree

The merge works because `>=` on sizes is monotone: everything in a high-size
subtree beats everything in a low-size subtree, so a node needs no knowledge of
the individual sizes it contains.

Divisibility has no such structure. Knowing that a gap sits in the high-size child
and an allocation in the low-size child says nothing about whether the width is a
multiple of the size — a 4-byte gap is a multiple of a 2-byte allocation, a
5-byte gap is not. To resolve a crossing pair, a node would have to remember every
distinct size beneath it, blowing its state up from `O(1)` to `O(subtree span)`
and destroying the `O(log n)` update.

The right move is therefore not to bend the main tree but to give divisibility its
own small index, and to combine the two at the top as a third candidate shape
alongside evacuation and slide.

### The index

Let `C` be the set of distinct **fixed sizes currently live**. This is small and
nearly static — the whole point of a fixed-size class is that the allocator mints
many allocations at each of a handful of sizes.

For each `s ∈ C`, two quantities:

- `max_alloc_pos[s]` — the highest-addressed live allocation of size `s`. This
  needs **no structure of its own**: it is the maximum over the main tree's key
  block for size `s`, and under stage 3's bucketing it is already materialised as
  the class's bucket entries.
- `min_multiple_gap_pos[s]` — the lowest-addressed gap whose width is a positive
  integer multiple of `s`. This one does need its own ordered set, for the reason
  below.

Then `best_multiple[s] = max_alloc_pos[s] − min_multiple_gap_pos[s] + μ`, with
`μ = μ₁` if the widths match exactly and `μₖ` otherwise, and a scan over the few
members of `C` gives the best multiple-fit candidate.

**Why an ordered set and not a scalar.** Insertion into a running extremum is
`O(1)` — `min = min(min, pos)` — and a hash map from `s` to a pair of scalars
would serve it perfectly. What a scalar cannot do is *restore* the extremum after
the extremal element is deleted, and gaps are destroyed on every `alloc` and
`free`. So the cost here is `O(log #gaps_in_that_class)` for **deletion**, not
for lookup, and it is the only reason order is needed at all.

Keeping scalars and clearing the entry when the extremum is deleted would be
`O(1)` and safe — this whole mechanism is a bonus term, so a stale class degrades
the preference and never the safety of the move — but the staleness is biased,
not random: the entry would be rebuilt only from newly-created gaps, and would
systematically under-report exactly the low-addressed destinations the term
exists to find. Not worth it.

**The outer map can be anything.** `HashMap<size, ClassGaps>` is the right shape;
with `|C|` around five, a linearly-scanned `Vec<(u32, ClassGaps)>` is likely
faster — no hashing, one cache line.

Updates:

- **A fixed-size allocation appears or disappears.** Nothing to do here; stage
  3's bucketing already tracks the class maximum.
- **A gap of width `G` appears or disappears.** Test each `s ∈ C` for
  `G mod s == 0`, and insert into or delete from those classes' gap sets.
  `O(|C| · log #gaps)`.

The second point is the one place this design departs from the textbook version.
The general "gap of width `G` pairs with any divisor of `G`" problem is normally
solved by enumerating all `O(√G)` divisors of `G`. Here that is unnecessary work:
only the sizes in `C` can ever be the size of a candidate allocation, so testing
the `|C|` fixed sizes for divisibility is both sufficient and cheaper — `|C|` is
a handful, `√G` for a kilobyte gap is 32.

**And none of it is on the critical path.** `O(|C| · log #gaps)` at `|C| = 5` and
~1 200 gaps is roughly 50 comparisons per gap event, against about `B · log_B n`
≈ 32 × 3 ≈ 96 for a *single* insertion into the main tree. The whole multiple-fit
index costs less than one main-tree update; optimising its logarithm away would
be tuning the wrong term.

### When a size class enters or leaves

- **A class enters** (the first allocation of a novel fixed size). Its gap set
  starts **empty and is not backfilled**. Scanning the existing gap index to
  populate it would be `O(#gaps)`, and the payoff is small: because this is a
  bonus term and not a correctness requirement, a new class simply sees only the
  gaps created after it entered, and converges as the heap churns. The main tree
  still proposes a valid, positive-gain evacuation throughout.
- **A class leaves** (its last allocation is freed). Drop the entry, or retain it
  and let it be reused; keeping a few stale classes costs only their divisibility
  tests.

---

## 5. Runs of adjacent allocations — deferred

### Why a run needs stage 2

A **run** is a sequence of allocations that are contiguous in the address space
with no gap between them. Its position is the start of its lowest member and its
size is the span to the end of its highest.

Under stage 1 there is no reason to prefer one. A run's start is at or below its
highest member's, so under pure distance `f(highest member) >= f(run)` always,
and the run additionally needs a *wider* gap. Runs are strictly dominated.

Stage 2 changes that. A run and its **lowest** member sit at the same position,
so they differ only by `λ·(reward(S) − reward(s₁)) > 0`: the run beats its own
lowest member under any increasing reward. The real trade is against a *higher*
member `A_j` at `p_j`:

```
f(A_j) − f(run) = (p_j − p)  −  λ · (reward(S) − reward(s_j))
```

The run wins when the reward's steepness in size outruns the distance the higher
member gains by sitting further up. A **convex** reward such as `s²` is steeper
at large `S` and therefore favours runs *more*, not less.

Which is precisely the tension: `λ·s² <= s` holds only for `s <= 1/λ`, so the
convex rewards that would make runs worthwhile are exactly the ones stage 2's
sign bound forbids. Under the adopted bound you cannot have both. **Full run
support is therefore deferred**, pending either a measured case for one of the
alternatives below, or evidence that a linear reward already buys enough.

### What full support would cost

For the record, since it is the reason for deferring. The tree itself would need
no change at all — it stores `(size, is_gap, score, address)` and neither knows
nor cares that an entry denotes several allocations. The expense is entirely on
the layout side: capping run length at `k_max` allocations, an allocation
participates in at most `k_max(k_max+1)/2` runs, so inserting or removing **one**
allocation means `O(k_max²)` point updates — ≤ 3 at `k_max = 2`, ≤ 10 at 4, ≤ 36
at 8 — and the tree grows to `O(k_max · n)` entries. All of it charged to the
mutation path, which runs roughly four times as often as the proposal path.

(Capping by *byte* size instead of length is worse: with 1-byte allocations a
byte cap `M` permits runs of `M` members and the bound degrades to `O(M²)`.)

### The minimal version, adopted now

There is a cheap fraction of the benefit that needs no new entries at all:
**after** the best single-allocation move has been chosen, try to extend it into
a run opportunistically, in both directions, as far as the gap width and the
budget allow.

This is safe and weakly profitable in every case, which follows from stage 1's
distance bound. Let the chosen step move `[from, from+len)` into a gap at `to` of
width `w`, and let `d = from − to`. Absorbing the neighbour immediately *above*
the run leaves `from` unchanged and increases `len`, adding `c·d > 0` to the
total potential drop. Absorbing the neighbour immediately *below* lowers `from`
by `b` and adds `b·(d − S)` where `S` is the new total size — non-negative,
because the run must still fit the gap (`S <= w`) and must still sit above it
(`d >= w`), so `d >= S` throughout.

So the rule is simply: extend while the run still fits in `min(w, budget)`. Note
that the downward extension degenerates gracefully — if it reaches the allocation
immediately above the gap, the step has become a slide.

### The slide is still separate

This project's other candidate shape is the **slide**: the maximal run above the
largest gap, shifted down into it. A slide is *not* an evacuation and cannot be
represented in this tree, because it does not require the run to fit —
`run.size > G.width` is the normal case, and the move is a partial overlapping
shift by `G.width`.

That matters beyond taxonomy. The slide is what guarantees a positive-gain move
exists whenever any gap does; the tree can legitimately report `best = 0` on a
heap full of gaps too narrow for anything (this project's `slivers` shape). **The
tree replaces the evacuation search, not the slide.** Both candidates are offered
and the better one wins, exactly as now.

---

## Alternatives considered

Both of these avoid stage 2's sign bound entirely. Neither is adopted, but the
bound is restrictive enough that they should be reconsidered if it proves to bite.

### Dual aggregates with fallback

Carry **both** scores in the same augmentation: `max_alloc_pos` (plain address,
no reward, no `α`) alongside `max_alloc_score`, and correspondingly both
`best_plain` and `best_scored`. `best_plain` is sign-safe by stage 1's argument
alone, with no constraint on anything.

Read the root. If `best_scored`'s witness is a downward pair, use it. Otherwise
fall back to `best_plain`, which is guaranteed downward whenever any beneficial
move exists.

Two extra scalars in the `Argument`, both still `O(1)` at the root, and **no
bound on `λ`, `reward` or `α` whatsoever** — a convex reward becomes expressible.
The only loss is that when the scored maximum happens to be an upward pair you
get the plain best rather than the best *downward scored* pair, which is an
approximation rather than an unsafe move. Cheap, and the natural fallback if the
bound turns out to make the reward inert.

### Suffix queries by size threshold

This one removes the reward term from the objective altogether and moves the
size preference into the *query*.

**The query.** "The best evacuation among allocations of size at least `T`",
under the plain stage-1 distance objective. This is a **pure suffix** query, and
simpler than the budget query, because both endpoints are confined to the suffix:
if `A.size >= T` then any valid gap has `G.width >= A.size >= T`, so nothing below
`T` in the key order can participate in a valid pair at all. (Contrast the budget
query, where the allocations are in the prefix but their gaps may be anywhere
above it — which is why that one needs the extra suffix scalar.)

**The mechanics.** Descend from the root toward the key `(T << 1) | 0`, the first
allocation entry of size `T`. At each level, the children strictly to the right
of the path lie wholly inside the suffix; collect them. That yields `O(log_B n)`
canonical subtrees, `O(B)` per level to gather, whose union is exactly the suffix
`[T, ∞)`. Run the ordinary right-to-left merge sweep over that list — identical
to the node merge, with canonical subtrees as children — and the result is the
exact best pair among allocations of size `>= T`. `O(B log_B n)`.

**Using it.** Evaluate a short ladder of thresholds, say `T ∈ {0, 64, 256, 1024}`,
and let an outer rule choose: *take the largest `T` whose best distance is still
at least `ρ` times the unrestricted (`T = 0`) best*. `ρ ∈ (0, 1]` replaces `λ` as
the exchange rate, and reads directly as "never give up more than a factor `ρ` of
travel distance in order to move something bigger".

**Why it is attractive.** The objective stays plain distance, so the sign test is
exact with no bound on anything; the size preference is explicit and
interpretable rather than folded into a weight; and the shape of the preference
is set by the ladder, so a strongly convex bias is expressible where the bounded
additive reward cannot express one. The cost is one query per rung — four rungs
is `~4 · O(B log_B n)`, still far below a linear walk.

**Why it is not adopted.** It is a ladder, not a continuum, and the outer rule is
a heuristic where the additive score is a single well-defined objective. It also
composes less cleanly with stage 3: `α · (nc − 1)` naturally lives *in* the score,
and moving the size term out of the score while the gap-count term stays in it
splits the policy across two mechanisms.

---

## The algorithms

Written against abstract queries; the section after this one says what implements
each of them.

### The query vocabulary

```
# layout
allocation_at(addr)              -> Option<Alloc>     # covering addr
allocation_starting_at(addr)     -> Option<Alloc>
allocation_ending_at(addr)       -> Option<Alloc>     # ends exactly at addr
gap_starting_at(addr)            -> Option<Gap>
free_neighbours(addr, size)      -> 0 | 1 | 2
run_len_from(addr, cap)          -> (len, truncated)  # contiguous, <= cap

# free space
lowest_gap_fitting(width)        -> Option<Gap>       # lowest with G.width >= width
widest_gap()                     -> Option<Gap>
lowest_exact_gap(width)          -> Option<Gap>       # G.width == width exactly
lowest_tileable_gap(s)           -> Option<Gap>       # G.width % s == 0, fixed classes only

# the evacuation index
best_evacuation()                -> Option<Candidate>
best_evacuation_within(budget)   -> Option<Candidate> # A.size <= budget

# the tileable-gap index
best_tiling_evacuation(budget)   -> Option<Candidate>
```

### Placement

Used by `alloc`, and by the relocating branch of `resize`. Placement is scored
against the same potential compaction is, but **without a cost term**: the bytes
are written wherever they go, so a lower address here is free where compaction
would pay a full copy for it.

```
fn place(size, is_fixed) -> Address:
    # An allocation of `size` bytes at `addr` adds size·(addr + (size−1)/2) to Φ;
    # the constant drops out of a comparison. Consuming a gap cleanly is worth
    # the same μ weights the compactor uses, so placement and compaction agree
    # about what a good destination is.
    cost(g) = size · g.pos
              − μ₁ if g.width == size
              − μₖ if is_fixed and g.width % size == 0 and g.width > size

    candidates = [ lowest_gap_fitting(size) ]
    if is_fixed:
        candidates += [ lowest_exact_gap(size),        # μ₁ candidate
                        lowest_tileable_gap(size) ]    # μₖ candidate

    match candidates.filter(Some).min_by_key(cost):
        Some(g) -> g.pos
        None    -> end                                 # nothing fits: extend
```

Only a fixed-size allocation may claim the fit bonuses, for the reason given in
stage 4. Three candidates suffice: the exact fit and the tileable fit are the
only gaps that can beat the lowest fitting one, since `cost` is otherwise
monotone in `g.pos`.

```
fn alloc(id, size) -> Address:
    addr = place(size, id.is_fixed_size())
    insert into the layout, splitting the gap it landed in
    return addr

fn resize(id, new_size) -> Relocation:
    addr = address_of(id); old = size_of(id)
    if new_size <= old:
        shrink in place; the freed tail becomes (or extends) a gap
        return None
    if gap_starting_at(addr + old) has width >= new_size − old:
        grow in place, consuming that much of the gap
        return None
    # evicting resize: relocate
    new_addr = place(new_size, is_fixed = false)   # resizable: no fit bonuses
    move the bytes; free the old extent, merging neighbouring gaps
    return Some((addr, new_addr))
```

### Proposing a step

```
fn propose_step(budget) -> Option<Step>:
    if no gaps exist:
        return None                       # already compact

    best = None

    # 1. The slide. Offered unconditionally: it is the only candidate that does
    #    not require the moved bytes to fit in the gap, and so the only one that
    #    guarantees progress while any gap exists.
    best = offer(best, slide_candidate(budget))

    # 2. The exact evacuation, from the augmented tree. Two reads: the best that
    #    fits the budget, and the best overall in case nothing does.
    best = offer(best, best_evacuation_within(budget))
    best = offer(best, best_evacuation())

    # 3. The tileable evacuation (fixed-size allocations only).
    best = offer(best, best_tiling_evacuation(budget))

    # 4. Opportunistic run extension -- stage 5's minimal version.
    if best is an evacuation:
        best = extend_into_run(best, budget)

    return best

fn slide_candidate(budget) -> Option<Candidate>:
    g = widest_gap()?
    from = g.pos + g.width
    (len, truncated) = run_len_from(from, budget)
    if len == 0: return None
    # A maximal run is flanked by free space above, so sliding it merges that
    # with the range it vacates, or lets `end` retreat: r = +1 either way.
    # A budget-truncated prefix merely relocates the gap: r = 0.
    return Candidate { from, to: g.pos, len, r: if truncated { 0 } else { 1 } }

fn extend_into_run(step, budget) -> Step:
    w   = gap_starting_at(step.to).width
    cap = min(w, budget)

    # Upward: absorb the allocation starting exactly where the run ends.
    # `from` is unchanged, so every added byte travels the same distance.
    while let Some(next) = allocation_starting_at(step.from + step.len),
          step.len + next.size <= cap:
        step.len += next.size

    # Downward: absorb the allocation ending exactly where the run starts.
    # `from` drops, so every byte travels less -- but the run must still fit the
    # gap and still sit above it, which forces distance >= size, so the trade is
    # never a loss.
    while let Some(prev) = allocation_ending_at(step.from),
          step.len + prev.size <= cap,
          prev.pos > step.to:
        step.len += prev.size
        step.from = prev.pos

    return step
```

`commit_compaction_step(step)` is unchanged from what this project already has:
`Step { from, to, len }` carries no id and re-keys every allocation in the moved
range, which is exactly what a run move needs.

---

## The data structures

Grouped by **what they afford**, not by what they are. Several are composites of
more than one generic container, held together because they answer one
semantically connected family of questions and must be maintained as a unit.

### A. The layout — ground truth

**Affords:** what occupies an address, what is adjacent to it, and how long a
contiguous run extends. Everything else is derived from this and must agree with
it.

**Composed of:**
- `BTreeMap<u64, Entry { len: u32, id: Id }>`, keyed by start address. This is
  the single source of truth.
- `HashMap<Id, u64>` — the id table's address column.
- `end: u64`, one past the highest live byte.

**Queries:**
- `allocation_at`, `allocation_starting_at` — a point or `range(..=addr)` lookup.
- `allocation_ending_at(addr)` — `range(..addr).next_back()`, then check whether
  its end equals `addr`.
- `gap_starting_at(addr)` — gaps are *not stored here*; the gap after the entry
  at `a` runs from that entry's end to the next entry's start, so this is one
  `range(addr..).next()`.
- `free_neighbours(addr, size)` — two range probes, one on each side, with the
  top of the heap counting as free.
- `run_len_from(addr, cap)` — a forward walk from `addr`, stopping at the first
  discontinuity or when `cap` is exceeded. Bounded by the budget, not by heap
  size.

**Maintained:** on every `alloc`, `free`, `resize` and `commit`. Every other
structure below is updated from the deltas this one produces, so the update
protocol is "mutate the layout, then push the resulting entry insertions and
removals into the derived indexes".

### B. The free-space directory

**Affords:** where the free space is, how wide, and which piece is the widest.

**Composed of:**
- `BTreeMap<u64 /*width*/, BTreeSet<u64 /*pos*/>>` — gaps grouped by width.

**Queries:**
- `widest_gap()` — `last_key_value()`, then `first()` of its set. Feeds the slide.
- `lowest_exact_gap(w)` — `get(&w)?.first()`. Feeds `μ₁`.
- `lowest_gap_fitting(w)` — **this one is subsumed by the evacuation index**: it
  is `min_gap_pos` aggregated over the key suffix from `(w << 1) | 1`, one
  descent. Keeping today's separate address-keyed `GapTree` with a max-width
  augmentation is the alternative if a suffix-aggregate descent proves awkward to
  express; the two are redundant with each other and only one is needed.

**Maintained:** on every gap creation, destruction, split and merge.

**Note:** today's `MoverTree` disappears entirely — its query ("the
highest-addressed allocation that fits in `w` bytes") is what the augmented
index now answers globally and in `O(1)`, rather than once per gap.

### C. The evacuation index — the best move, at the root

**Affords:** the single best evacuation in the whole heap, with and without a
budget constraint, in `O(1)` and `O(B log_B n)` respectively.

**Composed of:**
- An augmented B+ tree over the key `((size << 1) | is_gap, score, address)`,
  carrying `{ min_gap_pos, max_alloc_score, best, best_pair }` per subtree, merged
  by the right-to-left sweep of stage 1.
- Its entries: one per gap, one per resizable allocation, and one per non-empty
  `(fixed class, nc)` bucket (see D).

**Queries:**
- `best_evacuation()` — read `root_argument()`. `O(1)`.
- `best_evacuation_within(budget)` — the prefix descent of stage 1's "the budget,
  for free": collect canonical prefix and suffix subtrees, take the suffix's
  `min_gap_pos` as a seed, sweep the prefix. `O(B log_B n)`.
- `lowest_gap_fitting(w)` — as noted in B, a suffix aggregate.

**Maintained:** on every layout delta, and additionally whenever a neighbour
count changes, which re-keys the affected allocation (stage 3). Budget three to
four entry insertions or removals per mutation, plus up to four more for
re-bucketing when `α > 0`.

### D. The fixed-size class registry

**Affords:** for each live fixed size and each neighbour count, the best-scoring
member — which is what the class's tree entries carry.

**Composed of:**
- `HashMap<u32 /*size*/, [BTreeSet<u64 /*address*/>; 3]>`, indexed by
  `nc ∈ {0, 1, 2}`. With `|C|` around five, a linearly-scanned `Vec` is likely
  faster than hashing.

**Queries:**
- The maximum of bucket `(s, nc)` — `last()`. Since every member of a bucket
  shares `size` and `nc`, they share the whole score but for the address, so the
  highest-addressed member is the best-scoring one.
- `max_alloc_pos[s]` for stage 4 — the maximum across that class's three buckets.

**Maintained:** an allocation joins a bucket when created and leaves when freed;
it *moves between buckets* whenever a neighbour is allocated or freed. Whenever a
bucket's maximum changes, the corresponding entry in C is re-keyed.

**Replaces:** today's `live_by_size` (which is already split three ways by
`FreeNeighbours` for exactly this reason) and the mover-first branch's `tops`.

### E. The tileable-gap index

**Affords:** for each live fixed size `s`, the lowest-addressed gap whose width is
a positive integer multiple of `s` — the destination-side bonus of stage 4.

**Composed of:**
- `HashMap<u32 /*size*/, BTreeSet<u64 /*gap pos*/>>`, one set per live class.
  Again, a small `Vec` is likely better at `|C| ≈ 5`.

**Queries:**
- `lowest_tileable_gap(s)` — `first()` of that class's set. Used by both `place`
  and `propose_step`.
- `best_tiling_evacuation(budget)` — for each `s ∈ C`, pair D's class maximum with
  this set's minimum, add `μ₁` or `μₖ` according to whether the widths match
  exactly, and take the best. `O(|C|)`.

**Maintained:** when a gap of width `G` is created or destroyed, test each `s ∈ C`
for `G mod s == 0` and update those sets — `O(|C| · log #gaps)`, which is less
than one insertion into C. When a class enters, its set starts empty and is **not
backfilled**.

### Summary of the change against what exists today

| today | becomes |
|---|---|
| `allocations` + `by_id` + `end` | A, unchanged |
| `free_by_size` | B, unchanged |
| `GapTree` (address-keyed, max-width) | subsumed by C, or retained as B's fitting query |
| `MoverTree` (address-keyed, min-size) | **gone** — C answers its question globally |
| `live_by_size` (3-way by neighbours) | D, unchanged in shape |
| — | C, the new augmented index |
| — | E, the new tileable-gap index |

The net is one structure removed, one repurposed, and two added — and the
candidate search stops being a walk.

---

## Assessment

### Against what is implemented

Two search designs are implemented and measured on this project, both
address-ordered with branch-and-bound pruning. At 13 963 live allocations, over a
churny alloc/free/resize workload with compaction in bursts:

| | candidates examined per call | fraction of the space | µs per burst |
|---|---|---|---|
| mover-first, exact | 1 088 | 63.0% | 3 768 |
| destination-first, exact | 552 | 92.2% | 1 808 |
| destination-first, bounded to 16 | 15.6 | 5.4% | 133 |

The first two grow linearly in heap size — 8.0× and 31.9× across a 10× growth in
allocations. The third is flat, but it is an **approximation**: it examines a
fixed-size prefix of the candidate order and is blind to everything past it.

The augmented tree is a different point in that space: **exact, and `O(1)` to
query**. Its cost moves from the query to the update.

### Where the cost goes

The obvious objection is that this taxes the hot path to subsidize the cold one.
A burst is 25 workload mutations and 2–7 compaction steps, so anything charged
per mutation is charged roughly four times as often as anything charged per
proposal.

That objection is weaker than it looks, because **the mutation path already pays
this shape of cost**. `GapTree` and `MoverTree` are already augmented B+ trees,
and every insert and removal already recomputes an `O(B)` aggregate at every node
along the path. The marginal cost is not a new `B` factor; it is one additional
augmented tree whose merge maintains three running values instead of one — call
it 2–3× the per-node work of an existing augmentation — set against the removal
of `MoverTree` entirely.

Against replacing a 552-candidate walk with a root read, that looks like a clear
win at the measured scale, and the margin widens with the heap because one side
is logarithmic and the other linear.

Two further advantages are worth weighing:

- **The budget query is exact**, in `O(B log_B n)`, replacing a two-track
  heuristic.
- **Resizable allocations need no special handling** in the index. The current
  design excludes them from the size-class structure because they would scatter
  one per class; here they are ordinary entries. The fixed/resizable distinction
  survives only where it belongs: the destination-side bonus of stage 4.

### What I would not claim yet

- **The estimate above is an estimate.** No implementation has been benchmarked.
  It rests on the merge being 2–3× an existing augmentation's, which is a reading
  of the code, not a measurement.
- **Stage 3 has a cost stage 2 does not.** Making the score depend on neighbours
  means each mutation re-keys up to four other entries. `α = 0` avoids this
  entirely and collapses the bucketing, so the two should be measured against
  each other rather than adopted together on principle.
- **Stage 5 is deferred for a reason that may not survive tuning.** Runs need a
  steep reward, and the adopted sign bound forbids one. If `λ · reward(s) <= s`
  proves too tight in practice, the dual-aggregate alternative reopens both
  questions at once.
- **Stages 2 and 3 change the policy, not just the implementation.** The
  objective is no longer `ΔΦ / cost`, so the fragmentation behaviour would have to
  be re-measured from scratch. Every fragmentation number this project currently
  has was produced under distance-greed.
- **Nothing here models the top of the heap.** The objective is potential-based,
  and the potential is indifferent to whether free space sits below `end` or
  vanishes above it. Since file size is what is actually read off the heap, and
  since truncation is the only way compaction reduces it, a structure that
  optimizes evacuations exactly may still be optimizing the wrong thing. That is
  an open question about the objective, not about this data structure — but it
  bounds how much a better search can be expected to buy.

### Recommendation

Stages 1 and 2 are a contained, well-understood change: one additional augmented
B+ tree, one merge rule used at every level, `MoverTree` deleted, and the existing
`sweep-bptree` dependency already providing the trait. They replace a linear
search with a constant-time read and make the budget constraint exact rather than
heuristic. That is the piece worth building and measuring first, at `α = 0`.

Stage 4 is a small, self-contained addition whose motivation — keeping free space
in a shape the allocator can consume without residue — is the one best supported
by the measurements this project already has, and its index is shared between
placement and compaction rather than serving only the latter.

Stage 3 should follow only once stages 1, 2 and 4 have been measured, because it
is the one that makes an allocation's key depend on its neighbours.

Stage 5's opportunistic extension costs nothing and can ship with stage 1; its
full form should wait.
