# Incremental compaction, from a clean slate

Status: design exploration. Deliberately *not* anchored to the current `Allocator`/`Backend`
split from [address-ranges-id-pool-decoupling.md](address-ranges-id-pool-decoupling.md);
§6 discusses which conclusions of that note survive and which fall.

The one fixed constraint: **pointers are stable ids**, so an in-memory table of all live
allocations — `id → (address, size)` — exists no matter what. Everything below treats
that table as given and asks what the cleanest incremental memory manager around it looks
like.

## 1. Problem statement

Maintain a partition of the address space `[0, end)` into **allocations** (each tagged with
the stable **id** that names it) and **gaps** (the free space between them). Support:

- `alloc(id, size) → address`, `free(id)`, `resize(..) → Option<new address>` —
  the foreground operations, each O(log n) CPU, no byte movement except resize-relocation.
- a **compaction step** (`budget → progress`) — a background operation, callable arbitrarily
  often, each call guided by `budget` bytes copied and O(log n) CPU **including candidate
  discovery**, such that repeated calls converge to a compact heap (`end` = total live
  bytes). No stop-the-world phase, ever; a full compaction is just that step in a loop
  that something is allowed to interrupt.

Progress can be made precise with a potential function over live **bytes**, not allocations:
`Φ = Σ_{live bytes b} address(b)`
(`= Σ_{allocations a} size(a)·(address(a) + (size(a)−1)/2)`). With `L` total live bytes,
`Φ ≥ L(L−1)/2`, with equality for **every** gapless layout regardless of allocation order; the
excess `Φ − L(L−1)/2` counts exactly the (free byte, live byte) pairs where the free byte
sits *below* the live byte — the heap's "inversions" — so minimal `Φ` ⟺ compact, and the
excess doubles as a fragmentation-debt metric. (The simpler-looking variant that sums
allocation *start* addresses is subtly wrong: its minimum depends on allocation order — it insists
small allocations come first — and the bias this induces in the greedy policy is actively
harmful; see the second worked example in §4.) Every useful compaction step strictly
decreases `Φ`. `end` itself shrinks whenever a step (or a `free`) clears the suffix — so
`Φ` is the *progress* measure and `end` the *payoff*, which lags it.

Every step relocates live bytes downward, and one lens prices all of them: relocating an
allocation of size `s` down by a distance `d` copies `s` bytes and makes `ΔΦ = s·d` progress —
per byte copied, **gain `d`**, the travel distance. Two familiar shapes are extremes:

- **Slide**: the packed run directly above a gap of size `g` shifts down by `g`. Every
  byte in the run travels `d = g`, so the gain is the **gap size** — independent of what
  the run contains, and poor whenever the gap is small.
- **Evacuation**: a single allocation jumps from near the tail into a gap far below it; the
  gain is its full travel distance, possibly `d ≫ g` for every gap `g`. The extreme case
  is a few small allocations stranded above a large free region — copying a handful of bytes
  releases a huge suffix. That is the most valuable move a compactor ever gets, and it is
  precisely the state a file lands in after a burst of frees below a few survivors.

**Exact fit is about fragmentation, not gain.** Placing an allocation into a gap of exactly
its size leaves no remainder sliver and lets the vacated range coalesce fully. But among
candidate destinations it is a *tie-break*, not a gate: a deep, larger-than-needed gap
(large `d`) beats a shallow exact one on progress, and the split remainder is a tracked
gap like any other.

A warning the gain lens makes precise: **greed in `end` is myopic**. `end` is a
discontinuous payoff — a single unfortunately-sized tail allocation defers all of it, while
cheap interior moves, whose payoff to `end` arrives later via gap coalescence, score zero
and go untaken. `Φ` is the smooth surrogate: interior downward moves are credited
immediately, and truncation falls out whenever the suffix happens to clear. So the
scheduling rule is: **greedily maximize the per-byte gain — travel distance — wherever in
the file the move is**; prefer exact fits among comparable destinations; and find each
next move in O(log n), not by rescanning. §4 walks a worked example where `end`-greed
degenerates into ~11× overcopying while plain distance-greed compacts the same file
optimally, with no lookahead.

**Why exact fits should exist at all**: kladde deliberately hands out many equal-sized
fixed-size allocations (that was the point of the `Sizedness::Fixed` hint and the size-class
placement idea). Freeing any one of them mints a gap that is a plug-compatible slot for
every other live member of the same size class. In the degenerate case where all sizes are
unique, exact fits simply never fire and every step is an evacuation-with-split or a slide
— nothing breaks, compaction is just less tidy.

## 2. What incremental compactors in the wild do, and what is different here

Region-based collectors (Immix, G1, ZGC, Shenandoah) bound each increment by *region*, keep
only per-region summaries (live-byte counts) rather than a global per-object index, pick the
emptiest region, evacuate its survivors elsewhere, and spend almost all of their real
complexity on **pointer fixup** — remembered sets, forwarding pointers, load/write barriers —
because raw pointers into moved objects are scattered through the heap. Mesh (Powers et al.,
2019) compacts a `malloc` heap *without* moving pointers by merging pages whose live offsets
happen not to collide — an "exact-fit" idea at page granularity, driven purely by allocator
metadata.

kladde's situation is structurally easier on the hard axis and harder on the easy one:

- **Pointer fixup is free.** Every reference goes through the id table; a move is one table
  update. The entire barrier/remembered-set apparatus — most of the literature — is moot.
