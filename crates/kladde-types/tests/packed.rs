//! Packed vectors under random edits: elements whose packed encodings change
//! size through every path a guard offers -- a whole-value `set`, a field's
//! accessor, `parts()` with siblings alive, an enum switching variant inside
//! a struct inside a tuple -- checked against a plain model after every edit,
//! and against a reopened file after every few.

use kladde::{Kladde, MemoryStorage, Options, Persistable};
use kladde_types::{PackedPersistableVec, PersistableString};

#[derive(Persistable, Debug, Clone, PartialEq)]
enum Shape {
    Dot,
    Line {
        len: u32,
    },
    Poly {
        sides: u16,
        offset: i64,
        closed: bool,
    },
}

#[derive(Persistable, Debug)]
struct Item {
    id: u32,
    shape: Shape,
    name: PersistableString,
    pair: (u16, Shape),
    inner: PackedPersistableVec<Shape>,
}

/// What an `Item` holds, as plain values.
#[derive(Debug, Clone, PartialEq)]
struct Model {
    id: u32,
    shape: Shape,
    name: String,
    pair: (u16, Shape),
    inner: Vec<Shape>,
}

impl Model {
    fn of(item: &Item) -> Model {
        Model {
            id: item.id,
            shape: item.shape.clone(),
            name: item.name.to_string(),
            pair: item.pair.clone(),
            inner: item.inner.to_vec(),
        }
    }

    fn build(&self) -> Item {
        Item {
            id: self.id,
            shape: self.shape.clone(),
            name: PersistableString::from(self.name.as_str()),
            pair: self.pair.clone(),
            inner: self.inner.iter().cloned().collect(),
        }
    }
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

    /// A number whose varint takes anywhere from one byte to five.
    fn wide(&mut self) -> u32 {
        let bits = self.below(33) as u32;
        (self.next() as u32) >> (32 - bits.max(1))
    }

    fn shape(&mut self) -> Shape {
        match self.below(3) {
            0 => Shape::Dot,
            1 => Shape::Line { len: self.wide() },
            _ => Shape::Poly {
                sides: self.wide() as u16,
                offset: self.next() as i64 >> self.below(64),
                closed: self.below(2) == 0,
            },
        }
    }

    fn model(&mut self) -> Model {
        let inner = (0..self.below(4)).map(|_| self.shape()).collect();
        Model {
            id: self.wide(),
            shape: self.shape(),
            name: "x".repeat(self.below(5) as usize),
            pair: (self.wide() as u16, self.shape()),
            inner,
        }
    }
}

fn reopened(storage: &MemoryStorage) -> Kladde<PackedPersistableVec<Item>> {
    let image = MemoryStorage::from_image(storage.image());
    Kladde::open_in(Box::new(image), Options::default()).unwrap()
}

fn snapshot(items: &PackedPersistableVec<Item>) -> Vec<Model> {
    items.iter().map(Model::of).collect()
}

#[test]
fn random_edits_keep_every_offset_right() {
    let storage = MemoryStorage::new();
    let mut db = Kladde::create_in(
        Box::new(storage.clone()),
        PackedPersistableVec::<Item>::new(),
        Options::default(),
    )
    .unwrap();
    let mut model: Vec<Model> = Vec::new();
    let mut rng = Rng(0x5eed_1234_abcd_0001);

    for step in 0..3000 {
        let len = model.len() as u64;
        let op = if len == 0 { 0 } else { rng.below(12) };
        let i = if len == 0 { 0 } else { rng.below(len) as usize };
        let mut g = db.guard();
        match op {
            0 | 1 => {
                let m = rng.model();
                let at = rng.below(len + 1) as usize;
                g.insert(at, m.build()).unwrap();
                model.insert(at, m);
            }
            2 if len > 8 => {
                g.delete(i).unwrap();
                model.remove(i);
            }
            2 => {
                let mut removed = g.remove(i).unwrap();
                let m = model.remove(i);
                // Moved to the end without copying its allocations.
                removed.id = removed.id.wrapping_add(1);
                g.push(removed).unwrap();
                model.push(Model {
                    id: m.id.wrapping_add(1),
                    ..m
                });
            }
            3 => {
                let id = rng.wide();
                g.get_mut(i).unwrap().id_mut().set(id).unwrap();
                model[i].id = id;
            }
            4 => {
                let shape = rng.shape();
                g.get_mut(i)
                    .unwrap()
                    .shape_mut()
                    .set(shape.clone())
                    .unwrap();
                model[i].shape = shape;
            }
            5 => {
                // Every field at once: each one finds itself after the ones
                // before it changed size.
                let (id, shape, first, second) =
                    (rng.wide(), rng.shape(), rng.wide() as u16, rng.shape());
                let mut item = g.get_mut(i).unwrap();
                let mut parts = item.parts();
                parts.shape.set(shape.clone()).unwrap();
                let (mut a, mut b) = parts.pair.parts();
                b.set(second.clone()).unwrap();
                parts.id.set(id).unwrap();
                a.set(first).unwrap();
                parts.name.push_str("y").unwrap();
                model[i].id = id;
                model[i].shape = shape;
                model[i].pair = (first, second);
                model[i].name.push('y');
            }
            6 => {
                // A field inside an enum's variant.
                let value = rng.wide();
                let mut item = g.get_mut(i).unwrap();
                match item.shape_mut().parts() {
                    ShapeParts::Line { mut len } => {
                        len.set(value).unwrap();
                        model[i].shape = Shape::Line { len: value };
                    }
                    ShapeParts::Poly { mut sides, .. } => {
                        sides.set(value as u16).unwrap();
                        if let Shape::Poly { sides, .. } = &mut model[i].shape {
                            *sides = value as u16;
                        }
                    }
                    ShapeParts::Dot => {}
                }
            }
            7 => {
                let m = rng.model();
                g.get_mut(i).unwrap().set(m.build()).unwrap();
                model[i] = m;
            }
            8 => {
                let shape = rng.shape();
                g.get_mut(i)
                    .unwrap()
                    .inner_mut()
                    .push(shape.clone())
                    .unwrap();
                model[i].inner.push(shape);
            }
            9 if !model[i].inner.is_empty() => {
                let j = rng.below(model[i].inner.len() as u64) as usize;
                let shape = rng.shape();
                let mut item = g.get_mut(i).unwrap();
                let mut inner = item.inner_mut();
                inner.get_mut(j).unwrap().set(shape.clone()).unwrap();
                model[i].inner[j] = shape;
            }
            10 => {
                let first = rng.wide() as u16;
                let mut item = g.get_mut(i).unwrap();
                let mut pair = item.pair_mut();
                let (mut a, _) = pair.parts();
                a.set(first).unwrap();
                model[i].pair.0 = first;
            }
            _ => {
                if g.pop().unwrap().is_some() {
                    model.pop();
                }
            }
        }
        assert_eq!(snapshot(db.get()), model, "in memory, after step {step}");
        if step % 97 == 0 {
            assert_eq!(
                snapshot(reopened(&storage).get()),
                model,
                "on file, after step {step}"
            );
        }
        if step % 500 == 0 {
            db.flush().unwrap();
        }
    }
    db.flush().unwrap();
    db.store().check();
    let content: usize = model.len();
    assert_eq!(snapshot(reopened(&storage).get()), model);
    assert_eq!(db.get().len(), content);
}
