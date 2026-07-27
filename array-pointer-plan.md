# Plan: `UniqueArrayPointer<T>`, allocator-owned capacity, and rebasing blob/string on `PersistableVec`

Groundwork for a three-step change. Draft — open to revision.

## Problem

Variable-length structures (`PersistableVec`, `PersistableHashMap`,
`PersistableBlob`, `PersistableString`) store their length *ad hoc* in an
8-byte `{ pointer, len }` header written by the type. Because content is
currently resized-to-exact, that `len` duplicates the allocation's byte
size (which the allocator already tracks and `resize` already changes
atomically). Every grow/shrink is then two separately-persisted updates —
the `resize` and the header `Write` — which, without a transaction, can
disagree after a crash (temporary internal fragmentation; never corruption
now that the op order is crash-safe, but a real redundancy). All four types
also hand-roll the same pointer/resize/header/reuse-not-leak logic.

## Settled design decisions

1. **Capacity (allocator) vs. length (type).** The redundancy was an
   artifact of resize-to-exact. Allow **capacity ≥ length** (amortized
   growth) and the two numbers diverge, each with a natural owner:
   - the **allocator** owns per-allocation **capacity** (byte size) — it
     needs it for free-space/compaction anyway, and `resize` already
     changes it atomically;
   - the **type** owns the **logical length** (element count) — it's
     type-semantic (vec's contiguous prefix vs. hashmap's liveness tags).

   Then push-into-slack / pop are a single atomic length write; capacity
   changes are separate, independently-safe slack adjustments.

2. **Two pointer types.**
   - `UniquePointer<T>` — the `Box<T>` analog: one fixed-size `T`. Size =
     `size_of` the persisted `T`, **statically known**, so the allocator
     re-derives it on open and persists nothing.
   - `UniqueArrayPointer<T>` — the `Box<[T]>` analog: a run of `T` whose
     count is runtime-determined. Capacity is **not** derivable, so the
     allocator persists it in the allocation's block metadata and manages
     it. `resize` updates it via the index — **no `Location` argument**.

   Today `UniquePointer` is (mis)used for the variable content arrays of all
   four types; those become `UniqueArrayPointer`.

3. **Units: bytes at the allocator, T-units at the typed layer.** Capacity
   is in **bytes** — forced by the allocator being deliberately
   type-agnostic ("byte-range geometry, no type-specific callbacks"), and it
   yields a `RawArrayPointer` (untyped byte view) for free. The
   schema-evolution-stable quantity — the logical **element count** — stays
   T-units at the `UniqueArrayPointer<T>` / type layer, where byte offsets
   are re-derived per schema version.

4. **Blob is the degenerate array.** A `PersistableBlob` is `Box<[u8]>` with
   no slack: length == capacity == byte size, one number, allocator-owned.
   So it needs no type-side length at all; its inline representation is just
   the pointer, and `set` becomes a single atomic `resize` + content write.

5. **Empty content ⟺ default.** Relies on: *no non-default value serializes
   to an empty byte string.* Postcard gives any information-carrying value
   ≥1 byte, so an empty encoding implies a single-inhabitant (ZST-like)
   type whose one value is its `Default`. Breaks only for a pathological
   hand-written `Serialize` mapping several values to `[]`. This is the same
   assumption today's `pointer: None ⟺ value == default` already makes.
   **Document it in code at the load path that maps empty → `T::default()`.**

## Layering

```
UniqueArrayPointer<T>  (+ RawArrayPointer, byte view)   <- core / kladde-alloc + kladde-traits
        |
   PersistableVec<T>                                    <- kladde-types
        |
  PersistableBlob / PersistableString wrap PersistableVec<u8>
```

## Steps

### Step 1 — bulk crash-safe content-replace on `PersistableVec` (prerequisite)

- Add `PersistableVecGuard::set_content` (whole-array replace) with the
  crash-safe grow/shrink ordering, plus a fast path for plain-byte `T`
  (write the span directly, skip per-element `store`). This is the
  `later.md` bulk-replace primitive.
- Immediately fixes `PersistableString::set` (today an O(n²) pop-loop) and
  is the primitive the blob rebase needs.
- **Ordering caveat.** The exact-length consumers (string, general vec) have
  no trailing-byte tolerance, so a grow can't publish the longer length
  before writing content the way blob's postcard consumer can. Full
  atomicity of `set_content` therefore is completed in **Step 4** (the
  `splice` op); until then it uses the safest ordering with a documented
  residual window. This **supersedes** blob's current bespoke
  trailing-tolerance ordering — blob's crash-safety becomes the same as
  string's (both completed in Step 4).

### Step 2 — rebase `PersistableBlob` on `PersistableVec<u8>`

- `PersistableBlob<T> { value: T, serialized: PersistableVec<u8> }`. The
  `serialized.data` **is** the current `bytes` cache — same memory, no new
  cost. Deletes blob's hand-rolled pointer/resize/header/ordering code.
