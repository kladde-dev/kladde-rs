//! [`JournaledWriteBackend`] / [`JournaledReadBackend`]: an **attempt** at a
//! journaled backend, sharing the immediate-operation core ([`Composed`]) with
//! the unjournaled one -- journal *replay* is literally "run the immediate path
//! over the buffered ops."
//!
//! ## What this models
//!
//! - **Buffered writes.** `write` appends an op instead of touching storage;
//!   `flush` replays them. So nothing a `store` writes is visible until the
//!   transaction is flushed (there is no read surface on the write backend at
//!   all -- the phase split is enforced by the *types*).
//! - **Deferred resizes via the `resized_allocation` map** (the design decision
//!   that replaced `reserve_resize`). `resize` records the new size in a map and
//!   journals an op; it does **not** touch the allocator. `size` consults the map
//!   first, so callers see the pending size mid-transaction. `flush` applies the
//!   resizes (through `Composed`) and clears the map, restoring the invariant
//!   that in the **read phase the map is empty**.
//! - **The read/write phase split by ownership**: `JournaledWriteBackend`
//!   implements only `WriteBackend`; `flush(self)` consumes it and returns a
//!   `JournaledReadBackend` that implements only `ReadBackend`.
//!
//! ## What is deliberately NOT done here (see implementation-notes.md)
//!
//! This is an in-memory-journal mock; several load-bearing pieces are stubbed or
//! simplified because they were explicitly deferred or are blocked on machinery
//! this pass doesn't build:
//!
//! - **Self-hosting bootstrap** (persisting the journal + allocator state *in*
//!   `Storage`, and recovering them on open) is deferred (Problem 6). The
//!   journal and allocator table live in memory only; [`JournaledWriteBackend::open`]
//!   is a `todo!()` placeholder for the recovery path.
//! - **Deferred address assignment** (`reserve`/`claim`) is not exercised:
//!   `SimpleAllocator` assigns addresses eagerly and the journal is in memory
//!   (not in `Storage`), so the file-tail-collision problem that motivates
//!   deferral doesn't arise. A real journaled file needs a journaling allocator
//!   and an in-`Storage` journal together.
//! - **Crash-safe framing** (length prefixes / checksums) is absent; ops are a
//!   plain in-memory `Vec`.
//! - **`alloc`/`free`/`make_*` apply immediately** (only `write`/`resize` defer),
//!   so this mock is not atomic-rollback-capable; and **`splice` is `todo!()`**
//!   (deferring it interacts subtly with the `resized_allocation` map).
//! - **Auto-flush on journal overflow** is not implemented; `flush` is explicit.

use std::collections::HashMap;
use std::io::{Read, Seek};

use crate::allocator::{AllocError, Allocator};
use crate::backend::{Backend, BackendError, ReadBackend, WriteBackend};
use crate::composed::Composed;
use crate::pointer::{ResolvedPointer, UniquePointerFixedSize, UniquePointerResizable};
use crate::storage::Storage;
use std::cell::RefCell;

/// A buffered mutation. Only the operations whose durable effect on stored bytes
/// we defer are journaled (`write`, `resize`); `alloc`/`free`/`make_*` apply to
/// the in-memory table immediately in this mock.
enum Op<P, S> {
    Write {
        anchor: P,
        offset: S,
        bytes: Vec<u8>,
    },
    Resize {
        p: P,
        new_size: S,
    },
}

struct JournaledInner<S, A: Allocator> {
    composed: Composed<S, A>,
    journal: Vec<Op<A::Pointer, A::Size>>,
    resized: HashMap<A::Pointer, A::Size>,
}

impl<S: Storage, A: Allocator> JournaledInner<S, A> {
    /// Apply every buffered op through the shared immediate path, then clear the
    /// deferred-resize map (restoring the read-phase invariant that it's empty).
    fn replay(&mut self) {
        for op in std::mem::take(&mut self.journal) {
            match op {
                Op::Write {
                    anchor,
                    offset,
                    bytes,
                } => self.composed.write(anchor, offset, &bytes),
                Op::Resize { p, new_size } => {
                    let handle = self
                        .composed
                        .alloc
                        .resolve_resizable(p)
                        .expect("replay: resize target must be a live resizable allocation");
                    self.composed
                        .resize(&handle, new_size)
                        .expect("replay: resize must succeed");
                }
            }
        }
        self.resized.clear();
    }
}

