# Decoupling address-range management from id-pool management in `Allocator`

Status: design note. Motivated by the small-vec / on-file-table-layout investigation
(the long working-through lives in `small-vec-optimizatons.md` on the `main` branch).

## 1. The conflation

Today's `kladde-heap::Allocator` does two separable jobs:

**Task 1 — address-range management.** Given a size, find a fitting free range of the
`Address` space; on `resize`, decide whether the bytes grow in place or must relocate;
plan and apply *memory* compaction (relocating allocated byte ranges to defragment /
shrink the file). This is the reusable, type-agnostic heap — pure `Address`/`Size`
arithmetic over a set of non-overlapping ranges, with no notion of persistence.

**Task 2 — id-pool management.** Mint and recycle the stable `Pointer` ids that name
allocations; maintain the `id → (address, size, sizedness, meta)` table; and — the
load-bearing part — decide *how that table is laid out*, in memory and especially on file.

**Claim:** `Allocator` should own only task 1. Task 2 belongs to the `Backend`, because it
is coupled to the on-file table layout, which is a persistence concern.

**Judgment — agree, and the table-layout investigation is strong evidence.** The decisive
observation there: **the optimal id-assignment policy is a function of the on-file table
representation, which is the backend's choice.** Both layouts below want freed ids *reused*
(so `max_id` stays ≈ peak); what differs is whether the layout *dictates* the specific reuse
policy or *leaves it free*:

- a *compact positional* table (id implicit from row position, 0 bytes of key) **requires**
  smallest-free-id-first — its implicit-key encoding and O(1) navigation (the "id−1 exists"
  property, coldness = id-order) depend on that specific policy;
- a *swap-remove* table (dense, stores a narrow id) is **indifferent to which** freed id is
  reused: `max_id` is bounded by peak under *any* reuse (all its narrow id-column cares
  about), so smallest-first buys it nothing over an O(1) LIFO free-list — it leaves the
  backend free to pick the cheapest policy.

So the id policy genuinely tracks the layout: one dictates smallest-first, the other imposes
nothing — and that alone is enough, since the allocator can't know whether to constrain
itself to smallest-first or leave the choice open. Only the backend knows the layout.

The *converse* reinforces the split by showing the two axes are orthogonal: address-space
**slabbing / size-class grouping** — placing same-size allocations in contiguous address
regions to cut fragmentation — is a placement optimization the **allocator** owns under
task 1, and it needs only the `size` (and sizedness) already passed to `alloc`, never any say
over *ids*. The allocator decides *where in the address space*; the backend decides *which
id*; neither constrains the other.

If `Allocator` owns id minting it must bake in one policy, silently committing the whole
system to one table layout — and (see §2) to one corner of a fundamental trade-off.
Decoupling lets the backend pick ids to match its table and leaves `Allocator` a clean
address-space manager with zero persistence assumptions. It also dissolves the recurring
"`reserve`/`claim` couples the allocator to the backend's journaling needs" tension: with
the split, deferral is just *when the backend chooses to call the allocator* (§3).

**The one genuinely coupled sub-question — sizedness.** `resizable` vs `fixed` is both
(a) a *placement hint* the allocator legitimately uses (fixed allocations pack tightly / in
segregated pools; resizable ones may want growth headroom), and (b) a *fact recorded per
id* that `load` needs to rebuild the right handle. Clean split: **sizedness is an input the
caller passes at alloc time** (already true — `alloc_resizable` vs `alloc_fixed_size`), the
allocator may use it transiently for placement, and the **backend records `id → sizedness`**
in its table (it already must, to answer `resolve`). So the id→sizedness *mapping* is task 2
(backend); sizedness *as a placement hint* is a per-call parameter to task 1. The allocator
never needs an id→sizedness query.

## 2. Why fix the boundary now: the RUM / list-labeling wall

The two tasks meet a fundamental trade-off, and the current coupling picks a corner of it
for you — which is why the boundary is worth getting right before the abstractions set.

**The limitation.** For the on-file allocator table you cannot have all three of:

