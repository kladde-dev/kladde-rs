//! Kladde's serialization layer: how typed values encode into the one byte
//! sequence of [`kladde_store`], and how their `Mut`s mutate it.
//!
//! - [`Durable`]: a type that encodes itself into one buffer
//!   ([`Encoder`]), decodes itself from the sequence ([`Decoder`]), and
//!   hands out a [`Mut`] over its [`Region`]. It never allocates, frees,
//!   or knows where it lies.
//! - [`Region`]: the substring of the sequence holding one value, with
//!   writes and splices relative to its start; [`Node`] and [`FieldSizes`]:
//!   what a value keeps to find its children, which a child's change of
//!   size updates.
//! - [`Fixed`]: a type's values at a fixed size, for every type with a fixed
//!   form, which is [`Bounded`]; [`Option`] and [`Box`],
//!   the latter persisted as what it holds; tuples, and the scalars, whose
//!   `Mut` is a [`SetMut`].
//! - [`String`], whose [`StringMut`] edits it by splices, and [`Vec`] of
//!   fixed-size elements, whose [`VecMut`] finds element `i` by
//!   multiplication; [`splice_changed`], which replaces a whole value by a
//!   splice of what changed.
//! - [`Cut`]: a value cut out of one container, to be pasted into another
//!   without encoding it again.
//! - [`SchemaBuilder`] and the schema hooks on `Durable`, bridging to the
//!   language-neutral `kladde-schema` model.
//!
//! Applications use this crate through the `kladde` facade, which re-exports
//! everything here; a library of durable types may depend on it
//! directly.
//!
//! ```
//! use kladde_persist::{Durable, Region};
//! use kladde_store::{MemoryStorage, Store, WriteBackend};
//!
//! let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
//! let mut value = (7u32, 'a');
//! store.splice(0, 0, &value.to_bytes())?;
//! {
//!     let mut value_mut = value.mutate(Region::root(&store));
//!     let (mut n, mut c) = value_mut.parts();
//!     n.set(70_000)?; // a longer varint: `c` moves, and finds where it starts
//!     c.set('z')?;
//! }
//! store.flush()?;
//! assert_eq!(<(u32, char)>::from_bytes(&store.read_all()?)?, value);
//! # Ok::<(), kladde_store::Error>(())
//! ```

mod codec;
mod cut;
mod durable;
mod region;
mod scalar;
mod schema;
mod seq;
mod sizes;
mod text;
mod tuple;
mod wrappers;

pub use codec::{varint_len, Decoder, Encoder, TERMINATOR};
pub use cut::Cut;
pub use durable::{replace, Bounded, Durable, Mut, SetMut};
pub use region::{Backend, FieldSizes, Node, Region};
pub use schema::SchemaBuilder;
pub use seq::VecMut;
pub use sizes::{discriminant_width, enum_fixed_form_size, enum_fixed_size, sum_sizes};
pub use text::{splice_changed, StringMut};
pub use tuple::TupleMut;
pub use wrappers::{Fixed, OptionMut};

// Re-exported so `#[derive(Durable)]` output and hand-written `describe`
// impls can name every schema type through this crate alone.
pub use kladde_schema::{
    Field, Fingerprint, Primitive, TypeDescriptor, TypeRef, TypeTable, Variant, Version,
};

// The store items every `Durable` implementation names, so that it needs
// no second dependency.
pub use kladde_store::{CutId, Error, Result, WriteBackend};
