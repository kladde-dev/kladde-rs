# Schema Evolution

I'm currently deliberating the following ideas for schema evolution. Help me understand the requirements, typical solution strategies, trade-offs of possible solutions.

## Goals and typical approaches

*(Claude:)* "Schema" here means the **on-disk representation contract** of a type: for Kladde
specifically, its `INLINE_SIZE`, the order/offsets/types of a struct's fields, an enum's
variant set and discriminant assignment, and how "owning" types lay out their
`{target, len}` header + out-of-line content. The problem is that a `kladde` file outlives
any single build of the code that reads and writes it, but the current format is **positional
and silent**: field offsets are a compile-time running sum of `INLINE_SIZE`s and enum
discriminants are declaration order, so adding/reordering a field or reordering a variant
changes the byte layout with *no marker at all*. Today the failure mode isn't "clean error,"
it's "read garbage" — so even the weakest useful guarantee (notice and refuse) is currently absent.

### The concerns, disentangled

The umbrella term is **schema evolution**; underneath it are several distinct concerns that
the field names separately:

- **Compatibility direction.**
  - *Backward compatibility* — **new** code reads **old** files. (The dominant real need; an
    app must open files it wrote in prior versions.)
  - *Forward compatibility* — **old** code reads **new** files, at least the parts it
    understands. (Matters only when multiple app versions share files, or third parties read them.)
  - *Full compatibility* — both.

- **Level of support** (your intuition of a spectrum is exactly right; the standard rungs are):
  - **(a) Detection / fail-closed.** Recognize an incompatible file and refuse cleanly instead
    of misreading bytes. This is a *safety* property and the non-negotiable floor. Kladde
    doesn't have it yet.
  - **(b) Migration / upgrade-on-open.** Recognize the old schema and run a conversion
    (old → new) at open, usually rewriting in the new format. One-directional; the canonical
    model is database migrations / `ALTER TABLE`. This is the natural fit for Kladde (see below).
  - **(c) Tolerant read / schema resolution.** Read old (or new) data directly — defaulting
    fields the reader has but the writer lacked, ignoring fields the writer had but the reader
    lacks, promoting compatible types — *without* a hand-written migration per change. Avro's
    reader/writer *schema resolution*; serde's `#[serde(default)]`/`alias`. Typically does **not**
    preserve data the reader doesn't understand.
  - **(d) Round-trip preservation ("don't clobber what you don't understand").** Old code reads
    a newer file, **preserves** the unknown parts, and writes them back so the newer app still
    sees them. Protobuf's *unknown-field preservation*, Cap'n Proto. The strongest and hardest,
    and the most in tension with a fixed-offset format. Your phrase *"load an old schema … and
    write it back in the same old format so an old app can read it again"* is a specific corner
    of this: it's really about **not force-upgrading a shared file** — a format-pinning concern,
    which for Kladde is more about *policy* (do we rewrite on flush?) than encoding (see
    "rewrite-on-flush," below).

- **The atoms of change** (any scheme should classify each as compatible / migratable / breaking):
  add field, remove field, rename field, reorder fields, resize/retype a field, add enum
  variant, remove variant, reorder variants, change a variant's payload, wrap/unwrap
  (`T` → `Option<T>` / `PersistableBlob<T>`). Reorder-field and reorder-variant *should* be no-ops but
  today are silently breaking — that alone argues for decoupling identity from position.

- **Who owns the schema** (this changes everything):
  - *Single authoritative app* (a settings file, a save file): only that app and its versions
    touch it. Simplest — the app can carry migrations for every version it ever wrote. This is
    the common embedded-persistence case and probably Kladde's 80%.
  - *Multiple apps / forks / third parties*: no single migration authority; you need
    decentralized identity (structural or explicit stable IDs). This is where the "combinatorial
    hash explosion" you worry about in Idea 2 lives.
  - *Nested library types*: a foreign crate's `Persistable` type is embedded deep in the tree
    and the crate releases a new version; only that crate's author can decide what's compatible.

### How the established systems solve it (and which map to Kladde)

- **Protocol Buffers / Thrift** — every field has a stable integer **tag**; the wire format is a
  sequence of `(tag, wire-type, value)`. Unknown tags are skippable (the wire-type gives length)
  and preserved. Superb backward+forward+preservation compat. Cost: per-field tag overhead on
  *every instance*, and it's a **sequential-scan** format — no fixed offsets, no random access.
  *Poor fit for Kladde's fixed-offset, per-type-metadata goals.*
- **Apache Avro** — data is written **without inline tags** (compact, positional), but the
  **writer's schema travels with the data** (inline or by fingerprint into a registry). Reading
  needs *both* writer and reader schema and performs **schema resolution** (match by name, apply
  reader defaults, drop reader-unknown fields, promote types). This is essentially your Idea 1.
  Avro's schema **fingerprint** (Rabin/CRC-64 of the *Parsing Canonical Form*) is essentially
  your Idea 2. *Strong fit* — see the key observation below.
- **Cap'n Proto / FlatBuffers** — the *only* mainstream **fixed-offset, random-access** formats,
  so the closest structural analogs to Kladde. They evolve under strict rules: fields have stable
  **ordinals**; new fields may only be **appended** (never inserted/reordered/resized); absent
  fields read as **default**; a stored struct size / **vtable** tells the reader which fields the
  writer actually wrote. FlatBuffers' per-object **vtable** (field-id → offset, shared across
  identical objects) buys add/remove/reorder while keeping random access — at one indirection per
  access. *The most relevant precedents.*
- **serde** — schema-on-read at the *type* level: `#[serde(default, alias, rename, other, skip)]`.
  No built-in schema/version — the *code* is the schema, and evolvability is whatever the format
  affords (JSON tolerant; **bincode/postcard positional and fragile — same failure as Kladde
  today**).
- **SQL / ORMs** — explicit, ordered, imperative **migrations** (up/down). Schema-on-write; the DB
  is the authority. The model for level (b).
- **Content-addressed / Merkle (Git, IPLD, Unison)** — identify a definition by the **hash of its
  content (+ dependencies)**; names become metadata (free renames), identity is exactly
  structure. This is your Idea 2 taken to its conclusion. **Unison** is the sharpest reference:
  it hashes definition ASTs, and handles **mutual recursion** by hashing a whole cycle as one
  component (directly answering your DAG question below). The cost it illustrates: any change to a
  dependency changes every dependent's hash — manageable only with real tooling.

