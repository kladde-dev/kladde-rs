//! [`Store`]: one sequence of bytes, durable at every recorded operation.

use std::cell::RefCell;
use std::collections::BTreeSet;

use crate::backend::{CutId, WriteBackend};
use crate::consts::*;
use crate::error::{corrupt, Error};
use crate::hash::IdMap;
use crate::journal::{recover, Bytes, Cursor, JournalWriter, Record, Records};
use crate::load::{load, pick_header};
use crate::options::Options;
use crate::page::{encode_page, new_page, HeaderFields, PageBuf};
use crate::state::*;
use crate::stats::Stats;
use crate::storage::Storage;
use crate::treap::NIL;

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

    fn in_transaction(&self) -> bool {
        matches!(self, Tx::InTransaction { .. } | Tx::InTxInBatch { .. })
    }
}

/// The cuts of the open transaction.
#[derive(Clone, Debug, Default)]
pub(crate) struct Cuts {
    /// Which transaction they belong to: a count of the outermost
    /// transactions opened in this session.
    pub serial: u64,
    /// Each cut's length, oldest first, and whether it was pasted.
    pub list: Vec<(u64, bool)>,
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
    /// Resident span-table pages: the leaves and the governing header.
    pub table_pages: IdMap<Box<PageBuf>>,
    pub ready: BTreeSet<u32>,
    pub retiring: Vec<u32>,
    pub file_pages: u32,
    /// The schema chain's and the private chain's content, as committed.
    pub schema: Vec<u8>,
    pub private: Vec<u8>,
    /// New content for them, for the next flush to write.
    pub schema_pending: Option<Vec<u8>>,
    pub private_pending: Option<Vec<u8>>,
    // The write phase.
    pub journal: JournalWriter,
    /// Every operation since the last flush, appended to the journal pages
    /// or not.
    pub ops: Records,
    pub tx: Tx,
    /// The sequence's length as every operation so far leaves it.
    pub len: u64,
    pub cuts: Cuts,
    pub first_flush: bool,
    /// Data pages read at open, which decoding the sequence reads from;
    /// dropped by `end_load`.
    pub mirror: IdMap<Box<PageBuf>>,
    /// Data pages the flush in progress read.
    pub cache: IdMap<Box<PageBuf>>,
    pub cons: crate::consolidate::ConsState,
    pub stats: Stats,
}