1. **no stored id** (id implicit from a row's position — saves the id bytes),
2. **worst-case size `O(current)`** (table shrinks to the live count even after a burst
   drains), and
3. **`O(1)` worst-case moves per op** (no amortized compaction burst).

Dropping the stored id (1) forces *position encodes id*, which chains the physical layout
to the (adversary-controlled) live-id set; keeping that dense then demands relocations, and
bounding relocations in the worst case is exactly the **online list-labeling /
packed-memory-array problem**, whose deterministic lower bound in linear space is
**Θ(log² n)** moves per update — *not* `O(1)`. You can have any two: store a *narrow* id and
get (2)+(3) trivially via swap-remove; or drop the id and accept either amortized-only
compaction, or `O(peak)` size.

**How it bit the old design.** By minting ids *inside* the allocator with an implicit
smallest-first policy, the old `Allocator` steers you into regime (1) — the positional
table — which is precisely the corner that hits the wall for worst-case (2)+(3). The escape
(swap-remove with backend-chosen narrow ids, which meets the *practical* (2)+(3) for a small
stored id) is unavailable unless the backend controls id assignment. So the coupling doesn't
only mix concerns — it silently forecloses the cheapest robust implementation. Decoupling
restores the choice.

**References.**

- Online list labeling / file-maintenance: Itai–Konheim–Rodeh 1981; Willard 1982, and 1992
  for the `O(log² n)` *worst-case*; deterministic linear-space lower bound Ω(log² n):
  Bulánek–Koucký–Saks (STOC 2012), building on Dietz–Zhang.
- Packed-Memory Array: Bender–Demaine–Farach-Colton 2000.
- Randomized progress (oblivious adversary only): Bender et al., FOCS 2022.
- Read–Update–Memory trade-off framing: Athanassoulis et al. 2016 ("RUM conjecture").
- The escape is the **slot map / handle table** (generational-index pattern) with a
  free-list; de-amortizing its compaction is **global rebuilding** (Overmars 1983).

## 3. Redesign sketch

Guiding rule: **`Allocator` speaks only `Address`/`Size`; the `Backend` owns ids, the id
table, and its layout.**

The allocator here is **id-free**: it manages only free address space, exactly as a textbook
free-space allocator does — it never holds a per-allocation table. That is not merely cleaner
separation; it is how real (compacting) allocators are actually built, and it is *why*
`plan_compaction` works at the level of occupied *runs* it derives from its own free space,
rather than needing a per-allocation table (see §4 and §5). The minimal alternative —
`alloc(id, size)` with the allocator keeping its own `id → (address, size)` table — is weighed
and rejected in §4. This id-free split is also what makes `reserve`/`claim` vanish (below).

### `Allocator` and `CompactingAllocator` (task 1 only)

**Note — this `Allocator` does *less* than [`generic-allocator.md`](generic-allocator.md)
describes.** There, the allocator "manages a dynamic collection of address regions" and
answers per-allocation queries (`size`, `lookup`, `address`, `resolve`). Under this pivot it
does **not** track the allocations at all: it holds only the set of **free** address ranges
(what it needs to satisfy `alloc`), and it does **not** record how the occupied complement is
divided into individual allocations. Per-allocation facts (which id, what size, what
sizedness) live only in the backend's id table; the allocator is *told* a size when the
backend calls `free`/`resize`, and it plans compaction at the granularity of occupied *runs*
(the complement of its free ranges), never seeing the per-allocation breakdown at all. This is
deliberate — see §4 and the literature note (§5): it is how real free-space allocators work,
and it is what keeps this a genuinely thin, reusable heap.

```rust
// No `Pointer`, no `Meta`, no id-keyed queries, no reserve/claim, no lookup/resolve.
pub trait Allocator {
    type Address: Word;
    type Size: Word + Into<Self::Address>;

    /// Reserve a free range for a new allocation, returning its address.
    /// `sizedness` is a *placement hint* only (the allocator may segregate fixed vs
    /// resizable); it is NOT remembered as an id attribute -- that's the backend's table.
    fn alloc(&mut self, size: Self::Size, sizedness: Sizedness) -> Self::Address;

    /// Release the range `[address, address + size)`.
    fn free(&mut self, address: Self::Address, size: Self::Size, sizedness: Sizedness);

    /// Resize the range at `address`; report whether the bytes must move
    /// (in place iff the following space is free).
    fn resize(
        &mut self,
        address: Self::Address,
        old_size: Self::Size,
        new_size: Self::Size,
    ) -> Relocation<Self::Address>;

    /// highest allocated address = minimal length of a file to store all live regions
    /// without compaction.
    fn uncompacted_len(&self) -> Self::Address;
}

pub trait CompactingAllocator: Allocator {
    // --- compaction: run-level sliding, derived from free space alone ---
    /// Plan the moves that defragment the address space. Each `Move` slides one contiguous
    /// *run* of neighbouring allocations by a common delta -- the runs are the complement
    /// of the free ranges, so no per-allocation input is needed. The backend copies the
    /// bytes of each run and shifts every id in `[old, old+len)` by `new - old`.
    fn plan_compaction(&self) -> Vec<Move<Self::Address, Self::Size>>;
    fn apply_move(&mut self, m: Move<Self::Address, Self::Size>);   // commit one run's slide
    /// total live bytes = file length after a full compaction. Useful for estimating if
    /// compaction is worth doing.
    fn compacted_len(&self) -> Self::Address;
}

pub enum Sizedness { Resizable, Fixed }
/// One contiguous run of neighbouring allocations sliding as a unit.
pub struct Move<A, S> { pub old: A, pub new: A, pub len: S }
// Relocation<Addr>: Relocated { old, new } | InPlace { addr }   (Unclaimed no longer arises here)
```

What left the trait, and where it went:

- `type Pointer`, the owned handles, `ResolvedPointer` → **backend** (it mints ids and hands
  out handles). They stay concrete shared types; the allocator just stops referencing them.
- `type Meta`, `lookup`/`lookup_mut`, `Allocation`/`AllocationMut` → **backend's id table**
  (the backend is the one keeping per-id records now).
- `address(id)`, `size(id)`, `resolve(id)`, `meta(id)`, and every `*_fixed_size`/
  `*_resizable` query → **backend** (answered from its table).
- `reserve_*` / `claim_*` → **gone entirely** (see below).
- `alloc_*`/`resize`/`make_*` go from id-keyed to address-keyed; the sizedness *gate* (only
  resizable resizes) is now enforced by the backend's handle types, not the allocator.
- `Relocation::Unclaimed` becomes unreachable here (the allocator only sees claimed
  addresses) and can be dropped from the allocator's variant.

### The `reserve`/`claim` question — yes, it leaves `Allocator` entirely

`reserve`/`claim` existed to **mint an id early (for serialization) but assign its address
late (for journaling)**. Once id-minting is the backend's, that split is just *timing of the
two now-separate calls*:

- **Mint** the id (`backend`'s id pool) — immediate, so a `Persistable` can serialize it.
- **Allocate** the address (`allocator.alloc(size, …)`) — called *whenever the backend wants
  the address*.

`UnjournaledBackend` does both at once. `JournaledBackend` mints the id now, records
`id → (address: None, size, sizedness)` in its table, and defers `allocator.alloc` to journal
replay, filling in the address then. **The allocator has a single immediate `alloc`; there is
no reserved/unclaimed state in it at all** — the "valid but unclaimed" state now lives only in
the backend's table, exactly where the journaling logic already is. So it works, and it's
cleaner. (Consequence: a deferring backend represents "id minted, address pending" in its
table — trivial with `address: Option<Address>` — and must not answer address/read/write for
a pending id; the same invariant journal replay already enforces, just relocated.)

### `Backend` (now owns the id pool + table)

```rust
pub trait Backend {
    type Pointer: Copy;   // the id -- minted and recycled by the backend now
    type Size: Word;

    // Answered from the backend's OWN id table (sole owner of id -> address/size/sizedness/meta):
    fn size(&self, p: Self::Pointer) -> Result<Self::Size, AllocError>;
    fn resolve(&self, p: Self::Pointer) -> Result<ResolvedPointer<Self::Pointer>, AllocError>;
    fn resolve_fixed_size(&self, p: Self::Pointer)
        -> Result<UniquePointerFixedSize<Self::Pointer>, AllocError> { /* default via resolve */ }
    fn resolve_resizable(&self, p: Self::Pointer)
        -> Result<UniquePointerResizable<Self::Pointer>, AllocError> { /* default via resolve */ }
}
```

Shape is essentially unchanged — but the *meaning* firms up: these queries are now
definitionally the backend's, because it is the sole owner of the id table (previously they
delegated to the allocator's table).

### `ReadBackend` — unchanged

```rust
pub trait ReadBackend: Backend {
    fn read_at(&mut self, anchor: Self::Pointer, offset: Self::Size) -> impl Read + Seek + '_;
}
```

Reads still resolve `id → address` via the backend's table, then hit `Storage`.

### `WriteBackend` — same public surface, new internals

```rust
pub trait WriteBackend: Backend {
    fn alloc_resizable(&self, size: Self::Size) -> UniquePointerResizable<Self::Pointer>;
    fn alloc_fixed_size(&self, size: Self::Size) -> UniquePointerFixedSize<Self::Pointer>;
    fn free_resizable(&self, p: UniquePointerResizable<Self::Pointer>);
    fn free_fixed_size(&self, p: UniquePointerFixedSize<Self::Pointer>);
    fn resize(&self, p: &UniquePointerResizable<Self::Pointer>, new_size: Self::Size)
        -> Result<(), BackendError>;
    fn make_resizable(&self, p: UniquePointerFixedSize<Self::Pointer>, new_size: Self::Size)
        -> Result<UniquePointerResizable<Self::Pointer>, BackendError>;
    fn make_fixed_size(&self, p: UniquePointerResizable<Self::Pointer>, new_size: Self::Size)
        -> Result<UniquePointerFixedSize<Self::Pointer>, BackendError>;
    fn write(&self, anchor: Self::Pointer, offset: Self::Size, bytes: &[u8]);
    fn splice(&self, p: &UniquePointerResizable<Self::Pointer>,
              offset: Self::Size, old_len: Self::Size, new: &[u8]);
}
```

Public signatures are unchanged. The *bodies* move; e.g. `alloc_resizable`:

1. mint an id from the backend's id pool,
2. call `allocator.alloc(size, Resizable)` for an address (immediately, or deferred in the
   journaled backend),
3. record `id → (address, size, Resizable)` in the table,
4. return the handle.

`free_*` returns the id to the pool and calls `allocator.free(address, size, sizedness)`.
`resize` looks up the address, calls `allocator.resize(address, old, new)`, moves bytes on
relocation, updates the table. `make_*` re-tags sizedness in the table and asks the allocator
to place accordingly (in place when possible). **Compaction:** the backend calls
`allocator.plan_compaction()` (no argument — the allocator derives the runs from its free
space); for each returned `Move { old, new, len }` it copies `[old, old+len)` to `new` in
`Storage`, shifts every id whose address is in that range by `new - old` in its table, and
calls `allocator.apply_move(m)`. Because serialized pointers are stable ids, no on-disk
*pointer* is rewritten (unchanged from today) — only the table's address column.

## 4. Net effect and open decisions

Net effect:

- `Allocator` shrinks to a pure `Address`/`Size` heap: `alloc`/`free`/`resize` + compaction.
  Trivially reusable and testable; no persistence, no ids, no `Meta`.
- `Backend` gains explicit ownership of the id pool and the id-table layout — the whole
  point, since that layout is where the RUM/list-labeling choice is made, and it can now be
  chosen (positional, swap-remove, PMA, …) without touching the `Allocator` contract or
  `Persistable`.
- `reserve`/`claim` disappear as allocator concepts.

Decisions (A is settled — that's the point of this branch; B–D left for implementation):

- **A. Settled: the allocator is free-space-only; the `id → address` table lives in the
  backend.** The real reason isn't tidiness — it's how allocators are built. A malloc-family
  allocator keeps a *free* structure and recovers a block's size from an in-band boundary tag
  or from the caller; a compacting collector recovers per-object size from an in-band header
  or by tracing. Kladde's allocator has **neither** source: data in `Storage` is opaque (no
  in-band headers) and it is type-agnostic (can't trace). So per-allocation facts can come
  *only* from the external id table — the backend's. Hence the allocator holds only free
  space and is *told* sizes on `free`/`resize`. Compaction needs nothing handed to it either:
  a run-based sliding compactor works at the granularity of occupied *runs* (the complement of
  the free ranges) and never looks inside a run, so `plan_compaction()` takes no argument and
  emits one `Move` per run. (Only a finer strategy that relocates individual allocations into
  scattered gaps — best-fit, two-finger/Cheney — would need the per-allocation live ranges;
  sliding, the I/O-friendly default, does not.) The alternative — `alloc(id, size)` with the
  allocator keeping an `id → (address, size)` table — would force it into a per-allocation
  table that real free-space allocators don't keep, and duplicate the backend's table in
  memory. Rejected.
  (An earlier draft worried this "forces the backend to hold the table" — but the backend
  *owns* that table anyway, since it persists it; holding the in-memory copy is just the
  `PersistedVec` → `Vec` split, with the allocator as a pure free-space helper.)
- **B. Where the shared handle types live** (`Pointer`, `UniquePointer*`, `ResolvedPointer`)
  now that the allocator doesn't use them. Staying in `kladde-heap` is fine (the backends
  there use them) — they're simply no longer referenced by the `Allocator` module.

**Decision (Rob):** Yes, keep them in `kladde-heap`.

- **C. Sizedness on `free`/`resize`.** If the allocator segregates fixed vs resizable pools it
  needs sizedness on `free` too (or must derive the pool from the address). Passing it is
  simplest; the sketch does.

**Decision (Rob):** Yes, pass sizedness to `free`. It can always choose to ignore it.

- **D. Where `Relocation`/`AllocError` live.** `Relocation` stays with the allocator (`resize`
  needs it, minus `Unclaimed`); `AllocError`'s `DanglingPointer`/`WrongSizedness` become
  *backend-table* errors (the id table is the thing that can be queried with a bad id).

**Decision (Rob):** I think `Relocation` can go away. `Allocator::resize` can simply return `Option<Address>` since the caller already knows the old address (and it's semantically obvious that any returned address must be the new address). `DanglingPointer`/`WrongSizedness` should be folded into `BackendError`. But `Allocator::{alloc, free, resize}` should return a new `AllocError` for out-of-memory (`alloc` and `resize`) and if the provided address range overlaps with a free region (`free` and `resize`).

## 5. Prior art: this is a handle-based relocatable heap

The shape being built — **stable handles + relocatable blocks + compaction that rewrites a
handle → address table** — is old and well-charted. When actually implementing the allocator,
pull from this literature rather than inventing:

- **Handle-based relocatable memory managers.** The classic reference is the original
  **Macintosh Memory Manager**: a `Handle` is a double indirection through a "master pointer"
  table (id → address); heap blocks are relocatable; compaction slides blocks and rewrites the
  master pointers. That is almost exactly kladde's `id → address` table + compaction, so it is
  the closest prior art. The same pattern recurs as **handle tables / generational-index "slot
  maps"** in game engines.
- **Free-space management** (what the allocator here actually implements): Wilson, Johnstone,
  Neely & Boles, *Dynamic Storage Allocation: A Survey and Critical Review* (1995) — the
  canonical survey of free lists, boundary tags, coalescing, and fit policies.
- **Compaction:** Jones, Hosking & Moss, *The Garbage Collection Handbook* (2nd ed.) — the
  standard reference for compaction algorithms (mark-compact / sliding / threaded / one-pass)
  and the collector ↔ metadata interface. Sliding compaction is what makes a `Move` here span
  a whole contiguous *run*: sliding preserves order and shifts each run by a common delta, so
  the natural (and most I/O-efficient) move unit is the run, not the individual allocation.

**The one adaptation to keep in mind while reading them:** those systems recover a block's
size from an *in-band header* or by *tracing*. Kladde has neither, so its equivalent of "the
header" is the **external id table** — and *where that table lives* is exactly the split this
note makes: it lives in the backend, and the allocator stays free-space-only.
