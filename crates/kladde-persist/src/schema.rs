//! Turning live Rust types into a [`TypeTable`]: the build-time bridge between
//! [`Durable`](crate::Durable) and the language-neutral `kladde-schema`
//! model.

use crate::Durable;
use kladde_schema::{TypeDescriptor, TypeRef, TypeTable};
use std::any::TypeId;
use std::collections::HashMap;

/// Accumulates the descriptors of a type graph while
/// [`Durable::describe`](crate::Durable::describe) walks it.
///
/// You rarely touch this directly -- [`Durable::schema`] and
/// [`Durable::fingerprint`] drive it for you, and `#[derive(Durable)]`
/// generates the [`Durable::describe_local`] bodies whose field references
/// call [`describe`]. Both entry points deduplicate by Rust type identity and,
/// by reserving a type's slot *before* recursing into its fields, let recursive
/// and mutually-recursive types refer back to themselves without looping:
///
/// - [`describe::<T>()`][`describe`] is the common case -- register `T` under
///   its own `TypeId`, building its descriptor from
///   [`Durable::describe_local`]. This is what a field/element/parameter
///   reference resolves to (via [`Durable::describe`]'s default).
/// - [`describe_with`] is the low-level primitive it's built on: register under
///   a caller-supplied `TypeId` with a caller-supplied closure. Reach for it
///   only for something the generic form can't express -- e.g. two Rust types
///   sharing one node.
///
/// ```
/// use kladde_persist::{SchemaBuilder, TypeRef};
///
/// let mut builder = SchemaBuilder::new();
/// let root = builder.describe::<(u8, u8)>();
/// assert_eq!(root, TypeRef(0));
/// let table = builder.finish(root);
/// assert_eq!(table.descriptors().len(), 2); // the tuple, and `u8` once
/// ```
///
/// [`Durable::schema`]: crate::Durable::schema
/// [`Durable::fingerprint`]: crate::Durable::fingerprint
/// [`Durable::describe`]: crate::Durable::describe
/// [`Durable::describe_local`]: crate::Durable::describe_local
/// [`describe`]: SchemaBuilder::describe
/// [`describe_with`]: SchemaBuilder::describe_with
#[derive(Default)]
pub struct SchemaBuilder {
    // `None` marks a slot reserved for a type whose descriptor is still being
    // built -- what a cyclic reference resolves back to.
    descriptors: Vec<Option<TypeDescriptor>>,
    by_type: HashMap<TypeId, TypeRef>,
}

impl SchemaBuilder {
    /// A fresh, empty builder. See [`SchemaBuilder`] for an example.
    pub fn new() -> Self {
        SchemaBuilder::default()
    }

    /// Registers `T` and returns a reference to its descriptor -- the
    /// common-case entry point. Deduplicates by `TypeId::of::<T>()` and, the
    /// first time `T` is seen, builds its descriptor from
    /// [`Durable::describe_local`]; later calls (including a recursive
    /// one made *while* that build is still running) return the existing
    /// reference, so cycles terminate. See [`SchemaBuilder`] for an example.
    ///
    /// [`Durable::describe_local`]: crate::Durable::describe_local
    pub fn describe<T: Durable + 'static>(&mut self) -> TypeRef {
        self.describe_with(TypeId::of::<T>(), |builder| T::describe_local(builder))
    }

    /// The low-level primitive under [`describe`](Self::describe): register the
    /// type identified by `type_id`, building its descriptor with `build` the
    /// first time that id is seen. On any later call for the same `type_id` --
    /// including a recursive call made *while* `build` is still running -- the
    /// existing reference is returned and `build` is not called again, so cycles
    /// terminate.
    ///
    /// ```
    /// use kladde_persist::{Primitive, SchemaBuilder, TypeDescriptor};
    /// use std::any::TypeId;
    ///
    /// struct Celsius;
    /// let mut builder = SchemaBuilder::new();
    /// let a = builder.describe_with(TypeId::of::<Celsius>(), |_| {
    ///     TypeDescriptor::Primitive(Primitive::F64)
    /// });
    /// let b = builder.describe_with(TypeId::of::<Celsius>(), |_| unreachable!());
    /// assert_eq!(a, b);
    /// ```
    pub fn describe_with(
        &mut self,
        type_id: TypeId,
        build: impl FnOnce(&mut SchemaBuilder) -> TypeDescriptor,
    ) -> TypeRef {
        if let Some(&reference) = self.by_type.get(&type_id) {
            return reference;
        }
        let reference = TypeRef(self.descriptors.len());
        self.descriptors.push(None);
        self.by_type.insert(type_id, reference);
        let descriptor = build(self);
        self.descriptors[reference.0] = Some(descriptor);
        reference
    }

    /// Registers a descriptor that no Rust type stands for, such as the
    /// payload of an enum's struct variant, and returns a reference to it.
    /// Unlike [`describe_with`](Self::describe_with), it does not
    /// deduplicate: every call adds a node.
    ///
    /// ```
    /// use kladde_persist::{SchemaBuilder, TypeDescriptor};
    ///
    /// let mut builder = SchemaBuilder::new();
    /// let a = builder.describe_anonymous(|_| TypeDescriptor::Text);
    /// let b = builder.describe_anonymous(|_| TypeDescriptor::Text);
    /// assert_ne!(a, b);
    /// ```
    pub fn describe_anonymous(
        &mut self,
        build: impl FnOnce(&mut SchemaBuilder) -> TypeDescriptor,
    ) -> TypeRef {
        let reference = TypeRef(self.descriptors.len());
        self.descriptors.push(None);
        let descriptor = build(self);
        self.descriptors[reference.0] = Some(descriptor);
        reference
    }

    /// Finalizes the accumulated descriptors into a [`TypeTable`] rooted at
    /// `root`. `root` must be the first type described (index 0), which is
    /// automatically the case when [`Durable::schema`] drives the builder,
    /// and panics otherwise. See [`SchemaBuilder`] for an example.
    ///
    /// [`Durable::schema`]: crate::Durable::schema
    pub fn finish(self, root: TypeRef) -> TypeTable {
        assert_eq!(
            root.0, 0,
            "the root type must be the first one described (index 0)"
        );
        let descriptors = self
            .descriptors
            .into_iter()
            .map(|slot| slot.expect("every reserved descriptor slot is filled before finish"))
            .collect();
        TypeTable::new(descriptors)
    }
}
