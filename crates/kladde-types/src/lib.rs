//! The built-in backed container types (`PersistedVec`, `PersistedHashMap`
//! -- a rope is deferred, see `spec.md`) and blanket [`Persistable`] impls
//! for primitives.
//!
//! Per `spec.md`'s "Workspace Layout": most of what lives here is
//! hand-implemented directly against `Persistable`/`Guard`/`UniquePointer`
//! rather than derive-macro output, the same way `std`'s own collections
//! hand-write unsafe raw-pointer manipulation internally.
//!
//! Also re-exports the [`Persistable`](kladde_derive::Persistable) derive
//! macro for convenience, so application crates only need one dependency.

mod map;
mod vec;

#[cfg(test)]
mod test_support;

pub use map::{PersistedHashMap, PersistedHashMapGuard, PersistedHashMapOp};
pub use vec::{PersistedVec, PersistedVecGuard, PersistedVecOp};

// `Persistable` here names two different things in two different
// namespaces -- the trait (from `kladde-traits`) and the derive macro
// (from `kladde-derive`) -- the same way `serde::Serialize` does for the
// trait/derive pair it re-exports.
//
// The scalar `*Guard`/`*Op` blanket-impl types (`I32Guard`, `StringOp`,
// ...) live in `kladde-traits`, not here -- `impl Persistable for i32`
// inside this crate would be `impl ForeignTrait for ForeignType`, which
// the orphan rules forbid; `kladde-traits` (where `Persistable` is
// defined) is the only place that impl is legal. Re-exported here so
// application code only needs one dependency.
pub use kladde_derive::Persistable;
pub use kladde_traits::{
    Allocator, Backend, Guard, Journal, Persistable, ResolvedPointer, UniquePointer,
};
pub use kladde_traits::{
    BoolGuard, BoolOp, CharGuard, CharOp, F32Guard, F32Op, F64Guard, F64Op, I16Guard, I16Op,
    I32Guard, I32Op, I64Guard, I64Op, I8Guard, I8Op, StringGuard, StringOp, U16Guard, U16Op,
    U32Guard, U32Op, U64Guard, U64Op, U8Guard, U8Op,
};
