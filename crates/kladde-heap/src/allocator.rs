//! [`Allocator`]: a **free-space-only** manager of `Address` ranges, plus
//! [`CompactingAllocator`] and [`SimpleAllocator`], a simple functional
//! implementation.
//!
//! This allocator does *less* than a classic one: it tracks only the set of
//! **free** address ranges (what it needs to satisfy `alloc`) and does **not**
//! record how the occupied complement is divided into individual allocations.
//! Per-allocation facts (id, size, sizedness) live in the *backend's* id table;
//! the allocator is *told* a size on `free`/`resize` and derives occupied runs
//! (for compaction) as the complement of its free ranges. See
//! `address-ranges-id-pool-decoupling.md`.
//!
//! It is fallible only for out-of-memory ([`AllocError::OutOfMemory`], from
//! `alloc`/`resize`) and corrupt / overlapping ranges ([`AllocError::Overlap`],
//! from `free`/`resize` when the given range intersects a free region -- a
//! double-free or a bad argument).

use std::collections::BTreeMap;

use crate::pointer::Sizedness;
use crate::word::Word;

/// The allocator's own errors. (A bad *id* is a `BackendError`, not this -- the
/// id table lives in the backend, not the allocator.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocError {
    /// `alloc`/`resize` couldn't find room. (Never produced by the in-memory
    /// [`SimpleAllocator`], whose address space is effectively unbounded.)
    OutOfMemory,
    /// `free`/`resize` was given a range that overlaps a free region -- a
    /// double-free or a corrupt argument (the range wasn't fully live).
    Overlap,
}

/// One contiguous run of neighbouring allocations that slides as a unit during
/// compaction. Every allocation in `[old, old + len)` moves by `new - old`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Move<A, S> {
    pub old: A,
    pub new: A,
    pub len: S,
}

/// Pure free-space management over an `Address` space. No ids, no per-allocation
/// table -- the backend owns those.
pub trait Allocator {
    type Address: Word;
    type Size: Word + Into<Self::Address>;

    /// Reserve a free range of `size`, returning its address. `Err(OutOfMemory)`
    /// if none fits. `sizedness` is a placement hint the allocator may ignore.
    fn alloc(
        &mut self,
        size: Self::Size,
        sizedness: Sizedness,
    ) -> Result<Self::Address, AllocError>;

    /// Release `[address, address + size)`. `Err(Overlap)` if that range overlaps
    /// a free region (double-free / corrupt argument).
    fn free(
        &mut self,
        address: Self::Address,
        size: Self::Size,
        sizedness: Sizedness,
    ) -> Result<(), AllocError>;

    /// Resize the range at `address`. `Ok(Some(new))` iff the bytes must move (the
    /// caller already knows the old address); `Ok(None)` if resized in place.
    /// `Err(OutOfMemory)` if a grow can't be satisfied; `Err(Overlap)` if the old
    /// range wasn't fully allocated.
    fn resize(
        &mut self,
        address: Self::Address,
        old_size: Self::Size,
        new_size: Self::Size,
    ) -> Result<Option<Self::Address>, AllocError>;

    /// One-past-the-end of the highest-addressed live region: the minimal file
    /// length *without* compaction (what you can truncate to right now).
    fn uncompacted_len(&self) -> Self::Address;
}

/// An [`Allocator`] that can defragment by sliding occupied runs down.
pub trait CompactingAllocator: Allocator {
    /// Plan the moves that pack all live data to the low end. One `Move` per
    /// occupied run (in-place runs included as `old == new` no-ops, so the caller
    /// can skip the byte copy and `apply_move` stays trivial). Derived purely from
    /// the free ranges -- no per-allocation input.
    fn plan_compaction(&self) -> Vec<Move<Self::Address, Self::Size>>;

