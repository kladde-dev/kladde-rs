//! What every owning container shares: the inline pointer to its content
//! allocation, and size arithmetic that refuses to overflow.
//!
//! A container's pointer takes the encoding of the place it sits in: its
//! fixed width in a slotted place, all zeros for none, and the LEB128 varint
//! of its id in a packed one, `0` for none. The blob, which describes itself
//! as opaque, keeps the fixed width everywhere and passes `Slotted`.

use kladde_persist::{
    decode_option_slice, encode_option, varint_len, write_encoded, write_varint, Encoding, Error,
    Input, Place, PointerRepr, Word, WriteBackend,
};

/// How many bytes a container's pointer takes in a place of encoding `E`.
pub(crate) fn pointer_size<P: PointerRepr, E: Encoding>(pointer: Option<P>) -> usize {
    if E::PACKED {
        varint_len(pointer.map_or(0, |p| p.to_u32()) as u64)
    } else {
        P::BYTE_LEN
    }
}

/// Appends a container's pointer in encoding `E`.
pub(crate) fn encode_pointer<P: PointerRepr, E: Encoding>(pointer: Option<P>, out: &mut Vec<u8>) {
    if E::PACKED {
        write_varint(pointer.map_or(0, |p| p.to_u32()) as u64, out);
    } else {
        out.extend_from_slice(encode_option(pointer).as_ref());
    }
}

/// Reads back a pointer written by [`encode_pointer`].
pub(crate) fn decode_pointer<P: PointerRepr, E: Encoding>(
    input: &mut Input<'_>,
) -> Result<Option<P>, Error> {
    if E::PACKED {
        let id = input.varint()?;
        match u32::try_from(id) {
            Ok(0) => Ok(None),
            Ok(id) => Ok(Some(P::from_u32(id))),
            Err(_) => Err(Error::Corrupt(format!(
                "pointer {id} is beyond the 32 bits of an allocation id"
            ))),
        }
    } else {
        Ok(decode_option_slice(input.take(P::BYTE_LEN)?))
    }
}

/// Writes a container's new pointer `new` over `old` at `place`: in place if
/// its encoding keeps its size, and as a splice that the values around it
/// hear about if not.
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
