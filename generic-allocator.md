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
> **Decision (pointer width):** settled via default *type parameters*. The width lives in one place, `struct Pointer<W = NonZeroU32>(W)` (§`Pointer`), and everything above parameterizes over the *pointer type* `P` with a default (`trait Persistable<P = Pointer>`, `struct PersistableVec<T, P = Pointer>`). The allocator's own `Address`/`Size` similarly take struct-level defaults (`struct DefaultAllocator<Address = u64, Size = u32>`), and a type alias names the assembled Kladde backend (`type KladdeBackend = JournaledBackend<FileStorage, DefaultAllocator>`). There is no separate `Id` type any more — the stable id *is* the width `W` carried inside `Pointer<W>`.

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

### Pointer types (concrete)

The pointer types are concrete and shared across all allocators; only `Pointer` carries the raw width `W`, and everything above parameterizes over the *pointer type* `P` (default `Pointer`). Verified in `crates/generic-alloc`:

```rust
/// Copy, type- and size-erased identity: the serialized form of a pointer and
/// the `anchor` of a `Location`. The only type parameterized over the width W.
pub struct Pointer<W = NonZeroU32>(W);               // : Copy

/// Owned, single-owner handles, over the pointer type P (default `Pointer`).
pub struct UniquePointerResizable<P = Pointer>(P);   // not Copy
pub struct UniquePointerFixedSize<P = Pointer>(P);   // not Copy
pub struct UniquePointer<T, P = Pointer> {           // the typed Box<T>
    inner: UniquePointerFixedSize<P>,
    _marker: PhantomData<*const T>,
}

pub struct Location<P = Pointer> { pub anchor: P, pub offset: Size }
```

Inherent methods on the owned handles: `.raw() -> P`, and `UniquePointer::{from_fixed, into_fixed}`. The fixed/resizable **gate** (`resize`/`splice` accept only `UniquePointerResizable`) is unchanged from `allocator-spec.md`.

### `trait Allocator`
- Has associated types `Pointer`, `Address`, and `Size` (most implementors are parameterized by the latter two, e.g. `MyAllocator<Address, Size> { ... }`).
	- `Pointer` is the concrete `Copy` identity `Pointer<W>` (§`Pointer`) the allocator hands out — the stable id, serialized into other allocations, previously called `Index` in `allocator-spec.md`. It stays a *concrete* type (not opaque/associated), so the owned handles keep inherent `.raw()`/`.into_fixed()` methods and `Persistable` needs no backend type parameter, only the pointer type `P` — which a `Persistable<P>` pins to the backend via `Backend<Pointer = P>`.