    /// Commit one run's slide into the allocator's own free-space state. Apply the
    /// moves from [`plan_compaction`](CompactingAllocator::plan_compaction) in
    /// order; afterwards the allocator holds no free ranges and its length is
    /// [`compacted_len`](CompactingAllocator::compacted_len).
    fn apply_move(&mut self, m: Move<Self::Address, Self::Size>);

    /// Total live bytes = file length *after* a full compaction (contrast
    /// [`uncompacted_len`](Allocator::uncompacted_len)).
    fn compacted_len(&self) -> Self::Address;
}

// ============================ SimpleAllocator ============================

/// A simple, functional, **unoptimized** free-space allocator: a `BTreeMap` of
/// holes (below a bump `end`) with first-fit placement and neighbour coalescing.
/// `Address = u64`, `Size = u32`. Ignores the `sizedness` hint (single space).
///
/// Hole lengths are stored as `u64` (a coalesced hole can exceed a single
/// `Size`); individual allocations are still `Size`-bounded.
#[derive(Default)]
pub struct SimpleAllocator {
    free: BTreeMap<u64, u64>, // hole start -> length, all strictly below `end`
    end: u64,                 // high-water; everything at/above is unallocated
}

impl SimpleAllocator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Is `[address, address + size)` fully allocated (within `end`, disjoint from
    /// every hole)? The negation is the `Overlap` condition.
    fn is_allocated(&self, address: u64, size: u64) -> bool {
        let end = address + size;
        if end > self.end {
            return false;
        }
        // A hole starting inside the range?
        if self.free.range(address..end).next().is_some() {
            return false;
        }
        // The hole just before `address` reaching into the range?
        if let Some((&h_start, &h_len)) = self.free.range(..address).next_back() {
            if h_start + h_len > address {
                return false;
            }
        }
        true
    }

    /// First-fit over the holes, else bump `end`. Never fails (unbounded space).
    fn raw_alloc(&mut self, size: u64) -> u64 {
        let hit = self
            .free
            .iter()
            .find(|(_, &len)| len >= size)
            .map(|(&s, &l)| (s, l));
        if let Some((start, len)) = hit {
            self.free.remove(&start);
            if len > size {
                self.free.insert(start + size, len - size);
            }
            start
        } else {
            let addr = self.end;
            self.end += size;
            addr
        }
    }

    /// Return `[start, start + len)` to the free set, coalescing with neighbours,
    /// or lowering `end` if it is trailing.
    fn add_free(&mut self, start: u64, len: u64) {
        let end = start + len;
        if end == self.end {
            // Trailing: drop it into `end`, then absorb any hole now at the tail.
            self.end = start;
            while let Some((&h_start, &h_len)) = self.free.range(..self.end).next_back() {
                if h_start + h_len == self.end {
                    self.free.remove(&h_start);
                    self.end = h_start;
                } else {
                    break;
                }
            }
            return;
        }
        let mut new_start = start;
        let mut new_len = len;
        if let Some((&p_start, &p_len)) = self.free.range(..start).next_back() {
            if p_start + p_len == start {
                self.free.remove(&p_start);
                new_start = p_start;
                new_len += p_len;
            }
        }
        if let Some(&s_len) = self.free.get(&end) {
            self.free.remove(&end);
            new_len += s_len;
        }
        self.free.insert(new_start, new_len);
    }

    /// Occupied runs `(start, len)` = the complement of the free set below `end`.
    fn occupied_runs(&self) -> Vec<(u64, u64)> {
        let mut runs = Vec::new();
        let mut cursor = 0u64;
        for (&h_start, &h_len) in &self.free {
            if h_start > cursor {
                runs.push((cursor, h_start - cursor));
            }
            cursor = h_start + h_len;
        }
        if cursor < self.end {
            runs.push((cursor, self.end - cursor));
        }
        runs
    }
}

impl Allocator for SimpleAllocator {
    type Address = u64;
    type Size = u32;

    fn alloc(&mut self, size: u32, _sizedness: Sizedness) -> Result<u64, AllocError> {
        Ok(self.raw_alloc(size as u64))
    }

