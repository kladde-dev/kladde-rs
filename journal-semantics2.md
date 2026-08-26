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
| 1      | `Free(id)`                                    | releases the memory allocation with `id` (giving up `id` for recycling and freeing the associated memory)          |
| 2      | `Resize(id, new_size)`                        | changes `id`'s size, preserving `min(old, new)` bytes and appending `max(0, new-old)` bytes of uninitialized data. |
| 3      | `Convert(old_id, new_id, new_size)`           | `Resize` with additional changes of sizedness, minting `new_id` and giving up `old_id` for recycling               |
| 4      | `Write(id, offset, bytes)`                    | overwrites `bytes.len()` bytes at `offset` within `id`                                                             |
| 5      | `Splice(id, offset, old_len, bytes)`          | replaces `old_len` bytes at `offset` with `bytes`, shifting the tail and resizing                                  |
| 6      | `Copy(src, src_offset, len, dst, dst_offset)` | copies a byte range between (or within) allocations, overwriting at the destination                                |
`Splice` and `Copy` are the only records that read existing content, which matters to the folding phase of flushing, see below.
(TODO: reconsider this sentence once fold is formulated.)

TODO: maybe remove `new_size` from `Convert` and require caller to manually emit `Convert` *and* `Resize` in the correct order.
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
Since the `salt` changes after every flush, any stale data in the journal allocation that contains previously valid transactions from a previous journal have become invalid as soon as that previous journal was flushed (TODO: check ordering: when exactly does `salt` have to be updated on-storage during a flush?).

**Possible alternative:** If the CRCs turn out to eat up too much journal size, we might consider explicitly clearing out the journal allocation with zero bytes when it gets first allocated and after every flush (unless it's at the end of the file, in which case we can simply shrink the file), which would allow us to mark the end of each transaction with a single nonzero byte rather than a 4-byte CRC.
Claude: briefly discuss the trade-offs.

### In-memory representation

The in-memory representation of the journal maintains the data structures:

- `committed: &[u8]` — a mirror of the ops that are committed to the on-storage representation of the journal, without the journal header (salt) and the transaction framing (`count_tag` and `crc`); thus `ops := op* = (opcode:byte payload:byte*)*`;  a mirror of the on-storage representation.
  See [transactions and Batches](../kladde-docs/content/spec/transactions-and-batches) for an explanation how the backend maintains `committed` as a prefix of a buffer `ops: Vec<u8>` that grows any time a transaction is committed.
  The salt and transaction framing are stripped from `committed` because their only purpose is to make the *process of writing a transaction to the file* transactional.
  Including them in them in the in-memory representation would complicate parsing `committed` unnecessarily.
  Storing the in-file representation of committed ops in a serialized form rather than as `Vec<Op>` will likely simplify indices into literals and potentially reduce the number of heap allocations significantly, although but I'll have to check if this really works out (TODO; we'll probably still need to pass an `enum Op` around internally, so in order to reduce the memory allocations that enum would have to hold binary data by reference or `Cow<[u8]>`).
