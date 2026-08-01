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

- The trait `Allocator` is the foundation for memory management. It manages a dynamic collection of address regions but doesn't connect them to memory regions (i.e., it doesn't allow reading from or writing to any memory). Addresses **are** part of the `Allocator` contract (`address`, and `resize` reporting relocations) — they are *core* to management — but they are simply never surfaced *above* the backend layer, which hides them from `Persistable` types. Its mutating methods take a normal `&mut self` (it's a plain, reusable data structure; the interior-mutability gymnastics live in the backend adapter, not here).
- The traits `ReadBackend` and `WriteBackend` are **composed of** (they hold) an `Allocator` — they do *not* extend it. `WriteBackend` equips it with methods to write to some associated memory at the allocated address ranges and to allocate/resize/free (address-hidden); `ReadBackend` provides read access. They are split like `std::io::Read`/`std::io::Write`: `ReadBackend` has *no* write or allocation surface at all, so `&mut impl ReadBackend` genuinely cannot mutate — which is how the write/read phases of a `JournaledBackend` are enforced (a shared `&Backend` couldn't, because it still had `&self` write methods). A blanket convenience `trait Backend<P, S>: ReadBackend<Pointer = P, Size = S> + WriteBackend<Pointer = P, Size = S>` names both halves at once (pinning both the pointer type `P` and the size type `S`). These are the traits against which manual implementations of `Persistable` data types are written: `store` gets `&impl WriteBackend`, `load` gets `&mut impl ReadBackend`.
	- The two halves have a deliberate `&self`/`&mut self` **asymmetry**: **write = `&self`** (the guard model hands each nested field guard the *same* `&B` by reborrow, so sibling guards can't each hold `&mut B`), **read = `&mut self`** (`load` is *sequential* — one field/element after another — so a single `&mut` reborrowed down the recursion suffices, and this dissolves the `Seek`-takes-`&mut self` problem for reads and lets the read path hand out the real seekable cursor with no `RefCell`). As a free side effect the borrow checker forbids `load` (needs `&mut B`) while any guard (holds `&B`) is alive — exactly the "don't read stale data mid-write" rule.
- The trait `Storage` is orthogonal to `Allocator` and the backends. While `Allocator` models memory *management*, `Storage` models memory *access*. It provides unstructured random and sequential memory access to a large block of stored data (e.g., a file).
- Concrete backends (`UnjournaledBackend` and `Journaled[Write]Backend`) are **composed of** an `Allocator` and a `Storage`, generic over both. They perform two tasks:
	- they translate the high-level read/write operations ("read/write bytes `x..y` of memory allocation `p`") to low-level read/write operations in an encapsulated `Storage`; and
	- they delegate management calls to the encapsulated `Allocator`, translating as needed (e.g. `Allocator::resize` reports a relocation as `Some((old, new))` addresses, which the backend *consumes* to move the bytes in `Storage` and then returns nothing to its caller — users never see addresses). Mutating `Allocator` calls are also replicated to an in-`Storage` representation of the allocator state, from which the encapsulated `Allocator` can be recreated on open. The backend uses the encapsulated `Allocator` itself to manage the memory that holds this in-`Storage` allocator state, so allocator state and user data separate automatically without reserving fixed file regions for either.
	- Because the backend is a *composition*, it is exactly where the `&self`-write facade lives: it wraps the `&mut self` `Allocator` (and `Storage`) in a `RefCell` and borrows through it per call. The reusable `Allocator` stays a clean `&mut self` API; the interior mutability is contained in this one adapter.
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

pub struct Location<P = Pointer, S = u32> { pub anchor: P, pub offset: S }
```

`Location` is parametric over **both** the pointer type `P` and the size type `S` (default `u32`): `anchor` is a `P`, and `offset` is a displacement *within* an allocation, so it is `Size`-typed, not `usize`. Crucially, `S` does **not** become a second `Persistable` type parameter — `Persistable<P>` stays single-parameter and names the size as `Location<P, B::Size>` in its method signatures, so `S` *flows from the backend* (verified in `crates/generic-alloc`). See the `Persistable<P>` section.

Inherent methods on the owned handles: `.raw() -> P`, and `UniquePointer::{from_fixed, into_fixed}`. The fixed/resizable **gate** (`resize`/`splice` accept only `UniquePointerResizable`) is unchanged from `allocator-spec.md`.

### `trait Allocator`
- Has associated types `Pointer`, `Address`, and `Size` (most implementors are parameterized by the latter two, e.g. `MyAllocator<Address, Size> { ... }`).
	- `Pointer` is the concrete `Copy` identity `Pointer<W>` (§`Pointer`) the allocator hands out — the stable id, serialized into other allocations, previously called `Index` in `allocator-spec.md`. It stays a *concrete* type (not opaque/associated), so the owned handles keep inherent `.raw()`/`.into_fixed()` methods and `Persistable` needs no backend type parameter, only the pointer type `P` — which a `Persistable<P>` pins to the backend via `Backend<Pointer = P>`.
- Manages a set of non-overlapping `Address` ranges, where `Address` is an unsigned integer type that should probably somehow generically support `NonZero`.
- The trait definition does not assume any connection to persistency. It doesn't interpret the integer ranges it manages as memory and doesn't provide read or write access to them (that's what the backends are for), and you can implement an `Allocator` that holds its entire state in memory and doesn't deal with persistence at all. A persisted allocator then typically builds on top of an in-memory allocator, somewhat analogous to how, at a higher level, types that implement `Persistable` (like `PersistableHashMap<K,V>`) often use standard library types (like `std::collections::HashMap<K,(V,Index)>`) for their in-memory representation.
	- Concretely, this means that, different to the current `allocator-spec.md`, this new `Allocator` does not have the methods `read`, `write`, and `splice` (those go in the backends below)
- **Addresses are core to `Allocator`, and there is no `TransparentAllocator`.** Every `Allocator` can resolve a `Pointer` to its `Address` (`fn address(p) -> Option<Address>`; `None` for a reserved-but-unclaimed pointer, whose address doesn't exist yet), and `resize` reports a relocation directly by returning `Option<(old_address, new_address)>` (`Some` iff the bytes must move). Addresses are simply never surfaced *above* the backend layer — the backend consumes the relocation report to move bytes and hands its own callers nothing. This removes the old `TransparentAllocator`/`resize_transparently` split (which existed only to route addresses around an `Allocator` that pretended not to have them).
- **Mutating methods take `&mut self`.** `Allocator` is a plain, reusable data structure with a normal ownership-checked API — nice for standalone reuse, tooling, and compaction. The `&self` interior mutability that the guard model needs lives *only in the backend adapter* (which wraps the allocator in a `RefCell`), never in the reusable `Allocator` itself.
- Maybe exposes some `alloc_scratch` (allocate largest contiguous memory within the file) and/or `alloc_at_end` method if that is needed for journaling. Maybe not necessary if we can simply use the standard allocation methods and then query whether we happen to be at the end of the file, exploiting that a persisted allocator that builds on top of a *concrete* in-memory allocator may make assumption about the in-memory allocator's specifics beyond the `Allocator` trait.
- Should probably be self-hosting. I.e., the `UnjournaledBackend` below  uses the Allocator itself to manage the memory regions where in-file representation of the Allocator is stored. It will probably need special logic to manage it in-file, but that's separate.
- The pointer types (`UniquePointerResizable<P>`, `UniquePointerFixedSize<P>`, and the `Copy` `Pointer<W>`) are **concrete**, parameterized over `P`/`W` — *not* per-allocator associated types. (An earlier draft made them associated so an allocator could cache metadata like sizedness *in* the pointer; the `MetaData`/`lookup` table below already provides that, so the pointers stay concrete — which keeps inherent pointer methods and keeps `Persistable` free of a backend type parameter.)
- Provides default-implemented methods that allow generating unique IDs and reserving them for later allocation of a given size (this will be used by `JournaledBackend` below, and it's also nice that this gets exposed to implementors of `Persistable` types because it may be useful for some of them, and it's easily implemented for the `JournaledBackend` itself):
	- `fn reserve_resizable(&mut self, size: Self::Size) -> UniquePointerResizable;`
	- `fn reserve_fixed(&mut self, size: Self::Size) -> UniquePointerFixedSize;`
	- And methods that `claim` reservations (i.e., assign actual addresses)
- Default implementations of `reserve_{resizable, fixed}` simply forward to `alloc_{resizable, fixed}`. Default implementations of `claim_{resizable, fixed}` are no-ops.
- **The same reserve/claim deferral is needed for `resize`, not just `alloc`.** In a journaled backend, a live resize of an already-claimed region must not assign a new address on the spot (it could land past the current end of the journal and block the journal from growing). So there is a `reserve_resize` that records the intent now and defers the address assignment to journal replay's `claim`, exactly like `alloc`. This is a good sign that reserve/claim is the right primitive (it generalizes to any address-assigning op), but it does add a "reserved-resize" state on top of the "reserved-alloc" state (see [Problems and regressions](#problems-and-regressions)).
- It must be allowed to call all `Allocator` methods (like freeing, resizing, ...) on valid `UniquePointerResizable` or `UniquePointerFixedSize` regardless of whether they're reserved or actually allocated. But `WriteBackend` and `Backend` may return an error when provided pointers that are only reserved and where never claimed (they don't *have to* return an error, but if they don't then they must operate as if the allocations were normally created rather than just reserved, as in the default implementations)
- Has an additional associated type `MetaData: Default` for storing additional data for each allocation that can be queried either by a dedicated `meta_data` method or by a `lookup` method that returns everything about a given pointer (its address, sizedness, size, and meta data). This is used by `Backend` implementations to keep track of where allocator state is stored in `Storage`.
	- `Allocator`s that don't provide any `MetaData` can simply set `type MetaData = ()` and leave the query method default implemented (it returns `MetaData::default()`).
	- The reason why we push `MetaData` into `Allocator` instead of requiring the containing types to simply store the meta data in a hash map themselves is that most `Allocator` implementations will probably have some sort of table `Id --> (Address, Size, ...)` anyway, and many use cases where one would query for `MetaData` would also involve a query for the addresses or size, which would hit that table anyway. So it's probably more efficient to have it all in a single table.
- Apart from the above, model `Allocator` after the description in `allocator-spec.md`, with a few additional minor tweaks:
	- `Allocator` should support recovering an *owned* handle of the correct kind from a `Pointer` (this is what `load` needs — deserialize a `Pointer`, reconstruct the single owner, learning sizedness on the way): `fn resolve_owned(Pointer) -> Option<Owned>` with `enum Owned { Resizable(UniquePointerResizable), Fixed(UniquePointerFixedSize) }`; and a `lookup` returning size + sizedness + `MetaData`. (Caveat: like `from_index` today, `resolve_owned` can mint a *second* owner for an already-owned region, so it stays a load/allocator-internal method by convention.)
	- We should also require methods to convert between fixed sized and resizable allocations. These consume the old pointer and return a new pointer (possibly with a new `Id` because some `Allocator`s might use a bit in the `Id` as a flag for fixed size vs resizable).
	- Don't use the term `capacity` here when referring to allocation sizes. Always call it `size` (also not `byte_size`) because that's what it is *from the perspective of the allocator*. The implementation of a `Persistable` type might interpret the *size of the allocation* as a *capacity of a container* but that's at a higher level of abstraction.

A sketch (the concrete pointer shapes and the `&mut self` / `resize -> Option<(old, new)>` shapes are verified in `crates/generic-alloc`; `Word` is a helper trait bounding the generic unsigned integers — see the Word-bound item in [Problems and regressions](#problems-and-regressions)). Note the pointer types are the concrete `Pointer<W>`/`UniquePointer*<P>`, so `.raw()`/`.into_fixed()` are *inherent* on the handles and don't need allocator methods. Mutating methods take `&mut self` (the `&self` facade lives in the backend):

```rust
pub trait Allocator {
    type Pointer: Copy;                     // concrete, e.g. Pointer<NonZeroU32>; the serialized id
    type Address: Word;                     // core to the allocator, hidden above the backend
    type Size: Word + Into<Self::Address>;
    type Meta: Default;                     // per-allocation, kept in the allocator's own table

    // --- lifecycle (deliberately no read/write/splice -- those are on the backends) ---
    fn alloc_resizable(&mut self, size: Self::Size) -> UniquePointerResizable<Self::Pointer>;
    fn alloc_fixed(&mut self, size: Self::Size) -> UniquePointerFixedSize<Self::Pointer>;
    fn free_resizable(&mut self, p: UniquePointerResizable<Self::Pointer>);
    fn free_fixed(&mut self, p: UniquePointerFixedSize<Self::Pointer>);
    /// `Some((old, new))` iff the bytes must move; `None` = in-place or unclaimed.
    fn resize(&mut self, p: &UniquePointerResizable<Self::Pointer>, new_size: Self::Size)
        -> Option<(Self::Address, Self::Address)>;                        // FixedSize has no resize (the gate)

    // --- addresses are core (no TransparentAllocator) ---
    fn address(&self, p: Self::Pointer) -> Option<Self::Address>;         // None if reserved, never claimed

    // --- reserve an id now, assign an address later (journaling) ---
    fn reserve_resizable(&mut self, size: Self::Size) -> UniquePointerResizable<Self::Pointer> { self.alloc_resizable(size) }
    fn reserve_fixed(&mut self, size: Self::Size) -> UniquePointerFixedSize<Self::Pointer> { self.alloc_fixed(size) }
    fn reserve_resize(&mut self, p: &UniquePointerResizable<Self::Pointer>, new_size: Self::Size) { self.resize(p, new_size); }
    fn claim_resizable(&mut self, _p: &UniquePointerResizable<Self::Pointer>) {}
    fn claim_fixed(&mut self, _p: &UniquePointerFixedSize<Self::Pointer>) {}

    // --- convert kinds: consume the old handle, may mint a new id, must NOT move memory ---
    fn make_resizable(&mut self, p: UniquePointerFixedSize<Self::Pointer>) -> UniquePointerResizable<Self::Pointer>;
    fn make_fixed(&mut self, p: UniquePointerResizable<Self::Pointer>) -> UniquePointerFixedSize<Self::Pointer>;

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

(`reserve_*` sizes are `Self::Size`, applying the "call it `size`, in `Size`, not `usize`" rule from the bullets above. `Pointer` itself *is* the serialized id, so there's no separate `id()` method — you serialize a `Pointer` directly. `resize` returning `Some((old, new))` addresses is fine on the `Allocator` because addresses are core here; the backend consumes it and returns nothing to its own callers.)


### `trait WriteBackend` (composes an `Allocator`; write = `&self`)
- **Composed of** an `Allocator` (it holds one), not an extension of it. It equips that allocator with write access to the associated memory *and* re-exposes allocation/resize/free — but *address-hidden*: `WriteBackend::resize` never returns addresses (contrast `Allocator::resize`), because users of a backend must never see them.
- All its methods take **`&self`**. This is what the guard model needs: a parent guard holds `&B` and hands each nested field guard the *same* `&B` by reborrow, so two sibling field guards can both borrow it. The interior mutability this requires lives inside the concrete backend (a `RefCell` around the composed `Allocator`+`Storage`), *not* in the reusable `Allocator`.
- Does not provide `read` operations — that is `ReadBackend`. This is deliberate: `Guard`/`store` methods must never read from the backend, because a `JournaledBackend` may hold stale bytes for a not-yet-replayed write. So `Persistable::store` gets a `&WriteBackend`, and only `Persistable::load` gets a `&mut ReadBackend`.
- Does however provide *querying* allocator methods (`size`, `lookup`, …). These look read-like but are fine: any `WriteBackend` holds an always-up-to-date in-memory `Allocator`; only reads of *stored bytes* are forbidden mid-write, not queries of allocator state (which are often unavoidable).
- `write` **takes the bytes** (`&[u8]`, or `&[IoSlice]` for the `write_vectored` case) rather than returning a bare `impl Write`. Returning a writer was tempting for `write_vectored`, but for the *journaled* backend it can corrupt the op frame — see the note below — and it also fights the `&self`/single-cursor constraint. Taking the bytes lets the journaled backend write header+payload+checksum itself.

```rust
pub trait WriteBackend {
    type Pointer: Copy;
    type Size: Word;                                                                // re-exposed from the inner Allocator
    fn alloc_resizable(&self, size: Self::Size) -> UniquePointerResizable<Self::Pointer>;
    fn alloc_fixed(&self, size: Self::Size) -> UniquePointerFixedSize<Self::Pointer>;
    fn free_resizable(&self, p: UniquePointerResizable<Self::Pointer>);
    fn free_fixed(&self, p: UniquePointerFixedSize<Self::Pointer>);
    fn resize(&self, p: &UniquePointerResizable<Self::Pointer>, new_size: Self::Size); // no addresses out
    fn write(&self, anchor: Self::Pointer, offset: Self::Size, bytes: &[u8]);          // takes bytes, not `impl Write`
    /// Atomic resize + tail-shift + content overwrite of one region.
    fn splice(&self, p: &UniquePointerResizable<Self::Pointer>, offset: Self::Size, old_len: Self::Size, new: &[u8]);
    fn size(&self, p: Self::Pointer) -> Option<Self::Size>;                            // querying is fine mid-write
}
```

> **Claude (why `write` takes bytes):** handing out a bare `impl Write` for the *journaled* backend can corrupt the op frame. The journal's crash-safety rests on each op being a length-prefixed, checksummed frame (`spec.md`, "On-Disk Layout"). Write a header promising `size` bytes and then let a `Persistable` author write fewer/more, and the frame's length/checksum no longer match — recovery discards the torn frame and *silently loses the write*. Taking the bytes (or `&[IoSlice]`) keeps the backend in control of the frame.

### `trait ReadBackend` (read = `&mut self`)
- Provides read access to stored bytes, plus allocator *queries* (`size`). It has **no** write, alloc, or resize surface at all — split off from `WriteBackend` like `std::io::Read` from `std::io::Write`. This is what actually enforces the journaled read/write phase separation: a `&mut impl ReadBackend` genuinely cannot mutate, where the old "hand out `&Backend`" trick failed because `Backend` still carried `&self` write methods.
- `read` takes **`&mut self`**, and this is the key simplification: `load` is *sequential* (load field A fully, then B, …; a container loops one element at a time), so a single `&mut` reborrowed down the recursion is enough — no two simultaneous borrows. That means the read path needs **no `RefCell`** and can hand out the real seekable cursor (`impl Read + Seek`), dissolving the `Seek::seek`-takes-`&mut self` problem that plagued a `&self` read. The prototype's `read_at(&mut self) -> &mut S` demonstrates exactly this.

```rust
pub trait ReadBackend {
    type Pointer: Copy;
    type Size: Word;
    /// A reader positioned at `anchor + offset`. `&mut self` makes handing out a
    /// seekable cursor sound; the prototype returns the real `&mut Storage`.
    fn read_at(&mut self, anchor: Self::Pointer, offset: Self::Size) -> impl Read + Seek + '_;
    fn size(&self, p: Self::Pointer) -> Option<Self::Size>;
}

/// Convenience: name both halves at once, over the same pointer type AND size.
pub trait Backend<P, S>: ReadBackend<Pointer = P, Size = S> + WriteBackend<Pointer = P, Size = S> {}
impl<P, S, B: ReadBackend<Pointer = P, Size = S> + WriteBackend<Pointer = P, Size = S>> Backend<P, S> for B {}
```

> **Claude (the `&self`/`&mut self` asymmetry, verified in the prototype):** the natural split is **write = `&self`, read = `&mut self`**, and it falls out of *why* each exists. Writes go through guards that reborrow a shared `&B` (needs `&self`); reads happen during a sequential `load` that owns the backend exclusively for the duration (can take `&mut self`). A free side effect: the borrow checker then forbids `load` (needs `&mut B`) while any guard (holds `&B`) is alive — a correct, automatic enforcement of "don't read stale data mid-write". Because `ReadBackend` is `&mut self`, the whole `RefCell<Storage>`-on-the-read-path problem simply doesn't arise; interior mutability is confined to the *write* facade in the concrete backend.

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

### `struct UnjournaledBackend<S: Storage, A: Allocator<Meta=...>>`
- **Composes** an `S` (on-file bytes) and an `A` (in-memory allocation table) — it does *not* extend `Allocator`. It implements `WriteBackend` and `ReadBackend` (hence the convenience `Backend<P, S>`). Addresses stay hidden from user types: they exist on the inner `A`, but the backend never returns one.
- Implements read/write by asking the inner `A` for a pointer's address and then reading from / writing to the `Storage`. For `WriteBackend::resize`, it *consumes* the inner allocator's `Some((old, new))` relocation report to move the bytes in `Storage`, and returns nothing.
- Forwards mutating allocator calls to `A` but also immediately persists a compact version of the state (used only to recreate the in-memory `A` on open, not optimized for lookups). The allocator-state allocations are themselves managed by `A` (self-hosting; see the bootstrap problem).
- The `&self` write facade is exactly the composition boundary: the struct wraps `(S, A)` in a `RefCell` so `WriteBackend`'s `&self` methods can borrow the `&mut self` `Allocator` through it. The `ReadBackend` methods take `&mut self` and reach the inner state via `RefCell::get_mut` — no runtime borrow, and the real seekable cursor comes straight out.
- Probably not really useful for Kladde, but a good versatility test and a good first step before `JournaledBackend`.

```rust
pub struct UnjournaledBackend<S: Storage, A: Allocator> {
    inner: RefCell<(S, A)>,   // S = on-file bytes; A = in-memory allocation table
}
// impl WriteBackend (&self): inner.borrow_mut() -> a.address(p) -> s.seek(..) -> write;
//                            resize consumes a.resize(..) -> Some((old,new)) -> move bytes
// impl ReadBackend (&mut self): inner.get_mut() -> a.address(p) -> s.seek(..) -> hand out &mut S
// no TransparentAllocator; addresses never leave the backend
```

(The prototype implements exactly this and it composes — including `read_at(&mut self) -> &mut S` handing out a genuinely seekable cursor, which the earlier `&self` shape could not.)

### `struct JournaledBackend<S: Storage, A: Allocator<Meta=...>>` (and `JournaledWriteBackend`)
- Same composition as `UnjournaledBackend`, but only `JournaledBackend` provides read operations. The two are newtype wrappers around a shared private inner type; `JournaledWriteBackend::flush(&mut self) -> impl ReadBackend` replays, then hands back a read-only view. Never hand out an owned readable+writable journaled backend. Here the phase split is enforced *by the trait split itself*: `JournaledWriteBackend: WriteBackend` (no read surface), and the flushed view is a `ReadBackend` (no write surface); the `&mut self` on `flush` guarantees no live guard remains (guards hold `&self`).
- Forwards allocator calls to `A` but calls `reserve_*` instead of `alloc_*` (and `reserve_resize` instead of `resize`), appending a corresponding `Op`. The `Op` records the id, size, and sizedness but *not* the address — address assignment is deferred to replay's `claim_*`, so the journal can grow at the end of the file without colliding with regions reserved by earlier ops.
- May hold extra in-memory data tying the in-memory state to the persisted state.
- Uses the inner `A` to manage its journal, but can't journal the allocation of journal space itself (infinite loop) — needs the `Storage` header for the bootstrap (see the self-hosting problem).
- Runs journal replay automatically when the journal overflows outside a transaction: replay assigns real addresses to new allocations and resolves the addresses of buffered reads/writes.

```rust
struct JournaledInner<S: Storage, A: Allocator> {
    storage: S,
    alloc: A,
    journal: /* append-only, framed Op log; see spec.md On-Disk Layout */,
}
pub struct JournaledWriteBackend<S, A>(RefCell<JournaledInner<S, A>>);  // impl WriteBackend only
pub struct JournaledBackend<S, A>(JournaledInner<S, A>);                // impl ReadBackend after flush

impl<S: Storage, A: Allocator> JournaledWriteBackend<S, A> {
    fn flush(&mut self) -> &mut JournaledBackend<S, A> { /* replay, expose read-only view */ }
}
// alloc_* -> alloc.reserve_*(size) + append Alloc-Op (id, size, sizedness; NO address)
// resize  -> alloc.reserve_resize(..) + append Resize-Op (NO address)
// replay  -> alloc.claim_*(..)        + apply pending writes at now-known addresses
```

> **Claude (re "can the two backends share code?", and compaction):** yes — journal **replay** is essentially "run the `UnjournaledBackend` immediate path over the buffered ops." Factor a single `fn apply(alloc: &mut A, storage: &mut S, op: Op)` that both call: `UnjournaledBackend` calls it inline per mutation, `JournaledBackend` calls it per op during replay. Under composition this is cleaner still — both hold `(S, A)` and the shared `apply` takes `&mut A, &mut S` directly. And compaction becomes markedly simpler under Id-serialization: the allocator computes a move plan `[(id, old_addr, new_addr)]`, the backend performs the `storage` moves and updates the table — and because serialized pointers are stable ids, **no on-disk pointer needs rewriting** (contrast the old address-serialization design, which had to rewrite every serialized target). That is the upside of the ID-serialization pivot, now recorded in `spec.md`.

### `trait WriteBackendExt: WriteBackend`

Extension trait with a blanket implementation, defined in the crate where `Persistable` is defined. **It extends `WriteBackend`, not `Allocator`** — the raw `Allocator` is deliberately type-agnostic (it knows nothing of `Persistable`), and `Persistable` implementors hold a `&WriteBackend`, not a `&mut Allocator`. So the *typed* conveniences (which reference `T: Persistable` for `T::INLINE_SIZE`) live on the backend side, where `alloc_*` is `&self`. See Section 4.3 in `allocator-spec.md` except for the following nitpicks:
- Don't call methods `*_boxed`. Call them `*_typed` instead.
- `promote`/`demote` between `UniquePointer<T, P>` and `UniquePointerFixedSize<P>` are the *inherent* `UniquePointer::from_fixed` / `UniquePointer::into_fixed` now that pointers are concrete — no extension-trait method needed (`UniquePointer<T, P>` is just `UniquePointerFixedSize<P>` + `PhantomData<T>`).
- Split `alloc_array` into two methods:
	- `alloc_fixed_size_array<T: Persistable<Self::Pointer>>`, which returns a `UniquePointerFixedSize<Self::Pointer>`; and
	- `alloc_resizable_array<T: Persistable<Self::Pointer>>`, which returns a `UniquePointerResizable<Self::Pointer>`

```rust
// Defined in the `Persistable` crate; blanket-impl'd for every `WriteBackend`
// (methods are `&self`, matching `WriteBackend`).
pub trait WriteBackendExt: WriteBackend {
    fn alloc_typed<T: Persistable<Self::Pointer>>(&self) -> UniquePointer<T, Self::Pointer> {
        // T::INLINE_SIZE is a usize byte count -> convert to the backend's Size.
        UniquePointer::from_fixed(self.alloc_fixed(Word::from_usize(T::INLINE_SIZE)))
    }
    fn free_typed<T>(&self, p: UniquePointer<T, Self::Pointer>) { self.free_fixed(p.into_fixed()) }
    fn alloc_fixed_size_array<T: Persistable<Self::Pointer>>(&self, len: usize) -> UniquePointerFixedSize<Self::Pointer>;
    fn alloc_resizable_array<T: Persistable<Self::Pointer>>(&self, len: usize) -> UniquePointerResizable<Self::Pointer>;
}
impl<B: WriteBackend + ?Sized> WriteBackendExt for B {}
```

(With concrete pointers, `T` is the boxed value and `Self::Pointer` is the concrete id type — no allocator-carrying pointer type parameter to thread. Extending `WriteBackend` rather than `Allocator` keeps the raw allocator type-agnostic and matches the `&self` write API.)

### `Persistable<P>` and `kladde-types`

`Persistable` and the `kladde-types` containers are generic over the pointer type `P`, defaulted to `Pointer`, so the common case stays parameter-free while a file may opt into a wider `P`. A container that stores pointers sets the same default and implements `Persistable<P>` generically. `Persistable` and `kladde-types` containers are *not* generic over the `Size` type, however, because `Size` is used only for allocation sizes and offsets, and
- *allocation sizes belong to the allocator, not the container* (the `INLINE_SIZE` decision — containers don't store the size of allocated memory in the parent allocation; they query `backend.size(ptr)`, which returns `B::Size`); and
- *offsets are transient* — computed at the moment of a read/write (`Word::from_usize(i * T::INLINE_SIZE)`), never stored.

TODO: include the above two arguments in a doc comment on `trait Persistable`.

Verified in `crates/generic-alloc`:

```rust
pub trait Persistable<P = Pointer>: Sized {
    const INLINE_SIZE: usize;   // just the inline pointer id; may depend on P's width
    // store gets a shared &WriteBackend (guard reborrow); load gets an exclusive
    // &mut ReadBackend (sequential reads). The asymmetry is the whole point.
    // `Location<P, B::Size>`: the size type flows from the backend, so `S` is
    // NOT a `Persistable` parameter -- only `P` is.
    fn store<B: WriteBackend<Pointer = P>>(&mut self, backend: &B, location: Location<P, B::Size>);
    fn load<B: ReadBackend<Pointer = P>>(backend: &mut B, location: Location<P, B::Size>) -> Self;
}

// kladde-types: default P = Pointer here too, and implement generically over P.
pub struct PersistableVec<T, P = Pointer> { data: Vec<T>, pointer: Option<UniquePointerResizable<P>> }
impl<T: Persistable<P>, P: Copy> Persistable<P> for PersistableVec<T, P> {
    const INLINE_SIZE: usize = std::mem::size_of::<P>();   // just the pointer id, no inline len
    // store pins the write backend, load pins the read backend, both to `P`.
}
```

The `store`/`load` methods pin the backend's pointer type to `P` (`WriteBackend<Pointer = P>` / `ReadBackend<Pointer = P>`), so a `PersistableVec<T, P>` only works with a backend whose pointer type is `P`. `INLINE_SIZE` is a per-`P` const, and for a pointer-holding type it is **just the pointer id** — `size_of::<P>()` (4 for the default `NonZeroU32` id, 8 for a `NonZeroU64` one). There is *no inline `len`*: the length/size is owned by the allocator (query `size`/`lookup`; for a resize-to-exact vec, `len = size / elem_size`) or stored one indirection away for a chunked layout — matching the "the size only stored by the allocator, not by the `PersistableVec` itself" goal in the chunked-vec test case below. Empty is the null pointer, free via `Option<Pointer>`'s niche. This compiles (verified in `crates/generic-alloc`).

**TODO: write real implementor documentation for `Persistable`.** It should tell implementors there are three choices:

1. If `MyType` stores no pointers, `impl<P> Persistable<P> for MyType { ... }` — works at *every* pointer width; just don't assume any particular width.
2. If `MyType` must store pointers, make it generic over the pointer type with a default and implement generically: `struct MyType<P = kladde::Pointer> { ... }`, then `impl<P> Persistable<P> for MyType<P> { ... }`.
3. If you don't care about non-default widths, implement only for the default: `impl Persistable for MyType { ... }` (no `P` generic on `MyType`). This pins `MyType` to `P = Pointer`; using it in a non-default-width file won't compile.

## Problems and regressions

Written by Claude. This section collects the problems this proposal may run into and the concrete regressions relative to what is implemented today (the `allocator-spec.md` overhaul, now landed) or planned there. Several items are confirmed against the throwaway prototype in `crates/generic-alloc`; the rest are design-level. Resolved items that used to live here have been removed rather than kept for comparison: "associated pointer types infect the `Persistable` stack" and "serializing `Id` reinstates the on-disk handle table" (fixed by the concrete-pointer and ID-serialization decisions; the ID trade-off is now in `spec.md`); the **`&self`-read/`Seek` clash** and the **`&self`-vs-`&mut self` tension** (dissolved by making the backend a *composition* of an `Allocator` with **write = `&self`**, **read = `&mut self`** — see the backend sections); and the **journaled `write` returning a bare `impl Write`** hazard (the `WriteBackend::write` now *takes the bytes*, keeping the backend in control of the op frame). The restructure that dissolved those introduces new costs, below (items 3–5).

1. **The pointer-width parameter `P` leaks into generic code and into pointer-holding structs** *(new, from the `Persistable<P = Pointer>` decision; confirmed in the prototype).* The default keeps the common case parameter-free, but the width still surfaces in a few places:
   - **A *bound* `where T: Persistable` silently means `Persistable<Pointer>`** (omitted defaults apply in bounds, like `T: Add` ≡ `T: Add<T>`). So Id-polymorphic *library* code must write `T: Persistable<P>` and thread `P` — never bare `T: Persistable`, which would reject a `Persistable<Pointer<u64>>`-only type with a confusing error. This bites `kladde-types`/the derive macro, not application code at a fixed width.
   - **A hand-written `impl Persistable for MyType` covers only the default width;** universal support needs `impl<P> Persistable<P> for MyType` (trivial for pointer-free types — the derive emits it). This is implementor choice (3) below.
   - **Any struct that stores a pointer-holding field inherits `P`** to support non-default widths (`Foo { v: PersistableVec<T, P> }` ⇒ `Foo<P>`), because `INLINE_SIZE` and the stored owned handle both depend on `P`. The default keeps the *common* case `Foo` param-free; only files opting into a wider `P` pay.
   - **Fully-unconstrained inference needs an annotation** (`PersistableVec::new()` with nothing pinning `P` → "type annotations needed"), where a non-generic type never asks.

   All of this is confined to library/generic code and to non-default-width files; application code at one fixed width sees essentially the non-generic experience.

2. **Generic integer associated types need an "unsigned word" bound that std doesn't provide.** Making `Address`/`Size` associated types means every offset/size computation needs `+`, `<`, `Into<Address>`, and `usize` conversions over a generic type. Rust has no single "unsigned integer" trait, so you need a helper trait (the prototype's `Word`) or a dependency like `num-traits`, and its bounds ride along on every generic function that does address arithmetic. Manageable, and `Size: Into<Address>` is the right core relation, but it is real bound-noise that the current concrete-`u32` code doesn't have. (You'll also want `TryFrom<usize>`/`to_usize`, because offsets from Rust collections arrive as `usize`.)

3. **Duplicated surface: `alloc_*`/`free_*`/`resize`/`size` appear on both `Allocator` and the backends** *(new, from the composition-not-extension decision).* Because `Backend` no longer *is* an `Allocator`, each backend must forward-and-translate every management op: address-aware and `&mut self` on the `Allocator`, address-hidden and `&self` (write) / `&mut self` (read query) on the backend. That is real boilerplate — mostly mechanical, absorbable by a default-method layer or a small macro, but it exists where `Backend: Allocator` had none. It is the (worthwhile) price of letting the two layers choose their `&self`/`&mut self` and address-visibility independently. A related minor cost: `ReadBackend` and `WriteBackend` each carry their own `type Pointer` *and* `type Size`, so code wanting both over one pointer/size pair must say `ReadBackend<Pointer = P, Size = S> + WriteBackend<Pointer = P, Size = S>` — the blanket `Backend<P, S>` convenience packages exactly that, but bare `R + W` bounds must repeat both pins.

4. **`resize -> Option<(old, new)>` conflates two "no move" cases, and the in-place-*grow* case still needs backend work** *(new; confirmed in the prototype).* `None` means both *unclaimed* (no address yet) and *in-place claimed* (address unchanged) — fine for the byte-move decision, but on an in-place **grow** the backend must still ensure `Storage` covers `[addr, addr + new_size)`, which the `Option` doesn't signal. So the backend can't rely on the `Option` alone: either make the result 3-way (`Relocated{old,new}` | `InPlace{addr}` | `Unclaimed`), or keep `Option` and have the backend size `Storage` by *querying* the pointer's address+size after every resize regardless (the prototype does the query-based variant). Decide which; both work.

5. **`reserve`/`claim` adds a "valid but unclaimed" state — now with a *reserved-resize* on top of reserved-alloc** *(the reserved-resize part is new, from generalizing deferral to `resize`).* Allowing *every* `Allocator` method on reserved-but-unclaimed pointers means `free`, `resize`, `make_fixed`, … must each define their effect on a reservation; and because a journaled backend must also defer address assignment on `resize` (not just `alloc`), there is a second reserved state to specify. It's expressible (the prototype models a reservation as a table row with `address: None`), but write down the state machine (reserved-alloc → claimed → freed; reserved-resize → claimed; reserved → freed = cancel), and note that a backend returning an error for I/O on unclaimed pointers turns every write path fallible where it wasn't before.

6. **Self-hosting bootstrap is a genuine open problem, not a detail.** Storing the allocator's own state in allocations it manages, and its journal in memory it allocates, is circular: you can't journal the allocation of journal space, and you can't read the allocator table without first knowing where it lives. This needs a bootstrap anchor *outside* the general mechanism — the fixed `Storage` header holding the root address of the persisted allocator state, plus journal space managed specially. The current implementation avoids this entirely because the mock allocator is in-memory and never persists its state; making the allocator self-hosting is new, load-bearing work.

7. **General over-generalization risk.** The document worries (twice) about generalizations hurting the Kladde common case. With pointers concrete, the widths settled as defaulted type parameters, and the backend/allocator split now resolving the `&self`/read-isolation tensions, the sharpest instances are gone; the residual caution stands: gate each remaining generalization (the full `Address`/`Size`/`Meta` genericity, `reserve`/`claim` including reserved-resize, the duplicated backend surface) on a concrete need — the chunked-vec test case is the forcing function — rather than adopting all of it up front. Keep without hesitation: separating `Storage` from `Allocator`; the composition + `ReadBackend`/`WriteBackend` split with `read = &mut self` / `write = &self`; concrete pointers with defaulted widths; `size` (not `capacity`); `Meta`/`lookup`. Defer or drop: the `write_vectored`-through-a-returned-`Write` convenience (given up by having `write` take bytes — items above) and segments (deferred already — see `later.md`).

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
