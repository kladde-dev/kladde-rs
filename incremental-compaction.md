# Incremental compaction, from a clean slate

Status: design exploration. Deliberately *not* anchored to the current `Allocator`/`Backend`
split from [address-ranges-id-pool-decoupling.md](address-ranges-id-pool-decoupling.md);
§6 discusses which conclusions of that note survive and which fall.

The one fixed constraint: **pointers are stable ids**, so an in-memory table of all live
allocations — `id → (address, size, meta)` — exists no matter what. Everything below treats
that table as given and asks what the cleanest incremental memory manager around it looks
like.

## 1. Problem statement

Maintain a partition of the address space `[0, end)` into **live extents** (each tagged with
an opaque token = the stable id, plus meta such as sizedness) and **free extents** (gaps).
Support:

- `alloc(size, meta) → address`, `free(address..)`, `resize(..) → Option<new address>` —
  the foreground operations, each O(log n) CPU, no byte movement except resize-relocation.
- `compact_step(budget) → progress` — a background operation, callable arbitrarily often,
  each call bounded by `budget` bytes copied and O(log n) CPU **including candidate
  discovery**, such that repeated calls converge to a compact heap (`end` = total live
  bytes). No stop-the-world phase, ever; a full compaction is just `compact_step` in a loop
  that something is allowed to interrupt.

Progress can be made precise with a potential function over live **bytes**, not extents:
`Φ = Σ_{live bytes b} address(b)`
(`= Σ_{live extents a} size(a)·(address(a) + (size(a)−1)/2)`). With `L` total live bytes,
`Φ ≥ L(L−1)/2`, with equality for **every** gapless layout regardless of extent order; the
excess `Φ − L(L−1)/2` counts exactly the (free byte, live byte) pairs where the free byte
sits *below* the live byte — the heap's "inversions" — so minimal `Φ` ⟺ compact, and the
excess doubles as a fragmentation-debt metric. (The simpler-looking variant that sums
extent *start* addresses is subtly wrong: its minimum depends on extent order — it insists
small extents come first — and the bias this induces in the greedy policy is actively
harmful; see the second worked example in §4.) Every useful compaction step strictly
decreases `Φ`. `end` itself shrinks whenever a step (or a `free`) clears the suffix — so
`Φ` is the *progress* measure and `end` the *payoff*, which lags it.

Every step relocates live bytes downward, and one lens prices all of them: relocating an
extent of size `s` down by a distance `d` copies `s` bytes and makes `ΔΦ = s·d` progress —
per byte copied, **gain `d`**, the travel distance. Two familiar shapes are extremes:

- **Slide**: the packed run directly above a gap of size `g` shifts down by `g`. Every
  byte in the run travels `d = g`, so the gain is the **gap size** — independent of what
  the run contains, and poor whenever the gap is small.
- **Evacuation**: a single extent jumps from near the tail into a gap far below it; the
  gain is its full travel distance, possibly `d ≫ g` for every gap `g`. The extreme case
  is a few small extents stranded above a large free region — copying a handful of bytes
  releases a huge suffix. That is the most valuable move a compactor ever gets, and it is
  precisely the state a file lands in after a burst of frees below a few survivors.

**Exact fit is about fragmentation, not gain.** Placing an extent into a gap of exactly
its size leaves no remainder sliver and lets the vacated range coalesce fully. But among
candidate destinations it is a *tie-break*, not a gate: a deep, larger-than-needed gap
(large `d`) beats a shallow exact one on progress, and the split remainder is a tracked
gap like any other.

A warning the gain lens makes precise: **greed in `end` is myopic**. `end` is a
discontinuous payoff — a single unfortunately-sized tail extent defers all of it, while
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
  table, once for a shadow of it. Corollary: the free-space index and the live-extent index
  should be *views of one structure*, not sibling structs that mirror each other through an
  interface.
- **P2 — Incremental means incremental discovery.** Bounding the bytes copied per step is
  worthless if finding the step scans O(heap). The actionable-move set must be maintained
  by the foreground ops (`alloc`/`free`/`resize` each touch O(1) size classes) so that
  `compact_step` starts from a ready answer.
- **P3 — Discovery must be a query, not a scan.** What compaction consumes is neither
  "gaps" nor "movable extents" but *pairs* (mover, destination), weighted by gain. The
  candidate pairs must come from maintained per-class indexes and O(log n) queries (§4
  evaluates one candidate per size class this way — cheap because the class count is
  small), never from rescanning extents. Quiescence ("nothing worth moving") must be
  equally cheap to detect.
