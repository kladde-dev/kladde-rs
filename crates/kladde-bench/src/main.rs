//! Realistic workloads on real files, measured flush by flush.
//!
//! Every scenario drives a store on a file in the output directory with a
//! seeded workload, flushes at fixed intervals, and appends one CSV row per
//! flush to `<scenario>.csv`: what the file looks like, and the work done so
//! far. `plot-evaluation.py` in kladde-docs turns the tables into figures.
//!
//! ```text
//! cargo run --release -p kladde-bench -- <output directory> [scenario ...]
//! ```
//!
//! Scenarios: `uniform`, `skewed`, `mixed`, `append`, `churn`, `shrink`,
//! `typed`, `tuning`, which runs three of them with variants of the default
//! options, and `quick`, which makes whatever runs small. Without a scenario,
//! all run.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use kladde_store::{FileStorage, Options, Pointer, Stats, Store, UniquePointer, WriteBackend};

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
/// Operations between two explicit flushes.
const OPS_PER_FLUSH: u64 = 1000;

/// xorshift64: deterministic, and good enough for workloads.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }
    fn chance(&mut self, p: f64) -> bool {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }
    fn bytes(&mut self, len: usize) -> Vec<u8> {
        let seed = self.next();
        (0..len)
            .map(|i| (seed.wrapping_add(i as u64 * 0x9E37) >> 7) as u8)
            .collect()
    }
}

/// The header of every scenario's table.
const COLUMNS: &str =
    "scenario,variant,size,flush,ops,app_bytes,file_pages,data_pages,table_pages,\
free_pages,live_data,live_table,alloc_bytes,allocations,statements,fragments,budget,\
data_written,table_written,headers_written,journal_bytes,fresh_bytes,evacuated_pages,\
evacuated_bytes,free_filled,budget_pages,table_rewrites,window_restated,defrag_rewrites,\
defrag_bytes,compaction_flushes,truncations,flush_us,ops_us,\
mixed_pages,mixed_bytes,cold_bytes,cold_mixed_bytes";

/// The classes of allocation `mixed` assigns, as indexes of per-class sums.
const HOT: usize = 0;
const COOL: usize = 1;
const COLD: usize = 2;

/// One scenario run: a store on a file, the workload's counters, and the rows
/// recorded so far.
struct Run {
    store: Store,
    path: PathBuf,
    scenario: &'static str,
    variant: String,
    size: u64,
    flushes: u64,
    ops: u64,
    app_bytes: u64,
    /// The counters when the measured phase began, subtracted from each row.
    base: Stats,
    ops_start: Instant,
    rows: Vec<String>,
    /// Allocation id -> class, where the scenario assigns classes: each row
    /// then records how the classes share data pages.
    classes: Option<HashMap<u32, usize>>,
}

impl Run {
    fn new(dir: &Path, scenario: &'static str, variant: &str, size: u64, opts: Options) -> Run {
        let path = dir.join(format!("{scenario}-{variant}-{}.kladde", size / KIB));
        let storage = FileStorage::create(&path).expect("create the file");
        let store = Store::create(Box::new(storage), opts).expect("create the store");
        Run {
            store,
            path,
            scenario,
            variant: variant.to_string(),
            size,
            flushes: 0,
            ops: 0,
            app_bytes: 0,
            base: Stats::default(),
            ops_start: Instant::now(),
            rows: Vec::new(),
            classes: None,
        }
    }

    /// Ends the setup phase: flushes, and measures from here on.
    fn begin(&mut self) {
        self.store.flush().expect("flush");
        self.base = self.store.stats();
        self.ops_start = Instant::now();
    }

    fn write(&mut self, p: Pointer, offset: u32, bytes: &[u8]) {
        self.store.write(p, offset, bytes).expect("write");
        self.ops += 1;
        self.app_bytes += bytes.len() as u64;
    }

