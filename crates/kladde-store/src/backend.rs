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
pub trait Backend {
    /// The pointer type, whose encoding the serialization layer writes.
    type Pointer: PointerRepr;
    /// Offsets and sizes within allocations.
    type Size: Word;

    /// The size of allocation `p`, or [`Error::DanglingPointer`] if there is
    /// no such allocation.
    fn size(&self, p: Self::Pointer) -> Result<Self::Size, Error>;
}

/// Reading stored bytes, as a value's `load` does.
///
/// Has no write or allocation surface at all, so `&mut impl ReadBackend`
/// cannot mutate.
pub trait ReadBackend: Backend {
    /// A reader positioned at `offset` within allocation `anchor`, running to
    /// the allocation's end.
    fn read_at(
        &mut self,
        anchor: Self::Pointer,
        offset: Self::Size,
    ) -> Result<impl Read + Seek + '_, Error>;
}

/// Recording mutations, as guards do.
///
/// Every method appends to the journal before it returns, so a mutation that
/// returned `Ok` survives an application crash. Writes past an allocation's
/// end grow it, and bytes that growth exposes read as zero.
pub trait WriteBackend: Backend {
    /// A new allocation of `size` zero bytes.
    fn alloc(&self, size: Self::Size) -> Result<UniquePointer<Self::Pointer>, Error>;
    /// Ends `p`'s existence.
    fn free(&self, p: UniquePointer<Self::Pointer>) -> Result<(), Error>;
    /// Sets `p`'s size, keeping `min(old, new)` bytes.
    fn resize(&self, p: &UniquePointer<Self::Pointer>, size: Self::Size) -> Result<(), Error>;
    /// Overwrites `bytes.len()` bytes at `offset` within `anchor`.
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
    /// allocation.
    fn copy(
        &self,
        src: Self::Pointer,
        src_offset: Self::Size,
        len: Self::Size,
        dst: Self::Pointer,
        dst_offset: Self::Size,
    ) -> Result<(), Error>;
    /// Copies like [`copy`](WriteBackend::copy), then zeroes what the
    /// destination left of the source range.
    fn move_range(
        &self,
        src: Self::Pointer,
        src_offset: Self::Size,
        len: Self::Size,
        dst: Self::Pointer,
        dst_offset: Self::Size,
    ) -> Result<(), Error>;
}
