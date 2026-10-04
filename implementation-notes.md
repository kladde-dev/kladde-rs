# Implementation notes

Where this implementation deviates from kladde-docs, what the documentation leaves unclear, what could be done better, and what went wrong along the way.

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

- **`last_written` is the epoch of the flush that last wrote the id, a `u64`.**
  `impl/in-memory-state.md#3-the-allocation-map` sketches a `u32`; epochs are 64-bit in the format, and ages are computed as epoch differences, so the wider field needs no wrap-around rule.
- **`Origin::Arena` holds a `u64`.**
  `impl/consolidation.md#how-a-flush-and-consolidation-compose` halves `Origin` with a 32-bit arena position and a flush forced before the arena outgrows it.
  The implementation keeps 64 bits and relies on the journal budget to keep the arena small; a single transaction is still capped at `2^32 - 1` bytes by the journal's length prefix.
- **A page whose coverage reaches zero leaves its bucket at once.**
  It is retired at the next commit either way, and keeping it would waste the samples that victim selection draws from the sparsest bucket, which is exactly where such pages collect.
- **The statement slab also keeps each statement's id and kind: 14 bytes per slot, where `rust/store.md#the-statement-slab` has 9.**
  Releasing a statement charges its framing to its allocation's `statement_bytes`, the page rewrite and the window ask which ids a page names, and several paths ask whether a statement is an `Inline`.
  Without the two arrays, each of those would decode the statement from its page again.

## Flush

- **Bytes an allocation shifts within itself are restated; bytes from another allocation, and copies, are read and written afresh.**
  A piece that the fold sources from the same id's committed bytes at another offset — the tail a `Splice` shifts, or a range a `Move` within the id relocates — becomes the fragments those bytes lie in, each taken at its new offset where it lies: a `Ref` keeps its address, an `Inline` payload is copied into the new table page as the cut copies every payload, and zeros become a `Zero` below the old size.
  That is the shift of `impl/flush.md#per-record-rules`, applied only where it is a transfer: a piece is restated only if no other piece of the id names the same committed bytes, so every data byte stays referenced by at most one live statement, and the bytes a segment copied within an id are written afresh, in both places, at piece granularity.
  A piece is also written afresh when describing it would cost more than its bytes, by estimate: about 9 bytes per `Ref`, a payload and 4 per `Inline`, 4 per `Zero`, against the bytes and a `Ref` per page they fill, which happens only for tails made of fragments of a few bytes.
  Pieces from another id — `Copy` and `Move` between allocations — are still read and written afresh, so the flush needs no ordering phase (`impl/flush.md#phase-c--ordering`): a restated fragment moves no bytes, and the only care it needs is the reverse index's rule 1, widening the window of the page it lies in to its new offsets.
- **The header's eviction ranks by allocation, not by fragment.**
  A pending fragment's heat is the number of flushes since its allocation's `last_written`, where `impl/flush.md#the-header-as-write-buffer` keeps an eviction clock per fragment.
  All statements of one allocation therefore stay in the header or leave it together, which is coarser for a large allocation with a hot tail and a cold head, but needs no clock at all, and `impl/consolidator-state.md#what-stays-out` seeds the clock from the same content ages anyway.
- **The interior layer is rebuilt by every cut that needs one.**
  `impl/flush.md#the-shape-of-the-tree` path-copies from the changed leaves up.
  With the header naming up to 600 leaves directly, a file needs an interior layer only past that, and rebuilding it costs one page per 800 leaves per flush.
- **`Store::open` folds a recovered journal before it returns.**
  `spec/journal.md#the-start-of-a-session` asks only that a non-empty recovered journal be folded before anything is appended.
  Folding at open is simpler, and it lets reads, which see flushed state only, see every recovered transaction at once.
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
- **A derived enum's generated code bound its variants' fields by their own names**, so a field called `backend` or `location` shadowed the parameter of `store` it was stored with, and did not compile.
  The bindings are prefixed now; `tests/enum_derive.rs` has such an enum.
- **`parts()` on a temporary guard works in a `match` but not in a `let`**, for enums as for structs: it borrows the guard, and a `let … else` drops the temporary at the end of the statement, so `let ShapeParts::Circle(r) = guard.kind_mut().parts() else { .. }` fails with "temporary value dropped while borrowed", while a `match` keeps it alive to its end.
  kladde-svg's benchmark bound the guard with a `let` of its own at three of its nine `parts()` calls.
  A consuming `into_parts(self)`, handing out the field guards for the guard's whole lifetime, would make the `let` form work too; it would need a line in `rust/derive-macro.md` and `rust/tutorial/deriving.md`, and is proposed rather than built.

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
  A dead statement newer than every live one is rare (content past the size, from another writer), and the cost of missing it is an age that errs toward old.
- **The consolidator state's allocation is hidden from `Store::allocations` and the statistics**, since the application never allocated it.
- **`close` truncates after the last live page, and a new file's free pages lie below it.**
  `rust/tutorial/durability.md` said that `close()` "flushes and then shrinks the file to its live pages"; it now says that it does not, and that calling `flush()` a few times before `close()` reclaims more.
  Right after `Kladde::create`, the file holds free pages ahead of its data, most likely the pages the creating journal took; the code has not been checked for it.
  Space a flush frees becomes reusable only from the next flush on, and compaction mode then moves the data down, so a new file shrinks to its live pages only once `close` follows one or two explicit flushes.
  Measured with kladde-svg's drawings, closed right after `create` with 0, 1 and 2 explicit flushes: the coat of arms, 203 KB live, at 173, 173 and 57 pages; the tiger at 89, 89 and 31; the world map, 972 KB live, at 869, 253 and 256.
  `close` could instead flush until compaction mode has nothing left to return, at the price of a slower close.
- **Compaction mode counts holes by scanning the page table once per flush.**
  `impl/consolidation.md#compaction-mode` keeps a cached index of the highest live page instead; the scan costs `O(pages)` per flush, which is small next to what a flush writes, but is not the `O(1)` amortised the design promises.
  Interior pages are passed over like journal pages, since every cut that needs them writes them afresh.
