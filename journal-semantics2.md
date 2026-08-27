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
- while the journal is guaranteed to survive an application crash, it is not guaranteed to survive a power outage; kladde still guarantees that the file is in a consistent state even after a power outage, but a power outage may reset a file to its state after the last flush (which is never in the middle of a transaction). Lifting this limitation would make mutations orders of magnitude slower.

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
A subsequent flushing operation does not promote the "level of persistence" (except that it introduces an `fsync`, which guards against power outage).

### Transactions and ordering discipline

Mutations are recorded to the journal in units of transactions, where each transaction wraps a sequence of ops.
Journaling operates only on a flattened list of transactions — [[../kladde-docs/content/spec/transactions-and-batches|batches and nested transactions]] are higher-level user-facing concepts that the backend translates to a flat sequence of transactions where every op is part of exactly one transaction (including free-standing ops, which the backend wraps in individual single-op transactions).

Kladde guarantees two properties for transactions:

- **Atomicity**: every transaction is either persisted completely or not at all, even if the application crashes or the device suffers a power outage.
- **Immediate persistence:** *as soon as a transaction is fully recorded in the journal*, it is considered persisted to the file and will survive an application crash (but not necessarily a power outage, which may reset the file to an empty journal or a prefix of the journal up until any transaction boundary).

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

This holds because `Copy` and `Splice` take *pieces from the source's current table*, not bytes.
By the time the fold reaches op *n*, the source's table already reflects ops 1…*n*−1, so the copy inherits their resolution rather than referring to them.
It is worth being explicit about this because almost everything downstream depends on it: it is why the plan can be reordered at all, why the dependency graph in Step 2 has the shape it does, and why hoisting (if needed) can read the file directly rather than having to interleave with execution.

**Starting state of a table.**

- **Fresh** ids (an `Alloc` for them appears in this journal) start as a single `Undefined` segment.
  Their content is fully symbolic; nothing about them ever needs to be read from the file.
- **Persistent** ids (live from before this journal) start as a single identity segment `{0 → Storage(self, 0)}`.

Every table therefore begins with exactly one entry, which is worth exploiting in the representation: the overwhelmingly common case is that it still has one entry at the end.

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

> **Claude: you are right, and the rewrite is now removed from the table above.**
> It was carried over from the previous design without checking whether it still made sense here, and it does not — it contradicts exactly the invariant you quote.
>
> **Why it existed.** In the previous design the identity test was *offset-based*: a piece was identity iff it read `Storage(self, k)` with `k` equal to its own segment start.
> After a conversion, `new`'s table still said `Storage(old, 0)`, which is not `self` under the new id, so it failed the test and emitted a pointless self-copy.
> Rewriting it to `Storage(new, 0)` restored the test.
>
> **Why it is wrong here.** §2b makes the identity test *address-based*: `pre_addr(Z) + k == post_addr(id) + seg_start`.
> A conversion is a relabel, so `post_addr(new) == pre_addr(old)`, and `Storage(old, 0)` in `new`'s table already satisfies it.
> The rewrite is therefore unnecessary — and actively harmful, because `pre_addr(new)` **does not exist**: `new` was minted during this journal and had no address before the flush.
> A `Storage(new, k)` piece is unlowerable.
>
> That gives a clean invariant worth stating alongside the one above:
>
> > The id in a `Storage` source is always an id that **existed before this flush**.
> > `Storage` sources are looked up in the *pre-flush* address map; destinations are looked up in the *post-flush* one.
>
> **And to your second question — no, and that is the other reason to drop the rewrite.**
> If `Convert` rewrote its own table, consistency would demand rewriting every `Storage(old, k)` in *every other* allocation's table too, since those pieces mean the same physical bytes.
> That is a scan over the whole fold state per conversion, turning a cheap op into an expensive one, and it would have been a silent correctness bug the moment any other allocation had copied out of `old` earlier in the same journal.
> With no rewrite, those references keep working untouched: `old` still has a pre-flush address, which is all they need.

