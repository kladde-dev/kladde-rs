# Journal semantics, and the flush optimizer

Status: design note. Nothing here is implemented. The current
`JournaledWriteBackend` (`crates/kladde-heap/src/journaled.rs`) contradicts §2 in
three places and is known-broken; §10 gives the order in which to replace it.

The subject is the *deferred* write path: what a transaction records, what a flush
is allowed to do with the recording, and what "the same thing happened" means when
the flush is permitted to reorder and elide work. It does not cover placement
policy (see [incremental-compaction.md](incremental-compaction.md)) or the
compaction step itself (see [augmented-segment-tree.md](augmented-segment-tree.md)).

## 1. What is broken today, and why it is one bug

`JournaledWriteBackend` keeps **two** records of a transaction:

- `pending: HashMap<Pointer, Size>` — a *state snapshot*. Unordered,
  last-writer-wins, self-annihilating. `alloc`/`free`/`resize`/`make_*` fold into it.
- `journal: Vec<(Pointer, Size, Vec<u8>)>` — an *operation log*. Ordered,
  append-only, replayed verbatim.

No order relates the two, so any operation that invalidates an earlier journal entry
must reach into the journal and repair it by hand. Exactly one does — `remint`
scans the journal re-anchoring writes across a sizedness conversion. The
consequences of the ones that do not:

1. **`alloc → write → free` panics at flush.** The `alloc` and `free` annihilate
   inside `pending`, so the id is never claimed; the buffered write survives in the
   other structure and its replay reaches `heap.lookup` → `None` →
   `"operation on a dangling pointer"`.
2. **`write` then shrinking `resize` silently corrupts a neighbour.** `seek_to`
   does no bounds check, so a write buffered while the allocation was large replays
   at an offset that now lies outside it. Reproduced: allocate A at 128 bytes,
   write `0xAA` at offset 64, shrink A to 64, allocate B at 8 bytes; after flush B
   reads back as `[170; 8]`.
3. **`make_*` on an allocation claimed by an earlier checkpoint loses its
   content.** The immediate path (`Composed::convert`) is
   `mint → alloc(new) → copy min(old,new) → free(old)`; the deferred path performs
   only the `mint`. The copy is not deferred or approximated — it is absent.

The more fundamental statement of the same defect is not any of these. It is that
**the journal does not record every mutation**, so a crash mid-transaction is
unrecoverable in a way that has nothing to do with annihilation: a `Persistable`
that allocates a child and writes the child's id into its own header emits an
`Alloc` that the log does not contain and a `Write` that it does. Recovery then
reads a live pointer out of the parent that names an allocation nothing ever
created. Two operations, no `free` in sight, and the result is a dangling pointer
in recovered application data.

## 2. The model

> **The log is the sole authority.** It is a totally ordered sequence of every
> mutation. Everything else is derived from it and may be discarded and rebuilt.

The log carries six operation kinds, which is exactly the closure under "an entry
whose absence would make some other entry unreplayable or misinterpretable":

| op | why it must be logged |
| --- | --- |
| `Alloc(id, size)` | otherwise a `Write` has no target and a serialized id dangles. Sizedness rides on the id, so it needs no separate field. |
| `Free(id)` | annihilation, and the id's release |
| `Resize(id, size)` | bounds for replayed writes; content destruction |
| `MakeFixed`/`MakeResizable(old → new, size)` | id identity across a sizedness conversion |
| `Write(id, offset, bytes)` | content |
| `Splice(id, offset, old_len, bytes)` | content, with a tail shift |

A future `Copy(src, src_off, len, dst, dst_off)` joins the content group; §6 is
written with it in mind because it is the operation that makes the scheduling
question interesting.

Two derived structures, and it matters that they are derived:

- **`pending`** (§3) — write-phase geometry, so `size`/`resolve` are O(1).
  Discarded at checkpoint.
- **`Folded`** (§4) — geometry *and* content, built at flush from the log alone,
  consumed by the scheduler, then dropped.

`Folded` recomputes the geometry `pending` already had. That redundancy is the
point: comparing them on every flush is the consistency check that all three bugs
in §1 would have tripped, and it is asymptotically free (the fold is already
O(log length); the comparison is O(touched ids)).