- Manages a set of non-overlapping `Address` ranges, where `Address` is an unsigned integer type that should probably somehow generically support `NonZero`.
- The trait definition does not assume any connection to persistency. It doesn't interpret the integer ranges it manages as memory and doesn't provide read or write access to them (that's what `Backend` is for), and you can implement an `Allocator` that holds its entire state in memory and doesn't deal with persistence at all. A persisted allocator then typically builds on top of an in-memory allocator, somewhat analogous to how, at a higher level, types that implement `Persistable` (like `PersistableHashMap<K,V>`) often use standard library types (like `std::collections::HashMap<K,(V,Index)>`) for their in-memory representation.
	- Concretely, this means that, different to the current `allocator-spec.md`, this new `Allocator` does not have the methods `read`, `write`, and `splice` (those go in `Backend` below)
- Maybe exposes some `alloc_scratch` (allocate largest contiguous memory within the file) and/or `alloc_at_end` method if that is needed for journaling. Maybe not necessary if we can simply use the standard allocation methods and then query whether we happen to be at the end of the file, exploiting that a persisted allocator that builds on top of a *concrete* in-memory allocator may make assumption about the in-memory allocator's specifics beyond the `Allocator` trait.
- Should probably be self-hosting. I.e., the `UnjournaledBackend` below  uses the Allocator itself to manage the memory regions where in-file representation of the Allocator is stored. It will probably need special logic to manage it in-file, but that's separate.
- The pointer types (`UniquePointerResizable<P>`, `UniquePointerFixedSize<P>`, and the `Copy` `Pointer<W>`) are **concrete**, parameterized over `P`/`W` — *not* per-allocator associated types. (An earlier draft made them associated so an allocator could cache metadata like sizedness *in* the pointer; the `MetaData`/`lookup` table below already provides that, so the pointers stay concrete — which keeps inherent pointer methods and keeps `Persistable` free of a backend type parameter.)
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
	- `Allocator` should support recovering an *owned* handle of the correct kind from a `Pointer` (this is what `load` needs — deserialize a `Pointer`, reconstruct the single owner, learning sizedness on the way): `fn resolve_owned(Pointer) -> Option<Owned>` with `enum Owned { Resizable(UniquePointerResizable), Fixed(UniquePointerFixedSize) }`; and a `lookup` returning size + sizedness + `MetaData`. (Caveat: like `from_index` today, `resolve_owned` can mint a *second* owner for an already-owned region, so it stays a load/allocator-internal method by convention.)
	- We should also require methods to convert between fixed sized and resizable allocations. These consume the old pointer and return a new pointer (possibly with a new `Id` because some `Allocator`s might use a bit in the `Id` as a flag for fixed size vs resizable).
	- Don't use the term `capacity` here when referring to allocation sizes. Always call it `size` (also not `byte_size`) because that's what it is *from the perspective of the allocator*. The implementation of a `Persistable` type might interpret the *size of the allocation* as a *capacity of a container* but that's at a higher level of abstraction.

A sketch (the concrete pointer shapes are verified in `crates/generic-alloc`; `Word` is a helper trait bounding the generic unsigned integers — see the Word-bound item in [Problems and regressions](#problems-and-regressions)). Note the pointer types are the concrete `Pointer<W>`/`UniquePointer*<P>`, so `.raw()`/`.into_fixed()` are *inherent* on the handles and don't need allocator methods:

```rust
pub trait Allocator {
    type Pointer: Copy;                     // concrete, e.g. Pointer<NonZeroU32>; the serialized id
    type Address: Word;                     // internal only; never exposed above TransparentAllocator
    type Size: Word + Into<Self::Address>;
    type Meta: Default;                     // per-allocation, kept in the allocator's own table

    // --- lifecycle (deliberately no read/write/splice -- those are on Backends) ---
    fn alloc_resizable(&self, size: Self::Size) -> UniquePointerResizable<Self::Pointer>;
    fn alloc_fixed(&self, size: Self::Size) -> UniquePointerFixedSize<Self::Pointer>;
    fn free_resizable(&self, p: UniquePointerResizable<Self::Pointer>);
    fn free_fixed(&self, p: UniquePointerFixedSize<Self::Pointer>);
    fn resize(&self, p: &UniquePointerResizable<Self::Pointer>, new_size: Self::Size); // FixedSize has no resize (the gate)

    // --- reserve an id now, assign an address later (journaling) ---
    fn reserve_resizable(&self, size: Self::Size) -> UniquePointerResizable<Self::Pointer> { self.alloc_resizable(size) }
    fn reserve_fixed(&self, size: Self::Size) -> UniquePointerFixedSize<Self::Pointer> { self.alloc_fixed(size) }
    fn claim_resizable(&self, _p: &UniquePointerResizable<Self::Pointer>) {}
    fn claim_fixed(&self, _p: &UniquePointerFixedSize<Self::Pointer>) {}

    // --- convert kinds: consume the old handle, may mint a new id, must NOT move memory ---
    fn make_resizable(&self, p: UniquePointerFixedSize<Self::Pointer>) -> UniquePointerResizable<Self::Pointer>;
    fn make_fixed(&self, p: UniquePointerResizable<Self::Pointer>) -> UniquePointerFixedSize<Self::Pointer>;

    // --- reconstruct the owner on load / query the table ---
    fn resolve_owned(&self, p: Self::Pointer) -> Option<Owned<Self>>;     // learn sizedness, mint the owner
    fn lookup(&self, p: Self::Pointer) -> Option<Allocation<Self>>;       // size, sizedness, meta
    fn size(&self, p: Self::Pointer) -> Option<Self::Size> { /* default: self.lookup(p).map(..) */ }
    fn meta(&self, p: Self::Pointer) -> Option<Self::Meta> { /* default: self.lookup(p).map(..) */ }
}

pub enum Owned<A: Allocator + ?Sized> {
    Resizable(UniquePointerResizable<A::Pointer>),
    Fixed(UniquePointerFixedSize<A::Pointer>),
}
pub enum Sizedness { Fixed, Resizable }
pub struct Allocation<A: Allocator + ?Sized> { pub size: A::Size, pub sizedness: Sizedness, pub meta: A::Meta }
```

(`reserve_*` sizes are `Self::Size`, applying the "call it `size`, in `Size`, not `usize`" rule from the bullets above. `Pointer` itself *is* the serialized id, so there's no separate `id()` method — you serialize a `Pointer` directly.)


### `trait TransparentAllocator: Allocator`
- Adds a method to query for actual addresses of `Pointer`s, and default-implemented methods for querying `UniquePointerResizable` and `UniquePointerFixedSize`. These methods *may* return `None` if the provided pointers were only reserved but never claimed.
- Also adds a required `resize_transparently` method that is like `Allocator::resize` but returns a `Some(old_address, new_address)` if resizing requires moving data.

```rust
pub trait TransparentAllocator: Allocator {
    /// `None` if `p` was only reserved, never claimed.
    fn address(&self, p: Self::Pointer) -> Option<Self::Address>;

    /// Like `Allocator::resize`, but reports a relocation as `Some((old, new))`
    /// so the enclosing `Backend` can move the bytes in `Storage`.
    fn resize_transparently(&self, p: &UniquePointerResizable<Self::Pointer>, new_size: Self::Size)
        -> Option<(Self::Address, Self::Address)>;
}
```

### `trait WriteBackend: Allocator`
- Extends `Allocator` with write operations (+ splice, which combines writing with memory management).
- Does not provide `read` operations. This is deliberate: `Guard` methods should never read from the backend because it may have stale data (if we have a `JournaledBackend`). Thus,
	- we'll change the signature of `Persistable::guard` and `Persistable::store` so that they only get a `WriteBackend`. Only `Persistable::load` gets a full `Backend`.
	- The struct `JournaledBackend` below should implement `WriteBackend` but maybe not `Backend`. Instead, it could have a `flush` method that returns a shared reference to a `Backend`, which will allow reading from it but prevent writing to it while it's alive because it's a shared reference (that's what we want: users shouldn't be allowed to write to a journaled backend while they're allowed to read from it because they'd expect reads to reflect their writes, which will not be the case in a `JournaledBackend`).
- Does however provide querying allocator methods (like `size` or the above `downcast`), which may seem like they are like `read` operations and should thus be forbidden, but that's OK: any `WriteBackend` implementation will have to hold an in-memory `Allocator` that is always up to date. Only reads from *allocated memory* are forbidden in `Guard` methods; querying the allocator state is OK and often unavoidable.
- `write` should maybe take a `Pointer` and a `size: Size`, and return an `impl Write` (`UnjournaledBackend` below simply hands out the `&mut Storage` after seeking to the position; for `JournaledBackend`, we should check if we can write the `Op` header and then simply return the `&mut Storage` to let the user fill in the rest of the op. It seems a bit dangerous because it would break not only if the user overwrites but even if they write less than promised). But it would be nice to allow users to write through a `Write` because implementations of `Persistable` types might realistically want to call `write_vectored`

```rust
pub trait WriteBackend: Allocator {
    /// A writer positioned at `anchor`, promising exactly `size` bytes.
    fn write_at(&self, anchor: Self::Pointer, size: Self::Size) -> impl Write + '_;
    /// Atomic resize + tail-shift + content overwrite of one region.
    fn splice(&self, p: &UniquePointerResizable<Self::Pointer>, offset: Self::Size, old_len: Self::Size, new: &[u8]);
}
```

> **Claude:** You already sense the danger, and it's real: **handing out a bare `impl Write` for the *journaled* backend can corrupt the op frame.** The journal's crash-safety rests on each op being a length-prefixed, checksummed frame (see `spec.md`, "On-Disk Layout"). If you write the header promising `size` bytes and then the caller writes fewer (or more), the frame's length/checksum no longer match — recovery will (correctly) discard a torn frame and *silently lose the write*. So I'd keep "return `impl Write`" only for the **unjournaled** backend (no frame to corrupt), and make the **journaled** `write` take the bytes (it already knows `size`) or `&[IoSlice]` for the `write_vectored` case, writing header+payload+checksum itself. Separately, `write_at(&self) -> impl Write` collides with the single storage cursor and `&self` — see the `Backend` note below and problems items 3–4.
- Adds a `read` method. It should take a `Pointer` and an `offset: Size` and return an `impl Read + Seek` (the backends below will simply hand out the actual `&mut Storage`, after seeking to the appropriate position)
- Still implements `write` (because it is also a `WriteBackend`) and mutating allocator methods (because it is also an `Allocator`, and `Allocator` isn't split into 2 traits for querying and modifying). But that's OK: if you want to prevent writing while reading is allowed, never hand out an owned `Backend`, only a shared reference `&impl Backend`.
	- [ ] TODO: is this true? It seems like `write` and `alloc_*` only take `&self`. That's probably to make `Guard` implementations more ergonomic. But it will require some `Cell` or `RwLock` gymnastics -- can we elevate that to the `WriteBackend` level so that `Allocator` has a more normal API that takes `&mut self` for mutating methods?

```rust
pub trait Backend: WriteBackend {
    /// A reader positioned at `anchor + offset`.
    fn read_at(&self, anchor: Self::Pointer, offset: Self::Size) -> impl Read + Seek + '_;
}
```

> **Claude (answering the `&self`/`&mut self` TODO):** Keep the mutating methods `&self`. It is *not* gratuitous: the guard model relies on it. A parent guard holds `&B` and hands each nested field guard the *same* `&B` by reborrow; if mutating methods were `&mut self`, two sibling field guards would each need `&mut B` and conflict. Elevating to `&mut self` would break exactly the nested-guard ergonomics the whole design exists for, and the "`Cell`/`RwLock` gymnastics" is the contained price of that.
> But `&self` collides with returning `impl Write`/`impl Read + Seek` over a *single* storage cursor: **`Seek::seek` takes `&mut self`, so a reader that seeks into the one backing cursor cannot be handed out from a `&self` method** (my prototype hit this and had to stub `read_at`). You can have any *two* of {`&self` mutating methods, returning `impl Write`/`impl Read`, a single storage cursor} but not all three. Cleanest reconciliation: `&self` + `RefCell<Storage>`, and have `read`/`write` *take* the bytes (or a `&mut dyn Write`/`&mut dyn Read` callback) instead of *returning* a long-lived `impl Write`/`impl Read`. That gives up the `write_vectored`-through-a-returned-`Write` convenience but keeps `&self` and (for the journaled backend) frame control. See problems items 3–4.

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

> **Claude (re your Question "can the two backends share code?", and the compaction Question):** yes — journal **replay** is essentially "run the `UnjournaledBackend` immediate path over the buffered ops." Factor a single `fn apply(alloc: &mut A, storage: &mut S, op: Op)` that both call: `UnjournaledBackend` calls it inline per mutation, `JournaledBackend` calls it per op during replay. The prototype's `UnjournaledBackend` write path *is* that body. And compaction (Question 3) becomes markedly simpler under Id-serialization: the allocator computes a move plan `[(id, old_addr, new_addr)]`, the backend performs the `storage` moves and updates the table — and because serialized pointers are stable ids, **no on-disk pointer needs rewriting** (contrast the old address-serialization design, which had to rewrite every serialized target). That is the upside of the ID-serialization pivot, now recorded in `spec.md`.

### `trait AllocatorExt: Allocator`

Extension trait with a blanket implementation for every `Allocator`, but defined in the crate where `Persistable` is defined. See Section 4.3 in `allocator-spec.md` except for the following nitpicks:
- Don't call methods `*_boxed`. Call them `*_typed` instead.
- `promote`/`demote` between `UniquePointer<T, P>` and `UniquePointerFixedSize<P>` are the *inherent* `UniquePointer::from_fixed` / `UniquePointer::into_fixed` now that pointers are concrete — no extension-trait method needed (`UniquePointer<T, P>` is just `UniquePointerFixedSize<P>` + `PhantomData<T>`).
- Split `alloc_array` into two methods:
	- `alloc_fixed_size_array<T: Persistable<Self::Pointer>>`, which returns a `UniquePointerFixedSize<Self::Pointer>`; and
	- `alloc_resizable_array<T: Persistable<Self::Pointer>>`, which returns a `UniquePointerResizable<Self::Pointer>`

```rust
// Defined in the `Persistable` crate; blanket-impl'd for every `Allocator`.
pub trait AllocatorExt: Allocator {
    fn alloc_typed<T: Persistable<Self::Pointer>>(&self) -> UniquePointer<T, Self::Pointer> {
        UniquePointer::from_fixed(self.alloc_fixed(/* T::INLINE_SIZE as Self::Size */))
    }
    fn free_typed<T>(&self, p: UniquePointer<T, Self::Pointer>) { self.free_fixed(p.into_fixed()) }
    fn alloc_fixed_size_array<T: Persistable<Self::Pointer>>(&self, len: Self::Size) -> UniquePointerFixedSize<Self::Pointer>;
    fn alloc_resizable_array<T: Persistable<Self::Pointer>>(&self, len: Self::Size) -> UniquePointerResizable<Self::Pointer>;
}
impl<A: Allocator + ?Sized> AllocatorExt for A {}
```

(With concrete pointers, `T` is the boxed value and `Self::Pointer` is the concrete id type — no allocator-carrying pointer type parameter to thread.)

### `Persistable<P>` and `kladde-types`

`Persistable` and the `kladde-types` containers are generic over the pointer type `P`, defaulted to `Pointer`, so the common case stays parameter-free while a file may opt into a wider `P`. A container that stores pointers sets the same default and implements `Persistable<P>` generically. Verified in `crates/generic-alloc`:

```rust
pub trait Persistable<P = Pointer>: Sized {
    const INLINE_SIZE: usize;   // just the inline pointer id; may depend on P's width
    fn store<B: Backend<Pointer = P>>(&mut self, backend: &B, location: Location<P>);
    fn load<B: Backend<Pointer = P>>(backend: &B, location: Location<P>) -> Self;
}

// kladde-types: default P = Pointer here too, and implement generically over P.
pub struct PersistableVec<T, P = Pointer> { data: Vec<T>, pointer: Option<UniquePointerResizable<P>> }
impl<T: Persistable<P>, P: Copy> Persistable<P> for PersistableVec<T, P> {
    const INLINE_SIZE: usize = std::mem::size_of::<P>();   // just the pointer id, no inline len
    // store/load pin the backend to `P` via `B: Backend<Pointer = P>`.
}
```

The `store`/`load` methods pin the backend's pointer type to `P` (`Backend<Pointer = P>`), so a `PersistableVec<T, P>` only works with a backend whose pointer type is `P`. `INLINE_SIZE` is a per-`P` const, and for a pointer-holding type it is **just the pointer id** — `size_of::<P>()` (4 for the default `NonZeroU32` id, 8 for a `NonZeroU64` one). There is *no inline `len`*: the length/size is owned by the allocator (query `size`/`lookup`; for a resize-to-exact vec, `len = size / elem_size`) or stored one indirection away for a chunked layout — matching the "the size only stored by the allocator, not by the `PersistableVec` itself" goal in the chunked-vec test case below. Empty is the null pointer, free via `Option<Pointer>`'s niche. This compiles (verified in `crates/generic-alloc`).

**TODO: write real implementor documentation for `Persistable`.** It should tell implementors there are three choices:

1. If `MyType` stores no pointers, `impl<P> Persistable<P> for MyType { ... }` — works at *every* pointer width; just don't assume any particular width.
2. If `MyType` must store pointers, make it generic over the pointer type with a default and implement generically: `struct MyType<P = kladde::Pointer> { ... }`, then `impl<P> Persistable<P> for MyType<P> { ... }`.
3. If you don't care about non-default widths, implement only for the default: `impl Persistable for MyType { ... }` (no `P` generic on `MyType`). This pins `MyType` to `P = Pointer`; using it in a non-default-width file won't compile.

## Problems and regressions

Written by Claude. This section collects the problems this proposal may run into and the concrete regressions relative to what is implemented today (the `allocator-spec.md` overhaul, now landed) or planned there. Several items are confirmed against the throwaway prototype in `crates/generic-alloc`; the rest are design-level. (Two earlier items — "associated pointer types infect the `Persistable` stack" and "serializing `Id` reinstates the on-disk handle table" — are resolved by the concrete-pointer and ID-serialization decisions and have been removed; the ID-serialization trade-off is now recorded in `spec.md`.)

1. **The pointer-width parameter `P` leaks into generic code and into pointer-holding structs** *(new, from the `Persistable<P = Pointer>` decision; confirmed in the prototype).* The default keeps the common case parameter-free, but the width still surfaces in a few places:
   - **A *bound* `where T: Persistable` silently means `Persistable<Pointer>`** (omitted defaults apply in bounds, like `T: Add` ≡ `T: Add<T>`). So Id-polymorphic *library* code must write `T: Persistable<P>` and thread `P` — never bare `T: Persistable`, which would reject a `Persistable<Pointer<u64>>`-only type with a confusing error. This bites `kladde-types`/the derive macro, not application code at a fixed width.
   - **A hand-written `impl Persistable for MyType` covers only the default width;** universal support needs `impl<P> Persistable<P> for MyType` (trivial for pointer-free types — the derive emits it). This is implementor choice (3) below.
   - **Any struct that stores a pointer-holding field inherits `P`** to support non-default widths (`Foo { v: PersistableVec<T, P> }` ⇒ `Foo<P>`), because `INLINE_SIZE` and the stored owned handle both depend on `P`. The default keeps the *common* case `Foo` param-free; only files opting into a wider `P` pay.
   - **Fully-unconstrained inference needs an annotation** (`PersistableVec::new()` with nothing pinning `P` → "type annotations needed"), where a non-generic type never asks.

   All of this is confined to library/generic code and to non-default-width files; application code at one fixed width sees essentially the non-generic experience.

2. **Generic integer associated types need an "unsigned word" bound that std doesn't provide.** Making `Address`/`Size` associated types means every offset/size computation needs `+`, `<`, `Into<Address>`, and `usize` conversions over a generic type. Rust has no single "unsigned integer" trait, so you need a helper trait (the prototype's `Word`) or a dependency like `num-traits`, and its bounds ride along on every generic function that does address arithmetic. Manageable, and `Size: Into<Address>` is the right core relation, but it is real bound-noise that the current concrete-`u32` code doesn't have. (You'll also want `TryFrom<usize>`/`to_usize`, because offsets from Rust collections arrive as `usize`.)

3. **`read` returning `impl Read + Seek` cannot be a `&self` method over a single storage cursor.** `Seek::seek` takes `&mut self`; a reader that seeks into the one backing `Storage` cursor therefore needs `&mut` access to it, so `Backend::read(&self) -> impl Read + Seek` can't hand out the real cursor from `&self` — it needs `&mut self` or `RefCell<Storage>` (which then serializes readers, defeating handing out several). The prototype hit this and had to stub `read_at`. Combined with item 4, you can pick any **two** of {`&self` mutating methods, returning `impl Read`/`impl Write`, a single storage cursor}. Practical resolutions: `read(&mut self)` (giving up concurrent readers — fine for single-writer), or return an owned buffer (today's `Vec<u8>`, losing zero-copy), or a positioned-read API that borrows `&Storage` immutably and carries its own offset (`Read` but not `Seek`). *(The "New idea after the feedback" section proposes `read = &mut self`, which resolves this.)*

4. **`&self` interior mutability vs. `&mut self` (your TODO).** Keep mutating methods `&self`. The guard model depends on it (nested field guards reborrow the same `&B`); `&mut self` would make sibling field guards conflict. So the "`Cell`/`RwLock` gymnastics" isn't gratuitous — it's the price of the guard ergonomics, and it's contained in the backend. But `&self` collides with returning `impl Write`/`impl Read` (item 3): the clean reconciliation is `&self` + `RefCell<Storage>` with `read`/`write` **taking** bytes / a `&mut dyn Write` callback rather than **returning** a long-lived writer. That costs the `write_vectored`-through-a-returned-`Write` convenience. *(The "New idea" section's Allocator-is-`&mut self` / Backend-is-`&self`-facade split also resolves this.)*

5. **A journaled `write` that returns a bare `impl Write` can corrupt the op frame.** The journal's crash-safety rests on each op being a length-prefixed, checksummed frame (`spec.md`, On-Disk Layout). Hand out `&mut Storage` after a header promising `size` bytes and let a `Persistable` author write fewer/more, and the frame's length/checksum no longer agree — recovery discards the torn frame and *silently drops the write*. Keep "return `impl Write`" for the unjournaled backend only; make the journaled `write` take the bytes (or `&[IoSlice]`) and write header+payload+checksum itself.

6. **`reserve`/`claim` adds a "valid but unclaimed" state whose full semantics need pinning down.** Allowing *every* `Allocator` method on reserved-but-unclaimed pointers means `free`, `resize`, `make_fixed`, … must each define their effect on a reservation. It's expressible (the prototype models a reservation as a table row with `address: None`), but write down the state machine (reserved → claimed → freed, and reserved → freed = cancel), and note that `WriteBackend`/`Backend` returning an error for I/O on unclaimed pointers turns every write path fallible where it wasn't before.

7. **Self-hosting bootstrap is a genuine open problem, not a detail.** Storing the allocator's own state in allocations it manages, and its journal in memory it allocates, is circular: you can't journal the allocation of journal space, and you can't read the allocator table without first knowing where it lives. This needs a bootstrap anchor *outside* the general mechanism — the fixed `Storage` header holding the root address of the persisted allocator state, plus journal space managed specially. The current implementation avoids this entirely because the mock allocator is in-memory and never persists its state; making the allocator self-hosting is new, load-bearing work.

8. **General over-generalization risk.** The document worries (twice) about generalizations hurting the Kladde common case. With pointers now concrete and the widths settled as defaulted type parameters, the sharpest instance is gone; the residual caution stands: gate each remaining generalization (the full `Address`/`Size`/`Meta` genericity, `reserve`/`claim`, `impl Write`/`impl Read` returns) on a concrete need — the chunked-vec test case is the forcing function — rather than adopting all of it up front. Keep without hesitation: separating `Storage` from `Allocator`; concrete pointers with defaulted widths; `size` (not `capacity`); `Meta`/`lookup`. Defer or drop: returning `impl Write`/`impl Read` from `&self` (items 3–4) and segments (deferred already — see `later.md`).

## Test case: chunked `PersistedVec` with small-vec optimization

TODO: build a chunked `PersistableVec` implementation for kladde onto the redesigned `Allocator`. This implementation should deliberately be "prematurely optimized" to verify that it supports anything we might need one day.
- For content smaller than the chunk size, its should not require any more on-disk space than a pointer (inline in the parent struct), the size (*only* stored by the allocator, not by the `PersistableVec` itself), and the data (behind the pointer). There is no separate capacity for small vecs, the allocation size matches the vector size and gets resized by the allocator when the `PersistableVec` grows or shrinks.
- For content larger than the chunk size, it should have a fast mode where only fixed-sized chunks are allocated, they're stored on disk in a linked list (but held in memory by a `std::vec::Vec` of pointers for fast random access). The on-disk representation then holds the length and a pointer to the first chunk. Since the inline size is only one pointer, some of this information has to be stored one indirection away.
- There should also be a "compact" mode for content larger than the chunk size where the last chunk is variably sized and fits the content length. This could be generated, e.g., by an explicit "extreme" compaction before file closing (which is also a privacy measure as it removes stale data from unused memory regions in the file). This representation may turn out to be slightly suboptimal in disk size as it might store the overall vec size even though it could be determined from walking the linked list and querying the allocator for the size of the last chunk, but since this is only for large files the relative impact is small.
- To distinguish between the above three cases, ~~we must be able to sneak at least one extra bit into pointers and/or sizes.~~ query the allocator: if the inline pointer is variable-size, then it's small-vec optimized. If the inline pointer is fixed size, then it's the first part of a linked list.
- Switching between these representations (e.g., when the vector's size grows or shrinks across the one-chunk threshold) should not require a data move. Thus, linked-list pointers must probably be stored at the end of the chunk. Also, we need to be able to promote/demote allocations from/to resizable in place (index may change but memory location mustn't. Maybe use high bit of indices to distinguish resizable from fixed size). 

## Questions

(Segments are deferred — that discussion has moved to `later.md`'s "Allocator" section. The "should `Persistable` be parameterized by the backend?" question is resolved: concrete pointers, `Persistable<P = Pointer>` — see the decision note under "Detailed consequences" and the `Persistable<P>` section above.)

- [ ] Can `UnjournaledBackend` and `JournaledBackend` reuse some code from each other? Journal replay should do similar operations to what `UnjournaledBackend` does immediately. 
- [ ] How would compaction work in this setup? It's the allocator's job to figure out where everything should move, but the allocator needs access to the `Storage` to actually perform it. In addition, any changes to the allocator state would also have to be persisted and operations should probably be journaled, and this is `Backend` logic.

## Miscellaneous things to keep in mind

These items are not so urgent, maybe defer until we've ironed out the main points.

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
> **This is what fixes the `&self`/`&mut self` knot (my problems item 4), cleanly.** Decoupled, the layers can choose independently: give `Allocator` a *normal `&mut self`* mutating API (nice for standalone reuse, tooling, compaction), and let `Backend` be a `&self` facade that borrows the inner allocator through a `RefCell` per call. The "`Cell`/`RwLock` gymnastics" then live *only in the backend adapter* and never touch the reusable `Allocator` — which is exactly the separation you want. I had said "keep everything `&self`"; the better answer, which your restructure enables, is "`Allocator` is `&mut self`, `Backend` is the `&self` facade."
>
> **The `ReadBackend`/`WriteBackend` split (1d) genuinely fixes what the shared-reference trick couldn't.** `&Backend` still let you write because `Backend` *had* `&self` write methods; a `ReadBackend` with *no write methods at all* cannot. The `io::Read`/`io::Write` analogy is apt. And it composes with the phase model: `JournaledWriteBackend: WriteBackend` during the write phase, `flush(&mut self) -> impl ReadBackend` afterward — the `&mut self` on `flush` guarantees no live guard (guards hold `&self`), and the returned type simply has no write surface.
>
> **A bonus the split unlocks — read can be `&mut self`, which dissolves the `Seek` problem (my item 3) for reads.** `load` is *sequential* (load field A fully, then B, then …; a container loops one element at a time), so a `&mut ReadBackend` reborrowed down the recursion never needs two simultaneous borrows. That means `ReadBackend::read(&mut self) -> impl Read + Seek` is fine — no `RefCell<Storage>` needed on the read path, and you get the real seekable cursor. Meanwhile writes/allocs stay `&self` for the guard reborrow model. So the natural asymmetry is **read = `&mut self`, write = `&self`**, and it falls out of exactly *why* each exists (load is sequential; guards share a `&B`). As a free side effect, the borrow checker then forbids `load` (needs `&mut B`) while any guard (holds `&B`) is alive — a correct enforcement of your "don't read stale data mid-write" rule.
>
> **Trade-offs / things to pin down:**
> - **Duplicated surface.** `alloc_*`/`free_*`/`resize_*`/`lookup`/`size` now appear on both `Allocator` (address-aware) and `WriteBackend`/`ReadBackend` (address-hidden). That's real boilerplate — each backend forwards-and-translates every op. Mostly mechanical; a default-method layer or a small macro can absorb it. It's the price of the (worthwhile) separation.
> - **`resize` returning `Option<(old, new)>` conflates two "no move" cases.** `None` means both *unclaimed* (no address yet) and *in-place claimed* (address unchanged). The backend still has work in the in-place-*grow* case (ensure `Storage` covers `[addr, addr+new_size)`), which the `Option` doesn't signal. Either make it a 3-way result (`Relocated{old,new}` | `InPlace{addr}` | `Unclaimed`), or keep `Option` and have the backend size `Storage` by *querying* the pointer's address+size after every resize regardless. Fine either way, just decide.
> - **Your `resize`-reserve TODO is real and it's the right instinct.** A journaled backend must not let a live resize assign a new address (it could land past the journal). The fix is the same deferral you already use for `alloc`: record a *reserve-resize* op now, assign the address on replay via `claim`. So this **generalizes reserve/claim from `alloc` to `resize`** (and arguably to any address-assigning op) — which is a good sign the reserve/claim mechanism is the right primitive, but it does add surface (a reserved-resize state on top of the reserved-alloc state in problems item 6).
> - **Scope check — what this restructure does *not* touch.** It's orthogonal to, and does not resolve, problems items **2** (the `Word` bound), **5** (journaled `write` framing — still take bytes, don't return a raw `impl Write`), and **7** (self-hosting bootstrap). Worth keeping those separate in your head so this clean win doesn't get over-credited. (The pointer-representation and ID-serialization questions that used to head this section are now decided — concrete pointers and stable ids.)
> - **Naming.** With the split there may be no single "`Backend`" anymore. If a lot of code wants both, keep a convenience `trait Backend: ReadBackend + WriteBackend {}` (blanket-impl'd) so callers can still spell one bound; but define the real capabilities on the two halves.
>
> Net: adopt 1a (drop `TransparentAllocator`; addresses are core to `Allocator`, hidden by the `Backend`), 1c (composition, not extension), and 1d (`ReadBackend`/`WriteBackend`, `read = &mut self` / `write = &self`). Firm up the `resize` result shape and add the reserve-variant of `resize`. This removes `TransparentAllocator`, the `resize`/`resize_transparently` split, and the `&self`-vs-`&mut self` and read-isolation problems in one coherent move.
