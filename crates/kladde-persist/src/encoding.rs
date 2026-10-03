//! [`Encoding`]: which of a type's two encodings a place holds, chosen at
//! compile time.
//!
//! Every type has a *packed* encoding, which takes as many bytes as the value
//! needs and marks its own end, and most types also have a *fixed* one, which
//! takes the same number of bytes for every value. A place -- a field, an
//! element, a root -- holds one or the other: [`Slotted`] places hold the
//! fixed encoding, [`Packed`] places the packed one. The choice is a type
//! parameter of every guard and of `encode`, `decode` and `encoded_size`, so
//! that code for slotted places compiles to constant offsets and fixed-width
//! writes, with no branch on the choice at run time.

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::Slotted {}
    impl Sealed for super::Packed {}
}

/// Which of a type's two encodings a place holds: [`Slotted`] or [`Packed`].
///
/// Sealed: there are exactly these two.
///
/// ```
/// use kladde_persist::{Encoding, Packed, Persistable, Slotted};
///
/// assert!(!Slotted::PACKED && Packed::PACKED);
/// assert_eq!(<u32 as Persistable>::encoded_size::<Slotted>(&300), 4);
/// assert_eq!(<u32 as Persistable>::encoded_size::<Packed>(&300), 2);
/// ```
pub trait Encoding:
    sealed::Sealed + Copy + Default + std::fmt::Debug + Send + Sync + 'static
{
    /// Whether places of this encoding hold the packed encoding.
    const PACKED: bool;
}

/// The encoding of slotted places: a type's fixed encoding, the same number
/// of bytes for every value, at static offsets. Today's layout of every type
/// that has one, and what every place holds unless it chooses otherwise.
///
/// ```
/// use kladde_persist::{Persistable, Slotted};
///
/// assert_eq!(<u16 as Persistable>::to_bytes::<Slotted>(&7), [7, 0]);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Slotted;

/// The encoding of packed places: as many bytes as the value needs, which a
/// reader learns as it decodes. An enum takes its current variant only,
/// integers wider than a byte are LEB128 varints (zigzag for signed ones), and
/// a `char` is UTF-8.
///
/// ```
/// use kladde_persist::{Packed, Persistable};
///
/// let mut bytes = Vec::new();
/// <i32 as Persistable>::encode::<Packed>(&-2, &mut bytes);
/// <char as Persistable>::encode::<Packed>(&'é', &mut bytes);
/// assert_eq!(bytes, [3, 0xc3, 0xa9]);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Packed;

impl Encoding for Slotted {
    const PACKED: bool = false;
}

impl Encoding for Packed {
    const PACKED: bool = true;
}
