//! [`Location`]: where a value's inline bytes live -- an `anchor` allocation
//! and an `offset` within it.

use kladde_store::{Pointer, Word};

/// Where a value's inline bytes live: the nearest ancestor that owns a real
/// allocation, the `anchor`, plus a byte `offset` within it.
///
/// Parametric over both the pointer type `P` (default `Pointer`) and the size
/// type `S` (default `u32`), because `offset` is a size within an allocation,
/// not a `usize`. In a `Persistable` signature it is named
/// `Location<P, B::Size>`, so the size type flows from the backend.
///
/// ```
/// use kladde_persist::Location;
/// use kladde_store::Pointer;
///
/// let root = Location::new(Pointer::<u32>::from_raw(7).unwrap(), 0u32);
/// let field = root + 4; // a field four bytes into the root
/// assert_eq!(field.anchor, root.anchor);
/// assert_eq!(field.offset, 4);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Location<P = Pointer, S = u32> {
    /// The allocation holding the bytes.
    pub anchor: P,
    /// Where within it the bytes start.
    pub offset: S,
}

impl<P, S> Location<P, S> {
    /// The location `offset` bytes into `anchor`. See [`Location`] for an
    /// example.
    pub fn new(anchor: P, offset: S) -> Self {
        Self { anchor, offset }
    }
}

impl<P, S: Word> std::ops::Add<S> for Location<P, S> {
    type Output = Location<P, S>;

    /// Advances the location by `offset` bytes within the same anchor -- how
    /// an inline value reaches a field or element at a static offset.
    fn add(self, offset: S) -> Location<P, S> {
        Location {
            anchor: self.anchor,
            offset: self.offset + offset,
        }
    }
}
