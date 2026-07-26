//! The crate application code actually depends on: opening/creating a
//! [`Kladde`] root value, and the concrete [`DefaultBackend`] that backs
//! it.
//!
//! v1 has no real file behind any of this -- see `spec.md`'s "Crash
//! Consistency" note. `DefaultBackend` journals a small, fixed set of
//! type-agnostic memory-management microoperations (see `spec.md`'s "The
//! Trait Layer"/"Flushing") into memory and, on [`Kladde::flush`], replays
//! them against [`kladde_alloc::MockAllocator`]; there is deliberately no
//! `open`/`create` taking a `Path`, since v1 has nothing to open -- state
//! lives only as long as the process does.

use kladde_alloc::MockAllocator;
use kladde_traits::{Allocator, Location, Persistable, RawPointer, ResolvedPointer, UniquePointer};
use std::cell::{Cell, RefCell};
use std::num::NonZeroU32;

// The schema/fingerprint surface, so an application that depends on `kladde`
// for its root value can inspect that root type's schema
// (`T::schema()`/`T::fingerprint()`) without naming `kladde-schema` or
// `kladde-traits` directly.
pub use kladde_traits::{
    Field, Fingerprint, SchemaBuilder, TypeDescriptor, TypeRef, TypeTable, Variant, Version,
};

/// The five microoperations `spec.md`'s journal ever records -- nothing
/// type-specific, purely a byte-level effect on the allocator. `Alloc`'s
/// `index` is decided (by `DefaultBackend::alloc`'s own counter) at the
/// moment the entry is created, not during replay -- see the "which
/// instance" discussion this design is built on.
#[derive(Debug, Clone, PartialEq)]
enum Microop {
    Alloc {
        index: NonZeroU32,
        size: usize,
    },
    Free {
        index: NonZeroU32,
    },
    Write {
        index: NonZeroU32,
        offset: u32,
        bytes: Vec<u8>,
    },
    Copy {
        src: NonZeroU32,
        src_offset: u32,
        len: u32,
        dst: NonZeroU32,
        dst_offset: u32,
    },
    Resize {
        index: NonZeroU32,
        new_size: usize,
    },
}

/// The concrete [`Allocator`] implementation `Kladde` uses. Owns the
/// index-generation counter and the in-memory journal of `Microop`s;
/// delegates actual byte storage to a [`MockAllocator`], but only when
/// [`DefaultBackend::flush`] replays the journal into it -- calling
/// `Allocator`'s methods on a `DefaultBackend` never touches the
/// `MockAllocator` directly, it only ever appends to the journal (plus,
/// for `alloc`, minting a fresh index immediately).
pub struct DefaultBackend {
    journal: RefCell<Vec<Microop>>,
    next_index: Cell<u32>,
    allocator: MockAllocator,
}

impl DefaultBackend {
    pub fn new() -> Self {
        DefaultBackend {
            journal: RefCell::new(Vec::new()),
            next_index: Cell::new(0),
            allocator: MockAllocator::new(),
        }
    }

    /// Number of not-yet-flushed microoperations. Mainly for tests /
    /// introspection.
    pub fn journal_len(&self) -> usize {
        self.journal.borrow().len()
    }

    /// Number of currently-materialized (flushed, not yet freed)
    /// allocations. Mainly for tests asserting that a mutation didn't
    /// leak an allocation it should have reused or freed.
    pub fn live_count(&self) -> usize {
        self.allocator.live_count()
    }

    /// Replays every recorded microoperation against the underlying
    /// `MockAllocator`, in order, then drains the journal. See
    /// `spec.md`'s "Flushing": since the journal already holds nothing
    /// but microoperations, this never needs to call back into any
    /// data-type implementation.
    ///
    /// Public so tests/other crates can flush a bare `DefaultBackend`
    /// directly without going through `Kladde`; application code should
    /// generally prefer `Kladde::flush`, which additionally gets the
    /// borrow-checker guarantee that no `Guard` is still live (see its
    /// doc comment).
    pub fn flush(&self) {
        for op in self.journal.borrow_mut().drain(..) {
            match op {
                Microop::Alloc { index, size } => self.allocator.materialize_alloc(index, size),
                Microop::Free { index } => self.allocator.materialize_free(index),
                Microop::Write {
                    index,
                    offset,
                    bytes,
                } => self.allocator.materialize_write(index, offset, &bytes),
                Microop::Copy {
                    src,
                    src_offset,
                    len,
                    dst,
                    dst_offset,
                } => self
                    .allocator
                    .materialize_copy(src, src_offset, len, dst, dst_offset),
                Microop::Resize { index, new_size } => {
                    self.allocator.materialize_resize(index, new_size)
                }
            }
        }
    }
}

impl Default for DefaultBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Allocator for DefaultBackend {
    fn alloc<T>(&self, size: usize) -> UniquePointer<T> {
        let raw = self
            .next_index
            .get()
            .checked_add(1)
            .expect("DefaultBackend index space exhausted");
        self.next_index.set(raw);
        let index = NonZeroU32::new(raw).unwrap();
        self.journal
            .borrow_mut()
            .push(Microop::Alloc { index, size });
        UniquePointer::from_index(index)
    }

    fn free<T>(&self, pointer: UniquePointer<T>) {
        self.journal.borrow_mut().push(Microop::Free {
            index: pointer.index(),
        });
    }

