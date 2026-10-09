//! [`Store`]: the backend a kladde file is driven through.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io::{self, Read, Seek, SeekFrom};

use crate::backend::{Backend, ReadBackend, WriteBackend};
use crate::consts::*;
use crate::error::{corrupt, Error};
use crate::hash::IdMap;
use crate::journal::{recover, Cursor, JournalWriter, Record, Records};
use crate::load::{load, pick_header};
use crate::options::Options;
use crate::page::{encode_page, new_page, HeaderFields, PageBuf};
use crate::pointer::{Pointer, UniquePointer};
use crate::state::*;
use crate::stats::Stats;
use crate::storage::Storage;

/// Write-phase geometry of an id an operation since the last flush touched.
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
/// `impl/transactions-and-batches.md`, with the cursors into `ops` that a
/// state does not imply: `ops[..committed]` is in the journal pages,
/// `ops[committed..ready]` is a batch's ready run, and `ops[ready..]` is the
/// open transaction. `Immediate` implies `committed == ready == end`,
/// `InBatch` implies `ready == end`, and `InTransaction` implies
/// `ready == committed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tx {
    Immediate,
    InBatch {
        batch: u32,
        committed: Cursor,
    },
    InTransaction {
        depth: u32,
        batch: u32,
        committed: Cursor,
    },
    InTxInBatch {
        depth: u32,
        batch: u32,
        committed: Cursor,
        ready: Cursor,
    },
}

