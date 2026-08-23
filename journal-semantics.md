# Journal semantics, and the flush optimizer

Status: **§§1-8 implemented**, in `journal.rs`, `fold.rs`, `schedule.rs` and
`journaled.rs`. §9 (durability) is not: the journal lives in memory, `open` is still
a `todo!()`, and nothing survives a crash. §10 tracks what landed.

The three bugs in §1 were real and are fixed; each has a regression test naming
it. Two things below were adjusted against what the implementation found, and are
marked where they occur: the oracle compares the *observable* surface rather than
`heap.iter()`, and the scheduler needs hoisting to be switchable off before its
edges do any work.

The subject is the *deferred* write path: what a transaction records, what a flush
is allowed to do with the recording, and what "the same thing happened" means when
the flush is permitted to reorder and elide work. §9 covers what survives a crash.
It does not cover placement policy (see
[incremental-compaction.md](incremental-compaction.md)) or the compaction step
itself (see [augmented-segment-tree.md](augmented-segment-tree.md)).

## 1. What is broken today, and why it is one bug

`JournaledWriteBackend` keeps **two** records of a transaction:

- `pending: HashMap<Pointer, Size>` — a *state snapshot*. Unordered,
  last-writer-wins, self-annihilating. `alloc`/`free`/`resize`/`make_*` fold into it.
- `journal: Vec<(Pointer, Size, Vec<u8>)>` — an ordered *op sequence*:
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
   only the `mint`. The copy is not deferred or approximated — it is absent. §4.3
   fixes this by removing the copy rather than deferring it: the immediate path
   relocates on *every* conversion, which is its own problem.

The more fundamental statement of the same defect is not any of these. It is that
**the journal does not record every mutation**, so a crash mid-transaction is
unrecoverable in a way that has nothing to do with annihilation: a `Persistable`
that allocates a child and writes the child's id into its own header emits an
`Alloc` that the journal does not contain and a `Write` that it does. Recovery then
reads a live pointer out of the parent that names an allocation nothing ever
created. Two operations, no `free` in sight, and the result is a dangling pointer
in recovered application data.

## 2. The model

> **The journal is the sole authority.** It is a totally ordered sequence of every
> mutation. Everything else is derived from it and may be discarded and rebuilt.

The journal carries six operation kinds, which is exactly the closure under "an entry
whose absence would make some other entry unreplayable or misinterpretable":

| op | why it must be journalled |
| --- | --- |
| `Alloc(id, size)` | otherwise a `Write` has no target and a serialized id dangles. Sizedness rides on the id, so it needs no separate field. |
| `Free(id)` | annihilation, and the id's release |
| `Resize(id, size)` | bounds for replayed writes; content destruction |
| `ChangeSizedness(old → new)` | id identity when sizedness changes (§4.3) |
| `Write(id, offset, bytes)` | content |
| `Splice(id, offset, old_len, bytes)` | content, with a tail shift |

`make_resizable`/`make_fixed_size` are **not** journal ops. They decompose into a
`ChangeSizedness` and a `Resize` (§4.3), which is what lets the common case move no
bytes at all.

A future `Copy(src, src_off, len, dst, dst_off)` joins the content group; §6 is
written with it in mind because it is the only operation that makes the scheduling
question interesting.

Two derived structures, and it matters that they are derived:

- **`pending`** (§3) — write-phase geometry, so `size`/`resolve` are O(1).
  Discarded at checkpoint.
- **`Folded`** (§4) — geometry *and* content, built at flush from the journal alone,
  consumed by the scheduler, then dropped.

`Folded` recomputes the geometry `pending` already had. That redundancy is the
point: comparing them on every flush is the consistency check that all three bugs
in §1 would have tripped, and it is asymptotically free (the fold is already
O(journal length); the comparison is O(touched ids)).

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
optimization: the journal and both derived structures are keyed by id, so if a counter
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
| `Relabelled { from, S }` | yes | under `from` | `heap.relabel(from, id)`, then `heap.resize(id, S)` if `S` changed, then recycle `from`'s counter |
| `Freed` | yes | maybe | `heap.free(id)` **if live**, then recycle the counter |

These are distinct because they call different heap methods, not because they carry
different data. `relabel` is a new `RelocatableHeap` method: it rekeys one
allocation in place — same address, same size — which for `GainGreedyHeap` means
re-inserting its evacuation-index key with the other `is_fixed` bit. It copies no
bytes.

`Relabelled` owns *both* halves of a sizedness change, which is why the old id ends
up with no entry at all rather than a `Freed` one: making the relabel responsible
for recycling `from`'s counter removes an ordering coupling between two entries that
would otherwise have to be processed in the right sequence.

### 3.1 Transitions

