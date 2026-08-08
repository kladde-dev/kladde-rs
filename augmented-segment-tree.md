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

The development is in four stages, each one adding something the previous stage
could not express:

1. The simplest objective — potential, and cost measured in bytes moved.
2. An objective that prefers moving *large* allocations, for cost models with a
   fixed per-step overhead.
3. Moving **runs** of adjacent allocations together, which only becomes worth
   doing once stage 2 is in place.
4. Preferring gaps whose width is an integer multiple of a fixed-size
   allocation's size.

Stage 5 is an assessment against what is currently implemented.

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
detectable from the sign of the answer.

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

The key is a triple:

```
(size, kind, address)          kind: Allocation = 0, Gap = 1
```

ordered lexicographically, where `size` means `A.size` for an allocation and
`G.width` for a gap.

The `kind` field is not decoration. Consider ordering by `(size, address)` alone,
and an allocation `A` and a gap `G` of the *same* size with `G.pos < A.pos`. That
pair is valid — `G.width = A.size` satisfies `>=` — and it may well be the best
one available. But `G` sorts *before* `A`, so a merge rule that pairs
allocations with gaps to their right would never see it. Putting `kind` between
`size` and `address`, with allocations first, fixes this exactly:

> `key(G) > key(A)` **if and only if** `G.width >= A.size`.

Proof in both directions: if `G.width > A.size` the first component decides; if
`G.width = A.size` the `kind` component decides, and the pair is valid; if
`G.width < A.size` then `key(G) < key(A)` and the pair is invalid. So the key
order *is* the validity relation, and the address tiebreak — which now only
orders within a `(size, kind)` block — gives each size class's extremes for free.

This is why no auxiliary per-size heaps are needed. A "min-heap of gap start
addresses for all gaps of size `L`" is precisely the address-ordered run of
entries with key prefix `(L, Gap)`, and its minimum is the aggregate of the
subtrees covering that run.

### The augmentation

Every subtree — every B+ tree node, and every leaf — carries:

| field | meaning | identity |
|---|---|---|
| `min_gap_pos` | lowest `G.pos` over gaps in the subtree | `+∞` |
| `max_alloc_pos` | highest `A.pos` over allocations in the subtree | `−∞` |
| `best` | max of `A.pos − G.pos` over valid pairs *entirely inside* the subtree | `−∞` |
| `best_pair` | the `(A, G)` achieving `best` | — |

The first two are plain minima and maxima. `best` is what makes the root
answer the query in `O(1)`.

### The merge

A node's state is computed from its children's, which are in ascending key order
`c[0] .. c[k−1]`. A pair inside this node either lives entirely inside one child,
or crosses — allocation in `c[i]`, gap in `c[j]`, `i < j` — and every crossing
pair is valid by the key-order property above.

A single right-to-left sweep finds the best crossing pair in `O(k)`:

```
best          = max over children of child.best
min_gap_pos   = +∞
max_alloc_pos = −∞

for c in children, from the largest sizes down to the smallest:
    # every gap seen so far is at a size >= anything in c
    best          = max(best, c.max_alloc_pos − min_gap_pos)
    min_gap_pos   = min(min_gap_pos, c.min_gap_pos)
    max_alloc_pos = max(max_alloc_pos, c.max_alloc_pos)
```

The same sweep computes a **leaf's** state, treating each stored entry as a
degenerate child: an allocation contributes `max_alloc_pos = A.pos` and
`min_gap_pos = +∞`, a gap the reverse, and both contribute `best = −∞`. So
there is one merge rule, used at every level.

