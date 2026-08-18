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
//! - [`Guard`]: the mutation-capable view onto a `Persistable` value, and the
//!   per-scalar guards (`I32Guard`, `BoolGuard`, ...) and [`TupleGuard`].
//! - [`SchemaBuilder`] and the schema hooks on `Persistable`, bridging to the
//!   language-neutral `kladde-schema` model.
//! - [`WriteBackendExt`]: typed `alloc_*` conveniences (they reference
//!   `Persistable`, so they can't live in the type-agnostic heap crate).
//! - [`ChunkedVec`]: a chunked container exercising the sizedness-conversion
//!   paths (`make_fixed_size`/`make_resizable`) that nothing else reaches. The
//!   general-purpose container types live in `kladde-types`.
//!
//! This crate absorbed the trait half of the former `kladde-traits` when kladde
//! moved onto `kladde-heap`; the allocator half of that crate (and the
//! `kladde-alloc` mock behind it) was deleted rather than ported, since
//! `kladde-heap` subsumes it.

mod chunked_vec;
mod ext;
mod guard;
mod location;
mod persistable;

mod scalar;
mod schema;
mod tuple;

pub use chunked_vec::{ChunkedVec, ChunkedVecGuard};
pub use ext::WriteBackendExt;
pub use guard::Guard;
pub use location::Location;
pub use persistable::Persistable;
pub use scalar::{
    BoolGuard, CharGuard, F32Guard, F64Guard, I16Guard, I32Guard, I64Guard, I8Guard, U16Guard,
    U32Guard, U64Guard, U8Guard,
};
pub use schema::SchemaBuilder;
pub use tuple::TupleGuard;

// Re-exported so `#[derive(Persistable)]` output and hand-written `describe`
// impls can name every schema type through `kladde_persist` alone, without a
// separate `kladde-schema` dependency.
pub use kladde_schema::{
    Field, Fingerprint, Primitive, TypeDescriptor, TypeRef, TypeTable, Variant, Version,
};

// Re-export the heap types most `Persistable` code needs, so downstream can
// depend on just `kladde-persist` for the common case.
pub use kladde_heap::{
    decode_option, decode_option_slice, encode_option, Backend, BackendError, CompactingBackend,
    CompactionProgress, MockBackend, Pointer, PointerRepr, ReadBackend, UniquePointer,
    UniquePointerFixedSize, UniquePointerResizable, UnjournaledBackend, Word, WriteBackend,
};
