//! The built-in backed container types (`PersistableVec`, `PersistableHashMap`,
//! `PersistableString` -- a rope is deferred, see `spec.md`).
//!
//! Per `spec.md`'s "Workspace Layout": most of what lives here is
//! hand-implemented directly against `Persistable`/`Guard`/`UniquePointer`
//! rather than derive-macro output, the same way `std`'s own collections
//! hand-write unsafe raw-pointer manipulation internally.
//!
//! This crate is a *default* library, not a layer of the system: nothing
//! depends on it, and everything in it is written against the same public
//! `Persistable`/`Guard` surface any third-party crate can use. The derive
//! macro is not re-exported here -- it comes from `kladde`, which is where
//! generated code is rooted.

#[cfg(feature = "serde")]
mod blob;
mod map;
mod string;
mod vec;

#[cfg(test)]
mod test_support;

#[cfg(feature = "serde")]
pub use blob::{PersistableBlob, PersistableBlobEdit, PersistableBlobGuard};
pub use map::{PersistableHashMap, PersistableHashMapGuard};
pub use string::{PersistableString, PersistableStringGuard};
pub use vec::{PersistableVec, PersistableVecGuard};

/// This crate's own version, used as the `Opaque` descriptor version for every
/// built-in container (`type-descriptors.md` §2.4). Kept in sync with
/// `Cargo.toml` automatically via the `CARGO_PKG_VERSION_*` environment.
pub(crate) fn library_version() -> kladde_persist::Version {
    kladde_persist::Version {
        major: env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap(),
        minor: env!("CARGO_PKG_VERSION_MINOR").parse().unwrap(),
        patch: env!("CARGO_PKG_VERSION_PATCH").parse().unwrap(),
    }
}

pub use kladde_persist::{
    Backend, Guard, Location, Persistable, Pointer, ReadBackend, UniquePointer,
    UniquePointerFixedSize, UniquePointerResizable, WriteBackend, WriteBackendExt,
};
pub use kladde_persist::{
    BoolGuard, CharGuard, F32Guard, F64Guard, I16Guard, I32Guard, I64Guard, I8Guard, TupleGuard,
    U16Guard, U32Guard, U64Guard, U8Guard,
};

// Schema/fingerprint surface (originally from `kladde-schema`, re-exported
// through `kladde-persist`), so application code that builds or inspects a
// type's schema needs only this crate.
pub use kladde_persist::{
    Field, Fingerprint, Primitive, SchemaBuilder, TypeDescriptor, TypeRef, TypeTable, Variant,
    Version,
};