Overwrite is the only non-trivial primitive: split at both boundaries (materialising each from the segment it falls inside), drop everything strictly between, splice the new pieces in.

**The running size matters, not just the final one.**
Every content rule above is bounded by the size *at that point in the journal*: `Write` is clipped to it, `Resize` needs the old value to know whether to clip or extend, and `Splice` needs it to locate the tail.
Keep it in the geometry record and mutate it as the walk proceeds; the value it holds when the walk ends *is* the final size, so there is no need for a second structure.

**Four things fall out without being coded as special cases**, and they are most of why folding is worth doing:

- **Repeated writes to the same range emit one write**, because the table holds exactly one source per byte by construction.
- **An allocation created and freed within one journal never touches storage at all**, and a `Copy` out of it resolves to literals rather than forcing it to be materialised.
- **An identity piece emits nothing.** `Storage(self, k)` with `k` equal to the segment start means the bytes are already where they belong; without this check every untouched region of every persistent allocation would be copied onto itself.
- **A size-preserving `Convert` emits nothing**, because a relabel keeps the address, so every piece that was identity before is still identity after.
  This is what makes a chunked container that oscillates across a chunk boundary cheap, and it is worth a test.

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

1. **Frees, and the shrinking half of every resize** — everything that returns space to the heap.
2. **Claims, and the growing half of every resize** — everything that takes space, processed in **descending size order** (first-fit-decreasing), so the large ones are served while the large gaps are still intact.
3. **Relabels**, which change no geometry at all and can go anywhere.

Both of these were *best-effort* in the previous design and are **unconditional** here, which is worth dwelling on because it is the clearest single benefit of separating the decision from the action.

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
>
> **On `memmove` for slides:** it does not make them crash-resistant, only correct within a single uninterrupted execution — see the answer under §2d.
> And yes, `compact_incrementally` collating its byte movements and returning them rather than performing them is exactly the shape this wants; that is the same "report, don't act" change, applied one layer up.

#### 2b. Lower pieces to address ranges

Rewrite every piece as an absolute extent pair:

```
dst: [post_addr(id) + seg_start,  + len)   ←   src: Literal(offset in `committed`)
                                            |   Storage[pre_addr(Z) + k, + len)
                                            |   Undefined
```

Two things become simpler at this level:

- **The identity test is now an address comparison**, `pre_addr(Z) + k == post_addr(id) + seg_start`, rather than an offset comparison.
  A relocated allocation's previously-identity pieces automatically stop being identity and become ordinary copies — which is exactly the byte movement the heap used to do behind the piece table's back.
- **All destinations are globally disjoint.** Within an allocation the piece table partitions it, and distinct live allocations never overlap.
  §2c exploits this.

Drop `Undefined` pieces here: they need no write, by the content contract.
Drop identity pieces here: the bytes are already correct.
What remains is exactly the set of writes the flush must perform.

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
  In a flush with no `Copy` and no relocation — the overwhelmingly common case — *every* piece is `Literal`-sourced and the graph has **no edges at all**.
