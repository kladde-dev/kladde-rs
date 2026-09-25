//! [`Store`]: the backend a kladde file is driven through.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io::{self, Read, Seek, SeekFrom};

use crate::backend::{Backend, ReadBackend, WriteBackend};
use crate::consts::*;
use crate::error::{corrupt, Error};
use crate::hash::IdMap;
use crate::journal::{recover, Record, Records, SegmentWriter};
use crate::load::{load, pick_header};
use crate::options::Options;
use crate::page::{encode_page, new_page, HeaderFields, PageBuf};
use crate::pointer::{Pointer, UniquePointer};
use crate::state::*;
use crate::stats::Stats;
use crate::storage::Storage;

/// Write-phase geometry of an id this segment touched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Geo {
    Live(u32),
    Freed,
}

/// Hands out ids: recyclable ones lowest first, those whose tombstone is dead
/// before those whose tombstone lives, and fresh ones only when none waits.
#[derive(Debug, Default)]
pub(crate) struct IdAllocator {
    pub next_fresh: u32,
    pub free_dead: BTreeSet<u32>,
    pub free_live: BTreeSet<u32>,
}

impl IdAllocator {
    pub(crate) fn mint(&mut self) -> Result<u32, Error> {
        if let Some(id) = self.free_dead.pop_first() {
            return Ok(id);
        }
        if let Some(id) = self.free_live.pop_first() {
            return Ok(id);
        }
        if self.next_fresh == 0 {
            return Err(Error::IdsExhausted);
        }
        let id = self.next_fresh;
        self.next_fresh = self.next_fresh.wrapping_add(1);
        Ok(id)
    }

    pub fn release(&mut self, id: u32, live_tombstone: bool) {
        if live_tombstone {
            self.free_live.insert(id);
        } else {
            self.free_dead.insert(id);
        }
    }
}

/// The transaction and batch state machine of
/// `impl/transactions-and-batches.md`. `ops` holds what is not appended yet;
/// in `InTxInBatch`, `ops[..ready]` is the batch's ready run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tx {
    Immediate,
    InBatch {
        batch: u32,
    },
    InTransaction {
        depth: u32,
        batch: u32,
    },
    InTxInBatch {
        depth: u32,
        batch: u32,
        ready: usize,
    },
}

pub(crate) struct Inner {
    pub storage: Box<dyn Storage>,
    pub opts: Options,
    pub poisoned: bool,
    // The committed file.
    pub epoch: u64,
    pub header: HeaderFields,
    pub header_slot: u32,
    pub state: State,
    pub table_pages: IdMap<Box<PageBuf>>,
    pub children: IdMap<Vec<u32>>,
    pub ready: BTreeSet<u32>,
    pub retiring: Vec<u32>,
    pub file_pages: u32,
    // The write phase.
    pub segment: SegmentWriter,
    pub segment_records: Records,
    pub ops: Records,
    pub tx: Tx,
    pub geometry: IdMap<Geo>,
    pub ids: IdAllocator,
    pub first_flush: bool,
    pub must_flush_first: bool,
    /// Data pages read since the last flush or load.
    pub cache: IdMap<Box<PageBuf>>,
    /// Table pages whose statements this flush already dropped physically.
    pub flush_rewritten: crate::hash::IdSet,
    pub cons: crate::consolidate::ConsState,
    pub stats: Stats,
}

/// The storage backend of one kladde file.
///
/// A `Store` holds the file's committed state in memory and records every
/// mutation in the file's journal before the mutating call returns; a flush
/// folds the journal into fresh pages. All mutation goes through the
/// [`WriteBackend`] methods, which take `&self`; reads go through
/// [`ReadBackend`] and see the state as of the last flush.
///
/// ```
/// use kladde_store::{Backend, MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default()).unwrap();
/// let p = store.alloc(8).unwrap();
/// store.write(p.raw(), 0, b"kladde").unwrap();
/// assert_eq!(store.size(p.raw()).unwrap(), 8);
/// store.flush().unwrap();
/// assert_eq!(store.read_all(p.raw()).unwrap(), b"kladde\0\0");
/// ```
pub struct Store {
    pub(crate) inner: RefCell<Inner>,
}

impl Store {
    /// Creates an empty kladde file in `storage`, discarding whatever it held.
    ///
    /// The new file is durable when this returns.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// assert!(store.root().is_none());
    /// assert!(store.allocations().is_empty());
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn create(mut storage: Box<dyn Storage>, opts: Options) -> Result<Store, Error> {
        storage.set_len(0)?;
        storage.set_len(2 * PAGE_SIZE as u64)?;
        let header = HeaderFields {
            journal_pointer: 2,
            ..Default::default()
        };
        let mut page = new_page();
        encode_page(&mut page, Some(&header), KIND_ADDRESS_TABLE, 0, &[0]);
        storage.write_at(&page[..], 0)?;
        storage.sync()?;
        Store::open(storage, opts)
    }