- **P4 — Mechanism/policy split.** The mechanism is "move extent X into gap Y, update
  indexes, report the move." Which pair to pick, when to slide instead, when to stop — that
  is policy, and it should be swappable without touching the index maintenance.
- **P5 — Persistence concerns stay out.** Id *minting* policy and the on-file table layout
  remain coupled to the persistent format (that argument from the earlier note stands).
  The memory manager handles tokens opaquely; it never chooses them and never serializes.
- **P6 — Crash safety by construction.** A move copies into *free* space and then flips
  one table entry. Until the flip, the copy is invisible; after it, the old bytes are
  garbage. The flip is the sole commit point, so incremental moves are atomic and an
  interrupted move is abandoned at zero cost. (This falls out of the id-table indirection;
  it is worth preserving in whatever design wins.)

## 4. The core structure: one partition, derived indexes, gain-greedy steps

Single source of truth — the partition of the address space:

```rust
// Invariant: contiguous cover of [0, end), no two adjacent Free extents.
extents: BTreeMap<Address, Extent>       // keyed by start address
enum Extent { Live { len, token, meta }, Free { len } }
```

Secondary indexes, all derivable from `extents`, all maintained by the same handful of
mutation paths (`alloc`, `free`, `resize`, `apply move`):

```rust
by_token:  HashMap<Token, Address>              // id resolution (this IS the id table's
                                                // address column; the sunk cost, exploited)
free_by_size: BTreeMap<Size, BTreeSet<Address>> // gaps, grouped by exact size
live_by_size: BTreeMap<Size, BTreeSet<Address>> // movers: a class's best candidate is its
                                                // highest-addressed member
```

`BTreeSet<Address>` per class rather than a binary heap: min *and* max are O(log), and —
unlike a heap — arbitrary deletion is native, which matters because gaps are consumed by
ordinary `alloc` and extents die by ordinary `free`, not only by compaction. (This subsumes
the per-class min-heap idea; the heap's O(1) peek isn't worth losing cheap deletion.)

The destination lookup the corrected gain analysis (§1) demands is one query the per-class
sets cannot answer cheaply: **the lowest gap with `len ≥ s`** — a 2-D dominance query
(minimize address subject to a size bound). The textbook structure is an address-ordered
balanced tree augmented with each subtree's *maximum gap length*: descend leftmost-first
into any subtree whose max is `≥ s`, O(log n). `std` has no augmented `BTreeMap`, so this
is a small bespoke tree — or, initially, a scan over `free_by_size.range(s..)` classes
accepted as a stopgap until measured.

`compact_step(budget)` — **gain-greedy**:

