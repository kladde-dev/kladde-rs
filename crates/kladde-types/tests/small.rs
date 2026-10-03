//! Small values under random edits: small strings inside structs inside
//! small vectors inside a packed vector, so that an edit deep down can make
//! every small value around it rewrite its tag, spill into an allocation of
//! its own, or fold back inline, while guards of its siblings are alive. The
//! values are checked against a plain model after every edit, and against a
//! reopened file after every few. Also: slotted fields, packed pointers, and
//! a packed root.

use kladde::{Kladde, MemoryStorage, Options, Persistable};
use kladde_types::{
    PackedPersistableVec, PersistableVec, SmallPersistableString, SmallPersistableVec, FOLD_BELOW,
    SPILL_ABOVE,
};

#[derive(Persistable, Debug)]
#[kladde(packed_only)]
struct Attr {
    name: SmallPersistableString,
    value: SmallPersistableString,
    #[kladde(slotted)]
    count: u32,
}

#[derive(Persistable, Debug)]
#[kladde(packed_only)]
struct Element {
    id: SmallPersistableString,
    attrs: SmallPersistableVec<Attr>,
    points: PersistableVec<u16>,
}

type Model = Vec<(String, Vec<(String, String, u32)>, Vec<u16>)>;

fn snapshot(elements: &PackedPersistableVec<Element>) -> Model {
    elements
        .iter()
        .map(|e| {
            let attrs = e
                .attrs
                .iter()
                .map(|a| (a.name.to_string(), a.value.to_string(), a.count))
                .collect();
            (e.id.to_string(), attrs, e.points.to_vec())
        })
        .collect()
}

/// xorshift64*, seeded: the same edits on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// Text of a length that is mostly short, and sometimes long enough to
    /// cross a threshold on its own.
    fn text(&mut self) -> String {
        let len = match self.below(10) {
            0 => self.below(300),
            1..=3 => self.below(60),
            _ => self.below(8),
        };
        (0..len).map(|i| (b'a' + (i % 26) as u8) as char).collect()
    }
}

fn reopened(storage: &MemoryStorage) -> Kladde<PackedPersistableVec<Element>> {
    let image = MemoryStorage::from_image(storage.image());
    Kladde::open_in(Box::new(image), Options::default()).unwrap()
}

#[test]
fn random_edits_of_nested_small_values() {
    let storage = MemoryStorage::new();
    let mut db = Kladde::create_in(
        Box::new(storage.clone()),
        PackedPersistableVec::<Element>::new(),
        Options::default(),
    )
    .unwrap();
    let mut model: Model = Vec::new();
    let mut rng = Rng(0x0dd_ba11_5eed_0002);
    let (mut spilled, mut folded) = (0, 0);

    for step in 0..4000 {
        let len = model.len() as u64;
        let op = if len == 0 { 0 } else { rng.below(10) };
        let i = if len == 0 { 0 } else { rng.below(len) as usize };
        let attrs_len = model.get(i).map_or(0, |m| m.1.len() as u64);
        let j = if attrs_len == 0 {
            0
        } else {
            rng.below(attrs_len) as usize
        };
        let was_inline = db.get().get(i).map(|e| e.attrs.is_inline());
        let mut g = db.guard();
        match op {
            0 => {
                let id = rng.text();
                g.push(Element {
                    id: SmallPersistableString::from(id.as_str()),
                    attrs: SmallPersistableVec::new(),
                    points: PersistableVec::new(),
                })
                .unwrap();
                model.push((id, Vec::new(), Vec::new()));
            }
            1 => {
                let (name, value) = (rng.text(), rng.text());
                let mut element = g.get_mut(i).unwrap();
                element
                    .attrs_mut()
                    .push(Attr {
                        name: name.as_str().into(),
                        value: value.as_str().into(),
                        count: 0,
                    })
                    .unwrap();
                model[i].1.push((name, value, 0));
            }
            2 if attrs_len > 0 => {
                // Every part of an attribute at once, deep inside: the
                // value grows or shrinks, the slotted count moves with it.
                let (value, count) = (rng.text(), rng.next() as u32);
                let mut element = g.get_mut(i).unwrap();
                let mut attrs = element.attrs_mut();
                let mut attr = attrs.get_mut(j).unwrap();
                let mut parts = attr.parts();
                parts.value.set(value.as_str()).unwrap();
                parts.count.set(count).unwrap();
                parts.name.push_str("!").unwrap();
                model[i].1[j].1 = value;
                model[i].1[j].2 = count;
                model[i].1[j].0.push('!');
            }
            3 if attrs_len > 0 => {
                let mut element = g.get_mut(i).unwrap();
                element.attrs_mut().delete(j).unwrap();
                model[i].1.remove(j);
            }
            4 => {
                let id = rng.text();
                g.get_mut(i).unwrap().id_mut().set(id.as_str()).unwrap();
                model[i].0 = id;
            }
            5 => {
                // The id grows while the attributes' guards are alive.
                let mut element = g.get_mut(i).unwrap();
                let mut parts = element.parts();
                let extra = "x".repeat(rng.below(40) as usize);
                parts.id.push_str(&extra).unwrap();
                if let Some(mut attr) = parts.attrs.get_mut(0) {
                    attr.value_mut().push_str("y").unwrap();
                    model[i].1[0].1.push('y');
                }
                model[i].0.push_str(&extra);
            }
            6 => {
                // The packed pointer of a vector that gets its allocation.
                let point = rng.next() as u16;
                g.get_mut(i).unwrap().points_mut().push(point).unwrap();
                model[i].2.push(point);
            }
            7 if len > 3 => {
                g.delete(i).unwrap();
                model.remove(i);
            }
            8 if attrs_len > 0 => {
                let mut element = g.get_mut(i).unwrap();
                let mut attrs = element.attrs_mut();
                let attr = attrs.remove(j).unwrap();
                attrs.insert(0, attr).unwrap();
                let moved = model[i].1.remove(j);
                model[i].1.insert(0, moved);
            }
            _ => {
                let mut element = g.get_mut(i).unwrap();
                element.attrs_mut().clear().unwrap();
                model[i].1.clear();
            }
        }
        let is_inline = db.get().get(i).map(|e| e.attrs.is_inline());
        match (was_inline, is_inline) {
            (Some(true), Some(false)) => spilled += 1,
            (Some(false), Some(true)) => folded += 1,
            _ => {}
        }
        assert_eq!(snapshot(db.get()), model, "in memory, after step {step}");
        for element in db.get().iter() {
            let content = element.attrs.content_size();
            if element.attrs.is_inline() {
                assert!(content <= SPILL_ABOVE, "inline content of {content} bytes");
            } else {
                assert!(content >= FOLD_BELOW, "spilled content of {content} bytes");
            }
        }
        if step % 89 == 0 {
            assert_eq!(
                snapshot(reopened(&storage).get()),
                model,
                "on file, after step {step}"
            );
        }
        if step % 700 == 0 {
            db.flush().unwrap();
        }
    }
    db.flush().unwrap();
    db.store().check();
    assert_eq!(snapshot(reopened(&storage).get()), model);
    assert!(
        spilled > 10 && folded > 10,
        "spilled {spilled}, folded {folded}"
    );
}