    /// Opens the kladde file in `storage`, recovering every transaction its
    /// journal holds.
    ///
    /// Recovery is part of opening: if the journal holds transactions that
    /// no flush folded, because the process that wrote them died, they are
    /// flushed before this returns, so the store opens at the state after the
    /// last complete transaction. Reads the whole address table, and costs
    /// `O(statements · log statements)` time.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let storage = MemoryStorage::new();
    /// let store = Store::create(Box::new(storage.clone()), Default::default())?;
    /// let p = store.alloc(4)?;
    /// store.write(p.raw(), 0, b"note")?;
    /// // No flush: the process "dies" here, and the journal keeps the write.
    /// drop(store);
    /// let store = Store::open(Box::new(MemoryStorage::from_image(storage.image())), Default::default())?;
    /// assert_eq!(store.read_all(p.raw())?, b"note");
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn open(mut storage: Box<dyn Storage>, opts: Options) -> Result<Store, Error> {
        let header = pick_header(&*storage)?;
        // Retires the older world: after this, every page the governing header
        // does not reach is reusable.
        storage.sync()?;
        let loaded = load(&*storage, header)?;
        let epoch = loaded.header.epoch;
        let recovered = recover(&*storage, epoch + 1, loaded.header.fields.journal_pointer)?;
        let mut state = loaded.state;
        let file_pages = loaded.file_pages;
        state.ensure_page(file_pages.saturating_sub(1).max(1));
        let mut ready = BTreeSet::new();
        for &p in &recovered.pages {
            state.pages[p as usize].state = PageState::Journal;
        }
        // The page the header names for the journal is in its world, written
        // to or not.
        let jp = loaded.header.fields.journal_pointer;
        if jp >= 2 && jp < file_pages {
            state.pages[jp as usize].state = PageState::Journal;
        }
        for p in 2..file_pages {
            if state.pages[p as usize].state == PageState::Free {
                ready.insert(p);
            }
        }
        let mut ids = IdAllocator::default();
        let mut max_id = 0u32;
        for &id in state.allocs.keys() {
            max_id = max_id.max(id);
        }
        for (&id, r) in &state.recyclable {
            max_id = max_id.max(id);
            ids.release(id, r.tombstone.is_some());
        }
        // Ids the recovered journal names are in use too, or will be once it
        // is folded: none of them may be handed out again.
        for r in &recovered.records.records {
            for id in r.ids() {
                max_id = max_id.max(id);
                ids.free_dead.remove(&id);
                ids.free_live.remove(&id);
            }
        }
        ids.next_fresh = max_id.checked_add(1).unwrap_or(0).max(1);
        let journal_pointer = loaded.header.fields.journal_pointer;
        let mut segment = SegmentWriter::new(epoch + 1, journal_pointer);
        segment.pages = if recovered.pages.is_empty() {
            vec![journal_pointer]
        } else {
            recovered.pages.clone()
        };
        let must_flush_first = recovered.transactions > 0;
        let mut inner = Inner {
            storage,
            opts,
            poisoned: false,
            epoch,
            header: loaded.header.fields.clone(),
            header_slot: loaded.header.slot,
            state,
            table_pages: loaded.table_pages,
            children: loaded.children,
            ready,
            retiring: Vec::new(),
            file_pages,
            segment,
            segment_records: recovered.records,
            ops: Records::new(),
            tx: Tx::Immediate,
            geometry: IdMap::default(),
            ids,
            first_flush: true,
            must_flush_first,
            cache: IdMap::default(),
            flush_rewritten: Default::default(),
            cons: Default::default(),
            stats: Stats::default(),
        };
        inner.seed_after_load()?;
        // Replay: a recovered journal is folded now, so that what the
        // application loads is the state at its last transaction.
        if inner.must_flush_first {
            inner.flush_now()?;
        }
        Ok(Store {
            inner: RefCell::new(inner),
        })
    }

    /// Every live allocation's id and size, in id order, as of the last
    /// flush.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// let p = store.alloc(10)?;
    /// assert!(store.allocations().is_empty());
    /// store.flush()?;
    /// assert_eq!(store.allocations(), [(p.raw(), 10)]);
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn allocations(&self) -> Vec<(Pointer, u32)> {
        let i = self.inner.borrow();
        let own = i.header.consolidator_state;
        let mut v: Vec<(Pointer, u32)> = i
            .state
            .allocs
            .iter()
            .filter(|(&id, _)| id != own)
            .map(|(&id, m)| (Pointer::from_raw(id).unwrap(), m.size))
            .collect();
        v.sort_unstable_by_key(|(p, _)| p.raw());
        v
    }

    fn with<R>(&self, f: impl FnOnce(&mut Inner) -> Result<R, Error>) -> Result<R, Error> {
        let mut inner = self.inner.borrow_mut();
        if inner.poisoned {
            return Err(Error::Poisoned);
        }
        f(&mut inner)
    }

    /// Folds the journal into the file, consolidating along the way, and
    /// commits.
    ///
    /// Afterwards everything recorded so far, except the operations of an
    /// open transaction, survives a power cut, and reads see it. Costs one
    /// `fsync` and one header write, plus the pages the flush writes. A
    /// failed flush poisons the store: drop it and open the file again, which
    /// recovers the state after the last complete transaction.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// let p = store.alloc(2)?;
    /// store.write(p.raw(), 0, b"hi")?;
    /// store.flush()?;
    /// assert_eq!(store.read_all(p.raw())?, b"hi");
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn flush(&self) -> Result<(), Error> {
        self.with(|i| i.flush_now())
    }

