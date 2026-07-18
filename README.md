# Auto-Saved Data Strucktures in Rust

This repository is a rust workspace implementing a rust library "backed" data structures, i.e., data structures that are automatically persisted to a file with every mutation.
The library implements backed variants of common types such as vectors, hash maps, and ropes, and it also allows rust application developers to `#[derive(Backed)]` for their own data structures.
Any backed data structure can be "opened" from a file, which loads the data structure into memory but also keeps a connection to the file.
Any modification to the data structure (or to one of its nested members) mutates the representation in memory as usual, but is also immediately persisted to the file.

Read acces only goes to the in-memory representation (it does not touch the backing file).
The in-memory representations of the provided data types are optimized for reading speed similar to corresponding normal (non-backed) data types, but they typically hold some meta information to keep track of where data lies in the backed representation.
Write access affects both the in-memory and the backed representation in the file.
The backed representation is updated using an efficient combination of journalling and dynamic memory management that minimizes random access writes while still persisting any changes instantly and keeping the representation compact in the long run.


## In-Memory Representation

The in-memory representation of data types contains at least a part that is similar to the in-memory representation of corresponding normal (non-backed) data types.
In addition, the in-memory representation typically contains some pointers or other meta data that helps keeping track of how things are layed out in the backing file (so that the data types can translate data-type level operations to microoperations, see [sketched pipeline below](#flushing-the-journal)).

Further, complex container data types might need to hold a local journal in memory (i.e., the subset of the operations in the in-file journal that apply to this data type, possibly already in shorened diff representation that gets rid of redundant activities) in order to keep track of everything.
This would mean that, e.g., a lookup in a hash map would have to first look up the key in a primary table and then check a secondary table if that key was removed or replaced since the last journal flush.
I'm not sure yet if this will be necessary.
I'd prefer to get away without this since it seems to make more advanced read access such as iteration quite difficult.

Maybe a compromise could work: store the current state in a way that can be immediately accessed for read operations and keep additional metadata on the side that contains any information needed to flush the journal.
For example, a hash map implementation would contain a (mostly, except for additional pointer meta data) regular hash map as well as an additional "graveyard" hash map of any relevant data regarding keys that have been removed since the last journal flush (so that we can still recover their memory locations in the snapshot section of the backed representation, see below).


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


## Implementation Details

- Define operations as associated types of backed data types.
  Thus, declare a `Backed` trait that has an associated type `Op`, which has to implement `serde::Serialize` and `serde::Deserialize` (and probably some custom trait used for flushing the journal), and which implementations will typically define as an enum.
  Leverage an existing fast and compact binary serialization scheme for `serde` to serialize the sequence of operations in the journal.


## Open Questions

- How to represent pointers in the backed representation?
  - We want to be able to move memory around either without invalidating pointers or with a way to update existing pointers.
  - We want to be able to know which memory regions are allocated and where the pointer(s) to it is, ideally without needing to involve the data structure implementations so that the memory unit alone can do compactification.
  - Thus, we probably want to introduce an opaque "pointer" or "slice" type (or both) that the implementations of data types have to use whenever they want to record to the journal an operation that involves pointers.
    This pointer or slice type might help the memory management unit to keep track of allocated memory regions without requiring us to explicitly store this information in the backing file.
    Under the hood, the pointer type might be something like an `Rc<Cell<u32>>` (or its atomic variant), which would even allow us to move memory around while updating all pointers that point to it, if this is necessary.
    However, this would probably also somehow have to keep track of where the serialized pointer in the snapshot section of the backing file is located, so that it can be updated when we move the memory around.
  - But maybe it's not necessary to have an `Rc<Cell<T>>`.
    Maybe we can get by with requiring each memory location to be only pointed to from one place in the snapshot section (i.e., we treat this like "ownership").
    The journal section may contain additional pointers but that's OK because when we apply the journal we can keep track of moving pointers (or maybe we don't even have to because it is logically impossible to have pointer invalidations before an operation that uses a pointer; in this case pointers in the journal sections would be like normal rust references `&T`, which can no longer be around once we get to modifying `T`).
    But it's unclear how that would allow us to update not only the serialized pointer in the snapshot section of the backing file but also any pointers stored as meta data in the in-memory representation of the data structure.
- Do container data types need to keep track of their changes since the last journal flush (see [In-Memory Representation](#in-memory-representation))?
  How can this be done in the most efficient way and in a way that makes read and write access as easy as possible to get right.
- Does the workspace layout make sense or should it be broken up in a different way / did I miss any important components that would be required for this system to work?
