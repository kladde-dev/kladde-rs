//! [`Composed`]: the shared `(Storage, Allocator)` core holding every
//! **immediate** backend operation as a plain `&mut self` method.
//!
//! Both concrete backends are built on this one type instead of duplicating the
//! address-translation and byte-move logic:
//!
//! - `UnjournaledBackend` wraps `RefCell<Composed>` and calls these methods
//!   directly (its `&self` write facade borrows through the `RefCell`).
//! - `JournaledBackend` (later) holds a `Composed` too and calls the very same
//!   methods during journal *replay* -- replay is exactly "run the immediate
//!   path over the buffered ops."
//!
//! This is the shared-inner-type answer to "can the two backends reuse code?":
//! no `Op` enum matched on immediately, no free functions threading `(&mut A,
//! &mut S)` -- just one type whose methods are the reusable operations.

use std::io::{self, SeekFrom};

use crate::allocator::{Allocator, Relocation};
use crate::backend::BackendError;
use crate::pointer::{UniquePointerFixedSize, UniquePointerResizable};
use crate::storage::Storage;
use crate::word::Word;

/// The composition of a byte store `S` and an allocation table `A`. Field-public
/// within the crate so the backends can reach `alloc` for queries.
pub(crate) struct Composed<S, A> {
    pub storage: S,
    pub alloc: A,
}

impl<S: Storage, A: Allocator> Composed<S, A> {
    pub(crate) fn new(storage: S, alloc: A) -> Self {
        Self { storage, alloc }
    }

    /// Byte position in storage of `anchor + offset`. Panics on a dangling
    /// anchor: reads/writes only ever target a live allocation (a corrupt id is
    /// caught earlier, at `resolve`/`size` in `load`).
    fn position(&self, anchor: A::Pointer, offset: A::Size) -> u64 {
        let addr = self
            .alloc
            .address(anchor)
            .expect("read/write to a dangling pointer");
        (addr.to_usize() + offset.to_usize()) as u64
    }

    fn seek_to(&mut self, anchor: A::Pointer, offset: A::Size) -> io::Result<()> {
        let pos = self.position(anchor, offset);
        self.storage.seek(SeekFrom::Start(pos))?;
        Ok(())
    }

    /// Ensure the store is long enough to hold `[addr(anchor), addr + size)`.
    fn cover(&mut self, anchor: A::Pointer, size: A::Size) -> io::Result<()> {
        let addr = self.alloc.address(anchor).expect("live pointer");
        let end = (addr.to_usize() + size.to_usize()) as u64;
        if self.storage.len()? < end {
            self.storage.resize(end)?;
        }
        Ok(())
    }

    /// Consume an allocator relocation report, moving bytes in storage as needed.
    /// `old_size`/`new_size` bound how many prefix bytes to preserve.
    fn apply_relocation(
        &mut self,
        anchor: A::Pointer,
        old_size: A::Size,
        new_size: A::Size,
        reloc: Relocation<A::Address>,
    ) -> io::Result<()> {
        match reloc {
            Relocation::Relocated { old, new } => {
                self.cover(anchor, new_size)?;
                let copy_len = old_size.to_usize().min(new_size.to_usize());
                let mut buf = vec![0u8; copy_len];
                self.storage.seek(SeekFrom::Start(old.to_usize() as u64))?;
                self.storage.read_exact(&mut buf)?;
                self.storage.seek(SeekFrom::Start(new.to_usize() as u64))?;
                self.storage.write_all(&buf)?;
            }
            // In-place (incl. an in-place *grow*) still needs the store extended.
            Relocation::InPlace { .. } => self.cover(anchor, new_size)?,
            // No address yet (a reservation): nothing to move.
            Relocation::Unclaimed => {}
        }
        Ok(())
    }

    // ---- immediate operations (reused by both backends) ----

    pub(crate) fn alloc_resizable(&mut self, size: A::Size) -> UniquePointerResizable<A::Pointer> {
        let p = self.alloc.alloc_resizable(size);
        self.cover(p.raw(), size)
            .expect("extend storage for new allocation");
        p
    }