    fn alloc(&mut self, size: u32) -> UniquePointer {
        self.ops += 1;
        self.store.alloc(size).expect("alloc")
    }

    fn free(&mut self, p: UniquePointer) {
        self.ops += 1;
        self.store.free(p).expect("free");
    }

    fn resize(&mut self, p: &UniquePointer, size: u32) {
        self.ops += 1;
        self.store.resize(p, size).expect("resize");
    }

    /// Flushes if an interval of operations is complete.
    fn tick(&mut self) {
        if self.ops.is_multiple_of(OPS_PER_FLUSH) {
            self.flush();
        }
    }

    /// Flushes and records a row.
    fn flush(&mut self) {
        let ops_us = self.ops_start.elapsed().as_micros();
        let t = Instant::now();
        self.store.flush().expect("flush");
        let flush_us = t.elapsed().as_micros();
        self.flushes += 1;
        let s = self.store.stats();
        let mix = self
            .classes
            .as_ref()
            .map_or([0; 4], |c| mixing(&self.store, c));
        let b = &self.base;
        self.rows.push(format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            self.scenario,
            self.variant,
            self.size,
            self.flushes,
            self.ops,
            self.app_bytes,
            s.file_pages,
            s.data_pages,
            s.table_pages,
            s.free_pages,
            s.live_data_bytes,
            s.live_table_bytes,
            s.allocation_bytes,
            s.allocations,
            s.statements,
            s.fragments,
            s.budget,
            s.data_pages_written - b.data_pages_written,
            s.table_pages_written - b.table_pages_written,
            s.headers_written - b.headers_written,
            s.journal_bytes - b.journal_bytes,
            s.fresh_bytes - b.fresh_bytes,
            s.evacuated_pages - b.evacuated_pages,
            s.evacuated_bytes - b.evacuated_bytes,
            s.free_filled_pages - b.free_filled_pages,
            s.budget_pages - b.budget_pages,
            s.table_rewrites - b.table_rewrites,
            s.window_restated - b.window_restated,
            s.defrag_rewrites - b.defrag_rewrites,
            s.defrag_bytes - b.defrag_bytes,
            s.compaction_flushes - b.compaction_flushes,
            s.truncations - b.truncations,
            flush_us,
            ops_us,
            mix[0],
            mix[1],
            mix[2],
            mix[3],
        ));
        self.ops_start = Instant::now();
    }

    /// Closes the store, checks the file reopens with the same content, and
    /// returns the rows.
    fn finish(self, check: &[(Pointer, Vec<u8>)]) -> Vec<String> {
        self.store.close().expect("close");
        drop(self.store);
        let storage = FileStorage::open(&self.path).expect("reopen the file");
        let store = Store::open(Box::new(storage), Options::default()).expect("reopen the store");
        for (p, want) in check {
            let got = store.read_all(*p).expect("read");
            assert!(
                got == *want,
                "{}: allocation {} differs after reopening",
                self.scenario,
                p.raw()
            );
        }
        drop(store);
        std::fs::remove_file(&self.path).ok();
        self.rows
    }
}

fn opts(consolidate: bool) -> Options {
    Options {
        consolidate,
        ..Default::default()
    }
}

/// The label and options of the default policy, or of none.
fn on_off(consolidate: bool) -> (&'static str, Options) {
    (if consolidate { "on" } else { "off" }, opts(consolidate))
}