    /// Flushes, then shrinks the file to end at its last page that the
    /// current or the previous commit still needs.
    ///
    /// The store stays usable; closing is flushing plus giving unused space
    /// back to the file system. Pages that only the previous commit needs are
    /// its fallback until the next commit, so they can end the file for one
    /// more flush.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store, Storage, WriteBackend};
    ///
    /// let storage = MemoryStorage::new();
    /// let store = Store::create(Box::new(storage.clone()), Default::default())?;
    /// let p = store.alloc(100_000)?;
    /// store.write(p.raw(), 0, &[1; 100_000])?;
    /// store.flush()?;
    /// store.free(p)?;
    /// store.flush()?;
    /// let before = storage.len()?;
    /// store.close()?;
    /// assert!(storage.len()? < before);
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn close(&self) -> Result<(), Error> {
        self.with(|i| {
            i.flush_now()?;
            i.truncate(true)
        })
    }

    /// The header's root allocation, or `None` for a new file.
    ///
    /// See [`set_roots`](Self::set_roots) for an example.
    pub fn root(&self) -> Option<Pointer> {
        Pointer::from_raw(self.inner.borrow().header.root_allocation)
    }

    /// The header's descriptor-table allocation, or `None` for a new file.
    ///
    /// See [`set_roots`](Self::set_roots) for an example.
    pub fn schema_table(&self) -> Option<Pointer> {
        Pointer::from_raw(self.inner.borrow().header.schema_table)
    }

    /// The root fingerprint the header records; all zeros for a new file.
    ///
    /// See [`set_roots`](Self::set_roots) for an example.
    pub fn root_fingerprint(&self) -> [u8; 16] {
        self.inner.borrow().header.root_fingerprint
    }

    /// Sets what the header records as root allocation, descriptor table, and
    /// root fingerprint.
    ///
    /// Takes effect at once for [`root`](Self::root) and its siblings, and
    /// reaches the file with the next flush.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let storage = MemoryStorage::new();
    /// let store = Store::create(Box::new(storage.clone()), Default::default())?;
    /// let root = store.alloc(8)?;
    /// let table = store.alloc(8)?;
    /// store.set_roots(root.raw(), table.raw(), [7; 16]);
    /// store.flush()?;
    /// let store = Store::open(Box::new(storage), Default::default())?;
    /// assert_eq!(store.root(), Some(root.raw()));
    /// assert_eq!(store.schema_table(), Some(table.raw()));
    /// assert_eq!(store.root_fingerprint(), [7; 16]);
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn set_roots(&self, root: Pointer, schema_table: Pointer, fingerprint: [u8; 16]) {
        let mut i = self.inner.borrow_mut();
        i.header.root_allocation = root.raw();
        i.header.schema_table = schema_table.raw();
        i.header.root_fingerprint = fingerprint;
    }

    /// Reads the whole of allocation `p` as of the last flush.
    ///
    /// Fails with [`Error::DanglingPointer`] if `p` did not exist at the last
    /// flush.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// let p = store.alloc(3)?;
    /// store.write(p.raw(), 1, b"x")?;
    /// store.flush()?;
    /// assert_eq!(store.read_all(p.raw())?, b"\0x\0");
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn read_all(&self, p: Pointer) -> Result<Vec<u8>, Error> {
        let mut i = self.inner.borrow_mut();
        let size = i
            .state
            .allocs
            .get(&p.raw())
            .ok_or(Error::DanglingPointer(p.raw()))?
            .size;
        let mut out = vec![0u8; size as usize];
        i.read_committed(p.raw(), 0, &mut out)?;
        Ok(out)
    }

    /// Drops the data pages cached by reads; later reads fetch them again.
    ///
    /// Reads cache every data page they touch until the next flush. Call this
    /// after loading a large value to give that memory back early.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// store.end_load();
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn end_load(&self) {
        self.inner.borrow_mut().cache.clear();
    }

    /// Counters describing the file and the work done on it since opening.
    ///
    /// Costs `O(pages)` time, since it takes a snapshot of every page's fill.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// store.alloc(10_000)?;
    /// store.flush()?;
    /// let s = store.stats();
    /// assert_eq!(s.flushes, 1);
    /// assert_eq!(s.allocation_bytes, 10_000);
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn stats(&self) -> Stats {
        let i = self.inner.borrow();
        let mut s = i.stats.clone();
        i.fill_stats(&mut s);
        s
    }

    /// A description of how allocation `p` resolves, fragment by fragment:
    /// for debugging.
    #[doc(hidden)]
    pub fn describe(&self, p: Pointer) -> String {
        let i = self.inner.borrow();
        let id = p.raw();
        let mut out = format!("id {id}: {:?}\n", i.state.allocs.get(&id));
        for (&k, f) in i.state.frags.range(key(id, 0)..=key(id, u32::MAX)) {
            let stmt = f.stmt().map(|s| {
                let page = i.state.slab.page(s);
                format!(
                    "{:?} in page {page} ({:?}, epoch {}), pins {}",
                    i.state.slab.kinds[s.idx()],
                    i.state.pages[page as usize].state,
                    i.state.pages[page as usize].epoch,
                    i.state.slab.pins[s.idx()]
                )
            });
            out += &format!("  {} {:?} {}\n", koff(k), f, stmt.unwrap_or_default());
        }
        out
    }