**The single most important observation for Kladde.** Unlike Cap'n Proto/FlatBuffers, Kladde does
**not** read in place from the file forever. `load` reconstructs the whole structure into fresh
`Vec`/`HashMap`/… once per open, and steady-state reads/writes then hit the *in-memory* copy plus
whatever on-disk layout each allocation is operating in. The expensive part — reconciling the
writer's schema against the reader's — is therefore a **load-time-only, once-per-open cost**, never
per-read or per-mutation. This means Kladde can adopt Avro-style writer-schema resolution *without*
giving up its fixed-offset fast path: reconcile the foreign schema once at load, then operate
either in native layout or in the resolved writer layout (the latter is just a runtime offset
lookup, never re-resolution). It weakens Idea 1's "checks are expensive" worry (amortized over a
whole session) and reframes the whole problem as *"reconstruct a native in-memory value from a
possibly-foreign on-disk schema,"* which is exactly Avro's job. **Caveat, addressed next:** *which*
layout you then operate in — native vs. the writer's — is a real policy choice, not a given.

### Layout policy: upgrade-on-open vs. retain the writer's layout

The observation above quietly assumed every type *upgrades* to its native layout on open. That's
one policy, not a law — and it shouldn't be forced. Make **layout policy a per-type (really
per-allocation) choice** between:

- **Upgrade.** Read via the writer's layout, reconstruct native, and thereafter operate — and
  rewrite on flush — in the native layout. Simple, and keeps the compile-time-constant-offset fast
  path for live mutation; but (i) it forces a rewrite of every touched allocation, and (ii) once
  flushed, an *older* app can no longer read those allocations.
- **Retain.** Remember the writer's layout and keep operating *in it* for the session: reads and
  in-place writes use the writer's offsets/sizes, and flush writes back in the writer's layout.
  This buys two things worth wanting — **backward-compatible writes** (an older app can still read
  the file after a newer one has modified it) and **no rewrite on open** (touch nothing you didn't
  have to) — at the cost of carrying the writer's layout as *runtime* data and giving up the
  constant-offset fast path for that allocation.

Neither is universally right, so the choice belongs to the type (or a per-open flag), and can even
be made per allocation by inspecting the actual difference.

**What retain can and cannot absorb.** Retain works precisely for changes whose reader state still
*fits* the writer's layout: removed fields (become ignored dead space), reordered/renamed fields
(remapped, not moved), fields the reader recomputes rather than stores (your `cached_result`
case), and in-place-convertible width changes. It **cannot** absorb a *new persisted field* —
there is nowhere in the old layout to put it — so adding persisted state forces that allocation to
upgrade (migrate). Retain-mode is therefore exactly the "we only removed/reordered/renamed/
reinterpreted, we didn't add persisted state" subset of forward compatibility, obtained cheaply
and without any preservation/overflow area.

**Composition — the pointer boundary is the unit of independent layout.** This granularity only
composes because of Kladde's out-of-line owning types. An owning type occupies a fixed 8-byte
`{target, len}` header in its parent's allocation *regardless* of its content's layout, so a child
allocation can retain or upgrade **independently** of its parent — the parent's slot is 8 bytes
either way. **Inline** data (scalars, and derived structs/enums laid inline in their parent's
allocation) has no such isolation: its bytes are part of the parent allocation's layout, so it
shares whatever policy that allocation is in. Hence the natural unit is **per allocation** (per
owning-type instance): every inline value inside an allocation shares its layout, and only at a
pointer hop can the choice change. (This also exposes a cost of the current "derived structs are
inline, own no allocation" decision: two structurally-different inline structs can't evolve their
layout independently — only boxed/owning aggregates can. If independent per-struct evolution ever
matters, giving a struct its own allocation is the lever — at the price of an indirection each,
i.e. drifting toward a classic object-table design.)

**Consequences to design for:**

- **Offsets stop being compile-time constants in retain-mode.** `INLINE_SIZE` and the derived
  static offsets are inherently a *native-layout* notion. A retained allocation must compute
  offsets and element strides from a **runtime layout descriptor** (the writer's, recovered from
  the stored schema). So the mutation path (`Guard`/`store`/`load`/`Location`) must run in two
  modes: the fast const-offset path when native, a layout-driven path when retained — the derive
  macro generating both. This is a genuine complexity cost, borne only by types that opt into
  retain, and it is in effect a **scoped, opt-in slice of the vtable machinery** dismissed under
  Idea 5 (see the revision there).
