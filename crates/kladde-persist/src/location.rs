//! [`Location`]: where a value's inline bytes live -- an `anchor` allocation and
//! an `offset` within it.

use kladde_heap::Pointer;

/// A displacement within an allocation: the `anchor` pointer plus a byte
/// `offset`. Parametric over both the pointer type `P` (default `Pointer`) and
/// the size type `S` (default `u32`), because `offset` is a size within an
/// allocation, not a `usize`.
///
/// `S` is deliberately **not** a `Persistable` type parameter: in a
/// `Persistable` signature the offset is named `Location<P, B::Size>`, so the
/// size type flows from the backend rather than being pinned onto the type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Location<P = Pointer, S = u32> {
    pub anchor: P,
    pub offset: S,
}

impl<P, S> Location<P, S> {
    pub fn new(anchor: P, offset: S) -> Self {
        Self { anchor, offset }
    }
}
