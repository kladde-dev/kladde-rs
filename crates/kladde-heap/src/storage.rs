//! [`Storage`]: unstructured random + sequential byte access to a large block of
//! stored data (e.g. a file), orthogonal to [`Allocator`](crate::Allocator).
//!
//! Where `Allocator` models memory *management* (which id owns which address
//! range), `Storage` models memory *access* (the bytes themselves). A concrete
//! backend composes the two. The in-memory implementation used for tests and for
//! the public `MockBackend` lands with the backend that uses it, so this module
//! is just the trait; see `generic-allocator.md`'s note that the public in-memory
//! testing vehicle should be a `MockBackend`, not a public `MockStorage`.

use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};

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

/// In-memory [`Storage`] backing the public `MockBackend` and the crate's own
/// tests. Deliberately **not** public: `generic-allocator.md` wants the public
/// in-memory testing vehicle to be a `MockBackend` (which hides addresses),
/// not a raw storage. A `Cursor` provides the `Seek` a bare `Vec<u8>` lacks.
#[derive(Default)]
pub(crate) struct InMemoryStorage(Cursor<Vec<u8>>);

impl Read for InMemoryStorage {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}
impl Write for InMemoryStorage {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
impl Seek for InMemoryStorage {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.seek(pos)
    }
}
impl Storage for InMemoryStorage {
    fn resize(&mut self, new_len: u64) -> io::Result<()> {
        self.0.get_mut().resize(new_len as usize, 0);
        Ok(())
    }
    fn len(&self) -> io::Result<u64> {
        Ok(self.0.get_ref().len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_len_and_is_empty() {
        let mut s = InMemoryStorage::default();
        assert!(s.is_empty().unwrap());
        s.resize(4).unwrap();
        assert_eq!(s.len().unwrap(), 4);
        assert!(!s.is_empty().unwrap());

        s.seek(SeekFrom::Start(1)).unwrap();
        s.write_all(&[7, 8]).unwrap();
        s.seek(SeekFrom::Start(0)).unwrap();
        let mut buf = [0u8; 4];
        s.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [0, 7, 8, 0]);
    }
}
