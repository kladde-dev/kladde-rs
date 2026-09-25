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
- **The last-tombstone rule must not fire when the tombstone itself is what leaves — a gap in `impl/`.**
  `drop_physically` in `impl/address-table-operations.md#drop_physicallystmt` releases a non-existent id's tombstone once the id's mentions fall to 1, assuming the one mention left is the tombstone.
  When the statement dropped *is* the tombstone -- the header's take, or a page rewrite, drops every statement of its page -- the mention left is a dead statement of the id's previous incarnation, which the tombstone was denying.
  If the id is then re-allocated in the same flush, which recycling makes likely, `allocate` finds no tombstone to anchor the new incarnation on, the cut states no replacement, and after reopening the old incarnation's content resurfaces past the new size.
  The rule is skipped when the tombstone lives in the page being dropped: the cut restates it, or, for a re-allocated id, replaces it as the anchor with a `Shrink`; and it is released at the cut if nothing names the id any more.
  The randomized oracle never hit it; a benchmark replacing values in a key-value population did within two flushes, and `tests/churn.rs` now does too.
- **Ids a recovered journal allocates must be withheld from the id allocator.**
  The allocator is rebuilt at open from the loaded state, which knows nothing of the ids the unfolded journal brings into existence; before the fix, the first `alloc` after a recovery could hand one of them out again.
  `impl/id-recycling.md` should say that recovery counts the journal's ids as used; the consolidator state, whose allocation the recovery flush creates, exposed it.
- **Misusing the transaction and batch calls reports `Error::Corrupt`.**
  Ending a transaction that is not open is a caller's bug rather than a damaged file, and deserves an error variant of its own.
  The typed layers pair the calls through guards, so only direct users of `Store` can hit it.

## Typed layers

- **Every guard mutation is one transaction.**
  `rust/tutorial/durability.md` promises that "a partial mutation is never visible", but a mutation is often several records: a `push` of a string allocates, writes the string's bytes, and writes its pointer into the vector's new slot.
  The guards wrap every mutation in `WriteBackend::atomically`, which appends it as one transaction and discards it cleanly if it fails midway, so no ordering discipline between the records is needed for crash consistency any more; the containers keep "publish, then free" all the same.
  A consequence for `rust/containers.md`: a push is a transaction of a `Resize` and the element's writes, not the single `Write` it describes, since growing first keeps it correct for an element whose `store` writes fewer bytes than its inline size.
- **Guards dereference to their value but not mutably.**
  `rust/derive-macro.md` lists `Deref` *and* `DerefMut` on generated guards, but `DerefMut` lets `guard.field = value` compile and persist nothing, which is exactly what guards exist to prevent.
  The guards implement `Deref` only; `Guard::as_persistable_mut` remains as an explicit, documented escape hatch for container implementations.
- **A derived enum writes zeros where a smaller variant leaves bytes unused**, so that `store` always writes exactly `INLINE_SIZE` bytes and a value's bytes do not depend on what the slot held before.
- **Loading reads sizes as of the last flush**, through a `ReadBackend::read_size` added for it: `Backend::size` includes operations not yet flushed, and a vector loaded between a write and the next flush would otherwise count elements its reads cannot see.
- **`PersistableVec` reads through `Deref<Target = [T]>`**, so a slice's whole read API works on it; the whole-value `set` takes a `PersistableVec`, and the byte-level replacement `PersistableString` uses is `PersistableVecGuard<u8>::set_bytes`.
- **Removing an entry from a `PersistableHashMap` frees its key**, and an `insert` that replaces a value frees the key passed in, since the map keeps its own; `rust/freeing.md` discusses values only, but a `PersistableString` key owns an allocation too.
- **`PersistableBlob`'s in-place edit is a closure, `update(|value| ...)`**, rather than a handle that persists when dropped, since `Drop` cannot report the error that recording now returns.
- **`Kladde::create_in` and `Kladde::open_in` take any `Storage` and `Options`**; `Kladde::new` is `create_in` with a `MemoryStorage`, and cannot fail.
  The root's allocation and the descriptor table's are ordinary allocations, and count in `Kladde::stats`.

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
  It takes its victims before the cut, whose layout then needs about one more leaf per offer; the budget counts it as a page all the same, and holds it to the fill floor like a data offer.
  Since its restatements join leaves the cut packs full anyway, the fill floor could be dropped for table offers, with the budget charged their estimated restatements in fractions of a page.
  That was tried and not kept: with 64 MiB of uniform overwrites it raised the fill of table pages from 0.59 to 0.70 but shrank the file only from 1.53 to 1.51 times its live size, and made the median flush 38 % slower; at 1 MiB the file came out larger.
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
- **The controller moves the budget multiplicatively, and measures fill against the whole file.**
  After each commit it divides the live bytes by the capacity of every page but the headers, and multiplies the budget by `exp(4 · (τ − fill))`, within `budget_min` and `budget_max`; `impl/consolidation.md#constants-still-to-be-chosen` leaves the rule open.
  `impl/consolidation.md#the-churn-floor-is-a-parameter-not-an-identity` says "`live_bytes / (pages · C)`" without saying which pages.
  Counting only live pages would let holes go unnoticed: in a test that frees three quarters of a file, the fill of the live pages stayed near 1, the budget fell to its minimum, and compaction mode moved one page per flush.
  Counting every page makes holes raise the budget like sparse pages do, which also matches the bound the budget is meant to buy, a file of `live_size / τ`.
