//! The kladde storage layer: one sequence of bytes, kept in a file of
//! fixed-size pages as a chain of spans, mutated through a journal, flushed
//! copy-on-write, and consolidated as it goes.
//!
//! The sequence is the encoding of an application's root value, which the
//! layers above define; the store knows only bytes. Each span shows a run of
//! the sequence's bytes and names the span after it, so that a splice or a
//! move anywhere in the sequence states a few spans, however long the
//! sequence is. Besides the sequence, the file holds two short chains of the
//! same kind: the schema the layers above describe the sequence with, and the
//! implementation's private state.
//!
//! ```
//! use kladde_store::{MemoryStorage, Store, WriteBackend};
//!
//! let storage = MemoryStorage::new();
//! let store = Store::create(Box::new(storage.clone()), Default::default())?;
//! store.splice(0, 0, b"hello")?;
//! store.close()?;
//!
//! let store = Store::open(Box::new(storage), Default::default())?;
//! assert_eq!(store.read_all()?, b"hello");
//! # Ok::<(), kladde_store::Error>(())
//! ```

mod backend;
mod compact;
mod consolidate;
mod consts;
mod crc;
mod entries;
mod error;
mod flush;
mod fold;
mod hash;
mod journal;
mod load;
mod options;
mod page;
mod state;
mod stats;
mod storage;
mod store;
mod treap;

pub use backend::{CutId, WriteBackend};
pub use consts::{MAX_PAGE_CONTENT, PAGE_SIZE};
pub use error::{Error, Result};
pub use options::Options;
pub use stats::Stats;
pub use storage::{FileStorage, MemoryStorage, Storage};
pub use store::{SequenceReader, Store};
