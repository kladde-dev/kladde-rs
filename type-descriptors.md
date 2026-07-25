# Kladde Type Descriptors and Schema Fingerprints

## Status and scope

This document specifies three things, precisely enough that any conforming
implementation — in any language — produces byte-identical serializations and
bit-identical fingerprints:

1. the **type-descriptor model** — a language-neutral description of how a Kladde
   value type lays out and interprets its bytes;
1. a canonical **byte serialization** of a table of type descriptors; and
1. the computation of a **schema fingerprint** — a fixed-size, reproducible hash
   that identifies a type's representation.

**Out of scope:** schema *resolution*, migration, compatibility policy, and where
the descriptor table is physically placed inside a file. This document defines
only how descriptors are modeled, serialized, and fingerprinted.

## 1. Terminology

- **Type descriptor** (*descriptor*): a node describing one value type's on-disk
  representation.
- **Descriptor table**: an ordered array of descriptors. A descriptor refers to
  another by its **index** in this table — a table-local, otherwise-arbitrary
  integer. Indices are an artifact of one serialization; a type's fingerprint
  does not depend on them.
- **Type graph**: the directed graph whose vertices are descriptors and whose
  edges are the references one descriptor makes to others. It may contain cycles
  (recursive and mutually-recursive types).
- **Representation, not identity**: a descriptor describes *how bytes are laid out
  and interpreted*, not which source-language type produced them. Two types with
  byte-identical representations have identical descriptors and identical
  fingerprints; this is intentional and harmless, because a value is always
  reconstructed top-down from a known root type.

## 2. The descriptor model

A descriptor is one of four **kinds** — Primitive, Struct, Enum, or Opaque — plus
**Array**, a fifth kind reserved for a future revision (§2.5).

### 2.1 Primitive

A fixed-width scalar with a fixed byte encoding, identified by a **primitive
code**:

| code | type   | width (bytes) | encoding                                   |
| ---- | ------ | ------------- | ------------------------------------------ |
| 0    | `u8`   | 1             | little-endian                              |
| 1    | `u16`  | 2             | little-endian                              |
| 2    | `u32`  | 4             | little-endian                              |
| 3    | `u64`  | 8             | little-endian                              |
| 4    | `i8`   | 1             | little-endian two's-complement             |
| 5    | `i16`  | 2             | little-endian two's-complement             |
| 6    | `i32`  | 4             | little-endian two's-complement             |
| 7    | `i64`  | 8             | little-endian two's-complement             |
| 8    | `f32`  | 4             | little-endian IEEE-754                     |
| 9    | `f64`  | 8             | little-endian IEEE-754                     |
| 10   | `bool` | 1             | `0x00` = false, `0x01` = true              |
| 11   | `char` | 4             | little-endian Unicode scalar value (`u32`) |

The primitive **code** doubles as the descriptor's kind-tag byte: codes `0..=127`
are reserved for primitives (§3.2), so a Primitive descriptor is a *single byte*
with no further content. A Primitive's inline width is fixed by its code. The set
is extensible — future revisions may append codes within `0..=127`.

#### 2.1.1 Reserved primitive codes (not yet implemented)

The following codes are reserved for primitives that no `Persistable` type uses
yet. Their encoding is fixed here so that later additions cannot conflict, but an
implementation need not accept them until a corresponding type exists and can be
tested. Codes 12–15 extend the standard set; codes 16–23 are the integers whose
byte width is not a power of two, up to 7 bytes — each stored little-endian in
its full byte width (no packing).

| code | type   | width (bytes) | encoding                        |
| ---- | ------ | ------------- | ------------------------------- |
| 12   | `u128` | 16            | little-endian                   |
| 13   | `i128` | 16            | little-endian two's-complement  |
| 14   | `f16`  | 2             | little-endian IEEE-754 binary16 |
| 15   | `bf16` | 2             | little-endian bfloat16          |
| 16   | `u24`  | 3             | little-endian                   |
| 17   | `u40`  | 5             | little-endian                   |
| 18   | `u48`  | 6             | little-endian                   |
| 19   | `u56`  | 7             | little-endian                   |
| 20   | `i24`  | 3             | little-endian two's-complement  |
| 21   | `i40`  | 5             | little-endian two's-complement  |
| 22   | `i48`  | 6             | little-endian two's-complement  |
| 23   | `i56`  | 7             | little-endian two's-complement  |

### 2.2 Struct

