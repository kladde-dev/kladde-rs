# Kladde Type Descriptors and Schema Fingerprints

## Status and scope

This document specifies three things, precisely enough that any conforming
implementation — in any language — produces byte-identical serializations and
bit-identical fingerprints:

1. the **type-descriptor model** — a language-neutral description of how a Kladde
   value type lays out and interprets its bytes;
2. a canonical **byte serialization** of a table of type descriptors; and
3. the computation of a **schema fingerprint** — a fixed-size, reproducible hash
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

A descriptor is one of four **kinds**: Primitive, Struct, Enum, or Opaque.

### 2.1 Primitive

A fixed-width scalar with a fixed byte encoding, identified by a **primitive
code**:

| code | type | width (bytes) | encoding |
|---|---|---|---|
| 0  | `u8`   | 1 | little-endian |
| 1  | `u16`  | 2 | little-endian |
| 2  | `u32`  | 4 | little-endian |
| 3  | `u64`  | 8 | little-endian |
| 4  | `i8`   | 1 | little-endian two's-complement |
| 5  | `i16`  | 2 | little-endian two's-complement |
| 6  | `i32`  | 4 | little-endian two's-complement |
| 7  | `i64`  | 8 | little-endian two's-complement |
| 8  | `f32`  | 4 | little-endian IEEE-754 |
| 9  | `f64`  | 8 | little-endian IEEE-754 |
| 10 | `bool` | 1 | `0x00` = false, `0x01` = true |
| 11 | `char` | 4 | little-endian Unicode scalar value (`u32`) |

The set is extensible: future revisions may append new codes. A Primitive's inline
width is fixed by its code.

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
- `variants`: an ordered list, each
  `(discriminant_value: u64, variant_name: string, fields: [(field_name: string, type: reference)])`.

Variant order, each `discriminant_value`, each `variant_name`, and each variant's
field list are all significant.

### 2.4 Opaque

A type whose internal structure this format does **not** decompose — treated as a
black box identified nominally. Used for types with hand-written representations
(for example, length-prefixed blobs, externally-serialized payloads, or built-in
container types whose layout is not a plain field sum). Carries:

- `crate_name`: UTF-8 string;
- `type_name`: UTF-8 string;
- `version`: a triple `(major, minor, patch)`;
- `inline_size`: the fixed number of bytes the type occupies **inline** — required
  because it is not structurally derivable. (A blob type that stores its payload
  out of line occupies its fixed header here; a fixed-size inline type occupies
  its own width.)
- `parameters`: an ordered list of type references (the type's generic parameters,
  if any).

Only the **compatibility component** of `version` participates in the fingerprint
(§4.3): the leading nonzero component under the semantic-versioning convention —
`major` when `major > 0`, otherwise `minor`. (Patch- and, above 0.x, minor-level
changes are thereby treated as representation-compatible.)

### 2.5 References and the root

A **reference** is an index into the descriptor table. Every `type` field above is
a reference. By convention the descriptor at **index 0 is the root** — the type
actually stored.

## 3. Canonical storage serialization

### 3.1 Encoding primitives

- `byte(b)`: a single octet.
- `varint(n)`: unsigned LEB128 (7 bits per byte, little-endian groups, high bit =
  continuation).
- `string(s)`: `varint(byte_length)` followed by the UTF-8 bytes of `s`.
- `reference(index)`: `varint(index)`.

### 3.2 Descriptor encoding

Each descriptor is a **kind tag** byte — `Primitive = 0`, `Struct = 1`,
`Enum = 2`, `Opaque = 3` — followed by kind-specific content:

- **Primitive**: `varint(primitive_code)`.
- **Struct**: `string(name)`, `varint(field_count)`, then for each field
  `string(field_name) reference(type)`.
- **Enum**: `string(name)`, `byte(discriminant_width)`, `varint(variant_count)`,
  then for each variant `varint(discriminant_value) string(variant_name)
  varint(field_count)` then for each variant field `string(field_name)
  reference(type)`.
- **Opaque**: `string(crate_name) string(type_name) varint(major) varint(minor)
  varint(patch) varint(inline_size) varint(param_count)` then for each parameter
  `reference(type)`.

### 3.3 Table encoding

`varint(descriptor_count)` followed by each descriptor in index order (root first).

This is the canonical on-file encoding of a schema. The exact placement, framing,
alignment, and checksumming of this blob within a Kladde file is defined by the
file-format specification and is out of scope here.

## 4. Schema fingerprints

### 4.1 The fingerprint value

A **fingerprint** is 128 bits (16 bytes). The **most-significant bit of byte 0** is
the **cyclic flag**; the remaining 127 bits are the **hash**. Two fingerprints are
equal iff all 128 bits are equal (flag included).

### 4.2 Hash function

The hash is **SHA-256 truncated to its leading 128 bits** (the first 16 bytes of
the digest, byte 0 first). Where a 127-bit hash is required, take those 16 bytes
and clear the most-significant bit of byte 0; that bit position instead carries the
cyclic flag (§4.6). The one-bit reduction is negligible.

SHA-256 is chosen for **exact cross-language reproducibility**: it is present in
every mainstream language's standard library with universal test vectors, and
"compute SHA-256, keep the first 16 bytes" is trivial to reimplement identically.
128 bits gives an astronomically small accidental-collision probability for any
realistic number of distinct schemas (birthday bound ≈ N²/2¹²⁸); cryptographic
strength is deliberately *not* relied upon (see §4.9). A future format revision may
substitute another hash or width; the choice is a format-version property.

### 4.3 What is and isn't fingerprinted

The fingerprint of a type is computed from the **type graph reachable from that
type**, encoded as in §4.5. The encoding deliberately:

