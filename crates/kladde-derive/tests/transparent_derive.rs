//! `#[kladde(transparent)]`: a single-field newtype persisted exactly as its
//! one field -- same bytes, same fingerprint -- for both the tuple and the
//! braced form.
//!
//! The *representation* half holds for any single-field struct, transparent or
//! not: a one-field struct is just its field, at offset 0. Transparency changes
//! only the *schema* (a shared descriptor and fingerprint) and the guard's
//! accessors. The tests check both sides.

mod support;

use kladde_derive::Persistable;
use kladde_persist::{Packed, Persistable, Slotted};
use support::{Fixture, Number};

#[derive(Persistable)]
#[kladde(transparent)]
struct Meters(Number);

#[derive(Persistable)]
#[kladde(transparent)]
struct Label {
    value: Number,
}

/// Not transparent: the same representation as `Label`, a different schema
/// and guard.
#[derive(Persistable)]
struct PlainLabel {
    value: Number,
}

/// A transparent wrapper of a type whose packed encoding differs.
#[derive(Persistable)]
#[kladde(transparent)]
struct Count(u32);

#[test]
fn transparent_tuple_newtype_round_trips_as_its_field() {
    assert_eq!(
        <Meters as Persistable>::SLOTTED_SIZE,
        <Number as Persistable>::SLOTTED_SIZE
    );
    let mut f = Fixture::for_type::<Meters>();
    let mut m = Meters(Number(0));
    {
        let mut guard = m.guard(&f.store, f.place());
        // `get_mut()` hands back the inner type's own guard.
        guard.get_mut().set(42).unwrap();
        assert_eq!(guard.0, Number(42));
    }
    let reloaded: Meters = f.reload();
    assert_eq!(reloaded.0, Number(42));
}

#[test]
fn transparent_braced_newtype_round_trips_as_its_field() {
    let mut f = Fixture::for_type::<Label>();
    let mut label = Label { value: Number(0) };
    label.guard(&f.store, f.place()).get_mut().set(7).unwrap();
    let reloaded: Label = f.reload();
    assert_eq!(reloaded.value, Number(7));
}

#[test]
fn non_transparent_newtype_round_trips_the_same_way() {
    let mut f = Fixture::for_type::<PlainLabel>();
    let mut label = PlainLabel { value: Number(0) };
    label.guard(&f.store, f.place()).value_mut().set(7).unwrap();
    let reloaded: PlainLabel = f.reload();
    assert_eq!(reloaded.value, Number(7));
}

#[test]
fn a_transparent_wrapper_packs_as_its_field() {
    assert_eq!(
        Count(300).to_bytes::<Packed>(),
        <u32 as Persistable>::to_bytes::<Packed>(&300)
    );
    let mut f = Fixture::new(0);
    let mut count = Count(1);
    count.store::<_, Packed>(&f.store, f.location).unwrap();
    count
        .guard(&f.store, f.packed())
        .get_mut()
        .set(1 << 20)
        .unwrap();
    assert_eq!(f.bytes().len(), 3);
    let reloaded: Count = f.reload_as::<_, Packed>();
    assert_eq!(reloaded.0, 1 << 20);
}

#[test]
fn newtypes_share_layout_regardless_of_transparency() {
    let size = <Number as Persistable>::SLOTTED_SIZE;
    assert_eq!(<Meters as Persistable>::SLOTTED_SIZE, size);
    assert_eq!(<Label as Persistable>::SLOTTED_SIZE, size);
    assert_eq!(<PlainLabel as Persistable>::SLOTTED_SIZE, size);

    // Storing the same inner value through the transparent wrapper, the plain
    // one, or the bare inner type writes the same bytes.
    let via_inner = {
        let mut f = Fixture::for_type::<Number>();
        Number(42)
            .store::<_, Slotted>(&f.store, f.location)
            .unwrap();
        f.bytes()
    };
    let via_transparent = {
        let mut f = Fixture::for_type::<Meters>();
        Meters(Number(42))
            .store::<_, Slotted>(&f.store, f.location)
            .unwrap();
        f.bytes()
    };
    let via_plain = {
        let mut f = Fixture::for_type::<PlainLabel>();
        PlainLabel { value: Number(42) }
            .store::<_, Slotted>(&f.store, f.location)
            .unwrap();
        f.bytes()
    };
    assert_eq!(via_inner, via_transparent);
    assert_eq!(via_inner, via_plain);
}

#[test]
fn only_transparency_shares_the_inner_types_fingerprint() {
    assert_eq!(
        <Meters as Persistable>::fingerprint(),
        <Number as Persistable>::fingerprint()
    );
    assert_eq!(
        <Label as Persistable>::fingerprint(),
        <Number as Persistable>::fingerprint()
    );
    assert_eq!(
        <Meters as Persistable>::schema().descriptors().len(),
        1,
        "a transparent wrapper adds no node of its own",
    );
    assert_ne!(
        <PlainLabel as Persistable>::fingerprint(),
        <Number as Persistable>::fingerprint()
    );
    assert_eq!(<PlainLabel as Persistable>::schema().descriptors().len(), 2);
}

#[derive(Persistable)]
struct HasTransparent {
    a: Meters,
    b: Number,
}

#[test]
fn a_transparent_field_dedups_against_the_inner_type() {
    // `a: Meters` is transparent to `Number`, so both fields resolve to one
    // descriptor: the root struct plus a single `Number` node.
    let table = <HasTransparent as Persistable>::schema();
    assert_eq!(table.descriptors().len(), 2);
}
