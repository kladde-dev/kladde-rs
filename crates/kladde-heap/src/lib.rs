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
//! - (later commits) `Allocator`: pure address-range management over stable ids;
//!   `Storage`: an unstructured byte interface; and the
//!   `Backend`/`ReadBackend`/`WriteBackend` split that composes an `Allocator`
//!   with a `Storage`.
//!
//! The kladde-specific serialization layer (`Persistable`, `PointerRepr`,
//! `Location`, containers) lives on top of this crate, in `kladde-persist`.

mod pointer;
mod word;

pub use pointer::{
    Pointer, ResolvedPointer, UniquePointer, UniquePointerFixedSize, UniquePointerResizable,
};
pub use word::Word;
