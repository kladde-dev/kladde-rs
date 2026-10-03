//! [`PersistableHashMap`] -- the backed variant of `HashMap<K, V>`.
//!
//! The on-file form does not need to support lookup at all -- that is what the
//! in-memory map is for -- so it is a flat array of fixed-size slots, each a
//! one-byte liveness tag followed by a key and a value. Removal clears the tag
//! in place and moves nothing, and an insertion reuses the lowest cleared slot
//! before it grows the array. So no slot ever needs a reverse "which key is
//! here" lookup, each key is stored exactly once in memory, and `K: Clone` is
//! not required. The array's length is a capacity: the high-water mark of live
//! entries, not a live count.

use kladde_persist::{
    read_allocation, replace, slot_size, Encoding, Error, Field, Guard, Input, Location,
    Persistable, Place, Pointer, PointerRepr, ReadBackend, Slottable, Slotted, TypeDescriptor,
    UniquePointer, WriteBackend,
};
use std::any::TypeId;
use std::collections::{hash_map, BTreeSet, HashMap};
use std::hash::Hash;
use std::marker::PhantomData;
use std::ops::Deref;

use crate::slot::{decode_pointer, encode_pointer, pointer_size, publish_pointer, size};

/// A hash map whose contents are persisted.
///
/// Reads -- [`get`](Self::get), [`contains_key`](Self::contains_key),
/// [`iter`](Self::iter), [`len`](Self::len) -- touch only the in-memory copy.
/// Mutation goes through a [`PersistableHashMapGuard`]. `K` and `V` need
/// [`Persistable`], and `K: Eq + Hash`; `K` does not need `Clone`. Keys cannot
/// be mutated in place: no guard is ever handed out for one.
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::{PersistableHashMap, PersistableString};
///
/// let mut ages = Kladde::new(PersistableHashMap::<PersistableString, u8>::new());
/// ages.guard().insert(PersistableString::from("ada"), 36)?;
/// assert_eq!(ages.get().get(&PersistableString::from("ada")), Some(&36));
/// # Ok::<(), kladde::Error>(())
/// ```
#[derive(Debug)]
pub struct PersistableHashMap<K, V, P = Pointer> {
    /// Key -> (its slot, its value), live entries only.
    entries: HashMap<K, (usize, V)>,
    /// The slot count, tombstones included: the content allocation's size
    /// over the slot size.
    capacity: usize,
    /// Tombstoned slots, which insertions reuse lowest first.
    vacant: BTreeSet<usize>,
    /// The slot array, `None` until the first insertion.
    pointer: Option<UniquePointer<P>>,
}

impl<K, V, P> PersistableHashMap<K, V, P> {
    /// An empty map, holding no allocation.
    ///
    /// ```
    /// use kladde_types::PersistableHashMap;
    ///
    /// let m = PersistableHashMap::<u32, u32>::new();
    /// assert!(m.is_empty());
    /// ```
    pub fn new() -> Self {
        PersistableHashMap {
            entries: HashMap::new(),
            capacity: 0,
            vacant: BTreeSet::new(),
            pointer: None,
        }
    }

    /// The number of entries.
    ///
    /// ```
    /// use kladde_types::PersistableHashMap;
    ///
    /// assert_eq!(PersistableHashMap::<u8, u8>::new().len(), 0);
    /// ```
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the map has no entries.
    ///
    /// ```
    /// use kladde_types::PersistableHashMap;
    ///
    /// assert!(PersistableHashMap::<u8, u8>::new().is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entries, in no particular order.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableHashMap;
    ///
    /// let mut m = Kladde::new(PersistableHashMap::<u8, u8>::new());
    /// m.guard().insert(1, 2)?;
    /// assert_eq!(m.get().iter().collect::<Vec<_>>(), [(&1, &2)]);
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.entries.iter().map(|(key, (_, value))| (key, value))
    }
}

impl<K: Eq + Hash, V, P> PersistableHashMap<K, V, P> {
    /// The value under `key`.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableHashMap;
    ///
    /// let mut m = Kladde::new(PersistableHashMap::<u8, u8>::new());
    /// m.guard().insert(1, 2)?;
    /// assert_eq!(m.get().get(&1), Some(&2));
    /// assert_eq!(m.get().get(&3), None);
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn get(&self, key: &K) -> Option<&V> {
        self.entries.get(key).map(|(_, value)| value)
    }

