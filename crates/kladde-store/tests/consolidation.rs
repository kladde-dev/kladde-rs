//! Consolidation keeps a file near its sequence's length under sustained
//! edits, and the span policy keeps the spans few, without changing what the
//! file reads.

use kladde_store::{MemoryStorage, Options, Stats, Storage, Store, WriteBackend};

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

/// Checks that the store, and the file reopened, read `model`.
fn verify(store: &Store, storage: &MemoryStorage, opts: &Options, model: &[u8]) {
    store.check();
    assert!(store.read_all().unwrap() == model, "the sequence differs");
    let reopened = Store::open(
        Box::new(MemoryStorage::from_image(storage.image())),
        opts.clone(),
    )
    .unwrap();
    reopened.check();
    assert!(
        reopened.read_all().unwrap() == model,
        "the sequence differs after reopening"
    );
}

/// Overwrites random ranges of a sequence of `size` bytes, flushing every
/// `per_flush` writes.
fn overwrite(opts: Options, size: usize, write: usize, per_flush: usize, flushes: usize) -> Stats {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let storage = MemoryStorage::new();
    let store = Store::create(Box::new(storage.clone()), opts.clone()).unwrap();
    let mut model = vec![0u8; size];
    store.splice(0, 0, &model).unwrap();
    for f in 0..flushes {
        for _ in 0..per_flush {
            let len = (rng.below(write as u64) + 1) as usize;
            let off = rng.below((size - len) as u64 + 1) as usize;
            model[off..off + len].fill((f % 251 + 1) as u8);
            store.write(off as u64, &model[off..off + len]).unwrap();
        }
        store.flush().unwrap();
    }
    verify(&store, &storage, &opts, &model);
    store.stats()
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
    let without = overwrite(off, 512 * 1024, 600, 32, 300);
    let with = overwrite(Options::default(), 512 * 1024, 600, 32, 300);
    println!("without {without:#?}\nwith {with:#?}");
    assert!(with.evacuated_pages > 0);
    assert!(
        data_fill(&without) < 0.6,
        "data fill {}",
        data_fill(&without)
    );
    assert!(data_fill(&with) > 0.75, "data fill {}", data_fill(&with));
    assert!(with.data_pages * 3 < without.data_pages * 2);
}

#[test]
fn leaves_are_drained_and_stay_full() {
    let with = overwrite(Options::default(), 512 * 1024, 600, 32, 300);
    assert!(with.table_rewrites > 0);
    assert!(
        with.live_fraction() > 0.7,
        "live fraction {}",
        with.live_fraction()
    );
}

/// Types into a sequence of `size` bytes keystroke by keystroke at a cursor
/// that jumps now and then, flushing every `per_flush` keystrokes; returns
/// the spans at the end.
fn typing(opts: Options, size: usize, keystrokes: u64, per_flush: u64) -> Stats {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let storage = MemoryStorage::new();
    let store = Store::create(Box::new(storage.clone()), opts.clone()).unwrap();
    let mut model = vec![b' '; size];
    store.splice(0, 0, &model).unwrap();
    let mut cursor = 0u64;
    for k in 0..keystrokes {
        if rng.below(500) == 0 {
            cursor = rng.below(model.len() as u64 + 1);
        }
        if rng.below(10) == 0 && cursor > 0 {
            cursor -= 1;
            model.remove(cursor as usize);
            store.splice(cursor, 1, b"").unwrap();
        } else {
            let c = b'a' + (k % 26) as u8;
            model.insert(cursor as usize, c);
            store.splice(cursor, 0, &[c]).unwrap();
            cursor += 1;
        }
        if k % per_flush == per_flush - 1 {
            store.flush().unwrap();
        }
    }
    store.flush().unwrap();
    verify(&store, &storage, &opts, &model);
    store.stats()
}

#[test]
fn merges_keep_the_spans_of_typed_text_few() {
    let off = Options {
        span_min: 0,
        walk: 0,
        ..Default::default()
    };
    let without = typing(off, 64 * 1024, 30_000, 50);
    let with = typing(Options::default(), 64 * 1024, 30_000, 50);
    println!("without {without:#?}\nwith {with:#?}");
    assert!(with.evict_merges > 0 && with.walk_merges > 0);
    assert!(
        with.spans * 2 < without.spans,
        "{} against {}",
        with.spans,
        without.spans
    );
    // The description stays a small share of the bytes it describes.
    assert!(
        (with.live_table_bytes as f64) < 0.05 * with.sequence_bytes as f64,
        "{} bytes of entries for {} bytes",
        with.live_table_bytes,
        with.sequence_bytes
    );
}

/// Writes a sequence of `n` pages, cuts out all but every fourth, then runs
/// `flushes` flushes of a small overwrite each. Returns the file's length
/// in pages before and after.
fn shrink(opts: Options, n: usize, flushes: usize) -> (u64, u64) {
    let storage = MemoryStorage::new();
    let store = Store::create(Box::new(storage.clone()), opts.clone()).unwrap();
    let page = 4000usize;
    let mut model: Vec<u8> = (0..n * page).map(|i| (i / page % 251) as u8 + 1).collect();
    store.begin_batch().unwrap();
    for i in 0..n {
        store
            .splice((i * page) as u64, 0, &model[i * page..(i + 1) * page])
            .unwrap();
    }
    store.end_batch().unwrap();
    store.flush().unwrap();
    let before = storage.len().unwrap() / 4096;
    for i in (0..n).rev() {
        if i % 4 != 0 {
            model.drain(i * page..(i + 1) * page);
            store.splice((i * page) as u64, page as u64, b"").unwrap();
        }
    }
    for f in 0..flushes {
        model[f..f + 8].fill(f as u8);
        store.write(f as u64, &model[f..f + 8]).unwrap();
        store.flush().unwrap();
    }
    store.close().unwrap();
    verify(&store, &storage, &opts, &model);
    (before, storage.len().unwrap() / 4096)
}

#[test]
fn compaction_moves_content_off_the_end() {
    let off = Options {
        hole_share: 1.0,
        ..Default::default()
    };
    let (before, without) = shrink(off, 400, 60);
    let (_, with) = shrink(Options::default(), 400, 60);
    println!("before {before}, without compaction {without}, with {with}");
    assert!(without * 10 > before * 7, "{without} of {before} pages");
    assert!(with * 2 < before, "{with} of {before} pages");
}
