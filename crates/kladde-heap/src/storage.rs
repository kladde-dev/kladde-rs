//! [`Storage`]: unstructured random + sequential byte access to a large block of
//! stored data (e.g. a file), orthogonal to [`Allocator`](crate::Allocator).
//!
//! Where `Allocator` models memory *management* (which id owns which address
//! range), `Storage` models memory *access* (the bytes themselves). A concrete
//! backend composes the two. The in-memory implementation used for tests and for
//! the public `MockBackend` lands with the backend that uses it, so this module
//! is just the trait; see `generic-allocator.md`'s note that the public in-memory
//! testing vehicle should be a `MockBackend`, not a public `MockStorage`.

use std::io::{self, Read, Seek, Write};

/// A resizable, seekable byte store. `Read + Write + Seek` give random and
/// sequential access; `resize`/`len` manage the overall length.
///
/// A concrete implementation may reserve a tiny fixed-size header region it
/// manages itself by offsetting all addresses past it (holding only a magic
/// number and allocator parameters -- anything larger belongs in an allocation
/// so the header size can stay frozen).
pub trait Storage: Read + Write + Seek {
    /// Grow or shrink the store to exactly `new_len` bytes (new bytes zeroed).
    fn resize(&mut self, new_len: u64) -> io::Result<()>;
    /// The current length in bytes.
    fn len(&self) -> io::Result<u64>;
    /// Whether the store is empty.
    fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }
}
