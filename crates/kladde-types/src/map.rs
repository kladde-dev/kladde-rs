//! [`PersistedHashMap`] -- the backed variant of `HashMap<K, V>`.
//!
//! Snapshot layout, per the follow-up discussion on `FLUSHING_QUESTIONS.md`
//! question 11: the on-disk representation doesn't need to support O(1)
//! lookup by key at all -- that's already handled by the in-memory
//! reconstruction. So the content allocation is just a dense array of
//! fixed-size `(K, V)` slots (`K::INLINE_SIZE + V::INLINE_SIZE` bytes
//! each, `K` then `V`), exactly like `PersistedVec<(K, V)>`'s layout.
//! Removal is a `swap_remove` (move the last slot into the removed one)
//! rather than a stable-order shift, to keep it O(1) instead of O(n).

use kladde_traits::{
    read_header, write_header, Backend, Guard, Location, Persistable, RawPointer, UniquePointer,
};
use std::collections::HashMap;
use std::hash::Hash;
use std::ops::{Deref, DerefMut};

#[derive(Debug, PartialEq)]
pub struct PersistedHashMap<K: Eq + Hash, V> {
    /// key -> slot index into `slots` (and into the on-disk entry array).
    index: HashMap<K, usize>,
    /// slot -> `(key, value)`, kept in sync with the on-disk entry array
    /// -- also gives the reverse "which key lives at slot N" lookup
    /// `swap_remove` needs to fix up `index` after moving the last slot.
    slots: Vec<(K, V)>,
    pointer: Option<UniquePointer<PersistedHashMap<K, V>>>,
}

impl<K: Eq + Hash, V> PersistedHashMap<K, V> {
    pub fn new() -> Self {
        PersistedHashMap {
            index: HashMap::new(),
            slots: Vec::new(),
            pointer: None,
        }
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        let &slot = self.index.get(key)?;
        Some(&self.slots[slot].1)
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.index.contains_key(key)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.slots.iter().map(|(k, v)| (k, v))
    }
}

impl<K: Eq + Hash, V> Default for PersistedHashMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a, K: Eq + Hash, V> IntoIterator for &'a PersistedHashMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = std::iter::Map<std::slice::Iter<'a, (K, V)>, fn(&'a (K, V)) -> (&'a K, &'a V)>;

    fn into_iter(self) -> Self::IntoIter {
        self.slots.iter().map(|(k, v)| (k, v))
    }
}

impl<K, V> Persistable for PersistedHashMap<K, V>
where
    K: Eq + Hash + Clone + Persistable,
    V: Persistable,
{
    /// A fixed 8-byte `{ target, len }` header -- see `PersistedVec`'s
    /// identical layout note.
    const INLINE_SIZE: usize = 8;

    type Guard<'s, B: Backend>
        = PersistedHashMapGuard<'s, K, V, B>
    where
        Self: 's,
        B: 's;

    fn guard<'s, B: Backend>(
        &'s mut self,
        backend: &'s B,
        location: Location,
    ) -> Self::Guard<'s, B> {
        PersistedHashMapGuard {
            inner: self,
            backend,
            location,
        }
    }

    fn store<B: Backend>(&self, backend: &B, location: Location) {
        let entry_size = entry_size::<K, V>();
        let pointer = backend.alloc::<PersistedHashMap<K, V>>(self.slots.len() * entry_size);
        for (i, (key, value)) in self.slots.iter().enumerate() {
            let base = i as u32 * entry_size as u32;
            key.store(
                backend,
                Location {
                    anchor: pointer.raw(),
                    offset: base,
                },
            );
            value.store(
                backend,
                Location {
                    anchor: pointer.raw(),
                    offset: base + K::INLINE_SIZE as u32,
                },
            );
        }
        write_header(backend, location, pointer.index(), self.slots.len() as u32);
    }

    fn load<B: Backend>(backend: &B, location: Location) -> Self {
        let (target, len) = read_header(backend, location);
        let pointer = target.map(UniquePointer::from_index);
        let mut index = HashMap::new();
        let mut slots = Vec::with_capacity(len as usize);
        if let Some(target) = target {
            let anchor = RawPointer::from_index(target);
            let entry_size = entry_size::<K, V>() as u32;
            for i in 0..len {
                let base = i * entry_size;
                let key = K::load(
                    backend,
                    Location {
                        anchor,
                        offset: base,
                    },
                );
                let value = V::load(
                    backend,
                    Location {
                        anchor,
                        offset: base + K::INLINE_SIZE as u32,
                    },
                );
                index.insert(key.clone(), slots.len());
                slots.push((key, value));
            }
        }
        PersistedHashMap {
            index,
            slots,
            pointer,
        }
    }
}