### 2.1 Content semantics

A caller of `alloc` or `resize` **must not assume anything** about the content of
any uninitialized part of an allocation. Except for the first
`min(old_size, new_size)` bytes after a `resize`, an allocation may contain
arbitrary data or none at all.

Truncation is therefore *allowed to* destroy content, not required to. A
shrink immediately followed by a grow may be optimized away entirely, and the
bytes in the re-grown region may legally be whatever was there before. This is
what licenses most of §4's folding rules; without it they would be unsound.

### 2.2 One id, one lifetime

> **A counter returns to the id pool only at checkpoint**, after the frees are
> applied.

`Composed::free` already couples `heap.free` and `recycle`; the deferred path must
keep them coupled. This is a precondition for everything below, not an
optimization: the log and both derived structures are keyed by id, so if a counter
could be recycled mid-transaction then

```
Alloc(id), Write(id, …), Free(id), Alloc(id), Write(id, …)
```

would place two allocation lifetimes under one key. The fold cannot emit two claims
for one key, and the first `Write` would be attributed to the second allocation.

It also makes `Freed → New` unreachable rather than merely discouraged, which is
what keeps §3's table finite.

## 3. `pending`: the write-phase geometry cache

Five states, of which two are "absent" and must be distinguished:

| state | in `pending`? | live in heap? | checkpoint action |
| --- | --- | --- | --- |
| `Absent/live` | no | **yes** — claimed earlier, untouched this transaction | nothing |
| `Absent/dead` | no | no — never minted, or freed and already checkpointed | nothing |
| `New(S)` | yes | no | `heap.alloc(id, S)` |
| `Resized(S)` | yes | yes | `heap.resize(id, S)` |
| `Freed` | yes | maybe | `heap.free(id)` **if live**, then recycle the counter |

`New` and `Resized` are distinct because they call different heap methods, not
because they carry different data.

### 3.1 Transitions

| op | from | to | note |
| --- | --- | --- | --- |
| `alloc` | `Absent/dead` | `New(S)` | the only source state, by §2.2 |
| `write` | any live state | **unchanged** | content only; `pending` is geometry |
| `resize` | `New(S)` | `New(S′)` | |
| | `Absent/live` | `Resized(S′)` | |
| | `Resized(S)` | `Resized(S′)` | |
| | `Freed`, `Absent/dead` | — | `Err(DanglingPointer)` |
| `make_*` | old: `New(S)` | old: `Freed` | **two entries at once** |
| | old: `Absent/live` / `Resized(S)` | old: `Freed` | + content must move (§6) |
| | new: `Absent/dead` | new: `New(S′)` | fresh id, fresh counter |
| `splice` | as `resize` | as `resize` | size change plus content ops |
| `free` | `Absent/live` | `Freed` | |
| | `Resized(S)` | `Freed` | |
| | `New(S)` | `Freed` | **not `Absent`** — see below |
| checkpoint | `New` / `Resized` | `Absent/live` | map cleared |
| | `Freed` | `Absent/dead` | counters recycled here, and only here |

`write` earns a row precisely because it is a no-op on the map. If `write` ever
needs to touch `pending`, something has gone wrong.

`free` of a `New` entry goes to `Freed`, not `Absent`. Going to `Absent` loses the
information that the counter must be recycled at checkpoint — that is the current
counter leak. Making the checkpoint's free lookup-guarded
(`if heap.lookup(id).is_some() { heap.free(id) } ; recycle(id)`) lets one state
serve both cases without a fourth variant, and `size()` correctly reports
`DanglingPointer` either way.

Most of the error cells are statically unreachable: `free_*` and `make_*` consume
the owned handle and `resize` requires `&UniquePointerResizable`. The escape hatch
is `Backend::resolve`, whose own doc notes it "can mint a second owner of an owned
region" — so the `Freed` / `Absent/dead` cells *are* reachable through a resolved
duplicate and do need runtime checks. Query order is therefore: `pending` first,
heap second. Never trust the typestate alone.

## 4. Phase A — the fold

