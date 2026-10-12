//! [`PersistableMap<K, V>`]: a map, encoded as its count and its entries,
//! in the order its index keeps them.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash};
use std::ops::Deref;

use kladde_persist::{
    varint_len, Backend, Decoder, Encoder, Error, Field, Guard, Node, Persistable, Region,
    SchemaBuilder, TypeDescriptor,
};

use crate::index::Sizes;
use crate::vec::write_count;

/// A map from persistable keys to persistable values: the number of entries
/// as a varint, then each entry's key and value, in the order the map keeps
/// them.
///
/// In memory, an index map: the entries in a vector, in their order on
/// file, and a hash table of their positions, so that each key is stored
/// once; and, for entries whose size depends on their value, a Fenwick tree
/// of their sizes. A lookup is a hash and an index; an insertion appends;
/// a removal moves the last entry into the removed one's place, with a
/// `Move` that writes none of its bytes, so the file stays dense and the
/// only position that changes in memory is the moved entry's.
///
/// ```
/// use kladde::Kladde;
/// use kladde_types::{PersistableMap, PersistableString};
///
/// let mut loans = Kladde::new(PersistableMap::<PersistableString, u32>::new());
/// let mut guard = loans.guard();
/// guard.insert("Emma".into(), 3)?;
/// guard.insert("Persuasion".into(), 1)?;
/// guard.get_mut(&"Emma".into()).unwrap().set(4)?;
/// assert_eq!(guard.remove(&"Persuasion".into())?, Some(1));
/// assert_eq!(loans.get().get(&"Emma".into()), Some(&4));
/// # Ok::<(), kladde::Error>(())
/// ```
pub struct PersistableMap<K, V> {
    entries: Vec<(K, V)>,
    table: Table,
    sizes: Sizes,
}

/// An open-addressing hash table of positions in `entries`, probed
/// linearly, at most half full.
#[derive(Clone)]
struct Table {
    slots: Vec<u32>,
    hasher: RandomState,
}

const EMPTY: u32 = u32::MAX;

impl Default for Table {
    fn default() -> Self {
        Table {
            slots: vec![EMPTY; 8],
            hasher: RandomState::new(),
        }
    }
}

impl Table {
    fn mask(&self) -> usize {
        self.slots.len() - 1
    }

    fn home<K: Hash>(&self, key: &K) -> usize {
        self.hasher.hash_one(key) as usize & self.mask()
    }

    /// The slot holding `key`'s position, or the empty slot where it would
    /// go.
    fn find<K: Hash + Eq, V>(&self, entries: &[(K, V)], key: &K) -> (usize, Option<usize>) {
        let mut slot = self.home(key);
        loop {
            match self.slots[slot] {
                EMPTY => return (slot, None),
                i if entries[i as usize].0 == *key => return (slot, Some(i as usize)),
                _ => slot = (slot + 1) & self.mask(),
            }
        }
    }

    /// Rebuilds the table for `entries`, at a size that keeps it at most
    /// half full after one more insertion.
    fn rebuild<K: Hash, V>(&mut self, entries: &[(K, V)]) {
        let capacity = ((entries.len() + 1) * 2).next_power_of_two().max(8);
        self.slots.clear();
        self.slots.resize(capacity, EMPTY);
        for (i, (key, _)) in entries.iter().enumerate() {
            let mut slot = self.home(key);
            while self.slots[slot] != EMPTY {
                slot = (slot + 1) & self.mask();
            }
            self.slots[slot] = i as u32;
        }
    }

    /// Empties `slot`, shifting back the entries probed past it.
    fn vacate<K: Hash, V>(&mut self, entries: &[(K, V)], mut slot: usize) {
        let mask = self.mask();
        self.slots[slot] = EMPTY;
        let mut next = (slot + 1) & mask;
        while self.slots[next] != EMPTY {
            let i = self.slots[next];
            let home = self.home(&entries[i as usize].0);
            // The entry at `next` may move back to `slot` if its home does
            // not lie cyclically in `(slot, next]`.
            let stays = if slot <= next {
                slot < home && home <= next
            } else {
                slot < home || home <= next
            };
            if !stays {
                self.slots[slot] = i;
                self.slots[next] = EMPTY;
                slot = next;
            }
            next = (next + 1) & mask;
        }
    }

