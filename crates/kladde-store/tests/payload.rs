//! Payload pages: large bytes written once, into data pages the journal
//! names, which the flush that folds the journal takes into the file.

use kladde_store::{MemoryStorage, Options, Storage, Store, WriteBackend, MAX_PAGE_CONTENT};

const PAGE: u64 = 4096;

/// Bytes no run of zeros hides, which differ from page to page.
fn content(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed) | 1)
        .collect()
}

#[test]
fn creating_a_large_file_writes_its_content_once() {
    let storage = MemoryStorage::new();
    let store = Store::create(Box::new(storage.clone()), Options::default()).unwrap();
    let bytes = content(4_361_568, 7);
    store.splice(0, 0, &bytes).unwrap();
    store.flush().unwrap();
    let s = store.stats();
    // The bytes went to payload pages, which the flush took as they lie.
    assert!(s.payload_bytes >= bytes.len() as u64 - 2048, "{s:?}");
    assert_eq!(s.adopted_bytes, s.payload_bytes);
    assert!(s.data_pages_written <= 1, "{s:?}");
    assert!(s.journal_bytes < 20_000, "{s:?}");
    store.close().unwrap();
    let pages = storage.len().unwrap() / PAGE;
    let content_pages = (bytes.len() as u64).div_ceil(MAX_PAGE_CONTENT as u64);
    assert!(
        pages <= content_pages + 8,
        "{pages} pages for {content_pages} pages of content"
    );
    let store = Store::open(Box::new(storage), Options::default()).unwrap();
    store.check();
    assert_eq!(store.read_all().unwrap(), bytes);
}

#[test]
fn payload_pages_count_toward_the_budget_so_a_large_write_is_flushed_at_once() {
    let storage = MemoryStorage::new();
    let store = Store::create(Box::new(storage.clone()), Options::default()).unwrap();
    store.splice(0, 0, &content(1000, 1)).unwrap();
    assert_eq!(store.stats().flushes, 0);
    store.splice(500, 0, &content(200_000, 2)).unwrap();
    // One flush before it, for the journal that held the first splice, and
    // one after it.
    assert_eq!(store.stats().flushes, 2);
    // Durable at once, and the next write goes to an empty journal.
    let after = Store::open(
        Box::new(MemoryStorage::from_image(storage.image())),
        Options::default(),
    )
    .unwrap();
    assert_eq!(after.len(), 201_000);
}

#[test]
fn a_payload_page_that_never_reached_the_disk_drops_its_transaction() {
    let opts = Options {
        journal_budget_pages: Some(16),
        ..Default::default()
    };
    let storage = MemoryStorage::with_crash_tracking();
    let store = Store::create(Box::new(storage.clone()), opts.clone()).unwrap();
    let before = content(10_000, 3);
    store.splice(0, 0, &before).unwrap();
    store.flush().unwrap();
    // Small enough for the budget: no flush follows it.
    store.splice(100, 0, &content(5_000, 4)).unwrap();
    store.splice(0, 0, b"later").unwrap();
    assert_eq!(store.stats().flushes, 1);
    // Two full pages of the first splice, its remainder inline, and one of
    // the second, its remainder inline too.
    assert_eq!(store.stats().payload_pages_written, 2 + 1);
    drop(store);
    // The unsynced writes are the second splice's payload page and then the
    // journal's appends; losing the page loses its transaction and all
    // after it.
    let image = storage.crash_image(|i| i != 0);
    let store = Store::open(Box::new(MemoryStorage::from_image(image)), opts.clone()).unwrap();
    store.check();
    assert_eq!(store.read_all().unwrap(), before);
    // With every write kept, both transactions are there.
    let image = storage.crash_image(|_| true);
    let store = Store::open(Box::new(MemoryStorage::from_image(image)), opts).unwrap();
    store.check();
    assert_eq!(store.len(), 15_005);
}

#[test]
fn a_bit_flip_in_a_payload_page_drops_its_transaction() {
    let storage = MemoryStorage::new();
    let store = Store::create(Box::new(storage.clone()), Options::default()).unwrap();
    let before = content(10_000, 5);
    store.splice(0, 0, &before).unwrap();
    store.flush().unwrap();
    let later = content(5_000, 6);
    store.splice(0, 0, &later).unwrap();
    drop(store);
    // The payload page is the data page whose content starts with the new
    // bytes; flip a bit in its content.
    let mut image = storage.image();
    let page = (0..image.len() / PAGE as usize)
        .find(|&p| {
            let at = p * PAGE as usize;
            image[at] == 0x02 && image[at + 11..at + 11 + 64] == later[..64]
        })
        .expect("the payload page is in the file");
    image[page * PAGE as usize + 11 + 10] ^= 1;
    let store = Store::open(
        Box::new(MemoryStorage::from_image(image)),
        Options::default(),
    )
    .unwrap();
    assert_eq!(store.read_all().unwrap(), before);
}

/// A flush's header lost, and a large splice appended after it through the
/// seal: recovery replays both journals in one flush, which keeps the
/// payload pages of its own epoch where they lie and writes those of the
/// next one again.
#[test]
fn payload_pages_beyond_a_seal_are_written_again_by_the_flush_that_folds_them() {
    let opts = Options {
        journal_budget_pages: Some(16),
        ..Default::default()
    };
    let storage = MemoryStorage::with_crash_tracking();
    let store = Store::create(Box::new(storage.clone()), opts.clone()).unwrap();
    for i in 0..6u8 {
        store.splice(0, store.len(), &content(3 * 4096, i)).unwrap();
        store.flush().unwrap();
    }
    store.splice(0, 0, &content(6000, 10)).unwrap();
    store.flush().unwrap();
    let late = content(7000, 11);
    store.splice(10, 0, &late).unwrap();
    let mut expected = store.read_all().unwrap();
    expected.splice(10..10, late.iter().copied());
    assert_eq!(store.stats().flushes, 7);
    // The first write after the last flush's `fsync` is its header.
    let image = storage.crash_image(|i| i != 0);
    let store = Store::open(Box::new(MemoryStorage::from_image(image)), opts.clone()).unwrap();
    store.check();
    assert_eq!(store.read_all().unwrap(), expected);
    let s = store.stats();
    assert!(s.adopted_bytes >= 6000 - 2048, "{s:?}");
    assert!(s.fresh_bytes >= 7000, "{s:?}");
}

#[test]
fn a_threshold_of_u32_max_keeps_every_write_in_the_journal() {
    let opts = Options {
        payload_threshold: u32::MAX,
        ..Default::default()
    };
    let store = Store::create(Box::new(MemoryStorage::new()), opts).unwrap();
    store.splice(0, 0, &content(100_000, 9)).unwrap();
    store.flush().unwrap();
    let s = store.stats();
    assert_eq!(s.payload_bytes, 0);
    assert!(s.journal_bytes > 100_000);
}