/// The write half of a journaled transaction: buffers ops via `&self` (guard
/// model), applies none of the deferred ones until [`JournaledWriteBackend::flush`].
pub struct JournaledWriteBackend<S, A: Allocator> {
    inner: RefCell<JournaledInner<S, A>>,
}

impl<S: Storage, A: Allocator> JournaledWriteBackend<S, A> {
    pub fn new(storage: S, alloc: A) -> Self {
        Self {
            inner: RefCell::new(JournaledInner {
                composed: Composed::new(storage, alloc),
                journal: Vec::new(),
                resized: HashMap::new(),
            }),
        }
    }

    /// Reopen a journaled file, reconstructing the in-memory allocator from state
    /// persisted in `storage`.
    ///
    /// Blocked on the self-hosting bootstrap (Problem 6, deferred): the journal
    /// and allocator table currently live in memory only, so there is nothing in
    /// `storage` to recover from yet.
    pub fn open(_storage: S) -> Self {
        todo!("self-hosting bootstrap: recover the allocator table + journal from Storage (deferred, Problem 6)")
    }

    /// End the write transaction: replay every buffered op into storage and hand
    /// back a read-only view. Consuming `self` guarantees no live guard remains
    /// and that nothing writes after the flush.
    pub fn flush(self) -> JournaledReadBackend<S, A> {
        let mut inner = self.inner.into_inner();
        inner.replay();
        JournaledReadBackend { inner }
    }
}

impl<S: Storage, A: Allocator> Backend for JournaledWriteBackend<S, A> {
    type Pointer = A::Pointer;
    type Size = A::Size;

    fn size(&self, p: Self::Pointer) -> Result<Self::Size, AllocError> {
        let inner = self.inner.borrow();
        // Deferred-resize map wins over the allocator's (still-old) size.
        if let Some(&s) = inner.resized.get(&p) {
            return Ok(s);
        }
        inner.composed.alloc.size(p)
    }
    fn resolve(&self, p: Self::Pointer) -> Result<ResolvedPointer<Self::Pointer>, AllocError> {
        self.inner.borrow().composed.alloc.resolve(p)
    }
}

impl<S: Storage, A: Allocator> WriteBackend for JournaledWriteBackend<S, A> {
    fn alloc_resizable(&self, size: Self::Size) -> UniquePointerResizable<Self::Pointer> {
        // Applied immediately so the minted id is stable and queryable at once.
        self.inner.borrow_mut().composed.alloc_resizable(size)
    }
    fn alloc_fixed_size(&self, size: Self::Size) -> UniquePointerFixedSize<Self::Pointer> {
        self.inner.borrow_mut().composed.alloc_fixed_size(size)
    }
    fn free_resizable(&self, p: UniquePointerResizable<Self::Pointer>) {
        let mut inner = self.inner.borrow_mut();
        inner.resized.remove(&p.raw()); // drop any pending resize for this id
        inner.composed.free_resizable(p);
    }
    fn free_fixed_size(&self, p: UniquePointerFixedSize<Self::Pointer>) {
        self.inner.borrow_mut().composed.free_fixed_size(p);
    }

