# Implementation Summary: Type Descriptors & Schema Fingerprints

An executive overview of how [`type-descriptors.md`](type-descriptors.md) is
implemented, and where to go to change any given piece. This assumes you've read
the spec but not the plan or the code.

## The one-paragraph picture

`kladde-schema` is a **pure, self-contained crate** that implements the entire
spec (model + serialization + fingerprint) against hand-built descriptor graphs.
It depends only on `kladde-varint` — it knows nothing about `Persistable`, Rust
types, or the rest of the workspace. The bridge from *real Rust types* to that
model lives **outside** this crate, in `kladde-traits` (the `describe` hook +
`SchemaBuilder`) and `kladde-derive` (codegen). So there are really two layers:

1. **The spec engine** (`kladde-schema`) — model, bytes, hashes. Language-neutral,
   the thing a cross-language conformance suite would target.
2. **The Rust binding** (`kladde-traits` + `kladde-derive` + `kladde-types`) —
   turns a `Persistable` Rust type into a `TypeTable`, then calls the engine.

If you're changing *what a schema means or how it hashes*, you're in layer 1. If
you're changing *how Rust types map onto the model*, you're in layer 2.

## Layer 1: `kladde-schema` — file by file

Four small source files, ~1300 lines total including tests.

### [`descriptor.rs`](crates/kladde-schema/src/descriptor.rs) — the model (spec §2)

The in-memory data model. Nothing here computes anything; it's the shape.

- `TypeDescriptor` — the four-kind enum (`Primitive` / `Struct` / `Enum` /
  `Opaque`). Array and Pointer are reserved as tag *constants* only, not enum
  variants.
- `TypeTable` — a `Vec<TypeDescriptor>` with the **index-0-is-root** convention
  baked in (`root()`, `get()`, `new()` which asserts non-empty).
- `TypeRef(usize)`, `Field`, `Variant`, `Version` — the supporting types.
  `Version` carries the `stability_flag()` / `leading_nonzero()` methods that
  implement the §2.4 compatibility-component folding.
- Tag byte constants `TAG_STRUCT=128 … TAG_POINTER=132`.
- **Two shared helpers that keep serialization and fingerprinting in lockstep:**
  - `TypeDescriptor::references()` — a descriptor's outgoing edges in *canonical
    order*.
  - `canonical_variants()` — enum variants sorted ascending by discriminant.

  Both serialization and fingerprinting call these, so the two code paths can
  never disagree about traversal order. **If you add a new descriptor kind or
  change canonical ordering, this is the first place to touch.**

### [`serialize.rs`](crates/kladde-schema/src/serialize.rs) — canonical bytes (spec §3)

`TypeTable::encode()` / `TypeTable::decode()`, plus the `DecodeError` enum.

- `encode` walks descriptors in index order, emitting the kind tag then
  kind-specific content per §3.2. Enum variants go through `canonical_variants`,
  so variant source-order never affects the bytes.
- `decode` uses a tiny `Reader` cursor over `&[u8]` (helpers: `varint`, `byte`,
  `take`, `string`, `reference`, `fields`) and **validates**: rejects reserved
  kinds (Array/Pointer), unknown kinds, bad discriminant widths, out-of-range
  references, trailing bytes, and empty tables.
- `varint`/`string`/`reference` encoding primitives (§3.1) are the free functions
  at the top; the actual LEB128 lives in the separate `kladde-varint` crate.

This is where to look for **on-disk format** questions and round-trip behavior.
Note: encode/decode is a *serialization* device for writing cyclic graphs as a
flat array — it is deliberately **not** on the fingerprint path.

### [`fingerprint.rs`](crates/kladde-schema/src/fingerprint.rs) — the hash (spec §4)

The heart of the design, and where the subtle logic is. Entry point:
`TypeTable::fingerprint()`.

- `Fingerprint([u8; 16])` — a plain 128-bit hash, no flags or reserved bits.
  `to_hex()`, `as_bytes()`.