fn entry_size<K: Persistable, V: Persistable>() -> usize {
    K::INLINE_SIZE + V::INLINE_SIZE
}

pub struct PersistedHashMapGuard<'s, K: Eq + Hash, V, B = kladde::DefaultBackend> {
    inner: &'s mut PersistedHashMap<K, V>,
    backend: &'s B,
    location: Location,
}

// `get_mut` doesn't need `K`/`V: Clone` beyond what's already required by
// `Persistable` -- kept in its own impl block so it stays available
// regardless of what `insert`/`remove` additionally need.
impl<'s, K: Eq + Hash + Persistable, V: Persistable, B: Backend>
    PersistedHashMapGuard<'s, K, V, B>
{
    pub fn get_mut(&mut self, key: &K) -> Option<V::Guard<'_, B>> {
        let &slot = self.inner.index.get(key)?;
        let entry_size = entry_size::<K, V>() as u32;
        let pointer = self.inner.pointer.as_ref()?;
        let location = Location {
            anchor: pointer.raw(),
            offset: slot as u32 * entry_size + K::INLINE_SIZE as u32,
        };
        Some(self.inner.slots[slot].1.guard(self.backend, location))
    }
}

impl<'s, K, V, B: Backend> PersistedHashMapGuard<'s, K, V, B>
where
    K: Eq + Hash + Clone + Persistable,
    V: Persistable,
{
    /// Inserts `value` under `key`. If `key` already has a value, its
    /// slot is overwritten in place (the key itself doesn't need
    /// rewriting, since it can't have changed) and the old value is
    /// returned; otherwise a new slot is appended, exactly like
    /// `PersistedVec::push` -- growing (or creating) the content
    /// allocation to fit, writing the new entry, then publishing the
    /// updated header last.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        let entry_size = entry_size::<K, V>() as u32;

        if let Some(&slot) = self.inner.index.get(&key) {
            let pointer = self.inner.pointer.as_ref().unwrap();
            value.store(
                self.backend,
                Location {
                    anchor: pointer.raw(),
                    offset: slot as u32 * entry_size + K::INLINE_SIZE as u32,
                },
            );
            return Some(std::mem::replace(&mut self.inner.slots[slot].1, value));
        }

        let old_len = self.inner.slots.len();
        let new_len = old_len + 1;
        let new_byte_size = new_len * entry_size as usize;
        match &self.inner.pointer {
            Some(pointer) => self.backend.resize(pointer, new_byte_size),
            None => {
                self.inner.pointer =
                    Some(self.backend.alloc::<PersistedHashMap<K, V>>(new_byte_size))
            }
        }
        let pointer = self.inner.pointer.as_ref().unwrap();
        let base = old_len as u32 * entry_size;
        key.store(
            self.backend,
            Location {
                anchor: pointer.raw(),
                offset: base,
            },
        );
        value.store(
            self.backend,
            Location {
                anchor: pointer.raw(),
                offset: base + K::INLINE_SIZE as u32,
            },
        );
        write_header(self.backend, self.location, pointer.index(), new_len as u32);

        self.inner.index.insert(key.clone(), old_len);
        self.inner.slots.push((key, value));
        None
    }

    /// Removes and returns the value under `key`, if present, via
    /// `swap_remove`: the last slot's bytes are copied into the removed
    /// slot's position (one `copy`, covering the whole `(K, V)` entry),
    /// its key's `index` entry is repointed at the (now-reused) slot,
    /// then the content allocation shrinks by one entry and the header
    /// is published -- same shrink-after-compact ordering as
    /// `PersistedVec::remove`.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let &slot = self.inner.index.get(key)?;
        let entry_size = entry_size::<K, V>() as u32;
        let pointer = self.inner.pointer.as_ref().unwrap();
        let old_len = self.inner.slots.len();
        let last = old_len - 1;

        if slot != last {
            self.backend.copy(
                pointer.raw(),
                last as u32 * entry_size,
                entry_size,
                pointer.raw(),
                slot as u32 * entry_size,
            );
        }
        self.backend.resize(pointer, last * entry_size as usize);
        write_header(self.backend, self.location, pointer.index(), last as u32);

        self.inner.index.remove(key);
        let (_, removed_value) = self.inner.slots.swap_remove(slot);
        if slot < self.inner.slots.len() {
            let swapped_key = self.inner.slots[slot].0.clone();
            self.inner.index.insert(swapped_key, slot);
        }
        Some(removed_value)
    }
}

