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

    /// Fingerprints of every node reached in a single traversal from the
    /// root, indexed by descriptor position (`None` for unreachable nodes).
    /// A test hook for asserting the cyclic flag of *nested* nodes, which
    /// the root-only [`fingerprint`](TypeTable::fingerprint) cannot expose.
    #[cfg(test)]
    pub(crate) fn node_fingerprints(&self) -> Vec<Option<Fingerprint>> {
        let mut traversal = Traversal::new(self);
        traversal.visit(self.root().0, 0);
        traversal.memo
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
            TypeDescriptor::Primitive(primitive) => input.push(primitive.code()),
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
    /// `A <-> B` pair — one of whose members also references a *non-trivial,
    /// non-cyclic* inner subtree (`Inner { mid: Mid, count: i32 }`,
    /// `Mid { a: i32, b: i32 }`). `inner_first` controls whether the cycle
    /// member visits its inner-subtree field before or after its cycle
    /// field. Returns the table plus the indices of the cycle nodes and of
    /// the inner-subtree nodes.
    fn outer_cycle_with_inner(
        mutual: bool,
        inner_first: bool,
    ) -> (TypeTable, Vec<usize>, Vec<usize>) {
        if mutual {
            // 0:A, 1:B, 2:Inner, 3:Mid, 4:i32.
            let cycle_field = field("b", 1);
            let inner_field = field("inner", 2);
            let a_fields = if inner_first {
                vec![inner_field, cycle_field]
            } else {
                vec![cycle_field, inner_field]
            };
            let table = TypeTable::new(vec![
                strukt("A", a_fields),
                strukt("B", vec![field("a", 0)]),
                strukt("Inner", vec![field("mid", 3), field("count", 4)]),
                strukt("Mid", vec![field("a", 4), field("b", 4)]),
                primitive(Primitive::I32),
            ]);
            (table, vec![0, 1], vec![2, 3])
        } else {
            // 0:Outer, 1:Inner, 2:Mid, 3:i32.
            let cycle_field = field("next", 0);
            let inner_field = field("inner", 1);
            let outer_fields = if inner_first {
                vec![inner_field, cycle_field]
            } else {
                vec![cycle_field, inner_field]
            };
            let table = TypeTable::new(vec![
                strukt("Outer", outer_fields),
                strukt("Inner", vec![field("mid", 2), field("count", 3)]),
                strukt("Mid", vec![field("a", 3), field("b", 3)]),
                primitive(Primitive::I32),
            ]);
            (table, vec![0], vec![1, 2])
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
    fn recursion_sets_the_flag() {
        assert!(list().fingerprint().is_cyclic());
        assert!(!point().fingerprint().is_cyclic());
        assert!(mutual_pair().fingerprint().is_cyclic());
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
        assert!(rec_enum().fingerprint().is_cyclic());
    }

    #[test]
    fn cyclic_outer_leaves_inner_subtree_noncyclic() {
        // The dual of `contains_a_cycle_but_is_not_on_one`: an outer cycle
        // that references a non-cyclic subtree must not contaminate that
        // subtree's flag. Exercised across a 2x2 matrix -- self-referential
        // vs. mutual outer cycle, cycle field visited first vs. inner
        // subtree first -- since a lowlink bug could depend on either axis.
        for mutual in [false, true] {
            for inner_first in [false, true] {
                let (table, cycle_nodes, inner_nodes) = outer_cycle_with_inner(mutual, inner_first);
                let label = format!("mutual={mutual}, inner_first={inner_first}");
                let fingerprints = table.node_fingerprints();

                // The root is on the cycle, so it is flagged cyclic.
                assert!(table.fingerprint().is_cyclic(), "{label}: root");
                // Every node on the outer cycle is cyclic...
                for &node in &cycle_nodes {
                    assert!(
                        fingerprints[node].unwrap().is_cyclic(),
                        "{label}: cycle node {node} should be cyclic",
                    );
                }
                // ...while every node of the inner non-cyclic subtree is not,
                // even though it was discovered inside the cyclic traversal.
                for &node in &inner_nodes {
                    assert!(
                        !fingerprints[node].unwrap().is_cyclic(),
                        "{label}: inner node {node} should be non-cyclic",
                    );
                }
            }
        }
    }

    #[test]
    fn distinct_cyclic_structures_have_distinct_fingerprints() {
        // A hash cannot *guarantee* injectivity, but distinct type graphs
        // must not collide by construction (only by an astronomically
        // unlikely hash collision). This guards against an encoding bug that
        // fails to distinguish two different cyclic structures -- including
        // ones differing only in the field order of a cycle member.
        let (selfref_cycle_first, ..) = outer_cycle_with_inner(false, false);
        let (selfref_inner_first, ..) = outer_cycle_with_inner(false, true);
        let (mutual_cycle_first, ..) = outer_cycle_with_inner(true, false);
        let (mutual_inner_first, ..) = outer_cycle_with_inner(true, true);
        let structures = [
            ("list", list()),
            ("mutual_pair", mutual_pair()),
            ("rec_enum", rec_enum()),
            ("selfref_cycle_first", selfref_cycle_first),
            ("selfref_inner_first", selfref_inner_first),
            ("mutual_cycle_first", mutual_cycle_first),
            ("mutual_inner_first", mutual_inner_first),
        ];

        // Every structure here is genuinely cyclic.
        for (name, table) in &structures {
            assert!(table.fingerprint().is_cyclic(), "{name} should be cyclic");
        }
        // ...and all of their fingerprints are pairwise distinct.
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
