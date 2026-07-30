# Problems / deviations while implementing array-pointer-plan.md

Uncommitted scratch notes. Each entry: what the plan said, what I actually
did, and why.

## Step 1

- **`set_content` is byte-only, not generic over `T: Persistable`.** The plan
  framed a generic `PersistableVecGuard::set_content` with a "fast path for
  plain-byte `T`". I implemented it only on `PersistableVecGuard<'s, u8, B>`.
  Reason: a correct *generic* whole-array replace must free the previous
  elements' sub-allocations (e.g. replacing a `PersistableVec<PersistableString>`
  would otherwise leak every old string's content allocation), but no
  `Persistable` trait method exposes "free my sub-allocation", so it can't be
  done generically today. Every actual consumer (String, and Blob in Step 2)
  is a byte vec, so the byte-only primitive covers all real needs. A generic
  version is deferred until a trait hook for element teardown exists.

## Step 3

- **No separate `RawArrayPointer` type.** The plan mentioned a
  `RawArrayPointer` "untyped byte view". I didn't add one: type-erased byte
  addressing already goes through the existing `RawPointer` (which
  `UniqueArrayPointer::raw()` yields), and nothing in Steps 3-4 needs a
  *distinct* raw array type -- `splice` (Step 4) takes the typed
  `UniqueArrayPointer<T>`, and read/write/copy take `RawPointer`. Adding an
  unused type would be cruft. Can be introduced later if a real
  type-erased-array consumer appears.
- **`array_capacity` is added + unit-tested but not yet wired into `load`.**
  The plan mentions "a way to read an allocation's capacity on load". With
  resize-to-exact still in force (amortized growth is explicitly deferred),
  byte capacity == length * elem_size, derivable from the header `len`, so
  `load` doesn't need to call `array_capacity` yet. The method exists, is
  exercised by `array_allocation_capacity_round_trips_through_flush`, and is
  ready for the growth policy that will consume it.
- **Header still carries `len`.** Faithful to "type persists only its
  logical length; capacity comes from the allocator": the header's second
  field *is* the logical length (element count / slot capacity), which the
  type legitimately owns. Byte capacity moved to the allocator
  (`array_capacity`). They're numerically equal today only because there's
  no slack yet.

## Step 4

- **`splice` makes the *content* atomic but not the whole mutation.** The
  plan claims splice makes set_content/remove "single atomic ops ... atomic
  regardless of how the consumer reads." That's true of the **content
  region**: splice bundles resize + tail-move + write into one journal
  entry, so the array allocation is never seen half-shifted (this fully
  closes remove's old copy-then-resize duplicate-element window). But the
  **logical length still lives in a separate header region** (the parent
  anchor), published by a distinct `write`. A torn journal that applied the
  `splice` entry but not the header write would see new content under the
  old length. Closing *that* last gap requires the length to be read from
  the allocator's `array_capacity` instead of the header (plan decision 4 --
  "Blob is the degenerate array ... carries no separate length"), which
  means dropping the header `len` field and changing `load` to derive length
  from capacity. That's a representation change I deferred in Step 3 and did
  not pull into Step 4 (it's not in Step 4's literal task list, and doing it
  safely across vec + map + their `INLINE_SIZE`/`load` is a larger change
  than the last step should carry). The remaining window is documented in
  `set_content`/`remove` doc comments.
- **`insert`-onto-splice not applicable.** The plan's rewire list mentions
  `insert (inline case) -> splice`. `PersistableVec` has no `insert`-at-index
  method, and `PersistableHashMap::insert` appends at the end (already
  crash-safe via append-then-publish, which the plan itself says "needn't
  change") and writes entries through nested `K/V::store` calls rather than a
  flat byte buffer splice can take. So only `set_content` and `remove` were
  rewired onto splice; `push`/map-`insert` stay as safe appends.