An ordered, fixed set of named fields laid out consecutively (no discriminant, no
header). Carries:

- `name`: a UTF-8 string, **for diagnostics only** — a Struct's own name is *not*
  fingerprinted (see §4.3);
- `fields`: an ordered list of `(field_name: string, type: reference)`.

Field **order** is significant (it is the on-disk order). Each field's **name** is
significant. A positional (tuple) field's `field_name` is the decimal rendering of
its position (`"0"`, `"1"`, …), so the model stays uniform.

### 2.3 Enum

A discriminated union: a discriminant, followed by the fields of the selected
variant. Carries:

- `name`: UTF-8 string, diagnostics only (not fingerprinted);
- `discriminant_width`: the number of bytes the discriminant occupies (1, 2, 4,
  or 8);
- `variants`: a list — canonically ordered by ascending `discriminant_value`
  (§3.2) — each
  `(discriminant_value: u64, variant_name: string, fields: [(field_name: string, type: reference)])`.

Each `discriminant_value`, each `variant_name`, and each variant's field list is
significant. Variant *declaration* order is **not**: because a value's byte layout
is selected by the stored discriminant (not by a variant's position), variants are
canonicalized into ascending `discriminant_value` order, so reordering them in the
source — without changing their discriminants — changes neither the representation
nor the fingerprint.

### 2.4 Opaque

A type whose internal structure this format does **not** decompose — treated as a
black box identified nominally. Used for types with hand-written representations
(for example, length-prefixed blobs, externally-serialized payloads, or built-in
container types whose layout is not a plain field sum). Carries:

- `library_name`: UTF-8 string — the name of the library (package, module, …) that
  defines the type. (Language-neutral: not necessarily a Rust crate.)
- `type_name`: UTF-8 string;
- `version`: a triple `(major, minor, patch)`;
- `inline_size`: the fixed number of bytes the type occupies **inline** — required
  because it is not structurally derivable. (A blob type that stores its payload
  out of line occupies its fixed header here; a fixed-size inline type occupies
  its own width.)
- `parameters`: an ordered list of type references (the type's generic parameters,
  if any).

Only the **compatibility component** of `version` participates in the fingerprint
(§4.3). It has two parts, **both fingerprinted**:

- a **stability flag** — `true` when `major > 0` (the `1.x`-and-up "stable" regime),
  `false` when `major == 0` (the `0.x` "unstable" regime); and
- the **leading nonzero component** — `major` when `major > 0`, otherwise `minor`.

Both parts are necessary: without the stability flag, `0.1.z` and `1.y.z` would both
reduce to the value `1` and collide, even though crossing `0.x` → `1.x` is a breaking
change. (Patch-level changes, and minor-level changes above `0.x`, are thereby
treated as representation-compatible.)

### 2.5 Array (reserved for a future revision)

A fixed-length, homogeneous sequence: `count` consecutive values of a single element
type, laid out with no header and no discriminant (the byte layout of a
source-language `[T; N]`). Carries:

- `element`: a reference to the element type;
- `count`: the fixed number of elements.

An Array's inline width is `count × element.inline_size`.

This kind is **reserved but not yet implemented**: no `Persistable` type produces one
today, so its tag (`131`, §3.2), encoding, and fingerprint contribution are fixed
here, but an implementation need not accept it until an array type exists and can be
tested. An Array is *not* interchangeable with a Struct of `count` identically-typed
positional fields: their byte layouts coincide, but they carry distinct kind tags and
therefore distinct fingerprints. The kind exists precisely to avoid the descriptor-
and fingerprint-blow-up of spelling out a large `count` as that many Struct fields.

### 2.6 References and the root

A **reference** is an index into the descriptor table. Every `type`, `element`, and
parameter field above is a reference. By convention the descriptor at **index 0 is
the root** — the type actually stored.

## 3. Canonical storage serialization

### 3.1 Encoding primitives

- `byte(b)`: a single octet.
- `varint(n)`: unsigned LEB128 (7 bits per byte, little-endian groups, high bit =
  continuation).
- `string(s)`: `varint(byte_length)` followed by the UTF-8 bytes of `s`.
- `reference(index)`: `varint(index)`.

### 3.2 Descriptor encoding

Each descriptor begins with a single **kind-tag byte** whose value both selects the
kind and partitions the tag space:

- `0..=127` — a **Primitive**. The tag byte *is* the primitive code (§2.1); the
  descriptor has **no further content**.
- `128..=255` — a non-primitive kind, followed by kind-specific content:
    - **Struct** (`128`): `string(name)`, `varint(field_count)`, then for each field
      `string(field_name) reference(type)`.
    - **Enum** (`129`): `string(name)`, `byte(discriminant_width)`,
      `varint(variant_count)`, then each variant **in ascending `discriminant_value`
      order** (see canonical order below) as `varint(discriminant_value)
      string(variant_name) varint(field_count)` then for each variant field
      `string(field_name) reference(type)`.
    - **Opaque** (`130`): `string(library_name) string(type_name) varint(major)
      varint(minor) varint(patch) varint(inline_size) varint(param_count)` then for
      each parameter `reference(type)`.
    - **Array** (`131`, reserved — §2.5): `reference(element) varint(count)`.
    - **Pointer** (`132`, reserved): a kind tag reserved for a future revision; its
      content and semantics are not yet defined.

Tags `133..=255` are unassigned, reserved for future kinds.

**Canonical order.** Within a descriptor, the order in which sub-elements are
emitted is fixed, so the encoding (and the fingerprint of §4) is fully determined:

- **Struct fields** are emitted in **declaration order**, which is their on-disk
  layout order — a field's position determines its byte offset, so this order is
  part of the type's representation and is never re-sorted.
- **Enum variants** are emitted in **ascending `discriminant_value`** order. A
  variant's position does *not* affect any value's layout (the stored discriminant
  selects the variant), so declaration order is discarded in favor of this canonical
  order; two enums that differ only in the source order of their variants encode and
  fingerprint identically.
- **Opaque parameters** and an **Array**'s element keep their given order (positional
  type arguments).

