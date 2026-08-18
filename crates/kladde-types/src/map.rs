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

use crate::vec::{read_slot, write_slot};
use kladde_persist::{
    Guard, Location, Persistable, Pointer, PointerRepr, ReadBackend, UniquePointerResizable, Word,
    WriteBackend,
};
use std::collections::hash_map;
use std::collections::HashMap;
use std::hash::Hash;
use std::io::Read;
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
pub struct PersistableHashMap<K: Eq + Hash, V, P = Pointer> {
    /// key -> (slot index into the on-disk array, value). Only ever
    /// holds *live* entries -- a removed key is gone from here
    /// entirely, its former slot surviving on disk only as a tombstone.
    entries: HashMap<K, (usize, V)>,
    /// One past the highest slot index ever handed out by `insert` -- i.e. the
    /// on-disk array's current slot count, tombstones included. Always
    /// `>= entries.len()`; strictly greater once anything has ever been
    /// removed. Mirrors `allocation size / entry size`, which is where `load`
    /// recovers it from.
    capacity: usize,
    /// The variable-capacity slot array holding this map's entries. Its byte
    /// capacity is allocator-owned and *is* the slot count: nothing publishes
    /// a separate capacity field (see `PersistableVec`'s module docs).
    pointer: Option<UniquePointerResizable<P>>,
}

impl<K: Eq + Hash, V, P> PersistableHashMap<K, V, P> {
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

impl<K: Eq + Hash, V, P> Default for PersistableHashMap<K, V, P> {
    fn default() -> Self {
        Self::new()
    }
}

type EntriesIter<'a, K, V> = hash_map::Iter<'a, K, (usize, V)>;
type EntriesMapFn<'a, K, V> = fn((&'a K, &'a (usize, V))) -> (&'a K, &'a V);

impl<'a, K: Eq + Hash, V, P> IntoIterator for &'a PersistableHashMap<K, V, P> {
    type Item = (&'a K, &'a V);
    type IntoIter = std::iter::Map<EntriesIter<'a, K, V>, EntriesMapFn<'a, K, V>>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter().map(|(key, (_, value))| (key, value))
    }
}

