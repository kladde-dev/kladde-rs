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

/// Appends `piece` bytes to each of `n` allocations in every flush, and
/// checks every byte at the end.
fn append(opts: Options, n: usize, piece: usize, flushes: usize) -> Stats {
    let storage = MemoryStorage::new();
    let store = Store::create(Box::new(storage.clone()), opts.clone()).unwrap();
    let ptrs: Vec<Pointer> = (0..n).map(|_| store.alloc(0).unwrap().raw()).collect();
    let mut model: Vec<Vec<u8>> = vec![Vec::new(); n];
    for f in 0..flushes {
        for (i, p) in ptrs.iter().enumerate() {
            let bytes: Vec<u8> = (0..piece).map(|j| (f * 7 + i * 3 + j) as u8).collect();
            store.write(*p, model[i].len() as u32, &bytes).unwrap();
            model[i].extend(bytes);
        }
        store.flush().unwrap();
    }
    store.check();
    let stats = store.stats();
    drop(store);
    let store = Store::open(Box::new(MemoryStorage::from_image(storage.image())), opts).unwrap();
    for (i, p) in ptrs.iter().enumerate() {
        assert!(
            store.read_all(*p).unwrap() == model[i],
            "allocation {i} differs"
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

#[test]
fn exact_ranking_keeps_data_pages_full_too() {
    let exact = Options {
        exact_ranking: true,
        ..Default::default()
    };
    let with = overwrite(exact, 64, 8192, 600, 32, 300);
    assert!(with.evacuated_pages > 0);
    assert!(data_fill(&with) > 0.6, "data fill {}", data_fill(&with));
}

#[test]
fn page_rewrites_keep_leaves_full() {
    let off = Options {
        consolidate: false,
        ..Default::default()
    };
    let without = overwrite(off, 64, 8192, 600, 32, 300);
    let with = overwrite(Options::default(), 64, 8192, 600, 32, 300);
    println!("without {without:#?}\nwith {with:#?}");
    assert!(with.table_rewrites > 0 && with.window_restated > 0);
    assert!(
        with.live_fraction() > 0.6,
        "live fraction {}",
        with.live_fraction()
    );
    assert!(with.table_pages * 3 < without.table_pages);
}

/// Writes `n` allocations of a page each, frees all but every fourth, then
/// runs `flushes` flushes, each of `patches` small writes into what is left
/// and one to a note. Returns the file's length in pages before and after.
fn shrink(opts: Options, n: usize, flushes: usize, patches: usize) -> (u64, u64) {
    use kladde_store::Storage;
    let storage = MemoryStorage::new();
    let store = Store::create(Box::new(storage.clone()), opts.clone()).unwrap();
    let fill = |i: usize| (i % 251) as u8 + 1;
    let mut owned: Vec<_> = (0..n)
        .map(|i| {
            let p = store.alloc(4000).unwrap();
            store.write(p.raw(), 0, &[fill(i); 4000]).unwrap();
            p
        })
        .collect();
    store.flush().unwrap();
    let before = storage.len().unwrap() / 4096;
    let mut kept = Vec::new();
    for (i, p) in owned.drain(..).enumerate() {
        if i % 4 == 0 {
            kept.push((p, vec![fill(i); 4000]));
        } else {
            store.free(p).unwrap();
        }
    }
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let note = store.alloc(8).unwrap();
    for f in 0..flushes {
        for _ in 0..patches {
            let (p, model) = &mut kept[rng.below(n as u64 / 4) as usize];
            let off = rng.below(4000 - 32) as usize;
            model[off..off + 32].fill(f as u8);
            store
                .write(p.raw(), off as u32, &model[off..off + 32])
                .unwrap();
        }
        store
            .write(note.raw(), 0, &(f as u64).to_le_bytes())
            .unwrap();
        store.flush().unwrap();
    }
    store.close().unwrap();
    store.check();
    for (p, model) in &kept {
        assert!(store.read_all(p.raw()).unwrap() == *model);
    }
    (before, storage.len().unwrap() / 4096)
}

#[test]
fn compaction_moves_content_off_the_end() {
    let off = Options {
        hole_share: 1.0,
        ..Default::default()
    };
    let (before, without) = shrink(off, 400, 60, 0);
    let (_, with) = shrink(Options::default(), 400, 60, 0);
    println!("before {before}, without compaction {without}, with {with}");
    assert!(without * 10 > before * 9, "{without} of {before} pages");
    assert!(with * 2 < before, "{with} of {before} pages");
}

#[test]
fn compaction_moves_full_table_pages_too() {
    // The first flush after the frees states its patches inline, on leaves
    // at the end of the file, since the freed pages are not reusable yet:
    // a full table page becomes the tail. Without the rotating window, which
    // in a large file drains such a page only slowly, compaction must
    // rewrite it.
    let opts = Options {
        walk: 0,
        ..Default::default()
    };
    let (before, with) = shrink(opts, 400, 30, 100);
    println!("before {before}, with compaction {with}");
    assert!(with * 4 < before, "{with} of {before} pages");
}

#[test]
fn the_price_of_space_survives_a_reopen() {
    let storage = MemoryStorage::new();
    let store = Store::create(Box::new(storage.clone()), Options::default()).unwrap();
    let p = store.alloc(0).unwrap();
    for f in 0..40u32 {
        store.write(p.raw(), f * 100, &[1; 100]).unwrap();
        store.flush().unwrap();
    }
    let kappa = store.stats().kappa;
    assert_ne!(kappa, Options::default().kappa);
    drop(store);
    let store = Store::open(Box::new(storage), Options::default()).unwrap();
    // Kept as an `f32`.
    assert_eq!(store.stats().kappa, kappa as f32 as f64);
    assert_eq!(store.allocations().len(), 1);
}

#[test]
fn estimates_survive_a_reopen() {
    // Half of every page's content is overwritten, so pages drain; their
    // estimates must come back from the consolidator state, not from the
    // seed that assumes one rate since each page was written. Nothing is
    // ripe at this price, and the window walks nothing, so few pages lose
    // content to consolidation after their estimates are recorded, which a
    // reopen takes for a loss in the gap.
    let opts = Options {
        kappa: 1e-6,
        target_fill: 0.0,
        walk: 0,
        ..Default::default()
    };
    let close = |a: f64, b: f64| (a - b).abs() <= 0.01 * b.abs();
    let storage = MemoryStorage::new();
    let store = Store::create(Box::new(storage.clone()), opts.clone()).unwrap();
    let ptrs: Vec<_> = (0..64).map(|_| store.alloc(4000).unwrap()).collect();
    for p in &ptrs {
        store.write(p.raw(), 0, &[1; 4000]).unwrap();
    }
    store.flush().unwrap();
    for (i, p) in ptrs.iter().enumerate() {
        store
            .write(p.raw(), (i % 2) as u32 * 2000, &[2; 2000])
            .unwrap();
        store.flush().unwrap();
    }
    let before = store.describe_drains();
    let priors = store.describe_priors();
    let fresh = Store::create(Box::new(MemoryStorage::new()), opts.clone()).unwrap();
    assert_ne!(
        priors[0],
        fresh.describe_priors()[0],
        "no prior was learned"
    );
    drop(store);
    let store = Store::open(Box::new(storage), opts).unwrap();
    // Kept as `f32`s, so that the next session's first pages do not start
    // from nothing.
    for (a, b) in store.describe_priors().into_iter().zip(priors) {
        assert!(close(a.0, b.0) && close(a.1, b.1), "{a:?} against {b:?}");
    }
    let after = store.describe_drains();
    let same = before
        .iter()
        .filter(|b| after.iter().any(|a| a.0 == b.0 && close(a.1, b.1)))
        .count();
    assert!(before.len() >= 60, "{before:?}");
    assert!(
        same * 10 >= before.len() * 9,
        "{same} of {} estimates survived: {before:?} {after:?}",
        before.len()
    );
}

#[test]
fn defragmentation_merges_appended_pieces() {
    let off = Options {
        mu: f64::INFINITY,
        ..Default::default()
    };
    let without = append(off, 32, 16, 200);
    let with = append(Options::default(), 32, 16, 200);
    println!("without {without:#?}\nwith {with:#?}");
    assert!(with.defrag_rewrites > 0);
    assert!(with.statements * 2 < without.statements);
}
