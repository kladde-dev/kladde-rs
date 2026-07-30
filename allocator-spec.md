# Allocator & Pointer Overhaul — Specification

Status: **draft**, forward-looking. This document specifies a redesign of the
`Allocator` trait and the pointer types it deals in. It supersedes the current
`UniquePointer<T>` / `UniqueArrayPointer<T>` / `RawPointer` trio and the
byte-unit `Allocator` methods (`alloc_array`, `resize_array`, `array_capacity`,
`splice`, …). It does **not** change the on-disk journal microop set
(`Alloc`/`Free`/`Write`/`Copy`/`Resize`/`Splice`) or the crash-consistency
model — see `spec.md`; this is a change to the *type surface*, not the storage
mechanism.

Sections marked **(normative)** define the contract every implementation must
satisfy. Section [§5](#5-motivation-non-normative) is **(non-normative)** — it
motivates the layering but imposes no requirement.

---

## 1. The four pointer types (normative)

Every pointer is, representationally, a stable allocation identity — a
`NonZeroU32` `index` assigned once by the allocator and never changed for the
lifetime of the pointer, even as the region it names is relocated by compaction
and even before anything is flushed (see `spec.md`, "Pointers and Memory
Management"). The four types differ only in what *operations* they permit and in
whether they carry a static type.

| Type                     | Owned?                | `Copy`? | Type-erased? | Size                                         | Resizable? |
| ------------------------ | --------------------- | ------- | ------------ | -------------------------------------------- | ---------- |
| `UniquePointerResizable` | yes                   | no      | yes          | variable, stored by the allocator            | **yes**    |
| `UniquePointerFixedSize` | yes                   | no      | yes          | fixed, recoverable without per-block storage | no         |
| `UniquePointer<T>`       | yes                   | no      | no (`T`)     | fixed = `T::INLINE_SIZE`                     | no         |
| `RawPointer`             | no (borrowed address) | **yes** | yes          | — (address only)                             | —          |

### 1.1 `UniquePointerResizable` — owned, erased, variable-capacity

The `Box<[u8]>`-like handle: a single-owner reference to a region whose byte
capacity is chosen at runtime and may change. **This is the fundamental currency
of the required `Allocator` contract** ([§4.1](#41-required-layer-normative)):
the minimum every allocator must support is variable-sized, type-erased regions.

```rust
pub struct UniquePointerResizable {
    index: NonZeroU32,
}
```

Its capacity is owned and remembered by the allocator (queryable via
`capacity`), because it is not recoverable any other way. Not `Clone`/`Copy`
(single owner ⇒ no double-free) and not `Serialize`/`Deserialize` (`index` is
process-local — serialize via `resolve`, see `spec.md`).

### 1.2 `UniquePointerFixedSize` — owned, erased, fixed-size

The same single-owner, type-erased handle, but for a region whose size is
**fixed for the lifetime of the file** (up to schema evolution) and therefore
does not need to be persisted per block: it is recoverable either from the
static type of whatever lives there, or from metadata the owning container keeps
once (e.g. a chunked vector storing one chunk capacity for all its chunks).

```rust
pub struct UniquePointerFixedSize {
    index: NonZeroU32,
}
```

Carries no static `T` — it is the handle for fixed-size regions whose size is a
*runtime* value with no single `Persistable` type (the motivating case is a
future chunked container's chunks). Same ownership/serialization rules as
`UniquePointerResizable`. **Deliberately exposes no `resize`/`splice`** — see
[§3](#3-the-fixedresizable-gate-normative).

### 1.3 `UniquePointer<T>` — typed, fixed-size (extension layer)

`UniquePointer<T>` is `UniquePointerFixedSize` plus a phantom type, provided at
the extension-trait layer ([§4.3](#43-typed-convenience-extension-trait-normative)),
not by the core trait:

```rust
pub struct UniquePointer<T> {
    inner: UniquePointerFixedSize,
    _marker: PhantomData<*const T>,
}
```

The `Box<T>` analog: it names one fixed-size `T` whose size is the statically
known `T::INLINE_SIZE`, so the size need not be stored at all — the reader
re-derives it from the layout/schema at that location. `PhantomData<*const T>`
(not `PhantomData<T>`) gives covariance in `T` — sound here, since mutation only
happens through an exclusive `Guard`, never a shared reference — without the
"may drop a `T`" drop-check obligation, which `UniquePointer` never discharges
(it holds no backend and never runs `T::drop`; freeing is the owning type's job,
see `spec.md`, "Freeing"). Converts to/from its erasure with `into_fixed()` /
`UniquePointer::from_fixed`. Like `UniquePointerFixedSize`, exposes no
`resize`/`splice`.

### 1.4 `RawPointer` — the `Copy` address

```rust
pub struct RawPointer {
    index: NonZeroU32,
}
```

A type- and size-erased *address*, produced from any owned handle by `.raw()`.
It is the target of the address-based byte operations (`read`/`write`/`copy`)
and the `anchor` of a `Location`. It is `Copy` precisely because it owns nothing:
it is an address to read/write at, never a handle that frees or resizes the
region it names. It carries no size and needs none — every operation through it
supplies its own byte range.

### 1.5 Common surface

Every owned handle exposes `index() -> NonZeroU32` and `raw() -> RawPointer`.
These may be unified behind a small **sealed** `OwnedRegion` trait (implemented
by the three owned types) so generic code can name "any owned region" without
that trait being implementable downstream. Ownership transfers rather than
duplicating (moving a handle out of one value into another never frees it), so
at every point exactly one live value owns each region.

---

## 2. Why exactly one `Copy` (erased) type (normative rationale)

There are three *owned* handles but only one *borrowed* (`Copy`) one. This
asymmetry is intentional, not an omission, and there is no
`RawFixedSizePointer`/`RawResizablePointer` split:

**Size (fixed vs. resizable) is a *lifecycle* property, and only owned handles
perform lifecycle operations** (alloc / free / resize / splice / capacity).
`RawPointer` performs only *address-based byte I/O* (`read`/`write`/`copy`),
where the caller supplies the range on every call. That operation is size- **and**
shape-agnostic, so a fixed-vs-resizable raw split would encode a distinction no
raw operation ever consumes — two types with identical behavior.

The confirming detail: **all three owned handles' `.raw()` collapse to the same
`RawPointer`.** The raw layer deliberately erases type *and* size together,
because byte I/O needs neither. If code ever wants size information at a
`RawPointer`, that is a signal it actually needs the owned handle in scope —
which is the borrow-checked liveness property that gate ([§3](#3-the-fixedresizable-gate-normative))
provides anyway.

---

## 3. The fixed/resizable gate (normative)

**`resize` and `splice` accept only `&UniquePointerResizable`.** There is no
coercion from either fixed handle (`UniquePointerFixedSize`, `UniquePointer<T>`)
to `UniquePointerResizable`, so calling a size-changing operation on a fixed
region is a *compile error*, not a runtime check or a convention.

`resize` and `splice` are the *only* size-changing operations: `write`/`copy`
stay within existing bounds, and `free` removes the region entirely. Gating
exactly those two is therefore complete — there is no third growth path to leak
through.

**`free` remains available on every owned handle, fixed regions included** — a
fixed region must still be reclaimable (the root, boxed values, and future
chunks are all freed eventually). Only the *size-changing* operations are gated;
freeing, `raw()`, and `resolve` apply uniformly. Availability by handle:

| operation                            | `Resizable` | `FixedSize`                | `UniquePointer<T>`       | `RawPointer` |
| ------------------------------------ | ----------- | -------------------------- | ------------------------ | ------------ |
| `free` / `free_fixed` / `free_boxed` | ✓           | ✓                          | ✓                        | —            |
| `resize`                             | ✓           | ✗ (compile error)          | ✗ (compile error)        | —            |
| `splice`                             | ✓           | ✗ (compile error)          | ✗ (compile error)        | —            |
| `capacity`                           | ✓           | — (size known, not stored) | — (size = `INLINE_SIZE`) | —            |
| `raw()`                              | ✓           | ✓                          | ✓                        | (is one)     |
| `read`/`write`/`copy`                | via `raw()` | via `raw()`                | via `raw()`              | ✓            |
| `resolve`                            | ✓           | ✓                          | ✓                        | —            |

**Consequence (this is the point).** Type-enforcement upgrades "fixed" from a
*hint* to a *guarantee*: an allocator may assume a fixed region never resizes,
because the type system makes calling `resize`/`splice` on one impossible.
A fixed-size-exploiting allocator ([§5](#5-motivation-non-normative)) can
therefore bin fixed regions with zero growth headroom and needs **no
"someone resized a fixed block" fallback path at all**. Without the type gate,
`alloc_fixed` would be a mere hint and every exploiting allocator would have to
carry that fallback for correctness.

---

## 4. The layered `Allocator` trait (normative)

Three layers: a required, type-erased, resizable minimum; an optional,
defaulted, fixed-size layer allocators may specialize; and typed convenience on
top.

### 4.1 Required layer (normative)

The minimum contract. All erased; the sizeable operations are all *resizable*,
because resizable regions are the weakest assumption an allocator can be asked to
support.

```rust
pub trait Allocator {
    // --- Address-based byte I/O: type- and size-agnostic. ---
    // `read` is a query against already-flushed state, not a recorded mutation.
    fn read(&self, target: RawPointer, offset: u32, len: u32) -> Vec<u8>;
    fn write(&self, target: RawPointer, offset: u32, bytes: &[u8]);
    fn copy(&self, src: RawPointer, src_offset: u32, len: u32,
            dst: RawPointer, dst_offset: u32);

    // --- Resizable regions: the required lifecycle. ---
    fn alloc_resizable(&self, byte_size: usize) -> UniquePointerResizable;
    fn free_resizable(&self, pointer: UniquePointerResizable);
    fn resize(&self, pointer: &UniquePointerResizable, new_byte_size: usize);
    fn splice(&self, pointer: &UniquePointerResizable,
              byte_offset: u32, old_byte_len: u32, new: &[u8]);
    fn capacity(&self, pointer: &UniquePointerResizable) -> Option<usize>;

    // --- Resolve any owned region to its current on-disk target. ---
    // Takes a `RawPointer` (obtained via `.raw()`), since resolution is a
    // size-agnostic query; borrows `self` so the returned target cannot be
    // invalidated (e.g. by compaction) while the `ResolvedPointer` lives.
    fn resolve<'a>(&'a self, pointer: RawPointer) -> Option<ResolvedPointer<'a>>;

    // --- Fixed-size layer (see §4.2): defaulted, overridable. ---
    fn alloc_fixed(&self, byte_size: usize) -> UniquePointerFixedSize { /* default, see §4.2 */ }
    fn free_fixed(&self, pointer: UniquePointerFixedSize) { /* default, see §4.2 */ }
}
```

All parameters and returns are **bytes**. Unit conversion (element counts,
`INLINE_SIZE`) belongs to the caller and to the typed layer ([§4.3](#43-typed-convenience-extension-trait-normative)),
never to the allocator — the allocator stays byte-geometry-only, so it never
needs type-specific callbacks (`spec.md`, "Pointers and Memory Management").
Every mutating method records the same journal microop it does today; the
overhaul only renames and re-types the surface.

### 4.2 Fixed-size layer (normative: defaults; optional: overrides)

`alloc_fixed` and `free_fixed` are the *only* fixed-specific entry points. The
gate ([§3](#3-the-fixedresizable-gate-normative)) means fixed regions never
`resize`/`splice`, and `free`/compaction can recover whatever they need from
metadata recorded at allocation time — so no other fixed variant is needed.

Their **default implementations behave exactly as an ordinary resizable
allocation and free** (allocate/reclaim a byte region with no special treatment),
returning/consuming the fixed handle type. A conforming allocator that does not
care about fixed-size exploitation needs to write nothing here.

An allocator **may override** them to exploit fixed size (bin by size class,
omit per-block size storage, compact more aggressively — see
[§5](#5-motivation-non-normative)). Because `alloc_fixed`/`free_fixed` are the
allocation and reclamation points, size-class pooling is fully expressible
through overriding just these two.

### 4.3 Typed convenience (extension trait, normative)

The typed surface is a blanket-implemented extension trait, so the core
`Allocator` contract stays purely erased while call sites stay ergonomic:

```rust
pub trait AllocatorExt: Allocator {
    /// A boxed single fixed-size value (`Box<T>`), sized from `T::INLINE_SIZE`.
    fn alloc_boxed<T: Persistable>(&self) -> UniquePointer<T> {
        UniquePointer::from_fixed(self.alloc_fixed(T::INLINE_SIZE))
    }
    fn free_boxed<T>(&self, pointer: UniquePointer<T>) {
        self.free_fixed(pointer.into_fixed())
    }

    /// A resizable run of `len` elements, sized from `T::INLINE_SIZE`.
    /// Returns the *erased* resizable handle: `T` is consumed only to compute
    /// the byte size (the caller addresses elements itself via `.raw()`).
    fn alloc_array<T: Persistable>(&self, len: usize) -> UniquePointerResizable {
        self.alloc_resizable(len * T::INLINE_SIZE)
    }
}
impl<A: Allocator + ?Sized> AllocatorExt for A {}
```

Note the deliberate asymmetry: `alloc_boxed` *retains* `T` (a fixed value has a
meaningful static type), whereas `alloc_array` consumes `T` for the size
computation and returns an erased handle (the array's `T` was never doing more
than sizing — see the earlier design discussion). A typed *fixed*-array helper
(`-> UniquePointerFixedSize`) is the natural addition once a chunked
representation exists; it is intentionally omitted until then.

**Never parameterize a pointer or a typed method by a *container* type.** The
`T` in `alloc_boxed::<T>` / `alloc_array::<T>` is the boxed value or the array
*element*, whose `INLINE_SIZE` is the real stride — never
`PersistableVec<E>`/`PersistableHashMap<K, V>`, whose `INLINE_SIZE` is only the
8-byte header. For `PersistableHashMap`, the element that makes this work is a
real `Persistable` entry type — see [§6](#6-the-hashmap-element-type-normative).

---

## 5. Motivation (non-normative)

This section justifies the layering of [§4](#4-the-layered-allocator-trait-normative);
it imposes no requirement. Nothing here is needed for a conforming allocator,
and the v1 mock does none of it.

A concrete, file-backed allocator can exploit the knowledge that some
allocations are fixed-size — a well-established technique (slab / size-class
allocation: jemalloc/tcmalloc size classes, the Linux slab allocator, pool
allocators):

- **Size-class binning.** Fixed regions of the same size go in a per-class
  free-list: O(1) alloc/free, no intra-class fragmentation, good locality. Today
  there is one fixed allocation (the file root); under a future chunked
  representation there would be *many* fixed regions of the same size — the ideal
  size-class workload.
- **Differentiated compaction.** Fixed regions never grow, so they can be packed
  tightly; resizable regions benefit from slack so they can grow in place without
  relocating. Segregating the two lets each be managed on its own terms.
- **No per-block size metadata.** A fixed region's size is recoverable without
  storing it per block (from the static type, or from once-per-container
  metadata), so an exploiting allocator can omit the size field that resizable
  regions must persist.

The type-level gate ([§3](#3-the-fixedresizable-gate-normative)) is what makes
these safe *without a fallback path*: since `resize`/`splice` are compile-time
impossible on fixed handles, "this region never grows" is a guarantee the
allocator can build on, not a hint it must defensively handle. This is the
reason the fixed layer is a distinct, defaulted set of methods rather than a
runtime flag on the resizable ones — an allocator opts into exploiting it by
overriding `alloc_fixed`/`free_fixed`, and one that doesn't gets correct
behavior from the defaults for free.

"Fixed up to schema evolution" is the intended qualifier: within one schema
version a fixed size is constant; a schema change may alter `INLINE_SIZE`, but
that triggers a re-layout anyway, so binning/compaction operating within a
version is unaffected.

---

## 6. The hashmap element type (normative, not yet implemented -- TODO)

To satisfy the "never parameterize by container type" rule
([§4.3](#43-typed-convenience-extension-trait-normative)),
`PersistableHashMap<K, V>`'s content becomes an array of a real `Persistable`
element type rather than a hand-rolled tagged slot:

```rust
enum Entry<K, V> {
    Empty,
    Occupied(K, V),
    Tombstone,
}
// derives Persistable
```

The map's content allocation is then `alloc_array::<Entry<K, V>>(len)` — the same
typed array path the vector uses, with no container-type parameter anywhere. The
liveness/tombstone states that are currently a hand-written 1-byte tag become
proper variants of a sum type, and removal (tombstoning) becomes assignment of
the `Tombstone` variant.

Caveats to enter with eyes open:

- A derived enum's discriminant is **4 bytes**, versus today's 1-byte tag
  (+3 bytes/slot). The compact-enum-encoding idea in `later.md` (smallest byte
  width for a statically known variant range → 1 byte for a 3-variant enum)
  brings this back to parity; note the dependency.
- It changes the on-disk slot layout (discriminant-first vs. tag-first) — fine
  pre-release, but the file-format spec must reflect it.

---

## 7. Relationship to the current implementation

This overhaul renames and re-types; it does not touch the journal microops or
crash-consistency model. Approximate mapping from the current surface:

| Current                                                                                | This spec                                                                                            |
| -------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| `UniqueArrayPointer<T>` (content pointer, container-typed)                             | `UniquePointerResizable` (erased)                                                                    |
| `UniquePointer<T>` (typed fixed)                                                       | `UniquePointer<T>` (unchanged role; now `UniquePointerFixedSize` + phantom)                          |
| —                                                                                      | `UniquePointerFixedSize` (new: erased fixed)                                                         |
| `RawPointer`                                                                           | `RawPointer` (unchanged)                                                                             |
| `Allocator::alloc_array` / `free_array` / `resize_array` / `array_capacity` / `splice` | `alloc_resizable` / `free_resizable` / `resize` / `capacity` / `splice`                              |
| `Allocator::alloc` / `free` (typed fixed)                                              | `AllocatorExt::alloc_boxed` / `free_boxed` (typed) + `alloc_fixed` / `free_fixed` (erased, defaulted) |
| `PersistableHashMap` hand-rolled `1 + K + V` slot                                      | array of `Entry<K, V>`                                                                               |

Migration is contained: the resizable/erased handle is already nearly
encapsulated inside `PersistableVec`/`PersistableHashMap`, and `UniquePointer<T>`
keeps its one real client (the `Kladde` root). `UniquePointerFixedSize` has no
client yet — it exists ahead of the chunked representation that will use it, so
that "fixed never resizes" is a compile-time guarantee across both the typed and
erased cases from the outset, rather than a refactor deferred until chunks
arrive.
