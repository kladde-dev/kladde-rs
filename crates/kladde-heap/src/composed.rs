//! [`Composed`]: the shared `(Storage, Allocator, id table)` core holding every
//! **immediate** backend operation as a plain `&mut self` method.
//!
//! Under the free-space-allocator pivot, the `id → (address, size, sizedness)`
//! table and the id pool live **here in the backend**, not in the allocator: the
//! allocator only manages free `Address` ranges. `Composed` mints ids (over the
//! pointer width `W`), keeps the table, and translates id-ops to the allocator's
//! address-keyed calls, doing the `Storage` byte moves itself.
//!
//! Both concrete backends build on this one type: `UnjournaledBackend` calls
//! these methods directly (its `&self` write facade borrows through a `RefCell`),
//! and `JournaledBackend` calls the same methods during journal *replay*.

use std::collections::HashMap;
use std::io::{self, SeekFrom};

use crate::allocator::{Allocator, Sizedness};
use crate::backend::BackendError;
use crate::pointer::{Pointer, ResolvedPointer, UniquePointerFixedSize, UniquePointerResizable};
use crate::storage::Storage;
use crate::word::Word;

/// One row of the backend's id table.
#[derive(Clone, Copy)]
struct Entry<Addr, Sz> {
    address: Addr,
    size: Sz,
    sizedness: Sizedness,
}

/// Storage `S`, free-space allocator `A`, and the backend-owned id table + pool
/// over the pointer width `W`.
pub(crate) struct Composed<S, A: Allocator, W: Word = u32> {
    pub storage: S,
    pub alloc: A,
    table: HashMap<Pointer<W>, Entry<A::Address, A::Size>>,
    next_id: W,
    free_ids: Vec<Pointer<W>>,
}

impl<S: Storage, A: Allocator, W: Word> Composed<S, A, W> {
    pub(crate) fn new(storage: S, alloc: A) -> Self {
        Self {
            storage,
            alloc,
            table: HashMap::new(),
            next_id: W::zero(),
            free_ids: Vec::new(),
        }
    }

    /// Number of live allocations (for leak checks).
    pub(crate) fn live_count(&self) -> usize {
        self.table.len()
    }

    /// Mint a fresh id (reusing a freed one when possible -- LIFO keeps ids dense).
    fn fresh_id(&mut self) -> Pointer<W> {
        if let Some(p) = self.free_ids.pop() {
            return p;
        }
        self.next_id += W::from_usize(1);
        Pointer::from_raw(self.next_id).expect("id counter is nonzero after increment")
    }

    fn entry(&self, id: Pointer<W>) -> Entry<A::Address, A::Size> {
        *self
            .table
            .get(&id)
            .expect("operation on a dangling pointer")
    }

    fn position(&self, id: Pointer<W>, offset: A::Size) -> u64 {
        (self.entry(id).address.to_usize() + offset.to_usize()) as u64
    }

    fn seek_to(&mut self, id: Pointer<W>, offset: A::Size) -> io::Result<()> {
        let pos = self.position(id, offset);
        self.storage.seek(SeekFrom::Start(pos))?;
        Ok(())
    }

    /// Ensure the store covers `[address, address + size)`.
    fn cover(&mut self, address: A::Address, size: A::Size) -> io::Result<()> {
        let end = (address.to_usize() + size.to_usize()) as u64;
        if self.storage.len()? < end {
            self.storage.resize(end)?;
        }
        Ok(())
    }

    /// Copy `len` bytes `src -> dst` (disjoint, or a no-op when `src == dst`).
    fn copy_bytes(&mut self, src: A::Address, dst: A::Address, len: usize) -> io::Result<()> {
        if len == 0 || src == dst {
            return Ok(());
        }
        let mut buf = vec![0u8; len];
        self.storage.seek(SeekFrom::Start(src.to_usize() as u64))?;
        self.storage.read_exact(&mut buf)?;
        self.storage.seek(SeekFrom::Start(dst.to_usize() as u64))?;
        self.storage.write_all(&buf)?;
        Ok(())
    }

    // ---- immediate operations ----

    fn alloc(&mut self, size: A::Size, sizedness: Sizedness) -> Pointer<W> {
        let address = self
            .alloc
            .alloc(size, sizedness)
            .expect("in-memory allocator never runs out of memory");
        let id = self.fresh_id();
        self.table.insert(
            id,
            Entry {
                address,
                size,
                sizedness,
            },
        );
        self.cover(address, size)
            .expect("extend storage for new allocation");
        id
    }

    pub(crate) fn alloc_resizable(&mut self, size: A::Size) -> UniquePointerResizable<Pointer<W>> {
        UniquePointerResizable::from_pointer(self.alloc(size, Sizedness::Resizable))
    }
    pub(crate) fn alloc_fixed_size(&mut self, size: A::Size) -> UniquePointerFixedSize<Pointer<W>> {
        UniquePointerFixedSize::from_pointer(self.alloc(size, Sizedness::Fixed))
    }

    fn free(&mut self, id: Pointer<W>) {
        let e = self.table.remove(&id).expect("free of a dangling handle");
        self.alloc
            .free(e.address, e.size, e.sizedness)
            .expect("free of an already-free range");
        self.free_ids.push(id);
    }

    pub(crate) fn free_resizable(&mut self, p: UniquePointerResizable<Pointer<W>>) {
        self.free(p.raw());
    }
    pub(crate) fn free_fixed_size(&mut self, p: UniquePointerFixedSize<Pointer<W>>) {
        self.free(p.raw());
    }