Tracking `best_pair` alongside `best` is bookkeeping: record which of the three
sources — a child's own `best`, or the crossing pair `(c.max_alloc, running min
gap)` — supplied the winning value, and carry the witnessing keys up with it.
Because both the size and the address of every entry live in the key, the
witnesses are recoverable from keys alone, with no side table.

### Operations

**Query.** Read the root's `best` and `best_pair`. `O(1)`. A non-positive `best`
means no evacuation improves the potential.

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

It is the wrong shape here. Sizes are `u32`, so a flat array over the domain is
out of the question, and a dynamic pointer-based binary tree over a sparse domain
means ~32 dependent pointer dereferences per update, each a likely cache miss.
A B+ tree with `B` around 16–32 compresses that to 3–5 levels, and the `O(B)`
sweep inside a node runs over a contiguous array that arrives in one or two cache
lines. The theoretical cost goes from `O(log₂ n)` to `O(B log_B n)`, which is
larger on paper and considerably smaller in practice.

There is also a practical reason specific to this project: it already depends on
`sweep-bptree`, and the augmentation above is exactly its `Argument` trait —
`from_leaf(keys)` and `from_inner(keys, arguments)` correspond one-for-one to the
two uses of the sweep, `root_argument()` is the `O(1)` query, and both existing
indexes (`GapTree`, `MoverTree`) are already built this way. Critically,
`from_leaf` is handed only the **keys**, never the values, which is precisely why
size, kind and address all have to live in the key — a constraint this design
satisfies by construction rather than by accident.

### The budget, for free

`propose_compaction_step` takes a budget and prefers steps that fit inside it.
The current implementation handles this with a two-track selection: keep the best
candidate that fits, and separately the best overall, and prefer the former.

Because the tree is *keyed by size*, the budget is a prefix of the key order, and
the constrained query

> best pair with `A.size <= budget`

is answerable exactly, in `O(B log_B n)`. Descend once along the boundary
`budget`. This yields `O(log_B n)` canonical subtrees covering the prefix
`[0, budget]`, and `O(log_B n)` covering the suffix above it. Every allocation in
the prefix may pair with every gap in the suffix, so:

1. Take `min_gap_pos` over the suffix subtrees — one scalar.
2. Sweep the prefix subtrees right to left with that scalar as the initial
   running minimum, exactly as the node merge does.

The result is the true best budget-respecting pair, not a heuristic. This is a
genuine advantage of keying by size rather than by address, and it is not
available to either of the address-ordered searches currently implemented.

(In `sweep-bptree` this would need a custom descent — the crate exposes
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

where `reward` is any monotonically increasing function of size — `s`, `log s`,
`√s`, a step function — and `λ` sets the exchange rate between *distance
travelled* and *size moved*. With `reward(s) = s` and `λ = 2`, the compactor will
give up 20 bytes of travel distance in exchange for moving an allocation 10 bytes
larger.

### What changes

One field:

```
max_alloc_pos   →   max_alloc_score = max over allocations of
                                      (A.pos + λ · reward(A.size))
