//! Power cuts at every kind of moment: recovery must land exactly on a
//! transaction boundary no earlier than the last completed flush.

use kladde_store::{MemoryStorage, Options, Store, WriteBackend};

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

/// The sequence and the schema chain.
type Model = (Vec<u8>, Vec<u8>);

fn recovered(image: Vec<u8>, opts: &Options) -> Model {
    let store = Store::open(Box::new(MemoryStorage::from_image(image)), opts.clone())
        .expect("a crashed file opens");
    store.check();
    (store.read_all().unwrap(), store.schema())
}

/// Random operations until the storage fails, then power cuts; with
/// `mixed`, batches, transactions with cuts and pastes, and splices of up
/// to two pages too.
fn run(seed: u64, opts: &Options, mixed: bool) {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let storage = MemoryStorage::with_crash_tracking();
    let store = Store::create(Box::new(storage.clone()), opts.clone()).unwrap();
    // The model after each transaction; `history[flushed]` is the state the
    // last completed flush made durable.
    let mut model: Model = (Vec::new(), Vec::new());
    let mut history: Vec<Model> = vec![model.clone()];
    let mut flushed = 0usize;
    let mut in_batch = false;
    storage.fail_after(rng.below(400) + 1);
    for _ in 0..300 {
        let len = model.0.len() as u64;
        let kind = rng.below(if mixed { 11 } else { 8 });
        let r = match kind {
            8 => {
                in_batch = !in_batch;
                if in_batch {
                    store.begin_batch()
                } else {
                    store.end_batch()
                }
            }
            9 if len > 0 => {
                // A transaction: a cut, a write, and the cut pasted back
                // elsewhere, all or none of it.
                let seq = &mut model.0;
                let a = rng.below(len);
                let n = rng.below((len - a).min(900)) + 1;
                let w = rng.below(len - n + 1);
                let fill = (rng.below(250) + 1) as u8;
                let to = rng.below(len - n + 1);
                let cut: Vec<u8> = seq.drain(a as usize..(a + n) as usize).collect();
                let wn = (seq.len() as u64 - w).min(3);
                seq[w as usize..(w + wn) as usize].fill(fill);
                seq.splice(to as usize..to as usize, cut.iter().copied());
                store.atomically(|| {
                    let c = store.splice(a, n, b"")?.unwrap();
                    store.write(w, &vec![fill; wn as usize])?;
                    store.paste(to, 0, c, &cut)
                })
            }
            10 => {
                model.1 = vec![(rng.below(250) + 1) as u8; rng.below(300) as usize];
                store.set_schema(model.1.clone());
                Ok(())
            }
            0..=2 if len > 0 => {
                let pos = rng.below(len);
                let n = rng.below((len - pos).min(900)) + 1;
                let fill = (rng.below(250) + 1) as u8;
                model.0[pos as usize..(pos + n) as usize].fill(fill);
                store.write(pos, &vec![fill; n as usize]).map(|_| ())
            }
            3..=5 => {
                let pos = rng.below(len + 1);
                let removed = rng.below((len - pos).min(500) + 1);
                let max = if mixed && rng.below(10) == 0 {
                    8192
                } else {
                    900
                };
                let fill = (rng.below(250) + 1) as u8;
                let bytes = vec![fill; rng.below(max) as usize];
                model.0.splice(
                    pos as usize..(pos + removed) as usize,
                    bytes.iter().copied(),
                );
                store.splice(pos, removed, &bytes).map(|_| ())
            }
            6 if len > 0 => {
                let from = rng.below(len);
                let n = rng.below(len - from) + 1;
                let to = rng.below(len - n + 1);
                let m: Vec<u8> = model.0.drain(from as usize..(from + n) as usize).collect();
                model.0.splice(to as usize..to as usize, m);
                store.move_range(from, n, to)
            }
            _ => store.flush().map(|()| flushed = history.len() - 1),
        };
        // A failed operation may or may not have reached the journal, so both
        // the state before it and the state after it are boundaries. A new
        // schema reaches the file only with a flush, so the states with and
        // without it are both boundaries until then.
        if history.last() != Some(&model) {
            history.push(model.clone());
        }
        if r.is_err() {
            break;
        }
    }
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
        // The schema chain is written by flushes alone, so a state matches
        // a boundary whose sequence is the same, and whose schema is the
        // one the last flush before it wrote.
        let matches = history.iter().enumerate().rposition(|(k, h)| {
            h.0 == got.0 && (h.1 == got.1 || history[..=k].iter().rev().any(|e| e.1 == got.1))
        });
        match matches {
            Some(k) => assert!(
                k >= flushed,
                "seed {seed}: recovered transaction {k}, but {flushed} were flushed"
            ),
            None => panic!("seed {seed}: the recovered state is no transaction boundary"),
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
        run(seed, &opts, false);
    }
}

#[test]
fn power_cuts_with_consolidation() {
    for seed in 1..150 {
        run(seed, &Options::default(), false);
    }
}

#[test]
fn power_cuts_with_a_tiny_journal() {
    let opts = Options {
        journal_budget_pages: 1,
        ..Default::default()
    };
    for seed in 1..150 {
        run(seed, &opts, false);
    }
}

/// With a journal of one page, batches are cut and flushed often, and a
/// splice of two pages is flushed before and after.
#[test]
fn power_cuts_with_batches_cuts_and_overlong_splices_in_a_tiny_journal() {
    let opts = Options {
        journal_budget_pages: 1,
        ..Default::default()
    };
    for seed in 1..150 {
        run(seed, &opts, true);
    }
}
