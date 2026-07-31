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

See [`assessment.md`](assessment.md):
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

- The default allocator implemented in `kladde-alloc` should be parameterized by its address type (the actual memory addresses used only internally by the backend and never exposed to user types), a `Size` type (used to express sizes of memory allocations, and exposed to user types), and its index/ID type (the stable identifier of pointers that survives compaction, and that is exposed to user types so they can serialize it). At least (NonZero) 32 and 64 bit integers should be supported for all of them. This makes the default allocator usable in different situations from embedded to desktop, and puts the decision about pointer size on the actual format that uses the allocator.
	- I think when used inside Kladde, the default configuration should be `Address=u64`, `Size=u32`, `Id=NonZeroU32` (nonzero because the ID is exposed to user types, who will serialize it to allocated memory of the containing data structures, and we'll eventually want to implement niche optimizations). Choosing 64-bit `Address`es allows for essentially arbitrarily large files, and limiting `Size` and `Id` to 32 bit only limits the size of *individual allocations* (to just below 4 GiB) and the *number of allocations* (to 4 billion), both of which seem fine (allocating an individual chunk of memory > 4 GiB would render the advantages of Kladde, i.e., its automatic memory management, moot anyway; and an application that creates > 4 billion allocations is probably doing something wrong because Kladde is designed to make inline memory layout the default, avoiding unnecessary indirections). The smaller `Size` and `Id` will hopefully reduce storage costs of data structures that hold a lot of pointers, and possibly reduces memory requirements of the allocator (which will likely have to store IDs more often than addresses, we'll see). Maybe we want to set the defaults of trait parameters (e.g., for `Allocator`) or define type aliases accordingly. Which one is better (default type parameters or type aliases)?
> **Claude (defaults vs. type aliases):** These are not really alternatives, because `Id`/`Address`/`Size` are *associated* types (§`trait Allocator`), and associated types cannot carry caller-overridable defaults. So: put default *type parameters* on the concrete allocator **struct** (`struct DefaultAllocator<Id = NonZeroU32, Address = u64, Size = u32>`, which *can* default), and use a **type alias** to name the assembled Kladde backend (`type KladdeBackend = JournaledBackend<FileStorage, DefaultAllocator>;`). Defaults on the struct give ergonomics; the alias gives a single name to spell. Trait-level defaults don't enter into it.

- Completely decouple `Allocator` from `Persistable` types (except maybe via an extension trait, defined in the crate where `Persistable` is defined). Allocators don't know anything about types and aren't allowed to do anything different depending on the type of the stored data. They only know the size of an allocation and whether it is fixed or dynamically sized.

## Layered architecture

Traits and structs build on each other. At the lowest level (`Allocator` and `Storage`, which are independent of each other), they are very generic and meant for reusability outside of the wider Kladde system. Subsequent layers specialize increasingly to the requirements of Kladde. However, Kladde is still the main intended use case, and I don't want to introduce any generalization that would hurt performance or ergonomics (especially for *application developers*) in the Kladde use case. For ergonomics, we should set defaults or define shorthand type aliases motivated by the Kladde use case where we can (TODO: which one is better)?

- The trait `Allocator` is the foundation for memory management. It manages a dynamic collection of address regions but doesn't connect them to memory regions (i.e., it doesn't allow reading from or writing to any memory). In fact, `Allocator` doesn't even expose the actual address ranges (because they are only relevant for `Backend` Implementations, but they're no longer relevant once we go up to the schema or `Persistable` level). Only its extension `TransparentAllocator` exposes actual address ranges.
- The traits `WriteBackend` and `Backend` extend `Allocator` and equip it with methods write to and read from some associated memory at the allocated address ranges, respectively. These are the traits against which manual implementations of `Persistable` data types for Kladde's framework have to be implemented.
- The trait `Storage` is orthogonal to `Allocator` and its descendants. While `Allocator` models memory *management*, `Storage` models memory *access*. It provides unstructured random and sequential memory access to a large block of stored data (e.g., a file).
- Concrete implementations of `WriteBackend` and `Backend` (`UnjournaledBackend` and `Journaled[Write]Backend`) are composed of a `TransparentAllocator` and a `Storage`, and they are generic over the concrete types of both of these. They perform two tasks:
	- they translate the high-level read/write operations of the `Backend` trait ("read/write bytes `x..y` of memory allocation `p`") to low-level read/write operations in an encapsulated `Storage`; and
	- they delegate `Allocator` calls to the encapsulated allocator, while replicating mutating `Allocator` calls (like `alloc_*`, `free_*`) to an in-`Storage` representation of the allocator state from which the encapsulated `TransparentAllocator` can be recreated. The backend implementations use the encapsulated `TransparentAllocator` itself to manage memory allocations that hold allocator state in-`Storage`. This way, allocator state and user data are automatically separated without having to define fixed regions in the file that are reserved for one or the other.
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

> **Claude:** Two things before the sketch.
> - **Associated pointer types are the single most consequential choice in this document, and I'd reconsider them** — see [Problems and regressions](#problems-and-regressions), item 1. In brief: they force a backend type parameter through the entire `Persistable` stack (your Question at the bottom of the doc), and the `MetaData`/`lookup` table you already introduce makes the "cache sizedness/size *in the pointer*" motivation redundant (your own line notes those queries hit the table anyway). Keeping the pointers as concrete newtypes over `Id` (shared across allocators) preserves inherent `.raw()`/`.promote()` methods *and* keeps `Persistable` un-parameterized. The sketch below is the associated-type version you described; the concrete-`Id` alternative is spelled out in the problems section.
> - `downcast` (returning an *owned* handle from a raw pointer) is really the operation `load` needs: deserialize an `Id`, reconstruct the single owner **of the correct kind**, learning sizedness on the way. I sketch it as `resolve_owned(Id) -> Option<Owned>` (an enum), which the chunked-vec test case uses to tell small-vec from linked-list. Caveat: like today's `from_index`, it can mint a *second* owner for an already-owned region, so it stays a load/allocator-internal method by convention, not something handed to application code.

