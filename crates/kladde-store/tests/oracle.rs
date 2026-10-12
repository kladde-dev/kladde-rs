//! Differential tests: random writes, splices, moves, and cuts pasted within
//! transactions, against a plain byte vector, with flushes, reopenings and
//! new schema chains in between.

use kladde_store::{MemoryStorage, Options, Store, WriteBackend};

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
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        let fill = self.below(255) as u8 + 1;
        (0..n).map(|i| fill.wrapping_add((i % 7) as u8)).collect()
    }
}

struct World {
    store: Store,
    storage: MemoryStorage,
    model: Vec<u8>,
    schema: Vec<u8>,
    opts: Options,
}

impl World {
    fn new(opts: Options) -> World {
        let storage = MemoryStorage::new();
        let store = Store::create(Box::new(storage.clone()), opts.clone()).unwrap();
        World {
            store,
            storage,
            model: Vec::new(),
            schema: Vec::new(),
            opts,
        }
    }

    fn verify(&self, what: &str) {
        self.store.check();
        let got = self.store.read_all().unwrap();
        if got != self.model {
            let first = got
                .iter()
                .zip(&self.model)
                .position(|(a, b)| a != b)
                .unwrap_or(got.len().min(self.model.len()));
            panic!(
                "{what}: the sequence differs at {first} (lengths {} and {})\n{}",
                got.len(),
                self.model.len(),
                self.store.describe()
            );
        }
        assert_eq!(self.store.schema(), self.schema, "{what}: the schema");
    }

    fn reopen(&mut self) {
        let image = self.storage.image();
        let storage = MemoryStorage::from_image(image);
        self.store = Store::open(Box::new(storage.clone()), self.opts.clone()).unwrap();
        self.storage = storage;
        assert_eq!(self.store.len(), self.model.len() as u64);
    }

    /// One random operation, applied to the store and the model alike.
    fn step(&mut self, rng: &mut Rng, max_len: u64) {
        let len = self.model.len() as u64;
        let store = &self.store;
        match rng.below(12) {
            0..=2 if len > 0 => {
                let pos = rng.below(len);
                let n = rng.below((len - pos).min(600)) + 1;
                let bytes = rng.bytes(n as usize);
                self.model[pos as usize..(pos + n) as usize].copy_from_slice(&bytes);
                store.write(pos, &bytes).unwrap();
            }
            3..=6 => {
                // Mostly keystrokes and short deletions near one place, now
                // and then a long run, and the sequence kept below `max_len`.
                let pos = rng.below(len + 1);
                let removed = match rng.below(4) {
                    0 => rng.below((len - pos).min(3000) + 1),
                    _ => rng.below((len - pos).min(3) + 1),
                };
                let n = match rng.below(8) {
                    0 => rng.below(9000),
                    1..=3 => rng.below(40),
                    _ => 1,
                };
                let n = if len - removed + n > max_len { 0 } else { n };
                let bytes = rng.bytes(n as usize);
                self.model.splice(
                    pos as usize..(pos + removed) as usize,
                    bytes.iter().copied(),
                );
                store.splice(pos, removed, &bytes).unwrap();
            }
            7 if len > 0 => {
                let from = rng.below(len);
                let n = rng.below((len - from).min(5000)) + 1;
                let to = rng.below(len - n + 1);
                let m: Vec<u8> = self
                    .model
                    .drain(from as usize..(from + n) as usize)
                    .collect();
                self.model.splice(to as usize..to as usize, m);
                store.move_range(from, n, to).unwrap();
            }
            8 if len > 1 => {
                // Two cuts in one transaction, pasted in the other order, the
                // second over bytes it replaces.
                let model = &mut self.model;
                store
                    .atomically(|| {
                        let len = model.len() as u64;
                        let a = rng.below(len);
                        let an = rng.below((len - a).min(2000)) + 1;
                        let ca: Vec<u8> = model.drain(a as usize..(a + an) as usize).collect();
                        let cut_a = store.splice(a, an, b"")?.unwrap();
                        let len = model.len() as u64;
                        let b = rng.below(len + 1);
                        let bn = rng.below((len - b).min(2000) + 1);
                        let cb: Vec<u8> = model.drain(b as usize..(b + bn) as usize).collect();
                        let cut_b = store.splice(b, bn, b"x")?;
                        model.insert(b as usize, b'x');
                        let len = model.len() as u64;
                        let p = rng.below(len + 1);
                        model.splice(p as usize..p as usize, ca);
                        store.paste(p, 0, cut_a)?;
                        if let Some(cut_b) = cut_b {
                            let len = model.len() as u64;
                            let q = rng.below(len + 1);
                            let r = rng.below((len - q).min(10) + 1);
                            model.splice(q as usize..(q + r) as usize, cb);
                            store.paste(q, r, cut_b)?;
                        }
                        Ok(())
                    })
                    .unwrap();
            }
            9 => {
                let n = rng.below(400) as usize;
                self.schema = rng.bytes(n);
                store.set_schema(self.schema.clone());
            }
            10 => {
                store.flush().unwrap();
                self.verify("after a flush");
            }
            _ => {
                if rng.below(3) == 0 {
                    store.flush().unwrap();
                    self.reopen();
                    self.verify("after reopening");
                }
            }
        }
    }
}

