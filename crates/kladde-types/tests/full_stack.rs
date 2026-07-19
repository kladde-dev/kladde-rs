//! Exercises the whole stack together: `#[derive(Persistable)]` on a
//! custom struct/enum, nesting `kladde-types` containers, backed by
//! `kladde`'s real `Kladde`/`DefaultBackend` (not a test-local mock).
//!
//! Note the double-derive on `User`/`Role` below: `#[derive(Persistable)]`
//! alone is enough to make a type *mutable* through a `Guard`, but a type
//! also needs `Serialize`/`Deserialize` to be usable as a *container
//! element* (`PersistedVec::push`, `PersistedHashMap::insert`), since
//! inserting a brand-new element necessarily records its whole value in
//! the journal -- see `spec.md`'s "The Trait Layer". A plain derived
//! struct (`Op = ()`) doesn't get `Serialize` for free.

use kladde::Kladde;
use kladde_types::{Persistable, PersistedHashMap, PersistedVec};

#[derive(Persistable, Clone, serde::Serialize, serde::Deserialize, Debug, PartialEq)]
enum Role {
    Admin,
    Member,
}

#[derive(Persistable, Clone, serde::Serialize, serde::Deserialize)]
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

    // Insert a fresh user (one op: `Insert`).
    app.guard().users_mut().insert(
        "ada".to_string(),
        User {
            name: "Ada".to_string(),
            age: 30,
            role: Role::Admin,
            tags: PersistedVec::new(),
        },
    );

    // Mutate it through the derived, nested Guards (three more ops: two
    // scalar `Set`s and a `Push`).
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
    assert_eq!(ada.age, 31);
    assert_eq!(ada.role, Role::Member);
    assert_eq!(ada.tags.len(), 1);
    assert_eq!(ada.tags.get(0), Some(&"engineer".to_string()));

    assert_eq!(app.backend().journal_entries().len(), 4);
}