Per id, linear, no graph. For each id the log touches, walk its op subsequence
maintaining whether it exists, its current size, and a **piece table** over its
bytes.

```rust
enum Source {
    Literal(ArenaPos),      // bytes buffered in the journal's byte arena
    Storage(Id, Offset),    // must be read from the file
    Undefined,              // uninitialized; may be anything (§2.1)
}
```

A piece table is a sorted map from **segment start offset** to `Source` — one entry
per segment boundary, never per byte. A byte's origin is found with
`range(..=offset).next_back()`, giving `(seg_start, source)`; the origin is that
source advanced by `offset - seg_start`. `Source` is offset-relative precisely so
that advance is meaningful.

The distinction that drives everything:

- **Fresh** ids (an `Alloc` appears in this log) start `{0 → Undefined}`. Their
  content is *fully symbolic* — nothing about them ever needs to be read.
- **Persistent** ids (live from an earlier checkpoint) start
  `{0 → Storage(self, 0)}`.

Every table therefore begins with exactly **one** entry, which is what makes §7's
representation natural rather than bolted on.

### 4.1 Per-op rules

| op | effect on the table |
| --- | --- |
| `Alloc(s)` | fresh table, `Undefined` over `[0, s)` |
| `Write(off, bytes)` | overwrite `[off, off+len)` with `Literal` |
| `Resize(s′)` | clip to `s′`, or extend with `Undefined` |
| `Splice(off, old_len, new)` | overwrite, then **shift the suffix segments** |
| `Copy(src, …)` | overwrite the destination range with pieces taken from *src's current table* |
| `Free` | mark released |

Overwrite is the only non-trivial primitive: split at both boundaries (materializing
them from the covering segment), drop the entries strictly inside, insert the new
one. **Coalesce on insert** — a loop of sequential writes produces adjacent literals
contiguous in the arena, and without merging you get one entry per write where one
per run would do. This is not a micro-optimization; it is what keeps the common case
at one or two segments.

The two questions this answers directly: `Alloc(s1); Resize(s2)` folds to a single
`Alloc(s2)` with the clip deciding what happens to writes in between, and
`Resize(s1); Resize(s2)` folds to a single `Resize(s2)` — where, if `s1 < s2`, the
bytes in `[s1, s2)` become `Undefined` and may be left on disk exactly as they are.
That last part is legal only because of §2.1.

### 4.2 What falls out unasked

- **`splice` on a fresh allocation costs zero I/O.** It is a table edit; the shift
  never touches bytes. Since `PersistableString`/`PersistableVec` splice in loops,
  this is probably the largest single win available.
- **A transient allocation — fresh and freed in the same log — never touches
  storage.** Recycle its counter and emit nothing. A `Copy` *out* of it resolves to
  literals, so the read does not force it to be materialized.
- **An identity piece emits nothing.** `Storage(self, k)` with `k == seg_start`
  means the bytes are already where they belong. Without this check every untouched
  region of every persistent allocation would emit a self-copy.

## 5. Phase B — the schedule

Vertices are the **emitted actions**, not the log's ops — the fold changed the op
set:

```
Claim(id, size)          Release(id)
Reshape(id, size)        Transfer(src, src_off → dst, dst_off, len)
Write(id, off, bytes)
```

`Reshape` stays **one vertex**. It is tempting to decompose it into
`Claim`/`Transfer`/`Release` — that decomposition is a good explanation of why
`make_*` and the multi-transaction `resize` are the same latent hole — but it is the
wrong granularity here. The heap decides the new placement and moves the bytes as
one atomic operation; splitting it would put the old range's release into the
schedule as a separate vertex, and a frees-first pass could then hand that range to
another `Claim` before the copy ran.

### 5.1 Edges

1. `Claim(id)` → every action writing into `id`.
2. `Transfer` reading `id` → `Release(id)`. *(the `make_*` edge)*
3. `Transfer` reading a range of `id`'s storage → later actions writing that range
   (a WAR anti-dependency — easy to forget, since it runs backwards from the usual
   intuition).
4. Earlier writes to a range → a `Transfer` reading it. The fold has usually already
   turned these into literals, so this edge mostly evaporates.

**It is a DAG by construction**: every hard edge points from a lower log position to
a higher one.

