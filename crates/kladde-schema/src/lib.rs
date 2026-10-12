//! The Kladde **type-descriptor model**, its canonical byte
//! **serialization**, and its **schema fingerprint** — a language-neutral,
//! byte-for-byte reproducible description of how a value type encodes into
//! a kladde file's sequence, and a fixed-size hash that identifies that
//! encoding.
//!
//! This crate depends only on [`kladde-varint`](../kladde_varint/index.html)
//! and knows nothing about the rest of the workspace, so it can be tested in
//! isolation and could back a cross-language conformance suite.
//!
//! - [`TypeDescriptor`] / [`TypeTable`] model a type graph; the kinds are
//!   scalars, structs, enums, arrays, sequences, text, opaque types, and the
//!   `Fixed` wrapper.
//! - [`TypeTable::encode`] / [`TypeTable::decode`] are the canonical
//!   serialization.
//! - [`TypeTable::validate`] checks the rules of nesting, and
//!   [`TypeTable::fixed_size`], [`TypeTable::fixed_form_size`] and
//!   [`TypeTable::measure`] compute from the descriptors alone what a
//!   writer's types know about their encodings.
//!
//! The connection to real Rust types (turning a `Persistable` type into a
//! `TypeTable`) lives in `kladde-persist`, not here.

mod descriptor;
mod fingerprint;
mod nesting;
mod serialize;
mod sha256;

pub use descriptor::{
    Field, Primitive, TypeDescriptor, TypeRef, TypeTable, Variant, Version, TAG_ARRAY, TAG_ENUM,
    TAG_FIXED, TAG_OPAQUE, TAG_SEQUENCE, TAG_STRUCT, TAG_TEXT,
};
pub use fingerprint::Fingerprint;
pub use serialize::DecodeError;
