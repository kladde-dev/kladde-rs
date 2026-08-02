//! [`Persistable`]: a type that knows how to store itself into, and load itself
//! from, a backend allocation at a given [`Location`].
//!
//! Parametric over the *pointer type* `P` (default `Pointer`), **not** over the
//! size type: allocation sizes belong to the allocator (a container queries
//! `backend.size(ptr)` rather than storing a size inline), and offsets are
//! transient (computed at the moment of a read/write, never stored). So `P` is
//! the only width a `Persistable` type is pinned to, and `Size` flows from the
//! backend as `B::Size`.

use kladde_heap::{ReadBackend, WriteBackend};
use std::io::Read;

use crate::location::Location;
use crate::repr::PointerRepr;

/// A type with a fixed inline byte size that can round-trip through a backend.
///
/// The `P: PointerRepr` bound is what lets pointer-holding implementors serialize
/// `Option<P>` with the on-file null niche; a pointer-free type simply ignores
/// it and works at every width.
pub trait Persistable<P: PointerRepr = kladde_heap::Pointer>: Sized {
    /// The number of bytes this value occupies *inline* in its parent allocation
    /// (typically just a pointer id for a container; the payload lives behind it).
    const INLINE_SIZE: usize;

    /// Write `self`'s inline bytes at `location`. Gets a shared `&WriteBackend`
    /// (the guard-reborrow model); must never read from the backend.
    fn store<B: WriteBackend<Pointer = P>>(&mut self, backend: &B, location: Location<P, B::Size>);

    /// Reconstruct a value from its inline bytes at `location`. Gets an exclusive
    /// `&mut ReadBackend` (loads are sequential, so a single `&mut` reborrowed
    /// down the recursion suffices).
    fn load<B: ReadBackend<Pointer = P>>(backend: &mut B, location: Location<P, B::Size>) -> Self;
}

/// Fixed-width little-endian scalars are `Persistable` at *every* pointer width
/// (they hold no pointers), so they implement `Persistable<P>` for all `P`.
macro_rules! impl_scalar {
    ($($t:ty),* $(,)?) => {
        $(
            impl<P: PointerRepr> Persistable<P> for $t {
                const INLINE_SIZE: usize = std::mem::size_of::<$t>();

                fn store<B: WriteBackend<Pointer = P>>(
                    &mut self,
                    backend: &B,
                    location: Location<P, B::Size>,
                ) {
                    backend.write(location.anchor, location.offset, &self.to_le_bytes());
                }

                fn load<B: ReadBackend<Pointer = P>>(
                    backend: &mut B,
                    location: Location<P, B::Size>,
                ) -> Self {
                    let mut buf = [0u8; std::mem::size_of::<$t>()];
                    let mut cursor = backend.read_at(location.anchor, location.offset);
                    cursor.read_exact(&mut buf).expect("read scalar bytes");
                    <$t>::from_le_bytes(buf)
                }
            }
        )*
    };
}

impl_scalar!(u8, u16, u32, u64, i8, i16, i32, i64);

#[cfg(test)]
mod tests {
    use super::*;
    use kladde_heap::{MockBackend, WriteBackend};

    #[test]
    fn scalar_round_trips_through_a_backend() {
        let mut b = MockBackend::new();
        let root = b.alloc_fixed_size(<i32 as Persistable>::INLINE_SIZE as u32);

        let mut value = -12345i32;
        value.store(&b, Location::new(root.raw(), 0));

        let loaded = i32::load(&mut b, Location::new(root.raw(), 0));
        assert_eq!(loaded, -12345);
    }

    #[test]
    fn multiple_scalars_at_distinct_offsets() {
        let mut b = MockBackend::new();
        let root = b.alloc_fixed_size(12);
        (10u32).store(&b, Location::new(root.raw(), 0));
        (20u32).store(&b, Location::new(root.raw(), 4));
        (30u32).store(&b, Location::new(root.raw(), 8));

        assert_eq!(u32::load(&mut b, Location::new(root.raw(), 4)), 20);
        assert_eq!(u32::load(&mut b, Location::new(root.raw(), 8)), 30);
        assert_eq!(u32::load(&mut b, Location::new(root.raw(), 0)), 10);
    }
}