    /// Points the slot of `key`, which holds position `from`, at position
    /// `to`.
    fn renumber<K: Hash>(&mut self, key: &K, from: usize, to: usize) {
        let mut slot = self.home(key);
        while self.slots[slot] != from as u32 {
            slot = (slot + 1) & self.mask();
        }
        self.slots[slot] = to as u32;
    }
}

fn entry_len<K: Persistable, V: Persistable>(entry: &(K, V)) -> usize {
    entry.0.encoded_len() + entry.1.encoded_len()
}

impl<K: Persistable + Hash + Eq, V: Persistable> PersistableMap<K, V> {
    /// An empty map.
    ///
    /// ```
    /// assert!(kladde_types::PersistableMap::<u8, u8>::new().is_empty());
    /// ```
    pub fn new() -> Self {
        Self::from_entries(Vec::new())
    }

    fn fixed() -> bool {
        <(K, V)>::FIXED_SIZE.is_some()
    }

    /// The map of `entries`, of which a later one with the same key replaces
    /// an earlier one, in place.
    fn from_entries(entries: Vec<(K, V)>) -> Self {
        let mut map = PersistableMap {
            entries: Vec::with_capacity(entries.len()),
            table: Table::default(),
            sizes: Sizes::default(),
        };
        for (key, value) in entries {
            match map.table.find(&map.entries, &key) {
                (_, Some(i)) => map.entries[i].1 = value,
                (slot, None) => {
                    map.table.slots[slot] = map.entries.len() as u32;
                    map.entries.push((key, value));
                    map.grow_table();
                }
            }
        }
        map.reindex();
        map
    }

    /// Rebuilds the table larger once it is half full.
    fn grow_table(&mut self) {
        if self.table.slots.len() < self.entries.len() * 2 + 2 {
            self.table.rebuild(&self.entries);
        }
    }

    fn reindex(&mut self) {
        if !Self::fixed() {
            self.sizes = Sizes::build(
                self.entries.iter().map(entry_len),
                varint_len(self.entries.len() as u64) as u64,
            );
        }
    }

    /// The number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entries, in their order on file.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.entries.iter().map(|(k, v)| (k, v))
    }

    /// The value of `key`, if the map holds it, in `O(1)` expected time.
    pub fn get(&self, key: &K) -> Option<&V> {
        let (_, i) = self.table.find(&self.entries, key);
        i.map(|i| &self.entries[i].1)
    }

    /// Whether the map holds `key`.
    pub fn contains_key(&self, key: &K) -> bool {
        self.get(key).is_some()
    }

    /// Where entry `i` starts within the map's encoding.
    fn offset_of(&self, i: usize) -> u64 {
        varint_len(self.entries.len() as u64) as u64
            + match <(K, V)>::FIXED_SIZE {
                Some(size) => (size * i) as u64,
                None => self.sizes.prefix(i),
            }
    }

    fn content_len(&self) -> u64 {
        match <(K, V)>::FIXED_SIZE {
            Some(size) => (size * self.entries.len()) as u64,
            None => self.sizes.total(),
        }
    }
}

impl<K: Persistable + Hash + Eq, V: Persistable> Default for PersistableMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Persistable + Hash + Eq, V: Persistable> FromIterator<(K, V)> for PersistableMap<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self::from_entries(iter.into_iter().collect())
    }
}

impl<K: Clone, V: Clone> Clone for PersistableMap<K, V> {
    fn clone(&self) -> Self {
        PersistableMap {
            entries: self.entries.clone(),
            table: self.table.clone(),
            sizes: self.sizes.clone(),
        }
    }
}

impl<K: std::fmt::Debug, V: std::fmt::Debug> std::fmt::Debug for PersistableMap<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(self.entries.iter().map(|(k, v)| (k, v)))
            .finish()
    }
}

impl<K: Persistable + Hash + Eq, V: Persistable> Persistable for PersistableMap<K, V> {
    const FIXED_SIZE: Option<usize> = None;

