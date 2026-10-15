//! Closing into a fully consolidated file keeps what the file reads, and
//! leaves the smallest file that holds it: full data pages, one span per page
//! and chain, and an empty journal.

use kladde_store::{
    FileStorage, MemoryStorage, Options, Stats, Storage, Store, WriteBackend, MAX_PAGE_CONTENT,
    PAGE_SIZE,
};

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

/// Splices, writes and moves at random positions of a sequence that starts
/// with `size` bytes, `per_flush` operations between flushes; returns what
/// the sequence holds.
fn edit(store: &Store, size: usize, ops: usize, per_flush: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed);
    let mut model: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    store.splice(0, 0, &model).unwrap();
    for n in 0..ops {
        let len = model.len() as u64;
        match rng.below(3) {
            0 => {
                let pos = rng.below(len + 1);
                let removed = rng.below((len - pos).min(64) + 1);
                let bytes: Vec<u8> = (0..rng.below(200)).map(|_| rng.next() as u8).collect();
                store.splice(pos, removed, &bytes).unwrap();
                model.splice(pos as usize..(pos + removed) as usize, bytes);
            }
            1 if len > 0 => {
                let pos = rng.below(len);
                let bytes: Vec<u8> = (0..rng.below(len - pos) + 1)
                    .take(300)
                    .map(|_| rng.next() as u8)
                    .collect();
                store.write(pos, &bytes).unwrap();
                model[pos as usize..pos as usize + bytes.len()].copy_from_slice(&bytes);
            }
            _ if len > 0 => {
                let from = rng.below(len);
                let n = rng.below((len - from).min(5000)) + 1;
                let to = rng.below(len - n + 1);
                store.move_range(from, n, to).unwrap();
                let moved: Vec<u8> = model.drain(from as usize..(from + n) as usize).collect();
                model.splice(to as usize..to as usize, moved);
            }
            _ => {}
        }
        if per_flush > 0 && n % per_flush == per_flush - 1 {
            store.flush().unwrap();
        }
    }
    model
}

/// Checks that the file in `storage` reads `model`, `schema` and `private`,
/// and is laid out as a fully consolidated file of them is; returns its
/// stats.
fn check_consolidated(
    storage: &MemoryStorage,
    model: &[u8],
    schema: &[u8],
    private: &[u8],
) -> Stats {
    let store = Store::open(
        Box::new(MemoryStorage::from_image(storage.image())),
        Options::default(),
    )
    .unwrap();
    store.check();
    assert!(store.read_all().unwrap() == model, "the sequence differs");
    assert_eq!(store.schema(), schema);
    assert_eq!(store.private(), private);
    let s = store.stats();
    let total = (model.len() + schema.len() + private.len()) as u64;
    let data_pages = total.div_ceil(MAX_PAGE_CONTENT as u64);
    let chains = [model.len(), schema.len(), private.len()]
        .iter()
        .filter(|&&n| n > 0)
        .count() as u64;
    assert_eq!(s.data_pages, data_pages, "{s:?}");
    assert_eq!(s.live_data_bytes, total);
    // One span per page and chain: a page the end of one chain and the start
    // of the next share holds two.
    assert!(s.spans <= data_pages + chains.saturating_sub(1), "{s:?}");
    assert_eq!(
        storage.len().unwrap(),
        (2 + data_pages + s.table_pages) * PAGE_SIZE as u64,
        "{s:?}"
    );
    s
}

