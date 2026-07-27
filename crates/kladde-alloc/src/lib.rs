//! A mock, purely in-memory implementation of the replay target
//! [`kladde_traits::Allocator`] describes: given an already-decided index
//! (indices are minted eagerly by whatever calls the public `Allocator`
//! trait -- see `kladde`'s `DefaultBackend` -- not by this crate), this
//! is what actually materializes, reads, and mutates bytes.
//!
//! v1 deliberately defers the real, file-backed, compaction-capable
//! allocator (see `spec.md`'s "Pointers and Memory Management"): the plan
//! is to build the rest of the system against this mock first, and let
//! its real requirements emerge from that rather than guessing upfront.

use std::cell::RefCell;
use std::collections::HashMap;
use std::num::NonZeroU32;

/// Each allocation is a `Box<[u8]>`, keyed by an index that's assigned
/// *elsewhere* (by `DefaultBackend`, at the moment a `Guard` calls an
/// `Allocator` allocation method) -- this type only ever materializes an
/// index it's told to use, it never generates one itself. See `spec.md`'s
/// Pointers
/// and Memory Management for why index generation (eager, so an index is
/// a stable identity from the moment anything might reference it) and
/// content materialization (deferred to flush, here) are split this way.
#[derive(Default)]
pub struct MockAllocator {
    regions: RefCell<HashMap<NonZeroU32, Box<[u8]>>>,
}

