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

## Flush

- **A growth in the fold must not resolve through an anchor that the header's take retires — a gap in `impl/`.**
  The flush takes the header's content before the fold, and marks the anchors it held for replacement.
  When the fold then grows such an id, `grow_size_to` resolves the exposed range through the old anchor, and the replacement `Shrink(id, size)` the cut states anchors at the new size, so it covers none of that range.
  After the commit, the old anchor is gone from the governing world, and the exposed range resolves through whatever older statement matches it, which can be stale bytes the old anchor had denied.
  The flush therefore takes the exposed range as a pending zero whenever the id's anchor is being replaced, and the cut states it as a `Zero`.
  `impl/address-table-operations.md#grow_size_ton-and-shrink_size_ton` should say so; the differential oracle found it within a few hundred random operations.
- **A grow witness in a retired page must be released when another statement now witnesses the size — a gap in `impl/`.**
  `impl/address-table-operations.md#what-the-cut-states-for-a-touched-id` restates a retired page's grow witness "unless a row above already states one", and says nothing about the old witness in that case.
  It then stays pinned in a page that is no longer in the governing world, and in the header's case, in the slot the flush after next overwrites.
  The cut unpins it.
- **Every piece the fold sources from elsewhere is read and written afresh.**
  That covers `Copy` and `Move` sources and the tail a `Splice` shifts, which the flush writes as new bytes rather than restating existing ones at new offsets.
  So the flush has no ordering phase (`impl/flush.md#phase-c--ordering`) and no content-blind fast path; the fold's output is only ever new bytes or bytes left where they are.
  It keeps every data byte referenced at most once and the reverse index valid without any splice rule, at the price of rewriting a spliced tail, which makes a `remove(0)` on a large vector cost the vector's size.
- **The header's eviction ranks by allocation, not by fragment.**
  A pending fragment's heat is the number of flushes since its allocation's `last_written`, where `impl/flush.md#the-header-as-write-buffer` keeps an eviction clock per fragment.
  All statements of one allocation therefore stay in the header or leave it together, which is coarser for a large allocation with a hot tail and a cold head, but needs no clock at all, and `impl/consolidator-state.md#what-stays-out` seeds the clock from the same content ages anyway.
- **The interior layer is rebuilt by every cut that needs one.**
  `impl/flush.md#the-shape-of-the-tree` path-copies from the changed leaves up.
  With the header naming up to 600 leaves directly, a file needs an interior layer only past that, and rebuilding it costs one page per 800 leaves per flush.
- **`Store::open` folds a recovered journal before it returns.**
  `spec/journal.md#the-start-of-a-session` asks only that a non-empty recovered journal be folded before anything is appended.
  Folding at open is simpler, and it lets reads, which see flushed state only, see every recovered transaction at once.
- **Misusing the transaction and batch calls reports `Error::Corrupt`.**
  Ending a transaction that is not open is a caller's bug rather than a damaged file, and deserves an error variant of its own.
  The typed layers pair the calls through guards, so only direct users of `Store` can hit it.

## Consolidation

- **Free filling moves part of at most one victim per page, and the victim stays eligible.**
  `impl/consolidation.md#packing-in-id-order-with-look-ahead` lists "part of a victim too big to take whole" as the last filler without saying how many victims to try.
  Trying one per page is enough: a first version tried victim after victim while the room was too small for any statement's survivors, and each attempt excluded that victim from the rest of the flush, which starved the budget loop of exactly the sparse pages it wanted.
- **An offer takes whole victims by score, not by best fit.**
  `offer_page` in `impl/consolidation.md#victims-are-pulled-one-at-a-time` fills a budgeted page "by best fit".
  The implementation takes, again and again, the best-scoring sampled victim that still fits, from the sparsest bucket up, and cuts one more victim across the boundary when the page would otherwise close more than `θ` empty.
- **The churn floor also bounds each victim**: a budgeted page takes only victims with coverage at most `C / (1 + λ)`, besides checking the offer as a whole.
- **The budget loop never continues the last page of `pack`.**
  The design lets that page cut a victim's survivor whenever the loop opens another page, so that only the flush's very last data page can close short.
  Here, `pack`'s last page is filled by free filling alone, and can close short even when the loop goes on.
- **The cut's fillers state only ids the flush has not touched otherwise — a gap in `impl/`.**
  `impl/consolidation.md#one-dirty-set-and-why-statements-are-derived-last` fills the last page's room with table victims and the rotating window once the flush's own statements are laid out, and merges what they add into that page.
  But a victim can hold the anchor of an id whose size statements are laid out already: if the flush emitted a `Grow` for it, replacing the anchor needs a `Shrink`, and one epoch may not hold both.
  The implementation avoids the conflict: the cut takes a table victim only if the flush touched none of the ids it names, and the window restates an anchor or grow witness only for such ids, while restating content, which cannot conflict, for any id.
  A victim excluded this way remains a candidate for later flushes.
- **A table victim's restatements are estimated at 1.25 times its coverage.**
  Fragments split by shadowing need a statement each, and restated statements lose the delta encoding of their old neighbours; `impl/consolidation.md#the-page-rewrite` prices the rewrite by coverage alone.
  If the estimate is too low for a filler, what does not fit the last page spills into another leaf.
- **A budgeted page rewrite opens no page of its own.**
  It takes its victims before the cut, whose layout then needs about one more leaf per offer; the budget counts it as a page all the same.
- **The header keeps its hottest statements one by one, not the coldest key-contiguous run.**
  `impl/flush.md#the-header-as-write-buffer` recommends evicting the coldest run in key order, so that leaves cover coherent id ranges.
  The cut fills the header with the hottest statements that fit and cuts the rest into leaves in key order, which gets the leaves' key order but not the runs.
- **Description defragmentation weighs a pending fragment as if it were stated alone**: 8 bytes of framing for bytes in a data page, 4 for an `Inline`, 5 for a `Zero`.
  `impl/consolidation.md#the-envelope-is-a-maximum-subarray-problem` leaves the estimate open, and the re-check at execution corrects it either way.
- **The re-check refuses a candidate holding bytes the flush is moving, not only bytes it wrote — a gap in `impl/`.**
  `impl/consolidation.md#where-it-is-called-and-how-it-is-executed` relies on the age filter to keep rewrites off anything the flush is writing, since a fragment the fold took has age 0.
  But free filling executes candidates while `pack` runs, and by then evacuation may have moved survivors of the candidate's range into this flush's pages; their allocation's age can be anything, so the filter lets them through, and the rewrite would leave dead bytes in pages the flush is about to write.
  The re-check therefore refuses any range holding a chunk not placed yet or bytes placed in a page of this flush.
- **The reserved share counts every rewritten byte, `Inline`s included**, although an `Inline` takes no data page.
  Free filling uses only candidates too long for an `Inline`, since a page's room is what it offers.
- **The controller moves the budget multiplicatively.**
  After each commit it measures the live fraction of data and table pages and multiplies the budget by `exp(4 · (τ − fill))`, within `budget_min` and `budget_max`.
  `impl/consolidation.md#constants-still-to-be-chosen` leaves the rule open.