#[test]
fn a_nested_edit_spills_and_folds_the_vector_around_it() {
    let storage = MemoryStorage::new();
    let mut db = Kladde::create_in(
        Box::new(storage.clone()),
        PackedPersistableVec::<Element>::new(),
        Options::default(),
    )
    .unwrap();
    db.guard()
        .push(Element {
            id: "e".into(),
            attrs: ["a", "b"]
                .into_iter()
                .map(|name| Attr {
                    name: name.into(),
                    value: "short".into(),
                    count: 1,
                })
                .collect(),
            points: PersistableVec::new(),
        })
        .unwrap();
    let check = |db: &Kladde<PackedPersistableVec<Element>>, values: [&str; 2], inline: bool| {
        for element in [&db.get()[0], &reopened(&storage).get()[0]] {
            assert_eq!(element.attrs[0].value, values[0]);
            assert_eq!(element.attrs[1].value, values[1]);
            assert_eq!(element.attrs.is_inline(), inline);
            assert_eq!(element.attrs[1].count, 2);
        }
    };
    // Each attribute takes 77 bytes with a value of 70, still inline itself,
    // so the second one to grow takes the list past 128.
    let long = "v".repeat(70);
    {
        let mut g = db.guard();
        let mut element = g.get_mut(0).unwrap();
        let mut attrs = element.attrs_mut();
        attrs
            .get_mut(0)
            .unwrap()
            .value_mut()
            .set(long.as_str())
            .unwrap();
        let mut attr = attrs.get_mut(1).unwrap();
        attr.value_mut().set(long.as_str()).unwrap();
        // The same guard keeps working once its list has spilled.
        attr.count_mut().set(2).unwrap();
    }
    check(&db, [&long, &long], false);
    let mut shrink = |index: usize| {
        db.guard()
            .get_mut(0)
            .unwrap()
            .attrs_mut()
            .get_mut(index)
            .unwrap()
            .value_mut()
            .set("tiny")
            .unwrap();
    };
    shrink(0); // 88 bytes: between the thresholds, so it stays spilled
    check(&db, ["tiny", &long], false);
    let mut shrink = |index: usize| {
        db.guard()
            .get_mut(0)
            .unwrap()
            .attrs_mut()
            .get_mut(index)
            .unwrap()
            .value_mut()
            .set("tiny")
            .unwrap();
    };
    shrink(1); // 22 bytes: below 64, so it folds back inline
    check(&db, ["tiny", "tiny"], true);
}

#[test]
fn a_slotted_field_keeps_its_slot_in_a_packed_value() {
    let attr = Attr {
        name: "a".into(),
        value: "b".into(),
        count: 7,
    };
    // Two one-byte tags with a byte of text each, and four bytes of count.
    assert_eq!(
        attr.to_bytes::<kladde::Packed>(),
        [1, b'a', 1, b'b', 7, 0, 0, 0]
    );
    let table = Attr::schema();
    assert!(table
        .descriptors()
        .iter()
        .any(|d| matches!(d, kladde::TypeDescriptor::Slotted(_))));
    assert_eq!(table.validate(), Ok(()));
    assert!(!table.is_slottable(table.root()));
}

#[test]
fn a_packed_only_root_is_packed() {
    let storage = MemoryStorage::new();
    let mut db = Kladde::create_in(
        Box::new(storage.clone()),
        SmallPersistableVec::<SmallPersistableString>::new(),
        Options::default(),
    )
    .unwrap();
    for i in 0..40 {
        db.guard().push(format!("item {i}").into()).unwrap();
    }
    db.guard().get_mut(3).unwrap().set("three").unwrap();
    assert!(!db.get().is_inline());
    for _ in 0..35 {
        db.guard().pop().unwrap();
    }
    assert!(db.get().is_inline());
    let image = MemoryStorage::from_image(storage.image());
    let reopened = Kladde::<SmallPersistableVec<SmallPersistableString>>::open_in(
        Box::new(image),
        Options::default(),
    )
    .unwrap();
    let texts: Vec<&str> = reopened.get().iter().map(|s| &**s).collect();
    assert_eq!(texts, ["item 0", "item 1", "item 2", "three", "item 4"]);
}
