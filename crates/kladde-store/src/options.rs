//! Tuning knobs. Everything here is policy: two stores with different options
//! write files that read the same.

/// How a [`Store`](crate::Store) schedules and spends its work.
///
/// Every field has a default that suits most files; change one by starting
/// from [`Options::default`].
///
/// ```
/// use kladde_store::{MemoryStorage, Options, Store};
///
/// let opts = Options { journal_budget_pages: 16, ..Default::default() };
/// let store = Store::create(Box::new(MemoryStorage::new()), opts)?;
/// # Ok::<(), kladde_store::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct Options {
    /// The journal pages the journal is kept within: a transaction that
    /// would not fit in what is left of them is preceded by a flush, and a
    /// batch is cut where its next operation would not fit. Only a single
    /// transaction too large for them on its own takes more, and a flush
    /// follows it at once.
    pub journal_budget_pages: u32,
    /// Content runs up to this many bytes are stated `Inline`, up to the
    /// format's ceiling of 125 bytes, which caps any larger value.
    pub inline_threshold: u32,
    /// Whether flushes consolidate at all.
    pub consolidate: bool,
    /// Budgeted consolidation pages per flush to start from.
    pub budget_pages: u32,
    /// The bounds the budget controller moves within.
    pub budget_min: u32,
    pub budget_max: u32,
    /// The live fraction the controller aims the file at.
    pub target_fill: f64,
    /// The churn floor: space a budgeted victim must free per byte written.
    pub churn_floor: f64,
    /// The largest share of a page a flush's pages may close empty with.
    pub theta: f64,
    /// How far `pack` looks ahead for a chunk that fits.
    pub lookahead: usize,
    /// Victims sampled from the sparsest bucket.
    pub sample: usize,
    /// Fragments the rotating window walks per flush.
    pub walk: usize,
    /// Pages per flush reserved for description defragmentation.
    pub defrag_share: u32,
    /// The price per byte a defragmentation writes.
    pub mu: f64,
    /// The share of holes above which compaction mode runs.
    pub hole_share: f64,
    /// Truncate once the reusable tail reaches this many pages.
    pub truncate_tail: u32,
    /// Keep a consolidator state in the file.
    pub consolidator_state: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            journal_budget_pages: 256,
            inline_threshold: 64,
            consolidate: true,
            budget_pages: 8,
            budget_min: 1,
            budget_max: 256,
            target_fill: 0.8,
            churn_floor: 1.0,
            theta: 0.05,
            lookahead: 16,
            sample: 8,
            walk: 512,
            defrag_share: 1,
            mu: 0.02,
            hole_share: 0.25,
            truncate_tail: 16,
            consolidator_state: true,
        }
    }
}
