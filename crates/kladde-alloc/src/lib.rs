//! A mock, purely in-memory implementation of [`kladde_traits::Allocator`].
//!
//! v1 deliberately defers the real, file-backed, compaction-capable
//! allocator (see `spec.md`'s "Pointers and Memory Management" and
//! `V1_QUESTIONS.md`): the plan is to build the rest of the system
//! against this mock first, and let its real requirements emerge from
//! that rather than guessing upfront.

use kladde_traits::{Allocator, Persistable, ResolvedPointer, UniquePointer};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::num::NonZeroU32;

/// Each allocation is a `Box<[u8]>`, identified by an ID from a simple
/// incrementing counter, held in a `HashMap<Id, Box<[u8]>>` -- no
/// compaction, no real file, no journal/flush distinction.
///
/// Because there's no real flush pipeline, [`resolve`](Allocator::resolve)
/// never returns `None` the way a real `Allocator`'s would for a
/// not-yet-flushed pointer -- the mock has no "not yet flushed" state, so
/// every live allocation is immediately resolvable. It reuses the
/// pointer's own `index` as a stand-in `target`, since there's no real
/// file to compute an offset against.
#[derive(Default)]
pub struct MockAllocator {
    regions: RefCell<HashMap<NonZeroU32, Box<[u8]>>>,
    next_index: Cell<u32>,
}

impl MockAllocator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of currently-live (not yet freed) allocations -- mainly
    /// useful for tests and leak-checking.
    pub fn live_count(&self) -> usize {
        self.regions.borrow().len()
    }
}

impl Allocator for MockAllocator {
    fn alloc<T>(&self, size: usize) -> UniquePointer<T> {
        let raw = self
            .next_index
            .get()
            .checked_add(1)
            .expect("MockAllocator index space exhausted");
        self.next_index.set(raw);
        // `raw` starts at 1 and only increases, so this is infallible.
        let index = NonZeroU32::new(raw).unwrap();

        self.regions
            .borrow_mut()
            .insert(index, vec![0u8; size].into_boxed_slice());
        UniquePointer::from_index(index)
    }

    fn free<T: Persistable>(&self, pointer: UniquePointer<T>) {
        let existed = self.regions.borrow_mut().remove(&pointer.index()).is_some();
        assert!(
            existed,
            "MockAllocator::free called with a pointer it didn't allocate, or that was already freed"
        );
    }

    fn resolve<'a, T>(&'a self, pointer: &UniquePointer<T>) -> Option<ResolvedPointer<'a, T>> {
        if self.regions.borrow().contains_key(&pointer.index()) {
            Some(ResolvedPointer::from_target(pointer.index()))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kladde_traits::{Backend, Guard};

    // `Allocator::free`/`resolve` are bounded on `T: Persistable`, so
    // tests need *some* Persistable type as the pointer target -- this
    // one is never actually constructed or guarded, just named.
    #[derive(serde::Serialize, serde::Deserialize)]
    struct DummyOp;

    struct Dummy;

    impl Persistable for Dummy {
        type Op = DummyOp;
        type Guard<'s, B: Backend>
            = DummyGuard<'s, B>
        where
            Self: 's,
            B: 's;

        fn guard<'s, B: Backend>(&'s mut self, backend: &'s B) -> Self::Guard<'s, B> {
            DummyGuard {
                inner: self,
                backend,
            }
        }
    }

    struct DummyGuard<'s, B> {
        inner: &'s mut Dummy,
        backend: &'s B,
    }

    impl<'s, B: Backend> Guard for DummyGuard<'s, B> {
        type Persistable = Dummy;
        type Backend = B;

        fn as_persistable(&self) -> &Dummy {
            self.inner
        }
        fn as_persistable_mut(&mut self) -> &mut Dummy {
            self.inner
        }
        fn backend(&self) -> &B {
            self.backend
        }
    }

    #[test]
    fn alloc_gives_unique_increasing_indices() {
        let alloc = MockAllocator::new();
        let a = alloc.alloc::<Dummy>(4);
        let b = alloc.alloc::<Dummy>(4);
        assert_ne!(a.index(), b.index());
        assert!(b.index() > a.index());
    }

    #[test]
    fn resolve_succeeds_for_a_live_allocation() {
        let alloc = MockAllocator::new();
        let pointer = alloc.alloc::<Dummy>(8);

        let resolved = alloc.resolve(&pointer).expect("just allocated, should resolve");
        // The mock stands the index in for the target -- see the doc
        // comment on `MockAllocator`.
        assert_eq!(resolved.target(), pointer.index());
    }

    #[test]
    fn resolve_fails_after_free() {
        let alloc = MockAllocator::new();
        let pointer = alloc.alloc::<Dummy>(8);
        let index = pointer.index();

        alloc.free(pointer);

        // Can't call `alloc.resolve(&pointer)` any more -- `free` took
        // `pointer` by value, so the borrow checker already prevents use
        // after free here. Reconstruct a pointer with the same index to
        // confirm the *allocator's* bookkeeping also reflects the free
        // (not just that we no longer hold a live `UniquePointer`).
        let stale = UniquePointer::<Dummy>::from_index(index);
        assert!(alloc.resolve(&stale).is_none());
    }

    #[test]
    fn live_count_tracks_alloc_and_free() {
        let alloc = MockAllocator::new();
        assert_eq!(alloc.live_count(), 0);

        let a = alloc.alloc::<Dummy>(4);
        let _b = alloc.alloc::<Dummy>(4);
        assert_eq!(alloc.live_count(), 2);

        alloc.free(a);
        assert_eq!(alloc.live_count(), 1);
    }

    #[test]
    #[should_panic(expected = "didn't allocate")]
    fn freeing_an_unknown_pointer_panics() {
        let alloc = MockAllocator::new();
        let bogus = UniquePointer::<Dummy>::from_index(NonZeroU32::new(999).unwrap());
        alloc.free(bogus);
    }
}
