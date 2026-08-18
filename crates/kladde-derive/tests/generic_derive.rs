//! `#[derive(Persistable)]` on generic types (type parameters), including
//! the hygiene case where a user type parameter is literally named `B`
//! (the name the generated guard uses internally for its backend), and
//! generic tuple structs / enums.

mod support;

use kladde_derive::Persistable;
use kladde_persist::Persistable;
use support::{root_location, MockBackend, Number};

#[derive(Persistable)]
struct Pair<A, B> {
    a: A,
    b: B,
}

#[test]
fn generic_struct_round_trips_and_exposes_accessors() {
    let mut backend = MockBackend::default();
    let location = root_location(&backend, <Pair<Number, Number> as Persistable>::INLINE_SIZE);
    let mut pair = Pair {
        a: Number(1),
        b: Number(2),
    };
    {
        let mut guard = pair.guard(&backend, location);
        guard.a_mut().set(10);
        guard.b_mut().set(20);
    }
    assert_eq!(pair.a.0, 10);
    assert_eq!(pair.b.0, 20);

    let reloaded = <Pair<Number, Number> as Persistable>::load(&mut backend, location);
    assert_eq!(reloaded.a.0, 10);
    assert_eq!(reloaded.b.0, 20);

    // `describe`/`fingerprint` must also work for a generic instantiation.
    let _ = <Pair<Number, Number> as Persistable>::fingerprint();
}

/// A type parameter literally named `B` must not collide with the backend
/// parameter the generated guard introduces (which is `__B`).
#[derive(Persistable)]
struct Wrap<B> {
    value: B,
}

#[test]
fn type_param_named_b_does_not_collide_with_backend() {
    let mut backend = MockBackend::default();
    let location = root_location(&backend, <Wrap<Number> as Persistable>::INLINE_SIZE);
    let mut w = Wrap { value: Number(5) };
    w.guard(&backend, location).value_mut().set(7);
    assert_eq!(w.value.0, 7);
    assert_eq!(
        <Wrap<Number> as Persistable>::load(&mut backend, location)
            .value
            .0,
        7
    );
}

/// A generic *tuple* struct: positional fields get `field_{i}_mut()`
/// accessors.
#[derive(Persistable)]
struct TuplePair<T>(T, Number);

#[test]
fn generic_tuple_struct_round_trips() {
    let mut backend = MockBackend::default();
    let location = root_location(&backend, <TuplePair<Number> as Persistable>::INLINE_SIZE);
    let mut tp = TuplePair(Number(1), Number(2));
    {
        let mut guard = tp.guard(&backend, location);
        guard.field_0_mut().set(11);
        guard.field_1_mut().set(22);
    }
    assert_eq!((tp.0).0, 11);
    assert_eq!((tp.1).0, 22);

    let reloaded = <TuplePair<Number> as Persistable>::load(&mut backend, location);
    assert_eq!((reloaded.0).0, 11);
    assert_eq!((reloaded.1).0, 22);
}

#[test]
fn parts_gives_simultaneous_guards_for_all_fields() {
    let mut backend = MockBackend::default();
    let location = root_location(&backend, <Pair<Number, Number> as Persistable>::INLINE_SIZE);
    let mut pair = Pair {
        a: Number(0),
        b: Number(0),
    };
    {
        let mut guard = pair.guard(&backend, location);
        // Both field guards are live at once -- `a` is used *after* `b` was
        // created and used, which only compiles because they borrow
        // disjoint parts of the guard.
        let PairParts { mut a, mut b } = guard.parts();
        b.set(22);
        a.set(11);
    }
    assert_eq!(pair.a.0, 11);
    assert_eq!(pair.b.0, 22);

    let reloaded = <Pair<Number, Number> as Persistable>::load(&mut backend, location);
    assert_eq!(reloaded.a.0, 11);
    assert_eq!(reloaded.b.0, 22);
}

#[test]
fn tuple_struct_parts_is_a_tuple_struct_of_guards() {
    let backend = MockBackend::default();
    let location = root_location(&backend, <TuplePair<Number> as Persistable>::INLINE_SIZE);
    let mut tp = TuplePair(Number(0), Number(0));
    {
        let mut guard = tp.guard(&backend, location);
        let TuplePairParts(mut f0, mut f1) = guard.parts();
        f1.set(22);
        f0.set(11);
    }
    assert_eq!((tp.0).0, 11);
    assert_eq!((tp.1).0, 22);
}

#[derive(Persistable)]
enum Either<A, B> {
    Left(A),
    Right(B),
}

#[test]
fn generic_enum_round_trips() {
    let mut backend = MockBackend::default();
    let location = root_location(
        &backend,
        <Either<Number, Number> as Persistable>::INLINE_SIZE,
    );
    let mut e: Either<Number, Number> = Either::Left(Number(1));
    e.guard(&backend, location).set(Either::Right(Number(9)));
    match &e {
        Either::Right(n) => assert_eq!(n.0, 9),
        _ => panic!("wrong variant"),
    }

    let reloaded = <Either<Number, Number> as Persistable>::load(&mut backend, location);
    match reloaded {
        Either::Right(n) => assert_eq!(n.0, 9),
        _ => panic!("wrong variant after reload"),
    }
}