/// Overwrites random ranges of fixed-size allocations -- uniformly, or with
/// 90 % of the writes going to 10 % of the allocations -- until eight times
/// the live size has been written.
fn overwrite(
    dir: &Path,
    scenario: &'static str,
    (variant, options): (&str, Options),
    size: u64,
    skewed: bool,
) -> Vec<String> {
    const ALLOC: u64 = 16 * KIB;
    const AVG_WRITE: u64 = 256;
    let mut run = Run::new(dir, scenario, variant, size, options);
    let mut rng = Rng::new(size ^ skewed as u64);
    let n = (size / ALLOC).max(1);
    let mut model: Vec<Vec<u8>> = Vec::new();
    let mut ptrs = Vec::new();
    for _ in 0..n {
        let p = run.alloc(ALLOC as u32);
        let bytes = rng.bytes(ALLOC as usize);
        run.store.write(p.raw(), 0, &bytes).expect("fill");
        model.push(bytes);
        ptrs.push(p);
        if ptrs.len() % 64 == 0 {
            run.store.flush().expect("flush");
        }
    }
    run.begin();
    let hot = (n / 10).max(1);
    while run.app_bytes < 8 * size {
        let i = if skewed && rng.chance(0.9) {
            rng.below(hot)
        } else {
            rng.below(n)
        } as usize;
        let len = rng.range(1, 2 * AVG_WRITE) as usize;
        let off = rng.below(ALLOC - len as u64 + 1) as usize;
        let bytes = rng.bytes(len);
        model[i][off..off + len].copy_from_slice(&bytes);
        run.write(ptrs[i].raw(), off as u32, &bytes);
        run.tick();
    }
    run.flush();
    let check: Vec<(Pointer, Vec<u8>)> = ptrs.iter().map(|p| p.raw()).zip(model).collect();
    run.finish(&check)
}

/// How the classes of `classes` share the store's data pages: the pages that
/// hold more than one class, the live bytes on those pages, the cold bytes on
/// all data pages, and the cold bytes on those pages, which share a page with
/// content that still changes. A few cold bytes can live in leaves, where
/// description defragmentation states short runs inline.
fn mixing(store: &Store, classes: &HashMap<u32, usize>) -> [u64; 4] {
    let mut pages: HashMap<u32, [u64; 3]> = HashMap::new();
    for (page, id, bytes) in store.describe_data_pages() {
        if let Some(&class) = classes.get(&id) {
            pages.entry(page).or_default()[class] += bytes as u64;
        }
    }
    let mut out = [0; 4];
    for held in pages.values() {
        out[2] += held[COLD];
        if held.iter().filter(|&&b| b > 0).count() > 1 {
            out[0] += 1;
            out[1] += held.iter().sum::<u64>();
            out[3] += held[COLD];
        }
    }
    out
}

/// Skewed overwrites, as in `overwrite`, of allocations that are each hot,
/// cool, or cold at random, so that the three share pages: a tenth are hot and
/// take nine tenths of the writes, and the other writes go to any allocation
/// alike, except that cold ones never change, so writes drawn for them are not
/// made. The allocations are 1 KiB, four to a page, since cold content is only
/// ever written at the start, and an allocation of four pages would fill them
/// alone. Each row records how the three share data pages.
fn mixed(dir: &Path, (variant, options): (&str, Options), size: u64) -> Vec<String> {
    const ALLOC: u64 = KIB;
    const AVG_WRITE: u64 = 256;
    let mut run = Run::new(dir, "mixed", variant, size, options);
    let mut rng = Rng::new(size ^ 19);
    let n = (size / ALLOC).max(1);
    let mut model: Vec<Vec<u8>> = Vec::new();
    let mut ptrs = Vec::new();
    let mut class = Vec::new();
    let mut classes = HashMap::new();
    for _ in 0..n {
        let c = if rng.chance(0.1) {
            HOT
        } else if rng.chance(0.5) {
            COOL
        } else {
            COLD
        };
        let p = run.alloc(ALLOC as u32);
        let bytes = rng.bytes(ALLOC as usize);
        run.store.write(p.raw(), 0, &bytes).expect("fill");
        classes.insert(p.raw().raw(), c);
        class.push(c);
        model.push(bytes);
        ptrs.push(p);
        if ptrs.len() % 1024 == 0 {
            run.store.flush().expect("flush");
        }
    }
    let hot: Vec<usize> = (0..n as usize).filter(|&i| class[i] == HOT).collect();
    run.classes = Some(classes);
    run.begin();
    while run.app_bytes < 8 * size {
        let i = if !hot.is_empty() && rng.chance(0.9) {
            hot[rng.below(hot.len() as u64) as usize]
        } else {
            rng.below(n) as usize
        };
        if class[i] == COLD {
            continue;
        }
        let len = rng.range(1, 2 * AVG_WRITE) as usize;
        let off = rng.below(ALLOC - len as u64 + 1) as usize;
        let bytes = rng.bytes(len);
        model[i][off..off + len].copy_from_slice(&bytes);
        run.write(ptrs[i].raw(), off as u32, &bytes);
        run.tick();
    }
    run.flush();
    let check: Vec<(Pointer, Vec<u8>)> = ptrs.iter().map(|p| p.raw()).zip(model).collect();
    run.finish(&check)
}

