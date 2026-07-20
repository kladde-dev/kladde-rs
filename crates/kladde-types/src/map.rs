//! [`PersistedHashMap`] -- the backed variant of `HashMap<K, V>`.

use kladde_traits::{Backend, Guard, Persistable};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::HashMap;
use std::hash::Hash;
use std::ops::{Deref, DerefMut};

/// Per `V1_QUESTIONS.md` question 11: keys are `Hash + Eq +
/// Serialize + DeserializeOwned`, not `Persistable` -- there's no
/// mutable access to a key in place, matching how most map APIs treat
/// keys as immutable once inserted. Values *are* `Persistable`, so
/// `get_mut` can hand back a `Guard`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PersistedHashMapOp<K, V> {
    Insert(K, V),
    Remove(K),
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(bound(
    serialize = "K: Eq + std::hash::Hash + Serialize, V: Serialize",
    deserialize = "K: Eq + std::hash::Hash + DeserializeOwned, V: DeserializeOwned"
))]
pub struct PersistedHashMap<K: Eq + Hash, V> {
    data: HashMap<K, V>,
}

impl<K: Eq + Hash, V> PersistedHashMap<K, V> {
    pub fn new() -> Self {
        PersistedHashMap {
            data: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        self.data.get(key)
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.data.contains_key(key)
    }

    pub fn iter(&self) -> std::collections::hash_map::Iter<'_, K, V> {
        self.data.iter()
    }
}

impl<K: Eq + Hash, V> Default for PersistedHashMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a, K: Eq + Hash, V> IntoIterator for &'a PersistedHashMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = std::collections::hash_map::Iter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.data.iter()
    }
}

impl<K, V> Persistable for PersistedHashMap<K, V>
where
    K: Eq + Hash + Serialize + DeserializeOwned,
    V: Serialize + DeserializeOwned,
{
    type Op = PersistedHashMapOp<K, V>;
    type Guard<'s, B: Backend>
        = PersistedHashMapGuard<'s, K, V, B>
    where
        Self: 's,
        B: 's;

    fn guard<'s, B: Backend>(&'s mut self, backend: &'s B) -> Self::Guard<'s, B> {
        PersistedHashMapGuard {
            inner: self,
            backend,
        }
    }
}

pub struct PersistedHashMapGuard<'s, K: Eq + Hash, V, B = kladde::DefaultBackend> {
    inner: &'s mut PersistedHashMap<K, V>,
    backend: &'s B,
}

// `get_mut` doesn't need `K`/`V: Clone` -- kept in its own impl block so
// it stays available for non-`Clone` value types.
impl<'s, K: Eq + Hash, V: Persistable, B: Backend> PersistedHashMapGuard<'s, K, V, B> {
    pub fn get_mut(&mut self, key: &K) -> Option<V::Guard<'_, B>> {
        self.inner.data.get_mut(key).map(|v| v.guard(self.backend))
    }
}

impl<'s, K, V, B: Backend> PersistedHashMapGuard<'s, K, V, B>
where
    K: Eq + Hash + Clone + Serialize + DeserializeOwned,
    V: Clone + Serialize + DeserializeOwned,
{
    /// Inserts `value` under `key` (replacing any previous value),
    /// recording one `Insert` op. Requires `K`/`V: Clone` for the same
    /// reason `PersistedVec::push` does -- see `spec.md`'s Future Work.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.backend
            .record::<PersistedHashMap<K, V>>(&PersistedHashMapOp::Insert(
                key.clone(),
                value.clone(),
            ));
        self.inner.data.insert(key, value)
    }
}

impl<'s, K, V, B: Backend> PersistedHashMapGuard<'s, K, V, B>
where
    K: Eq + Hash + Clone + Serialize + DeserializeOwned,
    V: Serialize + DeserializeOwned,
{
    /// Removes and returns the value under `key`, if present, recording
    /// one `Remove` op.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        self.backend
            .record::<PersistedHashMap<K, V>>(&PersistedHashMapOp::Remove(key.clone()));
        self.inner.data.remove(key)
    }
}

impl<'s, K, V, B: Backend> Guard for PersistedHashMapGuard<'s, K, V, B>
where
    K: Eq + Hash + Serialize + DeserializeOwned,
    V: Serialize + DeserializeOwned,
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

    #[test]
    fn insert_adds_and_records_one_op() {
        let backend = MockBackend::default();
        let mut map = PersistedHashMap::<String, i32>::new();

        let mut guard = map.guard(&backend);
        guard.insert("a".to_string(), 1);
        guard.insert("b".to_string(), 2);

        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&"a".to_string()), Some(&1));
        assert_eq!(backend.record_count(), 2);
    }

    #[test]
    fn get_mut_returns_a_nested_guard_for_persistable_values() {
        let backend = MockBackend::default();
        let mut map = PersistedHashMap::<String, i32>::new();
        {
            let mut guard = map.guard(&backend);
            guard.insert("a".to_string(), 1);
        }

        let mut guard = map.guard(&backend);
        guard.get_mut(&"a".to_string()).unwrap().set(99);

        assert_eq!(map.get(&"a".to_string()), Some(&99));
        assert_eq!(backend.record_count(), 2);
    }

    #[test]
    fn remove_deletes_and_records_one_op() {
        let backend = MockBackend::default();
        let mut map = PersistedHashMap::<String, i32>::new();
        {
            let mut guard = map.guard(&backend);
            guard.insert("a".to_string(), 1);
        }

        let removed = map.guard(&backend).remove(&"a".to_string());

        assert_eq!(removed, Some(1));
        assert!(map.get(&"a".to_string()).is_none());
        assert_eq!(backend.record_count(), 2);
    }
}
