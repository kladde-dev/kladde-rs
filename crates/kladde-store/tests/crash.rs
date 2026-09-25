//! Power cuts at every kind of moment: recovery must land exactly on a
//! transaction boundary no earlier than the last completed flush.

use std::collections::BTreeMap;

use kladde_store::{MemoryStorage, Options, Pointer, Store, UniquePointer, WriteBackend};

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

type Model = BTreeMap<u32, Vec<u8>>;

fn recovered(image: Vec<u8>, opts: &Options) -> Model {
    let store = Store::open(Box::new(MemoryStorage::from_image(image)), opts.clone())
        .expect("a crashed file opens");
    store.check();
    store
        .allocations()
        .into_iter()
        .map(|(p, _)| (p.raw(), store.read_all(p).unwrap()))
        .collect()
}

/// Whether `got` is `want` plus one allocation of `size` zeros: what an
/// `alloc` leaves that failed after its transaction reached the journal.
fn plus_one_alloc(got: &Model, want: &Model, size: usize) -> bool {
    let extra: Vec<&u32> = got.keys().filter(|id| !want.contains_key(id)).collect();
    extra.len() == 1
        && got[extra[0]] == vec![0; size]
        && got.len() == want.len() + 1
        && want.iter().all(|(id, v)| got.get(id) == Some(v))
}

fn run(seed: u64, opts: &Options) {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let storage = MemoryStorage::with_crash_tracking();
    let store = Store::create(Box::new(storage.clone()), opts.clone()).unwrap();
    // The model after each transaction; `history[flushed]` is the state the
    // last completed flush made durable.
    let mut model = Model::new();
    let mut owned: BTreeMap<u32, UniquePointer> = BTreeMap::new();
    let mut history: Vec<Model> = vec![model.clone()];
    let mut flushed = 0usize;
    let mut failed_alloc: Option<usize> = None;
    storage.fail_after(rng.below(400) + 1);
    for _ in 0..300 {
        let ids: Vec<u32> = model.keys().copied().collect();
        let pick = |rng: &mut Rng| ids[rng.below(ids.len() as u64) as usize];
        let r = match rng.below(8) {
            0 | 1 => {
                let size = rng.below(3000) as u32;
                store
                    .alloc(size)
                    .map(|p| {
                        model.insert(p.raw().raw(), vec![0; size as usize]);
                        owned.insert(p.raw().raw(), p);
                    })
                    .inspect_err(|_| failed_alloc = Some(size as usize))
            }
            2 if !ids.is_empty() => {
                let id = pick(&mut rng);
                model.remove(&id);
                store.free(owned.remove(&id).unwrap())
            }
            3 if !ids.is_empty() => {
                let id = pick(&mut rng);
                let size = rng.below(3000) as u32;
                model.get_mut(&id).unwrap().resize(size as usize, 0);
                store.resize(&owned[&id], size)
            }
            4..=6 if !ids.is_empty() => {
                let id = pick(&mut rng);
                let v = model.get_mut(&id).unwrap();
                let off = rng.below(v.len() as u64 + 1) as u32;
                let fill = (rng.below(250) + 1) as u8;
                let bytes = vec![fill; rng.below(900) as usize];
                let end = off as usize + bytes.len();
                if v.len() < end {
                    v.resize(end, 0);
                }
                v[off as usize..end].copy_from_slice(&bytes);
                store.write(Pointer::from_raw(id).unwrap(), off, &bytes)
            }
            _ => store.flush().map(|()| flushed = history.len() - 1),
        };
        // A failed operation may or may not have reached the journal, so both
        // the state before it and the state after it are boundaries.
        if history.last() != Some(&model) {
            history.push(model.clone());
        }
        if r.is_err() {
            break;
        }
    }
    // Power cuts: nothing written since the last sync survives, all of it,
    // or random subsets of it.
    let n = storage.unsynced_ops();
    let mut images = vec![
        storage.crash_image(|_| false),
        storage.crash_image(|_| true),
    ];
    for _ in 0..4 {
        let keep: Vec<bool> = (0..n).map(|_| rng.below(2) == 0).collect();
        images.push(storage.crash_image(|i| keep[i]));
    }
    for image in images {
        let got = recovered(image, opts);
        match history.iter().rposition(|h| *h == got) {
            Some(k) => assert!(
                k >= flushed,
                "seed {seed}: recovered transaction {k}, but {flushed} were flushed"
            ),
            None => assert!(
                failed_alloc.is_some_and(|size| plus_one_alloc(
                    &got,
                    history.last().unwrap(),
                    size
                )),
                "seed {seed}: the recovered state is no transaction boundary"
            ),
        }
    }
}

#[test]
fn power_cuts_land_on_transaction_boundaries() {
    let opts = Options {
        consolidate: false,
        ..Default::default()
    };
    for seed in 1..150 {
        run(seed, &opts);
    }
}

#[test]
fn power_cuts_with_consolidation() {
    for seed in 1..150 {
        run(seed, &Options::default());
    }
}

#[test]
fn power_cuts_with_a_tiny_journal() {
    let opts = Options {
        journal_budget_pages: 1,
        ..Default::default()
    };
    for seed in 1..150 {
        run(seed, &opts);
    }
}