1. Candidate generation, one per size class `s` that has live members: the class's best
   mover is its **highest-addressed member** (`live_by_size[s].last()` — within a class
   the destination is shared, so the highest member maximizes `d`). Its destination is
   the exact-fit fast path (`free_by_size[s]`'s lowest address) or the augmented-tree
   query (lowest gap `≥ s`, split on use); its gain is its travel distance `d`. Add one
   *slide* candidate — the run above the **largest** gap, gain `g` — for the regime where
   no fit exists anywhere.
2. Pick the best candidate. Under kladde's design bet — many allocations of *few* distinct
   sizes (the `Sizedness::Fixed` classes) plus a handful of one-off resizable sizes — the
   class count is small, so evaluating every class is O(#classes · log n) per step with no
   incremental machinery. If class counts ever grow, the upgrade path is the bounded
   best-candidate scan of §4.1 (a naively gain-keyed priority queue is subtly broken —
   see there).
3. Execute: copy `s` bytes (chunked against `budget`), flip the table entry, update the
   indexes; the vacated range coalesces with its free neighbors (touching O(1) classes).
4. If the suffix of the address space is now free, retreat `end`. Repeat while budget
   remains; quiesce when no candidate's gain clears a policy threshold.

**Why distance-greed needs no lookahead — worked example.** Live extents
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

**Why the potential must weight by size — second example.** Extents
`E_bulk = 100..2000`, `E_90 = 2000..2090`, `E_100 = 2190..2290` (the tail), with gaps
`0..100` and `2090..2190`. The extent-*start* potential prices a move at `d/s`, which
ranks `E_90 → 0` (`2000/90 ≈ 22.2`) above `E_100 → 0` (`2190/100 ≈ 21.9`): the smaller
extent jumps first, *splits* the 100-byte gap down to a useless 10, and strands `E_100` —
the file bottoms out at `end = 2100` after 190 bytes copied, and finishing costs a
2000-byte slide over the sliver. Distance-greed ranks `E_100 → 0` first (`2190 > 2000`):
the tail lands in the exact-fitting gap, the suffix clears, and the file is fully compact
at `end = 2090` after copying exactly 100 bytes. The general lesson: `d/s` divides by
size and so systematically demotes large movers — but large extents are precisely the
ones that need large gaps, the scarce resource, and bin-packing folklore (first-fit
*decreasing*) says to serve them first. The by-byte `Φ` removes the bias, and as a bonus
keeps per-byte gain uniform within a move, so budget-chunked moves account cleanly.

Note what returned: this policy consumes *pairs* (a class's best mover, its best gap), so
`live_by_size` is core — the instinct to track the free/live intersection dynamically
survives the correction, in weighted (gain-ordered) form, with the small class count
keeping its maintenance trivial.

Splitting (step 1) makes "combination fits" fall out for free: a gap of size `3s` receives
an `s`-extent, and the `2s` remainder re-enters `free_by_size`, ready for the next mover.
Full bin-packing of combinations is NP-hard and not worth chasing; greedy splitting
captures the realistic case.

Cost accounting: all indexes together hold one entry per extent — O(live + gaps) — a
constant factor on the table the stable-id constraint already forces into memory (P1).

### 4.1 Finding the best candidate without a full class scan

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
stop condition, execute the best candidate found *so far* and let the next `compact_step`
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

### 4.2 A fragmentation term in the potential

So far exact fits earn only a tie-break, and on a byte-granular address space exact size
coincidences are rare outside the fixed classes. A **structural term** makes the trade-off
soft: with `G` = the number of gaps and a tuning knob `α ≥ 0`,

    Φ_α = Φ + α·G.

Unbiasedness survives: every gapless layout has `G = 0`, so the minimum is untouched and
still order-free; the excess becomes `inversions + α·G`, still zero iff compact. A move's
per-byte gain becomes

    gain = d + α·r/s,      r = r_src + r_dest ∈ {−1, 0, +1, +2},

where vacating an extent flanked by two gaps merges them (`r_src = +1`: the "plug"
extraction), one gap neighbor is neutral, two live neighbors mint a new gap
(`r_src = −1`), and an exact-fit destination erases one (`r_dest = +1`). Three notes:

- **Source triage is new information.** The base `Φ` is source-agnostic; the α-term is
  the first thing that prices *where a move takes from* — "extract the plug between two
  gaps" now outranks "carve a hole out of a solid run" at equal distance. (In the first
  worked example above, `E3` and `E4` are both plugs: the term reinforces exactly the
  moves distance-greed already took.)
- **The structural prize is per-move, so per-byte it scales as `1/s`** — small plugs are
  the cheap structural wins. That is correct accounting: the same `+1` costs 10 copied
  bytes via a 10-byte plug and 1000 via a 1000-byte extent.
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

**Smoothing: rewarding almost-exact fits.** Replace the count with a saturating sum,

    Φ_αβ = Φ + α·Σ_gaps f(g),      f(g) = g/(β+g):

gaps well above `β` cost ≈ `α` as before; slivers below `β` fade out. Filling a gap `g`
with `s` bytes leaves remainder `ρ = g − s` and earns `α·(f(g) − f(ρ))` ≈ `α·(1 − ρ/β)`
for `ρ ≪ β ≪ g` — the *snugness dial* the count version lacks, and the version that
actually fires for one-off (resizable) sizes, where exact fits essentially never occur.
Concavity caps the destination bonus: `f(g) − f(g−s) ≤ f(s)`, so per byte it is at most
`α/(β+s)`. Two honest observations:

- **The reward and its price are the same coin**: crediting a near-exact fill *is*
  discounting the sub-`β` sliver it leaves — the two are inseparable in this functional
  form, and the form is self-consistent about it: it declares slivers below `β` an
  acceptable price for killing gaps. Their byte-inversions stay charged by the base `Φ`,
  and under kladde's churn they tend to heal (a sliver plus a freed same-class neighbor
  is a usable hole again). So `β` means: *the sliver size worth stranding per fill* —
  keep it small relative to the common class sizes.
- **Exact greed is lost, boundedly.** The destination trade-off (depth vs. snugness)
  becomes continuous, so no fixed candidate set is provably sufficient. The practical
  scheme evaluates three destinations — lowest fitting, lowest exact, and lowest with
  remainder `≤ β` (one extra size-window query: a scan of `free_by_size.range(s..=s+β)`,
  or a second, size-keyed augmented tree) — with per-move suboptimality below the bonus
  cap `α/(β+s)`. Bounded-loss approximate greed, not exact greed.

All of this is a pure change of *scoring* (P4): the mechanism and indexes barely move
(three per-class sub-maxima; for the smooth form, one size-window query). Ship `α = 0`;
add the count term when traces show squandered-gap moves; smooth it when one-off sizes
dominate compaction traffic.

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
trade-off, handled the same bounded-loss way as §4.2's smooth term.

## 5. Architecture sketches

### Sketch A (recommended): a unified `RelocatableHeap`; the backend shrinks

The structure of §4 *is* the component. It owns geometry (addresses, sizes, free space,
compaction) end to end and treats `Token` and `Meta` as opaque:

```rust
struct RelocatableHeap<Token, Meta> { /* §4 fields */ }

impl RelocatableHeap {
    fn alloc(&mut self, tok: Token, size: Size, meta: Meta) -> Result<Address, AllocError>;
    fn free(&mut self, tok: Token) -> Result<(), Error>;
    fn resize(&mut self, tok: Token, new: Size) -> Result<Option<(Address, Address)>, Error>;
    fn address_of(&self, tok: Token) -> Option<(Address, Size, &Meta)>;

    /// Propose the next move within `budget`; None ⇔ compact (or nothing worth doing).
    fn propose_step(&self, budget: Size) -> Option<Step<Token>>;
    /// Called after the caller has copied the bytes (P6: copy first, then flip).
    fn commit_step(&mut self, step: Step<Token>);

    fn len(&self) -> Address;            // current end (file size)
    fn live_bytes(&self) -> Address;     // == len() when compact
    fn iter(&self) -> impl Iterator<Item = (Token, Address, Size, &Meta)>;  // for snapshots
}
```

The backend keeps exactly what P5 assigns it: **id minting/recycling** (it calls
`heap.alloc(id, ..)` with an id it chose), **storage I/O** (it executes `Step`s by copying
bytes, then `commit_step`), **journaling** (a `Step` is just another journal record; replay
is idempotent because the flip is the commit point), and **the persistent table layout**
(serialized from `iter()`, deserialized by replaying `alloc`s). There is no separate
in-memory id table in the backend anymore — `by_token` *is* it, held once (P1).

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

Same mechanism; `propose_step` delegates to a `CompactionPolicy` trait reading the indexes
(gain thresholds, exact-fit preferences, "only when `len > 2 × live_bytes`" hysteresis,
never-move-pinned…). This is not an alternative but the natural second story on
Sketch A once two policies actually exist; premature before then (P4 says keep the seam in
mind, not build the trait now).

## 6. What survives from the earlier design note

- **Survives**: ids are backend-chosen (minted by the caller, opaque tokens to the heap);
  sizedness/meta as a per-call input the manager may exploit for placement; run-granular
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
  becomes `propose_step`/`commit_step` (pull, bounded). A full compaction is a loop, not an
  API.

## 7. Open questions

- **Large-extent moves across transaction boundaries.** A chunked slide/move that spans
  several `compact_step` calls can interleave with user writes to the source range.
  Options: (a) cap: only move extents ≤ budget in one step, slide giants chunk-by-chunk
  where each chunk commits its own table-visible progress (a slide of the *frontier* extent
  can flip the address after each chunk — the extent is briefly "torn" across old/new only
  if a crash hits mid-chunk, which journaling covers); (b) a `moving` flag in `Meta` with
  write-redirection. (a) is simpler and probably enough.
- **Resizable extents.** They relocate on growth anyway and may deserve tail placement +
  headroom; should `live_by_size` include them (movable) or should meta exclude hot ones?
  Cheap default: include, but tail-first ordering naturally deprioritizes recently-grown
  ones only if growth allocates at the tail — worth a placement-policy decision.
- **When to run steps.** Per-flush? Every N foreground ops? Budgeted idle work? This is
  pure policy (P4) and can be decided last.
- **Index weight.** The gain-greedy core needs all four structures of §4, each touched
  on O(1) size classes per foreground op. Two structures stay deferred: the augmented gap
  tree ("lowest gap `≥ s`") — confirm the `free_by_size.range(s..)` scan stopgap is
  actually too slow before building it — and the gain-ordered class priority queue,
  pointless while the class count stays small.
- **Destination rule.** "Lowest gap that fits" maximizes gain but can squander a large
  gap on a small far-travelling extent — split it, strand the large extent that needed it
  — a failure no lookahead-free gain ordering avoids (it hits the by-byte and by-start
  potentials alike). §4.2's fragmentation term is the principled soft mitigation: it
  reroutes the small mover to an exact or near-exact fit when one exists at comparable
  depth. When none exists, the large gap still gets split — full protection would need
  lookahead. Tune `α` (and `β`) empirically.

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