/// The storage backend of one kladde file: one sequence of bytes.
///
/// A `Store` holds the file's committed state in memory and records every
/// mutation in the file's journal before the mutating call returns; a flush
/// folds the journal into fresh pages. All mutation goes through the
/// [`WriteBackend`] methods, which take `&self`; reads see the sequence as of
/// the last flush.
///
/// ```
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default()).unwrap();
/// store.splice(0, 0, b"kladde").unwrap();
/// assert_eq!(store.len(), 6);
/// store.flush().unwrap();
/// assert_eq!(store.read_all().unwrap(), b"kladde");
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
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// assert_eq!(store.len(), 0);
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
        encode_page(&mut page, Some(&header), KIND_TABLE, 0, &[0]);
        storage.write_at(&page[..], 0)?;
        storage.sync()?;
        Store::open(storage, opts)
    }

    /// Opens the kladde file in `storage`, recovering every transaction its
    /// journal holds.
    ///
    /// Recovery is part of opening: if the journal holds transactions that
    /// no flush folded, because the process that wrote them died, they are
    /// flushed before this returns. Reads the whole span table and every
    /// data page the sequence shows, in `O(entries + n)` time for a
    /// sequence of `n` bytes.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let storage = MemoryStorage::new();
    /// let store = Store::create(Box::new(storage.clone()), Default::default())?;
    /// store.splice(0, 0, b"note")?;
    /// // No flush: the process "dies" here, and the journal keeps the splice.
    /// drop(store);
    /// let store = Store::open(Box::new(MemoryStorage::from_image(storage.image())), Default::default())?;
    /// assert_eq!(store.read_all()?, b"note");
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
        // The page the header names for the journal is in its world, written
        // to or not, and so is every page the journal went on to.
        let jp = loaded.header.fields.journal_pointer;
        let named = (jp >= 2 && jp < file_pages).then_some(jp);
        for &p in recovered.pages.iter().chain(&named) {
            match state.pages[p as usize] {
                Page::Reusable | Page::Journal => state.pages[p as usize] = Page::Journal,
                _ => return Err(corrupt(format!("journal page {p} is in the table's world"))),
            }
        }
        for p in 2..file_pages {
            if state.pages[p as usize] == Page::Reusable {
                ready.insert(p);
            }
        }
        // A non-empty recovered journal is folded below, before anything can
        // be appended to it; an empty one is appended to from its start.
        let fold_recovered = recovered.transactions > 0;
        let journal = if fold_recovered {
            JournalWriter::recovered(epoch + 1, recovered.pages, recovered.transactions as u64)
        } else {
            JournalWriter::new(epoch + 1, jp)
        };
        let len = state.seq_len();
        let mut inner = Inner {
            storage,
            opts,
            poisoned: false,
            epoch,
            header: loaded.header.fields.clone(),
            header_slot: loaded.header.slot,
            state,
            table_pages: loaded.table_pages,
            ready,
            retiring: Vec::new(),
            file_pages,
            schema: loaded.schema,
            private: loaded.private,
            schema_pending: None,
            private_pending: None,
            journal,
            // All of it in the journal pages already, as `Immediate` implies.
            ops: recovered.records,
            tx: Tx::Immediate,
            len,
            cuts: Cuts::default(),
            first_flush: true,
            mirror: loaded.mirror,
            cache: IdMap::default(),
            cons: Default::default(),
            stats: Stats::default(),
        };
        inner.cons.budget = inner.opts.budget_pages as f64;
        // Replay: a recovered journal is folded now, so that what the
        // application decodes sees the state at its last transaction.
        if fold_recovered {
            inner.len = inner.replayed_len()?;
            inner.flush_now()?;
            inner.load_mirror()?;
        }
        Ok(Store {
            inner: RefCell::new(inner),
        })
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
    /// store.splice(0, 0, b"hi")?;
    /// store.flush()?;
    /// assert_eq!(store.read_all()?, b"hi");
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn flush(&self) -> Result<(), Error> {
        self.with(|i| i.flush_now())
    }

    /// Flushes, then shrinks the file to end at its last page that the
    /// current or the previous commit still needs.
    ///
    /// The store stays usable; closing is flushing plus giving unused space
    /// back to the file system.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store, Storage, WriteBackend};
    ///
    /// let storage = MemoryStorage::new();
    /// let store = Store::create(Box::new(storage.clone()), Default::default())?;
    /// store.splice(0, 0, &[1; 100_000])?;
    /// store.flush()?;
    /// store.splice(0, 100_000, b"")?;
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

    /// The schema chain's content as of the last flush, or the content
    /// [`set_schema`](Self::set_schema) gave it since.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store};
    ///
    /// let storage = MemoryStorage::new();
    /// let store = Store::create(Box::new(storage.clone()), Default::default())?;
    /// store.set_schema(b"types".to_vec());
    /// store.flush()?;
    /// let store = Store::open(Box::new(storage), Default::default())?;
    /// assert_eq!(store.schema(), b"types");
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn schema(&self) -> Vec<u8> {
        let i = self.inner.borrow();
        i.schema_pending.clone().unwrap_or_else(|| i.schema.clone())
    }

    /// Sets the schema chain's content, which the next flush writes.
    ///
    /// See [`schema`](Self::schema) for an example.
    pub fn set_schema(&self, bytes: Vec<u8>) {
        self.inner.borrow_mut().schema_pending = Some(bytes);
    }

    /// The private chain's content, as [`schema`](Self::schema) gives the
    /// schema's: an implementation's own state, which nothing else reads.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// assert!(store.private().is_empty());
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn private(&self) -> Vec<u8> {
        let i = self.inner.borrow();
        i.private_pending
            .clone()
            .unwrap_or_else(|| i.private.clone())
    }

    /// Sets the private chain's content, which the next flush writes.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// store.set_private(vec![1, 2, 3]);
    /// store.flush()?;
    /// assert_eq!(store.private(), [1, 2, 3]);
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn set_private(&self, bytes: Vec<u8>) {
        self.inner.borrow_mut().private_pending = Some(bytes);
    }

    /// The root fingerprint the header records; all zeros for a new file.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store};
    ///
    /// let storage = MemoryStorage::new();
    /// let store = Store::create(Box::new(storage.clone()), Default::default())?;
    /// store.set_root_fingerprint([7; 16]);
    /// store.flush()?;
    /// let store = Store::open(Box::new(storage), Default::default())?;
    /// assert_eq!(store.root_fingerprint(), [7; 16]);
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn root_fingerprint(&self) -> [u8; 16] {
        self.inner.borrow().header.root_fingerprint
    }

    /// Sets the root fingerprint, which the next flush writes.
    ///
    /// See [`root_fingerprint`](Self::root_fingerprint) for an example.
    pub fn set_root_fingerprint(&self, fingerprint: [u8; 16]) {
        self.inner.borrow_mut().header.root_fingerprint = fingerprint;
    }

    /// Runs `f` on a reader of the sequence as of the last flush, whose
    /// chunks borrow the store's pages.
    ///
    /// Reads the data pages the sequence shows first, if
    /// [`end_load`](Self::end_load) dropped them.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// store.splice(0, 0, &[9; 10_000])?;
    /// store.flush()?;
    /// let total: usize = store.with_reader(|r| r.map(|c| c.len()).sum())?;
    /// assert_eq!(total, 10_000);
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn with_reader<R>(&self, f: impl FnOnce(&mut SequenceReader<'_>) -> R) -> Result<R, Error> {
        self.with(|i| i.load_mirror())?;
        let i = self.inner.borrow();
        let mut r = SequenceReader {
            inner: &i,
            node: i.state.rope.first(i.state.root),
            zeros_left: 0,
        };
        Ok(f(&mut r))
    }

    /// The whole sequence as of the last flush, copied.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// store.splice(0, 0, b"abc")?;
    /// store.flush()?;
    /// assert_eq!(store.read_all()?, b"abc");
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn read_all(&self) -> Result<Vec<u8>, Error> {
        self.with_reader(|r| {
            let mut out = Vec::new();
            for c in r {
                out.extend_from_slice(c);
            }
            out
        })
    }

    /// Drops the data pages read at open; a later
    /// [`with_reader`](Self::with_reader) reads them again.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// store.end_load();
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn end_load(&self) {
        self.inner.borrow_mut().mirror.clear();
    }

    /// Counters describing the file and the work done on it since opening.
    ///
    /// Costs `O(pages)` time, since it takes a snapshot of every page's fill.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// store.splice(0, 0, &[1; 10_000])?;
    /// store.flush()?;
    /// let s = store.stats();
    /// assert_eq!(s.flushes, 1);
    /// assert_eq!(s.sequence_bytes, 10_000);
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn stats(&self) -> Stats {
        let i = self.inner.borrow();
        let mut s = i.stats.clone();
        i.fill_stats(&mut s);
        s
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
    /// store.splice(0, 0, b"x")?;
    /// store.flush()?;
    /// store.check();
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn check(&self) {
        let i = self.inner.borrow();
        i.state.check(i.header_slot);
        assert_eq!(i.header.sequence_head, i.state.seq_head(), "sequence head");
        assert_eq!(
            i.header.schema_head,
            i.state.schema.first().copied().unwrap_or(0)
        );
    }

    /// A description of the sequence's spans, in order: for debugging.
    #[doc(hidden)]
    pub fn describe(&self) -> String {
        let i = self.inner.borrow();
        let mut out = String::new();
        for id in i.state.seq_spans() {
            let s = i.state.span(id);
            out += &format!(
                "span {id}: {} bytes {:?}, next {}, entry in {} ({} bytes), heat {}\n",
                s.len, s.content, s.next, s.entry_page, s.entry_len, s.heat
            );
        }
        out
    }

    /// Opens a transaction: the operations recorded until the matching
    /// [`end_transaction`](Self::end_transaction) reach the journal as one,
    /// so a crash keeps all of them or none.
    ///
    /// Transactions nest, and only the outermost end appends. A splice
    /// inside a transaction makes a cut of what it removes, which a
    /// [`paste`](WriteBackend::paste) of the same transaction can insert
    /// elsewhere.
    ///
    /// ```
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let storage = MemoryStorage::new();
    /// let store = Store::create(Box::new(storage.clone()), Default::default())?;
    /// store.splice(0, 0, b"ab")?;
    /// store.begin_transaction()?;
    /// store.write(0, b"x")?;
    /// store.write(1, b"y")?;
    /// store.end_transaction()?;
    /// // Both writes are in the journal now, or would have been lost together.
    /// let store = Store::open(Box::new(storage), Default::default())?;
    /// assert_eq!(store.read_all()?, b"xy");
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn begin_transaction(&self) -> Result<(), Error> {
        self.with(|i| {
            if !i.tx.in_transaction() {
                i.cuts.serial += 1;
                i.cuts.list.clear();
            }
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
    /// operations to the journal, unless a batch holds them back, and ends
    /// its cuts.
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
                    i.cuts.list.clear();
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
                    i.cuts.list.clear();
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
    /// store.splice(0, 0, b"a")?;
    /// store.flush()?;
    /// store.begin_transaction()?;
    /// store.write(0, b"x")?;
    /// store.abandon_transaction();
    /// assert!(matches!(store.flush(), Err(Error::Poisoned)));
    /// let store = Store::open(Box::new(storage), Default::default())?;
    /// assert_eq!(store.read_all()?, b"a");
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
    /// use kladde_store::{MemoryStorage, Store, WriteBackend};
    ///
    /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
    /// store.splice(0, 0, &[0; 1000])?;
    /// store.begin_batch()?;
    /// for i in 0..1000u64 {
    ///     store.write(i, &[i as u8])?;
    /// }
    /// store.end_batch()?;
    /// assert_eq!(store.stats().transactions, 2);
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
                    i.append(committed, i.ops.end())?;
                    if i.journal.over_budget(i.opts.journal_budget_pages) {
                        i.fold_journal(i.ops.end())?;
                    }
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
}

impl Inner {
    // ------------------------------------------------------------ recording

    /// Records one operation; a bare one closes at once.
    fn record(&mut self, r: Record) -> Result<(), Error> {
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

    /// The cut of `n` bytes that the record about to be recorded makes, if a
    /// transaction is open; outside one nothing could paste it.
    fn new_cut(&mut self, n: u64) -> Option<CutId> {
        if !self.tx.in_transaction() {
            return None;
        }
        self.cuts.list.push((n, false));
        Some(CutId {
            serial: self.cuts.serial,
            index: (self.cuts.list.len() - 1) as u32,
            len: n,
        })
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
        let mut committed = committed;
        if !self.journal.fits(end.bytes - committed.bytes, budget) {
            committed = self.append(committed, ready)?;
            if !self.journal.is_empty() {
                self.fold_journal(committed)?;
            }
        }
        if in_batch && self.journal.fits(end.bytes - committed.bytes, budget) {
            return Ok(committed);
        }
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
        let mut cursor = self.journal.cursor;
        self.ops.encode(from, to, &mut cursor, &mut payload);
        self.ops.mark_start(from.at);
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
            take_page(ready, state, file_pages, s, stats, Page::Journal)
        };
        let before = journal.bytes;
        if let Err(e) = journal.append(&payload, &mut **storage, &mut alloc) {
            self.poisoned = true;
            return Err(e);
        }
        self.journal.cursor = cursor;
        self.stats.journal_bytes += self.journal.bytes - before;
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
                if q >= 2 && self.state.pages[q as usize] == Page::Reusable {
                    self.ready.insert(q);
                }
            }
            self.file_pages = p + 1;
            self.state.ensure_page(p);
        }
        self.ready.remove(&p);
        self.state.pages[p as usize] = Page::Journal;
        Ok(())
    }

    /// The sequence's length once the buffered records are applied.
    fn replayed_len(&self) -> Result<u64, Error> {
        let mut len = self.state.seq_len();
        for r in self.ops.iter() {
            len = match *r {
                Record::Write { .. } | Record::Move { .. } => len,
                Record::Splice { removed, bytes, .. } => {
                    len.checked_sub(removed).ok_or_else(past_end)? + bytes.len as u64
                }
                Record::Paste {
                    removed, len: n, ..
                } => len.checked_sub(removed).ok_or_else(past_end)? + n,
            };
        }
        Ok(len)
    }

    // ------------------------------------------------------------ reading

    /// Reads into the mirror every data page the sequence shows that it
    /// lacks, in page order.
    pub fn load_mirror(&mut self) -> Result<(), Error> {
        let mut want: Vec<u32> = self
            .state
            .data_spans
            .keys()
            .copied()
            .filter(|p| !self.mirror.contains_key(p))
            .collect();
        want.sort_unstable();
        for p in want {
            let mut buf = new_page();
            self.storage
                .read_at(&mut buf[..], p as u64 * PAGE_SIZE as u64)?;
            crate::page::decode_page(&buf, false)
                .map_err(|_| corrupt(format!("data page {p} fails its checksum")))?;
            self.stats.pages_read += 1;
            self.mirror.insert(p, buf);
        }
        Ok(())
    }

    /// The bytes of a page that holds committed content: a resident table
    /// page, a mirrored data page, or a data page read through the flush's
    /// cache.
    pub fn page_bytes(&mut self, page: u32) -> Result<&PageBuf, Error> {
        if self.table_pages.contains_key(&page) {
            return Ok(&self.table_pages[&page]);
        }
        if self.mirror.contains_key(&page) {
            return Ok(&self.mirror[&page]);
        }
        if !self.cache.contains_key(&page) {
            let mut buf = new_page();
            self.storage
                .read_at(&mut buf[..], page as u64 * PAGE_SIZE as u64)?;
            self.stats.pages_read += 1;
            self.cache.insert(page, buf);
        }
        Ok(&self.cache[&page])
    }

    // ------------------------------------------------------------ checks

    fn check_range(&self, pos: u64, n: u64) -> Result<(), Error> {
        if pos.checked_add(n).is_none_or(|e| e > self.len) {
            return Err(Error::OutOfBounds);
        }
        Ok(())
    }

    fn grown(&self, removed: u64, added: u64) -> Result<u64, Error> {
        let len = self.len - removed + added;
        if len >= MAX_SEQUENCE {
            return Err(Error::OutOfBounds);
        }
        Ok(len)
    }
}

fn past_end() -> Error {
    corrupt("a journal record reaches past the end of the sequence")
}

/// Takes the lowest reusable page, or grows the file by one page, and makes
/// it `page`.
pub(crate) fn take_page(
    ready: &mut BTreeSet<u32>,
    state: &mut State,
    file_pages: &mut u32,
    storage: &mut dyn Storage,
    stats: &mut Stats,
    page: Page,
) -> Result<u32, Error> {
    let p = match ready.pop_first() {
        Some(p) => p,
        None => {
            let p = *file_pages;
            if p == u32::MAX {
                return Err(Error::OutOfBounds);
            }
            storage.set_len((p as u64 + 1) * PAGE_SIZE as u64)?;
            *file_pages = p + 1;
            state.ensure_page(p);
            stats.file_grew += 1;
            p
        }
    };
    debug_assert_eq!(state.pages[p as usize], Page::Reusable, "taking page {p}");
    state.pages[p as usize] = page;
    Ok(p)
}

/// A reader of the sequence as of the last flush, chunk by chunk: each a
/// slice of the store's pages, or of zeros.
///
/// See [`Store::with_reader`] for an example.
pub struct SequenceReader<'a> {
    inner: &'a Inner,
    node: u32,
    zeros_left: u64,
}

static ZEROS: [u8; PAGE_SIZE] = [0; PAGE_SIZE];

impl<'a> Iterator for SequenceReader<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.zeros_left > 0 {
            let n = self.zeros_left.min(PAGE_SIZE as u64);
            self.zeros_left -= n;
            return Some(&ZEROS[..n as usize]);
        }
        if self.node == NIL {
            return None;
        }
        let st = &self.inner.state;
        let RopeItem::Span { id, len } = *st.rope.item(self.node) else {
            unreachable!("a literal in the rope between flushes");
        };
        self.node = st.rope.next(self.node);
        let len = len as usize;
        match st.span(id).content {
            Content::Ref { page, offset } => {
                let buf = self
                    .inner
                    .mirror
                    .get(&page)
                    .expect("the mirror holds every page the sequence shows");
                Some(&buf[offset as usize..offset as usize + len])
            }
            Content::Inline { page, offset } => {
                let buf = &self.inner.table_pages[&page];
                Some(&buf[offset as usize..offset as usize + len])
            }
            Content::Zero => {
                self.zeros_left = len as u64;
                self.next()
            }
            other => unreachable!("span {id} shows {other:?} between flushes"),
        }
    }
}