/// Many logs, each appended to with small records and rotated -- truncated
/// to nothing -- once it reaches 64 KiB.
fn append(dir: &Path, (variant, options): (&str, Options), size: u64) -> Vec<String> {
    const ROTATE: u64 = 64 * KIB;
    let mut run = Run::new(dir, "append", variant, size, options);
    let mut rng = Rng::new(size ^ 7);
    let n = (size / (ROTATE / 2)).max(1);
    let mut logs: Vec<(UniquePointer, Vec<u8>)> = Vec::new();
    for _ in 0..n {
        let p = run.alloc(0);
        logs.push((p, Vec::new()));
    }
    run.begin();
    while run.app_bytes < 8 * size {
        let i = rng.below(n) as usize;
        let len = rng.range(16, 240) as usize;
        let record = rng.bytes(len);
        if (logs[i].1.len() + record.len()) as u64 > ROTATE {
            run.resize(&logs[i].0, 0);
            logs[i].1.clear();
            run.tick();
        }
        let at = logs[i].1.len() as u32;
        run.write(logs[i].0.raw(), at, &record);
        logs[i].1.extend_from_slice(&record);
        run.tick();
    }
    run.flush();
    let check: Vec<(Pointer, Vec<u8>)> = logs.into_iter().map(|(p, v)| (p.raw(), v)).collect();
    run.finish(&check)
}

/// A key-value population of mixed small and large values, churned by
/// replacing values, overwriting them whole, and patching them in place.
fn churn(dir: &Path, size: u64, consolidate: bool) -> Vec<String> {
    let variant = if consolidate { "on" } else { "off" };
    let mut run = Run::new(dir, "churn", variant, size, opts(consolidate));
    let mut rng = Rng::new(size ^ 11);
    let value_size = |rng: &mut Rng| {
        if rng.chance(0.8) {
            rng.range(16, 128)
        } else {
            rng.range(KIB, 8 * KIB)
        }
    };
    let mut values: Vec<(UniquePointer, Vec<u8>)> = Vec::new();
    let mut live = 0u64;
    while live < size {
        let len = value_size(&mut rng);
        let p = run.alloc(len as u32);
        let bytes = rng.bytes(len as usize);
        run.store.write(p.raw(), 0, &bytes).expect("fill");
        live += len;
        values.push((p, bytes));
        if values.len() % 256 == 0 {
            run.store.flush().expect("flush");
        }
    }
    run.begin();
    while run.app_bytes < 8 * size {
        let i = rng.below(values.len() as u64) as usize;
        match rng.below(3) {
            0 => {
                // Replace: free the value and store a new one elsewhere.
                let len = value_size(&mut rng);
                let bytes = rng.bytes(len as usize);
                let p = run.alloc(len as u32);
                run.write(p.raw(), 0, &bytes);
                let (old, _) = std::mem::replace(&mut values[i], (p, bytes));
                run.free(old);
            }
            1 => {
                let bytes = rng.bytes(values[i].1.len());
                run.write(values[i].0.raw(), 0, &bytes);
                values[i].1 = bytes;
            }
            _ => {
                let v = &values[i].1;
                let len = rng.range(1, (v.len() as u64).min(64)) as usize;
                let off = rng.below((v.len() - len) as u64 + 1) as usize;
                let bytes = rng.bytes(len);
                run.write(values[i].0.raw(), off as u32, &bytes);
                values[i].1[off..off + len].copy_from_slice(&bytes);
            }
        }
        run.tick();
    }
    run.flush();
    let check: Vec<(Pointer, Vec<u8>)> = values.into_iter().map(|(p, v)| (p.raw(), v)).collect();
    run.finish(&check)
}

