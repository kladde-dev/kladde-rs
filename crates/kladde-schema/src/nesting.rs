//! What follows from a descriptor table without a value: which types are
//! fixed-size, how large a `Fixed` value is, the rules of nesting a table
//! keeps, and how long the encoding of a value at the front of some bytes
//! is.

use crate::descriptor::{Primitive, TypeDescriptor, TypeRef, TypeTable};
use crate::serialize::DecodeError;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    Unvisited,
    InProgress,
    Done,
}

impl TypeTable {
    /// How many bytes every value of the type at `reference` encodes to, or
    /// `None` if the size depends on the value: the property that lets a
    /// container find its `i`th element at `i` times the element's size.
    ///
    /// Floats, bytes, `bool`s, and structs and arrays of fixed-size types are
    /// fixed-size, and so is an enum whose discriminants are all below 128
    /// and whose variants all have the same fixed size; varints, `char`s,
    /// text and sequences are not.
    ///
    /// ```
    /// use kladde_schema::{Field, Primitive, TypeDescriptor, TypeRef, TypeTable};
    ///
    /// let point = TypeTable::new(vec![
    ///     TypeDescriptor::Struct {
    ///         name: "Point".into(),
    ///         fields: vec![
    ///             Field { name: "x".into(), ty: TypeRef(1) },
    ///             Field { name: "y".into(), ty: TypeRef(1) },
    ///         ],
    ///     },
    ///     TypeDescriptor::Primitive(Primitive::F32),
    /// ]);
    /// assert_eq!(point.fixed_size(TypeRef(0)), Some(8));
    /// ```
    pub fn fixed_size(&self, reference: TypeRef) -> Option<u64> {
        let mut memo = vec![Memo::Unknown; self.descriptors().len()];
        self.sized(reference.0, false, &mut memo)
    }

    /// How many bytes a value of `Fixed(T)` takes, for the type `T` at
    /// `reference`, or `None` if `T` has no fixed form: if it holds a
    /// sequence, text, an opaque type without a fixed size, or itself.
    ///
    /// The fixed form lays out everything at its largest: every integer at
    /// its full width, little-endian, a `char` as four bytes, every enum's
    /// discriminant at its descriptor's `discriminant_width` and its variant
    /// padded with zeros to the largest, and a struct or array as its
    /// fields or elements in that form.
    ///
    /// ```
    /// use kladde_schema::{Primitive, TypeDescriptor, TypeRef, TypeTable, Variant, Field};
    ///
    /// // `Option<u32>` as an enum with a one-byte discriminant.
    /// let option = TypeTable::new(vec![
    ///     TypeDescriptor::Enum {
    ///         name: "Option".into(),
    ///         discriminant_width: 1,
    ///         variants: vec![
    ///             Variant { discriminant: 0, name: "None".into(), fields: vec![] },
    ///             Variant {
    ///                 discriminant: 1,
    ///                 name: "Some".into(),
    ///                 fields: vec![Field { name: "0".into(), ty: TypeRef(1) }],
    ///             },
    ///         ],
    ///     },
    ///     TypeDescriptor::Primitive(Primitive::U32),
    /// ]);
    /// assert_eq!(option.fixed_size(TypeRef(0)), None);
    /// assert_eq!(option.fixed_form_size(TypeRef(0)), Some(5));
    /// ```
    pub fn fixed_form_size(&self, reference: TypeRef) -> Option<u64> {
        let mut memo = vec![Memo::Unknown; self.descriptors().len()];
        self.sized(reference.0, true, &mut memo)
    }

