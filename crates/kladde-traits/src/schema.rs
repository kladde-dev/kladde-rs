//! Turning live Rust types into a [`TypeTable`]: the build-time bridge
//! between [`Persistable`](crate::Persistable) and the language-neutral
//! `kladde-schema` model.

use kladde_schema::{TypeDescriptor, TypeRef, TypeTable};
use std::any::TypeId;
use std::collections::HashMap;

/// Accumulates the descriptors of a type graph while
/// [`Persistable::describe`](crate::Persistable::describe) walks it.
///
/// You rarely touch this directly — [`Persistable::schema`] and
/// [`Persistable::fingerprint`] drive it for you, and
/// `#[derive(Persistable)]` generates the `describe` bodies that call it.
/// When you *do* hand-write a `describe`, use [`describe`] exactly once per
/// type: it deduplicates by Rust type identity and, by reserving a type's
/// slot *before* recursing into its fields, lets recursive and
/// mutually-recursive types refer back to themselves without looping.
///
/// [`Persistable::schema`]: crate::Persistable::schema
/// [`Persistable::fingerprint`]: crate::Persistable::fingerprint
/// [`describe`]: SchemaBuilder::describe
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

    /// Records the type identified by `type_id` and returns a reference to
    /// it, building its descriptor with `build` the first time the type is
    /// seen. On any later call for the same `type_id` — including a
    /// recursive call made *while* `build` is still running — the existing
    /// reference is returned and `build` is not called again, so cycles
    /// terminate.
    ///
    /// `build` should produce this type's descriptor, obtaining references
    /// to its field/element/parameter types by calling
    /// [`Persistable::describe`](crate::Persistable::describe) on them
    /// (which routes back here).
    pub fn describe(
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