| op | from | to | note |
| --- | --- | --- | --- |
| `alloc` | `Absent/dead` | `New(S)` | the only source state, by §2.2 |
| `write` | any live state | **unchanged** | content only; `pending` is geometry |
| `resize` | `New(S)` | `New(S′)` | |
| | `Absent/live` | `Resized(S′)` | |
| | `Resized(S)` | `Resized(S′)` | |
| | `Relabelled { from, S }` | `Relabelled { from, S′ }` | composes, so `ChangeSizedness; Resize` needs no extra state |
| | `Freed`, `Absent/dead` | — | `Err(DanglingPointer)` |
| `ChangeSizedness` | old: `New(S)`, and&nbsp;new: `Absent/dead` | old: `Absent/dead`, and&nbsp;new: `New(S)` | never claimed, so no relabel — just move the entry |
| | old: `Absent/live` / `Resized(S)` / `Relabelled{..,S}`, and&nbsp;new: `Absent/dead` | old: `Absent/dead`, and&nbsp;new: `Relabelled { from: old, S }` | |
| `splice` | as `resize` | as `resize` | size change plus content ops |
| `free` | `Absent/live` | `Freed` | |
| | `Resized(S)` | `Freed` | |
| | `New(S)` | `Freed` | **not `Absent`** — see below |
| | `Relabelled { from, S }` | `Freed` | degenerates to releasing `from`; both counters recycle |
| checkpoint | `New` / `Resized` / `Relabelled` | `Absent/live` | map cleared |
| | `Freed` | `Absent/dead` | counters recycled here, and only here |

`ChangeSizedness` is the **only** op that mutates two entries in one call, which is
why its rows name the old and the new id explicitly: the two columns of one row fire
*together*, they are not alternatives.

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

Per id, linear, no graph. For each id the journal touches, walk its op subsequence
maintaining whether it exists, its current size, and a **piece table** over its
bytes.

```rust
enum Source {
    Literal(JournalOffset), // bytes carried by a record in the journal
    Storage(Id, Offset),    // must be read from the file
    Undefined,              // uninitialized; may be anything (§2.1)
}
```

A piece table is a sorted map from **segment start offset** to `Source` — one entry
per segment boundary, never per byte. A byte's origin is found with
`range(..=offset).next_back()`, giving `(seg_start, source)`; the origin is that
source advanced by `offset - seg_start`. `Source` is offset-relative precisely so
that advance is meaningful — which is also what lets a `Literal` survive being
partially overwritten, the survivor simply pointing into the middle of the original
payload.

`Literal` names a position in **the journal**, whatever the journal currently is: an
in-memory byte buffer today, a framed on-disk record later. Nothing below depends on
which, and §4.1 is written so the representation does not have to change when that
does.

The distinction that drives everything:

- **Fresh** ids (an `Alloc` appears in this journal) start `{0 → Undefined}`. Their
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
| `ChangeSizedness(old → new)` | move the table to `new`, rewriting `Storage(old, k)` → `Storage(new, k)` (§4.3) |
| `Free` | mark released |

Overwrite is the only non-trivial primitive: split at both boundaries (materializing
them from the covering segment), drop the entries strictly inside, insert the new
one.

The two questions this answers directly: `Alloc(s1); Resize(s2)` folds to a single
`Alloc(s2)` with the clip deciding what happens to writes in between, and
`Resize(s1); Resize(s2)` folds to a single `Resize(s2)` — where, if `s1 < s2`, the
bytes in `[s1, s2)` become `Undefined` and may be left on disk exactly as they are.
That last part is legal only because of §2.1.

### 4.1.1 Two kinds of coalescing, and only one of them saves I/O

It is tempting to merge adjacent segments in the table and call that the
optimization. It is not, and conflating the two costs a lot of confusion:

- **Source contiguity** — can two segments be represented as one `Source`? Two
  `Literal`s can only merge if their journal positions are adjacent, which they are
  *not* in general: consecutive `Write` records are separated by the next record's
  header. Two `Storage(id, base)` merge iff same `id` and `left.base + left.len ==
  right.base`; two `Undefined` always merge.
- **Destination contiguity** — can two segments be written with one call? This holds
  *by construction*, since adjacent segments in a piece table are adjacent in the
  destination allocation.

Only the second saves I/O, and it does not care where the bytes came from. So the
**emitter walks the table and issues one write per maximal run of resolvable
segments**, gathering from wherever the sources happen to live. Table coalescing
demotes to a memory optimization: do it when the merged form has a compact
representation (which is exactly the three cases above), skip it otherwise, and lose
nothing.

This is what keeps the common case at one or two entries, and it is why nothing here
needs writes to arrive in offset order.

**Bound the gather run.** Assembling one destination write from *n* scattered
sources means either copying them into a staging buffer or handing the kernel a
vectored write. Either way the run should be capped — a page or two of staging
buffer is plenty — and emission split at that boundary, so a single enormous
coalesced extent cannot turn into an unbounded allocation. The cap is a pure
throughput knob: splitting a run only costs an extra write.

§7 adds the one case where the emitter may merge across a segment it was *not*
obliged to write.