    /// The fixed size of the type at `index` (its fixed form's, if `form`),
    /// memoized; a type reached again while it is being sized is part of a
    /// cycle, and has none.
    fn sized(&self, index: usize, form: bool, memo: &mut [Memo]) -> Option<u64> {
        match memo[index] {
            Memo::Known(size) => return size,
            Memo::InProgress => return None,
            Memo::Unknown => {}
        }
        memo[index] = Memo::InProgress;
        let size = match &self.descriptors()[index] {
            TypeDescriptor::Primitive(p) if form => Some(p.fixed_form_size()),
            TypeDescriptor::Primitive(p) => p.fixed_size(),
            TypeDescriptor::Struct { fields, .. } => fields
                .iter()
                .try_fold(0u64, |sum, f| Some(sum + self.sized(f.ty.0, form, memo)?)),
            TypeDescriptor::Enum {
                discriminant_width,
                variants,
                ..
            } => {
                let mut sizes = Vec::with_capacity(variants.len());
                for v in variants {
                    let size = v
                        .fields
                        .iter()
                        .try_fold(0u64, |sum, f| Some(sum + self.sized(f.ty.0, form, memo)?));
                    sizes.push(size);
                }
                if variants.is_empty() || sizes.iter().any(Option::is_none) {
                    None
                } else if form {
                    Some(*discriminant_width as u64 + sizes.iter().flatten().max().unwrap())
                } else if variants.iter().all(|v| v.discriminant < 128)
                    && sizes.windows(2).all(|w| w[0] == w[1])
                {
                    Some(1 + sizes[0].unwrap())
                } else {
                    None
                }
            }
            TypeDescriptor::Opaque { fixed_size, .. } => *fixed_size,
            TypeDescriptor::Array { element, len } => {
                Some(self.sized(element.0, form, memo)?.checked_mul(*len)?)
            }
            TypeDescriptor::Sequence(_) | TypeDescriptor::Text => None,
            TypeDescriptor::Fixed(target) => self.sized(target.0, true, memo),
        };
        memo[index] = Memo::Known(size);
        size
    }

    /// Checks the rules of nesting, which [`decode`](TypeTable::decode)
    /// applies to every table it reads:
    ///
    /// - a `Fixed` wrapper holds a type with a
    ///   [fixed form](TypeTable::fixed_form_size);
    /// - a type reaches itself only through an enum or a sequence, whose
    ///   discriminant or count ends the recursion, and never through structs
    ///   and arrays alone, whose values would be infinite.
    ///
    /// ```
    /// use kladde_schema::{Field, TypeDescriptor, TypeRef, TypeTable};
    ///
    /// // A tree whose nodes hold their children inline.
    /// let tree = TypeTable::new(vec![
    ///     TypeDescriptor::Struct {
    ///         name: "Tree".into(),
    ///         fields: vec![
    ///             Field { name: "label".into(), ty: TypeRef(1) },
    ///             Field { name: "children".into(), ty: TypeRef(2) },
    ///         ],
    ///     },
    ///     TypeDescriptor::Text,
    ///     TypeDescriptor::Sequence(TypeRef(0)),
    /// ]);
    /// assert_eq!(tree.validate(), Ok(()));
    ///
    /// // A tree has no fixed form, so `Fixed(Tree)` does not exist.
    /// let fixed_tree = TypeTable::new(vec![
    ///     TypeDescriptor::Fixed(TypeRef(1)),
    ///     TypeDescriptor::Struct {
    ///         name: "Tree".into(),
    ///         fields: vec![Field { name: "children".into(), ty: TypeRef(2) }],
    ///     },
    ///     TypeDescriptor::Sequence(TypeRef(1)),
    /// ]);
    /// assert!(fixed_tree.validate().is_err());
    /// ```
    pub fn validate(&self) -> Result<(), DecodeError> {
        let descriptors = self.descriptors();
        for (index, descriptor) in descriptors.iter().enumerate() {
            if let TypeDescriptor::Fixed(target) = descriptor {
                if self.fixed_form_size(*target).is_none() {
                    return Err(DecodeError::InvalidNesting {
                        index,
                        reason: "a Fixed wrapper holds a type without a fixed form",
                    });
                }
            }
            if let TypeDescriptor::Enum {
                discriminant_width,
                variants,
                ..
            } = descriptor
            {
                let width = *discriminant_width as u32 * 8;
                if width < 64 && variants.iter().any(|v| v.discriminant >> width != 0) {
                    return Err(DecodeError::InvalidNesting {
                        index,
                        reason: "a discriminant does not fit the enum's discriminant width",
                    });
                }
            }
        }
        let mut marks = vec![Mark::Unvisited; descriptors.len()];
        for index in 0..descriptors.len() {
            self.endless_cycle(index, &mut marks)?;
        }
        Ok(())
    }