#[test]
fn a_consolidated_file_reads_as_the_store_did() {
    for (size, ops, per_flush, seed) in [
        (0, 0, 0, 1),
        (10, 50, 0, 2),
        (50_000, 3000, 200, 3),
        (300_000, 2000, 0, 4),
        (200_000, 5000, 50, 5),
    ] {
        let storage = MemoryStorage::new();
        let store = Store::create(Box::new(storage.clone()), Options::default()).unwrap();
        let schema = vec![7u8; size % 9000 + 3];
        store.set_schema(schema.clone());
        store.flush().unwrap();
        let model = edit(&store, size, ops, per_flush, seed);
        let private = b"state".to_vec();
        store.set_private(private.clone());
        store.set_root_fingerprint([size as u8; 16]);
        store.close_consolidated().unwrap();
        let s = check_consolidated(&storage, &model, &schema, &private);
        assert_eq!(s.table_pages, 0, "the header holds every entry");
        let store = Store::open(Box::new(storage), Options::default()).unwrap();
        assert_eq!(store.root_fingerprint(), [size as u8; 16]);
    }
}

#[test]
fn a_large_file_keeps_its_entries_in_leaves() {
    let storage = MemoryStorage::new();
    let store = Store::create(Box::new(storage.clone()), Options::default()).unwrap();
    let model = edit(&store, 5_000_000, 300, 100, 6);
    store.close_consolidated().unwrap();
    let s = check_consolidated(&storage, &model, b"", b"");
    assert!(s.table_pages > 0, "{s:?}");
}

#[test]
fn the_file_takes_further_edits_after_reopening() {
    let storage = MemoryStorage::new();
    let store = Store::create(Box::new(storage.clone()), Options::default()).unwrap();
    let mut model = edit(&store, 20_000, 500, 100, 7);
    store.close_consolidated().unwrap();
    let store = Store::open(Box::new(storage.clone()), Options::default()).unwrap();
    store.splice(100, 10, b"after").unwrap();
    model.splice(100..110, b"after".iter().copied());
    drop(store);
    let store = Store::open(Box::new(storage.clone()), Options::default()).unwrap();
    assert!(store.read_all().unwrap() == model);
    store.close_consolidated().unwrap();
    check_consolidated(&storage, &model, b"", b"");
}

#[test]
fn an_open_transaction_is_discarded_and_a_batch_kept() {
    let storage = MemoryStorage::new();
    let store = Store::create(Box::new(storage.clone()), Options::default()).unwrap();
    store.splice(0, 0, b"kept").unwrap();
    store.begin_batch().unwrap();
    store.splice(4, 0, b" too").unwrap();
    store.begin_transaction().unwrap();
    store.splice(0, 0, b"lost").unwrap();
    store.close_consolidated().unwrap();
    check_consolidated(&storage, b"kept too", b"", b"");
}

#[test]
fn a_crash_before_the_replacement_keeps_the_old_file() {
    let storage = MemoryStorage::with_crash_tracking();
    let store = Store::create(Box::new(storage.clone()), Options::default()).unwrap();
    store.splice(0, 0, b"old").unwrap();
    store.flush().unwrap();
    store.splice(3, 0, b" and new").unwrap();
    storage.fail_after(0);
    assert!(store.close_consolidated().is_err());
    // Every journal write made it, as they do but for a power cut.
    let crashed = storage.crash_image(|_| true);
    let store = Store::open(
        Box::new(MemoryStorage::from_image(crashed)),
        Options::default(),
    )
    .unwrap();
    assert_eq!(store.read_all().unwrap(), b"old and new");
}

#[test]
fn a_file_is_replaced_in_its_directory() {
    let dir =
        std::env::temp_dir().join(format!("kladde-close-consolidated-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("drawing.kladde");
    let store = Store::create(
        Box::new(FileStorage::create(&path).unwrap()),
        Options::default(),
    )
    .unwrap();
    let model = edit(&store, 100_000, 2000, 300, 8);
    let before = std::fs::metadata(&path).unwrap().len();
    store.close_consolidated().unwrap();
    let names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["drawing.kladde"], "no temporary file is left");
    let after = std::fs::metadata(&path).unwrap().len();
    assert!(after < before, "{after} < {before}");
    let image = std::fs::read(&path).unwrap();
    check_consolidated(&MemoryStorage::from_image(image), &model, b"", b"");
    std::fs::remove_dir_all(&dir).unwrap();
}
