//! `kladde-persist`: kladde's serialization layer on top of the standalone
//! [`kladde_heap`] relocatable heap.
//!
//! Where `kladde-heap` is type-agnostic (it moves bytes and manages address
//! ranges), this crate adds the kladde-specific meaning:
//!
//! - [`PointerRepr`]: the on-file little-endian byte encoding of pointer ids,
//!   plus the `Option<Pointer>` null-niche ([`encode_option`]/[`decode_option`]).
//! - [`Location`]: an `anchor` + `offset` naming where a value's inline bytes live.
//! - [`Persistable`]: a type that stores/loads itself through a backend, generic
//!   over the pointer type `P` (default [`Pointer`]) but never over the size type.
//! - [`WriteBackendExt`]: typed `alloc_*` conveniences (they reference
//!   `Persistable`, so they can't live in the type-agnostic heap crate).
//! - (next) `PersistableVec`: a chunked container built on all of the above.

mod ext;
mod location;
mod persistable;
mod repr;

pub use ext::WriteBackendExt;
pub use location::Location;
pub use persistable::Persistable;
pub use repr::{decode_option, encode_option, PointerRepr};

// Re-export the heap types most `Persistable` code needs, so downstream can
// depend on just `kladde-persist` for the common case.
pub use kladde_heap::{
    Backend, Pointer, ReadBackend, UniquePointer, UniquePointerFixedSize, UniquePointerResizable,
    WriteBackend,
};