- `pending: HashMap<Pointer, Size>` — a *state snapshot* of the sizes of any allocations whose size has changed since the last flush.
  Unordered, last-writer-wins, self-annihilating.
  Updated whenever operations are appended to `ops` (not `committed`) and cleared after flushing (operations that release `id`s are only delete the corresponding entry from `pending` if present, they don't introduce a tombstone, see note below).
  It is used by the backend to resolve size queries (any entry in `pending` has precedence over the heap).
  It is *not* consulted during flushing since it reflects the state at `ops.end()`, not at `committed.end() == commmitted_cursor` (see [transactions and batches](../kladde-docs/content/spec/transactions-and-batches)).
  TODO: `pending` is therefore actually independent from flushing.
  It is discussed here only to contrast the new proposal to the previous design.
- There is no analog to `pending` for the *contents* of changed allocations — kladde never serves any content from the file to application code or data type implementations while `ops` is non-empty.

**Note:** I think the definition of `pending` means that the backend can't resolve (without scanning `ops`) which allocations have been freed since the last flush.
Thus, a `resolve` query against a dangling pointer might return stale data of the pointer before the pointer was flushed.
Claude: is this correct?
If it is, I think that's OK — implementations of data types already must not make any assumptions about freed pointers because they might have been recycled by a different data type since.
The only thing this adds is that implementations of data types can't even assume that a freed pointer will report as dangling *immediately after it was freed*, and I think that's fine.
But it should be documented on `resolve` and on any other backend method that might return stale data for pointers that are freed in the currently active journal.

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

### Step 1: Folding

Claude: fill in the details here.
Folding remains mostly unchanged from the previous design, except that it should operate on `committed` (parsing the ops), indices for `Literal` should go into `committed` (not into the on-disk version of the journal), I think I slightly changed the signature of some ops, and the resulting shapes cannot be compared to `pending` since `pending` represents the state at `ops.end()` whereas the result of folding represents a transition to the state at `committed.end() == committed_cursor <= ops.end()`.

### Step 2: Planning

Claude: help me rethinking this part from scratch.
The only idea I want to keep here from the previous design is that I want to start with considering the piece table as a directed graph.
However, I want to now try to consider it as a graph where the vertices are the rows of the piece table, not whole allocations (or maybe, as an optimization, the edges should be the union of pieces and allocations to avoid having to create lots of edges for dependencies that truly affect the entire allocation).

Explain:

- Why was cycle-breaking an issue in the old design?
  Isn't the piece table a DAG by construction?
  Did cycles only occur because we considered a graph over allocations, not over pieces?
- Will treating each piece as an edge of the graph cost I/O performance by preventing sequential reads and writes?
  Or can we design a graph algorithm that sorts edges in a way that satisfies topological order but also enables sequential reads and writes as well as possible?

### Step 3: Hosting

Deferred for now until planning is designed (I'm not sure if we'll need it).

### Step 4: Executing

Deferred for now until planning is designed.

### Step 5: Cleaning Up

Deferred for now until it is clear whether hoisting is still required.

## Recovery

Claude: fill in the details but keep it brief.
What does kladde do when it opens a file and sees a journal allocation?
Defer the discussion of how the journal allocation is found (that's part of the self-hosting persistence of the heap, which is still TBD).
Steps are probably (but check and feel free to simplify if possible):

- Find the last valid CRC in the journal and copy everything until it except header and transaction frames into `committed`.
- If `committed` is non-empty probably find out somehow whether the crash was during the execution step of flushing (i.e., probably whether a completed hoisting section exists).
- Redo folding and planning, but if a completed hoisting table exists leave out hoisting and instead overwrite any relevant data in the plan (e.g., addresses of new or relocated allocations) with the information in the hoisting table; then execute the plan and clean up (Steps 4+5). 

**Ideas / Details:**

- Maybe extend the vocabulary of ops on the journal by an `Apply(hoisting_id)` op and emit a single-op transaction with that op at the end of Step 3 (hoisting) so that recovery can detect whether a crash was during execution.
  This would mean that batch splitting and the trigger for automatic flushing should take the size of that trailing `Apply` transaction into account.
  But there might also be a simpler solution.
- Does recovery need new methods on the heap to change the addresses of allocations?
- Some interactions with how the `id --> Address` table is maintained on disk probably affect the precise recovery process.
  List what needs to be clarified but sketch a recovery process on the assumption that those points will be resolved.


## Differences to the previous design

Claude: fill in the sections below.
Don't discuss all differences in the implementation (there are plenty in the details).
Only discuss the differences in the *effect* of the design: what guarantees does this design miss that the previous design either had or also missed?
What guarantees does this design add where the previous design was faulty.
What problems do you foresee?

### Improvements

Claude: fill in

### Regressions

Claude: fill in
