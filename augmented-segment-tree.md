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

1. The simplest objective — potential, and cost measured in bytes moved — plus
   the `K · end` term that makes file size part of what is being minimized.
2. An objective that prefers moving *large* allocations, for cost models with a
   fixed per-step overhead.
3. Pricing what a move does to the gap count **at the source**.
4. Pricing what it does **at the destination**, for fixed-size allocations, by
   preferring gaps whose width is an integer multiple of the allocation's size.
5. Moving **runs** of adjacent allocations together — deferred, except for a
   cheap opportunistic version adopted now. This is also where the other step
   shape lives: the **slide**, of which there are two, into the lowest gap and
   into the highest.

Four sections follow: alternatives considered and rejected, the algorithms and
the data structures that implement them, an assessment of what was built, and
extensions the measurements argue for but that are not built.

This document has been revised against a working implementation, so it reports
measurements rather than estimates where it can. Two of its original conclusions
did not survive contact: the slide's destination, which it treated as settled and
which turned out to matter more than anything else here; and the belief that a
potential-only objective was good enough, which stage 1 now corrects.

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

### The potential is not the whole objective: the `end` term

`Φ` is indifferent to whether free space sits below `end` or vanishes above it.
But `end` is the file size, which is what is actually read off the heap, and
truncation is the only way compaction reduces it. A compactor greedy in `Φ`
alone will happily spend a whole heap's worth of copying to rearrange free space
it never retires — and does, measurably. So the objective is

```
Φ' = Φ  +  K · end
```

with `K` in **addresses per byte of file size**. Since every candidate is ranked
per byte copied and both terms share that denominator,

```
score = [ (−ΔΦ) + K·(−Δend) ] / copied  =  −Δ[ Φ + K·end ] / copied
```

so the compactor is exactly **steepest descent on `Φ'` per byte copied**. That
is an identity, not an analogy: the potential half of each candidate's score
really is `−ΔΦ / copied` — the travel distance for an evacuation, the gap width
for a slide — and the file-size half really is `−Δend / copied`, a *rate* of
truncation. `Φ'` still falls monotonically, since every step lowers `Φ` and none
raises `end`, so termination is unaffected.

`K` reads directly: *at `K = 1024`, retiring one byte of `end` is worth moving
one byte 1024 addresses further down*. That it is a plain constant matters more
than it looks. Two earlier forms were tried and both had the coefficient depend
on something:

| form | objective | marginal price of a byte of `end` |
|---|---|---|
| `ν · end · rate` | `Φ + ½ν·end²` | grows with the heap |
| `ν · budget · rate` | `Φ + ν·budget·end` | constant, but varies with the caller's budget |
| `K · rate` | `Φ + K·end` | constant |

The first is the instructive failure. Scaling by `end` looks right — per-byte
gain is bounded by the height of the heap, so `end` puts both terms on one range
— but it makes the *price* of file size grow with the file, and on a large heap
every truncating move then dominates everything else regardless of how little it
retires. That was the objective behaving exactly as written, not a scoring
accident. Stages 2 and 3 below are additive terms in `A` alone and do **not**
have this form; at `λ = α = μ = 0` the identification with `Φ'` is exact, and
those three break it when set.

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
| `max_gap_pos` | highest `G.pos` over gaps in the subtree | `0` |
| `max_alloc_score` | highest `score` over allocations in the subtree | `0` |
| `best` | max of `score(A) − G.pos` over valid pairs *entirely inside* the subtree | `0` |
| `best_pair` | the `(A, G)` achieving `best` | — |

The first three are plain minima and maxima. `best` is what makes the root
answer the query in `O(1)`.

The two gap extremes each name a distinguished gap, and both turn out to be
step destinations in their own right — `min_gap_pos` is the **compaction
frontier** below which the heap is final, and `max_gap_pos` is the only gap
whose closure can retire `end`. Note that `0` is *not* a usable "no gap here"
sentinel for `max_gap_pos`, since a gap at address 0 is perfectly ordinary;
`min_gap_pos != u64::MAX` carries the emptiness predicate for both.

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

- *Allocate* — remove the gap that was landed in, insert the new allocation, and
  reinsert the trimmed remainder of the gap if any (a destination always lands at
  a gap's low end, so a gap is trimmed from the bottom, never split — but both
  `G.pos` and `G.width` are in the key, so the remainder is still a fresh entry).
  Three entries.
- *Free* — remove the allocation, remove up to two adjacent gaps, insert the
  merged gap. Four entries.
- *Commit an evacuation* — remove the allocation and reinsert it at its new
  address (its size is unchanged, so this is a key change, not just a value
  change), plus the trim at the destination and the merge with up to two
  neighbouring gaps at the source. A handful of entries.

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

(In `sweep-bptree` this needs a custom descent, via `descend_visit`. Its visitor
does express a two-sided collection: `visit_inner` receives a node's keys *and*
all `size + 1` child arguments and returns which child to enter, so the prefix
and suffix children can both be folded in on the way past. One detail is easy to
get wrong — the crate sends an exact separator match to the *right* child, so the
path is `keys.partition_point(|k| *k <= boundary)`, not `< boundary`, which would
step left of the match and drop the whole suffix.)

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

This is adopted, and in the initial implementation it is adopted in its simplest
possible form: `reward(s) = s`, with `λ` a **bool** — read as `λ = 1` when set
and `λ = 0` when clear. That sits exactly at the edge of the bound (`λ·s <= s`
holds with equality), so it is the strongest reward the bound permits, and it
needs no multiplication and no tuning sweep:

```
score(A) = A.pos + A.size     if λ            (plus stage 3's α term)
score(A) = A.pos              otherwise
```

The two settings are a **benchmark parameter**: run the compaction benchmarks
across `λ ∈ {true, false}` and compare them on file size and fragmentation.
Stage 2's entire justification is empirical — it trades exactness in `Φ` for a
preference the potential does not express — so shipping it without that
comparison would be adopting a policy change on faith.

Two consequences worth stating plainly:

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

Question: do I understand correctly that there is a separate list of one B-Tree per `(fixed class, nc)` bucket that holds all the entries so that we can maintain the highest address in the main tree (see Section"The data structures", Subsection E)? If so, what is the benefit of having the entries in a separate B-tree over having everything (non-deduped) in one (augmented) B-tree?

