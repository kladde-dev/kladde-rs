//! The schema fingerprint of `type-descriptors.md` §4: a 128-bit,
//! reproducible hash identifying a type's representation, computed by a
//! memoized white/gray/black DFS with de Bruijn back-references and a
//! lowlink-based cyclic flag.

use crate::descriptor::{canonical_variants, TypeDescriptor, TypeRef, TypeTable};
use crate::sha256::sha256;

/// A 128-bit schema fingerprint. The most-significant bit of byte 0 is the
/// **cyclic flag** (see [`is_cyclic`](Fingerprint::is_cyclic)); the
/// remaining 127 bits are the hash. Two fingerprints are equal only if all
/// 128 bits match, flag included.
///
/// When the cyclic flag is **clear**, the fingerprint is a *context-free
/// identity*: equal fingerprints mean structurally identical types wherever
/// they appear, so it is safe as a deduplication or cross-file key. When it
/// is **set** (the type is itself part of a cycle), the fingerprint is a
/// sound identity only for the whole type it was computed from — see
/// `type-descriptors.md` §4.7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint([u8; 16]);

impl Fingerprint {
    /// The raw 16 bytes, byte 0 first (its top bit is the cyclic flag).
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Whether the type this fingerprint identifies is itself part of a
    /// cycle (directly or mutually recursive). A type that merely *contains*
    /// a recursive type, without being reachable from it, is **not** cyclic.
    pub fn is_cyclic(&self) -> bool {
        self.0[0] & 0x80 != 0
    }

    /// Lowercase hex, byte 0 first — the form used for golden vectors.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Packs a 128-bit hash with the cyclic flag: byte 0's top bit is
    /// cleared and replaced by `cyclic`.
    fn pack(mut hash: [u8; 16], cyclic: bool) -> Self {
        hash[0] = (hash[0] & 0x7f) | if cyclic { 0x80 } else { 0 };
        Fingerprint(hash)
    }
}

impl std::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl TypeTable {
    /// The fingerprint of this table's root type (`type-descriptors.md`
    /// §4). Fully determined by the type graph — independent of descriptor
    /// indices, table order, and how enum variants are stored.
    pub fn fingerprint(&self) -> Fingerprint {
        Traversal::new(self).visit(self.root().0, 0).0
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Color {
    White,
    Gray,
    Black,
}

/// The "reaches back to nothing" sentinel for the lowlink minimum `m`.
const NO_BACKREF: usize = usize::MAX;

struct Traversal<'a> {
    table: &'a TypeTable,
    color: Vec<Color>,
    depth: Vec<usize>,
    memo: Vec<Option<Fingerprint>>,
}

impl<'a> Traversal<'a> {
    fn new(table: &'a TypeTable) -> Self {
        let n = table.descriptors().len();
        Traversal {
            table,
            color: vec![Color::White; n],
            depth: vec![0; n],
            memo: vec![None; n],
        }
    }

    /// Computes `node`'s fingerprint (entered gray at `depth`) and returns
    /// it together with the lowlink minimum `m` — the shallowest absolute
    /// depth `node`'s subtree reaches back to (`NO_BACKREF` if none).
    fn visit(&mut self, node: usize, depth: usize) -> (Fingerprint, usize) {
        self.color[node] = Color::Gray;
        self.depth[node] = depth;

        let table = self.table;
        let descriptor = &table.descriptors()[node];
        let mut input = Vec::new();
        let mut m = NO_BACKREF;

        self.encode_local(descriptor, depth, &mut input, &mut m);

        let hash = sha256(&input);
        let hash128: [u8; 16] = hash[..16].try_into().unwrap();
        let cyclic = m <= depth;
        let fingerprint = Fingerprint::pack(hash128, cyclic);

        self.memo[node] = Some(fingerprint);
        self.color[node] = Color::Black;
        (fingerprint, m)
    }

