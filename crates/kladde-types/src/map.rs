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
    replace, Error, Guard, Location, Persistable, Pointer, PointerRepr, ReadBackend, UniquePointer,
    Word, WriteBackend,
};
use std::collections::{hash_map, BTreeSet, HashMap};
use std::hash::Hash;
use std::io::Read;
use std::ops::Deref;

use crate::slot::{read_slot, size, write_slot};

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
fn slot_size<K: Persistable<P>, V: Persistable<P>, P: PointerRepr>() -> usize {
    1 + <K as Persistable<P>>::INLINE_SIZE + <V as Persistable<P>>::INLINE_SIZE
}

impl<K, V, P> Persistable<P> for PersistableHashMap<K, V, P>
where
    K: Eq + Hash + Persistable<P>,
    V: Persistable<P>,
    P: PointerRepr,
{
    /// Just the slot array's pointer: its slot count is its size over the slot
    /// size.
    const INLINE_SIZE: usize = P::BYTE_LEN;

    type Guard<'s, B: WriteBackend<Pointer = P>>
        = PersistableHashMapGuard<'s, K, V, B>
    where
        Self: 's,
        B: 's;

    #[inline]
    fn guard<'s, B: WriteBackend<Pointer = P>>(
        &'s mut self,
        backend: &'s B,
        location: Location<P, B::Size>,
    ) -> Self::Guard<'s, B> {
        PersistableHashMapGuard {
            inner: self,
            backend,
            location,
        }
    }

    /// Publishes this map's slot array at `location`. Guards keep the slots
    /// current, so there is nothing else to write; and there is no way to
    /// build a map with entries but no slot array.
    fn store<B: WriteBackend<Pointer = P>>(
        &mut self,
        backend: &B,
        location: Location<P, B::Size>,
    ) -> Result<(), Error> {
        debug_assert!(self.pointer.is_some() || self.entries.is_empty());
        write_slot(backend, location, self.pointer.as_ref().map(|p| p.raw()))
    }

    fn load<B: ReadBackend<Pointer = P>>(
        backend: &mut B,
        location: Location<P, B::Size>,
    ) -> Result<Self, Error> {
        let Some(target) = read_slot::<P, B>(backend, location)? else {
            return Ok(PersistableHashMap::new());
        };
        let slot = slot_size::<K, V, P>();
        let key_size = <K as Persistable<P>>::INLINE_SIZE;
        let capacity = backend.read_size(target)?.to_usize() / slot;
        let mut map = PersistableHashMap::new();
        for i in 0..capacity {
            let base = i * slot;
            let mut tag = [0u8; 1];
            backend.read_at(target, size(base)?)?.read_exact(&mut tag)?;
            if tag[0] == 0 {
                map.vacant.insert(i);
                continue;
            }
            let key = K::load(backend, Location::new(target, size(base + 1)?))?;
            let value = V::load(backend, Location::new(target, size(base + 1 + key_size)?))?;
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

    fn describe_local(builder: &mut kladde_persist::SchemaBuilder) -> kladde_persist::TypeDescriptor
    where
        Self: 'static,
    {
        kladde_persist::TypeDescriptor::Opaque {
            library_name: "kladde-types".into(),
            type_name: "PersistableHashMap".into(),
            version: crate::library_version(),
            inline_size: P::BYTE_LEN as u64,
            parameters: vec![
                <K as Persistable<P>>::describe(builder),
                <V as Persistable<P>>::describe(builder),
            ],
        }
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
pub struct PersistableHashMapGuard<'s, K, V, B: WriteBackend> {
    inner: &'s mut PersistableHashMap<K, V, B::Pointer>,
    backend: &'s B,
    location: Location<B::Pointer, B::Size>,
}

impl<'s, K, V, B> PersistableHashMapGuard<'s, K, V, B>
where
    K: Eq + Hash + Persistable<B::Pointer>,
    V: Persistable<B::Pointer>,
    B: WriteBackend,
{
    /// Where the value of slot `slot` lives.
    fn value_location(&self, slot: usize) -> Result<Location<B::Pointer, B::Size>, Error> {
        let at = slot * slot_size::<K, V, B::Pointer>()
            + 1
            + <K as Persistable<B::Pointer>>::INLINE_SIZE;
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
    pub fn get_mut(&mut self, key: &K) -> Option<<V as Persistable<B::Pointer>>::Guard<'_, B>> {
        let slot = self.inner.entries.get(key)?.0;
        let location = self.value_location(slot).ok()?;
        let (_, value) = self.inner.entries.get_mut(key)?;
        Some(value.guard(self.backend, location))
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
                value.store(backend, at)?;
                key.free(backend)
            })?;
            let (_, old) = self.inner.entries.insert(key, (slot, value)).unwrap();
            return Ok(Some(old));
        }
        let slot_bytes = slot_size::<K, V, B::Pointer>();
        let key_size = <K as Persistable<B::Pointer>>::INLINE_SIZE;
        let reused = self.inner.vacant.first().copied();
        let slot = reused.unwrap_or(self.inner.capacity);
        let location = self.location;
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
            key.store(backend, Location::new(pointer.raw(), size(base + 1)?))?;
            value.store(
                backend,
                Location::new(pointer.raw(), size(base + 1 + key_size)?),
            )?;
            backend.write(pointer.raw(), size(base)?, &[1])?;
            if fresh.is_some() {
                write_slot(backend, location, Some(pointer.raw()))?;
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
        let base = slot * slot_size::<K, V, B::Pointer>();
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
        replace(self.inner, value, self.backend, self.location)
    }
}

impl<'s, K, V, B: WriteBackend> Guard for PersistableHashMapGuard<'s, K, V, B> {
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

impl<'s, K, V, B: WriteBackend> Deref for PersistableHashMapGuard<'s, K, V, B> {
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
        let mut f = Fixture::new(<Map as Persistable>::INLINE_SIZE);
        let mut map = Map::new();
        {
            let mut guard = map.guard(&f.store, f.location);
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
        let mut f = Fixture::new(<Map as Persistable>::INLINE_SIZE);
        let mut map = Map::new();
        {
            let mut guard = map.guard(&f.store, f.location);
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
        let f = Fixture::new(<Map as Persistable>::INLINE_SIZE);
        let mut map = Map::new();
        map.guard(&f.store, f.location)
            .insert(PersistableString::from("key"), 1)
            .unwrap();
        f.store.flush().unwrap();
        let before = f.store.allocations().len();
        map.guard(&f.store, f.location)
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
