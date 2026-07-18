# Auto-Saved Data Structures in Rust

This repository is a rust workspace implementing a rust library "backed" data structures, i.e., data structures that are automatically persisted to a file with every mutation.
The library implements backed variants of common types such as vectors, hash maps, and ropes, and it also allows rust application developers to `#[derive(Backed)]` for their own data structures.
Any backed data structure can be "opened" from a file, which loads the data structure into memory but also keeps a connection to the file.
Any modification to the data structure (or to one of its nested members) mutates the representation in memory as usual, and is also durably appended to the file's journal immediately, so that no committed mutation is ever lost even if the process crashes right after the call returns.
Note that "immediately persisted" means "immediately appended to the journal", not "immediately reflected in the compact snapshot representation" — the latter only happens periodically, when the journal is flushed (see below).

Read acces only goes to the in-memory representation (it does not touch the backing file).
The in-memory representations of the provided data types are optimized for reading speed similar to corresponding normal (non-backed) data types, but they typically hold some meta information to keep track of where data lies in the backed representation.
Write access affects both the in-memory and the backed representation in the file.
The backed representation is updated using an efficient combination of journalling and dynamic memory management that minimizes random access writes while still persisting any changes instantly and keeping the representation compact in the long run.


## In-Memory Representation

The in-memory representation of data types contains at least a part that is similar to the in-memory representation of corresponding normal (non-backed) data types.
In addition, the in-memory representation typically contains some pointers or other meta data that helps keeping track of how things are layed out in the backing file (so that the data types can translate data-type level operations to microoperations, see [sketched pipeline below](#flushing-the-journal)).

Further, complex container data types need to hold some metadata in memory that tracks changes since the last journal flush, so that flushing can find and update the right locations in the snapshot section without re-deriving that information from scratch.

**Design decision:** the primary in-memory representation (e.g., a regular hash map, plus pointer meta data) is always fully up to date and is the *only* thing read operations ever touch — a lookup never has to consult anything beyond this primary structure, so read performance stays close to that of the non-backed equivalent, and iteration stays straightforward.
Alongside it, each container keeps a small amount of side metadata — e.g., for a hash map, a "graveyard" recording keys that have been removed or overwritten since the last flush, together with their old snapshot locations (so that flushing can free/reuse that memory).
This graveyard is consulted only while flushing, never during normal reads or writes, so it does not sit on the hot path.
(Note: this whole graveyard mechanism turns out to be unnecessary once `OwnedPointer`s are adopted — see the "nested backed types" discussion under [Open Questions](#open-questions).)

**Open questions:** this might need more thought.
Consider, e.g., a situation where since the last journal flush, key "A" was inserted into a hash map and then deleted.
Should the key end up in the graveyard?
The snapshot section of the backing file doesn't contain key "A", but the journal section does, so maybe it works out?
Now a different situation: consider key "A" already exists in the snapshot and now it is deleted, reinserted, and deleted again.
Should it end up in the graveyard twice?
The two insertions are likely at different memory locations, so we do have to delete both of them unless optimization cancels out the first deletion and re-insertion.
This can probably all be resolved but I feel like my way of thinking about this is too ad-hoc to be confident that I catched every corner case.
Is there a more structured way to think about this?
Maybe from the literature on CRDTs?

**Claude:** CRDTs solve a different problem than this one — they're about merging concurrent edits from multiple uncoordinated replicas without a central arbiter. Here there's a single writer producing one strictly ordered journal, so the more relevant literature is single-writer embedded-database design (write-ahead logging, buffer-pool "dirty page" tracking, shadow paging — see the crash-consistency answer near the bottom for a concrete example, LMDB).

The structured way to think about the graveyard: don't treat it as "one entry per delete op." Treat it as an idempotent, lazily-populated set keyed by *snapshot pointer*: the first time a key's live value diverges from its snapshot-backed copy (by delete or by overwrite), move that key's old snapshot pointer into the graveyard exactly once, and mark it as accounted for so nothing later re-adds it. With that rule:
- Case 1 (insert then delete, never flushed): the key never had a snapshot pointer, so nothing is ever added to the graveyard — the insert+delete pair simply cancels during the journal's own optimization pass (step 1→2 in the [flushing pipeline](#flushing-the-journal)) before it ever reaches the graveyard.
- Case 2 (existing snapshot entry, deleted → reinserted → deleted again): the *first* delete moves the pre-existing snapshot pointer into the graveyard once. The reinsertion creates a brand-new, not-yet-flushed value with no snapshot pointer of its own, so the second delete falls into case 1 and cancels for free. Net effect: exactly one graveyard entry, no special-casing required.

So the annihilation you were hoping for ("unless optimization cancels out the first deletion and re-insertion") isn't something to implement separately — it falls out automatically from the add-once rule plus the pipeline's existing op-cancellation step. (There's an even stronger resolution once `OwnedPointer` is adopted — see the reply under "nested backed types" below, where the graveyard turns out to be unnecessary altogether.)


## Backed Representation

The backed representation in the file consists of a collection of dynamically allocated memory regions, similar to the heap in main memory.
A (not necessarily contiguous) subset of these memory regions forms the so-called _journal section_.
The rest of the memory is referred to as the _snapshot section_.

- **The snapshot section** holds a typically recent but generally not fully up-to-date state of the backed data structures.
  The data structures in the snapshot section are stored in a binary form that is reminiscent to the in-memory representation of the data structure (albeit with different pointer representations to enable garbage collection and compactification).
  Thus, complex data structures like nested hash maps or ropes are typically distributed over not-necessarily contiguous memory regions in the file.
- **The journal section** is a linked list of allocated memory regions that holds a sequence of binary serialized operations.
  Every time the application performs a mutation on a data structure it appends an operation to the journal.
  Each operation consists of an id for the data type (see [below](#extensibility-and-limited-self-descriptiveness)), an op code for a high-level operation to perform on that data type, and possibly a payload (depending on op-code).
  The journal gets flushed (applied to the snapshot section) when it becomes too long, see next section.


## Flushing the Journal

Every once in a while (e.g, when the journal size exceeds a threshold) the journal gets "flushed", i.e., the operations get applied to the snapshot section and the journal is discarded.
These writes typically require memory management, such as reserving or freeing memory regions in the backed representation, which the framework implements by a data-type agnostic memory managment unit that is akin to a memory allocator but operates on a file.
Flushing is typically accompanied with compactification, i.e., moving existing memory allocations around or shortening them to the actually required length.

Flushing is performed in a multi-step process designed to minimize unnecessary random access to the file.
The precise process not yet clear, but it could look something like this.

1. Operations in the journal (data-type dependent, e.g., "remove key X from the hash map at pointer Y in the file")
    - ↓ get _optimized_ to (by the data type implementations)
2. optimized operations (e.g., if some entry of a hash map is first modified and then deleted, then remove the obsolete modification operations; or a rope implementation might simplify a sequence of single-character deletions to a single range deletion)
    - ↓ get _compiled_ to (by the data type implementations, possibly calling the framework's memory management unit)
3. microoperations (data-type independent, e.g., "allocate memory of size N / free memory location at pointer Z in the file")
    - ↓ get _optimized_ to (by the framework's memory management unit)
4. optimized microoperations (e.g., if some memory is first modified and then freed, or if it is modified twice, then remove any obsolete initial modification steps)
    - ↓ get _applied_ to the storage section of the file, leading to
5. updated storage section, empty journal
    - ↓ get _compactified_ to
6. compactified updated storage section.

Note that even the transformations in the above pipeline that are performed by the memory management unit (e.g., from step 4 to 5) may require calling back into the data type implementations to update some meta data that helps the implementation keep track of how things are layed out in the backed representation.


### Extensibility and Limited Self-Descriptiveness

The data format in the backing file is not fully self-describing, i.e., the declaration of data types cannot be extracted from looking at a backing file alone.
However, the library can detect whether a given backing file stores data of the types expected by the application.
Each backing file contains a dedicated storage slot for a vector of hashes of used data types that is otherwise stored and managed like any other vector.
This vector is used for two things: (i) to detect files with incompatible data, and (ii) the order in which these hashes are listed in the header defines the indices / IDs used to specify data types in the journal.
This approach makes the format somewhat extensible / backward compatible: a new version of an application can _add_ new data types and still read backing files generated by versions that did not have those extra data types.
And the old version of the software can even still read any parts of backing files generated by the new version that contain only the old data types and detect which parts of the file contain data it cannot understand.
Indices must never be reused: even if a data type is dropped in a later version, its slot in the hash vector stays reserved (e.g., with a tombstone hash) so that IDs already recorded in existing backing files remain valid.


## Workspace Layout

This rust workspace contains the following crates:

- `traits`: fundamental trait definitions, such as `Backed`.
  Meant to be used by libraries that manually implement `Backed` for their types where they cannot be `derived` (such as our own `types` crate below).
- `frontend`: data-type agnostic logic, such as opening files, reading file headers, running the [flushing pipeline](#flushing-the-journal); probably needs a better name.
  Meant to be used by application crates but usually not by library crates that implement their own `Backed` data type.
- `types`: definitions of fundamental backed types (mostly container types such as lists/vectors, hash maps, ropes).
  Builds on `traits` and maybe `alloc` and `derive` (for nested data) but not on `frontend`.
- `derive`: derive macro with which application developers can turn their `struct`s and `enum`s into `Backed` types if they consist of only `Backed` types.
- `alloc`: memory management agnostic to data types (allocating, freeing and moving, ponter types, ...).
  Should not need to use any of the above crates except maybe `traits`.

This layout implies two callback boundaries that `traits` needs to make possible without creating a dependency cycle:

1. `alloc` (which does not depend on `types`) must still be able to ask a concrete data type to update its pointer meta data during compactification. Originally proposed as resolved via a handle-table pointer design (see below) — revised, see the counter-proposal and reply immediately below, which settles on `OwnedPointer`s instead.
   - Actually, I'd like to explore if I can get away withoaut a handle table because I'd rather not have to include additional metadata to in the file that can be derived from the payload itself.
     Wouldn't it suffice if `alloc` defines an `OwnedPointer` type which, internally, is something like `Rc<Cell<(u32, u32)>>` and keeps track of (i) the pointer target in the fiel and (ii) where in the file the (unique, thus "owned") pointer to that position sits.
     These `OwnedPointer`s can be held by types in the `types` crate (or in user-defined crates) but those crates can obtain an `OwnedPointer` only from the `alloc` crate (by allocating memory).
     When an `OwnedPointer` gets serialized to the snapshot section of the file, serialization writes the target address (first `u32` of the tuple) and sets the position (second `u32`) to the current offset in the file.
     The alloc crate would hold on to a clone of all `Rc`s of `OwnedPointers` that it hands out and thus has the full picture of what points from where to where.
     Isn't this similar to how garbage collection and compactification in, e.g., the JVM works or am I missing something?
   - **Claude:** This works, and it's a good instinct — but the closer analogy isn't JVM-style tracing GC, it's classic Mac OS "Handles" (a master-pointer table) combined with the fact that your pointers are *single-owner*, which is what actually makes this tractable. Tracing collectors like the JVM's solve a harder problem: an object can be referenced from arbitrary places in a general graph, so a moving collector has to find all referents via root-scanning plus graph traversal, because there's no cheap way to enumerate "everyone who points at me." Your `OwnedPointer` sidesteps that by construction: because each allocation has exactly one owning pointer, `alloc`'s own per-allocation bookkeeping (which it needs anyway, just to track live regions) can simply *be* that pointer's `Rc<Cell<(target, position)>>` — no separate registry, no graph traversal, no tracing. That's a real simplification over general GC, and over the handle-table proposal it also avoids persisting any extra indirection structure to disk, which was your objection.

     There is one place this still needs care: `position` (where the pointer's own bytes currently sit) has to stay correct not just when the *target* moves, but also when the block *containing* the pointer moves — e.g., if `alloc` compacts an unrelated region and slides a `HashMap`'s bucket block over by a few bytes, something has to walk that block and patch every embedded `OwnedPointer`'s `position`. That "something" has to be type-aware (only the `HashMap` implementation knows which bytes inside its own block are pointer fields), which is exactly the `alloc`→`types` callback this point was trying to avoid. With `OwnedPointer` the callback doesn't disappear, but it only fires when a block *containing* embedded pointers is relocated — and in practice that already tends to happen as part of a type-driven rewrite (the type is already visiting every field to re-serialize it, so patching `position` is free at that point), not as a background operation `alloc` does silently behind a type's back. As long as we commit to "`alloc` never silently relocates a block with embedded pointers without going through its owning type," this stays cheap; generic defragmentation of arbitrary regions independent of type-level operations would need the callback for real.

     - **Response:** counterproposal: `alloc` keeps a list of all live `OwnedPointer`s that stores both the `target` and the `position` of each pointer, and that is sorted by `position` (e.g., a B-tree).
       When alloc moves memory, it does two things: (i) update the (single) pointer that points to the beginning of the memory section and (ii) search the sorted list for all live pointers whose `position` is within the memory area, update their `position`s, and ensure the B-tree stays sorted.
       In total, we should probably reconsider the pointer design:
       - `alloc` keeps indices with the full information: the `target` and `position` of each pointer, a B-tree or otherwise sorted list sorted by `position`, and any additional data required to keep track of allocated and free memory regions and the (indices of) pointers that point to them.
         These lists should only exist in memory, not in the backed file (they are constructed when the file is first opened and then kept up to date with every change).
       - the data type implementations probably don't need to hold both `target` and `position`.
         It suffices if they hold some small opaque data type `OwnedPointer` that can be serialized to the journal and that `alloc` can translate to a `target` and `position`.
         But I'm not sure yet how to do this.

     One trade-off worth flagging given your stated goal of many small, deeply nested types (thus lots of pointers): `Rc<Cell<(u32,u32)>>` is a heap allocation per pointer (refcounts + cell, roughly 24–40 bytes with allocator overhead), whereas a flat handle-table slot would be 4–8 contiguous bytes in an array with no per-pointer heap allocation. That's a real memory-density and cache-locality cost in `alloc`'s bookkeeping for a system meant to have many small pointers — but it stays off-disk either way, which was your main concern, so it seems like a reasonable trade to accept in exchange for not persisting a handle table. Recommendation: go with `OwnedPointer`, store it directly as the value in `alloc`'s block-metadata table rather than a separate registry (single ownership makes them the same thing), and treat "who relocates blocks with embedded pointers" as a rule — only the owning type does it, during its own rewrites — rather than something `alloc` does independently.
2. `types` (which does not depend on `frontend`) must still get its mutations appended to the file's journal. Resolved by having mutating methods on `types` return `Op` values rather than perform I/O themselves; `frontend` (and `derive`-generated glue) is what actually appends returned `Op`s to the journal.
   - Good catch. No, I don't want mutating methods to return `Op` values because I don't want the user to have to remember to persist changes.
     Application authors should not be able (in safe rust code assuming correctly implemented `types`) to mutate an instance of a backed type without automatically persisting it to the file.
     Thus, all types would either have to hold a reference to the overall machinary that persists changes or all their mutating operations should require such a reference as an additional function argument.
     I'm leaning towards the former at least per default.
     Come up with a clean way of organizing this into appropriate traits.
     Either `types` would have to depend on `frontend` (which seems fine because `frontend` shouldn't depend on `types` anyway); or, maybe it's better to be generic over the frontend anyway and define a trait for frontends that a type in `frontend` implements (would this be useful? what other implementations of such a trait are conceivable?).
   - **Claude:** Agreed, and this is the cleaner default. Concretely:
     - Define a trait in `traits`, something like:
       ```rust
       trait Sink {
           fn record<T: Backed>(&self, type_id: TypeId, op: T::Op);
       }
       ```
       (or generic over an already-serialized byte payload, if `Sink` itself shouldn't need `serde` bounds).
     - Every `Backed` value holds a handle to a `Sink` — recommend a type-erased `Rc<dyn Sink>` (or `&dyn Sink` where lifetimes allow) rather than threading a generic `S: Sink` parameter through every nested container type. The generic version would force `BackedHashMap<K, V, S>`, `BackedVec<T, S>`, every `#[derive(Backed)]` struct, etc. to all carry `S` as a parameter, cascading through the entire type tree for something that only matters on the write path. One dynamic-dispatch call per mutation is a fine price for avoiding that.
       - Response: Use static dispatch with a generic `S` instead but assign a default type from `frontend` to it. This provides generality without sacrificing performance or ergonomy (application can simply ignore the `S` type parameter if they only need the default sink).
     - Mutating methods call `self.sink.record(...)` themselves and don't return anything journal-related — matches your requirement that persistence isn't optional or forgettable.
     - Nested `Backed` fields need their `Sink` handle propagated from their parent at construction/open/deserialize time; this is a natural thing for `#[derive(Backed)]` to generate automatically (clone the parent's `Rc<dyn Sink>` into each nested field), so application authors never touch it directly.
       - Response: I'd rather like to use a normal reference `&S` (where `S: Sink`) that is attached to nested data structures on the fly when we access them so that there's no reference counting for every field access, and every nested data structure doesn't explictly have to hold a reference to the sink.
         Roughly, the way I'm thinking about this is that each mutating operation on a backed data type takes an additional function argument `sink: &mut impl Sink`.
         This includes mutable getters.
         One way I could do this would be with a wrapper type `struct Mut<'s, T, S: Sink + 's = DefaultSink>{ data: T, sink: &'s mut S }`.
         In this approach, backed data types like `HashMap<K, V>` only have non-mutating access methods but they don't directly declare any mutating access methods.
         For mutating access, one creates an `impl` block for `Mut<'s, HashMap<K, V>, S>`, which defines, for example, a method `.get_mut(&mut self, key: K) -> Mut<'_, &mut V, S>`, where the return type is generated on the fly from the `V` stored in the `HashMap` and the reference to `S` stored in the `Mut` wrapper.
         To make non-mutating access easy, `Mut<'s, T, S>` should probably implement `Deref<Target=T>`.
         Does this work? In particular, can third-party crates implement their own methods on `Mut<'s, MyType, S>` as long as `MyType` is theirs? Is there a simpler way to do this? Explain similarities and differences of this `Mut` wrapper compared to the `Py` wraper in the PyO3 library.
         How would this or any alternative approach work on `[#derive]`d `struct` types (where we want to make sure that mutable access to fields always has an associated reference to the `Sink`).
     - This resolves the dependency question in your favor without `types` needing to depend on `frontend` at all: `types` depends only on the `Sink` trait in `traits`; `frontend` provides the real file-backed implementation.
       - Response: In order to declare a default type for `S: Sink`, `types` probably needs to depend on `frontend`, but I think that's fine.
     - Being generic over `Sink` (rather than hard-coding the real frontend type) does earn its keep — other implementations worth having:
       - A no-op or purely in-memory `Sink`, for unit-testing `types` logic without touching a file.
       - A recording/spy `Sink`, for asserting exactly which `Op`s a mutation produces.
       - A batching `Sink` that buffers ops for an explicit `commit()` instead of persisting on every call, if that's ever wanted as an opt-in.
         - Response: yes, this is a good idea. Include this when formulating the final plans from these notes.
       - Eventually, a replicated/networked `Sink` — worth keeping the door open for given the concurrency question below, without committing to it now.


## Implementation Details

- Define operations as associated types of backed data types.
  Thus, declare a `Backed` trait that has an associated type `Op`, which has to implement `serde::Serialize` and `serde::Deserialize` (and probably some custom trait used for flushing the journal), and which implementations will typically define as an enum.
  Leverage an existing fast and compact binary serialization scheme for `serde` to serialize the sequence of operations in the journal.


## Open Questions

- **How to represent pointers in the backed representation?**
  We want to be able to move memory around either without invalidating pointers, or with a way to update existing pointers, and we want to know which memory regions are allocated and where the pointer(s) to them are without needing to involve the data structure implementations, so that the memory unit alone can do compactification.

  **Revised design decision** (see the full discussion under [Workspace Layout](#workspace-layout)): use single-owner `OwnedPointer`s (`Rc<Cell<(target, position)>>`, allocated only by `alloc`) rather than a separate on-disk handle table. Because every allocation has exactly one owning pointer, `alloc`'s own per-allocation bookkeeping doubles as that pointer's registry — no extra structure needs to live in the file. The trade-off against a true handle table: relocating a block that has `OwnedPointer`s *embedded in it* (as opposed to being the *target* of one) still needs type-specific cooperation to patch each embedded pointer's `position` — cheap when the owning type is already rewriting that block anyway (the common case), but not free if `alloc` ever wants to relocate arbitrary regions on its own.
  - **Response:** superseded by above discussion.

  Originally considered, kept here for context:
  - A `Rc<Cell<u32>>`-style shared pointer *without* the single-ownership restriction — rejected because with arbitrary aliasing you'd need a runtime registry to find every live copy of a pointer in order to rewrite it, not just one.
  - A dense on-disk *handle table* (conceptually a page table, or like classic Mac OS "Handles" / a generational arena) — fully decouples pointer identity from both the pointer's own location and its target's location, so block moves never require any patching at all, at the cost of persisting an extra indirection table to the file and one extra dereference per pointer access. Set aside in favor of `OwnedPointer` mainly because of that on-disk footprint, which matters for a system meant to hold many small, deeply nested pointers.

  Remaining implementation questions:
  - Whether `alloc` should hold a strong or weak reference to each `OwnedPointer` it hands out — a strong `Rc` clone would need an explicit `free()` call to release a block; relying on `Drop`/refcounting to free automatically would need `alloc` to hold only a `Weak` reference plus some way to reach the block's metadata without it.
    - Response: `alloc` should keep (in memory) a table that assigns a size to each life `OwnedPointer`.
      When a type implementation drops an `OwnedPointer`, the `Drop` implementation should remove the corresponding entry from `alloc`s table and mark the memory area as free (this dropping should probably only occur during flushing; unflushed removals of container entries only move the `OwnedPointer` to a graveyard).
      How can this be implemented?
      I think it would suffice to use the convention that the mutable wrappers of all container types must implement `Drop`, and the drop handler must (i) turn each contained item in a mutable wrapper and drop it and (ii) call `Sink::free` on any contained `OwnedPointer`s (which is a method that tells `alloc` to remove the pointer from the table and mark the corresponding memory as free, see sketched code example in the file `sketch.rs`).
      Imagine, for example, the user has an `Option<String>` with value `Some(s)`, and they want to replace it with `None` (here, `Option` and `String` are not std library `Option` and `String` but instead our backed implementation of those data structures from the crate `types`).
      Let's say that the user does this by calling the method `OptionMut<String>::take(&mut self) -> std::Option<StringMut>`.
      Note thre things: first, `take` is implemented on `OptionMut` and not on `Option` since it mutates;
      second, `take` returns a `std::Option` from the standard library because, at the time `take` returns, the original option with value `Some(s)` logically no longer exists in the file;
      and third, the type wrapped inside `std::Option` returned by `take` is `StringMut`, i.e., a mutable backed string because (i) we don't want the API to drop mutability unless necessary and (ii) unlike the original `Option`, the string `s` itself still exists in the backing file because the caller of `take` might just want to move it to a different location (we have to make sure that the operation recorded on the journal by `take` doesn't instruct the flushing mechanism to remove `s` yet, just to change the enum tag of the `Option`).
      In summary, the after calling `take`, the user has a `Some` variant of `std::Option` containing an `StringMut`.
      As soon as the user drops this `std::Option`, the `StringMut` gets dropped.
      Our `String` type might be implemented as a tuple `(std::String, OwnedPointer)` where the `OwnedPointer` is used to keep track of where the string is located in the backing file.
      As usual, `StringMut` additionally has access to a `Sink`.
      The above convention means that `StringMut` must implement `Drop` which must call `sink.free(owned_pointer)`.
      This is not ideal since it relies on a convention.
      Is there a way to enforce freeing, i.e., to call `sink.free()` automatically any time an `OwnedPointer` gets dropped?
      The problem is that the `OwnedPointer` itself doesn't have access to the `Sink` (and I don't want it to because that would make it unnecessarily big).
  - Whether pointers should carry a generation counter (to catch use-after-free / stale-handle bugs) at the cost of extra bytes per pointer.
    - **Response:** don't do this for the first version, but include this comment in a new section about things to be considered for the next version (along with concurrency).
  - The exact rule for when `alloc` is allowed to relocate a block containing embedded pointers on its own vs. only ever doing so through the owning type — this determines whether the `alloc`→`types` callback from [Workspace Layout](#workspace-layout) is ever actually exercised in practice.
    - **Response:** superseded by above discussion.

**See my comment above:** I'd rather explore alternatives where I don't have to store a handle table in the file.
I want this library to be used for nested data types that span a wide range of sizes, but I want that users shouldn't have to think differently about how they design their data types just because they are automatically backed to a file.
Thus, I expect that there will be quite a lot of small data types floating around, and thus lots of pointers, which is why I'd like to avoid having to store a handle table in the file.

- ~~Do container data types need to keep track of their changes since the last journal flush?~~ Resolved — see [In-Memory Representation](#in-memory-representation): yes, via a side "graveyard"-style structure that ordinary reads never touch.
  Still open: for *nested* backed types (e.g., a backed `Vec` of backed `HashMap`s), does every nested instance maintain its own local graveyard, or only the outermost opened object?
  Per-instance bookkeeping is simpler to implement in isolation but may duplicate work; a single root-level graveyard requires threading flush-time information down through arbitrary nesting.
  - The way that I'm thinking about it, I don't think that this is an issue.
    When a nested type (e.g., an entry of a `HashMap` in a `Vec`) is modified, then usually only the modification of the leaf type needs to be recorded.
    E.g., if the entry is a string, then only the modifications of the string are recorded to the journal, and the recorded operation identifies the string by a pointer to where it is stored in the file.
    If the memory location has to be moved (e.g., because we're making the string longer) then we know where in the serialized representation of the `HashMap` the pointer to the memory location written because of my above proposed in-memory representation of `OwnedPointer`, and so we can update that pointer without having to know anything about the internals of the `HashMap` or `Vec` implementations (again, I think this is similar to how the JVM does it in main memory, but I'm not sure if I'm missing something).
    Check if my thinking makes sense and if it does, rewrite it more eloquently and concisely.
  - **Claude:** Your reasoning holds, and it actually goes further: once pointers are first-class `OwnedPointer`s held directly by the entry itself, nested or not, you don't need a graveyard at all. The reason a graveyard seemed necessary was "we need to remember a deleted key's old snapshot location for later" — but if the primary structure's entry already holds that key's `OwnedPointer`, that information is already in hand at the exact moment of deletion, so there's nothing to remember separately: removing the entry immediately yields the pointer to free (either freeing it right away, or queuing a `free(pointer)` op), with no side table involved.

    This also fully resolves the earlier graveyard/CRDT question, and more cleanly than the "idempotent add-once" framing given there: a freshly-inserted-then-deleted key never had an `OwnedPointer` yet (nothing was allocated, so the matched insert/delete just cancels in the journal's optimization pass), while a pre-existing key's delete frees its one live pointer immediately, and any subsequent reinsert-then-delete within the same epoch produces a fresh, pointer-less entry that again just cancels. No graveyard, no double-counting, no CRDT-flavored reasoning needed — it's a direct consequence of "you can only free a pointer you're currently holding, and you free it exactly once, the moment you stop holding it." The "In-Memory Representation" section's graveyard design can be dropped once `OwnedPointer` is in place, and this whole open question can be marked fully resolved.

    And to your JVM comparison specifically: same correction as above — this works because pointers are single-owner, so there's always exactly one place to look up and patch, unlike a tracing collector that has to discover all referents.

- **Does the workspace layout make sense?** Broadly yes — see the two callback boundaries noted in [Workspace Layout](#workspace-layout), which the crate dependency directions force into narrow trait interfaces defined in `traits`. With `OwnedPointer` (rather than a handle table) as the pointer design, `alloc` owns the pointer/allocation bookkeeping entirely on its own — nothing needs to be bootstrapped before a file can be opened — so `traits` only needs to define the shared interfaces (`Op`, `Sink`, the `OwnedPointer` type itself) that both `alloc` and `types` use.

- **Concurrency scope.** Assuming v1 targets a single process holding exclusive write access to a backing file — no concurrent writers, no shared-memory-mapped readers from other processes. This keeps `alloc` and the journal free of locking concerns. Worth confirming before implementation starts, since retrofitting multi-writer support later would likely require format changes.
  - The first version will likely not support concurrency.
    I do want to think about concurrency before starting with the implementation to avoid making any architectural decisions that are fundamentally incompatible with concurrency, but I want to get a better idea of the overall architecture before thinking about issues that could arise if a future version supports concurrency.
  - **Claude:** Sounds right — single-writer for v1, revisit later. One thing already falls out in your favor: single-owner `OwnedPointer`s (rather than freely-aliased `Rc<Cell>`s) are also the friendlier starting point for a future concurrent version, since exclusive ownership maps naturally onto typical single-writer/multiple-reader schemes (e.g., MVCC-style snapshot isolation), whereas arbitrary aliasing would have made that harder to retrofit. The `Sink` abstraction above leaves room for a replicated/networked implementation later too, without touching `types`. No architectural decision is needed now beyond keeping both of those doors open, which they already are.

- **Crash consistency.** Since a core selling point is that "no committed mutation is ever lost," the on-disk journal entry format needs enough framing (e.g., a length prefix and checksum per entry) that, after a crash mid-write, reopening the file can detect and discard a torn trailing entry rather than misinterpreting garbage as a valid operation. This should be designed into the journal format from the start rather than retrofitted.
  - Yes, tell me more about the trade-offs.
    I think making the journal itself crash resistant should be relatively easy, but a crash consistent flushing operation might be hard or require more temporary memory and more writes.
    I could imagine a scenario where each operation (either on the data-type level or on the microoperation level) is transactional, but this would probably require us to do a lot more work (like multi-step updates of pointers, recording how far through the journal we are after each operation, ...).
    How can flushing be made crash consistent?
  - **Claude:** The journal being append-only and framed (length + checksum per entry) gets you crash-safety for the journal itself, as you said — a crash mid-append just leaves a detectable, truncatable torn entry at the tail; nothing before it is touched.

    For flushing, the standard technique that avoids needing per-operation transactionality is **copy-on-write / shadow paging** — this is how LMDB (a single-writer, file-backed embedded database with a very similar shape to this project) gets crash consistency, and it's the same family of idea as ZFS/Btrfs snapshots and ARIES-style database recovery, just the simplest member of that family:
    - During a flush, never overwrite live snapshot data in place. Write every new or moved block (compacted data, updated pointers, etc.) to *fresh, previously unused* space in the file.
    - The old snapshot stays byte-for-byte intact and fully valid throughout the entire flush.
    - At the very end, atomically switch over to the new state with a single small, `fsync`'d write of a root/commit marker (e.g., the file header's pointer to the current root object(s) plus the current journal-start offset).
    - If the process crashes at any point *before* that final marker write completes, the next open just sees the old marker, ignores all the half-written new data (inert, unreferenced garbage at that point), and either retries the flush from scratch or simply keeps running with the journal un-flushed. No partial state is ever live, so there's nothing to roll back and no per-operation progress tracking is needed.
    - Ordering matters: `fsync` the new data *before* writing/`fsync`ing the commit marker, otherwise the marker could become durable while the data it points to isn't yet, which would point at garbage after a crash.
    - Cost: temporarily more disk space during a flush (old and new coexist until commit), and the old, now-unreferenced space needs to be reclaimed afterward — but that reclamation is itself safe to do non-atomically, since nothing live references it anymore; worst case of a second crash there is leaked (wasted, not corrupted) space that a later flush can reclaim.

    This is considerably simpler than making individual operations transactional, and sidesteps "track how far through the journal we got" entirely, because the unit of atomicity is the whole flush, not each op. The more elaborate alternative — fine-grained redo logging of the flush's own writes (ARIES-style), so a crash mid-flush can *resume* instead of restarting — is only worth the extra complexity if flushes become expensive enough that discarding a half-finished one on every crash is unacceptable; I'd defer that until it's actually shown to matter.
