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

The sketches below take the *fuller* decoupling — an **id-free** allocator — rather than the
minimal "`alloc` gains an `id` argument" you floated. That minimal version works too; §4.A
weighs the two. The id-free version realizes the task split more completely and is what makes
`reserve`/`claim` vanish cleanly, so it's the one I'd aim for.

### `Allocator` (task 1 only)

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

    // --- compaction: pure address arithmetic; the backend drives it ---
    /// Plan where live ranges should move to defragment. The backend passes its live
    /// ranges (from its own table); the allocator returns address remappings, keyed by
    /// address -- it never learns ids.
    fn plan_compaction(&self, live: &[LiveRange<Self::Address, Self::Size>])
        -> Vec<Move<Self::Address>>;                 // Move { old, new }
    fn apply_move(&mut self, old: Self::Address, new: Self::Address, size: Self::Size);
    fn compacted_len(&self) -> Self::Address;
}

pub enum Sizedness { Resizable, Fixed }
pub struct LiveRange<A, S> { pub address: A, pub size: S }
pub struct Move<A> { pub old: A, pub new: A }
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
`allocator.plan_compaction(its live ranges)`, performs the `Storage` byte moves, and updates
its own `id → address` entries — no ids enter the allocator, and because serialized pointers
are stable ids, no on-disk pointer is rewritten (unchanged from today).

## 4. Net effect and open decisions

Net effect:

- `Allocator` shrinks to a pure `Address`/`Size` heap: `alloc`/`free`/`resize` + compaction.
  Trivially reusable and testable; no persistence, no ids, no `Meta`.
- `Backend` gains explicit ownership of the id pool and the id-table layout — the whole
  point, since that layout is where the RUM/list-labeling choice is made, and it can now be
  chosen (positional, swap-remove, PMA, …) without touching the `Allocator` contract or
  `Persistable`.
- `reserve`/`claim` disappear as allocator concepts.

Open decisions (deliberately left for implementation):

- **A. Id-free allocator (sketched, "B") vs `alloc(id, size)` ("A", your suggestion).** "B"
  keeps the *only* id table in the backend; compaction is driven by the backend passing live
  ranges, and it builds a transient `address → id` view to apply the returned moves (O(n) —
  and compaction is O(n) anyway). "A" lets the allocator keep an in-memory id-keyed table so
  compaction `Move`s can carry ids directly, at the cost of a second id table (allocator's
  in-memory vs backend's on-file). Recommend **B**; fall back to A only if that reverse-map
  proves genuinely annoying.
- **B. Where the shared handle types live** (`Pointer`, `UniquePointer*`, `ResolvedPointer`)
  now that the allocator doesn't use them. Staying in `kladde-heap` is fine (the backends
  there use them) — they're simply no longer referenced by the `Allocator` module.
- **C. Sizedness on `free`/`resize`.** If the allocator segregates fixed vs resizable pools it
  needs sizedness on `free` too (or must derive the pool from the address). Passing it is
  simplest; the sketch does.
- **D. Where `Relocation`/`AllocError` live.** `Relocation` stays with the allocator (`resize`
  needs it, minus `Unclaimed`); `AllocError`'s `DanglingPointer`/`WrongSizedness` become
  *backend-table* errors (the id table is the thing that can be queried with a bad id).