### 4.2 What falls out unasked

- **`splice` on a fresh allocation costs zero I/O.** It is a table edit; the shift
  never touches bytes. Since `PersistableString`/`PersistableVec` splice in loops,
  this is probably the largest single win available.
- **A transient allocation — fresh and freed in the same journal — never touches
  storage.** Recycle its counter and emit nothing. A `Copy` *out* of it resolves to
  literals, so the read does not force it to be materialized.
- **An identity piece emits nothing.** `Storage(self, k)` with `k == seg_start`
  means the bytes are already where they belong. Without this check every untouched
  region of every persistent allocation would emit a self-copy.
- **A sizedness conversion moves nothing** in the common case — §4.3, which is the
  whole reason `make_*` decomposes.

### 4.3 Sizedness conversion moves nothing

`make_resizable`/`make_fixed_size` necessarily mint a new id, because sizedness
rides on the id's low bit and there is nothing to re-tag in place. Today that is
implemented as `alloc(new) → copy → free(old)`, so it relocates the allocation
**every time, even when the size does not change**. Deferring that copy does not
help: a `Claim(new)` issued while the old id is still live cannot be placed at the
old address, so the bytes move regardless.

The fix is to decompose, and the ordering is forced by `Resize` being defined only
on resizable allocations:

```
make_resizable(p, s)   =  ChangeSizedness(p → p′)  ;  Resize(p′, s)
make_fixed_size(p, s)  =  Resize(p, s)             ;  ChangeSizedness(p → p′)
```

`ChangeSizedness` is a pure relabel: same address, same size, new id, no bytes
touched. What makes it *emit* nothing is a rule already stated above — relabelling
**preserves the identity piece**. `{0 → Storage(old, 0)}` becomes
`{0 → Storage(new, 0)}`, still identity under the new id, so §4.2 drops it. The
`alloc`/`copy`/`free` form destroys that property, since the new id sits at a new
address and the piece is no longer identity.

The `Resize` half may still relocate, so this is not a guarantee — §2.1 never
promised one. What it removes is the *unconditional* move.

Why it matters: consider a chunked vector whose length oscillates across a chunk
boundary, so the last chunk converts between resizable and fixed on every
`push`/`pop` pair. Under the old form each transition copies the entire chunk. Under
the decomposition, `push` resizes the chunk up (no move if there is room after it,
which is what the lift machinery in `augmented-segment-tree.md` tends to arrange
after the first evacuation) and then relabels; `pop` relabels and then shrinks, and
shrinking never relocates. The steady state costs two index operations per
transition instead of a chunk copy.

The real cost is placement *policy*, not correctness: an allocation that became
fixed-size by relabel sits at an address the resizable policy chose, and vice versa,
so the dense size classes dilute over time. That is a question for `size_classes.rs`
and is worth measuring on exactly the oscillating workload above.

## 5. Phase B — the schedule

Vertices are the **emitted actions**, not the journal's ops — the fold changed the op
set:

```
Claim(id, size)          Release(id)          Relabel(from → to)
Reshape(id, size)        Transfer(src, src_off → dst, dst_off, len)
Write(id, off, bytes)
```

`Reshape` stays **one vertex**. It is tempting to decompose it into
`Claim`/`Transfer`/`Release`, but that is the wrong granularity: the heap decides
the new placement and moves the bytes as one atomic operation, and splitting it
would put the old range's release into the schedule as a separate vertex, where a
frees-first pass could hand that range to another `Claim` before the copy ran.

`Relabel` is the cheapest vertex there is — an index rekey, no I/O (§4.3). It has
one edge, below, and never appears in a cycle.

### 5.1 Edges

1. `Claim(id)` → every action writing into `id`. `Relabel(from → id)` likewise.
2. `Transfer` reading `id` → `Release(id)`. *(the `Copy`-then-`free` edge)*
3. `Transfer` reading a range of `id`'s storage → later actions writing that range
   (a WAR anti-dependency — easy to forget, since it runs backwards from the usual
   intuition).
4. Earlier writes to a range → a `Transfer` reading it. The fold has usually already
   turned these into literals, so this edge mostly evaporates.

**It is a DAG by construction**: every hard edge points from a lower journal position to
a higher one.

### 5.2 Frees-first is a preference, not an edge

Running `Release`s before `Claim`s gives the placement pass more space to work with.
It must be encoded as a *priority*, never as an edge. A `Copy` into a fresh
allocation out of one that is then freed produces
`Claim(dst) → Transfer → Release(src)` — the §6.1 scenario; add `Release → Claim` as
a hard edge and that is a three-cycle.

So: **Kahn's algorithm with a priority queue over the ready set**, in order

1. anything that unblocks a `Release` (see §6 for why this matters more than it looks),
2. `Release`,
3. `Claim`, largest size first (FFD),
4. `Reshape`, `Transfer`, `Write`.

