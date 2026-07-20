//! Core vocabulary shared by every other `kladde-*` crate: the traits that
//! define what it means for a type to be persistable, the guard pattern
//! used for mutation, and the pointer types used to reference allocated
//! regions in the backed heap.
//!
//! See `spec.md` (in the repository root) for the full design rationale.

use std::marker::PhantomData;
use std::num::NonZeroU32;

mod scalar;
pub use scalar::*;

/// The offset of a pointer's own serialized bytes within the file.
/// Type alias rather than a bare integer so widening it later (to `u64`,
/// or to a variable-length encoding) only touches one definition.
pub type Position = NonZeroU32;

/// The offset of the region a pointer refers to.
pub type Target = NonZeroU32;

/// The size, in bytes, of an allocated region.
pub type Size = NonZeroU32;

/// A stable, in-memory-only handle to an allocated region, assigned by an
/// [`Allocator`]. Deliberately small and opaque: it does *not* hold a
/// reference to any [`Backend`], so it can't free itself in `Drop` -- see
/// the "Freeing" section of `spec.md` for why that's fine (generated
/// [`Guard`] wrapper types are responsible for freeing the
/// `UniquePointer`s they own).
///
/// `index` is a stable identity assigned once by `Allocator` and never
/// changes for the lifetime of this pointer -- even though the region it
/// (eventually) refers to may move around during compaction, and even
/// though this pointer may not have been flushed to the snapshot at all
/// yet.
///
/// Deliberately *not* `Clone`/`Copy` (so double-freeing is a compile-time
/// impossibility, matching the single-owner design) and *not*
/// `Serialize`/`Deserialize` (`index` is meaningless outside the process
/// that assigned it) -- see [`ResolvedPointer`] for how a `UniquePointer`
/// embedded in some other value actually gets written out.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct UniquePointer<T> {
    index: NonZeroU32,
    _marker: PhantomData<*const T>,
}

impl<T> UniquePointer<T> {
    /// Only [`Allocator`] implementations are expected to call this --
    /// `index` should always come from an allocator's own counter.
    pub fn from_index(index: NonZeroU32) -> Self {
        UniquePointer {
            index,
            _marker: PhantomData,
        }
    }

    pub fn index(&self) -> NonZeroU32 {
        self.index
    }
}

/// The serializable counterpart of [`UniquePointer`], produced by
/// [`Allocator::resolve`]. Holds the pointer's current on-disk `target` --
/// the offset of the region it points *at*, which is what a pointer's
/// serialized value actually is (as opposed to `position`, the offset of
/// the pointer's *own* serialized bytes, which is separate bookkeeping an
/// `Allocator` tracks internally once this value has actually been
/// written out somewhere -- not something `ResolvedPointer` itself knows).
///
/// Borrowing the allocator for `'a` is deliberate: it prevents, at compile
/// time, any allocator operation that could invalidate this snapshot
/// (most importantly, compaction moving the target) for as long as a
/// `ResolvedPointer` derived from it is still alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedPointer<'a, T> {
    target: NonZeroU32,
    _allocator: PhantomData<&'a ()>,
    _marker: PhantomData<*const T>,
}

impl<'a, T> ResolvedPointer<'a, T> {
    pub fn from_target(target: NonZeroU32) -> Self {
        ResolvedPointer {
            target,
            _allocator: PhantomData,
            _marker: PhantomData,
        }
    }

    pub fn target(&self) -> NonZeroU32 {
        self.target
    }
}

impl<'a, T> serde::Serialize for ResolvedPointer<'a, T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.target.serialize(serializer)
    }
}

/// Records operations to the journal. Appending may itself require
/// allocation (growing the journal's own storage, or a single oversized
/// `Op`) -- the reason [`Journal`] and [`Allocator`] are kept as two
/// traits unified by one [`Backend`] bound, rather than two independent
/// type parameters: a `Journal` implementation generally needs the same
/// `Allocator` the rest of the system uses, so splitting them into
/// separately-mixable type parameters wouldn't actually decouple them.
pub trait Journal {
    fn record<T: Persistable>(&self, op: &T::Op);
}

/// Allocates and frees regions in the backed heap, and resolves
/// [`UniquePointer`]s to their current target.
pub trait Allocator {
    /// Allocates a fresh region of `size` bytes, returning a pointer that
    /// uniquely (and, for now, exclusively in-memory) identifies it.
    fn alloc<T>(&self, size: usize) -> UniquePointer<T>;

    /// Frees the region `pointer` identifies. See the "Freeing" section
    /// of `spec.md`: this doesn't necessarily touch live allocator state
    /// synchronously -- a real, file-backed `Allocator` would journal the
    /// free and apply it at the next flush.
    fn free<T: Persistable>(&self, pointer: UniquePointer<T>);

    /// `None` if `pointer` hasn't been flushed yet (no target exists to
    /// resolve to). A real `Allocator` also needs some way to go the
    /// other direction -- reconstructing a `UniquePointer` (with a freshly
    /// assigned `index`) from a `target` read back off disk -- but that's
    /// a plain method call, not something threaded through `Deserialize`,
    /// so it isn't part of this trait's read path.
    fn resolve<'a, T>(&'a self, pointer: &UniquePointer<T>) -> Option<ResolvedPointer<'a, T>>;
}

