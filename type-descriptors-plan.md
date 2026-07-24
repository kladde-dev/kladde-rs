# Implementation Plan: Type Descriptors and Schema Fingerprints

## Goal and scope

Build the machinery to **describe** each `Persistable` type's representation, to
**serialize** those descriptors, and to compute their **fingerprints**, exactly as
specified in [`type-descriptors.md`](type-descriptors.md).

This is the foundation the broader schema-evolution work builds on; it deliberately
stops short of *resolution*, *migration*, and *detection at open* — those are later
milestones. Concretely, this milestone produces `T::schema()` and `T::fingerprint()`
and nothing yet reads or writes a schema into a real file.

## Design anchors

From [`schema-evolution.md`](schema-evolution.md):

- **Descriptor model** ≈ the `Type` enum of *Idea 1*, with the agreed refinements:
  `Manual` → **`Opaque`** and "descriptor = representation, not Rust type" (see the
  *Conclusions* → "Design decisions to lock in now"); field/variant identity **by
  name** (*Idea 4*); and Opaque folds only the **major** version component (the
  *Idea 2* note). The spec's §2 is the frozen form of this model.
- **Fingerprint** = the memoized white/gray/black DFS with de Bruijn back-references
  and a **cyclic flag** — the scheme settled in the *Idea 2* discussion, specified
  in [`type-descriptors.md`](type-descriptors.md) §4. Its intended consumers are the
  per-capsule schema table and the detection fast-path described under *"Capsules
  and segments: two orthogonal boundaries"* and *Idea 6*.

## Crate layout

- **New crate `kladde-schema`** — the `TypeDescriptor` model, canonical
  (de)serialization (§3), and the fingerprint algorithm (§4). Pure and
  self-contained (no dependency on the rest of the workspace), so it is unit-testable
  in isolation and could later back a cross-language conformance suite.
- **`kladde-traits`** gains a describe-hook on `Persistable` (depends on
  `kladde-schema`).
- **`kladde-derive`** generates that hook for `#[derive(Persistable)]` structs and
  enums.
- Hand-written `Persistable` impls provide it manually: scalars (in `kladde-traits`);
  `PersistedVec`, `PersistedHashMap`, `PersistedString`, `Persisted` (in
  `kladde-types`).

## Phases

Each phase is one or more commits; every commit passes `cargo build`,
`cargo test --workspace`, `cargo clippy --workspace --all-targets`, `cargo fmt`, and
`cargo doc` per `general-instructions.md`.

### Phase 1 — `kladde-schema`: model + serialization + fingerprint (standalone)

Build the whole spec against hand-constructed descriptor graphs, with no dependency
on `Persistable` yet.

- `TypeDescriptor` (Primitive / Struct / Enum / Opaque) and a `TypeTable`
  (the descriptor `Vec` + the root index), mirroring spec §2.
- Canonical storage (de)serialization, spec §3 (`varint`, `string`, per-kind
  encoding, table framing). Round-trip tests.
- `Fingerprint` (32 bytes; top bit = cyclic flag; §4.1) and the fingerprint
  algorithm, spec §4: white/gray/black DFS, memo of `(fingerprint, cyclic)`,
  reference tokens (inline child fingerprint vs. de Bruijn back-reference), cyclic
  flag propagation, SHA-256 with the top-bit-cleared packing.
- Tests:
  - **reproducibility** — same graph hashed twice (fresh state) → identical;
  - **index-invariance** — renumber/reorder the descriptor table → same fingerprint;
  - **structural sensitivity** — reorder a struct's fields → different; add a field →
    different; change a primitive code → different; rename a *field* → different;
    rename a *struct/enum type* → **same** (name excluded, §4.3);
  - **Opaque version folding** — patch/minor bump → same; major bump → different;
  - **recursion** — a linked list, a tree, and mutual recursion each fingerprint
    stably, with the **cyclic flag set**; every acyclic graph has the flag **clear**;
  - **flag-propagation soundness** — the two-referrer cycle (a non-cyclic parent that
    reuses a memoized cyclic child) has its flag set;
  - **golden vectors** — a handful of fixed descriptor tables mapped to fixed
    hexadecimal fingerprints, committed to lock the encoding and seed a future
    cross-language conformance suite (spec §6).

