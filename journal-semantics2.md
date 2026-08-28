# Journal and Flushing — Redesign

To optimize write performance, mutations to a `Kladde<T>` don't directly mutate the corresponding bytes in the kladde file.
They are instead recorded as a sequence of operations (*ops*) in a special allocation of the kladde file called the *journal*.
Whenever the journal reaches a threshold size it is *flushed*: the recorded operations get optimized to a minimal set of writes, these writes are applied to the on-file representation of the stored data in a crash-resistent way, and the journal is discarded.

The journal is an *optimization*, not a mechanism for atomicity (that's the job of [[../kladde-docs/content/spec/transactions-and-batches|transactions]]).
The journal is therefore mostly invisible to authors of application code or data type implementations — semantically, every mutation that an app performs is persisted as soon as its corresponding transaction (= single op or framed sequence of ops) is completely recorded in the journal.
Even if the application crashes, the next time kladde opens the file it will apply the journal before reading any data, so the application can't distinguish between mutations that have already been applied to the data and mutations whose ops are still queued in the journal, so both are considered persisted.
The only places where the journal is observable are:

- a small contribution to the file size *while the file is open* (upon closing, the journal will be flushed and a best-effort compaction is performed to reclaim its space);
- increased overall throughput of mutations at the cost of occasional small latency spikes for flushing (targeted at < 1 ms per flush but not yet measured; Claude: is this realistic? TODO: measure once implemented);

  > **Claude:** the right order of magnitude, but it is a target on the wrong variable, and one part of the current design will blow through it.
  >
  > Flush cost is roughly linear in journal size, so **the latency target should *set* the journal capacity, not be checked against it**.
  > Rough budget for a 64 KiB journal at ~20 bytes per op, so ~3000 ops:
  >
  > | phase | estimate | note |
  > | --- | --- | --- |
  > | fold | 0.3–1 ms | a few sorted-map operations per op, ~100 ns each; this dominates and is the part to measure first |
  > | plan | 0.05–0.3 ms | interval queries over pieces; see my note in Step 2 on avoiding the O(V²) trap |
  > | execute | 0.2–1 ms | ~1–2 µs per `write()` into page cache, times however many runs the plan emits |
  > | **total** | **0.5–2 ms** | |
  >
  > So < 1 ms is achievable at 64 KiB and clearly not at 1 MiB.
  > Since you get to choose the capacity, I would state the target as latency and *derive* the capacity, then re-derive it after measuring the fold.
  >
  > Two things that would break the budget, both worth designing around now:
  >
  > 1. **Compaction is inside step 5.** Compaction has no natural bound related to journal size, so bundling it into the flush makes the tail latency a property of the heap's fragmentation rather than of the journal.
  >    Either give it its own budget (it already takes one — `incremental-compaction.md`) and count that budget against this target, or move it out of the flush path entirely.
  >    My note under Step 5 argues for moving it out for an independent reason.
  > 2. **The `fsync` question.** The design says no `fsync` per transaction, which is right, but step 5 as written has no `fsync` either — see my note there on what that costs.
  >    If an `fsync` ever enters the flush path the budget becomes a device property (0.1–10 ms on an SSD, tens of ms on spinning rust) and everything above is noise.
  >
  > Worth also stating the *frequency*: at 3000 ops per flush and, say, 100 ops/s of interactive editing, that is one spike every 30 seconds, which no user perceives.
  > The number that matters for perceived latency is spike duration × spike rate, and it is the rate that makes this design comfortable, not the duration.
- while kladde gives the journal precedence over any stale data and therefore never hands out stale overwritten data, forensic tools may be able to recover outdated data before the journal is flushed (this is a somewhat pedantic since closing the file ordinarily will flush the journal, and since stale data can prevail in kladde files anyway due to fragmentation unless [[privacy-cleanups]] are performed); and
- while the journal is guaranteed to survive an application crash, it is not guaranteed to survive a power outage: a power outage may reset a file to its state after the last flush (which is never in the middle of a transaction). Lifting this limitation would make mutations orders of magnitude slower.
  A power outage additionally carries a narrower risk to *content*, because the unit of writeback is the page rather than the byte range that was written, so bytes that were never modified can be damaged as passengers in a page that was in flight.
  kladde's guarantee is therefore [[#durability-guarantees|tiered]]: the file's structure always survives and it always opens, and torn content is **detected and reported** rather than silently returned.

## Guiding principles
### Goals and non-goals

The journal is an optimization that makes persistence of many small edits cheaper, especially for storage on a device where sequential access is cheaper than random access.
It is tied to a flushing mechanism that applies the journal to the rest of the kladde file and then releases the journal.
The journal is *not* an undo buffer, and flushing is *not* a mechanism for atomicity. In detail,

- Every transaction (and every op outside of a transaction) is considered committed as soon as it is fully recorded to the journal.
  Flushing only changes how the effect of these transactions is represented internally in the kladde file (as actions or as their resulting state), not whether they have occurred.
- Atomicity is enforced at the level of transactions, not at the level of the journal.
  Since transactions are typically relatively small in kladde, the journal typically accumulates many transactions, which are flushed all at once when the journal is flushed.
  A crash during flushing doesn't affect which transactions will be recovered — the effects of all transactions will be recovered when kladde re-opens the file.
- Since all transactions are already considered committed once they hit the journal, flushing may ignore transaction boundaries and reorder, merge, or optimize out ops over the entire journal, not just within each transaction.
  This is beneficial in typical editor applications where mutations typically happen in bursts at nearby locations, sometimes even overwriting each other.
  Flushing only has to apply the net result of these burst edits.

### Resulting requirements

The above goals impose the following requirements:

- Appending to the journal should only involve sequential writes (no random access writes) and, more importantly, no `fsync`.
  Calling `fsync` for every transaction would introduce an unreasonable performance overhead since transactions are typically relatively small in kladde.
- Writing to the journal must be crash tolerant.
  When the application crashes mid journal-write, kladde must be able to recover the state before the last transaction was written to the journal.
- Flushing must be crash tolerant.
  When the application crashes mid flushing, kladde must be able to finish flushing when it reopens the file even if flushing reordered operations.
- Flushing should minimize the amount of disk operations (reads and writes) and linearize them where possible.
  Compute and memory access is much cheaper than disk operations, so we'd rather spend more time preparing an efficient plan for flushing than executing an inefficient one.
- The journal should not impose too much overhead (regarding both disk space and I/O operations) even if transactions are relatively short and thus plenty.
  At the same time, some overhead per transaction is unavoidable, which is why the design of `kladde-persist` should encourage bundling multiple ops into a single transaction.
  However, it should also not steer application authors into making exorbitantly large transactions since those would require the journal to grow very large (aiming at a middle ground of not too short but also not too large transactions is what motivated [[batches]]).
- Correctness is paramount, which is why invariants should be clearly stated in this document, and for every introduced complexity, its benefits must be weighted off against the risk of bugs.

## The journal

The journal is an append-only sequence of *transactions*, where each transaction consists of a single or multiple operations (*ops*).
Every transaction that is fully recorded in the journal is considered committed, i.e., its state change is persisted in the file and will survive an application crash.
A subsequent flushing operation does not promote the "level of persistence" (except that its `fsync` barriers make the effects durable against a power outage, subject to the [[#durability-guarantees|tiering]] below).

### Transactions and ordering discipline

Mutations are recorded to the journal in units of transactions, where each transaction wraps a sequence of ops.
Journaling operates only on a flattened list of transactions — [[../kladde-docs/content/spec/transactions-and-batches|batches and nested transactions]] are higher-level user-facing concepts that the backend translates to a flat sequence of transactions where every op is part of exactly one transaction (including free-standing ops, which the backend wraps in individual single-op transactions).

Kladde guarantees two properties for transactions:

- **Atomicity**: every transaction is either persisted completely or not at all, even if the application crashes or the device suffers a power outage (in the latter case subject to the [[#durability-guarantees|content tiering]] — a transaction is never *partly* recorded, but a power outage can damage bytes elsewhere in a page a flush was writing).
- **Immediate persistence:** *as soon as a transaction is fully recorded in the journal*, it is considered persisted to the file and will survive an application crash (but not necessarily a power outage, which may reset the file to an empty journal or a prefix of the journal up until any transaction boundary, and which carries the separate content risk covered in [[#durability-guarantees]]).

The immediate persistence property imposes an **ordering discipline** on authors of application code and type implementations, which the atomicity property allows them to break temporarily:

> **Every transaction must transform a valid state to a valid state** (*within* transactions, invalid intermediate state is allowed).

Here, what exactly "valid state" means is up to the application author.
The point is, if the app crashes while it performs a sequence of transactions, the next time kladde loads the file it will recover the state from after the last completed transaction (note that [[../kladde-docs/content/spec/transactions-and-batches|batches and nested transactions]] are resolved at the time ops reach the journal, so if an application uses these then the last transaction boundary in the journal may lie further in the past than the last innermost nested transaction boundary).

Since recording a transaction to the journal makes it immediately persistent, transaction boundaries cease to be meaningful from the point the transaction has been fully recorded to the journal, and they only become relevant again if the application crashes and kladde has to recover the last transaction boundary.
Thus, *flushing is oblivious to transactions*.
Flushing may (and does) reorder, combine, and annihilate ops across transactions as long as this leaves the resulting state invariant since flushing only changes *how* the current state is represented in the file, not *what* the state is (and since the entire flushing operation is crash-resistent in itself, see below).
Note that the order of recorded ops still matters, even within transactions: writing data to some allocation and then copying some content of that allocation to a different allocation copies the data that was just written, while recording the same two ops in reverse order copies the old data and then overwrites it only in the source allocation, leading to a different final state.
This holds regardless of whether the two operations are part of the same or different transactions.
Atomicity does not erase order of operations, it only guarantees that intermediate state is never materialized.

### Journal operations (ops)

The vocabulary of ops that can be recorded in the journal is deliberately **type-agnostic**.
Ops express byte-level mutations of allocations and changes to the shapes and existence of allocations, not application-level operations.
There is no "push an element into a `PersistableVec`" op and no "insert a (key, value) pair into a `PersistableHashMap`" op — the type implementations map these to (transactions containing) one or more of the multiple of the low-level ops below.
This type agnostic op vocabulary is a real trade: it means replay never dispatches on an application type, never needs a type registry, and never has to call back into a container implementation, which in turn is what makes a kladde file readable by generic tools (e.g., a future kladde file inspector or compactor) even if they don't know all opaque data types used in the file.
What the type agnostic op vocabulary gives up is the ability to collapse operations that cancel only at the semantic level bot not on the level of memory allocations (e.g., rope operations that change the shape of the rope but are semantically a no-op).

The following ops are defined:

| Opcode | Operation and payload                         | Effect                                                                                                             |
| ------ | --------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| 0      | `Alloc(id, size)`                             | brings a memory allocation with `id`, `size` (and sizedness inferrable from `id`) into existence                   |
| 1      | `Free(id)`                                    | releases the memory allocation with `id` (queuing `id` for recycling after the flush)                              |
| 2      | `Resize(id, new_size)`                        | changes `id`'s size, preserving `min(old, new)` bytes and appending `max(0, new-old)` bytes of uninitialized data. |
| 3      | `Convert(old_id, new_id, new_size)`           | `Resize` with additional changes of sizedness, minting `new_id` and queuing `old_id` for recycling after the flush |
| 4      | `Write(id, offset, bytes)`                    | overwrites `bytes.len()` bytes at `offset` within `id`                                                             |
| 5      | `Splice(id, offset, old_len, bytes)`          | replaces `old_len` bytes at `offset` with `bytes`, shifting the tail and resizing                                  |
| 6      | `Copy(src, src_offset, len, dst, dst_offset)` | copies a byte range between (or within) allocations, overwriting at the destination                                |
`Splice` and `Copy` are the only records that read existing content, which matters to the folding phase of flushing, see below.
(TODO: reconsider this sentence once fold is formulated.)

TODO: maybe remove `new_size` from `Convert` and require caller to manually emit `Convert` *and* `Resize` in the correct order.

> **Claude: keep `new_size`, and document the internal order instead.**
>
> The reason the order is delicate is real: `Resize` is only meaningful on a resizable allocation, so a fixed→resizable conversion must convert first and resize second, while resizable→fixed must resize first and convert second.
> But that ordering is a *function of the sizedness*, which is inferrable from `old_id` and `new_id` — so the op can determine it itself, and there is nothing the caller knows that the op does not.
>
> Pushing it to the caller therefore does not remove the subtlety, it just relocates it to every call site and makes getting it wrong the caller's privilege.
> Since the wrong order fails in a way that is silent (a resize applied to the wrong sizedness, or content dropped), that is a bad trade for the varint you save.
>
> Two further points in favour of one op:
>
> - It is atomic in the journal without needing a transaction, so a data type doing a conversion does not have to open one.
> - The common case is `new_size == old_size` — a pure relabel with no bytes moved, which is what makes a chunked container oscillating across a chunk boundary cheap.
>   With two ops, that case costs two journal entries where one would do, and the fold has to recognise the pair to prove nothing moved.
>
> What I would add to the table is an explicit statement of the internal order, something like: *applies the size change on whichever side of the sizedness change is resizable*.
> That is one clause, and it puts the knowledge in the one place it belongs.
### On-storage representation

Although the journal is not implemented as a `Persistable` type, it has a similar structure as most `Persistable` types: it has an in-memory and an on-storage representation, where the on-storage representation is authoritative and the in-memory representation optimizes read access but can be fully derived from the on-storage representation.

The on-storage representation of the journal sits inside a regular allocation of the kladde file that is tracked by the heap like any other allocation.
It contains a header, a sequence of serialized and framed transactions, and an optional tail with arbitrary bytes (potentially left over from a previous use of the address region).

```
journal      := header transaction* tail:byte*  ; tail is ignored; it may be any data left over from before the last flush
header       := salt:crc                        ; CRC of previous journal before it was flushed
transaction  := (op | multiple_ops), crc        ; CRC is cumulative over `salt` and all `op`s to this point, see below
op           := opcode:byte payload:byte*       ; payload length and format is determined by opcode, see below
multiple_ops := (count_tag:varint, op{count})   ; count_tag = count + 5 (where count > 1; count_tag >= 7 avoids clashes with single op)
crc          := byte{4}                         ; 4-byte checksum
```

The on-storage representation is designed to be compact but complete.
It only needs to be read from disk when recovering from a crash; normal flushing operations consult its mirror in the in-memory representation instead, see below.
The `payload` of ops is serialized as the concatenation of the parameters listed in the column "Operation and payload" in the above table of ops, in the stated order, where all integers (including pointers ids) are encoded as varint and `bytes` is encoded as a varint `len` followed by `len` bytes.

The CRCs in combination with the `salt` buy crash resistance: after a crash, kladde can recover the end of the last fully recorded transaction by reading until either the end of the allocation or until a wrong CRC is reached and then back-tracking to the last correct CRC (see [[#recovery]]).
Since the `salt` changes after every flush, any stale data in the journal allocation that contains previously valid transactions from a previous journal have become invalid as soon as that previous journal was flushed.

**Possible alternative:** If the CRCs turn out to eat up too much journal size, we might consider explicitly clearing out the journal allocation with zero bytes when it gets first allocated and after every flush (unless it's at the end of the file, in which case we can simply shrink the file), which would allow us to mark the end of each transaction with a single nonzero byte rather than a 4-byte CRC.
Claude: briefly discuss the trade-offs.

> **Claude: don't do this — it trades 3 bytes per transaction for a full journal write per flush, and gives up torn-write detection.**
>
> **The I/O arithmetic is decisive.** Zeroing a 64 KiB journal costs 64 KiB of writes at *every* flush.
> The saving is 3 bytes × transactions-per-flush; at ~3000 single-op transactions that is ~9 KiB.
> You would be writing 64 KiB to save 9 KiB, and doing it on the latency-sensitive path.
> The ratio only gets worse as transactions get larger, because the CRC is already amortised per *transaction*, not per op — a batch of 50 ops pays one CRC.
>
> **It also does not detect what a CRC detects.** A zero terminator only distinguishes "written" from "never written".
> A torn write leaves *arbitrary* bytes, not zeros: a `write()` interrupted mid-buffer, or a page the OS wrote partially.
> Those bytes can parse as a plausible op and will be replayed.
> The CRC is not really buying you end-of-journal detection — that is the cheap part — it is buying you *validity* of what precedes it, which is the whole reason recovery can trust the journal at all.
>
> **Three smaller problems**, any one of which is survivable but which add up:
>
> - Zero is currently a valid opcode (`Alloc`), so you would have to reserve it and shift every opcode by one.
> - It breaks the "tail may be arbitrary leftover bytes" property in the grammar above, which is otherwise a nice thing to be able to say.
> - The exemption for a journal at the end of the file ("simply shrink the file") makes the journal's cost depend on where it happens to sit, which will make performance mysteriously bimodal.
>
> **If CRC space genuinely becomes a problem, cheaper levers first:**
>
> 1. **Encourage batching**, which the design already does — this is the intended answer, and it is why per-transaction rather than per-op framing was the right call.
> 2. **Shrink the CRC to 2 bytes.** A 1-in-65536 chance of accepting a torn tail is poor for a general-purpose format, but note the chained construction means a false accept must *also* produce a valid chain for every subsequent frame, so the effective probability is far lower than 2⁻¹⁶ for anything but the very last frame.
> 3. **Only then** consider structural changes.
>
> One thing the current design already gets right and should keep: **the salt makes stale frames from a previous journal fail their CRC**, so you get the invalidation that zero-filling would provide, without doing any writing.
> That is strictly better than clearing, and it is worth saying so explicitly in the paragraph above, because it is the reason clearing looks tempting and isn't.

### In-memory representation

The in-memory representation of the journal maintains the data structures:

- `committed: &[u8]` — a mirror of the ops that are committed to the on-storage representation of the journal, without the journal header (salt) and the transaction framing (`count_tag` and `crc`); thus `ops := op* = (opcode:byte payload:byte*)*`;  a mirror of the on-storage representation.
  See [transactions and Batches](../kladde-docs/content/spec/transactions-and-batches) for an explanation how the backend maintains `committed` as a prefix of a buffer `ops: Vec<u8>` that grows any time a transaction is committed.
  The salt and transaction framing are stripped from `committed` because their only purpose is to make the *process of writing a transaction to the file* transactional.
  Including them in them in the in-memory representation would complicate parsing `committed` unnecessarily.
  Storing the in-file representation of committed ops in a serialized form rather than as `Vec<Op>` will likely simplify indices into literals and potentially reduce the number of heap allocations significantly, although but I'll have to check if this really works out (TODO; we'll probably still need to pass an `enum Op` around internally, so in order to reduce the memory allocations that enum would have to hold binary data by reference or `Cow<[u8]>`).
- `pending: HashMap<Pointer, Option<Size>>` — a *state snapshot* of any allocation whose geometry has changed since the last flush.
  `Some(size)` records a new size; `None` is a **tombstone** marking an allocation that has been freed but whose `Free` has not yet been applied to the heap.
  Unordered, last-writer-wins.
  Updated whenever operations are appended to `ops` (not `committed`), and repopulated after flushing from whatever ops remain past `committed_cursor` (see below).
  It is used by the backend to resolve size and liveness queries — any entry in `pending` has precedence over the heap.
- There is no analog to `pending` for the *contents* of changed allocations — kladde never serves any content from the file to application code or data type implementations while `ops` is non-empty.

**Why tombstones rather than simply deleting the entry.**
A `Free(id)` that is recorded in `ops` but not yet flushed leaves the heap's `id → address` entry intact, so without a tombstone `resolve` would fall through to the heap and report the allocation as live at its old address and size — the *pre-flush truth*, which by then disagrees with the logical state.
The tombstone costs one discriminant per touched allocation and nothing in the common path, and it buys three things: `resolve`/`size` are correct rather than merely documented as approximate; a **double free** is caught where it happens rather than at the next flush, where the fold would hit `Free` on an id it has already released; and live-allocation counts and any future leak checker keep working while a journal is outstanding, which is most of the time.

**Invariant: an id's counter returns to the pool only at flush**, after the corresponding `Free` has been applied to the heap.
Within one journal, every id therefore names at most one allocation.

This has to be stated because the natural implementation of `Free` recycles the counter at once, and that would be a soundness bug: the journal and every structure derived from it are keyed by id, so

```
Alloc(id, 8)      # id minted
Write(id, 0, …)
Free(id)          # if the counter were returned to the pool here …
Alloc(id, 16)     # … the same counter could be handed out again — same id!
Write(id, 0, …)
```

would put two allocation lifetimes under one key, and the fold could not tell them apart: it cannot emit two claims for one key, and the first `Write` would be attributed to the second allocation.
The id pool therefore defers its free-list pushes to the cleanup step, where "recycle every id with a tombstone" is exactly the action to take — so the tombstones and the invariant implement each other.

## Flushing

Flushing can be triggered either manually or by the mechanisms described in [transactions and batches](../kladde-docs/content/spec/transactions-and-batches) and consists of five steps.

1. **Folding:** iterates over `committed` to create a complete representation of the state transition from the state at the beginning of `committed` to the state at the end of `committed`: a *piece-table* of what goes where from where, and (probably) lists of newly minted and recycled allocation ids(?).
   Doesn't yet tell *how* to achieve that state transition.
2. **Planning:** calculates a sequence of operations (the "plan") that achieve the effect found by folding.
   Loosely speaking, planning is the inverse of folding, but the inversion is deliberately lossy: apart from using a slightly different vocabulary of operations, planning may recreate operations that don't interfere with each other in a different order than how they appeared in `committed`, and it won't recover any operations from `committed` whose effect is overwritten by later operations.
   This lossiness is by design, it's what makes flushing an optimizing compiler.
3. **Hoisting:** create a write-ahead log of any parts of the plan that will either be overwritten during the execution step below (in the case of cyclic dependencies) or that are difficult to exactly recreate deterministically from the on-disk state alone (TODO: check if there are any).
   Write the write-ahead log to the journal allocation or to a separate hoisting allocation.
4. **Executing:** apply the plan.
   This is the only step that changes the shape and content of any allocations.
   It therefore has to be idempotent, i.e., executing it multiple times (due to recovery after a crash) must lead to the same final state as executing it only once.
5. **Cleaning up:** advance the in-memory representations of the journal, invalidate its on-disk representation, and possibly run compaction (maybe allowing it to reclaim all or some of the storage space taken up by the journal).
   This step is deliberately called "cleaning up" and not "committing" because it does not change the observable state of the file.
   If the application crashes just before the cleanup step, then reopening the file at this point would just re-execute the plan, which changes nothing since execution is idempotent, and clean up then.
   The cleanup step only prepares the journal for future transactions to be recorded.

overwrite the salt with the CRC of `concatenate(salt, committed)` (which is also the CRC of the last committed transaction) to invalidate the existing journal and reset all in-memory data structures as described in [transactions and batches](../kladde-docs/content/spec/transactions-and-batches).

> **Claude — three things about this five-step shape, before the steps are filled in.**
>
> **1. The salt overwrite is the commit point of the whole flush, and its ordering is the single most important invariant in the document.**
> Your TODO in the on-storage section asks when the salt has to be updated; the answer is *last, and strictly after every write the plan makes*.
> Before that write, a crash re-runs the flush (harmless, by idempotence).
> After it, the journal is gone and the file must already be complete.
> There is no third possibility, which is what makes this design tractable — so I would promote it from a step-5 detail to a stated invariant:
>
> > **The salt overwrite is the only irreversible act of a flush.**
> > Every other write a flush performs may be repeated any number of times with the same result.
>
> It also wants to be a single small write that no device will tear — 4 bytes inside one sector qualifies.
>
> **2. Idempotence of execution is assumed but not yet earned, and it is where the hard problem lives.**
> Step 4 says execution "has to be idempotent".
> Writing bytes to a known address is idempotent; **choosing** an address is not.
> If the plan says "claim 200 bytes for id 7" and a crash happens midway, the re-run must place id 7 at the *same* address, or every write that referenced it lands somewhere else.
> There are only two ways to get that, and the document should pick one explicitly rather than leave it to step 3:
>
> - **Record the addresses** (in the hoist log), so re-execution reads decisions rather than making them.
> - **Re-derive them**, which requires placement to be a deterministic function of the pre-flush heap plus the journal — no hash iteration order, no time, no addresses-as-input anywhere in the fold, the planner, or the allocator.
>
> Recording is the more robust of the two: under re-derivation, the crash-recovery path re-runs the entire optimiser, so any nondeterminism *or* any planner bug corrupts the file on the path that is hardest to test.
> Re-derivation is cheaper in bytes, but it makes the placement algorithm **part of the file format**: any implementation that has to finish an interrupted flush must reproduce this heap's exact decisions, byte for byte.
> Recording is what keeps placement an implementation detail, because recovery reads the addresses instead of re-computing them — so the heap stays free to change its placement policy between versions, and a second implementation in another language only has to be able to *replay a list*.
> Either way, **this choice determines what step 3 is for**, so it belongs above step 3, not inside it.

> **3. The journal lives in a heap allocation, and the heap is what the journal protects.**
> This circularity is not addressed anywhere yet and it has sharp edges:
>
> - **Growing the journal** is a heap mutation. If it were journalled, it would have to fit in the journal it is trying to grow.
>   So journal geometry changes must be applied *outside* the journal, with their own crash story — probably meaning the journal allocation is fixed-size and pre-allocated, and "the journal is full" is a hard limit rather than something that resolves itself.
>   That connects directly to the oversized-transaction gap I flagged in the transactions document.
> - **Compaction must not relocate the journal** while it is live, or must do so as a specially-handled case, since the address is needed to find it at open time.
> - **The `id → address` table** has to be recoverable before the journal can be replayed, since the ops are id-based.
>   You correctly defer "how the journal allocation is found" to the self-hosting work, but note that finding it is not enough — replay needs the *whole* table as of the last flush.
>   That is the same durable-heap-snapshot requirement that recovery needs, so it is one problem, not two.

### Step 1: Folding

Folding walks `committed` once, in order, parsing ops as it goes.
It produces a complete description of the state transition without deciding anything about how to realise it.

**Output.** For every id the journal mentions:

- its **fate** — born this journal, died this journal, both, or neither;
- its **final size**; and
- a **piece table**: for a surviving allocation, where each of its final bytes comes from.

**Sources.** A piece table maps a segment start offset to a source:

```rust
enum Source {
    Literal(usize),      // an offset into `committed`, naming payload bytes of a Write/Splice op
    Storage(Id, usize),  // the *pre-flush* content of another (or the same) allocation
    Undefined,           // uninitialized; may legally hold anything
}
```

Sources are offset-relative, so advancing one by `k` bytes is meaningful.
That is what lets a `Literal` survive being partially overwritten: the surviving fragment simply names a position inside the original payload.

**The single most important property, and the one to state as an invariant:**

> Every `Storage(Z, k)` in a completed fold names **`Z`'s content as it was before this flush began** — never a value that this flush is going to produce.
> The id in a `Storage` source is always an id that **existed before this flush**.
> `Storage` sources are looked up in the *pre-flush* address map; destinations are looked up in the *post-flush* one.

This holds because `Copy` and `Splice` take *pieces from the source's current table*, not bytes.
By the time the fold reaches op *n*, the source's table already reflects ops 1…*n*−1, so the copy inherits their resolution rather than referring to them.
It is worth being explicit about this because almost everything downstream depends on it: it is why the plan can be reordered at all, why the dependency graph in Step 2 has the shape it does, and why hoisting (if needed) can read the file directly rather than having to interleave with execution.

**Starting state of a table.**

- **Fresh** ids (an `Alloc` for them appears in this journal) start as a single `Undefined` segment.
  Their content is fully symbolic; nothing about them ever needs to be read from the file.
- **Persistent** ids (live from before this journal) start as a single identity segment `{0 → Storage(self, 0)}`.

Every table therefore begins with exactly one entry, which is worth exploiting in the representation (TODO): the overwhelmingly common case is that it still has one entry at the end.

**Per-op rules.**

| op                                   | effect on geometry                   | effect on the table                                                                                |
| ------------------------------------ | ------------------------------------ | -------------------------------------------------------------------------------------------------- |
| `Alloc(id, size)`                    | born, size                           | fresh table, `Undefined` over `[0, size)`                                                          |
| `Free(id)`                           | died                                 | dropped                                                                                            |
| `Resize(id, new_size)`               | size                                 | clip, or extend the tail with `Undefined`                                                          |
| `Convert(old, new, new_size)`        | `old` dies, `new` born at `new_size` | move `old`'s table to `new` **unchanged**; then clip/extend                                        |
| `Write(id, offset, bytes)`           | —                                    | overwrite `[offset, offset+len)` with `Literal`                                                    |
| `Splice(id, offset, old_len, bytes)` | size                                 | overwrite, then shift the suffix segments                                                          |
| `Copy(src, so, len, dst, do)`        | —                                    | overwrite `dst`'s `[do, do+len)` with the pieces `src`'s *current* table holds over `[so, so+len)` |
Overwrite is the only non-trivial primitive: split at both boundaries (materialising each from the segment it falls inside), drop everything strictly between, splice the new pieces in.

**The running size matters, not just the final one.**
Every content rule above is bounded by the size *at that point in the journal*: `Write` is clipped to it, `Resize` needs the old value to know whether to clip or extend, and `Splice` needs it to locate the tail.
Keep it in the geometry record and mutate it as the walk proceeds; the value it holds when the walk ends *is* the final size, so there is no need for a second structure.

**Four things fall out without being coded as special cases**, and they are most of why folding is worth doing:

- **Repeated writes to the same range emit one write**, because the table holds exactly one source per byte by construction.
- **An allocation created and freed within one journal never touches storage at all**, and a `Copy` out of it resolves to literals rather than forcing it to be materialized.
- **An identity piece emits nothing.** `Storage(self, k)` with `k` equal to the segment start means the bytes are already where they belong; without this check every untouched region of every persistent allocation would be copied onto itself.
- **A `Convert` that does not relocate emits nothing**, because a relabel keeps the address, so every piece that was identity before is still identity after; this is worth a test.

  > **Claude: agreed on the correction, and it makes the property *stronger* than I had it.**
  > You are right that a chunked container's conversion is not size-preserving — the promoted chunk gains a `next` pointer and a length field, so `new_size > old_size`.
  > But the condition that matters is **non-relocating**, not size-preserving, and the growing case still qualifies whenever there is room above the allocation: the original pieces stay identity (the address test does not care that the extent grew), and the new tail is `Undefined`, which emits nothing either.
  > So a growing conversion that fits in place still costs **zero bytes**, and only a conversion that has to relocate costs a full copy.
  > That is the version worth stating, and it keeps the motivation intact — the oscillating-chunk workload is cheap because the chunk stays put, not because its size is unchanged.
  > It also connects to the lift machinery in `augmented-segment-tree.md`, which exists precisely to arrange that there *is* room above an allocation that keeps growing.

**The fold does not describe the same point in time as `pending`.**
The fold describes the transition ending at `committed_cursor`; `pending` describes the state at `ops.len()`.
Under the flush-trigger policy in [transactions and batches](../kladde-docs/content/spec/transactions-and-batches), those two points **do not** coincide in general: a flush can be triggered while a completed transaction is still sitting in `ops` past `committed_cursor`, precisely so that the transaction can be written into an empty journal afterwards.
Two consequences:

- `pending` must be **repopulated** from the remaining ops after a flush, not cleared.
- The fold's output must **not** be compared against `pending` as a consistency check.
  The previous design used exactly that comparison and it was sound there because the whole buffer was consumed at flush; here it would report a spurious mismatch whenever anything outlives the flush.
  If an equivalent check is wanted, fold the *remaining* ops as well and compare the composition — but that is a debug-only luxury, not a load-bearing invariant.

### Step 2: Planning

Your instinct to move the graph down to piece granularity is right, and it buys more than you may expect.
But it works far better if combined with a second change: **decide addresses first, and let the graph be over byte ranges rather than over pieces-of-allocations.**
That turns several special cases into instances of one rule, so I will describe that shape and then answer the two questions against it.

#### 2a. Decide geometry first

Before any dependency reasoning, run the geometry the fold computed — frees, claims, size changes — against an *in-memory copy* of the heap, without touching a single byte of the file.
The output is a map from every surviving id to its **post-flush address**, plus the pre-flush addresses which the heap still knows.

This is only a decision, not an action, so it is safe to do first even though a claim may be placed over a range some piece still needs to read.
That conflict does not disappear — it becomes visible, and §2c handles it as an ordinary overlap.

**Order within this step: space-releasing operations first, then space-consuming ones largest-first.**

1. **Releases** — frees, and the tail of every shrink.
2. **Placements** — claims and grows, in **descending final size order** (first-fit-decreasing, FFD), each under its allocation's *final* id.

> **Claude: this replaces a three-phase list that had `Relabel` as its own step, which was wrong — see below.**
>
> **On grouping the frees hidden inside a relocation.** A relocating resize *is* a release plus a claim, so the temptation is to split it and put the release in phase 1.
> That would maximise the space available to phase 2, but it costs more than it gains: releasing an allocation's whole extent before placing it forfeits the in-place case, and the placement policy would have to rediscover "put it back where it was" — which for a shrink it would probably do, and for a grow it might not, turning a free operation into a full copy.
>
> The cheaper arrangement is to keep the resize atomic and let phase 2 release lazily: **when a placement decides to relocate, return the old extent to the free pool immediately**, so the subsequent (smaller) placements in the same descending-size pass can use it.
> Since phase 2 is a sequential pass and §2a touches no bytes, that costs nothing and recovers most of the space that splitting would have.
>
> **On relabels needing to move up: the question dissolves, and I had the phase list wrong.**
> `Relabel` is not a separate step at all, because the fold has already collapsed `Convert` and any following `Resize` into a *single* entry — `Relabelled { from, size }`.
> §2a therefore performs one action per allocation: rekey `from` to the new id and give it its final size.
> There is no relabel-then-resize ordering to get right here; that ordering constraint lives in the journal op vocabulary (§4.3), and the fold consumes it.
>
> What I got wrong was calling relabels geometry-neutral so they "can go anywhere".
> They change no *extent*, but they change the allocation's **class** — sizedness rides on the id, and `GainGreedyHeap` keys its evacuation index on `is_fixed` — so the class affects placement.
> A relabelled allocation must therefore be placed under its **final** id, which the two-phase list above now says explicitly.

Both of these were *best-effort* in the previous design and are **unconditional** here, which is worth dwelling on because it is the clearest single benefit of separating the decision from the action.

**Question:** I understand that any order is *correct*, but don't we still want to prefer an order that minimizes dependencies in the conflict graph (Step 2c below)?
I'm not sure how we could do that or even if that's maybe already (partially) achieved by construction.
Try to find examples where the order of frees and claims has an effect on the number of dependencies in the conflict graph and explain the general principles that this search or logical arguments brought up.

> **Claude: yes, and the effect is real — though it is *placement* rather than *order* that does the work.**
>
> **Where edges come from.** An edge exists iff a **post-flush destination** overlaps a **pre-flush range that something reads**.
> Destinations are chosen in §2a; the read ranges are fixed by the fold.
> So §2a is exactly the step that decides how many edges there will be.
>
> **The worked example.** `A1` at `[1000, 1256)` is freed, but some piece still reads `A1[0..64)`, i.e. `[1000, 1064)`. A new allocation `A3` of 200 bytes is claimed.
>
> - Placed at 1000 — reusing the freed range — its write covers `[1000, 1200)`, which overlaps the read. **One edge**, and by §2d the reading piece's 64 bytes go into the write-ahead plan.
> - Placed anywhere else, including past the end of the file: **no edge**, but the file grows by 200 bytes until compaction reclaims it.
>
> So frees-before-claims, which exists to *encourage* reuse, is precisely what *creates* these edges. That is not an argument against it — it is the trade made visible.
>
> **The general principles this brings out:**
>
> 1. **Only "hot" ranges matter.** A range is hot if some surviving piece reads it, which is computable from the fold before any placement happens.
>    Placing over cold free space — the overwhelming majority of it — costs nothing, so this is not a pervasive tax.
> 2. **The cost of an edge is the size of the *read*, not of the claim.** The buffered bytes are the reading piece's source.
>    So reuse is *most* attractive for a large claim over a small hot range, and least attractive for a small claim over a large hot range.
>    That asymmetry is the useful part: it says which reuses to avoid, not that reuse is bad.
> 3. **The benefit of reuse is the size of the claim**, in avoided file growth — which compaction would otherwise have to work off later.
>    So the comparison is roughly "read bytes into the plan now" against "claim bytes of temporary file growth plus future compaction".
> 4. **A flush with no `Storage`-sourced pieces has no hot ranges at all**, so placement cannot create edges however it chooses. Whatever pressure this puts on §2a is proportional to how much cross-allocation copying and relocation the flush contains.
>
> **What I would actually do:** pass the hot set to §2a as a *placement hint* — "prefer not to place over these ranges when an equally good alternative exists" — rather than as a constraint.
> It is a tie-break, it costs one interval-set lookup per placement, and it leaves the heap free to ignore it when the alternative is worse.
> I would not build it before measuring, but the hot set is free to compute, so it is worth threading through even if the hint is initially ignored.

Previously, releasing an allocation early was unsafe whenever some transfer still had to read it, so "frees before claims" could only be a *priority* over a ready set — and a release could sit blocked behind a transfer that was itself blocked, which is why the previous design needed an "unblocks a release" priority rule and why that rule had to be transitive to work at all.
For the same reason FFD degraded to best-effort: a claim stuck behind a transfer got placed after smaller ones.

Here, none of that applies. Releasing an allocation in step 1 destroys no bytes — it only removes an entry from an in-memory map — so nothing can be blocked, there is no ready set, and the sort is global.
If a later claim is then placed over a released range that some piece still reads, §2c sees the address overlap and orders the two writes; the placement decision itself was never in danger.

Merging the shrink half of a resize into step 1 is a small extra gain the previous design did not have: it treated reshapes as a separate late phase, so space freed by a shrink was not available to claims in the same flush.

> **This implies a change in where byte movement is decided, though a smaller one than I first wrote.**
> Correction: `RelocatableHeap::resize` does *not* move bytes — it returns a `Relocation` and the caller (`Composed::resize`) does the copying, and `GainGreedyHeap` holds no `Storage` at all.
> The same is true of compaction: `propose_compaction_step` returns a `Step` and the caller copies.
> So the heap already reports rather than acts, and the change needed is at the layer above it: **the flush should be the only thing that moves bytes, and it should move them by emitting pieces.**
>
> The payoff is that byte movement has exactly one mechanism, one dependency graph, and one idempotence argument.
> In the previous design the copies driven by `Relocation` and `Step` were the only ones the piece table could not see, which is precisely where the unfixable problem was: an overlapping compaction slide is not re-runnable after a partial application, and hoisting could not help because hoisting only resolves what the fold can see.
> Expressed as a piece, an overlapping slide is a self-conflicting vertex, handled by §2d like any other.
>
> **Your distinction between evacuations and slides is the right one, and it is sharper than "compaction is a problem".**
> An evacuation writes into free space, so its destination overlaps nothing live and nothing else reads it — as a piece it has no outgoing edges and no self-loop, and it is idempotent for free.
> A slide is the only compaction move whose destination overlaps its own source, and it is therefore the only one that needs the treatment in §2d.
> So the cost of folding compaction into the flush is proportional to the *slide* volume, not to the compaction budget.
> Worth exploiting: a flush-time compaction pass could prefer evacuations and defer slides, or bound slides by how many bytes they would add to the plan.

**Question:** can we exploit the fact that evacuations write into free space to optimize construction of the conflict graph? I.e., avoid searching for edges that cannot exist by construction.

> **Claude: yes, and it prunes the *index* rather than the search — which is the cheaper end.**
>
> An evacuation's destination is space that was free **before the flush**. An edge points *at* a destination only if that destination overlaps a pre-flush range something reads, and nothing reads free space.
> So **a destination lying entirely in pre-flush-free space can never receive an edge**, and can be left out of the interval index in §2c altogether.
>
> That is a pure win: a smaller sorted array means a cheaper binary search and fewer scan steps for every query, and it costs one interval-set membership test per destination while building the index — which you are already iterating.
> Destinations past the pre-flush `end` are the trivial case of the same rule and can be excluded by a comparison.
>
> Note the asymmetry, because it is easy to get backwards: an evacuation piece receives no *incoming* edges, but it may well have *outgoing* ones — its source is the allocation's old location, which is about to be freed and possibly reused. So evacuations still have to be queried; they just do not have to be *indexed*.

#### 2b. Lower pieces to address ranges

Rewrite every piece as an absolute extent pair:

```
dst: [post_addr(id) + seg_start,  + len)   ←   src: Literal(offset in `committed`)
                                            |   Storage[pre_addr(Z) + k, + len)
                                            |   Undefined
```

Note: `len` is the same on the destination and source side, so the actual data structure should store it only once per entry.

> **Claude: agreed, and it is forced rather than merely economical** — a piece copies a range onto a range of equal length by definition, so storing `len` twice would create a representable-but-invalid state.
> The natural shape is `{ dst_start, len, src }` with `src` an enum, which also makes the §2c index a projection of the first two fields.

Two things become simpler at this level:

- **The identity test is now an address comparison**, `pre_addr(Z) + k == post_addr(id) + seg_start`, rather than an offset comparison.
  A relocated allocation's previously-identity pieces automatically stop being identity and become ordinary copies — which is exactly the byte movement the heap used to do behind the piece table's back.
- **All destinations are globally disjoint.** Within an allocation the piece table partitions it, and distinct live allocations never overlap.
  §2c exploits this.

Drop `Undefined` pieces here: they need no write, by the content contract (note: we may have to keep short `Undefined` pieces that are flanked by `Literal` and/or `Storage` on both sides to enable the write-through optimization discussed below; to be determined).
Drop identity pieces here: the bytes are already correct.
What remains is exactly the set of writes the flush must perform.

> **Claude: agreed on keeping them, with one refinement.**
> The emitter could in principle *infer* the gaps rather than keep the pieces, since a piece table partitions its allocation and so any gap in the emitted set is exactly where something was dropped.
> But inference is not enough, because the two gap kinds have different costs: writing through an `Undefined` gap is free, while writing through an identity gap rewrites bytes that were already correct.
> The emitter has to tell them apart to apply a threshold sensibly.
>
> So keep a discriminant rather than a full piece — a gap marker carrying only "undefined" or "identity" is enough, and it keeps the dropped-piece set from costing what the kept-piece set costs.

#### 2c. The conflict graph

**Vertices:** the surviving pieces.
**Edge P → Q** ("P must run before Q") **iff `Q.dst` overlaps `P.src`** — Q would destroy bytes P still has to read.

That is the whole rule.
Note what it subsumes, all of which were separate edge kinds or separate mechanisms in the previous design:

| previously | now |
| --- | --- |
| a copy must precede the `Release` of its source | if the freed range is *reused*, the reusing piece's `dst` overlaps; if it is not reused, nothing writes there and there is no edge — correctly, since freeing moves no bytes |
| a copy must precede a `Reshape` of its source | the reshape's relocation *is* pieces now, and they overlap |
| a copy must precede a later write to its source | the same overlap |
| an allocation must be claimed before it is written | not an edge at all — claims happen in §2a |

Two structural facts make this cheap and make the graph small:

- **Only `Storage`-sourced pieces have outgoing edges.** A `Literal` piece reads the in-memory journal, not the file, so nothing in the file can invalidate it.
  In a flush with no `Copy` and no relocation *every* piece is `Literal`-sourced and the graph has **no edges at all** — though see the note below on how common that stays once compaction is folded in.
- **Destinations are disjoint**, so they can be sorted into one array of non-overlapping intervals over the address space.
  Finding all destinations overlapping a given source range is then a binary search plus a contiguous scan, giving **O(V log V + E)** rather than the O(V²) that pairwise comparison would cost.
  This is the reason not to coarsen the graph: per-piece edges are already cheap to build, and coarsening to allocations reintroduces exactly the false cycles §2d is about.

**Note:**
I am not sure if flushes with no `Storage`-sourced pieces will really be the the "overwhelmingly common case" now that the idea is to include compaction steps in the modifications that get folded into the target state.
But if we hold an explicit list of `Storage`-sourced pieces rather than a flag "are there any `Storage`-sourced pieces" then the algorithm below still has a fast-pass even in the general case.
Comment if you disagree.

> **Claude: I agree on both counts, and the "overwhelmingly common case" claim should come out of this document wherever it appears.**
>
> Once compaction is folded in, every compaction move is a `Storage`-sourced piece, so a flush that does any compaction at all has some — and if compaction runs on every flush, the edgeless case stops being common and becomes the *empty-compaction* case.
> The claim was inherited from a design where compaction lived outside the flush entirely, and it did not survive the move.
>
> Your replacement is strictly better, and the reason is that it changes the complexity rather than just the branch.
> Let `S` be the number of `Storage`-sourced pieces and `V` the total.
> With a flag, you either skip everything or pay `O(V log V + E)`.
> With a list, you always pay `O(|S| · log V + E)` — the query loop iterates `S`, not `V` — and since the edits dominate `V` while compaction contributes only `S`, that is a large constant-factor difference in exactly the regime you are worried about.
> It also degrades gracefully instead of falling off a cliff the first time a flush contains one `Copy`.
>
> The pseudocode below already loops over `Storage`-sourced pieces only; the accompanying prose about `E == 0` being expected is what needs rewording, which I have done there.

> **Claude: the O(V log V + E) is the total for all source ranges, not for one.** My sentence was ambiguous; here is the derivation.
>
> The cost per source range is `O(log V + deg(P))`: one binary search, then a scan that visits exactly the destinations it will emit edges to, plus one extra to notice it has gone past the end.
> Summing over all V vertices gives `O(V log V) + O(Σ deg(P)) = O(V log V + E)`.
> The scan is **output-sensitive** — it never looks at a destination it does not emit an edge for — which is what stops it degenerating to `O(V²)`.
>
> The property that makes this work is the one from §2b: **destinations are globally disjoint**, so sorting them by start address also sorts them by end address, and the ones overlapping any query range form a *contiguous slice* of the sorted array.
> That would not hold for arbitrary intervals, which is why the general problem needs an interval tree and this one does not.
>
> ```text
> # --- build the index: O(V log V) ---
> D = [(dst.start, dst.end, vertex_id) for each vertex]
> sort D by dst.start
>
> # --- collect edges: O(V log V + E) ---
> succ = [[] for each vertex]           # adjacency lists
> for P in vertices where P.src is Storage:
>     # first destination that could overlap: the last one starting at or
>     # before P.src.start might still extend into it, so start one earlier.
>     i = lower_bound(D, P.src.start) - 1
>     if i < 0: i = 0
>     while i < len(D) and D[i].start < P.src.end:
>         if D[i].end > P.src.start:
>             succ[P].append(D[i].vertex)   # P must run before D[i]
>         i += 1
> ```
>
> The `- 1` is the only fiddly part: `lower_bound` finds the first destination *starting* at or after the query start, but the destination immediately before it may still overlap, so back up one and let the `D[i].end > P.src.start` test reject it if it does not.
>
> **Edge representation.** Adjacency lists (`Vec<Vec<u32>>`) are fine and are what Tarjan and Kahn both want.
> If the allocation churn shows up in a profile, count degrees in a first pass and pack into CSR — one `Vec<u32>` of neighbours plus one `Vec<u32>` of offsets — but do not start there.
> The loop is driven by the list of `Storage`-sourced pieces, so its cost is `O(|S| · log V + E)` where `S` is that list — not `O(V log V + E)`.
> In a flush that does no copying and no relocation `S` is empty and the loop does not run at all; once compaction is folded in, `S` is the compaction moves plus any `Copy`, which is small next to the edits that dominate `V`.

**Questions:**
- How would the algorithm change if we wanted to exploit knowledge that certain `Storage` pieces cannot emit edges (see my earlier comment on evacuations)?
- Doesn't the condition `D[i].vertex != P` in the code above prevent us from detecting self-cycles?
  Or are they detected separately?
  I think, at this point, any `Storage` where source and destination are exactly equal (not just overlapping) should have been removed already (see above: "Drop identity pieces here").

> **Claude — answers, and the second one was a bug that is now fixed above.**
>
> **On pruning.** The knowledge is about *destinations*, not sources, so it changes the index rather than the loop: build `D` only from destinations that overlap the pre-flush **live** region, since a destination lying in pre-flush-free space can never be the target of an edge.
> The loop body is unchanged; it simply queries a shorter array.
> Combined with driving the loop from the `Storage` list, the two prunings hit the two different factors of `|S| · log V`, which is why both are worth doing.
>
> **On the self-loop: you are right, and `D[i].vertex != P` was wrong.** I have removed it.
>
> Your reasoning is exactly the justification: §2b has already dropped every piece whose source and destination are *exactly* equal, so a piece that reaches §2c and overlaps itself must overlap *partially* — which is a `memmove`, and precisely the thing that has to be detected rather than skipped.
> The guard was defensive against a self-edge that cannot arise, and it suppressed the only self-edge that can.
>
> With it gone, a self-overlapping piece records an edge to itself, which is exactly what the SCC step expects — a single-vertex component with a self-loop is one of the two shapes it treats as needing to be broken.

#### 2d. Preserving endangered reads

**Note:** re-consider whether these complications are really limited to cycles.
Remember that execution has to be *idempotent*.
Consider the piece table `[0, 10) ← [30, 40); [30, 40) ← [70, 80)`.
No cycles, and yet I don't see how this can be executed in an idempotent way without copying out some data to a write-ahead log.
Explain where I'm wrong or rewrite this section (Step 2d): state what kind of subgraphs are problematic and explain why (and why only these); propose how to find them and how to break them in a way minimizes the write-ahead log.

> **Claude: you are right, and the section is rewritten below. This is the same conclusion the swap walkthrough under Step 3 reached from the other direction; §2d had not been updated to match.**
>
> Your example is the minimal witness. `P1 = [0,10) ← [30,40)` and `P2 = [30,40) ← [70,80)` give one edge `P1 → P2` and no cycle.
> Run `P1` then `P2`: correct.
> Crash *after both* but before the salt overwrite — which is a perfectly ordinary crash, since the salt is the last thing written — and re-run from the start: `P1` now reads `[30,40)`, which `P2` has already overwritten.
> Wrong bytes, deterministically, on every subsequent attempt.
>
> So the problematic structure is not a cycle but **any edge at all**, and the reason is that the two properties are different:
>
> | property | problematic subgraph | why |
> | --- | --- | --- |
> | **schedulable** — a valid execution order exists | cycles only | an acyclic graph has a topological order, and running it once in that order is correct |
> | **restartable** — re-running the whole plan is correct | any edge | the tail of an edge reads a range the head overwrites, so after a completed run its source is gone |
>
> Restartability strictly subsumes schedulability, and since a flush must be restartable, the weaker property never governs.

**What must be preserved.** Exactly the ranges that are *read* by one piece and *overwritten* by another — call them **endangered reads**.
For every piece `P` whose source overlaps some other piece's destination, the overlapping bytes must be read up front and carried in the write-ahead plan, so that re-execution reads them from the plan rather than from a file that may already have moved on.

**Why only those.** A piece whose source range no other piece writes has a source that is stable for the whole flush, however many times execution is restarted; it needs nothing.
A piece whose source is a `Literal` reads the journal, not the file, and is stable by construction.

**Finding them** is the §2c query with the result used differently: for each `Storage`-sourced piece, enumerate the destinations overlapping its source.
No adjacency lists, no indegrees, no SCC pass — the graph was only ever needed to *locate* these, not to order anything.

**Minimising the plan** turns on splitting rather than on choosing victims:

- **Split at overlap boundaries.** If only part of `P`'s source is overwritten, split `P` into the endangered part (buffered) and the safe part (left as `Storage`). Only genuinely endangered bytes reach the plan.
- **Avoid creating the overlap in the first place**, which is the placement hint in §2a: an edge exists because a destination was placed over a hot range.
- **Prefer evacuations to slides** when compaction is folded in, since an evacuation's destination is pre-flush-free space and so overlaps no read at all.

**What this collapses.** After the endangered reads are buffered, **no edges remain** — every surviving `Storage` piece reads a range nothing writes, and every buffered piece reads the plan.
So the conflict graph is edgeless by construction at the end of §2d, and with it go the topological sort, the cycle detection and the priority queue:

- **Cycles need no separate treatment.** A cycle is a set of pieces that all have outgoing edges, so buffering every edge tail breaks every cycle as a side effect. The swap below is handled without ever being recognised as a swap.
- **Self-overlaps need no separate treatment.** A `memmove` is a piece whose destination overlaps its own source, i.e. a self-edge, so its overlapping `L − d` bytes are buffered by the same rule.
- **§2e's scheduling reduces to sorting** by destination address, because any order is now legal.

> **Why cycle-breaking was an issue in the old design.**

> **Why cycle-breaking was an issue in the old design.**
>
> **Most old cycles were indeed artifacts of allocation granularity.**
> The old design had one vertex per allocation, so "A1 reads A2" and "A2 reads A1" produced a two-cycle regardless of *which parts* of A1 and A2 were involved.
> Take the worked example that broke it — `Copy(A1,0,64 → A2,0)` then `Copy(A2,32,64 → A1,0)`, folding to
>
> ```
> A1: { 0 → Storage(A1,32),  32 → Storage(A2,64),  64 → Storage(A1,64) }
> A2: { 0 → Storage(A1,0),   64 → Storage(A2,64) }
> ```
>
> At piece granularity the writes are `P1 = A1[0..32) ← A1[32..64)`, `P2 = A1[32..64) ← A2[64..96)` and `P3 = A2[0..64) ← A1[0..64)`, and the edges are `P3 → P1`, `P3 → P2`, `P1 → P2`.
> The order `P3, P1, P2` works.
> **Acyclic** — the cycle was entirely an artifact of the coarse vertices, exactly as you suspected.
>
> **But piece granularity is not cycle-free in general**, and it is worth knowing what survives, because it is a much smaller and more meaningful class.
> Three ops suffice:
>
> ```
> Copy(Z, 0, 8, W, 0)   # W[0..8) = Storage(Z,0)   — W witnesses Z's old value
> Copy(V, 0, 8, Z, 0)   # Z[0..8) = Storage(V,0)
> Copy(W, 0, 8, V, 0)   # V[0..8) = Storage(Z,0)   — resolved through W's table
> ```
>
> giving `Pz = Z[0..8) ← V[0..8)` and `Pv = V[0..8) ← Z[0..8)`, hence `Pz → Pv` and `Pv → Pz`.
>
> That is a **swap**, and it is intrinsic: no ordering of two writes can exchange two byte ranges, with or without a graph.
> The general statement is that a cycle in the conflict graph is exactly a **cyclic permutation of content**, and every such cycle needs one buffer.
> Notice the fold cannot remove these but *does* remove the near misses — without the temporary (i.e., `Copy(V, 0, 8, Z, 0); Copy(Z, 0, 8, V, 0)`), the second copy reads through the first, folding to an identity piece that gets dropped; only `Pz` survives and there is no cycle.
>
> So cycle-breaking is still needed, but it goes from "triggered by any mutual reference between two allocations" to "triggered by an actual rotation of bytes", which is rare enough that you can afford to handle it in the obviously-correct way.

**Detection and buffering**, concretely:

```text
for P in storage_sourced_pieces:
    for D in destinations overlapping P.src:      # the §2c query
        overlap = intersect(P.src, D)
        split P at overlap's boundaries
        read overlap from the file into the plan
        mark that fragment of P as Literal(plan offset)
```

The cost is the §2c query plus the bytes read, and both are proportional to the endangered set rather than to the flush.

> **Claude — on Tarjan, which this replaces.**
> An earlier draft of this section detected strongly connected components and buffered the cheapest vertex in each.
> That is the right algorithm for *schedulability* and the wrong one for *restartability*: it buffers a strict subset of what is needed, so it produces plans that are correct on a first run and wrong on a restart.
> Since restartability is not optional, the SCC pass has no job left — every cycle is broken as a by-product of buffering edge tails, and nothing has to notice that a cycle existed.
> Worth stating explicitly because "run Tarjan here" is the intuitive answer and it is a trap: it is more machinery producing a weaker guarantee.

**Self-overlaps** — a piece whose `dst` overlaps its own `src` — arise from a splice shifting a tail or from a compaction slide.
They are self-edges, so the rule above already buffers their overlapping bytes; the `L − d` figure below is exactly the size of that overlap.

> **Claude — on your `memmove` question, and it is the one that changes the design most.**
>
> **There is no `memmove` for files on any of the three platforms.**
> Linux's `copy_file_range` rejects overlapping ranges within one file; macOS's `fcopyfile` and Windows have no intra-file equivalent at all.
> The closest thing is `mmap` plus a real `memmove`, which works but hands you `msync`'s durability semantics instead of `write`'s — a different set of problems, not fewer.
> So in practice an overlapping in-file move is a loop of bounded reads and writes in the direction that reads ahead of where it writes, i.e. you are buffering anyway, just a chunk at a time.

**Question:** *briefly* mention what the "different set of problems" with `mmap` and `msync`'s durability semantics are.

> **Claude, briefly:** three, of which the third is the one that matters here.
>
> - **You lose control of *when*.** The kernel may write back dirty pages at any time in any order, so "nothing has reached disk yet" stops being something you can arrange — which is fatal to a barrier discipline that depends on the plan landing before the data.
> - **Errors arrive as `SIGBUS`**, not as a return value, so an I/O failure or a truncation under the mapping becomes a signal to handle rather than an error to propagate.
> - **`msync` is weaker and less portable than `fsync`**: it says nothing about ordering between mappings, and Windows needs `FlushViewOfFile` *plus* `FlushFileBuffers` to get an equivalent guarantee.

> **It is efficient but not crash-resistant, and the second part is what matters.**
> Take a slide of `L` bytes down by `d`, copying ascending.
> When you read byte `s+i` you have already written up to `s−d+i`, so once the move completes, source bytes `[s, s−d+L)` — that is `L−d` bytes, the overlap — hold their *new* values, not their old ones.
> Re-running the move from the start after a crash therefore reads bytes the previous attempt already destroyed.
>
> **How to make it idempotent:** buffer the overlap — `L − d` bytes — into the write-ahead plan, so re-execution reads them from the plan rather than from the file.
> Nothing cheaper works: recording *progress* instead would need an `fsync` per chunk, which costs far more than the bytes.
>
> The `L − d` formula is worth keeping in mind because it says exactly when this is cheap: a **large shift over a short run costs nothing** (if `d ≥ L` there is no overlap at all), while a **small shift over a long run costs almost the whole run**.
> Compaction slides are the second kind, which is the concrete reason to prefer evacuations at flush time and to bound slides by the bytes they would add to the plan.
>
> This also retires the "direction trick as an optimisation" line I wrote earlier: the direction trick is what makes a single uninterrupted execution correct, but it does nothing for restartability, so under a recorded plan you buffer regardless.

#### 2e. Ordering for sequential I/O

> **Does piece granularity cost sequentiality?**
> No — and it is *better* placed to deliver sequentiality than allocation granularity was, for two reasons.
>
> Second, a topological order is a *partial* order, so even when there are edges you choose among the ready set.
> Spend that freedom on locality rather than on anything else.

**Sort the pieces by destination address and emit them in that order.**
After §2d the conflict graph is edgeless, so every order is legal and the only thing left to optimise is locality — which one ascending sweep delivers, and which is what the device wants.

> **Claude: this used to say "Kahn's algorithm over a priority queue with an elevator heuristic", and the §2d rewrite retired it.**
> Once every endangered read is buffered there is nothing to schedule around: no edges, no ready set, no topological order, no elevator.
> A sort is not a degenerate case of the scheduler here, it is the whole of it.
>
> The pseudocode below is kept because it is the right answer *if* a future variant reintroduces edges — for instance by buffering less than §2d requires and accepting a non-restartable flush, or by hoisting lazily under memory pressure.
> It is not what this design needs.

> **Claude — pseudocode.** Kahn's algorithm is just "repeatedly take a vertex nothing is waiting on":
>
> ```text
> indegree[v] = number of edges pointing *at* v
> ready = a set of all v with indegree[v] == 0     # ordered by dst.start
> cursor = 0                                       # last address written
>
> while ready is not empty:
>     # elevator: nearest destination at or above the cursor, else wrap
>     v = ready.first_at_or_after(cursor) or ready.first()
>     ready.remove(v)
>     emit v
>     cursor = v.dst.end
>     for w in succ[v]:
>         indegree[w] -= 1
>         if indegree[w] == 0:
>             ready.insert(w)
> ```
>
> `ready` wants to be something with an ordered "next at or after x" query — a `BTreeSet` keyed by `(dst.start, vertex_id)` gives both that and the wrap, since `range(cursor..).next()` is the seek and `iter().next()` is the wrap.
> Total cost `O((V + E) log V)`.
>
> **Why the wrap rather than restarting at zero:** you want one sweep up the address space per pass, not a reset after every vertex.
> Wrapping only when nothing remains above the cursor is what produces long ascending runs — it is the same reason a lift serves everyone going up before reversing.
>
> **Correctness does not depend on the heuristic.** Any vertex from `ready` is a legal choice; the elevator only decides *which* legal order you get.
> With no edges, `ready` contains everything from the start and the whole loop degenerates to sorting by `dst.start` — which is why the section above now simply says that.

**Question:** briefly explain why you proposed a `BTreeSet` keyed by `(dst.start, vertex_id)` and not a `BTreeHashMap` keyed by `dst.start` with value `vertex_id` (I think their memory layout is almost identical but I'm not sure, and the hash map sorts only by `dst.start` out of the box).

> **Claude: `BTreeMap<dst.start, vertex_id>` is the better choice and I should have proposed it. Two corrections, and then a caveat that makes the whole question moot.**
>
> I reached for the tuple set out of habit, to guarantee a total order when keys might collide. **Here they cannot**: destinations are globally disjoint (§2b), so `dst.start` is already unique across all vertices, and the extra tuple component buys nothing.
>
> On layout, your instinct is right and slightly understates the case. `BTreeMap` stores keys and values in *separate* arrays inside each node, so a `u64` key array and a `u32` value array pack tightly. A `BTreeSet<(u64, u32)>` stores 16-byte tuples — the `u32` is padded to the `u64`'s alignment — so it is meaningfully *worse*, not merely equal.
>
> **The caveat:** after the §2d rewrite the graph is edgeless, so there is no `ready` set to maintain and no Kahn's algorithm to run. Emission is a sort of the pieces by destination address. Keep this answer for the day the scheduler comes back, but do not build the structure now.

Then **merge adjacent emitted writes**, and merge *across short gaps*:

- across an `Undefined` gap for free, since uninitialized bytes may legally receive anything (note: requires keeping `Undefined` around, see my earlier comment);
- across an identity gap at the cost of rewriting bytes that were already correct (note: this probably needs to be decided in the write-ahead log because I think a torn write over an identity gap can corrupt it — is that correct?).

  > **Claude: the answer splits by failure model — safe against a program crash, *not* unconditionally safe against power loss.**
  >
  > **Against a program crash: safe.** Each `write()` that returns is fully applied to the page cache; an interrupted `write_all` loop leaves a prefix of *completed* calls. The bytes going into an identity gap are the bytes already there, so any prefix of them leaves the gap unchanged, and re-running is idempotent for the same reason.
  >
  > **Against power loss: not safe, and this is worth being precise about**, because "torn" covers three quite different outcomes:
  >
  > 1. **Reordering.** Writeback order is not issue order, so what survives is an arbitrary *subset* of dirty pages, not a prefix. Harmless for an identity gap — every page holds the same bytes either way.
  > 2. **A partially updated sector**, old bytes in one part and new in the other. Also harmless here, for the same reason: old and new are equal.
  > 3. **A corrupt sector.** A sector interrupted mid-write can come back with an ECC failure or arbitrary contents, and on flash a partial program of an internal page can damage data elsewhere in that page. **This is not harmless**, and it is the case my earlier answer waved away.
  >
  > **But the exposure is much smaller than that makes it sound, because the unit of writeback is the page, not the byte range.** A write of *any* size into a page dirties the whole page, and writeback submits the whole page — so every byte sharing a page with something the flush writes is already in flight whether or not the emitter merges across it. Merging changes nothing for those bytes.
  >
  > That gives the criterion directly. Merging `[a1, b1)` and `[a2, b2)` across a gap dirties `pages(a1..b2)` instead of `pages(a1..b1) ∪ pages(a2..b2)`; the difference is exactly the pages lying **wholly inside the gap**. Hence:
  >
  > > Merging across an identity gap adds no exposure at all unless the gap wholly contains a page — which a gap smaller than a page never can.
  >
  > So the safety criterion and the throughput heuristic coincide at the page size, which is a pleasant accident and means there is nothing extra to tune. Note it applies only to identity gaps: an `Undefined` gap has no contents worth protecting and can be merged across on throughput grounds alone.
  >
  > What does *not* improve is repair. If a gap's page is corrupted by a power outage, the data pieces in it are restored from the plan on the next recovery, but the gap is not: the emitter reads the gap from the file to fill its staging buffer, so a re-run reads the corrupt bytes and writes them back. The corruption is confined to the gap, and it survives — which is exactly the residual risk [[#durability-guarantees]] chooses to *detect* rather than prevent.
  >
  > **The separate point stands: this write must not count as a write for §2d's purposes.** If some piece reads the gap and the emitter merges across it, a naive edge test would see a destination overlapping that read and demand the reader be buffered — paying real bytes for a write that changes nothing.
  >
  > The clean way to avoid that is ordering: **do gap merging in the emitter, after §2d has finished.** The analysis then only ever sees writes that actually change bytes. This stays correct across a restart even though the plan does not describe the merged writes, because merging is deterministic given the plan and, in any case, writes identical bytes — recovery reaches the same final state whether or not it merges the same way.

A threshold of about a page is the right rule for both, though for different reasons: throughput for `Undefined` gaps, and page-level write exposure for identity gaps (see above).
This is the main lever the piece table offers for turning a scattered set of small edits into one sequential write, and it is worth building at the same time as the emitter rather than later — it is the step that converts the fold's *logical* deduplication into an actual reduction in I/O operations.

Reads need less care than writes: `Literal` sources are in memory, and `Storage` sources were, in the common case, written by this same process recently and are in page cache.
If reads ever do show up in a profile, the same elevator applied to a read queue would work, but I would not design for it before measuring.

### Step 3: Hoisting

Deferred for now until planning is designed (I'm not sure if we'll need it).

> **Claude — now that planning is designed, "hoisting" has split into two unrelated jobs, and conflating them is what made this confusing in the previous design.**
>
> **Job 1 — cycle-breaking buffers (§2d).** Needed for the plan to exist at all.
> Purely in memory; nothing about it has to reach the file.
> Small by construction, since you buffer the cheapest vertex per cycle and cycles are rare.
> This job is unconditional but nearly free.
>
> **Job 2 — the write-ahead record for restartability.** Needed only because execution can be interrupted.
> This is where the choice I flagged above the five steps has to be paid for:
>
> - If you **record addresses**, this record holds the geometry decisions from §2a — roughly `(id, post_address, size)` per touched allocation — and re-execution reads them instead of re-deriving.
>   It must also hold any bytes that re-execution could no longer read, which after §2d means the cycle-breaking buffers, since their source ranges are exactly the ones a partial execution overwrites.
> - If you **re-derive**, this record is empty and step 3 disappears — but then the buffers of job 1 must be re-derivable too, which means re-running fold and plan, which means determinism becomes load-bearing for correctness rather than merely nice.
>
> **Recommendation: record.**
> The record is small (tens of bytes per touched allocation, against a data-region write volume typically an order of magnitude larger), and it collapses recovery from "re-run the optimiser and hope it is bit-identical" to "replay a list".
> Given that the recovery path is the least-tested code in any storage system, buying it that simplicity is worth a few percent of write volume.
>
> That also answers your parenthetical: **yes, you will need this step**, but for job 2, not job 1 — and it is better named something like *"journal the plan"* than *hoisting*, since the bytes it carries are incidental.
>
> One consequence worth noting: if the plan is recorded, §2a's placement no longer has to be deterministic, so the heap keeps its freedom to change placement policy between versions without breaking recovery of files written by an older build.
> That is a real compatibility property and it is worth writing down as a reason, not just a side effect.

> **Claude: working the example through shows that my sentence was wrong — re-derivation cannot make step 3 disappear whenever the flush has to read anything it also writes. This is the most important correction in this round.**
>
> **The example**, expanded as you suggest so the net effect is a pure swap:
>
> ```
> Alloc(W, 8); Copy(Z,0,8,W,0); Copy(V,0,8,Z,0); Copy(W,0,8,V,0); Free(W)
> ```
>
> The fold gives `W` born-and-died (so it emits nothing at all), and two surviving pieces:
>
> ```
> Pz = Z[0..8) ← Storage(V, 0)     # pre-flush V
> Pv = V[0..8) ← Storage(Z, 0)     # pre-flush Z, resolved through W's table
> ```
>
> `Pz → Pv` and `Pv → Pz`: a cycle. §2d breaks it by buffering, say, `Pv`'s source — pre-flush `Z[0..8)` — and rewriting `Pv` as a `Literal`. The order is then `Pz`, `Pv`.
>
> **Execute, and crash between the two writes.** The file now has `Z = old V`, and `V` untouched, so `V = old V`. **`old Z` exists nowhere on disk.**
>
> **Now re-derive.** Recovery re-folds `committed` (intact) and re-plans. It reaches the identical cycle and the identical decision to buffer `Pv`'s source — so far so good, determinism delivered everything it promised. Then it *reads pre-flush `Z[0..8)` from the file* to fill the buffer, and gets `old V`, because the interrupted run already overwrote it. `Pz` re-runs harmlessly, `Pv` writes `old V` into `V`. Final state: both hold `old V`. **The swap is lost and the file is corrupt.**
>
> Re-derivation reproduces the *plan*, but a plan is not bytes. The bytes were destroyed by the very execution being restarted.
>
> **The boundary, stated precisely.** Re-execution from scratch is correct only if no piece's source is overwritten by another piece — i.e. **only if the conflict graph has no edges at all.** Note this is much stronger than "no cycles":
>
> - With `P → Q` and no cycle, the order `P, Q` is fine on the *first* run. But re-running from the start after both completed means re-running `P`, whose source `Q` has by then overwritten. Same failure, one edge instead of two.
> - So even §2c's acyclic worked example (`P3 → P1 → P2`) is not restartable: re-running `P3` reads `A1[0..64)`, which `P1` and `P2` wrote.
>
> **Therefore the set that must be buffered durably is not "one piece per cycle" but "every piece with an outgoing edge".**
> Schedulability and restartability are different questions with different answers, and the second subsumes the first:
>
> | requirement | what must be buffered |
> | --- | --- |
> | a valid order exists (§2d) | one piece per cycle — rare |
> | re-execution after a crash is correct | every piece with an outgoing edge |
>
> This is worth dwelling on because it retroactively explains the previous design: its rule of "hoist every read this flush disturbs" was not the overkill I called it, it was *exactly* the restartability requirement — the previous design simply never articulated that this was why, and so could not tell which of its mechanisms were load-bearing.
>
> **What this does not change.** In a flush with no `Storage` sources at all there are no edges and hence nothing to buffer — so re-derivation *is* viable there, and step 3 really does reduce to recording addresses. That case is less common than it first appeared, since folding compaction into the flush gives every compacting flush some `Storage` pieces. The hybrid is nevertheless well defined and worth stating as the rule:
>
> > If the conflict graph is edgeless, the plan needs only addresses (or nothing at all, under re-derivation).
> > Otherwise it must additionally carry the bytes of every piece with an outgoing edge.
>
> And since the plan has to exist anyway to carry those bytes, carrying the addresses in it too is free — which is the practical argument for **recording always** rather than switching strategies on a graph property.

### Step 4: Executing

Deferred for now until planning is designed.

### Step 5: Cleaning Up

Deferred for now until it is clear whether hoisting is still required.

**Questions:**
1. Before I'll write up the `fsync` policy: what can happen if a power outage or kernel fault occurs *during* an `fsync` call (or its equivalent on the three main OSes)? Assume the power outage doesn't physically damage the hardware. Can this only result in states that could also have occurred if the program had crashed at any time since the last `fsync` or can it lead to other unforeseen states? If it's the former then I think we should be fine. If it's the latter, then how do sqlite and other database systems deal with it?

   > **Claude: it is the latter, and the difference matters more than it looks.**
   >
   > A **program crash** is benign: the kernel survives, the page cache survives, and every `write()` that returned is visible to any later reader, in the order issued. The set of surviving writes is exactly "all of them".
   >
   > A **power outage** loses the page cache, and what reaches the platter is an **arbitrary subset** of the pending writes — not a prefix. Devices and the block layer reorder freely.
   >
   > So a power outage can produce states no program crash could: write B present without write A, or a block holding neither its old nor its new contents. `fsync` returning is the only thing that converts "issued" into "durable", and a power outage *during* `fsync` simply means it never returned, so nothing it was flushing is guaranteed.
   >
   > **Three different units are in play here, and conflating them makes this much harder to reason about than it is:**
   >
   > | unit | typical size | what it is |
   > | --- | --- | --- |
   > | **byte range** | any | what your `write()` call names |
   > | **page** | 4 KiB (16 KiB on Apple Silicon) | what the kernel tracks dirtiness in, and the unit of writeback |
   > | **sector** | 512 B or 4 KiB | the device's atomic write unit |
   >
   > None of them is "the bytes you modified". The kernel does **not** remember byte ranges: a `write()` of four bytes marks the whole enclosing page dirty, and if that page was not already cached it must first be **read in** so the rest of it can be preserved. Writeback then submits the **whole page**.
   >
   > **The consequence, which is the part that matters here: a power outage can damage bytes your program never wrote.** They were passengers in a page that was in flight because *some* byte in it was modified. Their correct values were in the page cache and were duly submitted; the damage is at the device, below the level where "which bytes did the application care about" still exists.
   >
   > How likely that is depends on the device and the filesystem, and the layers disagree about what they promise:
   >
   > - A drive that honours single-sector atomicity gives you all-or-nothing *per sector*, and nothing at all across sectors — so a multi-sector page can land half old and half new.
   > - Intra-sector tearing is not supposed to happen but is observed on cheap devices.
   > - On flash, a partial program of an internal page (often 16 KiB) can damage *unrelated* logical blocks that share it. Drives with power-loss protection capacitors avoid this; many consumer drives do not.
   > - Copy-on-write filesystems (btrfs, ZFS) never overwrite in place, so the hazard largely disappears — but `ext4` in its default `data=ordered` mode does overwrite in place.
   >
   > **This is a real design question, not a footnote**, and it is now decided: see [[#durability-guarantees]], which surveys how SQLite, PostgreSQL, InnoDB and LMDB handle it and settles on detecting torn content rather than preventing it. The short version is that full-page writes — PostgreSQL's answer — cost roughly a hundredfold amplification on kladde's scattered writes, so the plan instead carries a checksum per dirtied page and recovery reports the ranges that fail.
   >
   > **How SQLite and friends handle it**, three techniques, all applicable here:
   >
   > - **Checksums over everything self-describing**, to detect torn sectors rather than trusting them. Your chained CRCs already do this for the journal; the write-ahead plan needs the same.
   > - **`fsync` as a barrier, not as durability.** The pattern is always *write A; fsync; write B* where B's presence is meant to imply A's. Without the barrier the implication is false.
   > - **A single-sector commit record**, small enough that the device's own atomicity guarantee covers it. SQLite assumes 512-byte sector atomicity by default and exposes `SQLITE_IOCAP_ATOMIC` for devices promising more.
   >
   > **What this means for kladde, and it is a real consequence for the current design.**
   > The barriers are what make the *structural* half of the [[#durability-guarantees|tiered guarantee]] hold. Without them the salt overwrite could reach the platter while some of the data writes it commits do not, and recovery would discard a journal whose effects are only partly applied — corruption that no amount of checksumming would let it repair, because the journal describing the repair would be gone.
   >
   > The minimum is **two `fsync`s per flush**:
   >
   > 1. after the write-ahead plan is written, before execution begins — so that a recovery which finds the plan can trust it, and so that no data write can overtake it;
   > 2. after all data writes, before the salt overwrite — so that discarding the journal implies its effects are on disk.
   >
   > At one flush per few thousand ops that is cheap, and it is confined to the flush path rather than the per-transaction path, which was the whole point. But it does mean the flush latency budget above becomes device-dependent, so the two `fsync`s should be counted there explicitly.

2. Assuming no power outage, just a program crash: what are the guarantees on *ordering* for the sequence `write, seek, write`, assuming the writes are non-overlapping but the second write location may sit before the first? If the program crashes during the second write, is there a guarantee that, if the second write changed something in the file, the first write must have had its normal effect? This is relevant for the step that overwrites the `salt`: if a crash occurred either during or shortly after we overwrite the salt, and the slat did change in the file (either to the intended new salt or to some garbage because of a crash), then is it guaranteed that all writes that happened before have had the desired effect? If yes, does this also hold if the `seek` happened to be a seek by 0 bytes?

   > **Claude: yes, guaranteed, and the seek distance is irrelevant — including zero.**
   >
   > For a program crash the reasoning is short: a `write()` that returned has already been applied to the page cache, which is the same object any later reader sees. There is no reordering visible to a reader of the same file, no matter where the writes land relative to each other, and `seek` only moves a cursor. So if the second write is observable, the first necessarily is too.
   >
   > **One caveat, which is about tearing rather than ordering.** `write()` may return a *short* count, so a loop like `write_all` can be interrupted part-way and leave a partially written buffer. That applies to the salt too if it were ever written by a loop — so write it with a single `write` call of 4 bytes (or `pwrite`), which either happens or does not. This is exactly why the salt wants to be small.
   >
   > **The important qualification is scope.** All of the above holds *only* for program crashes. Under power loss, question 1's answer applies and the ordering guarantee evaporates entirely — which is why the salt overwrite needs an `fsync` barrier before it if the post-power-loss consistency promise is to hold, even though it needs nothing at all for the crash-resistance promise.

**Question:** explain how a torn write to the salt can be an issue. I understand that this is a pedantic point for a 4-byte salt but I want to understand the principle behind it.

> **Claude: the principle is that a commit record must have exactly two readings, and every failure must land in the *safe* one — which for this design it does, but only because of the ordering, not because of the size.**
>
> The salt is the commit record: its old value means "journal valid, flush not committed", its new value means "journal invalid, flush committed". A torn write produces a third state that is neither.
>
> Enumerate what recovery does with that third state. The chained CRCs are computed *from* the salt, so a garbage salt makes the very first frame fail its check, and recovery concludes the journal is empty — which is the *committed* reading. That is safe here **only because the salt is written last, after the barrier**: execution had already finished, so "committed" is true.
>
> Invert the ordering and the same tear becomes catastrophic. If the salt were written before execution, a torn salt would still read as "committed" while the file was still half-updated, and the journal that could have repaired it would have been discarded. So the atomicity of the salt is not what makes this work — **the ordering is**, and the atomicity is a second line of defence.
>
> What the second line actually defends against is narrow and worth naming: a garbage salt that happens to make some prefix of frames validate, which would replay a fabricated journal. A chained 4-byte CRC makes that negligible rather than impossible, and keeping the write inside one sector removes the possibility rather than shrinking it.
>
> The general rule to carry forward: **for any commit record, ask which reading an unreadable value falls into, and arrange the write order so that reading is the conservative one.** Making the record small is how you avoid needing the answer; knowing the answer is how you stay correct when you cannot.
## Durability guarantees

The guarantee is **tiered**, and saying so plainly is better than a flat promise that only holds on some hardware.

| failure | structure | content |
| --- | --- | --- |
| **program crash** | intact | intact — every transaction fully recorded in the journal is recovered |
| **power outage** | intact | recovered to either its pre-flush or post-flush value **for everything the plan rewrites**; anything else in a page that was in flight may be torn, and torn regions are **detected and reported**, not silently returned |

Structure survives unconditionally because the heap's `id → address` table and the journal are protected separately from the data region: the file always opens and walks, whatever happened to the bytes inside it.

### Why content cannot be protected for free

The unit of writeback is the **page**, not the byte range you wrote (see the answer to question 1 under Step 5).
A four-byte write dirties a whole page and submits the whole page, so every byte sharing that page is in flight — including bytes the flush never intended to change.
A power outage mid-writeback can therefore damage **passengers**: identity gaps, `Undefined` regions, and neighbouring allocations that happen to share a page.

Note what is *not* at risk, because it narrows the problem considerably: **the plan is already a repair mechanism.**
Recovery replays it until the salt flips, so any torn write inside the plan's destination set is simply rewritten.
The exposure is only the uncovered remainder of the pages the flush dirties:

```
plan bytes = changed bytes + uncovered bytes in dirtied pages
```

The second term is what full protection costs. For fifty scattered forty-byte writes it is roughly 200 KiB to protect 2 KiB of change — a hundredfold amplification, and the heap-like layout of a kladde file makes scattered writes the expected case rather than the exception.

### How other systems solve this

Every one of them either writes full pages somewhere durable before overwriting in place, or never overwrites in place at all.
There is no third mechanism.

| system | mechanism | cost |
| --- | --- | --- |
| **SQLite** (rollback journal) | copy the *original* page to a journal file, fsync, then modify in place; recovery rolls back | each modified page written twice; 2–4 fsyncs per transaction |
| **SQLite** (WAL) | append full pages as checksummed frames; readers consult the WAL first; checkpoint copies back | full pages, but written sequentially |
| **PostgreSQL** | `full_page_writes`: the *first* modification of a block after a checkpoint puts the whole block image in the WAL; later changes in that interval are deltas | amortised — one full page per block per checkpoint interval |
| **InnoDB** | doublewrite buffer: pages go to a contiguous staging area, fsync, then to their final home; restore from staging on checksum failure | each page written twice, but the staging write is sequential |
| **LMDB** | copy-on-write B+ tree; live data is never overwritten; a single-sector meta page flip commits | write amplification from copying the path to the root |
| **ZFS / btrfs** | copy-on-write plus checksums, at the filesystem layer | absorbed by the filesystem — which is why PostgreSQL and InnoDB let you *disable* their own protection on ZFS |

**Why it is affordable for them and not for us.** All of them organise data as arrays of fixed-size pages, so "the modified page" is a natural bounded unit, and their workloads are page-oriented: a row update dirties one or two pages, a B-tree update O(log n).
PostgreSQL additionally amortises across a checkpoint interval.
kladde has neither property — allocations are arbitrary-sized and arbitrarily placed, and a flush's writes are scattered by construction.

Worth noting how Postgres *knows* where its page boundaries are, since it is a fair question: it does not discover them, it **imposes** them.
A relation file is a flat array of 8 KiB blocks from offset 0, so block *N* sits at offset *N* × 8192; alignment to the device follows from filesystem blocks being sector-aligned and partitions being 1 MiB-aligned.
The unit it protects is its own structural unit, not the OS page — and kladde has no such unit, which is the root of the difficulty.

### The options, and the decision

**Option 1 — give up the promise.** Concede content integrity for passenger regions after a power outage.
Cheaper than it sounds, since structure survives regardless and the file always opens.
The fatal flaw is that it is **silent**: the application reads plausible garbage and computes on it.

**Option 1′ — detect rather than prevent. ← chosen.**
The flush already knows every page it dirties, and — the useful observation — **tearing can only happen during a flush**, because that is the only time writes are in flight.
So the plan carries a **checksum per dirtied page**, and recovery verifies each one and reports the ranges that fail.

Cost: four bytes per dirtied page, in a structure that is discarded at the end of the flush anyway.
It converts silent corruption into a detected, localised error that the application can surface or repair from a backup — which is most of the value of full protection for a fraction of a percent of the cost.

**Option 2 — locality.** Co-locate allocations that are edited together so a flush dirties few pages, making full-page writes affordable.
The mechanism is sound; it is PostgreSQL's amortisation in another form.
Two objections to the *adaptive* version: learning co-edit statistics is exactly the tune-for-observed-workloads approach this project avoids, and it does not bound the worst case, since an application that genuinely edits scattered data still scatters.
It also fights compaction, which packs by size class and address rather than by access pattern.

The **declarative** version avoids both objections and already has a home: `later.md`'s **segments**, where allocations in a segment are co-located by construction from a hint the container gives rather than from learned statistics.
If segments happen this becomes nearly free; on its own, affinity tracking is not worth building.

**Option 3 — detect the substrate and skip.** PostgreSQL and InnoDB both allow disabling their protection on filesystems that already provide it.
Cheap to expose as a configuration knob, unreliable to detect automatically.

**Full-page writes remain available as an opt-in mode** for deployments on hardware that is not trusted, at the cost model above.

### The number that decides whether more is needed

**Distinct pages dirtied per flush.**
If the fold's coalescing and compaction's densification already keep it low, full-page writes may turn out affordable after all and this question closes.
If it is high, that is the evidence for segments — and a considerably better argument for them than the peak-memory one in `later.md`.
It is measurable as soon as the emitter exists, and it should be measured before anything beyond Option 1′ is built.

## Recovery

Your three steps are right, and one of them can be dropped.

**The simplification: recovery does not need to detect whether a crash happened during a flush.**
Because execution is idempotent and the salt overwrite is the only irreversible act, "we crashed mid-execution" and "we never started" are the same situation from recovery's point of view: in both cases the journal is intact and the correct response is to flush it.
So recovery is *unconditional*, which removes your second step and the `Apply(hoisting_id)` op you sketched to support it.

> **Claude: still true, and the distinction you are worried about is answerable — but only because of an ordering rule that has to be stated, not assumed.**
>
> The rule that makes it work:
>
> > **The plan's existence is published *after* the plan is complete, and *before* any byte of execution.**
>
> Given that, recovery has three cases and can tell them apart from the published pointer alone:
>
> | what recovery sees | what happened | what to do |
> | --- | --- | --- |
> | no plan published | crashed before or during writing the plan | generate a fresh plan — safe, because execution had not started |
> | plan published, CRC valid | crashed during or after execution | replay *that* plan; never regenerate |
> | plan published, CRC invalid | the publish itself was torn | treat as "no plan" and regenerate — safe for the same reason as case 1 |
>
> Case 3 is the one that needs the barrier from the `fsync` discussion: the pointer must not become visible before the plan bytes it points at, or a torn publish could name a half-written plan that nonetheless checksums. With `fsync` between "write plan" and "publish pointer", and a CRC over the plan, the three cases are exhaustive and disjoint.
>
> **A crash while writing the plan is therefore harmless**, which is the key asymmetry: writing the plan changes nothing about the file's contents, so it can be redone from scratch any number of times. Only *publishing* it is a commitment, and only *execution* depends on that commitment. That gives the flush two irreversible acts rather than one — publishing the plan, and overwriting the salt — with the invariant that between them, and only between them, the plan is authoritative.
>
> So the statement above should be refined rather than abandoned: recovery still does not need to detect *whether* a flush was interrupted, only *whether a plan was published*, which is a single pointer read.

The procedure:

1. **Recover `committed`.** Read the journal allocation, walk the frame chain from the header salt, and stop at the first frame whose chained CRC does not match or at the end of the allocation.
   Copy every op up to that point — stripping the salt and the frame headers — into the in-memory `committed`.
   If the first frame already fails, the journal is empty and there is nothing to do.
2. **If `committed` is non-empty, flush it**: fold, plan, execute, clean up, exactly as a normal flush would.
   The only difference is where the plan's addresses come from — see below.
3. **Otherwise, and afterwards, proceed to open normally.**

**Where the plan's addresses come from** is the one place the choice from step 3 shows up.
If the plan was recorded, read it and skip §2a's placement entirely; the recorded addresses are authoritative, and this is what guarantees that a re-run puts everything back where the interrupted run put it.
If the plan is re-derived instead, run §2a again and rely on determinism.
Note that under the recorded-plan option a *second* crash during recovery is handled by the same argument, recursively, with no extra machinery.

**One ordering constraint worth stating:** recovery must apply the journal **before** any application data is read, and before any new op is recorded.
Otherwise a reader would see the pre-flush bytes for anything the journal still holds.
This is what makes the journal invisible to application code as promised in the introduction.

**Answers to your two follow-up questions:**

- **Does recovery need new heap methods to change addresses?**
  Under the recorded-plan option, yes: something like `heap.place_at(id, address, size)` that installs a decision rather than making one, plus the ability to reconstruct the id pool.
  That method is worth having regardless — it is also what a file-format tool or an offline compactor would need — but note it is *dangerous* enough to keep out of the normal API surface, since it can produce overlapping allocations if misused.
  Under the re-derivation option you need no new method, which is that option's main argument in its favour.
- **Interactions with the on-disk `id → address` table.**
  The blocking dependency is that **replay needs the whole table as of the last flush**, not just the journal's own address, because every op is id-based.
  Concretely, still to be clarified: whether the table is written as part of each flush's cleanup or maintained incrementally; whether it is itself covered by the journal (it cannot be, for the circularity reason above — see my note on the journal's own allocation); and how its own torn-write case is handled, which probably wants the same chained-CRC treatment as the journal.
  Sketching recovery on the assumption that a consistent table is available at open time is the right call, but it is worth flagging that this is the largest remaining unknown in the whole design, and that it is on the critical path for *everything* here.

**Ideas / Details:**

- Maybe extend the vocabulary of ops on the journal by an `Apply(hoisting_id)` op and emit a single-op transaction with that op at the end of Step 3 (hoisting) so that recovery can detect whether a crash was during execution.
  This would mean that batch splitting and the trigger for automatic flushing should take the size of that trailing `Apply` transaction into account.
  But there might also be a simpler solution.
- Does recovery need new methods on the heap to change the addresses of allocations?
- Some interactions with how the `id --> Address` table is maintained on disk probably affect the precise recovery process.
  List what needs to be clarified but sketch a recovery process on the assumption that those points will be resolved.


## Differences to the previous design

### Improvements

**The journal is no longer pretending to do two jobs.**
The previous design left "is a flush a durability boundary or a memory-relief checkpoint?" open, and every question downstream — what goes in the write-ahead log, whether uncommitted data may reach storage, whether undo is needed — inherited that ambiguity.
Declaring commit to happen *at journal append* settles all of them at once: flushing changes representation, never state, so it needs no commit records, no undo, and no notion of a committed prefix.
That is the single biggest change, and most of the improvements below are consequences of it.

**Flushing is now genuinely free to reorder.**
The old design reached the same conclusion but had to argue for it; here it is a theorem: if every transaction is already committed, transaction boundaries carry no information a flush could violate.
This is what licenses the whole optimiser.

**The dependency graph now describes something real.**
Moving vertices from allocations to pieces — and, with §2a/§2b, to address ranges — turns a graph that cycled on any mutual reference into one that cycles only on genuine content rotations.
The previous design responded to those spurious cycles by hoisting *every* disturbed read, which made almost all of its edge machinery unreachable: instrumented, it fired once in 122 tests.
Here the edges do work, and buffering is reserved for cases where no ordering exists.

**Byte movement gets one mechanism.**
If §2a's recommendation is taken and the heap stops moving bytes, then relocation and compaction stop being invisible to the piece table.
That retires the one problem the previous design could not solve: an overlapping compaction slide was not re-runnable after partial application, and hoisting could not help because hoisting only resolves what the fold can see.
As a piece, an overlapping slide is a self-conflicting vertex and is handled like any other.

**Frees-before-claims and FFD stop being heuristics.**
The previous design wanted both and could only approximate them, because a release could be blocked by a transfer reading it — hence a priority queue, an "unblocks a release" rule, and the discovery that the rule had to be transitive to fire at all.
Separating the geometry decision from the byte movement (§2a) removes the obstacle entirely: releases block on nothing, so frees really do come first and FFD really is global.

Worth checking against the example that defeated the old scheduler — a copy out of an allocation that is then freed, followed by a new allocation that would fit in the freed space.
Under §2a the free is processed first, so the new allocation is placed in the vacated range; §2b then lowers the copy's source and the new allocation's write to the *same* address; §2c sees the overlap and emits one edge ordering the read before the write.
The old design needed hoisting to reach this outcome and a non-transitive priority rule that did not actually fire.
Here it is one edge, and it falls out of the same rule that handles everything else.

**Recovery collapses to "flush the journal".**
No crash detection, no `Apply` marker, no distinction between an interrupted flush and an untouched one.
This falls out of idempotent execution plus a single irreversible act, and it is the strongest argument that the shape of this design is right — the hardest-to-test path became the same path as the easy one.

**Crash tolerance is now specified rather than aspirational.**
The previous design had no framing, no checksums and a `todo!()` where `open` should be.
Chained salted CRCs give both end-of-journal detection and invalidation of stale frames from a previous journal, without the periodic full-journal write that zero-filling would need.

### Regressions

**Power-outage durability is explicitly given up**, where the previous design left it open.
This is the right call — an `fsync` per transaction would cost orders of magnitude — but it is now a *stated* limitation rather than an unexamined one, and it deserves prominent documentation, because "persisted immediately" and "survives power loss" are the same thing to most readers.
Worth also saying what the fallback is: a power outage may reset the file to the state at the last flush, which is structurally valid but may be seconds or minutes old, and may contain content ranges reported as torn ([[#durability-guarantees]]).

**Very large transactions cost file growth rather than only memory.**
The previous design's journal was an unbounded `Vec` in memory, so an oversized transaction was merely expensive.
Here the journal is a region of the file, so an oversized transaction grows it — but only while the journal is *empty*, per the policy in the transactions document, which makes the growth a free-and-reallocate rather than a resize-in-place.
The regression is therefore mild and bounded rather than undefined: a huge transaction now shows up as a temporary spike in file size, in addition to the in-memory buffer it already cost.
Note the in-memory cost has not gone away — `ops` still holds the whole transaction — so the journal capacity does not bound memory.

**Decision:** resolved by our earlier discussion in the chat and the updates to `kladde-docs/content/spec/transactions-and-batches.md`. The journal may grow but only when it's empty. Still unresolved: if we end up needing a write-ahead log (for minted addresses and possibly hoisted data in case of cycles), then we might still need to either grow the journal or allocate some special new space while the journal is actively used. This might be easier though than a generic journal growth because (i) we can pre-calculate the exact size of this allocation, (ii) it only needs to be placed once all other address updates are resolved, so it can be placed somewhere where it doesn't interfere with the execution phase of flushing (note that this is complicated though: it also must not reuse any space that is freed in the journal but that will still be read from during execution), and (iii) it's short-lived: once flushing is committed (i.e., the salt is overwritten), it can be removed. I think Properties (ii) and (iii) together would even allow us to forego giving this allocation an actual ID that would have to be recorded in the on-storage representation of the address table while flushing is going on. We only have to link to this special allocation from the journal (e.g., by reserving space in the journal the above `Apply` op, except that it wraps an address rather than an id). If a crash occurs during flushing, the link is still there and will be read during recovery. If a crash occurs after the salt was overwritten but before we could reclaim this special allocation, then the special allocation has already been forgotten anyway and will be reclaimed by the next compaction. So the order of operations might be: fold, run part 2a of planning to get the address (re-)assignments, do some incremental compaction steps and apply them to the calculated state, continue with the rest of planning to get the lowered piece table (which now covers both the explicit edits and anything resulting from compaction) and to resolve conflicts across both explicit edits and compaction, serialize a write-ahead plan in memory, find space for the write-ahead plan but don't technically allocate it, write the plan to that space, append the `Apply` op (in a transaction, as with any op) which links to the address of the write-ahead plan, execute the plan, overwrite the salt, shrink the file if the write-ahead plan sat at the end. This entire paragraph is a very rough idea an probably contains a lot of logical errors or overcomplications. Discuss the ideas in this paragraph, formulate in better language the simplest variant of it that you can come up with, and discuss alternatives.

> **Claude: the core insight is right and worth keeping. Two of the mechanisms are more complicated than they need to be, and one has a hole.**
>
> **What is right, and is the good idea here: the plan does not need to be an allocation.**
> Your properties (ii) and (iii) really do combine into "the heap never has to know about it".
> The reasoning is worth stating plainly because it is not obvious: the space is chosen *after* all placement decisions, so the heap already considers it free; nothing else runs during a flush, so nothing can claim it meanwhile; and after the salt overwrite it is simply forgotten.
> There is no leak to reclaim later — the heap never believed it was in use, so the next allocation is free to take it.
> That is strictly better than allocating it properly, which would be a heap mutation needing its own crash story.
>
> **Simplification 1: publish the plan in the journal *header*, not via an `Apply` op.**
> The `Apply`-op route costs you a journal append during a flush, a size reservation that batch-splitting and the flush trigger both have to account for, and a new opcode.
> All of that disappears if the journal header carries a fixed field next to the salt:
>
> ```
> header := salt:crc  plan_addr:u64  plan_len:u32  plan_crc:crc
> ```
>
> Publishing is then one small write to the header, and *un*-publishing is free: overwriting the salt already invalidates the header, so a stale `plan_addr` from a previous flush can never be mistaken for a current one — the same trick the salt already plays for stale frames.
> Keep the whole header inside one sector so both the publish and the salt overwrite are single untearable writes.
>
> **Simplification 2: put the plan past the end of the file.**
> Your caveat about not reusing space that is freed-but-still-read is real and it is fiddly: the constraint is that the plan's extent must avoid every post-flush live allocation *and* every pre-flush range that any surviving piece still reads.
> Computing that is possible but it is an easy place to get a subtle bug, and the bug would only show up under crash-plus-specific-layout.
>
> Appending past `end` satisfies both constraints trivially and needs no analysis at all.
> It costs a temporary file extension of the plan's size, and the truncation you already proposed at the end reclaims it.
> Given that the plan is small in the common case — addresses only, no bytes — this is close to free, and it removes an entire class of reasoning from the design.
>
> **The hole: your ordering puts compaction inside the crash window.**
> Running compaction steps between 2a and the rest of planning is attractive — it unifies compaction with the flush's byte movement, which is what §2a wants — but note what §2d and the `memmove` discussion above now imply: **compaction slides are exactly the pieces whose sources must be buffered into the plan**, and a slide of `L` bytes by `d` contributes `L − d` bytes of buffer.
> A small shift over a long run therefore puts nearly the whole run into the plan.
> So folding compaction into the flush can inflate the plan from "tens of bytes per touched allocation" to "megabytes", which changes both the latency budget and the temporary file growth from Simplification 2.
>
> This does not sink the idea, but it needs a bound: prefer evacuations (which produce no edges and cost nothing in the plan), and cap slides by the bytes they would add rather than by the bytes they would move.
>
> ---
>
> **The simplest variant I can formulate:**
>
> 1. Fold `committed`.
> 2. Decide geometry (§2a) — in memory, no writes.
> 3. Optionally run bounded compaction against that in-memory state, budgeted by *plan bytes added*, and merge its moves into the piece set.
> 4. Lower to address ranges (§2b), build the conflict graph (§2c), buffer every piece with an outgoing edge (§2d and the restartability rule above).
> 5. Serialise the plan in memory: the geometry decisions, plus the buffered bytes.
> 6. Write the plan past the end of the file. **`fsync`.**
> 7. Publish it by writing the journal header's `plan_*` fields. **`fsync`.**
> 8. Execute the plan.
> 9. **`fsync`**, then overwrite the salt — which simultaneously un-publishes the plan.
> 10. Truncate the file to drop the plan, and apply the deferred id recycling.
>
> Steps 6–7 collapse into one `fsync` if the plan and the header are written together and the header is last, which is worth doing: it brings the flush back to two barriers rather than three.
>
> **Alternatives worth knowing about, neither of which I would pick:**
>
> - **Reserve a fixed plan region adjacent to the journal**, sized for the worst case. Removes the file-growth wobble and the "find space" step entirely, at the cost of permanently occupying space that is almost always unused, and of a hard failure when the worst case is exceeded.
> - **Shadow paging**: never overwrite live bytes; write new versions elsewhere and flip a root pointer. This makes execution idempotent by construction and needs no plan at all — but it turns every in-place edit into a copy, which is precisely the cost model kladde is built to avoid. Worth naming so it is visibly rejected rather than overlooked.

**There is no testing strategy at all.**
This is the largest omission, and it is a regression against the *document*, not against the code.
The previous design opened its correctness section with "this is a small optimizing compiler, so build the oracle before the optimizer", and that was not decoration: the differential oracle found a pre-existing heap bug on its first run, and its shrinker reduced a second bug to a two-action reproducer.
Nothing here says how any of the fold, the conflict graph, the cycle breaker or the emitter will be checked.

What carries over unchanged, and should be written into this document:

- **A pure in-memory model** as a third implementation, so bugs shared by two implementations are still caught.
- **Randomised op sequences with a shrinker**, hand-rolled rather than generic, because op sequences have internal invariants a generic shrinker mostly violates.
- **Comparison on the observable surface only** — live ids, sizes, and bytes with `Undefined` ranges masked out — never on addresses, which are not observable and which two correct implementations may legitimately choose differently.

Two new things this design needs that the previous one did not, both of which are the interesting cases and neither of which a naive generator will produce by accident:

- **Crash injection.** Every claim about idempotence and restartability is currently unverified. The natural shape is to run a flush, abort it after *k* writes for every *k*, then run recovery and compare against the uninterrupted result.
- **Deliberate generation of conflicting copies**, since the cycle and edge cases are exactly what uniform random sampling misses — the previous oracle ran both hoisting modes and still never generated the mutually-conflicting copy that broke it.

**The gather run is no longer bounded.**
The previous design capped how many bytes one coalesced destination write could stage, so a single enormous run could not turn into an unbounded allocation, and split emission at that boundary.
§2e describes merging adjacent writes and merging across short gaps but says nothing about a cap, so as written a flush that touches one large contiguous region will try to stage all of it.
The cap should come back; it is a pure throughput knob, since splitting a run only costs an extra write.
Note the previous design also had an exception — a run reading its own allocation was staged whole, because splitting it would let the first write clobber bytes a later piece still had to read — which under the restartability rule above is subsumed: such a run is buffered into the plan anyway.

**The piece table's cheap representation is unspecified.**
The previous design spelled out `Uniform` (one segment, no allocation) spilling into a single flush-wide `BTreeMap<(Id, Offset), Source>` on the second segment, so the common case allocated no tree at all.
§4 notes only that "every table begins with exactly one entry, which is worth exploiting"; the exploitation should be written down, since it is what keeps the fold's cost proportional to *touched* allocations rather than to allocations with any structure.

**A bug in the optimiser is now a bug in recovery.**
The fold and planner run on the recovery path too.
Under the recorded-plan option this is bounded — recovery replays a list — but under re-derivation the entire optimiser runs on the least-tested path in the system, and a deterministic-but-wrong plan corrupts the file identically every time.
This is not worse than the previous design, which had the same property and had not noticed it, but it is worth stating as a reason to prefer recording.

**More state exists between "committed" and "in the file".**
The previous design's flush consumed the whole buffer; here `ops` may extend past `committed_cursor`, `pending` describes yet another point in time, and the fold describes the transition between two of them.
Three cursors into the same buffer is a lot of opportunity for off-by-one reasoning, and the flush-trigger policy deliberately keeps them distinct — a flush can leave a completed transaction outstanding so that it can go into an empty journal afterwards.
The concrete costs are that `pending` must be repopulated rather than cleared, and that the fold's output cannot be cross-checked against `pending`, which the previous design used as its main consistency assertion.
Worth compensating for with a different debug check, since that assertion was what caught divergence bugs cheaply.

### Problems I foresee

Ranked by how much I would want them resolved before writing code:

1. **The `id → address` table's own persistence** (see Recovery).
   Everything here assumes it is available and consistent at open time, and it cannot be protected by the journal, because the journal is addressed through it.
   This is the critical path.
2. **The journal's own allocation** — relocation by compaction, and the interaction between journal growth and the write-ahead plan's placement.
   The oversized-transaction case is now settled (grow only while empty); what remains is whether the plan can always go past the end of the file, and what happens if compaction wants to move the journal.
3. **How much compaction belongs inside the flush.**
   Sharper than it was: evacuations are free (no edges, nothing to buffer), slides cost `L − d` bytes of plan each.
   So the question is not "compaction or not" but "how many slide-bytes per flush", which is a budget that can be set once the plan's size is measurable.
4. **Deferred id recycling**, now stated as an invariant but still easy to violate silently, since the natural implementation of `Free` recycles at once.
5. **The `fsync` policy**, which the structural half of the [[#durability-guarantees|guarantee]] turns out to require — two barriers per flush, and they belong in the latency budget.
6. **The testing strategy**, which is absent rather than unresolved.
   It is last in this list only because it is not a *design* question; in implementation order it comes first, for the same reason it did previously.

**No longer open:** whether the plan is recorded or re-derived.
The restartability argument settles it: re-execution from scratch is correct only if the conflict graph is edgeless, so any flush with a single edge must carry the affected bytes durably regardless of how addresses are obtained.
Once the plan exists to carry those bytes, putting the addresses in it too is free — so **record always**, and treat the edgeless case as an optimisation that lets the plan shrink to addresses alone rather than as a separate strategy.