/// The bound every [`Persistable`]/[`Guard`] is generic over. A blanket
/// impl means concrete backend types only ever need to implement
/// [`Journal`] and [`Allocator`] separately; application code names
/// `Backend`, not the two halves.
pub trait Backend: Journal + Allocator {}
impl<B: Journal + Allocator> Backend for B {}

/// A plain, in-memory value type that *could* be persisted -- it has the
/// right shape (an `Op` log, a paired [`Guard`] view) -- but isn't yet
/// tied to any backend. Only ever accessed read-only; see `Guard` for
/// mutation.
pub trait Persistable: Sized {
    /// Plain (non-lifetime-parameterized) and owned, deliberately, for
    /// v1 -- see `spec.md`'s "The Trait Layer" for why, and Future Work
    /// for the lifetime-GAT version this is expected to eventually become.
    type Op: serde::Serialize + serde::de::DeserializeOwned;

    type Guard<'s, B: Backend>: Guard<Persistable = Self, Backend = B>
    where
        Self: 's,
        B: 's;

    /// Borrows both `self` and a `Backend` for `'s`, producing a `Guard`
    /// through which mutations are recorded and applied.
    fn guard<'s, B: Backend>(&'s mut self, backend: &'s B) -> Self::Guard<'s, B>;
}

/// A live, mutation-capable, RAII-style view onto a [`Persistable`]
/// value, tied to a [`Backend`] for its lifetime -- the write-side
/// counterpart of `Persistable`, in the same relationship `MutexGuard`
/// has to `Mutex`. Rarely implemented manually; usually generated by
/// `#[derive(Persistable)]`.
pub trait Guard {
    type Persistable: Persistable;
    type Backend: Backend;

    fn as_persistable(&self) -> &Self::Persistable;
    fn as_persistable_mut(&mut self) -> &mut Self::Persistable;
    fn backend(&self) -> &Self::Backend;
}

// Note: there is deliberately no blanket `impl<T: Guard> Deref for T`
// here. `Guard` is a foreign trait from the point of view of any
// downstream crate, so `Self` in a blanket impl over it would be a fully
// generic, uncovered type parameter -- exactly what Rust's orphan rules
// forbid (E0210), regardless of the trait bound on it. Instead, each
// concrete guard type gets its own `Deref`/`DerefMut` impl generated
// alongside it, since those are local types in whatever crate the macro
// (or hand-written container implementation) expands in.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_pointer_round_trips_its_index() {
        let index = NonZeroU32::new(7).unwrap();
        let pointer = UniquePointer::<()>::from_index(index);
        assert_eq!(pointer.index(), index);
    }

    #[test]
    fn resolved_pointer_serializes_as_a_plain_target() {
        let target = NonZeroU32::new(42).unwrap();
        let resolved = ResolvedPointer::<()>::from_target(target);

        // The whole point of `ResolvedPointer` is that it serializes
        // exactly like the raw `target` would -- no extra framing, no
        // trace of the allocator borrow it carries at the type level.
        let resolved_bytes = postcard::to_allocvec(&resolved).unwrap();
        let target_bytes = postcard::to_allocvec(&target).unwrap();
        assert_eq!(resolved_bytes, target_bytes);

        let round_tripped: NonZeroU32 = postcard::from_bytes(&resolved_bytes).unwrap();
        assert_eq!(round_tripped, target);
    }

    // A minimal, self-contained mock of a `Persistable`/`Guard`/`Backend`
    // triple, exercised only to prove the trait definitions actually
    // compose the way real implementations (kladde-alloc, kladde-types)
    // are expected to use them -- not a real container.
    mod mock_usage {
        use super::*;
        use std::cell::RefCell;

        #[derive(serde::Serialize, serde::Deserialize)]
        enum CounterOp {
            Set(u32),
        }

        struct Counter(u32);

        impl Persistable for Counter {
            type Op = CounterOp;
            type Guard<'s, B: Backend>
                = CounterGuard<'s, B>
            where
                Self: 's,
                B: 's;

            fn guard<'s, B: Backend>(&'s mut self, backend: &'s B) -> Self::Guard<'s, B> {
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

        impl<'s, B: Backend> CounterGuard<'s, B> {
            fn set(&mut self, value: u32) {
                self.backend.record::<Counter>(&CounterOp::Set(value));
                self.inner.0 = value;
            }
        }

        impl<'s, B: Backend> Guard for CounterGuard<'s, B> {
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

        #[derive(Default)]
        struct MockBackend {
            recorded: RefCell<Vec<u32>>,
        }

        impl Journal for MockBackend {
            fn record<T: Persistable>(&self, _op: &T::Op) {
                // Real journals would serialize `op`; this mock just
                // counts calls to prove the plumbing works.
                self.recorded.borrow_mut().push(1);
            }
        }

        impl Allocator for MockBackend {
            fn alloc<T>(&self, _size: usize) -> UniquePointer<T> {
                UniquePointer::from_index(NonZeroU32::new(1).unwrap())
            }
            fn free<T: Persistable>(&self, _pointer: UniquePointer<T>) {}
            fn resolve<'a, T>(
                &'a self,
                _pointer: &UniquePointer<T>,
            ) -> Option<ResolvedPointer<'a, T>> {
                None
            }
        }

        #[test]
        fn traits_compose_end_to_end() {
            let backend = MockBackend::default();
            let mut counter = Counter(0);

            let mut guard = counter.guard(&backend);
            guard.set(5);
            assert_eq!(guard.as_persistable().0, 5);
            assert_eq!(backend.recorded.borrow().len(), 1);
        }
    }
}