### 5.2 Frees-first is a preference, not an edge

Running `Release`s before `Claim`s gives the placement pass more space to work with.
It must be encoded as a *priority*, never as an edge. `make_*` on a persistent id
produces `Claim(new) → Transfer → Release(old)`; add `Release → Claim` as a hard
edge and that is a three-cycle.

So: **Kahn's algorithm with a priority queue over the ready set**, in order

1. anything that unblocks a `Release` (see §6 for why this matters more than it looks),
2. `Release`,
3. `Claim`, largest size first (FFD),
4. `Reshape`, `Transfer`, `Write`.

FFD is therefore *best-effort over the ready set* rather than global — a `Claim`
blocked behind a `Transfer` is placed after smaller ones. Only `make_*`-style chains
block, but the guarantee is weaker than the current unconditional sort and that
should be stated where it is relied on.

Compaction stays outside the graph as a final phase: it moves only live ranges, and
by then every `Storage` read has been resolved.

## 6. Read hoisting, and a worked example

### 6.1 The scenario

After an earlier checkpoint the heap holds `A1` at address 1000, size 256, and `A2`
at address 2000, size 128 — both persistent. This transaction logs:

```
1.  Copy(src = A1, src_off = 0, len = 64, dst = A2, dst_off = 32)
2.  Free(A1)
3.  Alloc(A3, 200)          // 200 ≤ 256, so it would fit in A1's gap
4.  Write(A3, 0, [64 bytes])
```

The fold gives:

- **A1** — persistent, released, table untouched: `{0 → Storage(A1, 0)}`.
- **A2** — `{0 → Storage(A2,0), 32 → Storage(A1,0), 96 → Storage(A2,96)}`. The
  outer two are identity and emit nothing; the middle is a real 64-byte read of A1.
- **A3** — `New(200)`, `{0 → Literal(w), 64 → Undefined}`.

Actions: `Transfer(A1@0 → A2@32, 64)`, `Release(A1)`, `Claim(A3, 200)`,
`Write(A3, 0, …)`. Edges: `Transfer → Release(A1)` and `Claim(A3) → Write(A3)`.

### 6.2 What the naive priority does

Ready set is `{Transfer, Claim(A3)}` — `Release(A1)` is blocked. With the priority
of §5.2 minus rule 1, `Claim` outranks `Transfer`, so `Claim(A3, 200)` runs first.
At that instant **A1 is still live in the heap**, so the gap at 1000 does not exist
and `heap.alloc` cannot place A3 there; A3 lands elsewhere or extends the file.
Only then do `Transfer`, `Release(A1)` and `Write(A3)` run.

Result: A3 does not reuse A1's space, and compaction claws it back on a later
round. Note the scheduler did not "postpone" the free — it ran it as early as the
dependencies allowed. The free simply could not be early enough.

### 6.3 What priority rule 1 fixes

Prefer actions that unblock a `Release`. Then `Transfer` runs first; `Release(A1)`
becomes ready and top-priority, opening the gap at 1000; `Claim(A3, 200)` lands at
1000; `Write(A3)` follows. A3 reuses A1's space, with no address-level reasoning
whatsoever.

This is why the priority-queue formulation beats a fixed phase order. A fixed order
cannot express it: if `A2` were fresh rather than persistent the correct sequence is
`Claim(A2) → Transfer → Release(A1) → Claim(A3)`, interleaving claims and releases.

### 6.4 The aggressive alternative, and why not to build it

The other option is to free A1 first, let A3 be placed over its address range —
`heap.free` moves no bytes, so A1's content is still physically there — and only
then ensure the read happens before those particular bytes are overwritten. That is
sound, and it needs:

- **lowering the graph from ids to addresses**, since `address_of(A1)` stops
  working the moment `Release(A1)` runs;
- **dynamic edge insertion**: when `Claim(A3)` picks 1000, discover that
  `Write(A3, 0, 64)` targets 1000..1064, which overlaps the pending transfer's
  source, and add `Transfer → Write(A3)` at execution time;
- **overlapping-move care**, since the transfer's source and some other action's
  destination can now be the same bytes.