impl<K, V, P> Persistable<P> for PersistableHashMap<K, V, P>
where
    K: Eq + Hash + Persistable<P>,
    V: Persistable<P>,
    P: PointerRepr,
{
    /// Just the slot array's id -- the slot count is the allocation's size
    /// divided by the (fixed) entry size, so nothing publishes it separately.
    /// See `PersistableVec`'s module docs.
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

    /// Publishes this map's content pointer at `location` -- used when a whole
    /// `PersistableHashMap` is being written as a brand-new value somewhere
    /// (e.g. a struct field being assembled) rather than via incremental
    /// `insert`/`remove`.
    ///
    /// Deliberately does *not* rewrite or compact any entries:
    /// `insert`/`remove` already keep the on-disk content at `self.pointer` in
    /// sync with `self.entries`/`self.capacity` incrementally, so if a pointer
    /// already exists its content is already correct and `store` only needs to
    /// point a new slot at it. Rewriting entries at compacted positions here
    /// would be a correctness bug, not just wasted work: the map stays reachable
    /// and mutable at its original location too (it's typically a struct field
    /// being copied into another container), and its in-memory `(slot, value)`
    /// tracking would then disagree with the moved on-disk layout, so a later
    /// `get_mut`/`remove` there would read/write the wrong bytes. (Regression
    /// test: `store_does_not_disturb_further_mutation_of_the_same_live_map`.)
    fn store<B: WriteBackend<Pointer = P>>(&mut self, backend: &B, location: Location<P, B::Size>) {
        // Unlike `PersistableVec` (which has a backend-free `FromIterator`),
        // there is no way to construct a `PersistableHashMap` with entries but
        // no pointer -- `new` and `load` are the only constructors, and both
        // keep the two in sync. So `pointer` being `None` here always does mean
        // `entries`/`capacity` are genuinely empty too, and there is nothing to
        // allocate.
        debug_assert!(
            self.pointer.is_some() || (self.entries.is_empty() && self.capacity == 0),
            "a pointerless PersistableHashMap must be empty",
        );
        write_slot(backend, location, self.pointer.as_ref().map(|p| p.raw()));
    }

    fn load<B: ReadBackend<Pointer = P>>(backend: &mut B, location: Location<P, B::Size>) -> Self {
        let Some(target) = read_slot::<P, B>(backend, location) else {
            return PersistableHashMap::new();
        };
        let entry_size = entry_size::<K, V, P>();
        let capacity = backend
            .size(target)
            .expect("PersistableHashMap slot array is live")
            .to_usize()
            / entry_size;

        let mut entries = HashMap::new();
        for slot in 0..capacity {
            let base = slot * entry_size;
            let mut tag = [0u8; 1];
            backend
                .read_at(target, Word::from_usize(base))
                .read_exact(&mut tag)
                .expect("read slot liveness tag");
            if tag[0] == 0 {
                continue;
            }
            let key = K::load(backend, Location::new(target, Word::from_usize(base + 1)));
            let value = V::load(
                backend,
                Location::new(
                    target,
                    Word::from_usize(base + 1 + <K as Persistable<P>>::INLINE_SIZE),
                ),
            );
            entries.insert(key, (slot, value));
        }
        PersistableHashMap {
            entries,
            capacity,
            pointer: Some(UniquePointerResizable::from_pointer(target)),
        }
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

/// One slot: a liveness tag, then the key, then the value.
fn entry_size<K: Persistable<P>, V: Persistable<P>, P: PointerRepr>() -> usize {
    1 + <K as Persistable<P>>::INLINE_SIZE + <V as Persistable<P>>::INLINE_SIZE
}

/// Writes one live slot (tag + key + value) at `offset` within `target`.
fn write_entry<B, K, V, P>(backend: &B, target: P, offset: usize, key: &mut K, value: &mut V)
where
    B: WriteBackend<Pointer = P>,
    K: Persistable<P>,
    V: Persistable<P>,
    P: PointerRepr,
{
    backend.write(target, Word::from_usize(offset), &[1u8]);
    key.store(backend, Location::new(target, Word::from_usize(offset + 1)));
    value.store(
        backend,
        Location::new(
            target,
            Word::from_usize(offset + 1 + <K as Persistable<P>>::INLINE_SIZE),
        ),
    );
}

/// The mutation-capable view onto a [`PersistableHashMap`].
pub struct PersistableHashMapGuard<'s, K: Eq + Hash, V, B: WriteBackend> {
    inner: &'s mut PersistableHashMap<K, V, B::Pointer>,
    backend: &'s B,
    location: Location<B::Pointer, B::Size>,
}

// `get_mut` doesn't need any extra bounds beyond `Persistable` -- kept in its
// own impl block so it stays available regardless of what `insert`/`remove`
// additionally need.
impl<'s, K, V, B> PersistableHashMapGuard<'s, K, V, B>
where
    K: Eq + Hash + Persistable<B::Pointer>,
    V: Persistable<B::Pointer>,
    B: WriteBackend,
{
    #[inline]
    pub fn get_mut(&mut self, key: &K) -> Option<<V as Persistable<B::Pointer>>::Guard<'_, B>> {
        let slot = self.inner.entries.get(key)?.0;
        let entry_size = entry_size::<K, V, B::Pointer>();
        let pointer = self.inner.pointer.as_ref()?.raw();
        let location = Location::new(
            pointer,
            Word::from_usize(slot * entry_size + 1 + <K as Persistable<B::Pointer>>::INLINE_SIZE),
        );
        let (_, value) = self.inner.entries.get_mut(key)?;
        Some(value.guard(self.backend, location))
    }
}

