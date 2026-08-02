//! [`PointerRepr`]: the on-file byte (de)serialization of pointer ids, plus the
//! `Option<Pointer>` null-niche encoding.
//!
//! Modeled on `num-traits`' `ToBytes`/`FromBytes`. Little-endian is kladde's
//! blessed byte ordering, so the method names carry **no** `le`. Because every
//! [`Pointer`] is nonzero (its field is a `W::NonZero`), a valid pointer never
//! encodes to all-zero bytes -- so `Option<Pointer>` reuses that all-zero pattern
//! for `None`, giving the null niche **on file** as well as in memory. The
//! [`encode_option`]/[`decode_option`] helpers implement exactly that.

use kladde_heap::{Pointer, Word};

/// Byte (de)serialization of a pointer id. The `Bytes` associated type stands in
/// for `[u8; SIZE]` (an associated-const array length in a trait signature needs
/// unstable `generic_const_exprs`), exactly as `num-traits` does.
pub trait PointerRepr: Copy {
    type Bytes: Copy + AsRef<[u8]>;

    /// Canonical little-endian bytes of a (valid, nonzero) pointer.
    fn to_bytes(self) -> Self::Bytes;
    /// Reconstruct from canonical little-endian bytes. The bytes must encode a
    /// valid (nonzero) pointer; the all-zero pattern is reserved for `None` and
    /// is handled by [`decode_option`], not here.
    fn from_bytes(bytes: Self::Bytes) -> Self;
    /// The all-zero encoding, reserved for `None` in the `Option<Self>` niche.
    fn zeroed_bytes() -> Self::Bytes;
}

// Sound because `unsafe Word` guarantees `W::NonZero` is genuinely nonzero, so a
// valid `Pointer<W>` never round-trips through all-zero bytes.
impl<W: Word> PointerRepr for Pointer<W> {
    type Bytes = W::Bytes;

    #[inline]
    fn to_bytes(self) -> W::Bytes {
        self.raw().to_bytes()
    }
    #[inline]
    fn from_bytes(bytes: W::Bytes) -> Self {
        Pointer::from_raw(W::from_bytes(bytes))
            .expect("PointerRepr::from_bytes on a valid (nonzero) pointer id")
    }
    #[inline]
    fn zeroed_bytes() -> W::Bytes {
        W::zero().to_bytes()
    }
}

/// Encode `Option<P>` into `P::Bytes`, using the all-zero pattern for `None`.
#[inline]
pub fn encode_option<P: PointerRepr>(p: Option<P>) -> P::Bytes {
    p.map_or_else(P::zeroed_bytes, P::to_bytes)
}

/// Decode `Option<P>` from `P::Bytes`: all-zero is `None`, anything else is
/// `Some(P::from_bytes(..))`.
#[inline]
pub fn decode_option<P: PointerRepr>(bytes: P::Bytes) -> Option<P> {
    if bytes.as_ref().iter().all(|&b| b == 0) {
        None
    } else {
        Some(P::from_bytes(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_bytes_round_trip_little_endian() {
        let p = Pointer::<u32>::from_raw(0x0403_0201).unwrap();
        let bytes = p.to_bytes();
        assert_eq!(bytes, [0x01, 0x02, 0x03, 0x04]);
        assert_eq!(Pointer::<u32>::from_bytes(bytes), p);
    }

    #[test]
    fn option_pointer_uses_the_all_zero_niche_on_file() {
        let some = Some(Pointer::<u32>::from_raw(7).unwrap());
        let none: Option<Pointer<u32>> = None;

        assert_eq!(encode_option(none), [0, 0, 0, 0]);
        assert_ne!(encode_option(some), [0, 0, 0, 0]);

        assert_eq!(decode_option::<Pointer<u32>>([0, 0, 0, 0]), None);
        assert_eq!(decode_option::<Pointer<u32>>(encode_option(some)), some);
    }

    #[test]
    fn wide_pointer_encodes_at_its_width() {
        let p = Pointer::<u64>::from_raw(1).unwrap();
        assert_eq!(p.to_bytes(), [1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(Pointer::<u64>::zeroed_bytes(), [0u8; 8]);
    }
}