### 3.3 Table encoding

`varint(descriptor_count)` followed by each descriptor in index order (root first).

This is the canonical on-file encoding of a schema. The exact placement, framing,
alignment, and checksumming of this blob within a Kladde file is defined by the
file-format specification and is out of scope here.

## 4. Schema fingerprints

### 4.1 The fingerprint value

A **fingerprint** is a 128-bit (16-byte) hash. Two fingerprints are equal iff all
128 bits are equal. It carries no flags or reserved bits — every bit is hash output.

### 4.2 Hash function

The hash is **SHA-256 truncated to its leading 128 bits** — the first 16 bytes of
the digest, byte 0 first.

SHA-256 is chosen for **exact cross-language reproducibility**: it is present in
every mainstream language's standard library with universal test vectors, and
"compute SHA-256, keep the first 16 bytes" is trivial to reimplement identically.
128 bits gives an astronomically small accidental-collision probability for any
realistic number of distinct schemas (birthday bound ≈ N²/2¹²⁸); cryptographic
strength is deliberately *not* relied upon (see §4.8). A future format revision may
substitute another hash or width; the choice is a format-version property.

### 4.3 What is and isn't fingerprinted

The fingerprint of a type is computed from the **type graph reachable from that
type**, encoded as in §4.5. The encoding deliberately:

- **includes** the kind, primitive codes, discriminant widths and values, field
  and variant **names**, and field/variant **order** — all of which affect the
  representation or the identities used to reconcile it;
- **excludes** the `name` of Struct and Enum descriptors — a type's own name does
  not affect its byte layout, so renaming a type must not change its fingerprint;
- reduces an Opaque `version` to its **compatibility component** — the stability
  flag plus the leading nonzero component (§2.4);
- **excludes table indices entirely** — references are encoded structurally
  (§4.5), so the fingerprint is invariant under any renumbering or reordering of
  the descriptor table.

### 4.4 Traversal: white/gray/black DFS with memoization

The fingerprint of a start type `R` is computed by a depth-first search over the
type graph from `R`, using the standard three-color marking of **nodes**:

- **white** — not yet visited;
- **gray** — on the current DFS stack (its fingerprint is in progress);
- **black** — fully visited (its fingerprint is known and memoized).

Maintain: `color[·]` (all white initially), a stack-depth counter, `depth[·]` for
gray nodes, and a **memo** mapping each black node to its fingerprint. Memoization
makes each node's hash input built exactly once, so the whole computation is
**linear** in the size of the reachable graph. The fingerprint of `R` is a pure
function of `R`'s reachable subgraph; each computation begins from an all-white
state.

