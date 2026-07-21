//! [`PersistedHashMap`] -- the backed variant of `HashMap<K, V>`.
//!
//! Snapshot layout: the on-disk representation doesn't need to support
//! O(1) lookup by key at all -- that's already handled by the in-memory
//! reconstruction. So the content allocation is a dense-*ish* array of
//! fixed-size slots (a 1-byte liveness tag, then `K`, then `V` --
//! `1 + K::INLINE_SIZE + V::INLINE_SIZE` bytes each), grown by `insert`
//! exactly like `PersistedVec`, but *not* compacted by `remove`: removing
//! an entry just clears its slot's liveness tag in place, leaving a
//! tombstone, rather than swapping the last slot into its place. This
//! means the on-disk array's length is really a *capacity* (the highest
//! slot ever used, dead or alive), not a live count -- `load` skips
//! tombstoned slots when reconstructing.
//!
//! This trades a `swap_remove`-based design's guarantee (the array is
//! always fully packed, no wasted space) for a much simpler one: nothing
//! ever moves on `remove`, so there's no need for a reverse "which key is
//! at slot N" lookup, which in turn means each key only ever needs to be
//! stored once in memory -- `K: Clone` isn't needed at all (unlike the
//! swap_remove-based version this replaced, which needed either `Clone`
//! or `Rc`-shared keys to make that reverse lookup possible). The real
//! cost: dead slots are only ever reclaimed by a real compaction pass
//! (not yet built -- see `spec.md`'s Crash Consistency section), so
//! sustained insert/remove churn grows the on-disk array without bound
//! for now. See `later.md` for a cheaper middle ground (an in-memory
//! free-list reusing tombstoned slots on insert) if that matters before
//! real compaction exists.

use kladde_traits::{
    read_header, write_header, Backend, Guard, Location, Persistable, RawPointer, UniquePointer,
};
use std::collections::hash_map;
use std::collections::HashMap;
use std::hash::Hash;
use std::ops::{Deref, DerefMut};

#[derive(Debug, PartialEq)]
pub struct PersistedHashMap<K: Eq + Hash, V> {
    /// key -> (slot index into the on-disk array, value). Only ever
    /// holds *live* entries -- a removed key is gone from here
    /// entirely, its former slot surviving on disk only as a tombstone.
    entries: HashMap<K, (usize, V)>,
    /// One past the highest slot index ever handed out by `insert` --
    /// i.e. the on-disk array's current length, tombstones included.
    /// Always `>= entries.len()`; strictly greater once anything has
    /// ever been removed.
    capacity: usize,
    pointer: Option<UniquePointer<PersistedHashMap<K, V>>>,
}

impl<K: Eq + Hash, V> PersistedHashMap<K, V> {
    pub fn new() -> Self {
        PersistedHashMap {
            entries: HashMap::new(),
            capacity: 0,
            pointer: None,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        self.entries.get(key).map(|(_, value)| value)
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.entries.iter().map(|(key, (_, value))| (key, value))
    }
}

impl<K: Eq + Hash, V> Default for PersistedHashMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

type EntriesIter<'a, K, V> = hash_map::Iter<'a, K, (usize, V)>;
type EntriesMapFn<'a, K, V> = fn((&'a K, &'a (usize, V))) -> (&'a K, &'a V);

impl<'a, K: Eq + Hash, V> IntoIterator for &'a PersistedHashMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = std::iter::Map<EntriesIter<'a, K, V>, EntriesMapFn<'a, K, V>>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter().map(|(key, (_, value))| (key, value))
    }
}

impl<K, V> Persistable for PersistedHashMap<K, V>
where
    K: Eq + Hash + Persistable,
    V: Persistable,
{
    /// A fixed 8-byte `{ target, capacity }` header -- see `PersistedVec`'s
    /// identical layout note (the second field means slot *capacity*
    /// here, not live count -- see this module's doc comment).
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

    /// Writes every current (live) entry fresh into a newly-sized,
    /// fully-packed content allocation -- no tombstones survive a
    /// `store`, since it only ever sees `self.entries`, which never
    /// tracks dead slots at all. Used when a whole `PersistedHashMap` is
    /// being written as a brand-new value somewhere (e.g. a struct field
    /// being assembled), not via incremental `insert`/`remove`.
    fn store<B: Backend>(&self, backend: &B, location: Location) {
        let entry_size = entry_size::<K, V>();
        let pointer = backend.alloc::<PersistedHashMap<K, V>>(self.entries.len() * entry_size);
        for (slot, (key, (_, value))) in self.entries.iter().enumerate() {
            write_entry(
                backend,
                pointer.raw(),
                slot as u32 * entry_size as u32,
                key,
                value,
            );
        }
        write_header(
            backend,
            location,
            pointer.index(),
            self.entries.len() as u32,
        );
    }

    fn load<B: Backend>(backend: &B, location: Location) -> Self {
        let (target, capacity) = read_header(backend, location);
        let pointer = target.map(UniquePointer::from_index);
        let mut entries = HashMap::new();
        if let Some(target) = target {
            let anchor = RawPointer::from_index(target);
            let entry_size = entry_size::<K, V>() as u32;
            for slot in 0..capacity {
                let base = slot * entry_size;
                let live = backend.read(anchor, base, 1)[0] != 0;
                if live {
                    let key = K::load(
                        backend,
                        Location {
                            anchor,
                            offset: base + 1,
                        },
                    );
                    let value = V::load(
                        backend,
                        Location {
                            anchor,
                            offset: base + 1 + K::INLINE_SIZE as u32,
                        },
                    );
                    entries.insert(key, (slot as usize, value));
                }
            }
        }
        PersistedHashMap {
            entries,
            capacity: capacity as usize,
            pointer,
        }
    }
}

