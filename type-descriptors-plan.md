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
  name** (*Idea 4*); and Opaque folds `version` to its **compatibility component** —
  the stability flag plus the leading nonzero component (spec §2.4, refining the
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
- `Fingerprint` (16 bytes; top bit = cyclic flag; §4.1) and the fingerprint
  algorithm, spec §4: white/gray/black DFS, memo of `(fingerprint, cyclic)`,
  reference tokens (inline child fingerprint vs. de Bruijn back-reference), the
  lowlink-based cyclic flag (§4.6 — each call returns a min-depth; flag set iff
  `m ≤ depth`; memo hits contribute nothing), SHA-256-truncated-to-128-bits with the
  top-bit-cleared packing.
- **Compile-time fingerprints (bonus, not a requirement).** The goal is to bake the
  root type's fingerprint into the binary so the load fast-path just compares it
  against the file's stored fingerprint. Keep the fingerprint logic **separable from
  the index table** — the table is only a *serialization* device (needed to write
  cyclic graphs to disk as a flat array); fingerprinting is a pure recursive
  traversal `fp(node, gray_stack) -> (hash, cyclic)` that needs no global table. Three
  routes, in increasing order of what they unlock and cost:
  - **Runtime, computed once (do this — meets the goal).** A `LazyLock<Fingerprint>`
    (or compute-on-open) gives the binary's root fingerprint with no `const` anything
    and works for *all* types, recursion included. It is a few SHA-256s over a tiny
    graph, dwarfed by the file I/O — the compile-time win over this is negligible for
    the fast-path use case.
  - **`const FINGERPRINT` for acyclic types (feasible on stable, optional).** For an
    acyclic type the spec's DFS collapses to a bottom-up fold
    `FP = hash(kind, names, [child FPs…])`, which maps onto an associated const:
    `const FINGERPRINT: Fingerprint = fp_struct(b"…", &[<FieldTy as Describe>::FINGERPRINT, …])`.
    Referencing an associated *const* of a generic parameter is stable (unlike calling
    a trait *method* in const), and the hasher is a free `const fn` (a const SHA-256 is
    easy to write/vendor — one reason §4.2 pins SHA-256). No table, no const traits,
    bit-identical to the runtime path on acyclic graphs. It cannot be emitted
    unconditionally (a recursive type's `const FINGERPRINT` is a const-eval cycle
    error), so it must be opt-in / acyclic-only.
  - **Full `const` incl. recursive types (needs nightly).** A recursive fingerprint is
    context-dependent (§4.7), so it can't be a bottom-up const fold; it needs the
    stateful DFS run from the root. A `const fn` DFS over a flat `[TypeDescriptor; N]`
    handles cycles via indices with fixed-size memo/stack and *no* trait dispatch —
    but *generating* that array from a generic `T` needs `const_trait_impl` (to recurse
    across field types) and const heap / const-generic sizing. Both unstable; revisit
    when they land. (This — not the index representation — is the real blocker.)
- Tests:
  - **reproducibility** — same graph hashed twice (fresh state) → identical;
  - **index-invariance** — renumber/reorder the descriptor table → same fingerprint;
  - **structural sensitivity** — reorder a struct's fields → different; add a field →
    different; change a primitive code → different; rename a *field* → different;
    rename a *struct/enum type* → **same** (name excluded, §4.3);
  - **Opaque version folding** — above `0.x`: patch/minor bump → same, major bump →
    different; within `0.x`: patch bump → same, minor bump → **different** (minor is
    the leading component); and crossing `0.y → 1.0` → different (the stability flag,
    §2.4);
  - **recursion** — a linked list, a tree, and mutual recursion each fingerprint
    stably, with the **cyclic flag set**; every acyclic graph has the flag **clear**;
  - **flag precision (lowlink)** — a type that merely *contains* a cycle but is not
    itself on one (example (d) in spec §5: `C → A`, `A ↔ B`) has its flag **clear**;
    the same holds when the cyclic child is reached via a memo hit (a non-cyclic
    parent referencing an already-black cyclic node); only types on a cycle are
    flagged, and a DAG whose two referrers differ in cyclicity flags them
    independently;
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
  each variant's payload recurses; `discriminant_value` = the variant's explicit Rust
  discriminant if present, else declaration order — the same number the inline
  `store`/`load` uses, routed through one id step; see the *Discriminant id source*
  decision).
- Hand impls:
  - scalars → `Primitive` (matching the codes in spec §2.1);
  - `PersistedVec<T>` → `Opaque { library_name: "kladde-types", type_name:
    "PersistedVec", inline_size: 8, parameters: [describe::<T>()] }`; likewise
    `PersistedHashMap<K,V>`
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
- **Discriminant id source.** `discriminant_value` must equal the value the enum
  actually stores, so `describe` and the inline-layout `store`/`load` codegen take it
  from one source, per variant: the **explicit Rust discriminant** if the author wrote
  one (`enum E { A = 42, B = 137 }`), else **declaration order** (`0, 1, 2, …`, which
  is what Rust assigns anyway). Honor explicit discriminants — an author who pins one
  expects it to stay put, and doing so *is* *Idea 4*'s stable-id mechanism expressed in
  native syntax, likely obviating a custom `#[kladde(id = N)]` attribute. (Its only
  remaining niche: pinning a stable id on a *data-carrying* variant without the
  primitive `#[repr(...)]` that explicit discriminants on fieldful enums require since
  Rust 1.66 — leave the door open, don't build it now.) Route every variant's number
  through a single "id" step so the source can change without touching serialization
  or the descriptor encoding (§3.2 always stores `varint(discriminant_value)`).
  Uniqueness is already compiler-enforced (Rust rejects duplicate/colliding
  discriminants); lean on that by emitting each variant's discriminant as a `const`
  expression — the explicit expr, or `prev + 1` for implicit ones — and letting the
  compiler evaluate and reject collisions, which also handles non-literal exprs
  (`A = 1 << 4`) the macro can't compute itself.
- **Hash choice.** SHA-256 truncated to 128 bits per spec §4.2 — chosen for exact
  cross-language reproducibility (ubiquitous stdlib support, universal test vectors)
  with a 16-byte fingerprint; cryptographic strength is not relied upon (spec §4.9).
  Revisit only if fingerprints ever become trusted cross-party content-addresses
  (widen back to a full digest) or a const/build-time path wants a const-friendly
  hash; either is a format-version change.

## Verification checklist (whole milestone)

- Golden vectors committed (byte input → hex fingerprint), guarding against accidental
  encoding drift and seeding cross-language conformance.
- Property test: fingerprint invariant under descriptor-table renumbering/reordering.
- Determinism: any type hashed twice from fresh state yields an identical fingerprint.
- Cyclic-flag correctness: set for types on a cycle, clear for a type that only
  *contains* one (the lowlink precision case, incl. cyclic children reached via memo
  hits).