- **Destinations are disjoint**, so they can be sorted into one array of non-overlapping intervals over the address space.
  Finding all destinations overlapping a given source range is then a binary search plus a contiguous scan, giving **O(V log V + E)** rather than the O(V²) that pairwise comparison would cost.
  This is the reason not to coarsen the graph: per-piece edges are already cheap to build, and coarsening to allocations reintroduces exactly the false cycles §2d is about.

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
>         if D[i].end > P.src.start and D[i].vertex != P:
>             succ[P].append(D[i].vertex)   # P must run before D[i]
>         i += 1
> ```
>
> The `- 1` is the only fiddly part: `lower_bound` finds the first destination *starting* at or after the query start, but the destination immediately before it may still overlap, so back up one and let the `D[i].end > P.src.start` test reject it if it does not.
>
> **Edge representation.** Adjacency lists (`Vec<Vec<u32>>`) are fine and are what Tarjan and Kahn both want.
> If the allocation churn shows up in a profile, count degrees in a first pass and pack into CSR — one `Vec<u32>` of neighbours plus one `Vec<u32>` of offsets — but do not start there.
> The expected case is `E == 0`: without `Copy` and without relocation, no vertex has a `Storage` source at all, the loop body never runs, and every adjacency list stays empty.
> That is worth special-casing at the top (`if no vertex has a Storage source: skip Steps 2c–2d entirely`), because it turns the common flush into a sort plus a linear emit.

#### 2d. Cycles

> **Why cycle-breaking was an issue in the old design, and whether the piece table is a DAG by construction.**
>
> Taking the second question first, because the phrasing hides the answer: **the piece table is not a graph, so it cannot be a DAG.**
> What can cycle is the *conflict graph over pieces*, which is a different object, and it depends on the granularity you build it at.
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
> Notice the fold cannot remove these but *does* remove the near misses — a swap written without the witness `W` folds to a no-op, because the second copy reads through the first.
>
> So cycle-breaking is still needed, but it goes from "triggered by any mutual reference between two allocations" to "triggered by an actual rotation of bytes", which is rare enough that you can afford to handle it in the obviously-correct way.

**Detection and breaking.** Run Tarjan's SCC over the conflict graph.
Any component with more than one vertex, plus any self-looping vertex, must be broken.
Break by picking the **smallest-byte-count vertex** in the component, reading its source range into memory, and rewriting it as a `Literal`.
That removes its outgoing edges — a literal reads nothing in the file — and therefore its participation in every cycle through it.
Repeat until the graph is acyclic; each break strictly reduces the number of `Storage`-sourced vertices, so it terminates.

> **Claude — pseudocode.** You do not have to implement Tarjan yourself; `petgraph`'s `tarjan_scc` (or `kosaraju_scc`) does it, and either is a fine starting point.
> What matters for the mental model is the shape of the loop around it:
>
> ```text
> loop:
>     components = strongly_connected_components(graph)   # Tarjan, O(V + E)
>     bad = [c for c in components
>              if len(c) > 1 or c[0] has an edge to itself]
>     if bad is empty:
>         break                       # graph is now a DAG
>     for c in bad:
>         victim = the vertex in c with the fewest bytes
>         read victim.src from the file into memory
>         victim.src = Literal(that buffer)   # now reads nothing in the file
>         delete every outgoing edge of victim
> ```
>
> **Why it terminates:** each pass converts at least one `Storage`-sourced vertex into a `Literal`-sourced one, and a `Literal` vertex can never be in a cycle (a cycle needs every member to have an outgoing edge).
> So the number of `Storage` vertices strictly decreases, and in the worst case the graph ends up entirely `Literal` and trivially acyclic.
>
> **Intuition for what a "component" means here:** a strongly connected component is a set of pieces that all transitively wait for each other, i.e. a rotation of content.
> Picking the smallest one to buffer is what breaks the rotation with the least I/O — the same trick as picking the smallest element to hold in a temporary when swapping.
>
> In practice this loop runs zero times, because `bad` is empty on the first pass in every flush that has no cycle.

**Self-loops** — a piece whose `dst` overlaps its own `src` — arise from a splice shifting a tail or from a compaction slide.
They are handled by the same rule as any other cycle: buffer the overlapping bytes.

> **Claude — on your `memmove` question, and it is the one that changes the design most.**
>
> **There is no `memmove` for files on any of the three platforms.**
> Linux's `copy_file_range` rejects overlapping ranges within one file; macOS's `fcopyfile` and Windows have no intra-file equivalent at all.
> The closest thing is `mmap` plus a real `memmove`, which works but hands you `msync`'s durability semantics instead of `write`'s — a different set of problems, not fewer.
> So in practice an overlapping in-file move is a loop of bounded reads and writes in the direction that reads ahead of where it writes, i.e. you are buffering anyway, just a chunk at a time.
>
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
> First, **the graph is nearly always edgeless**, as noted in §2c: without `Copy` and without relocation there are no `Storage` sources at all, so the topological constraint is vacuous and you are free to emit in whatever order you like.
> Coarse vertices did not give you that freedom; they gave you *fewer* vertices with the *same* constraints.
>
> Second, a topological order is a *partial* order, so even when there are edges you choose among the ready set.
> Spend that freedom on locality rather than on anything else.

Emit with Kahn's algorithm over a priority queue keyed by **destination address**, taking the ready vertex whose destination is nearest above the last one written and wrapping to the lowest when none is (the elevator heuristic).
This produces long ascending runs of destinations, which is what the device wants.

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
> So this can be replaced wholesale by "emit `ready` sorted by `dst.start`" if the graph is edgeless — which, again, is the common case, and is why the edgeless fast path is worth having.

Then **merge adjacent emitted writes**, and merge *across short gaps*:

- across an `Undefined` gap for free, since uninitialized bytes may legally receive anything;
- across an identity gap at the cost of rewriting bytes that were already correct.

A threshold of about a page for both is the obvious rule.
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
> **What this does not change.** In the overwhelmingly common flush there are no `Storage` sources at all, hence no edges, hence nothing to buffer — so re-derivation *is* viable there, and step 3 really does reduce to recording addresses. The hybrid is therefore well defined and worth stating as the rule:
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
   > A **power outage** loses the page cache, and what reaches the platter is an **arbitrary subset** of the pending writes — not a prefix. Devices and the block layer reorder freely. On top of that, the sector being written at the moment power fails can be **torn**: partially updated, or on some consumer SSDs garbage, because the drive was mid-way through a read-modify-write of a larger internal page.
   >
   > So a power outage can produce states no program crash could: write B present without write A, or a single sector holding neither its old nor its new contents. `fsync` returning is the only thing that converts "issued" into "durable", and a power outage *during* `fsync` simply means it never returned, so nothing it was flushing is guaranteed.
   >
   > **How SQLite and friends handle it**, three techniques, all applicable here:
   >
   > - **Checksums over everything self-describing**, to detect torn sectors rather than trusting them. Your chained CRCs already do this for the journal; the write-ahead plan needs the same.
   > - **`fsync` as a barrier, not as durability.** The pattern is always *write A; fsync; write B* where B's presence is meant to imply A's. Without the barrier the implication is false.
   > - **A single-sector commit record**, small enough that the device's own atomicity guarantee covers it. SQLite assumes 512-byte sector atomicity by default and exposes `SQLITE_IOCAP_ATOMIC` for devices promising more.
   >
   > **What this means for kladde, and it is a real consequence for the current design.**
   > The introduction promises the file is *consistent* after a power outage even though it is not *durable*. That promise needs barriers — without them, the salt overwrite could reach the platter while some of the data writes it commits do not, and recovery would then discard a journal whose effects are only partly applied. That is precisely the corruption the design is trying to avoid.
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
Worth also saying what the fallback is: a power outage may reset the file to the state at the last flush, which is a consistent state but may be seconds or minutes old.

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
5. **The `fsync` policy**, which the consistency-after-power-loss promise turns out to require — two barriers per flush, and they belong in the latency budget.
6. **The testing strategy**, which is absent rather than unresolved.
   It is last in this list only because it is not a *design* question; in implementation order it comes first, for the same reason it did previously.

**No longer open:** whether the plan is recorded or re-derived.
The restartability argument settles it: re-execution from scratch is correct only if the conflict graph is edgeless, so any flush with a single edge must carry the affected bytes durably regardless of how addresses are obtained.
Once the plan exists to carry those bytes, putting the addresses in it too is free — so **record always**, and treat the edgeless case as an optimisation that lets the plan shrink to addresses alone rather than as a separate strategy.
