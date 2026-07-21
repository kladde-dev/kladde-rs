mod support;

use kladde_derive::Persistable;
use kladde_traits::Persistable as _;
use support::{MockBackend, Number};

#[derive(Persistable)]
struct Point {
    x: Number,
    y: Number,
}

#[test]
fn generated_guard_exposes_a_mut_accessor_per_field() {
    let backend = MockBackend::default();
    let location = backend.root_location(Point::INLINE_SIZE);
    let mut point = Point {
        x: Number(1),
        y: Number(2),
    };

    let mut guard = point.guard(&backend, location);
    guard.x_mut().set(10);
    guard.y_mut().set(20);

    // Deref gives read-only access to the plain type without a separate
    // accessor.
    assert_eq!(guard.x.0, 10);
    assert_eq!(guard.y.0, 20);

    assert_eq!(point.x.0, 10);
    assert_eq!(point.y.0, 20);

    // Round trip: `x`/`y` were written at static, non-overlapping
    // offsets within `Point`'s own inline representation.
    let reloaded = Point::load(&backend, location);
    assert_eq!(reloaded.x.0, 10);
    assert_eq!(reloaded.y.0, 20);
}

#[derive(Persistable)]
struct Empty;

#[test]
fn unit_struct_derives_without_error() {
    let backend = MockBackend::default();
    let location = backend.root_location(Empty::INLINE_SIZE);
    let mut empty = Empty;
    let _guard = empty.guard(&backend, location);
    assert_eq!(Empty::INLINE_SIZE, 0);
}

#[derive(Persistable)]
struct Nested {
    point: Point,
}

#[test]
fn nested_persistable_fields_reborrow_the_same_backend() {
    let backend = MockBackend::default();
    let location = backend.root_location(Nested::INLINE_SIZE);
    let mut nested = Nested {
        point: Point {
            x: Number(0),
            y: Number(0),
        },
    };

    let mut guard = nested.guard(&backend, location);
    guard.point_mut().x_mut().set(7);

    assert_eq!(nested.point.x.0, 7);

    let reloaded = Nested::load(&backend, location);
    assert_eq!(reloaded.point.x.0, 7);
}
