# Kladde — Auto-Saved Data Structures in Rust

This repository is a Rust workspace implementing "backed" data structures: data structures that behave like their normal in-memory counterparts but are automatically and durably persisted to a file with every mutation. The library provides backed variants of common container types (vectors, hash maps, ropes) and a derive macro so application developers can turn their own `struct`s and `enum`s into backed types, as long as those types are themselves built from backed types.

**Mental model.** A backed data structure is "opened" from a file, which loads it into memory and keeps a live connection to the file. Reads only ever touch the in-memory representation — never the file — so read performance stays close to the non-backed equivalent. A mutation does two things at once: it updates the in-memory representation immediately, as usual, and it durably appends a description of the change to an on-disk journal, so that no committed mutation is ever lost even if the process crashes right after the call returns. The bulk, compact on-disk representation (the "snapshot") is *not* updated on every mutation — it's brought up to date periodically, when the journal is flushed (see [Flushing](#flushing)).

A working sketch of the core traits and a concrete example (`Vec<T>`) lives in [`sketch.rs`](sketch.rs), including inline review notes on a few things that don't compile as currently written.

## On-Disk Layout

The file consists of a collection of dynamically allocated memory regions, similar to a heap, split into two kinds:

- **The snapshot** holds a typically-recent-but-not-fully-up-to-date state of every backed data structure, in a compact binary form similar to (but not identical to) the in-memory representation. Complex data structures — nested hash maps, ropes — are generally distributed over several non-contiguous regions of the file.
- **The journal** is an append-only, linked sequence of allocated regions holding serialized high-level operations. Every mutation appends one entry: a data-type ID, an op code, and (depending on the op code) a payload. Each entry is framed with a length prefix and a checksum, so a crash mid-append leaves a detectable, truncatable torn entry at the tail — nothing before it is affected.

Operations are defined per data type as an associated `Op` type (typically an enum) on the `Backable` trait — see [The Trait Layer](#the-trait-layer). `Op` must implement `serde::Serialize`/`Deserialize`; entries are serialized with a fast, compact binary `serde` format.

### Type Registry and Extensibility

The file format isn't fully self-describing — the declaration of a data type can't be recovered by looking at a file alone — but the library can detect whether a file's contents match the types an application expects. Each file has a dedicated slot holding a vector of type hashes, stored and managed like any other backed vector. This vector serves two purposes: detecting files with incompatible data, and defining the indices/IDs used to reference data types in the journal (a type's ID is its position in this vector).

This makes the format extensible: a new application version can *add* data types and still read files written by older versions, and an old version can still read the parts of a newer file that use only types it recognizes. The one hard rule: **indices are never reused**. Even if a type is dropped in a later version, its slot stays reserved (e.g., with a tombstone hash) so IDs already recorded in existing files stay valid.

- **Comment:** I don't think this rule is necessary, or I might be misunderstanding it. If a type is dropped and that type is indeed no longer referenced anywhere in the file, then its slot in the list of type hashes can be overwritten with a new type and thus its index reused by the new type. Since the order in which indices appear in the file defines the mapping from IDs to types, this mapping has to be defined on a per-file basis anyway (two different files intended for the same version of an application might list the type hashes in different orders, and that's fine as long as the contents of each file uses type IDs according to the order in which type hashes are listed in the respective file).

## Pointers and Memory Management

Pointers are serialized to the snapshot as byte-offsets into the file. Every pointer points at the beginning of an allocated memory region, and every live region has exactly a single pointer pointing to it serialized in the snapshot (the "owner" of that memory region).
Thus, when a memory region is moved around (for compactification), only the single owning pointer has to be updated to point to the new location.
The journal can contain additional, non-owning pointers (aka references), which do not necessarily have to point at the beginning of a memory region but can point anywhere from the beginning to the end (both inclusively) of any memory region.
These references are untracked, which is why compactification is only allowed when the journal is empty.
References in the journal also have to satisfy a lifeness guarantee, i.e., when flushing the journal one operation after another, and an operation frees a memory region, then subsequent references in the journal must not point into that memory region.
It will likely turn out to be impossible to violate this constraint anyway, but we should keep it in mind until we've fully designe the system.

