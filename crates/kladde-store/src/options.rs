//! Tuning knobs. Everything here is policy: two stores with different options
//! write files that read the same.

use crate::consts::PAGE_SIZE;

/// How a [`Store`](crate::Store) schedules and spends its work.
///
/// Every field has a default that suits most files; change one by starting
/// from [`Options::default`].
///
/// ```
/// use kladde_store::{MemoryStorage, Options, Store};
///
/// let opts = Options { journal_budget_pages: Some(16), ..Default::default() };
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
    ///
    /// `None`, the default, sizes the budget to the sequence, as
    /// [`journal_budget`](Self::journal_budget) describes; `Some(n)` fixes
    /// it at `n` pages.
    pub journal_budget_pages: Option<u32>,
    /// Whether a flush seals the journal it folds, naming in it where the
    /// next journal begins, so that recovery reaches the next journal from
    /// either header (`spec/journal.md#seals`).
    pub seal: bool,
    /// Whether a flush that a mutation triggers hands its `fsync` and its
    /// header write to a thread of its own, so that the mutation returns
    /// without waiting for the disk, and the next ones append to the next
    /// journal meanwhile. It does so only after sealing, and only with a
    /// storage that [`Storage::try_clone`](crate::Storage::try_clone)
    /// supports; an explicit flush, the next flush, and dropping the store
    /// wait for it.
    pub sync_on_worker: bool,
    /// New runs of bytes up to this many bytes are kept inline in their
    /// span's entry, up to the format's ceiling of 126 bytes.
    pub inline_threshold: u32,
    /// The bytes of a write or splice of at least this many bytes are
    /// written once, into data pages of their own that the journal names,
    /// rather than into the journal and then again by the flush; `u32::MAX`
    /// keeps every write's bytes in the journal.
    ///
    /// Either way the bytes are in the journal's world when the call
    /// returns, and those data pages count toward
    /// [`journal_budget_pages`](Self::journal_budget_pages), so a write
    /// larger than the budget is followed by a flush, which keeps them where
    /// they lie.
    pub payload_threshold: u32,
    /// A span of bytes in a data page shorter than this is written again
    /// whole where an edit cuts into it, rather than split in two.
    pub rewrite_below: u32,
    /// Two neighbouring spans whose entries leave the header for a leaf are
    /// merged into one if they are shorter than this together.
    ///
    /// A larger bound keeps the span table smaller and costs more writes:
    /// every merge writes its spans' bytes again, and leaves the bytes it
    /// replaced for consolidation to clean.
    pub span_min: u32,
    /// Flushes a span's bytes must have stayed unwritten for it to count as
    /// cold, which lets the rotating walk merge it with its neighbours.
    pub cold_flushes: u32,
    /// Spans the rotating walk looks at per flush.
    pub walk: usize,
    /// The price per byte a merge writes, in entry bytes it must retire.
    pub mu: f64,
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
    /// How far packing looks ahead for a chunk that fits.
    pub lookahead: usize,
    /// Victims sampled from the sparsest bucket.
    pub sample: usize,
    /// The share of holes above which compaction mode runs.
    pub hole_share: f64,
    /// Truncate once the reusable tail reaches this many pages.
    pub truncate_tail: u32,
}

impl Options {
    /// The share of the sequence's length that the journal budget is sized
    /// to when [`journal_budget_pages`](Self::journal_budget_pages) is `None`.
    pub const JOURNAL_BUDGET_SHARE: f64 = 0.02;
    /// The fewest journal pages a budget sized to the sequence allows.
    pub const JOURNAL_BUDGET_MIN: u32 = 2;
    /// The most journal pages a budget sized to the sequence allows.
    pub const JOURNAL_BUDGET_MAX: u32 = 10;

    /// The journal budget, in pages, for a sequence of `len` bytes.
    ///
    /// With [`journal_budget_pages`](Self::journal_budget_pages) set, this is
    /// that number. Otherwise it is
    /// [`JOURNAL_BUDGET_SHARE`](Self::JOURNAL_BUDGET_SHARE) of `len` in whole
    /// pages, within [`JOURNAL_BUDGET_MIN`](Self::JOURNAL_BUDGET_MIN) and
    /// [`JOURNAL_BUDGET_MAX`](Self::JOURNAL_BUDGET_MAX): every flush is a
    /// chance to consolidate, and a journal much longer than the share keeps
    /// the cuts its records made, and the pages it occupies, for longer than
    /// it saves flushes. The store sizes the budget to the sequence as it
    /// stands whenever it checks the journal against it.
    ///
    /// ```
    /// use kladde_store::Options;
    ///
    /// let auto = Options::default();
    /// assert_eq!(auto.journal_budget(50_000), 2);
    /// assert_eq!(auto.journal_budget(1_000_000), 4);
    /// assert_eq!(auto.journal_budget(100_000_000), 10);
    /// let fixed = Options { journal_budget_pages: Some(16), ..Default::default() };
    /// assert_eq!(fixed.journal_budget(50_000), 16);
    /// ```
    pub fn journal_budget(&self, len: u64) -> u32 {
        self.journal_budget_pages.unwrap_or_else(|| {
            let pages = (len as f64 * Self::JOURNAL_BUDGET_SHARE / PAGE_SIZE as f64) as u32;
            pages.clamp(Self::JOURNAL_BUDGET_MIN, Self::JOURNAL_BUDGET_MAX)
        })
    }
}

impl Default for Options {
    fn default() -> Self {
        Options {
            journal_budget_pages: None,
            seal: true,
            sync_on_worker: false,
            inline_threshold: 64,
            payload_threshold: 2048,
            rewrite_below: 64,
            span_min: 64,
            cold_flushes: 16,
            walk: 256,
            mu: 0.02,
            consolidate: true,
            budget_pages: 8,
            budget_min: 1,
            budget_max: 256,
            target_fill: 0.8,
            churn_floor: 1.0,
            theta: 0.05,
            lookahead: 16,
            sample: 8,
            hole_share: 0.25,
            truncate_tail: 16,
        }
    }
}
