//! Canonical byte serialization of a [`TypeTable`]: a `varint(count)`
//! followed by each descriptor in index order, every descriptor a kind-tag
//! byte plus kind-specific content. Byte-for-byte reproducible across
//! implementations.

use crate::descriptor::{
    canonical_variants, Field, Primitive, TypeDescriptor, TypeRef, TypeTable, Variant, Version,
    TAG_ARRAY, TAG_ENUM, TAG_FIXED, TAG_MAP, TAG_OPAQUE, TAG_SEQUENCE, TAG_STRUCT, TAG_TEXT,
};

/// Why a byte string could not be decoded into a [`TypeTable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The input ended before a value was complete.
    Truncated,
    /// A string field was not valid UTF-8.
    InvalidUtf8,
    /// A kind-tag byte names no known kind.
    UnknownKind(u8),
    /// A discriminant width other than 1, 2, 4, or 8.
    InvalidDiscriminantWidth(u8),
    /// A primitive code with no implemented primitive (a reserved or future
    /// code).
    UnsupportedPrimitive(u8),
    /// A reference points outside the table.
    ReferenceOutOfRange { index: usize, table_len: usize },
    /// The table had no descriptors (a table must have at least a root).
    Empty,
    /// Bytes remained after the last descriptor.
    TrailingBytes,
    /// The bytes decode, but are not the canonical encoding of what they
    /// decode to: a varint longer than it needs to be, or enum variants out
    /// of ascending discriminant order or sharing one.
    NotCanonical(&'static str),
    /// Descriptor `index` breaks a rule of nesting: a `Fixed` wrapper around
    /// a type without a fixed form, or a type that reaches itself without
    /// passing through an enum or a sequence. See
    /// [`TypeTable::validate`].
    InvalidNesting { index: usize, reason: &'static str },
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Truncated => f.write_str("schema is truncated"),
            DecodeError::InvalidUtf8 => f.write_str("schema string is not valid UTF-8"),
            DecodeError::UnknownKind(t) => write!(f, "unknown kind tag {t}"),
            DecodeError::InvalidDiscriminantWidth(w) => {
                write!(f, "invalid discriminant width {w} (expected 1, 2, 4, or 8)")
            }
            DecodeError::UnsupportedPrimitive(c) => {
                write!(f, "unsupported primitive code {c}")
            }
            DecodeError::ReferenceOutOfRange { index, table_len } => {
                write!(f, "reference {index} out of range for table of {table_len}")
            }
            DecodeError::Empty => f.write_str("schema has no descriptors"),
            DecodeError::TrailingBytes => f.write_str("trailing bytes after schema"),
            DecodeError::NotCanonical(what) => write!(f, "schema is not canonical: {what}"),
            DecodeError::InvalidNesting { index, reason } => {
                write!(f, "descriptor {index}: {reason}")
            }
        }
    }
}

impl std::error::Error for DecodeError {}

impl From<kladde_varint::Error> for DecodeError {
    fn from(_: kladde_varint::Error) -> Self {
        // Both varint failures (truncation and overflow) mean the byte
        // stream did not hold a value we could read where one was expected.
        DecodeError::Truncated
    }
}

