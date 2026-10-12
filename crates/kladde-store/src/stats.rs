//! Counters for measuring what a store does.

/// Work counters since the store was opened, and a snapshot of the file.
///
/// ```
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// store.splice(0, 0, &[1; 20_000])?;
/// store.flush()?;
/// let s = store.stats();
/// assert_eq!(s.data_pages, 5);
/// assert!(s.live_fraction() > 0.9);
/// # Ok::<(), kladde_store::Error>(())
/// ```
#[derive(Clone, Debug, Default)]
pub struct Stats {
    /// Flushes committed.
    pub flushes: u64,
    /// Transactions appended to the journal.
    pub transactions: u64,
    /// Journal stream bytes appended.
    pub journal_bytes: u64,
    /// Data pages written by flushes.
    pub data_pages_written: u64,
    /// Span-table pages written by flushes, headers excluded.
    pub table_pages_written: u64,
    /// Header pages written.
    pub headers_written: u64,
    /// Pages read after opening.
    pub pages_read: u64,
    /// Times the file grew by a page.
    pub file_grew: u64,
    /// Truncations.
    pub truncations: u64,
    /// Data pages evacuated.
    pub evacuated_pages: u64,
    /// Survivor bytes evacuation wrote again.
    pub evacuated_bytes: u64,
    /// Victims taken in pages the flush wrote anyway.
    pub free_filled_pages: u64,
    /// Data pages opened only to take victims.
    pub budget_pages: u64,
    /// Flushes that ran in compaction mode, moving content off the end of
    /// the file.
    pub compaction_flushes: u64,
    /// Leaves whose live entries a rewrite moved, to drain them.
    pub table_rewrites: u64,
    /// Bytes of new content flushes wrote into data pages.
    pub fresh_bytes: u64,
    /// Entries written, in the header and in leaves.
    pub entries_stated: u64,
    /// Spans split where an edit cut into them.
    pub span_splits: u64,
    /// Bytes of short spans written again where an edit cut into them,
    /// rather than split.
    pub rewritten_bytes: u64,
    /// Merges of short spans as their entries left the header, and the
    /// bytes they wrote again.
    pub evict_merges: u64,
    pub evict_merge_bytes: u64,
    /// Merges of cold spans by the rotating walk, and the bytes they wrote
    /// again.
    pub walk_merges: u64,
    pub walk_merge_bytes: u64,

    // A snapshot of the file, filled when the stats are read.
    /// The file's length in pages.
    pub file_pages: u64,
    /// Live data pages.
    pub data_pages: u64,
    /// Live span-table pages, the headers excluded.
    pub table_pages: u64,
    /// Pages within the file that hold nothing live: reusable, or retired
    /// by the last commit.
    pub free_pages: u64,
    /// Bytes of data pages current state relies on.
    pub live_data_bytes: u64,
    /// Bytes of leaves current state relies on.
    pub live_table_bytes: u64,
    /// The sequence's length.
    pub sequence_bytes: u64,
    /// The schema chain's length.
    pub schema_bytes: u64,
    /// Spans the chains reach, each with one live entry.
    pub spans: u64,
    /// The consolidation budget in pages.
    pub budget: u64,
}

impl Stats {
    /// Live bytes over the content capacity of every live page, data and
    /// span table alike; 1 for a file without such pages.
    ///
    /// See [`Stats`] for an example.
    pub fn live_fraction(&self) -> f64 {
        let pages = self.data_pages + self.table_pages;
        if pages == 0 {
            return 1.0;
        }
        (self.live_data_bytes + self.live_table_bytes) as f64
            / (pages as f64 * crate::consts::MAX_PAGE_CONTENT as f64)
    }
}