    fn encoded_len(&self) -> usize {
        varint_len(self.entries.len() as u64) + self.content_len() as usize
    }

    fn encode(&self, out: &mut Encoder<'_>) {
        out.varint(self.entries.len() as u64);
        for (key, value) in &self.entries {
            key.encode(out);
            value.encode(out);
        }
    }

    fn decode(input: &mut Decoder<'_>) -> Result<Self, Error> {
        let count = input.varint_as::<usize>()?;
        let mut entries = Vec::with_capacity(count.min(1 << 16));
        for _ in 0..count {
            entries.push((K::decode(input)?, V::decode(input)?));
        }
        let map = Self::from_entries(entries);
        if map.len() != count {
            return Err(input.corrupt("a map holds a key twice"));
        }
        Ok(map)
    }

    type Guard<'r, B: Backend + 'r>
        = PersistableMapGuard<'r, K, V, B>
    where
        Self: 'r;

    fn guard<'r, B: Backend + 'r>(&'r mut self, region: Region<'r, B>) -> Self::Guard<'r, B> {
        PersistableMapGuard {
            inner: self,
            region,
        }
    }

    fn describe_local(builder: &mut SchemaBuilder) -> TypeDescriptor
    where
        Self: 'static,
    {
        // A sequence of entries, each a struct of a key and a value.
        let entry = builder.describe_with(std::any::TypeId::of::<(K, V, Self)>(), |b| {
            TypeDescriptor::Struct {
                name: "Entry".into(),
                fields: vec![
                    Field {
                        name: "key".into(),
                        ty: K::describe(b),
                    },
                    Field {
                        name: "value".into(),
                        ty: V::describe(b),
                    },
                ],
            }
        });
        TypeDescriptor::Sequence(entry)
    }
}

/// The [`Guard`] of a [`PersistableMap`]: insert, remove, and reach each
/// value's own guard. See [`PersistableMap`] for an example.
pub struct PersistableMapGuard<'r, K, V, B> {
    inner: &'r mut PersistableMap<K, V>,
    region: Region<'r, B>,
}