    fn resolve<'a, T>(&'a self, pointer: &UniquePointer<T>) -> Option<ResolvedPointer<'a, T>> {
        self.allocator
            .resolve(pointer.index())
            .map(ResolvedPointer::from_target)
    }

    fn read(&self, target: RawPointer, offset: u32, len: u32) -> Vec<u8> {
        self.allocator.read(target.index(), offset, len)
    }

    fn write(&self, target: RawPointer, offset: u32, bytes: &[u8]) {
        self.journal.borrow_mut().push(Microop::Write {
            index: target.index(),
            offset,
            bytes: bytes.to_vec(),
        });
    }

    fn copy(&self, src: RawPointer, src_offset: u32, len: u32, dst: RawPointer, dst_offset: u32) {
        self.journal.borrow_mut().push(Microop::Copy {
            src: src.index(),
            src_offset,
            len,
            dst: dst.index(),
            dst_offset,
        });
    }

    fn resize<T>(&self, pointer: &UniquePointer<T>, new_size: usize) {
        self.journal.borrow_mut().push(Microop::Resize {
            index: pointer.index(),
            new_size,
        });
    }
}

/// A root [`Persistable`] value paired with its own [`DefaultBackend`] --
/// the entry point application code actually uses.
///
/// Unlike every other `Persistable` value, the root gets its own
/// allocation *eagerly*, in [`Kladde::new`] -- it doesn't have the
/// "constructed without a backend in hand" problem that makes every other
/// container's pointer lazy, since `root` and `backend` are brought
/// together in the same call. See `spec.md`'s "Pointers and Memory
/// Management".
pub struct Kladde<T> {
    root: T,
    backend: DefaultBackend,
    root_pointer: UniquePointer<T>,
}

impl<T: Persistable> Kladde<T> {
    /// Wraps `root` with a fresh, empty `DefaultBackend`, allocating the
    /// root's own storage immediately.
    pub fn new(root: T) -> Self {
        let backend = DefaultBackend::new();
        let root_pointer = backend.alloc::<T>(T::INLINE_SIZE);
        Kladde {
            root,
            backend,
            root_pointer,
        }
    }

    /// Read-only access to the root value.
    pub fn get(&self) -> &T {
        &self.root
    }

    /// A `Guard` through which mutations to the root value are recorded
    /// and applied.
    pub fn guard(&mut self) -> T::Guard<'_, DefaultBackend> {
        let location = Location {
            anchor: self.root_pointer.raw(),
            offset: 0,
        };
        self.root.guard(&self.backend, location)
    }

    pub fn backend(&self) -> &DefaultBackend {
        &self.backend
    }

    /// Replays every not-yet-flushed microoperation against the backend's
    /// allocator. Takes `&mut self` deliberately: `Guard`s are only ever
    /// obtainable via `&mut self` too, so the borrow checker guarantees a
    /// flush can never run while a (possibly multi-microop, not yet fully
    /// recorded) mutation is still in progress -- see `spec.md`'s Crash
    /// Consistency section.
    pub fn flush(&mut self) {
        self.backend.flush();
    }

    /// Reconstructs a fresh `T` purely from the backend's (post-flush)
    /// storage -- no reference to `self.root`. The primary correctness
    /// test for flushing: this should equal `self.root` after a flush.
    pub fn load(&self) -> T {
        let location = Location {
            anchor: self.root_pointer.raw(),
            offset: 0,
        };
        T::load(&self.backend, location)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kladde_traits::Guard;

    struct Counter(u32);

    impl Persistable for Counter {
        const INLINE_SIZE: usize = 4;

        type Guard<'s, B: kladde_traits::Backend>
            = CounterGuard<'s, B>
        where
            Self: 's,
            B: 's;

        fn guard<'s, B: kladde_traits::Backend>(
            &'s mut self,
            backend: &'s B,
            location: Location,
        ) -> Self::Guard<'s, B> {
            CounterGuard {
                inner: self,
                backend,
                location,
            }
        }

        fn store<B: kladde_traits::Backend>(&mut self, backend: &B, location: Location) {
            backend.write(location.anchor, location.offset, &self.0.to_le_bytes());
        }

        fn load<B: kladde_traits::Backend>(backend: &B, location: Location) -> Self {
            let bytes = backend.read(location.anchor, location.offset, 4);
            Counter(u32::from_le_bytes(bytes.try_into().unwrap()))
        }

        fn describe_local(
            _builder: &mut kladde_traits::SchemaBuilder,
        ) -> kladde_traits::TypeDescriptor {
            kladde_traits::TypeDescriptor::Primitive(kladde_traits::Primitive::U32)
        }
    }

    struct CounterGuard<'s, B> {
        inner: &'s mut Counter,
        backend: &'s B,
        location: Location,
    }

    impl<'s, B: kladde_traits::Backend> CounterGuard<'s, B> {
        fn set(&mut self, value: u32) {
            self.backend.write(
                self.location.anchor,
                self.location.offset,
                &value.to_le_bytes(),
            );
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
    fn mutation_updates_state_and_journals_microops() {
        let mut kladde = Kladde::new(Counter(0));

        kladde.guard().set(42);

        assert_eq!(kladde.get().0, 42);
        // One `Alloc` (the root, from `Kladde::new`) plus one `Write`
        // (the `set` call) are still sitting in the journal, unflushed.
        assert_eq!(kladde.backend().journal_len(), 2);
    }

    #[test]
    fn flush_drains_the_journal_and_makes_load_reflect_the_mutation() {
        let mut kladde = Kladde::new(Counter(0));
        kladde.guard().set(7);

        kladde.flush();

        assert_eq!(kladde.backend().journal_len(), 0);
        assert_eq!(kladde.load().0, 7);
    }

    #[test]
    fn load_before_any_flush_would_read_unmaterialized_storage() {
        // Documenting current behavior rather than asserting a
        // requirement: nothing has been flushed yet, so the root's
        // allocation was only *minted*, never *materialized* -- reading
        // it is a logic error in application code (flush first), not
        // something `load` is expected to handle gracefully.
        let kladde = Kladde::new(Counter(0));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| kladde.load()));
        assert!(result.is_err());
    }
}