fn run(seed: u64, opts: Options, steps: usize, max_len: u64) {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut w = World::new(opts);
    for _ in 0..steps {
        w.step(&mut rng, max_len);
    }
    w.store.flush().unwrap();
    w.verify(&format!("seed {seed}, at the end"));
    w.reopen();
    w.verify(&format!("seed {seed}, reopened at the end"));
}

#[test]
fn short_sequences() {
    for seed in 1..60 {
        run(seed, Options::default(), 300, 20_000);
    }
}

#[test]
fn long_sequences() {
    for seed in 1..12 {
        run(seed, Options::default(), 600, 400_000);
    }
}

#[test]
fn without_consolidation() {
    let opts = Options {
        consolidate: false,
        ..Default::default()
    };
    for seed in 1..30 {
        run(seed, opts.clone(), 300, 50_000);
    }
}

#[test]
fn with_a_tiny_journal() {
    let opts = Options {
        journal_budget_pages: 1,
        ..Default::default()
    };
    for seed in 1..30 {
        run(seed, opts.clone(), 300, 50_000);
    }
}

#[test]
fn with_eager_merges_and_a_tiny_header() {
    // Every flush walks the whole sequence, everything is cold at once, and
    // short spans merge readily.
    let opts = Options {
        cold_flushes: 0,
        walk: 100_000,
        span_min: 4000,
        mu: 0.0,
        rewrite_below: 0,
        inline_threshold: 1,
        ..Default::default()
    };
    for seed in 1..30 {
        run(seed, opts.clone(), 300, 50_000);
    }
}

#[test]
fn a_cut_from_another_transaction_is_refused() {
    let store = Store::create(Box::new(MemoryStorage::new()), Options::default()).unwrap();
    store.splice(0, 0, b"abcdef").unwrap();
    store.begin_transaction().unwrap();
    let cut = store.splice(0, 2, b"").unwrap().unwrap();
    store.end_transaction().unwrap();
    store.begin_transaction().unwrap();
    assert!(store.paste(0, 0, cut).is_err());
    store.end_transaction().unwrap();
    // Outside a transaction, a splice makes no cut.
    assert!(store.splice(0, 1, b"").unwrap().is_none());
    store.flush().unwrap();
    assert_eq!(store.read_all().unwrap(), b"def");
}

#[test]
fn a_failed_transaction_leaves_nothing() {
    let store = Store::create(Box::new(MemoryStorage::new()), Options::default()).unwrap();
    store.splice(0, 0, b"abc").unwrap();
    let r: Result<(), _> = store.atomically(|| {
        store.splice(0, 1, b"xyz")?;
        store.write(100, b"past the end")
    });
    assert!(r.is_err());
    assert_eq!(store.len(), 3);
    store.flush().unwrap();
    assert_eq!(store.read_all().unwrap(), b"abc");
}

#[test]
fn a_splice_states_a_few_spans_however_long_the_sequence() {
    // Without consolidation, whose compaction mode would move the file's
    // tail down over the pages the first megabyte's journal left free.
    let opts = Options {
        consolidate: false,
        ..Default::default()
    };
    let store = Store::create(Box::new(MemoryStorage::new()), opts).unwrap();
    store.splice(0, 0, &vec![7u8; 1 << 20]).unwrap();
    store.flush().unwrap();
    let before = store.stats();
    store
        .splice(1000, 0, b"a long sequence, a short splice")
        .unwrap();
    store.flush().unwrap();
    let after = store.stats();
    // The span the splice lands in is split, and the new bytes are inline:
    // no data page and no leaf is written, only the header.
    assert_eq!(after.span_splits, before.span_splits + 1);
    assert_eq!(after.data_pages_written, before.data_pages_written);
    assert_eq!(after.table_pages_written, before.table_pages_written);
    assert_eq!(after.headers_written, before.headers_written + 1);
    store.check();
}
