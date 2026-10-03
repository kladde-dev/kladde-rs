//! What every owning container shares: the inline pointer to its content
//! allocation, and size arithmetic that refuses to overflow.
//!
//! The pointer helpers take the encoding of the place the pointer sits in,
//! although it decides nothing yet: the containers describe themselves as
//! opaque, which keeps their pointers at their fixed width in packed places.
#![allow(clippy::extra_unused_type_parameters)]

use kladde_persist::{
    decode_option_slice, encode_option, write_encoded, Encoding, Error, Input, Place, PointerRepr,
    Word, WriteBackend,
};

/// How many bytes a container's pointer takes inline: the pointer's fixed
/// width, in either encoding, since the containers describe themselves as
/// opaque and so keep their inline bytes in packed places too.
pub(crate) fn pointer_size<P: PointerRepr, E: Encoding>(_pointer: Option<P>) -> usize {
    P::BYTE_LEN
}

/// Appends a container's inline pointer, `None` as the all-zero null niche.
pub(crate) fn encode_pointer<P: PointerRepr, E: Encoding>(pointer: Option<P>, out: &mut Vec<u8>) {
    out.extend_from_slice(encode_option(pointer).as_ref());
}

/// Reads back a pointer written by [`encode_pointer`].
pub(crate) fn decode_pointer<P: PointerRepr, E: Encoding>(
    input: &mut Input<'_>,
) -> Result<Option<P>, Error> {
    Ok(decode_option_slice(input.take(P::BYTE_LEN)?))
}

/// Writes a container's new inline pointer `new` over `old` at `place`.
pub(crate) fn publish_pointer<B: WriteBackend, E: Encoding>(
    backend: &B,
    place: &Place<'_, B, E>,
    old: Option<B::Pointer>,
    new: Option<B::Pointer>,
) -> Result<(), Error> {
    let mut bytes = Vec::with_capacity(B::Pointer::BYTE_LEN);
    encode_pointer::<B::Pointer, E>(new, &mut bytes);
    write_encoded(backend, place, pointer_size::<B::Pointer, E>(old), &bytes)
}

/// `n` bytes as the backend's size type, or [`Error::OutOfBounds`] if it does
/// not fit.
pub(crate) fn size<S: Word>(n: usize) -> Result<S, Error> {
    S::try_from_usize(n).ok_or(Error::OutOfBounds)
}
