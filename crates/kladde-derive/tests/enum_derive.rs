mod support;

use kladde_derive::Persistable;
use kladde_persist::Persistable;
use support::{root_location, MockBackend, Number};

#[derive(Persistable, Debug, PartialEq)]
enum Shape {
    Origin,
    Circle(Number),
    Rectangle { width: Number, height: Number },
}

#[test]
fn each_variant_kind_round_trips_through_store_and_load() {
    let mut backend = MockBackend::default();
    let location = root_location(&backend, <Shape as Persistable>::INLINE_SIZE);

    let mut value = Shape::Origin;
    value.store(&backend, location);
    assert_eq!(
        <Shape as Persistable>::load(&mut backend, location),
        Shape::Origin
    );

    let mut value = Shape::Circle(Number(7));
    value.store(&backend, location);
    assert_eq!(
        <Shape as Persistable>::load(&mut backend, location),
        Shape::Circle(Number(7))
    );

    let mut value = Shape::Rectangle {
        width: Number(3),
        height: Number(4),
    };
    value.store(&backend, location);
    assert_eq!(
        <Shape as Persistable>::load(&mut backend, location),
        Shape::Rectangle {
            width: Number(3),
            height: Number(4),
        }
    );
}

#[test]
fn guard_set_replaces_the_whole_value_across_variant_kinds() {
    let mut backend = MockBackend::default();
    let location = root_location(&backend, <Shape as Persistable>::INLINE_SIZE);
    let mut value = Shape::Origin;

    let mut guard = value.guard(&backend, location);
    guard.set(Shape::Circle(Number(5)));
    assert_eq!(*guard, Shape::Circle(Number(5)));
    assert_eq!(value, Shape::Circle(Number(5)));
    assert_eq!(
        <Shape as Persistable>::load(&mut backend, location),
        Shape::Circle(Number(5))
    );

    let mut guard = value.guard(&backend, location);
    guard.set(Shape::Rectangle {
        width: Number(1),
        height: Number(2),
    });
    assert_eq!(
        value,
        Shape::Rectangle {
            width: Number(1),
            height: Number(2),
        }
    );
    assert_eq!(
        <Shape as Persistable>::load(&mut backend, location),
        Shape::Rectangle {
            width: Number(1),
            height: Number(2),
        }
    );

    let mut guard = value.guard(&backend, location);
    guard.set(Shape::Origin);
    assert_eq!(value, Shape::Origin);
    assert_eq!(
        <Shape as Persistable>::load(&mut backend, location),
        Shape::Origin
    );
}

#[test]
fn inline_size_fits_the_largest_variant_plus_the_discriminant() {
    // `Rectangle` (two `Number`s, 4 bytes each) is the largest variant --
    // `Origin` (no fields) and `Circle` (one `Number`) both fit within it.
    assert_eq!(
        <Shape as Persistable>::INLINE_SIZE,
        4 + 2 * <Number as Persistable>::INLINE_SIZE
    );
}
