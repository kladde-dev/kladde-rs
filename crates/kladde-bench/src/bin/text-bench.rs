//! Benchmarks typing into one long text stored in kladde, as a text editor
//! would.
//!
//! ```text
//! text-bench <output directory> [--quick] [--size BYTES] [--keystrokes N] [--seed N] [--variant NAME] [--focus SHARE] [--focus-moves N] [--journal-budget PAGES] [--shift-decay FACTOR] [--churn-floor λ] [--target-fill τ]
//! ```
//!
//! The text is `--size` bytes, 64 KiB by default, of random words of one to
//! nine lowercase letters, each followed by a space, in one allocation of a
//! [`Store`]: below the typed layers, so that the store alone is measured.
//! It is stored and flushed first, which is not measured, and then edited
//! keystroke by keystroke at a cursor, 100,000 times (10,000 with `--quick`,
//! or `N` with `--keystrokes`):
//!
//! - 85 % type a letter at the cursor, which moves past it;
//! - 10 % delete the byte before the cursor, if there is one;
//! - 4.5 % move the cursor;
//! - 0.5 % paste 200 random letters at the cursor, which moves past them.
//!
//! Every keystroke that changes the text is one `splice` of the store, and
//! one transaction. The cursor moves as a person's does: nine moves in ten
//! land in a window of a tenth of the text, which jumps to a random place
//! every 5000 keystrokes, and the tenth anywhere. `--focus` sets the window's
//! share of the text, `--focus-moves` the keystrokes between its jumps, and
//! `--focus 0` sends every move anywhere.
//!
//! Every 1000 keystrokes the store is flushed and one row is recorded in
//! kladde-bench's format, to `text-<variant>.csv` in the output directory,
//! so that `plot-evaluation.py` in kladde-docs reads it like kladde-bench's
//! tables. The variant is the text's size in KiB, as in `64k`, unless
//! `--variant` says otherwise. `app_bytes` counts the bytes typed and pasted;
//! deletions count nothing. `ops_us` counts the time spent in the store's
//! calls only, not choosing the keystrokes or keeping the model.
//!
//! `--journal-budget` sets the store's [`Options::journal_budget_pages`], so
//! that it also flushes by itself whenever the journal outgrows that many
//! pages, `--shift-decay` its [`Options::shift_decay`], `--churn-floor` its
//! [`Options::churn_floor`], and `--target-fill` its [`Options::target_fill`].
//!
//! At the end, the store is closed and reopened, and the text must be what a
//! model of every keystroke says. A summary on stderr gives what the store
//! wrote per keystroke, by where it went, what it restated and wrote again
//! for the bytes the splices shifted, what the file holds, its size once
//! closed, and the time to reopen it, as the median of 21 opens.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kladde_bench::{row, save, Rng};
use kladde_store::{FileStorage, Options, Stats, Store, UniquePointer, WriteBackend};

/// Keystrokes between two explicit flushes, as in kladde-bench.
const OPS_PER_FLUSH: u64 = 1000;
/// The text's size, by default.
const SIZE: u64 = 64 * 1024;
/// Keystrokes, by default, and with `--quick`.
const KEYSTROKES: u64 = 100_000;
const QUICK_KEYSTROKES: u64 = 10_000;
/// The share of the text that the cursor's moves keep to, by default.
const FOCUS: f64 = 0.1;
/// The share of the moves that keep to it.
const FOCUS_HITS: f64 = 0.9;
/// The keystrokes between two jumps of the focus, by default.
const FOCUS_MOVES: u64 = 5000;
/// The bytes a paste inserts.
const PASTE: usize = 200;
/// What a keystroke does, out of 1000: type, delete, move, and the rest
/// paste.
const TYPE: u64 = 850;
const DELETE: u64 = 950;
const MOVE: u64 = 995;
/// A page's size, for counting the bytes of pages written.
const PAGE: u64 = 4096;

const USAGE: &str = "usage: text-bench <output directory> [--quick] [--size BYTES] [--keystrokes N] [--seed N] [--variant NAME] [--focus SHARE] [--focus-moves N] [--journal-budget PAGES] [--shift-decay FACTOR] [--churn-floor λ] [--target-fill τ]";

struct Bench {
    store: Store,
    text: UniquePointer,
    /// What the text should hold, keystroke by keystroke.
    model: Vec<u8>,
    rng: Rng,
    cursor: usize,
    keystrokes: u64,
    app_bytes: u64,
    /// Time spent in the store's calls since the last row.
    store_time: Duration,
    /// The keystrokes of each kind so far: type, delete, move, paste.
    counts: [u64; 4],
    focus: f64,
    focus_moves: u64,
    /// Where the focus starts, as a share of the text.
    focus_at: f64,
}

impl Bench {
    fn letter(&mut self) -> u8 {
        b'a' + self.rng.below(26) as u8
    }