impl TypeTable {
    /// The canonical byte serialization of this table. Enum variants are
    /// emitted in ascending-discriminant order regardless of how they are
    /// stored, so two tables that differ only in variant order encode
    /// identically.
    ///
    /// ```
    /// use kladde_schema::{TypeDescriptor, TypeRef, TypeTable};
    ///
    /// let table = TypeTable::new(vec![TypeDescriptor::Sequence(TypeRef(1)), TypeDescriptor::Text]);
    /// assert_eq!(table.encode(), [2, 133, 1, 134]);
    /// assert_eq!(TypeTable::decode(&table.encode()), Ok(table));
    /// ```
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        kladde_varint::encode(self.descriptors().len() as u64, &mut out);
        for descriptor in self.descriptors() {
            encode_descriptor(descriptor, &mut out);
        }
        out
    }

    /// Decodes a table from bytes produced by [`encode`](TypeTable::encode).
    /// Validates that the table is non-empty, that every reference is
    /// in range, that no trailing bytes remain, and that the table keeps
    /// the [rules of nesting](TypeTable::validate).
    pub fn decode(input: &[u8]) -> Result<TypeTable, DecodeError> {
        let mut reader = Reader { data: input };
        let count = reader.varint()? as usize;
        let mut descriptors = Vec::with_capacity(count);
        for _ in 0..count {
            descriptors.push(decode_descriptor(&mut reader)?);
        }
        if !reader.data.is_empty() {
            return Err(DecodeError::TrailingBytes);
        }
        if descriptors.is_empty() {
            return Err(DecodeError::Empty);
        }
        for descriptor in &descriptors {
            for reference in descriptor.references() {
                if reference.0 >= descriptors.len() {
                    return Err(DecodeError::ReferenceOutOfRange {
                        index: reference.0,
                        table_len: descriptors.len(),
                    });
                }
            }
        }
        let table = TypeTable::new(descriptors);
        table.validate()?;
        Ok(table)
    }
}

fn encode_string(s: &str, out: &mut Vec<u8>) {
    kladde_varint::encode(s.len() as u64, out);
    out.extend_from_slice(s.as_bytes());
}

fn encode_reference(reference: TypeRef, out: &mut Vec<u8>) {
    kladde_varint::encode(reference.0 as u64, out);
}

fn encode_field(field: &Field, out: &mut Vec<u8>) {
    encode_string(&field.name, out);
    encode_reference(field.ty, out);
}

fn encode_descriptor(descriptor: &TypeDescriptor, out: &mut Vec<u8>) {
    match descriptor {
        TypeDescriptor::Primitive(primitive) => out.push(primitive.code()),
        TypeDescriptor::Struct { name, fields } => {
            out.push(TAG_STRUCT);
            encode_string(name, out);
            kladde_varint::encode(fields.len() as u64, out);
            for field in fields {
                encode_field(field, out);
            }
        }
        TypeDescriptor::Enum {
            name,
            discriminant_width,
            variants,
        } => {
            out.push(TAG_ENUM);
            encode_string(name, out);
            out.push(*discriminant_width);
            kladde_varint::encode(variants.len() as u64, out);
            for variant in canonical_variants(variants) {
                kladde_varint::encode(variant.discriminant, out);
                encode_string(&variant.name, out);
                encode_reference(variant.payload, out);
            }
        }
        TypeDescriptor::Opaque {
            library_name,
            type_name,
            version,
            fixed_size,
            parameters,
        } => {
            out.push(TAG_OPAQUE);
            encode_string(library_name, out);
            encode_string(type_name, out);
            kladde_varint::encode(version.major, out);
            kladde_varint::encode(version.minor, out);
            kladde_varint::encode(version.patch, out);
            kladde_varint::encode(encode_size_mode(*fixed_size), out);
            kladde_varint::encode(parameters.len() as u64, out);
            for parameter in parameters {
                encode_reference(*parameter, out);
            }
        }
        TypeDescriptor::Array { element, len } => {
            out.push(TAG_ARRAY);
            encode_reference(*element, out);
            kladde_varint::encode(*len, out);
        }
        TypeDescriptor::Sequence(element) => {
            out.push(TAG_SEQUENCE);
            encode_reference(*element, out);
        }
        TypeDescriptor::Map { key, value } => {
            out.push(TAG_MAP);
            encode_reference(*key, out);
            encode_reference(*value, out);
        }
        TypeDescriptor::Text => out.push(TAG_TEXT),
        TypeDescriptor::Fixed(target) => {
            out.push(TAG_FIXED);
            encode_reference(*target, out);
        }
    }
}

/// An opaque type's size mode as one varint: 0 for a length-prefixed value,
/// and one more than the size for a fixed one.
pub(crate) fn encode_size_mode(fixed_size: Option<u64>) -> u64 {
    fixed_size.map_or(0, |n| n + 1)
}

struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    fn varint(&mut self) -> Result<u64, DecodeError> {
        let (value, rest) = match kladde_varint::decode_minimal(self.data) {
            Err(kladde_varint::Error::Overlong) => {
                return Err(DecodeError::NotCanonical(
                    "a varint longer than it needs to be",
                ))
            }
            r => r?,
        };
        self.data = rest;
        Ok(value)
    }

    fn byte(&mut self) -> Result<u8, DecodeError> {
        let (&first, rest) = self.data.split_first().ok_or(DecodeError::Truncated)?;
        self.data = rest;
        Ok(first)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.data.len() < n {
            return Err(DecodeError::Truncated);
        }
        let (head, tail) = self.data.split_at(n);
        self.data = tail;
        Ok(head)
    }

    fn string(&mut self) -> Result<String, DecodeError> {
        let len = self.varint()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| DecodeError::InvalidUtf8)
    }

    fn reference(&mut self) -> Result<TypeRef, DecodeError> {
        Ok(TypeRef(self.varint()? as usize))
    }

    fn fields(&mut self) -> Result<Vec<Field>, DecodeError> {
        let count = self.varint()? as usize;
        let mut fields = Vec::with_capacity(count);
        for _ in 0..count {
            let name = self.string()?;
            let ty = self.reference()?;
            fields.push(Field { name, ty });
        }
        Ok(fields)
    }
}

fn decode_descriptor(reader: &mut Reader) -> Result<TypeDescriptor, DecodeError> {
    let tag = reader.byte()?;
    match tag {
        0..=127 => Primitive::from_code(tag)
            .map(TypeDescriptor::Primitive)
            .ok_or(DecodeError::UnsupportedPrimitive(tag)),
        TAG_STRUCT => {
            let name = reader.string()?;
            let fields = reader.fields()?;
            Ok(TypeDescriptor::Struct { name, fields })
        }
        TAG_ENUM => {
            let name = reader.string()?;
            let discriminant_width = reader.byte()?;
            if !matches!(discriminant_width, 1 | 2 | 4 | 8) {
                return Err(DecodeError::InvalidDiscriminantWidth(discriminant_width));
            }
            let variant_count = reader.varint()? as usize;
            let mut variants = Vec::with_capacity(variant_count);
            for _ in 0..variant_count {
                let discriminant = reader.varint()?;
                if variants
                    .last()
                    .is_some_and(|v: &Variant| v.discriminant >= discriminant)
                {
                    return Err(DecodeError::NotCanonical(
                        "enum variants out of ascending discriminant order",
                    ));
                }
                let name = reader.string()?;
                let payload = reader.reference()?;
                variants.push(Variant {
                    discriminant,
                    name,
                    payload,
                });
            }
            Ok(TypeDescriptor::Enum {
                name,
                discriminant_width,
                variants,
            })
        }
        TAG_OPAQUE => {
            let library_name = reader.string()?;
            let type_name = reader.string()?;
            let version = Version {
                major: reader.varint()?,
                minor: reader.varint()?,
                patch: reader.varint()?,
            };
            let fixed_size = reader.varint()?.checked_sub(1);
            let param_count = reader.varint()? as usize;
            let mut parameters = Vec::with_capacity(param_count.min(reader.data.len()));
            for _ in 0..param_count {
                parameters.push(reader.reference()?);
            }
            Ok(TypeDescriptor::Opaque {
                library_name,
                type_name,
                version,
                fixed_size,
                parameters,
            })
        }
        TAG_ARRAY => Ok(TypeDescriptor::Array {
            element: reader.reference()?,
            len: reader.varint()?,
        }),
        TAG_SEQUENCE => Ok(TypeDescriptor::Sequence(reader.reference()?)),
        TAG_MAP => Ok(TypeDescriptor::Map {
            key: reader.reference()?,
            value: reader.reference()?,
        }),
        TAG_TEXT => Ok(TypeDescriptor::Text),
        TAG_FIXED => Ok(TypeDescriptor::Fixed(reader.reference()?)),
        other => Err(DecodeError::UnknownKind(other)),
    }
}

