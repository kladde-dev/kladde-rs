//! The rules of nesting a descriptor table keeps: which kinds may stand
//! where, and which places hold which encoding.
//!
//! A place is *slotted*, holding a type's fixed encoding, or *packed*,
//! holding its packed one. Places inherit the choice of the value they sit
//! in; what a pointer points to starts slotted unless it is wrapped in
//! `Packed`; a `Slotted` wrapper declares a place slotted inside a packed
//! value; a small value's content and spilled form are packed. A type is
//! *slottable* if it has a fixed encoding: every type but a small value and a
//! struct or enum that holds a type without one inline.

use crate::descriptor::{TypeDescriptor, TypeRef, TypeTable};
use crate::serialize::DecodeError;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    Unvisited,
    InProgress,
    Done,
}

impl TypeTable {
    /// Whether the type at `reference` has a fixed encoding, and so may
    /// stand in a slotted place: everything but a small value, and a struct
    /// or enum holding a type without a fixed encoding inline.
    ///
    /// ```
    /// use kladde_schema::{Field, Primitive, TypeDescriptor, TypeRef, TypeTable};
    ///
    /// let table = TypeTable::new(vec![
    ///     TypeDescriptor::Struct {
    ///         name: "Labelled".into(),
    ///         fields: vec![Field { name: "label".into(), ty: TypeRef(1) }],
    ///     },
    ///     TypeDescriptor::Small { content: TypeRef(2), spilled: TypeRef(4) },
    ///     TypeDescriptor::Sequence(TypeRef(3)),
    ///     TypeDescriptor::Primitive(Primitive::Char),
    ///     TypeDescriptor::Pointer(TypeRef(5)),
    ///     TypeDescriptor::Packed(TypeRef(2)),
    /// ]);
    /// assert!(!table.is_slottable(TypeRef(0)));
    /// assert!(table.is_slottable(TypeRef(4)));
    /// assert!(table.validate().is_ok()); // a packed-only root is packed
    /// ```
    pub fn is_slottable(&self, reference: TypeRef) -> bool {
        let mut memo = vec![None; self.descriptors().len()];
        self.slottable(reference.0, &mut memo)
    }

    fn slottable(&self, index: usize, memo: &mut [Option<bool>]) -> bool {
        if let Some(known) = memo[index] {
            return known;
        }
        // A type that contains itself inline is refused by `validate`; here
        // the cycle is cut by assuming the best.
        memo[index] = Some(true);
        let fields: Vec<usize> = match &self.descriptors()[index] {
            TypeDescriptor::Small { .. } => {
                memo[index] = Some(false);
                return false;
            }
            TypeDescriptor::Struct { .. } | TypeDescriptor::Enum { .. } => self.descriptors()
                [index]
                .references()
                .iter()
                .map(|r| r.0)
                .collect(),
            _ => Vec::new(),
        };
        let slottable = fields.into_iter().all(|f| self.slottable(f, memo));
        memo[index] = Some(slottable);
        slottable
    }