impl WriteBackend for Store {
    fn len(&self) -> u64 {
        self.inner.borrow().len
    }

    fn write(&self, pos: u64, bytes: &[u8]) -> Result<Option<CutId>, Error> {
        self.with(|i| {
            if bytes.is_empty() {
                return Ok(None);
            }
            i.check_range(pos, bytes.len() as u64)?;
            let bytes = i.ops.stash(bytes);
            let cut = i.new_cut(bytes.len as u64);
            i.record(Record::Write { pos, bytes })?;
            Ok(cut)
        })
    }

    fn splice(&self, pos: u64, removed: u64, bytes: &[u8]) -> Result<Option<CutId>, Error> {
        if removed == bytes.len() as u64 {
            return self.write(pos, bytes);
        }
        self.with(|i| {
            i.check_range(pos, removed)?;
            let len = i.grown(removed, bytes.len() as u64)?;
            let stashed: Bytes = i.ops.stash(bytes);
            let cut = if removed > 0 {
                i.new_cut(removed)
            } else {
                None
            };
            i.len = len;
            i.record(Record::Splice {
                pos,
                removed,
                bytes: stashed,
            })?;
            Ok(cut)
        })
    }

    fn move_range(&self, from: u64, len: u64, to: u64) -> Result<(), Error> {
        self.with(|i| {
            i.check_range(from, len)?;
            if to > i.len - len {
                return Err(Error::OutOfBounds);
            }
            if len == 0 || from == to {
                return Ok(());
            }
            i.record(Record::Move { from, len, to })
        })
    }

