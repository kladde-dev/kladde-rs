//! [`PersistableHashMap`] -- the backed variant of `HashMap<K, V>`.
//!
//! Snapshot layout: the on-disk representation doesn't need to support
//! O(1) lookup by key at all -- that's already handled by the in-memory
//! reconstruction. So the content allocation is a dense-*ish* array of
//! fixed-size slots (a 1-byte liveness tag, then `K`, then `V` --
//! `1 + K::INLINE_SIZE + V::INLINE_SIZE` bytes each), grown by `insert`
//! exactly like `PersistableVec`, but *not* compacted by `remove`: removing
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
    read_header, write_header, Backend, Guard, Location, Persistable, RawPointer,
    UniquePointerResizable,
};
use std::collections::hash_map;
use std::collections::HashMap;
use std::hash::Hash;
use std::ops::{Deref, DerefMut};

/// A hash map whose contents are persisted to the backing store.
///
/// Behaves like `std::collections::HashMap<K, V>` for reads (`get`,
/// `contains_key`, `iter`, `len`, ...), which touch only the in-memory
/// copy. Mutation goes through a [`PersistableHashMapGuard`] obtained from
/// [`Persistable::guard`] (or a derived parent's `_mut()` accessor):
/// `insert`/`remove`/`get_mut` each update memory *and* record the change
/// to the backend in one step. Both `K` and `V` need to implement
/// [`Persistable`] (and `K: Eq + Hash`, as usual); notably `K` does *not*
/// need `Clone`. Keys are not mutable in place -- no guard is ever handed
/// out for a key -- so a `PersistableString` key, for instance, is written
/// once and thereafter only read.
#[derive(Debug, PartialEq)]
pub struct PersistableHashMap<K: Eq + Hash, V> {
    /// key -> (slot index into the on-disk array, value). Only ever
    /// holds *live* entries -- a removed key is gone from here
    /// entirely, its former slot surviving on disk only as a tombstone.
    entries: HashMap<K, (usize, V)>,
    /// One past the highest slot index ever handed out by `insert` --
    /// i.e. the on-disk array's current length, tombstones included.
    /// Always `>= entries.len()`; strictly greater once anything has
    /// ever been removed.
    capacity: usize,
    /// The variable-capacity slot array holding this map's entries -- a
    /// [`UniquePointerResizable`] (a `Box<[u8]>`-like handle). Its byte
    /// capacity is allocator-owned; the type keeps only the logical slot
    /// count (`capacity`, published as the header's second field).
    pointer: Option<UniquePointerResizable>,
}