fn entry_size<K: Persistable, V: Persistable>() -> usize {
    1 + K::INLINE_SIZE + V::INLINE_SIZE
}

/// Writes one live slot (tag + key + value) at `offset` within `target`.
fn write_entry<B: Backend, K: Persistable, V: Persistable>(
    backend: &B,
    target: RawPointer,
    offset: u32,
    key: &K,
    value: &V,
) {
    backend.write(target, offset, &[1u8]);
    key.store(
        backend,
        Location {
            anchor: target,
            offset: offset + 1,
        },
    );
    value.store(
        backend,
        Location {
            anchor: target,
            offset: offset + 1 + K::INLINE_SIZE as u32,
        },
    );
}

pub struct PersistedHashMapGuard<'s, K: Eq + Hash, V, B = kladde::DefaultBackend> {
    inner: &'s mut PersistedHashMap<K, V>,
    backend: &'s B,
    location: Location,
}

// `get_mut` doesn't need any extra bounds beyond `Persistable` -- kept
// in its own impl block so it stays available regardless of what
// `insert`/`remove` additionally need.
impl<'s, K: Eq + Hash + Persistable, V: Persistable, B: Backend>
    PersistedHashMapGuard<'s, K, V, B>
{
    pub fn get_mut(&mut self, key: &K) -> Option<V::Guard<'_, B>> {
        let slot = self.inner.entries.get(key)?.0;
        let entry_size = entry_size::<K, V>() as u32;
        let pointer = self.inner.pointer.as_ref()?.raw();
        let location = Location {
            anchor: pointer,
            offset: slot as u32 * entry_size + 1 + K::INLINE_SIZE as u32,
        };
        let (_, value) = self.inner.entries.get_mut(key)?;
        Some(value.guard(self.backend, location))
    }
}

impl<'s, K, V, B: Backend> PersistedHashMapGuard<'s, K, V, B>
where
    K: Eq + Hash + Persistable,
    V: Persistable,
{
    /// Inserts `value` under `key`. If `key` already occupies a slot,
    /// its value is overwritten in place (the key itself doesn't need
    /// rewriting -- it can't have changed) and the old value is
    /// returned; otherwise a brand-new slot is appended at the current
    /// capacity, exactly like `PersistedVec::push` -- growing (or
    /// creating) the content allocation to fit, writing the new entry,
    /// then publishing the updated header last.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        let entry_size = entry_size::<K, V>() as u32;

        if let Some(&(slot, _)) = self.inner.entries.get(&key) {
            let pointer = self.inner.pointer.as_ref().unwrap();
            value.store(
                self.backend,
                Location {
                    anchor: pointer.raw(),
                    offset: slot as u32 * entry_size + 1 + K::INLINE_SIZE as u32,
                },
            );
            let (_, old_value) = self.inner.entries.insert(key, (slot, value)).unwrap();
            return Some(old_value);
        }

        let slot = self.inner.capacity;
        let new_capacity = slot + 1;
        let new_byte_size = new_capacity * entry_size as usize;
        match &self.inner.pointer {
            Some(pointer) => self.backend.resize(pointer, new_byte_size),
            None => {
                self.inner.pointer =
                    Some(self.backend.alloc::<PersistedHashMap<K, V>>(new_byte_size))
            }
        }
        let pointer = self.inner.pointer.as_ref().unwrap();
        write_entry(
            self.backend,
            pointer.raw(),
            slot as u32 * entry_size,
            &key,
            &value,
        );
        write_header(
            self.backend,
            self.location,
            pointer.index(),
            new_capacity as u32,
        );

        self.inner.capacity = new_capacity;
        self.inner.entries.insert(key, (slot, value));
        None
    }

    /// Removes and returns the value under `key`, if present, by
    /// tombstoning: clears the slot's liveness tag and forgets the key
    /// in memory. Nothing else's slot changes, so -- unlike a
    /// `swap_remove`-based design -- there's no other bookkeeping to fix
    /// up, and no header update (capacity doesn't shrink).
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let (slot, _) = self.inner.entries.get(key)?;
        let slot = *slot;
        let entry_size = entry_size::<K, V>() as u32;
        let pointer = self.inner.pointer.as_ref().unwrap();

        self.backend
            .write(pointer.raw(), slot as u32 * entry_size, &[0u8]);

        let (_, value) = self.inner.entries.remove(key).unwrap();
        Some(value)
    }
}