    /// Every live data page and leaf emptier than the fill survivors are
    /// packed at, with its estimate: `(page, rate at the estimate's epoch,
    /// that epoch)`. For tests.
    #[doc(hidden)]
    pub fn describe_drains(&self) -> Vec<(u32, f32, u64)> {
        let i = self.inner.borrow();
        let packed = crate::ripeness::packed_fill(i.opts.theta);
        (2..i.state.pages.len())
            .filter_map(|p| {
                let info = &i.state.pages[p];
                let live = matches!(info.state, PageState::Data | PageState::Table);
                (live && info.coverage > 0 && (info.coverage as f64) < packed).then_some((
                    p as u32,
                    info.drain.rho,
                    info.drain.at,
                ))
            })
            .collect()
    }

    /// Checks the in-memory invariants, panicking at the first violation.
    ///
    /// Takes time linear in the size of the in-memory state; meant for
    /// tests.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// store.alloc(10)?;
    /// store.flush()?;
    /// store.check();
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn check(&self) {
        let i = self.inner.borrow();
        i.state.check();
    }

    /// Opens a transaction: the operations recorded until the matching
    /// [`end_transaction`](Self::end_transaction) reach the journal as one,
    /// so a crash keeps all of them or none.
    ///
    /// Transactions nest, and only the outermost end appends. Reads still see
    /// the state of the last flush, and a flush in the middle of a
    /// transaction folds only what was appended before it.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let storage = MemoryStorage::new();
    /// let store = Store::create(Box::new(storage.clone()), Default::default())?;
    /// let from = store.alloc(1)?;
    /// let to = store.alloc(1)?;
    /// store.flush()?;
    /// store.begin_transaction()?;
    /// store.write(from.raw(), 0, &[0])?;
    /// store.write(to.raw(), 0, &[1])?;
    /// store.end_transaction()?;
    /// // Both writes are in the journal now, or would have been lost together.
    /// let store = Store::open(Box::new(storage), Default::default())?;
    /// assert_eq!(store.read_all(to.raw())?, [1]);
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn begin_transaction(&self) -> Result<(), Error> {
        self.with(|i| {
            i.tx = match i.tx {
                Tx::Immediate => Tx::InTransaction { depth: 1, batch: 0 },
                Tx::InBatch { batch } => Tx::InTxInBatch {
                    depth: 1,
                    batch,
                    ready: i.ops.len(),
                },
                Tx::InTransaction { depth, batch } => Tx::InTransaction {
                    depth: depth + 1,
                    batch,
                },
                Tx::InTxInBatch {
                    depth,
                    batch,
                    ready,
                } => Tx::InTxInBatch {
                    depth: depth + 1,
                    batch,
                    ready,
                },
            };
            Ok(())
        })
    }

    /// Ends the innermost transaction; ending the outermost appends its
    /// operations to the journal, unless a batch holds them back.
    ///
    /// Fails if no transaction is open, or if a batch opened inside the
    /// transaction is still open. See
    /// [`begin_transaction`](Self::begin_transaction) for an example.
    pub fn end_transaction(&self) -> Result<(), Error> {
        self.with(|i| {
            match i.tx {
                Tx::InTransaction { depth: 1, batch: 0 } => {
                    i.tx = Tx::Immediate;
                    i.append_ops(i.ops.len())?;
                    i.maybe_auto_flush()?;
                }
                Tx::InTransaction { depth: 1, .. } => {
                    return Err(Error::Corrupt(
                        "a batch opened inside a transaction is still open".into(),
                    ))
                }
                Tx::InTransaction { depth, batch } => {
                    i.tx = Tx::InTransaction {
                        depth: depth - 1,
                        batch,
                    }
                }
                Tx::InTxInBatch {
                    depth: 1, batch, ..
                } => {
                    i.tx = Tx::InBatch { batch };
                    i.maybe_append_batch()?;
                }
                Tx::InTxInBatch {
                    depth,
                    batch,
                    ready,
                } => {
                    i.tx = Tx::InTxInBatch {
                        depth: depth - 1,
                        batch,
                        ready,
                    }
                }
                _ => return Err(Error::Corrupt("no transaction is open".into())),
            }
            Ok(())
        })
    }

