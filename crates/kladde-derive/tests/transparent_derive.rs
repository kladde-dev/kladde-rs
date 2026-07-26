//! `#[kladde(transparent)]`: a single-field newtype persisted exactly as
//! its one field -- same bytes, same fingerprint -- for both the tuple and
//! the braced form.

mod support;

use kladde_derive::Persistable;
use kladde_traits::Persistable as _;
use support::{MockBackend, Number};

#[derive(Persistable)]
#[kladde(transparent)]
struct Meters(Number);

#[derive(Persistable)]
#[kladde(transparent)]
struct Label {
    value: Number,
}

#[test]
fn transparent_tuple_newtype_round_trips_as_its_field() {
    // The wrapper reserves exactly what the inner type does.
    assert_eq!(Meters::INLINE_SIZE, Number::INLINE_SIZE);

    let backend = MockBackend::default();
    let location = backend.root_location(Meters::INLINE_SIZE);

    let mut m = Meters(Number(0));
    {
        let mut guard = m.guard(&backend, location);
        // `get_mut()` hands back the inner type's own guard, so `Number`'s
        // full API (here `set`) is reachable through the wrapper.
        guard.get_mut().set(42);
        // Deref exposes the wrapper itself for reads.
        assert_eq!(guard.0, Number(42));
    }
    assert_eq!(m.0, Number(42));

    // Reloading reconstructs the wrapper from the same bytes.
    let reloaded = Meters::load(&backend, location);
    assert_eq!(reloaded.0, Number(42));
}

#[test]
fn transparent_braced_newtype_round_trips_as_its_field() {
    let backend = MockBackend::default();
    let location = backend.root_location(Label::INLINE_SIZE);

    let mut label = Label { value: Number(0) };
    {
        let mut guard = label.guard(&backend, location);
        guard.get_mut().set(7);
    }
    assert_eq!(label.value, Number(7));

    let reloaded = Label::load(&backend, location);
    assert_eq!(reloaded.value, Number(7));
}

#[test]
fn transparent_shares_the_inner_types_fingerprint() {
    // The whole point: a transparent wrapper is schema-identical to its
    // field, so it fingerprints the same and owns no descriptor of its own.
    assert_eq!(Meters::fingerprint(), Number::fingerprint());
    assert_eq!(Label::fingerprint(), Number::fingerprint());

    let table = Meters::schema();
    assert_eq!(
        table.descriptors().len(),
        1,
        "a transparent wrapper adds no node -- just the inner type's",
    );
}

#[derive(Persistable)]
struct HasTransparent {
    a: Meters,
    b: Number,
}

#[test]
fn a_transparent_field_dedups_against_the_inner_type() {
    // `a: Meters` is transparent to `Number` and `b: Number` is `Number`,
    // so both resolve to the *same* descriptor: the schema is the root
    // struct plus a single shared `Number` node, not two.
    let table = HasTransparent::schema();
    assert_eq!(table.descriptors().len(), 2);
}