A sketch (this compiles in `crates/generic-alloc`; `Word` is a helper trait bounding the generic unsigned integers — see problems item 3):

```rust
pub trait Allocator {
    type Id: Copy + Eq + Hash;              // stable, serializable (`Index` in allocator-spec.md)
    type Address: Word;                     // internal only; never exposed above TransparentAllocator
    type Size: Word + Into<Self::Address>;
    type Meta: Default;                     // per-allocation, kept in the allocator's own table

    type ResizablePointer;                  // owned, single-owner, impl-chosen representation
    type FixedPointer;                      // owned, single-owner
    type RawPointer: Copy;                  // Copy identity: carries no size and no sizedness

    // --- lifecycle (deliberately no read/write/splice -- those are on Backends) ---
    fn alloc_resizable(&self, size: Self::Size) -> Self::ResizablePointer;
    fn alloc_fixed(&self, size: Self::Size) -> Self::FixedPointer;
    fn free_resizable(&self, p: Self::ResizablePointer);
    fn free_fixed(&self, p: Self::FixedPointer);
    fn resize(&self, p: &Self::ResizablePointer, new_size: Self::Size);   // FixedPointer has no resize (the gate)

    // --- reserve an Id now, assign an Address later (journaling) ---
    fn reserve_resizable(&self, size: Self::Size) -> Self::ResizablePointer { self.alloc_resizable(size) }
    fn reserve_fixed(&self, size: Self::Size) -> Self::FixedPointer { self.alloc_fixed(size) }
    fn claim_resizable(&self, _p: &Self::ResizablePointer) {}
    fn claim_fixed(&self, _p: &Self::FixedPointer) {}

    // --- convert kinds: consume the old handle, may mint a new Id, must NOT move memory ---
    fn make_resizable(&self, p: Self::FixedPointer) -> Self::ResizablePointer;
    fn make_fixed(&self, p: Self::ResizablePointer) -> Self::FixedPointer;

    // --- erase to a Copy identity / serialize + recover the Id ---
    fn raw_resizable(&self, p: &Self::ResizablePointer) -> Self::RawPointer;
    fn raw_fixed(&self, p: &Self::FixedPointer) -> Self::RawPointer;
    fn id(&self, raw: Self::RawPointer) -> Self::Id;                       // serialize this
    fn resolve_owned(&self, id: Self::Id) -> Option<Owned<Self>>;         // reconstruct owner on load

    // --- query the table (NB: address is NOT here -- see TransparentAllocator) ---
    fn lookup(&self, raw: Self::RawPointer) -> Option<Allocation<Self>>;  // id, size, sizedness, meta
    fn size(&self, raw: Self::RawPointer) -> Option<Self::Size> { /* default: self.lookup(raw).map(..) */ }
    fn meta(&self, raw: Self::RawPointer) -> Option<Self::Meta> { /* default: self.lookup(raw).map(..) */ }
}

pub enum Owned<A: Allocator + ?Sized> { Resizable(A::ResizablePointer), Fixed(A::FixedPointer) }
pub enum Sizedness { Fixed, Resizable }
pub struct Allocation<A: Allocator + ?Sized> {
    pub id: A::Id, pub size: A::Size, pub sizedness: Sizedness, pub meta: A::Meta,
}
```

(I renamed the `reserve_*` byte-size parameters from `usize` to `Self::Size`, applying your own "call it `size`, in `Size`, not `usize`" rule from the bullet above.)


### `trait TransparentAllocator: Allocator`
- Adds a method to query for actual addresses of `RawPointer`s, and default-implemented methods for querying `UniquePointerResizable` and `UniquePointerFixedSize`. These methods *may* return `None` if the provided pointers were only reserved but never claimed.
- Also adds a required `resize_transparently` method that is like `Allocator::resize` but returns a `Some(old_address, new_address)` if resizing requires moving data.

```rust
pub trait TransparentAllocator: Allocator {
    /// `None` if `raw` was only reserved, never claimed.
    fn address(&self, raw: Self::RawPointer) -> Option<Self::Address>;

    /// Like `Allocator::resize`, but reports a relocation as `Some((old, new))`
    /// so the enclosing `Backend` can move the bytes in `Storage`.
    fn resize_transparently(&self, p: &Self::ResizablePointer, new_size: Self::Size)
        -> Option<(Self::Address, Self::Address)>;
}
```

### `trait WriteBackend: Allocator`
- Extends `Allocator` with write operations (+ splice, which combines writing with memory management).
- Does not provide `read` operations. This is deliberate: `Guard` methods should never read from the backend because it may have stale data (if we have a `JournaledBackend`). Thus,
	- we'll change the signature of `Persistable::guard` and `Persistable::store` so that they only get a `WriteBackend`. Only `Persistable::load` gets a full `Backend`.
	- The struct `JournaledBackend` below should implement `WriteBackend` but maybe not `Backend`. Instead, it could have a `flush` method that returns a shared reference to a `Backend`, which will allow reading from it but prevent writing to it while it's alive because it's a shared reference (that's what we want: users shouldn't be allowed to write to a journaled backend while they're allowed to read from it because they'd expect reads to reflect their writes, which will not be the case in a `JournaledBackend`).