    fn resize(
        &self,
        p: &UniquePointerResizable<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<(), BackendError> {
        // Deferred: record in the map + journal, don't touch the allocator.
        let mut inner = self.inner.borrow_mut();
        inner.resized.insert(p.raw(), new_size);
        inner.journal.push(Op::Resize {
            p: p.raw(),
            new_size,
        });
        Ok(())
    }

    fn make_resizable(
        &self,
        p: UniquePointerFixedSize<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<UniquePointerResizable<Self::Pointer>, BackendError> {
        // Applied immediately (mock simplification): the new handle must be
        // available synchronously for the caller to serialize.
        self.inner.borrow_mut().composed.make_resizable(p, new_size)
    }
    fn make_fixed_size(
        &self,
        p: UniquePointerResizable<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<UniquePointerFixedSize<Self::Pointer>, BackendError> {
        self.inner
            .borrow_mut()
            .composed
            .make_fixed_size(p, new_size)
    }

    fn write(&self, anchor: Self::Pointer, offset: Self::Size, bytes: &[u8]) {
        // Buffered: nothing hits storage until `flush`.
        self.inner.borrow_mut().journal.push(Op::Write {
            anchor,
            offset,
            bytes: bytes.to_vec(),
        });
    }

    fn splice(
        &self,
        _p: &UniquePointerResizable<Self::Pointer>,
        _offset: Self::Size,
        _old_len: Self::Size,
        _new: &[u8],
    ) {
        todo!(
            "journaled splice: buffering a splice (resize + tail-shift + overwrite) \
             interacts with the resized_allocation map and buffered writes; deferred"
        )
    }
}

/// The read-only view produced by [`JournaledWriteBackend::flush`]. Implements
/// only `ReadBackend`; there is no way back to writing without a fresh
/// transaction (which, once self-hosting exists, would reopen from `Storage`).
pub struct JournaledReadBackend<S, A: Allocator> {
    inner: JournaledInner<S, A>,
}

impl<S: Storage, A: Allocator> JournaledReadBackend<S, A> {
    /// Consume the view, returning the raw storage and allocator (test inspection).
    pub fn into_parts(self) -> (S, A) {
        (self.inner.composed.storage, self.inner.composed.alloc)
    }
}

impl<S: Storage, A: Allocator> Backend for JournaledReadBackend<S, A> {
    type Pointer = A::Pointer;
    type Size = A::Size;

    fn size(&self, p: Self::Pointer) -> Result<Self::Size, AllocError> {
        // Read phase: the resized map is empty, so the allocator is authoritative.
        self.inner.composed.alloc.size(p)
    }
    fn resolve(&self, p: Self::Pointer) -> Result<ResolvedPointer<Self::Pointer>, AllocError> {
        self.inner.composed.alloc.resolve(p)
    }
}

impl<S: Storage, A: Allocator> ReadBackend for JournaledReadBackend<S, A> {
    fn read_at(&mut self, anchor: Self::Pointer, offset: Self::Size) -> impl Read + Seek + '_ {
        self.inner.composed.read_at(anchor, offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pointer::Pointer;
    use crate::storage::InMemoryStorage;
    use crate::SimpleAllocator;
    use std::io::Read;

    fn write_backend() -> JournaledWriteBackend<InMemoryStorage, SimpleAllocator> {
        JournaledWriteBackend::new(InMemoryStorage::default(), SimpleAllocator::new())
    }

    #[test]
    fn writes_are_buffered_until_flush_then_visible() {
        let wb = write_backend();
        let p = wb.alloc_fixed_size(4);
        wb.write(p.raw(), 0, &[1, 2, 3, 4]);
        let id = p.raw();

        // Flush -> read view; the buffered write is now applied.
        let mut rb = wb.flush();
        let mut cursor = rb.read_at(id, 0);
        let mut buf = [0u8; 4];
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);
    }

    #[test]
    fn resize_is_deferred_and_size_reflects_the_map() {
        let wb = write_backend();
        let p = wb.alloc_resizable(4);
        // Before resize: allocator size.
        assert_eq!(wb.size(p.raw()), Ok(4));
        wb.resize(&p, 8).unwrap();
        // After a deferred resize: the map reports the pending size...
        assert_eq!(wb.size(p.raw()), Ok(8));
        // ...but the allocator hasn't been touched yet.
        assert_eq!(wb.inner.borrow().composed.alloc.size(p.raw()), Ok(4));

        // Flush applies it and clears the map.
        let id = p.raw();
        let (_s, a) = wb.flush().into_parts();
        assert_eq!(a.size(id), Ok(8));
    }

    #[test]
    fn resize_then_write_into_the_grown_region_replays_in_order() {
        let wb = write_backend();
        let p = wb.alloc_resizable(4);
        wb.write(p.raw(), 0, &[1, 2, 3, 4]);
        wb.resize(&p, 8).unwrap();
        wb.write(p.raw(), 4, &[5, 6, 7, 8]); // into the grown region
        let id = p.raw();

        let mut rb = wb.flush();
        let mut cursor = rb.read_at(id, 0);
        let mut buf = [0u8; 8];
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn free_drops_a_pending_resize() {
        let wb = write_backend();
        let p = wb.alloc_resizable(4);
        wb.resize(&p, 8).unwrap();
        assert_eq!(wb.size(p.raw()), Ok(8));
        wb.free_resizable(p);
        // the id is gone from both the map and the table
        let bogus = Pointer::from_raw(1).unwrap();
        assert_eq!(wb.size(bogus), Err(AllocError::DanglingPointer));
    }
}
