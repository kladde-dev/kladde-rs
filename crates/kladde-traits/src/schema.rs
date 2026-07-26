//! Turning live Rust types into a [`TypeTable`]: the build-time bridge
//! between [`Persistable`](crate::Persistable) and the language-neutral
//! `kladde-schema` model.

use crate::Persistable;
use kladde_schema::{TypeDescriptor, TypeRef, TypeTable};
use std::any::TypeId;
use std::collections::HashMap;

/// Accumulates the descriptors of a type graph while
/// [`Persistable::describe`](crate::Persistable::describe) walks it.
///
/// You rarely touch this directly — [`Persistable::schema`] and
/// [`Persistable::fingerprint`] drive it for you, and
/// `#[derive(Persistable)]` generates the [`Persistable::describe_local`]
/// bodies whose
/// field references call [`describe`]. Both entry points deduplicate by
/// Rust type identity and, by reserving a type's slot *before* recursing
/// into its fields, let recursive and mutually-recursive types refer back
/// to themselves without looping:
///
/// - [`describe::<T>()`][`describe`] is the common case — register `T`
///   under its own `TypeId`, building its descriptor from
///   [`Persistable::describe_local`]. This is what a field/element/parameter
///   reference resolves to (via [`Persistable::describe`]'s default).
/// - [`describe_with`] is the low-level primitive it's built on: register
///   under a caller-supplied `TypeId` with a caller-supplied closure. Reach
///   for it only for something the generic form can't express — e.g. a
///   schema-transparent wrapper, or two Rust types sharing one node.
///
/// [`Persistable::schema`]: crate::Persistable::schema
/// [`Persistable::fingerprint`]: crate::Persistable::fingerprint
/// [`Persistable::describe`]: crate::Persistable::describe
/// [`Persistable::describe_local`]: crate::Persistable::describe_local
/// [`describe`]: SchemaBuilder::describe
/// [`describe_with`]: SchemaBuilder::describe_with
#[derive(Default)]
pub struct SchemaBuilder {
    // `None` marks a slot reserved for a type whose descriptor is still
    // being built -- what a cyclic reference resolves back to.
    descriptors: Vec<Option<TypeDescriptor>>,
    by_type: HashMap<TypeId, TypeRef>,
}

impl SchemaBuilder {
    /// A fresh, empty builder.
    pub fn new() -> Self {
        SchemaBuilder::default()
    }

    /// Registers `T` and returns a reference to its descriptor — the
    /// common-case entry point. Deduplicates by `TypeId::of::<T>()` and, the
    /// first time `T` is seen, builds its descriptor from
    /// [`Persistable::describe_local`]; later calls (including a recursive
    /// one made *while* that build is still running) return the existing
    /// reference, so cycles terminate.
    ///
    /// This is what [`Persistable::describe`]'s default routes through, and
    /// what a generated/hand-written [`describe_local`] body uses to
    /// reference a field's type — indirectly, via
    /// `<FieldTy as Persistable>::describe(builder)`, so that a field whose
    /// own type is schema-transparent is still honored.
    ///
    /// [`Persistable::describe`]: crate::Persistable::describe
    /// [`Persistable::describe_local`]: crate::Persistable::describe_local
    /// [`describe_local`]: crate::Persistable::describe_local
    pub fn describe<T: Persistable + 'static>(&mut self) -> TypeRef {
        self.describe_with(TypeId::of::<T>(), |builder| T::describe_local(builder))
    }

    /// The low-level primitive under [`describe`](Self::describe): register
    /// the type identified by `type_id`, building its descriptor with
    /// `build` the first time that id is seen. On any later call for the
    /// same `type_id` — including a recursive call made *while* `build` is
    /// still running — the existing reference is returned and `build` is not
    /// called again, so cycles terminate.
    ///
    /// Prefer [`describe`](Self::describe) unless you need something it
    /// can't express: keying a node under an identity other than a single
    /// Rust type's (sharing one node across types, a schema-transparent
    /// wrapper that returns another type's reference instead of building its
    /// own, ...). `build` obtains references to field/element/parameter
    /// types by calling [`Persistable::describe`](crate::Persistable::describe)
    /// on them (which routes back here).
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

    /// Finalizes the accumulated descriptors into a [`TypeTable`] rooted at
    /// `root`. `root` must be the first type described (index 0), which is
    /// automatically the case when [`Persistable::schema`] drives the
    /// builder.
    ///
    /// [`Persistable::schema`]: crate::Persistable::schema
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
