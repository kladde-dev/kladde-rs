//! Kladde's serialization layer: how typed values are laid out in the byte
//! allocations of [`kladde_store`].
//!
//! - [`Persistable`]: a type that encodes and decodes itself, in a slotted or
//!   a packed place, generic over the pointer type `P` (default [`Pointer`])
//!   but never over the size type, and frees what it owns.
//! - [`Encoding`]: which of a type's two encodings a place holds --
//!   [`Slotted`], the fixed encoding, or [`Packed`], as many bytes as the value
//!   needs.
//! - [`Location`], [`Place`] and [`Node`]: where a value lives, and whom a
//!   value in a packed place tells when its size changes.
//! - [`Guard`]: the mutation-capable view onto a `Persistable` value, the
//!   guards of the scalars (`I32Guard`, `BoolGuard`, ...) and of tuples
//!   ([`TupleGuard`]), and [`replace`], the whole-value `set` they share.
//! - [`SchemaBuilder`] and the schema hooks on `Persistable`, bridging to the
//!   language-neutral `kladde-schema` model.
//!
//! Applications use this crate through the `kladde` facade, which re-exports
//! everything here; a library of backed types may depend on it directly.
//!
//! ```
//! use kladde_persist::{Location, Persistable, Slotted};
//! use kladde_store::{MemoryStorage, Store, WriteBackend};
//!
//! let mut store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
//! let size = <(u32, char) as Persistable>::SLOTTED_SIZE.unwrap();
//! let p = store.alloc(size as u32)?;
//! let at = Location::new(p.raw(), 0);
//! let mut value = (0u32, 'a');
//! value.guard(&store, Slotted::at(at)).set((7, 'z'))?;
//! store.flush()?;
//! assert_eq!(<(u32, char)>::load::<_, Slotted>(&mut store, at)?, (7, 'z'));
//! # Ok::<(), kladde_store::Error>(())
//! ```

mod encoding;
mod guard;
mod input;
mod location;
mod persistable;
mod place;
mod scalar;
mod schema;
mod sizes;
mod tuple;

pub use encoding::{Encoding, Packed, Slotted};
pub use guard::Guard;
pub use input::{read_allocation, Input};
pub use location::Location;
pub use persistable::{replace, slot_size, Persistable};
pub use place::{splice_at, write_encoded, FieldOffsets, Link, Node, Place};
pub use scalar::{
    BoolGuard, CharGuard, F32Guard, F64Guard, I16Guard, I32Guard, I64Guard, I8Guard, U16Guard,
    U32Guard, U64Guard, U8Guard,
};
pub use schema::SchemaBuilder;
pub use sizes::{enum_packed_size, enum_slotted_size, sum_sizes, varint_len, write_varint};
pub use tuple::TupleGuard;

// Re-exported so `#[derive(Persistable)]` output and hand-written `describe`
// impls can name every schema type through this crate alone.
pub use kladde_schema::{
    Field, Fingerprint, Primitive, TypeDescriptor, TypeRef, TypeTable, Variant, Version,
};

// The store items every `Persistable` implementation names, so that it needs
// no second dependency.
pub use kladde_store::{
    decode_option, decode_option_slice, encode_option, Backend, Error, Pointer, PointerRepr,
    ReadBackend, Result, UniquePointer, Word, WriteBackend,
};
