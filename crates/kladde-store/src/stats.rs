//! Counters for measuring what a store does.

/// Work counters since the store was opened, and a snapshot of the file.
///
/// ```
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let p = store.alloc(20_000)?;
/// store.write(p.raw(), 0, &[1; 20_000])?;
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
    /// Address-table pages written by flushes, headers excluded.
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
    /// Survivor bytes evacuation rewrote.
    pub evacuated_bytes: u64,
    /// Victims taken in pages the flush wrote anyway.
    pub free_filled_pages: u64,
    /// Address-table pages the page rewrite drained.
    pub table_rewrites: u64,
    /// Fragments the rotating window restated.
    pub window_restated: u64,
    /// Description defragmentation rewrites executed.
    pub defrag_rewrites: u64,
    /// Bytes description defragmentation rewrote.
    pub defrag_bytes: u64,
    /// Bytes the application wrote into data pages through flushes.
    pub fresh_bytes: u64,

    // A snapshot of the file, filled when the stats are read.
    /// The file's length in pages.
    pub file_pages: u64,
    /// Live data pages.
    pub data_pages: u64,
    /// Live address-table pages, the headers excluded.
    pub table_pages: u64,
    /// Reusable pages, ready or retiring.
    pub free_pages: u64,
    /// Bytes of data pages current state relies on.
    pub live_data_bytes: u64,
    /// Bytes of table pages current state relies on.
    pub live_table_bytes: u64,
    /// Bytes of all live allocations.
    pub allocation_bytes: u64,
    /// Live allocations.
    pub allocations: u64,
    /// Fragments in the fragment map.
    pub fragments: u64,
    /// Live statements.
    pub statements: u64,
    /// The consolidation budget in pages.
    pub budget: u64,
}

impl Stats {
    /// Live bytes over the content capacity of every live page, data and
    /// address table alike; 1 for a file without such pages.
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