The in-memory representations of some complex backed data structures (e.g., container types) also contain pointers to the snapshot section as part of their meta data to keep track of where things are layed out in memory.
To simplify memory management, we require that the collection of all live backed data structures must hold exactly one pointer in memory for each allocated memory region.
In-memory representations of pointers are small opaque handles (`struct OwnedPointer<T> { index: NonZeroU32, _marker: PhantomData<T> }`) where the `index` is used by the `alloc` trait to resolve the pointer to an up-to-date optional `position` (where the only serilized representation of this pointer is in the snapshot, if it is already flushed) and an optional `target` (where the pointed to memory region sits, if the allocation operation has already been flushed).
It is yet unclear how exactly this resolution will work.
We'll mock `alloc` initially and implement the higher-level parts of the system to see what the exact requirements on `alloc` are before we reconsider how `alloc` works precisely.

**Comments:**
- Is `OwnedPointer` a good name or would `OwningPointer` or `UniquePointer` be better?
- I removed some text that indicated that the in-memory representation of `OwnedPointer`s gets resolved by looking up the pointer's `position` in a table held by alloc. I'm not sure if this would work since memory can be moved around, so if memory contains serialized pointers, than those pointers will be moved around, so indexing by position is unstable.
- Variance: should `OwnedPointer<T>` contain `PhantomData<T>` or `PhantomData<*T>` or `PhantomData<NonZero<T>>`?