    /// Discards the operations of the open transaction and poisons the store.
    ///
    /// Whatever in-memory values the discarded operations changed no longer
    /// match the file, so every later call fails with [`Error::Poisoned`];
    /// opening the file again gives the state before the transaction.
    ///
    /// ```
    /// use kladde_store::{Error, MemoryStorage, Store, WriteBackend};
    ///
    /// let storage = MemoryStorage::new();
    /// let store = Store::create(Box::new(storage.clone()), Default::default())?;
    /// let p = store.alloc(1)?;
    /// store.flush()?;
    /// store.begin_transaction()?;
    /// store.write(p.raw(), 0, b"x")?;
    /// store.abandon_transaction();
    /// assert!(matches!(store.flush(), Err(Error::Poisoned)));
    /// let store = Store::open(Box::new(storage), Default::default())?;
    /// assert_eq!(store.read_all(p.raw())?, [0]);
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn abandon_transaction(&self) {
        let mut i = self.inner.borrow_mut();
        let keep = match i.tx {
            Tx::InTxInBatch { ready, .. } => ready,
            Tx::InTransaction { .. } => 0,
            _ => i.ops.len(),
        };
        i.ops.records.truncate(keep);
        i.poisoned = true;
    }

    /// Opens a batch: the operations recorded until the matching
    /// [`end_batch`](Self::end_batch) are held back and appended to the
    /// journal in few, large pieces.
    ///
    /// A piece is appended when the batch ends, or whenever what it holds
    /// outgrows [`Options::batch_bytes`]. A batch saves journal writes and is
    /// no transaction: a crash may keep some of its pieces and lose the rest,
    /// though never part of a transaction inside it. Batches nest, and only
    /// the outermost end appends.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// let p = store.alloc(1000)?;
    /// store.begin_batch()?;
    /// for i in 0..1000u32 {
    ///     store.write(p.raw(), i, &[i as u8])?;
    /// }
    /// store.end_batch()?;
    /// assert_eq!(store.stats().transactions, 2);
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn begin_batch(&self) -> Result<(), Error> {
        self.with(|i| {
            i.tx = match i.tx {
                Tx::Immediate => Tx::InBatch { batch: 1 },
                Tx::InBatch { batch } => Tx::InBatch { batch: batch + 1 },
                Tx::InTransaction { depth, batch } => Tx::InTransaction {
                    depth,
                    batch: batch + 1,
                },
                Tx::InTxInBatch {
                    depth,
                    batch,
                    ready,
                } => Tx::InTxInBatch {
                    depth,
                    batch: batch + 1,
                    ready,
                },
            };
            Ok(())
        })
    }

    /// Ends the innermost batch; ending the outermost appends what it held
    /// back.
    ///
    /// Fails if no batch is open, or if a transaction opened inside the batch
    /// is still open. See [`begin_batch`](Self::begin_batch) for an example.
    pub fn end_batch(&self) -> Result<(), Error> {
        self.with(|i| {
            match i.tx {
                Tx::InBatch { batch: 1 } => {
                    i.tx = Tx::Immediate;
                    i.append_ops(i.ops.len())?;
                    i.maybe_auto_flush()?;
                }
                Tx::InBatch { batch } => i.tx = Tx::InBatch { batch: batch - 1 },
                Tx::InTransaction { depth, batch } if batch > 0 => {
                    i.tx = Tx::InTransaction {
                        depth,
                        batch: batch - 1,
                    }
                }
                Tx::InTxInBatch {
                    depth,
                    batch,
                    ready,
                } if batch > 1 => {
                    i.tx = Tx::InTxInBatch {
                        depth,
                        batch: batch - 1,
                        ready,
                    }
                }
                _ => {
                    return Err(Error::Corrupt(
                        "no batch is open, or a transaction inside it is".into(),
                    ))
                }
            }
            Ok(())
        })
    }

    /// Whether an earlier failure poisoned the store.
    ///
    /// A poisoned store fails every call with [`Error::Poisoned`]; drop it and
    /// open the file again.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store};
    ///
    /// let storage = MemoryStorage::new();
    /// let store = Store::create(Box::new(storage.clone()), Default::default())?;
    /// storage.fail_syncs();
    /// assert!(store.flush().is_err());
    /// assert!(store.is_poisoned());
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn is_poisoned(&self) -> bool {
        self.inner.borrow().poisoned
    }

    /// Poisons the store, for a caller that can no longer vouch for the
    /// in-memory values mirroring it.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// store.poison();
    /// assert!(store.flush().is_err());
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn poison(&self) {
        self.inner.borrow_mut().poisoned = true;
    }
}

impl Inner {
    // ------------------------------------------------------------ geometry

    /// The size of `id` as the write phase sees it, or `None` if it does not
    /// exist.
    pub fn live_size(&self, id: u32) -> Option<u32> {
        match self.geometry.get(&id) {
            Some(Geo::Live(s)) => Some(*s),
            Some(Geo::Freed) => None,
            None => self.state.allocs.get(&id).map(|m| m.size),
        }
    }

    fn apply_geometry(&mut self, r: &Record) {
        match *r {
            Record::Free { id } => {
                self.geometry.insert(id, Geo::Freed);
            }
            Record::Resize { id, size } => {
                self.geometry.insert(id, Geo::Live(size));
            }
            Record::Write { id, offset, bytes } => {
                let s = self.live_size(id).unwrap_or(0);
                self.geometry
                    .insert(id, Geo::Live(s.max(offset + bytes.len)));
            }
            Record::Splice {
                id,
                offset,
                old_len,
                bytes,
            } => {
                let s = self.live_size(id).unwrap_or(0).max(offset + old_len);
                self.geometry.insert(id, Geo::Live(s - old_len + bytes.len));
            }
            Record::Copy {
                len,
                dst,
                dst_offset,
                ..
            }
            | Record::Move {
                len,
                dst,
                dst_offset,
                ..
            } => {
                let s = self.live_size(dst).unwrap_or(0);
                self.geometry
                    .insert(dst, Geo::Live(s.max(dst_offset + len)));
            }
        }
    }