- Does however provide querying allocator methods (like `size` or the above `downcast`), which may seem like they are like `read` operations and should thus be forbidden, but that's OK: any `WriteBackend` implementation will have to hold an in-memory `Allocator` that is always up to date. Only reads from *allocated memory* are forbidden in `Guard` methods; querying the allocator state is OK and often unavoidable.
- `write` should maybe take a `RawPointer` and a `size: Size`, and return an `impl Write` (`UnjournaledBackend` below simply hands out the `&mut Storage` after seeking to the position; for `JournaledBackend`, we should check if we can write the `Op` header and then simply return the `&mut Storage` to let the user fill in the rest of the op. It seems a bit dangerous because it would break not only if the user overwrites but even if they write less than promised). But it would be nice to allow users to write through a `Write` because implementations of `Persistable` types might realistically want to call `write_vectored`

```rust
pub trait WriteBackend: Allocator {
    /// A writer positioned at `raw`, promising exactly `size` bytes.
    fn write_at(&self, raw: Self::RawPointer, size: Self::Size) -> impl Write + '_;
    /// Atomic resize + tail-shift + content overwrite of one region.
    fn splice(&self, p: &Self::ResizablePointer, offset: Self::Size, old_len: Self::Size, new: &[u8]);
}
```

> **Claude:** You already sense the danger, and it's real: **handing out a bare `impl Write` for the *journaled* backend can corrupt the op frame.** The journal's crash-safety rests on each op being a length-prefixed, checksummed frame (see `spec.md`, "On-Disk Layout"). If you write the header promising `size` bytes and then the caller writes fewer (or more), the frame's length/checksum no longer match — recovery will (correctly) discard a torn frame and *silently lose the write*. So I'd keep "return `impl Write`" only for the **unjournaled** backend (no frame to corrupt), and make the **journaled** `write` take the bytes (it already knows `size`) or `&[IoSlice]` for the `write_vectored` case, writing header+payload+checksum itself. Separately, `write_at(&self) -> impl Write` collides with the single storage cursor and `&self` — see the `Backend` note below and problems items 4–5.
- Adds a `read` method. It should take a `RawPointer` and an `offset: Size` and return an `impl Read + Seek` (the backends below will simply hand out the actual `&mut Storage`, after seeking to the appropriate position)
- Still implements `write` (because it is also a `WriteBackend`) and mutating allocator methods (because it is also an `Allocator`, and `Allocator` isn't split into 2 traits for querying and modifying). But that's OK: if you want to prevent writing while reading is allowed, never hand out an owned `Backend`, only a shared reference `&impl Backend`.
	- [ ] TODO: is this true? It seems like `write` and `alloc_*` only take `&self`. That's probably to make `Guard` implementations more ergonomic. But it will require some `Cell` or `RwLock` gymnastics -- can we elevate that to the `WriteBackend` level so that `Allocator` has a more normal API that takes `&mut self` for mutating methods?

```rust
pub trait Backend: WriteBackend {
    /// A reader positioned at `raw + offset`.
    fn read_at(&self, raw: Self::RawPointer, offset: Self::Size) -> impl Read + Seek + '_;
}
```

> **Claude (answering the `&self`/`&mut self` TODO):** Keep the mutating methods `&self`. It is *not* gratuitous: the guard model relies on it. A parent guard holds `&B` and hands each nested field guard the *same* `&B` by reborrow; if mutating methods were `&mut self`, two sibling field guards would each need `&mut B` and conflict. Elevating to `&mut self` would break exactly the nested-guard ergonomics the whole design exists for, and the "`Cell`/`RwLock` gymnastics" is the contained price of that.
> But `&self` collides with returning `impl Write`/`impl Read + Seek` over a *single* storage cursor: **`Seek::seek` takes `&mut self`, so a reader that seeks into the one backing cursor cannot be handed out from a `&self` method** (my prototype hit this and had to stub `read_at`). You can have any *two* of {`&self` mutating methods, returning `impl Write`/`impl Read`, a single storage cursor} but not all three. Cleanest reconciliation: `&self` + `RefCell<Storage>`, and have `read`/`write` *take* the bytes (or a `&mut dyn Write`/`&mut dyn Read` callback) instead of *returning* a long-lived `impl Write`/`impl Read`. That gives up the `write_vectored`-through-a-returned-`Write` convenience but keeps `&self` and (for the journaled backend) frame control. See problems items 4–5.

### `trait Storage: Read + Write + Seek`
- Provides both random and sequential read and write access (via `Read` and `Write`, and `Seek` for random access), plus resizing (via a trait method), in some abstract storage. The storage could be a file but doesn't have to be.
- A concrete implementation might reserve a tiny fixed-size header region that it manages itself by offsetting addresses accordingly (the header should only hold very basic information like a magic number and parameters of the allocator; any other header data should go into an allocated memory region so that the header size can evolve without having to rewrite the whole file).
- A `#[cfg(test)] struct MockStorage(Vec<u8>)` should implement `Storage` in memory for tests. Don't make it public because I can't yet see a use case outside of tests for it. We'll eventually want to add a public in-memory storage for testing `Persistable` user types, but that should probably be a `MockBackend` that implements `Backend`, not a `MockStorage`.

```rust
pub trait Storage: Read + Write + Seek {
    fn resize(&mut self, new_len: u64) -> io::Result<()>;
    fn len(&self) -> io::Result<u64>;
}

// test-only, in-memory: a Cursor gives it the Seek that a bare Vec<u8> lacks.
#[cfg(test)]
struct MockStorage(std::io::Cursor<Vec<u8>>);
```

> **Claude:** small correction — `struct MockStorage(Vec<u8>)` can't implement `Storage` directly, because `Vec<u8>` implements `Write` but not `Seek`. Wrap it in `std::io::Cursor<Vec<u8>>` (which is `Read + Write + Seek`), as the prototype does. `resize` then goes through `cursor.get_mut().resize(..)`.

### `struct UnjournaledBackend<S: Storage, A: TransparentAllocator<Meta=...>>`
- Implements `Backend` (and therefore `WriteBackend` and `Allocator`) but probably does not implement `TransparentAllocator` (because true addresses should never be exposed to the implementations of user types since they're not stable and users can't read from or write to arbitrary memory addresses anyway).
- Holds:
	- an `S`to manage on-file state and
	- an `A` for managing its in-memory state.
- Implements read / write calls of `Backend` by using the `TransparentAllocator` to resolve pointers to addresses and then reading from / writing to the `Storage`
- Forwards `Allocator` calls to its in-memory allocator `A`. For modifying allocator calls (allocating / freeing / resizing), forwards to its in-memory allocator but also immediately persists a compact version of the state to the `Backend` (this `Backend` representation of the allocator state is not optimized for fast lookups. It will only be used to recreate the in-memory allocator state when we load a Kladde file).
- Holds its own immediate (inline fixed-size) state probably at a fixed address in the file (which it shifts away in all `Backend::{read, write, seek}` calls), but then all indirect parts of the state in memory allocations managed by the in-memory allocator. For the `Storage` representation of the allocator, the allocator-related allocations may have to be stored in a special bootstrapped way, different from how those allocations then contain the tables of allocations that hold user data.
- Probably not really useful for Kladde, but a good test of the versatility of the allocator design, and probably a good first step before implementing `JournaledBackend`.

```rust
pub struct UnjournaledBackend<S: Storage, A: TransparentAllocator> {
    storage: S,   // on-file bytes
    alloc: A,     // in-memory allocation table
}
// impl Allocator      -> forwards to `alloc`
// impl WriteBackend   -> alloc.address(raw) -> storage.seek(..) -> write
// impl Backend        -> alloc.address(raw) -> storage.seek(..) -> read
// does NOT impl TransparentAllocator (addresses stay hidden from user types)
```

(The prototype implements exactly this skeleton and it composes — modulo the `read_at`/`&self`/`Seek` snag above, which it stubs.)

### `struct JournaledBackend<S: Storage, A: TransparentAllocator<Meta=...>>` (and `JournaledWriteBackend`)
- Similar to `UnjournaledBackend` except that only `JournaledBackend` provides read operations. The two types are probably newtype wrappers around a shared private inner type, and they should have methods to turn one into the other (e.g., `JournaledWriteBackend::flush(&mut self) -> &JournaledBackend`; never hand out an owned `JournaledBackend` because that would allow reading and writing).
- Forwards any `Allocator` method calls to the in-memory allocator but only calls `reserve_*` instead of `alloc_*`.  Appends a corresponding `Op` to the journal. The `Op` for allocation encapsulates the index of the returned allocation, its size, and whether it's resizable, but not the actual address because that doesn't even get assigned by `reserve_*` and is deferred to journal replay (which calls `claim_*`). Deferring address assignments will allow us to grow the journal at the end of the file without bumping into any address regions that are already reserved for memory allocations by `Op`s that are earlier in the journal.
- May need to hold some additional data in memory to tie the in-memory state to the persisted state.
- Uses the inner `Allocator` to manage its journal, but doesn't use the generic journaling method to allocate new journal memory because that might lead to an infinite loop. Not sure how to resolve this. Probably need to use the `Storage` header for it.
- allocating on a `JournaledAllocator` doesn't actually pick an address yet. It only records a new index and the allocation size (and whether it's resizable) in the journal. True addresses get assigned on journal replay. 
- Implements journal replay and calls it automatically when the journal overflows and we're not inside a transaction. Journal replay assigns actual addresses to new allocations, and resolves the addresses of any memory reads/writes on the journal.