**What this does, in one picture.** Classify each *edge* by the color of the node it
points to when it is first followed. The **white** edges (to not-yet-visited nodes)
form a spanning tree of the subgraph reachable from `R`. The **gray** edges (to a
node still on the stack) are exactly the back-edges: each points to the current node
itself or to one of its ancestors in that spanning tree. Removing them leaves the white plus
**black** edges (black = to an already-finished node), which form a DAG. The
fingerprint **merkle-hashes that DAG** — each node's hash is built from its
children's hashes, and a black edge simply reuses the child's already-computed hash,
so a subtree reached more than once is hashed only once — while each gray back-edge
is encoded not by a hash (which would recurse forever) but by a small integer, its
**de Bruijn index**: how many levels up the tree it points, `0` meaning the node
itself (§4.5). That relative encoding is what lets a cyclic graph be hashed as if it
were a finite tree, and — because neither the merkle hashes nor the de Bruijn
integers mention table positions — what makes the fingerprint independent of how the
descriptor table is numbered, ordered, or shared/deduplicated.

### 4.5 Per-node hash input

To compute the fingerprint of a node `N` — colored gray on entry at the current
`depth[N]` — build a byte string:

1. `N`'s **kind tag** and **local scalar content**, encoded exactly as in §3.2,
   **except** that a Struct/Enum `name` is omitted and an Opaque `version` triple is
   replaced by its compatibility component (§2.4) — `byte(stability_flag)
   varint(leading_nonzero_component)`, where `stability_flag` is `1` if `major > 0`
   else `0`; and with **every reference replaced by a reference token** as defined
   next.
1. For each outgoing reference to a child `C`, in canonical (§3.2) order, emit a
   **reference token**:

   - if `color[C]` is **white**: recursively compute `C`'s fingerprint (this
     colors `C` black and memoizes it); emit `byte(0x00)` followed by `C`'s 16-byte
     fingerprint.
   - if `color[C]` is **black**: emit `byte(0x00)` followed by `C`'s memoized
     16-byte fingerprint.
   - if `color[C]` is **gray**: emit `byte(0x01)` followed by
     `varint(depth[N] − depth[C])` — a **de Bruijn back-reference** giving the
     number of stack levels from `N` up to the in-progress ancestor `C` (`0`
     denotes `N` itself). Absolute depths never appear; only this difference does.

`N`'s **fingerprint** is the leading 128 bits of the SHA-256 of this byte string.
Record it in the memo and color `N` black.

### 4.6 Identity semantics

A fingerprint is always computed for a **start type, rooted at itself** (§4.4, from
an all-white state). Read that way it is a clean identity:

- **A root fingerprint identifies its whole type.** Two independent computations of
  the same start type yield the same fingerprint, and two structurally different
  start types yield different fingerprints (up to the 128-bit hash's collision
  resistance, §4.8). It is invariant to how the descriptor table is numbered,
  ordered, or shared (§4.4), so it is safe to compare across files and
  implementations. This — comparing whole types by their root fingerprint — is the
  intended use.

A fingerprint is **not** an identity for a type as it sits *nested inside another
type's* traversal. Two things go wrong if you lift such a nested value out and reuse
it:

- **Entry-relativity (a recursive node is context-dependent).** A gray back-edge is
  encoded relative to the current traversal (§4.5), so a recursive node reached via a
  different entry point unfolds to different bytes. The same nested recursive type can
  therefore have *different* fingerprints in different surroundings.
- **Non-injectivity (a back-edge hides its target).** A de Bruijn index records only
  *how far up* an ancestor sits, never *which* ancestor. So two structurally
  different nested nodes whose back-edges happen to point the same number of levels up
  can share a fingerprint.

Both failure modes vanish when a type is fingerprinted **as its own root** (no
enclosing traversal, so no gray edge escapes it): that computation is the type's
canonical identity. So to identify a nested type — e.g. to deduplicate sub-schemas or
give a capsule its own version tag — do not reuse its fingerprint as it appeared
inside a parent; recompute it rooted at that type.

### 4.7 Determinism

Given a type graph, the fingerprint is fully determined: all encodings are
byte-exact and endian-fixed; traversal order follows the canonical field/variant
order; and no table indices, table order, timestamps, or random seeds participate.
Two conforming implementations, in any language, produce identical fingerprints.

### 4.8 Security considerations

A fingerprint is an **identity and detection** mechanism, not an **authenticity**
one. Two properties matter here, and they are different:

- **Accidental-collision resistance.** Two structurally *different* schemas must not
  produce the same fingerprint by chance, or a reader could take a fast path and
  interpret bytes under the wrong layout. 128 bits makes this negligible for any
  realistic population of distinct schemas (§4.2), and SHA-256 truncation gives
  near-ideal distribution even for schemas that differ in a single byte. This is the
  property the format *does* rely on.
- **Adversarial-collision resistance** is **not** relied upon. In the intended model
  a Kladde file is written by an application for itself, or received whole from
  another party; the schema descriptors and any stored fingerprint travel *inside*
  the file. An attacker who can supply a file already controls its declared schema
  and its bytes, so they can make a reader interpret arbitrary bytes under any layout
  simply by *declaring* that layout honestly — a crafted fingerprint collision grants
  no capability beyond that. The fingerprint selects an interpretation; it is never a
  trust or privilege boundary.

Two obligations follow, and they belong to the surrounding system rather than to the
hash:

1. **The read path must stay memory-safe under a mismatched schema.** Because a
   collision (accidental or adversarial) degrades to "read bytes under the wrong
   layout," the loader must bounds-check every read and validate every decoded
   pointer/index, treating any inconsistency as a clean error — never undefined
   behavior. With that in place, a mismatch yields wrong data, a clean "corrupt
   file" error, or a panic, not memory unsafety. This is required anyway, for
   truncated files and ordinary corruption.
1. **The fingerprint provides no tamper protection.** A bare hash never does: an
   attacker who edits the schema or data can recompute it. Detecting *adversarial*
   modification requires a signature or MAC over the file, which is out of scope
   here; the fingerprint detects only *accidental* schema corruption.

Cryptographic-strength collision resistance would become relevant only if
fingerprints were ever used as **trusted content-addresses shared between mutually
distrusting parties** (as Git and IPFS use object hashes) — a multi-writer/sharing
scenario outside this specification. Should that arise, widen the fingerprint back
to a full-length cryptographic digest (a format-version change); nothing else in
§4 depends on the width.

## 5. Worked examples

**(a) A flat struct.** `struct Point { x: i32, y: i32 }`.
The graph is `Point → i32` with no cycle. Hashing `Point` builds
`[tag=Struct] varint(2) string("x") <i32 fingerprint> string("y") <i32
fingerprint>` (the struct's own name omitted). Both field references are `0x00`
tokens carrying `i32`'s fingerprint — every edge is a white/black merkle edge, none
is a back-edge.

**(b) A singly-linked list.** `enum List { Nil, Cons(i32, List) }`.
Hashing `List` colors `List` gray at depth 0, then serializes its two variants;
inside `Cons` the second field references `List`, which is gray at depth 0. The node
emitting that edge is `List` itself (depth 0), so the token is `byte(0x01) varint(0)`
— a de Bruijn self back-reference. Rooted at `List` this is `List`'s canonical
identity; recomputing it from `List` yields the same value.

**(c) Mutual recursion.** `struct A { b: B }`, `struct B { a: A }`.
Hashing `A` (depth 0) recurses into `B` (depth 1); `B`'s field references `A`, which
is gray at depth 0, so `B` emits `byte(0x01) varint(1)`. `fingerprint(A)` and
`fingerprint(B)` differ — different kind/field content, and the back-edge sits at a
different level — correctly reflecting that `A` and `B` are different types. (Note
these are the fingerprints *rooted at `A`* and *rooted at `B`* respectively; `B`'s
value as it appears nested inside `A`'s traversal is a different, entry-relative
thing — §4.6.)

**(d) A shared subtree (a DAG).** `struct Pair { first: Inner, second: Inner }` for
some non-trivial `Inner`. Hashing `Pair` follows `first` to `Inner`, which is white,
so it is visited and hashed once (a white edge) and its fingerprint memoized; the
`second` edge finds `Inner` **black** and simply reuses that memoized fingerprint (a
black edge). So `Inner` is hashed once and referenced twice. Because the reuse is by
*content* (`Inner`'s fingerprint), `Pair`'s fingerprint is identical whether the two
fields share one `Inner` descriptor or point at two byte-identical ones — the
fingerprint depends on structure, not on how the table represents sharing.

## 6. Conformance

An implementation conforms if, for every type graph, it produces (a) the §3
serialization byte-for-byte and (b) the §4 fingerprint bit-for-bit, with the identity
semantics of §4.6. A shared suite of **golden vectors** (fixed descriptor tables
mapped to fixed hexadecimal fingerprints) is the recommended cross-language
conformance test.