    /// Recomputes the write-phase geometry from the operations still buffered.
    /// Recomputes the write-phase geometry from the operations recorded since
    /// the last flush: those appended already, then those still buffered.
    pub fn repopulate_geometry(&mut self) {
        self.geometry.clear();
        let appended = std::mem::take(&mut self.segment_records.records);
        let buffered = std::mem::take(&mut self.ops.records);
        for r in appended.iter().chain(&buffered) {
            self.apply_geometry(r);
        }
        self.segment_records.records = appended;
        self.ops.records = buffered;
    }

    // ------------------------------------------------------------ recording

    /// Records one operation, appending it when the state machine says so.
    fn record(&mut self, r: Record) -> Result<(), Error> {
        self.apply_geometry(&r);
        self.ops.records.push(r);
        match self.tx {
            Tx::Immediate => {
                self.append_ops(self.ops.len())?;
                self.maybe_auto_flush()
            }
            Tx::InBatch { .. } => self.maybe_append_batch(),
            _ => Ok(()),
        }
    }

    fn maybe_append_batch(&mut self) -> Result<(), Error> {
        if self.ops.arena.len() + self.ops.len() * 8 >= self.opts.batch_bytes {
            self.append_ops(self.ops.len())?;
            self.maybe_auto_flush()?;
        }
        Ok(())
    }

    /// Appends `ops[..n]` as one transaction.
    pub fn append_ops(&mut self, n: usize) -> Result<(), Error> {
        if n == 0 {
            return Ok(());
        }
        if self.must_flush_first {
            // A recovered, non-empty journal is folded before anything is
            // appended to it (`spec/journal.md#the-start-of-a-session`).
            self.flush_now()?;
        }
        let mut payload = Vec::new();
        self.ops.encode(0..n, &mut payload);
        self.ensure_segment_page()?;
        let Inner {
            storage,
            segment,
            ready,
            state,
            file_pages,
            stats,
            ..
        } = self;
        let mut alloc = |s: &mut dyn Storage| -> Result<u32, Error> {
            let p = take_page(ready, state, file_pages, s, stats)?;
            state.pages[p as usize].state = PageState::Journal;
            Ok(p)
        };
        let before = segment.bytes;
        if let Err(e) = segment.append(&payload, &mut **storage, &mut alloc) {
            self.poisoned = true;
            return Err(e);
        }
        self.stats.journal_bytes += segment_delta(before, self.segment.bytes);
        self.stats.transactions += 1;
        // Move the appended records into the segment the next flush folds.
        let appended = Records {
            records: self.ops.records[..n].to_vec(),
            arena: Vec::new(),
        };
        let mut moved = Records::new();
        for r in &appended.records {
            moved.push_from(*r, &self.ops);
        }
        self.segment_records.extend_from(&moved);
        let rest: Vec<Record> = self.ops.records[n..].to_vec();
        let mut kept = Records::new();
        for r in rest {
            kept.push_from(r, &self.ops);
        }
        self.ops = kept;
        if let Tx::InTxInBatch {
            depth,
            batch,
            ready,
        } = self.tx
        {
            self.tx = Tx::InTxInBatch {
                depth,
                batch,
                ready: ready.saturating_sub(n),
            };
        }
        Ok(())
    }

    /// Makes sure the page the segment appends to exists in the file.
    fn ensure_segment_page(&mut self) -> Result<(), Error> {
        let p = self.segment.current_page();
        if p >= self.file_pages {
            let len = (p as u64 + 1) * PAGE_SIZE as u64;
            self.storage.set_len(len)?;
            for q in self.file_pages..p {
                self.state.ensure_page(q);
                if q >= 2 && self.state.pages[q as usize].state == PageState::Free {
                    self.ready.insert(q);
                }
            }
            self.file_pages = p + 1;
            self.state.ensure_page(p);
        }
        self.ready.remove(&p);
        self.state.pages[p as usize].state = PageState::Journal;
        Ok(())
    }

    pub fn maybe_auto_flush(&mut self) -> Result<(), Error> {
        if self.segment.pages.len() as u32 > self.opts.journal_budget_pages
            && self.tx == Tx::Immediate
            || (self.segment.pages.len() as u32 > self.opts.journal_budget_pages
                && matches!(self.tx, Tx::InBatch { .. })
                && self.ops.is_empty())
        {
            self.flush_now()?;
        }
        Ok(())
    }

    // ------------------------------------------------------------ reading