    /// Checks the rules of nesting, which [`decode`](TypeTable::decode)
    /// applies to every table it reads:
    ///
    /// - a `Sequence` stands only as what a pointer points to, inside a
    ///   `Packed` wrapper there, or as a small value's content, and a
    ///   `Packed` wrapper only as what a pointer points to;
    /// - a `Slotted` wrapper holds a type with a fixed encoding;
    /// - a type without a fixed encoding stands only where every value
    ///   around it, up to the nearest pointer, is packed;
    /// - no struct or enum contains itself inline.
    ///
    /// The root is slotted if its type has a fixed encoding, and packed
    /// otherwise. See [`is_slottable`](TypeTable::is_slottable) for an
    /// example.
    pub fn validate(&self) -> Result<(), DecodeError> {
        let descriptors = self.descriptors();
        let fail =
            |index: usize, reason: &'static str| Err(DecodeError::InvalidNesting { index, reason });

        if matches!(
            descriptors[0],
            TypeDescriptor::Sequence(_) | TypeDescriptor::Packed(_)
        ) {
            return fail(0, "a sequence or a packed wrapper cannot be a root");
        }
        for descriptor in descriptors {
            let content = match descriptor {
                TypeDescriptor::Small { content, .. } => Some(*content),
                _ => None,
            };
            for child in descriptor.references() {
                let allowed = match &descriptors[child.0] {
                    TypeDescriptor::Packed(_) => matches!(descriptor, TypeDescriptor::Pointer(_)),
                    TypeDescriptor::Sequence(_) => {
                        matches!(
                            descriptor,
                            TypeDescriptor::Pointer(_) | TypeDescriptor::Packed(_)
                        ) || content == Some(child)
                    }
                    _ => true,
                };
                if !allowed {
                    return fail(
                        child.0,
                        "a sequence or a packed wrapper stands away from a pointer or small value",
                    );
                }
            }
            if let TypeDescriptor::Slotted(target) = descriptor {
                if !self.is_slottable(*target) {
                    return fail(
                        target.0,
                        "a slotted place holds a type without a fixed encoding",
                    );
                }
            }
        }

        let mut marks = vec![Mark::Unvisited; descriptors.len()];
        for index in 0..descriptors.len() {
            self.inline_cycle(index, &mut marks)?;
        }

        let mut slottable = vec![None; descriptors.len()];
        let mut seen = vec![[false; 2]; descriptors.len()];
        let root_slotted = self.slottable(0, &mut slottable);
        self.place(0, root_slotted, &mut slottable, &mut seen)
    }

    /// Refuses a struct or enum that reaches itself through inline fields.
    fn inline_cycle(&self, index: usize, marks: &mut [Mark]) -> Result<(), DecodeError> {
        match marks[index] {
            Mark::Done => return Ok(()),
            Mark::InProgress => {
                return Err(DecodeError::InvalidNesting {
                    index,
                    reason: "a type contains itself inline",
                })
            }
            Mark::Unvisited => {}
        }
        marks[index] = Mark::InProgress;
        let inline = match &self.descriptors()[index] {
            TypeDescriptor::Struct { .. }
            | TypeDescriptor::Enum { .. }
            | TypeDescriptor::Slotted(_) => self.descriptors()[index].references(),
            _ => Vec::new(),
        };
        for child in inline {
            self.inline_cycle(child.0, marks)?;
        }
        marks[index] = Mark::Done;
        Ok(())
    }