/// Fills the file, deletes three quarters of it at random, then keeps it
/// lightly busy: how far and how fast the file shrinks back.
fn shrink(dir: &Path, size: u64, variant: &str) -> Vec<String> {
    const ALLOC: u64 = 64 * KIB;
    let options = match variant {
        "off" => opts(false),
        "no-compaction" => Options {
            hole_share: 1.0,
            ..Default::default()
        },
        _ => Options::default(),
    };
    let mut run = Run::new(dir, "shrink", variant, size, options);
    let mut rng = Rng::new(size ^ 13);
    let mut kept: Vec<(UniquePointer, Vec<u8>)> = Vec::new();
    let mut doomed = Vec::new();
    for i in 0..(size / ALLOC).max(4) {
        let p = run.alloc(ALLOC as u32);
        let bytes = rng.bytes(ALLOC as usize);
        run.store.write(p.raw(), 0, &bytes).expect("fill");
        if i % 16 == 0 {
            run.store.flush().expect("flush");
        }
        if rng.chance(0.25) {
            kept.push((p, bytes));
        } else {
            doomed.push(p);
        }
    }
    run.begin();
    for p in doomed {
        run.free(p);
    }
    run.flush();
    for _ in 0..400 {
        for _ in 0..100 {
            let i = rng.below(kept.len() as u64) as usize;
            let len = rng.range(1, 64) as usize;
            let off = rng.below(ALLOC - len as u64 + 1) as usize;
            let bytes = rng.bytes(len);
            kept[i].1[off..off + len].copy_from_slice(&bytes);
            let p = kept[i].0.raw();
            run.write(p, off as u32, &bytes);
        }
        run.flush();
    }
    let check: Vec<(Pointer, Vec<u8>)> = kept.into_iter().map(|(p, v)| (p.raw(), v)).collect();
    run.finish(&check)
}

