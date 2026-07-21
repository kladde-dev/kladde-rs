//! Exercises the whole stack together: `#[derive(Persistable)]` on a
//! custom struct/enum, nesting `kladde-types` containers, backed by
//! `kladde`'s real `Kladde`/`DefaultBackend` (not a test-local mock), and
//! -- the point of this whole round -- an explicit `flush()` followed by
//! reconstructing everything fresh purely from the backend's storage.
//!
//! Note the double-derive on `Role` below: `#[derive(Persistable)]` alone
//! is enough to make a type *mutable* through a `Guard`; `Serialize`/
//! `Deserialize` is needed in addition because a derived enum is treated
//! as an "owning" type (see `kladde-derive`'s crate doc comment) whose
//! content is its whole value, postcard-serialized.

use kladde::Kladde;
use kladde_types::{Persistable, PersistedHashMap, PersistedVec};

#[derive(Persistable, serde::Serialize, serde::Deserialize, Debug, PartialEq)]
enum Role {
    Admin,
    Member,
}

#[derive(Persistable)]
struct User {
    name: String,
    age: i32,
    role: Role,
    tags: PersistedVec<String>,
}

#[derive(Persistable)]
struct AppState {
    users: PersistedHashMap<String, User>,
}

#[test]
fn derived_struct_nested_in_a_persisted_hash_map_via_kladde() {
    let mut app = Kladde::new(AppState {
        users: PersistedHashMap::new(),
    });

    app.guard().users_mut().insert(
        "ada".to_string(),
        User {
            name: "Ada".to_string(),
            age: 30,
            role: Role::Admin,
            tags: PersistedVec::new(),
        },
    );

    // Mutate it through the derived, nested Guards.
    {
        let mut guard = app.guard();
        let mut users = guard.users_mut();
        let mut ada = users
            .get_mut(&"ada".to_string())
            .expect("ada should be present");
        ada.age_mut().set(31);
        ada.role_mut().set(Role::Member);
        ada.tags_mut().push("engineer".to_string());
    }

    let ada = app.get().users.get(&"ada".to_string()).unwrap();
    assert_eq!(ada.name, "Ada");
    assert_eq!(ada.age, 31);
    assert_eq!(ada.role, Role::Member);
    assert_eq!(ada.tags.len(), 1);
    assert_eq!(ada.tags.get(0), Some(&"engineer".to_string()));

    // The actual point of this round: flush, then reconstruct a *fresh*
    // `AppState` purely from the backend's storage -- no reference to
    // `app`'s live value -- and check it matches.
    app.flush();
    let reloaded = app.load();

    let reloaded_ada = reloaded.users.get(&"ada".to_string()).unwrap();
    assert_eq!(reloaded_ada.name, "Ada");
    assert_eq!(reloaded_ada.age, 31);
    assert_eq!(reloaded_ada.role, Role::Member);
    assert_eq!(reloaded_ada.tags.len(), 1);
    assert_eq!(reloaded_ada.tags.get(0), Some(&"engineer".to_string()));

    assert_eq!(app.backend().journal_len(), 0);
}

#[test]
fn mutating_one_element_does_not_disturb_an_unrelated_sibling() {
    let mut app = Kladde::new(AppState {
        users: PersistedHashMap::new(),
    });

    app.guard().users_mut().insert(
        "ada".to_string(),
        User {
            name: "Ada".to_string(),
            age: 30,
            role: Role::Admin,
            tags: PersistedVec::new(),
        },
    );
    app.guard().users_mut().insert(
        "bob".to_string(),
        User {
            name: "Bob".to_string(),
            age: 25,
            role: Role::Member,
            tags: PersistedVec::new(),
        },
    );

    {
        let mut guard = app.guard();
        let mut users = guard.users_mut();
        users.get_mut(&"ada".to_string()).unwrap().age_mut().set(99);
    }

    app.flush();
    let reloaded = app.load();

    assert_eq!(reloaded.users.get(&"ada".to_string()).unwrap().age, 99);
    assert_eq!(reloaded.users.get(&"bob".to_string()).unwrap().age, 25);
    assert_eq!(reloaded.users.get(&"bob".to_string()).unwrap().name, "Bob");
}
