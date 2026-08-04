//! [`JournaledWriteBackend`] / [`JournaledReadBackend`]: an in-memory-journal
//! backend on the free-space-allocator model.
//!
//! It defers *everything* to flush: `alloc` mints an id immediately (so a `store`
//! can serialize it) but **records only a pending `(size, sizedness)`** -- the
//! allocator's `alloc` (address assignment) is deferred to [`flush`], the
//! `claim`. Writes are buffered. `size`/`resolve` answer from the pending map
//! during the write phase. On `flush`, every still-live id is claimed (addresses
//! assigned via `Composed::claim`) and the buffered writes are replayed at the
//! now-known addresses; the result is a read-only [`JournaledReadBackend`].
//!
//! The read/write phase split is by ownership: `JournaledWriteBackend` implements
//! only `WriteBackend`; `flush(self)` consumes it and returns a
//! `JournaledReadBackend` that implements only `ReadBackend`.
//!
//! Still an in-memory-journal *mock*: the journal + id table live in memory only,
//! so [`JournaledWriteBackend::open`] (recovery from `Storage`) is a `todo!()`
//! -- the self-hosting bootstrap is deferred. `splice` is likewise `todo!()`, and
//! freed-during-write ids are dropped from the pending set without recycling
//! their id number until flush (no aliasing within a transaction; ids stay dense
//! *enough* for the mock).

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Seek};

use crate::allocator::{Allocator, Sizedness};
use crate::backend::{Backend, BackendError, ReadBackend, WriteBackend};
use crate::composed::Composed;
use crate::pointer::{Pointer, ResolvedPointer, UniquePointerFixedSize, UniquePointerResizable};
use crate::storage::Storage;
use crate::word::Word;

struct JournaledInner<S, A: Allocator, W: Word = u32> {
    composed: Composed<S, A, W>,
    /// Minted-but-not-yet-claimed allocations: the deferred allocator state,
    /// mutated by `alloc`/`resize`/`free`/`make_*`.
    pending: HashMap<Pointer<W>, (A::Size, Sizedness)>,
    /// Buffered writes `(id, offset, bytes)`, replayed after claim.
    journal: Vec<(Pointer<W>, A::Size, Vec<u8>)>,
}

fn handle_for<W: Word>(id: Pointer<W>, sizedness: Sizedness) -> ResolvedPointer<Pointer<W>> {
    match sizedness {
        Sizedness::Resizable => {
            ResolvedPointer::Resizable(UniquePointerResizable::from_pointer(id))
        }
        Sizedness::Fixed => ResolvedPointer::Fixed(UniquePointerFixedSize::from_pointer(id)),
    }
}

/// The write half of a journaled transaction.
pub struct JournaledWriteBackend<S, A: Allocator, W: Word = u32> {
    inner: RefCell<JournaledInner<S, A, W>>,
}

impl<S: Storage, A: Allocator, W: Word> JournaledWriteBackend<S, A, W> {
    pub fn new(storage: S, alloc: A) -> Self {
        Self {
            inner: RefCell::new(JournaledInner {
                composed: Composed::new(storage, alloc),
                pending: HashMap::new(),
                journal: Vec::new(),
            }),
        }
    }

    /// Reopen a journaled file, reconstructing state persisted in `storage`.
    /// Blocked on the self-hosting bootstrap (deferred): the journal + id table
    /// live in memory only, so there is nothing in `storage` to recover yet.
    pub fn open(_storage: S) -> Self {
        todo!("self-hosting bootstrap: recover the id table + journal from Storage (deferred)")
    }

    /// End the write transaction: claim every still-live id (assigning addresses),
    /// replay the buffered writes, and hand back a read-only view.
    pub fn flush(self) -> JournaledReadBackend<S, A, W> {
        let mut inner = self.inner.into_inner();
        let pending: Vec<(Pointer<W>, A::Size, Sizedness)> = inner
            .pending
            .drain()
            .map(|(id, (size, sizedness))| (id, size, sizedness))
            .collect();
        for (id, size, sizedness) in pending {
            inner.composed.claim(id, size, sizedness);
        }
        for (id, offset, bytes) in std::mem::take(&mut inner.journal) {
            inner.composed.write(id, offset, &bytes);
        }
        JournaledReadBackend { inner }
    }
}

impl<S: Storage, A: Allocator, W: Word> Backend for JournaledWriteBackend<S, A, W> {
    type Pointer = Pointer<W>;
    type Size = A::Size;

    fn size(&self, p: Self::Pointer) -> Result<Self::Size, BackendError> {
        self.inner
            .borrow()
            .pending
            .get(&p)
            .map(|&(size, _)| size)
            .ok_or(BackendError::DanglingPointer)
    }
    fn resolve(&self, p: Self::Pointer) -> Result<ResolvedPointer<Self::Pointer>, BackendError> {
        let inner = self.inner.borrow();
        let &(_, sizedness) = inner.pending.get(&p).ok_or(BackendError::DanglingPointer)?;
        Ok(handle_for(p, sizedness))
    }
}