- **includes** the kind, primitive codes, discriminant widths and values, field
  and variant **names**, and field/variant **order** — all of which affect the
  representation or the identities used to reconcile it;
- **excludes** the `name` of Struct and Enum descriptors — a type's own name does
  not affect its byte layout, so renaming a type must not change its fingerprint;
- reduces an Opaque `version` to its **compatibility component** (§2.4);
- **excludes table indices entirely** — references are encoded structurally
  (§4.5), so the fingerprint is invariant under any renumbering or reordering of
  the descriptor table.

### 4.4 Traversal: white/gray/black DFS with memoization

The fingerprint of a start type `R` is computed by a depth-first search over the
type graph from `R`, using the standard three-color marking:

- **white** — not yet visited;
- **gray** — on the current DFS stack (its fingerprint is in progress);
- **black** — fully visited (its fingerprint is known and memoized).

Maintain: `color[·]` (all white initially), a stack-depth counter, `depth[·]` for
gray nodes, and a **memo** mapping each black node to its `(fingerprint, cyclic)`
pair. Memoization makes each node's hash input built exactly once, so the whole
computation is **linear** in the size of the reachable graph.

The fingerprint of `R` is a pure function of `R`'s reachable subgraph; each
computation begins from an all-white state. (An implementation *may* cache results
across different start types, but only for results whose cyclic flag is clear —
see §4.7.)

### 4.5 Per-node hash input

To compute the fingerprint of a node `N` — colored gray on entry at the current
`depth[N]` — build a byte string:

1. `N`'s **kind tag** and **local scalar content**, encoded exactly as in §3.2,
   **except** that a Struct/Enum `name` is omitted and an Opaque `version` triple
   is replaced by `varint(compatibility_component)` (a single value); and with
   **every reference replaced by a reference token** as defined next.
2. For each outgoing reference to a child `C`, in canonical (§3.2) order, emit a
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

`N`'s **hash** is the leading 128 bits of the SHA-256 of this byte string, top bit
cleared.

### 4.6 Cyclic flag

`N`'s **cyclic flag** is the logical OR of:

- `true` for every reference token that was a de Bruijn back-reference (§4.5, gray
  case); and
- the cyclic flag carried by every child fingerprint incorporated (§4.5, white and
  black cases).

Equivalently, the flag is set iff **any cycle occurs anywhere in `N`'s reachable
subgraph.** `N`'s fingerprint packs `N`'s 127-bit hash with this flag in the top
bit. Record `(fingerprint, cyclic)` in the memo and color `N` black.

### 4.7 Equality semantics

- Two fingerprints that are **fully equal** were produced by structurally identical
  inputs (up to the 128-bit hash's collision resistance; see §4.9).
- If two fingerprints are equal **and their cyclic flag is clear**, the two types
  have **structurally identical representations in every context**. Such a
  fingerprint is a **context-free identity**: it is valid for deduplication, for
  cross-file comparison, and as a cache key, wherever the type appears.
- If the cyclic flag is **set**, the fingerprint is a sound identity **only for the
  whole start type it was computed from** — e.g., two independently-computed
  fingerprints of the same recursive root type are equal. It must **not** be used
  to identify a *nested* type lifted out of a larger traversal, because a recursive
  node's fingerprint depends on the traversal's entry point: a de Bruijn
  back-reference encodes a position relative to the traversal, and the same
  recursive node reached via a different entry unfolds to different bytes.

### 4.8 Determinism

Given a type graph, the fingerprint is fully determined: all encodings are
byte-exact and endian-fixed; traversal order follows the canonical field/variant
order; and no table indices, table order, timestamps, or random seeds participate.
Two conforming implementations, in any language, produce identical fingerprints.

### 4.9 Security considerations

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
2. **The fingerprint provides no tamper protection.** A bare hash never does: an
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
The graph is `Point → i32` (shared) with no cycle. Hashing `Point` builds
`[tag=Struct] varint(2) string("x") <i32 fingerprint> string("y") <i32
fingerprint>` (the struct's own name omitted). Both field references are `0x00`
tokens carrying `i32`'s fingerprint. The cyclic flag is **clear**, so `Point`'s
fingerprint is a context-free identity.

**(b) A singly-linked list.** `enum List { Nil, Cons(i32, List) }`.
Hashing `List` colors `List` gray at depth 0, then serializes its two variants;
inside `Cons` the second field references `List`, which is gray at depth 0. The
current node emitting that edge is `List` itself (depth 0), so the token is
`byte(0x01) varint(0)` — a self back-reference. The cyclic flag is **set**.
`List`'s fingerprint is still a sound identity for `List` as a whole (recomputing
it from `List` yields the same value), but the flag warns that it must not be
reused to identify some nested recursive type reached from elsewhere.

**(c) Mutual recursion.** `struct A { b: B }`, `struct B { a: A }`.
Hashing `A` (depth 0) recurses into `B` (depth 1); `B`'s field references `A`,
which is gray at depth 0, so `B` emits `byte(0x01) varint(1)`. `B`'s hash and
flag (set) are memoized, then folded into `A`'s hash; `A`'s flag is set too.
`hash(A)` and `hash(B)` differ (their kind/field content and the depth of the
back-reference differ), correctly reflecting that `A` and `B` are different types.

## 6. Conformance

An implementation conforms if, for every type graph, it produces (a) the §3
serialization byte-for-byte and (b) the §4 fingerprint bit-for-bit, including the
cyclic flag and the equality semantics of §4.7. A shared suite of
**golden vectors** (fixed descriptor tables mapped to fixed hexadecimal
fingerprints) is the recommended cross-language conformance test.
