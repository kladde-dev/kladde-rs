//! `#[derive(Persistable)]` on generic types, including the hygiene case of a
//! type parameter literally named `B`, and generic tuple structs and enums.

mod support;

use kladde_derive::Persistable;
use kladde_persist::Persistable;
use support::{Fixture, Number};

#[derive(Persistable)]
struct Pair<A, B> {
    a: A,
    b: B,
}

#[test]
fn generic_struct_round_trips_and_exposes_accessors() {
    let mut f = Fixture::for_type::<Pair<Number, Number>>();
    let mut pair = Pair {
        a: Number(1),
        b: Number(2),
    };
    {
        let mut guard = pair.guard(&f.store, f.place());
        guard.a_mut().set(10).unwrap();
        guard.b_mut().set(20).unwrap();
    }
    assert_eq!((pair.a.0, pair.b.0), (10, 20));
    let reloaded: Pair<Number, Number> = f.reload();
    assert_eq!((reloaded.a.0, reloaded.b.0), (10, 20));
    let _ = <Pair<Number, Number> as Persistable>::fingerprint();
}

/// A type parameter named `B` must not collide with the backend parameter the
/// generated guard introduces, which is `__B`.
#[derive(Persistable)]
struct Wrap<B> {
    value: B,
}

#[test]
fn type_param_named_b_does_not_collide_with_backend() {
    let mut f = Fixture::for_type::<Wrap<Number>>();
    let mut w = Wrap { value: Number(5) };
    w.guard(&f.store, f.place()).value_mut().set(7).unwrap();
    assert_eq!(w.value.0, 7);
    let reloaded: Wrap<Number> = f.reload();
    assert_eq!(reloaded.value.0, 7);
}

/// A type parameter named `E` must not collide with the encoding parameter,
/// which is `__E`.
#[derive(Persistable)]
struct Tagged<E> {
    tag: u8,
    value: E,
}

#[test]
fn type_param_named_e_does_not_collide_with_encoding() {
    let mut f = Fixture::for_type::<Tagged<Number>>();
    let mut t = Tagged {
        tag: 1,
        value: Number(5),
    };
    t.store::<_, kladde_persist::Slotted>(&f.store, f.location)
        .unwrap();
    t.guard(&f.store, f.place()).value_mut().set(6).unwrap();
    let reloaded: Tagged<Number> = f.reload();
    assert_eq!((reloaded.tag, reloaded.value.0), (1, 6));
}

/// A generic tuple struct: positional fields get `field_{i}_mut()` accessors.
#[derive(Persistable)]
struct TuplePair<T>(T, Number);

#[test]
fn generic_tuple_struct_round_trips() {
    let mut f = Fixture::for_type::<TuplePair<Number>>();
    let mut tp = TuplePair(Number(1), Number(2));
    {
        let mut guard = tp.guard(&f.store, f.place());
        guard.field_0_mut().set(11).unwrap();
        guard.field_1_mut().set(22).unwrap();
    }
    let reloaded: TuplePair<Number> = f.reload();
    assert_eq!(((reloaded.0).0, (reloaded.1).0), (11, 22));
}

#[test]
fn parts_gives_simultaneous_guards_for_all_fields() {
    let mut f = Fixture::for_type::<Pair<Number, Number>>();
    let mut pair = Pair {
        a: Number(0),
        b: Number(0),
    };
    {
        let mut guard = pair.guard(&f.store, f.place());
        // Both field guards are live at once, which compiles only because
        // they borrow disjoint parts of the value.
        let PairParts { mut a, mut b } = guard.parts();
        b.set(22).unwrap();
        a.set(11).unwrap();
    }
    let reloaded: Pair<Number, Number> = f.reload();
    assert_eq!((reloaded.a.0, reloaded.b.0), (11, 22));
}

#[test]
fn tuple_struct_parts_is_a_tuple_struct_of_guards() {
    let f = Fixture::for_type::<TuplePair<Number>>();
    let mut tp = TuplePair(Number(0), Number(0));
    {
        let mut guard = tp.guard(&f.store, f.place());
        let TuplePairParts(mut f0, mut f1) = guard.parts();
        f1.set(22).unwrap();
        f0.set(11).unwrap();
    }
    assert_eq!(((tp.0).0, (tp.1).0), (11, 22));
}

#[derive(Persistable)]
enum Either<A, B> {
    Left(A),
    Right(B),
}

#[test]
fn generic_enum_round_trips() {
    let mut f = Fixture::for_type::<Either<Number, Number>>();
    let mut e: Either<Number, Number> = Either::Left(Number(1));
    e.guard(&f.store, f.place())
        .set(Either::Right(Number(9)))
        .unwrap();
    assert!(matches!(&e, Either::Right(n) if n.0 == 9));
    let reloaded: Either<Number, Number> = f.reload();
    assert!(matches!(reloaded, Either::Right(n) if n.0 == 9));
}
