//! `kladde-heap`: a standalone, type-agnostic relocatable persistent heap.
//!
//! This crate is the reusable foundation of kladde's memory management, kept
//! deliberately free of any `Persistable`/serde/schema dependency so it can be
//! used on its own (see `assessment.md`, "kladde-alloc as a standalone
//! product"). It provides:
//!
//! - [`Word`]: the unsigned-integer abstraction for address/size arithmetic.
//! - [`Pointer`] and the owned handles: concrete, `Copy` stable ids and their
//!   single-owner resizable/fixed-size wrappers. A `Pointer`'s low bit carries
//!   its [`Sizedness`], which is what [`AllocationId`] reads.
//! - [`RelocatableHeap`]: the whole partition of the address space into
//!   allocations and gaps, including the `id -> address` table and the ability
//!   to close the gaps one bounded [`Step`] at a time
//!   ([`IncrementallyCompactableHeap`]). [`GainGreedyHeap`] implements it. See
//!   `incremental-compaction.md`.
//! - [`Storage`]: an unstructured, resizable byte interface.
//! - [`Backend`]/[`ReadBackend`]/[`WriteBackend`]: the trait split that a
//!   concrete backend composing a `RelocatableHeap` with a `Storage` implements
//!   ([`UnjournaledBackend`], [`JournaledWriteBackend`], [`MockBackend`]).
//!
//! The kladde-specific serialization layer (`Persistable`, `PointerRepr`,
//! `Location`, containers) lives on top of this crate, in `kladde-persist`.

mod backend;
mod composed;
mod evacuation_index;
mod gain_greedy;
mod heap;
mod journaled;
mod mock;
mod pointer;
mod repr;
mod size_classes;
mod storage;
mod unjournaled;
mod word;

pub use backend::{Backend, BackendError, CompactingBackend, ReadBackend, WriteBackend};
pub use gain_greedy::GainGreedyHeap;
pub use heap::{
    AllocationId, CompactionProgress, HeapError, IncrementallyCompactableHeap, RelocatableHeap,
    Relocation, Step,
};
pub use journaled::{JournaledReadBackend, JournaledWriteBackend, DEFAULT_COMPACTION_BUDGET};
pub use mock::MockBackend;
pub use pointer::{
    Pointer, ResolvedPointer, Sizedness, UniquePointer, UniquePointerFixedSize,
    UniquePointerResizable,
};
pub use repr::{decode_option, decode_option_slice, encode_option, PointerRepr};
pub use storage::Storage;
pub use unjournaled::UnjournaledBackend;
pub use word::Word;

/// Internals exposed **only** so that `benches/` -- which compiles as a separate
/// crate and can therefore see nothing private -- measures the code that
/// actually ships rather than a copy of it.
///
/// Not part of the public API: hidden from the docs, and free to change or
/// vanish without notice. Do not depend on it.
#[doc(hidden)]
pub mod bench_support {
    pub use crate::evacuation_index::{EvacuationIndex, Key};
}