    /// Reads committed bytes of `id` at `offset` into `out`; bytes past the
    /// size read as zero.
    pub fn read_committed(&mut self, id: u32, offset: u32, out: &mut [u8]) -> Result<(), Error> {
        let size = self.state.size_of(id);
        let mut pos = offset;
        let end = offset as u64 + out.len() as u64;
        let mut done = 0usize;
        while (pos as u64) < end {
            if pos >= size {
                out[done..].fill(0);
                break;
            }
            let (k, f, fend) = self
                .state
                .resolve(id, pos)
                .ok_or_else(|| corrupt("a hole in the fragment map"))?;
            let n = ((fend.min(end.min(u32::MAX as u64) as u32)) - pos) as usize;
            let rel = pos - koff(k);
            self.read_fragment(f, rel, &mut out[done..done + n])?;
            done += n;
            pos += n as u32;
        }
        Ok(())
    }

    /// Reads `out.len()` bytes of fragment `f`, from `rel` bytes into it.
    pub fn read_fragment(&mut self, f: Fragment, rel: u32, out: &mut [u8]) -> Result<(), Error> {
        match f {
            Fragment::Bytes { page, offset, .. } => {
                self.read_page_bytes(page, offset as usize + rel as usize, out)
            }
            Fragment::ZeroExplicitly { .. } | Fragment::ZeroByDefault => {
                out.fill(0);
                Ok(())
            }
            Fragment::Pending(p) => {
                let pe = self.state.pending[p as usize].advanced(rel);
                match pe.origin {
                    crate::state::Origin::Arena(a) => {
                        out.copy_from_slice(
                            &self.segment_records.arena[a as usize..a as usize + out.len()],
                        );
                        Ok(())
                    }
                    crate::state::Origin::File(addr) => {
                        let (page, off) = split_address(addr);
                        self.read_page_bytes(page, off as usize, out)
                    }
                    crate::state::Origin::None => {
                        out.fill(0);
                        Ok(())
                    }
                }
            }
        }
    }

    /// Reads bytes of a page: a resident table page, or a data page through
    /// the cache.
    pub fn read_page_bytes(&mut self, page: u32, at: usize, out: &mut [u8]) -> Result<(), Error> {
        if let Some(buf) = self.table_pages.get(&page) {
            out.copy_from_slice(&buf[at..at + out.len()]);
            return Ok(());
        }
        if !self.cache.contains_key(&page) {
            let mut buf = new_page();
            self.storage
                .read_at(&mut buf[..], page as u64 * PAGE_SIZE as u64)?;
            self.stats.pages_read += 1;
            self.cache.insert(page, buf);
        }
        let buf = &self.cache[&page];
        out.copy_from_slice(&buf[at..at + out.len()]);
        Ok(())
    }

    /// Checks and normalizes an operation's pointer.
    fn existing(&self, id: u32) -> Result<u32, Error> {
        self.live_size(id).ok_or(Error::DanglingPointer(id))
    }
}

fn segment_delta(before: u64, after: u64) -> u64 {
    after - before
}

/// Takes the lowest reusable page, or grows the file by one page.
pub(crate) fn take_page(
    ready: &mut BTreeSet<u32>,
    state: &mut State,
    file_pages: &mut u32,
    storage: &mut dyn Storage,
    stats: &mut Stats,
) -> Result<u32, Error> {
    if let Some(p) = ready.pop_first() {
        state.pages[p as usize].state = PageState::Claimed;
        return Ok(p);
    }
    let p = *file_pages;
    if p == u32::MAX {
        return Err(Error::OutOfBounds);
    }
    storage.set_len((p as u64 + 1) * PAGE_SIZE as u64)?;
    *file_pages = p + 1;
    state.ensure_page(p);
    state.pages[p as usize] = PageInfo {
        state: PageState::Claimed,
        ..Default::default()
    };
    stats.file_grew += 1;
    Ok(p)
}

impl Backend for Store {
    type Pointer = Pointer;
    type Size = u32;

    fn size(&self, p: Pointer) -> Result<u32, Error> {
        let i = self.inner.borrow();
        i.live_size(p.raw()).ok_or(Error::DanglingPointer(p.raw()))
    }
}

impl WriteBackend for Store {
    fn alloc(&self, size: u32) -> Result<UniquePointer, Error> {
        self.with(|i| {
            let id = i.ids.mint()?;
            i.record(Record::Resize { id, size })?;
            Ok(UniquePointer::from_pointer(Pointer::from_raw(id).unwrap()))
        })
    }

    fn free(&self, p: UniquePointer) -> Result<(), Error> {
        self.with(|i| {
            let id = p.raw().raw();
            i.existing(id)?;
            i.record(Record::Free { id })
        })
    }

    fn resize(&self, p: &UniquePointer, size: u32) -> Result<(), Error> {
        self.with(|i| {
            let id = p.raw().raw();
            i.existing(id)?;
            i.record(Record::Resize { id, size })
        })
    }

    fn write(&self, anchor: Pointer, offset: u32, bytes: &[u8]) -> Result<(), Error> {
        self.with(|i| {
            let id = anchor.raw();
            i.existing(id)?;
            if offset as u64 + bytes.len() as u64 > MAX_ALLOCATION_SIZE {
                return Err(Error::OutOfBounds);
            }
            let b = i.ops.stash(bytes);
            i.record(Record::Write {
                id,
                offset,
                bytes: b,
            })
        })
    }