/// The typed layers on top: a `Kladde` holding a hash map from ids to
/// strings, with inserts, updates, appends, and deletes. Measures what an
/// operation costs through the guards, and what the file looks like.
fn typed(dir: &Path, size: u64) -> Vec<String> {
    use kladde::Kladde;
    use kladde_types::{PersistableHashMap, PersistableString};
    type Book = PersistableHashMap<u64, PersistableString>;

    let path = dir.join(format!("typed-{}.kladde", size / KIB));
    let mut book: Kladde<Book> = Kladde::create(&path, Book::new()).expect("create");
    let mut rng = Rng::new(size ^ 17);
    let text = |rng: &mut Rng, lo: u64, hi: u64| -> String {
        let len = rng.range(lo, hi);
        (0..len)
            .map(|_| (b'a' + rng.below(26) as u8) as char)
            .collect()
    };
    let mut keys: Vec<u64> = Vec::new();
    let mut next_key = 0u64;
    let mut live = 0u64;
    while live < size {
        let s = text(&mut rng, 50, 500);
        live += s.len() as u64;
        book.guard()
            .insert(next_key, PersistableString::from(s))
            .expect("insert");
        keys.push(next_key);
        next_key += 1;
    }
    book.flush().expect("flush");
    let base = book.stats();
    let (mut ops, mut app_bytes, mut flushes) = (0u64, 0u64, 0u64);
    let mut rows = Vec::new();
    let mut t = Instant::now();
    while app_bytes < 8 * size {
        let i = rng.below(keys.len() as u64) as usize;
        match rng.below(10) {
            0..=3 => {
                let s = text(&mut rng, 50, 500);
                app_bytes += s.len() as u64;
                book.guard().get_mut(&keys[i]).unwrap().set(s).expect("set");
            }
            4..=6 => {
                let s = text(&mut rng, 8, 64);
                app_bytes += s.len() as u64;
                book.guard()
                    .get_mut(&keys[i])
                    .unwrap()
                    .push_str(&s)
                    .expect("push_str");
            }
            7 | 8 => {
                let s = text(&mut rng, 50, 500);
                app_bytes += s.len() as u64;
                book.guard()
                    .insert(next_key, PersistableString::from(s))
                    .expect("insert");
                keys.push(next_key);
                next_key += 1;
            }
            _ => {
                let key = keys.swap_remove(i);
                book.guard().delete(&key).expect("delete");
            }
        }
        ops += 1;
        if ops.is_multiple_of(OPS_PER_FLUSH) {
            let ops_us = t.elapsed().as_micros();
            let f = Instant::now();
            book.flush().expect("flush");
            let flush_us = f.elapsed().as_micros();
            flushes += 1;
            let s = book.stats();
            rows.push(format!(
                "typed,on,{size},{flushes},{ops},{app_bytes},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{flush_us},{ops_us},0,0,0,0",
                s.file_pages,
                s.data_pages,
                s.table_pages,
                s.free_pages,
                s.live_data_bytes,
                s.live_table_bytes,
                s.allocation_bytes,
                s.allocations,
                s.statements,
                s.fragments,
                s.budget,
                s.data_pages_written - base.data_pages_written,
                s.table_pages_written - base.table_pages_written,
                s.headers_written - base.headers_written,
                s.journal_bytes - base.journal_bytes,
                s.fresh_bytes - base.fresh_bytes,
                s.evacuated_pages - base.evacuated_pages,
                s.evacuated_bytes - base.evacuated_bytes,
                s.free_filled_pages - base.free_filled_pages,
                s.budget_pages - base.budget_pages,
                s.table_rewrites - base.table_rewrites,
                s.window_restated - base.window_restated,
                s.defrag_rewrites - base.defrag_rewrites,
                s.defrag_bytes - base.defrag_bytes,
                s.compaction_flushes - base.compaction_flushes,
                s.truncations - base.truncations,
            ));
            t = Instant::now();
        }
    }
    let expected: Vec<(u64, String)> = keys
        .iter()
        .map(|k| (*k, book.get().get(k).unwrap().to_string()))
        .collect();
    book.close().expect("close");
    let book: Kladde<Book> = Kladde::open(&path).expect("reopen");
    for (k, v) in &expected {
        assert_eq!(book.get().get(k).map(|s| s.to_string()).as_ref(), Some(v));
    }
    assert_eq!(book.get().len(), expected.len());
    drop(book);
    std::fs::remove_file(&path).ok();
    rows
}

/// Writes `rows` to `<name>.csv`, and reports how long they took since `t`.
fn save(out: &Path, name: &str, rows: &[String], t: Instant) {
    let mut f = BufWriter::new(File::create(out.join(format!("{name}.csv"))).expect("create csv"));
    writeln!(f, "{COLUMNS}").unwrap();
    for r in rows {
        writeln!(f, "{r}").unwrap();
    }
    eprintln!(
        "{name}: {} rows in {:.1} s",
        rows.len(),
        t.elapsed().as_secs_f64()
    );
}

