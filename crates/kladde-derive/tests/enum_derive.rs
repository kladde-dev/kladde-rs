mod support;

use kladde_derive::Persistable;
use kladde_persist::Persistable;
use support::{Fixture, Number};

#[derive(Persistable, Debug, PartialEq)]
enum Shape {
    Origin,
    Circle(Number),
    Rectangle { width: Number, height: Number },
}

#[test]
fn each_variant_kind_round_trips_through_store_and_load() {
    let mut f = Fixture::new(<Shape as Persistable>::INLINE_SIZE);
    for mut value in [
        Shape::Origin,
        Shape::Circle(Number(7)),
        Shape::Rectangle {
            width: Number(3),
            height: Number(4),
        },
    ] {
        value.store(&f.store, f.location).unwrap();
        let reloaded: Shape = f.reload();
        assert_eq!(reloaded, value);
    }
}

#[test]
fn guard_set_replaces_the_whole_value_across_variant_kinds() {
    let mut f = Fixture::new(<Shape as Persistable>::INLINE_SIZE);
    let mut value = Shape::Origin;
    for next in [
        Shape::Circle(Number(5)),
        Shape::Rectangle {
            width: Number(1),
            height: Number(2),
        },
        Shape::Origin,
    ] {
        let expected = format!("{next:?}");
        value.guard(&f.store, f.location).set(next).unwrap();
        assert_eq!(format!("{value:?}"), expected);
        let reloaded: Shape = f.reload();
        assert_eq!(reloaded, value);
    }
}

#[test]
fn a_smaller_variant_zeroes_what_it_leaves_unused() {
    let mut f = Fixture::new(<Shape as Persistable>::INLINE_SIZE);
    let mut value = Shape::Rectangle {
        width: Number(-1),
        height: Number(-1),
    };
    value.store(&f.store, f.location).unwrap();
    let mut value = Shape::Circle(Number(9));
    value.store(&f.store, f.location).unwrap();
    // The discriminant 1, the circle's number, and zeros where the
    // rectangle's height was.
    assert_eq!(f.bytes(), [1, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0]);
}

#[test]
fn an_unknown_discriminant_is_corruption() {
    let mut f = Fixture::new(<Shape as Persistable>::INLINE_SIZE);
    kladde_store::WriteBackend::write(&f.store, f.location.anchor, 0, &[9, 0, 0, 0]).unwrap();
    f.store.flush().unwrap();
    assert!(matches!(
        <Shape as Persistable>::load(&mut f.store, f.location),
        Err(kladde_persist::Error::Corrupt(_))
    ));
}

#[test]
fn inline_size_fits_the_largest_variant_plus_the_discriminant() {
    assert_eq!(
        <Shape as Persistable>::INLINE_SIZE,
        4 + 2 * <Number as Persistable>::INLINE_SIZE
    );
}
