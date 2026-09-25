//! The traits the serialization layer drives a store through.
//!
//! [`WriteBackend`] takes `&self` so that a parent guard can hand one backend
//! to all of its field guards by reborrow; [`ReadBackend`] takes `&mut self`
//! so that a load can hold a real cursor, and so that the borrow checker keeps
//! loads and guards apart.

use std::io::{Read, Seek};

use crate::error::Error;
use crate::pointer::{PointerRepr, UniquePointer, Word};

/// What both halves share: the pointer and size types, and size queries.
///
/// ```
/// use kladde_store::{Backend, MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let p = store.alloc(3)?;
/// assert_eq!(store.size(p.raw())?, 3);
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub trait Backend {
    /// The pointer type, whose encoding the serialization layer writes.
    type Pointer: PointerRepr;
    /// Offsets and sizes within allocations.
    type Size: Word;

    /// The size of allocation `p`, including every operation recorded so
    /// far, or [`Error::DanglingPointer`] if there is no such allocation.
    fn size(&self, p: Self::Pointer) -> Result<Self::Size, Error>;
}

/// Reading stored bytes, as a value's `load` does.
///
/// Has no write or allocation surface at all, so `&mut impl ReadBackend`
/// cannot mutate. Reads see the state as of the last flush.
///
/// ```
/// use std::io::Read;
/// use kladde_store::{MemoryStorage, ReadBackend, Store, WriteBackend};
///
/// let mut store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let p = store.alloc(4)?;
/// store.write(p.raw(), 0, &7u32.to_le_bytes())?;
/// store.flush()?;
/// let mut buf = [0u8; 4];
/// store.read_at(p.raw(), 0)?.read_exact(&mut buf)?;
/// assert_eq!(u32::from_le_bytes(buf), 7);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub trait ReadBackend: Backend {
    /// A reader positioned at `offset` within allocation `anchor`, running to
    /// the allocation's end, or [`Error::DanglingPointer`] if `anchor` did
    /// not exist at the last flush.
    fn read_at(
        &mut self,
        anchor: Self::Pointer,
        offset: Self::Size,
    ) -> Result<impl Read + Seek + '_, Error>;
}

/// Recording mutations, as guards do.
///
/// Outside a transaction or batch, every method appends to the journal before
/// it returns, so a mutation that returned `Ok` survives an application
/// crash; a power cut may still lose it until the next flush. Writes past an
/// allocation's end grow it, and bytes that growth exposes read as zero.
/// Reads see none of it until the next flush.
///
/// ```
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let p = store.alloc(0)?;
/// store.write(p.raw(), 2, b"xy")?; // grows to 4 bytes
/// store.splice(&p, 0, 2, b"a")?; // replaces the two zeros by one byte
/// store.flush()?;
/// assert_eq!(store.read_all(p.raw())?, b"axy");
/// store.free(p)?;
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub trait WriteBackend: Backend {
    /// A new allocation of `size` zero bytes.
    fn alloc(&self, size: Self::Size) -> Result<UniquePointer<Self::Pointer>, Error>;
    /// Ends `p`'s existence. Its id may be handed out again after the next
    /// flush.
    fn free(&self, p: UniquePointer<Self::Pointer>) -> Result<(), Error>;
    /// Sets `p`'s size, keeping `min(old, new)` bytes; growth exposes zeros.
    fn resize(&self, p: &UniquePointer<Self::Pointer>, size: Self::Size) -> Result<(), Error>;
    /// Overwrites `bytes.len()` bytes at `offset` within `anchor`, growing it
    /// if they reach past its end.
    fn write(&self, anchor: Self::Pointer, offset: Self::Size, bytes: &[u8]) -> Result<(), Error>;
    /// Replaces the `old_len` bytes at `offset` with `new`, shifting the tail
    /// and resizing.
    fn splice(
        &self,
        p: &UniquePointer<Self::Pointer>,
        offset: Self::Size,
        old_len: Self::Size,
        new: &[u8],
    ) -> Result<(), Error>;
    /// Copies `len` bytes from `src` to `dst`, like `memmove` within one
    /// allocation. Bytes past `src`'s end copy as zeros.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// let a = store.alloc(0)?;
    /// store.write(a.raw(), 0, b"abcd")?;
    /// store.copy(a.raw(), 0, 3, a.raw(), 1)?;
    /// store.move_range(a.raw(), 0, 1, a.raw(), 4)?;
    /// store.flush()?;
    /// assert_eq!(store.read_all(a.raw())?, b"\0abca");
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    fn copy(
        &self,
        src: Self::Pointer,
        src_offset: Self::Size,
        len: Self::Size,
        dst: Self::Pointer,
        dst_offset: Self::Size,
    ) -> Result<(), Error>;
    /// Copies like [`copy`](WriteBackend::copy), then zeroes what the
    /// destination left of the source range. See
    /// [`copy`](WriteBackend::copy) for an example.
    fn move_range(
        &self,
        src: Self::Pointer,
        src_offset: Self::Size,
        len: Self::Size,
        dst: Self::Pointer,
        dst_offset: Self::Size,
    ) -> Result<(), Error>;
}
