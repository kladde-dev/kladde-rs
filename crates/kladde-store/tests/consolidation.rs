//! Consolidation keeps a file near its live size under sustained overwrites,
//! without changing what it reads.

use kladde_store::{MemoryStorage, Options, Pointer, Stats, Store, WriteBackend};

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
}

/// Overwrites random ranges of `n` allocations of `size` bytes, flushing
/// every `per_flush` writes, and checks every byte at the end.
fn overwrite(
    opts: Options,
    n: usize,
    size: usize,
    write: usize,
    per_flush: usize,
    flushes: usize,
) -> Stats {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let storage = MemoryStorage::new();
    let store = Store::create(Box::new(storage.clone()), opts.clone()).unwrap();
    let mut model: Vec<Vec<u8>> = vec![vec![0; size]; n];
    let ptrs: Vec<Pointer> = (0..n)
        .map(|_| store.alloc(size as u32).unwrap().raw())
        .collect();
    for f in 0..flushes {
        for _ in 0..per_flush {
            let i = rng.below(n as u64) as usize;
            let len = (rng.below(write as u64) + 1) as usize;
            let off = rng.below((size - len) as u64 + 1) as usize;
            let fill = (f % 251 + 1) as u8;
            model[i][off..off + len].fill(fill);
            store
                .write(ptrs[i], off as u32, &model[i][off..off + len])
                .unwrap();
        }
        store.flush().unwrap();
    }
    store.check();
    for (i, p) in ptrs.iter().enumerate() {
        assert!(
            store.read_all(*p).unwrap() == model[i],
            "allocation {i} differs"
        );
    }
    let stats = store.stats();
    drop(store);
    let store = Store::open(Box::new(MemoryStorage::from_image(storage.image())), opts).unwrap();
    for (i, p) in ptrs.iter().enumerate() {
        assert!(
            store.read_all(*p).unwrap() == model[i],
            "allocation {i} differs after reopening"
        );
    }
    stats
}

fn data_fill(s: &Stats) -> f64 {
    s.live_data_bytes as f64 / (s.data_pages as f64 * kladde_store::MAX_PAGE_CONTENT as f64)
}

#[test]
fn evacuation_keeps_data_pages_full() {
    let off = Options {
        consolidate: false,
        ..Default::default()
    };
    let without = overwrite(off, 64, 8192, 600, 32, 300);
    let with = overwrite(Options::default(), 64, 8192, 600, 32, 300);
    assert!(with.evacuated_pages > 0);
    assert!(
        data_fill(&without) < 0.3,
        "data fill {}",
        data_fill(&without)
    );
    assert!(data_fill(&with) > 0.6, "data fill {}", data_fill(&with));
    assert!(with.data_pages * 3 < without.data_pages);
}