```rust
struct JournaledInner<S: Storage, A: TransparentAllocator> {
    storage: S,
    alloc: A,
    journal: /* append-only, framed Op log; see spec.md On-Disk Layout */,
}
pub struct JournaledWriteBackend<S, A>(JournaledInner<S, A>);  // impl WriteBackend only
pub struct JournaledBackend<S, A>(JournaledInner<S, A>);       // impl Backend (read allowed post-flush)

impl JournaledWriteBackend<S, A> {
    fn flush(&mut self) -> &JournaledBackend<S, A> { /* replay, then reborrow as read-only */ }
}
// alloc_* -> alloc.reserve_*(size) + append Alloc-Op (id, size, sizedness; NO address)
// replay  -> alloc.claim_*(..)     + apply pending writes at now-known addresses
```

> **Claude (re your Question "can the two backends share code?", and the compaction Question):** yes — journal **replay** is essentially "run the `UnjournaledBackend` immediate path over the buffered ops." Factor a single `fn apply(alloc: &mut A, storage: &mut S, op: Op)` that both call: `UnjournaledBackend` calls it inline per mutation, `JournaledBackend` calls it per op during replay. The prototype's `UnjournaledBackend` write path *is* that body. And compaction (Question 3) becomes markedly simpler under Id-serialization: the allocator computes a move plan `[(id, old_addr, new_addr)]`, the backend performs the `storage` moves and updates the table — and because serialized pointers are `Id`s, **no on-disk pointer needs rewriting** (contrast the current design, which must rewrite every serialized target address). That is the upside of the handle-table tradeoff flagged in problems item 2.