    fn free(&mut self, address: u64, size: u32, _sizedness: Sizedness) -> Result<(), AllocError> {
        if !self.is_allocated(address, size as u64) {
            return Err(AllocError::Overlap);
        }
        self.add_free(address, size as u64);
        Ok(())
    }

    fn resize(
        &mut self,
        address: u64,
        old_size: u32,
        new_size: u32,
    ) -> Result<Option<u64>, AllocError> {
        if !self.is_allocated(address, old_size as u64) {
            return Err(AllocError::Overlap);
        }
        if new_size <= old_size {
            let tail = (old_size - new_size) as u64;
            if tail > 0 {
                self.add_free(address + new_size as u64, tail);
            }
            return Ok(None);
        }
        let grow = (new_size - old_size) as u64;
        let tail_start = address + old_size as u64;
        // Grow in place off the top?
        if tail_start == self.end {
            self.end += grow;
            return Ok(None);
        }
        // Grow into the hole immediately following?
        if let Some(&hlen) = self.free.get(&tail_start) {
            if hlen >= grow {
                self.free.remove(&tail_start);
                if hlen > grow {
                    self.free.insert(tail_start + grow, hlen - grow);
                }
                return Ok(None);
            }
        }
        // Relocate: carve a fresh range (old still allocated, so disjoint), free old.
        let new_addr = self.raw_alloc(new_size as u64);
        self.add_free(address, old_size as u64);
        Ok(Some(new_addr))
    }

    fn uncompacted_len(&self) -> u64 {
        self.end
    }
}

impl CompactingAllocator for SimpleAllocator {
    fn plan_compaction(&self) -> Vec<Move<u64, u32>> {
        let mut moves = Vec::new();
        let mut packed = 0u64;
        for (start, len) in self.occupied_runs() {
            // Split runs longer than `Size` into Size-bounded chunks (same delta).
            let mut s = start;
            let mut remaining = len;
            while remaining > 0 {
                let chunk = remaining.min(u32::MAX as u64);
                moves.push(Move {
                    old: s,
                    new: packed,
                    len: chunk as u32,
                });
                s += chunk;
                packed += chunk;
                remaining -= chunk;
            }
        }
        moves
    }

    fn apply_move(&mut self, m: Move<u64, u32>) {
        // Runs are applied low-to-high; below the frontier everything is packed.
        let frontier = m.new + m.len as u64;
        self.free.retain(|&start, _| start >= frontier);
        self.end = frontier;
    }