```

`min_gap_pos` is untouched. The merge is unchanged — still
`c.max_alloc_score − min_gap_pos`. The constraint `G.width >= A.size` is
unchanged, so the key order and the validity argument are unchanged. Both `A.pos`
and `A.size` are in the key, so the score is still computable from keys alone.

That is the whole modification.

### What it costs conceptually

`f` is no longer `ΔΦ / cost` for any cost model — it is a stated objective in its
own right, of which the potential's per-byte term is one component. The greedy
policy is therefore no longer greedy *in the potential*, and any argument that
rested on that (for instance, that a step's gain is credited immediately and
needs no lookahead) has to be restated in terms of `f`.

This is less of a loss than it sounds. Measurements on this project have already
established that greed in `Φ` is not aligned with the quantity actually read off
the heap — file size, and free bytes below `end` — so `Φ`-exactness is not a
property worth protecting for its own right. But it does mean `λ` and `reward`
are tuning parameters whose effect on fragmentation has to be *measured*, not
assumed.

---

## 3. Moving runs of adjacent allocations

### Why only now

A **run** is a maximal-or-shorter sequence of allocations that are contiguous in
the address space with no gap between them. Its position is the start of its
lowest member and its size is the span from there to the end of its highest.

Under the stage-1 objective there is no reason to prefer a run. Per-byte gain is
travel distance, and a run travels no further than its best member would
individually — so moving the members one at a time is at least as good and more
flexible. It is only once the objective rewards size (stage 2) that moving `k`
adjacent allocations as one unit becomes attractive: the run earns
`λ · reward(total size)` where its members would each earn
`λ · reward(their own size)`, and for any concave-or-linear `reward` the single
large move can win.

### The tree does not change

This is the appealing part. The tree never knew what an entry *was* — it stores
`(size, kind, address)` and aggregates positions. A run is just an entry with a
larger size and the position of its first member. Insert runs alongside single
allocations and everything above applies verbatim, including the reward term and
the budget query. A run of length 1 is a single allocation, so runs subsume the
stage-2 entries rather than sitting beside them.

The commit is also already expressible: this project's `Step { from, to, len }`
carries **no id** and re-keys every allocation in the moved range, which is
exactly a run move.

### The cost: the layout side, not the tree side

Runs have to be enumerated and maintained, and this is where the expense lives.
Cap the run length at `k_max` allocations. Then an allocation at index `i`
participates in the runs `[a, b]` with `a <= i <= b` and `b − a + 1 <= k_max`, of
which there are at most `k_max(k_max + 1)/2`.

So inserting or removing **one** allocation requires `O(k_max²)` point updates to
the tree, each `O(B log_B n)`:

| `k_max` | tree updates per allocation inserted or removed |
|---|---|
| 2 | ≤ 3 |
| 4 | ≤ 10 |
| 8 | ≤ 36 |

Memory grows by the same factor in the other direction: each allocation is the
start of at most `k_max` runs, so the tree holds `O(k_max · n)` entries.

Two things make this heavier than it first appears:

- **The cost is paid on workload mutations, not on compaction steps.** Every
  `alloc` joins runs and every `free` splits them. Under this project's measured
  schedule there are 25 mutations per compaction burst of 2–7 steps, so the
  `k_max²` factor lands overwhelmingly on the hot path.
- Capping by *byte size* instead of length is worse, not better: with a minimum
  allocation size of 1 byte, a byte cap `M` permits runs of `M` members and the
  bound degrades to `O(M²)` with `M` in the hundreds. Cap by length.

`k_max = 2` or `4` is the defensible range. Beyond that this stops being a cheap
extension.

### The slide is still separate

This project's other candidate shape is the **slide**: the maximal run above the
largest gap, shifted down into it. A slide is *not* an evacuation and cannot be
represented in this tree, because it does not require the run to fit —
`run.size > G.width` is the normal case, and the move is a partial overlapping
shift by `G.width`.

That matters beyond taxonomy. The slide is what guarantees a positive-gain move
exists whenever any gap does; the tree can legitimately report `best = −∞` on a
heap full of gaps too narrow for anything (this project's `slivers` shape). **The
tree replaces the evacuation search, not the slide.** Both candidates are offered
and the better one wins, exactly as now.

---

## 4. Gaps that are integer multiples of a fixed size

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
into a round trip.

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

For each `s ∈ C` maintain two scalars:

- `max_alloc_pos[s]` — the highest-addressed live allocation of size `s`.
- `min_multiple_gap_pos[s]` — the lowest-addressed gap whose width is a positive
  integer multiple of `s`.

then `best_multiple[s] = max_alloc_pos[s] − min_multiple_gap_pos[s]`, and a single
maximum over `s ∈ C` gives the best multiple-fit candidate.

Updates:

- **A fixed-size allocation of size `s` appears or disappears.** Update
  `max_alloc_pos[s]` and recompute `best_multiple[s]`. `O(log n)`.
- **A gap of width `G` appears or disappears.** Test each `s ∈ C` for
  `G mod s == 0` and update those classes' gap sets. `O(|C| log n)`.

The second point is the one place this design departs from the textbook version.
The general "gap of width `G` pairs with any divisor of `G`" problem is normally
solved by enumerating all `O(√G)` divisors of `G`. Here that is unnecessary work:
only the sizes in `C` can ever be the size of a candidate allocation, so testing
the `|C|` fixed sizes for divisibility is both sufficient and cheaper — `|C|` is
a handful, `√G` for a kilobyte gap is 32.

### When a size class enters or leaves

`|C|` changing is the awkward case, and it is why `min_multiple_gap_pos` has to be
maintained as a set rather than recomputed.

- **A class enters** (the first allocation of a novel fixed size). Its gap set is
  empty and does not reflect the gaps that already exist. Populating it correctly
  is one pass over the gap index, `O(#gaps)`. This is expensive but rare — in a
  workload whose fixed sizes come from a fixed schedule it happens only during
  warm-up.
- **A class leaves** (its last allocation is freed). Drop the entry, or leave it
  and let it be reused; retaining a few stale classes costs only the divisibility
  tests.

There is a cheap alternative to the entry scan: simply *don't* backfill, and let a
new class see only gaps created after it entered. Because this whole mechanism is
a **bonus term** and not a correctness requirement — the main tree still proposes
a valid, positive-gain evacuation — an under-populated index degrades the quality
of the preference and never the safety of the move. That is a reasonable trade if
class churn turns out to be frequent.

### The honest caveat about `k > 1`

For `k = 1` the reward is unambiguous: the gap is gone. For `k > 1` it is
**speculative**. Filling `s` of a `k·s` gap leaves `(k−1)·s`, which is only
valuable if the remaining `k−1` allocations of that class actually arrive and
actually get placed there. If the workload's mix shifts, the reward was paid for
a benefit never realized.

So the `k > 1` term should be weighted below the `k = 1` term — a reward that
decays in `k` — and should be understood as pricing *tileability*, a fragmentation
property, rather than *gap erasure*, a countable one.

---

## 5. Assessment

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
along the path. The marginal cost of this design is not a new `B` factor; it is
one additional augmented tree whose merge maintains three running values instead
of one — call it 2–3× the per-node work of an existing augmentation.

Set against replacing a 552-candidate walk with a root read, that looks like a
clear win at the measured scale, and the margin widens with the heap because one
side is logarithmic and the other linear.

Two further advantages are worth weighing:

- **The budget query is exact**, in `O(B log_B n)`, replacing a two-track
  heuristic.
- **Resizable allocations need no special handling.** The current design excludes
  them from the size-class index because they would scatter one per class; here
  there are no size classes, so they are ordinary entries. The fixed/resizable
  distinction survives only in §4, where it belongs.

### What I would not claim yet

- **The estimate above is an estimate.** No implementation has been benchmarked.
  The comparison rests on the merge being 2–3× an existing augmentation's, which
  is a reading of the code, not a measurement.
- **Stage 3 should be judged separately.** The `O(k_max²)` run maintenance is
  paid per mutation and is the one part of this design with a genuinely
  unfavourable cost profile. Stages 1, 2 and 4 stand without it.
- **Stage 2 changes the policy, not just the implementation.** The additive
  objective is not `ΔΦ / cost`, so the fragmentation behaviour of the compactor
  would have to be re-measured from scratch. Every fragmentation number this
  project currently has was produced under distance-greed.
- **Nothing here models the top of the heap.** The objective is potential-based,
  and the potential is indifferent to whether free space sits below `end` or
  vanishes above it. Since file size is what is actually read off the heap, and
  since truncation is the only way compaction reduces it, a structure that
  optimizes evacuations exactly may still be optimizing the wrong thing. That is
  an open question about the objective, not about this data structure — but it
  bounds how much a better search can be expected to buy.

### Recommendation

Stages 1 and 2 are a contained, well-understood change: one additional augmented
B+ tree over `(size, kind, address)`, one merge rule used at every level, and the
existing `sweep-bptree` dependency already provides the trait. They replace a
linear search with a constant-time read and make the budget constraint exact
rather than heuristic. That is the piece worth building and measuring first.

Stage 4 is a small, self-contained addition whose motivation — keeping free space
in a shape the allocator can consume without residue — is the one best supported
by the measurements this project already has.

Stage 3 is the speculative one and should wait for stage 2's `λ` and `reward` to
have been tuned against measured fragmentation, since without a reward for size
there is no reason to move runs at all.