FFD is therefore *best-effort over the ready set* rather than global — a `Claim`
blocked behind a `Transfer` is placed after smaller ones. Only `Copy` chains block,
but the guarantee is weaker than the current unconditional sort and that should be
stated where it is relied on.

Compaction stays outside the graph as a final phase: it moves only live ranges, and
by then every `Storage` read has been resolved.

## 6. Read hoisting, and a worked example

### 6.1 The scenario

After an earlier checkpoint the heap holds `A1` at address 1000, size 256, and `A2`
at address 2000, size 128 — both persistent. This transaction records:

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

At fold time, if a `Storage(src, …)` piece references a range that this journal will
**disturb**, read those bytes immediately and turn the piece into a `Literal`.
Disturbed means any of:

- `src` is released this journal,
- the range is written this journal,
- `src` is reshaped this journal (it may relocate).

All three are known during the fold, which already runs at flush with full read
access to storage.

A sizedness change needs no bullet of its own: the `ChangeSizedness` half relocates
nothing, and the `Resize` half is covered by "reshaped" (§4.3). It is only
conservative in one direction — a reshape that turns out not to relocate will have
hoisted a read it did not need, which costs memory and no correctness.

Applied to §6.1: A1 is released, so A2's middle piece becomes a `Literal` during the
fold. The `Transfer` disappears, `Release(A1)` loses its only predecessor, and the
schedule is `Release(A1)` → `Claim(A3)` at 1000 → `Write(A2, 32, lit)` →
`Write(A3, 0, …)`. Same result as §6.3 and §6.4, and the only cost is 64 bytes held
in the journal between fold and execution — bytes that were going to be read anyway.

**Hoisting every disturbed read collapses the DAG.** Every surviving `Transfer` then
reads a range that is not released, not written and not reshaped, and its source
stays live in the heap so no `Claim` can be placed over it. The only remaining edge
is `Claim(dst) → Transfer`, which the fixed phase order
`releases → claims → reshapes → transfers and writes` already satisfies.

So §5 describes the general mechanism, but the recommendation is to **build
hoisting first and skip the graph entirely**. It is dramatically simpler and it is
correct; its only cost is memory proportional to the volume of disturbed reads,
which for kladde's containers is small — with `make_*` decomposed (§4.3), the only
op that produces one at all is `Copy`. Build the scheduler when a workload shows the
buffering hurting — the differential oracle of §8 makes that transition safe, and
the fold is the same function either way.

## 7. The content-blind fast path

Set a flag when any op produces a piece `Storage(other_id, …)` with
`other_id != self`. That is `Copy`, and nothing else — in particular **not** plain
`resize` (which is why `Reshape` must stay one vertex, §5), **not** `splice` (source
and destination are the same id, so it creates no cross-id constraint; it needs
correct memmove direction internally, but that is local), and **not**
`ChangeSizedness`, whose rewrite is a relabel rather than a read (§4.3).

When the flag is clear at flush — the overwhelming majority of transactions — run
`releases → relabels → FFD claims → reshapes → writes` with no graph, no hoisting
and no *cross-id* piece analysis. The fold still runs: both paths emit from
`Folded`, never from the raw journal, so the per-byte deduplication below is not
something the fast path gives up.

### 7.1 What the fold already deduplicates

The piece table holds exactly one `Source` per byte, by construction — `overwrite`
splits both boundaries and drops everything strictly inside. So a journal containing a
thousand writes to the same eight bytes emits **one** eight-byte write, on either
path. No byte is written twice in a flush.

What the fast path does not do by itself is merge across a segment it was not
obliged to write at all. Two literals separated by a short `Undefined` or identity
`Storage` gap emit two writes and a seek, where one write would do. §2.1 licenses
closing that gap: **an `Undefined` region may legally receive arbitrary bytes**, so
the emitter is free to write straight through one. An identity gap can be closed the
same way at the cost of rewriting bytes that were already correct.

A threshold of roughly one page is the obvious rule, and it turns a scattered set of
small updates into a single sequential write — which is the main lever the piece
table offers for making replay I/O sequential rather than random. It composes with
the gather-run cap of §4.1.1: merge across gaps first, then split the result at the
cap.

### 7.2 Representing a piece table cheaply

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
the optimized flush against it: same journal, then compare the observable state, with
`Undefined` ranges masked out (§2.1 makes them unconstrained, so comparing them
would reject legal schedules). Randomized journals with shrinking.

**Adjusted in implementation.** This section originally said to compare
`heap.iter()`. That is too strong: addresses are not observable through the
backend API, and two correct implementations legitimately place things
differently — naive replay claims in journal order, the scheduler claims
first-fit-decreasing. Comparing addresses would reject the optimization the
oracle exists to validate. The compared surface is therefore which allocations
are live, how big each is, and what bytes it holds.

A third implementation is worth more than two: a pure in-memory model catches
bugs both backends share, and it sidesteps id divergence, since the journaled
backend's deferred frees recycle counters at different moments than the
unjournaled one's. Actions therefore name model-level handles rather than ids.

