//! The whole stack together: `#[derive(Persistable)]` on a struct and an enum,
//! nested `kladde-types` containers, a real `Kladde`, and reopening the file
//! to reconstruct everything from its bytes alone.

use kladde::{Kladde, MemoryStorage, Options, Persistable};
use kladde_types::{PersistableHashMap, PersistableString, PersistableVec};

#[derive(Persistable, Debug, PartialEq)]
enum Role {
    Admin,
    Member,
}

#[derive(Persistable)]
struct User {
    name: PersistableString,
    age: i32,
    role: Role,
    tags: PersistableVec<PersistableString>,
}

#[derive(Persistable)]
struct AppState {
    users: PersistableHashMap<PersistableString, User>,
}

fn new_app(storage: &MemoryStorage) -> Kladde<AppState> {
    let empty = AppState {
        users: PersistableHashMap::new(),
    };
    Kladde::create_in(Box::new(storage.clone()), empty, Options::default()).unwrap()
}

/// The file as another process would find it, flushed or not.
fn reopened(storage: &MemoryStorage) -> Kladde<AppState> {
    let image = MemoryStorage::from_image(storage.image());
    Kladde::open_in(Box::new(image), Options::default()).unwrap()
}

fn user(name: &str, age: i32, role: Role) -> User {
    User {
        name: PersistableString::from(name),
        age,
        role,
        tags: PersistableVec::new(),
    }
}

#[test]
fn a_derived_struct_nested_in_a_hash_map_round_trips() {
    let storage = MemoryStorage::new();
    let mut app = new_app(&storage);
    app.guard()
        .users_mut()
        .insert(PersistableString::from("ada"), user("Ada", 30, Role::Admin))
        .unwrap();
    {
        let mut guard = app.guard();
        let mut users = guard.users_mut();
        let mut ada = users.get_mut(&PersistableString::from("ada")).unwrap();
        ada.age_mut().set(31).unwrap();
        ada.role_mut().set(Role::Member).unwrap();
        ada.tags_mut()
            .push(PersistableString::from("engineer"))
            .unwrap();
    }
    for app in [reopened(&storage), {
        app.flush().unwrap();
        reopened(&storage)
    }] {
        let ada = app
            .get()
            .users
            .get(&PersistableString::from("ada"))
            .unwrap();
        assert_eq!(ada.name, "Ada");
        assert_eq!(ada.age, 31);
        assert_eq!(ada.role, Role::Member);
        assert_eq!(ada.tags.len(), 1);
        assert_eq!(ada.tags[0], "engineer");
    }
}

#[test]
fn mutating_one_entry_does_not_disturb_a_sibling() {
    let storage = MemoryStorage::new();
    let mut app = new_app(&storage);
    {
        let mut guard = app.guard();
        let mut users = guard.users_mut();
        users
            .insert(PersistableString::from("ada"), user("Ada", 30, Role::Admin))
            .unwrap();
        users
            .insert(
                PersistableString::from("bob"),
                user("Bob", 25, Role::Member),
            )
            .unwrap();
        users
            .get_mut(&PersistableString::from("ada"))
            .unwrap()
            .age_mut()
            .set(99)
            .unwrap();
    }
    let app = reopened(&storage);
    let users = &app.get().users;
    assert_eq!(users.get(&PersistableString::from("ada")).unwrap().age, 99);
    let bob = users.get(&PersistableString::from("bob")).unwrap();
    assert_eq!((bob.age, &*bob.name), (25, "Bob"));
}

#[test]
fn deleting_frees_everything_an_entry_owns() {
    let storage = MemoryStorage::new();
    let mut app = new_app(&storage);
    app.flush().unwrap();
    let empty = app.stats().allocations;
    {
        let mut guard = app.guard();
        let mut users = guard.users_mut();
        let mut ada = user("Ada", 30, Role::Admin);
        ada.tags = ["a", "b"]
            .into_iter()
            .map(PersistableString::from)
            .collect();
        users.insert(PersistableString::from("ada"), ada).unwrap();
    }
    app.flush().unwrap();
    // The map's slots, the key, the name, the tags vec, and two tags.
    assert_eq!(app.stats().allocations, empty + 6);
    app.guard()
        .users_mut()
        .delete(&PersistableString::from("ada"))
        .unwrap();
    app.flush().unwrap();
    assert_eq!(
        app.stats().allocations,
        empty + 1,
        "only the empty slot array stays"
    );
}
