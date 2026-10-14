//! The built-in containers: [`DurableVec`], [`DurableMap`], and, with the
//! `serde` feature, `DurableBlob`.
//!
//! Every container is inline in its parent, in the one sequence of a kladde
//! file: a vector and a map state their count and then their elements. In
//! memory, a container of elements whose size depends on their value keeps
//! a Fenwick tree of their sizes, so that an element's `Mut` finds where it
//! starts in `O(log m)`. Text is a plain `String`, and a vector of
//! fixed-size elements may be a plain `Vec`: both are durable in
//! `kladde-persist`, since they need nothing beyond what the standard types
//! hold.
//!
//! They are hand-implemented against the same public `Durable` and `Mut`
//! surface any crate can use. This crate is a default library, not a layer
//! of the system: nothing depends on it, and third-party libraries of
//! durable types can stand beside it on equal terms.
//!
//! ```
//! use kladde::{Kladde, Durable};
//! use kladde_types::{DurableMap, DurableVec};
//!
//! #[derive(Durable)]
//! struct Library {
//!     titles: DurableVec<String>,
//!     loans: DurableMap<String, u32>,
//! }
//!
//! let mut lib = Kladde::new(Library {
//!     titles: DurableVec::new(),
//!     loans: DurableMap::new(),
//! });
//! let mut library = lib.get_mut();
//! library.titles_mut().push("Emma".into())?;
//! library.loans_mut().insert("Emma".into(), 3)?;
//! assert_eq!(lib.get().titles.len(), 1);
//! # Ok::<(), kladde::Error>(())
//! ```

#[cfg(feature = "serde")]
mod blob;
mod index;
mod map;
mod vec;

#[cfg(feature = "serde")]
pub use blob::{DurableBlob, DurableBlobMut};
pub use map::{DurableMap, DurableMapMut};
pub use vec::{DurableVec, DurableVecMut};

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