    /// Emits `node`'s per-kind hash input (§4.5 item 1): the §3.2 encoding
    /// with Struct/Enum names omitted, an Opaque version reduced to its
    /// compatibility component, and every reference replaced by a token.
    fn encode_local(
        &mut self,
        descriptor: &TypeDescriptor,
        depth: usize,
        input: &mut Vec<u8>,
        m: &mut usize,
    ) {
        match descriptor {
            TypeDescriptor::Primitive(code) => input.push(*code),
            TypeDescriptor::Struct { fields, .. } => {
                input.push(crate::descriptor::TAG_STRUCT);
                kladde_varint::encode(fields.len() as u64, input);
                for field in fields {
                    push_string(&field.name, input);
                    self.emit_reference(field.ty, depth, input, m);
                }
            }
            TypeDescriptor::Enum {
                discriminant_width,
                variants,
                ..
            } => {
                input.push(crate::descriptor::TAG_ENUM);
                input.push(*discriminant_width);
                kladde_varint::encode(variants.len() as u64, input);
                for variant in canonical_variants(variants) {
                    kladde_varint::encode(variant.discriminant, input);
                    push_string(&variant.name, input);
                    kladde_varint::encode(variant.fields.len() as u64, input);
                    for field in &variant.fields {
                        push_string(&field.name, input);
                        self.emit_reference(field.ty, depth, input, m);
                    }
                }
            }
            TypeDescriptor::Opaque {
                library_name,
                type_name,
                version,
                inline_size,
                parameters,
            } => {
                input.push(crate::descriptor::TAG_OPAQUE);
                push_string(library_name, input);
                push_string(type_name, input);
                input.push(version.stability_flag() as u8);
                kladde_varint::encode(version.leading_nonzero(), input);
                kladde_varint::encode(*inline_size, input);
                kladde_varint::encode(parameters.len() as u64, input);
                for parameter in parameters {
                    self.emit_reference(*parameter, depth, input, m);
                }
            }
        }
    }

    /// Emits the reference token for an edge from a node at `parent_depth`
    /// to `child` (§4.5 item 2), updating the running lowlink minimum `m`.
    fn emit_reference(
        &mut self,
        child: TypeRef,
        parent_depth: usize,
        input: &mut Vec<u8>,
        m: &mut usize,
    ) {
        let child = child.0;
        match self.color[child] {
            Color::White => {
                let (child_fp, child_m) = self.visit(child, parent_depth + 1);
                input.push(0x00);
                input.extend_from_slice(child_fp.as_bytes());
                *m = (*m).min(child_m);
            }
            Color::Black => {
                // A memo hit: a finished SCC cannot close a cycle through
                // the current node, so it contributes nothing to `m`.
                input.push(0x00);
                input.extend_from_slice(self.memo[child].unwrap().as_bytes());
            }
            Color::Gray => {
                input.push(0x01);
                let back = parent_depth - self.depth[child];
                kladde_varint::encode(back as u64, input);
                *m = (*m).min(self.depth[child]);
            }
        }
    }
}

fn push_string(s: &str, out: &mut Vec<u8>) {
    kladde_varint::encode(s.len() as u64, out);
    out.extend_from_slice(s.as_bytes());
}

#[cfg(test)]
mod tests {
    use crate::descriptor::{Field, TypeDescriptor, TypeRef, TypeTable, Variant, Version};

    fn primitive(code: u8) -> TypeDescriptor {
        TypeDescriptor::Primitive(code)
    }

    /// `struct Point { x: i32, y: i32 }` (i32 = code 6).
    fn point() -> TypeTable {
        TypeTable::new(vec![
            TypeDescriptor::Struct {
                name: "Point".into(),
                fields: vec![
                    Field {
                        name: "x".into(),
                        ty: TypeRef(1),
                    },
                    Field {
                        name: "y".into(),
                        ty: TypeRef(1),
                    },
                ],
            },
            primitive(6),
        ])
    }

    /// `enum List { Nil, Cons(i32, List) }`.
    fn list() -> TypeTable {
        TypeTable::new(vec![
            TypeDescriptor::Enum {
                name: "List".into(),
                discriminant_width: 4,
                variants: vec![
                    Variant {
                        discriminant: 0,
                        name: "Nil".into(),
                        fields: vec![],
                    },
                    Variant {
                        discriminant: 1,
                        name: "Cons".into(),
                        fields: vec![
                            Field {
                                name: "0".into(),
                                ty: TypeRef(1),
                            },
                            Field {
                                name: "1".into(),
                                ty: TypeRef(0),
                            },
                        ],
                    },
                ],
            },
            primitive(6),
        ])
    }

    #[test]
    fn reproducible() {
        assert_eq!(point().fingerprint(), point().fingerprint());
        assert_eq!(list().fingerprint(), list().fingerprint());
    }