impl<'s, K, V, B: Backend> Guard for PersistedHashMapGuard<'s, K, V, B>
where
    K: Eq + Hash + Clone + Persistable,
    V: Persistable,
{
    type Persistable = PersistedHashMap<K, V>;
    type Backend = B;

    fn as_persistable(&self) -> &PersistedHashMap<K, V> {
        self.inner
    }
    fn as_persistable_mut(&mut self) -> &mut PersistedHashMap<K, V> {
        self.inner
    }
    fn backend(&self) -> &B {
        self.backend
    }
}

impl<'s, K: Eq + Hash, V, B> Deref for PersistedHashMapGuard<'s, K, V, B> {
    type Target = PersistedHashMap<K, V>;
    fn deref(&self) -> &PersistedHashMap<K, V> {
        self.inner
    }
}

impl<'s, K: Eq + Hash, V, B> DerefMut for PersistedHashMapGuard<'s, K, V, B> {
    fn deref_mut(&mut self) -> &mut PersistedHashMap<K, V> {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockBackend;
    use kladde_traits::Allocator;

    fn root_location(backend: &MockBackend) -> Location {
        let pointer = backend.alloc::<()>(PersistedHashMap::<String, i32>::INLINE_SIZE);
        Location {
            anchor: pointer.raw(),
            offset: 0,
        }
    }

    #[test]
    fn insert_adds_in_memory() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut map = PersistedHashMap::<String, i32>::new();

        let mut guard = map.guard(&backend, location);
        guard.insert("a".to_string(), 1);
        guard.insert("b".to_string(), 2);

        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&"a".to_string()), Some(&1));
        assert_eq!(map.get(&"b".to_string()), Some(&2));
    }

    #[test]
    fn insert_replacing_an_existing_key_returns_the_old_value() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut map = PersistedHashMap::<String, i32>::new();
        {
            let mut guard = map.guard(&backend, location);
            guard.insert("a".to_string(), 1);
        }

        let old = map.guard(&backend, location).insert("a".to_string(), 2);

        assert_eq!(old, Some(1));
        assert_eq!(map.get(&"a".to_string()), Some(&2));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn get_mut_returns_a_nested_guard_for_persistable_values() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut map = PersistedHashMap::<String, i32>::new();
        {
            let mut guard = map.guard(&backend, location);
            guard.insert("a".to_string(), 1);
        }

        let mut guard = map.guard(&backend, location);
        guard.get_mut(&"a".to_string()).unwrap().set(99);

        assert_eq!(map.get(&"a".to_string()), Some(&99));
    }

    #[test]
    fn remove_deletes_and_swaps_the_last_slot_into_place() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut map = PersistedHashMap::<String, i32>::new();
        {
            let mut guard = map.guard(&backend, location);
            guard.insert("a".to_string(), 1);
            guard.insert("b".to_string(), 2);
            guard.insert("c".to_string(), 3);
        }

        let removed = map.guard(&backend, location).remove(&"a".to_string());

        assert_eq!(removed, Some(1));
        assert!(map.get(&"a".to_string()).is_none());
        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&"b".to_string()), Some(&2));
        assert_eq!(map.get(&"c".to_string()), Some(&3));
    }

    #[test]
    fn flushing_and_reloading_round_trips_the_content() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut map = PersistedHashMap::<String, i32>::new();
        {
            let mut guard = map.guard(&backend, location);
            guard.insert("a".to_string(), 1);
            guard.insert("b".to_string(), 2);
            guard.insert("c".to_string(), 3);
        }
        {
            map.guard(&backend, location).remove(&"b".to_string());
        }

        backend.flush();

        let reloaded = PersistedHashMap::<String, i32>::load(&backend, location);
        assert_eq!(reloaded.get(&"a".to_string()), Some(&1));
        assert_eq!(reloaded.get(&"b".to_string()), None);
        assert_eq!(reloaded.get(&"c".to_string()), Some(&3));
        assert_eq!(reloaded.len(), 2);
    }
}
