//! Shared `#[cfg(test)]`-only fixtures used across this crate's unit
//! tests.

use kladde_traits::{Allocator, Journal, Persistable, ResolvedPointer, UniquePointer};
use std::cell::{Cell, RefCell};

#[derive(Default)]
pub struct MockBackend {
    recorded: RefCell<usize>,
    next_index: Cell<u32>,
}

impl MockBackend {
    pub fn record_count(&self) -> usize {
        *self.recorded.borrow()
    }
}

impl Journal for MockBackend {
    fn record<T: Persistable>(&self, _op: &T::Op) {
        *self.recorded.borrow_mut() += 1;
    }
}

impl Allocator for MockBackend {
    fn alloc<T>(&self, _size: usize) -> UniquePointer<T> {
        let raw = self.next_index.get() + 1;
        self.next_index.set(raw);
        UniquePointer::from_index(std::num::NonZeroU32::new(raw).unwrap())
    }
    fn free<T: Persistable>(&self, _pointer: UniquePointer<T>) {}
    fn resolve<'a, T>(&'a self, _pointer: &UniquePointer<T>) -> Option<ResolvedPointer<'a, T>> {
        None
    }
}
