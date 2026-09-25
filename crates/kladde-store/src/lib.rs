//! The kladde storage layer: a file of fixed-size pages holding byte
//! allocations, described by an address table of statements, mutated through
//! a journal, flushed copy-on-write, and consolidated as it goes.
//!
//! The format is specified in kladde-docs' `spec/`, and the algorithms here
//! follow its `impl/`. Everything is type-agnostic: allocations are byte
//! ranges, and what their bytes mean is the business of the layers above.

mod backend;
mod consts;
mod crc;
mod error;
mod fold;
mod hash;
mod journal;
mod load;
mod page;
mod pointer;
mod state;
mod statement;
mod storage;

pub use backend::{Backend, ReadBackend, WriteBackend};
pub use consts::{MAX_PAGE_CONTENT, PAGE_SIZE};
pub use error::{Error, Result};
pub use pointer::{
    decode_option, decode_option_slice, encode_option, Pointer, PointerRepr, UniquePointer, Word,
};
pub use storage::{FileStorage, MemoryStorage, Storage};