- `Traversal` — the DFS state: `color[]` (white/gray/black), `depth[]`, and
  `memo[]` of computed fingerprints, all `Vec`s indexed by table index.
- `visit(node, depth)` — colors the node gray, builds its hash input, SHA-256s it,
  truncates to 16 bytes, memoizes, colors black, and returns the fingerprint.
- `encode_local` — item 1 of §4.5: the §3.2 encoding with Struct/Enum **names
  omitted** and the Opaque version **folded** to `stability_flag + leading_nonzero`.
- `emit_reference` — item 2 of §4.5, the crux. Three branches on child color:
  - **white** → recurse (`visit`), emit `0x00` + child's 16-byte fingerprint;
  - **black** → memo hit: emit `0x00` + the memoized fingerprint (a subtree reached
    more than once is hashed once, referenced by content);
  - **gray** → emit `0x01` + `varint(depth[N] − depth[C])`, a de Bruijn back-edge.

  In graph terms (§4.4): the white edges form the DFS spanning tree, white + black
  edges form a DAG that gets merkle-hashed, and gray edges are the back-edges,
  encoded by relative depth so a cyclic graph hashes as a finite tree.

**If you're changing what's fingerprinted or the token encoding, it's all here.**
The in-module tests cover reproducibility (including recursive graphs terminating
and hashing deterministically), index-invariance, structural sensitivity, Opaque
version folding, **sharing-invariance** (a shared vs. a duplicated subtree hash
identically — spec §5(d)), distinct structures staying distinct, and the **golden
vectors** (`GOLDEN_POINT`, `GOLDEN_LIST`, `GOLDEN_OPAQUE_1_2_3`) that lock the
byte-exact hash output.

### [`sha256.rs`](crates/kladde-schema/src/sha256.rs) — the hash primitive (spec §4.2)

