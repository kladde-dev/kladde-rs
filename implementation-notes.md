# Implementation notes

Where this implementation deviates from kladde-docs, what the documentation leaves unclear, what could be done better, and what went wrong along the way.

The design lives in kladde-docs, so findings from implementing it are recorded here first, and proposed for the documents from here.
Paths like `spec/journal.md` are relative to kladde-docs' `content/`.

## Where the implementation falls short

What the code does not do yet, or does less well than kladde-docs or its own goals ask, and what that costs.

- ⚠️ **The span table has two levels only: the header, and the leaves it names.**
  A leaf that names table pages of its own is refused at open.
  The header's room for references to leaves, one or two bytes each, bounds a file to some 2,000 to 4,000 leaves, one to two million spans: a sequence of a few hundred MB in spans of the length the measurements find, and far more than any workload measured, the largest of which takes 21 to 26 leaves.
  Beyond the bound, a flush fails with `Error::OutOfBounds`, and since opening flushes the replayed journal, a file whose journal holds such a flush no longer opens.
  The fix, interior table pages and the path copy of `impl/consolidation.md#unlinking-an-emptied-page`, is recorded in `later.md`.
- **The decode recurses without a limit on its depth**, where `impl/opening.md` asks for a limit or an iterative decode, so a file that nests deeply enough overflows the stack.
- **A container's index panics on sizes of 4 GiB or more**, where `spec/span-table.md#bounds` asks every limit to fail cleanly: the Fenwick tree keeps sizes in `u32` cells, and converts with `expect`.
- **The load checks a reached `Ref` against its page's frame, not against the page's `content_size`.**
  It refuses a `Ref` into page 0 or 1, into a page that is not a `Data` page of the header's world, or reaching into the framing, but not one that reaches past the content into the padding.
- **Nothing is kept in the private chain**, where `impl/consolidator-state.md` designs what consolidation would carry there: every session starts its budget from `Options::budget_pages`, its rotating walk at the sequence's start rather than where `impl/opening.md` seeds it, and each span's heat from the epoch of the page that holds its entry.
- **A span costs about 90 bytes in memory, not 32.**
  The span table takes 48 bytes per span, since each record also holds the span's chain, its rope node, its heat, its entry's size and place in its table page, and its place in its data page's reverse list, which `impl/in-memory-state.md` keeps beside the span table; and the rope is an implicit treap of 40 bytes per node rather than a counted B-tree of about 15 per span.
- **The vector's index takes 40 bytes even where it is empty or the elements are fixed-size**: a `Vec` of cells, its cached total, and the count's varint length.
  The total can live in the tree's allocation, and the count's length is the vector's to pass in, which would leave 24 bytes, or 8 with a thin vector.
- **None of the three refinements of `impl/consolidation.md` is implemented**: evacuation writes a victim's spans as they are, splitting the one that crosses the end of the page it fills, and merges no neighbours; the walk merges runs of cold spans and renumbers nothing; and the churn floor, budget controller and free filling are the address-table design's, unchanged, over pages of spans.
- **The header's eviction ranks by heat alone**, rather than evicting the coldest runs contiguous in id, and merges each evicted span with its neighbours in the sequence, extending each run while a neighbour whose entry also goes to a leaf makes a pair shorter than the bound.
- **A value is encoded into a buffer of its own, which the store copies into the record**, a copy that a store method taking an encoding closure would save.
- **Cuts exist on the vector only**: `PersistableVec`'s `cut`, `insert_cut` and `push_cut`; the map, fields, `Option` and enums neither make nor take them.
- **`split_at_mut` and the count of changes are not implemented**, only `parts()`, for derived structs and enums and for tuples.
  `split_at_mut` was left out only because the evaluation needs it nowhere; the vector's Fenwick tree lies in cells and answers any element's offset, so it would serve as the two runs' index as it is.
- **`parts()` on a temporary guard works in a `match` but not in a `let … else`**, which drops the temporary at the end of the statement.
  A consuming `into_parts(self)` would make the `let` form work too; it would need a line in `rust/derive-macro.md` and `rust/tutorial/deriving.md`, and is proposed rather than built.
- **`MemoryStorage` models a power cut as any subset of the writes since the last sync, each write whole.**
  It does not model a write torn within a page; the page CRCs are meant to catch those, but no test exercises them yet.
- **Misusing the transaction and batch calls reports `Error::Corrupt`.**
  Ending a transaction that is not open is a caller's bug rather than a damaged file, and deserves an error variant of its own; the typed layers pair the calls through guards, so only direct users of `Store` can hit it.

## Choices kladde-docs leaves open

Where kladde-docs leaves a question open, or allows any answer, and what the implementation chose.

- **The journal is replayed by folding the recovered transactions in a flush at open**, before the decode, so that the decode reads the state after the last of them and needs no replay of its own; `impl/opening.md#replaying-through-a-flush` allows it.
- **The rope is an implicit treap**, one arena type of under 400 lines that also serves the fold's piece table and a transaction's cuts, since a split at a position and a join of two trees are all three need.
- **The span id allocator hands out the id after the predecessor's, if it is free, and the lowest free id otherwise**, which keeps the spans a flush makes one after another consecutive without looking for a run.
- **The flush makes a run of 32 zeros or more among new bytes a `Zero` span.**
- **The span policy's constants**, chosen by the measurements of `evaluation/one-sequence.md`: `Options::span_min`, the bound `L`, 64 bytes; `Options::rewrite_below`, 64 bytes; `Options::inline_threshold`, 64 bytes; `Options::cold_flushes`, 16; `Options::walk`, 256 spans; and `Options::mu`, 0.02.
- **A `Box<T>`'s `FIXED_SIZE` is `None` whatever `T`'s**, since a type that holds itself through a box would otherwise ask its own size of itself while rustc evaluates it, which rustc refuses as a cycle (E0391); a vector of boxes then keeps an index it could do without.
- **The guard of a `Fixed<T>` replaces the value whole**, since a guard of one of `T`'s fields would have to write that field in its fixed form, which would take a second guard for every type.
- **The map's hash table is its own**: open addressing over 32-bit positions, probed linearly and at most half full.
- **`FileStorage` grows the file with `set_len`, not with `fallocate`.**
  The new region stays unallocated until it is written, so a full disk shows up as a failed write or `fsync` during a flush, which poisons the store, rather than as a failed growth before the flush starts; `spec/durability.md#headroom` asks only that failure be clean, which poisoning provides.
- **The controller moves the budget multiplicatively, and measures fill against the whole file**: after each commit it divides the live bytes by the capacity of every page but the headers, and multiplies the budget by `exp(4 · (τ − fill))`, within `budget_min` and `budget_max`.
  Counting only live pages would let holes go unnoticed.
- **A page's record holds what its state has, and nothing else**: an enum over the page states, with a state of its own, filling, for pages a flush takes, and journal pages apart from live ones; only a live page has an epoch and a victim bucket.
- **The churn floor also bounds each victim**, and an offer takes whole victims by score, not by best fit.
