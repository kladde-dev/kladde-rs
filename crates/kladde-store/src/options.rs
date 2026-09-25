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
    /// Flush once the journal segment outgrows this many pages.
    pub journal_budget_pages: u32,
    /// Bytes a batch may hold back before it appends them as one transaction.
    pub batch_bytes: usize,
    /// Content runs up to this many bytes are stated `Inline`.
    pub inline_threshold: u32,
    /// Whether flushes consolidate at all.
    pub consolidate: bool,
    /// Pages a flush may open for consolidation's sake: a cap on its work.
    pub budget_pages: u32,
    /// The live fraction the controller aims the data pages and leaves at,
    /// by moving the price of space.
    pub target_fill: f64,
    /// The price of space to start from: the page writes that one page of
    /// garbage, kept for one flush, is worth. The higher it is, the fuller
    /// the pages that are worth cleaning.
    pub kappa: f64,
    /// How far one flush's miss of the target fill moves the price: by a
    /// factor of `exp(kappa_gain · miss)`. At 0, the price stays where it
    /// starts.
    pub kappa_gain: f64,
    /// The largest share of a page a flush's pages may close empty with; no
    /// page fuller than the rest is ever cleaned.
    pub theta: f64,
    /// How far `pack` looks ahead for a chunk that fits.
    pub lookahead: usize,
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
            batch_bytes: 64 * 1024,
            inline_threshold: 64,
            consolidate: true,
            budget_pages: 256,
            target_fill: 0.75,
            kappa: 0.01,
            kappa_gain: 2.0,
            theta: 0.05,
            lookahead: 16,
            walk: 512,
            defrag_share: 1,
            mu: 0.02,
            hole_share: 0.25,
            truncate_tail: 16,
            consolidator_state: true,
        }
    }
}
