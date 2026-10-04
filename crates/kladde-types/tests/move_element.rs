//! `move_element` reorders a vector as removing and inserting would, takes
//! the element's bytes from where they lie, and so leaves the next flush no
//! bytes to write but the statements that say where they are now.

use kladde::{Kladde, MemoryStorage, Options, Persistable};
use kladde_types::{
    PackedPersistableVec, PersistableString, PersistableVec, SmallPersistableString,
    SmallPersistableVec,
};

/// Moves applied to `0..n` as removing and inserting would.
fn moved(n: usize, moves: &[(usize, usize)]) -> Vec<usize> {
    let mut v: Vec<usize> = (0..n).collect();
    for &(from, to) in moves {
        let x = v.remove(from);
        v.insert(to, x);
    }
    v
}

const MOVES: [(usize, usize); 5] = [(0, 9), (9, 0), (3, 17), (25, 2), (12, 13)];

/// Stores `strings(0..30)` in a `V`, flushes, applies `MOVES` through
/// `move_element`, and checks the order before and after a reopen, and that
/// the flush after the moves wrote no fresh bytes.
fn check<V>(
    make: impl Fn(Vec<String>) -> V,
    texts: impl Fn(&V) -> Vec<String>,
    apply: impl Fn(&mut Kladde<V>, usize, usize) -> Result<(), kladde::Error>,
) where
    V: Persistable + 'static,
{
    let words: Vec<String> = (0..30).map(|i| format!("element number {i}")).collect();
    let storage = MemoryStorage::new();
    let mut db = Kladde::create_in(
        Box::new(storage.clone()),
        make(words.clone()),
        Options::default(),
    )
    .unwrap();
    db.flush().unwrap();
    let before = db.stats();
    for &(from, to) in &MOVES {
        apply(&mut db, from, to).unwrap();
    }
    db.flush().unwrap();
    let after = db.stats();
    let want: Vec<String> = moved(30, &MOVES)
        .into_iter()
        .map(|i| words[i].clone())
        .collect();
    assert_eq!(texts(db.get()), want);
    assert_eq!(
        after.fresh_bytes - before.fresh_bytes,
        0,
        "fresh bytes written"
    );
    assert!(after.restated_bytes > before.restated_bytes);
    drop(db);
    let image = MemoryStorage::from_image(storage.image());
    let reopened = Kladde::<V>::open_in(Box::new(image), Options::default()).unwrap();
    assert_eq!(texts(reopened.get()), want);
}

#[test]
fn a_slotted_vector_moves_its_slots() {
    check::<PersistableVec<PersistableString>>(
        |w| w.into_iter().map(PersistableString::from).collect(),
        |v| v.iter().map(|s| s.to_string()).collect(),
        |db, from, to| db.guard().move_element(from, to),
    );
}

#[test]
fn a_packed_vector_moves_its_encodings() {
    check::<PackedPersistableVec<PersistableString>>(
        |w| w.into_iter().map(PersistableString::from).collect(),
        |v| v.iter().map(|s| s.to_string()).collect(),
        |db, from, to| db.guard().move_element(from, to),
    );
}

#[test]
fn a_spilled_small_vector_moves_within_its_allocation() {
    check::<SmallPersistableVec<SmallPersistableString>>(
        |w| w.into_iter().map(SmallPersistableString::from).collect(),
        |v| v.iter().map(|s| s.to_string()).collect(),
        |db, from, to| db.guard().move_element(from, to),
    );
}

#[test]
fn an_inline_small_vector_moves_within_its_owner() {
    // Short strings, so that the content stays inline, in the root.
    let storage = MemoryStorage::new();
    let items: SmallPersistableVec<SmallPersistableString> = ["a", "bb", "ccc", "dddd"]
        .into_iter()
        .map(SmallPersistableString::from)
        .collect();
    let mut db = Kladde::create_in(Box::new(storage.clone()), items, Options::default()).unwrap();
    assert!(db.get().is_inline());
    db.guard().move_element(0, 3).unwrap();
    db.guard().move_element(2, 0).unwrap();
    let texts = |v: &SmallPersistableVec<SmallPersistableString>| -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    };
    assert_eq!(texts(db.get()), ["dddd", "bb", "ccc", "a"]);
    db.flush().unwrap();
    drop(db);
    let image = MemoryStorage::from_image(storage.image());
    let reopened = Kladde::<SmallPersistableVec<SmallPersistableString>>::open_in(
        Box::new(image),
        Options::default(),
    )
    .unwrap();
    assert_eq!(texts(reopened.get()), ["dddd", "bb", "ccc", "a"]);
    assert!(reopened.get().is_inline());
}
