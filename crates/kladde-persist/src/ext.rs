//! [`WriteBackendExt`]: typed allocation conveniences layered on `WriteBackend`.
//!
//! These live here (in the `Persistable` crate), not in `kladde-heap`, because
//! they reference `T: Persistable` for `T::INLINE_SIZE` -- the raw heap is
//! deliberately type-agnostic. They extend `WriteBackend` (whose `alloc_*` is
//! `&self`), so the conveniences are `&self` too. Blanket-implemented for every
//! `WriteBackend`.

use kladde_heap::{
    UniquePointer, UniquePointerFixedSize, UniquePointerResizable, Word, WriteBackend,
};

use crate::persistable::Persistable;
use crate::repr::PointerRepr;

/// Typed convenience allocations. Available on any [`WriteBackend`] whose pointer
/// type is a [`PointerRepr`] (so the element type can be `Persistable` against it).
pub trait WriteBackendExt: WriteBackend
where
    Self::Pointer: PointerRepr,
{
    /// Allocate a fixed-size region sized for one `T` and tag it with `T`.
    fn alloc_typed<T: Persistable<Self::Pointer>>(&self) -> UniquePointer<T, Self::Pointer> {
        UniquePointer::from_fixed(self.alloc_fixed_size(Word::from_usize(T::INLINE_SIZE)))
    }

    /// Free a typed allocation (the inherent demotion, then `free_fixed_size`).
    fn free_typed<T>(&self, p: UniquePointer<T, Self::Pointer>) {
        self.free_fixed_size(p.into_fixed());
    }

    /// A fixed-size array of `len` inline `T`s.
    fn alloc_fixed_size_array<T: Persistable<Self::Pointer>>(
        &self,
        len: usize,
    ) -> UniquePointerFixedSize<Self::Pointer> {
        self.alloc_fixed_size(Word::from_usize(len * T::INLINE_SIZE))
    }

    /// A resizable array of `len` inline `T`s.
    fn alloc_resizable_array<T: Persistable<Self::Pointer>>(
        &self,
        len: usize,
    ) -> UniquePointerResizable<Self::Pointer> {
        self.alloc_resizable(Word::from_usize(len * T::INLINE_SIZE))
    }
}

impl<B: WriteBackend + ?Sized> WriteBackendExt for B where B::Pointer: PointerRepr {}

#[cfg(test)]
mod tests {
    use super::*;
    use kladde_heap::MockBackend;

    #[test]
    fn alloc_typed_sizes_the_region_for_the_type() {
        let b = MockBackend::new();
        let p = b.alloc_typed::<u64>();
        // u64's INLINE_SIZE is 8; the backend should report that size.
        assert_eq!(kladde_heap::Backend::size(&b, p.raw()).unwrap(), 8);
        b.free_typed(p);
    }

    #[test]
    fn alloc_array_sizes_by_element_count() {
        let b = MockBackend::new();
        let p = b.alloc_fixed_size_array::<u32>(3);
        assert_eq!(kladde_heap::Backend::size(&b, p.raw()).unwrap(), 12);
    }
}
