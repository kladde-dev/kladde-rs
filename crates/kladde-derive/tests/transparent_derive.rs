//! `#[kladde(transparent)]`: a single-field newtype persisted exactly as
//! its one field -- same bytes, same fingerprint -- for both the tuple and
//! the braced form.
//!
//! The *representation* half of that (identical `INLINE_SIZE` and byte
//! layout) actually holds for **any** single-field struct, transparent or
//! not -- a one-field struct is just its field, at offset 0. Transparency
//! only changes the *schema* (shared descriptor / fingerprint) and the
//! guard's accessor surface. Several tests below check both sides of that.

mod support;

use kladde_derive::Persistable;
use kladde_traits::Persistable as _;
use kladde_traits::{Allocator, Location};
use support::{MockBackend, Number};

#[derive(Persistable)]
#[kladde(transparent)]
struct Meters(Number);

#[derive(Persistable)]
#[kladde(transparent)]
struct Label {
    value: Number,
}

// A *non-transparent* single-field struct wrapping the same inner type.
// Same representation as `Label`; different schema and guard.
#[derive(Persistable)]
struct PlainLabel {
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
fn non_transparent_newtype_round_trips_the_same_way() {
    // The very same round-trip as the transparent cases -- a plain
    // single-field struct is representationally a newtype too; only its
    // guard accessor (`value_mut`) and schema differ.
    let backend = MockBackend::default();
    let location = backend.root_location(PlainLabel::INLINE_SIZE);

    let mut label = PlainLabel { value: Number(0) };
    {
        let mut guard = label.guard(&backend, location);
        guard.value_mut().set(7);
    }
    assert_eq!(label.value, Number(7));

    let reloaded = PlainLabel::load(&backend, location);
    assert_eq!(reloaded.value, Number(7));
}

/// Stores something into a fresh root region and returns that region's raw
/// bytes -- so two ways of storing the same value can be compared byte for
/// byte.
fn stored_region(size: usize, store: impl FnOnce(&MockBackend, Location)) -> Vec<u8> {
    let backend = MockBackend::default();
    let location = backend.root_location(size);
    store(&backend, location);
    backend.read(location.anchor, 0, size as u32)
}

#[test]
fn newtypes_share_layout_regardless_of_transparency() {
    // Same inline size across the inner type and both wrappers...
    assert_eq!(Number::INLINE_SIZE, 4);
    assert_eq!(Meters::INLINE_SIZE, Number::INLINE_SIZE);
    assert_eq!(Label::INLINE_SIZE, Number::INLINE_SIZE);
    assert_eq!(PlainLabel::INLINE_SIZE, Number::INLINE_SIZE);

    // ...and identical *bytes*: storing the same inner value through the
    // transparent wrapper, the non-transparent wrapper, or the bare inner
    // type writes the same region. Memory layout is a property of the
    // fields, not of `#[kladde(transparent)]`.
    let via_inner = stored_region(Number::INLINE_SIZE, |b, loc| Number(42).store(b, loc));
    let via_transparent = stored_region(Meters::INLINE_SIZE, |b, loc| {
        Meters(Number(42)).store(b, loc)
    });
    let via_plain = stored_region(PlainLabel::INLINE_SIZE, |b, loc| {
        PlainLabel { value: Number(42) }.store(b, loc)
    });
    assert_eq!(via_inner, via_transparent);
    assert_eq!(via_inner, via_plain);
}

#[test]
fn only_transparency_shares_the_inner_types_fingerprint() {
    // Transparent: schema-identical to the field, so same fingerprint and
    // no descriptor of its own.
    assert_eq!(Meters::fingerprint(), Number::fingerprint());
    assert_eq!(Label::fingerprint(), Number::fingerprint());
    assert_eq!(
        Meters::schema().descriptors().len(),
        1,
        "a transparent wrapper adds no node -- just the inner type's",
    );

    // Non-transparent: owns its own `Struct` descriptor, so a *different*
    // fingerprint and an extra node. This is the one thing transparency
    // changes about the schema.
    assert_ne!(PlainLabel::fingerprint(), Number::fingerprint());
    assert_eq!(PlainLabel::schema().descriptors().len(), 2);
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
