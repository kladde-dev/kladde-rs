//! [`Cut`]: a value cut out of its container, whose bytes the store keeps at
//! hand until the journal that removed them is folded.

use kladde_store::{CutId, Error};

use crate::durable::Durable;
use crate::region::Backend;

/// A value cut out of a container, to be pasted into another without being
/// encoded again: the in-memory value, and the cut of its bytes.
///
/// Containers make cuts (`DurableVec::cut`, say) and take them where
/// they take values (`DurableVec::insert_cut`). A cut is a weak promise:
/// while the journal that removed the value lasts, a paste writes none of
/// the value's bytes, and the flush states a few spans for the cut and the
/// paste together, however large the value; once a flush has folded that
/// journal, a paste encodes the value again, as an insertion of a plain
/// value would.
///
/// - It dereferences to its value, but gives no mutable access, since its
///   bytes must stay the value's encoding until they are pasted.
/// - It is neither `Copy` nor `Clone`, and typed by its value's type, so it
///   is pasted at most once, and only where its type goes.
/// - It borrows the store, not the `Mut` it came from, so the application
///   can drop the source's `Mut` and make the destination's while it holds
///   the cut, and it may be pasted in a later transaction than the one
///   that cut it.
/// - Dropping it unpasted drops the value: the container removed it when it
///   cut it.
///
/// ```
/// use kladde_persist::{Cut, Durable, Region};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// store.splice(0, 0, b"abcdef")?;
/// // What a container does: splice the value out, and hand the bytes' cut
/// // to a `Cut` with the value.
/// let id = Region::root(&store).splice(1, 2, b"")?;
/// let cut = Cut::new((b'b', b'c'), id, &store); // a pair of `u8`s is two bytes
/// assert!(cut.is_alive());
/// let value = cut.paste_with(|id, bytes| store.paste(4, 0, id.unwrap(), bytes))?;
/// assert_eq!(value, (b'b', b'c'));
/// store.flush()?;
/// assert_eq!(store.read_all()?, b"adefbc");
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub struct Cut<T, B: Backend> {
    value: T,
    id: Option<CutId>,
    backend: B,
}

impl<T: Durable, B: Backend> Cut<T, B> {
    /// A cut of `value`, whose bytes the store's cut `id` holds, or `None`
    /// for a value that encodes to no bytes. See [`Cut`] for an example.
    pub fn new(value: T, id: Option<CutId>, backend: B) -> Self {
        Cut { value, id, backend }
    }

    /// Whether pasting the cut now would write none of its bytes: whether
    /// the journal that removed the value has not been folded yet.
    ///
    /// See [`Cut`] for an example.
    pub fn is_alive(&self) -> bool {
        self.id.as_ref().is_some_and(|id| self.backend.is_alive(id))
    }

    /// Records the paste of the cut with `paste`, which a container makes
    /// record a paste of the cut's id with the value's encoding, which it is
    /// handed, and whatever else its insertion needs, in one transaction;
    /// then returns the value for the container to keep. See [`Cut`] for an
    /// example.
    pub fn paste_with(
        self,
        paste: impl FnOnce(Option<CutId>, &[u8]) -> Result<(), Error>,
    ) -> Result<T, Error> {
        let bytes = self.value.to_bytes();
        paste(self.id, &bytes)?;
        Ok(self.value)
    }

    /// The bytes the value's encoding takes.
    pub fn encoded_len(&self) -> u64 {
        self.id.as_ref().map_or(0, CutId::len)
    }

    /// The value, given up without pasting it.
    pub fn into_inner(self) -> T {
        self.value
    }
}

impl<T, B: Backend> std::ops::Deref for Cut<T, B> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}
