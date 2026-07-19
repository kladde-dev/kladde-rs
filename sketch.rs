// CRATE `traits` =============================================================
use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::ops::{Deref, DerefMut};

/// Assigned by `Allocator::alloc`. Deliberately small and opaque: it does
/// *not* hold a reference to any `Backend`, so it can't free itself in
/// `Drop` -- see spec.md's "Freeing" section for why that's fine (the
/// generated `Guard` wrapper types are responsible for freeing the
/// `UniquePointer`s they own).
///
/// `index` is a stable identity assigned once by `Allocator` and never
/// changes for the lifetime of this pointer -- even though the region it
/// (eventually) refers to may move around on disk during compaction, and
/// even though this pointer may not have been flushed to the snapshot yet
/// at all. `Allocator` resolves `index` to the pointer's current
/// `position` (where it's serialized in the snapshot, once flushed) and
/// `target` (where the pointed-to region currently sits, once its own
/// allocation is flushed); both start out `None` and become `Some` the
/// first time a flush touches them.
///
/// Variance: `PhantomData<*const T>` rather than plain `PhantomData<T>`.
/// Both give covariance in `T`, which is sound here -- mutation only ever
/// happens through a `Guard` requiring exclusive access, never through a
/// shared reference the way `Cell<T>`'s interior mutability does, so the
/// aliasing+mutation+covariance combination that forces `Cell<T>` to be
/// invariant doesn't apply. `*const T` is preferred over plain `T` because
/// `UniquePointer` never actually runs `T`'s destructor (freeing just
/// discards a byte range), so it shouldn't impose `PhantomData<T>`'s
/// "may drop a T" (drop-check) obligation.
///
/// Deliberately *not* `Serialize`/`Deserialize` -- `index` is meaningless
/// outside the process that assigned it. See `ResolvedPointer` for how a
/// `UniquePointer` embedded in some other value actually gets written out.
pub struct UniquePointer<T> {
    index: NonZeroU32,
    _marker: PhantomData<*const T>,
}

/// The serializable counterpart of `UniquePointer`, produced by
/// `Allocator::resolve`. Holds the pointer's current on-disk `target` --
/// the offset of the region it points *at*, which is what a pointer's
/// serialized value actually is (as opposed to `position`, the offset of
/// the pointer's *own* serialized bytes, which is separate bookkeeping
/// `Allocator` tracks internally once this value has actually been
/// written out somewhere -- not something `ResolvedPointer` itself knows).
/// Borrowing `Allocator` for `'a` is deliberate: it prevents, at compile
/// time, any `Allocator` operation that could invalidate this snapshot
/// (most importantly, compaction moving the target) for as long as a
/// `ResolvedPointer` derived from it is still alive -- the borrow checker
/// enforces that a serialized `target` was still current when it was
/// written.
pub struct ResolvedPointer<'a, T> {
    target: NonZeroU32,
    _allocator: PhantomData<&'a ()>,
    _marker: PhantomData<*const T>,
}

impl<'a, T> serde::Serialize for ResolvedPointer<'a, T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.target.serialize(serializer)
    }
}

/// Records operations to the journal. Appending may itself require
/// allocation (growing the journal's own storage, or a single oversized
/// `Op`) -- which is the reason `Journal` and `Allocator` are kept as two
/// traits unified by one `Backend` bound, rather than two independent type
/// parameters: a `Journal` implementation will generally need the same
/// `Allocator` the rest of the system uses, so splitting them into
/// separately-mixable type parameters wouldn't actually decouple them.
pub trait Journal {
    fn record<T: Persistable>(&self, op: &T::Op);
}

/// Allocates and frees regions in the backed heap, and resolves
/// `UniquePointer`s to their current `(position, target)`. The concrete
/// registry (index-keyed table + position-ordered range-query structure,
/// see spec.md) is intentionally not sketched here yet -- see "mock
/// `Allocator`" under spec.md's Open Questions.
pub trait Allocator {
    fn free<T: Persistable>(&self, pointer: UniquePointer<T>);

    /// `None` if `pointer` hasn't been flushed yet (no `target` exists
    /// to resolve to). Deserializing goes the other way without needing
    /// serde's stateful-deserialization machinery at all: the raw
    /// `target` is read back as an ordinary integer via plain `Deserialize`,
    /// and a fresh `UniquePointer` (with a newly assigned `index`) is
    /// minted for it via a separate, plain `Allocator` method (not
    /// sketched here yet), not via `Deserialize` itself.
    fn resolve<'a, T>(&'a self, pointer: &UniquePointer<T>) -> Option<ResolvedPointer<'a, T>>;
}

/// The bound every `Persistable`/`Guard` is generic over. A blanket impl
/// means concrete backend types only ever need to implement `Journal` and
/// `Allocator` separately; application code names `Backend`, not the two
/// halves.
pub trait Backend: Journal + Allocator {}
impl<B: Journal + Allocator> Backend for B {}

