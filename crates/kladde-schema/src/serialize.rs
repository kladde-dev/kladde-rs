//! Canonical byte serialization of a [`TypeTable`], per
//! `type-descriptors.md` §3: a `varint(count)` followed by each descriptor
//! in index order, every descriptor a kind-tag byte plus kind-specific
//! content. Byte-for-byte reproducible across implementations.

use crate::descriptor::{
    canonical_variants, Field, Primitive, TypeDescriptor, TypeRef, TypeTable, Variant, Version,
    TAG_ARRAY, TAG_ENUM, TAG_OPAQUE, TAG_POINTER, TAG_STRUCT,
};

/// Why a byte string could not be decoded into a [`TypeTable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The input ended before a value was complete.
    Truncated,
    /// A string field was not valid UTF-8.
    InvalidUtf8,
    /// A kind-tag byte names a kind this revision reserves but does not yet
    /// implement (Array or Pointer).
    ReservedKind(u8),
    /// A kind-tag byte names no known kind.
    UnknownKind(u8),
    /// A discriminant width other than 1, 2, 4, or 8.
    InvalidDiscriminantWidth(u8),
    /// A primitive code with no implemented primitive (a reserved or future
    /// code, `type-descriptors.md` §2.1.1).
    UnsupportedPrimitive(u8),
    /// A reference points outside the table.
    ReferenceOutOfRange { index: usize, table_len: usize },
    /// The table had no descriptors (a table must have at least a root).
    Empty,
    /// Bytes remained after the last descriptor.
    TrailingBytes,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Truncated => f.write_str("schema is truncated"),
            DecodeError::InvalidUtf8 => f.write_str("schema string is not valid UTF-8"),
            DecodeError::ReservedKind(t) => write!(f, "reserved (unimplemented) kind tag {t}"),
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
    /// The canonical byte serialization of this table (`type-descriptors.md`
    /// §3). Enum variants are emitted in ascending-discriminant order
    /// regardless of how they are stored, so two tables that differ only in
    /// variant order encode identically.
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
    /// in range, and that no trailing bytes remain.
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
        Ok(TypeTable::new(descriptors))
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
                kladde_varint::encode(variant.fields.len() as u64, out);
                for field in &variant.fields {
                    encode_field(field, out);
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
            out.push(TAG_OPAQUE);
            encode_string(library_name, out);
            encode_string(type_name, out);
            kladde_varint::encode(version.major, out);
            kladde_varint::encode(version.minor, out);
            kladde_varint::encode(version.patch, out);
            kladde_varint::encode(*inline_size, out);
            kladde_varint::encode(parameters.len() as u64, out);
            for parameter in parameters {
                encode_reference(*parameter, out);
            }
        }
    }
}

struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    fn varint(&mut self) -> Result<u64, DecodeError> {
        let (value, rest) = kladde_varint::decode(self.data)?;
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
                let name = reader.string()?;
                let fields = reader.fields()?;
                variants.push(Variant {
                    discriminant,
                    name,
                    fields,
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
            let inline_size = reader.varint()?;
            let param_count = reader.varint()? as usize;
            let mut parameters = Vec::with_capacity(param_count);
            for _ in 0..param_count {
                parameters.push(reader.reference()?);
            }
            Ok(TypeDescriptor::Opaque {
                library_name,
                type_name,
                version,
                inline_size,
                parameters,
            })
        }
        TAG_ARRAY | TAG_POINTER => Err(DecodeError::ReservedKind(tag)),
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
                        fields: vec![],
                    },
                    Variant {
                        discriminant: 1,
                        name: "Num".into(),
                        fields: vec![Field {
                            name: "0".into(),
                            ty: TypeRef(1),
                        }],
                    },
                    Variant {
                        discriminant: 2,
                        name: "List".into(),
                        fields: vec![Field {
                            name: "0".into(),
                            ty: TypeRef(2),
                        }],
                    },
                ],
            },
            TypeDescriptor::Primitive(Primitive::I64),
            TypeDescriptor::Opaque {
                library_name: "kladde-types".into(),
                type_name: "PersistedVec".into(),
                version: Version {
                    major: 0,
                    minor: 1,
                    patch: 0,
                },
                inline_size: 8,
                parameters: vec![TypeRef(0)],
            },
        ]);
        let bytes = table.encode();
        assert_eq!(TypeTable::decode(&bytes).unwrap(), table);
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
        let ascending = TypeTable::new(vec![TypeDescriptor::Enum {
            name: "E".into(),
            discriminant_width: 4,
            variants: vec![
                Variant {
                    discriminant: 0,
                    name: "A".into(),
                    fields: vec![],
                },
                Variant {
                    discriminant: 1,
                    name: "B".into(),
                    fields: vec![],
                },
            ],
        }]);
        let descending = TypeTable::new(vec![TypeDescriptor::Enum {
            name: "E".into(),
            discriminant_width: 4,
            variants: vec![
                Variant {
                    discriminant: 1,
                    name: "B".into(),
                    fields: vec![],
                },
                Variant {
                    discriminant: 0,
                    name: "A".into(),
                    fields: vec![],
                },
            ],
        }]);
        assert_eq!(ascending.encode(), descending.encode());
    }

    #[test]
    fn rejects_reserved_kinds() {
        assert_eq!(
            TypeTable::decode(&[1, crate::descriptor::TAG_ARRAY]),
            Err(super::DecodeError::ReservedKind(
                crate::descriptor::TAG_ARRAY
            ))
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
