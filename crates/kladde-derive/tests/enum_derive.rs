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
    // The one-byte discriminant 1, the circle's number, and zeros where the
    // rectangle's height was.
    assert_eq!(f.bytes(), [1, 9, 0, 0, 0, 0, 0, 0, 0]);
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
        1 + 2 * <Number as Persistable>::INLINE_SIZE
    );
}

fn descriptor_width<T: Persistable + 'static>() -> u8 {
    match &T::schema().descriptors()[0] {
        kladde_persist::TypeDescriptor::Enum {
            discriminant_width, ..
        } => *discriminant_width,
        other => panic!("not an enum: {other:?}"),
    }
}

#[test]
fn parts_mutates_a_named_field_in_place() {
    let mut f = Fixture::new(<Shape as Persistable>::INLINE_SIZE);
    let mut value = Shape::Rectangle {
        width: Number(3),
        height: Number(4),
    };
    value.store(&f.store, f.location).unwrap();
    match value.guard(&f.store, f.location).parts() {
        ShapeParts::Rectangle { mut width, .. } => width.set(30).unwrap(),
        _ => panic!("the value is a rectangle"),
    }
    assert_eq!(
        value,
        Shape::Rectangle {
            width: Number(30),
            height: Number(4),
        }
    );
    // The discriminant 2 is untouched, the width is rewritten in place.
    assert_eq!(f.bytes(), [2, 30, 0, 0, 0, 4, 0, 0, 0]);
    let reloaded: Shape = f.reload();
    assert_eq!(reloaded, value);
}

#[test]
fn parts_of_tuple_and_unit_variants() {
    let mut f = Fixture::new(<Shape as Persistable>::INLINE_SIZE);
    let mut value = Shape::Circle(Number(7));
    value.store(&f.store, f.location).unwrap();
    if let ShapeParts::Circle(mut radius) = value.guard(&f.store, f.location).parts() {
        radius.set(8).unwrap();
    } else {
        panic!("the value is a circle");
    }
    assert_eq!(value, Shape::Circle(Number(8)));
    let reloaded: Shape = f.reload();
    assert_eq!(reloaded, value);

    let mut guard = value.guard(&f.store, f.location);
    guard.set(Shape::Origin).unwrap();
    assert!(matches!(guard.parts(), ShapeParts::Origin));
}

#[derive(Persistable, Debug, PartialEq)]
enum Maybe<T> {
    Absent,
    Present(T),
}

#[test]
fn parts_of_a_generic_enum() {
    let mut f = Fixture::new(<Maybe<Number> as Persistable>::INLINE_SIZE);
    let mut value = Maybe::Present(Number(1));
    value.store(&f.store, f.location).unwrap();
    if let MaybeParts::Present(mut inner) = value.guard(&f.store, f.location).parts() {
        inner.set(2).unwrap();
    }
    assert_eq!(value, Maybe::Present(Number(2)));
    let reloaded: Maybe<Number> = f.reload();
    assert_eq!(reloaded, value);
    let absent: Maybe<Number> = Maybe::Absent;
    assert_eq!(absent, Maybe::Absent);
}

// Fields named like the parameters of the generated `store` and `load`.
#[derive(Persistable, Debug, PartialEq)]
enum Shadowing {
    Only { backend: Number, location: Number },
}

#[test]
fn fields_named_like_generated_parameters_round_trip() {
    let mut f = Fixture::new(<Shadowing as Persistable>::INLINE_SIZE);
    let mut value = Shadowing::Only {
        backend: Number(1),
        location: Number(2),
    };
    value.store(&f.store, f.location).unwrap();
    let mut guard = value.guard(&f.store, f.location);
    let ShadowingParts::Only { mut location, .. } = guard.parts();
    location.set(3).unwrap();
    let reloaded: Shadowing = f.reload();
    assert_eq!(reloaded, value);
}

#[derive(Persistable, Debug, PartialEq)]
enum Wide {
    Low,
    High = 256,
}

#[derive(Persistable, Debug, PartialEq)]
#[repr(u16)]
enum PinnedU16 {
    A,
    B,
}

#[derive(Persistable, Debug, PartialEq)]
#[repr(C, u64)]
enum PinnedU64 {
    A(Number),
    B,
}

#[test]
fn the_width_is_the_smallest_that_holds_every_discriminant() {
    assert_eq!(<Shape as Persistable>::INLINE_SIZE, 9);
    assert_eq!(descriptor_width::<Shape>(), 1);
    assert_eq!(<Wide as Persistable>::INLINE_SIZE, 2);
    assert_eq!(descriptor_width::<Wide>(), 2);

    let mut f = Fixture::new(<Wide as Persistable>::INLINE_SIZE);
    let mut value = Wide::High;
    value.store(&f.store, f.location).unwrap();
    assert_eq!(f.bytes(), [0x00, 0x01]);
    let reloaded: Wide = f.reload();
    assert_eq!(reloaded, Wide::High);
}

#[cfg(target_pointer_width = "64")]
#[test]
fn a_discriminant_beyond_32_bits_takes_8_bytes() {
    // Only compiled for 64-bit targets, where `isize` holds the value.
    #[allow(clippy::enum_clike_unportable_variant)]
    #[derive(Persistable)]
    enum Huge {
        A = 0x1_0000_0000,
    }
    assert_eq!(<Huge as Persistable>::INLINE_SIZE, 8);
    assert_eq!(descriptor_width::<Huge>(), 8);
}

#[test]
fn an_integer_repr_fixes_the_width() {
    assert_eq!(<PinnedU16 as Persistable>::INLINE_SIZE, 2);
    assert_eq!(descriptor_width::<PinnedU16>(), 2);
    assert_eq!(<PinnedU64 as Persistable>::INLINE_SIZE, 8 + 4);
    assert_eq!(descriptor_width::<PinnedU64>(), 8);

    let mut f = Fixture::new(<PinnedU64 as Persistable>::INLINE_SIZE);
    for mut value in [PinnedU64::A(Number(5)), PinnedU64::B] {
        value.store(&f.store, f.location).unwrap();
        let reloaded: PinnedU64 = f.reload();
        assert_eq!(reloaded, value);
    }
    let mut value = PinnedU64::A(Number(5));
    value.store(&f.store, f.location).unwrap();
    assert_eq!(f.bytes(), [0, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0]);
}