### `trait AllocatorExt: Allocator`

Extension trait with a blanket implementation for every `Allocator`, but defined in the crate where `Persistable` is defined. See Section 4.3 in `allocator-spec.md` except for the following nitpicks:
- Don't call methods `*_boxed`. Call them `*_typed` instead.
- Add `promote<T>` and `demote` methods to convert between `UniquePointer<T>` and `UniquePointerFixedSize` (they should probably be on the pointer types themselves but that's only possible for `UniquePointer<T>::demote` but not for `UniquePointerFixedSize::promote` since `UniquePointerFixedSize` is defined externally).
- Split `alloc_array` into two methods:
	- `alloc_fixed_size_array<T: Persistable>`, which returns a `UniquePointerFixedSize`; and
	- `alloc_resizable_array<T: Persistable>`, which returns a `UniquePointerResizable`

```rust
// Defined in the `Persistable` crate; blanket-impl'd for every `Allocator`.
pub trait AllocatorExt: Allocator {
    fn alloc_typed<T: Persistable>(&self) -> UniquePointer<T>;               // Box<T>, size = T::INLINE_SIZE
    fn free_typed<T>(&self, p: UniquePointer<T>);
    fn alloc_fixed_size_array<T: Persistable>(&self, len: usize) -> Self::FixedPointer;
    fn alloc_resizable_array<T: Persistable>(&self, len: usize) -> Self::ResizablePointer;
    // promote/demote between `UniquePointer<T>` and `Self::FixedPointer`
}
impl<A: Allocator + ?Sized> AllocatorExt for A {}
```

> **Claude:** naming (`*_typed`, `alloc_{fixed_size,resizable}_array`) is fine. The catch is `UniquePointer<T>`: with associated pointer types it must wrap `Self::FixedPointer`, so it becomes `UniquePointer<T, A::FixedPointer>` (or must carry the allocator) — the same generics infection as your Persistable Question. `promote`/`demote` can only be inherent on `UniquePointer<T>` (local) → `demote` yes; `promote` from `FixedPointer` must be a trait method here since `FixedPointer` is foreign, exactly as you note. All of this evaporates if pointers stay concrete (problems item 1), where `UniquePointer<T>` is just `FixedPointer` + `PhantomData<T>` as in `allocator-spec.md`.

## Problems and regressions

Written by Claude. This section collects the problems this proposal may run into and the concrete regressions relative to what is implemented today (the `allocator-spec.md` overhaul, now landed) or planned there. Items 1–4 I confirmed against the throwaway prototype in `crates/generic-alloc`; the rest are design-level.

1. **Associated pointer types infect the whole `Persistable` stack with a backend type parameter, and the ergonomic pointer methods are lost.** If `ResizablePointer`/`FixedPointer`/`RawPointer` are `Allocator` associated types, then every container that stores a pointer inline must name *which* allocator's pointer it holds. That is your Question at the bottom of the doc, and the answer propagates: `Persistable` gains a `B` (or `Id`) parameter, which spreads to every `#[derive(Persistable)]` type, every generated guard, and every application `struct` — a real ergonomics and compile-time cost for exactly the audience (application developers) the "Layered architecture" section says it wants to protect. Inherent methods (`p.raw()`, `p.into_fixed()`, `p.promote()`) also become impossible (you can't add inherent methods to an opaque associated type) and must move onto the allocator/`AllocatorExt` (`allocator.raw_resizable(&p)`), which is markedly clunkier than today's `p.raw()`.
   - **The motivation is largely redundant with `MetaData`.** The stated reason for associated pointers (line ~57: let allocators "cache some metadata like whether it's fixed-size … in the pointer") is exactly what the `MetaData`/`lookup` table already provides — and you note yourself (line ~66) that sizedness/size queries hit that table anyway. So the table already buys the metadata; the pointer needn't.
   - **Recommendation:** keep the pointers **concrete newtypes over `Id`** (`Resizable<Id>`, `Fixed<Id>`, `Raw<Id>`, `UniquePointer<T> = Fixed<Id> + PhantomData<T>`), shared across all allocators, exactly as `allocator-spec.md` has them but generic over `Id` (defaulting to `NonZeroU32`). Then inherent methods stay, `Persistable` needs no backend parameter (at most an `Id` parameter, defaulted), and allocators still differentiate behavior via the `Meta`/`lookup` table. This is your own third option, and I think it is clearly best. The only thing you give up is allocator-specific *bit-packing of sizedness into the `Id`* — an optimization that saves one table lookup you're doing anyway.
   - The fixed/resizable **type gate** (allocator-spec §3: `resize`/`splice` only on the resizable handle) survives either way — good, no regression there.

2. **Serializing `Id` instead of the on-disk target reinstates the handle-table design `spec.md` deliberately rejected.** Today a pointer serializes as its current on-disk *target address* (self-locating; compaction rewrites the single owning copy, tracked by a position index). Here it serializes as its `Id`, so the persisted `Id → Address` table becomes **load-bearing**: nothing on disk can be located on open without it. That is precisely the "dense on-disk handle table" that `spec.md`'s *Alternatives Considered* set aside for footprint reasons. The trade is legitimate and has real upsides — stable serialized pointers, **zero pointer rewrites on compaction** (see the compaction note above), cleaner decoupling — but it is a **reversal of a recorded decision** and should be made with eyes open: you now pay a persisted indirection table's footprint, and it is mandatory rather than a reconstructable optimization. (Per-access runtime cost is roughly a wash: both designs already do an in-memory `index → address` lookup.)

