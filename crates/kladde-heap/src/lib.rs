//! `kladde-heap`: a standalone, type-agnostic relocatable persistent heap.
//!
//! This crate is the reusable foundation of kladde's memory management, kept
//! deliberately free of any `Persistable`/serde/schema dependency so it can be
//! used on its own (see `assessment.md`, "kladde-alloc as a standalone
//! product"). It provides:
//!
//! - [`Word`]: the unsigned-integer abstraction for address/size arithmetic.
//! - [`Pointer`] and the owned handles: concrete, `Copy` stable ids and their
//!   single-owner resizable/fixed-size wrappers.
//! - [`Allocator`]: pure address-range management over stable ids, with
//!   [`SimpleAllocator`] a simple in-memory implementation; and [`Storage`]: an
//!   unstructured, resizable byte interface.
//! - [`Backend`]/[`ReadBackend`]/[`WriteBackend`]: the trait split that a
//!   concrete backend composing an `Allocator` with a `Storage` implements.
//!   (Concrete backends land in later commits.)
//!
//! The kladde-specific serialization layer (`Persistable`, `PointerRepr`,
//! `Location`, containers) lives on top of this crate, in `kladde-persist`.

mod allocator;
mod backend;
mod composed;
mod journaled;
mod mock;
mod pointer;
mod storage;
mod unjournaled;
mod word;

pub use allocator::{
    AllocError, Allocation, AllocationMut, Allocator, Relocation, SimpleAllocator,
};
pub use backend::{Backend, BackendError, ReadBackend, WriteBackend};
pub use journaled::{JournaledReadBackend, JournaledWriteBackend};
pub use mock::MockBackend;
pub use pointer::{
    Pointer, ResolvedPointer, UniquePointer, UniquePointerFixedSize, UniquePointerResizable,
};
pub use storage::Storage;
pub use unjournaled::UnjournaledBackend;
pub use word::Word;