/// Variants of the default options, one table per workload so that its
/// variants compare directly: `tuning-uniform.csv` and `tuning-skewed.csv`
/// vary the churn floor and the target fill, `tuning-append.csv` the pages
/// reserved for description defragmentation.
fn tuning(out: &Path, work: &Path, quick: bool) {
    let with = |f: &dyn Fn(&mut Options)| {
        let mut o = Options::default();
        f(&mut o);
        o
    };
    let size = if quick { MIB } else { 8 * MIB };
    for (name, skewed) in [("uniform", false), ("skewed", true)] {
        let t = Instant::now();
        let mut rows = Vec::new();
        for (variant, options) in [
            ("lambda=0.5", with(&|o| o.churn_floor = 0.5)),
            ("lambda=1", Options::default()),
            ("lambda=2", with(&|o| o.churn_floor = 2.0)),
            ("tau=0.6", with(&|o| o.target_fill = 0.6)),
            ("tau=0.5", with(&|o| o.target_fill = 0.5)),
        ] {
            rows.extend(overwrite(work, name, (variant, options), size, skewed));
        }
        save(out, &format!("tuning-{name}"), &rows, t);
    }
    let t = Instant::now();
    let size = if quick { MIB } else { 16 * MIB };
    let mut rows = Vec::new();
    for share in [1, 4, 16] {
        let options = with(&|o| o.defrag_share = share);
        rows.extend(append(work, (&format!("share={share}"), options), size));
    }
    save(out, "tuning-append", &rows, t);
}

fn scenario(out: &Path, work: &Path, name: &str, quick: bool) {
    let t = Instant::now();
    if name == "tuning" {
        return tuning(out, work, quick);
    }
    let sizes: &[u64] = if quick {
        &[MIB]
    } else {
        &[MIB, 8 * MIB, 64 * MIB]
    };
    let small: &[u64] = if quick { &[MIB] } else { &[MIB, 8 * MIB] };
    let rows: Vec<String> = match name {
        "uniform" | "skewed" => {
            let (label, skewed) = if name == "uniform" {
                ("uniform", false)
            } else {
                ("skewed", true)
            };
            let mut rows = Vec::new();
            for &size in sizes {
                rows.extend(overwrite(work, label, on_off(true), size, skewed));
            }
            for &size in small {
                rows.extend(overwrite(work, label, on_off(false), size, skewed));
            }
            rows
        }
        "mixed" => {
            let mut rows = Vec::new();
            for &size in sizes {
                rows.extend(mixed(work, on_off(true), size));
            }
            for &size in small {
                rows.extend(mixed(work, on_off(false), size));
            }
            rows
        }
        "append" => {
            let size = if quick { MIB } else { 16 * MIB };
            let mut rows = append(work, on_off(true), size);
            rows.extend(append(work, on_off(false), size));
            rows
        }
        "churn" => {
            let size = if quick { MIB } else { 16 * MIB };
            let mut rows = churn(work, size, true);
            rows.extend(churn(work, size, false));
            rows
        }
        "shrink" => {
            let size = if quick { 4 * MIB } else { 64 * MIB };
            let mut rows = Vec::new();
            for variant in ["on", "no-compaction", "off"] {
                rows.extend(shrink(work, size, variant));
            }
            rows
        }
        "typed" => typed(work, if quick { MIB } else { 16 * MIB }),
        other => panic!("unknown scenario {other:?}"),
    };
    save(out, name, &rows, t);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(
        args.next()
            .expect("usage: kladde-bench <output directory> [scenario ...]"),
    );
    std::fs::create_dir_all(&out).expect("create the output directory");
    let work = out.join("files");
    std::fs::create_dir_all(&work).expect("create the work directory");
    let mut names: Vec<String> = args.collect();
    let quick = names.iter().any(|n| n == "quick");
    names.retain(|n| n != "quick");
    if names.is_empty() {
        names = [
            "uniform", "skewed", "mixed", "append", "churn", "shrink", "typed", "tuning",
        ]
        .map(String::from)
        .to_vec();
    }
    for name in &names {
        scenario(&out, &work, name, quick);
    }
    std::fs::remove_dir(&work).ok();
}