impl<K: Persistable + Hash + Eq, V: Persistable, B: Backend> PersistableMapGuard<'_, K, V, B> {
    /// The guard of `key`'s value, or `None` if the map does not hold it. A
    /// change of the value's size updates the entry's size in the map's
    /// Fenwick tree.
    pub fn get_mut(&mut self, key: &K) -> Option<V::Guard<'_, B>> {
        let (_, i) = self.inner.table.find(&self.inner.entries, key);
        let i = i?;
        let at = self.inner.offset_of(i) + self.inner.entries[i].0.encoded_len() as u64;
        let fixed = PersistableMap::<K, V>::fixed();
        let PersistableMap { entries, sizes, .. } = &mut *self.inner;
        let value = &mut entries[i].1;
        let len = value.encoded_len() as u64;
        let node: Option<&dyn Node> = if fixed { None } else { Some(&*sizes) };
        Some(value.guard(self.region.child(at, len, node, i)))
    }

    /// Sets `key`'s value: replaces it in place and returns the old one if
    /// the map holds the key, and appends an entry otherwise.
    pub fn insert(&mut self, key: K, value: V) -> Result<Option<V>, Error> {
        let n = self.inner.entries.len();
        match self.inner.table.find(&self.inner.entries, &key) {
            (_, Some(i)) => {
                let at = self.inner.offset_of(i) + self.inner.entries[i].0.encoded_len() as u64;
                let old_len = self.inner.entries[i].1.encoded_len() as u64;
                let bytes = value.to_bytes();
                self.region.replace_at(at, old_len, &bytes)?;
                let map = &mut *self.inner;
                if !PersistableMap::<K, V>::fixed() {
                    map.sizes.add(i, bytes.len() as i64 - old_len as i64);
                }
                Ok(Some(std::mem::replace(&mut map.entries[i].1, value)))
            }
            (slot, None) => {
                if n >= EMPTY as usize - 1 {
                    return Err(Error::OutOfBounds);
                }
                let at = self.inner.offset_of(n);
                let mut bytes = key.to_bytes();
                value.encode(&mut Encoder::new(&mut bytes));
                self.region.transaction(|r| {
                    r.splice(at, 0, &bytes)?;
                    write_count(r, n, n + 1)
                })?;
                let map = &mut *self.inner;
                map.table.slots[slot] = n as u32;
                map.entries.push((key, value));
                map.grow_table();
                if !PersistableMap::<K, V>::fixed() {
                    map.sizes.push(bytes.len());
                    map.sizes.set_base(varint_len(n as u64 + 1) as u64);
                }
                Ok(None)
            }
        }
    }

    /// Removes `key`'s entry and returns its value: the last entry moves
    /// into its place, with a `Move` that writes none of its bytes.
    pub fn remove(&mut self, key: &K) -> Result<Option<V>, Error> {
        let (slot, i) = self.inner.table.find(&self.inner.entries, key);
        let Some(i) = i else { return Ok(None) };
        let n = self.inner.entries.len();
        let last = n - 1;
        let at = self.inner.offset_of(i);
        let len = entry_len(&self.inner.entries[i]) as u64;
        let last_at = self.inner.offset_of(last);
        let last_len = entry_len(&self.inner.entries[last]) as u64;
        self.region.transaction(|r| {
            if i != last {
                r.move_bytes(last_at, last_len, at)?;
            }
            r.splice(at + if i != last { last_len } else { 0 }, len, &[])?;
            write_count(r, n, n - 1)
        })?;
        let map = &mut *self.inner;
        map.table.vacate(&map.entries, slot);
        if i != last {
            let moved = &map.entries[last].0;
            map.table.renumber(moved, last, i);
        }
        let (_, value) = map.entries.swap_remove(i);
        if !PersistableMap::<K, V>::fixed() {
            if i != last {
                map.sizes.add(i, last_len as i64 - len as i64);
                map.sizes.pop(last_len as usize);
            } else {
                map.sizes.pop(len as usize);
            }
            map.sizes.set_base(varint_len(last as u64) as u64);
        }
        Ok(Some(value))
    }

    /// Removes `key`'s entry and drops it; whether the map held it.
    pub fn delete(&mut self, key: &K) -> Result<bool, Error> {
        self.remove(key).map(|v| v.is_some())
    }

    /// Removes every entry.
    pub fn clear(&mut self) -> Result<(), Error> {
        self.set(PersistableMap::new())
    }

    /// Replaces the whole map.
    pub fn set(&mut self, value: PersistableMap<K, V>) -> Result<(), Error> {
        kladde_persist::replace(self.inner, value, &self.region)
    }
}

impl<K, V, B> Guard for PersistableMapGuard<'_, K, V, B> {
    type Persistable = PersistableMap<K, V>;

    fn as_persistable(&self) -> &PersistableMap<K, V> {
        self.inner
    }
}

impl<K, V, B> Deref for PersistableMapGuard<'_, K, V, B> {
    type Target = PersistableMap<K, V>;

    fn deref(&self) -> &PersistableMap<K, V> {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_finds_every_key_through_removals() {
        let mut model = std::collections::HashMap::new();
        let mut map: PersistableMap<u32, u32> = PersistableMap::new();
        let mut seed = 1u64;
        for _ in 0..5000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let key = ((seed >> 33) % 200) as u32;
            if seed >> 63 == 0 {
                model.insert(key, key + 1);
                if map.table.find(&map.entries, &key).1.is_none() {
                    map = map.entries.into_iter().chain([(key, key + 1)]).collect();
                }
            } else if let (slot, Some(i)) = map.table.find(&map.entries, &key) {
                model.remove(&key);
                let last = map.entries.len() - 1;
                map.table.vacate(&map.entries, slot);
                if i != last {
                    let moved = map.entries[last].0;
                    map.table.renumber(&moved, last, i);
                }
                map.entries.swap_remove(i);
            }
            for k in 0..200 {
                assert_eq!(map.get(&k), model.get(&k), "key {k}");
            }
        }
    }
}