Two properties worth asserting separately, because they fail differently:

- **fold agreement** — `Folded`'s geometry equals `pending`. Cheap enough to leave
  on outside tests; it is the check all three §1 bugs would have tripped.
- **schedule equivalence** — the differential comparison above.

## 9. Durability: checkpoint, commit, and restartable flush

### 9.1 Terminology

First, two names for one byte stream. The **journal** is the ordered sequence of
ops — a *logical* log, recording operations rather than page images. The
**write-ahead log** is that same sequence made durable *ahead of* the data writes it
describes. So they are not two structures: "journal" names what it contains,
"write-ahead log" names when it is written relative to the data region. This note
says "journal" throughout except where the write-ahead property is the point.

Four more words that have to be kept apart:

- A **prefix** of the journal is op₁…opₖ — the journal truncated at a point.
- A **commit boundary** is a position marking a completed logical change: a
  guard-method boundary in the auto-checkpoint model, or an explicit `commit()`
  inside a transaction. A **committed prefix** is one ending exactly on such a
  boundary, so it contains only whole logical changes.
- A **checkpoint** takes some prefix, folds it, applies the resulting writes to the
  data region, and releases that prefix from memory.
- A **commit** is a boundary the recovered state is allowed to snap to.

Elsewhere this note says "at flush" for what §3 calls "at checkpoint". They are the
same event; *flush* is the operation and *checkpoint* is the role it plays when it
runs for memory relief rather than for durability.

The intended model separates checkpoint from commit. Application authors do not call
`flush`; they mutate state, and when the journal grows too long the guard method that
would overflow it checkpoints automatically. A flush triggered by buffer size cannot
also be a durability boundary, or durability would be set by how much memory the
journal happens to use.

Three consequences follow immediately:

**`flush` takes `&self`.** The auto-checkpoint fires from inside a guard method,
which holds `&B`; there is no `&mut B` in reach. This is safe: callers hold ids and
`Location`s, never addresses, and writes resolve addresses at write time, so
relocation and compaction under a live guard are transparent. The checkpoint is a
*reentrant* call into the backend — it is entered while the call stack already
contains backend code — so no `WriteBackend` method may hold the interior `RefCell`
borrow across anything that could trigger one, or the nested `borrow_mut` panics.

**The check belongs at guard-method boundaries, not at append time.** A check on
append can fire between two ops of one logical mutation — a `set` that resizes and
then writes. A guard method is the natural atomic unit, being the smallest thing an
application author perceives as one change.

**`flush` splits into two knobs**: fold-and-apply (bounds memory, safe whenever the
fold is consistent) and truncate-the-journal (safe only up to the last commit). They are
currently one operation.

### 9.2 Two failure modes, two unrelated fixes

Both leave torn state on disk, which is why they are easy to conflate:

| | what recovery sees | fixed by |
| --- | --- | --- |
| **A** | ops applied that the application never committed | choosing *what* to apply |
| **B** | a crash midway through applying | making apply *restartable* |

**(a) A write-ahead log** — make the journal durable *before* applying it, mark
commits with a commit record, and recover by replaying the write-ahead log.
Fixes **B**.

**(b) Checkpoint only committed prefixes** — stop at the most recent commit boundary
rather than partway into an in-flight change, so storage never holds uncommitted
data. Fixes **A**.

These are **not alternatives**: (a) is a mechanism for surviving a crash during
apply, (b) is a policy about what to apply. Reading them as a choice is a mistake —
(b) alone does nothing for a crash mid-apply, because a committed prefix folds to
many writes and dying after some of them still leaves a half-applied change.

The real menu:

| | crash-tolerant? | needs undo? | memory bounded inside a transaction? |
| --- | --- | --- | --- |
| (b) alone | no | no | no |
| (a) alone | yes | **yes** | yes |
| **(a) + (b)** | **yes** | **no** | no |

**Decision: (a) + (b).** Redo-only recovery with no undo machinery, at the cost of a
long transaction pinning its own write set — an honest, documented limit, and the
one that can be lifted later by adding undo without disturbing anything else.

### 9.3 What "durable" means

Three layers, and which one is needed is a decision about the failure model, not a
detail:

1. **`write()` returned** — bytes are in the OS page cache. Survives the *process*
   dying.
2. **`fsync`/`fdatasync` returned** — bytes are on the device. Survives power loss
   and kernel panic.
3. **Device write cache flushed** (FUA/barriers), plus an `fsync` on the *directory*
   for a newly created file.

The stated requirement — recover after the application crashes anywhere inside
`flush` — is process death, so **layer 1 suffices**. The page cache outlives the
process, and one process's `write()` calls land in program order, so writing the
journal ahead of the data writes gives the ordering for free. No `fsync` anywhere.

Surviving power loss means layer 2, which costs roughly one `fsync` per flush. That
is the expensive part and should be chosen deliberately rather than inherited.