- **No liveness discovery for free, either.** GCs avoid per-object tables by *tracing*;
  kladde has no tracer, but doesn't need one: the stable-id constraint forces the
  per-allocation table into memory anyway. What a GC gets from tracing, kladde already holds
  as a data structure.

So the residual problem is neither barriers nor tracing — it is purely the **matching and
scheduling** problem of §1. That is why importing a GC architecture (regions, evacuation)
would over-engineer this: the natural design here is index-driven, not scan-driven.

## 3. Principles

- **P1 — Sunk state is free state.** The live table must exist (stable ids). Any design
  that hides it from the component doing placement/compaction pays twice: once for the
  table, once for a shadow of it. Corollary: the free-space index and the allocation index
  should be *views of one structure*, not sibling structs that mirror each other through an
  interface.
- **P2 — Incremental means incremental discovery.** Bounding the bytes copied per step is
  worthless if finding the step scans O(heap). The actionable-move set must be maintained
  by the foreground ops (`alloc`/`free`/`resize` each touch O(1) size classes) so that
  each compaction step starts from a ready answer.
- **P3 — Discovery must be a query, not a scan.** What compaction consumes is neither
  "gaps" nor "movable allocations" but *pairs* (mover, destination), weighted by gain. The
  candidate pairs must come from maintained per-class indexes and O(log n) queries (§4
  evaluates one candidate per size class this way — cheap because the class count is
  small), never from rescanning allocations. Quiescence ("nothing worth moving") must be
  equally cheap to detect.
- **P4 — Mechanism/policy split.** The mechanism is "move allocation X into gap Y, update
  indexes, report the move." Which pair to pick, when to slide instead, when to stop — that
  is policy, and it should be swappable without touching the index maintenance.
- **P5 — Persistence concerns stay out.** Id *minting* policy and the on-file table layout
  remain coupled to the persistent format (that argument from the earlier note stands).
  The memory manager never chooses ids and never serializes them. It is *nearly* opaque
  about them: the one thing it reads is a single placement-relevant bit, sizedness, via the
  `AllocationId` trait (§5) — which leaves the minting policy entirely with the backend.
- **P6 — Crash safety by construction.** A move copies into *free* space and then flips
  one table entry. Until the flip, the copy is invisible; after it, the old bytes are
  garbage. The flip is the sole commit point, so incremental moves are atomic and an
  interrupted move is abandoned at zero cost. (This falls out of the id-table indirection;
  it is worth preserving in whatever design wins.)

## 4. The core structure: one partition, derived indexes, gain-greedy steps

Single source of truth — the allocations; gaps are *implied* by what lies between them:

```rust
// Keyed by start address. Gaps are never stored here: the gap preceding the entry at
// `a` runs from `prev.address + prev.len` (or 0 if there is no predecessor) to `a`,
// and may be empty. `end` is the last entry's end.
allocations: BTreeMap<Address, Allocation>
struct Allocation { len: Size, id: Id }
```