    /// Whether `key` has an entry.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableHashMap;
    ///
    /// let mut m = Kladde::new(PersistableHashMap::<u8, u8>::new());
    /// m.guard().insert(1, 2)?;
    /// assert!(m.get().contains_key(&1));
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn contains_key(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }
}

impl<K, V, P> Default for PersistableHashMap<K, V, P> {
    fn default() -> Self {
        Self::new()
    }
}

type EntriesIter<'a, K, V> = hash_map::Iter<'a, K, (usize, V)>;
type EntriesMapFn<'a, K, V> = fn((&'a K, &'a (usize, V))) -> (&'a K, &'a V);

impl<'a, K, V, P> IntoIterator for &'a PersistableHashMap<K, V, P> {
    type Item = (&'a K, &'a V);
    type IntoIter = std::iter::Map<EntriesIter<'a, K, V>, EntriesMapFn<'a, K, V>>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter().map(|(key, (_, value))| (key, value))
    }
}

/// One slot: a liveness tag, then the key, then the value.
fn entry_size<K: Persistable<P>, V: Persistable<P>, P: PointerRepr>() -> usize {
    1 + slot_size::<K, P>() + slot_size::<V, P>()
}

impl<K, V, P> Slottable<P> for PersistableHashMap<K, V, P>
where
    K: Eq + Hash + Slottable<P>,
    V: Slottable<P>,
    P: PointerRepr,
{
}

/// The identity under which a map's slot type is described.
struct SlotOf<K, V, P>(PhantomData<(K, V, P)>);

impl<K, V, P> Persistable<P> for PersistableHashMap<K, V, P>
where
    K: Eq + Hash + Slottable<P>,
    V: Slottable<P>,
    P: PointerRepr,
{
    /// Just the slot array's pointer: its slot count is its size over the slot
    /// size.
    const SLOTTED_SIZE: Option<usize> = Some(P::BYTE_LEN);
    /// None: packed, the pointer is a varint.
    const PACKED_SIZE: Option<usize> = None;

    type RootEncoding = Slotted;

    type Guard<'s, B: WriteBackend<Pointer = P>, E: Encoding>
        = PersistableHashMapGuard<'s, K, V, B, E>
    where
        Self: 's,
        B: 's;

    #[inline]
    fn guard<'s, B: WriteBackend<Pointer = P>, E: Encoding>(
        &'s mut self,
        backend: &'s B,
        place: Place<'s, B, E>,
    ) -> Self::Guard<'s, B, E> {
        PersistableHashMapGuard {
            inner: self,
            backend,
            place,
        }
    }

    fn encoded_size<E: Encoding>(&self) -> usize {
        pointer_size::<P, E>(self.pointer.as_ref().map(|p| p.raw()))
    }

    /// This map's slot array's pointer. Guards keep the slots current, so
    /// there is nothing else to write; and there is no way to build a map
    /// with entries but no slot array.
    fn encode<E: Encoding>(&self, out: &mut Vec<u8>) {
        debug_assert!(self.pointer.is_some() || self.entries.is_empty());
        encode_pointer::<P, E>(self.pointer.as_ref().map(|p| p.raw()), out);
    }

    fn decode<B: ReadBackend<Pointer = P>, E: Encoding>(
        backend: &mut B,
        input: &mut Input<'_>,
    ) -> Result<Self, Error> {
        let Some(target) = decode_pointer::<P, E>(input)? else {
            return Ok(PersistableHashMap::new());
        };
        let slot = entry_size::<K, V, P>();
        let bytes = read_allocation(backend, target)?;
        if bytes.len() % slot != 0 {
            return Err(Error::Corrupt(format!(
                "a map's slots of {} bytes are no whole number of {slot}-byte slots",
                bytes.len()
            )));
        }
        let capacity = bytes.len() / slot;
        let mut map = PersistableHashMap::new();
        let mut content = Input::new(&bytes);
        for i in 0..capacity {
            if content.byte()? == 0 {
                map.vacant.insert(i);
                content.take(slot - 1)?;
                continue;
            }
            let key = K::decode::<B, Slotted>(backend, &mut content)?;
            let value = V::decode::<B, Slotted>(backend, &mut content)?;
            map.entries.insert(key, (i, value));
        }
        map.capacity = capacity;
        map.pointer = Some(UniquePointer::from_pointer(target));
        Ok(map)
    }

    /// Frees every key and value, then the slot array.
    fn free<B: WriteBackend<Pointer = P>>(&mut self, backend: &B) -> Result<(), Error> {
        for (mut key, (_, mut value)) in self.entries.drain() {
            key.free(backend)?;
            value.free(backend)?;
        }
        if let Some(pointer) = self.pointer.take() {
            backend.free(pointer)?;
        }
        Ok(())
    }

    /// `Pointer(Sequence(Slot))`, where `Slot` is a struct of a liveness
    /// flag, a key and a value: an allocation of slots, of which those whose
    /// flag is clear hold no entry.
    fn describe_local(builder: &mut kladde_persist::SchemaBuilder) -> TypeDescriptor
    where
        Self: 'static,
    {
        let slot = builder.describe_with(TypeId::of::<SlotOf<K, V, P>>(), |builder| {
            TypeDescriptor::Struct {
                name: "Slot".into(),
                fields: vec![
                    Field {
                        name: "live".into(),
                        ty: <bool as Persistable<P>>::describe(builder),
                    },
                    Field {
                        name: "key".into(),
                        ty: <K as Persistable<P>>::describe(builder),
                    },
                    Field {
                        name: "value".into(),
                        ty: <V as Persistable<P>>::describe(builder),
                    },
                ],
            }
        });
        let sequence = builder.describe_with(TypeId::of::<Vec<SlotOf<K, V, P>>>(), |_| {
            TypeDescriptor::Sequence(slot)
        });
        TypeDescriptor::Pointer(sequence)
    }
}