impl<S: Storage, A: Allocator, W: Word> WriteBackend for JournaledWriteBackend<S, A, W> {
    fn alloc_resizable(&self, size: Self::Size) -> UniquePointerResizable<Self::Pointer> {
        let mut inner = self.inner.borrow_mut();
        let id = inner.composed.mint();
        inner.pending.insert(id, (size, Sizedness::Resizable));
        UniquePointerResizable::from_pointer(id)
    }
    fn alloc_fixed_size(&self, size: Self::Size) -> UniquePointerFixedSize<Self::Pointer> {
        let mut inner = self.inner.borrow_mut();
        let id = inner.composed.mint();
        inner.pending.insert(id, (size, Sizedness::Fixed));
        UniquePointerFixedSize::from_pointer(id)
    }
    fn free_resizable(&self, p: UniquePointerResizable<Self::Pointer>) {
        self.inner.borrow_mut().pending.remove(&p.raw());
    }
    fn free_fixed_size(&self, p: UniquePointerFixedSize<Self::Pointer>) {
        self.inner.borrow_mut().pending.remove(&p.raw());
    }
    fn resize(
        &self,
        p: &UniquePointerResizable<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<(), BackendError> {
        let mut inner = self.inner.borrow_mut();
        match inner.pending.get_mut(&p.raw()) {
            Some(e) => {
                e.0 = new_size;
                Ok(())
            }
            None => Err(BackendError::DanglingPointer),
        }
    }
    fn make_resizable(
        &self,
        p: UniquePointerFixedSize<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<UniquePointerResizable<Self::Pointer>, BackendError> {
        let id = p.raw();
        self.inner
            .borrow_mut()
            .pending
            .insert(id, (new_size, Sizedness::Resizable));
        Ok(UniquePointerResizable::from_pointer(id))
    }
    fn make_fixed_size(
        &self,
        p: UniquePointerResizable<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<UniquePointerFixedSize<Self::Pointer>, BackendError> {
        let id = p.raw();
        self.inner
            .borrow_mut()
            .pending
            .insert(id, (new_size, Sizedness::Fixed));
        Ok(UniquePointerFixedSize::from_pointer(id))
    }
    fn write(&self, anchor: Self::Pointer, offset: Self::Size, bytes: &[u8]) {
        self.inner
            .borrow_mut()
            .journal
            .push((anchor, offset, bytes.to_vec()));
    }
    fn splice(
        &self,
        _p: &UniquePointerResizable<Self::Pointer>,
        _offset: Self::Size,
        _old_len: Self::Size,
        _new: &[u8],
    ) {
        todo!(
            "journaled splice: deferring a splice interacts with the pending map + buffered writes"
        )
    }
}

/// The read-only view produced by [`JournaledWriteBackend::flush`].
pub struct JournaledReadBackend<S, A: Allocator, W: Word = u32> {
    inner: JournaledInner<S, A, W>,
}

impl<S: Storage, A: Allocator, W: Word> JournaledReadBackend<S, A, W> {
    /// Consume the view, returning the raw storage and allocator.
    pub fn into_parts(self) -> (S, A) {
        (self.inner.composed.storage, self.inner.composed.alloc)
    }
}

impl<S: Storage, A: Allocator, W: Word> Backend for JournaledReadBackend<S, A, W> {
    type Pointer = Pointer<W>;
    type Size = A::Size;

    fn size(&self, p: Self::Pointer) -> Result<Self::Size, BackendError> {
        self.inner.composed.size(p)
    }
    fn resolve(&self, p: Self::Pointer) -> Result<ResolvedPointer<Self::Pointer>, BackendError> {
        self.inner.composed.resolve(p)
    }
}

impl<S: Storage, A: Allocator, W: Word> ReadBackend for JournaledReadBackend<S, A, W> {
    fn read_at(&mut self, anchor: Self::Pointer, offset: Self::Size) -> impl Read + Seek + '_ {
        self.inner.composed.read_at(anchor, offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

        let mut rb = wb.flush();
        let mut cursor = rb.read_at(id, 0);
        let mut buf = [0u8; 4];
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);
    }

    #[test]
    fn resize_is_deferred_and_size_reflects_the_pending_state() {
        let wb = write_backend();
        let p = wb.alloc_resizable(4);
        assert_eq!(wb.size(p.raw()).unwrap(), 4);
        wb.resize(&p, 8).unwrap();
        assert_eq!(wb.size(p.raw()).unwrap(), 8); // pending reflects the new size

        let id = p.raw();
        let rb = wb.flush();
        assert_eq!(rb.size(id).unwrap(), 8); // claimed at the final size
    }

    #[test]
    fn resize_then_write_into_the_grown_region_replays_in_order() {
        let wb = write_backend();
        let p = wb.alloc_resizable(4);
        wb.write(p.raw(), 0, &[1, 2, 3, 4]);
        wb.resize(&p, 8).unwrap();
        wb.write(p.raw(), 4, &[5, 6, 7, 8]);
        let id = p.raw();

        let mut rb = wb.flush();
        let mut cursor = rb.read_at(id, 0);
        let mut buf = [0u8; 8];
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn free_drops_the_pending_allocation() {
        let wb = write_backend();
        let p = wb.alloc_resizable(4);
        wb.resize(&p, 8).unwrap();
        assert_eq!(wb.size(p.raw()).unwrap(), 8);
        let id = p.raw();
        wb.free_resizable(p);
        assert!(matches!(wb.size(id), Err(BackendError::DanglingPointer)));
    }
}
