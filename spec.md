# Kladde — Auto-Saved Data Structures in Rust

This repository is a Rust workspace implementing "backed" data structures: data structures that behave like their normal in-memory counterparts but are automatically and durably persisted to a file with every mutation. The library provides backed variants of common container types (vectors, hash maps, ropes) and a derive macro so application developers can turn their own `struct`s and `enum`s into backed types, as long as those types are themselves built from backed types.

**Mental model.** A backed data structure is "opened" from a file, which loads it into memory and keeps a live connection to the file. Reads only ever touch the in-memory representation — never the file — so read performance stays close to the non-backed equivalent. A mutation does two things at once: it updates the in-memory representation immediately, as usual, and it durably appends a description of the change to an on-disk journal, so that no committed mutation is ever lost even if the process crashes right after the call returns. The bulk, compact on-disk representation (the "snapshot") is *not* updated on every mutation — it's brought up to date periodically, when the journal is flushed (see [Flushing](#flushing)).

## On-Disk Layout

The file consists of a collection of dynamically allocated memory regions, similar to a heap, split into two kinds:

- **The snapshot** holds a typically-recent-but-not-fully-up-to-date state of every backed data structure, in a compact binary form similar to (but not identical to) the in-memory representation. Complex data structures — nested hash maps, ropes — are generally distributed over several non-contiguous regions of the file.
- **The journal** is an append-only, linked sequence of allocated regions holding a small, fixed set of type-agnostic memory-management microoperations — not one entry per high-level mutation, but one entry per underlying byte-level effect that mutation has. Every entry is one of: `Alloc(size) -> index`, `Free(index)`, `Write(index, offset..offset+len, bytes)`, `Copy(src_index, src_offset..src_offset+len, dst_index, dst_offset)`, `Resize(index, new_size)`. None of these know or care what application-level type is involved — see [The Trait Layer](#the-trait-layer)'s `Allocator`. Each entry is framed with a length prefix and a checksum, so a crash mid-append leaves a detectable, truncatable torn entry at the tail — nothing before it is affected. **Note on ordering:** for a single high-level mutation that emits several of these (e.g. a vec push that has to grow), the emitting code must order them so that *any* prefix of the sequence, replayed alone, leaves a valid — if stale or slightly leaky — state: nothing becomes reachable (a length bump, a pointer's target changing) until everything it depends on is already earlier in the sequence, and nothing gets freed until whatever superseded it has already been published. See [Crash Consistency](#crash-consistency).

A microoperation's own encoding (which variant, which index/offset/len) is small and fixed-shape. The *payload* bytes a `Write` or `Copy` carries are produced by whatever `Guard` method is emitting it — for an application value, that's still [`postcard`](https://crates.io/crates/postcard) for now (chosen after `bincode`, an earlier candidate, lost its maintainers — see [RUSTSEC-2025-0141](https://rustsec.org/advisories/RUSTSEC-2025-0141.html)), though a `Write`'s payload is just opaque bytes as far as the journal and `Allocator` are concerned — see `later.md`'s note on eventually letting large payloads become allocations directly, without a postcard round-trip.

### Type Registry and Extensibility

**Deferred, not part of the current design.** Earlier drafts of this section described a dedicated file slot holding a vector of type hashes, serving two purposes: tagging journal entries with a type ID for per-type dispatch during replay, and detecting files with incompatible data. The first purpose is now moot — the journal only ever holds the fixed, type-agnostic microops described in [On-Disk Layout](#on-disk-layout), so replay never needs to know what type anything is. The second (file-compatibility detection) is still a real problem eventually, but a type-hash vector was always a fairly brittle way to solve it; the plan is to replace it with an explicit semantic-versioning scheme instead (tracking, per file, both the minimum and most-recent app/library versions that wrote it — see `later.md`) once that's designed. Not needed for v1, which has no real file to be incompatible with in the first place.

## Pointers and Memory Management

Pointers are serialized to the snapshot as byte-offsets into the file. Every pointer points at the beginning of an allocated memory region, and every live region has exactly a single pointer pointing to it serialized in the snapshot (the "owner" of that memory region). When a memory region is moved (for compactification), only its single owning pointer has to be updated.

The journal can additionally contain non-owning pointers ("references"), which don't have to point at the beginning of a region — they can point anywhere from its beginning to its end (inclusive). These references are untracked, which is why compactification is only allowed when the journal is empty. References in the journal also have to satisfy a liveness guarantee: when flushing the journal operation by operation, once an operation frees a memory region, no subsequent reference in the journal may point into that region. (This is likely impossible to violate by construction, but worth keeping in mind until the system is fully designed.)

The in-memory representation of a complex backed data structure (e.g. a container type) also holds pointers into the snapshot as part of its own metadata, to track where things are laid out. To simplify memory management, the collection of all live backed data structures holds exactly one in-memory pointer per allocated memory region. In-memory pointers are small, opaque handles:

```rust
struct UniquePointer<T> {
    index: NonZeroU32,
    _marker: PhantomData<*const T>,
}
```

`index` is a stable identity assigned once by `Allocator` and never changes for the lifetime of the pointer — even though the region it (eventually) refers to may move during compaction, and even though the pointer may not have been flushed to the snapshot at all yet. (An earlier design used the pointer's own serialized `position` as its identity; that breaks the moment a block containing embedded pointers is relocated, since `position` changes but nothing else about the pointer does — `index` fixes that.) `PhantomData<*const T>` gives covariance in `T` (sound here, since mutation only ever happens through an exclusive `Guard`, never through a shared reference the way `Cell<T>`'s interior mutability does) without the "may drop a `T`" drop-check obligation `PhantomData<T>` would impose — `UniquePointer` never actually runs `T::drop`; see [Freeing](#freeing).

`Allocator` keeps an in-memory-only registry (reconstructed from the file when it's opened, never itself persisted):

- A primary structure (a dense table/slab, keyed by `index`) mapping each pointer's `index` to its current, mutable `(position, target, size)` — each represented as `Option<NonZeroU32>`, `None` before the pointer's allocation and its own serialization have actually been flushed. Real positions/targets are never zero (the file starts with a fixed-size header region), so `None` costs nothing extra via niche optimization. `Position`/`Size` are their own type aliases rather than bare integers, so widening them later (to `u64`, or to a variable-length encoding) only touches one definition. Supports point lookup: given an `index`, resolve its current target.
- An auxiliary structure ordered by `position` (e.g. a `BTreeMap<Position, Index>`), kept in sync whenever a `position` changes, supporting range queries: given a byte range being relocated, find every `index` whose current `position` falls inside it. Needed whenever `Allocator` moves a block that has `UniquePointer`s embedded in its own serialized bytes (e.g. a hash map's bucket array), so their `position`s can be updated. Because this structure is purely an internal lookup aid — never treated as identity by anything outside `Allocator` — keeping it in sync with a moving key isn't a problem.

`UniquePointer` itself is never `Serialize`/`Deserialize` — its `index` is meaningless outside the process that assigned it, and exposing it to serde directly would invite accidentally serializing that meaningless value. Instead, `Allocator::resolve` borrows both the allocator and the pointer to produce a `ResolvedPointer<'a, T>`, which *is* serializable — it holds the pointer's current on-disk `target` (the offset of the region it points *at*, which is what a pointer's serialized value actually is; `position` is a separate concept, the offset of the pointer's *own* serialized bytes, tracked by `Allocator` as internal bookkeeping once this value has actually landed somewhere in the file — not something `ResolvedPointer` itself knows). Borrowing `Allocator` for `'a` is deliberate: it prevents, at compile time, any `Allocator` operation that could invalidate that snapshot (most importantly, compaction moving the target) for as long as a `ResolvedPointer` derived from it is still alive — the borrow checker enforces that a serialized `target` was still current when it was written. Deserializing goes the other way without needing serde's stateful-deserialization machinery: the raw `target` is read back as an ordinary integer, and a fresh `UniquePointer` (with a newly assigned `index`) is minted for it via a plain `Allocator` method call, not via `Deserialize`.

Each block's own allocation metadata additionally records the identity of its single owning pointer, so that when a block's *content* moves, updating the one pointer that targets it is an O(1) lookup rather than a search. Whether this extra bookkeeping actually pays for itself — versus always already having a pointer in hand whenever a block needs to move — is left open until the higher-level system has been built against a mock `Allocator` and shows what's actually required.

Together, these let `Allocator` relocate and compact memory — including blocks that themselves contain embedded pointers — entirely on its own, using nothing but byte-range geometry, without ever needing to call back into type-specific code to ask "which of your bytes are pointer fields." (An earlier version of this design didn't have this property; see [Alternatives Considered](#alternatives-considered).)

### Freeing

`Allocator::free::<T>(pointer)` (see [The Trait Layer](#the-trait-layer)) doesn't touch `Allocator`'s live registry directly — it just appends a `Free` microoperation to the journal. The actual bookkeeping update (marking the registry entry free, making that space available for reuse) only happens when that entry is replayed during the next flush, subject to the same microop-log optimization as everything else. Note this is slightly weaker than an earlier draft of this section claimed: `Allocator::alloc` assigns a real `index` and creates a real (if still unresolved, `target: None`) registry entry *immediately*, at mutation time — not deferred to flush — so a value created and deleted within a single not-yet-flushed epoch does still consume and release an index (an index needs to be a stable, real identity from the moment anything might reference it, not just from the moment it's first flushed). What's genuinely free is that the pair never gets its allocation *materialized*: if the matching `Alloc`/`Free` microops both cancel during optimization before flush, no space is ever actually reserved in the file, and no bytes are ever actually written for it.

`UniquePointer` is deliberately *not* self-freeing: it doesn't hold a reference to the backend, to keep it small, so it can't call `Allocator::free` from its own `Drop`. Instead, freeing is the responsibility of the generated container wrapper types: a wrapper's `Drop` impl calls `Allocator::free` on any `UniquePointer`s it directly owns. For example, taking the value out of a backed `Option<String>` (`OptionGuard::take(&mut self) -> std::Option<StringGuard>`) transfers ownership of the string's `UniquePointer` from the option's slot to the returned `StringGuard` without ever freeing it; only once *that* value is eventually dropped does its `UniquePointer` get freed — at every point exactly one live value owns the pointer, so the transfer can't double-free or leak.

Because application code never constructs or holds a bare `UniquePointer` — only derive-macro-generated code does, as an implementation detail of the generated `Drop` impls — this isn't a convention a human needs to remember each time they write a mutating method; it's an invariant of generated code, verified once. A debug-only leak check (e.g. a thread-local counter of outstanding `UniquePointer`s that should net to zero when a `Backend` closes) would be a cheap, optional addition for extra insurance during development, but the core design doesn't depend on it.

Not all data types will be derive-macro-generatable: complex container types will typically require manual use of `UniquePointer` directly, the same way `std`'s own collections manually manage raw pointers rather than expressing their internals in safe, derivable terms. The derive macro targets the common case — application-level structs/enums composed from already-`Persistable` fields — not container internals; see the `kladde-types` crate under [Workspace Layout](#workspace-layout).

### Alternatives Considered

- A freely-aliased `Rc<Cell<u32>>`-style pointer, without the single-owner restriction — rejected because aliasing means a moved target could have arbitrarily many live copies elsewhere, requiring a full registry just to find and rewrite all of them.
- A dense on-disk *handle table* (conceptually a page table, or classic Mac OS "Handles") — fully decouples pointer identity from location, so block moves never need any patching, at the cost of persisting an extra indirection structure to the file. Set aside mainly because of that footprint, given the target workload (many small, deeply nested pointers).
- An intermediate design holding `Rc<Cell<(target, position)>>` per pointer, with the allocator retaining a clone of each — superseded by the plain, index-keyed registry above, which needs no per-pointer heap allocation or refcounting at all.

## Flushing

Periodically (e.g., when the journal exceeds a size threshold), the journal is *flushed*: its microoperations are applied to the snapshot section and the journal is discarded. Since the journal is already nothing but `Alloc`/`Free`/`Write`/`Copy`/`Resize` entries (see [On-Disk Layout](#on-disk-layout)), this pipeline is shorter than earlier drafts of this section described — there's no separate "compile a high-level op down to microoperations" step, because nothing high-level ever reaches the journal in the first place. `Guard` methods emit microoperations directly, at mutation time, as part of doing whatever they do (a `PersistedVec::push` that needs to grow might emit `Resize` then `Write` then a header `Write`); replaying them later never needs to call back into any data-type implementation, `PersistedVec`'s or otherwise — flush only ever talks to `kladde-alloc`:

1. **Microoperations** in the journal, in the order they were recorded.
2. → **optimized microoperations**, by `kladde-alloc` alone, purely mechanically (e.g. an `Alloc(I)`...`Free(I)` pair with nothing else referencing `I` in between cancels entirely; a `Write` fully superseded by a later `Write` to the same span drops the earlier one). No data-type awareness is needed or possible here — see [The Trait Layer](#the-trait-layer) for why this is a deliberate trade against the alternative (semantically-aware optimization, e.g. a rope collapsing a run of edits that cancel out at the *meaning* level even though their bytes don't). **Not yet implemented** — v1's `Kladde::flush` replays every recorded microoperation naively, in order, with no cancellation/dedup pass; this step is deliberately deferred to a follow-up (per the discussion that settled option (b) — journal replay — over re-snapshotting) now that the replay mechanics it'd optimize actually exist.
3. → **applied** to the snapshot section.
4. → **compactified**, by `kladde-alloc` alone — relocating and shortening allocations to reclaim fragmentation, using the pointer registry described above. This was already out-of-band with respect to data-type implementations in earlier drafts and remains so: compaction changes a pointer's `target`, never its `index`, so it isn't expressible as journal microops at all (an `Alloc`/`Free` pair would mint a *new* index) — it's `kladde-alloc` relocating a live allocation in place, without needing anything in the journal to say so.

### Crash Consistency

The journal's own append-only, framed format already makes it crash-safe (see [On-Disk Layout](#on-disk-layout)). Making the *flush* crash-consistent uses copy-on-write / shadow paging — the same technique LMDB uses, and the simplest member of the family that includes ZFS/Btrfs snapshots and ARIES-style database recovery:

- During a flush, never overwrite live snapshot data in place; write every new or moved block to fresh, previously unused space.
- The old snapshot stays byte-for-byte intact and valid for the entire duration of the flush.
- At the end, atomically switch to the new state with a single small, `fsync`'d write of a root/commit marker (the file header's pointer to the current root object(s) plus the current journal-start offset). The new data must be `fsync`'d *before* the marker, so the marker can never become durable while pointing at data that isn't.
- A crash before that marker write completes just means the next open sees the old marker, ignores the half-written new data as inert garbage, and either retries the flush or keeps running with the journal un-flushed. No partial state is ever live, so there's nothing to roll back and no per-operation progress tracking is needed.
- Cost: temporarily more disk space during a flush (old and new coexist until commit), and the old space needs reclaiming afterward — safe to do non-atomically, since nothing live references it; a second crash there just leaks (not corrupts) space that a later flush reclaims.

A more elaborate alternative — fine-grained ARIES-style redo logging of the flush's own writes, so a crash mid-flush can *resume* instead of restart — is only worth the complexity if flushes become expensive enough that discarding a half-finished one on every crash is unacceptable; not needed until that's shown to matter (see [Future Work](#future-work-v2)).

**Crash before any flush — a torn journal tail.** The above covers a crash *during* a flush's own replay; a crash while the journal is merely being *appended to* (no flush of that data has started yet) is a different, narrower case, but still needs an invariant that isn't automatic: for one high-level mutation's microoperations, *any prefix* of that sequence, if that's all that ever gets replayed, must leave a valid (if stale or slightly leaky) state — see the ordering note in [On-Disk Layout](#on-disk-layout). This is satisfiable for every mutation shape checked so far (grow-a-vec, remove-from-the-middle, replace-a-map-entry all decompose into "prepare new/unreachable state → one publish `Write` → cleanup"), but it's a discipline each hand-written container implementation has to get right, not something the type system enforces. Where it can't be satisfied cleanly, `later.md` sketches a fallback: bracket the group with `StartAtomic`/`EndAtomic` microops, and on reopen, truncate back through a dangling (unmatched) `StartAtomic` as if the whole group never happened.

This concern doesn't extend to a *future* auto-flush-on-threshold trigger interrupting a mutation mid-flight, even without crashes: `Kladde::flush` needs `&mut Kladde<T>`, and any live `Guard` (which is how a multi-microop mutation is ever in progress at all) already holds a conflicting borrow derived from `Kladde<T>`. Auto-flush can only ever fire between complete mutations, never inside one — as long as it's never exposed through a path that only needs `&Backend` (which is all a `Guard` holds).

Note: none of this crash-consistency machinery is exercised by the v1 implementation, which runs against a mock, in-memory `Allocator` rather than a real file — see `kladde-alloc` under [Workspace Layout](#workspace-layout).

## The Trait Layer

Three traits, all defined in the `kladde-traits` crate:

```rust
trait Persistable: Sized {
    // Every type has a fixed-size inline representation -- see
    // "Where a Guard writes", below.
    const INLINE_SIZE: usize;

    type Guard<'s, B: Backend>: Guard<Persistable = Self, Backend = B>
    where
        Self: 's,
        B: 's;
    fn guard<'s, B: Backend>(&'s mut self, backend: &'s B, location: Location) -> Self::Guard<'s, B>;

    // The write/read counterparts of a value's inline representation.
    fn store<B: Backend>(&self, backend: &B, location: Location);
    fn load<B: Backend>(backend: &B, location: Location) -> Self;
}

trait Guard {
    type Persistable: Persistable;
    type Backend: Backend;
    fn as_persistable(&self) -> &Self::Persistable;
    fn as_persistable_mut(&mut self) -> &mut Self::Persistable;
    fn backend(&self) -> &Self::Backend;
}

trait Allocator {
    // Pointer lifecycle.
    fn alloc<T>(&self, size: usize) -> UniquePointer<T>;
    fn free<T>(&self, pointer: UniquePointer<T>);
    fn resolve<'a, T>(&'a self, pointer: &UniquePointer<T>) -> Option<ResolvedPointer<'a, T>>;

    // Type-agnostic content microoperations -- the only things the
    // journal ever records; see On-Disk Layout. `read` is the one
    // exception -- a query against already-flushed state, not itself a
    // recorded mutation.
    fn read(&self, target: RawPointer, offset: u32, len: u32) -> Vec<u8>;
    fn write(&self, target: RawPointer, offset: u32, bytes: &[u8]);
    fn copy(&self, src: RawPointer, src_offset: u32, len: u32, dst: RawPointer, dst_offset: u32);
    fn resize<T>(&self, pointer: &UniquePointer<T>, new_size: usize);
}

trait Backend: Allocator {}
impl<B: Allocator> Backend for B {}
```

`Persistable` is implemented by every plain value type that has the right shape to be persisted (`PersistedVec<T>`, a user's `#[derive(Persistable)]` struct, ...) but isn't yet tied to any backend — read-only, matching ordinary in-memory access. Via a generic associated type it declares the type of its *guard*, the write-side counterpart obtained from a `Persistable`, bound to a `Backend` for a lifetime — the same relationship `MutexGuard` has to `Mutex`, and the naming is deliberate: a `Guard` is exactly an RAII token proving exclusive, backend-recording access. **Naming convention**: a `Persistable` type named `Foo` gets a generated guard type named `FooGuard`.

Earlier drafts of this doc had `Persistable` also declare its own operation-log entry type (`Op`, typically an enum), so mutations were recorded and later replayed at the semantic level — "push X", "remove key K". That's gone: mutations are now recorded as a small, fixed set of type-agnostic byte-level microoperations (`write`/`copy`/`resize`, above), emitted directly by `Guard` methods at mutation time, rather than compiled from a semantic `Op` during a later flush. See [Flushing](#flushing) for why. One concrete, if secondary, benefit: since a `Guard` method now serializes a value into an owned byte buffer (still borrowing it, e.g. via `postcard::to_allocvec(&value)`) rather than moving a copy of it into an `Op` enum that also needs to survive independently of the value being pushed into live storage, mutating methods like `PersistedVec::push` no longer need `T: Clone` at all — see [Future Work](#future-work-v2), which retires the lifetime-GAT-`Op` idea that used to be the planned fix for this, since it's no longer needed.

`Allocator` absorbs what used to be a separate `Journal` trait's job. Earlier drafts kept them apart specifically because recording a semantic `Op` was conceptually distinct from managing pointers; now that everything a `Guard` ever records *is* one of `Allocator`'s own primitives, a second trait around a `record` method with nothing type-specific left to record would just split one cohesive concern in two. `Backend: Allocator` remains as a thin, blanket-implemented marker, mostly so application and derive-generated code still only ever needs to write `B: Backend`.

**Where a `Guard` writes.** Every `Guard`, not only container-owning ones, needs to know *where* to write, not just *that* it should. A leaf field's `Guard` (an `i32`, a derived struct's field, ...) locates the nearest ancestor that owns a real allocation and its byte offset within that ancestor's blob via a small `Location { anchor: RawPointer, offset: u32 }`, threaded down the same reborrow chain that already carries `backend` (`Persistable::guard`/`store`/`load` all take one). `RawPointer` type-erases `UniquePointer<T>`'s index (`UniquePointer::raw()`) since a deeply nested leaf doesn't know or care what concrete type its ancestor's allocation was created as.

This is also where `Persistable::INLINE_SIZE` comes from: every type has a fixed-size inline representation (a scalar's own bytes; the sum of a derived struct's fields', so sibling offsets are static; an 8-byte `{ target, len }` header for anything that owns a separate content allocation), plus a symmetric `store`/`load` pair (`store` writes the current value's inline representation, allocating/growing whatever content it owns as needed; `load` is the read-side counterpart, used by the round-trip test). A derived enum is treated as an owning type too — despite whole-value-replacement semantics matching structs more than containers, giving each variant its own static byte layout isn't worth it (see Future Work); it just stores its `postcard`-serialized whole value behind the same 8-byte header `String`/`PersistedVec` use.

One real limitation this surfaced, not resolved here: `String` and derived enums have no room for a persistent `pointer` field of their own (unlike `PersistedVec`/`PersistedHashMap`, which are library-defined structs with room to spare) — so every `store` call allocates a *fresh* content region rather than resizing/reusing the previous one, leaking it. Harmless for the in-memory mock (dies with the process), but a real, file-backed `Allocator` would leak space this way; revisit with a `PersistedString`-shaped wrapper (own `pointer` field, same shape as `PersistedVec`) if a growable string field needs to stop leaking, or if derived enums need the same fix.

### Guards and the `Backend`

Mutating access never happens directly on the plain type — it always goes through a generated wrapper (e.g. `PersistedVecGuard<'s, T, B>`), obtained via `.guard(backend, location)`, which borrows both the underlying value and the `Backend` for its lifetime `'s`. Every mutating method on the wrapper emits whatever `write`/`copy`/`resize`/`alloc`/`free` microoperations its mutation requires directly on `self.backend` (see [The Trait Layer](#the-trait-layer)) and mutates the in-memory data directly. Mutating methods never *return* anything for the caller to separately apply — persistence isn't optional or forgettable by construction. Every field of a `#[derive(Persistable)]` type gets a `_mut()` accessor that reborrows the *same* backend further down into a nested guard, extending `location` by that field's static offset — including scalar/primitive fields (`i32`, `bool`, `String`, ...), which get blanket `Persistable` impls in `kladde-traits` (not `kladde-types` — see [Workspace Layout](#workspace-layout) for why) specifically so the derive macro can treat every field uniformly rather than special-casing "leaf" types differently from nested `Persistable` fields. Callers only ever supply a backend once, at the point they obtain the outermost wrapper:

```rust
/// Derive-generated accessors for a `struct Point { x: i32, y: String }`
/// — every field, including `i32`, is itself `Persistable`, so the macro
/// generates the same kind of accessor uniformly. Each accessor extends
/// the struct's own `Location` by that field's static offset (the sum of
/// every *earlier* field's `INLINE_SIZE`) -- `y`'s offset is `i32`'s
/// `INLINE_SIZE` (4), regardless of how long the string it currently
/// holds is, since `String`'s own inline representation is a fixed-size
/// header (see "Where a Guard writes").
impl<'s, B: Backend> PointGuard<'s, B> {
    fn x_mut(&mut self) -> I32Guard<'_, B> {
        self.data.x.guard(self.backend, Location { anchor: self.location.anchor, offset: self.location.offset })
    }
    fn y_mut(&mut self) -> StringGuard<'_, B> {
        self.data.y.guard(self.backend, Location { anchor: self.location.anchor, offset: self.location.offset + 4 })
    }
}
```

Non-mutating access uses the plain type directly; the wrapper implements `Deref` (to the plain type) so read-only methods stay available while inside a mutating context.

Each guard type is generated *per concrete `Persistable` type* (rather than one shared generic wrapper) specifically so that application and library authors can write ordinary `impl` blocks on their own generated wrapper types without hitting Rust's orphan rules — a single shared wrapper defined in `kladde-traits` would be a foreign type from the point of view of any downstream crate, and Rust forbids inherent `impl` blocks on foreign types outright, regardless of what its generic parameters are filled with. (Downstream crates *can* still extend a foreign wrapper via their own local extension trait, but that's more boilerplate than just owning the type.)

`B: Backend` is a static (generic, not trait-object) type parameter, defaulted to a concrete `DefaultBackend` provided by `kladde`, so application code that only ever uses the default backend never has to name `B` at all. Being generic over `Backend` still earns its keep for cases that want a different one:

- A no-op / purely in-memory `Backend`, for unit-testing `kladde-types` logic without touching a file.
- A recording/spy `Backend`, for asserting exactly which microoperations a mutation produces.
- A batching `Backend` that buffers ops for an explicit `commit()` instead of persisting on every call, as an opt-in.
- Eventually, a replicated/networked `Backend` — kept possible, not committed to (see [Concurrency](#concurrency)).

## Workspace Layout

- `kladde-traits`: `Persistable`, `Guard`, `Allocator`, `Backend`, `UniquePointer`, `ResolvedPointer` — the shared vocabulary `kladde-alloc` and `kladde-types` both build on. Also the blanket `Persistable` impls for primitives (`i32`, `bool`, `String`, ...): these have to live here, not in `kladde-types` as originally planned, because `impl Persistable for i32` is `impl ForeignTrait for ForeignType` from any other crate's point of view, which the orphan rules forbid — only the crate that defines `Persistable` can implement it for a foreign type like `i32`. `kladde-types` re-exports these names for convenience.
- `kladde-alloc`: the mock, in-memory byte store `kladde`'s `DefaultBackend` replays a flush's microoperations into. Doesn't itself implement `Allocator` — that trait's index-generation half has to happen *eagerly*, at `Guard`-call time (see [Pointers and Memory Management](#pointers-and-memory-management)'s "which instance" reasoning), which only `DefaultBackend` is positioned to do; `kladde-alloc` only ever materializes, reads, or mutates bytes for an index it's told about, via its own inherent methods rather than `Allocator`'s. Depends only on `kladde-traits`. v1 ships only this mock (no real file, no compaction, no crash consistency) — see [Crash Consistency](#crash-consistency). Unlike an earlier draft of this section, `PersistedVec`/`PersistedHashMap` (below) *do* need `UniquePointer` directly, once flushing is implemented against real, journaled microoperations rather than re-snapshotting live state: every container gets its own lazily-created pointer, not just a contiguous in-memory blob.
- `kladde-types`: the built-in backed container types (`PersistedVec`, `PersistedHashMap`, eventually a rope) and their generated guard wrappers. Builds on `kladde-traits`, `kladde-alloc`, `kladde-derive` (for nested data), and `kladde` (solely for the `DefaultBackend` default type argument — a convenience dependency, not a functional one; `kladde-types`'s own logic is written purely against the `B: Backend` bound). Most of the container types are hand-implemented directly against `Persistable`/`Guard`/`UniquePointer` rather than derive-macro output — the same way `std`'s own collections hand-write unsafe raw-pointer manipulation internally; the derive macro targets the common case (application-level structs/enums composed from already-`Persistable` fields), not container internals. A container's *element* type only needs `Persistable` now — no `Serialize`/`Deserialize`/`Clone` requirement at all, since `store`/`load` write and read a fixed-size inline representation directly rather than going through `serde`. `PersistedVec<T>`'s snapshot layout mirrors `Vec<T>`'s in-memory one (an 8-byte header plus a dense array of fixed-size element slots, grown/shrunk via `Allocator::resize`). `PersistedHashMap<K, V>`'s content doesn't need to support key lookup at all (that's what the in-memory reconstruction is for), so it's a similar fixed-size-slot array — but with a 1-byte liveness tag prefixing each `(K, V)` slot, and removal implemented as tombstoning (clear the tag in place) rather than `PersistedVec`'s shift or a `swap_remove`: nothing else's slot ever moves on removal, so there's no "which key is at slot N" reverse lookup needed, which in turn means each key is stored exactly once in memory (a single `HashMap<K, (usize, V)>`, mapping straight to its slot). The trade-off: the on-disk array's length becomes a *capacity* (highest slot ever used, dead or alive) rather than a live count, so sustained insert/remove churn grows it without bound until a real compaction pass exists to reclaim tombstoned slots — acceptable for now since v1 has no such pass yet regardless. Note this changes `PersistedHashMap`'s key bound from what an earlier round settled on (`Hash + Eq + Serialize + DeserializeOwned`, deliberately *not* `Persistable`, since keys aren't mutable in place): keys now need `Persistable` too, for `INLINE_SIZE`/`store`/`load`, to occupy a fixed-size slot alongside their value — but, thanks to storing each key exactly once, not `Clone`. Keys still aren't mutable in place — no `Guard` is ever handed out for one — this only affects how a key's bytes get read/written, not whether it can be mutated.
- `kladde-derive`: the macro that turns a user's `struct`/`enum` into a `Persistable` type, generating the same wrapper/`Deref`/`Drop` machinery `kladde-types` uses internally for its own built-in types. Since `Persistable` no longer declares an `Op` type, the macro no longer generates one either — a struct's guard's field accessors just reborrow into the field's own `Guard`, recursively, with nothing of the struct's own to record. Generated code references `::kladde_traits::...`/`::serde::...` paths directly, so any crate using this macro needs `kladde-traits` and `serde` as *direct* dependencies too, not just transitively via `kladde-types` — the same reason `#[derive(serde::Serialize)]` requires a direct `serde` dependency, not just `serde_derive`.
- `kladde`: opening files, reading headers, running the flushing pipeline, and the concrete `DefaultBackend` implementation of `Allocator`. This is the crate application code actually depends on — there's no separate facade crate, since this is already the application-facing entry point. (An earlier draft called this crate `frontend`; renamed after it also became the crate implementing `Backend`, which made "frontend" read backwards.) v1's `DefaultBackend` journals microoperations into an in-memory `Vec` and delegates the rest to `kladde-alloc`'s mock — no real file, no `open`/`create` taking a `Path`, since v1 has nothing to open; state lives only as long as the process does.

An `example` crate (not part of the published library) demonstrates the whole stack end to end.

The two cross-crate callback boundaries this layout originally seemed to require are both resolved by the design above, rather than needing a runtime callback mechanism:

1. `kladde-alloc` asking a concrete data type to update pointer metadata during compaction — resolved by the index-keyed registry, which lets `Allocator` patch every affected pointer using only byte-range geometry.
2. `kladde-types` getting its mutations into the journal without depending on `kladde` — resolved by `Backend`, which every guard wrapper already holds.

## Concurrency

v1 targets a single process with exclusive write access to a file — no concurrent writers, no shared-memory-mapped readers from other processes — which keeps `kladde-alloc` and the journal free of locking concerns. This is deliberate: get the core architecture right first, but avoid decisions that would be fundamentally incompatible with a concurrent future. Two choices already made lean the right way: single-owner `UniquePointer`s (rather than freely-aliased pointers) map more naturally onto typical single-writer/multiple-reader schemes (e.g. MVCC-style snapshot isolation) than aliasing would have, and `Backend` being swappable leaves room for a replicated/networked implementation later without touching `kladde-types`.

## Future Work (v2+)

Deliberately deferred, to be revisited once the v1 architecture is validated:

- **Concurrency** — see above.
- **Generation counters on pointers**, to catch use-after-free / stale-handle bugs, at the cost of extra bytes per pointer.
- **Resumable, ARIES-style flush recovery** (fine-grained redo logging of the flush's own writes, instead of shadow-paging's restart-from-scratch) — only worth it if flush cost becomes a real problem.
- **Fine-grained enum mutation** — v1's `#[derive(Persistable)]` enums only support replacing the whole value; mutating a field within the current variant in place, and/or matching directly on a generated `MyEnumGuard<'_, B>`, is deferred.
- **Semantically-aware microop optimization** — dropping type-specific `Op`s (see [The Trait Layer](#the-trait-layer)) means the flush pipeline's optimization pass is purely mechanical (matched `Alloc`/`Free`, superseded `Write`s) and can never merge or cancel operations that are only equivalent at the *meaning* level rather than the byte level — a rope collapsing a run of edits that cancel out semantically, even though their underlying bytes differ, is the kind of optimization this design deliberately gives up in exchange for never needing per-type replay logic or a type registry for dispatch. Revisit only if a concrete case shows the mechanical optimizer leaving significant, recoverable savings on the table.

## Prior Work

This design overlaps with several existing systems, though none combine all of its constraints at once: native Rust types (not a fixed generic document model), no translation to database commands, no full-object/full-document reserialization on each mutation, and read speed close to the non-backed equivalent.

### Tier 1: transparent/"orthogonal" persistence for ordinary objects

These give you ordinary-looking mutable objects with automatic, no-SQL persistence, but generally at *per-object* (or per-page) granularity rather than per-mutation-operation granularity — a single field write still tends to trigger rewriting some encompassing structure (a whole pickled object in ZODB, a chain of B+-tree nodes in Realm), not just one small logged op.

- **ZODB** (Python) — `Persistent` subclasses track dirty attributes via `__setattr__`; only dirty objects get re-pickled on commit. Its `FileStorage` backend is an append-only transaction log with periodic `pack()` compaction — structurally close to our journal + flush.
- **GemStone/S** (Smalltalk) — the deepest historical precedent for our pointer design: its *object table* maps object IDs to physical page locations, so compaction only ever touches that one indirection layer, closely matching our index-keyed `kladde-alloc` registry.
- **Realm** (Swift/Kotlin/JS) — "live objects" with native property syntax over an MVCC, copy-on-write storage engine; no SQL, fine page-level granularity. See detailed comparison below.
- **db4o** / **ObjectDB** (Java/.NET) — bytecode enhancement intercepts field writes on plain objects; no SQL, no ORM-to-relational translation.

### Tier 2: fine-grained, per-operation journaling

Closer to our actual granularity — the CRDT / local-first world — despite solving a different problem (multi-writer merge, not single-writer durability):

- **Automerge** (`automerge-rs`) — ordinary-looking maps/lists/text; every change is captured as a small op, persisted as an append-only, compact (columnar-encoded) change log with periodic compaction into a snapshot. See detailed comparison below.
- **Yjs** — same idea, JS ecosystem.

### Academic lineage

The general idea has a name: **orthogonal persistence** — persistence that's automatic and independent of an object's type or size, with no explicit save step. It originates with **PS-algol** and **Napier88** (Glasgow/St Andrews, 1980s); Sun's **PJama** was the closest attempt to bring it to a mainstream language, via incremental, differential persistence of a Java object heap. None of these are things to depend on today, but they're the right search terms for how far this idea has historically been pushed, and what tends to go wrong at scale.

One observation ties this lineage back to our own design: ZODB, Realm, and GemStone all make persistence syntactically invisible (`self.foo = bar` just works) because Python/Swift/Smalltalk each have a hook — `__setattr__`, property wrappers + KVO, message dispatch — that intercepts an ordinary-looking mutation. Rust has no such hook, which is exactly why our design needs the more visible `Guard` wrapper and an explicit `.guard()` call: we're compensating for a language capability those systems quietly rely on and Rust doesn't have.

### Realm, in more detail

**Overlap:** both maintain a canonical, cheap-to-read representation, use copy-on-write so writes never corrupt live data mid-commit, and hide persistence behind generated wrapper types with native-feeling mutation syntax (Realm's codegen'd `@Persisted` properties ~ our derive-generated `Guard` wrappers).

**Where it diverges:** Realm has no representation separate from the file — its "live objects" are thin accessors directly over the mmap'd, columnar on-disk layout; reads go through that layout (cheap, page-cached, zero-copy, but still not "a plain `HashMap` sitting in your process"). We deliberately went the other way: keep a fully separate, natively-typed in-memory structure that's independently as fast as the non-backed equivalent, and let the file lag behind via the journal. This also means Realm writes cost more per-mutation in principle — a single property write still triggers copy-on-write up a B+-tree-like path to the root (O(log n) node copies), where our journal append is O(1) until the next flush. Realm also does real multi-thread/multi-process concurrency with auto-updating objects (explicitly out of scope for our v1 — see [Concurrency](#concurrency)), and is schema/table-oriented (a fixed set of property types, links, lists-of-X) rather than arbitrary recursively-nested user-defined Rust types. It's also a full product (query language, hosted sync service, SDKs across many languages) — a different weight class from a building-block library.

### Automerge, in more detail

**Overlap is specific and real:** `save_incremental()` appends a compact chunk of new ops; `save()`/compaction folds everything into one dense base representation; loading replays that back into a queryable structure — essentially our journal+flush pattern, already implemented. Its columnar op-encoding (splitting ops into separate compressed columns rather than serializing each op as a struct) is more sophisticated than our plain serde-per-op journal and worth studying for our own format.

**Where it diverges — driven by the problem it solves (multi-writer merge), not incidental:**

- **Retention.** CRDT lists need tombstones for deleted elements (a concurrent insert might still reference a deleted position), and enough causal history survives to resolve concurrent conflicting writes deterministically. We never need this — a delete is final and its `UniquePointer` is immediately, unconditionally freeable.
- **Value representation.** Every Automerge value can, in principle, need conflict resolution, so reading "the current value" is resolving something, not a raw field access. Our read-speed goal is only reachable because we don't have to support that.
- **Data model.** Automerge's document is a fixed generic tree of Map/List/Text/scalar; getting your own Rust types in and out goes through a separate reflection/reconciliation layer (`autosurgeon`). Our derive macro makes *your* type the backed type directly, with real methods and arbitrary nesting.
- **Scope of the storage layer.** Automerge's persistence is bespoke to its own op log, not a reusable file-backed heap — there's no analog to our `kladde-alloc` crate as a general-purpose, relocatable-pointer foundation other backed structures could be built on.

**What our design provides that Automerge doesn't**, as a direct consequence of not needing to support merge:

1. Native persistence of arbitrary Rust types, with real derive-generated methods and arbitrary nesting — not a fixed document model plus a reflection layer.
2. No CRDT retention tax — deletions are final and immediately reclaim storage.
3. Read performance that can genuinely match native collections, unconditionally.
4. A reusable, general-purpose persistent-heap abstraction (`kladde-alloc` + `UniquePointer`) as a foundation for building new kinds of backed structures.
5. Mutation ergonomics closer to plain `std::collections` — direct `.push()`/`.insert()`/`.remove()` through a `Guard` view, rather than operations scoped inside a transaction against a generic document tree.

None of this makes Automerge worse — every trade-off above is the direct cost of solving a harder problem (multi-writer merge) that this project explicitly scopes out for now (see [Concurrency](#concurrency) and [Future Work](#future-work-v2)). The reverse trade holds too: the moment concurrency/sync becomes a real requirement, Automerge's model is already there, and ours would have to grow substantially to get anywhere close.

## Why the Name "Kladde"?

*Kladde* is German for a merchant's rough day-book: transactions scribbled down messily, in order, as they happened, later transcribed into the clean *Hauptbuch* ("main ledger").
This project's on-disk format follows the same two-part shape — a chaotic, continuously-appended journal, periodically compiled into a clean snapshot (see [Flushing](#flushing)) — so the name doubles as a description of the file format itself, not just the library. It's also, fittingly, the name of the one crate application code actually depends on (see [Workspace Layout](#workspace-layout)).

## Open Questions

- **Derive macro coverage for generic types and where-clauses.** The guard-generation pattern is sketched above for plain, non-generic structs (the `Point` example). Enum support is scoped for v1 (whole-value replacement only — see [Future Work](#future-work-v2) for the deferred fine-grained version), but generic types and types with where-clauses still need to be worked out.