    /// Refuses a type that reaches itself through structs, arrays and
    /// `Fixed` wrappers alone.
    fn endless_cycle(&self, index: usize, marks: &mut [Mark]) -> Result<(), DecodeError> {
        match marks[index] {
            Mark::Done => return Ok(()),
            Mark::InProgress => {
                return Err(DecodeError::InvalidNesting {
                    index,
                    reason: "a type contains itself without an enum or a sequence between",
                })
            }
            Mark::Unvisited => {}
        }
        marks[index] = Mark::InProgress;
        let inline = match &self.descriptors()[index] {
            TypeDescriptor::Struct { .. }
            | TypeDescriptor::Array { .. }
            | TypeDescriptor::Fixed(_) => self.descriptors()[index].references(),
            _ => Vec::new(),
        };
        for child in inline {
            self.endless_cycle(child.0, marks)?;
        }
        marks[index] = Mark::Done;
        Ok(())
    }

    /// The length of the encoding of one value of the type at `reference`
    /// at the front of `bytes`, or `None` if `bytes` does not start with
    /// one: what a reader of a foreign schema skips, and what a test checks
    /// a writer's encoding against.
    ///
    /// Checks what the encoding itself constrains: discriminants that name
    /// variants, valid UTF-8, minimal varints in range of their type, and
    /// zeros as a `Fixed` value's padding. Takes time linear in the value's
    /// encoded size.
    ///
    /// ```
    /// use kladde_schema::{Primitive, TypeDescriptor, TypeRef, TypeTable};
    ///
    /// let list = TypeTable::new(vec![
    ///     TypeDescriptor::Sequence(TypeRef(1)),
    ///     TypeDescriptor::Primitive(Primitive::U32),
    /// ]);
    /// // Two elements, 1 and 300.
    /// assert_eq!(list.measure(TypeRef(0), &[2, 1, 0xac, 0x02, 99]), Some(4));
    /// assert_eq!(list.measure(TypeRef(0), &[2, 1]), None);
    /// ```
    pub fn measure(&self, reference: TypeRef, bytes: &[u8]) -> Option<usize> {
        let mut at = 0;
        self.skip(reference.0, false, bytes, &mut at)?;
        Some(at)
    }

    /// Advances `at` past one value of the type at `index`, in its fixed form
    /// if `form`.
    fn skip(&self, index: usize, form: bool, bytes: &[u8], at: &mut usize) -> Option<()> {
        match &self.descriptors()[index] {
            TypeDescriptor::Primitive(p) if form => {
                let n = p.fixed_form_size() as usize;
                let raw = take(bytes, at, n)?;
                let mut wide = [0u8; 8];
                wide[..n].copy_from_slice(raw);
                let value = u64::from_le_bytes(wide);
                match p {
                    Primitive::Bool if value > 1 => None,
                    Primitive::Char => char::from_u32(value as u32).map(|_| ()),
                    _ => Some(()),
                }
            }
            TypeDescriptor::Primitive(p) => skip_primitive(*p, bytes, at),
            TypeDescriptor::Struct { fields, .. } => {
                for f in fields {
                    self.skip(f.ty.0, form, bytes, at)?;
                }
                Some(())
            }
            TypeDescriptor::Enum {
                discriminant_width,
                variants,
                ..
            } => {
                let start = *at;
                let discriminant = if form {
                    let n = *discriminant_width as usize;
                    let mut wide = [0u8; 8];
                    wide[..n].copy_from_slice(take(bytes, at, n)?);
                    u64::from_le_bytes(wide)
                } else {
                    varint(bytes, at)?
                };
                let variant = variants.iter().find(|v| v.discriminant == discriminant)?;
                for f in &variant.fields {
                    self.skip(f.ty.0, form, bytes, at)?;
                }
                if form {
                    let size = self.fixed_form_size(TypeRef(index))? as usize;
                    let padding = take(bytes, at, (start + size).checked_sub(*at)?)?;
                    if padding.iter().any(|&b| b != 0) {
                        return None;
                    }
                }
                Some(())
            }
            TypeDescriptor::Opaque { fixed_size, .. } => {
                let n = match fixed_size {
                    Some(n) => *n,
                    None => varint(bytes, at)?,
                };
                take(bytes, at, usize::try_from(n).ok()?).map(|_| ())
            }
            TypeDescriptor::Array { element, len } => {
                for _ in 0..*len {
                    self.skip(element.0, form, bytes, at)?;
                }
                Some(())
            }
            TypeDescriptor::Sequence(element) if !form => {
                let count = varint(bytes, at)?;
                for _ in 0..count {
                    self.skip(element.0, false, bytes, at)?;
                }
                Some(())
            }
            TypeDescriptor::Text if !form => {
                let end = *at + bytes.get(*at..)?.iter().position(|&b| b == 0xFF)?;
                std::str::from_utf8(&bytes[*at..end]).ok()?;
                *at = end + 1;
                Some(())
            }
            TypeDescriptor::Sequence(_) | TypeDescriptor::Text => None,
            TypeDescriptor::Fixed(target) => self.skip(target.0, true, bytes, at),
        }
    }
}