    fn splice(
        &self,
        p: &UniquePointer,
        offset: u32,
        old_len: u32,
        new: &[u8],
    ) -> Result<(), Error> {
        self.with(|i| {
            let id = p.raw().raw();
            let size = i.existing(id)?;
            let end = offset as u64 + old_len as u64;
            let new_size = (size as u64).max(end) - old_len as u64 + new.len() as u64;
            if end > MAX_ALLOCATION_SIZE || new_size > MAX_ALLOCATION_SIZE {
                return Err(Error::OutOfBounds);
            }
            let b = i.ops.stash(new);
            i.record(Record::Splice {
                id,
                offset,
                old_len,
                bytes: b,
            })
        })
    }

    fn copy(
        &self,
        src: Pointer,
        src_offset: u32,
        len: u32,
        dst: Pointer,
        dst_offset: u32,
    ) -> Result<(), Error> {
        self.with(|i| {
            i.existing(dst.raw())?;
            if src_offset as u64 + len as u64 > MAX_ALLOCATION_SIZE
                || dst_offset as u64 + len as u64 > MAX_ALLOCATION_SIZE
            {
                return Err(Error::OutOfBounds);
            }
            i.record(Record::Copy {
                src: src.raw(),
                src_offset,
                len,
                dst: dst.raw(),
                dst_offset,
            })
        })
    }

    fn atomically<R>(&self, f: impl FnOnce() -> Result<R, Error>) -> Result<R, Error> {
        let (mark, tx) = self.with(|i| Ok((i.ops.len(), i.tx)))?;
        self.begin_transaction()?;
        match f() {
            Ok(r) => {
                self.end_transaction()?;
                Ok(r)
            }
            Err(e) => {
                // Inside a transaction nothing is appended, so what `f`
                // recorded is still buffered past `mark`, and dropping it
                // undoes `f` as far as the file is concerned.
                let mut i = self.inner.borrow_mut();
                if !i.poisoned {
                    i.ops.records.truncate(mark);
                    i.tx = tx;
                    i.repopulate_geometry();
                }
                Err(e)
            }
        }
    }

    fn move_range(
        &self,
        src: Pointer,
        src_offset: u32,
        len: u32,
        dst: Pointer,
        dst_offset: u32,
    ) -> Result<(), Error> {
        self.with(|i| {
            i.existing(dst.raw())?;
            if src_offset as u64 + len as u64 > MAX_ALLOCATION_SIZE
                || dst_offset as u64 + len as u64 > MAX_ALLOCATION_SIZE
            {
                return Err(Error::OutOfBounds);
            }
            i.record(Record::Move {
                src: src.raw(),
                src_offset,
                len,
                dst: dst.raw(),
                dst_offset,
            })
        })
    }
}

/// A reader over one allocation's bytes as of the last flush, from
/// [`ReadBackend::read_at`] on a [`Store`].
///
/// It reads up to the allocation's end and can seek anywhere within it;
/// seeking past the end reads nothing.
///
/// ```
/// use std::io::{Read, Seek, SeekFrom};
/// use kladde_store::{MemoryStorage, ReadBackend, Store, WriteBackend};
///
/// let mut store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let p = store.alloc(5)?;
/// store.write(p.raw(), 0, b"hello")?;
/// store.flush()?;
/// let mut r = store.read_at(p.raw(), 1)?;
/// let mut s = String::new();
/// r.read_to_string(&mut s)?;
/// assert_eq!(s, "ello");
/// r.seek(SeekFrom::Start(0))?;
/// let mut first = [0u8; 1];
/// r.read_exact(&mut first)?;
/// assert_eq!(&first, b"h");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct AllocationReader<'a> {
    inner: &'a mut Inner,
    id: u32,
    pos: u64,
    size: u64,
}

impl Read for AllocationReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let (k, f, fend) = self
            .inner
            .state
            .resolve(self.id, self.pos as u32)
            .ok_or_else(|| io::Error::other("a hole in the fragment map"))?;
        let n = ((fend as u64).min(self.pos + buf.len() as u64) - self.pos) as usize;
        let rel = self.pos as u32 - koff(k);
        self.inner
            .read_fragment(f, rel, &mut buf[..n])
            .map_err(io::Error::other)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for AllocationReader<'_> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let new = match to {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => self.size as i64 + d,
        };
        if new < 0 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

impl ReadBackend for Store {
    fn read_size(&mut self, p: Pointer) -> Result<u32, Error> {
        let id = p.raw();
        let inner = self.inner.get_mut();
        Ok(inner
            .state
            .allocs
            .get(&id)
            .ok_or(Error::DanglingPointer(id))?
            .size)
    }

    fn read_at(&mut self, anchor: Pointer, offset: u32) -> Result<impl Read + Seek + '_, Error> {
        let inner = self.inner.get_mut();
        let id = anchor.raw();
        let size = inner
            .state
            .allocs
            .get(&id)
            .ok_or(Error::DanglingPointer(id))?
            .size;
        Ok(AllocationReader {
            inner,
            id,
            pos: offset as u64,
            size: size as u64,
        })
    }
}
