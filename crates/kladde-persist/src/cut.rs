//! [`Cut`]: a value cut out of its container, whose bytes the store keeps
//! until a paste of the same transaction puts them elsewhere.

use kladde_store::{CutId, Error};

use crate::region::Backend;

/// A value cut out of a container, to be pasted into another without being
/// encoded again: the in-memory value, and the cut of its bytes in the
/// store's open transaction, which the `Cut` holds open, nested in any the
/// application opened, for as long as it lives.
///
/// Containers make cuts (`PersistableVec::cut`, say) and take them where
/// they take values (`PersistableVec::insert_cut`); a paste writes none of
/// the value's bytes, and the flush states a few spans for the cut and the
/// paste together, however large the value.
///
/// - It dereferences to its value, but gives no mutable access, since its
///   bytes must stay the value's encoding until they are pasted.
/// - It is neither `Copy` nor `Clone`, and typed by its value's type, so it
///   is pasted at most once, and only where its type goes.
/// - It borrows the store, not the guard it came from, so the application
///   can drop the source's guard and make the destination's while it holds
///   the cut.
/// - Dropping it unpasted ends the transaction it holds open, which removes
///   the value for good; [`commit`](Cut::commit) does the same and returns a
///   `Result`. A drop cannot return the failure of its append, so it
///   poisons the store instead, and the next call reports it.
///
/// No flush can happen while a cut lives, since the transaction is open:
/// a cut is for a move within one operation, not for a clipboard.
///
/// ```
/// use kladde_persist::{Cut, Persistable, Region};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// store.splice(0, 0, b"abcdef")?;
/// // What a container does: open the cut's transaction, splice the value out
/// // inside it, and hand the bytes' cut to a `Cut` with the value.
/// store.begin_transaction()?;
/// let id = Region::root(&store).transaction(|r| r.splice(1, 2, b""))?;
/// let cut = Cut::new(*b"bc", id, &store);
/// assert_eq!(*cut, *b"bc");
/// let value = cut.paste_with(|id| store.paste(4, 0, id.unwrap()))?;
/// assert_eq!(value, *b"bc");
/// store.flush()?;
/// assert_eq!(store.read_all()?, b"adefbc");
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub struct Cut<T, B: Backend> {
    value: Option<T>,
    id: Option<CutId>,
    backend: B,
    /// Whether the transaction the cut held open has ended.
    ended: bool,
}

impl<T, B: Backend> Cut<T, B> {
    /// A cut of `value`, whose bytes the store's cut `id` holds (`None` for a
    /// value that encodes to no bytes), holding open the transaction in
    /// which the container spliced it out. The container opens that
    /// transaction with [`begin_transaction`](kladde_store::WriteBackend::begin_transaction)
    /// before it splices; the cut ends it. See [`Cut`] for an example.
    pub fn new(value: T, id: Option<CutId>, backend: B) -> Self {
        Cut {
            value: Some(value),
            id,
            backend,
            ended: false,
        }
    }

    /// Drops the value for good: ends the transaction the cut held open,
    /// which appends it to the journal.
    ///
    /// ```
    /// use kladde_persist::{Cut, Region};
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// store.splice(0, 0, b"abc")?;
    /// store.begin_transaction()?;
    /// let id = Region::root(&store).transaction(|r| r.splice(0, 1, b""))?;
    /// Cut::new(b'a', id, &store).commit()?;
    /// store.flush()?;
    /// assert_eq!(store.read_all()?, b"bc");
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn commit(mut self) -> Result<(), Error> {
        self.value = None;
        self.id = None;
        self.end()
    }

    /// Records the paste of the cut's bytes with `paste`, which a container
    /// makes record a paste of the cut, and whatever else its insertion
    /// needs, in one transaction; then ends the transaction the cut held
    /// open, and returns the value for the container to keep. See [`Cut`]
    /// for an example.
    pub fn paste_with(
        mut self,
        paste: impl FnOnce(Option<CutId>) -> Result<(), Error>,
    ) -> Result<T, Error> {
        let value = self
            .value
            .take()
            .expect("a cut holds its value until pasted");
        let pasted = paste(self.id.take());
        let ended = self.end();
        pasted?;
        ended?;
        Ok(value)
    }

    /// The bytes the value's encoding takes.
    pub fn encoded_len(&self) -> u64 {
        self.id.as_ref().map_or(0, CutId::len)
    }

    fn end(&mut self) -> Result<(), Error> {
        let backend = self.backend;
        // Done with the transaction either way: make the drop a no-op.
        self.value = None;
        self.id = None;
        self.ended = true;
        backend.end_transaction()
    }
}

impl<T, B: Backend> std::ops::Deref for Cut<T, B> {
    type Target = T;

    fn deref(&self) -> &T {
        self.value
            .as_ref()
            .expect("a cut holds its value until pasted")
    }
}

impl<T, B: Backend> Drop for Cut<T, B> {
    fn drop(&mut self) {
        if !self.ended && self.end().is_err() {
            self.backend.poison();
        }
    }
}