impl Tx {
    /// The same state with its committed cursor at `c`.
    pub(crate) fn with_committed(self, c: Cursor) -> Tx {
        match self {
            Tx::Immediate => Tx::Immediate,
            Tx::InBatch { batch, .. } => Tx::InBatch {
                batch,
                committed: c,
            },
            Tx::InTransaction { depth, batch, .. } => Tx::InTransaction {
                depth,
                batch,
                committed: c,
            },
            Tx::InTxInBatch {
                depth,
                batch,
                ready,
                ..
            } => Tx::InTxInBatch {
                depth,
                batch,
                committed: c,
                ready,
            },
        }
    }
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
    pub journal: JournalWriter,
    /// Every operation since the last flush, appended to the journal pages
    /// or not.
    pub ops: Records,
    pub tx: Tx,
    pub geometry: IdMap<Geo>,
    pub ids: IdAllocator,
    pub first_flush: bool,
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
        for r in recovered.records.iter() {
            for id in r.ids() {
                max_id = max_id.max(id);
                ids.free_dead.remove(&id);
                ids.free_live.remove(&id);
            }
        }
        ids.next_fresh = max_id.checked_add(1).unwrap_or(0).max(1);
        // A non-empty recovered journal is folded below, before anything can
        // be appended to it; an empty one is appended to from its start.
        let fold_recovered = recovered.transactions > 0;
        let journal = if fold_recovered {
            JournalWriter::recovered(epoch + 1, recovered.pages, recovered.transactions as u64)
        } else {
            JournalWriter::new(epoch + 1, loaded.header.fields.journal_pointer)
        };
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
            journal,
            // All of it in the journal pages already, as `Immediate` implies.
            ops: recovered.records,
            tx: Tx::Immediate,
            geometry: IdMap::default(),
            ids,
            first_flush: true,
            cache: IdMap::default(),
            flush_rewritten: Default::default(),
            cons: Default::default(),
            stats: Stats::default(),
        };
        inner.seed_after_load()?;
        // Replay: a recovered journal is folded now, so that what the
        // application loads, and every read, sees the state at its last
        // transaction.
        if fold_recovered {
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
                Tx::Immediate => Tx::InTransaction {
                    depth: 1,
                    batch: 0,
                    committed: i.ops.end(),
                },
                Tx::InBatch { batch, committed } => Tx::InTxInBatch {
                    depth: 1,
                    batch,
                    committed,
                    ready: i.ops.end(),
                },
                Tx::InTransaction {
                    depth,
                    batch,
                    committed,
                } => Tx::InTransaction {
                    depth: depth + 1,
                    batch,
                    committed,
                },
                Tx::InTxInBatch {
                    depth,
                    batch,
                    committed,
                    ready,
                } => Tx::InTxInBatch {
                    depth: depth + 1,
                    batch,
                    committed,
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
                Tx::InTransaction {
                    depth: 1,
                    batch: 0,
                    committed,
                } => {
                    i.close(committed, committed, false)?;
                    i.tx = Tx::Immediate;
                }
                Tx::InTransaction { depth: 1, .. } => {
                    return Err(Error::Corrupt(
                        "a batch opened inside a transaction is still open".into(),
                    ))
                }
                Tx::InTransaction {
                    depth,
                    batch,
                    committed,
                } => {
                    i.tx = Tx::InTransaction {
                        depth: depth - 1,
                        batch,
                        committed,
                    }
                }
                Tx::InTxInBatch {
                    depth: 1,
                    batch,
                    committed,
                    ready,
                } => {
                    let committed = i.close(committed, ready, true)?;
                    i.tx = Tx::InBatch { batch, committed };
                }
                Tx::InTxInBatch {
                    depth,
                    batch,
                    committed,
                    ready,
                } => {
                    i.tx = Tx::InTxInBatch {
                        depth: depth - 1,
                        batch,
                        committed,
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
            Tx::InTransaction { committed, .. } => committed,
            _ => i.ops.end(),
        };
        i.ops.truncate(keep);
        i.poisoned = true;
    }

    /// Opens a batch: the operations recorded until the matching
    /// [`end_batch`](Self::end_batch) are held back and appended to the
    /// journal in as few pieces as its page budget allows.
    ///
    /// The batch holds its operations in memory until the next would no
    /// longer fit, together with them, in what is left of
    /// [`Options::journal_budget_pages`]; then it appends them as one
    /// transaction, and the journal is folded. Its end appends what it holds
    /// still. A batch is cut only between operations, and never inside a
    /// transaction opened in it. It saves journal writes and is no
    /// transaction: a crash may keep some of its pieces and lose the rest.
    /// Batches nest, and only the outermost end appends.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Options, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// let p = store.alloc(1000)?;
    /// store.begin_batch()?;
    /// for i in 0..1000u32 {
    ///     store.write(p.raw(), i, &[i as u8])?;
    /// }
    /// store.end_batch()?;
    /// assert_eq!(store.stats().transactions, 2);
    ///
    /// // With a budget of one journal page, the batch is cut into pieces of
    /// // a page each, and the journal is folded after each piece.
    /// let opts = Options { journal_budget_pages: 1, ..Default::default() };
    /// let store = Store::create(Box::new(MemoryStorage::new()), opts)?;
    /// let p = store.alloc(100)?;
    /// store.begin_batch()?;
    /// for _ in 0..100 {
    ///     store.write(p.raw(), 0, &[7; 100])?;
    /// }
    /// store.end_batch()?;
    /// assert!(store.stats().transactions > 2);
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn begin_batch(&self) -> Result<(), Error> {
        self.with(|i| {
            i.tx = match i.tx {
                Tx::Immediate => Tx::InBatch {
                    batch: 1,
                    committed: i.ops.end(),
                },
                Tx::InBatch { batch, committed } => Tx::InBatch {
                    batch: batch + 1,
                    committed,
                },
                Tx::InTransaction {
                    depth,
                    batch,
                    committed,
                } => Tx::InTransaction {
                    depth,
                    batch: batch + 1,
                    committed,
                },
                Tx::InTxInBatch {
                    depth,
                    batch,
                    committed,
                    ready,
                } => Tx::InTxInBatch {
                    depth,
                    batch: batch + 1,
                    committed,
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
                Tx::InBatch {
                    batch: 1,
                    committed,
                } => {
                    // The ready run fits, as it always does, so no flush
                    // follows.
                    i.append(committed, i.ops.end())?;
                    debug_assert!(!i.journal.over_budget(i.opts.journal_budget_pages));
                    i.tx = Tx::Immediate;
                }
                Tx::InBatch { batch, committed } => {
                    i.tx = Tx::InBatch {
                        batch: batch - 1,
                        committed,
                    }
                }
                Tx::InTransaction {
                    depth,
                    batch,
                    committed,
                } if batch > 0 => {
                    i.tx = Tx::InTransaction {
                        depth,
                        batch: batch - 1,
                        committed,
                    }
                }
                Tx::InTxInBatch {
                    depth,
                    batch,
                    committed,
                    ready,
                } if batch > 1 => {
                    i.tx = Tx::InTxInBatch {
                        depth,
                        batch: batch - 1,
                        committed,
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
    /// Recomputes the write-phase geometry from every operation the buffer
    /// holds: those in the journal pages, then those not appended yet.
    pub fn repopulate_geometry(&mut self) {
        self.geometry.clear();
        let ops = std::mem::take(&mut self.ops);
        for r in ops.iter() {
            self.apply_geometry(r);
        }
        self.ops = ops;
    }

    // ------------------------------------------------------------ recording

    /// Records one operation; a bare one closes at once.
    fn record(&mut self, r: Record) -> Result<(), Error> {
        self.apply_geometry(&r);
        let start = self.ops.end();
        self.ops.push(r);
        match self.tx {
            Tx::Immediate => self.close(start, start, false).map(|_| ()),
            Tx::InBatch { batch, committed } => {
                let committed = self.close(committed, start, true)?;
                self.tx = Tx::InBatch { batch, committed };
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Appends, holds back, and flushes as
    /// `impl/transactions-and-batches.md#flush-triggers` has it, now that the
    /// operations `ops[ready..]` have closed, a bare operation or a whole
    /// transaction, and `ops[committed..ready]` is a batch's ready run, or
    /// empty outside a batch. Returns the committed cursor after.
    fn close(&mut self, committed: Cursor, ready: Cursor, in_batch: bool) -> Result<Cursor, Error> {
        let (budget, end) = (self.opts.journal_budget_pages, self.ops.end());
        if ready == end {
            return Ok(committed);
        }
        // The candidate is `ops[committed..]`; if it does not fit, the batch
        // is cut before what closed: the ready run, which always fits, is
        // appended, and the journal folded unless it is empty.
        let mut committed = committed;
        if !self.journal.fits(end.bytes - committed.bytes, budget) {
            committed = self.append(committed, ready)?;
            if !self.journal.is_empty() {
                self.fold_journal(committed)?;
            }
        }
        // A batch holds back a candidate that fits.
        if in_batch && self.journal.fits(end.bytes - committed.bytes, budget) {
            return Ok(committed);
        }
        // Otherwise it is appended, and only an overlong transaction, which
        // the journal had to itself, can take it past the budget.
        committed = self.append(committed, end)?;
        if self.journal.over_budget(budget) {
            self.fold_journal(committed)?;
        }
        Ok(committed)
    }

    /// Appends `ops[from..to]` to the journal pages as one transaction, if it
    /// is not empty, and returns `to`, the committed cursor after.
    pub fn append(&mut self, from: Cursor, to: Cursor) -> Result<Cursor, Error> {
        if from == to {
            return Ok(to);
        }
        let mut payload = Vec::with_capacity((to.bytes - from.bytes) as usize);
        self.ops.encode(from, to, &mut payload);
        self.ensure_journal_page()?;
        let Inner {
            storage,
            journal,
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
        let before = journal.bytes;
        if let Err(e) = journal.append(&payload, &mut **storage, &mut alloc) {
            self.poisoned = true;
            return Err(e);
        }
        self.stats.journal_bytes += journal_delta(before, self.journal.bytes);
        self.stats.transactions += 1;
        Ok(to)
    }

    /// Makes sure the page the journal appends to exists in the file.
    fn ensure_journal_page(&mut self) -> Result<(), Error> {
        let p = self.journal.current_page();
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
            Fragment::Zero { .. } => {
                out.fill(0);
                Ok(())
            }
            Fragment::Pending(p) => {
                let pe = self.state.pending[p as usize].advanced(rel);
                match pe.origin {
                    crate::state::Origin::Arena(a) => {
                        self.ops.copy_bytes(a, out);
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

fn journal_delta(before: u64, after: u64) -> u64 {
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
        let (mark, tx) = self.with(|i| Ok((i.ops.end(), i.tx)))?;
        self.begin_transaction()?;
        match f() {
            Ok(r) => {
                self.end_transaction()?;
                Ok(r)
            }
            Err(e) => {
                // Inside a transaction nothing past `mark` is appended, so
                // moving the buffer's end back to it undoes `f` as far as the
                // file is concerned. An explicit flush inside `f` may have
                // appended a batch's ready run, which the committed cursor
                // keeps.
                let mut i = self.inner.borrow_mut();
                if !i.poisoned {
                    i.ops.truncate(mark);
                    let committed = match i.tx {
                        Tx::Immediate => mark,
                        Tx::InBatch { committed, .. }
                        | Tx::InTransaction { committed, .. }
                        | Tx::InTxInBatch { committed, .. } => committed,
                    };
                    i.tx = tx.with_committed(committed);
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

#[cfg(test)]
mod tests {
    //! The flush triggers of `impl/transactions-and-batches.md`, checked
    //! against a model of them after every operation.

    use super::*;
    use crate::journal::{framed_len, record_len, Bytes};
    use crate::storage::MemoryStorage;

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n.max(1)
        }
    }

    fn store(budget: u32) -> (Store, MemoryStorage) {
        let storage = MemoryStorage::new();
        let opts = Options {
            journal_budget_pages: budget,
            ..Default::default()
        };
        (
            Store::create(Box::new(storage.clone()), opts).unwrap(),
            storage,
        )
    }

    /// The journal's stream bytes, and whether it is empty.
    fn journal(s: &Store) -> (u64, bool) {
        let i = s.inner.borrow();
        assert!(!i.journal.over_budget(i.opts.journal_budget_pages));
        (i.journal.bytes, i.journal.is_empty())
    }

    /// The model: a journal of `bytes` stream bytes, `empty` or not, a
    /// budget of `cap` stream bytes, and the flushes and transactions so far.
    struct Model {
        bytes: u64,
        empty: bool,
        cap: u64,
        flushes: u64,
        transactions: u64,
    }

    impl Model {
        fn of(s: &Store) -> Model {
            let (bytes, empty) = journal(s);
            let stats = s.stats();
            Model {
                bytes,
                empty,
                cap: s.inner.borrow().opts.journal_budget_pages as u64 * JOURNAL_STREAM as u64,
                flushes: stats.flushes,
                transactions: stats.transactions,
            }
        }

        fn fits(&self, n: u64) -> bool {
            self.bytes + framed_len(n) <= self.cap
        }

        fn append(&mut self, n: u64) {
            self.bytes += framed_len(n);
            self.empty = false;
            self.transactions += 1;
        }

        fn flush(&mut self) {
            self.bytes = 0;
            self.empty = true;
            self.flushes += 1;
        }

        /// What the rules do once operations of `n` record bytes close,
        /// with `ready` record bytes held back by a batch, if `in_batch`;
        /// returns what the batch holds back after.
        fn close(&mut self, ready: u64, n: u64, in_batch: bool) -> u64 {
            let mut ready = ready;
            if !self.fits(ready + n) {
                if ready > 0 {
                    self.append(ready);
                }
                if !self.empty {
                    self.flush();
                }
                ready = 0;
            }
            if in_batch && self.fits(ready + n) {
                return ready + n;
            }
            self.append(ready + n);
            if self.bytes > self.cap {
                self.flush();
            }
            0
        }

        fn check(&self, s: &Store, what: &str) {
            let stats = s.stats();
            assert_eq!(journal(s), (self.bytes, self.empty), "{what}: the journal");
            assert_eq!(stats.flushes, self.flushes, "{what}: flushes");
            assert_eq!(
                stats.transactions, self.transactions,
                "{what}: transactions"
            );
        }
    }

    /// Writes `len` bytes at a random offset of `p`, mirrored in `model`,
    /// and returns the record bytes the write encodes to.
    fn write(s: &Store, p: Pointer, len: usize, rng: &mut Rng, model: &mut Vec<u8>) -> u64 {
        let offset = rng.below(2000) as u32;
        let fill = rng.below(255) as u8 + 1;
        s.write(p, offset, &vec![fill; len]).unwrap();
        let end = offset as usize + len;
        if model.len() < end {
            model.resize(end, 0);
        }
        model[offset as usize..end].fill(fill);
        let r = Record::Write {
            id: p.raw(),
            offset,
            bytes: Bytes {
                start: 0,
                len: len as u32,
            },
        };
        record_len(&r) as u64
    }

    /// Mostly short, now and then longer than a budget of `cap` stream
    /// bytes.
    fn length(rng: &mut Rng, cap: u64) -> usize {
        if rng.below(25) == 0 {
            rng.below(3 * cap) as usize
        } else {
            rng.below(300) as usize
        }
    }

    #[test]
    fn a_transaction_that_does_not_fit_is_preceded_by_a_flush_and_an_overlong_one_followed_by_one()
    {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for budget in 1..4u32 {
            let (s, _) = store(budget);
            let p = s.alloc(1).unwrap();
            let mut bytes = vec![0u8];
            s.flush().unwrap();
            for step in 0..300 {
                let mut m = Model::of(&s);
                let what = format!("budget {budget}, step {step}");
                if rng.below(3) == 0 {
                    // A transaction of a few writes, nested now and then.
                    let nested = rng.below(2) == 0;
                    let mut inner_open = false;
                    s.begin_transaction().unwrap();
                    let mut n = 0;
                    for k in 0..rng.below(5) + 1 {
                        if nested && k == 1 {
                            s.begin_transaction().unwrap();
                            inner_open = true;
                        }
                        n += write(&s, p.raw(), length(&mut rng, m.cap), &mut rng, &mut bytes);
                        m.check(&s, &what);
                    }
                    if inner_open {
                        s.end_transaction().unwrap();
                        m.check(&s, &what);
                    }
                    s.end_transaction().unwrap();
                    m.close(0, n, false);
                } else {
                    let n = write(&s, p.raw(), length(&mut rng, m.cap), &mut rng, &mut bytes);
                    m.close(0, n, false);
                }
                m.check(&s, &what);
            }
            s.flush().unwrap();
            assert_eq!(s.read_all(p.raw()).unwrap(), bytes, "budget {budget}");
        }
    }

    #[test]
    fn a_batch_is_cut_at_the_last_boundary_at_which_it_still_fits() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        for budget in 1..4u32 {
            let (s, storage) = store(budget);
            let p = s.alloc(1).unwrap();
            let mut bytes = vec![0u8];
            s.flush().unwrap();
            for round in 0..25 {
                let mut m = Model::of(&s);
                let mut ready = 0;
                s.begin_batch().unwrap();
                for item in 0..rng.below(80) {
                    let what = format!("budget {budget}, round {round}, item {item}");
                    let len = length(&mut rng, m.cap);
                    if rng.below(4) == 0 {
                        // A transaction inside the batch: never cut.
                        s.begin_transaction().unwrap();
                        let mut n = write(&s, p.raw(), len, &mut rng, &mut bytes);
                        for _ in 0..rng.below(4) {
                            n += write(&s, p.raw(), rng.below(100) as usize, &mut rng, &mut bytes);
                            m.check(&s, &what);
                        }
                        s.end_transaction().unwrap();
                        ready = m.close(ready, n, true);
                    } else {
                        let n = write(&s, p.raw(), len, &mut rng, &mut bytes);
                        ready = m.close(ready, n, true);
                    }
                    m.check(&s, &what);
                }
                s.end_batch().unwrap();
                if ready > 0 {
                    m.append(ready);
                }
                m.check(&s, &format!("budget {budget}, end of round {round}"));
            }
            // What the batches appended reads back, after a flush and after
            // reopening.
            s.flush().unwrap();
            assert_eq!(s.read_all(p.raw()).unwrap(), bytes, "budget {budget}");
            drop(s);
            let s = Store::open(
                Box::new(MemoryStorage::from_image(storage.image())),
                Options::default(),
            )
            .unwrap();
            assert_eq!(
                s.read_all(p.raw()).unwrap(),
                bytes,
                "budget {budget}, reopened"
            );
        }
    }

    #[test]
    fn an_explicit_flush_folds_even_an_empty_journal() {
        let (s, _) = store(4);
        s.flush().unwrap();
        let before = s.stats().flushes;
        s.flush().unwrap();
        assert_eq!(s.stats().flushes, before + 1);
    }

    #[test]
    fn an_explicit_flush_in_a_batch_appends_its_ready_run_and_keeps_the_open_transaction() {
        let (s, storage) = store(4);
        let p = s.alloc(4).unwrap();
        s.flush().unwrap();
        let before = s.stats();
        s.begin_batch().unwrap();
        s.write(p.raw(), 0, b"ab").unwrap();
        s.begin_transaction().unwrap();
        s.write(p.raw(), 2, b"cd").unwrap();
        s.flush().unwrap();
        let after = s.stats();
        assert_eq!(after.transactions, before.transactions + 1);
        assert_eq!(after.flushes, before.flushes + 1);
        assert_eq!(s.read_all(p.raw()).unwrap(), b"ab\0\0");
        s.end_transaction().unwrap();
        s.end_batch().unwrap();
        assert_eq!(s.stats().transactions, before.transactions + 2);
        // No flush since: the transaction is in the journal pages only, and
        // reopening replays it.
        drop(s);
        let s = Store::open(
            Box::new(MemoryStorage::from_image(storage.image())),
            Options::default(),
        )
        .unwrap();
        assert_eq!(s.read_all(p.raw()).unwrap(), b"abcd");
    }

    #[test]
    fn a_failed_atomic_mutation_in_a_batch_leaves_the_ready_run() {
        let (s, _) = store(4);
        let p = s.alloc(4).unwrap();
        s.flush().unwrap();
        s.begin_batch().unwrap();
        s.write(p.raw(), 0, b"ab").unwrap();
        let r: Result<(), Error> = s.atomically(|| {
            s.write(p.raw(), 2, b"cd")?;
            Err(Error::OutOfBounds)
        });
        assert!(r.is_err());
        s.write(p.raw(), 3, b"e").unwrap();
        s.end_batch().unwrap();
        s.flush().unwrap();
        assert_eq!(s.read_all(p.raw()).unwrap(), b"ab\0e");
    }
}
