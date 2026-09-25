//! What every owning container shares: the inline slot holding its content
//! allocation's pointer, and size arithmetic that refuses to overflow.

use kladde_persist::{
    decode_option_slice, encode_option, Error, Location, PointerRepr, ReadBackend, Word,
    WriteBackend,
};
use std::io::Read;

/// Writes the inline pointer slot, `None` as the all-zero null niche.
pub(crate) fn write_slot<P: PointerRepr, B: WriteBackend<Pointer = P>>(
    backend: &B,
    location: Location<P, B::Size>,
    pointer: Option<P>,
) -> Result<(), Error> {
    backend.write(
        location.anchor,
        location.offset,
        encode_option(pointer).as_ref(),
    )
}

/// Reads back a slot written by [`write_slot`].
pub(crate) fn read_slot<P: PointerRepr, B: ReadBackend<Pointer = P>>(
    backend: &mut B,
    location: Location<P, B::Size>,
) -> Result<Option<P>, Error> {
    let mut bytes = vec![0u8; P::BYTE_LEN];
    backend
        .read_at(location.anchor, location.offset)?
        .read_exact(&mut bytes)?;
    Ok(decode_option_slice(&bytes))
}

/// `n` bytes as the backend's size type, or [`Error::OutOfBounds`] if it does
/// not fit.
pub(crate) fn size<S: Word>(n: usize) -> Result<S, Error> {
    S::try_from_usize(n).ok_or(Error::OutOfBounds)
}
