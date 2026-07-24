//! The Kladde **type-descriptor model**, its canonical byte
//! **serialization**, and its **schema fingerprint** — a language-neutral,
//! byte-for-byte reproducible description of how a value type lays out and
//! interprets its bytes, and a fixed-size hash that identifies that layout.
//!
//! This crate is the frozen, self-contained implementation of the
//! `type-descriptors.md` specification (in the repository root); it depends
//! only on [`kladde-varint`](../kladde_varint/index.html) and knows nothing
//! about the rest of the workspace, so it can be tested in isolation and
//! could back a cross-language conformance suite.
//!
//! - [`TypeDescriptor`] / [`TypeTable`] model a type graph (§2).
//! - [`TypeTable::encode`] / [`TypeTable::decode`] are the canonical
//!   serialization (§3).
//!
//! The connection to real Rust types (turning a `Persistable` type into a
//! `TypeTable`) lives in `kladde-traits`, not here.

mod descriptor;
mod fingerprint;
mod serialize;
mod sha256;

pub use descriptor::{
    Field, TypeDescriptor, TypeRef, TypeTable, Variant, Version, TAG_ARRAY, TAG_ENUM, TAG_OPAQUE,
    TAG_POINTER, TAG_STRUCT,
};
pub use fingerprint::Fingerprint;
pub use serialize::DecodeError;