It does not deadlock — a dynamic edge always points at an action that was only just
enabled, so nothing depends on it yet — but in this scenario it produces *exactly
the same* reads, writes and placement as §6.3. It wins only when the free must
precede the read for placement reasons the priority cannot reach, which is exotic.

### 6.5 Hoisting, which makes the question moot

At fold time, if a `Storage(src, …)` piece references a range that this log will
**disturb**, read those bytes immediately and turn the piece into a `Literal`.
Disturbed means any of:

- `src` is released this log,
- the range is written this log,
- `src` is reshaped this log (it may relocate).

All three are known during the fold, which already runs at flush with full read
access to storage.

Applied to §6.1: A1 is released, so A2's middle piece becomes a `Literal` during the
fold. The `Transfer` disappears, `Release(A1)` loses its only predecessor, and the
schedule is `Release(A1)` → `Claim(A3)` at 1000 → `Write(A2, 32, lit)` →
`Write(A3, 0, …)`. Same result as §6.3 and §6.4, and the only cost is 64 bytes held
in the arena between fold and execution — bytes that were going to be read anyway.

**Hoisting every disturbed read collapses the DAG.** Every surviving `Transfer` then
reads a range that is not released, not written and not reshaped, and its source
stays live in the heap so no `Claim` can be placed over it. The only remaining edge
is `Claim(dst) → Transfer`, which the fixed phase order
`releases → claims → reshapes → transfers and writes` already satisfies.

So §5 describes the general mechanism, but the recommendation is to **build
hoisting first and skip the graph entirely**. It is dramatically simpler and it is
correct; its only cost is memory proportional to the volume of disturbed reads,
which for kladde's containers is small (`make_*` moves one allocation's content,
once). Build the scheduler when a workload shows the buffering hurting — the
differential oracle of §8 makes that transition safe, and the fold is the same
function either way.

## 7. The content-blind fast path

Set a flag when any op produces a piece `Storage(other_id, …)` with
`other_id != self`. That is `Copy` and `make_*` on a persistent id, and nothing
else — in particular **not** plain `resize` (which is why `Reshape` must stay one
vertex, §5) and **not** `splice` (source and destination are the same id, so it
creates no cross-id constraint; it needs correct memmove direction internally, but
that is local).

When the flag is clear at flush — the overwhelming majority of transactions — run
`releases → FFD claims → reshapes → writes` with no graph, no hoisting and no
per-piece analysis. Today's cost is preserved for the common case and the general
machinery only bills the transactions that need it.

### 7.1 Representing a piece table cheaply

Most touched allocations have exactly one segment. Allocating a `BTreeMap` each is
wasteful, so:

```rust
enum Content {
    Uniform(Source),   // one segment over [0, size) — no allocation at all
    Spilled,           // entries live in the flush-wide map
}
```

`Uniform` sits inline in the `Folded` entry and covers freshly-allocated-and-fully-
written (one `Literal`), untouched-persistent (one identity `Storage`), and
grown-but-unwritten (`Undefined`). On the second segment, spill into **one**
flush-wide `BTreeMap<(Id, Offset), Source>`, queried per id with
`range((id, 0)..=(id, Offset::MAX))`. Zero tree allocations in the common case,
exactly one in total otherwise.

This lives in `Folded`, not in `pending` — `pending` stays geometry-only and
write-phase-only (§2).

## 8. Correctness: the differential oracle

This is a small optimizing compiler, so build the oracle before the optimizer.

Keep the naive in-order replayer as a reference implementation. Differential-test
the optimized flush against it: same log, then compare the entire observable state —
`heap.iter()` plus every live allocation's bytes, with `Undefined` ranges masked out
(§2.1 makes them unconstrained, so comparing them would reject legal schedules).
Randomized logs with shrinking.

Two properties worth asserting separately, because they fail differently:

- **fold agreement** — `Folded`'s geometry equals `pending`. Cheap enough to leave
  on outside tests; it is the check all three §1 bugs would have tripped.
- **schedule equivalence** — the differential comparison above.

## 9. Open: checkpoint versus commit

Undecided, and deliberately so. The distinction:

