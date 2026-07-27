//! Integration: tuples of `Persistable` types used through the real
//! containers and the `#[derive(Persistable)]` macro, end to end via
//! `Kladde` (store + flush + reload).

use kladde::Kladde;
use kladde_types::{Persistable, PersistableString, PersistableVec};

#[test]
fn tuple_as_a_vec_element_round_trips() {
    let mut db = Kladde::new(PersistableVec::<(i32, bool)>::new());
    db.guard().push((1, true));
    db.guard().push((2, false));

    assert_eq!(db.get().get(0), Some(&(1, true)));
    assert_eq!(db.get().get(1), Some(&(2, false)));

    db.flush();
    let reloaded = db.load();
    assert_eq!(reloaded.get(0), Some(&(1, true)));
    assert_eq!(reloaded.get(1), Some(&(2, false)));
}

#[test]
fn tuple_with_an_owning_component_round_trips() {
    // A component that owns its own allocation (a `PersistableString`)
    // must be laid out and reconstructed correctly inside the tuple.
    let mut db = Kladde::new(PersistableVec::<(PersistableString, i32)>::new());
    db.guard().push((PersistableString::from("hello"), 7));

    db.flush();
    let reloaded = db.load();
    let (text, count) = reloaded.get(0).unwrap();
    assert_eq!(text.to_string(), "hello");
    assert_eq!(*count, 7);
}

#[derive(Persistable)]
struct WithTupleField {
    pair: (i32, bool),
    name: PersistableString,
}

#[test]
fn derived_struct_with_a_tuple_field_round_trips() {
    let mut db = Kladde::new(WithTupleField {
        pair: (0, false),
        name: PersistableString::new(),
    });
    {
        let mut guard = db.guard();
        guard.pair_mut().set((42, true));
        guard.name_mut().set("kladde");
    }
    assert_eq!(db.get().pair, (42, true));

    db.flush();
    let reloaded = db.load();
    assert_eq!(reloaded.pair, (42, true));
    assert_eq!(reloaded.name.to_string(), "kladde");
}
