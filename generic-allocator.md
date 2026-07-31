# Generic Allocator

This document summarizes ideas for making the allocator part of Kladde more modular and separable from the rest of kladde. This plan reuses some ideas from `allocator-spec.md`, but it deviates quite substantially from it.

## My planned order of operations

1. State goals in this document, discuss trade-offs and describe the design in words.
2. Create a new, mostly empty crate where we can experiment with this.
3. Add code examples to this document that sketch the APIs of each trait and struct. Run some minimal experiments in the newly created crate if it helps to reason about this.
4. Implement a prototype in the new crate, without touching any existing crates.
5. Iterate over the prototype and decide where to set crate boundaries.
6. Move the rest of Kladde onto the new allocator definitions.

## Core idea: separate allocator into a re-usable standalone project

See [`assessment.md`]:
> **`kladde-alloc` as a standalone product.** The type-agnostic relocatable persistent heap is reusable well beyond this library; the [`spec.md`](spec.md) Automerge comparison already hints at this. Worth keeping the crate boundary clean with that option in mind.

A few design decisions follow from this that currently don't hold:
1. **Limited scope:** the `Allocator` trait should be decoupled from other components of Kladde. For example,
	- The actual reading, writing, seeking, and resizing of the file should go into `Storage`. The allocator is just in charge of managing contiguous address ranges for stable allocation identifies.
	- Kladde's schema specification builds *on top of* allocation, so the allocator should not rely on type descriptions.
2. **Fundamental features** (somewhat in tension with Item 1): while `Alloc`'s scope should be limited to features that are useful beyond Kladde's use case, it should have enough features to be generally useful. For example, sizes of fixed-sized regions should be known by the allocator alone without requiring it to rely on Kladde features like schema.
3. **Versatility:** `Allocator` should support use cases with a broad range of constraints, e.g., by being generic over fundamental decisions (like pointer sizes) and by not imposing the same representation of pointers (e.g., via integer indices) onto every `Allocator` implementation.