`alloc` keeps an in-memory-only registry (reconstructed from the file when it's opened, never itself persisted) mapping each live pointer's `position` to its current target and size — conceptually a `BTreeMap<Position, (Target, Size)>`. This one structure supports two operations:

- **Point lookup** (given a pointer, find its target): needed whenever type-level code wants to actually follow/dereference a pointer.
- **Range query** (given a byte range being relocated, find every pointer whose *position* falls inside it): needed whenever `alloc` moves a block of memory that has `OwnedPointer`s embedded inside its own serialized bytes (e.g., a hash map's bucket array), so their `position` fields can be updated to reflect the new location.

Each block's own allocation metadata additionally records the identity of its single owning pointer, so that when a block's *content* moves, updating the one pointer that targets it is an O(1) lookup rather than a search.

- **Comment:** this might turn out to be necessary, but I'm not sure yet. It might also turn out that any time we want to move a memory block we already come from a pointer to it. Let's defer this issue, mock `alloc` for now, and then revisit the internals of compactification once we know more about the requirements on `alloc`.

Together, these let `alloc` relocate and compact memory — including blocks that themselves contain embedded pointers — entirely on its own, using nothing but byte-range geometry, without ever needing to call back into type-specific code to ask "which of your bytes are pointer fields." (An earlier version of this design didn't have this property; see [Alternatives Considered](#alternatives-considered).)

### Freeing

`Sink::free::<T>(pointer)` (see [The Trait Layer](#the-trait-layer)) doesn't talk to `alloc` directly — like `record`, it just appends a `Free` entry to the journal. The actual bookkeeping update (marking the registry entry free, making that space available for reuse) only happens when that entry is compiled into a microoperation during the next flush, where it's subject to the same op-log optimization as everything else. This is what makes an insert-then-delete within a single not-yet-flushed epoch free in the "no cost" sense: the matched allocate/free pair cancels before either op ever reaches `alloc`, so a value that's created and deleted before it's ever flushed never gets a real `OwnedPointer` and never touches `alloc` at all.

`OwnedPointer` is deliberately *not* self-freeing: it doesn't hold a reference to `alloc`/`Sink`, to keep it small, so it can't call `Sink::free` from its own `Drop`. Instead, freeing is the responsibility of the generated container wrapper types: a wrapper's `Drop` impl calls `Sink::free` on any `OwnedPointer`s it directly owns. For example, taking the value out of a backed `Option<String>` (`OptionMut::take(&mut self) -> std::Option<StringMut>`) transfers ownership of the string's `OwnedPointer` from the option's slot to the returned `StringMut` without ever freeing it; only once *that* value is eventually dropped does its `OwnedPointer` get freed — at every point exactly one live value owns the pointer, so the transfer can't double-free or leak.

Because application code never constructs or holds a bare `OwnedPointer` — only derive-macro-generated code does, as an implementation detail of the generated `Drop` impls — this isn't a convention a human needs to remember each time they write a mutating method; it's an invariant of generated code, verified once. A debug-only leak check (e.g., a thread-local counter of outstanding `OwnedPointer`s that should net to zero when a `Sink` closes) would be a cheap, optional addition for extra insurance during development, but the core design doesn't depend on it.

- **Comment:** note that not all date types will be derive-macro-generatable. Complex data types such as containers will likely require some manual fiddeling with backed pointers, similar to how the implementation of containers in the rust standard library has to manually fiddle with raw pointers. But this is fine because we'll try to provide implementations for the most common such container types in the `types` crate.

### Alternatives Considered

- A freely-aliased `Rc<Cell<u32>>`-style pointer, without the single-owner restriction — rejected because aliasing means a moved target could have arbitrarily many live copies elsewhere, requiring a full registry just to find and rewrite all of them.
- A dense on-disk *handle table* (conceptually a page table, or classic Mac OS "Handles") — fully decouples pointer identity from location, so block moves never need any patching, at the cost of persisting an extra indirection structure to the file. Set aside mainly because of that footprint, given the target workload (many small, deeply nested pointers).
- An intermediate design holding `Rc<Cell<(target, position)>>` per pointer, with `alloc` retaining a clone of each — superseded by the plain, `alloc`-owned position-keyed registry above, which needs no per-pointer heap allocation or refcounting at all.

## Flushing

Periodically (e.g., when the journal exceeds a size threshold), the journal is *flushed*: its operations are applied to the snapshot section and the journal is discarded. This is a multi-step pipeline designed to minimize random-access writes:

1. **Operations** in the journal (data-type-dependent, e.g. "remove key X from the hash map at pointer Y").
2. → **optimized operations**, by the data type implementations (e.g. a modify-then-delete on the same key drops the obsolete modification; a rope implementation might collapse a run of single-character deletions into one range deletion).
3. → **microoperations**, compiled by the data type implementations calling into `alloc` (data-type-independent: "allocate size N" / "free pointer P").
4. → **optimized microoperations**, by `alloc` (e.g. modify-then-free on the same region drops the obsolete modification).
5. → **applied** to the snapshot section.
6. → **compactified**, by `alloc` alone — relocating and shortening allocations to reclaim fragmentation, using the position-keyed registry described above. Unlike earlier drafts of this pipeline, this step no longer needs to call back into data-type implementations.

### Crash Consistency

The journal's own append-only, framed format already makes it crash-safe (see [On-Disk Layout](#on-disk-layout)). Making the *flush* crash-consistent uses copy-on-write / shadow paging — the same technique LMDB uses, and the simplest member of the family that includes ZFS/Btrfs snapshots and ARIES-style database recovery:

- During a flush, never overwrite live snapshot data in place; write every new or moved block to fresh, previously unused space.
- The old snapshot stays byte-for-byte intact and valid for the entire duration of the flush.
- At the end, atomically switch to the new state with a single small, `fsync`'d write of a root/commit marker (the file header's pointer to the current root object(s) plus the current journal-start offset). The new data must be `fsync`'d *before* the marker, so the marker can never become durable while pointing at data that isn't.
- A crash before that marker write completes just means the next open sees the old marker, ignores the half-written new data as inert garbage, and either retries the flush or keeps running with the journal un-flushed. No partial state is ever live, so there's nothing to roll back and no per-operation progress tracking is needed.
- Cost: temporarily more disk space during a flush (old and new coexist until commit), and the old space needs reclaiming afterward — safe to do non-atomically, since nothing live references it; a second crash there just leaks (not corrupts) space that a later flush reclaims.

A more elaborate alternative — fine-grained ARIES-style redo logging of the flush's own writes, so a crash mid-flush can *resume* instead of restart — is only worth the complexity if flushes become expensive enough that discarding a half-finished one on every crash is unacceptable; not needed until that's shown to matter (see [Future Work](#future-work-v2)).

## The Trait Layer

Three traits (names still under discussion — see [Open Questions](#open-questions)), all defined in the `traits` crate:

```rust
trait Backable {
    type Op;
    type Backed<'s, S: Sink>: Backed;
    fn back<'s, S: Sink>(&'s mut self, sink: &'s S) -> Self::Backed<'s, S>;
}

trait Sink {
    fn record<T: Backable>(&self, op: &T::Op);
    fn free<T: Backable>(&self, pointer: OwnedPointer<T>);
}
```

`Backable` is implemented by every plain, non-backed-looking value type (`Vec<T>`, a user's `#[derive(Backable)]` struct, ...). It declares its own operation-log entry type (`Op`) and, via a generic associated type, the type of its *mutable view*. The generated mutable-view type additionally implements a small `Backed` trait (exposing `as_backable`/`as_backable_mut`/`sink()`) that `Deref`/`DerefMut` are implemented against — see `sketch.rs` for the exact shape.

### Mutable Views and the `Sink`

Mutating access never happens directly on the plain type — it always goes through a generated wrapper (e.g. `VecMut<'s, T, S>`), obtained via `.back(sink)`, which borrows both the underlying value and the `Sink` for its lifetime `'s`. Every mutating method on the wrapper records its `Op` via `self.sink.record(...)` and mutates the in-memory data directly. Mutating methods never *return* an `Op` for the caller to separately apply — persistence isn't optional or forgettable by construction. Nested `Backable` fields get their own `_mut()` accessor that reborrows the *same* sink further down, so callers only ever supply a sink once, at the point they obtain the outermost wrapper:

```rust
/// Derive-generated accessors for a `struct Point { x: i32, y: BackedString }`:
impl<'s, S: Sink> PointMut<'s, S> {
    fn set_x(&mut self, value: i32) {
        self.sink.record(&PointOp::SetX(value));
        self.data.x = value;
    }
    fn y_mut(&mut self) -> BackedStringMut<'_, S> {
        self.data.y.back(self.sink)
    }
}
```

Non-mutating access uses the plain type directly; the wrapper implements `Deref` (to the plain type) so read-only methods stay available while inside a mutating context.

Each wrapper type is generated *per concrete `Backable` type* (rather than one shared generic wrapper) specifically so that application and library authors can write ordinary `impl` blocks on their own generated wrapper types without hitting Rust's orphan rules — a single shared wrapper defined in `traits` would be a foreign type from the point of view of any downstream crate, and Rust forbids inherent `impl` blocks on foreign types outright, regardless of what its generic parameters are filled with. (Downstream crates *can* still extend a foreign wrapper via their own local extension trait, but that's more boilerplate than just owning the type.)

`S: Sink` is a static (generic, not trait-object) type parameter, defaulted to a concrete `DefaultSink` provided by `frontend`, so application code that only ever uses the default sink never has to name `S` at all. Being generic over `Sink` still earns its keep for cases that want a different one:

- A no-op / purely in-memory `Sink`, for unit-testing `types` logic without touching a file.
- A recording/spy `Sink`, for asserting exactly which `Op`s a mutation produces.
- A batching `Sink` that buffers ops for an explicit `commit()` instead of persisting on every call, as an opt-in.
- Eventually, a replicated/networked `Sink` — kept possible, not committed to (see [Concurrency](#concurrency)).

## Workspace Layout

- `traits`: `Backable`, `Backed`, `Sink`, `OwnedPointer` (or a trait for it) — the shared vocabulary `alloc` and `types` both build on.
- `alloc`: owns all pointer/allocation bookkeeping (the position-keyed registry, block metadata) and the microoperation-level memory management. Depends only on `traits`.
- `types`: the built-in backed container types (vectors, hash maps, ropes) and their generated mutable-view wrappers. Builds on `traits`, `alloc`, `derive` (for nested data), and `frontend` (solely for the `DefaultSink` default type argument — a convenience dependency, not a functional one; `types`'s own logic is written purely against the `S: Sink` bound).
- `derive`: the macro that turns a user's `struct`/`enum` into a `Backable` type, generating the same wrapper/`Deref`/`Drop` machinery `types` uses internally for its own built-in types.
- `frontend`: opening files, reading headers, running the flushing pipeline, and the concrete `DefaultSink` implementation. Meant to be used by application crates, not by library crates implementing their own `Backable` types.

The two cross-crate callback boundaries this layout originally seemed to require are both resolved by the design above, rather than needing a runtime callback mechanism:

1. `alloc` asking a concrete data type to update pointer metadata during compaction — resolved by the position-keyed registry, which lets `alloc` patch every affected pointer using only byte-range geometry.
2. `types` getting its mutations into the journal without depending on `frontend` — resolved by `Sink`, which every mutable-view wrapper already holds.

## Concurrency

v1 targets a single process with exclusive write access to a file — no concurrent writers, no shared-memory-mapped readers from other processes — which keeps `alloc` and the journal free of locking concerns. This is deliberate: get the core architecture right first, but avoid decisions that would be fundamentally incompatible with a concurrent future. Two choices already made lean the right way: single-owner `OwnedPointer`s (rather than freely-aliased pointers) map more naturally onto typical single-writer/multiple-reader schemes (e.g. MVCC-style snapshot isolation) than aliasing would have, and `Sink` being swappable leaves room for a replicated/networked implementation later without touching `types`.

## Future Work (v2+)

Deliberately deferred, to be revisited once the v1 architecture is validated:

- **Concurrency** — see above.
- **Generation counters on pointers**, to catch use-after-free / stale-handle bugs, at the cost of extra bytes per pointer.
- **Resumable, ARIES-style flush recovery** (fine-grained redo logging of the flush's own writes, instead of shadow-paging's restart-from-scratch) — only worth it if flush cost becomes a real problem.

## Prior Work

This design overlaps with several existing systems, though none combine all of its constraints at once: native Rust types (not a fixed generic document model), no translation to database commands, no full-object/full-document reserialization on each mutation, and read speed close to the non-backed equivalent.

### Tier 1: transparent/"orthogonal" persistence for ordinary objects

These give you ordinary-looking mutable objects with automatic, no-SQL persistence, but generally at *per-object* (or per-page) granularity rather than per-mutation-operation granularity — a single field write still tends to trigger rewriting some encompassing structure (a whole pickled object in ZODB, a chain of B+-tree nodes in Realm), not just one small logged op.

- **ZODB** (Python) — `Persistent` subclasses track dirty attributes via `__setattr__`; only dirty objects get re-pickled on commit. Its `FileStorage` backend is an append-only transaction log with periodic `pack()` compaction — structurally close to our journal + flush.
- **GemStone/S** (Smalltalk) — the deepest historical precedent for our pointer design: its *object table* maps object IDs to physical page locations, so compaction only ever touches that one indirection layer, closely matching our position-keyed `alloc` registry.
- **Realm** (Swift/Kotlin/JS) — "live objects" with native property syntax over an MVCC, copy-on-write storage engine; no SQL, fine page-level granularity. See detailed comparison below.
- **db4o** / **ObjectDB** (Java/.NET) — bytecode enhancement intercepts field writes on plain objects; no SQL, no ORM-to-relational translation.

### Tier 2: fine-grained, per-operation journaling

Closer to our actual granularity — the CRDT / local-first world — despite solving a different problem (multi-writer merge, not single-writer durability):

- **Automerge** (`automerge-rs`) — ordinary-looking maps/lists/text; every change is captured as a small op, persisted as an append-only, compact (columnar-encoded) change log with periodic compaction into a snapshot. See detailed comparison below.
- **Yjs** — same idea, JS ecosystem.

### Academic lineage

The general idea has a name: **orthogonal persistence** — persistence that's automatic and independent of an object's type or size, with no explicit save step. It originates with **PS-algol** and **Napier88** (Glasgow/St Andrews, 1980s); Sun's **PJama** was the closest attempt to bring it to a mainstream language, via incremental, differential persistence of a Java object heap. None of these are things to depend on today, but they're the right search terms for how far this idea has historically been pushed, and what tends to go wrong at scale.

One observation ties this lineage back to our own design: ZODB, Realm, and GemStone all make persistence syntactically invisible (`self.foo = bar` just works) because Python/Swift/Smalltalk each have a hook — `__setattr__`, property wrappers + KVO, message dispatch — that intercepts an ordinary-looking mutation. Rust has no such hook, which is exactly why our design needs the more visible `Mut` wrapper and explicit `.back(sink)`: we're compensating for a language capability those systems quietly rely on and Rust doesn't have.

### Realm, in more detail

**Overlap:** both maintain a canonical, cheap-to-read representation, use copy-on-write so writes never corrupt live data mid-commit, and hide persistence behind generated wrapper types with native-feeling mutation syntax (Realm's codegen'd `@Persisted` properties ~ our derive-generated `Mut` wrappers).

**Where it diverges:** Realm has no representation separate from the file — its "live objects" are thin accessors directly over the mmap'd, columnar on-disk layout; reads go through that layout (cheap, page-cached, zero-copy, but still not "a plain `HashMap` sitting in your process"). We deliberately went the other way: keep a fully separate, natively-typed in-memory structure that's independently as fast as the non-backed equivalent, and let the file lag behind via the journal. This also means Realm writes cost more per-mutation in principle — a single property write still triggers copy-on-write up a B+-tree-like path to the root (O(log n) node copies), where our journal append is O(1) until the next flush. Realm also does real multi-thread/multi-process concurrency with auto-updating objects (explicitly out of scope for our v1 — see [Concurrency](#concurrency)), and is schema/table-oriented (a fixed set of property types, links, lists-of-X) rather than arbitrary recursively-nested user-defined Rust types. It's also a full product (query language, hosted sync service, SDKs across many languages) — a different weight class from a building-block library.

### Automerge, in more detail

**Overlap is specific and real:** `save_incremental()` appends a compact chunk of new ops; `save()`/compaction folds everything into one dense base representation; loading replays that back into a queryable structure — essentially our journal+flush pattern, already implemented. Its columnar op-encoding (splitting ops into separate compressed columns rather than serializing each op as a struct) is more sophisticated than our plain serde-per-op journal and worth studying for our own format.

**Where it diverges — driven by the problem it solves (multi-writer merge), not incidental:**

- **Retention.** CRDT lists need tombstones for deleted elements (a concurrent insert might still reference a deleted position), and enough causal history survives to resolve concurrent conflicting writes deterministically. We never need this — a delete is final and its `OwnedPointer` is immediately, unconditionally freeable.
- **Value representation.** Every Automerge value can, in principle, need conflict resolution, so reading "the current value" is resolving something, not a raw field access. Our read-speed goal is only reachable because we don't have to support that.
- **Data model.** Automerge's document is a fixed generic tree of Map/List/Text/scalar; getting your own Rust types in and out goes through a separate reflection/reconciliation layer (`autosurgeon`). Our derive macro makes *your* type the backed type directly, with real methods and arbitrary nesting.
- **Scope of the storage layer.** Automerge's persistence is bespoke to its own op log, not a reusable file-backed heap — there's no analog to our `alloc` crate as a general-purpose, relocatable-pointer foundation other backed structures could be built on.

**What our design provides that Automerge doesn't**, as a direct consequence of not needing to support merge:

1. Native persistence of arbitrary Rust types, with real derive-generated methods and arbitrary nesting — not a fixed document model plus a reflection layer.
2. No CRDT retention tax — deletions are final and immediately reclaim storage.
3. Read performance that can genuinely match native collections, unconditionally.
4. A reusable, general-purpose persistent-heap abstraction (`alloc` + `OwnedPointer`) as a foundation for building new kinds of backed structures.
5. Mutation ergonomics closer to plain `std::collections` — direct `.push()`/`.insert()`/`.remove()` through a `Mut` view, rather than operations scoped inside a transaction against a generic document tree.

None of this makes Automerge worse — every trade-off above is the direct cost of solving a harder problem (multi-writer merge) that this project explicitly scopes out for now (see [Concurrency](#concurrency) and [Future Work](#future-work-v2)). The reverse trade holds too: the moment concurrency/sync becomes a real requirement, Automerge's model is already there, and ours would have to grow substantially to get anywhere close.

## Why the Name "Kladde"?

*Kladde* is German for a merchant's rough day-book: transactions scribbled down messily, in order, as they happened, later transcribed into the clean *Hauptbuch* ("main ledger").
This project's on-disk format follows the same two-part shape — a chaotic, continuously-appended journal, periodically compiled into a clean snapshot (see [Flushing](#flushing)) — so the name doubles as a description of the file format itself, not just the library.

## Open Questions

- **Naming.** `Backable`, `Backed` (the mutable-view trait), and `Sink` are all placeholders. `Sink` in particular no longer fits well now that it also owns freeing (`free`), not just journaling (`record`) — worth deciding whether that should stay one trait or split into a journaling half and a freeing/allocation half.
  - **Comment:** (regarding `Backable` and `Backed`): Come up with appropriate names for these. They should be short and communicate at least some of the following aspects:
    - `Backable` is a type that -- in principle -- supports orthogonal persistence but doesn't actually because it's not associated with any backing store.
    - `Backable` therefore only allows read access.
    - `Backed` is a type that actually suports orthogonal persistence (it is associated with a backing store / sink / whatever we rename sink to below)
    - `Backed` allows write access (mutation).
    - `Backed` is typically but not strictly necessarily a wrapper around a `Backable`.
    - `Backed` and `Backable` go hand in hand, ideally the names should reflect this but make them less easy to confuse as they currently are.
    - There should be a convention of how the name of a `Backed` wrapper is derived from the name of its corresponding `Backable` (in `sketch.rs`, I appended `Mut` to the type name). Ideally, the names of the traits should somehow fit to the naming convention of the types that implement them.
  - **Comment:** (regarding `Sink`) three options:
    - (a) come up with a new name for `Sink` to refer to the full "persistency backend"; or
    - (b) split `Sink` into two traits: one related to recording operations in the journal and one related to memory allocation. Find appropriate names for both parts. Then find an appropriate name or letter for the type parameter formerly referred to as `S`, which now has to implement both of the traits; or
    - (c) split `Sink` into two traits as in option (b), and also split the type parameter `S` into two type parameters, so that one can mix and match. Consider, however, that appending to the journal might require allocation (when the journal exceeds its current memory allocation but not its flushing threshold, or when a single large Op has to be appended to the journal)
    Respond below with a deliberation of the pros and cons of the above three options a-c.
- **The exact shape of `alloc`'s registry.** A position-keyed `BTreeMap` covers point lookup and range query conceptually, but the concrete data structure — including how block-size/free-list bookkeeping integrates with it, and how it's efficiently rebuilt when a large file is opened — still needs to be designed and prototyped.
  - **Comment:** Defer this discussion. Since backed types are generic over the sink / allocator, we'll want to start developing with a mock allocator anyway. The mock allocator essentially falls back to creating `Box<[u8]>` (or maybe `Rc<[u8]>` but that's probably not necessary), it associates each allocated memory region with an ID based on an simple incrementing counter, and it has a hash map that maps those ids to the `Box`es and some meta data. This will allow us to design the higher-level parts of the system on top of the mock allocator, which whill show us what the exact requirements on the allocator are.
- **The `Backable`/`Backed` GAT signature.** The lifetime-parameterized associated type above needs to be checked against a real, compiling example — an earlier draft in `sketch.rs` had a lifetime-arity mismatch between the trait declaration and its impl. Worth nailing down before it's relied on elsewhere.
  - **Comment:** fix all my errors in `sketch.rs` using your best judgment. Don't take my code in `sketch.rs` too literally, I sketched it up hastily and I am aware that it is internally inconsistent. Once you've converged to a consistent picture in `sketch.rs`, remove any obsolete comments.
- **Derive macro coverage.** The mutable-view-wrapper generation pattern is sketched above for plain structs (the `Point` example); it still needs to be worked out for enums with multiple variants, generic types, and types with where-clauses.
- **Concurrency design**, once revisited (see [Future Work](#future-work-v2)).
