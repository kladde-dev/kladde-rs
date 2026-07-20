# Questions Before Implementing Flushing (v1, still on the mock `Allocator`)

Same pattern as `V1_QUESTIONS.md`: please answer inline, commit, and I'll implement without further check-ins, making my own documented calls on anything left unanswered. Recommendations are marked; most of these you can probably just confirm.

## 0. The scoping question everything else depends on

Right now the journal is genuinely decorative: `Journal::record` serializes an `Op` and appends the bytes to a flat `Vec<Vec<u8>>` (`DefaultBackend`, in `crates/kladde/src/lib.rs`) with no type tag, and nothing ever reads it back. `PersistedVec`/`PersistedHashMap` hold their live state directly in an ordinary `std::vec::Vec`/`std::collections::HashMap` — that state is *already* current, it was never derived from the journal. There are two genuinely different things "implement flushing" could mean:

- **(a) Re-snapshot.** A flush walks the *current live state* (not the journal) of whichever `Persistable` values need their own storage, serializes each one, and writes it via `Allocator` (allocating fresh space if it doesn't fit, freeing the old space, updating that value's own pointer). The journal is then cleared. This produces the correct end state, but a flush doesn't actually *interpret* individual `Op`s — it doesn't need to know that `Push` means "append one element" versus "the vec somehow changed"; it just re-serializes the whole current value. Simpler: no type registry, no generic op-dispatch needed.
- **(b) Journal replay.** A flush walks the recorded `Op`s in order and applies each one individually to update the snapshot (matching `spec.md`'s "compile to microoperations" step literally). This is more faithful to the full design and is a real prerequisite for the op-level optimizations `spec.md` describes (matched insert+delete cancelling, etc.) — but it requires solving a problem v1 doesn't touch yet: journal entries currently have no type tag, so nothing can know *which* `Persistable` type's `Op` a given entry is, or how to deserialize/apply it generically. That's `spec.md`'s type registry, which isn't built.

**I'd recommend (a) for this round**, and treat (b) — real per-op replay, with a type registry — as a separate, later piece of work once (a) has established the pointer/allocation mechanics. Reasonable, since v1 has no real crash scenario forcing "reconstruct purely from the journal" to matter yet; that's specifically what (b) would buy you, and it's expensive to build correctly. Flag if you want to go straight for (b).

No, go with option (b): journal replay. Start with a simple journal replay that just iterates over all `Op`s and executes them (akin to an interpreter for a scripting language). In subsequent commits, implement the optimization steps mentioned in `spec.md`. I also think this means the `Flushable` trait you recommended in the chat is probably not needed, but I might be mistaken here. If you still find a use for `Flushable`, feel free to declare it.

**Follow-up (round 2):** I think you're right that a separate `Flushable` trait isn't needed, and I'd go one step further with the mechanism — folded into the new Question 2 below. Rather than storing serialized bytes that some later interpreter looks a type tag up for, `Journal::record` could capture a small closure at the call site, where the concrete type is still fully known to the compiler. That closure *is* the "compiled program" your interpreter analogy describes, so there's nothing left for a `Flushable` trait to dispatch through. It doesn't fully replace what I was reaching for with `Flushable`, though — Question 2 is really about a different problem (which *instance* an entry targets) that exists no matter how an entry carries its "what to do."

## 1. Which types get their own allocation/pointer, and which are inline?

Not every `Persistable` value should need its own `UniquePointer` — a scalar (`i32`) or a plain derived struct (`Op = ()`, embedded by value in whatever contains it) clearly shouldn't. The natural line: only *container* types (`PersistedVec`, `PersistedHashMap`, and presumably any future type with the same "can grow independently, benefits from being relocatable" shape) get their own allocation; everything nested inside one is serialized inline as part of that container's single blob, no matter how deeply nested.

Agreed. More precisely: similar to the memory layout of containers in rust or C++, containers aren't necessarily just a single pointer. For example, a `PersistedVec` would be stored as a length and a pointer (and possibly a capacity, although that can probably be deduced from the size of the allocation). This might also tie in with the discussion about `Flushable`.

**Follow-on:** does that mean a `PersistedVec<PersistedVec<i32>>` serializes as *one* blob containing everything, with the inner vecs *not* independently relocatable? That's the simple version, and I'd recommend it for now — independently-relocatable nested containers is real complexity (each one needs its own pointer, its own entry in the allocator's registry, updated on its own schedule) that doesn't seem justified before there's a concrete need for it. **Agree, or do nested containers need independent allocations from the start?**

No, see our discussion in the chat. In summary: in principle, every data type is stored inline and has a fixed size. However, for some data types (mostly variably-sized collections), the inline stored part is mostly a small fixed number of pointers and maybe some small amount of meta data, and the bulk of the payload sits in a separate memory allocation pointed to by the pointers. All pointers in the snapshot region are "owning", i.e., the memory allocation they point to semantically "belongs to" the data type that holds the pointer (i.e., that data type is in charge of freeing the memory allocation). Whether a data type has pointers to other memory allocations or not depends only on the data type and not whether the _instance_ sits at the top level of the persisted data structure or somewhere nested inside another variable-sized container.

**Follow-up (round 2):** agreed, and this is a cleaner statement of where I ended up too — "has pointers to separate allocations" is a fixed, structural fact about a *type* (does its inline representation include a pointer field at all), never a fact about where a particular instance happens to sit. Nothing further open here; I'll treat this as settled and build the derive/container logic so a type's own shape (not its call site) decides whether it owns any allocations.

## 2. How does a journal entry know what to run, and which instance to run it against?

This is the real gap opened up by choosing (b), and I think it's the crux decision for this round, the way (0) was last round — everything past this point assumes some answer to it.

**What to run.** Per your follow-up on (0): rather than `Journal::record` serializing bytes that a later interpreter has to look up a type for, it can capture a closure at the call site — `self.journal.push(Box::new(move |allocator: &A| { /* apply this specific op to this specific target */ }))` roughly. The concrete type is fully known to the compiler right there, at record time, so the closure already *is* the specialized code a `Flushable::apply` method would otherwise have needed to be looked up and dispatched to. This is why I agree no separate trait is needed for dispatch — the closure supplies its own "how," so there's nothing left to name a trait method after.

No. Stick with `Journal::record` and separate the serialization into two parts:

- the `Journal` itself serializes a brief header that identifies the type (via its index into the list of type hashes mentioned in `spec.md`);
- either the type or the `Op` serializes the rest (for now, use `serde` for this, but later we might want to serialize ourselves into a representation that can immediately be reused -- either copied byte for byte or even declared as a memory allocation; see `later.md`).
- Before letting the `Op` serialize itself, the `Journal` might have to ensure that there's enough room left in the current journal (if it's not at the end of the file). For this, we probably need to introduce an `Op` trait with a trait method `serialized_len(&self) -> u32`.

There's are two minor open questions:

- Most `Op`s will probably be enum types. We might want to enforce this and even require them to implement `EnumTag` from the `enum-tag` crate or something similar. Then, the journal itself would serialize not only the type ide but also the enum tag.
- One advantage of this could be that it would allow the `Journal` to know the serialized size of all `Op`s upon replay, if this is necessary: we could modify the signature of `serialized_len` such that it returns a tuple `(len: u32, dynamic: bool)` where `dynamic` tells us whether the obtaining the `len` required inspecting the contents of the `Op` (not just the enum variant). If `dynamic` is `true`, upon serialization, the `Journal` itself would write the size as part of the header. Upon deserialization, the `Journal` calls a method `Op::size(variant: <Op as EnumTag>::Tag) -> Option<u32>)` which returns `Some(len)` if `serialized_len` on the deserialized value would return `(len, true)`, and `None` if `serialized_len` would return `(_, false)`. If `None` was returned, then the `Journal` knows that this `Op` variant has a dynamic serialized size, which it can read from the header. If `Some(len)` is returned, then the `Journal` knows that the header does not contain the length (which is already known at this point).
- But I'm not sure yet if `Op` deserialization (journal replay) will even need to know the size of serialized `Op`s. Maybe we can simply allow the `Op` to read as many bytes as it needs and let it figure out the length itself.

**Which instance.** This is the part a closure doesn't solve for free, and it's the part `Flushable` was actually reaching for. A closure captured inside, say, `PersistedVecGuard::push` can close over `&self.inner` (the live Rust reference) — but that's exactly how ordinary mutation already works, and it's not enough for replay: replay needs to know *which allocated region in the snapshot* this instance corresponds to, so the closure can go update that region specifically (per (1), only types with pointers in their inline representation have a region to update at all — so this only applies to those). Concretely: two different `Contact`s inside a `PersistedHashMap` each have their own `phones: PersistedVec<PhoneNumber>` — both are the same *type*, so both need *some* per-instance handle the closure can capture, not just type-level dispatch info.

The pieces I think this needs, regardless of trait-vs-closure: every value of a type that (per (1)) owns pointers needs a **stable in-memory identity from the moment it's first touched**, not just once it's first flushed — because a closure recorded *before* the first flush still needs to close over *something* that will resolve to the right region once a flush eventually runs. Mechanically, that's a `UniquePointer<Self>` (or an as-yet-unresolved reservation of one), lazily created the first time it's needed. It can't be eager at construction, because containers are built without a backend in hand (`PersistedVec::new()` takes no arguments) — so it has to be created lazily, inside whichever `Guard` method first needs it, where `backend: &'s B` is already available. That likely means `PersistedVec`/`PersistedHashMap` grow an interior-mutable `pointer: Cell<Option<UniquePointer<Self>>>`-shaped field they don't have today, and the guard's mutating methods get-or-create it before capturing the closure. This is a real, if small, structural change to both container types — flagging it explicitly rather than just doing it, since it also touches whatever the derive macro would need for a future user-defined container.

I don't like the closure approach. I was picturing the approach as follows:

- I think construction should be regarded as a mutating operation (it turns nothing into something) so, in principle, I wouldn't mind constructors requiring a `Backend`.
- In most cases we still probably don't need a `Backend` during construction because the constructor itself does not need to allocate any space. This is analogous to how `Vec::new` from the rust std library doesn't allocate memory; only when one pushes the first entry does the memory allocation get created.
- I agree that we need a stable in-memory identity for a pointer from the moment it is constructed, even if the actual memory allocation is deferred to when the journal gets flushed. This is why `UniquePointer` in `spec.md` contains an `index`, not the target address. Creating a `UniquePointer` creates an entry in the allocator's table and wraps the `index` to that entry in a `UniquePointer`. The allocator's table maps the `index` to (among maybe other meta data like the size) an `Option<NonZeroU32>`. For pointers whose memory region has been allocated, this has value `Some(target_addres)`; for pointers whose memory region has not yet been allocated, it's `None`.
- Thus, as far as I can tell, the container types never need to worry about whether the memory region for any `UniquePointer` they hold has already been allocated. They just hold the index, and the allocator worries about lazy/deferred allocations.

Does this resolve your concerns or are there any open questions?

**Question for you:** does the closure-capturing-identity approach above sound right, or would you rather keep entries as plain serialized bytes plus an explicit (small, in-process — not `spec.md`'s persisted file registry, which is a separate, later concern) type tag, with a lookup table doing the dispatch a closure would otherwise do inline? The closure version is less code and matches "interpreter" more literally (the closure *is* the compiled instruction), but it means journal entries are never actually bytes for as long as we're on the mock — which is probably fine now, but worth confirming you're OK with that being true of this whole round, not just a temporary implementation detail.

I don't quite follow. Does my above explanation resolve this or are there open questions?

## 3. What does "doesn't fit, needs to move" actually do, mechanically?

On replaying a structural op (e.g. `Push`): serialize the new element/entry; if there's no existing allocation yet, `alloc` one. If there is one but the new content doesn't fit in the existing region, `alloc` a new region, write into it, `free` the old one, update the pointer -- no in-place grow, matching how `Allocator` is already shaped (`alloc`/`free`/`resolve`, no `realloc`). **I'd add a small amount of slack on allocation (e.g. round up, or allocate ~2x like `std::Vec` does) so a run of pushes doesn't reallocate on every single flush** -- worth it, or keep it exact-size-only for now and revisit once it's clear whether flush frequency makes this matter?

Yes, implement the snapshot representation of `PersistedVec` analogous to how `Vec` from the rust std library is implemented in memory for now, with two additional comments:

- Add a trait method `resize(&mut UniquPointer, new_size)` on `Allocator`, which records an `Op` that changes an existing allocation to a new size. When this operation is replayed and the allocator realizes that the new allocation still fits at the same position, then it just reuses the existing allocation with an adjusted size. If it doesn't, the allocator finds a new memory region, copies out `min(new_size, old_size)` bytes, frees the old region, and updates where the provided pointer points in its internal table.
- After testing this version of `PersistedVec` and committing it to git, use the more appropriate memory layout described in `later.md`.

## 4. How is a flush triggered?

`spec.md` describes an automatic threshold ("when the journal exceeds a size threshold"). For v1, given there's no real durability pressure yet (nothing crashes, nothing reopens), **I'd implement only an explicit `Kladde::flush(&mut self)`** and defer auto-flush-on-threshold as a thin wrapper to add later once flush exists. Agree?

Yes.

## 5. Root value handling

The root (`Kladde<T>`'s `root: T`) needs to end up allocated too, for the round-trip test in (0) to mean anything (something has to identify "where is the root's blob" so a fresh reload can find it). Unlike other containers, the root doesn't have the "constructed without a backend" problem from Question 2 -- `Kladde::new` already brings `root` and `backend` together in one place -- but I'd still give it a `root_pointer: Option<UniquePointer<T>>` field for symmetry with everything else, populated lazily the same way. **Does the root itself need to be a type that owns pointers in the sense of (1) (i.e., must the app's root type be a `PersistedVec`/`PersistedHashMap`/similar, not an arbitrary `#[derive(Persistable)]` struct)?** I'd guess yes for now, since a plain derived struct has no allocation of its own to be found by a pointer -- meaning `AppState` in the example (a plain derived struct wrapping a `PersistedHashMap`) would need its own allocation-owning wrapper, or `#[derive(Persistable)]` structs need to gain that capability generally. This is also where a trait bound (if any survives Question 2's closure discussion) would actually earn its keep: `Kladde<T>::flush`'s generic `T` needs *some* bound to know it's a legal root type at all, even if nothing else in the design ends up needing a named trait.

This question might be rendered obsolete by now. Let me know if it's still relevant, otherwise delete it.

## 6. Op-log optimization and compaction (`spec.md` pipeline steps 2, 4, 6)

Under (b), this now genuinely applies -- a run of `Push`/`Remove` against the same instance really is a sequence of real operations that could be optimized (matched push+remove cancelling, etc.), matching `spec.md`'s pipeline steps 2 and 4 literally for the first time (unlike under option (a) last round, where re-snapshotting was already maximally "optimized" by construction). Per your answer to (0), **I'm reading this as explicitly in scope eventually, but deferred to a follow-up commit** -- this round implements naive in-order replay, with optimization as a separate later piece of work. Confirming that reading before I build anything that assumes it. **Compaction** (step 6, reclaiming fragmentation across many allocations) still doesn't obviously mean anything for the mock -- each allocation is its own independent `Box<[u8]>`, not a contiguous file region, so there's no fragmentation to reclaim. **I'd still treat compaction specifically as out of scope until there's a real, file-backed `Allocator`.** Agree?

Yes.

## 7. What happens to the journal itself after a flush?

Still: clear it (`DefaultBackend` needs something like `clear_journal(&self)`, or `flush` just drains it) once every entry has run. **Anything you want the journal to keep doing after a flush, or is drain-and-discard right?**

Drain and discard is right. Eventually, we'll want to implement a `full_compaction` method that application code can call when the user wants to explicitly export the file. This method would flush the journal, compact the memory, and then actually delete everything that has been freed, including the journal (i.e., actually shorten the file). Add this to `later.md`.

---

## Assumptions I'll proceed with unless you say otherwise

- The round-trip test from (0) is the primary/required test for this feature; I'll add smaller unit tests per type alongside it, plus a test asserting a run of structural ops is actually replayed incrementally (not silently reduced to a single re-snapshot) -- otherwise a naive implementation could accidentally collapse back into option (a) without the test suite noticing.
- `PersistedHashMap` gets the same treatment as `PersistedVec` (own lazily-created pointer, own replay logic), same round-trip test shape.
- ~~No change to `Journal`'s signature or `DefaultBackend`'s per-op storage format (still untyped bytes)~~ -- superseded now that we're doing (b). Pending Question 2's answer, `DefaultBackend`'s journal most likely stops being `Vec<Vec<u8>>` and becomes something like `Vec<Box<dyn FnOnce(&MockAllocator)>>`, or gains an explicit type tag alongside the bytes -- exact shape depends on which way Question 2 goes.
- ~~`MockAllocator` doesn't need new methods beyond `alloc`/`free`/`resolve`~~ -- superseded. Actually applying a replayed op means writing real content into an allocated region and reading it back for `load`/the round-trip test; `Allocator` needs read/write-bytes primitives it doesn't have yet (`alloc` currently only zero-fills, nothing ever writes real content into what it returns). I'll add these unless you'd rather see the exact shape first.