## Advantages
- Makes Kladde's default allocator reusable outside of Kladde
- Even within Kladde, we can use the new allocator for more than the allocation of user types. We can use it to manage the schema and possibly to manage the journal (if it doesn't turn out that that has to be part of the allocator).
- Easier and more widely applicable tooling: allocation-related operations like reordering and compaction should be possible even without knowing that a file is a Kladde file (i.e., that it has a schema, a journal, a certain set of `Persistable` types including `Opaque` types, ...).

## Detailed consequences

- The default allocator implemented in `kladde-alloc` should be parameterized by its address type (the actual memory addresses used only internally by the backend and never exposed to user types), a `Size` type (used to express sizes of memory allocations, and exposed to user types), and its index/ID type (the stable identifier of pointers that survives compaction, and that is exposed to user types so they can serialize it). At least (NonZero) 32 and 64 bit integers should be supported for all of them. This makes the default allocator usable different situations from embedded to desktop, and puts the decision about pointer size on the actual format that uses the allocator.
	- I think when used inside Kladde, the default configuration should be `Address=u64`, `Size=u32`, `Id=NonZeroU32` (nonzero because the ID is exposed to user types, who will serialize it to allocated memory of the containing data structures, and we'll eventually want to implement niche optimizations). Choosing 64-bit `Address`es allows for essentially arbitrarily large files, and limiting `Size` and `Id` to 32 bit only limits the size of *individual allocations* (to just below 4 GiB) and the *number of allocations* (to 4 billion), both of which seem fine (allocating an individual chunk of memory > 4 GiB would render the advantages of Kladde, i.e., its automatic memory management, moot anyway; and an application that creates > 4 billion allocations is probably doing something wrong because Kladde is designed to make inline memory layout the default, avoiding unnecessary indirections). The smaller `Size` and `Id` will hopefully reduce storage costs of data structures that hold a lot of pointers, and possibly reduces memory requirements of the allocator (which will likely have to store IDs more often than addresses, we'll see). Maybe we want to set the defaults of trait parameters (e.g., for `Allocator`) or define type aliases accordingly. Which one is better (default type parameters or type aliases)?
- Completely decouple `Allocator` from `Persistable` types (except maybe via an extension trait, defined in the crate where `Persistable` is defined). Allocators don't know anything about types and aren't allowed to do anything different depending on the type of the stored data. They only know the size of an allocation and whether it is fixed or dynamically sized.

## Layered architecture

Traits and structs build on each other. At the lowest level (`Allocator` and `Storage`, which are independent of each other), they are very generic and meant for reusability outside of the wider Kladde system. Subsequent layers specialize increasingly to the requirements of Kladde. However, Kladde is still the main intended use case, and I don't want to introduce any generalization that would hurt performance or ergonomics (especially for *application developers*) in the Kladde use case. For ergonomics, we should set defaults or define shorthand type aliases motivated by the Kladde use case where we can (TODO: which one is better)?

- The trait `Allocator` is the foundation for memory management. It manages a dynamic collection of address regions but doesn't connect them to memory regions (i.e., it doesn't allow reading from or writing to any memory). In fact, `Allocator` doesn't even expose the actual address ranges (because they are only relevant for `Backend` Implementations, but they're no longer relevant once we go up to the schema or `Persistable` level). Only its extension `TransparentAllocator` exposes actual address ranges.
- The traits `WriteBackend` and `Backend` extend `Allocator` and equip it with methods write to and read from some associated memory at the allocated address ranges, respectively. These are the traits against which manual implementations of `Persistable` data types for Kladde's framework have to be implemented.
- The trait `Storage` is orthogonal to `Allocator` and its descendants. While `Allocator` models memory *management*, `Storage` models memory *access*. It provides unstructured random and sequential memory access to a large block of stored data (e.g., a file).
- Concrete implementations of `WriteBackend` and `Backend` (`UnjournaledBackend` and `Journaled[Write]Backend`) are composed of a `TransparentAllocator` and a `Storage`, and they are generic over the concrete types of both of these. They perform two tasks:
	- they translate the high-level read/write operations of the `Backend` trait ("read/write bytes `x..y` of memory allocation `p`") to low-level read/write operations in an encapsulated `Storage`; and
	- they delegate `Allocator` calls to the encapsulated allocator, while replicating mutating `Allocator` calls (like `alloc_*`, `free_*`) to an in-`Store` representation of the allocator state from which the encapsulated `TransparentAllocator` can be recreated. The backend implementations use the encapsulated `TransparentAllocator` itself to manage memory allocations that hold allocator state in-`Storeage`. This way, allocator state and user data are automatically separated without having to define fixed regions in the file that are reserved for one or the other.
- On top of `Backend`, the header, schema, and its fingerprint are stored in normal allocations, reachable from a pointer stored in some fixed-sized header. For the serialized schema, we might actually reuse `PersistableVec<u8>` once we have a good implementation of that and pin it to a given version there.

### `trait Allocator`
- Has associated types `Id`, `Address`, and `Size` (most types that implement `Allocator` will probably be parameterized by these, i.e., `MyAllocator<Id, Address, Size> { ... }` and then `impl<Id, Address, Size> Allocator for MyAllocator<Id, Address, Size> where Size: Into<Address>, ... { type Id = Id; ... }`)
	- `Id` is the type for stable identifiers of each allocation. It can be serialized into other allocations, i.e., it's the inner representation of pointers, previously called `Index` in `allocator-spec.md`.
