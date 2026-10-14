//! The type-descriptor model: an in-memory description of how a value type
//! encodes into the sequence, and what its bytes mean.

/// A reference from one descriptor to another: an index into the
/// [`TypeTable`] it belongs to. Indices are an artifact of one particular
/// table and carry no meaning across tables (and, deliberately, do not
/// affect a type's fingerprint).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeRef(pub usize);

/// A `(major, minor, patch)` semantic version, carried by an
/// [`TypeDescriptor::Opaque`] to describe the library that defines the
/// type. Only its *compatibility component* participates in a fingerprint
/// (see [`Version::stability_flag`] / [`Version::leading_nonzero`] and
/// `type-descriptors.md` §2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// `true` in the `1.x`-and-up "stable" regime, `false` in the `0.x`
    /// "unstable" regime. Fingerprinted alongside [`leading_nonzero`] so
    /// that, e.g., `0.1.z` and `1.y.z` never collide.
    ///
    /// [`leading_nonzero`]: Version::leading_nonzero
    pub fn stability_flag(&self) -> bool {
        self.major > 0
    }

    /// The leading nonzero version component under the semantic-versioning
    /// convention: `major` when `major > 0`, otherwise `minor`. Two
    /// versions that agree on this value and on [`stability_flag`] are
    /// treated as representation-compatible.
    ///
    /// [`stability_flag`]: Version::stability_flag
    pub fn leading_nonzero(&self) -> u64 {
        if self.major > 0 {
            self.major
        } else {
            self.minor
        }
    }
}

/// One named field of a [`TypeDescriptor::Struct`]. A tuple (positional)
/// field's `name` is the decimal rendering of its position (`"0"`, `"1"`,
/// …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub ty: TypeRef,
}

/// One variant of a [`TypeDescriptor::Enum`]: the discriminant value that
/// selects it in the stored bytes, its name, and the type of the one value
/// it holds, its payload.
///
/// A variant that holds nothing holds the empty Struct, and one that holds
/// several fields a Struct of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    pub discriminant: u64,
    pub name: String,
    pub payload: TypeRef,
}

/// A scalar primitive. The `#[repr(u8)]` discriminant *is* the descriptor's
/// code, so [`code`](Primitive::code) / [`from_code`] round-trip to and from
/// the wire byte.
///
/// Codes the specification reserves but does not yet implement (e.g.
/// `u128`) are deliberately absent: a schema that uses one fails to decode
/// here until a future revision adds the corresponding variant.
///
/// [`from_code`]: Primitive::from_code
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Primitive {
    U8 = 0,
    U16 = 1,
    U32 = 2,
    U64 = 3,
    I8 = 4,
    I16 = 5,
    I32 = 6,
    I64 = 7,
    F32 = 8,
    F64 = 9,
    Bool = 10,
    Char = 11,
}

impl Primitive {
    /// This primitive's on-disk code byte (`0..=11`).
    pub fn code(self) -> u8 {
        self as u8
    }

    /// The primitive for a code byte, or `None` if no implemented primitive
    /// uses that code.
    pub fn from_code(code: u8) -> Option<Primitive> {
        Some(match code {
            0 => Primitive::U8,
            1 => Primitive::U16,
            2 => Primitive::U32,
            3 => Primitive::U64,
            4 => Primitive::I8,
            5 => Primitive::I16,
            6 => Primitive::I32,
            7 => Primitive::I64,
            8 => Primitive::F32,
            9 => Primitive::F64,
            10 => Primitive::Bool,
            11 => Primitive::Char,
            _ => return None,
        })
    }

    /// How many bytes every value of this primitive encodes to, or `None`
    /// for the varints and `char`, whose size depends on the value.
    ///
    /// ```
    /// use kladde_schema::Primitive;
    ///
    /// assert_eq!(Primitive::F32.fixed_size(), Some(4));
    /// assert_eq!(Primitive::U32.fixed_size(), None);
    /// ```
    pub fn fixed_size(self) -> Option<u64> {
        match self {
            Primitive::U8 | Primitive::I8 | Primitive::Bool => Some(1),
            Primitive::F32 => Some(4),
            Primitive::F64 => Some(8),
            _ => None,
        }
    }

    /// How many bytes this primitive takes inside a
    /// [`Fixed`](TypeDescriptor::Fixed) value: its full width, and four
    /// bytes for a `char`.
    ///
    /// ```
    /// use kladde_schema::Primitive;
    ///
    /// assert_eq!(Primitive::U16.fixed_form_size(), 2);
    /// assert_eq!(Primitive::Char.fixed_form_size(), 4);
    /// ```
    pub fn fixed_form_size(self) -> u64 {
        match self {
            Primitive::U8 | Primitive::I8 | Primitive::Bool => 1,
            Primitive::U16 | Primitive::I16 => 2,
            Primitive::U32 | Primitive::I32 | Primitive::F32 | Primitive::Char => 4,
            Primitive::U64 | Primitive::I64 | Primitive::F64 => 8,
        }
    }
}

