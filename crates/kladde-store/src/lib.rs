//! The kladde storage layer: a file of fixed-size pages holding byte
//! allocations, described by an address table of statements, mutated through
//! a journal, flushed copy-on-write, and consolidated as it goes.
//!
//! The format is specified in kladde-docs' `spec/`, and the algorithms here
//! follow its `impl/`. Everything is type-agnostic: allocations are byte
//! ranges, and what their bytes mean is the business of the layers above.
//!
//! ```
//! use kladde_store::{MemoryStorage, Store, WriteBackend};
//!
//! let storage = MemoryStorage::new();
//! let store = Store::create(Box::new(storage.clone()), Default::default())?;
//! let p = store.alloc(5)?;
//! store.write(p.raw(), 0, b"hello")?;
//! store.close()?;
//!
//! let store = Store::open(Box::new(storage), Default::default())?;
//! assert_eq!(store.read_all(p.raw())?, b"hello");
//! # Ok::<(), kladde_store::Error>(())
//! ```

mod backend;
mod consolidate;
mod constate;
mod consts;
mod crc;
mod cut;
mod defrag;
mod error;
mod flush;
mod fold;
mod hash;
mod journal;
mod load;
mod options;
mod page;
mod pointer;
mod ripeness;
mod state;
mod statement;
mod stats;
mod storage;
mod store;

pub use backend::{Backend, ReadBackend, WriteBackend};
pub use consts::{MAX_PAGE_CONTENT, PAGE_SIZE};
pub use error::{Error, Result};
pub use options::{Options, RipenessRule};
pub use pointer::{
    decode_option, decode_option_slice, encode_option, Pointer, PointerRepr, UniquePointer, Word,
};
pub use stats::Stats;
pub use storage::{FileStorage, MemoryStorage, Storage};
pub use store::{AllocationReader, Store};
