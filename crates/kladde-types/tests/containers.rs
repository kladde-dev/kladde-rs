//! Differential tests: random edits of nested containers through their
//! guards, after which the file's sequence must equal the encoding of the
//! value in memory, byte for byte, before and after reopening, and every
//! container's index must match its elements.

use std::collections::HashMap;

use kladde::{Kladde, MemoryStorage, Options, Persistable, TypeRef};
use kladde_types::{PersistableMap, PersistableString, PersistableVec};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn text(&mut self) -> String {
        let n = self.below(12);
        (0..n)
            .map(|_| (b'a' + self.below(26) as u8) as char)
            .collect()
    }
}

#[derive(Persistable, Debug, Clone, PartialEq)]
struct Item {
    name: PersistableString,
    tags: PersistableVec<u32>,
    points: PersistableVec<(f32, f32)>,
}

#[derive(Persistable, Debug, Clone, PartialEq)]
struct Doc {
    title: PersistableString,
    items: PersistableVec<Item>,
    archive: PersistableVec<Item>,
}

fn item(rng: &mut Rng) -> Item {
    Item {
        name: rng.text().into(),
        tags: (0..rng.below(4))
            .map(|_| (rng.next() >> rng.below(64)) as u32)
            .collect(),
        points: (0..rng.below(3)).map(|i| (i as f32, 1.0)).collect(),
    }
}

/// Checks the file against the value, and every vector's index.
fn check<T: Persistable + 'static>(k: &mut Kladde<T>, storage: &MemoryStorage, what: &str) {
    k.flush().unwrap();
    k.store().check();
    let bytes = k.store().read_all().unwrap();
    assert!(bytes == k.get().to_bytes(), "{what}: the sequence");
    assert_eq!(
        T::schema().measure(TypeRef(0), &bytes),
        Some(bytes.len()),
        "{what}: the schema measures the sequence"
    );
    let reopened = Kladde::<T>::open_in(
        Box::new(MemoryStorage::from_image(storage.image())),
        Options::default(),
    )
    .unwrap();
    assert!(
        reopened.get().to_bytes() == bytes,
        "{what}: after reopening"
    );
}

fn check_indexes(doc: &Doc) {
    for list in [&doc.items, &doc.archive] {
        list.check();
        for item in list.iter() {
            item.tags.check();
            item.points.check();
        }
    }
}

/// One random edit, through the guards, mirrored on `model`.
fn step(k: &mut Kladde<Doc>, model: &mut Doc, rng: &mut Rng) {
    let n = model.items.len();
    let mut g = k.guard();
    match rng.below(12) {
        0 => {
            let it = item(rng);
            let i = rng.below(n + 1);
            g.items_mut().insert(i, it.clone()).unwrap();
            let mut items = model.items.clone().into_vec();
            items.insert(i, it);
            model.items = items.into();
        }
        1 if n > 0 => {
            let i = rng.below(n);
            let removed = g.items_mut().remove(i).unwrap();
            let mut items = model.items.clone().into_vec();
            assert_eq!(items.remove(i), removed);
            model.items = items.into();
        }
        2 if n > 1 => {
            let (from, to) = (rng.below(n), rng.below(n));
            g.items_mut().move_element(from, to).unwrap();
            let mut items = model.items.clone().into_vec();
            let it = items.remove(from);
            items.insert(to, it);
            model.items = items.into();
        }
        3 if n > 0 => {
            // A cut from the items, pasted into the archive.
            let i = rng.below(n);
            let j = rng.below(model.archive.len() + 1);
            let mut parts = g.parts();
            let cut = parts.items.cut(i).unwrap();
            parts.archive.insert_cut(j, cut).unwrap();
            let mut items = model.items.clone().into_vec();
            let mut archive = model.archive.clone().into_vec();
            archive.insert(j, items.remove(i));
            model.items = items.into();
            model.archive = archive.into();
        }
        4 if !model.archive.is_empty() => {
            // Back again, within the items, or dropped.
            let i = rng.below(model.archive.len());
            let cut = g.archive_mut().cut(i).unwrap();
            let mut archive = model.archive.clone().into_vec();
            let it = archive.remove(i);
            model.archive = archive.into();
            if rng.below(3) == 0 {
                cut.commit().unwrap();
            } else {
                g.items_mut().push_cut(cut).unwrap();
                let mut items = model.items.clone().into_vec();
                items.push(it);
                model.items = items.into();
            }
        }
        5 | 6 if n > 0 => {
            // Edit one item's parts together: a tag that changes size, then
            // the name after it moved.
            let i = rng.below(n);
            let text = rng.text();
            let tag = (rng.next() >> rng.below(64)) as u32;
            let mut items = g.items_mut();
            let mut it = items.get_mut(i).unwrap();
            let mut parts = it.parts();
            if parts.tags.is_empty() {
                parts.tags.push(tag).unwrap();
            } else {
                parts.tags.get_mut(0).unwrap().set(tag).unwrap();
            }
            parts.name.push_str(&text).unwrap();
            if let Some(mut p) = parts.points.get_mut(0) {
                let (mut x, _) = p.parts();
                x.set(2.5).unwrap();
            }
            let mut items = model.items.clone().into_vec();
            let m = &mut items[i];
            let mut tags = m.tags.clone().into_vec();
            if tags.is_empty() {
                tags.push(tag);
            } else {
                tags[0] = tag;
            }
            m.tags = tags.into();
            m.name = format!("{}{text}", m.name).into();
            let mut points = m.points.clone().into_vec();
            if let Some(p) = points.first_mut() {
                p.0 = 2.5;
            }
            m.points = points.into();
            model.items = items.into();
        }
        7 if n > 0 => {
            let i = rng.below(n);
            let name = rng.text();
            g.items_mut()
                .get_mut(i)
                .unwrap()
                .name_mut()
                .set(name.as_str())
                .unwrap();
            let mut items = model.items.clone().into_vec();
            items[i].name = name.into();
            model.items = items.into();
        }
        8 => {
            let title = rng.text();
            g.title_mut().set(title.as_str()).unwrap();
            model.title = title.into();
        }
        9 if n > 0 => {
            let i = rng.below(n);
            let removed = g.items_mut().get_mut(i).unwrap().tags_mut().pop().unwrap();
            let mut items = model.items.clone().into_vec();
            let mut tags = items[i].tags.clone().into_vec();
            assert_eq!(tags.pop(), removed);
            items[i].tags = tags.into();
            model.items = items.into();
        }
        _ => {
            let it = item(rng);
            g.archive_mut().push(it.clone()).unwrap();
            let mut archive = model.archive.clone().into_vec();
            archive.push(it);
            model.archive = archive.into();
        }
    }
}

