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
    /// How consolidation judges, from what a page's losses say about it, when
    /// cleaning the page pays.
    pub ripeness_rule: RipenessRule,
    /// Whether to judge every page afresh in every flush, for experiments.
    ///
    /// A page's ripeness changes with every flush, whether it loses content
    /// or not; by default, it is worked out exactly only when the page
    /// loses content, and follows a close approximation in between. With
    /// this set, every flush works it out exactly for every page that could
    /// be ripe, which costs far more; it exists to measure what the
    /// approximation costs.
    pub exact_ranking: bool,
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
            ripeness_rule: RipenessRule::default(),
            exact_ranking: false,
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

/// How consolidation judges when cleaning a page pays.
///
/// A page's losses tell the store how much of the page's content still dies,
/// and how fast, but only so surely: a page that has lost little could drain
/// slowly or not at all. Both rules clean a page once cleaning it now gains
/// more than waiting; they differ in what they count waiting as worth.
///
/// ```
/// use kladde_store::{MemoryStorage, Options, RipenessRule, Store};
///
/// let opts = Options { ripeness_rule: RipenessRule::OptionToWait, ..Default::default() };
/// let store = Store::create(Box::new(MemoryStorage::new()), opts)?;
/// # Ok::<(), kladde_store::Error>(())
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RipenessRule {
    /// Clean a page once cleaning it now gains more than cleaning it one
    /// flush later, on average over what its content may yet do.
    #[default]
    ExpectedGain,
    /// Clean a page once cleaning it now gains more than keeping the option
    /// to decide later, after more of its losses are known, is worth. It
    /// waits longer on pages of which little is known.
    OptionToWait,
}
