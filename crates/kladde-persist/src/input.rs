//! [`Input`]: the bytes a `decode` reads from, front to back.

use std::io::Read;

use kladde_store::{Error, ReadBackend, Word};

/// A cursor over stored bytes that values decode themselves from, front to
/// back, each learning where it ends as it goes.
///
/// A load reads an allocation's bytes once and decodes every value in it from
/// them, following pointers into other allocations through the backend.
/// Every read checks its bounds and fails with [`Error::Corrupt`] rather than
/// panicking.
///
/// ```
/// use kladde_persist::Input;
///
/// let mut input = Input::new(&[0xac, 0x02, 7]);
/// assert_eq!(input.varint()?, 300);
/// assert_eq!(input.byte()?, 7);
/// assert!(input.is_empty());
/// assert!(input.byte().is_err());
/// # Ok::<(), kladde_store::Error>(())
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Input<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Input<'a> {
    /// A cursor at the start of `bytes`. See [`Input`] for an example.
    pub fn new(bytes: &'a [u8]) -> Self {
        Input { bytes, position: 0 }
    }

    /// How many bytes have been read.
    pub fn position(&self) -> usize {
        self.position
    }

    /// How many bytes are left.
    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }

    /// Whether every byte has been read. See [`Input`] for an example.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// The next `n` bytes.
    ///
    /// ```
    /// let mut input = kladde_persist::Input::new(b"abc");
    /// assert_eq!(input.take(2)?, b"ab");
    /// assert!(input.take(2).is_err());
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if n > self.remaining() {
            return Err(Error::Corrupt(format!(
                "a value runs {} bytes past the end of its allocation",
                n - self.remaining()
            )));
        }
        let bytes = &self.bytes[self.position..self.position + n];
        self.position += n;
        Ok(bytes)
    }

    /// The next `N` bytes, as an array.
    ///
    /// ```
    /// let mut input = kladde_persist::Input::new(&[1, 0, 0, 0]);
    /// assert_eq!(u32::from_le_bytes(input.array()?), 1);
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        Ok(self.take(N)?.try_into().expect("take returns N bytes"))
    }

    /// The next byte. See [`Input`] for an example.
    pub fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    /// The next byte, without reading it.
    pub fn peek(&self) -> Result<u8, Error> {
        let mut ahead = *self;
        ahead.byte()
    }

    /// An unsigned LEB128 varint, which must be minimal, so that every value
    /// has one encoding. See [`Input`] for an example.
    pub fn varint(&mut self) -> Result<u64, Error> {
        let rest = &self.bytes[self.position..];
        let (value, after) = kladde_varint::decode_minimal(rest)
            .map_err(|e| Error::Corrupt(format!("invalid varint: {e}")))?;
        self.position += rest.len() - after.len();
        Ok(value)
    }

    /// A cursor over the next `n` bytes, which this one skips: for content
    /// whose length is stated before it.
    ///
    /// ```
    /// let mut input = kladde_persist::Input::new(b"abcd");
    /// let mut inner = input.sub(2)?;
    /// assert_eq!(inner.take(2)?, b"ab");
    /// assert!(inner.is_empty());
    /// assert_eq!(input.take(2)?, b"cd");
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn sub(&mut self, n: usize) -> Result<Input<'a>, Error> {
        Ok(Input::new(self.take(n)?))
    }

    /// Moves to `position` bytes from the start, which must not be behind
    /// the current position: for skipping the padding of a slot.
    pub fn skip_to(&mut self, position: usize) -> Result<(), Error> {
        if position < self.position {
            return Err(Error::Corrupt(format!(
                "a value took {} bytes more than its slot",
                self.position - position
            )));
        }
        self.take(position - self.position).map(|_| ())
    }
}

/// Every byte of allocation `p`, as of the last flush: what a load decodes
/// the values in an allocation from.
///
/// ```
/// use kladde_persist::read_allocation;
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let mut store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let p = store.alloc(0)?;
/// store.write(p.raw(), 0, b"hi")?;
/// store.flush()?;
/// assert_eq!(read_allocation(&mut store, p.raw())?, b"hi");
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub fn read_allocation<B: ReadBackend>(backend: &mut B, p: B::Pointer) -> Result<Vec<u8>, Error> {
    let size = backend.read_size(p)?.to_usize();
    let mut bytes = vec![0u8; size];
    backend
        .read_at(p, <B::Size as Word>::zero())?
        .read_exact(&mut bytes)?;
    Ok(bytes)
}
