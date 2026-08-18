//! Shared test fixtures for this crate's container tests.
//!
//! The vehicle is `kladde-heap`'s public [`MockBackend`], which applies every
//! operation immediately -- so a test can write and read back without a flush
//! step -- and counts live allocations, which is what the leak-checking
//! assertions here rely on.

pub use kladde_persist::MockBackend;

use kladde_persist::{Backend, Location, Word, WriteBackend};

/// A location backed by a real allocation, sized for whatever root type a test
/// uses.
pub fn root_location(
    backend: &MockBackend,
    size: usize,
) -> Location<<MockBackend as Backend>::Pointer, <MockBackend as Backend>::Size> {
    let pointer = backend.alloc_fixed_size(Word::from_usize(size));
    Location::new(pointer.raw(), 0)
}