Even at layer 1, `write_all` loops over short writes, so a process dying mid-loop
leaves a partially written record. Framing (§9.5) is required at every layer.

### 9.4 What has to be in the write-ahead log

The organizing question is **what is derivable**:

| | derivable? | must be in the write-ahead log |
| --- | --- | --- |
| the ops themselves | no | **yes** — and they already are; the journal is written as ops are recorded |
| hoisted bytes (§6.5) | no, *after partial application* | **yes** — and this is new work at flush time |
| the fold, and the schedule's shape | yes, from the journal | no |
| ids of claimed regions | — | **yes**, but for free: `Alloc` and `ChangeSizedness` already carry them |
| the address each `Claim` receives | yes, **if** placement is deterministic and the pre-flush heap state is recoverable | no |

Two things this makes precise.

**Only the hoisted bytes are new at flush time.** The ops are in the file already —
that is what the journal is for. Hoisting, by contrast, happens *during* the fold, so
flush begins with a genuine append: the hoist blob, plus a marker naming the journal
range this flush covers. That append is what must be ordered before the data writes.

**Id minting stays unspecified; placement does not.** Because `Alloc(id, size)` and
`ChangeSizedness(old → new)` record their ids, recovery never re-derives them, and
the id allocator is free to be an implementation detail — including its `free_counters`
LIFO, whose order affects only *future* ids, which future ops will record in turn.
After recovery it is enough to rebuild a non-colliding allocator (e.g.
`next_counter = max(live id) + 1` with an empty free list), at the cost of some
counter density.

Placement is the opposite: the schedule is **deterministic and specified**, so
recovery re-derives each `Claim`'s address rather than reading it from the
write-ahead log. That saves writing a plan proportional to touched ids on every
flush, and costs two things:

- **Determinism becomes load-bearing.** No `HashMap` iteration order, no time, no
  addresses-as-input, no floats — anywhere in the fold, the placement policy, or the
  compaction policy. Some of that discipline exists already (today's flush sorts
  pending FFD partly "so the layout is reproducible from one run to the next"), but
  it must become a *tested* property: re-derive the schedule twice from the same
  inputs and assert byte-equality. A violation is silent until a crash.
- **It couples the file format to the placement algorithm.** Any other
  implementation of kladde must reproduce `GainGreedyHeap`'s exact decisions to
  recover a torn flush. That is a heavy constraint given the cross-language goal in
  `later.md`'s design constraints, and it is the argument for recording the plan
  instead. Worth revisiting when the format is specified.

Either way, recovery needs the heap's `id → (address, size)` table as of the last
checkpoint, to re-derive placement against. That is a durable snapshot — and it is
the same thing `JournaledWriteBackend::open` needs anyway (the self-hosting bootstrap
currently sitting at `todo!()`), so it is not extra work.

One nicety of a re-derivable schedule: if the schedule is re-derivable then so is
the fold, so the number and order of hoisted reads is known to recovery, which can
consume the hoist blob **positionally**. No per-item delimiters — one length and one
checksum over the whole blob.

### 9.5 Framing, and the unit of durability

Ops are grouped into **frames**. A frame is self-delimiting — a header carrying
`byte_len` and a checksum, then that many bytes of ops — so the next frame begins
immediately after, at no particular alignment. Frames are **variable-length and
never padded**: with the length in the header, a reader walks the chain from the
start and always knows where the next one begins.

(SQLite pads because its frames each carry exactly one fixed-size database page.
That constraint comes from its payload domain, not from checksumming, and does not
apply here.)

**The write granularity decides the checksum granularity, not the other way
round.** A frame's checksum cannot be computed until the frame is complete, so a
frame is exactly what one `write()` emits. That makes the real question "how often
do we call `write()`", which is a durability question, not a framing one:

| write granularity | loss window on a process crash |
| --- | --- |
| one frame per op | the op in flight |
| one frame per group of ops | everything buffered since the last frame |

Tying frames to a fixed byte budget — fill 4 KiB, then write — is the tempting
answer and the wrong one. It makes durability depend on how fast the user happens
to be typing, so a pause leaves recent edits in memory indefinitely; fixing that
with a timer puts "how much work may be lost" inside a library that cannot know the
answer. That belongs to the application.

So: **one frame per op by default.** An op that reaches the journal with a valid
checksum is persisted. At roughly 1–2 µs per `write()` on a page-cached file, that
costs 0.01–0.2 % of a core at interactive rates (10–1000 ops/s) and becomes
significant only in bulk work — which is what §9.5.1 gives an explicit escape hatch
for.

**Checksums detect a torn tail.** Use **CRC-32C** (Castagnoli): 4 bytes, hardware
accelerated on x86-64 (SSE4.2 `crc32`) and AArch64 (the CRC32 extension), and with
*provable* burst-error detection up to 32 bits, which XXH3 and friends do not offer.
Speed is not the deciding factor — correctness guarantees on short frames are.

