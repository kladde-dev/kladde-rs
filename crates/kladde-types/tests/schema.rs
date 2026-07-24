//! End-to-end `describe`/`schema`/`fingerprint` over real `Persistable`
//! types: derived structs and enums, built-in containers, and recursion.

use kladde_types::{Persistable, PersistedString, PersistedVec};

#[derive(kladde_types::Persistable)]
struct Point {
    x: i32,
    y: i32,
}

// Same field names and types as `Point`; only the type's own name differs.
#[derive(kladde_types::Persistable)]
struct Coordinate {
    x: i32,
    y: i32,
}

#[derive(kladde_types::Persistable)]
struct PointSwapped {
    y: i32,
    x: i32,
}

#[derive(kladde_types::Persistable)]
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

#[derive(kladde_types::Persistable)]
struct WithContainers {
    name: PersistedString,
    scores: PersistedVec<i32>,
}

#[test]
fn containers_fingerprint_stably() {
    assert_eq!(WithContainers::fingerprint(), WithContainers::fingerprint());
    // Its schema has the root struct plus one descriptor per distinct
    // container/scalar type reached: PersistedString, PersistedVec, i32.
    assert_eq!(WithContainers::schema().descriptors().len(), 4);
}

#[derive(kladde_types::Persistable)]
enum Shape {
    Empty,
    Circle(i32),
    Rect { w: i32, h: i32 },
}

#[test]
fn enum_fingerprint_is_stable_and_acyclic() {
    assert_eq!(Shape::fingerprint(), Shape::fingerprint());
    assert!(!Shape::fingerprint().is_cyclic());
}

// A recursive type: a tree whose children are more trees. `Tree` reaches
// itself through `PersistedVec<Tree>`, so it is on a cycle.
#[derive(kladde_types::Persistable)]
struct Tree {
    value: i32,
    children: PersistedVec<Tree>,
}

#[test]
fn recursive_type_sets_the_cyclic_flag() {
    assert!(Tree::fingerprint().is_cyclic());
    assert_eq!(Tree::fingerprint(), Tree::fingerprint());
    // `Point`, which contains no recursion, does not.
    assert!(!Point::fingerprint().is_cyclic());
}

#[test]
fn explicit_enum_discriminants_are_honored() {
    #[derive(kladde_types::Persistable)]
    enum Explicit {
        A = 10,
        B = 20,
    }
    #[derive(kladde_types::Persistable)]
    enum Implicit {
        A,
        B,
    }
    // Different discriminant values -> different fingerprints, even though
    // the variant names and (empty) fields match.
    assert_ne!(Explicit::fingerprint(), Implicit::fingerprint());

    // The schema reports the pinned values.
    let table = Explicit::schema();
    if let kladde_types::TypeDescriptor::Enum { variants, .. } = &table.descriptors()[0] {
        let mut discriminants: Vec<u64> = variants.iter().map(|v| v.discriminant).collect();
        discriminants.sort_unstable();
        assert_eq!(discriminants, vec![10, 20]);
    } else {
        panic!("root should be an enum");
    }
}