impl<K: Eq + Hash, V> PersistableHashMap<K, V> {
    pub fn new() -> Self {
        PersistableHashMap {
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

impl<K: Eq + Hash, V> Default for PersistableHashMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

type EntriesIter<'a, K, V> = hash_map::Iter<'a, K, (usize, V)>;
type EntriesMapFn<'a, K, V> = fn((&'a K, &'a (usize, V))) -> (&'a K, &'a V);

impl<'a, K: Eq + Hash, V> IntoIterator for &'a PersistableHashMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = std::iter::Map<EntriesIter<'a, K, V>, EntriesMapFn<'a, K, V>>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter().map(|(key, (_, value))| (key, value))
    }
}

impl<K, V> Persistable for PersistableHashMap<K, V>
where
    K: Eq + Hash + Persistable,
    V: Persistable,
{
    /// A fixed 8-byte `{ target, capacity }` header -- see `PersistableVec`'s
    /// identical layout note (the second field means slot *capacity*
    /// here, not live count -- see this module's doc comment).
    const INLINE_SIZE: usize = 8;

    type Guard<'s, B: Backend>
        = PersistableHashMapGuard<'s, K, V, B>
    where
        Self: 's,
        B: 's;

    #[inline]
    fn guard<'s, B: Backend>(
        &'s mut self,
        backend: &'s B,
        location: Location,
    ) -> Self::Guard<'s, B> {
        PersistableHashMapGuard {
            inner: self,
            backend,
            location,
        }
    }

    /// Publishes a header at `location` pointing at this map's content --
    /// used when a whole `PersistableHashMap` is being written as a
    /// brand-new value somewhere (e.g. a struct field being assembled)
    /// rather than via incremental `insert`/`remove`.
    ///
    /// Deliberately does *not* rewrite or compact any entries: `insert`/
    /// `remove` already keep the on-disk content at `self.pointer` in sync
    /// with `self.entries`/`self.capacity` incrementally, so if a pointer
    /// already exists its content is already correct and `store` only
    /// needs to point a new header at it. Rewriting entries at compacted
    /// positions here would be a correctness bug, not just wasted work:
    /// the map stays reachable and mutable at its original location too
    /// (it's typically a struct field being copied into another
    /// container), and its in-memory `(slot, value)` tracking would then
    /// disagree with the moved on-disk layout, so a later `get_mut`/
    /// `remove` there would read/write the wrong bytes. (Regression test:
    /// `store_does_not_disturb_further_mutation_of_the_same_live_map`.)
    fn store<B: Backend>(&mut self, backend: &B, location: Location) {
        match &self.pointer {
            Some(existing) => {
                write_header(backend, location, existing.index(), self.capacity as u32);
            }
            None => {
                // Unlike `PersistableVec` (which has a backend-free
                // `FromIterator`), there's no way to construct a
                // `PersistableHashMap` with entries but no pointer -- `new`
                // and `load` are the only constructors, and both keep the
                // two in sync. So `pointer` being `None` here always does
                // mean `entries`/`capacity` are genuinely empty too;
                // nothing to allocate, just record "no allocation yet"
                // directly (`write_header` requires a real index, so this
                // can't go through it).
                debug_assert!(self.entries.is_empty() && self.capacity == 0);
                backend.write(location.anchor, location.offset, &[0u8; 8]);
            }
        }
    }

    fn load<B: Backend>(backend: &B, location: Location) -> Self {
        let (target, capacity) = read_header(backend, location);
        let pointer = target.map(UniquePointerResizable::from_index);
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
        PersistableHashMap {
            entries,
            capacity: capacity as usize,
            pointer,
        }
    }

    fn describe_local(builder: &mut kladde_traits::SchemaBuilder) -> kladde_traits::TypeDescriptor
    where
        Self: 'static,
    {
        kladde_traits::TypeDescriptor::Opaque {
            library_name: "kladde-types".into(),
            type_name: "PersistableHashMap".into(),
            version: crate::library_version(),
            inline_size: 8,
            parameters: vec![
                <K as Persistable>::describe(builder),
                <V as Persistable>::describe(builder),
            ],
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
    key: &mut K,
    value: &mut V,
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

pub struct PersistableHashMapGuard<'s, K: Eq + Hash, V, B = kladde::DefaultBackend> {
    inner: &'s mut PersistableHashMap<K, V>,
    backend: &'s B,
    location: Location,
}

// `get_mut` doesn't need any extra bounds beyond `Persistable` -- kept
// in its own impl block so it stays available regardless of what
// `insert`/`remove` additionally need.
impl<'s, K: Eq + Hash + Persistable, V: Persistable, B: Backend>
    PersistableHashMapGuard<'s, K, V, B>
{
    #[inline]
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

impl<'s, K, V, B: Backend> PersistableHashMapGuard<'s, K, V, B>
where
    K: Eq + Hash + Persistable,
    V: Persistable,
{
    /// Inserts `value` under `key`. If `key` already occupies a slot,
    /// its value is overwritten in place (the key itself doesn't need
    /// rewriting -- it can't have changed) and the old value is
    /// returned; otherwise a brand-new slot is appended at the current
    /// capacity, exactly like `PersistableVec::push` -- growing (or
    /// creating) the content allocation to fit, writing the new entry,
    /// then publishing the updated header last.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        let entry_size = entry_size::<K, V>() as u32;

        if let Some(&(slot, _)) = self.inner.entries.get(&key) {
            let pointer = self.inner.pointer.as_ref().unwrap();
            let mut value = value;
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
            // A hash map slot is a hand-rolled `{ tag, K, V }` layout with no
            // single `Persistable` element type, so it allocates raw bytes via
            // the erased `alloc_resizable` rather than the typed `alloc_array`.
            None => self.inner.pointer = Some(self.backend.alloc_resizable(new_byte_size)),
        }
        let pointer = self.inner.pointer.as_ref().unwrap();
        let mut key = key;
        let mut value = value;
        write_entry(
            self.backend,
            pointer.raw(),
            slot as u32 * entry_size,
            &mut key,
            &mut value,
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

impl<'s, K, V, B: Backend> Guard for PersistableHashMapGuard<'s, K, V, B>
where
    K: Eq + Hash + Persistable,
    V: Persistable,
{
    type Persistable = PersistableHashMap<K, V>;
    type Backend = B;

    fn as_persistable(&self) -> &PersistableHashMap<K, V> {
        self.inner
    }
    fn as_persistable_mut(&mut self) -> &mut PersistableHashMap<K, V> {
        self.inner
    }
    fn backend(&self) -> &B {
        self.backend
    }
}

impl<'s, K: Eq + Hash, V, B> Deref for PersistableHashMapGuard<'s, K, V, B> {
    type Target = PersistableHashMap<K, V>;
    fn deref(&self) -> &PersistableHashMap<K, V> {
        self.inner
    }
}

impl<'s, K: Eq + Hash, V, B> DerefMut for PersistableHashMapGuard<'s, K, V, B> {
    fn deref_mut(&mut self) -> &mut PersistableHashMap<K, V> {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockBackend;
    use crate::PersistableString;
    use kladde_traits::Allocator;

    fn root_location(backend: &MockBackend) -> Location {
        let pointer =
            backend.alloc_fixed(PersistableHashMap::<PersistableString, i32>::INLINE_SIZE);
        Location {
            anchor: pointer.raw(),
            offset: 0,
        }
    }

    #[test]
    fn insert_adds_in_memory() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut map = PersistableHashMap::<PersistableString, i32>::new();

        let mut guard = map.guard(&backend, location);
        guard.insert(PersistableString::from("a"), 1);
        guard.insert(PersistableString::from("b"), 2);

        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&PersistableString::from("a")), Some(&1));
        assert_eq!(map.get(&PersistableString::from("b")), Some(&2));
    }

    #[test]
    fn insert_replacing_an_existing_key_returns_the_old_value() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut map = PersistableHashMap::<PersistableString, i32>::new();
        {
            let mut guard = map.guard(&backend, location);
            guard.insert(PersistableString::from("a"), 1);
        }

        let old = map
            .guard(&backend, location)
            .insert(PersistableString::from("a"), 2);

        assert_eq!(old, Some(1));
        assert_eq!(map.get(&PersistableString::from("a")), Some(&2));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn get_mut_returns_a_nested_guard_for_persistable_values() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut map = PersistableHashMap::<PersistableString, i32>::new();
        {
            let mut guard = map.guard(&backend, location);
            guard.insert(PersistableString::from("a"), 1);
        }

        let mut guard = map.guard(&backend, location);
        guard
            .get_mut(&PersistableString::from("a"))
            .unwrap()
            .set(99);

        assert_eq!(map.get(&PersistableString::from("a")), Some(&99));
    }

    #[test]
    fn remove_deletes_but_leaves_other_entries_untouched() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut map = PersistableHashMap::<PersistableString, i32>::new();
        {
            let mut guard = map.guard(&backend, location);
            guard.insert(PersistableString::from("a"), 1);
            guard.insert(PersistableString::from("b"), 2);
            guard.insert(PersistableString::from("c"), 3);
        }

        let removed = map
            .guard(&backend, location)
            .remove(&PersistableString::from("a"));

        assert_eq!(removed, Some(1));
        assert!(map.get(&PersistableString::from("a")).is_none());
        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&PersistableString::from("b")), Some(&2));
        assert_eq!(map.get(&PersistableString::from("c")), Some(&3));
    }

    #[test]
    fn insert_after_remove_appends_past_capacity_rather_than_reusing_the_tombstone() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut map = PersistableHashMap::<PersistableString, i32>::new();
        {
            let mut guard = map.guard(&backend, location);
            guard.insert(PersistableString::from("a"), 1);
            guard.insert(PersistableString::from("b"), 2);
            guard.remove(&PersistableString::from("a"));
            guard.insert(PersistableString::from("c"), 3);
        }

        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&PersistableString::from("b")), Some(&2));
        assert_eq!(map.get(&PersistableString::from("c")), Some(&3));

        backend.flush();
        let reloaded = PersistableHashMap::<PersistableString, i32>::load(&backend, location);
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded.get(&PersistableString::from("b")), Some(&2));
        assert_eq!(reloaded.get(&PersistableString::from("c")), Some(&3));
        assert_eq!(reloaded.get(&PersistableString::from("a")), None);
    }

    #[test]
    fn flushing_and_reloading_round_trips_the_content() {
        let backend = MockBackend::default();
        let location = root_location(&backend);
        let mut map = PersistableHashMap::<PersistableString, i32>::new();
        {
            let mut guard = map.guard(&backend, location);
            guard.insert(PersistableString::from("a"), 1);
            guard.insert(PersistableString::from("b"), 2);
            guard.insert(PersistableString::from("c"), 3);
        }
        {
            map.guard(&backend, location)
                .remove(&PersistableString::from("b"));
        }

        backend.flush();

        let reloaded = PersistableHashMap::<PersistableString, i32>::load(&backend, location);
        assert_eq!(reloaded.get(&PersistableString::from("a")), Some(&1));
        assert_eq!(reloaded.get(&PersistableString::from("b")), None);
        assert_eq!(reloaded.get(&PersistableString::from("c")), Some(&3));
        assert_eq!(reloaded.len(), 2);
    }

    /// A key type that deliberately does *not* implement `Clone` --
    /// standing in for a future pointer-owning key type (e.g. a
    /// `PersistableString`) that couldn't implement `Clone` even if it
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

        fn store<B: Backend>(&mut self, backend: &B, location: Location) {
            self.0.store(backend, location);
        }

        fn load<B: Backend>(backend: &B, location: Location) -> Self {
            NonCloneKey(i32::load(backend, location))
        }

        // Schema-transparent: this fixture shares `i32`'s descriptor rather
        // than owning one, so it overrides `describe` and leaves
        // `describe_local` as the (never-called) default -- the escape hatch
        // the two-layer `describe`/`describe_local` split exists to keep open.
        fn describe(builder: &mut kladde_traits::SchemaBuilder) -> kladde_traits::TypeRef {
            <i32 as Persistable>::describe(builder)
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
        let mut map = PersistableHashMap::<NonCloneKey, i32>::new();

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
        let reloaded = PersistableHashMap::<NonCloneKey, i32>::load(&backend, location);
        assert_eq!(reloaded.get(&NonCloneKey(2)), Some(&20));
        assert_eq!(reloaded.get(&NonCloneKey(3)), Some(&30));
    }

    #[test]
    fn store_reuses_an_existing_allocation_instead_of_leaking_it_with_i32_keys() {
        let backend = MockBackend::default();
        let location_a = root_location(&backend);
        let location_b = root_location(&backend);

        let mut map = PersistableHashMap::<i32, i32>::new();
        {
            let mut guard = map.guard(&backend, location_a);
            guard.insert(1, 10);
            guard.insert(2, 20);
        }
        backend.flush();
        let live_before = backend.live_count();

        // `map` already owns a live allocation from the inserts above --
        // `store` writing it somewhere new (e.g. as part of assembling a
        // struct field, without ever resetting `map`'s own pointer)
        // should reuse that allocation rather than leaking it.
        map.store(&backend, location_b);
        backend.flush();

        assert_eq!(
            backend.live_count(),
            live_before,
            "store() should reuse the existing allocation, not leak a second one"
        );

        let reloaded = PersistableHashMap::<i32, i32>::load(&backend, location_b);
        assert_eq!(reloaded.get(&1), Some(&10));
        assert_eq!(reloaded.get(&2), Some(&20));
    }

    /// The `String`-keyed version of the test above originally attempted
    /// back when this regression was first fixed (commit `99fa688`) --
    /// dropped at the time because a plain `String` key's own leak (every
    /// `store` allocating fresh, having no room for a pointer of its own)
    /// conflated two different bugs, and switched to `i32` keys to
    /// isolate what was actually being tested. Now that `PersistableString`
    /// exists (and doesn't have that problem), this closes the gap and
    /// should just pass.
    #[test]
    fn store_reuses_an_existing_allocation_instead_of_leaking_it_with_persisted_string_keys() {
        let backend = MockBackend::default();
        let location_a = root_location(&backend);
        let location_b = root_location(&backend);

        let mut map = PersistableHashMap::<PersistableString, i32>::new();
        {
            let mut guard = map.guard(&backend, location_a);
            guard.insert(PersistableString::from("a"), 10);
            guard.insert(PersistableString::from("b"), 20);
        }
        backend.flush();
        let live_before = backend.live_count();

        map.store(&backend, location_b);
        backend.flush();

        assert_eq!(
            backend.live_count(),
            live_before,
            "store() should reuse the existing allocation, not leak a second one"
        );

        let reloaded = PersistableHashMap::<PersistableString, i32>::load(&backend, location_b);
        assert_eq!(reloaded.get(&PersistableString::from("a")), Some(&10));
        assert_eq!(reloaded.get(&PersistableString::from("b")), Some(&20));
    }

    /// Proves the bug the *previous* version of `store` had, which this
    /// version's design (never rewrite existing content, only point a
    /// new header at it) sidesteps entirely: with tombstones already
    /// present, `store` used to rewrite every live entry at a freshly
    /// `enumerate()`d, compacted position -- but had no way to update
    /// `self.entries`' own tracked `(slot, value)` per key (`store` only
    /// has `&self`), so continuing to mutate the same live map
    /// afterward, through `get_mut`/`remove`, would compute offsets from
    /// those now-stale slots and silently read/write the wrong bytes.
    #[test]
    fn store_does_not_disturb_further_mutation_of_the_same_live_map() {
        let backend = MockBackend::default();
        let location_a = root_location(&backend);
        let location_b = root_location(&backend);

        let mut map = PersistableHashMap::<PersistableString, i32>::new();
        {
            let mut guard = map.guard(&backend, location_a);
            guard.insert(PersistableString::from("a"), 1);
            guard.insert(PersistableString::from("b"), 2);
            guard.insert(PersistableString::from("c"), 3);
            // Tombstones slot 0, leaving "b"/"c" at their original
            // slots (1, 2) with a gap before them -- capacity (3) now
            // exceeds the live count (2).
            guard.remove(&PersistableString::from("a"));
        }

        // Simulate writing this already-tombstone-laden map as a value
        // somewhere else (e.g. it's a struct field, and the struct is
        // being inserted as a new entry into another container).
        map.store(&backend, location_b);

        // Still reachable and mutable at its *original* location -- must
        // keep working correctly, using "b"/"c"'s pre-existing tracked
        // slots, which `store` must not have disturbed.
        {
            let mut guard = map.guard(&backend, location_a);
            guard
                .get_mut(&PersistableString::from("b"))
                .unwrap()
                .set(20);
            guard.remove(&PersistableString::from("c"));
        }

        assert_eq!(map.get(&PersistableString::from("b")), Some(&20));
        assert_eq!(map.get(&PersistableString::from("c")), None);

        backend.flush();
        let reloaded_a = PersistableHashMap::<PersistableString, i32>::load(&backend, location_a);
        assert_eq!(reloaded_a.get(&PersistableString::from("b")), Some(&20));
        assert_eq!(reloaded_a.get(&PersistableString::from("c")), None);
    }
}
