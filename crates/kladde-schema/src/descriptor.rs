//! The type-descriptor model of `type-descriptors.md` §2: an in-memory
//! description of how a value type lays out and interprets its bytes.

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

/// One named field of a [`TypeDescriptor::Struct`] or of an [`Enum`]
/// variant. A tuple (positional) field's `name` is the decimal rendering
/// of its position (`"0"`, `"1"`, …).
///
/// [`Enum`]: TypeDescriptor::Enum
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub ty: TypeRef,
}

/// One variant of a [`TypeDescriptor::Enum`]: the discriminant value that
/// selects it in the stored bytes, its name, and its own fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    pub discriminant: u64,
    pub name: String,
    pub fields: Vec<Field>,
}

/// A fixed-width scalar primitive — one of the implemented primitive codes
/// of `type-descriptors.md` §2.1. The `#[repr(u8)]` discriminant *is* the
/// on-disk code, so [`code`](Primitive::code) / [`from_code`] round-trip to
/// and from the wire byte.
///
/// Codes the spec reserves but does not yet implement (§2.1.1, e.g. `u128`)
/// are deliberately absent: a schema that uses one fails to decode here
/// until a future revision adds the corresponding variant.
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
}

/// One node of the type graph: a description of a single value type's
/// on-disk representation. See `type-descriptors.md` §2 for the model and
/// each kind's meaning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeDescriptor {
    /// A fixed-width scalar (`type-descriptors.md` §2.1).
    Primitive(Primitive),
    /// An ordered set of named fields laid out consecutively, no
    /// discriminant and no header.
    Struct { name: String, fields: Vec<Field> },
    /// A discriminated union: a `discriminant_width`-byte discriminant
    /// followed by the fields of the selected variant.
    Enum {
        name: String,
        discriminant_width: u8,
        variants: Vec<Variant>,
    },
    /// A type whose internal structure this format does not decompose,
    /// identified nominally by library, name, and version.
    Opaque {
        library_name: String,
        type_name: String,
        version: Version,
        inline_size: u64,
        parameters: Vec<TypeRef>,
    },
}

/// Kind-tag byte for [`TypeDescriptor::Struct`] (`type-descriptors.md` §3.2).
pub const TAG_STRUCT: u8 = 128;
/// Kind-tag byte for [`TypeDescriptor::Enum`].
pub const TAG_ENUM: u8 = 129;
/// Kind-tag byte for [`TypeDescriptor::Opaque`].
pub const TAG_OPAQUE: u8 = 130;
/// Reserved kind-tag byte for the not-yet-implemented Array kind.
pub const TAG_ARRAY: u8 = 131;
/// Reserved kind-tag byte for the not-yet-defined Pointer kind.
pub const TAG_POINTER: u8 = 132;

impl TypeDescriptor {
    /// This descriptor's outgoing references, in canonical
    /// (`type-descriptors.md` §3.2) order — struct/variant fields in
    /// declaration order, enum variants in ascending discriminant order,
    /// Opaque parameters as given. Used by both serialization and
    /// fingerprinting so they always agree on traversal order.
    pub(crate) fn references(&self) -> Vec<TypeRef> {
        match self {
            TypeDescriptor::Primitive(_) => Vec::new(),
            TypeDescriptor::Struct { fields, .. } => fields.iter().map(|f| f.ty).collect(),
            TypeDescriptor::Enum { variants, .. } => canonical_variants(variants)
                .into_iter()
                .flat_map(|v| v.fields.iter().map(|f| f.ty))
                .collect(),
            TypeDescriptor::Opaque { parameters, .. } => parameters.clone(),
        }
    }
}

/// A type's descriptors plus the convention that **index 0 is the root** —
/// the type actually being described (`type-descriptors.md` §2.6).
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