/// A plain, in-memory value type that *could* be persisted -- it has the
/// right shape (an `Op` log, a paired `Guard` view) -- but isn't yet tied
/// to any backend. Only ever accessed read-only; see `Guard` for mutation.
pub trait Persistable {
    /// Plain (non-lifetime-parameterized) and owned, deliberately, for v1:
    /// the alternative -- a lifetime-parameterized `type Op<'a>`, letting
    /// e.g. `Vec::push` record a *reference* to the value it just pushed
    /// instead of cloning it -- is real complexity (it would propagate
    /// into `Journal::record`'s signature, and the derive macro would need
    /// to generate correct GAT impls for every derived type). v1 accepts
    /// the clone cost instead (see `VecGuard::push` below, which needs
    /// `T: Clone`) to keep the first end-to-end implementation simpler;
    /// see spec.md's Future Work for revisiting this.
    type Op: serde::Serialize + serde::de::DeserializeOwned;

    type Guard<'s, B: Backend>: Guard;

    /// Borrows both `self` and a `Backend` for `'s`, producing a `Guard`
    /// through which mutations are recorded and applied.
    fn guard<'s, B: Backend>(&'s mut self, backend: &'s B) -> Self::Guard<'s, B>;
}

/// A live, mutation-capable, RAII-style view onto a `Persistable` value,
/// tied to a `Backend` for its lifetime -- the write-side counterpart of
/// `Persistable`, in the same relationship `MutexGuard` has to `Mutex`.
/// Rarely implemented manually; usually generated by `#[derive(Persistable)]`.
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
// concrete guard type (e.g. `VecGuard` below) gets its own `Deref`/
// `DerefMut` impl generated alongside it, since those are local types in
// whatever crate the macro expands in.

// CRATE `types` ==============================================================

/// Naming convention: a `Persistable` named `Foo` gets a generated guard
/// type named `FooGuard`.
#[derive(Persistable(op = VecOp, guard = VecGuard))]
pub struct Vec<T> {
    data: std::vec::Vec<T>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub enum VecOp<T> {
    Push(T),
}

impl<T> Vec<T> {
    pub fn new() -> Self {
        Vec {
            data: std::vec::Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }
}

// `T: Clone` is only needed for `push` (see below), not `get_mut` -- kept
// in a separate impl block rather than bounding the whole type so
// `get_mut` stays available for non-`Clone` element types.
impl<'s, T: Persistable, B: Backend> VecGuard<'s, T, B> {
    pub fn get_mut(&mut self, index: usize) -> Option<T::Guard<'_, B>> {
        self.inner
            .data
            .get_mut(index)
            .map(|item| item.guard(self.backend))
    }
}

impl<'s, T: Persistable + Clone, B: Backend> VecGuard<'s, T, B> {
    pub fn push(&mut self, value: T) {
        // With `Op` owned (see `Persistable::Op`), recording and pushing
        // both need their own copy of `value` -- clone into the `Op`,
        // move the original into `self.inner.data`. `.data` here is
        // `Vec<T>`'s own private field -- accessible because this impl
        // block is generated into the same module as `struct Vec<T>`.
        self.backend.record::<Vec<T>>(&VecOp::Push(value.clone()));
        self.inner.data.push(value);
    }
}

// MACRO-GENERATED CODE IN CRATE `types` ======================================

/// Generated from `#[derive(Persistable)]` on `struct Vec<T>`.
impl<T: serde::Serialize + serde::de::DeserializeOwned> Persistable for Vec<T> {
    type Op = VecOp<T>;
    type Guard<'s, B: Backend> = VecGuard<'s, T, B>;

    fn guard<'s, B: Backend>(&'s mut self, backend: &'s B) -> Self::Guard<'s, B> {
        VecGuard {
            inner: self,
            backend,
        }
    }
}

/// Generated from `#[derive(Persistable)]` on `struct Vec<T>`.
/// `inner` borrows the real `Vec<T>` (the `Persistable` wrapper, not its
/// private `std::vec::Vec` field directly) for `'s`, so that
/// `as_persistable`/`Deref` below have something to return a reference to.
/// `backend` is a plain (non-owning, non-refcounted) reference, stored
/// once here and reborrowed down into nested guards (see `get_mut` above)
/// rather than re-supplied by the caller at each call.
pub struct VecGuard<'s, T, B = DefaultBackend> {
    inner: &'s mut Vec<T>,
    backend: &'s B,
}

/// Generated from `#[derive(Persistable)]` on `struct Vec<T>`.
impl<'s, T, B: Backend> Guard for VecGuard<'s, T, B> {
    type Persistable = Vec<T>;
    type Backend = B;

    fn as_persistable(&self) -> &Self::Persistable {
        self.inner
    }

    fn as_persistable_mut(&mut self) -> &mut Self::Persistable {
        self.inner
    }

    fn backend(&self) -> &Self::Backend {
        self.backend
    }
}

/// Generated from `#[derive(Persistable)]` on `struct Vec<T>`.
impl<'s, T, B> Deref for VecGuard<'s, T, B> {
    type Target = Vec<T>;

    fn deref(&self) -> &Self::Target {
        self.inner
    }
}

/// Generated from `#[derive(Persistable)]` on `struct Vec<T>`.
impl<'s, T, B> DerefMut for VecGuard<'s, T, B> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner
    }
}
