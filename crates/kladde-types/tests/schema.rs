//! End-to-end `describe`/`schema`/`fingerprint` over real `Persistable`
//! types: derived structs and enums, built-in containers, and recursion.

use kladde::Persistable;
use kladde_types::{PersistableString, PersistableVec};

#[derive(kladde::Persistable)]
struct Point {
    x: i32,
    y: i32,
}

// Same field names and types as `Point`; only the type's own name differs.
#[derive(kladde::Persistable)]
struct Coordinate {
    x: i32,
    y: i32,
}

#[derive(kladde::Persistable)]
struct PointSwapped {
    y: i32,
    x: i32,
}

#[derive(kladde::Persistable)]
struct PointRenamedField {
    x: i32,
    z: i32,
}

#[test]
fn fingerprint_is_reproducible() {
    assert_eq!(Point::fingerprint(), Point::fingerprint());
}

#[test]
fn type_name_does_not_affect_fingerprint_but_field_name_and_order_do() {
    let base = Point::fingerprint();
    assert_eq!(base, Coordinate::fingerprint(), "type name is excluded");
    assert_ne!(base, PointSwapped::fingerprint(), "field order matters");
    assert_ne!(base, PointRenamedField::fingerprint(), "field name matters");
}

#[derive(kladde::Persistable)]
struct WithContainers {
    name: PersistableString,
    scores: PersistableVec<i32>,
}

#[test]
fn containers_fingerprint_stably() {
    assert_eq!(WithContainers::fingerprint(), WithContainers::fingerprint());
    // The root struct; the string's `Pointer`, `Packed`, `Sequence` and
    // `char`; the vector's `Pointer`, `Sequence`, and `i32`.
    let table = WithContainers::schema();
    assert_eq!(table.descriptors().len(), 8);
    assert_eq!(table.validate(), Ok(()));
}

#[test]
fn containers_describe_their_structure() {
    use kladde::{Primitive, TypeDescriptor as D, TypeRef};
    let string = PersistableString::<kladde::Pointer>::schema();
    assert_eq!(string.get(TypeRef(0)), &D::Pointer(TypeRef(1)));
    assert_eq!(string.get(TypeRef(1)), &D::Packed(TypeRef(2)));
    assert_eq!(string.get(TypeRef(2)), &D::Sequence(TypeRef(3)));
    assert_eq!(string.get(TypeRef(3)), &D::Primitive(Primitive::Char));
    // The same layout as a packed vector of chars, so the same fingerprint.
    assert_eq!(
        PersistableString::<kladde::Pointer>::fingerprint(),
        kladde_types::PackedPersistableVec::<char>::fingerprint()
    );
    assert_ne!(
        PersistableVec::<u8>::fingerprint(),
        PersistableString::<kladde::Pointer>::fingerprint()
    );
}

#[derive(kladde::Persistable)]
enum Shape {
    Empty,
    Circle(i32),
    Rect { w: i32, h: i32 },
}

#[test]
fn enum_fingerprint_is_stable() {
    assert_eq!(Shape::fingerprint(), Shape::fingerprint());
}

// A recursive type: a tree whose children are more trees. `Tree` reaches
// itself through `PersistableVec<Tree>`, exercising the de Bruijn back-edge.
#[derive(kladde::Persistable)]
struct Tree {
    value: i32,
    children: PersistableVec<Tree>,
}

#[test]
fn recursive_type_fingerprints_reproducibly() {
    // The cycle must terminate and hash deterministically.
    assert_eq!(Tree::fingerprint(), Tree::fingerprint());
    // A recursive type and a non-recursive one are different types.
    assert_ne!(Tree::fingerprint(), Point::fingerprint());
    assert_eq!(Tree::schema().validate(), Ok(()));
}

#[test]
fn explicit_enum_discriminants_are_honored() {
    #[derive(kladde::Persistable)]
    enum Explicit {
        A = 10,
        B = 20,
    }
    #[derive(kladde::Persistable)]
    enum Implicit {
        A,
        B,
    }
    // Different discriminant values -> different fingerprints, even though
    // the variant names and (empty) fields match.
    assert_ne!(Explicit::fingerprint(), Implicit::fingerprint());

    // The schema reports the pinned values.
    let table = Explicit::schema();
    if let kladde::TypeDescriptor::Enum { variants, .. } = &table.descriptors()[0] {
        let mut discriminants: Vec<u64> = variants.iter().map(|v| v.discriminant).collect();
        discriminants.sort_unstable();
        assert_eq!(discriminants, vec![10, 20]);
    } else {
        panic!("root should be an enum");
    }
}
