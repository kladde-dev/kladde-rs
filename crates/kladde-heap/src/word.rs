//! The "unsigned integer word" abstraction the standard library doesn't
//! provide, used for allocator `Address`/`Size` arithmetic and as the raw width
//! carried by [`Pointer`](crate::Pointer).
//!
//! `Word` *extends* the arithmetic and bit operations the primitive unsigned
//! integers already implement (so allocator implementations can do offset math
//! and pack flag bits into ids), and adds a null-niche `NonZero` associated type
//! plus canonical little-endian byte (de)serialization. It is implemented **only
//! for primitive unsigned integers** (currently `u32` and `u64`) via the
//! [`impl_word!`] macro -- never for `Pointer`, which is a nominal id, not an
//! arithmetic type. Narrower integers and `usize` are intentionally omitted for
//! now (`usize` would encourage architecture-dependent on-file pointer widths,
//! which kladde does not yet want to commit to).

use std::hash::Hash;
use std::ops::{
    Add, AddAssign, BitAnd, BitAndAssign, BitOr, BitOrAssign, Shl, ShlAssign, Shr, ShrAssign, Sub,
    SubAssign,
};

/// An unsigned integer word: the arithmetic/bit surface allocators need, plus a
/// null-niche `NonZero` form and canonical little-endian byte serialization.
///
/// # Safety
///
/// This trait is `unsafe` to implement because downstream code relies on two
/// guarantees for memory safety and on-file correctness:
///
/// - `NonZero` must be a genuine null-niche type for `Self` (so `Option<Pointer>`
///   is layout-compatible with `Pointer`), and `to_nonzero`/`from_nonzero` must
///   round-trip with `to_nonzero(x) == None` iff `x == zero()`.
/// - `to_bytes`/`from_bytes` must be the canonical **little-endian** form and
///   round-trip. (Little-endian is kladde's blessed byte ordering, which is why
///   the method names carry no `le`.)
pub unsafe trait Word:
    Copy
    + Ord
    + Hash
    + Add<Output = Self>
    + Sub<Output = Self>
    + Shl<u32, Output = Self>
    + Shr<u32, Output = Self>
    + BitAnd<Output = Self>
    + BitOr<Output = Self>
    + AddAssign
    + SubAssign
    + ShlAssign<u32>
    + ShrAssign<u32>
    + BitAndAssign
    + BitOrAssign
{
    /// The null-niche counterpart of `Self` (e.g. `NonZeroU32` for `u32`).
    type NonZero: Copy + Eq + Hash;
    /// The fixed-width little-endian byte array of `Self` (e.g. `[u8; 4]`).
    ///
    /// This is an associated type rather than `[u8; SIZE]` because an
    /// associated-const array length in a trait signature needs
    /// `generic_const_exprs` (unstable); this is what `num-traits` does too.
    type Bytes: Copy + AsRef<[u8]>;

    /// The additive identity (`0`). Handy for bit-flag initialization.
    fn zero() -> Self;
    /// The value `1`, in `NonZero` form (it is nonzero by definition). A base
    /// for building flag masks (`Self::from_nonzero(Self::one()) << k`).
    fn one() -> Self::NonZero;

    /// Truncating conversion from `usize` (offsets from Rust collections arrive
    /// as `usize`). See [`Word::try_from_usize`] for the checked form.
    fn from_usize(n: usize) -> Self;
    /// Widening conversion to `usize`.
    fn to_usize(self) -> usize;
    /// Checked conversion from `usize`; `None` if `n` doesn't fit in `Self`.
    fn try_from_usize(n: usize) -> Option<Self>;

    /// `None` iff `self == zero()`; otherwise the nonzero form.
    fn to_nonzero(self) -> Option<Self::NonZero>;
    /// Widen a nonzero value back to `Self` (always nonzero).
    fn from_nonzero(nz: Self::NonZero) -> Self;

    /// Canonical little-endian bytes.
    fn to_bytes(self) -> Self::Bytes;
    /// Reconstruct from canonical little-endian bytes.
    fn from_bytes(bytes: Self::Bytes) -> Self;
    /// Reconstruct from the first `size_of::<Self::Bytes>()` little-endian bytes
    /// of `bytes` (which must be at least that long). Useful when the bytes come
    /// from a runtime-sized buffer rather than the exact `Bytes` array.
    fn from_bytes_slice(bytes: &[u8]) -> Self;
}

/// Stamps out one `impl Word for <primitive>` per line. Every method is
/// `#[inline]` -- generic `NonZero`/bare-int conversion has caused a real
/// performance regression when left uninlined, so the attribute is load-bearing.
macro_rules! impl_word {
    ($($int:ty => $nz:ty, $bytes:literal);* $(;)?) => {
        $(
            // SAFETY: `$nz` is the standard library's null-niche `NonZero` type
            // for `$int`; `new` returns `None` exactly at `0`; `to_le_bytes`/
            // `from_le_bytes` are the canonical little-endian round trip.
            unsafe impl Word for $int {
                type NonZero = $nz;
                type Bytes = [u8; $bytes];

                #[inline]
                fn zero() -> Self { 0 }
                #[inline]
                fn one() -> Self::NonZero { <$nz>::MIN } // NonZero::MIN == 1

                #[inline]
                fn from_usize(n: usize) -> Self { n as $int }
                #[inline]
                fn to_usize(self) -> usize { self as usize }
                #[inline]
                fn try_from_usize(n: usize) -> Option<Self> { <$int>::try_from(n).ok() }

                #[inline]
                fn to_nonzero(self) -> Option<Self::NonZero> { <$nz>::new(self) }
                #[inline]
                fn from_nonzero(nz: Self::NonZero) -> Self { nz.get() }

                #[inline]
                fn to_bytes(self) -> Self::Bytes { self.to_le_bytes() }
                #[inline]
                fn from_bytes(bytes: Self::Bytes) -> Self { <$int>::from_le_bytes(bytes) }
                #[inline]
                fn from_bytes_slice(bytes: &[u8]) -> Self {
                    let mut arr = [0u8; $bytes];
                    arr.copy_from_slice(&bytes[..$bytes]);
                    <$int>::from_le_bytes(arr)
                }
            }
        )*
    };
}

impl_word! {
    u32 => std::num::NonZeroU32, 4;
    u64 => std::num::NonZeroU64, 8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonzero_round_trips_and_maps_zero_to_none() {
        assert_eq!(<u32 as Word>::zero(), 0);
        assert_eq!(Word::to_nonzero(0u32), None);
        let nz = Word::to_nonzero(7u32).unwrap();
        assert_eq!(<u32 as Word>::from_nonzero(nz), 7);
        assert_eq!(<u32 as Word>::from_nonzero(<u32 as Word>::one()), 1);
    }

    #[test]
    fn bytes_are_little_endian_and_round_trip() {
        let bytes = Word::to_bytes(0x0403_0201u32);
        assert_eq!(bytes, [0x01, 0x02, 0x03, 0x04]);
        assert_eq!(<u32 as Word>::from_bytes(bytes), 0x0403_0201);

        let wide = Word::to_bytes(0x0807_0605_0403_0201u64);
        assert_eq!(wide, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(<u64 as Word>::from_bytes(wide), 0x0807_0605_0403_0201);
    }

    #[test]
    fn usize_conversions() {
        assert_eq!(<u32 as Word>::from_usize(9), 9u32);
        assert_eq!(Word::to_usize(9u32), 9usize);
        assert_eq!(<u32 as Word>::try_from_usize(usize::MAX), None);
        assert_eq!(<u64 as Word>::try_from_usize(5), Some(5u64));
    }
}
