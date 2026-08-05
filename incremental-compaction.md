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

Progress can be made precise with a potential function: let
`Φ = Σ_{live extents a} address(a)`. Every useful compaction step strictly decreases `Φ`;
`Φ` is minimal exactly when the heap is compact. `end` itself shrinks whenever a step (or a
`free`) leaves the suffix of the address space free — so `Φ` is the *progress* measure and
`end` the *payoff*, which lags it.

Two kinds of step exist, distinguished by cost-effectiveness:

- **Exact-fit move**: a live extent of size `s` at a high address moves into a free gap of
  size `s` at a lower address. Copies `s` bytes, frees `s` bytes of high address space.
  Gain (bytes of `Φ`-progress per byte copied): **1**.
- **Slide**: the live run directly above a gap of size `g` shifts down by `g`. Copies `R`
  bytes (the run) to make `g` bytes of progress. Gain: **`g/R ≤ 1`**, arbitrarily bad for
  long runs above small gaps.

So the scheduling problem is: *prefer exact-fit moves whenever one exists; slide only as
fallback* — and find each next move in O(log n), not by rescanning.

**Why exact fits should exist at all**: kladde deliberately hands out many equal-sized
fixed-size allocations (that was the point of the `Sizedness::Fixed` hint and the size-class
placement idea). Freeing any one of them mints a gap that is a plug-compatible slot for
every other live member of the same size class. In the degenerate case where all sizes are
unique, the mechanism degrades gracefully to slide-only — nothing breaks, compaction is just
less cheap.

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
- **P3 — Track the intersection, not the ingredients.** What compaction consumes is
  neither "gaps" nor "movable extents" but the *pairs* (low gap, high same-size extent).
  Represent that intersection as its own incrementally-maintained set (§4), so a step is
  pop-and-execute, and quiescence ("nothing worth moving") is a cheap emptiness test.
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

## 4. The core structure: one partition, three indexes, one actionable set

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
live_by_size: BTreeMap<Size, BTreeSet<Address>> // movable live extents, grouped by size
```

`BTreeSet<Address>` per class rather than a binary heap: min *and* max are O(log), and —
unlike a heap — arbitrary deletion is native, which matters because gaps are consumed by
ordinary `alloc` and extents die by ordinary `free`, not only by compaction. (This subsumes
the per-class min-heap idea; the heap's O(1) peek isn't worth losing cheap deletion.)

The intersection, maintained rather than recomputed (P3):

```rust
actionable: BTreeSet<(Address, Size)>   // ordered by the movable extent's address, descending use
// s is actionable  ⇔  min(free_by_size[s]) < max(live_by_size[s])
```

Every foreground mutation touches O(1) size classes and re-checks this one inequality for
each touched class — flipping a class in or out of `actionable` is the "switching a flag"
this design was missing. `compact_step` is then:

1. Pop the actionable class whose movable extent has the **highest address** (tail-first:
   most likely to shrink `end` soon). O(log n).
2. Move that extent into the class's lowest gap: copy `s` bytes, flip the table entry,
   update the four indexes. The vacated range coalesces with free neighbors (possibly
   changing *their* class membership — still O(1) classes touched).
3. If `actionable` is empty, optionally fall back to a bounded **slide**: take the lowest
   gap, and shift the single extent immediately above it down (its successor in `extents`
   — one lookup, no scan). Chunk the copy front-to-back if the extent exceeds the budget.
4. If the suffix of the address space is now free, retreat `end`.

Gap splitting makes "combination fits" fall out for free: a gap of size `3s` never matches
class `3s` extents? Fine — best-fit `alloc` or an explicit policy can place an `s`-extent at
its front, and the `2s` remainder re-enters `free_by_size` as a smaller class, ready for the
next exact fit. Full bin-packing of combinations is NP-hard and not worth chasing; greedy
splitting captures the realistic case (many allocations of few distinct sizes).

Cost accounting: all indexes together hold one entry per extent — O(live + gaps) — a
constant factor on the table the stable-id constraint already forces into memory (P1).

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
(tail-first exact-fit, gain-greedy hybrid with slides, "only when `len > 2 × live_bytes`"
hysteresis, never-move-pinned…). This is not an alternative but the natural second story on
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
- **`free_by_size` fragmentation of classes.** Coalescing merges gaps into ever-rarer
  sizes; if exact-match rates in practice disappoint, the fallback query "smallest gap of
  size ≥ s" is one `BTreeMap::range(s..)` step away — measure first.

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