/// One node of the type graph: a description of a single value type's
/// encoding. Every type has one encoding, which a reader decodes front to
/// back from its descriptor.
///
/// | kind | encoding |
/// | --- | --- |
/// | `u8`, `i8`, `bool` | one byte |
/// | `u16`, `u32`, `u64` | unsigned LEB128 |
/// | `i16`, `i32`, `i64` | zigzag, then LEB128 |
/// | `f32`, `f64` | IEEE-754, little-endian |
/// | `char` | UTF-8 |
/// | Struct | its fields' encodings, concatenated |
/// | Enum | the discriminant as LEB128, then the variant's payload |
/// | Array | `len` elements |
/// | Sequence | the number of elements as LEB128, then the elements |
/// | Map | the number of entries as LEB128, then each entry's key and value |
/// | Text | UTF-8, then the byte `0xFF` |
/// | Opaque | its bytes, prefixed with their number as LEB128 unless the type declares a fixed size |
/// | Fixed | the wrapped type's values at a fixed size: see [`TypeTable::fixed_form_size`] |
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeDescriptor {
    /// A scalar.
    Primitive(Primitive),
    /// An ordered set of named fields, encoded back to back.
    Struct { name: String, fields: Vec<Field> },
    /// A discriminated union: the discriminant, then the payload of the
    /// variant it selects. `discriminant_width` is the discriminant's width
    /// in bytes inside a [`Fixed`](TypeDescriptor::Fixed) value.
    Enum {
        name: String,
        discriminant_width: u8,
        variants: Vec<Variant>,
    },
    /// A type whose internal structure this format does not decompose,
    /// identified nominally by library, name, and version: `fixed_size`
    /// bytes, or, if it is `None`, a length and that many bytes.
    Opaque {
        library_name: String,
        type_name: String,
        version: Version,
        fixed_size: Option<u64>,
        parameters: Vec<TypeRef>,
    },
    /// `len` values of the element type, back to back.
    Array { element: TypeRef, len: u64 },
    /// A count, then that many values of the element type.
    Sequence(TypeRef),
    /// A count, then that many entries, each a key and a value.
    Map { key: TypeRef, value: TypeRef },
    /// Text: UTF-8, then the byte `0xFF`, which UTF-8 never contains.
    Text,
    /// The wrapped type's values, each encoded at the same size.
    Fixed(TypeRef),
}

/// Kind-tag byte for [`TypeDescriptor::Struct`].
pub const TAG_STRUCT: u8 = 128;
/// Kind-tag byte for [`TypeDescriptor::Enum`].
pub const TAG_ENUM: u8 = 129;
/// Kind-tag byte for [`TypeDescriptor::Opaque`].
pub const TAG_OPAQUE: u8 = 130;
/// Kind-tag byte for [`TypeDescriptor::Array`].
pub const TAG_ARRAY: u8 = 131;
/// Kind-tag byte for [`TypeDescriptor::Map`].
pub const TAG_MAP: u8 = 132;
/// Kind-tag byte for [`TypeDescriptor::Sequence`].
pub const TAG_SEQUENCE: u8 = 133;
/// Kind-tag byte for [`TypeDescriptor::Text`].
pub const TAG_TEXT: u8 = 134;
/// Kind-tag byte for [`TypeDescriptor::Fixed`].
pub const TAG_FIXED: u8 = 135;

impl TypeDescriptor {
    /// This descriptor's outgoing references, in canonical order — struct
    /// fields in declaration order, enum payloads in ascending discriminant
    /// order, a map's key before its value, Opaque parameters as given.
    /// Used by both serialization and fingerprinting so they always agree
    /// on traversal order.
    pub(crate) fn references(&self) -> Vec<TypeRef> {
        match self {
            TypeDescriptor::Primitive(_) | TypeDescriptor::Text => Vec::new(),
            TypeDescriptor::Struct { fields, .. } => fields.iter().map(|f| f.ty).collect(),
            TypeDescriptor::Enum { variants, .. } => canonical_variants(variants)
                .into_iter()
                .map(|v| v.payload)
                .collect(),
            TypeDescriptor::Map { key, value } => vec![*key, *value],
            TypeDescriptor::Opaque { parameters, .. } => parameters.clone(),
            TypeDescriptor::Array { element, .. } => vec![*element],
            TypeDescriptor::Sequence(target) | TypeDescriptor::Fixed(target) => vec![*target],
        }
    }
}

/// A type's descriptors plus the convention that **index 0 is the root** —
/// the type actually being described.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeTable {
    descriptors: Vec<TypeDescriptor>,
}

impl TypeTable {
    /// Builds a table from its descriptors; index 0 is the root. Panics if
    /// `descriptors` is empty (a table always has at least a root).
    pub fn new(descriptors: Vec<TypeDescriptor>) -> Self {
        assert!(
            !descriptors.is_empty(),
            "a TypeTable must have at least a root descriptor"
        );
        TypeTable { descriptors }
    }

    /// All descriptors, in index order (root first).
    pub fn descriptors(&self) -> &[TypeDescriptor] {
        &self.descriptors
    }

    /// The root reference — always index 0.
    pub fn root(&self) -> TypeRef {
        TypeRef(0)
    }

    /// The descriptor a reference points at.
    pub fn get(&self, reference: TypeRef) -> &TypeDescriptor {
        &self.descriptors[reference.0]
    }
}

/// An enum's variants in canonical (ascending-`discriminant`) order, the
/// order both serialization and fingerprinting emit them in.
pub(crate) fn canonical_variants(variants: &[Variant]) -> Vec<&Variant> {
    let mut ordered: Vec<&Variant> = variants.iter().collect();
    ordered.sort_by_key(|v| v.discriminant);
    ordered
}