> **Answer.** Yes — with one correction to the cross-reference: that is
> subsection **D**, the fixed-size class registry (`HashMap<size, [BTreeSet<address>; 3]>`,
> one ordered set per `(fixed class, nc)` bucket holding *every* member). E is the
> tileable-gap index, which is a different thing entirely. The main tree carries
> only each bucket's maximum.
>
> De-duplicating is *sound* because every member of a bucket shares `size`,
> `is_gap` and every score term but the address, so only the highest-addressed
> member can ever supply the bucket's `max_alloc_score`; the others can never win
> a pair and are pure update traffic.
>
> The benefit is **not** the entry count — the paragraph below already concedes
> that the height saving is negligible. It is that a bucket update is a plain
> `BTreeSet` insert or removal (a binary search per level, one memmove at the
> leaf, no augmentation), whereas a main-tree update recomputes the augmentation
> at every node on the path, folding all `B` children at each — `B · log_B n ≈ 96`
> comparisons against `log₂ n ≈ 12`. Bucketing converts main-tree updates into
> `BTreeSet` updates for every mutation that leaves the bucket maximum alone.
>
> How often that is depends on which mutation, and the accounting is closer than
> it looks — see the next two paragraphs. The short version: it is roughly a wash
> for `alloc`/`free`, and a clear win only for stage 3's re-bucketing traffic.

Sized on this project's measured workload — 13 963 allocations, 25% resizable,
five fixed classes, 1 218 gaps — that is roughly `3 490 + 15 + 1 218 ≈ 4 700`
entries instead of `15 200`. Three-fold fewer entries is only about 0.4 of a
level at `B = 32`, so the height saving is negligible; whatever gain there is has
to come from a fixed-size allocate or free touching the main tree **only when its
bucket's maximum changes**.

On allocation that gain is largely illusory, and for a reason specific to this
project: because compaction runs incrementally and keeps the heap nearly
defragmented, a new allocation very often lands at `end` — which makes it the
highest-addressed member of its class, hence its bucket's new maximum, on
essentially every such allocation. Worse, a maximum *change* costs more than a
plain insert: score and address are both in the key, so it is a delete plus an
insert — two main-tree updates where an undeduplicated tree would have paid one.
On the free side the saving is real: the freed allocation is its bucket's maximum
with probability about `1/m`, giving `2/m` expected main-tree updates against `1`
undeduplicated.

Netted over an alloc/free pair those two effects roughly cancel. What does not
cancel is stage 3's re-bucketing traffic, below: a neighbour-count change is a
move between two `BTreeSet`s and reaches the main tree only when it displaces a
bucket maximum, where an undeduplicated tree would pay a delete-and-insert in the
augmented tree *every time*. **So the bucketing earns its keep only at `α > 0`**,
and the `α = 0` first implementation recommended at the end of this document
should carry one entry per allocation in the main tree and skip D entirely.

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

For each `s ∈ C`, three quantities — of which only one needs a new structure:

- `max_alloc_pos[s]` — the highest-addressed live allocation of size `s`. This
  needs **no structure of its own**: it is the maximum over the main tree's key
  block for size `s`, and under stage 3's bucketing it is already materialised as
  the class's bucket entries.
- `min_multiple_gap_pos[s]` — the lowest-addressed gap whose width is a positive
  integer multiple of `s`. This one does need its own ordered set, for the reason
  below.
- `min_exact_gap_pos[s]` — the lowest-addressed gap of width *exactly* `s`. Like
  the first quantity, this needs **no structure of its own**: gaps of width `s`
  form a contiguous key block in the main tree with prefix `(s << 1) | 1`,
  ordered within it by `score = G.pos`, so this is the first entry of that block
  — the `lowest_exact_gap(s)` query already in the vocabulary.

Then, per class,

```
best_multiple[s] = max( max_alloc_pos[s] − min_exact_gap_pos[s]    + μ₁ ,
                        max_alloc_pos[s] − min_multiple_gap_pos[s] + μₖ )
```

with each term dropped when its gap does not exist, and — importantly — each term
taken only when that gap lies **below** `max_alloc_pos[s]`. That check is exact
rather than conservative: if the class's lowest tracked gap sits above its
highest-addressed allocation, then *every* such gap does, so no downward pair
exists at all. It is what stops `μ` from rescuing an upward pair, exactly as
stage 2's bound stops `λ·reward` from doing so. A scan over the few members of
`C` then gives the best multiple-fit candidate.

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
- **A class leaves** (its last allocation is freed). **Retain the entry**, empty,
  and let it be reused; keeping a few stale classes costs only their divisibility
  tests, and a class that re-enters then finds its gaps already tracked instead
  of starting blind — which is worth more here than the memory, given that
  entering classes are deliberately not backfilled.

  *TODO in the implementation:* retention is only safe while `|C|` stays a
  handful, and nothing stops an application from minting fixed-size allocations
  at hundreds of distinct sizes. Since `|C|` multiplies every gap creation and
  destruction, it has to be **bounded** rather than merely expected to be small,
  so the index should eventually cap the number of distinct size classes it
  tracks. Eviction is benign: the whole mechanism is a bonus term, so an untracked
  class simply earns no destination-side reward — in `place` as well as in
  `propose_step` — and every move it does make stays valid. Leave a comment
  recording this where the map is declared.

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

### The slides are still separate, and there are two of them

A **slide** shifts the run above a gap down into it. It is *not* an evacuation
and cannot be represented in this tree, because it does not require the run to
fit — `run.size > G.width` is the normal case, and the move is a partial
overlapping shift by `G.width`.

