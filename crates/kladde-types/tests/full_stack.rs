//! Exercises the whole stack together: `#[derive(Persistable)]` on a
//! custom struct and enum, nesting `kladde-types` containers, backed by
//! `kladde`'s real `Kladde`/`DefaultBackend` (not a test-local mock), and
//! -- the point of this whole round -- an explicit `flush()` followed by
//! reconstructing everything fresh purely from the backend's storage.

use kladde::Kladde;
use kladde_types::{Persistable, PersistedHashMap, PersistedString, PersistedVec};

#[derive(Persistable, Debug, PartialEq)]
enum Role {
    Admin,
    Member,
}

#[derive(Persistable)]
struct User {
    name: PersistedString,
    age: i32,
    role: Role,
    tags: PersistedVec<PersistedString>,
}

#[derive(Persistable)]
struct AppState {
    users: PersistedHashMap<PersistedString, User>,
}

#[test]
fn derived_struct_nested_in_a_persisted_hash_map_via_kladde() {
    let mut app = Kladde::new(AppState {
        users: PersistedHashMap::new(),
    });

    let ada = User {
        name: PersistedString::from("Ada"),
        age: 30,
        role: Role::Admin,
        tags: PersistedVec::new(),
    };
    app.guard()
        .users_mut()
        .insert(PersistedString::from("ada"), ada);

    // Mutate it through the derived, nested Guards.
    {
        let mut guard = app.guard();
        let mut users = guard.users_mut();
        let mut ada = users
            .get_mut(&PersistedString::from("ada"))
            .expect("ada should be present");
        ada.age_mut().set(31);
        ada.role_mut().set(Role::Member);
        ada.tags_mut().push(PersistedString::from("engineer"));
    }

    let ada = app.get().users.get(&PersistedString::from("ada")).unwrap();
    assert_eq!(ada.name, "Ada");
    assert_eq!(ada.age, 31);
    assert_eq!(ada.role, Role::Member);
    assert_eq!(ada.tags.len(), 1);
    assert_eq!(ada.tags.get(0), Some(&PersistedString::from("engineer")));

    // The actual point of this round: flush, then reconstruct a *fresh*
    // `AppState` purely from the backend's storage -- no reference to
    // `app`'s live value -- and check it matches.
    app.flush();
    let reloaded = app.load();

    let reloaded_ada = reloaded.users.get(&PersistedString::from("ada")).unwrap();
    assert_eq!(reloaded_ada.name, "Ada");
    assert_eq!(reloaded_ada.age, 31);
    assert_eq!(reloaded_ada.role, Role::Member);
    assert_eq!(reloaded_ada.tags.len(), 1);
    assert_eq!(
        reloaded_ada.tags.get(0),
        Some(&PersistedString::from("engineer"))
    );

    assert_eq!(app.backend().journal_len(), 0);
}

#[test]
fn mutating_one_element_does_not_disturb_an_unrelated_sibling() {
    let mut app = Kladde::new(AppState {
        users: PersistedHashMap::new(),
    });

    let ada = User {
        name: PersistedString::from("Ada"),
        age: 30,
        role: Role::Admin,
        tags: PersistedVec::new(),
    };
    app.guard()
        .users_mut()
        .insert(PersistedString::from("ada"), ada);
    let bob = User {
        name: PersistedString::from("Bob"),
        age: 25,
        role: Role::Member,
        tags: PersistedVec::new(),
    };
    app.guard()
        .users_mut()
        .insert(PersistedString::from("bob"), bob);

    {
        let mut guard = app.guard();
        let mut users = guard.users_mut();
        users
            .get_mut(&PersistedString::from("ada"))
            .unwrap()
            .age_mut()
            .set(99);
    }

    app.flush();
    let reloaded = app.load();

    assert_eq!(
        reloaded
            .users
            .get(&PersistedString::from("ada"))
            .unwrap()
            .age,
        99
    );
    assert_eq!(
        reloaded
            .users
            .get(&PersistedString::from("bob"))
            .unwrap()
            .age,
        25
    );
    assert_eq!(
        reloaded
            .users
            .get(&PersistedString::from("bob"))
            .unwrap()
            .name,
        "Bob"
    );
}