#[derive(Clone, Copy)]
enum Memo {
    Unknown,
    InProgress,
    Known(Option<u64>),
}

fn take<'a>(bytes: &'a [u8], at: &mut usize, n: usize) -> Option<&'a [u8]> {
    let out = bytes.get(*at..at.checked_add(n)?)?;
    *at += n;
    Some(out)
}

/// A minimal LEB128 varint.
fn varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let (value, rest) = kladde_varint::decode(bytes.get(*at..)?).ok()?;
    let n = bytes.len() - *at - rest.len();
    if n != kladde_varint::encoded_len(value) {
        return None;
    }
    *at += n;
    Some(value)
}

fn skip_primitive(p: Primitive, bytes: &[u8], at: &mut usize) -> Option<()> {
    let max = match p {
        Primitive::U8 | Primitive::I8 | Primitive::Bool => {
            let b = *take(bytes, at, 1)?.first()?;
            return (p != Primitive::Bool || b <= 1).then_some(());
        }
        Primitive::F32 => return take(bytes, at, 4).map(|_| ()),
        Primitive::F64 => return take(bytes, at, 8).map(|_| ()),
        Primitive::Char => {
            let first = *bytes.get(*at)?;
            let n = match first {
                0x00..=0x7F => 1,
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF7 => 4,
                _ => return None,
            };
            let raw = take(bytes, at, n)?;
            return std::str::from_utf8(raw).ok().map(|_| ());
        }
        Primitive::U16 | Primitive::I16 => u16::MAX as u64,
        Primitive::U32 | Primitive::I32 => u32::MAX as u64,
        Primitive::U64 | Primitive::I64 => u64::MAX,
    };
    (varint(bytes, at)? <= max).then_some(())
}

#[cfg(test)]
mod tests {
    use crate::descriptor::{Field, Primitive, TypeDescriptor, TypeRef, TypeTable, Variant};
    use crate::serialize::DecodeError;

    fn field(name: &str, ty: usize) -> Field {
        Field {
            name: name.into(),
            ty: TypeRef(ty),
        }
    }

    fn variant(discriminant: u64, fields: Vec<Field>) -> Variant {
        Variant {
            discriminant,
            name: format!("V{discriminant}"),
            fields,
        }
    }

