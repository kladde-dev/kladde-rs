//! An allocation's size is what its newest statement of the size states, so
//! a growth must state the bytes it exposes: otherwise they would resolve
//! through statements that held them before a shrink, which the leaves of a
//! file of many allocations keep for many flushes.

use kladde_store::{MemoryStorage, Options, Store, UniquePointer, WriteBackend};

/// Enough small allocations, rewritten flush after flush, to keep the header
/// full of hot statements, so that a cold allocation's statements are
/// evicted to leaves.
fn churn(store: &Store, others: &[UniquePointer], round: u8) {
    for (i, p) in others.iter().enumerate() {
        store.write(p.raw(), 0, &[round ^ i as u8; 40]).unwrap();
    }
    store.flush().unwrap();
}

/// The file in `storage`, reopened from a copy of its image, which the store
/// writes to from then on.
fn reopened(storage: &mut MemoryStorage, opts: &Options) -> Store {
    *storage = MemoryStorage::from_image(storage.image());
    let store = Store::open(Box::new(storage.clone()), opts.clone()).unwrap();
    store.check();
    store
}

fn a_growth_never_exposes_truncated_bytes(opts: Options) {
    let mut storage = MemoryStorage::new();
    let store = Store::create(Box::new(storage.clone()), opts.clone()).unwrap();
    let cold = store.alloc(3000).unwrap();
    let old: Vec<u8> = (0..3000).map(|i| (i % 251 + 1) as u8).collect();
    store.write(cold.raw(), 0, &old).unwrap();
    let others: Vec<UniquePointer> = (0..400).map(|_| store.alloc(40).unwrap()).collect();
    for round in 0..4 {
        churn(&store, &others, round);
    }
    assert!(
        store.stats().table_pages > 1,
        "the statements fit the header"
    );
    // Truncate, let the statement of the new size go cold too, and grow back
    // without writing.
    store.resize(&cold, 10).unwrap();
    for round in 4..8 {
        churn(&store, &others, round);
    }
    store.resize(&cold, 3000).unwrap();
    store.flush().unwrap();
    store.check();
    let mut want = old[..10].to_vec();
    want.resize(3000, 0);
    assert_eq!(store.read_all(cold.raw()).unwrap(), want);
    for round in 8..12 {
        churn(&store, &others, round);
    }
    let store = reopened(&mut storage, &opts);
    assert_eq!(store.read_all(cold.raw()).unwrap(), want);
    // Shrink once more, below what the growth stated, and reopen.
    store.resize(&cold, 5).unwrap();
    store.flush().unwrap();
    let store = reopened(&mut storage, &opts);
    assert_eq!(store.read_all(cold.raw()).unwrap(), &old[..5]);
}

#[test]
fn a_growth_never_exposes_truncated_bytes_without_consolidation() {
    a_growth_never_exposes_truncated_bytes(Options {
        consolidate: false,
        ..Default::default()
    });
}

#[test]
fn a_growth_never_exposes_truncated_bytes_with_consolidation() {
    a_growth_never_exposes_truncated_bytes(Options::default());
}
