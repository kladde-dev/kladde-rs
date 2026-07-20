//! The crate application code actually depends on: opening/creating a
//! [`Kladde`] root value, and the concrete [`DefaultBackend`] (`Journal` +
//! `Allocator`) that backs it.
//!
//! v1 has no real file behind any of this -- see `spec.md`'s "Crash
//! Consistency" note and `V1_QUESTIONS.md` question 1. `DefaultBackend`
//! journals operations (serialized via `postcard`) into memory and
//! delegates allocation to [`kladde_alloc::MockAllocator`]; there is
//! deliberately no `open`/`create` taking a `Path`, since v1 has nothing
//! to open -- state lives only as long as the process does.

use kladde_alloc::MockAllocator;
use kladde_traits::{Allocator, Journal, Persistable, ResolvedPointer, UniquePointer};
use std::cell::{Ref, RefCell};

/// `Journal` + `Allocator` combined: `Journal` is a simple growing,
/// in-memory log of serialized `Op`s; `Allocator` is delegated entirely
/// to a [`MockAllocator`]. Real on-disk journal framing (a length prefix
/// and checksum per entry in one contiguous byte stream, per `spec.md`'s
/// "On-Disk Layout") is deferred to the real file-backed implementation --
/// there's no file here for framing to protect.
pub struct DefaultBackend {
    journal: RefCell<Vec<Vec<u8>>>,
    allocator: MockAllocator,
}

impl DefaultBackend {
    pub fn new() -> Self {
        DefaultBackend {
            journal: RefCell::new(Vec::new()),
            allocator: MockAllocator::new(),
        }
    }

    /// The postcard-serialized bytes of every op recorded so far, in
    /// order. Mainly for tests / introspection.
    pub fn journal_entries(&self) -> Ref<'_, Vec<Vec<u8>>> {
        self.journal.borrow()
    }
}

impl Default for DefaultBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Journal for DefaultBackend {
    fn record<T: Persistable>(&self, op: &T::Op) {
        let bytes = postcard::to_allocvec(op)
            .expect("postcard serialization of an in-memory Op should not fail");
        self.journal.borrow_mut().push(bytes);
    }
}

impl Allocator for DefaultBackend {
    fn alloc<T>(&self, size: usize) -> UniquePointer<T> {
        self.allocator.alloc(size)
    }

    fn free<T: Persistable>(&self, pointer: UniquePointer<T>) {
        self.allocator.free(pointer)
    }

    fn resolve<'a, T>(&'a self, pointer: &UniquePointer<T>) -> Option<ResolvedPointer<'a, T>> {
        self.allocator.resolve(pointer)
    }
}

/// A root [`Persistable`] value paired with its own [`DefaultBackend`] --
/// the entry point application code actually uses. Per `spec.md`'s
/// "`frontend`'s API shape" discussion, a `Kladde` holds exactly one root
/// value; a struct with named fields already gets most of the benefit of
/// multiple independent roots.
pub struct Kladde<T> {
    root: T,
    backend: DefaultBackend,
}

impl<T: Persistable> Kladde<T> {
    /// Wraps `root` with a fresh, empty `DefaultBackend`.
    pub fn new(root: T) -> Self {
        Kladde {
            root,
            backend: DefaultBackend::new(),
        }
    }

    /// Read-only access to the root value.
    pub fn get(&self) -> &T {
        &self.root
    }

    /// A `Guard` through which mutations to the root value are recorded
    /// and applied.
    pub fn guard(&mut self) -> T::Guard<'_, DefaultBackend> {
        self.root.guard(&self.backend)
    }

    pub fn backend(&self) -> &DefaultBackend {
        &self.backend
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kladde_traits::Guard;

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    enum CounterOp {
        Set(u32),
    }

    struct Counter(u32);

    impl Persistable for Counter {
        type Op = CounterOp;
        type Guard<'s, B: kladde_traits::Backend>
            = CounterGuard<'s, B>
        where
            Self: 's,
            B: 's;

        fn guard<'s, B: kladde_traits::Backend>(
            &'s mut self,
            backend: &'s B,
        ) -> Self::Guard<'s, B> {
            CounterGuard {
                inner: self,
                backend,
            }
        }
    }

    struct CounterGuard<'s, B> {
        inner: &'s mut Counter,
        backend: &'s B,
    }

    impl<'s, B: kladde_traits::Backend> CounterGuard<'s, B> {
        fn set(&mut self, value: u32) {
            self.backend.record::<Counter>(&CounterOp::Set(value));
            self.inner.0 = value;
        }
    }

    impl<'s, B: kladde_traits::Backend> Guard for CounterGuard<'s, B> {
        type Persistable = Counter;
        type Backend = B;

        fn as_persistable(&self) -> &Counter {
            self.inner
        }
        fn as_persistable_mut(&mut self) -> &mut Counter {
            self.inner
        }
        fn backend(&self) -> &B {
            self.backend
        }
    }

    #[test]
    fn journal_records_serialized_ops_in_order() {
        let backend = DefaultBackend::new();
        backend.record::<Counter>(&CounterOp::Set(1));
        backend.record::<Counter>(&CounterOp::Set(2));

        let entries = backend.journal_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            postcard::from_bytes::<CounterOp>(&entries[0]).unwrap(),
            CounterOp::Set(1)
        );
        assert_eq!(
            postcard::from_bytes::<CounterOp>(&entries[1]).unwrap(),
            CounterOp::Set(2)
        );
    }

    #[test]
    fn backend_delegates_allocation_to_the_mock_allocator() {
        let backend = DefaultBackend::new();
        let pointer = backend.alloc::<Counter>(4);
        assert!(backend.resolve(&pointer).is_some());
        backend.free(pointer);
    }

    #[test]
    fn kladde_mutation_updates_state_and_journals_the_op() {
        let mut kladde = Kladde::new(Counter(0));

        kladde.guard().set(42);

        assert_eq!(kladde.get().0, 42);
        assert_eq!(kladde.backend().journal_entries().len(), 1);
    }
}
