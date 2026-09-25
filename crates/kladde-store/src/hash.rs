//! A multiplicative hasher for maps keyed by allocation ids and page numbers.
//!
//! `std`'s SipHash defends against HashDoS, which a map whose keys come from
//! the file and the id allocator does not need, at a cost that shows on every
//! lookup of the allocation map.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

/// FxHash's mixing step, which is all an integer key needs.
#[derive(Default, Clone, Copy)]
pub struct IdHasher(u64);

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl Hasher for IdHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ b as u64).wrapping_mul(SEED);
        }
    }
    #[inline]
    fn write_u32(&mut self, n: u32) {
        self.0 = (self.0.rotate_left(5) ^ n as u64).wrapping_mul(SEED);
    }
    #[inline]
    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(SEED);
    }
    #[inline]
    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

pub type BuildIdHasher = BuildHasherDefault<IdHasher>;
/// A map keyed by an allocation id or a page number.
pub type IdMap<V> = HashMap<u32, V, BuildIdHasher>;
/// A set of allocation ids or page numbers.
pub type IdSet = HashSet<u32, BuildIdHasher>;