    fn nesting_error(table: &TypeTable) -> &'static str {
        match table.validate() {
            Err(DecodeError::InvalidNesting { reason, .. }) => reason,
            other => panic!("expected a nesting error, got {other:?}"),
        }
    }

    /// `enum Shape { Circle(f32), Rect(f32, f32) }` at `[0]`, `f32` at `[1]`.
    fn shape() -> Vec<TypeDescriptor> {
        vec![
            TypeDescriptor::Enum {
                name: "Shape".into(),
                discriminant_width: 2,
                variants: vec![
                    variant(0, vec![field("0", 1)]),
                    variant(1, vec![field("0", 1), field("1", 1)]),
                ],
            },
            TypeDescriptor::Primitive(Primitive::F32),
        ]
    }

    #[test]
    fn fixed_sizes_follow_the_encoding() {
        let table = TypeTable::new(shape());
        assert_eq!(table.fixed_size(TypeRef(0)), None);
        assert_eq!(table.fixed_form_size(TypeRef(0)), Some(2 + 8));
        // With variants of one size, the enum is fixed-size, its
        // discriminant one byte.
        let mut d = shape();
        d[0] = TypeDescriptor::Enum {
            name: "Pair".into(),
            discriminant_width: 4,
            variants: vec![
                variant(0, vec![field("0", 1)]),
                variant(127, vec![field("0", 1)]),
            ],
        };
        assert_eq!(TypeTable::new(d.clone()).fixed_size(TypeRef(0)), Some(5));
        // A discriminant of 128 takes two bytes, so the size depends on it.
        if let TypeDescriptor::Enum { variants, .. } = &mut d[0] {
            variants[1].discriminant = 128;
        }
        assert_eq!(TypeTable::new(d).fixed_size(TypeRef(0)), None);
    }

    #[test]
    fn a_fixed_wrapper_needs_a_fixed_form() {
        let table = TypeTable::new(vec![
            TypeDescriptor::Fixed(TypeRef(1)),
            TypeDescriptor::Sequence(TypeRef(2)),
            TypeDescriptor::Primitive(Primitive::U8),
        ]);
        assert_eq!(
            nesting_error(&table),
            "a Fixed wrapper holds a type without a fixed form"
        );
        let table = TypeTable::new(vec![
            TypeDescriptor::Fixed(TypeRef(1)),
            TypeDescriptor::Primitive(Primitive::U64),
        ]);
        assert_eq!(table.validate(), Ok(()));
        assert_eq!(table.fixed_size(TypeRef(0)), Some(8));
    }

    #[test]
    fn recursion_needs_an_enum_or_a_sequence() {
        let table = TypeTable::new(vec![TypeDescriptor::Struct {
            name: "S".into(),
            fields: vec![field("s", 0)],
        }]);
        assert_eq!(
            nesting_error(&table),
            "a type contains itself without an enum or a sequence between"
        );
        // `enum List { Nil, Cons(u8, List) }` holds itself inline.
        let list = TypeTable::new(vec![
            TypeDescriptor::Enum {
                name: "List".into(),
                discriminant_width: 1,
                variants: vec![
                    variant(0, vec![]),
                    variant(1, vec![field("0", 1), field("1", 0)]),
                ],
            },
            TypeDescriptor::Primitive(Primitive::U8),
        ]);
        assert_eq!(list.validate(), Ok(()));
        assert_eq!(list.fixed_form_size(TypeRef(0)), None);
        assert_eq!(list.measure(TypeRef(0), &[1, 7, 1, 8, 0]), Some(5));
    }

    #[test]
    fn a_discriminant_must_fit_its_width() {
        let table = TypeTable::new(vec![TypeDescriptor::Enum {
            name: "E".into(),
            discriminant_width: 1,
            variants: vec![variant(256, vec![])],
        }]);
        assert!(nesting_error(&table).contains("discriminant width"));
    }

    #[test]
    fn measure_reads_fixed_forms_and_refuses_bad_bytes() {
        let mut d = shape();
        d.push(TypeDescriptor::Fixed(TypeRef(0)));
        let table = TypeTable::new(d);
        // `Fixed(Shape)`: a two-byte discriminant, then eight bytes.
        let circle = [0, 0, 0, 0, 0x80, 0x3f, 0, 0, 0, 0];
        assert_eq!(table.measure(TypeRef(2), &circle), Some(10));
        let mut bad = circle;
        bad[9] = 1; // padding must be zeros
        assert_eq!(table.measure(TypeRef(2), &bad), None);
        // Text needs its terminator and valid UTF-8; a varint must be
        // minimal.
        let text = TypeTable::new(vec![TypeDescriptor::Text]);
        assert_eq!(text.measure(TypeRef(0), b"ab\xff"), Some(3));
        assert_eq!(text.measure(TypeRef(0), b"ab"), None);
        assert_eq!(text.measure(TypeRef(0), b"\xc3\xff"), None);
        let int = TypeTable::new(vec![TypeDescriptor::Primitive(Primitive::U16)]);
        assert_eq!(int.measure(TypeRef(0), &[0x80, 0x00]), None);
        assert_eq!(int.measure(TypeRef(0), &[0xff, 0xff, 0x04]), None);
        assert_eq!(int.measure(TypeRef(0), &[0xff, 0xff, 0x03]), Some(3));
    }

    #[test]
    fn decode_applies_the_rules() {
        let table = TypeTable::new(vec![TypeDescriptor::Struct {
            name: "S".into(),
            fields: vec![field("s", 0)],
        }]);
        assert!(matches!(
            TypeTable::decode(&table.encode()),
            Err(DecodeError::InvalidNesting { .. })
        ));
    }
}