3. **Generic integer associated types need an "unsigned word" bound that std doesn't provide.** Making `Address`/`Size` associated types means every offset/size computation needs `+`, `<`, `Into<Address>`, and `usize` conversions over a generic type. Rust has no single "unsigned integer" trait, so you need a helper trait (the prototype's `Word`) or a dependency like `num-traits`, and its bounds ride along on every generic function that does address arithmetic. Manageable, and `Size: Into<Address>` is the right core relation, but it is real bound-noise that the current concrete-`u32` code doesn't have. (You'll also want `TryFrom<usize>`/`to_usize`, because offsets from Rust collections arrive as `usize`.)

4. **`read` returning `impl Read + Seek` cannot be a `&self` method over a single storage cursor.** `Seek::seek` takes `&mut self`; a reader that seeks into the one backing `Storage` cursor therefore needs `&mut` access to it, so `Backend::read(&self) -> impl Read + Seek` can't hand out the real cursor from `&self` — it needs `&mut self` or `RefCell<Storage>` (which then serializes readers, defeating handing out several). The prototype hit this and had to stub `read_at`. Combined with item 5, you can pick any **two** of {`&self` mutating methods, returning `impl Read`/`impl Write`, a single storage cursor}. Practical resolutions: `read(&mut self)` (giving up concurrent readers — fine for single-writer), or return an owned buffer (today's `Vec<u8>`, losing zero-copy), or a positioned-read API that borrows `&Storage` immutably and carries its own offset (`Read` but not `Seek`).

5. **`&self` interior mutability vs. `&mut self` (your TODO).** Keep mutating methods `&self`. The guard model depends on it (nested field guards reborrow the same `&B`); `&mut self` would make sibling field guards conflict. So the "`Cell`/`RwLock` gymnastics" isn't gratuitous — it's the price of the guard ergonomics, and it's contained in the backend. But `&self` collides with returning `impl Write`/`impl Read` (item 4): the clean reconciliation is `&self` + `RefCell<Storage>` with `read`/`write` **taking** bytes / a `&mut dyn Write` callback rather than **returning** a long-lived writer. That costs the `write_vectored`-through-a-returned-`Write` convenience.

6. **A journaled `write` that returns a bare `impl Write` can corrupt the op frame.** The journal's crash-safety rests on each op being a length-prefixed, checksummed frame (`spec.md`, On-Disk Layout). Hand out `&mut Storage` after a header promising `size` bytes and let a `Persistable` author write fewer/more, and the frame's length/checksum no longer agree — recovery discards the torn frame and *silently drops the write*. Keep "return `impl Write`" for the unjournaled backend only; make the journaled `write` take the bytes (or `&[IoSlice]`) and write header+payload+checksum itself.

7. **`reserve`/`claim` adds a "valid but unclaimed" state whose full semantics need pinning down.** Allowing *every* `Allocator` method on reserved-but-unclaimed pointers (line ~63) means `free`, `resize`, `make_fixed`, … must each define their effect on a reservation. It's expressible (the prototype models a reservation as a table row with `address: None`), but write down the state machine (reserved → claimed → freed, and reserved → freed = cancel), and note that `WriteBackend`/`Backend` returning an error for I/O on unclaimed pointers turns every write path fallible where it wasn't before.

8. **Self-hosting bootstrap is a genuine open problem, not a detail.** Storing the allocator's own state in allocations it manages, and its journal in memory it allocates, is circular (lines ~56, ~110): you can't journal the allocation of journal space, and you can't read the allocator table without first knowing where it lives. This needs a bootstrap anchor *outside* the general mechanism — the fixed `Storage` header holding the root address of the persisted allocator state, plus journal space managed specially. The current implementation avoids this entirely because the mock allocator is in-memory and never persists its state; making the allocator self-hosting is new, load-bearing work.

9. **General over-generalization risk.** The document itself worries (twice) about generalizations hurting the Kladde common case. The associated-type pointers are the main instance, but the whole "generic over `Id`/`Address`/`Size`, pointers as associated types, `impl Write`/`impl Read` returns, reserve/claim, `MetaData`" surface is a large jump in API and monomorphization from the concrete, landed `allocator-spec.md`. Most of it is defensible for the "reusable standalone allocator" goal, but I'd gate each piece on a concrete need (the chunked-vec test case is a good forcing function) rather than adopting all of it up front. Concretely, the pieces I'd keep without hesitation: separating `Storage` from `Allocator`; the generic `Id`/`Address`/`Size` (as *struct* params with defaults); `reserve`/`claim`; `size` (not `capacity`); `Meta`/`lookup`. The pieces I'd defer or drop: pointers as *associated* types (item 1), returning `impl Write`/`impl Read` from `&self` (items 4–6), and segments (deferred already).

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
	- > **Claude:** agree — defer segments; capsules first. On the static-safety sub-question: with *nested* allocators, a pointer minted by the inner (segment) allocator is meaningless to the outer one, and nothing in the type system stops you from passing it to the wrong allocator unless the pointer *carries* its allocator's identity. The two ways to get that statically are (a) generativity/brand lifetimes (`GhostCell`-style invariant lifetime tokens tying a pointer to one allocator instance) — sound but viral and painful for application ergonomics, or (b) making the segment part of the pointer's *type* — which multiplies the pointer-type-parameter problem from item 1. Realistically you'll want a **runtime** check (the pointer's `Id`/high bits encode which segment; the wrong allocator returns `None` from `lookup`). So: defer, and when you do it, expect a runtime segment check rather than a compile-time guarantee. The lazy-bookkeeping advantage you note is real and attractive.