- **The consolidator state's layout is this implementation's own**, as `spec/file-format.md#the-consolidator-state` allows: the tag `kladders`, `up_to_date`, the budget as an `f32`, the window's key, the snapshot's length, and then the age records, as `impl/consolidator-state.md` describes them.
  A fresh snapshot is due once the appended records outgrow the snapshot, or 64 bytes if the snapshot is smaller, so that a nearly empty snapshot does not force one every flush.
  A state that does not parse is overwritten in place, keeping its allocation.
- **An age record is current if no *live* statement naming its allocation is newer than `up_to_date`.**
  `impl/consolidator-state.md#checking-it` asks about every statement naming it; statements that are physically present but dead are not in memory after a load.
  A dead statement newer than every live one is rare (a `Grow` below the size, from another writer), and the cost of missing it is an age that errs toward old.
- **The consolidator state's allocation is hidden from `Store::allocations` and the statistics**, since the application never allocated it.
- **An offer holding compaction mode's tail passes the churn floor and the fill floor unconditionally — a gap in `impl/`.**
  `impl/consolidation.md#compaction-mode` argues that a tail victim at fill `u` returns a whole page for `u · C` written, a ratio of `1 / u` that passes a floor near 1 even for a full page, "so it is the budget that paces the mode".
  With restatements estimated at 1.25 times coverage, a table page more than 80 % full fails `λ = 1`, and at first it did not even fit one leaf's offer.
  In the 64 MiB `shrink` benchmark, a leaf full of `Inline` patches at the end of the file stopped compaction for 180 flushes, until the rotating window had drained it statement by statement.
  The tail is now offered whatever its size, and the budget alone paces the mode, as the design intends.
- **Compaction takes the tail only while a page that is reusable now lies below it — a gap in `impl/`.**
  Right after a mass free, the freed pages are in quarantine for two commits, and the ready pool may be empty although holes abound.
  Moving the tail then opens pages at the end of the file, which become the next tail, and the same content moves again: a test that frees three quarters of its file grew it by another 96 pages over 30 flushes and truncated twice rather than ten times.
  For the same reason, free filling moves the tail only into a page below it.
- **Compaction mode counts holes by scanning the page table once per flush.**
  `impl/consolidation.md#compaction-mode` keeps a cached index of the highest live page instead; the scan costs `O(pages)` per flush, which is small next to what a flush writes, but is not the `O(1)` amortised the design promises.
  Interior pages are passed over like journal pages, since every cut that needs them writes them afresh.
- **A hole is a page that is reusable now, not one in quarantine.**
  Counting quarantined pages too kept the mode on in 30 of 33 flushes of a 1 MiB file whose flushes rewrite a quarter of it: the pages the last commit released are working space, which the next flushes reuse by themselves, and moving the tail meanwhile only cost table rewrites and a file 6 % larger.