impl<'s, K, V, B: Backend> Guard for PersistedHashMapGuard<'s, K, V, B>
where
    K: Eq + Hash + Persistable,
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
    fn remove_deletes_but_leaves_other_entries_untouched() {
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
    fn insert_after_remove_appends_past_capacity_rather_than_reusing_the_tombstone() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut map = PersistedHashMap::<String, i32>::new();
        {
            let mut guard = map.guard(&backend, location);
            guard.insert("a".to_string(), 1);
            guard.insert("b".to_string(), 2);
            guard.remove(&"a".to_string());
            guard.insert("c".to_string(), 3);
        }

        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&"b".to_string()), Some(&2));
        assert_eq!(map.get(&"c".to_string()), Some(&3));

        backend.flush();
        let reloaded = PersistedHashMap::<String, i32>::load(&backend, location);
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded.get(&"b".to_string()), Some(&2));
        assert_eq!(reloaded.get(&"c".to_string()), Some(&3));
        assert_eq!(reloaded.get(&"a".to_string()), None);
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

    /// A key type that deliberately does *not* implement `Clone` --
    /// standing in for a future pointer-owning key type (e.g. a
    /// `PersistedString`) that couldn't implement `Clone` even if it
    /// wanted to, since it'd own a `UniquePointer`. Proves the map never
    /// needed `Clone` in the first place with this layout, not just that
    /// it no longer happens to exercise it.
    #[derive(Debug, PartialEq, Eq, Hash)]
    struct NonCloneKey(i32);

    impl Persistable for NonCloneKey {
        const INLINE_SIZE: usize = i32::INLINE_SIZE;

        type Guard<'s, B: Backend>
            = NonCloneKeyGuard<'s, B>
        where
            Self: 's,
            B: 's;

        fn guard<'s, B: Backend>(
            &'s mut self,
            backend: &'s B,
            location: Location,
        ) -> Self::Guard<'s, B> {
            NonCloneKeyGuard {
                inner: self,
                backend,
                location,
            }
        }

        fn store<B: Backend>(&self, backend: &B, location: Location) {
            self.0.store(backend, location);
        }

        fn load<B: Backend>(backend: &B, location: Location) -> Self {
            NonCloneKey(i32::load(backend, location))
        }
    }

    struct NonCloneKeyGuard<'s, B> {
        inner: &'s mut NonCloneKey,
        backend: &'s B,
        // Never read -- a key is never mutated in place (no `set`-style
        // method), so this fixture only needs to exist to satisfy
        // `Persistable::Guard`'s shape.
        #[allow(dead_code)]
        location: Location,
    }

    impl<'s, B: Backend> Guard for NonCloneKeyGuard<'s, B> {
        type Persistable = NonCloneKey;
        type Backend = B;

        fn as_persistable(&self) -> &NonCloneKey {
            self.inner
        }
        fn as_persistable_mut(&mut self) -> &mut NonCloneKey {
            self.inner
        }
        fn backend(&self) -> &B {
            self.backend
        }
    }

    #[test]
    fn non_clone_keys_work_end_to_end() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut map = PersistedHashMap::<NonCloneKey, i32>::new();

        {
            let mut guard = map.guard(&backend, location);
            guard.insert(NonCloneKey(1), 10);
            guard.insert(NonCloneKey(2), 20);
            guard.insert(NonCloneKey(3), 30);
            guard.remove(&NonCloneKey(1));
        }

        assert_eq!(map.get(&NonCloneKey(2)), Some(&20));
        assert_eq!(map.get(&NonCloneKey(3)), Some(&30));
        assert_eq!(map.get(&NonCloneKey(1)), None);
        assert_eq!(map.len(), 2);

        backend.flush();
        let reloaded = PersistedHashMap::<NonCloneKey, i32>::load(&backend, location);
        assert_eq!(reloaded.get(&NonCloneKey(2)), Some(&20));
        assert_eq!(reloaded.get(&NonCloneKey(3)), Some(&30));
    }
}
