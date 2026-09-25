//! Tuples of `Persistable` types, through the containers and the derive macro,
//! end to end via `Kladde`: store, flush, and reopen.

use kladde::{Kladde, MemoryStorage, Options, Persistable};
use kladde_types::{PersistableString, PersistableVec};

fn roundtrip<T: Persistable + 'static>(root: T, mutate: impl FnOnce(&mut Kladde<T>)) -> Kladde<T> {
    let storage = MemoryStorage::new();
    let mut db = Kladde::create_in(Box::new(storage.clone()), root, Options::default()).unwrap();
    mutate(&mut db);
    db.flush().unwrap();
    Kladde::open_in(
        Box::new(MemoryStorage::from_image(storage.image())),
        Options::default(),
    )
    .unwrap()
}

#[test]
fn tuple_as_a_vec_element_round_trips() {
    let db = roundtrip(PersistableVec::<(i32, bool)>::new(), |db| {
        db.guard().push((1, true)).unwrap();
        db.guard().push((2, false)).unwrap();
    });
    assert_eq!(db.get().as_slice(), &[(1, true), (2, false)]);
}

#[test]
fn tuple_with_an_owning_component_round_trips() {
    let db = roundtrip(PersistableVec::<(PersistableString, i32)>::new(), |db| {
        db.guard()
            .push((PersistableString::from("hello"), 7))
            .unwrap();
    });
    let (text, count) = &db.get()[0];
    assert_eq!((&**text, *count), ("hello", 7));
}

#[derive(Persistable)]
struct WithTupleField {
    pair: (i32, bool),
    name: PersistableString,
}

#[test]
fn derived_struct_with_a_tuple_field_round_trips() {
    let root = WithTupleField {
        pair: (0, false),
        name: PersistableString::new(),
    };
    let db = roundtrip(root, |db| {
        let mut guard = db.guard();
        guard.pair_mut().set((42, true)).unwrap();
        guard.name_mut().set("kladde").unwrap();
    });
    assert_eq!(db.get().pair, (42, true));
    assert_eq!(db.get().name, "kladde");
}