#[test]
fn random_edits_of_nested_containers() {
    for seed in 1..40u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let storage = MemoryStorage::new();
        let doc = Doc {
            title: "doc".into(),
            items: (0..3).map(|_| item(&mut rng)).collect(),
            archive: PersistableVec::new(),
        };
        let mut model = doc.clone();
        let mut k = Kladde::create_in(Box::new(storage.clone()), doc, Options::default()).unwrap();
        for s in 0..150 {
            step(&mut k, &mut model, &mut rng);
            assert_eq!(*k.get(), model, "seed {seed}, step {s}");
            check_indexes(k.get());
            if s % 25 == 0 {
                check(&mut k, &storage, &format!("seed {seed}, step {s}"));
            }
        }
        check(&mut k, &storage, &format!("seed {seed}, at the end"));
    }
}

#[test]
fn a_map_keeps_its_keys_through_swap_removals() {
    let storage = MemoryStorage::new();
    let mut k = Kladde::create_in(
        Box::new(storage.clone()),
        PersistableMap::<u32, PersistableString>::new(),
        Options::default(),
    )
    .unwrap();
    let mut model: HashMap<u32, String> = HashMap::new();
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    for s in 0..2000 {
        let key = rng.below(60) as u32;
        let mut g = k.guard();
        match rng.below(4) {
            0 | 1 => {
                let v = rng.text();
                let old = g.insert(key, v.as_str().into()).unwrap();
                assert_eq!(old.map(String::from), model.insert(key, v));
            }
            2 => {
                let old = g.remove(&key).unwrap();
                assert_eq!(old.map(String::from), model.remove(&key));
            }
            _ => {
                let more = rng.text();
                match g.get_mut(&key) {
                    Some(mut v) => v.push_str(&more).unwrap(),
                    None => assert!(!model.contains_key(&key)),
                }
                if let Some(v) = model.get_mut(&key) {
                    v.push_str(&more);
                }
            }
        }
        assert_eq!(k.get().len(), model.len());
        for (key, value) in &model {
            assert_eq!(k.get().get(key).map(|v| &**v), Some(value.as_str()));
        }
        if s % 200 == 0 {
            check(&mut k, &storage, &format!("step {s}"));
        }
    }
    check(&mut k, &storage, "at the end");
}

#[test]
fn moving_an_element_writes_none_of_its_bytes() {
    let storage = MemoryStorage::new();
    let big: PersistableString = "x".repeat(20_000).into();
    let mut k = Kladde::create_in(
        Box::new(storage.clone()),
        PersistableVec::from(vec![big, "a".into(), "b".into()]),
        Options::default(),
    )
    .unwrap();
    let before = k.stats();
    k.guard().move_element(0, 2).unwrap();
    k.flush().unwrap();
    let after = k.stats();
    assert!(after.journal_bytes - before.journal_bytes < 64);
    assert_eq!(after.data_pages_written, before.data_pages_written);
    assert_eq!(k.get()[0], "a");
    check(&mut k, &storage, "after the move");
}

#[test]
fn a_dropped_cut_removes_its_value_for_good() {
    let storage = MemoryStorage::new();
    let mut k = Kladde::create_in(
        Box::new(storage.clone()),
        PersistableVec::<PersistableString>::from(vec!["a".into(), "b".into()]),
        Options::default(),
    )
    .unwrap();
    {
        let mut g = k.guard();
        let cut = g.cut(0).unwrap();
        assert_eq!(*cut, "a");
    }
    assert_eq!(k.get().as_slice(), ["b"]);
    assert!(!k.store().is_poisoned());
    check(&mut k, &storage, "after the dropped cut");
}