**Chain the checksums**, SQLite-style: each frame's CRC is computed over its own
content *and* the previous frame's CRC. This subsumes the epoch counter. After a
checkpoint releases a journal prefix, that file region gets reused, and a stale
frame left there has a *perfectly valid* standalone CRC — a checksum alone cannot
tell it is from a previous life. A chained CRC can, because the stale frame's
running value will not match the current chain. It also catches reordering, and it
costs nothing extra in bytes: one salt in the journal header replaces a per-frame
epoch.

Per-op framing therefore costs a `u32` CRC plus a small length field — call it 5–6
bytes on a ~20-byte op, so 25–30 %. That is **journal** space, which is truncated
at every checkpoint, not permanent growth of the data region.

The hoist blob is one contiguous region written once per flush, so a single length
and CRC over the whole thing is negligible.

#### 9.5.1 Batches and transactions

Two grouping concepts, deliberately distinct, because durability grouping and
atomicity grouping are different things — the same split that lets a database offer
deferred durability without weakening atomicity.

- A **batch** is a group whose persistence *may* be deferred until the group ends.
  It is a hint, not a guarantee: the backend may interrupt a long batch, frame what
  it has, optionally flush, and open a new one. This is the tool for the bulk-import
  case, and its interruptibility is what keeps the journal bounded without the
  application having to think about it.
- A **transaction** is a batch with **atomic** persistence: all of its ops are
  applied or none are. It is the standard atomic commit unit.

**Nesting flattens by subsumption**: an inner scope is absorbed into an outer one
exactly when the outer guarantee is at least as strong as the inner. That single
rule gives all four cases —

| inner | outer | | |
| --- | --- | --- | --- |
| batch | batch | equal | flatten |
| batch | transaction | outer stronger | flatten |
| transaction | transaction | equal | flatten (**flat nesting**, as opposed to savepoints) |
| **transaction** | **batch** | outer *weaker* | **stays explicit** |

The last row is the whole point: a batch has no atomicity to lend, so absorbing a
transaction into one would silently discard a guarantee the application asked for.

One rule follows that is worth stating separately: **a batch may be interrupted only
at a transaction boundary.** The backend's freedom to split is real, but it cannot
cut through an atomic unit.

**Assessment.** The split is right, and the subsumption rule is better than
enumerating four cases — it is one comparison on an ordered enum, and it stays
correct if a third level is ever added.

Four things worth being explicit about:

- **The journal bound becomes "largest transaction", not "batch size".** An
  application that wraps its bulk import in one enormous *transaction* rather than a
  batch reintroduces exactly the unbounded-journal problem the batch was meant to
  solve. That is inherent to redo-only recovery (§9.2), not a flaw in the design,
  but it should be documented where application authors will see it, because the two
  spellings look interchangeable and are not.
- **A transaction should buffer in memory and reach the journal at commit.** This
  makes abort free — drop the buffer, nothing to undo, nothing to mark aborted — and
  it delivers the one-checksum-per-transaction saving. It also composes with the
  interruption rule: the backend cannot frame a partial transaction anyway.
- **A batch is observationally invisible.** Because it promises no atomicity,
  nothing can detect that one was split. That is what makes interruption safe, and
  it means a batch must be documented as purely a performance hint so nobody builds
  correctness on it.
- **Nested batches need a depth count, not a flag**, so dropping an inner scope does
  not end the outer one. Trivial, and trivially easy to get wrong.

This also unifies a loose end in §9.1: a guard method is an **implicit
transaction** — the smallest thing an application author perceives as one change,
and already the natural atomicity boundary. An explicit transaction simply widens
that boundary, and a batch widens the *durability* boundary without touching
atomicity.

Finally, transactions are not only an optimization. Until they exist, every
multi-op guard method must be hand-audited so that any prefix of its ops leaves a
valid state — the discipline in `later.md`'s write-ahead-logging section, whose cost
scales with the number of container types. Transactions **retire** that discipline.
That is an argument for their priority, not just their performance.

### 9.6 Which actions survive being replayed

Recovery re-derives the schedule and re-runs it, which is sound only if every action
survives partial application followed by repetition.

**Re-runnable.** `Write` (bytes come from the write-ahead log; the address
re-derives identically). `Claim`, `Release`, `Relabel` (heap operations re-derived
from the snapshot plus the write-ahead log). A `Transfer` whose source was hoisted —
because hoisting turned it into a `Write`. This is the strongest argument for §6.5:
hoisting is not merely a scheduling simplification, it is what makes replay
idempotent, and §6.5's disturb set is exactly the set of bytes a partial apply could
have clobbered.

