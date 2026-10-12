//! The trait the serialization layer records mutations through.
//!
//! [`WriteBackend`] takes `&self` so that a parent guard can hand one backend
//! to all of its children's guards by reborrow.

use crate::error::Error;

/// The bytes a splice removed inside a transaction, which a
/// [`paste`](WriteBackend::paste) of the same transaction can insert
/// elsewhere, once, without carrying them.
///
/// It is neither `Copy` nor `Clone`, so it is pasted at most once.
///
/// ```
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// store.splice(0, 0, b"abcdef")?;
/// store.atomically(|| {
///     let cut = store.splice(1, 2, b"")?.expect("a cut");
///     assert_eq!(cut.len(), 2);
///     store.paste(3, 0, cut)
/// })?;
/// store.flush()?;
/// assert_eq!(store.read_all()?, b"adebcf");
/// # Ok::<(), kladde_store::Error>(())
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct CutId {
    pub(crate) serial: u64,
    pub(crate) index: u32,
    pub(crate) len: u64,
}

impl CutId {
    /// The bytes the cut holds.
    ///
    /// See [`CutId`] for an example.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the cut holds no bytes, which never happens: a splice that
    /// removes nothing makes no cut.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Recording mutations of the sequence, as guards do.
///
/// Outside a transaction or batch, every method appends to the journal before
/// it returns, so a mutation that returned `Ok` survives an application
/// crash; a power cut may still lose it until the next flush. Positions name
/// bytes of the sequence as every operation recorded so far leaves it; one
/// outside it fails with [`Error::OutOfBounds`] and records nothing.
///
/// ```
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// store.splice(0, 0, b"hello")?;
/// store.write(0, b"j")?;
/// store.move_range(0, 1, 4)?;
/// assert!(store.write(9, b"x").is_err());
/// store.flush()?;
/// assert_eq!(store.read_all()?, b"elloj");
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub trait WriteBackend {
    /// The sequence's length, including every operation recorded so far.
    fn len(&self) -> u64;

    /// Whether the sequence is empty.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// assert!(store.is_empty());
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Overwrites `bytes.len()` bytes at `pos`.
    fn write(&self, pos: u64, bytes: &[u8]) -> Result<(), Error>;

    /// Replaces the `removed` bytes at `pos` with `bytes`, shifting what
    /// follows. Inside a transaction, what it removes is a cut, which it
    /// returns for a later [`paste`](Self::paste); otherwise, or if it
    /// removes nothing, it returns `None`.
    fn splice(&self, pos: u64, removed: u64, bytes: &[u8]) -> Result<Option<CutId>, Error>;

    /// Cuts `[from, from + len)` and inserts it at `to`, a position in the
    /// sequence without the cut bytes. Writes none of the moved bytes.
    fn move_range(&self, from: u64, len: u64, to: u64) -> Result<(), Error>;

    /// Replaces the `removed` bytes at `pos` with the bytes of `cut`, which a
    /// splice of the open transaction made. See [`CutId`] for an example.
    ///
    /// Fails with [`Error::Corrupt`], recording nothing, if `cut` comes from
    /// another transaction.
    fn paste(&self, pos: u64, removed: u64, cut: CutId) -> Result<(), Error>;

    /// Runs `f` as one transaction: the operations it records reach the
    /// journal together once it returns `Ok`, and a crash keeps all of them
    /// or none.
    ///
    /// If `f` fails, what it recorded is discarded and the error returned,
    /// as if `f` had never run; if the append at the end fails, the store is
    /// poisoned. Nests inside other transactions and batches.
    fn atomically<R>(&self, f: impl FnOnce() -> Result<R, Error>) -> Result<R, Error>;

    /// Opens a transaction, as [`atomically`](Self::atomically) does around
    /// its closure; for a caller that holds one open across calls, such as
    /// a cut.
    fn begin_transaction(&self) -> Result<(), Error>;

    /// Ends the transaction [`begin_transaction`](Self::begin_transaction)
    /// opened.
    fn end_transaction(&self) -> Result<(), Error>;

    /// Poisons the store, for a caller that can no longer vouch for the
    /// in-memory values mirroring it, or that failed where it cannot report
    /// the failure.
    fn poison(&self);
}

/// A shared reference records through what it refers to, so that a guard
/// can hold `&Store` by value and hand copies of it to whatever outlives
/// the guard, such as a cut.
///
/// ```
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// fn append(backend: impl WriteBackend, bytes: &[u8]) -> kladde_store::Result<()> {
///     backend.splice(backend.len(), 0, bytes).map(|_| ())
/// }
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// append(&store, b"ab")?;
/// assert_eq!(store.len(), 2);
/// # Ok::<(), kladde_store::Error>(())
/// ```
impl<B: WriteBackend> WriteBackend for &B {
    fn len(&self) -> u64 {
        (**self).len()
    }
    fn write(&self, pos: u64, bytes: &[u8]) -> Result<(), Error> {
        (**self).write(pos, bytes)
    }
    fn splice(&self, pos: u64, removed: u64, bytes: &[u8]) -> Result<Option<CutId>, Error> {
        (**self).splice(pos, removed, bytes)
    }
    fn move_range(&self, from: u64, len: u64, to: u64) -> Result<(), Error> {
        (**self).move_range(from, len, to)
    }
    fn paste(&self, pos: u64, removed: u64, cut: CutId) -> Result<(), Error> {
        (**self).paste(pos, removed, cut)
    }
    fn atomically<R>(&self, f: impl FnOnce() -> Result<R, Error>) -> Result<R, Error> {
        (**self).atomically(f)
    }
    fn begin_transaction(&self) -> Result<(), Error> {
        (**self).begin_transaction()
    }
    fn end_transaction(&self) -> Result<(), Error> {
        (**self).end_transaction()
    }
    fn poison(&self) {
        (**self).poison()
    }
}
