//! The built-in containers: [`DurableVec`], [`DurableString`],
//! [`DurableMap`], and, with the `serde` feature, `DurableBlob`.
//!
//! Every container is inline in its parent, in the one sequence of a kladde
//! file: a vector and a map state their count and then their elements, and
//! text ends in the byte `0xFF`. In memory, a container of elements whose
//! size depends on their value keeps a Fenwick tree of their sizes, so that
//! an element's guard finds where it starts in `O(log m)`.
//!
//! They are hand-implemented against the same public `Durable` and
//! `Guard` surface any crate can use. This crate is a default library, not a
//! layer of the system: nothing depends on it, and third-party libraries of
//! durable types can stand beside it on equal terms.
//!
//! ```
//! use kladde::{Kladde, Durable};
//! use kladde_types::{DurableMap, DurableString, DurableVec};
//!
//! #[derive(Durable)]
//! struct Library {
//!     titles: DurableVec<DurableString>,
//!     loans: DurableMap<DurableString, u32>,
//! }
//!
//! let mut lib = Kladde::new(Library {
//!     titles: DurableVec::new(),
//!     loans: DurableMap::new(),
//! });
//! let mut guard = lib.guard();
//! guard.titles_mut().push("Emma".into())?;
//! guard.loans_mut().insert("Emma".into(), 3)?;
//! assert_eq!(lib.get().titles.len(), 1);
//! # Ok::<(), kladde::Error>(())
//! ```

#[cfg(feature = "serde")]
mod blob;
mod index;
mod map;
mod string;
mod vec;

#[cfg(feature = "serde")]
pub use blob::{DurableBlob, DurableBlobGuard};
pub use map::{DurableMap, DurableMapGuard};
pub use string::{DurableString, DurableStringGuard};
pub use vec::{DurableVec, DurableVecGuard};

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