A vendored, textbook FIPS 180-4 SHA-256, exposed as `Sha256Hasher` — a
**streaming** hasher (`new` / `update` / `finalize`) that buffers one 64-byte
block at a time and compresses as it fills. `fingerprint.rs` feeds each node's
hash input straight in (bytes, `s.as_bytes()`, child digests, and — via the
hasher's `impl Extend<u8>` — `kladde_varint::encode`), so a node is hashed
without ever materializing its input in a `Vec`. Vendored *on purpose*: it keeps
the crate dependency-free and the fingerprint exactly reproducible from the
published algorithm. Not perf-tuned (runs once per schema over a few hundred
bytes). Tested against standard NIST vectors plus a streaming-equals-one-shot
check. You'd only touch this if the format-version ever changed the hash function.

## Layer 2: the Rust binding (where types meet the model)

The spec engine hashes *tables you hand it*. Building those tables from live Rust
types happens here. This is layer-2 context — read it to understand the whole
pipeline, but changes to the spec itself don't reach this far.

- **`Persistable::describe_local(builder) -> TypeDescriptor`** and
  **`Persistable::describe(builder) -> TypeRef`** in
  [`kladde-traits/src/lib.rs`](crates/kladde-traits/src/lib.rs) — a **two-layer
  hook**. The common case implements only `describe_local`, returning its *own*
  descriptor node and referencing field types via `<FieldTy>::describe`; the
  provided `describe` default registers that node under `Self`'s `TypeId`
  (`builder.describe::<Self>()`). A type wanting to be *schema-transparent* (reuse
  another type's descriptor) instead overrides `describe` and leaves
  `describe_local` as its panicking default. Convenience methods `schema()` (drive
  a fresh builder) and `fingerprint()` (fingerprint the built table's root) sit on
  the same trait.
- **`SchemaBuilder`** in
  [`kladde-traits/src/schema.rs`](crates/kladde-traits/src/schema.rs) — the
  **runtime analog of the gray/black DFS**, with a matching two-layer API.
  `describe::<T>()` is the ergonomic entry point (key by `TypeId::of::<T>()`, build
  from `T::describe_local`); it's sugar over the primitive `describe_with(type_id,
  build)`, which reserves a slot (`push(None)`) and records the `TypeId → TypeRef`
  mapping *before* calling `build`, so a recursive type's nested `describe` call
  finds its own reserved index instead of looping. Dedups shared types by `TypeId`.
  `finish(root)` asserts the root landed at index 0 and unwraps every reserved slot.
  A runtime builder is used rather than a `const` because const-eval can't yet
  express cyclic, `TypeId`-keyed graphs.
- **`kladde-derive`** [`src/lib.rs`](crates/kladde-derive/src/lib.rs) — generates
  `describe_local` bodies: structs → a `Struct` descriptor whose fields recurse via
  `<FieldTy as Persistable>::describe`; enums → an `Enum` descriptor where
  `discriminant_value` follows Rust's own discriminant rule (explicit value, else
  predecessor + 1), emitted as a `const` so it *coincides with the real Rust
  discriminant*.
- **Hand impls** — scalars → `Primitive` (in
  [`kladde-traits/src/scalar.rs`](crates/kladde-traits/src/scalar.rs)); the
  built-in containers `PersistedVec` / `PersistedHashMap` / `Persisted` /
  `PersistedString` → `Opaque` (nominal, `inline_size: 8`) in `kladde-types`.
- **Example / eyeballing tool**:
  [`kladde-types/examples/schema_dump.rs`](crates/kladde-types/examples/schema_dump.rs)
  dumps a type's descriptor table + fingerprint (`cargo run -p kladde-types
  --example schema_dump`). Good for reading off golden vectors.

## "I want to change X — where do I go?"

| Change | Where |
| --- | --- |
| Add / change a **primitive code** | spec §2.1 table; `descriptor.rs` (just a `u8`, no code change needed); a `Primitive` hand-impl in `scalar.rs` if a Rust type should map to it |
| Implement a **reserved kind** (Array/Pointer) | add an enum variant in `descriptor.rs`, wire `references()`; encode/decode in `serialize.rs` (currently returns `ReservedKind`); hash input in `fingerprint.rs::encode_local` |
| Change **canonical ordering** | `descriptor.rs`: `references()` + `canonical_variants()` (both paths follow them) |
| Change the **on-disk byte format** | `serialize.rs` (and update golden-vector bytes) |
| Change **what's fingerprinted** or the **token encoding** | `fingerprint.rs`: `encode_local` (per-node content) / `emit_reference` (reference tokens) / `visit` (hashing) — then re-bless the golden vectors |
| Swap the **hash function / width** | `sha256.rs` + the 16-byte truncation in `fingerprint.rs::visit` (a format-version change) |
| Change **Rust → model mapping** (new derive behavior, container modeling, discriminant sourcing) | layer 2: `kladde-derive`, the hand impls, `SchemaBuilder` |

## Design invariants worth keeping in mind

- **Two code paths, one ordering.** Serialization and fingerprinting are separate
  implementations that must stay byte-compatible on ordering; they share
  `references()` / `canonical_variants()` precisely so they can't drift.
- **Indices are serialization-only.** The fingerprint never sees table indices —
  it encodes references structurally (inline child hash or de Bruijn back-ref).
  The `index_invariant` test guards this.
- **A fingerprint identifies a type only when rooted at that type.** A root
  fingerprint is a canonical, layout-invariant identity — schema-evolution
  detection compares these. A type's fingerprint *as it appears nested inside
  another traversal* is entry-relative, and even non-injective (a de Bruijn
  back-edge records only how far up an ancestor sits, not which one), so it must
  never be lifted out and reused; recompute rooted at the type instead (spec §4.6).
- **Golden vectors are the tripwire.** Any accidental change to encoding or hashing
  trips `golden_vectors` in `fingerprint.rs`; deliberate format changes mean
  re-blessing those constants on purpose.
