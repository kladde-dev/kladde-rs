//! The schema fingerprint of `type-descriptors.md` §4: a 128-bit,
//! reproducible hash identifying a type's representation, computed by a
//! memoized white/gray/black DFS that merkle-hashes the acyclic edges and
//! encodes each back-edge as a de Bruijn index.

use crate::descriptor::{canonical_variants, TypeDescriptor, TypeRef, TypeTable};
use crate::sha256::Sha256Hasher;

/// A 128-bit schema fingerprint — a plain hash, no flags or reserved bits.
/// Two fingerprints are equal iff all 128 bits match.
///
/// Computed for a type **rooted at itself**, a fingerprint is that type's
/// identity: two computations of the same type agree, different types
/// differ (up to hash collision), and the value is invariant to how the
/// descriptor table is numbered, ordered, or shared. Comparing whole types
/// by their root fingerprint is the intended use. A type's fingerprint *as
/// it appears nested inside another type's traversal* is **not** a reusable
/// identity — see `type-descriptors.md` §4.6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint([u8; 16]);

impl Fingerprint {
    /// The raw 16 bytes, byte 0 first.
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Lowercase hex, byte 0 first — the form used for golden vectors.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
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
        Traversal::new(self).visit(self.root().0, 0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Color {
    White,
    Gray,
    Black,
}

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

    /// Computes and returns `node`'s fingerprint, entered gray at `depth`.
    fn visit(&mut self, node: usize, depth: usize) -> Fingerprint {
        self.color[node] = Color::Gray;
        self.depth[node] = depth;

        let table = self.table;
        let descriptor = &table.descriptors()[node];
        let mut hasher = Sha256Hasher::new();

        self.encode_local(descriptor, depth, &mut hasher);

        let hash = hasher.finalize();
        let fingerprint = Fingerprint(hash[..16].try_into().unwrap());

        self.memo[node] = Some(fingerprint);
        self.color[node] = Color::Black;
        fingerprint
    }

    /// Emits `node`'s per-kind hash input (§4.5 item 1): the §3.2 encoding
    /// with Struct/Enum names omitted, an Opaque version reduced to its
    /// compatibility component, and every reference replaced by a token.
    fn encode_local(
        &mut self,
        descriptor: &TypeDescriptor,
        depth: usize,
        hasher: &mut Sha256Hasher,
    ) {
        match descriptor {
            TypeDescriptor::Primitive(primitive) => hasher.update([primitive.code()]),
            TypeDescriptor::Struct { fields, .. } => {
                hasher.update([crate::descriptor::TAG_STRUCT]);
                kladde_varint::encode(fields.len() as u64, hasher);
                for field in fields {
                    push_string(&field.name, hasher);
                    self.emit_reference(field.ty, depth, hasher);
                }
            }
            TypeDescriptor::Enum {
                discriminant_width,
                variants,
                ..
            } => {
                hasher.update([crate::descriptor::TAG_ENUM]);
                hasher.update([*discriminant_width]);
                kladde_varint::encode(variants.len() as u64, hasher);
                for variant in canonical_variants(variants) {
                    kladde_varint::encode(variant.discriminant, hasher);
                    push_string(&variant.name, hasher);
                    kladde_varint::encode(variant.fields.len() as u64, hasher);
                    for field in &variant.fields {
                        push_string(&field.name, hasher);
                        self.emit_reference(field.ty, depth, hasher);
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
                hasher.update([crate::descriptor::TAG_OPAQUE]);
                push_string(library_name, hasher);
                push_string(type_name, hasher);
                hasher.update([version.stability_flag() as u8]);
                kladde_varint::encode(version.leading_nonzero(), hasher);
                kladde_varint::encode(*inline_size, hasher);
                kladde_varint::encode(parameters.len() as u64, hasher);
                for parameter in parameters {
                    self.emit_reference(*parameter, depth, hasher);
                }
            }
        }
    }

    /// Emits the reference token for an edge from a node at `parent_depth`
    /// to `child` (§4.5 item 2): an inline child fingerprint for a white
    /// (recurse) or black (memo hit) edge, or a de Bruijn back-reference for
    /// a gray (already-on-stack) edge.
    fn emit_reference(&mut self, child: TypeRef, parent_depth: usize, hasher: &mut Sha256Hasher) {
        let child = child.0;
        match self.color[child] {
            Color::White => {
                let child_fp = self.visit(child, parent_depth + 1);
                hasher.update([0x00]);
                hasher.update(child_fp.as_bytes());
            }
            Color::Black => {
                let child_fp = self.memo[child].unwrap();
                hasher.update([0x00]);
                hasher.update(child_fp.as_bytes());
            }
            Color::Gray => {
                hasher.update([0x01]);
                let back = parent_depth - self.depth[child];
                kladde_varint::encode(back as u64, hasher);
            }
        }
    }
}

fn push_string(s: &str, hasher: &mut Sha256Hasher) {
    kladde_varint::encode(s.len() as u64, hasher);
    hasher.update(s.as_bytes());
}

#[cfg(test)]
mod tests {
    use crate::descriptor::{
        Field, Primitive, TypeDescriptor, TypeRef, TypeTable, Variant, Version,
    };

    fn primitive(p: Primitive) -> TypeDescriptor {
        TypeDescriptor::Primitive(p)
    }

    fn field(name: &str, ty: usize) -> Field {
        Field {
            name: name.into(),
            ty: TypeRef(ty),
        }
    }

    fn strukt(name: &str, fields: Vec<Field>) -> TypeDescriptor {
        TypeDescriptor::Struct {
            name: name.into(),
            fields,
        }
    }

    /// `struct A { b: B }`, `struct B { a: A }` — a two-node cycle.
    fn mutual_pair() -> TypeTable {
        TypeTable::new(vec![
            strukt("A", vec![field("b", 1)]),
            strukt("B", vec![field("a", 0)]),
        ])
    }

    /// `enum Rec { Stop, Go(Rec) }` — a self-recursive enum.
    fn rec_enum() -> TypeTable {
        TypeTable::new(vec![TypeDescriptor::Enum {
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
                    fields: vec![field("0", 0)],
                },
            ],
        }])
    }

    /// An outer cycle — either a self-referential struct or a mutual
    /// `A <-> B` pair — one of whose members also references a non-trivial
    /// inner subtree (`Inner { mid: Mid, count: i32 }`, `Mid { a: i32, b:
    /// i32 }`). `inner_first` controls whether the cycle member lists its
    /// inner-subtree field before or after its cycle field, which — since
    /// struct field order is significant — makes each `(mutual, inner_first)`
    /// combination a structurally distinct type graph.
    fn outer_cycle_with_inner(mutual: bool, inner_first: bool) -> TypeTable {
        if mutual {
            let cycle_field = field("b", 1);
            let inner_field = field("inner", 2);
            let a_fields = if inner_first {
                vec![inner_field, cycle_field]
            } else {
                vec![cycle_field, inner_field]
            };
            TypeTable::new(vec![
                strukt("A", a_fields),
                strukt("B", vec![field("a", 0)]),
                strukt("Inner", vec![field("mid", 3), field("count", 4)]),
                strukt("Mid", vec![field("a", 4), field("b", 4)]),
                primitive(Primitive::I32),
            ])
        } else {
            let cycle_field = field("next", 0);
            let inner_field = field("inner", 1);
            let outer_fields = if inner_first {
                vec![inner_field, cycle_field]
            } else {
                vec![cycle_field, inner_field]
            };
            TypeTable::new(vec![
                strukt("Outer", outer_fields),
                strukt("Inner", vec![field("mid", 2), field("count", 3)]),
                strukt("Mid", vec![field("a", 3), field("b", 3)]),
                primitive(Primitive::I32),
            ])
        }
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
            primitive(Primitive::I32),
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
            primitive(Primitive::I32),
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
            primitive(Primitive::I32),
            primitive(Primitive::U64),
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
            primitive(Primitive::U64),
            primitive(Primitive::I32),
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
            primitive(Primitive::I32),
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
            primitive(Primitive::I32),
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
            primitive(Primitive::I64),
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
            primitive(Primitive::I32),
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
    fn recursion_fingerprints_reproducibly() {
        // A cyclic graph must terminate and hash deterministically (the de
        // Bruijn back-references break the cycle). Recomputing from a fresh
        // all-white state yields the same value.
        for table in [
            list(),
            mutual_pair(),
            rec_enum(),
            outer_cycle_with_inner(false, false),
            outer_cycle_with_inner(true, true),
        ] {
            assert_eq!(table.fingerprint(), table.fingerprint());
        }
    }

    #[test]
    fn fingerprint_is_invariant_to_sharing() {
        // `struct Pair { first: Inner, second: Inner }` where `Inner` is a
        // small struct. Whether the two fields share one `Inner` descriptor
        // or point at two byte-identical ones, the fingerprint is the same:
        // the merkle reuse is by content, not by table position (spec §5(d)).
        let shared = TypeTable::new(vec![
            strukt("Pair", vec![field("first", 1), field("second", 1)]),
            strukt("Inner", vec![field("a", 2), field("b", 2)]),
            primitive(Primitive::I32),
        ]);
        let duplicated = TypeTable::new(vec![
            strukt("Pair", vec![field("first", 1), field("second", 2)]),
            strukt("Inner", vec![field("a", 3), field("b", 3)]),
            strukt("Inner", vec![field("a", 3), field("b", 3)]),
            primitive(Primitive::I32),
        ]);
        assert_eq!(shared.fingerprint(), duplicated.fingerprint());
    }

    #[test]
    fn distinct_structures_have_distinct_fingerprints() {
        // A hash cannot *guarantee* injectivity, but distinct type graphs
        // must not collide by construction (only by an astronomically
        // unlikely hash collision). This guards against an encoding bug that
        // fails to distinguish two structures -- including recursive ones,
        // and ones differing only in the field order of a cycle member.
        let structures = [
            ("point", point()),
            ("list", list()),
            ("mutual_pair", mutual_pair()),
            ("rec_enum", rec_enum()),
            ("selfref_cycle_first", outer_cycle_with_inner(false, false)),
            ("selfref_inner_first", outer_cycle_with_inner(false, true)),
            ("mutual_cycle_first", outer_cycle_with_inner(true, false)),
            ("mutual_inner_first", outer_cycle_with_inner(true, true)),
        ];
        for (i, (name_i, table_i)) in structures.iter().enumerate() {
            for (name_j, table_j) in &structures[i + 1..] {
                assert_ne!(
                    table_i.fingerprint(),
                    table_j.fingerprint(),
                    "{name_i} and {name_j} must not collide",
                );
            }
        }
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