    fn paste(&self, pos: u64, removed: u64, cut: CutId) -> Result<(), Error> {
        self.with(|i| {
            if !i.tx.in_transaction() || cut.serial != i.cuts.serial {
                return Err(Error::Corrupt(
                    "a cut is pasted outside the transaction that made it".into(),
                ));
            }
            let entry = i
                .cuts
                .list
                .get(cut.index as usize)
                .copied()
                .ok_or_else(|| Error::Corrupt("no such cut".into()))?;
            if entry.1 {
                return Err(Error::Corrupt("a cut is pasted twice".into()));
            }
            i.check_range(pos, removed)?;
            let len = i.grown(removed, entry.0)?;
            let distance = (i.cuts.list.len() - 1 - cut.index as usize) as u32;
            i.cuts.list[cut.index as usize].1 = true;
            i.len = len;
            i.record(Record::Paste {
                pos,
                removed,
                cut: distance,
                len: entry.0,
            })
        })
    }

    fn atomically<R>(&self, f: impl FnOnce() -> Result<R, Error>) -> Result<R, Error> {
        let (mark, tx, len, cuts) =
            self.with(|i| Ok((i.ops.end(), i.tx, i.len, i.cuts.clone())))?;
        self.begin_transaction()?;
        match f() {
            Ok(r) => {
                self.end_transaction()?;
                Ok(r)
            }
            Err(e) => {
                // Inside a transaction nothing past `mark` is appended, so
                // moving the buffer's end back to it undoes `f` as far as the
                // file is concerned.
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
                    i.len = len;
                    i.cuts = cuts;
                }
                Err(e)
            }
        }
    }

    fn begin_transaction(&self) -> Result<(), Error> {
        Store::begin_transaction(self)
    }

    fn end_transaction(&self) -> Result<(), Error> {
        Store::end_transaction(self)
    }

    fn poison(&self) {
        self.inner.borrow_mut().poisoned = true;
    }
}