impl MockAllocator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of currently-live (not yet freed) allocations -- mainly
    /// useful for tests and leak-checking.
    pub fn live_count(&self) -> usize {
        self.regions.borrow().len()
    }

    /// `Some(index)` (the mock stands the index in for a real on-disk
    /// target, since there's no real file to compute an offset against)
    /// if `index` has actually been materialized (an `alloc` for it has
    /// been replayed); `None` if it's only been minted, not yet flushed.
    pub fn resolve(&self, index: NonZeroU32) -> Option<NonZeroU32> {
        self.regions.borrow().contains_key(&index).then_some(index)
    }

    /// Returns the current byte size of the region at `index`, or `None`
    /// if no such region is materialized.
    ///
    /// ```
    /// use kladde_alloc::MockAllocator;
    /// use std::num::NonZeroU32;
    ///
    /// let alloc = MockAllocator::new();
    /// let index = NonZeroU32::new(1).unwrap();
    /// assert_eq!(alloc.capacity(index), None);
    /// alloc.materialize_alloc(index, 8);
    /// assert_eq!(alloc.capacity(index), Some(8));
    /// ```
    pub fn capacity(&self, index: NonZeroU32) -> Option<usize> {
        self.regions.borrow().get(&index).map(|region| region.len())
    }

    pub fn read(&self, index: NonZeroU32, offset: u32, len: u32) -> Vec<u8> {
        let regions = self.regions.borrow();
        let region = regions
            .get(&index)
            .unwrap_or_else(|| panic!("MockAllocator: read from unmaterialized index {index}"));
        let start = offset as usize;
        region[start..start + len as usize].to_vec()
    }

    /// Materializes real storage for `index`, which must have been
    /// minted (by whatever assigns indices) but not already materialized.
    pub fn materialize_alloc(&self, index: NonZeroU32, size: usize) {
        let previous = self
            .regions
            .borrow_mut()
            .insert(index, vec![0u8; size].into_boxed_slice());
        assert!(
            previous.is_none(),
            "MockAllocator: index {index} already has a live allocation"
        );
    }

    pub fn materialize_free(&self, index: NonZeroU32) {
        let existed = self.regions.borrow_mut().remove(&index).is_some();
        assert!(
            existed,
            "MockAllocator: free of index {index}, which wasn't materialized or was already freed"
        );
    }

    pub fn materialize_write(&self, index: NonZeroU32, offset: u32, bytes: &[u8]) {
        let mut regions = self.regions.borrow_mut();
        let region = regions
            .get_mut(&index)
            .unwrap_or_else(|| panic!("MockAllocator: write to unmaterialized index {index}"));
        let start = offset as usize;
        region[start..start + bytes.len()].copy_from_slice(bytes);
    }

    pub fn materialize_copy(
        &self,
        src: NonZeroU32,
        src_offset: u32,
        len: u32,
        dst: NonZeroU32,
        dst_offset: u32,
    ) {
        let mut regions = self.regions.borrow_mut();
        if src == dst {
            let region = regions
                .get_mut(&src)
                .unwrap_or_else(|| panic!("MockAllocator: copy within unmaterialized index {src}"));
            let src_start = src_offset as usize;
            let dst_start = dst_offset as usize;
            region.copy_within(src_start..src_start + len as usize, dst_start);
        } else {
            let bytes = {
                let region = regions.get(&src).unwrap_or_else(|| {
                    panic!("MockAllocator: copy from unmaterialized index {src}")
                });
                let start = src_offset as usize;
                region[start..start + len as usize].to_vec()
            };
            let dst_region = regions
                .get_mut(&dst)
                .unwrap_or_else(|| panic!("MockAllocator: copy into unmaterialized index {dst}"));
            let dst_start = dst_offset as usize;
            dst_region[dst_start..dst_start + bytes.len()].copy_from_slice(&bytes);
        }
    }

    /// Replaces the `old_len` bytes at `offset` in the region at `index`
    /// with `new`, shifting the trailing bytes and resizing the region by
    /// `new.len() - old_len`.
    ///
    /// The region's identity (`index`) is unchanged.
    ///
    /// ```
    /// use kladde_alloc::MockAllocator;
    /// use std::num::NonZeroU32;
    ///
    /// let alloc = MockAllocator::new();
    /// let index = NonZeroU32::new(1).unwrap();
    /// alloc.materialize_alloc(index, 4);
    /// alloc.materialize_write(index, 0, &[1, 2, 3, 4]);
    /// alloc.materialize_splice(index, 1, 2, &[9, 9, 9]); // replace 2 bytes with 3
    /// assert_eq!(alloc.read(index, 0, 5), vec![1, 9, 9, 9, 4]);
    /// ```
    pub fn materialize_splice(&self, index: NonZeroU32, offset: u32, old_len: u32, new: &[u8]) {
        let mut regions = self.regions.borrow_mut();
        let region = regions
            .get_mut(&index)
            .unwrap_or_else(|| panic!("MockAllocator: splice of unmaterialized index {index}"));
        let mut bytes = std::mem::take(region).into_vec();
        let start = offset as usize;
        bytes.splice(start..start + old_len as usize, new.iter().copied());
        *region = bytes.into_boxed_slice();
    }

    /// Grows or shrinks the region at `index` in place, preserving
    /// `min(old_size, new_size)` bytes from the start -- `index` itself
    /// never changes, only how much storage it identifies.
    pub fn materialize_resize(&self, index: NonZeroU32, new_size: usize) {
        let mut regions = self.regions.borrow_mut();
        let region = regions
            .get_mut(&index)
            .unwrap_or_else(|| panic!("MockAllocator: resize of unmaterialized index {index}"));
        let mut new_region = vec![0u8; new_size].into_boxed_slice();
        let keep = region.len().min(new_size);
        new_region[..keep].copy_from_slice(&region[..keep]);
        *region = new_region;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_then_write_then_read_round_trips() {
        let alloc = MockAllocator::new();
        let index = NonZeroU32::new(1).unwrap();
        alloc.materialize_alloc(index, 8);
        alloc.materialize_write(index, 2, &[1, 2, 3]);
        assert_eq!(alloc.read(index, 2, 3), vec![1, 2, 3]);
        assert_eq!(alloc.read(index, 0, 2), vec![0, 0]);
    }

    #[test]
    fn resolve_reflects_materialization_not_mere_minting() {
        let alloc = MockAllocator::new();
        let index = NonZeroU32::new(1).unwrap();
        assert_eq!(alloc.resolve(index), None);
        alloc.materialize_alloc(index, 4);
        assert_eq!(alloc.resolve(index), Some(index));
    }

    #[test]
    fn resolve_fails_after_free() {
        let alloc = MockAllocator::new();
        let index = NonZeroU32::new(1).unwrap();
        alloc.materialize_alloc(index, 4);
        alloc.materialize_free(index);
        assert_eq!(alloc.resolve(index), None);
    }

    #[test]
    fn live_count_tracks_alloc_and_free() {
        let alloc = MockAllocator::new();
        assert_eq!(alloc.live_count(), 0);

        let a = NonZeroU32::new(1).unwrap();
        let b = NonZeroU32::new(2).unwrap();
        alloc.materialize_alloc(a, 4);
        alloc.materialize_alloc(b, 4);
        assert_eq!(alloc.live_count(), 2);

        alloc.materialize_free(a);
        assert_eq!(alloc.live_count(), 1);
    }

    #[test]
    #[should_panic(expected = "already has a live allocation")]
    fn double_alloc_of_the_same_index_panics() {
        let alloc = MockAllocator::new();
        let index = NonZeroU32::new(1).unwrap();
        alloc.materialize_alloc(index, 4);
        alloc.materialize_alloc(index, 4);
    }

    #[test]
    #[should_panic(expected = "wasn't materialized or was already freed")]
    fn freeing_an_unknown_index_panics() {
        let alloc = MockAllocator::new();
        alloc.materialize_free(NonZeroU32::new(999).unwrap());
    }

    #[test]
    fn resize_grows_preserving_prefix_and_zero_fills_the_rest() {
        let alloc = MockAllocator::new();
        let index = NonZeroU32::new(1).unwrap();
        alloc.materialize_alloc(index, 2);
        alloc.materialize_write(index, 0, &[9, 9]);
        alloc.materialize_resize(index, 4);
        assert_eq!(alloc.read(index, 0, 4), vec![9, 9, 0, 0]);
    }

    #[test]
    fn resize_shrinks_truncating_the_tail() {
        let alloc = MockAllocator::new();
        let index = NonZeroU32::new(1).unwrap();
        alloc.materialize_alloc(index, 4);
        alloc.materialize_write(index, 0, &[1, 2, 3, 4]);
        alloc.materialize_resize(index, 2);
        assert_eq!(alloc.read(index, 0, 2), vec![1, 2]);
    }

    #[test]
    fn copy_shifts_a_span_within_the_same_allocation() {
        let alloc = MockAllocator::new();
        let index = NonZeroU32::new(1).unwrap();
        alloc.materialize_alloc(index, 8);
        alloc.materialize_write(index, 0, &[1, 2, 3, 4, 5, 6, 7, 8]);
        // Simulate removing element 0 of a 4-element, 2-byte-wide vec:
        // shift elements 1..4 down into slots 0..3.
        alloc.materialize_copy(index, 2, 6, index, 0);
        assert_eq!(alloc.read(index, 0, 6), vec![3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn splice_deletes_inserts_and_replaces_adjusting_size() {
        let alloc = MockAllocator::new();
        let index = NonZeroU32::new(1).unwrap();
        alloc.materialize_alloc(index, 6);
        alloc.materialize_write(index, 0, &[1, 2, 3, 4, 5, 6]);

        // Delete the 2 bytes at offset 1 (shifts the tail, shrinks to 4).
        alloc.materialize_splice(index, 1, 2, &[]);
        assert_eq!(alloc.capacity(index), Some(4));
        assert_eq!(alloc.read(index, 0, 4), vec![1, 4, 5, 6]);

        // Insert 3 bytes at offset 2 (no removal, grows to 7).
        alloc.materialize_splice(index, 2, 0, &[7, 8, 9]);
        assert_eq!(alloc.capacity(index), Some(7));
        assert_eq!(alloc.read(index, 0, 7), vec![1, 4, 7, 8, 9, 5, 6]);

        // Replace 7 bytes at offset 0 with 2 (whole-content replace).
        alloc.materialize_splice(index, 0, 7, &[42, 43]);
        assert_eq!(alloc.capacity(index), Some(2));
        assert_eq!(alloc.read(index, 0, 2), vec![42, 43]);
    }

    #[test]
    fn copy_moves_a_span_between_two_allocations() {
        let alloc = MockAllocator::new();
        let src = NonZeroU32::new(1).unwrap();
        let dst = NonZeroU32::new(2).unwrap();
        alloc.materialize_alloc(src, 4);
        alloc.materialize_alloc(dst, 4);
        alloc.materialize_write(src, 0, &[1, 2, 3, 4]);
        alloc.materialize_copy(src, 0, 4, dst, 0);
        assert_eq!(alloc.read(dst, 0, 4), vec![1, 2, 3, 4]);
    }
}