    /// Replaces the `old_len` bytes at `at` with `new`, in the store and in
    /// the model, timing the store's call only.
    fn splice(&mut self, at: usize, old_len: usize, new: &[u8]) {
        let t = Instant::now();
        self.store
            .splice(&self.text, at as u32, old_len as u32, new)
            .expect("splice");
        self.store_time += t.elapsed();
        self.model.splice(at..at + old_len, new.iter().copied());
    }

    /// Where a move puts the cursor: in the focus, unless this move is one
    /// of those that go anywhere.
    fn target(&mut self) -> usize {
        let n = self.model.len() as u64 + 1;
        if self.focus > 0.0 && self.rng.chance(FOCUS_HITS) {
            let window = ((n as f64 * self.focus) as u64).max(1);
            let start = (self.focus_at * n as f64) as u64;
            return ((start + self.rng.below(window)) % n) as usize;
        }
        self.rng.below(n) as usize
    }

    /// One keystroke, chosen from the mix; the focus jumps first if it is
    /// due.
    fn keystroke(&mut self) {
        if self.focus > 0.0 && self.keystrokes.is_multiple_of(self.focus_moves) {
            self.focus_at = self.rng.below(1 << 20) as f64 / (1 << 20) as f64;
        }
        let x = self.rng.below(1000);
        if x < TYPE {
            let c = [self.letter()];
            self.splice(self.cursor, 0, &c);
            self.cursor += 1;
            self.app_bytes += 1;
            self.counts[0] += 1;
        } else if x < DELETE {
            if self.cursor > 0 {
                self.splice(self.cursor - 1, 1, &[]);
                self.cursor -= 1;
            }
            self.counts[1] += 1;
        } else if x < MOVE {
            self.cursor = self.target();
            self.counts[2] += 1;
        } else {
            let paste: Vec<u8> = (0..PASTE).map(|_| self.letter()).collect();
            self.splice(self.cursor, 0, &paste);
            self.cursor += PASTE;
            self.app_bytes += PASTE as u64;
            self.counts[3] += 1;
        }
        self.keystrokes += 1;
    }
}

/// `size` bytes of random words of one to nine letters, each followed by a
/// space.
fn words(rng: &mut Rng, size: usize) -> Vec<u8> {
    let mut text = Vec::with_capacity(size + 10);
    while text.len() < size {
        for _ in 0..rng.range(1, 9) {
            text.push(b'a' + rng.below(26) as u8);
        }
        text.push(b' ');
    }
    text.truncate(size);
    text
}

fn median(mut v: Vec<u128>) -> u128 {
    v.sort_unstable();
    v.get(v.len() / 2).copied().unwrap_or(0)
}

/// The value after `flag`, parsed, or the usage and an exit.
fn value<T: std::str::FromStr>(args: &mut impl Iterator<Item = String>, flag: &str) -> T {
    args.next().and_then(|s| s.parse().ok()).unwrap_or_else(|| {
        eprintln!("{flag} needs a value\n{USAGE}");
        std::process::exit(2)
    })
}

/// Bytes the store wrote between `base` and `s`: the journal, and every page,
/// headers included.
fn written(s: &Stats, base: &Stats) -> [u64; 4] {
    [
        s.journal_bytes - base.journal_bytes,
        PAGE * (s.data_pages_written - base.data_pages_written),
        PAGE * (s.table_pages_written - base.table_pages_written),
        PAGE * (s.headers_written - base.headers_written),
    ]
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut out, mut quick, mut seed, mut variant) = (None::<PathBuf>, false, 1u64, None);
    let (mut size, mut keystrokes) = (SIZE, None::<u64>);
    let (mut focus, mut focus_moves) = (FOCUS, FOCUS_MOVES);
    let mut opts = Options::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--quick" => quick = true,
            "--size" => size = value(&mut args, "--size"),
            "--keystrokes" => keystrokes = Some(value(&mut args, "--keystrokes")),
            "--seed" => seed = value(&mut args, "--seed"),
            "--variant" => variant = Some(value::<String>(&mut args, "--variant")),
            "--focus" => focus = value(&mut args, "--focus"),
            "--focus-moves" => focus_moves = value(&mut args, "--focus-moves"),
            "--journal-budget" => opts.journal_budget_pages = value(&mut args, "--journal-budget"),
            "--shift-decay" => opts.shift_decay = value(&mut args, "--shift-decay"),
            "--churn-floor" => opts.churn_floor = value(&mut args, "--churn-floor"),
            "--target-fill" => opts.target_fill = value(&mut args, "--target-fill"),
            _ if out.is_none() && !arg.starts_with("--") => out = Some(PathBuf::from(arg)),
            _ => {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
        }
    }
    let Some(out) = out else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    if !(0.0..=1.0).contains(&focus) || focus_moves == 0 || size == 0 {
        eprintln!("--focus is a share from 0 to 1, and --focus-moves and --size are at least 1");
        std::process::exit(2);
    }
    let keystrokes = keystrokes.unwrap_or(if quick { QUICK_KEYSTROKES } else { KEYSTROKES });
    let variant = variant.unwrap_or_else(|| format!("{}k", size.div_ceil(1024)));
    std::fs::create_dir_all(out.join("files")).expect("create the output directory");
    let file = out.join("files").join(format!("text-{variant}.kladde"));
    run(
        &out,
        &file,
        &variant,
        size as usize,
        keystrokes,
        seed,
        focus,
        focus_moves,
        opts,
    );
}