    pub(crate) fn resize(
        &mut self,
        p: &UniquePointerResizable<Pointer<W>>,
        new_size: A::Size,
    ) -> Result<(), BackendError> {
        let id = p.raw();
        let e = self.entry(id);
        match self
            .alloc
            .resize(e.address, e.size, new_size)
            .expect("resize of a range that isn't fully allocated")
        {
            Some(new_addr) => {
                let copy_len = e.size.to_usize().min(new_size.to_usize());
                self.cover(new_addr, new_size)?;
                self.copy_bytes(e.address, new_addr, copy_len)?;
                let slot = self.table.get_mut(&id).unwrap();
                slot.address = new_addr;
                slot.size = new_size;
            }
            None => {
                self.cover(e.address, new_size)?;
                self.table.get_mut(&id).unwrap().size = new_size;
            }
        }
        Ok(())
    }

    /// Convert sizedness, keeping the same id: alloc a fresh range of the new
    /// sizedness, copy the data, free the old range, re-tag the table.
    fn convert(
        &mut self,
        id: Pointer<W>,
        new_size: A::Size,
        new_sizedness: Sizedness,
    ) -> Result<(), BackendError> {
        let e = self.entry(id);
        let new_addr = self
            .alloc
            .alloc(new_size, new_sizedness)
            .expect("in-memory allocator never runs out of memory");
        let copy_len = e.size.to_usize().min(new_size.to_usize());
        self.cover(new_addr, new_size)?;
        self.copy_bytes(e.address, new_addr, copy_len)?;
        self.alloc
            .free(e.address, e.size, e.sizedness)
            .expect("free of an already-free range");
        self.table.insert(
            id,
            Entry {
                address: new_addr,
                size: new_size,
                sizedness: new_sizedness,
            },
        );
        Ok(())
    }

    pub(crate) fn make_resizable(
        &mut self,
        p: UniquePointerFixedSize<Pointer<W>>,
        new_size: A::Size,
    ) -> Result<UniquePointerResizable<Pointer<W>>, BackendError> {
        let id = p.raw();
        self.convert(id, new_size, Sizedness::Resizable)?;
        Ok(UniquePointerResizable::from_pointer(id))
    }
    pub(crate) fn make_fixed_size(
        &mut self,
        p: UniquePointerResizable<Pointer<W>>,
        new_size: A::Size,
    ) -> Result<UniquePointerFixedSize<Pointer<W>>, BackendError> {
        let id = p.raw();
        self.convert(id, new_size, Sizedness::Fixed)?;
        Ok(UniquePointerFixedSize::from_pointer(id))
    }

    pub(crate) fn write(&mut self, anchor: Pointer<W>, offset: A::Size, bytes: &[u8]) {
        self.seek_to(anchor, offset).expect("seek for write");
        self.storage.write_all(bytes).expect("write bytes");
    }

    pub(crate) fn splice(
        &mut self,
        p: &UniquePointerResizable<Pointer<W>>,
        offset: A::Size,
        old_len: A::Size,
        new: &[u8],
    ) {
        let id = p.raw();
        let e = self.entry(id);
        let old_size = e.size.to_usize();
        let off = offset.to_usize();
        let tail_start = off + old_len.to_usize();
        assert!(tail_start <= old_size, "splice range out of bounds");
        let tail_len = old_size - tail_start;

        // Save the trailing bytes (at the current address) before any relocation.
        let mut tail = vec![0u8; tail_len];
        self.seek_to(id, Word::from_usize(tail_start))
            .expect("seek to tail");
        self.storage.read_exact(&mut tail).expect("read tail");

        // Resize to fit `new` in place of the spliced-out range.
        let new_size_u = off + new.len() + tail_len;
        let new_size: A::Size = Word::from_usize(new_size_u);
        match self
            .alloc
            .resize(e.address, e.size, new_size)
            .expect("splice resize of a range that isn't fully allocated")
        {
            Some(new_addr) => {
                let copy_len = old_size.min(new_size_u);
                self.cover(new_addr, new_size).expect("cover for splice");
                self.copy_bytes(e.address, new_addr, copy_len)
                    .expect("relocate for splice");
                let slot = self.table.get_mut(&id).unwrap();
                slot.address = new_addr;
                slot.size = new_size;
            }
            None => {
                self.cover(e.address, new_size).expect("cover for splice");
                self.table.get_mut(&id).unwrap().size = new_size;
            }
        }

        // Lay down `new`, then the saved tail immediately after it.
        self.seek_to(id, offset).expect("seek for splice write");
        self.storage.write_all(new).expect("write new");
        self.storage.write_all(&tail).expect("write tail");
    }

    /// Position the cursor and hand out the store as a seekable reader.
    pub(crate) fn read_at(&mut self, anchor: Pointer<W>, offset: A::Size) -> &mut S {
        self.seek_to(anchor, offset).expect("seek for read");
        &mut self.storage
    }

    // ---- queries (from the id table) ----

    pub(crate) fn size(&self, id: Pointer<W>) -> Result<A::Size, BackendError> {
        self.table
            .get(&id)
            .map(|e| e.size)
            .ok_or(BackendError::DanglingPointer)
    }

    pub(crate) fn resolve(
        &self,
        id: Pointer<W>,
    ) -> Result<ResolvedPointer<Pointer<W>>, BackendError> {
        let e = self.table.get(&id).ok_or(BackendError::DanglingPointer)?;
        Ok(match e.sizedness {
            Sizedness::Resizable => {
                ResolvedPointer::Resizable(UniquePointerResizable::from_pointer(id))
            }
            Sizedness::Fixed => ResolvedPointer::Fixed(UniquePointerFixedSize::from_pointer(id)),
        })
    }
}