    fn compacted_len(&self) -> u64 {
        self.end - self.free.values().sum::<u64>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_bumps_then_reuses_freed_holes() {
        let mut a = SimpleAllocator::new();
        let x = a.alloc(4, Sizedness::Fixed).unwrap();
        let y = a.alloc(8, Sizedness::Resizable).unwrap();
        assert_eq!(x, 0);
        assert_eq!(y, 4);
        assert_eq!(a.uncompacted_len(), 12);

        a.free(x, 4, Sizedness::Fixed).unwrap(); // hole [0,4)
        let z = a.alloc(4, Sizedness::Fixed).unwrap();
        assert_eq!(z, 0); // first-fit reuses the hole
        assert_eq!(a.uncompacted_len(), 12);
    }

    #[test]
    fn free_at_the_top_lowers_the_high_water() {
        let mut a = SimpleAllocator::new();
        let _x = a.alloc(4, Sizedness::Fixed).unwrap();
        let y = a.alloc(4, Sizedness::Fixed).unwrap();
        a.free(y, 4, Sizedness::Fixed).unwrap();
        assert_eq!(a.uncompacted_len(), 4); // trailing free reclaimed, not a hole
    }

    #[test]
    fn free_coalesces_adjacent_holes() {
        let mut a = SimpleAllocator::new();
        let x = a.alloc(4, Sizedness::Fixed).unwrap();
        let y = a.alloc(4, Sizedness::Fixed).unwrap();
        let _z = a.alloc(4, Sizedness::Fixed).unwrap(); // keep 12 as high-water
        a.free(x, 4, Sizedness::Fixed).unwrap();
        a.free(y, 4, Sizedness::Fixed).unwrap(); // coalesces into [0,8)
                                                 // an 8-byte alloc fits the coalesced hole at 0
        assert_eq!(a.alloc(8, Sizedness::Fixed).unwrap(), 0);
    }

    #[test]
    fn double_free_is_an_overlap_error() {
        let mut a = SimpleAllocator::new();
        let x = a.alloc(4, Sizedness::Fixed).unwrap();
        let _keep = a.alloc(4, Sizedness::Fixed).unwrap();
        a.free(x, 4, Sizedness::Fixed).unwrap();
        assert_eq!(a.free(x, 4, Sizedness::Fixed), Err(AllocError::Overlap));
    }

    #[test]
    fn resize_grows_in_place_off_the_top() {
        let mut a = SimpleAllocator::new();
        let x = a.alloc(4, Sizedness::Resizable).unwrap();
        assert_eq!(a.resize(x, 4, 8), Ok(None)); // top -> just bump
        assert_eq!(a.uncompacted_len(), 8);
    }

    #[test]
    fn resize_grows_into_a_following_hole_in_place() {
        let mut a = SimpleAllocator::new();
        let x = a.alloc(4, Sizedness::Resizable).unwrap();
        let y = a.alloc(4, Sizedness::Fixed).unwrap();
        let _z = a.alloc(4, Sizedness::Fixed).unwrap();
        a.free(y, 4, Sizedness::Fixed).unwrap(); // hole [4,8)
        assert_eq!(a.resize(x, 4, 8), Ok(None)); // grows into the hole, in place
    }

    #[test]
    fn resize_relocates_when_it_cannot_grow_in_place() {
        let mut a = SimpleAllocator::new();
        let x = a.alloc(4, Sizedness::Resizable).unwrap();
        let _y = a.alloc(4, Sizedness::Fixed).unwrap(); // blocks growth after x
        match a.resize(x, 4, 8).unwrap() {
            Some(new) => assert_ne!(new, x),
            None => panic!("expected relocation"),
        }
    }

    #[test]
    fn resize_shrinks_in_place_and_frees_the_tail() {
        let mut a = SimpleAllocator::new();
        let x = a.alloc(8, Sizedness::Resizable).unwrap();
        let _keep = a.alloc(4, Sizedness::Fixed).unwrap();
        assert_eq!(a.resize(x, 8, 4), Ok(None));
        // the freed tail [4,8) is reused by the next alloc
        assert_eq!(a.alloc(4, Sizedness::Fixed).unwrap(), 4);
    }

    #[test]
    fn compaction_packs_live_runs_and_reports_lengths() {
        let mut a = SimpleAllocator::new();
        let _x = a.alloc(4, Sizedness::Fixed).unwrap(); // [0,4)  live
        let y = a.alloc(4, Sizedness::Fixed).unwrap(); //  [4,8)  freed -> gap
        let _z = a.alloc(4, Sizedness::Fixed).unwrap(); // [8,12) live
        a.free(y, 4, Sizedness::Fixed).unwrap();
        assert_eq!(a.uncompacted_len(), 12);
        assert_eq!(a.compacted_len(), 8);

        let moves = a.plan_compaction();
        // run [0,4) stays (no-op), run [8,12) slides down to [4,8)
        assert_eq!(
            moves,
            vec![
                Move {
                    old: 0,
                    new: 0,
                    len: 4
                },
                Move {
                    old: 8,
                    new: 4,
                    len: 4
                },
            ]
        );
        for m in moves {
            a.apply_move(m);
        }
        assert_eq!(a.uncompacted_len(), 8); // now packed; no interior holes
        assert_eq!(a.compacted_len(), 8);
        assert!(a.free.is_empty());
    }
}