    pub(crate) fn alloc_fixed_size(&mut self, size: A::Size) -> UniquePointerFixedSize<A::Pointer> {
        let p = self.alloc.alloc_fixed_size(size);
        self.cover(p.raw(), size)
            .expect("extend storage for new allocation");
        p
    }

    pub(crate) fn free_resizable(&mut self, p: UniquePointerResizable<A::Pointer>) {
        self.alloc.free_resizable(p);
    }

    pub(crate) fn free_fixed_size(&mut self, p: UniquePointerFixedSize<A::Pointer>) {
        self.alloc.free_fixed_size(p);
    }

    pub(crate) fn resize(
        &mut self,
        p: &UniquePointerResizable<A::Pointer>,
        new_size: A::Size,
    ) -> Result<(), BackendError> {
        let old_size = self.alloc.size(p.raw())?;
        let reloc = self.alloc.resize(p, new_size);
        self.apply_relocation(p.raw(), old_size, new_size, reloc)?;
        Ok(())
    }

    pub(crate) fn make_resizable(
        &mut self,
        p: UniquePointerFixedSize<A::Pointer>,
        new_size: A::Size,
    ) -> Result<UniquePointerResizable<A::Pointer>, BackendError> {
        let old_size = self.alloc.size(p.raw())?;
        let (handle, reloc) = self.alloc.make_resizable(p, new_size);
        self.apply_relocation(handle.raw(), old_size, new_size, reloc)?;
        Ok(handle)
    }

    pub(crate) fn make_fixed_size(
        &mut self,
        p: UniquePointerResizable<A::Pointer>,
        new_size: A::Size,
    ) -> Result<UniquePointerFixedSize<A::Pointer>, BackendError> {
        let old_size = self.alloc.size(p.raw())?;
        let (handle, reloc) = self.alloc.make_fixed_size(p, new_size);
        self.apply_relocation(handle.raw(), old_size, new_size, reloc)?;
        Ok(handle)
    }

    pub(crate) fn write(&mut self, anchor: A::Pointer, offset: A::Size, bytes: &[u8]) {
        self.seek_to(anchor, offset).expect("seek for write");
        self.storage.write_all(bytes).expect("write bytes");
    }

    pub(crate) fn splice(
        &mut self,
        p: &UniquePointerResizable<A::Pointer>,
        offset: A::Size,
        old_len: A::Size,
        new: &[u8],
    ) {
        let anchor = p.raw();
        let old_size = self.alloc.size(anchor).expect("live pointer").to_usize();
        let off = offset.to_usize();
        let tail_start = off + old_len.to_usize();
        assert!(tail_start <= old_size, "splice range out of bounds");
        let tail_len = old_size - tail_start;

        // Save the trailing bytes (at the current address) before any relocation.
        let mut tail = vec![0u8; tail_len];
        self.seek_to(anchor, Word::from_usize(tail_start))
            .expect("seek to tail");
        self.storage.read_exact(&mut tail).expect("read tail");

        // Resize to fit `new` in place of the spliced-out range, moving bytes.
        let new_size_u = off + new.len() + tail_len;
        let new_size: A::Size = Word::from_usize(new_size_u);
        let old_size_s: A::Size = Word::from_usize(old_size);
        let reloc = self.alloc.resize(p, new_size);
        self.apply_relocation(anchor, old_size_s, new_size, reloc)
            .expect("relocate for splice");

        // Lay down `new`, then the saved tail immediately after it.
        self.seek_to(anchor, offset).expect("seek for splice write");
        self.storage.write_all(new).expect("write new");
        self.storage.write_all(&tail).expect("write tail");
    }

    /// Position the cursor and hand out the store as a seekable reader. `&mut
    /// self` is what makes returning a real seekable cursor sound.
    pub(crate) fn read_at(&mut self, anchor: A::Pointer, offset: A::Size) -> &mut S {
        self.seek_to(anchor, offset).expect("seek for read");
        &mut self.storage
    }
}