/// The mutation-capable view onto a [`PersistableHashMap`].
///
/// Each method records its change as one transaction and then applies it.
/// [`remove`](Self::remove) hands the value back with everything it owns;
/// [`delete`](Self::delete) frees it. The key is freed either way, since no
/// one else can own it.
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::{PersistableHashMap, PersistableString};
///
/// let mut db = Kladde::new(PersistableHashMap::<PersistableString, u32>::new());
/// let mut m = db.guard();
/// m.insert(PersistableString::from("a"), 1)?;
/// m.insert(PersistableString::from("b"), 2)?;
/// assert_eq!(m.remove(&PersistableString::from("a"))?, Some(1));
/// m.get_mut(&PersistableString::from("b")).unwrap().set(3)?;
/// assert_eq!(db.get().get(&PersistableString::from("b")), Some(&3));
/// # Ok::<(), kladde::Error>(())
/// ```
pub struct PersistableHashMapGuard<'s, K, V, B: WriteBackend, E: Encoding = Slotted> {
    inner: &'s mut PersistableHashMap<K, V, B::Pointer>,
    backend: &'s B,
    place: Place<'s, B, E>,
}

impl<'s, K, V, B, E> PersistableHashMapGuard<'s, K, V, B, E>
where
    K: Eq + Hash + Slottable<B::Pointer>,
    V: Slottable<B::Pointer>,
    B: WriteBackend,
    E: Encoding,
{
    /// Where the value of slot `slot` lives.
    fn value_location(&self, slot: usize) -> Result<Location<B::Pointer, B::Size>, Error> {
        let at = slot * entry_size::<K, V, B::Pointer>() + 1 + slot_size::<K, B::Pointer>();
        let target = self
            .inner
            .pointer
            .as_ref()
            .expect("a map with entries has slots")
            .raw();
        Ok(Location::new(target, size(at)?))
    }

    /// The guard of the value under `key`.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableHashMap;
    ///
    /// let mut m = Kladde::new(PersistableHashMap::<u8, u8>::new());
    /// m.guard().insert(1, 2)?;
    /// m.guard().get_mut(&1).unwrap().set(5)?;
    /// assert_eq!(m.get().get(&1), Some(&5));
    /// # Ok::<(), kladde::Error>(())
    /// ```
    #[inline]
    pub fn get_mut(
        &mut self,
        key: &K,
    ) -> Option<<V as Persistable<B::Pointer>>::Guard<'_, B, Slotted>> {
        let slot = self.inner.entries.get(key)?.0;
        let location = self.value_location(slot).ok()?;
        let (_, value) = self.inner.entries.get_mut(key)?;
        Some(value.guard(self.backend, Slotted::at(location)))
    }

    /// Inserts `value` under `key`, in one transaction.
    ///
    /// If `key` has an entry already, its value is overwritten in place and
    /// the old value returned, allocations and all, as `HashMap::insert`
    /// returns it; the `key` passed in is freed, since the map keeps its own.
    /// Otherwise the entry takes the lowest vacant slot, or a new one past the
    /// end, with its liveness tag written last.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableHashMap;
    ///
    /// let mut m = Kladde::new(PersistableHashMap::<u8, u8>::new());
    /// assert_eq!(m.guard().insert(1, 2)?, None);
    /// assert_eq!(m.guard().insert(1, 3)?, Some(2));
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn insert(&mut self, mut key: K, mut value: V) -> Result<Option<V>, Error> {
        let backend = self.backend;
        if let Some(&(slot, _)) = self.inner.entries.get(&key) {
            let at = self.value_location(slot)?;
            backend.atomically(|| {
                value.store::<B, Slotted>(backend, at)?;
                key.free(backend)
            })?;
            let (_, old) = self.inner.entries.insert(key, (slot, value)).unwrap();
            return Ok(Some(old));
        }
        let slot_bytes = entry_size::<K, V, B::Pointer>();
        let reused = self.inner.vacant.first().copied();
        let slot = reused.unwrap_or(self.inner.capacity);
        let place = &self.place;
        let existing = &self.inner.pointer;
        let fresh = backend.atomically(|| {
            let fresh = match existing {
                Some(_) => None,
                None => Some(backend.alloc(size(0)?)?),
            };
            let pointer = fresh.as_ref().or(existing.as_ref()).unwrap();
            let base = slot * slot_bytes;
            if reused.is_none() {
                backend.resize(pointer, size(base + slot_bytes)?)?;
            }
            key.prepare(backend)?;
            value.prepare(backend)?;
            let mut entry = Vec::with_capacity(slot_bytes - 1);
            key.encode::<Slotted>(&mut entry);
            value.encode::<Slotted>(&mut entry);
            backend.write(pointer.raw(), size(base + 1)?, &entry)?;
            backend.write(pointer.raw(), size(base)?, &[1])?;
            if fresh.is_some() {
                publish_pointer(backend, place, None, Some(pointer.raw()))?;
            }
            Ok(fresh)
        })?;
        if fresh.is_some() {
            self.inner.pointer = fresh;
        }
        match reused {
            Some(s) => {
                self.inner.vacant.remove(&s);
            }
            None => self.inner.capacity += 1,
        }
        self.inner.entries.insert(key, (slot, value));
        Ok(None)
    }

    /// Clears the entry of `key`'s slot, and frees the key; the caller decides
    /// about the value, which is returned.
    fn vacate(&mut self, key: &K, free_value: bool) -> Result<Option<V>, Error> {
        let Some((mut owned_key, (slot, mut value))) = self.inner.entries.remove_entry(key) else {
            return Ok(None);
        };
        let backend = self.backend;
        let target = self
            .inner
            .pointer
            .as_ref()
            .expect("a map with entries has slots")
            .raw();
        let base = slot * entry_size::<K, V, B::Pointer>();
        let done = backend.atomically(|| {
            backend.write(target, size(base)?, &[0])?;
            owned_key.free(backend)?;
            if free_value {
                value.free(backend)?;
            }
            Ok(())
        });
        if let Err(e) = done {
            // Recording failed: the entry stays, as the file still has it.
            self.inner.entries.insert(owned_key, (slot, value));
            return Err(e);
        }
        self.inner.vacant.insert(slot);
        Ok(Some(value))
    }

    /// Removes the entry of `key` and returns its value, allocations and all,
    /// or `None` if there is none. The key is freed. See
    /// [`PersistableHashMapGuard`] for an example.
    pub fn remove(&mut self, key: &K) -> Result<Option<V>, Error> {
        self.vacate(key, false)
    }

    /// Removes the entry of `key` and frees its key and value, in one
    /// transaction. Whether there was one.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableHashMap;
    ///
    /// let mut m = Kladde::new(PersistableHashMap::<u8, u8>::new());
    /// m.guard().insert(1, 2)?;
    /// assert!(m.guard().delete(&1)?);
    /// assert!(!m.guard().delete(&1)?);
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn delete(&mut self, key: &K) -> Result<bool, Error> {
        Ok(self.vacate(key, true)?.is_some())
    }

    /// Removes every entry and frees their keys and values, in one
    /// transaction. The slot array stays, empty.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableHashMap;
    ///
    /// let mut m = Kladde::new(PersistableHashMap::<u8, u8>::new());
    /// m.guard().insert(1, 2)?;
    /// m.guard().clear()?;
    /// assert!(m.get().is_empty());
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn clear(&mut self) -> Result<(), Error> {
        let backend = self.backend;
        let PersistableHashMap {
            entries, pointer, ..
        } = &mut *self.inner;
        let Some(pointer) = pointer.as_ref() else {
            return Ok(());
        };
        backend.atomically(|| {
            backend.resize(pointer, size(0)?)?;
            for (_, value) in entries.values_mut() {
                value.free(backend)?;
            }
            Ok(())
        })?;
        // Keys can be freed only once they are owned again.
        for (mut key, _) in self.inner.entries.drain() {
            // The resize already made them unreachable; a failure here leaks.
            let _ = key.free(self.backend);
        }
        self.inner.capacity = 0;
        self.inner.vacant.clear();
        Ok(())
    }

    /// Replaces the whole map: stores `value`, then frees the old entries and
    /// slots, in one transaction.
    ///
    /// ```
    /// use kladde::Kladde;
    /// use kladde_types::PersistableHashMap;
    ///
    /// let mut m = Kladde::new(PersistableHashMap::<u8, u8>::new());
    /// m.guard().insert(1, 2)?;
    /// m.guard().set(PersistableHashMap::new())?;
    /// assert!(m.get().is_empty());
    /// # Ok::<(), kladde::Error>(())
    /// ```
    pub fn set(&mut self, value: PersistableHashMap<K, V, B::Pointer>) -> Result<(), Error> {
        replace(self.inner, value, self.backend, &self.place)
    }
}