**Not re-runnable: compaction slides.** A `Step` has `to < from`, and its source and
destination overlap whenever the gap is smaller than the run. Hoisting cannot help,
because it resolves the reads the *fold* can see, and the heap's own byte moves
appear in no piece table. Recorded as an open problem in
[later.md](later.md#regarding-write-ahead-logging-discipline), with three candidate
fixes.

**`Reshape` is safe — but only incidentally.** `GainGreedyHeap::resize` finds the new
home before releasing the old one (with a comment saying exactly why), so
`Relocation::Single` never overlaps; the lift's `Relocation::Double` does not either,
because its target sits at or above `addr + size` and its replacement is drawn from
above the vacated gap — which holds only because nothing live can sit *inside* a gap.
That argument is structural and implicit, and nothing tests it. **Add a debug
assertion that no `Relocation` a heap returns has overlapping ranges**, so a future
placement policy that lets an allocation grow *downward* into the gap below it fails
a test rather than corrupting files after a crash.

### 9.7 What (a) unlocks

Two things become available once the write-ahead log exists and is self-contained:

- **Deferred replay.** Nothing in memory is needed to finish applying, so replay can
  be scheduled to idle time or run asynchronously.
- **Journal pruning.** An incrementally maintained piece table can drop literals that
  have since been overwritten, shrinking the in-memory journal and directly reducing how
  often the auto-checkpoint fires. Only safe once a durable copy exists.

Until then, build the fold at flush (§4): it is a pure function of the journal, which
makes it trivially checkable, and nothing pulls it earlier because `size`/`resolve`
need geometry only.

## 10. Implementation order

Steps 1–7 are **done**; step 8 is not.

1. **Journal every mutation** (§2). This alone fixes the recovery hole and makes bugs
   1–3 expressible; it is the only step that is not optional.
2. **`pending` as the §3 delta**, replacing the current exhaustive map. Fixes the
   post-checkpoint `size`/`resolve`/`free`/`resize` holes and the counter leak.
3. **The differential oracle** (§8), against the naive in-order replayer. Before any
   optimization, so every later step is a validated rewrite. Add the **determinism
   assertion** here too — re-derive the schedule twice, assert byte-equality — even
   though nothing depends on it until step 8. It is cheap now and near-impossible to
   retrofit a diagnosis for later (§9.4).
4. **The fold** (§4) with the `Uniform`/spill representation (§7.2), and the
   fold-agreement assertion. Bugs 1–3 all become consequences of the per-piece
   bounds rather than three separate fixups. Emit by gathering runs (§4.1.1).
5. **`ChangeSizedness`** (§4.3), needing a `relabel` on `RelocatableHeap`. This is
   what makes `make_*` implementable *and* stops it relocating; it is independent of
   the two steps below and can land before them. Add the **`Relocation` overlap
   debug assertion** (§9.6) with it, since both touch the heap's relocation surface.
6. **Read hoisting** (§6.5) and the content-blind fast path (§7). `splice` becomes
   implementable and `Copy` becomes addable.
7. **The scheduler** (§5) — only if buffering measured in step 6 proves too
   expensive.
8. **Durability** (§9): self-delimiting frames with chained CRC-32C (§9.5), the
   durable heap snapshot that `open` needs anyway, ordering the write-ahead log's
   append before the data writes, and the commit-prefix policy. Restartable flush
   arrives here.

Steps 1–3 are prerequisites for anything else. Steps 4–6 are where the I/O
reductions live. Step 7 may never be needed.

Step 8 is deliberately last, and it is genuinely separable: the journal is already the
authority from step 1, so nothing above it changes shape when the journal becomes
durable. Two things make that separation hold rather than merely look plausible, and
both are cheap enough to do early — the determinism assertion in step 3, and
reserving the length/CRC frame-header fields in the record format from the start
even while writing zeros into them. Without those two, step 8 becomes a retrofit
rather than an addition.

Of those two, only the first landed. The determinism assertion is in place; the
record format is still an in-memory `Vec<Op>` plus a byte arena, with no framing
at all, so step 8 will have to introduce it. `Source::Literal` already names a
position in "the journal" rather than a private arena, so the fold does not change
when it does.

Three things surfaced during implementation that the note had not anticipated:

- **A zero-sized allocation overflows `GainGreedyHeap`**, which keys allocations
  by address and assumes a positive extent. Unrelated to the journal; recorded in
  `later.md`.
- **`Freed` needs a lookup-guarded release.** §3 says so, but it is easy to miss:
  an allocation minted and freed in one transaction never reached the heap, so
  freeing it is `UnknownId` — while its counter is still owed.
- **A gathered run that reads its own allocation must not be split.** A splice
  shifting a tail produces one; splitting it lets the first write clobber bytes a
  later piece still has to read.

## References

- [incremental-compaction.md](incremental-compaction.md) — the heap model this sits
  on: stable ids, the `id → address` table, the bounded compaction step.
- [augmented-segment-tree.md](augmented-segment-tree.md) — the placement and
  compaction policy the scheduler's `Claim` calls into.
- [allocator-spec.md](allocator-spec.md) / [generic-allocator.md](generic-allocator.md)
  — the pointer and backend trait surface.