- `set`/`edit` commit: serialize `value` → `serialized.guard().set_content`.
  `load`: read the vec's bytes → deserialize (empty ⟹ `T::default()`, per
  decision 5). `store`/`INLINE_SIZE`/guard delegate to the vec.
- Keep `value: T` cached for cheap `Deref`/`edit` reads.
- Removes the eager/`None`-is-default special-casing: laziness comes
  uniformly from the vec (empty ⟹ no allocation). `new` can become
  backend-free (`PersistableVec::from_iter(bytes)`) like `PersistableString::from`.
- `describe` stays `Opaque` (unchanged — independent of representation).

### Step 3 — `UniqueArrayPointer<T>` + allocator-owned capacity

- New `UniqueArrayPointer<T>` (name settled; `Box<[T]>` analog) and a
  `RawArrayPointer` byte view. Allocator persists capacity (bytes) in block
  metadata for these; `resize` updates it. `UniquePointer<T>` reverts to the
  fixed-size `Box<T>` role (no persisted size).
- Retype the content pointers of `PersistableVec`/`HashMap`/`Blob`(via vec)/
  `String`(via vec) from `UniquePointer` to `UniqueArrayPointer`.
- Split capacity (allocator) from length (type): the type persists only its
  logical length; capacity comes from the allocator. `PersistableBlob` (via
  its `PersistableVec<u8>`) has length == capacity, so it carries no
  separate length.
- Trait/API impact: an array-allocation `alloc`/`resize` that persists the
  size; a way to read an allocation's capacity on `load`. Mostly a
  `kladde-traits` (`Allocator`) + mock (`kladde-alloc`/`test_support`)
  change in v1; the real backend's block-metadata format follows later.

### Step 4 — the `splice` op (atomic content replace), against the mock

Makes the content-shift operations *single atomic ops*, closing the
residual crash-consistency windows Steps 1–3 leave documented. Built against
the mock allocator for now; the real backend's replay follows later.

- **New byte-level op.** Add `Allocator::splice` and a `Splice` journal
  microop that replaces `[offset, offset + old_len)` inside an array
  allocation with `new` bytes — shifting the tail and adjusting capacity —
  bundling the `resize` + content `write` + tail `move` into **one** journal
  entry. Append-time atomicity then comes for free (a torn tail is a
  truncated, un-applied entry); flush-time atomicity is already the
  shadow-paging's job, same as every other op.
  - Signature stays byte-oriented, so the allocator remains type-agnostic:
    `fn splice<T>(&self, pointer: &UniqueArrayPointer<T>, offset: u32, old_len: u32, new: &[u8])`.
    It takes the array pointer (like `resize`) because it changes the
    allocator-owned capacity; `offset`/`old_len`/`new` are bytes, and the
    typed layer converts element indices.
  - Mock impl is essentially one line — `region.splice(offset..offset + old_len, new.iter().copied())`
    (Rust's `Vec::splice` is exactly this); then wire `Splice` through
    `DefaultBackend` recording and `Kladde::flush` replay.
- **Rewire the content-shift ops onto it**, collapsing each to one atomic op
  and deleting its residual-window note:
  - `PersistableVecGuard::set_content` (Step 1) → `splice(ptr, 0, old_bytes, new)`
    — removes the trailing-tolerance-vs-exact ordering split; atomic
    regardless of how the consumer reads.
  - `PersistableVecGuard::remove` → `splice(ptr, index·elem, elem, &[])`;
    `insert` (inline case) → `splice(ptr, index·elem, 0, &elem_bytes)`.
    Removes the `copy → header → resize` sequence and its dup-window `TODO`.
  - `PersistableBlob::set` / `PersistableString::set` inherit atomicity
    through `set_content`; this closes the interim gap Step 2 opened.
- **Scope note.** `splice` shifts *inline* content bytes. For
  `PersistableVec<T>` whose `T` owns sub-allocations, it shifts the inline
  headers; the sub-allocation lifecycle stays separate `Alloc`/`Free`,
  ordered around the splice (alloc-then-publish on insert, publish-then-free
  on remove) — the same safe pattern used elsewhere. `push` already appends
  safely and needn't change.

## Deferred / open

- **`entry`/cursor handle** over a `UniqueArrayPointer` (resolve once, then
  size/read/write/resize without re-lookup, pinning the location). Sound and
  composes with `ResolvedPointer`'s borrow-to-pin, but the lookup is O(1) and
  the payoff depends on the real allocator — defer until the mock shows a
  need (matches the spec's stated deferral of such bookkeeping).
- **Real-backend `splice` replay.** Step 4 lands `splice` against the mock;
  the real file backend's block-metadata resize + replay of the `Splice`
  microop follows with the rest of the real-backend work.
- **Amortized-growth policy** and the chunked/linked on-disk vec layout
  (`later.md`) — the capacity/length split is their prerequisite; the policy
  itself is separate.
```