    /// Checks that the type at `index`, standing in a place that is
    /// `slotted` or not, and everything inline in it, may stand there.
    fn place(
        &self,
        index: usize,
        slotted: bool,
        slottable: &mut [Option<bool>],
        seen: &mut [[bool; 2]],
    ) -> Result<(), DecodeError> {
        if std::mem::replace(&mut seen[index][slotted as usize], true) {
            return Ok(());
        }
        if slotted && !self.slottable(index, slottable) {
            return Err(DecodeError::InvalidNesting {
                index,
                reason: "a type without a fixed encoding stands in a slotted place",
            });
        }
        let descriptors = self.descriptors();
        match &descriptors[index] {
            TypeDescriptor::Primitive(_) => Ok(()),
            TypeDescriptor::Struct { .. } | TypeDescriptor::Enum { .. } => {
                for field in descriptors[index].references() {
                    self.place(field.0, slotted, slottable, seen)?;
                }
                Ok(())
            }
            TypeDescriptor::Opaque { parameters, .. } => {
                // An opaque type lays out its parameters by its own rules:
                // each starts afresh, as a root does.
                for parameter in parameters {
                    let own = self.slottable(parameter.0, slottable);
                    self.place(parameter.0, own, slottable, seen)?;
                }
                Ok(())
            }
            TypeDescriptor::Pointer(target) => {
                let packed = matches!(descriptors[target.0], TypeDescriptor::Packed(_));
                self.place(target.0, !packed, slottable, seen)
            }
            TypeDescriptor::Packed(target) => self.place(target.0, false, slottable, seen),
            TypeDescriptor::Sequence(element) => self.place(element.0, slotted, slottable, seen),
            TypeDescriptor::Slotted(target) => self.place(target.0, true, slottable, seen),
            TypeDescriptor::Small { content, spilled } => {
                self.place(content.0, false, slottable, seen)?;
                self.place(spilled.0, false, slottable, seen)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::descriptor::{Field, Primitive, TypeDescriptor, TypeRef, TypeTable};
    use crate::serialize::DecodeError;

    fn field(name: &str, ty: usize) -> Field {
        Field {
            name: name.into(),
            ty: TypeRef(ty),
        }
    }

    /// `[0]` is filled in by each test; `[1..]` is a small string:
    /// `Small(Sequence(char), Pointer(Packed(Sequence(char))))`.
    fn with_small_string(root: TypeDescriptor) -> TypeTable {
        TypeTable::new(vec![
            root,
            TypeDescriptor::Small {
                content: TypeRef(2),
                spilled: TypeRef(4),
            },
            TypeDescriptor::Sequence(TypeRef(3)),
            TypeDescriptor::Primitive(Primitive::Char),
            TypeDescriptor::Pointer(TypeRef(5)),
            TypeDescriptor::Packed(TypeRef(2)),
        ])
    }

    fn nesting_error(table: &TypeTable) -> &'static str {
        match table.validate() {
            Err(DecodeError::InvalidNesting { reason, .. }) => reason,
            other => panic!("expected a nesting error, got {other:?}"),
        }
    }

    #[test]
    fn a_vector_of_small_strings_must_be_packed() {
        let slotted = with_small_string(TypeDescriptor::Pointer(TypeRef(6)));
        let mut descriptors = slotted.descriptors().to_vec();
        descriptors.push(TypeDescriptor::Sequence(TypeRef(1)));
        let slotted = TypeTable::new(descriptors.clone());
        assert_eq!(
            nesting_error(&slotted),
            "a type without a fixed encoding stands in a slotted place"
        );
        descriptors[0] = TypeDescriptor::Pointer(TypeRef(7));
        descriptors.push(TypeDescriptor::Packed(TypeRef(6)));
        assert_eq!(TypeTable::new(descriptors).validate(), Ok(()));
    }

    #[test]
    fn a_slotted_wrapper_needs_a_fixed_encoding() {
        let mut descriptors = with_small_string(TypeDescriptor::Struct {
            name: "S".into(),
            fields: vec![field("s", 6)],
        })
        .descriptors()
        .to_vec();
        descriptors.push(TypeDescriptor::Slotted(TypeRef(1)));
        assert_eq!(
            nesting_error(&TypeTable::new(descriptors.clone())),
            "a slotted place holds a type without a fixed encoding"
        );
        descriptors[6] = TypeDescriptor::Slotted(TypeRef(3));
        assert_eq!(TypeTable::new(descriptors).validate(), Ok(()));
    }

    #[test]
    fn sequences_and_packed_wrappers_stand_only_behind_pointers() {
        let table = with_small_string(TypeDescriptor::Struct {
            name: "S".into(),
            fields: vec![field("s", 2)],
        });
        assert!(nesting_error(&table).starts_with("a sequence or a packed wrapper"));
        let table = TypeTable::new(vec![TypeDescriptor::Sequence(TypeRef(0))]);
        assert!(nesting_error(&table).contains("cannot be a root"));
    }

    #[test]
    fn a_struct_cannot_contain_itself_inline() {
        let table = TypeTable::new(vec![TypeDescriptor::Struct {
            name: "S".into(),
            fields: vec![field("s", 0)],
        }]);
        assert_eq!(nesting_error(&table), "a type contains itself inline");
        // Through a pointer it can.
        let table = TypeTable::new(vec![
            TypeDescriptor::Struct {
                name: "S".into(),
                fields: vec![field("children", 1)],
            },
            TypeDescriptor::Pointer(TypeRef(2)),
            TypeDescriptor::Sequence(TypeRef(0)),
        ]);
        assert_eq!(table.validate(), Ok(()));
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