- [ ] Can `UnjournaledBackend` and `JournaledBackend` reuse some code from each other? Journal replay should do similar operations to what `UnjournaledBackend` does immediately. 
- [ ] How would compaction work in this setup? It's the allocator's job to figure out where everything should move, but the allocator needs access to the `Storage` to actually perform it. In addition, any changes to the allocator state would also have to be persisted and operations should probably be journaled, and this is `Backend` logic.
- [ ] Making pointer types associated types of `Allocator` (and thus of `Backend`) makes the layout of `Opaque` types that store pointers dependent on it, in principle. It probably also means that `Persistable<B>` will need to get a type parameter for the backend. So we'll either have:
	- `SimplePersistableVec<T, B: Backend> { in_memory: std::vec::Vec<T>, on_disk: B::UniquePointerResizable }` and `impl<T, B: Backend> Persistable<B> for SimplePersistableVec<T, B>`;
	- or: `SimplePersistableVec<T, P> { in_memory: std::vec::Vec<T>, on_disk: P }` and `impl<T, B: Backend> Persistable<B> for SimplePersistableVec<T, B::UniquePointerResizable>`;
	- or we'll keep it generic only at the low level, and specialize everything from the level of `Persistable` and upwards to the types actually used in kladde files. Maybe we could still make the trait definition of `Persistable` generic but set default parameters: `trait Persistable<B = JournaledBackend<...>>` if that's possible.
	- Claude: what's the best choice here? Is there a simpler way?
	- > **Claude:** The simpler way is to **not make the pointer types associated at all** — then this whole question disappears and `Persistable` needs no `B` parameter (this is [Problems and regressions](#problems-and-regressions) item 1). Keep the *serializable* pointer a concrete newtype over `Id` (e.g. `struct Pointer<Id = NonZeroU32>` / `Resizable<Id>` / `Fixed<Id>`), shared across all allocators, and let allocators keep any per-allocation metadata in the `Meta`/`lookup` table you already have. `SimplePersistableVec<T>` then just stores `Resizable<Id>` inline, no `B`.
	>   If you insist on associated pointer types, your **third** option is the least-bad: keep it generic only at the very bottom and specialize from `Persistable` upward, and — yes — `trait Persistable<B = KladdeBackend>` with a default *does* work in Rust (default trait type parameters are allowed; only *associated* types can't default). Your first two options both spread `B` through every derived type and guard; the second (`impl Persistable<B> for Vec<T, B::ResizablePointer>`) reads slightly cleaner but is the same infection. Net recommendation: concrete `Id`-newtype pointers; make `Persistable` generic over `Id` only if you truly want 32/64-bit `Id` to be a per-file choice, and default it.

## Miscellaneous things to keep in mind

These items are not so urgent, maybe defer until we've ironed out the main points.
	
- Maybe don't couple `UniquePointer<T>` to `UniquePointerFixedSize` in general. Only do this by default but allow for optimizations.
	- > **Claude:** this is another face of item 1 — decoupling `UniquePointer<T>` from a specific `FixedPointer` representation *is* the associated-type direction, with the same downstream cost. If you keep pointers concrete, `UniquePointer<T> = FixedPointer + PhantomData<T>` (as in `allocator-spec.md`) is the simple default and I'd not generalize it until a concrete optimization demands it.
- In `Location`, use `Size` and not `Address` (and not `u32`) for `offset`.
	- > **Claude:** agree. `offset` is a displacement *within* an allocation, so it's bounded by that allocation's `Size`; `Address` (absolute, `u64`) would be wrong-typed, and `u32` hard-codes what you're trying to parameterize. (`anchor` stays the erased pointer, not an `Address`, since user-level `Location`s never see addresses.)

## New idea after the feedback

I think some of the problems you raised are indirect results of my choice to make `WriteBackend` and `Backend` be extensions of `Allocator`. I'm now reconsidering this decision. I think it might be cleaner if `Allocator` really only deals with the management of address ranges, never with the actual data stored in some memory at those addresses -- that part is completely reserved for `Backend` / `WriteBackend`. Thus:
- `TransparentAllocator` becomes obsolete: every `Allocator` supports resolving to addresses (except for allocations that are only reserved and not claimed if the allocator supports this)
- No awkward `resize` and `resize_transparently` split: `resize` always returns an `Option<(old_address, new_address)>` because every allocation has an address (for reserved but unclaimed addresses: I it's semantically correct to return `None` here because the address doesn't exist yet so it can't change either; and that's exactly what my motivating `JournalingBackend` use case needs in this case)
	- TODO: not clear what to do in a journaled scenario where resizing of an already claimed allocation puts it after the current journal, thus preventing the journal from growing. We kind of need a `reserve` variant of `resize` as well.
- `Backend` and `WriteBackend` no longer automatically implement `Allocator`. Instead, `Backend` and `WriteBackend` have their own methods for allocating, resizing, etc.
	- This allows us to do some deviations from the `Allocator` API where appropriate: `Allocator::resize` returns addresses if memory has to be moved but `WriteBackend::resize` never returns addresses because users of backends are never supposed to see any addresses.
	- `Backend` is no longer forced to extend `Allocator`, maybe we should make the split differently: currently, we have `WriteBackend` (for write-only access) and `Backend` (for read+write access that I tried to turn into read-only access by hiding it behind a shared reference but that didn't really work). Now we could instead split it into `WriteBackend` (same as now) and `ReadBackend` (no longer an extension of `Backend`, implements only read access, i.e., no `alloc_*` or `resize_*`, no `write`). This is somewhat analogous to `std::io::Read` and `std::io::Write`.

> **Claude (analysis of "New idea after the feedback"):** Yes — this makes sense, and I think it's the right call. It's the cleanest resolution of the biggest tension in my feedback, and your diagnosis (that several problems were *indirect results of `Backend: Allocator`*) is largely correct. Details, then trade-offs.
>
> **The strongest argument *for* it, which you don't quite state:** the backend's ops are not thin forwards of the allocator's — they are genuine *translations*, and `resize` proves it. `Allocator::resize` returns `Some((old, new))` on a move; `WriteBackend::resize` must *consume* that to copy `min(old, new)` bytes in `Storage` and then return nothing (users never see addresses). So `WriteBackend::resize` ≠ `Allocator::resize` in return type *and* in what it does. The same holds for `read`/`write` (resolve id → address → seek Storage). `Backend: Allocator` forced these genuinely-different operations to share one signature, which is exactly why the address-returning methods kept leaking upward and why `TransparentAllocator` had to exist. Composition (`Backend` *has an* `Allocator`) instead of extension is the correct shape. So: strong yes on 1c.
>
> **This is what fixes the `&self`/`&mut self` knot (my problems item 5), cleanly.** Decoupled, the layers can choose independently: give `Allocator` a *normal `&mut self`* mutating API (nice for standalone reuse, tooling, compaction), and let `Backend` be a `&self` facade that borrows the inner allocator through a `RefCell` per call. The "`Cell`/`RwLock` gymnastics" then live *only in the backend adapter* and never touch the reusable `Allocator` — which is exactly the separation you want. I had said "keep everything `&self`"; the better answer, which your restructure enables, is "`Allocator` is `&mut self`, `Backend` is the `&self` facade."
>
> **The `ReadBackend`/`WriteBackend` split (1d) genuinely fixes what the shared-reference trick couldn't.** `&Backend` still let you write because `Backend` *had* `&self` write methods; a `ReadBackend` with *no write methods at all* cannot. The `io::Read`/`io::Write` analogy is apt. And it composes with the phase model: `JournaledWriteBackend: WriteBackend` during the write phase, `flush(&mut self) -> impl ReadBackend` afterward — the `&mut self` on `flush` guarantees no live guard (guards hold `&self`), and the returned type simply has no write surface.
>
> **A bonus the split unlocks — read can be `&mut self`, which dissolves the `Seek` problem (my item 4) for reads.** `load` is *sequential* (load field A fully, then B, then …; a container loops one element at a time), so a `&mut ReadBackend` reborrowed down the recursion never needs two simultaneous borrows. That means `ReadBackend::read(&mut self) -> impl Read + Seek` is fine — no `RefCell<Storage>` needed on the read path, and you get the real seekable cursor. Meanwhile writes/allocs stay `&self` for the guard reborrow model. So the natural asymmetry is **read = `&mut self`, write = `&self`**, and it falls out of exactly *why* each exists (load is sequential; guards share a `&B`). As a free side effect, the borrow checker then forbids `load` (needs `&mut B`) while any guard (holds `&B`) is alive — a correct enforcement of your "don't read stale data mid-write" rule.
>
> **Trade-offs / things to pin down:**
> - **Duplicated surface.** `alloc_*`/`free_*`/`resize_*`/`lookup`/`size` now appear on both `Allocator` (address-aware) and `WriteBackend`/`ReadBackend` (address-hidden). That's real boilerplate — each backend forwards-and-translates every op. Mostly mechanical; a default-method layer or a small macro can absorb it. It's the price of the (worthwhile) separation.
> - **`resize` returning `Option<(old, new)>` conflates two "no move" cases.** `None` means both *unclaimed* (no address yet) and *in-place claimed* (address unchanged). The backend still has work in the in-place-*grow* case (ensure `Storage` covers `[addr, addr+new_size)`), which the `Option` doesn't signal. Either make it a 3-way result (`Relocated{old,new}` | `InPlace{addr}` | `Unclaimed`), or keep `Option` and have the backend size `Storage` by *querying* the pointer's address+size after every resize regardless. Fine either way, just decide.
> - **Your `resize`-reserve TODO is real and it's the right instinct.** A journaled backend must not let a live resize assign a new address (it could land past the journal). The fix is the same deferral you already use for `alloc`: record a *reserve-resize* op now, assign the address on replay via `claim`. So this **generalizes reserve/claim from `alloc` to `resize`** (and arguably to any address-assigning op) — which is a good sign the reserve/claim mechanism is the right primitive, but it does add surface (a reserved-resize state on top of the reserved-alloc state in problems item 7).
> - **Scope check — what this restructure does *not* touch.** It's orthogonal to, and does not resolve, problems items **1** (associated vs. concrete pointer types → `Persistable` parameterization), **2** (serializing `Id` = handle table), **3** (the `Word` bound), **6** (journaled `write` framing — still take bytes, don't return a raw `impl Write`), and **8** (self-hosting bootstrap). Worth keeping those separate in your head so this clean win doesn't get over-credited. In particular, item 1 is still the one I'd most want you to settle, and it's independent of this.
> - **Naming.** With the split there may be no single "`Backend`" anymore. If a lot of code wants both, keep a convenience `trait Backend: ReadBackend + WriteBackend {}` (blanket-impl'd) so callers can still spell one bound; but define the real capabilities on the two halves.
>
> Net: adopt 1a (drop `TransparentAllocator`; addresses are core to `Allocator`, hidden by the `Backend`), 1c (composition, not extension), and 1d (`ReadBackend`/`WriteBackend`, `read = &mut self` / `write = &self`). Firm up the `resize` result shape and add the reserve-variant of `resize`. This removes `TransparentAllocator`, the `resize`/`resize_transparently` split, and the `&self`-vs-`&mut self` and read-isolation problems in one coherent move.