#[allow(clippy::too_many_arguments)]
fn run(
    out: &Path,
    file: &Path,
    variant: &str,
    size: usize,
    keystrokes: u64,
    seed: u64,
    focus: f64,
    focus_moves: u64,
    opts: Options,
) {
    let mut rng = Rng::new(seed);
    let model = words(&mut rng, size);
    let storage = FileStorage::create(file).expect("create the kladde file");
    let store = Store::create(Box::new(storage), opts.clone()).expect("create the store");
    let text = store.alloc(model.len() as u32).expect("alloc");
    store.write(text.raw(), 0, &model).expect("write the text");
    store.flush().expect("flush");
    let base = store.stats();

    let mut bench = Bench {
        store,
        text,
        cursor: model.len() / 2,
        model,
        rng,
        keystrokes: 0,
        app_bytes: 0,
        store_time: Duration::ZERO,
        counts: [0; 4],
        focus,
        focus_moves,
        focus_at: 0.0,
    };
    let started = Instant::now();
    let (mut rows, mut flushes, mut flush_times) = (Vec::new(), 0u64, Vec::new());
    while bench.keystrokes < keystrokes {
        bench.keystroke();
        if bench.keystrokes.is_multiple_of(OPS_PER_FLUSH) || bench.keystrokes == keystrokes {
            let t = Instant::now();
            bench.store.flush().expect("flush");
            let flush_us = t.elapsed().as_micros();
            flushes += 1;
            flush_times.push(flush_us);
            let s = bench.store.stats();
            rows.push(row(
                "text",
                variant,
                size as u64,
                flushes,
                bench.keystrokes,
                bench.app_bytes,
                &s,
                &base,
                flush_us,
                bench.store_time.as_micros(),
            ));
            bench.store_time = Duration::ZERO;
        }
    }

    // What the run cost, before closing changes the counters.
    let s = bench.store.stats();
    let [journal, data, table, headers] = written(&s, &base);
    let total = journal + data + table + headers;
    let all_flushes = s.flushes - base.flushes;
    bench.store.close().expect("close");
    drop(bench.store);
    let closed_bytes = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
    let reopen = || {
        let storage = FileStorage::open(file).expect("reopen");
        let t = Instant::now();
        let store = Store::open(Box::new(storage), opts.clone()).expect("reopen");
        (t.elapsed(), store)
    };
    let (open_time, reopened) = reopen();
    assert!(
        reopened.read_all(bench.text.raw()).expect("read the text") == bench.model,
        "{variant}: the text differs after reopening"
    );
    drop(reopened);
    let mut open_times = vec![open_time.as_micros()];
    for _ in 1..21 {
        open_times.push(reopen().0.as_micros());
    }
    std::fs::remove_file(file).ok();
    std::fs::remove_dir(out.join("files")).ok();
    save(out, &format!("text-{variant}"), &rows, started);

    let n = bench.keystrokes.max(1) as f64;
    let per = |bytes: u64| bytes as f64 / n;
    let [typed, deleted, moved, pasted] = bench.counts;
    eprintln!(
        "{} keystrokes on a text of {size} bytes, now {}: {typed} typed, {deleted} deleted, {moved} moves, {pasted} pastes, storing {} bytes; {all_flushes} flushes, {flushes} of them at rows",
        bench.keystrokes,
        bench.model.len(),
        bench.app_bytes,
    );
    eprintln!(
        "kladde wrote {total} bytes, {:.1} per keystroke: journal {:.1}, data pages {:.1}, table pages {:.1}, headers {:.1}",
        per(total),
        per(journal),
        per(data),
        per(table),
        per(headers),
    );
    eprintln!(
        "shifted bytes per keystroke: {:.1} restated where they lie, {:.1} written again",
        per(s.restated_bytes - base.restated_bytes),
        per(s.merged_bytes - base.merged_bytes),
    );
    eprintln!(
        "the file holds {} statements, {} fragments, {} table pages and {} data pages; flushes take {:.1} ms at the median",
        s.statements,
        s.fragments,
        s.table_pages,
        s.data_pages,
        median(flush_times) as f64 / 1e3,
    );
    eprintln!(
        "closed: the file takes {closed_bytes} bytes, {:.2} times the text; reopening it takes {:.2} ms at the median of 21",
        closed_bytes as f64 / bench.model.len().max(1) as f64,
        median(open_times) as f64 / 1e3,
    );
}