That matters beyond taxonomy. A slide is what guarantees a positive-gain move
exists whenever any gap does; the tree can legitimately report `best = 0` on a
heap full of gaps too narrow for anything (this project's `slivers` shape).
**The tree replaces the evacuation search, not the slide.**

#### Which gap to slide into: the lowest, not the widest

The obvious choice is the *widest* gap, since its per-byte gain is the largest.
That is locally optimal and globally disastrous. Both choices close exactly one
gap per untruncated slide, so gap-count progress does not separate them. What
separates them is **order**, and the cost of closing a gap is the live bytes
between it and the next gap still open above it:

- Closing a gap *above* `G` lengthens the run `G` must later move, from `R` to
  `R + R'`. Do that repeatedly and the total is quadratic in the gap count.
- Closing them bottom-up telescopes: `Σ Rᵢ` is exactly the live bytes above the
  lowest gap, each moved **once** — which is also the lower bound, since every
  byte above the lowest gap must move at least once.

Sharper: a slide into any gap that is not the lowest moves bytes that are
*provably going to move again*, because the gapless layout drops everything
above the lowest gap by at least its width. `min_gap_pos` at the root is the
frontier below which the heap is final, and a slide into it is the only step
shape that advances that frontier.

Measured on the quiescing workload at 40 000 churn rounds — 6 980 allocations,
554 721 live bytes, 1 455 gaps, widest 236 bytes:

| | steps | bytes copied | ms |
|---|---|---|---|
| widest-first | 81 280 | 157 432 614 | 7 510 |
| lowest-first | 2 135 | 572 888 | 63 |

573 KB against 555 KB live is the bound above, met to within 3%. The frontier
slide needs no width in the key at all: the gap's width is the distance to the
next allocation, so the destination is a root read and the width one probe of
the layout.

It is not free. A burst costs 4–5× more while a workload is running, because the
frontier sits at the bottom of an already-compacted region, so the run above it
is long and made of small allocations — a step re-keys ~7× more allocations for
the same bytes moved. And `end` is retired later, since a frontier slide only
lets `end` retreat once the frontier reaches the last gap. That second cost is
what the `end` term and the shapes below exist to answer.

#### The end slide

The **end slide** is the mirror: the run above the *highest* gap, which is the
only gap with no gap above it, so the run sitting on it reaches `end`. Sliding
it down by that gap's width `w` retires exactly `w` bytes of file size. Nothing
else has that property unconditionally.

It carries a guard that is not optional: **it is offered only when the whole top
run fits the remaining budget.** A budget-truncated slide retires nothing at all
— it shifts a prefix down and re-forms the gap one budget higher — and repeating
that walks the gap through the entire heap at a cost of one full copy per
budget, which is precisely the 157 MB in the table above. With the guard, an end
slide copies at most one budget and always destroys a gap, so total end-slide
copying is bounded by `gaps · budget`.

The end slide is the weakest of the truncating shapes and remains the open
question in this design. Measured over the quiesce phase it retires **0.07**
bytes of `end` per byte copied, against the end evacuation's **1.00**, and
removing it entirely restores the endgame to the `K = 0` optimum (1.02 bytes
copied per live byte, against 1.16–1.22 with it) at the cost of about 0.1
percentage points of overhead on the growing workload. Both variants are
measured and neither dominates.

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

# free space -- all answered by the evacuation index, which is keyed by size
lowest_gap()                     -> Option<Address>   # the compaction frontier; root read
highest_gap()                    -> Option<Address>   # the only gap that can retire `end`; root read
lowest_gap_fitting(width)        -> Option<Gap>       # lowest with G.width >= width
lowest_exact_gap(width)          -> Option<Gap>       # G.width == width exactly

# the evacuation index
best_evacuation()                -> Option<Candidate>
best_evacuation_within(budget)   -> Option<Candidate> # A.size <= budget

# the tileable-gap index
lowest_tileable_gap(s)           -> Option<Gap>       # G.width % s == 0, fixed classes only
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
              − μ₁ if is_fixed and g.width == size
              − μₖ if is_fixed and g.width % size == 0 and g.width > size

    candidates = [ lowest_gap_fitting(size) ]
    if is_fixed:
        candidates += [ lowest_exact_gap(size),        # μ₁ candidate
                        lowest_tileable_gap(size) ]    # μₖ candidate

    match candidates.filter(Some).min_by_key(cost):
        Some(g) -> g.pos
        None    -> end                                 # nothing fits: extend
```

Only a fixed-size allocation may claim the fit bonuses — both of them, `μ₁` as
well as `μₖ`, for the reason given in stage 4. Three candidates suffice: the
exact fit and the tileable fit are the only gaps that can beat the lowest fitting
one, since `cost` is otherwise monotone in `g.pos`.

```
fn alloc(id, size) -> Address:
    addr = place(size, id.is_fixed_size())
    insert into the layout; the gap it landed in is consumed whole if the widths
    match, and otherwise trimmed from the bottom: G.pos += size, G.width -= size
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

**No destination ever splits a gap.** `place`, an evacuation and a slide all land
at the gap's *low* end, so a gap is only ever consumed whole or trimmed from the
bottom — `G.pos += n`, `G.width -= n` — and nothing ever lands in the middle of
one, leaving free space on both sides. That is worth stating as an invariant
because it bounds the update fan-out everywhere below: a destination touches one
gap, never two.

It does not, however, make the trim cheap in the evacuation index: both `G.pos`
and `G.width` are in the key, so a trim is still a delete-and-insert there rather
than an in-place edit.

### Proposing a step

Every candidate is scored `−ΔΦ/copied + K·retired/copied`, the two halves of
`Φ'`. `retired` is what the move does to `end`, and is zero for most moves:

| candidate | `−ΔΦ / copied` | `retired` |
|---|---|---|
| frontier slide | `gap_len` | `gap_len` if the run reaches `end`, else **0** |
| end slide | `w` | `w`, always |
| end evacuation | `score(A) − to` | `end − prev_end(A.pos)`, always `>= A.size` |
| classic evacuation | `score(A) − G.pos` | — *see below* |

```
fn propose_step(budget) -> Option<Step>:
    if no gaps exist:
        return None                       # already compact

    best = None

    # 1. The frontier slide. Offered unconditionally: it is the only candidate
    #    that does not require the moved bytes to fit in the gap, and so the only
    #    one that guarantees progress while any gap exists.
    best = offer(best, slide_candidate(budget))

    # 1b. The end slide, at K > 0. Offered second so a tie keeps the frontier
    #     slide, which is the shape that guarantees termination.
    best = offer(best, end_slide_candidate(budget))

    # 1c. The end evacuation: the topmost allocation into the lowest gap that
    #     takes it. Offered before the index's evacuation so that when the two
    #     agree, the shape is attributed to the candidate that sought it.
    best = offer(best, end_evacuation_candidate(budget))

    # 2. The exact evacuation within the budget, from the augmented tree.
    best = offer(best, best_evacuation_within(budget))

    # 3. The tileable evacuation (fixed-size allocations only). This can only
    #    re-rank evacuations step 2 already scored, never contribute one step 2
    #    could not see, so it cannot be the sole candidate -- see below.
    if best.is_some():
        best = offer(best, best_tiling_evacuation(budget))

    # 4. Opportunistic run extension -- stage 5's minimal version.
    if best is an evacuation:
        best = extend_into_run(best, budget)

    # 5. Nothing fit the budget. `best_evacuation()` is an O(1) root read, not a
    #    second search, so consulting it is strictly cheaper than making the
    #    caller re-enter with a larger budget -- but it is returned *flagged as
    #    over budget*, never merged into the comparison above, so the choice to
    #    exceed the budget stays the caller's.
    if best.is_none():
        return best_evacuation().map(mark_over_budget)

    return best
```

Three things about that shape are worth spelling out.

**The over-budget fallback stays, because it is not a second search.**
`best_evacuation()` is `root_argument()` — a single field read at the root, `O(1)`
— so it can never be more expensive than the caller re-entering `propose_step`
with a larger budget, which would redo the slide and the budget descent as well.
What it must *not* do is compete with the in-budget candidates on equal footing,
or an over-budget evacuation could win outright and silently break the budget.
Hence it is consulted only once nothing else has been found, and returned marked,
leaving the accept-or-decline to the caller. That is the same two-track selection
the current implementation has, and the reason for keeping it is cost, not policy.

**The tiling short-circuit is sound.** A tiling evacuation *is* an ordinary
evacuation — a fixed-size allocation moving down into a gap at least as wide — so
step 2 scores that very pair; `μ₁`/`μₖ` change only its rank. So if a downward,
in-budget tiling candidate exists, step 2 returned *something* (not necessarily
the same pair), and `best.is_none()` implies there is no tiling candidate either.
The "downward" qualifier is load-bearing: `best_tiling_evacuation` maximises its
two sides independently, so it must apply the per-class `min_gap_pos < max_alloc_pos`
check from stage 4 — without it, `μ` could rescue an upward pair and the
short-circuit's premise would fail along with the sign test.

**Both guards are defensive rather than hot.** `propose_step` returns early when
there are no gaps, and if a gap exists there is always at least one allocation
above it — otherwise it would be trailing free space above `end`, not a gap — so
`slide_candidate` always yields something and `best` is in practice never `None`
at steps 3 and 5. The guards cost a branch and buy the invariant that neither
path can be reached in a state it does not handle.

**The classic evacuation earns no `K` term, and cannot need one.** An evacuation
retires `end` only if its mover is the topmost allocation — that is what
`from + len == end` means — and there is exactly one such allocation. For a
fixed mover the index maximizes `score(A) − G.pos` by minimizing `G.pos` over
gaps that fit, which is `lowest_gap_fitting`: precisely the destination the end
evacuation picks. Same mover, same destination, same step, same gain, and the
end evacuation is offered first. (Verified byte-identical across 66 907
measurement rows.) That is a property of *the index's objective*, not of
evacuations in general — the tiling candidate below picks its destination by fit
rather than by position, so it does carry the term.

The end evacuation needs to be a candidate rather than a score term because "is
the topmost allocation" is not a function of a key and would re-key on every
mutation. But exactly one allocation has the property, so enumerating it
directly costs one `next_back()` and beats indexing it outright. It dominates
the end slide at the job `K` rewards: vacating the topmost allocation drops
`end` all the way to the top of whatever is below, so it retires `>= A.size`
while copying `A.size` — a rate of at least **1**, against the end slide's
measured 0.07.

```
fn slide_candidate(budget) -> Option<Candidate>:      # the frontier slide
    to = lowest_gap()?
    # A gap always has an allocation above it -- free space at the top is `end`
    # retreating, not a gap -- so the width need not be in the key: it is the
    # distance to the next allocation.
    from = next_start(to)?
    (len, _) = run_len_from(from, budget)
    if len == 0: return None
    retired = if from + len == end { from - to } else { 0 }
    return Candidate { from, to, len, retired }

fn end_slide_candidate(budget) -> Option<Candidate>:
    if K == 0: return None
    to = highest_gap()?
    from = next_start(to)?
    r = end - from                 # no gaps above `to`, so the run reaches `end`
    if r == 0 or r > budget: return None      # never truncate: see the guard
    return Candidate { from, to, len: r, retired: from - to }

fn end_evacuation_candidate(budget) -> Option<Candidate>:
    if K == 0: return None
    (from, A) = topmost_allocation()?
    if A.size > budget: return None
    to = lowest_gap_fitting(A.size)?
    if to >= from: return None     # nothing below it fits: this would be upward
    return Candidate { from, to, len: A.size, retired: end - prev_end(from) }

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

### B. The free-space directory — subsumed by C, never built

**Affords:** where the free space is, how wide, and which piece is the widest.

**All of them are answerable from C** (since C is keyed by size with gaps sorting
last at equal size, which is exactly the order a free-space directory wants):
- `lowest_gap()` / `highest_gap()` — `min_gap_pos` and `max_gap_pos` at the
  root. `O(1)`. These feed the two slides.
- `lowest_gap_fitting(w)` — `min_gap_pos` aggregated over the key suffix from
  `(w << 1) | 1`, one descent. Feeds placement and the end evacuation.
- `lowest_exact_gap(w)` — the gaps of width exactly `w` form a contiguous key
  block with prefix `(w << 1) | 1`, ordered within it by `score = G.pos`, so this
  is the first entry of that block: one `range(((w << 1) | 1, 0, 0)..).next()`,
  accepted only if its size field still reads `w`. Feeds `μ₁`.

`widest_gap()` is **no longer a production query at all**. Once the slide moved
to the frontier nothing needed it, so the two aggregate fields it used to
require (`max_gap_width` and the position achieving it) came out of the merge —
which runs `B` times per level on every update path — and the query survives
only as a diagnostic, as a descent entering the rightmost child whose
`min_gap_pos != u64::MAX`. It is compiled out of real builds and of benchmarks.

**B was never built.** The caveat flagged in stage 1 — that these are custom
descents against `sweep-bptree`'s `descend_visit`, whose visitor interface had not
been verified to support them — turned out not to bite: the interface is handed a
node's keys *and* all `size + 1` child arguments and returns which child to enter,
which is enough for a suffix aggregate, a two-sided canonical-subtree collection,
and a steered descent alike. So C answers everything B was proposed for, and the
two cheapest queries of all (`lowest_gap`, `highest_gap`) are root reads.

**Note:** today's `MoverTree` disappears entirely — its query ("the
highest-addressed allocation that fits in `w` bytes") is what the augmented
index now answers globally and in `O(1)`, rather than once per gap.

### C. The evacuation index — the best move, at the root

**Affords:** the single best evacuation in the whole heap, with and without a
budget constraint, in `O(1)` and `O(B log_B n)` respectively.

**Composed of:**
- An augmented B+ tree over the key `((size << 1) | is_gap, score, address)`,
  carrying `{ min_gap_pos, max_gap_pos, max_alloc_score, best, best_pair }` per
  subtree, merged by the right-to-left sweep of stage 1.
- Its entries: one per gap, one per resizable allocation, and — once `α > 0` —
  one per non-empty `(fixed class, nc)` bucket (see D). At `α = 0`, which is where
  the first implementation should start, there is no bucketing: simply one entry
  per fixed-size allocation.

**Queries:**
- `best_evacuation()` — read `root_argument()`. `O(1)`.
- `best_evacuation_within(budget)` — the prefix descent of stage 1's "the budget,
  for free": collect canonical prefix and suffix subtrees, take the suffix's
  `min_gap_pos` as a seed, sweep the prefix. `O(B log_B n)`.
- `lowest_gap()`, `highest_gap()` — root reads. These are why the two slides need
  no free-space directory of their own.
- `lowest_gap_fitting(w)`, `lowest_exact_gap(w)` — a suffix aggregate and a range
  lookup, as set out in B. These are why B does not need to exist.

**Maintained:** on every layout delta, and additionally whenever a neighbour
count changes, which re-keys the affected allocation (stage 3). Budget three to
four entry insertions or removals per mutation, plus up to four more for
re-bucketing when `α > 0`.

### D. The fixed-size class registry — needed only once `α > 0`

**Affords:** for each live fixed size and each neighbour count, the best-scoring
member — which is what the class's tree entries carry. At `α = 0` there is
nothing to bucket by, the de-duplication does not pay for itself (stage 3), and
fixed-size allocations should simply be ordinary entries in C; stage 4 then reads
`max_alloc_pos[s]` from C's key block for size `s` instead of from here.

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
  Again, a small `Vec` is likely better at `|C| ≈ 5`. These are plain
  (unaugmented) B-trees: they pay `log₂` comparisons on the way down, not the
  `B`-per-level fold that C's augmentation costs.
- Nothing for the exact-fit case: the `μ₁` candidate is `lowest_exact_gap(s)`,
  a range lookup in C.

**Queries:**
- `lowest_tileable_gap(s)` — `first()` of that class's set. Used by both `place`
  and `propose_step`.
- `best_tiling_evacuation(budget)` — for each `s ∈ C`, pair the class maximum
  (from D, or from C's size-`s` key block at `α = 0`) against **two** gaps: the
  lowest exact fit, worth `μ₁`, and this set's minimum, worth `μₖ`. Take the
  better, skipping either whose gap does not sit below the class maximum — that
  test is exact, since if the lowest such gap is above the highest allocation of
  the class then all of them are. `O(|C|)`.

**Maintained:** when a gap of width `G` is created or destroyed, test each `s ∈ C`
for `G mod s == 0` and update those sets — `O(|C| · log #gaps_of_that_class)`,
which is less than one insertion into C. Typically much less: a width is
divisible by `s` for only about `1/s` of widths, so the usual gap event is `|C|`
modulo tests and zero or one set update. When a class enters, its set starts
empty and is **not backfilled**; when a class leaves, its (now empty) entry is
retained for reuse.

**TODO:** cap the number of tracked classes. `|C|` multiplies every gap creation
and destruction, and retaining entries means it never shrinks, so an application
that mints fixed-size allocations at hundreds of distinct sizes would make this
unbounded. Eviction is safe — an untracked class just earns no destination-side
bonus in `place` or `propose_step`, and every move it makes stays valid.

### Summary of the change against what exists today

| today | becomes |
|---|---|
| `allocations` + `by_id` + `end` | A, unchanged |
| `free_by_size` | **gone** — subsumed by C; B was never built |
| `GapTree` (address-keyed, max-width) | **gone** — subsumed by C's descents and root reads |
| `MoverTree` (address-keyed, min-size) | **gone** — C answers its question globally |
| `live_by_size` (3-way by neighbours) | D, unchanged in shape — but only once `α > 0` |
| — | C, the new augmented index |
| — | E, the new tileable-gap index |

The net is three of today's five structures removed and two new ones added,
leaving A, C, D, E — and the candidate search stops being a walk. The `α = 0`
first implementation drops `live_by_size`/D as well, leaving just A, C and E.

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

**In the event the query cost did move to the update path, and the interesting
number is not the tree's.** A burst's cost tracks the number of allocations it
*re-keys*, at a few microseconds each, and each re-key is `O(B log_B n)` with the
augmentation refolded over all `B` children at every level on the path. That is
the design's stated trade, and it is why the frontier slide is expensive in the
active regime: it drags long runs of *small* allocations across the compacted
region, re-keying ~7× more entries for the same bytes moved. Roughly 16 index
operations are issued per allocation moved, of which only 2 — remove the old key,
insert the new one — are irreducible; the rest is gap bookkeeping repeated once
per member of the run, and neighbour re-keying that is skipped entirely once `α`
is known to be unspendable. A bulk commit path for contiguous runs is the obvious
unexploited saving, and is not built.

Two further advantages are worth weighing:

- **The budget query is exact**, in `O(B log_B n)`, replacing a two-track
  heuristic.
- **Resizable allocations need no special handling** in the index. The current
  design excludes them from the size-class structure because they would scatter
  one per class; here they are ordinary entries. The fixed/resizable distinction
  survives only where it belongs: the destination-side bonus of stage 4.

### What I would not claim yet

- **Stage 3 costs more than the estimate suggested, and the fix was elsewhere.**
  Making the score depend on neighbours means each mutation re-keys up to four
  other entries — and the implementation was doing that *unconditionally*, even
  at `α = 0` where the score ignores neighbours entirely. Skipping it when `α`
  cannot be spent (which includes `λ = 1`, since the per-allocation cap gives `λ`
  first claim on the headroom) took 26–45% off a burst on the realistic shapes.
  That was pure overhead the design note had described as "the cost `α` is
  measured against" without noticing it was being paid at `α = 0` too.
- **Timing measurements on this hardware are not trustworthy build-to-build.**
  Five consecutive runs of one binary span 6%, but two *builds* can differ by
  30%+ — the first run after a rebuild is reliably fast. A criterion A/B against
  a saved baseline lies along exactly that axis, and produced a 20–35%
  "regression" that a repeat run showed to be nothing. Only alternating the two
  versions several times in one session is reliable. Every structural figure in
  this document (steps, bytes, gaps, overhead) is deterministic and unaffected.
- **Stage 5 is deferred for a reason that may not survive tuning.** Runs need a
  steep reward, and the adopted sign bound forbids one. If `λ · reward(s) <= s`
  proves too tight in practice, the dual-aggregate alternative reopens both
  questions at once.
- **Stages 2 and 3 change the policy, not just the implementation.** The
  objective is no longer `ΔΦ / cost`, so the fragmentation behaviour would have to
  be re-measured from scratch. Every fragmentation number this project currently
  has was produced under distance-greed.
- **The top of the heap is modelled, but only just.** `K · end` is the term that
  says file size matters, and adding it is what makes the compactor greedy on a
  potential that includes the quantity actually read off the heap. But the
  measured spread across a 256× range of `K` is about 0.13 percentage points of
  overhead on one seed, and `shrinking` is non-monotone in `K`. What is robust is
  the sign — every `K > 0` improves the growing workload and every `K > 0` costs
  13–19% more copying to converge — not the placement of any particular value.

### What was built

All of stage 1, stage 2 (`λ` as a bool), stage 4 (`μ₁`/`μₖ` with the size-class
registry E), and stage 5's opportunistic run extension. Stage 3's `α` is
implemented but is inert at `λ = 1` and measures as a tie-breaker; D was never
needed, because at `α = 0` there is nothing to bucket by, and C carries one entry
per allocation.

Beyond the five stages, two things the original document did not anticipate:

- **The slide became the frontier slide**, which is where the largest single
  improvement came from — a 38× reduction in steps and 275× in bytes copied to
  converge. The document had treated "which gap to slide into" as settled.
- **The objective grew the `K · end` term**, and with it two new candidate
  shapes whose product is file size rather than potential.

`widest_gap()` and the two aggregate fields serving it were removed as a
consequence of the first; `highest_gap()` and `max_gap_pos` were added as a
consequence of the second.

### What is still open

The **end slide** earns its place only in the active regime, and pays for it
during convergence; removing it is measured and defensible, keeping it is
measured and defensible. The **`K` value** is chosen from one seed. And the
pathology in the next section has a design but no implementation.

---

## Future extensions

### The stress test: a shrinking allocation at a low address

Everything above is measured on workloads that churn roughly uniformly. Here is
a shape they do not cover, and on which the current policy is catastrophic.

Put a resizable allocation at a low address and shrink it a little at a time,
with a compact heap above it. Each shrink opens a **sliver at the frontier** that
nothing fits: no evacuation can use it, no end evacuation is available (the gap
fits nothing), and the end slide declines because the top run is far larger than
one budget. The frontier slide is the only candidate left — and it walks that
sliver to the top of the heap one budget at a time, reclaiming nothing until the
last step of each chain.

Measured, with 400 allocations above the shrinker and a 2048-byte budget:

| shrink step | reclaimed | steps | bytes copied | copied per byte reclaimed |
|---|---|---|---|---|
| 8 B | 128 | 128 | 255 360 | **1 995** |
| 64 B | 1 024 | 144 | 255 360 | 249 |
| 512 B | 8 192 | 144 | 255 360 | 31 |

The copying is *identical* in all three: `255 360 = 16 shrinks × 15 960 bytes
live above the shrinker`, exactly. So

> the cost of a shrink is the whole live heap above it, **regardless of how many
> bytes the shrink freed**.

It scales with the heap, so on the 555 KB heap of the main measurements one
8-byte shrink would copy 555 KB.

`K` cannot help, and correctly does nothing: a truncated frontier slide retires
no `end`, so its `retired` is zero. Nor is the move dishonest in `Φ` — sliding
15 960 bytes down by 8 really does drop `Φ` by 127 680. That is the whole
problem. The move is exactly as good as the objective says it is, and the
objective is not sensitive to the fact that this particular byte of gap is worth
nothing to anybody.

Note also that the compactor is destroying something valuable. A narrow gap at a
*low* address is the best destination `place` has, since placement takes the
lowest gap that fits; this project's own free-space accounting says most free
space is destroyed by allocations landing in gaps rather than by truncation. So
the eager slide spends 15 960 bytes to remove the allocator's preferred slot.

### The design, in two halves

Two mechanisms answer it, and both are adopted. The first changes where
allocations *settle* and is expressed in the objective; the second changes where
a demonstrably churning allocation *is put*, and is expressed as a discrete
action on the resize path. Neither works alone: γ has no way to lift an
allocation that is already low, and the lift has no way to pay for itself
without γ.

### Half 1: γ — weighing resizable against fixed-size allocations

Make the potential distinguish the two kinds:

```
Φ = Σ_{bytes of resizable allocations} address
  + γ · Σ_{bytes of fixed-size allocations} address        (γ > 1)
```

Fixed-size bytes are heavier, so the compactor prefers to move *them* down into
gaps, and the minimum-`Φ` layout puts every fixed-size allocation below every
resizable one. That is where you want the allocation that will later release
space in place: space released near `end` is retired by truncation, space
released at address 0 costs a heap-walk.

**This looks like the objection of stage 2 and is not.** A per-allocation
multiplicative weight normally destroys the merge — `gain = w(A)·A.pos −
w(A)·G.pos` makes the coefficient on `G.pos` depend on which allocation is
chosen, so the two sides can no longer be maximized independently, which is why
`w(s) = s/(c₀+s)` was rejected in favour of an additive reward. But γ takes
**finitely many values**, so you partition instead of linearize:

```
best = max over classes c of   γ_c · ( lower.max_alloc_score[c] − self.min_gap_pos )
```

Within a class the weight is constant, so `γ_c·(A.pos − G.pos)` factors cleanly
and independent maximization is restored *per class*. The max across classes is
taken at the end, in `O(#classes)` — still `O(1)` per merge at two classes.

```
Aggregate {
    min_gap_pos, max_gap_pos,                       # unchanged
    max_alloc_score[c], max_alloc_addr[c], max_alloc_size[c],   # one triple per class
    best, best_from, best_to, best_len,             # still one combined winner
}

fn extend_left(self, lower):
    for c in classes:
        # saturate *before* scaling: an upward pair must collapse to 0 first, or
        # γ turns a wrapped difference into a large positive.
        crossing = γ[c].saturating_mul(
            lower.max_alloc_score[c].saturating_sub(self.min_gap_pos))
        if crossing > self.best:
            self.best = crossing
            self.best_from, self.best_len = lower.max_alloc_addr[c], lower.max_alloc_size[c]
            self.best_to = self.min_gap_pos
    if lower.best > self.best:
        self.best, self.best_from, self.best_to, self.best_len = lower.best, ...
    self.min_gap_pos = min(self.min_gap_pos, lower.min_gap_pos)
    self.max_gap_pos = max(self.max_gap_pos, lower.max_gap_pos)
    for c in classes:
        if lower.max_alloc_score[c] > self.max_alloc_score[c]:
            self.max_alloc_score[c], self.max_alloc_addr[c], self.max_alloc_size[c] = lower...
```

What it costs:

- **Three fields per extra class** in the augmentation, ~20 bytes per node, and
  one more compare-and-subtract in `extend_left`. Call it 30–40% more merge work,
  which is the hot path.
- **The budgeted descent is mechanical.** The prefix sweep uses the same rule,
  and the whole suffix still collapses to the single `min_gap_pos` scalar,
  because that argument is about sizes and is indifferent to class.
- **No new metadata.** `AllocationId::is_fixed_size()` already exists and
  `alloc_key` has the id in hand at every call site. The classification is
  declared, immutable, and free.
- **The sign bound loosens** to `λ·reward(s) + α <= γ·size`, so stages 2 and 3
  are unaffected.
- **The slide's score becomes composition-dependent.** A run of mixed classes
  gains `(weighted mean γ) · gap_len` per byte, so `run_len_from` has to
  accumulate the class mix as it walks. It already walks the entries, so this is
  cheap, but the score stops being a one-liner.

**What γ alone cannot do.** The compactor only ever moves things *down* into
gaps. Preferring to move fixed allocations down does not lift resizables up;
they become relatively higher only as things beneath them compact. So γ biases
which allocation claims a low gap, but will not repair a layout that is already
wrong — which is what the second half is for. (A cheaper partial substitute,
not adopted, would be to have `place` steer resizable allocations away from the
lowest fitting gap, so the layout segregates itself at allocation time.)

### Half 2: the shrink counter and the lift

#### The counter

A **sparse side map**, `shrink_counts: HashMap<Id, u8>`, in which an absent key
means zero.

Keyed by `Id`, which is what makes it cheap in the way that matters: a
compaction move changes an allocation's *address*, not its id, and
`insert_raw`/`remove_raw` maintain location state only — `allocations`, keyed by
address, and `by_id`, whose value is the address. A shrink counter is not
location state, so **neither primitive mentions it**. The count survives every
compaction move because nothing is acting on it, rather than because something
is carefully preserving it. That asymmetry is the whole reason for a side map
rather than a field in `Entry` (+4 bytes on every allocation, measured) or a byte
packed into `by_id`'s address word (free, but destroyed and recreated on every
move, so it would have to be threaded through both primitives — an obligation
invisible to anyone reading `commit_compaction_step`, and silent when forgotten).

It is touched in exactly three places, none of them on the compaction path:

| site | action |
|---|---|
| `resize`, shrink branch | increment, then test the trigger |
| the lift | **remove** the entry (reset means remove, so the map stays sparse) |
| `free` | remove the entry |

`free` is the right and only home for cleanup: `remove_raw` is also called from
both branches of `resize` and from `commit_compaction_step`, and in every case
but `free` the allocation lives on at a new address or a new size. `alloc`,
`free` and `resize` are the whole public surface, so there is no other death
path. `assert_consistent` checks that every key corresponds to a live resizable
allocation, which is what would catch a fourth caller appearing later.

#### When the counter advances

Only on a shrink that **actually creates a frontier sliver** — that is, only when
no gap already sits below C. If one does, the frontier slide targets that gap
instead and this shrink costs nothing extra, so counting it would measure
activity rather than damage.

The test is one root read. After the in-place shrink the sliver starts at
`c + s`, so "no gap below C" is exactly "the sliver we just made is the
frontier":

```
index.lowest_gap() == Some(c + s)
```

That condition has a consequence used throughout below: **the gap C would leave
starts exactly at `c`.** Nothing merges downward, because there is nothing free
below C to merge with. So the vacated gap is `[c, gap_end)` with
`w = gap_end − c ≥ old_size`, and the replacement's destination is C's own old
address.

#### The trigger

```
count × L  ≥  T · (s + r_len)      and      count > 1
```

with `T` dimensionless — "how many times over must the lift have paid for itself
before I do it" — and

```
L = min(r_pos, target) − c
```

`L` is the height C actually gains, discounted by the assumption that the next
compaction may pull it back down to where the replacement came from. It stands in
for the quantity that drives the pathology, live bytes above C, and is a
conservative proxy for it: `bytes_above(c) − bytes_above(c + L) ≤ L`. It also
correctly refuses to credit a lift that barely raises C.

The trigger self-tunes in the way a fixed count would not:

- C low in a large heap, with a distant replacement: `L` is large, the threshold
  is ~1, lift almost at once. Correct — each shrink is costing a heap-walk.
- C already high, or the replacement immediately above it: `L` is small, the
  threshold is large, effectively never. Correct — the lift buys nothing.
- C large, or an expensive replacement: `s + r_len` is large, so demand more
  evidence.

A `u8` still suffices, and saturating at 255 means "never lift", which is the
right answer whenever the computed threshold exceeds it.

Because the exact test needs both a target and a replacement, a cheap
**necessary** condition prunes it first. `L ≤ end − c` and `s + r_len ≥ s`, so

```
count × (end − c)  <  T · s        ⟹        the exact test fails too
```

and the searches below can be skipped without changing any decision.

#### The target

```
target = match index.highest_gap_fitting(s):
    Some(g) if g > c + s  =>  g          # strictly above the vacated span
    _                     =>  end
```

`highest_gap_fitting(s)` is the exact mirror of `lowest_gap_fitting(s)` — the
same suffix descent from key `(s << 1) | 1`, collecting `max_gap_pos` instead of
`min_gap_pos` — and `max_gap_pos` is already in the aggregate, so it is nearly
free. It puts C as high as possible, which is the goal, and needs none of the
2-D machinery that "the nearest gap above C" would.

The single comparison `g > c + s` rejects two cases at once: a gap *below* C,
which would move C the wrong way, and the sliver C just made, which is contiguous
with C's own span — a "lift" into it would raise C by exactly its own size, and
the arithmetic of vacated-span-merges-destination-gap is an easy way to corrupt
the layout.

The whole operation is skipped when C is already the highest allocation.

#### The replacement

The largest allocation that fits, **in each class**, with the two candidates then
compared by γ-weighted gain — so blocking and the class preference are both
expressed. Largest-fit rather than best-score because the point is to leave a
residue too small to take C back.

```
fn best_replacement(c, w) -> Option<Alloc>:
    best = None
    for class in classes:
        if let Some(a) = largest_fitting_above(class, w, floor = c):
            if a.addr > c:                       # exact check; see the descent
                best = max_by(best, a, key = γ[class] · (score(a) − c))
    if best.is_some(): return best

    # (3) The fallback: the allocation directly above the gap, slid down. It
    # always exists, because C is not the highest allocation -- but note that a
    # slide moves the gap up rather than consuming it, so this case blocks
    # nothing. It is taken anyway; see "what is still open".
    return allocation_starting_at(c + w)
```

#### The steered descent

`largest_fitting_above(class, w, floor)` finds the largest allocation of that
class with `size ≤ w` and `addr > floor`. Two phases, because the size-prefix
`[0, w]` is not a single subtree — it is the union of the canonical subtrees
collected along a descent to the boundary key, exactly as
`best_evacuation_within` collects them.

```
fn largest_fitting_above(class, w, floor) -> Option<Alloc>:
    boundary = (w + 1) << 1                      # first key of any larger size
    prefix   = canonical_subtrees_below(boundary)        # ascending key order

    # Right to left: the first subtree that can hold a qualifying allocation
    # holds the largest one, since the key order is by size.
    for sub in prefix.reversed():
        if sub.max_alloc_addr[class] > floor:
            if let Some(a) = descend_rightmost(sub, class, floor):
                return Some(a)
            # sound but incomplete -- keep looking leftward
    return None

fn descend_rightmost(node, class, floor) -> Option<Alloc>:
    while node is inner:
        i = rightmost index with node.argument[i].max_alloc_addr[class] > floor
        if i is none: return None
        node = node.child(i)
    return rightmost key k in node with
        k.is_alloc and class(k) == class and k.addr > floor
```

**The predicate is sound but not complete.** `max_alloc_addr[class]` is the
address of the subtree's highest-*scoring* allocation of that class. At
`λ = α = 0` the score *is* the address, so the field is the maximum address and
the predicate is exact. Once `λ` or `α` is set they diverge, and a subtree
holding a qualifying allocation whose score is not the maximum can be skipped.
The consequences are a smaller replacement than the true largest, or a fall
through to the slide — never an invalid one, since the predicate holding
guarantees a qualifying allocation is present. The leftward retry above recovers
some of what the incompleteness loses; the `a.addr > c` check in
`best_replacement` is then an assertion that never fires.

#### The two moves

```
fn on_shrink(id, c, old_size, s) -> Relocation:
    shrink_in_place(id, s)                       # always fits; leaves the sliver
    if not id.is_resizable():                    return Relocation::None
    if index.lowest_gap() != Some(c + s):        return Relocation::None   # not the frontier

    count = shrink_counts.get(id) + 1
    shrink_counts.insert(id, count)
    if count <= 1:                               return Relocation::None
    if c == highest_allocation_start():          return Relocation::None
    if count · (end − c) < T · s:                return Relocation::None   # cheap prune

    gap_end = next_start(c + s) or end
    w       = gap_end − c                        # >= old_size
    target  = target_for(c, s)
    r       = best_replacement(c, w)?
    L       = min(r.pos, target) − c
    if count · L < T · (s + r.len):              return Relocation::None

    # ORDER IS MANDATORY. The replacement's destination is `c` -- C's own old
    # address -- so C's bytes must be copied out before the replacement's are
    # copied in.
    move_allocation(id, from = c, to = target)
    shrink_counts.remove(id)                     # reset == remove
    move_allocation(r.id, from = r.pos, to = c)
    return Relocation::Double { first: (c, target), then: (r.pos, c) }
```

`Relocation` becomes an ordered enum:

```
enum Relocation<A> { None, Single { old: A, new: A }, Double { first: (A, A), then: (A, A) } }
```

The blast radius is smaller than it looks: `Relocation` is only `resize`'s return
type, and exactly one place acts on it — `ComposedBackend::resize`, which covers
the destination and copies the bytes. `MockBackend` and `UnjournaledBackend`
forward it untouched. The `Double` arm does the same thing twice, in order.

Note that the second move is a *downward* move of a single allocation, so it is
the ordinary evacuation shape; when it comes from the fallback it is a slide, and
its source and destination overlap, which `copy_bytes` already handles because
`commit_compaction_step` produces overlapping slides today. The first move is
*upward*, which is why none of this can go through `commit_compaction_step` —
whose `assert!(to < from)` is what underwrites "every step independently lowers
`Φ'`".

### What this design does not fix

- **The lift is unbudgeted.** The compaction budget bounds per-flush copying;
  this happens on the resize path and bypasses it. Bounded by `s + r_len`, but
  those are `u32` sizes, so the bound is nominally large and an adversary picks
  the moment. Capping the lift at some multiple of the compaction budget — and
  letting the count keep rising when it does not fit — would close that.
- **Moving to `end` grows the file**, and the operation is not scored against
  `K` because it does not happen during compaction. Later compaction reclaims
  it, so it is transient, but it is a regression in the quantity `K` exists to
  protect, taken to avoid an unbounded copying cost. `highest_gap_fitting`
  avoids it whenever any gap above will take C.
- **The undo is bounded, not prevented.** The vacated gap is at least C's *old*
  size, so C — now smaller — always fits it. If the best replacement leaves a
  residue of at least `s`, or if the slide fallback is taken (which moves the gap
  up rather than consuming it), the next compaction can pull C straight back.
  What limits the damage is the counter: the reset means at most one wasted lift
  per `T` qualifying shrinks. And it is never wholly wasted — because the vacated
  gap starts exactly at `c`, a dragged-back C lands at `c + r_len`, strictly
  above where it started.
- **`T` is unmeasured**, and unlike `λ`, `α`, `μ` and `K` it gates a discrete
  action rather than weighting a continuous one, so it cannot be swept the same
  way.
- **The descent's predicate is approximate away from the default policy**, as
  set out above.
