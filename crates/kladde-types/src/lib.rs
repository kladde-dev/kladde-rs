//! The built-in backed containers: [`PersistableVec`] and
//! [`PackedPersistableVec`], [`PersistableString`], [`PersistableHashMap`],
//! and, with the `serde` feature, `PersistableBlob`.
//!
//! They are hand-implemented against the same public `Persistable` and `Guard`
//! surface any crate can use, the way `std`'s collections hand-write their raw
//! pointer manipulation. This crate is a default library, not a layer of the
//! system: nothing depends on it, and third-party libraries of backed types can
//! stand beside it on equal terms.
//!
//! ```
//! use kladde::{Kladde, Persistable};
//! use kladde_types::{PersistableHashMap, PersistableString, PersistableVec};
//!
//! #[derive(Persistable)]
//! struct Library {
//!     titles: PersistableVec<PersistableString>,
//!     loans: PersistableHashMap<PersistableString, u32>,
//! }
//!
//! let mut lib = Kladde::new(Library {
//!     titles: PersistableVec::new(),
//!     loans: PersistableHashMap::new(),
//! });
//! let mut guard = lib.guard();
//! guard.titles_mut().push(PersistableString::from("Emma"))?;
//! guard.loans_mut().insert(PersistableString::from("Emma"), 3)?;
//! assert_eq!(lib.get().titles.len(), 1);
//! # Ok::<(), kladde::Error>(())
//! ```

#[cfg(feature = "serde")]
mod blob;
mod map;
mod offsets;
mod packed;
mod slot;
mod small;
mod string;
mod vec;

#[cfg(test)]
mod test_support;

#[cfg(feature = "serde")]
pub use blob::{PersistableBlob, PersistableBlobGuard};
pub use map::{PersistableHashMap, PersistableHashMapGuard};
pub use packed::{PackedPersistableVec, PackedPersistableVecGuard};
pub use small::{
    SmallPersistableString, SmallPersistableStringGuard, SmallPersistableVec,
    SmallPersistableVecGuard, FOLD_BELOW, SPILL_ABOVE,
};
pub use string::{PersistableString, PersistableStringGuard};
pub use vec::{PersistableVec, PersistableVecGuard};

/// This crate's own version, the `Opaque` descriptor version of the blob, the
/// one container that does not describe its structure.
#[cfg(feature = "serde")]
pub(crate) fn library_version() -> kladde_persist::Version {
    kladde_persist::Version {
        major: env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap(),
        minor: env!("CARGO_PKG_VERSION_MINOR").parse().unwrap(),
        patch: env!("CARGO_PKG_VERSION_PATCH").parse().unwrap(),
    }
}