impl<'s, K, V, B> PersistableHashMapGuard<'s, K, V, B>
where
    K: Eq + Hash + Persistable<B::Pointer>,
    V: Persistable<B::Pointer>,
    B: WriteBackend,
{
    /// Inserts `value` under `key`. If `key` already occupies a slot, its value
    /// is overwritten in place (the key itself doesn't need rewriting -- it
    /// can't have changed) and the old value is returned; otherwise a brand-new
    /// slot is appended at the current capacity, exactly like
    /// `PersistableVec::push` -- growing (or creating) the slot array to fit,
    /// then writing the new entry. Growing the array *is* publishing the new
    /// slot count, so there is no separate header write.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        let entry_size = entry_size::<K, V, B::Pointer>();
        let key_size = <K as Persistable<B::Pointer>>::INLINE_SIZE;

        if let Some(&(slot, _)) = self.inner.entries.get(&key) {
            let pointer = self.inner.pointer.as_ref().unwrap();
            let mut value = value;
            value.store(
                self.backend,
                Location::new(
                    pointer.raw(),
                    Word::from_usize(slot * entry_size + 1 + key_size),
                ),
            );
            let (_, old_value) = self.inner.entries.insert(key, (slot, value)).unwrap();
            return Some(old_value);
        }

        let slot = self.inner.capacity;
        let new_capacity = slot + 1;
        let new_byte_size = Word::from_usize(new_capacity * entry_size);
        match &self.inner.pointer {
            Some(pointer) => self
                .backend
                .resize(pointer, new_byte_size)
                .expect("grow PersistableHashMap slot array"),
            // A hash map slot is a hand-rolled `{ tag, K, V }` layout with no
            // single `Persistable` element type, so it allocates raw bytes via
            // the erased `alloc_resizable` rather than a typed helper.
            None => {
                let pointer = self.backend.alloc_resizable(new_byte_size);
                write_slot(self.backend, self.location, Some(pointer.raw()));
                self.inner.pointer = Some(pointer);
            }
        }
        let pointer = self.inner.pointer.as_ref().unwrap();
        let mut key = key;
        let mut value = value;
        write_entry(
            self.backend,
            pointer.raw(),
            slot * entry_size,
            &mut key,
            &mut value,
        );

        self.inner.capacity = new_capacity;
        self.inner.entries.insert(key, (slot, value));
        None
    }

    /// Removes and returns the value under `key`, if present, by tombstoning:
    /// clears the slot's liveness tag and forgets the key in memory. Nothing
    /// else's slot changes, so -- unlike a `swap_remove`-based design -- there
    /// is no other bookkeeping to fix up, and the slot array does not shrink.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let (slot, _) = self.inner.entries.get(key)?;
        let slot = *slot;
        let entry_size = entry_size::<K, V, B::Pointer>();
        let pointer = self.inner.pointer.as_ref().unwrap();

        self.backend
            .write(pointer.raw(), Word::from_usize(slot * entry_size), &[0u8]);

        let (_, value) = self.inner.entries.remove(key).unwrap();
        Some(value)
    }
}

impl<'s, K: Eq + Hash, V, B: WriteBackend> Guard for PersistableHashMapGuard<'s, K, V, B> {
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

impl<'s, K: Eq + Hash, V, B: WriteBackend> Deref for PersistableHashMapGuard<'s, K, V, B> {
    type Target = PersistableHashMap<K, V, B::Pointer>;
    fn deref(&self) -> &Self::Target {
        self.inner
    }
}

impl<'s, K: Eq + Hash, V, B: WriteBackend> DerefMut for PersistableHashMapGuard<'s, K, V, B> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{root_location as root_alloc, MockBackend};
    use crate::PersistableString;

