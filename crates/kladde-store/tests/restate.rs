//! Bytes an allocation shifts within itself, by a splice or a move, are
//! stated at their new offsets where they lie, rather than written again;
//! bytes it copies are written again, so that no data byte has two owners.

use kladde_store::{MemoryStorage, Options, Store, UniquePointer, WriteBackend};

const SIZE: u32 = 64 * 1024;

/// A store holding one allocation of `SIZE` patterned bytes, flushed.
fn one_allocation() -> (MemoryStorage, Store, UniquePointer, Vec<u8>) {
    let storage = MemoryStorage::new();
    let store = Store::create(Box::new(storage.clone()), Options::default()).unwrap();
    let p = store.alloc(0).unwrap();
    let bytes: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();
    store.write(p.raw(), 0, &bytes).unwrap();
    store.flush().unwrap();
    (storage, store, p, bytes)
}

/// The store reopened from `storage`, checked, and the allocation's bytes.
fn reopened(storage: &MemoryStorage, p: &UniquePointer) -> Vec<u8> {
    let image = MemoryStorage::from_image(storage.image());
    let store = Store::open(Box::new(image), Options::default()).unwrap();
    store.check();
    store.read_all(p.raw()).unwrap()
}

#[test]
fn a_splice_restates_the_tail_behind_it() {
    let (storage, store, p, bytes) = one_allocation();
    let before = store.stats();
    store.splice(&p, 0, 0, &[7]).unwrap();
    store.flush().unwrap();
    store.check();
    let after = store.stats();
    assert_eq!(after.fresh_bytes - before.fresh_bytes, 1);
    assert_eq!(after.restated_bytes - before.restated_bytes, SIZE as u64);
    let got = reopened(&storage, &p);
    assert_eq!(got[0], 7);
    assert_eq!(&got[1..], &bytes[..]);
}

#[test]
fn a_removal_restates_what_follows_and_shrinks() {
    let (storage, store, p, mut bytes) = one_allocation();
    let before = store.stats();
    store.splice(&p, 100, 5000, &[]).unwrap();
    store.flush().unwrap();
    store.check();
    let after = store.stats();
    assert_eq!(after.fresh_bytes - before.fresh_bytes, 0);
    assert_eq!(
        after.restated_bytes - before.restated_bytes,
        (SIZE - 5100) as u64
    );
    bytes.drain(100..5100);
    assert_eq!(reopened(&storage, &p), bytes);
}

#[test]
fn a_move_within_the_allocation_is_restated() {
    let (storage, store, p, mut bytes) = one_allocation();
    // Move element-sized ranges around as a list's restack does: open a gap,
    // move the bytes into it, close the old place.
    let before = store.stats();
    store.splice(&p, 9000, 0, &[0; 72]).unwrap();
    store.move_range(p.raw(), 3000, 72, p.raw(), 9000).unwrap();
    store.splice(&p, 3000, 72, &[]).unwrap();
    store.flush().unwrap();
    store.check();
    let after = store.stats();
    assert_eq!(after.fresh_bytes - before.fresh_bytes, 0);
    let moved: Vec<u8> = bytes.drain(3000..3072).collect();
    bytes.splice(9000 - 72..9000 - 72, moved);
    assert_eq!(reopened(&storage, &p), bytes);
}

#[test]
fn a_copy_within_the_allocation_is_written_again() {
    let (storage, store, p, mut bytes) = one_allocation();
    let before = store.stats();
    store.copy(p.raw(), 0, 4096, p.raw(), 20_000).unwrap();
    store.splice(&p, 0, 0, &[1, 2, 3]).unwrap();
    store.flush().unwrap();
    store.check();
    let after = store.stats();
    // The copied bytes appear twice: both copies are written again.
    assert!(after.fresh_bytes - before.fresh_bytes >= 2 * 4096);
    let copy: Vec<u8> = bytes[..4096].to_vec();
    bytes[20_000..24_096].copy_from_slice(&copy);
    bytes.splice(0..0, [1, 2, 3]);
    assert_eq!(reopened(&storage, &p), bytes);
}
