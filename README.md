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

1. `alloc` (which does not depend on `types`) must still be able to ask a concrete data type to update its pointer meta data during compactification. Resolved by the handle-table pointer design below, which removes the need for this callback in the common case (see [Open Questions](#open-questions)).
   - Actually, I'd like to explore if I can get away withoaut a handle table because I'd rather not have to include additional metadata to in the file that can be derived from the payload itself.
     Wouldn't it suffice if `alloc` defines an `OwnedPointer` type which, internally, is something like `Rc<Cell<(u32, u32)>>` and keeps track of (i) the pointer target in the fiel and (ii) where in the file the (unique, thus "owned") pointer to that position sits.
     These `OwnedPointer`s can be held by types in the `types` crate (or in user-defined crates) but those crates can obtain an `OwnedPointer` only from the `alloc` crate (by allocating memory).
     When an `OwnedPointer` gets serialized to the snapshot section of the file, serialization writes the target address (first `u32` of the tuple) and sets the position (second `u32`) to the current offset in the file.
     The alloc crate would hold on to a clone of all `Rc`s of `OwnedPointers` that it hands out and thus has the full picture of what points from where to where.
     Isn't this similar to how garbage collection and compactification in, e.g., the JVM works or am I missing something?
2. `types` (which does not depend on `frontend`) must still get its mutations appended to the file's journal. Resolved by having mutating methods on `types` return `Op` values rather than perform I/O themselves; `frontend` (and `derive`-generated glue) is what actually appends returned `Op`s to the journal.
   - Good catch. No, I don't want mutating methods to return `Op` values because I don't want the user to have to remember to persist changes.
     Application authors should not be able (in safe rust code assuming correctly implemented `types`) to mutate an instance of a backed type without automatically persisting it to the file.
     Thus, all types would either have to hold a reference to the overall machinary that persists changes or all their mutating operations should require such a reference as an additional function argument.
     I'm leaning towards the former at least per default.
     Come up with a clean way of organizing this into appropriate traits.
     Either `types` would have to depend on `frontend` (which seems fine because `frontend` shouldn't depend on `types` anyway); or, maybe it's better to be generic over the frontend anyway and define a trait for frontends that a type in `frontend` implements (would this be useful? what other implementations of such a trait are conceivable?).


## Implementation Details

- Define operations as associated types of backed data types.
  Thus, declare a `Backed` trait that has an associated type `Op`, which has to implement `serde::Serialize` and `serde::Deserialize` (and probably some custom trait used for flushing the journal), and which implementations will typically define as an enum.
  Leverage an existing fast and compact binary serialization scheme for `serde` to serialize the sequence of operations in the journal.


## Open Questions

- **How to represent pointers in the backed representation?**
  We want to be able to move memory around either without invalidating pointers, or with a way to update existing pointers, and we want to know which memory regions are allocated and where the pointer(s) to them are without needing to involve the data structure implementations, so that the memory unit alone can do compactification.

  **Design decision:** use a level of indirection — an allocator-maintained *handle table* (conceptually a page table, or like classic Mac OS "Handles" / a generational arena). A pointer stored anywhere (in the snapshot section, in the journal, or as in-memory meta data) is really just a stable integer *handle* that indexes into this table; the table maps each handle to its current physical file offset (and, in memory, to whatever native representation is needed). When the memory management unit moves or compacts an allocation, it only updates the *one* table entry for that handle — every holder of the handle keeps working unchanged, because it never stored a raw offset, only the handle.

  This avoids both problems raised by the alternatives originally considered: the `Rc<Cell<u32>>`-style shared pointer would need a runtime registry to find and rewrite every live copy of a pointer (plus refcounting overhead on the hot path), while the single-ownership scheme left it unclear how to update pointers duplicated into in-memory meta data when the underlying memory moves. With a handle table, neither problem arises: there's nothing to find and rewrite, because every copy only ever holds the immutable handle.

  Remaining implementation questions:
  - How the handle table itself is stored and grown on disk — it's another dynamically sized structure, so it likely needs to be bootstrapped specially in `alloc` rather than being just another backed type.
  - Whether handles should be generational (to catch use-after-free / stale-handle bugs) at the cost of extra bytes per handle.
  - How aggressively to prune or reuse freed handle-table slots.

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

- **Does the workspace layout make sense?** Broadly yes — see the two callback boundaries noted in [Workspace Layout](#workspace-layout), which the crate dependency directions force into narrow trait interfaces defined in `traits`. One likely addition: the handle table from the pointer design above is needed just to open a file at all, before any *typed* backed value exists, so it probably belongs entirely inside `alloc`, with `traits` only defining the shared interfaces (`Op`, handle types) that both `alloc` and `types` use.

- **Concurrency scope.** Assuming v1 targets a single process holding exclusive write access to a backing file — no concurrent writers, no shared-memory-mapped readers from other processes. This keeps `alloc` and the journal free of locking concerns. Worth confirming before implementation starts, since retrofitting multi-writer support later would likely require format changes.
  - The first version will likely not support concurrency.
    I do want to think about concurrency before starting with the implementation to avoid making any architectural decisions that are fundamentally incompatible with concurrency, but I want to get a better idea of the overall architecture before thinking about issues that could arise if a future version supports concurrency.

- **Crash consistency.** Since a core selling point is that "no committed mutation is ever lost," the on-disk journal entry format needs enough framing (e.g., a length prefix and checksum per entry) that, after a crash mid-write, reopening the file can detect and discard a torn trailing entry rather than misinterpreting garbage as a valid operation. This should be designed into the journal format from the start rather than retrofitted.
  - Yes, tell me more about the trade-offs.
    I think making the journal itself crash resistant should be relatively easy, but a crash consistent flushing operation might be hard or require more temporary memory and more writes.
    I could imagine a scenario where each operation (either on the data-type level or on the microoperation level) is transactional, but this would probably require us to do a lot more work (like multi-step updates of pointers, recording how far through the journal we are after each operation, ...).
    How can flushing be made crash consistent?