impl<'s, K, V, B: WriteBackend, E: Encoding> Guard for PersistableHashMapGuard<'s, K, V, B, E> {
    type Persistable = PersistableHashMap<K, V, B::Pointer>;
    type Backend = B;

    fn as_persistable(&self) -> &Self::Persistable {
        self.inner
    }
    fn as_persistable_mut(&mut self) -> &mut Self::Persistable {
        self.inner
    }
    fn backend(&self) -> &B {
        self.backend
    }
}

impl<'s, K, V, B: WriteBackend, E: Encoding> Deref for PersistableHashMapGuard<'s, K, V, B, E> {
    type Target = PersistableHashMap<K, V, B::Pointer>;
    fn deref(&self) -> &Self::Target {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Fixture;
    use crate::PersistableString;

    type Map = PersistableHashMap<PersistableString, i32>;

    #[test]
    fn insert_remove_and_reload() {
        let mut f = Fixture::for_type::<Map>();
        let mut map = Map::new();
        {
            let mut guard = map.guard(&f.store, f.place());
            guard.insert(PersistableString::from("a"), 1).unwrap();
            guard.insert(PersistableString::from("b"), 2).unwrap();
            assert_eq!(
                guard.remove(&PersistableString::from("a")).unwrap(),
                Some(1)
            );
        }
        let reloaded: Map = f.reload();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded.get(&PersistableString::from("b")), Some(&2));
    }