#[cfg(test)]
mod tests {
    use crate::descriptor::{
        Field, Primitive, TypeDescriptor, TypeRef, TypeTable, Variant, Version,
    };

    /// `struct Point { x: i32, y: i32 }` over shared `i32` (code 6).
    fn point_table() -> TypeTable {
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
            TypeDescriptor::Primitive(Primitive::I32),
        ])
    }

    #[test]
    fn round_trips_every_kind() {
        let table = TypeTable::new(vec![
            TypeDescriptor::Enum {
                name: "Value".into(),
                discriminant_width: 4,
                variants: vec![
                    Variant {
                        discriminant: 0,
                        name: "Nil".into(),
                        payload: TypeRef(3),
                    },
                    Variant {
                        discriminant: 1,
                        name: "Num".into(),
                        payload: TypeRef(1),
                    },
                    Variant {
                        discriminant: 2,
                        name: "List".into(),
                        payload: TypeRef(2),
                    },
                    Variant {
                        discriminant: 3,
                        name: "Dict".into(),
                        payload: TypeRef(4),
                    },
                ],
            },
            TypeDescriptor::Primitive(Primitive::I64),
            TypeDescriptor::Opaque {
                library_name: "kladde-types".into(),
                type_name: "DurableVec".into(),
                version: Version {
                    major: 0,
                    minor: 1,
                    patch: 0,
                },
                fixed_size: None,
                parameters: vec![TypeRef(0)],
            },
            TypeDescriptor::Struct {
                name: "Unit".into(),
                fields: vec![],
            },
            TypeDescriptor::Map {
                key: TypeRef(5),
                value: TypeRef(0),
            },
            TypeDescriptor::Text,
        ]);
        let bytes = table.encode();
        assert_eq!(TypeTable::decode(&bytes).unwrap(), table);
    }

    #[test]
    fn a_map_is_its_tag_and_two_references() {
        let table = TypeTable::new(vec![
            TypeDescriptor::Map {
                key: TypeRef(1),
                value: TypeRef(2),
            },
            TypeDescriptor::Text,
            TypeDescriptor::Primitive(Primitive::U32),
        ]);
        assert_eq!(table.encode(), [3, 132, 1, 2, 134, 2]);
        assert_eq!(TypeTable::decode(&table.encode()).unwrap(), table);
    }

    #[test]
    fn round_trips_the_structural_kinds() {
        // A struct holding a name, a list of counts, a fixed-size count, and
        // an array of three bytes.
        let table = TypeTable::new(vec![
            TypeDescriptor::Struct {
                name: "Named".into(),
                fields: vec![
                    Field {
                        name: "name".into(),
                        ty: TypeRef(1),
                    },
                    Field {
                        name: "counts".into(),
                        ty: TypeRef(2),
                    },
                    Field {
                        name: "total".into(),
                        ty: TypeRef(4),
                    },
                    Field {
                        name: "rgb".into(),
                        ty: TypeRef(5),
                    },
                ],
            },
            TypeDescriptor::Text,
            TypeDescriptor::Sequence(TypeRef(3)),
            TypeDescriptor::Primitive(Primitive::U32),
            TypeDescriptor::Fixed(TypeRef(3)),
            TypeDescriptor::Array {
                element: TypeRef(6),
                len: 3,
            },
            TypeDescriptor::Primitive(Primitive::U8),
        ]);
        let bytes = table.encode();
        // Text, Sequence, u32, Fixed, Array, u8: each a tag and its
        // references, and the array its length.
        assert_eq!(
            &bytes[bytes.len() - 10..],
            [134, 133, 3, 2, 135, 3, 131, 6, 3, 0]
        );
        assert_eq!(TypeTable::decode(&bytes).unwrap(), table);
    }

    #[test]
    fn round_trips_both_size_modes_of_opaque_types() {
        for fixed_size in [None, Some(0), Some(16)] {
            let table = TypeTable::new(vec![TypeDescriptor::Opaque {
                library_name: "lib".into(),
                type_name: "T".into(),
                version: Version {
                    major: 1,
                    minor: 0,
                    patch: 0,
                },
                fixed_size,
                parameters: vec![],
            }]);
            assert_eq!(TypeTable::decode(&table.encode()).unwrap(), table);
        }
    }

    #[test]
    fn point_has_the_expected_bytes() {
        // count=2; [tag=128, string "Point", 2 fields, "x"->1, "y"->1]; [6].
        let expected: Vec<u8> = vec![
            2, // descriptor count
            128, 5, b'P', b'o', b'i', b'n', b't', 2, // Struct "Point", 2 fields
            1, b'x', 1, // field "x" -> ref 1
            1, b'y', 1, // field "y" -> ref 1
            6, // Primitive i32
        ];
        assert_eq!(point_table().encode(), expected);
    }

    #[test]
    fn variant_order_does_not_affect_encoding() {
        let unit = TypeDescriptor::Struct {
            name: "Unit".into(),
            fields: vec![],
        };
        let ascending = TypeTable::new(vec![
            TypeDescriptor::Enum {
                name: "E".into(),
                discriminant_width: 4,
                variants: vec![
                    Variant {
                        discriminant: 0,
                        name: "A".into(),
                        payload: TypeRef(1),
                    },
                    Variant {
                        discriminant: 1,
                        name: "B".into(),
                        payload: TypeRef(1),
                    },
                ],
            },
            unit.clone(),
        ]);
        let descending = TypeTable::new(vec![
            TypeDescriptor::Enum {
                name: "E".into(),
                discriminant_width: 4,
                variants: vec![
                    Variant {
                        discriminant: 1,
                        name: "B".into(),
                        payload: TypeRef(1),
                    },
                    Variant {
                        discriminant: 0,
                        name: "A".into(),
                        payload: TypeRef(1),
                    },
                ],
            },
            unit,
        ]);
        assert_eq!(ascending.encode(), descending.encode());
    }

    #[test]
    fn rejects_what_is_not_canonical() {
        // One Text descriptor, its count as a two-byte varint.
        assert!(matches!(
            TypeTable::decode(&[0x81, 0x00, 134]),
            Err(super::DecodeError::NotCanonical(_))
        ));
        // An enum whose variants come in descending order, then twice the
        // same discriminant; each holds the unit struct at [1].
        let enum_of = |d0: u8, d1: u8| {
            vec![
                2, 129, 1, b'E', 1, 2, d0, 1, b'A', 1, d1, 1, b'B', 1, 128, 0, 0,
            ]
        };
        assert!(TypeTable::decode(&enum_of(0, 1)).is_ok());
        assert!(matches!(
            TypeTable::decode(&enum_of(1, 0)),
            Err(super::DecodeError::NotCanonical(_))
        ));
        assert!(matches!(
            TypeTable::decode(&enum_of(1, 1)),
            Err(super::DecodeError::NotCanonical(_))
        ));
    }

    #[test]
    fn rejects_unknown_kinds() {
        assert_eq!(
            TypeTable::decode(&[1, 136]),
            Err(super::DecodeError::UnknownKind(136))
        );
    }

    #[test]
    fn rejects_out_of_range_reference() {
        // One Struct whose field points at index 5, which does not exist.
        let table = TypeTable::new(vec![TypeDescriptor::Struct {
            name: "S".into(),
            fields: vec![Field {
                name: "f".into(),
                ty: TypeRef(5),
            }],
        }]);
        let bytes = table.encode();
        assert!(matches!(
            TypeTable::decode(&bytes),
            Err(super::DecodeError::ReferenceOutOfRange { .. })
        ));
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = point_table().encode();
        bytes.push(0xff);
        assert_eq!(
            TypeTable::decode(&bytes),
            Err(super::DecodeError::TrailingBytes)
        );
    }
}
