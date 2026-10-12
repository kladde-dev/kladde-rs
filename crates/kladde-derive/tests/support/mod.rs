//! Shared fixtures for exercising `#[derive(Persistable)]`-generated code: a
//! value in a file in memory, and the checks that its sequence holds the
//! value's encoding, before and after reopening.
//!
//! Not every item is used by every test binary that includes this module
//! (each `tests/*.rs` file compiles as its own crate).
#![allow(dead_code)]

use kladde::{Kladde, MemoryStorage, Options, Persistable, TypeRef};

/// A value in a file in memory.
pub struct Fixture<T: Persistable + 'static> {
    pub kladde: Kladde<T>,
    storage: MemoryStorage,
}

impl<T: Persistable + 'static> Fixture<T> {
    pub fn new(value: T) -> Fixture<T> {
        let storage = MemoryStorage::new();
        let kladde =
            Kladde::create_in(Box::new(storage.clone()), value, Options::default()).unwrap();
        Fixture { kladde, storage }
    }

    /// Checks that the file holds the value's encoding, which its schema
    /// measures whole, and that the file reopens to the same encoding.
    pub fn check(&mut self) {
        self.kladde.flush().unwrap();
        let store = self.kladde.store();
        store.check();
        let bytes = store.read_all().unwrap();
        assert_eq!(bytes, self.kladde.get().to_bytes(), "the sequence");
        assert_eq!(
            T::schema().measure(TypeRef(0), &bytes),
            Some(bytes.len()),
            "the schema measures the sequence"
        );
        let image = MemoryStorage::from_image(self.storage.image());
        let reopened = Kladde::<T>::open_in(Box::new(image), Options::default()).unwrap();
        assert_eq!(reopened.get().to_bytes(), bytes, "after reopening");
    }
}