- Manages a set of non-overlapping `Address` ranges, where `Address` is an unsigned integer type that should probably somehow generically support `NonZero`.
- The trait definition does not assume any connection to persistency. It doesn't interpret the integer ranges it manages as memory and doesn't provide read or write access to them (that's what `Backend` is for), and you can implement an `Allocator` that holds its entire state in memory and doesn't deal with persistence at all. A persisted allocator then typically builds on top of an in-memory allocator, somewhat analogous to how, at a higher level, types that implement `Persistable` (like `PersistableHashMap<K,V>`) often use standard library types (like `std::collections::HashMap<K,(V,Index)>`) for their in-memory representation.
	- Concretely, this means that, different to the current `allocator-spec.md`, this new `Allocator` does not have the methods `read`, `write`, and `splice` (those go in `Backend` below)
- Maybe exposes some `alloc_scratch` (allocate largest contiguous memory within the file) and/or `alloc_at_end` method if that is needed for journaling. Maybe not necessary if we can simply use the standard allocation methods and then query whether we happen to be at the end of the file, exploiting that a persisted allocator that builds on top of a *concrete* in-memory allocator may make assumption about the in-memory allocator's specifics beyond the `Allocator` trait.
- Should probably be self-hosting. I.e., the `UnjournaledBackend` below  uses the Allocator itself to manage the memory regions where in-file representation of the Allocator is stored. It will probably need special logic to manage it in-file, but that's separate.
- Probably exposes `UniquePointerResizable`, `UniquePointerFixedSize`, and `RawPointer` as *associated types* so that `Allocator` implementations may choose different memory representations for these (e.g., caching some meta data like whether it's fixed-size, and if so, which size it has). This would deviate from the current `allocator-spec.md`, which defines them as concrete types shared across all `Allocator` implementations. It can't expose `UniquePointer<T>` as an associated type because that requires `T: Persistable` for `T::INLINE_SIZE`, and I want to decouple the `Allocator` trait from `Persistable` (and probably put it in a separate crate).
- Provides default-implemented methods that allow generating unique IDs and reserving them for later allocation of a given size (this will be used by `JournaledBackend` below, and it's also nice that this gets exposed to implementors of `Persistable` types because it may be useful for some of them, and it's easily implemented for the `JournaledBackend` itself):
	- `fn reserve_resizable(&self, byte_size: usize) -> UniquePointerResizable;`
	- `fn reserve_fixed(&self, byte_size: usize) -> UniquePointerFixedSize;`
	- And methods that `claim` reservations (i.e., assign actual addresses)
- Default implementations of `reserve_{resizable, fixed}` simply forward to `alloc_{resizable, fixed}`. Default implementations of `claim_{resizable, fixed}` are no-ops.
- It must be allowed to call all `Allocator` methods (like freeing, resizing, ...) on valid `UniquePointerResizable` or `UniquePointerFixedSize` regardless of whether they're reserved or actually allocated. But `WriteBackend` and `Backend` may return an error when provided pointers that are only reserved and where never claimed (they don't *have to* return an error, but if they don't then they must operate as if the allocations were normally created rather than just reserved, as in the default implementations)
- Has an additional associated type `MetaData: Default` for storing additional data for each allocation that can be queried either by a dedicated `meta_data` method or by a `lookup` method that returns everything about a given pointer (its address, sizedness, size, and meta data). This is used by `Backend` implementations to keep track of where allocator state is stored in `Storage`.
	- `Allocator`s that don't provide any `MetaData` can simply set `type MetaData = ()` and leave the query method default implemented (it returns `MetaData::default()`).
	- The reason why we push `MetaData` into `Allocator` instead of requiring the containing types to simply store the meta data in a hash map themselves is that most `Allocator` implementations will probably have some sort of table `Id --> (Address, Size, ...)` anyway, and many use cases where one would query for `MetaData` would also involve a query for the addresses or size, which would hit that table anyway. So it's probably more efficient to have it all in a single table.
- Apart from the above, model `Allocator` after the description in `allocator-spec.md`, with a few additional minor tweaks:
	- `Allocator` should support querying whether a given raw pointer is fixed-size or resizable: `fn downcast(RawPointer) -> Result<X, ...>` with `enum X { Resizable(UniquePointerResizable), FixedSize(UniquePointerFixedSize) }` or maybe a `lookup` method that returns an `Option` instead of a `Result` and also returns the size of the allocation and the `MetaData`. Let's defer this until we can guide the design by a use case, see `PersistableVec` example below.
	- We should also require methods to convert between fixed sized and resizable allocations. These consume the old pointer and return a new pointer (possibly with a new `Id` because some `Allocator`s might use a bit in the `Id` as a flag for fixed size vs resizable).
	- Don't use the term `capacity` here when referring to allocation sizes. Always call it `size` (also not `byte_size`) because that's what it is *from the perspective of the allocator*. The implementation of a `Persistable` type might interpret the *size of the allocation* as a *capacity of a container* but that's at a higher level of abstraction.


### `trait TransparentAllocator: Allocator`
- Adds a method to query for actual addresses of `RawPointer`s, and default-implemented methods for querying `UniquePointerResizable` and `UniquePointerFixedSize`. These methods *may* return `None` if the provided pointers were only reserved but never claimed.
- Also adds a required `resize_transparently` method that is like `Allocator::resize` but returns a `Some(old_address, new_address)` if resizing requires moving data.

### `trait WriteBackend: Allocator`
- Extends `Allocator` with write operations (+ splice, which combines writing with memory management).
- Does not provide `read` operations. This is deliberate: `Guard` methods should never read from the backend because it may have stale data (if we have a `JournaledBackend`). Thus,
	- we'll change the signature of `Persistable::guard` and `Persistable::store` so that they only get a `WriteBackend`. Only `Persistable::load` gets a full `Backend`.
	- The struct `JournaledBackend` below should implement `WriteBackend` but maybe not `Backend`. Instead, it could have a `flush` method that returns a shared reference to a `Backend`, which will allow reading from it but prevent writing to it while it's alive because it's a shared reference (that's what we want: users shouldn't be allowed to write to a journaled backend while they're allowed to read from it because they'd expect reads to reflect their writes, which will not be the case in a `JournaledBackend`).
- Does however provide querying allocator methods (like `size` or the above `downcast`), which may seem like they are like `read` operations and should thus be forbidden, but that's OK: any `WriteBackend` implementation will have to hold an in-memory `Allocator` that is always up to date. Only reads from *allocated memory* are forbidden in `Guard` methods; querying the allocator state is OK and often unavoidable.
- `write` should maybe take a `RawPointer` and a `size: Size`, and return an `impl Write` (`UnjournaledBackend` below simply hands out the `&mut Storage` after seeking to the position; for `JournaledBackend`, we should check if we can write the `Op` header and then simply return the `&mut Storage` to let the user fill in the rest of the op. It seems a bit dangerous because it would break not only if the user overwrites but even if they write less than promised). But it would be nice to allow users to write through a `Write` because implementations of `Persistable` types might realistically want to call `write_vectored`


### `trait Backend: WriteBackend`
- Adds a `read` method. It should take a `RawPointer` and an `offset: Size` and return an `impl Read + Seek` (the backends below will simply hand out the actual `&mut Storage`, after seeking to the appropriate position)
- Still implements `write` (because it is also a `WriteBackend`) and mutating allocator methods (because it is also an `Allocator`, and `Allocator` isn't split into 2 traits for querying and modifying). But that's OK: if you want to prevent writing while reading is allowed, never hand out an owned `Backend`, only a shared reference `&impl Backend`.
	- [ ] TODO: is this true? It seems like `write` and `alloc_*` only take `&self`. That's probably to make `Guard` implementations more ergonomic. But it will require some `Cell` or `RwLock` gymnastics -- can we elevate that to the `WriteBackend` level so that `Allocator` has a more normal API that takes `&mut self` for mutating methods?

### `trait Storage: Read + Write + Seek`
- Provides both random and sequential read and write access (via `Read` and `Write`, and `Seek` for random access), plus resizing (via a trait method), in some abstract storage. The storage could be a file but doesn't have to be.
- A concrete implementation might reserve a tiny fixed-size header region that it manages itself by offsetting addresses accordingly (the header should only hold very basic information like a magic number and parameters of the allocator; any other header data should go into an allocated memory region so that the header size can evolve without having to rewrite the whole file).
- A `#[cfg(test)] struct MockStorage(Vec<u8>)` should implement `Storage` in memory for tests. Don't make it public because I can't yet see a use case outside of tests for it. We'll eventually want to add a public in-memory storage for testing `Persistable` user types, but that should probably be a `MockBackend` that implements `Backend`, not a `MockStorage`.

### `struct UnjournaledBackend<S: Storage, A: TransparentAllocator<Meta=...>>`
- Implements `Backend` (and therefore `WriteBackend` and `Allocator`) but probably does not implement `TransparentAllocator` (because true addresses should never be exposed to the implementations of user types since they're not stable and users can't read from or write to arbitrary memory addresses anyway).
- Holds:
	- an `S`to manage on-file state and
	- an `A` for managing its in-memory state.
- Implements read / write calls of `Backend` by using the `TransparentAllocator` to resolve pointers to addresses and then reading from / writing to the `Storage`
- Forwards `Allocator` calls to its in-memory allocator `A`. For modifying allocator calls (allocating / freeing / resizing), forwards to its in-memory allocator but also immediately persists a compact version of the state to the `Backend` (this `Backend` representation of the allocator state is not optimized for fast lookups. It will only be used to recreate the in-memory allocator state when we load a Kladde file).
- Holds its own immediate (inline fixed-size) state probably at a fixed address in the file (which it shifts away in all `Backend::{read, write, seek}` calls), but then all indirect parts of the state in memory allocations managed by the in-memory allocator. For the `Storage` representation of the allocator, the allocator-related allocations may have to be stored in a special bootstrapped way, different from how those allocations then contain the tables of allocations that hold user data.
- Probably not really useful for Kladde, but a good test of the versatility of the allocator design, and probably a good first step before implementing `JournaledBackend`.

### `struct JournaledBackend<S: Storage, A: TransparentAllocator<Meta=...>>` (and `JournaledWriteBackend`)
- Similar to `UnjournaledBackend` except that only `JournaledBackend` provides read operations. The two types are probably newtype wrappers around a shared private inner type, and they should have methods to turn one into the other (e.g., `JournaledWriteBackend::flush(&mut self) -> &JournaledBackend`; never hand out an owned `JournaledBackend` because that would allow reading and writing).
- Forwards any `Allocator` method calls to the in-memory allocator but only calls `reserve_*` instead of `alloc_*`.  Appends a corresponding `Op` to the journal. The `Op` for allocation encapsulates the index of the returned allocation, its size, and whether it's resizable, but not the actual address because that doesn't even get assigned by `reserve_*` and is deferred to journal replay (which calls `claim_*`). Deferring address assignments will allow us to grow the journal at the end of the file without bumping into any address regions that are already reserved for memory allocations by `Op`s that are earlier in the journal.
- May need to hold some additional data in memory to tie the in-memory state to the persisted state.
- Uses the inner `Allocator` to manage its journal, but doesn't use the generic journaling method to allocate new journal memory because that might lead to an infinite loop. Not sure how to resolve this. Probably need to use the `Storage` header for it.
- allocating on a `JournaledAllocator` doesn't actually pick an address yet. It only records a new index and the allocation size (and whether it's resizable) in the journal. True addresses get assigned on journal replay. 
- Implements journal replay and calls it automatically when the journal overflows and we're not inside a transaction. Journal replay assigns actual addresses to new allocations, and resolves the addresses of any memory reads/writes on the journal.

### `trait AllocatorExt: Allocator`

Extension trait with a blanket implementation for every `Allocator`, but defined in the crate where `Persistable` is defined. See Section 4.3 in `allocator-spec.md` except for the following nitpicks:
- Don't call methods `*_boxed`. Call them `*_typed` instead.
- Add `promote<T>` and `demote` methods to convert between `UniquePointer<T>` and `FixedSizedPointer` (they should probably be on the pointer types themselves but that's only possible for `UniquePointer<T>::demote` but not for `FixedSizePointer::promote` since `FixedSizePointer` is defined externally).
- Split `alloc_array` into two methods:
	- `alloc_fixed_size_array<T: Persistable>`, which returns a `UniquePointerFixedSize`; and
	- `alloc_resizable_array<T: Persistable>`, which returns a `UniquePointerResizable`

## Test case: chunked `PersistedVec` with small-vec optimization

TODO: build a chunked `PersistableVec` implementation for kladde onto the redesigned `Allocator`. This implementation should deliberately be "prematurely optimized" to verify that it supports anything we might need one day.
- For content smaller than the chunk size, its should not require any more on-disk space than a pointer (inline in the parent struct), the size (*only* stored by the allocator, not by the `PersistableVec` itself), and the data (behind the pointer). There is no separate capacity for small vecs, the allocation size matches the vector size and gets resized by the allocator when the `PersistableVec` grows or shrinks.
- For content larger than the chunk size, it should have a fast mode where only fixed-sized chunks are allocated, they're stored on disk in a linked list (but held in memory by a `std::vec::Vec` of pointers for fast random access). The on-disk representation then holds the length and a pointer to the first chunk. Since the inline size is only one pointer, some of this information has to be stored one indirection away.
- There should also be a "compact" mode for content larger than the chunk size where the last chunk is variably sized and fits the content length. This could be generated, e.g., by an explicit "extreme" compaction before file closing (which is also a privacy measure as it removes stale data from unused memory regions in the file). This representation may turn out to be slightly suboptimal in disk size as it might store the overall vec size even though it could be determined from walking the linked list and querying the allocator for the size of the last chunk, but since this is only for large files the relative impact is small.
- To distinguish between the above three cases, ~~we must be able to sneak at least one extra bit into pointers and/or sizes.~~ query the allocator: if the inline pointer is variable-size, then it's small-vec optimized. If the inline pointer is fixed size, then it's the first part of a linked list.
- Switching between these representations (e.g., when the vector's size grows or shrinks across the one-chunk threshold) should not require a data move. Thus, linked-list pointers must probably be stored at the end of the chunk. Also, we need to be able to promote/demote allocations from/to resizable in place (index may change but memory location mustn't. Maybe use high bit of indices to distinguish resizable from fixed size). 

## Questions

- [ ] Should allocators support `Segment`s? Different to `Capsule`s (which refer to types, which are outside of kladde), `Segment`s seem to be the allocator's responsibility because they restrict the order of memory allocations. But segments introduce significant complexity (when one segment grows we have to move some allocations of a neighboring segment away if we don't want to copy the complete segment out; or maybe we have to divide segments into chunks/pages anyway)
	- We should probably defer segments to later anyway. I think capsules are more important, but can hopefully be implemented more easily and completely independently from segments.
	- Alternatively: maybe implement Segments as nested allocators: the outer allocator allocates a vector of pages for the `Segment`, and the inner allocator then manages both its state and its allocated memory inside those pages. Requires the outer allocator to translate addresses, or the inner allocator to respect page boundaries, and maybe requires some gluing when an allocation inside a `Segment` spans multiple pages.
		- Advantage: this way, not only the memory of the `Segment` but also the allocator-bookkeeping for the `Segment` is loaded lazily (because all the bookkeeping is literally stored inside the `Segment`). Disadvantage: Keeping many segments in memory means we'll have to keep many allocators in memory. But segments are supposed to be large anyway, so the relative overhead might not be so bad.
		- Unclear: can we statically ensure that the correct allocator is used for every user-space edit operation inside or outside segments?
- [ ] Can `UnjournaledBackend` and `JournaledBackend` reuse some code from each other? Journal replay should do similar operations to what `UnjournaledBackend` does immediately. 
- [ ] How would compaction work in this setup? It's the allocator's job to figure out where everything should move, but the allocator needs access to the `Storage` to actually perform it. In addition, any changes to the allocator state would also have to be persisted and operations should probably be journaled, and this is `Backend` logic.
- [ ] Making pointer types associated types of `Allocator` (and thus of `Backend`) makes the layout of `Opaque` types that store pointers dependent on it, in principle. It probably also means that `Persistable<B>` will need to get a type parameter for the backend. So we'll either have:
	- `SimplePersistableVec<T, B: Backend> { in_memory: std::vec::Vec<T>, on_disk: B::UniquePointerResizable }` and `impl<T, B: Persistable> Persistable<B> for SimplePersistableVec<T, B>`;
	- or: `SimplePersistableVec<T, P> { in_memory: std::vec::Vec<T>, on_disk: P }` and `impl<T, B: Persistable> Persistable<B> for SimplePersistableVec<T, B::UniquePointerResizable>`;
	- or we'll keep it generic only at the low level, and specialize everything from the level of `Persistable` and upwards to the types actually used in kladde files. Maybe we could still make the trait definition of `Persistable` generic but set default parameters: `trait Persistable<B = JournaledBackend<...>>` if that's possible.
	- Claude: what's the best choice here? Is there a simpler way?

## Miscellaneous things to keep in mind

These items are not so urgent, maybe defer until we've ironed out the main points.
	
- Maybe don't couple `UniquePoitner<T>` to `UniquePointerFixedSize` in general. Only do this by default but allow for optimizations.
- In `Location`, use `Size` and not `Address` (and not `u32`) for `offset`.
