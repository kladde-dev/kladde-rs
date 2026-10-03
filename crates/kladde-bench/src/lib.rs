//! What kladde-bench's scenarios share with other benchmarks that want to be
//! plotted the same way: the table's columns, one row per flush, a seeded
//! random number generator, and writing a table out.
//!
//! `plot-evaluation.py` in kladde-docs draws figures from any table with
//! these columns, so a benchmark built on [`row`] and [`save`] plots with it
//! unchanged.
//!
//! ```
//! use kladde_bench::{row, Rng, COLUMNS};
//!
//! let mut rng = Rng::new(7);
//! assert!(rng.below(10) < 10);
//!
//! let stats = kladde_store::Stats::default();
//! let line = row("demo", "on", 1024, 1, 1000, 4096, &stats, &stats, 12, 34);
//! assert_eq!(line.split(',').count(), COLUMNS.split(',').count());
//! ```

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Instant;

use kladde_store::Stats;

/// The header of every table: one column per field of a [`row`].
pub const COLUMNS: &str =
    "scenario,variant,size,flush,ops,app_bytes,file_pages,data_pages,table_pages,\
free_pages,live_data,live_table,alloc_bytes,allocations,statements,fragments,budget,\
data_written,table_written,headers_written,journal_bytes,fresh_bytes,evacuated_pages,\
evacuated_bytes,free_filled,budget_pages,table_rewrites,window_restated,defrag_rewrites,\
defrag_bytes,compaction_flushes,truncations,flush_us,ops_us";

/// xorshift64: deterministic, and good enough for workloads.
///
/// The same seed gives the same sequence on every machine, which is what
/// makes a benchmark deterministic but for its times.
///
/// ```
/// let mut a = kladde_bench::Rng::new(1);
/// let mut b = kladde_bench::Rng::new(1);
/// assert_eq!(a.next(), b.next());
/// ```
pub struct Rng(u64);

impl Rng {
    /// A generator started from `seed`; any seed, zero included, is valid.
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    /// The next 64 random bits.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A number in `0..n`, or 0 if `n` is 0.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    /// A number in `lo..=hi`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }

    /// `true` with probability `p`.
    pub fn chance(&mut self, p: f64) -> bool {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }

    /// `len` bytes of filler content.
    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        let seed = self.next();
        (0..len)
            .map(|i| (seed.wrapping_add(i as u64 * 0x9E37) >> 7) as u8)
            .collect()
    }
}

/// One row of a table, in the order of [`COLUMNS`]: the workload's own
/// counters, what the file looks like now (`s`), and the work done since the
/// measured phase began (`s` minus `base`).
///
/// `app_bytes` is what the application wrote, by whatever measure the
/// benchmark defines; the figures divide the file's writes by it.
#[allow(clippy::too_many_arguments)]
pub fn row(
    scenario: &str,
    variant: &str,
    size: u64,
    flushes: u64,
    ops: u64,
    app_bytes: u64,
    s: &Stats,
    base: &Stats,
    flush_us: u128,
    ops_us: u128,
) -> String {
    let b = base;
    format!(
        "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
        scenario,
        variant,
        size,
        flushes,
        ops,
        app_bytes,
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
    )
}

/// Writes `rows` under a [`COLUMNS`] header to `<out>/<name>.csv`, and
/// reports on stderr how many there were and how long they took since `t`.
///
/// # Panics
///
/// If the file cannot be written.
pub fn save(out: &Path, name: &str, rows: &[String], t: Instant) {
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