    #[test]
    fn insertion_reuses_the_lowest_vacant_slot() {
        let mut f = Fixture::for_type::<Map>();
        let mut map = Map::new();
        {
            let mut guard = map.guard(&f.store, f.place());
            for (i, k) in ["a", "b", "c"].into_iter().enumerate() {
                guard.insert(PersistableString::from(k), i as i32).unwrap();
            }
            guard.delete(&PersistableString::from("a")).unwrap();
            guard.delete(&PersistableString::from("b")).unwrap();
            guard.insert(PersistableString::from("d"), 3).unwrap();
        }
        assert_eq!(map.capacity, 3);
        let reloaded: Map = f.reload();
        assert_eq!(reloaded.capacity, 3);
        assert_eq!(reloaded.vacant.iter().copied().collect::<Vec<_>>(), [1]);
        assert_eq!(reloaded.get(&PersistableString::from("d")), Some(&3));
    }

    #[test]
    fn removal_frees_the_key() {
        let f = Fixture::for_type::<Map>();
        let mut map = Map::new();
        map.guard(&f.store, f.place())
            .insert(PersistableString::from("key"), 1)
            .unwrap();
        f.store.flush().unwrap();
        let before = f.store.allocations().len();
        map.guard(&f.store, f.place())
            .delete(&PersistableString::from("key"))
            .unwrap();
        f.store.flush().unwrap();
        assert_eq!(
            f.store.allocations().len(),
            before - 1,
            "the key's content is freed"
        );
    }
}
