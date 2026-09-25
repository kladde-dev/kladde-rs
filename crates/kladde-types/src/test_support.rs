//! Shared fixtures for this crate's container tests: a store in memory, and a
//! root allocation to put the value under test in.

use kladde_persist::{Location, Persistable, Pointer, WriteBackend};
use kladde_store::{MemoryStorage, Store};

pub struct Fixture {
    pub store: Store,
    pub location: Location<Pointer, u32>,
}

impl Fixture {
    /// A store whose root allocation holds `size` bytes.
    pub fn new(size: usize) -> Fixture {
        let store = Store::create(Box::new(MemoryStorage::new()), Default::default()).unwrap();
        let root = store.alloc(size as u32).unwrap();
        Fixture {
            store,
            location: Location::new(root.raw(), 0),
        }
    }

    /// Flushes, then loads a `T` from the root allocation.
    pub fn reload<T: Persistable>(&mut self) -> T {
        self.store.flush().unwrap();
        self.store.check();
        T::load(&mut self.store, self.location).unwrap()
    }
}
