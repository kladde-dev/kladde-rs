# Implementation notes

Where this implementation deviates from kladde-docs, what the documentation leaves unclear, what could be done better, and what went wrong along the way.

This file takes over the role that `general-instructions.md` gives to `spec.md`.
The design lives in kladde-docs, so findings from implementing it are recorded here first, and proposed for the documents from here.
Paths like `spec/journal.md` are relative to kladde-docs' `content/`.

## Storage

- **`FileStorage` grows the file with `set_len`, not with `fallocate`.**
  The new region stays unallocated until it is written, so a full disk shows up as a failed write or `fsync` during a flush, which poisons the store, rather than as a failed growth before the flush starts.
  `impl/` says nothing about preallocation, and `spec/durability.md#headroom` asks only that failure be clean, which poisoning provides.
- **`MemoryStorage` models a power cut as any subset of the writes since the last sync, each write whole.**
  It does not model a write torn within a page.
  The page CRCs are meant to catch those, but no test exercises them yet.

## Journal and fold

- **The fold keeps one `BTreeMap` piece table per touched id.**
  `impl/flush.md#representing-a-piece-table-cheaply` proposes a `Uniform` source per id that spills into one flush-wide map only on a second piece.
  The simpler form costs a tree per touched id, which has not mattered at the sizes measured so far.
- **`Move` is folded exactly like `Copy`, followed by zeroing the vacated range.**
  The record is specified in `spec/journal.md#move`, and the fold implements its semantics.
  What `drafts/move-op.md` sketches, handing the source's `Ref`s to the destination without copying bytes, is not implemented: the flush copies the moved bytes like any other read (see [Flush](#flush)).

## In-memory state and loading

- **A `Grow` above a `Tombstone` anchor must survive `n <= anchor.n` — a gap in `impl/liveness.md`.**
  The second of the three death tests in `impl/liveness.md#grow-is-locally-decidable-and-shrink-is-not` kills a `Grow` whose bound is at most the anchor's, "since the anchor's own term" then achieves the size.
  That is true of the size, but for a `Tombstone` anchor the `Grow` may also be the only statement asserting existence: `Tombstone`@3 followed by `Grow(0)`@5 is an id re-allocated at size 0, and dropping the `Grow` deletes it.
  The implementation applies the second test only to a `Shrink` anchor.
- **A `Grow` stays the grow witness at load whenever the size is 0 — the same gap, seen from the sweep.**
  `impl/address-table-operations.md#what-the-sweep-leaves-behind` makes the admitted `Grow` the witness exactly when its bound exceeds `run_start`, which a `Grow(0)` never does.
  For a zero-sized allocation, the `Grow` can be the only proof of existence, so the loader keeps it as the witness whenever the size is 0, at worst one pinned statement too many.
- **`last_written` is the epoch of the flush that last wrote the id, a `u64`.**
  `impl/in-memory-state.md#3-the-allocation-map` sketches a `u32`; epochs are 64-bit in the format, and ages are computed as epoch differences, so the wider field needs no wrap-around rule.
- **`Origin::Arena` holds a `u64`.**
  `impl/consolidation.md#how-a-flush-and-consolidation-compose` halves `Origin` with a 32-bit arena position and a flush forced before the arena outgrows it.
  The implementation keeps 64 bits and relies on the journal budget to keep the arena small; a single transaction is still capped at `2^32 - 1` bytes by the journal's length prefix.
- **A page whose coverage reaches zero leaves its bucket at once.**
  It is retired at the next commit either way, and keeping it would waste the samples that victim selection draws from the sparsest bucket, which is exactly where such pages collect.