No sizedness field: it is carried by `id` itself (§5's `AllocationId::is_fixed_size`).

Keeping only the allocations (rather than a tagged cover of `[0, end)` by an
`enum { Live, Free }`) is the lighter representation on every axis: fewer entries, no
`Live`/`Free` match on a lookup whose variant the caller already knows, and — the useful
invariant — **compaction never changes the entry count**, since a move rewrites one key
rather than splitting or merging cover entries. A gap query at `address` is
`allocations.range(..address).next_back()`, and the gap's length falls out of that
entry's end.

Secondary indexes, all derivable from `allocations`, all maintained by the same handful of
mutation paths (`alloc`, `free`, `resize`, `apply move`):

```rust
by_id:  HashMap<Id, Address>                    // id resolution (this IS the id table's
                                                // address column; the sunk cost, exploited)
free_by_size: BTreeMap<Size, BTreeSet<Address>> // gaps, grouped by exact size
live_by_size: BTreeMap<Size, BTreeSet<Address>> // movers (fixed-size allocations only — see
                                                // below): a class's best candidate is its
                                                // highest-addressed member
```

`free_by_size` is the one place gaps are materialized — a derived index over the implied
gaps, not a second source of truth. Empty gaps (two allocations flush against each other)
are simply never inserted.

**`live_by_size` holds only `Fixed`-sizedness allocations.** Those are the ones kladde
deliberately mints in bulk at a handful of distinct sizes, so their classes are few and
densely populated — exactly what `BTreeMap<Size, BTreeSet<Address>>` is good at. Resizable
allocations would instead scatter across many classes with often a single member each: a poor
fit for the structure, and poor candidates besides, since parking a resizable allocation in a
snugly fitting gap only guarantees it must relocate again the moment it grows a byte. They
remain movable by *slides* (which are derived from geometry and so cover every allocation
regardless of sizedness); they just do not generate exact-fit candidates.

`BTreeSet<Address>` per class rather than a binary heap: min *and* max are O(log), and —
unlike a heap — arbitrary deletion is native, which matters because gaps are consumed by
ordinary `alloc` and allocations die by ordinary `free`, not only by compaction. (This subsumes
the per-class min-heap idea; the heap's O(1) peek isn't worth losing cheap deletion.)

The destination lookup the corrected gain analysis (§1) demands is one query the per-class
sets cannot answer cheaply: **the lowest gap with `len ≥ s`** — a 2-D dominance query
(minimize address subject to a size bound). The textbook structure is an address-ordered
balanced tree augmented with each subtree's *maximum gap length*: descend leftmost-first
into any subtree whose max is `≥ s`, O(log n). `std` has no augmented `BTreeMap`; the crate
`sweep-bptree` does, via its `Argument` (maintain) and `SearchArgument` (descend) traits.

**Gaps are materialized as tree entries rather than left implicit between allocations.**
Augmenting the `allocations` map directly stores nothing new, which is why it looked
preferable, but it is a much sharper knife: a gap can *straddle* the boundary between two
children and so belongs to no child's subtree, forcing the augmentation up to the monoid

    A = (min_start, max_end, max_gap)
    A(left) ∘ A(right) = ( left.min_start, right.max_end,
                           max(left.max_gap, right.max_gap,
                               right.min_start − left.max_end) )

and forcing the *descent* to thread the left-hand boundary down through the query to
reconstruct those crossing gaps. On top of that the leading gap (address 0 to the first
allocation) has no left neighbour and needs handling outside the tree, and empty subtrees
must be told apart from zero-length ones or the crossing subtraction underflows.
(`benches/btree_point_lookup.rs`'s augmentation is exactly this shape, and is ad-hoc: it
skips the leading gap and underflows on an empty child.) Materializing gaps dissolves all
of it — the augmentation becomes a plain bottom-up maximum of a value each entry already
carries — at the cost of one index the heap was keeping anyway, since `free_by_size` stays
for *exact*-fit lookups regardless. One wrinkle drove the key's shape:
`SearchArgument::locate_in_leaf` receives only the leaf's **keys**, never its values, so
the length has to live in the key.

**Measured** (`benches/lowest_fitting_gap.rs`, and the reason §7 no longer lists this as
open): the axis that matters is not the gap count but the number of distinct gap
*lengths*, since the scan costs one probe per distinct length `≥ s`. With gap lengths
clustered on 4 values the scan is *faster* — ~4.5 µs vs ~5.1 µs per 256 queries at 100
gaps — so kladde's design bet, taken alone, argues against the tree. But the scan degrades
with spread while the descent does not: at 64 distinct lengths the tree is 7–9× faster, and
at 1024 it is 11–118× (976 µs vs 8.3 µs at 100k gaps). The tree stays in the 5–8 µs band
across every configuration. Since split remainders and resizable allocations generate
arbitrary lengths regardless of how disciplined the fixed-size classes are, the flat
profile is worth the constant factor in the clustered case.

`sweep-bptree` seems to be unmaintained, so we'll eventually want to replace it with either
a vendored implementation or something else, maybe `btree-slab` or `rust-lapper`.

A compaction step (`propose_compaction_step(budget)`, then `commit_compaction_step`) —
**gain-greedy**:

1. Candidate generation, one per fixed-size class `s` that has live members: the class's
   best mover is its **highest-addressed member** (`live_by_size[s].last()` — within a
   class the destination is shared, so the highest member maximizes `d`). Its destination
   is the exact-fit fast path (`free_by_size[s]`'s lowest address) or the augmented-tree
   query (lowest gap `≥ s`, split on use); its gain is its travel distance `d`. Add one
   *slide* candidate — the run above the **largest** gap, gain `g` — for the regime where
   no fit exists anywhere; the slide is also what moves resizable allocations, which generate
   no candidates of their own.
2. Pick the best candidate, by **iterating over all classes**. Under kladde's design bet —
   many allocations of *few* distinct fixed sizes — the class count is small, so evaluating
   every class is O(#classes · log n) per step with no incremental machinery. This naive
   scan is what ships first; §4.1 describes the bounded best-candidate upgrade for the day
   class counts grow (and why a naively gain-keyed priority queue is subtly broken).
3. Execute: copy the bytes (chunked against `budget`), flip the affected table entries,
   update the indexes; the vacated range coalesces with its free neighbors (touching O(1)
   classes).
4. If the suffix of the address space is now free, retreat `end`. Repeat while budget
   remains; quiesce when no candidate's gain clears a policy threshold.

**A `Step` is a contiguous byte-range move, not a per-allocation one.**

```rust
pub struct Step<Address, Size> { pub from: Address, pub to: Address, pub len: Size }
```

It deliberately carries **no id**. An id would be redundant — the backend never needs one
(it just copies `len` bytes `from → to`; it owns no table anymore), and
`commit_compaction_step` recovers it with a single `allocations[from]` lookup. Dropping it
is what lets one `Step` describe a whole **run**: the slide candidate moves the maximal
contiguous run above the largest gap as one transfer, and `commit_compaction_step` shifts
every entry in `[from, from+len)` by `to − from`. Evacuation candidates are the degenerate
`k = 1` case of the same shape.

This costs nothing and is what §4.3's cost model actually wants: the per-operation term
`c₀` is paid once per run rather than once per allocation. Three properties make it work
with no extra machinery:

- **The potential is grouping-invariant.** `Φ` sums over *bytes*, so a run of total size
  `S` sliding by `d` scores `ΔΦ = S·d` — the identical per-byte gain `d` as a single
  allocation. Run and single-allocation candidates are therefore directly comparable.
- **Run discovery is output-sensitive and needs no index.** A run is found by walking
  forward from the first allocation above the gap while
  `next.address == cur.address + cur.len` — O(k) to move k allocations. Crucially the
  algorithm never *searches over* runs (which would need an index that splits on `free` and
  merges on `alloc`); the **gap** is chosen first, and it determines the run above it.
- **Prefix-chunking stays valid.** Taking only the bottom `j` allocations of a run is
  itself a legal move: the gap simply reopens above them. So §5.2's chunking needs no
  special case, and the copy is a downward (overlapping-safe) transfer whenever `g < S`.

**`budget` shapes the choice; it does not hard-cap it.** The budget participates in
candidate *ranking* (§4.3's cost model), but a step may be proposed whose cost exceeds it
when no worthwhile cheaper step exists — otherwise a heap whose only remaining useful move
is one big slide would report quiescence and never compact. The caller can read a step's
cost and decide for itself whether to execute, defer, or chunk it, so the honest contract
is "prefer to stay within `budget`", not "never exceed it". The parameter keeps its value
as long as a real Pareto front on the gain/cost trade-off exists.

**Why distance-greed needs no lookahead — worked example.** Allocations
`E1 = 0..1000`, `E2 = 1020..2000`, `E3 = 2100..2110`, `E4 = 2200..2210`,
`E5 = 2310..2420`, with gaps of 20, 100, 90, 100 between them. The tail `E5` (110 bytes)
fits no gap, so an `end`-greedy compactor stalls or slides — and pure sliding copies
`110 + 120 + 130 + 1100 = 1460` bytes to fully compact (the tail run grows as it
descends). Gain-greedy instead reads the cheap interior moves off the top of the
candidate list:

1. `E4 → 1000` (gain `d = 1200`); its vacated slot coalesces with the gap above it
   into 110 free bytes at `2200`.
2. `E3 → 1010` (gain `1090`, an exact fit into the split remainder — and it outranks
   `E5 → 2200`, gain `110`, which would waste a copy); its vacated slot merges the two
   gaps around it into 200 free bytes at `2000`.
3. `E5 → 2000` (gain `310` — the coalesced gap now fits it); the heap is gapless,
   truncate `2420 → 2110`.

Fully compact, 130 bytes copied, no planning: the "enabling" interior moves were
themselves the highest-gain single moves, because `Φ` credits deferred payoff immediately
— coalescence is a *side effect* of taking them, not a goal needing foresight. (Gain-greed
is still greed: bin-packing hides inside exact-fit choices, so no general optimality claim
— but the "unfortunately-sized tail holds everything hostage" trap is dissolved
structurally, not by luck.)

**Why the potential must weight by size — second example.** Allocations
`E_bulk = 100..2000`, `E_90 = 2000..2090`, `E_100 = 2190..2290` (the tail), with gaps
`0..100` and `2090..2190`. The allocation-*start* potential prices a move at `d/s`, which
ranks `E_90 → 0` (`2000/90 ≈ 22.2`) above `E_100 → 0` (`2190/100 ≈ 21.9`): the smaller
allocation jumps first, *splits* the 100-byte gap down to a useless 10, and strands `E_100` —
the file bottoms out at `end = 2100` after 190 bytes copied, and finishing costs a
2000-byte slide over the sliver. Distance-greed ranks `E_100 → 0` first (`2190 > 2000`):
the tail lands in the exact-fitting gap, the suffix clears, and the file is fully compact
at `end = 2090` after copying exactly 100 bytes. The general lesson: `d/s` divides by
size and so systematically demotes large movers — but large allocations are precisely the
ones that need large gaps, the scarce resource, and bin-packing folklore (first-fit
*decreasing*) says to serve them first. The by-byte `Φ` removes the bias, and as a bonus
keeps per-byte gain uniform within a move, so budget-chunked moves account cleanly.

Note what returned: this policy consumes *pairs* (a class's best mover, its best gap), so
`live_by_size` is core — the instinct to track the free/live intersection dynamically
survives the correction, in weighted (gain-ordered) form, with the small class count
keeping its maintenance trivial.

Splitting (step 1) makes "combination fits" fall out for free: a gap of size `3s` receives
an `s`-allocation, and the `2s` remainder re-enters `free_by_size`, ready for the next mover.
Full bin-packing of combinations is NP-hard and not worth chasing; greedy splitting
captures the realistic case.

Cost accounting: all indexes together hold one entry per allocation — O(live + gaps) — a
constant factor on the table the stable-id constraint already forces into memory (P1).

### 4.1 Finding the best candidate without a full class scan (deferred)

**Not in the first implementation.** The naive all-classes iteration of step 2 ships first;
this section lands afterwards as its own commit, so it stays revertible if the added
machinery does not earn its keep.

The obvious upgrade for a large class count — a priority queue over classes keyed by
last-computed gain, lazily revalidated on pop — is subtly **incorrect**. Gains go stale in
both directions, and only one direction is benign. Stale-*high* keys are fixed by
recomputing on pop; stale-*low* keys are not: when a `free` opens a deep gap, the true
gains of *every* class with size ≤ that gap rise at once, their stored keys do not, and a
max-queue can sit on the globally best move indefinitely. Updating the affected classes
eagerly is a range update over sizes — exactly the scan the queue was meant to avoid.

The fix is to key classes not by their (globally volatile) gains but by a bound that is
*class-locally maintainable*: a mover's gain never exceeds its own address
(`d = a − dest ≤ a`, since `dest ≥ 0`), so

    gain(s) ≤ top(s)        (widened by the bonus cap `2α/s` under §4.2's term)

where `top(s)`, the class's highest member, changes only on alloc/free/move of that
class's own members — never through gap events. Keep one ordered map
`tops: BTreeMap<Address, Size>` (top-member address → class), updated by the same
class-local hooks that maintain `live_by_size`. Then search branch-and-bound style:

1. Walk `tops` in descending address order; for each class, compute the true gain (the
   destination queries of §4, or §4.2's variants).
2. Track the best true gain `B` found; **stop as soon as the next class's `top ≤ B`** —
   no unvisited class can beat `B`, because gain ≤ top.

When a good move exists high in the file — the common case during active compaction — `B`
is large after a class or two and the scan stops immediately. It degenerates toward
O(#classes) only when *all* gains are small, i.e. near quiescence, where compaction has
little left to do anyway. Bound even that with a visit cap: if the cap fires before the
stop condition, execute the best candidate found *so far* and let the next step
resume the walk from a cursor instead of re-walking the prefix. Two provisos: the cursor
belongs to the capped regime only — a scan that terminated via the bound found the exact
best, and the next scan should restart from the top (the executed move often makes the
same top classes the best again) — and cursor invalidation cannot be made exact cheaply:
a `free` *anywhere* can drop below a visited class's previous destination, an alloc can
mint a member above a visited top, and the compactor's own vacated ranges can hand a
destination-less class its first candidate. So invalidate on any foreground op, keep the
cursor only across consecutive compactor-driven steps, and accept it as best-effort: a
stale cursor merely misorders positive-gain moves (`Φ` still strictly decreases), and the
capped regime is exactly where all gains are small and misordering is cheapest. One fresh
from-the-top scan before declaring quiescence restores exactness. (The slide candidate
stays a single separate O(log n) lookup — largest gap — outside the scan.)

### 4.2 A fragmentation term in the potential (deferred)

**Not in the first implementation either.** Ship the plain potential (`α = 0`); the `α·G`
term below lands as its own revertible commit afterwards. When it does, **`α` is a field
of the heap with a getter and a setter — never a parameter of a trait method**: the trait
models the *capability* to compact, and must not leak the tuning parameters of whichever
policy a particular implementation happens to use.

So far exact fits earn only a tie-break, which leaves one real failure mode: "lowest gap
that fits" maximizes gain but can squander a large gap on a small far-travelling allocation,
splitting it and stranding the large allocation that needed it. No lookahead-free gain ordering
avoids this (it hits the by-byte and by-start potentials alike). A **structural term** is
the principled soft mitigation — it reroutes the small mover to an exact fit when one exists
at comparable depth, though when none exists the large gap still gets split. With `G` = the
number of gaps and a tuning knob `α ≥ 0`,

    Φ_α = Φ + α·G.

Unbiasedness survives: every gapless layout has `G = 0`, so the minimum is untouched and
still order-free; the excess becomes `inversions + α·G`, still zero iff compact. A move's
per-byte gain becomes

    gain = d + α·r/s,      r = r_src + r_dest ∈ {−1, 0, +1, +2},

where vacating an allocation flanked by two gaps merges them (`r_src = +1`: the "plug"
extraction), one gap neighbor is neutral, two live neighbors mint a new gap
(`r_src = −1`), and an exact-fit destination erases one (`r_dest = +1`). Three notes:

- **Source triage is new information.** The base `Φ` is source-agnostic; the α-term is
  the first thing that prices *where a move takes from* — "extract the plug between two
  gaps" now outranks "carve a hole out of a solid run" at equal distance. (In the first
  worked example above, `E3` and `E4` are both plugs: the term reinforces exactly the
  moves distance-greed already took.)
- **The structural prize is per-move, so per-byte it scales as `1/s`** — small plugs are
  the cheap structural wins. That is correct accounting: the same `+1` costs 10 copied
  bytes via a 10-byte plug and 1000 via a 1000-byte allocation.
- **Termination is safe for every `α`**: a *maximal* run is always flanked by free space,
  so a full-run slide always merges two gaps (or truncates) — `r ≥ +1`, gain strictly
  positive — so positive-gain moves never run out before compactness.

Greedy stays **exact** and nearly as cheap. Destinations: for a fixed mover, among
non-exact gaps the lowest maximizes `d`, and among exact gaps likewise — so *two*
candidates provably suffice: the lowest fitting gap and the lowest exact one
(`free_by_size[s]`'s first). Movers: within a class the best member now maximizes
`a + α·r_src/s`, so a single per-class top no longer suffices — keep the top member per
*neighbor category* (gap|gap, gap|live, live|live), three sub-maxima maintained by the
O(1)-neighbor updates each foreground op performs anyway.

**No smoothing.** A saturating variant (`Φ + α·Σ f(g)` with `f(g) = g/(β+g)`, rewarding
*almost*-exact fits and discounting sub-`β` slivers) is deliberately **not** part of this
design. Its whole motivation was that byte-granular exact-size coincidences are rare — but
that premise is wrong here: `live_by_size` holds only fixed-size allocations, and kladde is
built end to end to mint many allocations at few identical sizes, so exact fits should be the
common case rather than a lucky one. Smoothing would buy a snugness dial for exactly the
population (resizable, one-off sizes) that generates no exact-fit candidates anyway, at
the price of losing exact greed and adding a size-window query.

All of this is a pure change of *scoring* (P4): the mechanism and indexes barely move
(three per-class sub-maxima). Ship `α = 0`; add the count term when traces show
squandered-gap moves.

### 4.3 A more realistic cost model

"Cost = bytes copied" is only half-true on a real OS/hardware stack. A contiguous
transfer costs roughly **`c₀ + c₁·s`**: a fixed per-operation term — syscall, page-cache
work, and 4K page granularity (a small write to a cold page is a read-modify-write of the
whole page; any move's unaligned edges RMW their boundary pages) — plus a throughput term
per byte. On SSDs `c₀` is tens of microseconds; on HDDs, milliseconds of seek that
dominate small transfers. Travel distance `d` is essentially free (no seek-distance term
worth modeling on SSDs, a mild one on HDDs), and slides are *sequential* I/O where
evacuations are random — representable as a larger `c₀` for evacuations, not a new cost
shape.

Under the affine cost, the objective becomes

    gain/cost = (s·d + α·r) / (c₀ + c₁·s):

`≈ d/c₁` for `s ≫ c₀/c₁` (nothing changes for bulk moves), while tiny moves are
discounted by their fixed overhead — a healthy counterweight to §4.2's small-plug
favoritism: a 10-byte plug that costs a full-page RMW is no longer priced as 10 bytes.

Greedy difficulty is unchanged *in kind*, because the affine cost keeps the two
properties the machinery relies on: it is **class-uniform** (a function of `s` only, so
the best mover within a class is still the same sub-maxima) and
**destination-independent** (so the 2–3 destination candidates per mover still suffice).
The only real change is §4.1's key: the class-local bound becomes
`b(s) = (s·top(s) + 2α)/(c₀ + c₁·s)` — still a function of `s` and `top(s)` only — and
the `tops` map is ordered by `b(s)` instead of raw top address. One formula, one sort
key. More generally: the *potential* governs soundness (any positive-gain move decreases
`Φ`, under any cost model), the cost model only reshapes the ordering — and the one cost
feature that would genuinely complicate the greedy is destination-*dependence* (real
seek-distance costs), which would turn destination choice into a continuous two-term
trade-off admitting only bounded-loss approximate greed rather than the exact greed the
current candidate sets give.

## 5. Architecture sketches

### Sketch A (chosen): a `RelocatableHeap` trait; the backend shrinks

The structure of §4 *is* the component. It owns geometry (addresses, sizes, free space,
compaction) end to end and reads exactly one bit of an otherwise opaque id. It is a
**trait**, so a backend can be generic over heap implementations that do or do not compact:

```rust
/// What the heap needs to know about an id it never mints: whether the allocation it
/// names is fixed-size. See below for why this rides on the id.
pub trait AllocationId: Copy + Eq + Hash {
    fn is_fixed_size(&self) -> bool;
}

/// Geometry: what lives where, where the free space is, and how to shrink it.
/// Ids are minted by the caller (P5); the heap only ever asks them one question.
pub trait RelocatableHeap {
    type Id: AllocationId;
    type Address: Word;
    type Size: Word + Into<Self::Address>;

    fn alloc(&mut self, id: Self::Id, size: Self::Size)
        -> Result<Self::Address, AllocError>;
    fn free(&mut self, id: Self::Id) -> Result<(), AllocError>;
    /// `Some((old, new))` iff the bytes must be moved by the caller.
    fn resize(&mut self, id: Self::Id, new_size: Self::Size)
        -> Result<Option<(Self::Address, Self::Address)>, AllocError>;
    fn lookup(&self, id: Self::Id) -> Option<(Self::Address, Self::Size)>;

    fn len(&self) -> Self::Address;            // current end (file size)
    fn live_bytes(&self) -> Self::Address;     // == len() when compact
    fn iter(&self) -> impl Iterator<Item = (Self::Id, Self::Address, Self::Size)>;

    /// Propose the next incremental compaction step, preferring one that costs at most
    /// `budget`. `None` ⇔ compact, or nothing left worth doing.
    ///
    /// May return a step costing **more** than `budget` when no worthwhile cheaper step
    /// exists (see §4): the caller can price the returned step itself and decide whether
    /// to execute, chunk, or drop it. Default: `None` — this heap does not compact.
    fn propose_compaction_step(&self, _budget: Self::Size)
        -> Option<Step<Self::Address, Self::Size>> { None }

    /// Apply a step previously obtained from `propose_compaction_step`, re-keying every
    /// allocation in the moved range. Moving the actual bytes is the caller's job.
    fn commit_compaction_step(&mut self, _step: Step<Self::Address, Self::Size>) {
        unreachable!("commit_compaction_step called on a heap that proposes no steps")
    }
}

/// Marker: this heap's `propose_compaction_step` really proposes steps. Compare
/// `ExactSizeIterator`, which marks `size_hint` as meaning something.
pub trait IncrementallyCompactableHeap: RelocatableHeap {}
```

`Step` (§4) stays a concrete struct rather than an associated type: the backend must be
able to *serialize* it — a step is just another journal record — so an
implementation-private step payload would have to be re-exposed anyway.

**This shape does deliver the gating you want.** A generic backend takes `H: RelocatableHeap`
and can call `propose_compaction_step` unconditionally in its flush path (a non-compacting
heap answers `None`, which monomorphizes to nothing); user-facing controls then go in a
*second* inherent impl block that is bounded on the marker, so they simply do not exist for
non-compacting heaps:

```rust
impl<S, H: RelocatableHeap> Backend<S, H> { /* alloc, free, flush, … */ }

impl<S, H: IncrementallyCompactableHeap> Backend<S, H> {
    pub fn compact_incrementally(&mut self, budget: H::Size) -> CompactionProgress { … }
}
```

This is also why the two methods carry defaults on the *base* trait rather than living on
the subtrait: putting them on the subtrait would make the shared flush path uncallable
without the bound, forcing either duplicate flush implementations or specialization. The
naming keeps "incremental" explicit throughout, leaving room for a later
`compact_fully` / `FullyCompactableHeap` with an algorithm optimized for the
stop-the-world case.

The concrete implementation of §4 — `allocations` plus the three derived indexes and the
gain-greedy policy — is **`GainGreedyHeap`**, named for the policy rather than the
structure, since a future sibling implementation would differ exactly there.

#### Why sizedness rides on the id (and `Meta` is gone)

`live_by_size` holds only fixed-size allocations (§4), so the heap *must* be able to ask "is
this allocation fixed-size?". An opaque `Meta` associated type cannot answer that, so `Meta`
would have needed a trait bound leaking exactly that one bit — at which point it is
carrying nothing else, since sizedness is the only per-allocation fact the heap has ever
needed. So `Meta` is **removed**, and the bit moves onto the id via the `AllocationId` bound.

**Implementing it on `Pointer<W>`: one dedicated flag bit of `W`.** The existing types were
already built for this — `Pointer`'s field is private precisely to buy "representation
independence, e.g. later packing a fixed/resizable flag bit into the id without touching
call sites", and `Word` already exposes shifts, `BitAnd`/`BitOr`, and `one()` as "a base
for building flag masks". So:

```rust
// raw = (counter << 1) | fixed_bit,  counter >= 1
impl<W: Word> AllocationId for Pointer<W> {
    fn is_fixed_size(&self) -> bool {
        self.raw() & W::from_nonzero(W::one()) != W::zero()
    }
}
```

**Use the low bit, not the high bit.** Both keep the nonzero invariant intact as long as
the counter starts at 1, but they differ where it matters: with a high-bit flag the two
sizedness classes occupy disjoint halves of the numeric range, so every fixed-size id is a
huge number — which costs a full-width encoding under the varint representation kladde
already has a crate for, and wrecks any on-file table that wants ids to stay dense. With a
low-bit flag, `raw ≈ 2·counter` stays small and dense, and a positional/dense on-file table
just indexes by `raw >> 1`. The counter pool stays **shared** across both sizednesses so
that `raw >> 1` remains unique. The price is half the id space (≈2.1 billion ids at
`W = u32`), which is not a real constraint.

Two consequences worth being explicit about:

- **Sizedness becomes free to query, everywhere.** Neither the heap nor the backend needs
  to store it: `Backend::resolve`, which today reads the table to decide which handle type
  to rebuild, can answer from the id alone with no lookup at all. This is what makes
  dropping `Meta` a simplification rather than a relocation of the same cost.
- **The flag is immutable for the life of an id, so sizedness conversion must re-mint.**
  `make_resizable`/`make_fixed_size` currently keep the same id and merely re-tag the
  table; with the bit inside the id they must instead free the old id and mint a new one.
  That is expressible — the conversion already consumes a `UniquePointer*` handle and
  returns the other kind, so the single owner is handed the new id by the type system —
  but it is a genuine semantic change: any `Pointer` already copied into a *persisted*
  structure before conversion would dangle. Worth confirming the single-owner discipline
  really holds at every `make_*` call site before relying on it.

The backend keeps exactly what P5 assigns it: **id minting/recycling** (it calls
`heap.alloc(id, ..)` with an id it chose, sizedness bit already set — the caller of
`alloc_fixed_size`/`alloc_resizable` knows the sizedness, so nothing new has to be
threaded through), **storage I/O** (it executes `Step`s by copying
bytes), **journaling** (a `Step` is a journal record; the persisted table entry is only
updated after the persisted copy, so an interrupted move is abandoned at zero cost — P6),
and **the persistent table layout** (serialized from `iter()`, deserialized by replaying
`alloc`s). There is no separate in-memory id table in the backend anymore — `by_id` *is*
it, held once (P1).

Note that P6's "copy first, then flip" is a constraint on the **journal replay order**, not
on the order of the two heap calls: under the per-flush schedule (§5.1) the in-memory heap
is deliberately advanced *ahead* of the store, and `commit_compaction_step` is pure
bookkeeping whose durability is carried by the journal record.

What this gives up: the "textbook thin allocator" as a standalone reusable piece. What it
actually becomes is arguably a more coherent reusable product: *a relocatable heap with
stable handles* — the Mac-Memory-Manager shape — rather than a bare free-list that cannot
compact without a chaperone.

### Sketch B (rejected): thin allocator + compactor fed by backend notifications

Keep the free-space-only allocator; add a compactor that maintains `live_by_size` from
backend callbacks on every alloc/free/resize/move. This is the design the symptoms pointed
at: `live_by_size` mirrors the id table through an interface (double bookkeeping, two
sources of truth to keep consistent), the actionable test needs state from both siblings, so
either one polls the other (recompute — violates P2/P3) or they exchange notifications
(coupling that is the merge of Sketch A, but with extra steps). Named here mainly to record
*why* it loses.

### Sketch C: Sketch A plus an explicit policy object

Same mechanism; `propose_compaction_step` delegates to a `CompactionPolicy` trait reading
the indexes (gain thresholds, exact-fit preferences, "only when `len > 2 × live_bytes`"
hysteresis, never-move-pinned…). This is not an alternative but the natural second story on
Sketch A once two policies actually exist; premature before then (P4 says keep the seam in
mind, not build the trait now). Note that the `RelocatableHeap`/`IncrementallyCompactableHeap`
split already provides the coarse version of this seam: a second policy can simply be a
second implementing struct alongside `GainGreedyHeap`.

### 5.1 When steps run

**Per flush.** The backend runs `compact_incrementally` up to a fixed budget on each
journal flush, terminating early as soon as `propose_compaction_step` returns `None`. That
is deliberately the naive schedule; the sketch of a smarter one — replaying the journal
onto the heap first, appending the resulting steps to the journal, and then optimizing the
combined record before it reaches the store — is recorded under "Incremental compaction"
in [`later.md`](later.md).

### 5.2 Chunking large moves

A move whose bytes exceed what one step should copy is handled by **capping and chunking**,
not by a `moving` flag with write-redirection. A long run slides a prefix at a time — which
§4 already establishes is a legal move in its own right — so each chunk commits its own
table-visible progress, and the tearing window is one chunk wide. A single allocation
larger than the budget is the residual case: it moves whole, or its copy is chunked
front-to-back with the address flipped once at the end. Either way an interrupted move is
covered by the journal. This is what makes `budget` a ranking input rather than a hard cap
(§4): the heap may still propose an over-budget step when nothing cheaper is worth doing,
and the caller decides whether to run it, chunk it, or skip it.

## 6. What survives from the earlier design note

- **Survives**: ids are backend-chosen (minted by the caller, and the heap reads only the
  one sizedness bit of them); sizedness as an input the manager exploits for placement —
  though it now arrives *through* the id rather than as a separate parameter; run-granular
  slides derived from geometry; `Relocation`-free `resize` reporting; the journal's
  mint-early/claim-at-flush pattern (`claim` becomes `heap.alloc(id, ..)` at flush).
- **Falls**: "the allocator holds no per-allocation state." Its justification was that real
  allocators get sizes from in-band headers or tracing, which kladde lacks — so the table
  had to live *somewhere else*. That argument silently assumed the table is a cost to be
  quarantined. Under the stable-id constraint it is a *mandatory* structure (P1), and the
  moment compaction must be incremental, the component choosing moves needs joint,
  index-grade access to both sides of the intersection (P3). Quarantining the table then
  produces exactly the bolted-on shapes (callback queries, mirrored indexes) that motivated
  this restart.
- **Reframed**: `CompactingAllocator::plan_compaction() → Vec<Move>` (batch, whole-heap)
  becomes `propose_compaction_step`/`commit_compaction_step` (pull, bounded). A full
  compaction is a loop, not an API — for now; the "incremental" in the names leaves room
  for a dedicated full-compaction algorithm later.

## 7. Implementation order

Each item is its own commit, so the two deferred refinements stay revertible if they turn
out not to earn their keep.

1. **Core.** The `AllocationId` bound plus its `Pointer<W>` impl (low-bit sizedness flag, and
   the id-pool change to shift the counter), then the `RelocatableHeap` +
   `IncrementallyCompactableHeap` traits and `GainGreedyHeap`: `allocations`, the three
   derived indexes (`live_by_size` restricted to fixed-size allocations), gain-greedy
   `propose_compaction_step`/`commit_compaction_step` with the plain potential (`α = 0`)
   and the naive all-classes iteration.
2. **Backend integration.** A generic backend over `RelocatableHeap`, compacting per flush
   up to a budget (§5.1), with `compact_incrementally` exposed only under the marker bound.
3. **Candidate scan (§4.1).** The `tops` map and the branch-and-bound walk, replacing the
   all-classes iteration.
4. **Fragmentation term (§4.2).** `Φ + α·G`, with `α` a field of `GainGreedyHeap` behind a
   getter/setter.
5. **Augmented gap tree (§4).** `GapTree` behind "the lowest gap that fits", replacing the
   `free_by_size.range(s..)` scan, which stays as the tests' oracle.

Still genuinely open: how to tune `α`. (Whether the augmented tree earns its place is
settled — see the measurement in §4.)

## References

- Mesh: compacting `malloc` without moving pointers —
  [Powers, Tench, Berger, McGregor, PLDI 2019](https://arxiv.org/abs/1902.04738).
- Immix / defragmenting mark-region collection —
  [Blackburn & McKinley, PLDI 2008](https://www.steveblackburn.org/pubs/papers/immix-pldi-2008.pdf).
- Incremental copying collection — [Baker 1978](https://doi.org/10.1145/359460.359470).
- The Compressor (single-pass, pause-bounded compaction) —
  [Kermany & Petrank, PLDI 2006](https://doi.org/10.1145/1133981.1134023).
- Compaction algorithms survey: Jones, Hosking & Moss,
  [*The Garbage Collection Handbook*](https://gchandbook.org) (2nd ed.), ch. 3 & 17
  (mark-compact; concurrent compaction).
- Handle-table prior art: the
  [Macintosh Memory Manager](https://developer.apple.com/library/archive/documentation/mac/pdf/Memory/Intro_to_Mem_Mgmt.pdf)
  (master pointers = the id table; compaction slides relocatable blocks).
