//! Differential tests: random mutations against a naive model of byte
//! vectors, with flushes and reopenings in between.

use std::collections::HashMap;

use kladde_store::{Backend, MemoryStorage, Options, Pointer, Store, UniquePointer, WriteBackend};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn chance(&mut self, p: f64) -> bool {
        (self.next() % 1_000_000) as f64 / 1_000_000.0 < p
    }
}

struct World {
    store: Store,
    storage: MemoryStorage,
    model: HashMap<u32, Vec<u8>>,
    owned: HashMap<u32, UniquePointer>,
    opts: Options,
}

impl World {
    fn new(opts: Options) -> World {
        let storage = MemoryStorage::new();
        let store = Store::create(Box::new(storage.clone()), opts.clone()).unwrap();
        World {
            store,
            storage,
            model: HashMap::new(),
            owned: HashMap::new(),
            opts,
        }
    }

    fn reopen(&mut self) {
        let image = self.storage.image();
        let storage = MemoryStorage::from_image(image);
        self.store = Store::open(Box::new(storage.clone()), self.opts.clone()).unwrap();
        self.storage = storage;
        self.owned = self
            .model
            .keys()
            .map(|&id| {
                (
                    id,
                    UniquePointer::from_pointer(Pointer::from_raw(id).unwrap()),
                )
            })
            .collect();
    }

    fn verify(&mut self) {
        self.store.flush().unwrap();
        self.store.check();
        for (&id, want) in &self.model {
            let p = Pointer::from_raw(id).unwrap();
            assert_eq!(
                self.store.size(p).unwrap() as usize,
                want.len(),
                "size of {id}"
            );
            let got = self.store.read_all(p).unwrap();
            assert!(
                got == *want,
                "content of {id}: got {:?}.. want {:?}..",
                &got[..got.len().min(16)],
                &want[..want.len().min(16)]
            );
        }
        let stats = self.store.stats();
        assert_eq!(stats.allocations as usize, self.model.len());
    }

    fn step(&mut self, rng: &mut Rng, max_size: u64) {
        let ids: Vec<u32> = self.model.keys().copied().collect();
        let pick = |rng: &mut Rng| ids[rng.below(ids.len() as u64) as usize];
        match rng.below(10) {
            0 | 1 if ids.len() < 200 => {
                let size = rng.below(max_size) as u32;
                let p = self.store.alloc(size).unwrap();
                let id = p.raw().raw();
                assert!(!self.model.contains_key(&id));
                self.model.insert(id, vec![0; size as usize]);
                self.owned.insert(id, p);
            }
            2 if !ids.is_empty() => {
                let id = pick(rng);
                let p = self.owned.remove(&id).unwrap();
                self.store.free(p).unwrap();
                self.model.remove(&id);
            }
            3 if !ids.is_empty() => {
                let id = pick(rng);
                let size = rng.below(max_size) as u32;
                self.store.resize(&self.owned[&id], size).unwrap();
                self.model.get_mut(&id).unwrap().resize(size as usize, 0);
            }
            4..=6 if !ids.is_empty() => {
                let id = pick(rng);
                let v = self.model.get_mut(&id).unwrap();
                let off = rng.below(v.len() as u64 + 8) as u32;
                let cap = if rng.chance(0.2) { max_size } else { 64 };
                let len = rng.below(cap) as usize;
                let fill = (rng.below(255) + 1) as u8;
                let bytes: Vec<u8> = (0..len).map(|i| fill.wrapping_add(i as u8)).collect();
                self.store
                    .write(Pointer::from_raw(id).unwrap(), off, &bytes)
                    .unwrap();
                let end = off as usize + len;
                if v.len() < end {
                    v.resize(end, 0);
                }
                v[off as usize..end].copy_from_slice(&bytes);
            }
            7 if !ids.is_empty() => {
                let id = pick(rng);
                let v = self.model.get_mut(&id).unwrap();
                let off = rng.below(v.len() as u64 + 1) as u32;
                let old = rng.below(16) as u32;
                let len = rng.below(24) as usize;
                let bytes: Vec<u8> = (0..len).map(|i| 200u8.wrapping_add(i as u8)).collect();
                self.store
                    .splice(&self.owned[&id], off, old, &bytes)
                    .unwrap();
                let end = (off + old) as usize;
                if v.len() < end {
                    v.resize(end, 0);
                }
                v.splice(off as usize..end, bytes);
            }
            8 | 9 if !ids.is_empty() => {
                let src = pick(rng);
                let dst = pick(rng);
                let slen = self.model[&src].len() as u64;
                let so = rng.below(slen + 1) as u32;
                let len = rng.below(64) as u32;
                let doff = rng.below(self.model[&dst].len() as u64 + 1) as u32;
                let is_move = rng.chance(0.5);
                let from = self.model[&src].clone();
                let bytes: Vec<u8> = (0..len as usize)
                    .map(|i| from.get(so as usize + i).copied().unwrap_or(0))
                    .collect();
                let (sp, dp) = (
                    Pointer::from_raw(src).unwrap(),
                    Pointer::from_raw(dst).unwrap(),
                );
                if is_move {
                    self.store.move_range(sp, so, len, dp, doff).unwrap();
                } else {
                    self.store.copy(sp, so, len, dp, doff).unwrap();
                }
                let v = self.model.get_mut(&dst).unwrap();
                let end = (doff + len) as usize;
                if v.len() < end {
                    v.resize(end, 0);
                }
                v[doff as usize..end].copy_from_slice(&bytes);
                if is_move {
                    let sv = self.model.get_mut(&src).unwrap();
                    for i in so as usize..(so + len) as usize {
                        let in_dst = src == dst && i >= doff as usize && i < end;
                        if i < sv.len() && !in_dst {
                            sv[i] = 0;
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn run(seed: u64, steps: usize, max_size: u64, opts: Options) {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut w = World::new(opts);
    for step in 0..steps {
        w.step(&mut rng, max_size);
        if rng.chance(0.05) {
            w.verify();
        }
        if rng.chance(0.01) {
            w.verify();
            w.reopen();
            w.verify();
        }
        let _ = step;
    }
    w.verify();
    w.reopen();
    w.verify();
}

#[test]
fn small_allocations_without_consolidation() {
    let opts = Options {
        consolidate: false,
        ..Default::default()
    };
    for seed in 1..40 {
        run(seed, 400, 200, opts.clone());
    }
}

#[test]
fn large_allocations_without_consolidation() {
    let opts = Options {
        consolidate: false,
        ..Default::default()
    };
    for seed in 1..15 {
        run(seed, 300, 20_000, opts.clone());
    }
}

#[test]
fn small_allocations() {
    for seed in 1..40 {
        run(seed, 400, 200, Options::default());
    }
}

#[test]
fn large_allocations() {
    for seed in 1..15 {
        run(seed, 300, 20_000, Options::default());
    }
}

#[test]
fn a_tiny_journal_budget_flushes_often() {
    let opts = Options {
        journal_budget_pages: 1,
        ..Default::default()
    };
    for seed in 1..10 {
        run(seed, 500, 3000, opts.clone());
    }
}