    fn root_location(backend: &MockBackend) -> Location<Pointer, u32> {
        root_alloc(
            backend,
            <PersistableHashMap<PersistableString, i32> as Persistable>::INLINE_SIZE,
        )
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
        let mut backend = MockBackend::default();
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
        let reloaded = <PersistableHashMap<PersistableString, i32> as Persistable>::load(
            &mut backend,
            location,
        );
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded.get(&PersistableString::from("b")), Some(&2));
        assert_eq!(reloaded.get(&PersistableString::from("c")), Some(&3));
        assert_eq!(reloaded.get(&PersistableString::from("a")), None);
    }

    #[test]
    fn flushing_and_reloading_round_trips_the_content() {
        let mut backend = MockBackend::default();
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

        let reloaded = <PersistableHashMap<PersistableString, i32> as Persistable>::load(
            &mut backend,
            location,
        );
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

    impl<P: PointerRepr> Persistable<P> for NonCloneKey {
        const INLINE_SIZE: usize = <i32 as Persistable<P>>::INLINE_SIZE;

        type Guard<'s, B: WriteBackend<Pointer = P>>
            = NonCloneKeyGuard<'s, B>
        where
            Self: 's,
            B: 's;

        fn guard<'s, B: WriteBackend<Pointer = P>>(
            &'s mut self,
            backend: &'s B,
            location: Location<P, B::Size>,
        ) -> Self::Guard<'s, B> {
            NonCloneKeyGuard {
                inner: self,
                backend,
                location,
            }
        }

        fn store<B: WriteBackend<Pointer = P>>(
            &mut self,
            backend: &B,
            location: Location<P, B::Size>,
        ) {
            self.0.store(backend, location);
        }

        fn load<B: ReadBackend<Pointer = P>>(
            backend: &mut B,
            location: Location<P, B::Size>,
        ) -> Self {
            NonCloneKey(i32::load(backend, location))
        }

        // Schema-transparent: this fixture shares `i32`'s descriptor rather
        // than owning one, so it overrides `describe` and leaves
        // `describe_local` as the (never-called) default -- the escape hatch
        // the two-layer `describe`/`describe_local` split exists to keep open.
        fn describe(builder: &mut kladde_persist::SchemaBuilder) -> kladde_persist::TypeRef {
            <i32 as Persistable<P>>::describe(builder)
        }
    }

    struct NonCloneKeyGuard<'s, B: WriteBackend> {
        inner: &'s mut NonCloneKey,
        backend: &'s B,
        // Never read -- a key is never mutated in place (no `set`-style
        // method), so this fixture only needs to exist to satisfy
        // `Persistable::Guard`'s shape.
        #[allow(dead_code)]
        location: Location<B::Pointer, B::Size>,
    }

    impl<'s, B: WriteBackend> Guard for NonCloneKeyGuard<'s, B> {
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
        let mut backend = MockBackend::default();
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
        let reloaded =
            <PersistableHashMap<NonCloneKey, i32> as Persistable>::load(&mut backend, location);
        assert_eq!(reloaded.get(&NonCloneKey(2)), Some(&20));
        assert_eq!(reloaded.get(&NonCloneKey(3)), Some(&30));
    }

    #[test]
    fn store_reuses_an_existing_allocation_instead_of_leaking_it_with_i32_keys() {
        let mut backend = MockBackend::default();
        let location_a = root_location(&backend);
        let location_b = root_location(&backend);

        let mut map = PersistableHashMap::<i32, i32>::new();
        {
            let mut guard = map.guard(&backend, location_a);
            guard.insert(1, 10);
            guard.insert(2, 20);
        }
        let live_before = backend.live_count();

        // `map` already owns a live allocation from the inserts above --
        // `store` writing it somewhere new (e.g. as part of assembling a
        // struct field, without ever resetting `map`'s own pointer)
        // should reuse that allocation rather than leaking it.
        map.store(&backend, location_b);
        assert_eq!(
            backend.live_count(),
            live_before,
            "store() should reuse the existing allocation, not leak a second one"
        );

        let reloaded =
            <PersistableHashMap<i32, i32> as Persistable>::load(&mut backend, location_b);
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
        let mut backend = MockBackend::default();
        let location_a = root_location(&backend);
        let location_b = root_location(&backend);

        let mut map = PersistableHashMap::<PersistableString, i32>::new();
        {
            let mut guard = map.guard(&backend, location_a);
            guard.insert(PersistableString::from("a"), 10);
            guard.insert(PersistableString::from("b"), 20);
        }
        let live_before = backend.live_count();

        map.store(&backend, location_b);
        assert_eq!(
            backend.live_count(),
            live_before,
            "store() should reuse the existing allocation, not leak a second one"
        );

        let reloaded = <PersistableHashMap<PersistableString, i32> as Persistable>::load(
            &mut backend,
            location_b,
        );
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
        let mut backend = MockBackend::default();
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
        let reloaded_a = <PersistableHashMap<PersistableString, i32> as Persistable>::load(
            &mut backend,
            location_a,
        );
        assert_eq!(reloaded_a.get(&PersistableString::from("b")), Some(&20));
        assert_eq!(reloaded_a.get(&PersistableString::from("c")), None);
    }
}