### Phase 2 — `Persistable` describe-hook + derive + hand impls

Connect real types to Phase 1.

- Add to `Persistable` (in `kladde-traits`) a method roughly:
  `fn describe(builder: &mut SchemaBuilder) -> TypeRef;`
  `SchemaBuilder` accumulates a `TypeTable`, **dedups by `TypeId`**, and **handles
  cycles** by registering a placeholder index for a type *before* recursing into its
  fields, so a recursive type resolves to its own already-registered index instead of
  looping. (This is the build-time analog of the gray/black marking in spec §4.4.)
  - *Rationale for a runtime builder rather than a `const`:* the descriptor graph has
    cycles and cross-type sharing; `TypeId`-keyed dedup + placeholder-before-recurse
    expresses both, which current const-eval cannot. A build-script or `const`
    precomputation of per-type fingerprints is a possible future optimization — note
    it, don't do it now.
- `kladde-derive`: generate `describe` for structs (a `Struct` descriptor; each field
  recurses via `<FieldTy as Persistable>::describe`) and enums (an `Enum` descriptor;
  each variant's payload recurses; `discriminant_value` sourced from the same
  mechanism the derive already uses — declaration order for now, but routed through
  an explicit id so *Idea 4*'s stable ids can replace it without touching this code).
- Hand impls:
  - scalars → `Primitive` (matching the codes in spec §2.1);
  - `PersistedVec<T>` → `Opaque { crate: "kladde-types", name: "PersistedVec",
    inline_size: 8, parameters: [describe::<T>()] }`; likewise `PersistedHashMap<K,V>`
    (params `[K, V]`), `Persisted<T>` (param `[T]`), `PersistedString` (no params) —
    all `inline_size: 8` (their `{target, len}` header). *Decision:* model the
    built-in containers as **Opaque** (nominal) for now; a *structural* descriptor for
    them (so external tools could walk their layout) is a later, separate concern.
  - `String` stays without a `Persistable` impl (unchanged).
- Convenience: `T::schema() -> TypeTable` (run `describe` from a fresh builder) and
  `T::fingerprint() -> Fingerprint` (fingerprint the built table's root).
- Tests: end-to-end fingerprints of the example/`full_stack` types; a recursive
  `Persistable` type (e.g. a tree of `PersistedVec<Self>`) fingerprints stably with
  the cyclic flag set; field rename/reorder change the fingerprint while a type
  rename does not.

### Phase 3 — thin integration surface

- Re-export `schema()`/`fingerprint()` (and the `kladde-schema` types needed to use
  them) through `kladde-types`/`kladde`.
- Optional: a tiny example that dumps a type's descriptor table and fingerprint, for
  debugging and for eyeballing golden vectors.
- **Do not** yet write the schema into files, compare fingerprints at open, or gate
  any read path — that is the detection/resolution milestone, deferred to the
  schema-evolution roadmap (*Idea 3* → *Idea 6*).

## Open decisions to confirm while coding

- **Opaque vs. structural for the built-in containers.** Opaque (nominal) is simplest
  and honest ("not a plain field sum"); a structural descriptor would let them dedup
  across crates and be walkable by generic tooling, but needs its own layout spec.
  Recommend **Opaque now**, revisit if/when a generic file-analysis tool needs to walk
  container internals.
- **Discriminant id source.** For this milestone, declaration order is an acceptable
  `discriminant_value` source; route it through an explicit id field so *Idea 4*'s
  stable ids can replace it later without changing the descriptor encoding.
- **Hash choice.** SHA-256 per spec §4.2 — ubiquitous and reproducible across
  languages. Revisit only if a future const/build-time fingerprint path wants a
  const-friendly hash; that would be a format-version change.

## Verification checklist (whole milestone)

- Golden vectors committed (byte input → hex fingerprint), guarding against accidental
  encoding drift and seeding cross-language conformance.
- Property test: fingerprint invariant under descriptor-table renumbering/reordering.
- Determinism: any type hashed twice from fresh state yields an identical fingerprint.
- Cyclic-flag correctness on the recursion and two-referrer cases above.