- **The file becomes layout-heterogeneous, which wants a per-allocation schema tag.** If some
  allocations are upgraded and others retained (or were written by different past versions), each
  must be interpretable by *its own* writer layout — so a small **per-allocation schema id** (an
  index into the file's schema table), not one global root schema. That in turn enables **lazy,
  incremental migration**: upgrade an allocation only when you choose to, never a big-bang rewrite
  on open. This tag is close to the "type registry" `spec.md` currently defers — likely the same
  mechanism. Cost is a few bytes per *allocation* (not per value — inline data inherits its
  allocation's tag), a far gentler tax than protobuf-style per-value tags. In fact the natural home
  for this tag is coarser than the allocation — a **capsule** (a schema/version boundary), backed by
  a resident deduplicated schema table, and orthogonal to the lazy-load boundary (a **segment**);
  see "Capsules and segments: two orthogonal boundaries" below.

## Current ideas for schema evolution

Complete the lists of advantages and disadvantages of each idea below and add any other relevant comments or questions inline. Also try to come up with better ideas than the ones I wrote up in this document.

### Idea 1: fully self-describing schemas

A `kladde` file stores an array of serialized type descriptions at a pre-defined memory address. Each type description is essentially a binary serialized representation of something similar to the following:

```rust
enum Type {
    Primitive(PrimitiveType),
    Struct{ name: String, fields: Vec<(String, Index)> },
    Enum{ name: String, variants: Vec<EnumVariant> }, // with `enum EnumVariant { Tuple{name: ..., fields: ...}, Struct{...} }`
    Manual{ name: String, crate_name: String, version: (u32, u32, u32), type_parameters: Vec<Index> },
}
```

Here, `Index` is an integer index into the array of serialized type description. The first type in this array has to be the root type stored in the `kladde` file.

When opening a file, kladde first parses the types list into a `Vec<Type>`. It then constructs the data, which calls `Persistable::load` for the root type and all its nested types as already implemented, except that `Persistable::load` gets a new argument of type `&Type` and is made fallible.

**Advantages:**

- Fully self describing and even human readable (debuggable).
- Type-level backward compatibility: if somewhere deep in the nested stored data structure a type from a library is used and that library releases a new verison, the library may choose to be backward-compatible, i.e., still support the old type as well. Since `load` gets the `&Type` argument, it can check against a list of types it chooses to support.
- Little runtime overhead for storing because type descriptions can probably a `const` trait item of `Persistable` (check if the derive macro can create them at compile time).
- Relatively space efficient: we store type information only once _per type_, not per instance (this is different to, e.g., JSON). But for small files or for settings files that basically contain a large nested struct with no repeated data types, the impact on file size might still be substantial.
- *(Claude:)* **Per-type metadata is the right cost model for a data-structure library.** With millions of instances of a type, protobuf-style per-instance tags or FlatBuffers-style per-instance vtables would be a large steady-state tax; a once-per-file schema blob is negligible by comparison. This is Idea 1's strongest structural advantage and worth stating explicitly.
- *(Claude:)* **It is the only one of your ideas that carries enough information to actually *resolve* differences,** not merely detect them. A bare hash (Idea 2) can say "different"; the full descriptor can say "the writer had fields `a, c` where you have `a, b, c`, so read `a`/`c` from these offsets and default `b`." That is what unlocks levels (b) and (c). See the "most important observation" above: because resolution runs only at load, its generality is nearly free.
- *(Claude:)* **Generics are handled cleanly** — a generic instantiation is just a `Type` whose descriptor references its parameter types by `Index` (your `Manual{..., type_parameters}` already does this; `Struct`/`Enum` do it structurally). This is a real advantage over Idea 2, where a pure hash can't separate "the generic changed" from "its parameter changed."

**Disadvantages:**

- `load` should probably check the type description of every loaded instance of every type. These checks can be expensive because we're potentially comparing complex nested data structures with lots of strings, and we have to look up indices.
    - *(Claude, response:)* Two mitigations make this a non-issue in practice. (1) **Resolve per *type*, not per *instance*.** You reconcile the writer's `Type` graph against the reader's *once* at open, producing a small per-type "read plan" (for each reader field: read from writer-offset X, or use default); every instance then follows the cached plan with no string comparison. (2) **Fingerprint fast-path** (borrow Idea 2): also store a hash of the root schema; if it equals the current build's hash, the schemas are identical — skip resolution entirely and read at static offsets. So the common case (same version) costs one hash compare, and the rare case (evolved) costs one graph reconciliation per open. The per-instance string-comparison cost you're worried about never happens.
- I'm not sure if we can allow updates to type implementations to change the `INLINE_SIZE` because this would change the logic of types further up the hierarchy (i.e., if `ForeignType` from `foreign_crate` changes its `INLINE_SIZE` in a new version of `foreign_crate`, then a `PersistentVec<ForeignType>` has to change its logic). Unfortunately, we can't simply include the `INLINE_SIZE` in the type description because, for example for `Struct` types, the `INLINE_SIZE` depends on the `INLINE_SIZE` of the fields, which might depend on their versions.
    - *(Claude, response — this is the crux, and it's more tractable than it looks.)* You don't need to *store* `INLINE_SIZE` as an independent fact; you need `load` to compute the **writer's** offsets from the **writer's** schema graph, which the descriptor fully determines (a struct's inline size is the sum of its fields' inline sizes, recursively down to primitives and owning-type headers — all present in the graph). The reader then reads each field at the *writer's* offset and stores it into the *reader's* in-memory layout. So an `INLINE_SIZE` change deep in the tree is fine: the writer schema encodes the old sizes, the reader schema (compile-time consts) encodes the new ones, and resolution bridges them. The thing that makes this *work* is precisely the "most important observation": you interpret the foreign layout **only at load**, then rewrite native — you never try to do in-place fixed-offset mutation against a foreign `INLINE_SIZE`. **Corollary:** `Persistable::load` must be able to take *writer* offsets rather than assuming `Self::INLINE_SIZE`; that's a slightly bigger change than "add a `&Type` argument," and worth designing for now.
- For `Struct` and `Enum`, we're essentially implementing structural typing. Two struct types with the same name and same fields will serialize to the same description even if they're different rust types. For derived types, I'm not sure if this is an issue: we need the type description only to check if the data representation in the file matches what the program's `Persistable` implementation of the type does, and for derived types, that's the case if the type is structurally identical. Note that the serialized type description does not affect which rust type will be used to actually handle the data structure. That's handled as implemented now, via rust's normal type system.
    - *(Claude, response — agreed, and it's not a problem.)* Your reasoning is right: the descriptor certifies **representation**, not **identity**. Because reconstruction always proceeds *top-down from the known root type*, you never ask "what Rust type is this anonymous blob?" — the parent field's declared type already tells you which `load` to call; the descriptor is only there to check "does the writer's byte layout for this field match what my `load` expects, and if not, how do I bridge it?" Two structurally-identical-but-semantically-different types (`{x:i32,y:i32}` as Point vs. as Complex) are indistinguishable, but that can only happen if one is substituted for the other at the *same position in the tree*, which is a type error the Rust side already prevents. So structural descriptors are exactly the right granularity.
- Names of `Manual` can clash. The `(crate_name, name)` identification is not versatile (assumes globally known crate names, ...).
    - *(Claude, response:)* Two sub-points. (i) `(crate_name, name, major_version)` is the same identity Cargo/semver already rely on; clashes are the same risk `cargo` already manages, so this is acceptable *if* you also allow a manual type to declare a **structural** descriptor for its content when it has one (e.g. `PersistableString` = "u32-length-prefixed UTF-8 blob"), and reserve opaque nominal identity only for genuinely opaque types (`PersistableBlob<T>` = "postcard blob of T"). Then structural types get structural compatibility (robust), and only truly-opaque types depend on the fragile `(crate, name, version)` tuple. (ii) For the opaque ones, compatibility is the *type's own decision*: `load` sees the writer's `Manual{version}` and chooses whether to support it — which is your second advantage bullet, and it's the correct place to put foreign-type-evolution policy (with the foreign author, not with Kladde).
- Some unclear cases: consider the type `struct Calculation { formula: PersistableString }` with a derived `Persistable` implementation. Its type description is `Type::Struct{ ... }`. Now assume that we want to cache the result of the calculation, but we don't want to store the result in the file (we'll redo the calculation when we open the file). Thus, we now have `struct Calculation { formula: PersistableString, cached_result: i32 }`. We don't want to derive `Persistable` for it because that would also persist the cached result. So we manually implement `Persistable` as if the field `cached_result` wasn't there. Should this type's description now be `Type::Manual` or should it be the same `Type::Struct` as we had before because the representaiton in the file remained unchanged?
    - *(Claude, response — it should remain `Type::Struct`.)* This case actually *confirms* the guiding principle: **the descriptor describes the representation, not the Rust type.** The manual impl writes exactly the bytes of `Struct{ formula: PersistableString }`, so its descriptor should be exactly that. A reader must be able to interoperate with anyone who writes that representation, regardless of how many non-persisted fields sit in the author's Rust struct. Concretely: the derive macro emits the `Struct{…}` descriptor automatically; a manual impl emits **whatever descriptor matches the bytes it actually reads/writes** (here, hand-written to match `Struct{ formula }`). `Manual` is therefore *not* "any hand-written impl" — it's specifically "a type whose representation is not expressible as a plain field-sum / variant-set" (owning types, opaque blobs). So the answer falls straight out of "descriptor = representation." *(This suggests renaming `Manual` to something like `Opaque` to avoid exactly this confusion — "manual impl" and "opaque representation" are different axes.)*

### Idea 2: merkelize Idea 1

Think of the list of serialized data types as a vertices of a graph, where any `Index` in the definition of `Type` above defines a vertex of the graph. Merkelize that graph: calculate a reproducible hash of each `Type` entry in the list. If a `Type` contains an `Index`, replace the `Index` with the hash of the referenced data structure.

**Main practicaly difference to Idea 1:** I think this moves all updates, regardless of how deep in the tree of nested data types, to the application layer. If we include some type from a foreign crate somewhere nested in our type, and we update that foreign crate to a version with a non-backward-compatible change of the type, then all upstream hashes including the hash of the root data type change. For `kladde` files that are supposed to be used only by a single authoritative app, this might work: when application authors prepare a new release, they can update all dependencies and record the new root hash. Then they keep a table of app versions and root hashes, and they implement support for all previously supported root hashes (Claude: how would one do this ergonomically?). However, when people start to fork the app, the space of possible root hashes grows probably combinatorically.

*(Claude, on "how would one support all previously-supported root hashes ergonomically?":)* You would **not** want a `match` over historical root hashes each dispatching a bespoke loader — that's unmaintainable and exactly the combinatorial trap. The ergonomic move is to **not throw away the schema.** Keep Idea 1's full descriptor in the file, use the hash *only* as a fast-path/cache key: on open, `if root_hash == THIS_BUILD.root_hash { fast native read } else { generic schema resolution against the stored descriptor, plus any registered semantic migration hooks }`. Then "supporting old versions" is *one* generic resolver (handles add/remove/reorder/rename/default structurally) plus a small set of explicit hooks for changes that are *semantic*, not just structural (e.g. "v3 splits `full_name` into `first`/`last`"). The hash count stops mattering because you never dispatch on it. This is the standard Avro answer: fingerprint = registry key, schema = source of truth.

**Note:** in this approach, the `version` field of `Type::Manual` should probably only contain the leading version number so that changes in the minor or patch version number are indeed recognized as backward compatible.

*(Claude:)* Agreed — hash only the semver **major** for `Manual` types, so patch/minor releases don't churn every upstream hash. Caveat: pre-1.0 (`0.y`) semver treats `y` as the breaking component, so the "leading nonzero" convention (à la Cargo) is the more correct rule. Deeper caveat: this *trusts the author to bump major on every representation change*, which a structural hash wouldn't need to trust — but `Manual`/opaque types are opaque by definition, so a version number is the pragmatic best you can do for them (another reason to prefer structural descriptors wherever possible, per the `Manual`-clash discussion in Idea 1).

**Advantages:**

- Even more space efficient. Hashes are short, and we probably only need to store the root hash.
- Changes in the `INLINE_SIZE` of a type are now allowed. If some leaf type of the system changes its `INLINE_SIZE`, its definition or the version of its crate must have changed somehow, so its hash changes. That change propagates up the merkelized data structure to the root hash, so everybody upstream knows that their memory layout might change.
    - *(Claude, response:)* True, but note the *same* is true of Idea 1 without hashing: a leaf `INLINE_SIZE` change changes the leaf descriptor, hence every ancestor descriptor, so it's equally *detectable* there. The hash's added value is only that detection collapses to a single **O(1) root compare** instead of a graph walk — a performance/compactness win, not a capability Idea 1 lacks. And detection is the *easy* half; the hash gives you nothing for *resolution* (you still need the descriptors to actually bridge the change), which is why the hash should complement, not replace, the schema.
- *(Claude:)* **Excellent tamper/corruption check and a natural cache key** — the root hash doubles as a cheap integrity check and as the fast-path discriminator described above. Keep it for those reasons even under a schema-based design.

**Disadvantages:**

- No cyclic dependencies allowed (i.e., no linked list) because only DAGs can be merkelized. Is this true? Is there a way out? Or do we have that limitation anyway somehow in the system?
    - *(Claude, response:)* It's true of *naïve* Merkle hashing but **not fundamental**, and you'll need to solve it regardless because recursive Kladde types are legitimate (a tree node holding `PersistableVec<Node>`, any linked structure). The standard techniques: (1) **hash the whole cycle / strongly-connected component as one unit** — compute the SCCs of the type graph, and hash each SCC together, with intra-SCC `Index`es replaced by a canonical marker (their position within the component). This is exactly how **Unison** hashes mutually-recursive definitions. (2) Equivalently, replace a back-edge with a **de Bruijn-style relative reference** ("the k-th enclosing type currently being hashed") so the hash is still a pure function of structure. Either way the recursion limitation dissolves; it just needs cycle-aware canonicalization rather than a plain post-order fold. (Note this is *also* needed for Idea 1's descriptor *equality* check to terminate on recursive types, so it's shared work, not extra.)
- Types that contain other (non-primitive) types can no longer detect whether a changed hash is due to their own update or an update of the contained types. Generic types can't use the hash at all.
    - *(Claude, response:)* Right, and this is the decisive limitation. A pure hash **conflates "I changed" with "my dependency changed,"** which is precisely the information a *tolerant/backward-compatible* reader needs to make a fine-grained decision ("my own layout is unchanged; only a nested opaque type bumped major — can I still read the rest?"). And generics are unusable as you say, because `Vec<Av1>` and `Vec<Av2>` hash differently with nothing shared. **Conclusion: a hash alone can implement level (a) detection but cannot implement (b)/(c).** It belongs *on top of* Idea 1, not instead of it.

### Other ideas

*(Claude:)* Four more options, roughly in increasing order of power and cost. The first is the
"ship-nothing-without-this" floor; the last is my actual recommendation.

#### Idea 3: Magic + format fingerprint (detection only, fail-closed) — the minimum viable step

Write a small header: magic bytes, a Kladde-format version, and a single **root schema
fingerprint** (Idea 2 degenerate to just the root hash). On open, if the fingerprint doesn't
match the current build's, **refuse with a clear error**. No resolution, no migration — just turn
today's silent misread into a loud, safe failure.

- **Advantages:** tiny (a few bytes/file, one const per build, one compare on open); ships in days;
  immediately removes the worst failure mode (corruption-on-misread); forward-compatible with
  every richer scheme below (they all want a fingerprint anyway).
- **Disadvantages:** zero evolution — *any* change locks users out of their files. Only acceptable
  as a **stepping stone** or for throwaway/cache files. But it is strictly better than the status
  quo and should land regardless of which full design wins.

#### Idea 4: Stable field/variant identity (decouple identity from position)

Independently of *how* schemas are stored, fix the root cause of "reorder silently breaks":
identify struct fields and enum variants by something **stable under reordering**. Two flavors:

- **By name** (Avro/serde style): the derive macro records field/variant *names*; resolution
  matches by name; `#[kladde(alias = "old_name")]` covers renames; `#[kladde(default)]` covers
  added fields; `#[kladde(unknown)]` designates a fallback variant for unknown enum tags (serde
  `#[serde(other)]`). Rust-idiomatic, low boilerplate, names already exist.
- **By explicit ordinal** (protobuf/Cap'n Proto style): `#[kladde(id = 3)]`. Robust to rename,
  survives even name changes, but un-Rusty boilerplate and easy to get wrong.

- **Advantages:** turns reorder-field, reorder-variant, and (with defaults) add-field into
  *non-breaking, non-migration* changes — the single highest-leverage fix, and it composes with
  any storage choice. Enum discriminants stop being declaration-order (which also fixes the
  latent "insert a variant → corrupt old files" landmine and pairs naturally with `later.md`'s
  small-int discriminant idea: assign a stable id, encode it in the smallest width).
- **Disadvantages:** name-based needs an alias discipline for renames; ordinal-based needs a
  "never reuse a retired id" discipline (like protobuf `reserved`). Either way it's *convention +
  tooling*, not a pure format guarantee.
- **Recommendation:** name-based, because Kladde's whole audience is `#[derive]` users who already
  have field names; reserve ordinals for a possible future opt-in on hot/large types.

#### Idea 5: Vtable / append-only layout (make the fixed-offset format itself evolvable)

Borrow FlatBuffers/Cap'n Proto directly: either (a) **append-only** — new fields may be added only
at the end, existing fields never move/resize, and each struct records the writer's field-count
(or `INLINE_SIZE`) so a reader defaults the missing tail and ignores the extra tail
(prefix-compatibility); or (b) **vtable** — each struct type carries a small
`field-id → offset` table so add/remove/reorder work with one indirection, absent fields default.

- **Advantages:** supports *in-place* reading of foreign layouts (never re-lay-out), and (b) even
  gives partial **forward compatibility + preservation** (level d) if you keep a per-instance
  overflow area for unknown fields. Battle-tested for exactly Kladde's fixed-offset shape.
- **Disadvantages:** gives up Kladde's cleanest property — *offset = compile-time constant sum* —
  for *offset = table lookup* (a real perf/complexity regression on the mutation hot path), or
  (append-only) accepts a genuinely restrictive change model.
- **Where Kladde actually needs a slice of this:** exactly the **retain-mode** allocations from
  "Layout policy" above — they must compute offsets from a runtime layout, which *is* a small,
  per-allocation, opt-in vtable. So adopt the *mechanism* narrowly (a runtime layout descriptor for
  retained allocations) but **decline the *global* vtable**: upgrade-mode allocations keep the
  constant-offset fast path, and the fingerprint fast-path (Idea 6) skips resolution entirely when
  writer and reader schemas match, so native/native files — the common case — pay nothing. Full
  always-on vtables would tax that common case to serve the rare one.

#### Idea 6 (recommended): writer-schema resolution at load + fingerprint fast-path + name identity

Synthesize the above into one design that fits Kladde's load-reconstruct-rewrite model:

1. **Store the full writer schema once per file** (Idea 1's structural descriptors; derive-macro
   `const`), with fields/variants identified **by name** (Idea 4), plus a **root fingerprint**
   (Idea 2) and a magic/version header (Idea 3).
2. **On open:** compare fingerprints. *Match* → fast path: read at static native offsets exactly as
   today (zero evolution cost for the common case). *Mismatch* → **schema resolution**: reconcile
   writer vs. reader descriptor into a per-type read plan (read shared fields at *writer* offsets,
   default reader-only fields, drop writer-only fields, promote compatible primitives), then run
   the existing top-down `load` driven by that plan. Genuinely *semantic* changes (field split,
   unit change) are handled by a small registry of explicit **migration hooks** keyed on
   (from-fingerprint → to), the DB-migration model — but only for changes structural resolution
   can't infer.
3. **Per-allocation layout policy on write** (see "Layout policy" above): each allocation either
   **upgrades** (rewrite native on flush; keeps the constant-offset fast path) or **retains** the
   writer's layout (backward-compatible writes, no rewrite, at the cost of runtime offsets).
   Adding a persisted field forces upgrade for that allocation; everything else may retain. Tag
   each allocation with its schema id so the file can stay layout-heterogeneous and migrate lazily,
   and expose the default (upgrade vs. retain) as a per-type and/or per-open choice.
4. **Opaque/`Manual` types** self-identify (`crate`, `name`, major) and decide their own
   compatibility in `load(&Type)`; structural types get structural compatibility for free.

This gives: (a) detection always; (b) migration via hooks; (c) tolerant structural read for the
common atoms (add/remove/reorder/rename field, add variant); per-**type**/per-**file** metadata
cost only; the fast path stays byte-identical to today; and it degrades gracefully to Idea 3 if you
ship the header first and add resolution later.

#### Capsules and segments: two orthogonal boundaries

Everything above spoke of a "per-allocation schema tag" and (in an earlier draft) bundled schema
separation together with the lazy-loading idea from `later.md` under one word ("chunks"). Those are
really **two orthogonal concerns**, and separating them is cleaner. Name them:

- A **capsule** is a *schema/version boundary*: a subtree that carries its own writer-schema and
  version, migrates independently, and picks its own upgrade/retain policy. It owns a header + a
  content region precisely so that region can carry its own schema tag (an inline value would share
  its parent allocation's tag). A capsule need not be lazy.
- A **segment** is a *load boundary* (`later.md`): a subtree in its own disjoint, contiguous file
  region, materialized on first access and evictable, so peak memory is bounded by the working set
  rather than by the whole file. A segment need not be a schema boundary.

They compose freely — a subtree can be a capsule, a segment, both, or neither:

| | ¬ schema boundary | **capsule** (schema boundary) |
|---|---|---|
| ¬ load boundary | plain inline/owning data | versions independently, always resident |
| **segment** (load boundary) | loaded lazily *for memory*, same version as its surroundings | loaded lazily *and* versions independently |

**Schema handling attaches to the capsule**, not to every allocation and not to every segment — the
capsule is the schema/migration unit:

- **Resident, deduplicated schema table**, interned by fingerprint (Idea 2's hash); each capsule
  holds a small **index** (or the hash) into it. Distinct schemas in use are far fewer than
  capsules, so this is far more compact than a schema per capsule; the full schemas live here
  (Idea 1) so a reader can resolve generically without having pre-enumerated that historical version.
- **Eager detection with minimal resident state.** The table lists *every* live schema, so at open
  you verify the current build supports all of them — and if so, *every* capsule is guaranteed
  readable whatever index it holds. You never need the capsule→schema mapping resident, only the
  schema *set*, so this restores the fail-fast-at-open that lazy loading would otherwise lose,
  without touching a single segment. (Resolution stays lazy per capsule — detect eagerly, migrate
  lazily.)
- **Resolve once per schema, not per capsule.** Compute each writer→reader read plan (and its
  native / needs-resolution / unreadable verdict) once at open; every capsule sharing that schema
  reuses it.
- **Reclaim entries by reference counting.** Otherwise the table grows monotonically as old,
  never-rewritten capsules keep old schema versions alive. Ref-count each entry by the number of
  capsules referencing it and drop it at zero (on capsule migrate/delete), so the table stays
  proportional to *schemas currently in use*, not to the file's whole version history. If the table
  is ever compacted, rewrite capsule indices in that pass, or reference schemas by hash (stable, a
  few bytes more) instead of by index.
- **Ordering invariant.** A schema must be durable in the table *before* any capsule references it —
  the same write-ahead discipline the journal already needs, and what makes eager detection sound
  (the table is authoritative and complete by construction, never re-derived by scanning).

**The one rule linking the two axes:** a **segment that is not also a capsule shares — and migrates
with — its enclosing capsule's schema.** So you cannot migrate a capsule's schema in place while one
of its unloaded segments still holds bytes in the old schema (the resident metadata would advertise
the new schema while that region is stale → corruption on eventual load). Two clean resolutions,
chosen per capsule: migrate the whole capsule as a unit (load its segments, rewrite), or keep the
capsule in **retain-mode** (never migrate in place; unloaded segments stay valid). To migrate a
lazily-loaded region *independently and incrementally*, make it a capsule too.

Net: the schema table is always-resident and a shared write point on schema introduction/migration,
but it holds *metadata* bounded by distinct schemas in use (GC'd by refcount) — trading "hold all
data at open" (which segments exist to avoid) for "hold all schema metadata at open" (small). This
table is very likely the same mechanism as the file-level semantic-versioning slot and "type
registry" `spec.md` defers — unify them.

#### Capsules cut the fingerprint graph — erasure, and why fingerprinting stays cheap

A capsule doesn't just *carry* its own schema; it **erases its inner type from every fingerprint
computed outside it.** When the enclosing type's schema is fingerprinted (`type-descriptors.md`
§4), the traversal treats a `Capsule<T>` as an opaque boundary and does **not** descend into `T`
— it emits "a capsule sits here" (the capsule's table id/hash), not `T`'s structure. So the
enclosing (**outer**) fingerprint is *independent of `T`'s internals*, and the two versioning axes
decouple cleanly:

- the **outer** structure — does `Root` still hold `PersistableVec<Capsule<_>>` in that shape? — is
  tracked by the outer fingerprint;
- each capsule's **inner** type is tracked by its own fingerprint in the schema table.

Change `T` deep inside and the outer fingerprint doesn't move — only `T`'s table entry does. Add a
field to `Root` and the outer fingerprint moves but the capsules' don't. This is what lets you
detect a changed capsule *locally*, by comparing table fingerprints, instead of noticing only that
some global root fingerprint changed and not knowing where.

Erasure is also what keeps fingerprinting **linear**. Under the de Bruijn scheme
(`type-descriptors.md` §4.7) a *nested* type's fingerprint is entry-relative — only a type
fingerprinted **as its own root** ("canonical") is a context-free, collision-free identity.
Producing a canonical fingerprint for *every* node would be superlinear (rooting a traversal at
each member of a strongly-connected component is quadratic in the SCC's size). But you never need
every node's canonical fingerprint — only the ones referenced *across* a boundary, i.e. the **entry
points**, and **capsule boundaries are exactly those entry points**, declared by the application.
So the rule is:

- fingerprint the **root** as one canonical traversal that *stops* at each capsule (emitting the
  capsule's table id/hash, not the inner type); and
- fingerprint **each capsule type** as its own canonical root, likewise stopping at any nested
  capsules (reusing their already-computed fingerprints).

Each is one linear traversal of the region *between* boundaries, so the whole file's schema
fingerprints in `O(total schema size)` whenever capsules partition it — no all-node SCC
canonicalization ever runs. (A cycle threaded *through* two capsule boundaries would place them in
one SCC and need de Bruijn between them, but nested capsules normally form a DAG.) Until capsules
exist, the fingerprint code stays whole-root (which is already canonical and sound for whole-type
comparison); per-boundary canonicalization is added only when `Capsule<T>` lands.

#### Worked example: detecting a changed capsule in `PersistableVec<Capsule<T>>`

Take `struct Root { items: PersistableVec<Capsule<T>> }` with `T = struct { a: i32, b:
PersistableString }`. On disk:

- the resident **schema table** holds, deduplicated, `(id → serialized descriptor + canonical
  fingerprint)` — e.g. `0 → FP_outer` (`Root` with the capsule erased), `1 → FP_T_v1` (`T` as
  written), and, *only if the file was partly migrated before*, `2 → FP_T_v2`;
- the vec holds N capsule *instances*; each stores a data pointer plus a small **`schema_id`**
  (`1` or `2`) in its header. A homogeneous, never-migrated file has every element → `1`; a
  partly-migrated file is mixed.

The running binary's code now has `T` = v3. **On open:**

1. Read the table's fingerprint array `[FP_outer, FP_T_v1, FP_T_v2]`.
2. Compute the build's *expected* fingerprints — only two canonical roots: `FP_outer'` (its `Root`
   shape) and `FP_T_v3` (its current `T`).
3. Compare, `O(table size)`, 16 bytes each: `FP_outer' == FP_outer` → the vec/capsule scaffolding
   reads natively; `FP_T_v3` matches neither `1` nor `2` → both are **stale** (stale set `{1, 2}`).
   **No descriptor table was parsed.**
4. If the stale set were empty → whole-file fast path, done. It isn't, so **which capsules?** Walk
   the vec reading only each element's `schema_id` (a cheap integer, no schema parse) and test
   membership in the stale set. All are stale here → all need migration; in a mixed file only the
   `→1`/`→2` elements flag, any `→3` reads natively.
5. **Resolve once per stale schema, not per instance** (the table rule above): parse `FP_T_v1` /
   `FP_T_v2` once each to build the old→v3 read/migration plan, then rewrite each stale capsule
   lazily (as its segment pages in), flipping its `schema_id → 3` and refcounting the old entries
   down so they can be reclaimed.

So the fast path is a scan of the table's fingerprint array; the only per-instance cost is an
integer read and a set test; and full-schema parsing happens once per distinct stale version, never
per element. That "iterate an array of fingerprint ids, don't compare full schemas" detection falls
straight out of (a) erasure decoupling the outer fingerprint from inner schemas and (b) the
deduplicated table keyed by canonical fingerprint.

#### Representing capsules and segments in code

Both are **wrapper types written directly at the field** — `Capsule<T>` and `Segment<T>`, the same
family as the existing `PersistableBlob<T>` — *not* derive attributes:

```rust
struct Document {
    body:  Segment<PersistableString>,   // load boundary: paged in on demand
    prefs: Capsule<Preferences>,       // schema boundary: versions independently
}
```

They must be wrapper types because each carries runtime state that has to live *in the value*.
`Segment<T>`'s whole point is that its inner `T` may be **non-resident** — its representation is
essentially `enum { OnDisk(handle), Loaded(T) }` plus a region pointer, and a bare `T` field is
*always* materialized, so laziness simply cannot be an annotation on a `T`-typed field. `Capsule<T>`
owns a header plus a cached `UniquePointer` (to avoid re-leaking a fresh allocation on every store)
and its on-disk schema id (for retain-mode write-back).

**Attributes can't express this** — worth stating, because it's tempting and it doesn't work:

- A *field* attribute (`#[kladde(capsule)] prefs: Preferences`): a `derive` may *read* helper
  attributes but cannot retype the field or add sibling state, so there's nowhere for the
  pointer/handle/tag to live — the field stays a bare, always-resident `Preferences`.
- A *type* attribute (`#[kladde(capsule)] struct Preferences`) escapes the "can't retype fields"
  limit (the derive controls `Preferences`'s own impl) but not the "can't *add* fields" limit — it
  can't add the cached-pointer field, so the best it could emit is a *leaky* capsule (no retain).
- Only an *attribute macro* rewriting the whole struct (`prefs: T` → `prefs: Capsule<T>`) could do
  it, but that's the bad kind of magic: the struct you wrote isn't the one that exists, direct field
  access silently changes type, tooling suffers. Reject it — the wrappers' `Deref` already gives the
  ergonomics an attribute would have promised.

**Access asymmetry:** `Capsule<T>: Deref<Target = T>` reads transparently (a capsule is always
resident). `Segment<T>` deliberately does **not** `Deref` — materializing an unloaded segment needs
the *backend* at read time (the "reads now need the backend" tension already noted in `later.md`),
so segment access goes through a backend-taking method or its guard, not a free `&*`.

**Composition — `Capsule<Segment<T>>` vs `Segment<Capsule<T>>`.** The outer wrapper is the
containing boundary:

- `Capsule<Segment<T>>` = *a versioned unit that is stored lazily* — one schema stamped over the
  whole thing, the lazy region inside it, migrate-as-one-capsule.
- `Segment<Capsule<T>>` = *a lazily-loaded region that contains an independently-versioned thing* —
  the load boundary is outer, the schema switch is inside.

For a lone `T` these are nearly equivalent; the difference bites when the wrapper contains more than
one thing, and the rule is **make the coarser concern the outer wrapper**: one version spanning
several separately-paged pieces → capsule outside (`Capsule<{ Segment<A>, Segment<B> }>`); one paged
region holding several independently-versioned pieces → segment outside
(`Segment<{ Capsule<A>, Capsule<B> }>`). The common default is **capsule outside**
(`Capsule<Segment<T>>`): versioning usually attaches to a whole conceptual object as its stable
outer identity, schema boundaries are coarse and few while load boundaries are fine and
opportunistic, and "migrate this capsule (loading its segments)" is the cleaner operation.

## Prior art: Avro (and why we borrow its schema model, not its format)

Much of the above — a stored writer schema, resolution at load by matching fields **by name**,
reader defaults for missing fields, skipping unknown ones, and a canonical **fingerprint** for
identity/dedup — is **Apache Avro's** schema-resolution model, and the convergence is deliberate.
Avro is the reference solution for evolving a *tagless, positional* binary format (Kladde likewise
writes tagless, offset-addressed data and keeps the schema out of band — the Avro side of the fork,
not Protobuf's inline-per-field-tag side), so we borrow its **schema model, resolution rules, and
canonical-form/fingerprint spec** rather than re-deriving the corner cases.

We do **not** adopt Avro the *format*. Avro serializes an **immutable** value read start-to-finish;
Kladde is a **mutable, random-access pointer heap** with in-place edits, journaling, and compaction
— none of which Avro has or could host. The fixed-offset, resolve-at-writer-offsets mechanics are
closer to **Cap'n Proto/FlatBuffers**; the live-persistence model descends from object databases
(GemStone/ZODB/Realm — see `spec.md`'s Prior Work). And the parts with *no* Avro analog —
retain-mode, the capsule/segment split, lazy per-capsule migration — exist precisely because Kladde
mutates, which Avro does not. In short: Avro's *schema policy*, on a substrate Avro was never built for.

## Conclusions

*(Claude:)*

**What to build, and in what order.**

1. **Ship Idea 3 (magic + version + root fingerprint, fail-closed) essentially now.** It's a few
   bytes and one compare, it converts silent corruption into a safe error, and every richer design
   subsumes it. There is no reason to have a real-file release without at least this. Treat it as a
   correctness fix, not a feature.
2. **Adopt Idea 4 (name-based field/variant identity) as soon as the derive macro grows schema
   descriptors.** This is the highest power-to-cost ratio change: it makes field/variant reordering
   and (with defaults) field addition non-breaking, and it removes the declaration-order
   discriminant landmine. Do it *before* any files exist in the wild, because it changes the
   encoding of discriminants and the meaning of "compatible."
3. **Grow into Idea 6 (writer-schema + resolution at load + fingerprint fast-path)** as the real
   evolution story. It is the natural fit because — the load-time-only observation bears repeating —
   Kladde already reconstructs a fresh in-memory value at open and rewrites native on flush, so it
   can interpret a foreign schema exactly once and never pays for evolution on the hot path. Idea 1
   is the right backbone; Idea 2's hash is the right fast-path/integrity key *on top of* it, not a
   replacement.
4. **Decline Idea 5 (vtables)** unless a future requirement demands true *in-place forward
   compatibility with preservation* (level d) for files concurrently shared by multiple app
   versions. It taxes the steady state to solve a problem Kladde only has at load.

**Scope the ambition honestly.** Aim squarely at **backward-compatible migration + tolerant
structural read** (levels a–c) for the *single-authoritative-app* case, which is Kladde's bread and
butter. **Retain-mode** additionally delivers a cheap *partial* forward compatibility — an older
app keeps reading after a newer one writes, as long as the newer one added no persisted fields — so
that much of level (d) comes almost for free once the layout-policy machinery exists. What stays a
non-goal for the first real-file release is the *harder* flavor of (d): a *new* persisted field
surviving a round-trip through an old app, which needs a preservation/overflow area and genuinely
fights the fixed-offset model. Robust **multi-fork** identity is likewise out of scope (revisit
only if a concrete multi-writer requirement appears — and note that if it does, it converges with
the concurrency/CRDT direction already deferred in `spec.md`).

**Design decisions to lock in now, because they constrain the on-disk format:**

- **Descriptor = representation, not Rust type** (resolves your `Calculation` question and the
  structural-typing worry). Rename `Manual` → `Opaque` and reserve it for representations that
  aren't a plain field-sum/variant-set; everything derivable stays structural.
- **`Persistable::load` must be able to read at *writer* offsets**, not just `Self::INLINE_SIZE`.
  This is a bigger change than "add a `&Type` arg" and is the mechanism that lets `INLINE_SIZE`
  evolve — design the trait for it now even if resolution lands later.
- **In-place mutation must run against the allocation's *active* layout, not `Self::INLINE_SIZE`.**
  Today's `load` keeps old pointers and would mutate a foreign layout with native offsets — latent
  corruption. Make "which layout is this allocation in" explicit: an *upgrade*-mode allocation is
  re-laid-out native (at load or first touch); a *retain*-mode allocation carries a runtime layout
  the `Guard`/`store` path uses. `INLINE_SIZE`-derived static offsets are valid *only* for native
  allocations. (This is a real trait-shape change — plan for it now even if retain-mode ships later.)
- **Cycle-aware schema canonicalization** (SCC/de-Bruijn hashing) is needed for recursive types by
  *both* ideas — build it once.

**Open questions worth deciding before committing:**

- Do struct/enum descriptors identify by **name** (my recommendation) or **explicit ordinal**? Name
  is more ergonomic; ordinal is more rename-robust. (You can allow both: name by default, optional
  `#[kladde(id=…)]`.)
- **Layout policy — default and granularity.** Is *upgrade* or *retain* the default, and is it a
  per-type attribute, a per-open flag, or decided per allocation by inspecting the diff? And is the
  schema tagged **per allocation**, **per capsule** (a schema boundary — backed by the resident
  deduplicated schema table, which also restores eager, fail-fast detection), or **globally**
  (simpler; all-or-nothing rewrite)? This is the decision the "don't always assume native layout"
  point turns on. (Lazy loading — **segments** — is an orthogonal axis; see "Capsules and segments".)
- Default policy on **flush after a resolving open**: upgrade-and-rewrite (simple, breaks old-app
  readback) vs. an opt-in pin-writer-format mode. Pick a default; expose the other.
- How much **semantic-migration** machinery to expose (from/to-fingerprint hooks) vs. leaving
  non-structural changes to the app entirely.
- Where the fingerprint lives relative to `later.md`'s deferred **file-level semantic-versioning**
  slot — they should be the *same* mechanism, not two parallel ones.