- A **checkpoint** applies buffered work to storage so the in-memory journal can be
  released. It is triggered by resource pressure and lands wherever that pressure
  happens to fall.
- A **commit** is a boundary the recovered state is allowed to snap to. It is
  triggered by the application's notion of a completed change.

Today they are the same event, which is what gives atomicity for free. The intended
model breaks them apart: application authors do not call `flush`, they mutate state;
when the journal grows too long the guard method that would overflow it checkpoints
automatically. A flush triggered by buffer size cannot also be a durability
boundary, or durability would be determined by how much memory the journal happens
to use.

Three consequences follow immediately, and one decision does not.

**`flush` takes `&self`.** The auto-checkpoint fires from inside a guard method,
which holds `&B`; there is no `&mut B` in reach. This is safe: callers hold ids and
`Location`s, never addresses, and writes resolve addresses at write time, so
relocation and compaction under a live guard are transparent. Note the checkpoint is
a *reentrant* call into the backend, so no `WriteBackend` method may hold the
interior `RefCell` borrow across a callback.

**The check belongs at guard-method boundaries, not at append time.** A check on
append can fire between two writes of one logical mutation — a `set` that resizes
and then writes. A guard method is the natural atomic unit, being the smallest thing
an application author perceives as one change.

**`flush` splits into two knobs**: fold-and-apply (bounds memory, safe whenever the
fold is consistent) and truncate-the-log (safe only up to the last commit). They are
currently one operation.

The undecided part is how uncommitted data is kept out of the recovered state:

**(a) Write-ahead the log.** Persist records before applying them; a commit record
marks the boundary; recovery replays to the last commit and undoes past it. Bounds
memory inside arbitrarily long transactions. Needs undo information or a no-steal
policy, plus a real on-disk log format.

**(b) Checkpoint only committed prefixes.** Storage never holds uncommitted data, so
recovery is redo-only and no undo exists. Far simpler. But memory cannot be bounded
*inside* a transaction — a long one still pins its whole write set.

Since transactions are meant to be the explicit exception, (b) is the cheaper
starting point: outside a transaction every auto-checkpoint already sits on a commit
boundary, so (b) costs nothing there, and inside one the unbounded journal is an
honest documented limit. (a) can be added later for long transactions without
disturbing the common path. This note does not decide it.

Note also that (a) is what would make incremental construction of the piece tables
worthwhile: an incrementally maintained table can **prune the arena**, dropping
literals that have since been overwritten, which directly reduces how often the
auto-checkpoint fires. That is only safe once a durable copy of the log exists. Until
then, build the fold at flush (§4): it is a pure function of the log, which makes it
trivially checkable, and nothing pulls it earlier because `size`/`resolve` need
geometry only.

## 10. Implementation order

1. **Log every mutation** (§2). This alone fixes the recovery hole and makes bugs
   1–3 expressible; it is the only step that is not optional.
2. **`pending` as the §3 delta**, replacing the current exhaustive map. Fixes the
   post-checkpoint `size`/`resolve`/`free`/`resize` holes and the counter leak.
3. **The differential oracle** (§8), against the naive in-order replayer. Before any
   optimization, so every later step is a validated rewrite.
4. **The fold** (§4) with the `Uniform`/spill representation (§7.1), and the
   fold-agreement assertion. Bugs 1–3 all become consequences of the per-piece
   bounds rather than three separate fixups.
5. **Read hoisting** (§6.5) and the content-blind fast path (§7). At this point
   `splice` and `make_*` become implementable, and `Copy` becomes addable.
6. **The scheduler** (§5) — only if buffering measured in step 5 proves too
   expensive.

Steps 1–3 are prerequisites for anything else. Steps 4–5 are where the I/O
reductions live. Step 6 may never be needed.

## References

- [incremental-compaction.md](incremental-compaction.md) — the heap model this sits
  on: stable ids, the `id → address` table, the bounded compaction step.
- [augmented-segment-tree.md](augmented-segment-tree.md) — the placement and
  compaction policy the scheduler's `Claim` calls into.
- [allocator-spec.md](allocator-spec.md) / [generic-allocator.md](generic-allocator.md)
  — the pointer and backend trait surface.
