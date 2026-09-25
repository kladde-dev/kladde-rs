//! Replacing values -- allocating a new one and freeing the old -- churns ids
//! through recycling: an id is re-minted soon after its tombstone was stated.
//! Every flush must reopen to the same content.
//!
//! This caught an id re-allocated in the flush that rewrote the page holding
//! its tombstone: the tombstone was released as "the last mention", the new
//! incarnation had nothing to anchor on, and the old incarnation's content
//! resurfaced after reopening.

use kladde_store::{MemoryStorage, Options, Store, UniquePointer, WriteBackend};

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
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }
    fn chance(&mut self, p: f64) -> bool {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }
    fn bytes(&mut self, len: usize) -> Vec<u8> {
        let seed = self.next();
        (0..len)
            .map(|i| (seed.wrapping_add(i as u64 * 0x9E37) >> 7) as u8)
            .collect()
    }
}

fn check(
    store: &Store,
    values: &[(UniquePointer, Vec<u8>)],
    storage: &MemoryStorage,
    opts: &Options,
    when: &str,
) {
    for (p, v) in values {
        let got = store.read_all(p.raw()).unwrap();
        assert!(
            got == *v,
            "{when}: allocation {} differs in memory",
            p.raw().raw()
        );
    }
    let reopened = Store::open(
        Box::new(MemoryStorage::from_image(storage.image())),
        opts.clone(),
    )
    .unwrap();
    reopened.check();
    for (p, v) in values {
        let got = reopened.read_all(p.raw()).unwrap();
        if got != *v {
            let first = got.iter().zip(v).position(|(a, b)| a != b);
            panic!(
                "{when}: allocation {} differs after reopening: len {} vs {}, first difference at {:?}\nin memory:\n{}\nreopened:\n{}",
                p.raw().raw(),
                got.len(),
                v.len(),
                first,
                store.describe(p.raw()),
                reopened.describe(p.raw()),
            );
        }
    }
}

#[test]
fn recycled_ids_never_resurrect_old_content() {
    for consolidate in [false, true] {
        let opts = Options {
            consolidate,
            ..Default::default()
        };
        let storage = MemoryStorage::new();
        let store = Store::create(Box::new(storage.clone()), opts.clone()).unwrap();
        let size = 1u64 << 18;
        let mut rng = Rng(size ^ 11);
        rng.0 = rng.0.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let value_size = |rng: &mut Rng| {
            if rng.chance(0.8) {
                rng.range(16, 128)
            } else {
                rng.range(1024, 8192)
            }
        };
        let mut values: Vec<(UniquePointer, Vec<u8>)> = Vec::new();
        let mut live = 0u64;
        while live < size {
            let len = value_size(&mut rng);
            let p = store.alloc(len as u32).unwrap();
            let bytes = rng.bytes(len as usize);
            store.write(p.raw(), 0, &bytes).unwrap();
            live += len;
            values.push((p, bytes));
            if values.len() % 256 == 0 {
                store.flush().unwrap();
            }
        }
        store.flush().unwrap();
        check(&store, &values, &storage, &opts, "after setup");
        let mut ops = 0u64;
        let mut app = 0u64;
        let mut flushes = 0;
        while app < 8 * size {
            let i = rng.below(values.len() as u64) as usize;
            match rng.below(3) {
                0 => {
                    let len = value_size(&mut rng);
                    let bytes = rng.bytes(len as usize);
                    let p = store.alloc(len as u32).unwrap();
                    store.write(p.raw(), 0, &bytes).unwrap();
                    app += len;
                    let (old, _) = std::mem::replace(&mut values[i], (p, bytes));
                    store.free(old).unwrap();
                    ops += 3;
                }
                1 => {
                    let bytes = rng.bytes(values[i].1.len());
                    store.write(values[i].0.raw(), 0, &bytes).unwrap();
                    app += bytes.len() as u64;
                    values[i].1 = bytes;
                    ops += 1;
                }
                _ => {
                    let v = &values[i].1;
                    let len = rng.range(1, (v.len() as u64).min(64)) as usize;
                    let off = rng.below((v.len() - len) as u64 + 1) as usize;
                    let bytes = rng.bytes(len);
                    store.write(values[i].0.raw(), off as u32, &bytes).unwrap();
                    app += len as u64;
                    values[i].1[off..off + len].copy_from_slice(&bytes);
                    ops += 1;
                }
            }
            if ops >= 1000 {
                ops = 0;
                store.flush().unwrap();
                flushes += 1;
                check(
                    &store,
                    &values,
                    &storage,
                    &opts,
                    &format!("consolidate {consolidate} flush {flushes}"),
                );
            }
        }
    }
}