    #[test]
    fn index_invariant() {
        // `struct Pair(i32, u64)` with its two leaf descriptors at indices
        // 1 and 2, then the same graph with those two indices swapped and
        // references remapped. The root stays at index 0; the fingerprint
        // must not depend on the leaves' positions.
        let a = TypeTable::new(vec![
            TypeDescriptor::Struct {
                name: "Pair".into(),
                fields: vec![
                    Field {
                        name: "0".into(),
                        ty: TypeRef(1),
                    },
                    Field {
                        name: "1".into(),
                        ty: TypeRef(2),
                    },
                ],
            },
            primitive(6),
            primitive(3),
        ]);
        // Swap indices 1 and 2 (i32 and u64), remap references accordingly.
        let b = TypeTable::new(vec![
            TypeDescriptor::Struct {
                name: "Pair".into(),
                fields: vec![
                    Field {
                        name: "0".into(),
                        ty: TypeRef(2),
                    },
                    Field {
                        name: "1".into(),
                        ty: TypeRef(1),
                    },
                ],
            },
            primitive(3),
            primitive(6),
        ]);
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn structurally_sensitive() {
        let base = point().fingerprint();

        // Reorder fields -> different.
        let reordered = TypeTable::new(vec![
            TypeDescriptor::Struct {
                name: "Point".into(),
                fields: vec![
                    Field {
                        name: "y".into(),
                        ty: TypeRef(1),
                    },
                    Field {
                        name: "x".into(),
                        ty: TypeRef(1),
                    },
                ],
            },
            primitive(6),
        ]);
        assert_ne!(base, reordered.fingerprint());

        // Rename a field -> different.
        let renamed_field = TypeTable::new(vec![
            TypeDescriptor::Struct {
                name: "Point".into(),
                fields: vec![
                    Field {
                        name: "x".into(),
                        ty: TypeRef(1),
                    },
                    Field {
                        name: "z".into(),
                        ty: TypeRef(1),
                    },
                ],
            },
            primitive(6),
        ]);
        assert_ne!(base, renamed_field.fingerprint());

        // Change a primitive code -> different.
        let retyped = TypeTable::new(vec![
            TypeDescriptor::Struct {
                name: "Point".into(),
                fields: vec![
                    Field {
                        name: "x".into(),
                        ty: TypeRef(1),
                    },
                    Field {
                        name: "y".into(),
                        ty: TypeRef(1),
                    },
                ],
            },
            primitive(7),
        ]);
        assert_ne!(base, retyped.fingerprint());

        // Rename the *type* -> same (struct name excluded).
        let renamed_type = TypeTable::new(vec![
            TypeDescriptor::Struct {
                name: "Coordinate".into(),
                fields: vec![
                    Field {
                        name: "x".into(),
                        ty: TypeRef(1),
                    },
                    Field {
                        name: "y".into(),
                        ty: TypeRef(1),
                    },
                ],
            },
            primitive(6),
        ]);
        assert_eq!(base, renamed_type.fingerprint());
    }

    fn opaque_versioned(major: u64, minor: u64, patch: u64) -> TypeTable {
        TypeTable::new(vec![TypeDescriptor::Opaque {
            library_name: "lib".into(),
            type_name: "T".into(),
            version: Version {
                major,
                minor,
                patch,
            },
            inline_size: 8,
            parameters: vec![],
        }])
    }

    #[test]
    fn opaque_version_folding() {
        // Above 0.x: patch/minor same, major differs.
        let base = opaque_versioned(1, 2, 3).fingerprint();
        assert_eq!(base, opaque_versioned(1, 2, 9).fingerprint());
        assert_eq!(base, opaque_versioned(1, 5, 0).fingerprint());
        assert_ne!(base, opaque_versioned(2, 0, 0).fingerprint());

        // Within 0.x: patch same, minor differs.
        let unstable = opaque_versioned(0, 1, 2).fingerprint();
        assert_eq!(unstable, opaque_versioned(0, 1, 9).fingerprint());
        assert_ne!(unstable, opaque_versioned(0, 2, 0).fingerprint());

        // The stability flag keeps 0.1.z and 1.y.z apart.
        assert_ne!(
            opaque_versioned(0, 1, 0).fingerprint(),
            opaque_versioned(1, 1, 0).fingerprint()
        );
    }

    #[test]
    fn recursion_sets_the_flag() {
        assert!(list().fingerprint().is_cyclic());
        assert!(!point().fingerprint().is_cyclic());
        // Mutual recursion: struct A { b: B }, struct B { a: A }.
        let mutual = TypeTable::new(vec![
            TypeDescriptor::Struct {
                name: "A".into(),
                fields: vec![Field {
                    name: "b".into(),
                    ty: TypeRef(1),
                }],
            },
            TypeDescriptor::Struct {
                name: "B".into(),
                fields: vec![Field {
                    name: "a".into(),
                    ty: TypeRef(0),
                }],
            },
        ]);
        assert!(mutual.fingerprint().is_cyclic());
    }

    #[test]
    fn contains_a_cycle_but_is_not_on_one() {
        // struct C(A); struct A(B); struct B(A). A<->B is a cycle; C only
        // references it, so C's flag must be clear (spec example (d)).
        let table = TypeTable::new(vec![
            TypeDescriptor::Struct {
                name: "C".into(),
                fields: vec![Field {
                    name: "0".into(),
                    ty: TypeRef(1),
                }],
            },
            TypeDescriptor::Struct {
                name: "A".into(),
                fields: vec![Field {
                    name: "0".into(),
                    ty: TypeRef(2),
                }],
            },
            TypeDescriptor::Struct {
                name: "B".into(),
                fields: vec![Field {
                    name: "0".into(),
                    ty: TypeRef(1),
                }],
            },
        ]);
        assert!(!table.fingerprint().is_cyclic());
    }

    #[test]
    fn flag_clear_when_cyclic_child_is_a_memo_hit() {
        // struct Root(Mid, Rec); struct Mid(Rec); enum Rec { Stop, Go(Rec) }.
        // Rec is visited first under `Mid`... but Root's field order puts
        // Mid before Rec, so when Root reaches Rec directly it is already a
        // memo hit. Neither Root nor Mid is on a cycle -> both clear.
        let table = TypeTable::new(vec![
            TypeDescriptor::Struct {
                name: "Root".into(),
                fields: vec![
                    Field {
                        name: "0".into(),
                        ty: TypeRef(1),
                    },
                    Field {
                        name: "1".into(),
                        ty: TypeRef(2),
                    },
                ],
            },
            TypeDescriptor::Struct {
                name: "Mid".into(),
                fields: vec![Field {
                    name: "0".into(),
                    ty: TypeRef(2),
                }],
            },
            TypeDescriptor::Enum {
                name: "Rec".into(),
                discriminant_width: 4,
                variants: vec![
                    Variant {
                        discriminant: 0,
                        name: "Stop".into(),
                        fields: vec![],
                    },
                    Variant {
                        discriminant: 1,
                        name: "Go".into(),
                        fields: vec![Field {
                            name: "0".into(),
                            ty: TypeRef(2),
                        }],
                    },
                ],
            },
        ]);
        assert!(!table.fingerprint().is_cyclic());
        // The recursive Rec type on its own is cyclic.
        let rec_only = TypeTable::new(vec![TypeDescriptor::Enum {
            name: "Rec".into(),
            discriminant_width: 4,
            variants: vec![
                Variant {
                    discriminant: 0,
                    name: "Stop".into(),
                    fields: vec![],
                },
                Variant {
                    discriminant: 1,
                    name: "Go".into(),
                    fields: vec![Field {
                        name: "0".into(),
                        ty: TypeRef(0),
                    }],
                },
            ],
        }]);
        assert!(rec_only.fingerprint().is_cyclic());
    }

    #[test]
    fn golden_vectors() {
        // Locks the byte encoding and hash. If the encoding changes on
        // purpose, update these; an accidental change trips them.
        assert_eq!(point().fingerprint().to_hex(), GOLDEN_POINT);
        assert_eq!(list().fingerprint().to_hex(), GOLDEN_LIST);
        assert_eq!(
            opaque_versioned(1, 2, 3).fingerprint().to_hex(),
            GOLDEN_OPAQUE_1_2_3
        );
    }

    const GOLDEN_POINT: &str = "0d91791f5827efadfa8150af9812fad6";
    const GOLDEN_LIST: &str = "af12af0167771e03506952fff8a274fd";
    const GOLDEN_OPAQUE_1_2_3: &str = "11435cae785f4d2027e535f33aa7447a";
}
