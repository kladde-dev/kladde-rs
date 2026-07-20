# Questions Before Implementing Flushing (v1, still on the mock `Allocator`)

Same pattern as `V1_QUESTIONS.md`: please answer inline, commit, and I'll implement without further check-ins, making my own documented calls on anything left unanswered. Recommendations are marked; most of these you can probably just confirm.

## 0. The scoping question everything else depends on

Right now the journal is genuinely decorative: `Journal::record` serializes an `Op` and appends the bytes to a flat `Vec<Vec<u8>>` (`DefaultBackend`, in `crates/kladde/src/lib.rs`) with no type tag, and nothing ever reads it back. `PersistedVec`/`PersistedHashMap` hold their live state directly in an ordinary `std::vec::Vec`/`std::collections::HashMap` — that state is *already* current, it was never derived from the journal. There are two genuinely different things "implement flushing" could mean:

- **(a) Re-snapshot.** A flush walks the *current live state* (not the journal) of whichever `Persistable` values need their own storage, serializes each one, and writes it via `Allocator` (allocating fresh space if it doesn't fit, freeing the old space, updating that value's own pointer). The journal is then cleared. This produces the correct end state, but a flush doesn't actually *interpret* individual `Op`s — it doesn't need to know that `Push` means "append one element" versus "the vec somehow changed"; it just re-serializes the whole current value. Simpler: no type registry, no generic op-dispatch needed.
- **(b) Journal replay.** A flush walks the recorded `Op`s in order and applies each one individually to update the snapshot (matching `spec.md`'s "compile to microoperations" step literally). This is more faithful to the full design and is a real prerequisite for the op-level optimizations `spec.md` describes (matched insert+delete cancelling, etc.) — but it requires solving a problem v1 doesn't touch yet: journal entries currently have no type tag, so nothing can know *which* `Persistable` type's `Op` a given entry is, or how to deserialize/apply it generically. That's `spec.md`'s type registry, which isn't built.

**I'd recommend (a) for this round**, and treat (b) — real per-op replay, with a type registry — as a separate, later piece of work once (a) has established the pointer/allocation mechanics. Reasonable, since v1 has no real crash scenario forcing "reconstruct purely from the journal" to matter yet; that's specifically what (b) would buy you, and it's expensive to build correctly. Flag if you want to go straight for (b).

No, go with option (b): journal replay. Start with a simple journal replay that just iterates over all `Op`s and executes them (akin to an interpreter for a scripting language). In subsequent commits, implement the optimization steps mentioned in `spec.md`. I also think this means the `Flushable` trait you recommended in the chat is probably not needed, but I might be mistaken here. If you still find a use for `Flushable`, feel free to declare it.

## 1. Which types get their own allocation/pointer, and which are inline?

Not every `Persistable` value should need its own `UniquePointer` — a scalar (`i32`) or a plain derived struct (`Op = ()`, embedded by value in whatever contains it) clearly shouldn't. The natural line: only *container* types (`PersistedVec`, `PersistedHashMap`, and presumably any future type with the same "can grow independently, benefits from being relocatable" shape) get their own allocation; everything nested inside one is serialized inline as part of that container's single blob, no matter how deeply nested.

Agreed. More precisely: similar to the memory layout of containers in rust or C++, containers aren't necessarily just a single pointer. For example, a `PersistedVec` would be stored as a length and a pointer (and possibly a capacity, although that can probably be deduced from the size of the allocation). This might also tie in with the discussion about `Flushable`.

**Follow-on:** does that mean a `PersistedVec<PersistedVec<i32>>` serializes as *one* blob containing everything, with the inner vecs *not* independently relocatable? That's the simple version, and I'd recommend it for now — independently-relocatable nested containers is real complexity (each one needs its own pointer, its own entry in the allocator's registry, updated on its own schedule) that doesn't seem justified before there's a concrete need for it. **Agree, or do nested containers need independent allocations from the start?**

No, see our discussion in the chat. In summary: in principle, every data type is stored inline and has a fixed size. However, for some data types (mostly variably-sized collections), the inline stored part is mostly a small fixed number of pointers and maybe some small amount of meta data, and the bulk of the payload sits in a separate memory allocation pointed to by the pointers. All pointers in the snapshot region are "owning", i.e., the memory allocation they point to semantically "belongs to" the data type that holds the pointer (i.e., that data type is in charge of freeing the memory allocation). Whether a data type has pointers to other memory allocations or not depends only on the data type and not whether the _instance_ sits at the top level of the persisted data structure or somewhere nested inside another variable-sized container.

## 2. Does this need a new trait, or does it belong on `Persistable`?

Given (1), only some `Persistable` types need "serialize myself into `Allocator`-backed storage" / "reconstruct myself from a pointer." Putting that capability directly on `Persistable` would force every scalar and every derived struct to implement it trivially (or not at all, which is worse — an unused trait requirement). **I'd add a separate trait** (something like `Allocated`/`Flushable`, exact name TBD) that only `PersistedVec`/`PersistedHashMap` (and the root, via `Kladde`) implement, roughly:

```rust
trait Flushable: Persistable {
    fn flush<A: Allocator>(&self, pointer: &mut Option<UniquePointer<Self>>, allocator: &A);
    fn load<A: Allocator>(pointer: &UniquePointer<Self>, allocator: &A) -> Self;
}
```

**Does this shape look right, or would you rather fold flush/load into `Persistable` itself (with a default no-op impl for types that don't need it) or into `Allocator`?**

## 3. What does "doesn't fit, needs to move" actually do, mechanically?

Given (0)(a): on flush, serialize the current value; if there's no existing allocation yet, `alloc` one. If there is one but the new serialized size doesn't fit, `alloc` a new region, write into it, `free` the old one, update the pointer -- no in-place grow, matching how `Allocator` is already shaped (`alloc`/`free`/`resolve`, no `realloc`). **I'd add a small amount of slack on allocation (e.g. round up, or allocate ~2x like `std::Vec` does) so a run of pushes doesn't reallocate on every single flush** -- worth it, or keep it exact-size-only for now and revisit once it's clear whether flush frequency makes this matter?

## 4. How is a flush triggered?

`spec.md` describes an automatic threshold ("when the journal exceeds a size threshold"). For v1, given there's no real durability pressure yet (nothing crashes, nothing reopens), **I'd implement only an explicit `Kladde::flush(&mut self)`** and defer auto-flush-on-threshold as a thin wrapper to add later once flush exists. Agree?

## 5. Root value handling

The root (`Kladde<T>`'s `root: T`) needs to end up allocated too, for the round-trip test in (0) to mean anything (something has to identify "where is the root's blob" so a fresh reload can find it). I'd give `Kladde<T>` a `root_pointer: Option<UniquePointer<T>>` field alongside `root`, populated on first flush, requiring `T: Flushable`. **Does the root itself need to be a "container" type in the sense of (1) (i.e., must the app's root type be a `PersistedVec`/`PersistedHashMap`/similar, not an arbitrary `#[derive(Persistable)]` struct)?** I'd guess yes for now, since a plain derived struct doesn't have anywhere to serialize itself independently under this design -- meaning `AppState` in the example (a plain derived struct wrapping a `PersistedHashMap`) would need to become `Flushable` itself somehow, or the root requirement needs `#[derive(Persistable)]` structs to also be flushable as a whole. Worth deciding explicitly since it affects whether `#[derive(Persistable)]` needs to change at all for this feature.

## 6. Op-log optimization and compaction (`spec.md` pipeline steps 2, 4, 6)

Under (0)(a), there's no per-op interpretation, so "optimize matched insert+delete pairs" doesn't apply the way `spec.md` describes it -- re-snapshotting the current state is already maximally "optimized" (it never does wasted work reflecting a value that was later overwritten). **Compaction** (reclaiming fragmentation across many allocations) doesn't obviously mean anything for the mock either -- each allocation is its own independent `Box<[u8]>`, not a contiguous file region, so there's no fragmentation to reclaim. **I'd treat both as out of scope until there's a real, file-backed `Allocator`** where they actually matter. Agree?

## 7. What happens to the journal itself after a flush?

Straightforward given (0)(a): clear it (`DefaultBackend` needs something like `clear_journal(&self)`, or `flush` just drains it). Since nothing replays it, its post-flush role is purely informational (e.g. "how many ops since last flush" for a future auto-flush trigger). **Anything you want the journal to keep doing after a flush, or is drain-and-discard right?**

---

## Assumptions I'll proceed with unless you say otherwise

- The round-trip test from (0) is the primary/required test for this feature; I'll add smaller unit tests per type alongside it.
- `MockAllocator` doesn't need new methods beyond `alloc`/`free`/`resolve` -- the "grow" logic (alloc new, copy, free old) lives in the `Flushable` impls, not in `Allocator` itself.
- `PersistedHashMap` gets the same treatment as `PersistedVec` (own pointer, own `flush`/`load`), same round-trip test shape.
- No change to `Journal`'s signature or `DefaultBackend`'s per-op storage format (still untyped bytes) under scoping choice (a) -- that only becomes necessary if we do (b) later.
