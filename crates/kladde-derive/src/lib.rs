//! composed from already-`Persistable` fields. See `spec.md`'s "The Trait
//! Layer" and "Workspace Layout" for the pattern this generates:
//!
//! - Both named structs (`struct S { x: T }`) and tuple structs
//!   (`struct S(T, U)`) are supported, laid out identically. Every field is
//!   treated uniformly (including primitives, via blanket `Persistable`
//!   impls in `kladde-traits`) -- no special-cased scalar setters, just a
//!   `{field}_mut()` accessor per field (`field_{i}_mut()` for a tuple
//!   struct's positional fields). Tuples of `Persistable` types, `(T, U)`,
//!   are `Persistable` too (see `kladde-traits`), laid out the same way.
//! - A derived struct's own `INLINE_SIZE` is the sum of its fields'
//!   (each field's offset within it is therefore static, computable at
//!   compile time), and it never owns an allocation of its own -- it just
//!   threads the `Location` it's given down to its fields, extended by
//!   each field's static offset.
//! - An `enum`'s own `INLINE_SIZE` is a 4-byte discriminant plus whichever
//!   variant's fields are largest -- each variant is laid out like a
//!   struct in its own right (the same per-field offset computation,
//!   based at offset 4 instead of 0), so an enum never owns an allocation
//!   of its own either and needs no `serde`/`postcard` at all. Only
//!   whole-value replacement is supported for now (`guard.set(new_value)`)
//!   -- mutating a field within the current variant in place, and/or
//!   matching directly on a generated `Guard`, is deferred (see
//!   `spec.md`'s Future Work).
//! - A single-field struct (tuple `struct S(T)` or braced `struct S { x: T }`)
//!   marked `#[kladde(transparent)]` is persisted *exactly* as its one
//!   field `T`, analogous to `#[serde(transparent)]`: same bytes, and the
//!   same fingerprint (it reuses `T`'s schema descriptor instead of
//!   registering its own).
//! - **Generic types** are supported for type parameters: `#[derive(Persistable)]`
//!   adds a `T: Persistable` bound to each type parameter (the same
//!   heuristic `#[derive(Debug)]` uses), so a `struct List<T> { inner:
//!   PersistableVec<T> }` derives an `impl<T: Persistable> Persistable for
//!   List<T>`. Lifetime and const-generic parameters are not supported yet.
//!
//! **Field requirements:** every field of a derived `struct`/`enum` has
//! to be a type that itself implements `Persistable`. Plain `String`
//! doesn't (use `kladde_types::PersistableString` instead), but scalars
//! (`i32`, `bool`, ...), `kladde-types` containers (`PersistableVec`,
//! `PersistableHashMap`, `PersistableString`), and other
//! `#[derive(Persistable)]` types all do. There's no special error
//! message for this -- an unsuitable field type just fails to compile
//! with an ordinary `` `Foo` doesn't implement `Persistable` `` error
//! pointing at the field:
//!
//! ```compile_fail
//! #[derive(kladde_derive::Persistable)]
//! struct Contact {
//!     name: String, // error[E0277]: the trait bound `String: Persistable` is not satisfied
//! }
//! ```
//!
//! Swapping in any type that *does* implement `Persistable` compiles the
//! same way -- a scalar like `i32` here, or in a real application a
//! `kladde-types` container, another `#[derive(Persistable)]` type, or
//! (for a `String`-like field specifically) `kladde_types::PersistableString`:
//!
//! ```
//! #[derive(kladde_derive::Persistable)]
//! struct Contact {
//!     name: i32, // more realistically, kladde_types::PersistableString in this case
//! }
//! ```
//!
//! **Transparent newtypes:** `#[kladde(transparent)]` requires exactly one
//! field, so a multi-field struct is a compile error (as it is for
//! `#[serde(transparent)]`):
//!
//! ```compile_fail
//! #[derive(kladde_derive::Persistable)]
//! #[kladde(transparent)]
//! struct TwoFields {
//!     a: i32,
//!     b: i32, // error: #[kladde(transparent)] requires exactly one field
//! }
//! ```
//!
//! A one-field version compiles, and is persisted exactly as that field:
//!
//! ```
//! #[derive(kladde_derive::Persistable)]
//! #[kladde(transparent)]
//! struct Meters(i32);
//! ```
//!
//! **Dependency note:** generated code references `::kladde_traits::...`
//! paths directly, so any crate using this macro needs `kladde-traits` as
//! a *direct* dependency too -- re-exports (e.g. via `kladde-types`)
//! aren't enough to make `::kladde_traits` resolve. The same reason
//! `#[derive(serde::Serialize)]` requires a direct `serde` dependency,
//! not just `serde_derive`.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, parse_quote, Data, DeriveInput, Fields, GenericParam};

#[proc_macro_derive(Persistable, attributes(kladde))]
pub fn derive_persistable(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    let ctx = match Ctx::build(&input) {
        Ok(ctx) => ctx,
        Err(err) => return err.to_compile_error().into(),
    };

    let transparent = match transparent_attr(&input.attrs) {
        Ok(transparent) => transparent,
        Err(err) => return err.to_compile_error().into(),
    };

    let expanded = if transparent {
        derive_transparent(&input, &ctx)
    } else {
        match &input.data {
            Data::Struct(data) => derive_struct(&input, data, &ctx),
            Data::Enum(data) => derive_enum(&input, data, &ctx),
            Data::Union(data) => syn::Error::new_spanned(
                data.union_token,
                "#[derive(Persistable)] does not support unions",
            )
            .to_compile_error(),
        }
    };

    expanded.into()
}

/// The generics plumbing every derive path shares, precomputed once from
/// the input type's own generic parameters. The generated code has to
/// name three related-but-distinct generic lists, so they're built here
/// rather than re-derived in each `derive_*` function:
///
/// - the *type*'s own `impl`/type/where fragments (`impl_generics`,
///   `type_generics`, `where_clause`), with a `T: Persistable` bound
///   added to each type parameter (the `#[derive(Debug)]` heuristic); and
/// - the generated *guard*'s generic lists, which extend the type's own
///   parameters with a fresh lifetime `'__s` and backend `__B`. Those two
///   are given deliberately underscore-prefixed, collision-proof names so
///   a user type like `struct Foo<B> { .. }` (its own `B`) doesn't clash
///   with the guard's backend parameter -- the same hygiene trick serde's
///   derive uses.
///
/// Only type parameters are supported; lifetime and const parameters are
/// rejected in [`build`](Ctx::build).
struct Ctx {
    /// The type's `impl` generics with `T: Persistable` bounds added,
    /// e.g. `<T: Persistable>` (empty for a non-generic type).
    impl_generics: proc_macro2::TokenStream,
    /// The type's type generics, e.g. `<T>` (empty for a non-generic type).
    type_generics: proc_macro2::TokenStream,
    /// The type's own `where`-clause, verbatim (empty if none).
    where_clause: proc_macro2::TokenStream,
    /// Generic list for *declaring* the guard struct and for the generic
    /// list of every `impl` block on it: `<'__s, T: Persistable, __B: Backend>`.
    guard_impl_generics: proc_macro2::TokenStream,
    /// Generic list for *naming* the guard type (bare parameter names, no
    /// bounds): `<'__s, T, __B>`.
    guard_use_generics: proc_macro2::TokenStream,
}

impl Ctx {
    fn build(input: &DeriveInput) -> syn::Result<Ctx> {
        for param in &input.generics.params {
            match param {
                GenericParam::Type(_) => {}
                GenericParam::Lifetime(lt) => {
                    return Err(syn::Error::new_spanned(
                        lt,
                        "#[derive(Persistable)] does not support lifetime parameters",
                    ));
                }
                GenericParam::Const(c) => {
                    return Err(syn::Error::new_spanned(
                        c,
                        "#[derive(Persistable)] does not support const generic parameters yet",
                    ));
                }
            }
        }

        // Add a `T: Persistable` bound to every type parameter -- the same
        // (slightly conservative) rule `#[derive(Debug)]` uses. A future
        // `#[kladde(bound = "...")]` escape hatch could override this; see
        // `later.md`.
        let mut bounded = input.generics.clone();
        for tp in bounded.type_params_mut() {
            tp.bounds.push(parse_quote!(::kladde_traits::Persistable));
        }
        let (impl_generics, type_generics, where_clause) = bounded.split_for_impl();

        // The guard's own generic lists: the type's (bounded) parameters,
        // bracketed by a fresh lifetime and backend with collision-proof
        // names. `bounded_params` render with their bounds
        // (`T: ... + Persistable`); `param_idents` are the bare names.
        let bounded_params: Vec<proc_macro2::TokenStream> =
            bounded.type_params().map(|tp| quote!(#tp)).collect();
        let param_idents: Vec<&syn::Ident> =
            input.generics.type_params().map(|tp| &tp.ident).collect();

        Ok(Ctx {
            impl_generics: quote!(#impl_generics),
            type_generics: quote!(#type_generics),
            where_clause: quote!(#where_clause),
            guard_impl_generics: quote! {
                <'__s, #(#bounded_params,)* __B: ::kladde_traits::Backend>
            },
            guard_use_generics: quote! { <'__s, #(#param_idents,)* __B> },
        })
    }
}

/// Whether the type carries `#[kladde(transparent)]`. Errors on any other
/// `#[kladde(...)]` contents, so a typo is a compile error rather than a
/// silent no-op.
fn transparent_attr(attrs: &[syn::Attribute]) -> syn::Result<bool> {
    let mut transparent = false;
    for attr in attrs {
        if !attr.path().is_ident("kladde") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("transparent") {
                transparent = true;
                Ok(())
            } else {
                Err(meta.error(
                    "unknown `#[kladde(...)]` option; the only one supported is `transparent`",
                ))
            }
        })?;
    }
    Ok(transparent)
}

/// For a list of field types meant to be laid out contiguously, back to
/// back (a struct's fields, or one `enum` variant's fields, based at some
/// offset the caller adds separately), computes each field's own static
/// byte offset within that layout: the sum of every *earlier* field's
/// `INLINE_SIZE`. Purely a function of the field *types*, not how the
/// caller's generated code actually accesses each field (`self.foo`,
/// `self.0`, a `match`-bound local, ...) -- shared by struct-derive and
/// (per-variant) enum-derive.
fn field_offsets(field_ty: &[syn::Type]) -> Vec<proc_macro2::TokenStream> {
    (0..field_ty.len())
        .map(|i| {
            let earlier = &field_ty[..i];
            quote! {
                (0usize #( + <#earlier as ::kladde_traits::Persistable>::INLINE_SIZE )*) as u32
            }
        })
        .collect()
}

/// The total inline size of a list of field types laid out contiguously,
/// back to back -- the sum of each field's own `INLINE_SIZE`.
fn total_size(field_ty: &[syn::Type]) -> proc_macro2::TokenStream {
    quote! {
        0usize #( + <#field_ty as ::kladde_traits::Persistable>::INLINE_SIZE )*
    }
}

/// The guard type shared by *every* derived `Persistable`: a
/// `{ inner, backend, location }` struct plus its `Guard`/`Deref`/
/// `DerefMut` impls. These are structurally identical across the struct,
/// unit, enum, and transparent derives -- each of those differs only in the
/// *accessor* `impl` block it adds on top (per-field `_mut()`, an enum's
/// whole-value `set`, a transparent newtype's `get_mut`, ...) and in its
/// `Persistable` body. Paired with [`guard_assoc`], which emits the
/// matching items *inside* the `Persistable` impl.
fn guard_scaffold(
    ctx: &Ctx,
    ident: &syn::Ident,
    vis: &syn::Visibility,
    guard_ident: &syn::Ident,
) -> proc_macro2::TokenStream {
    let Ctx {
        type_generics,
        where_clause,
        guard_impl_generics,
        guard_use_generics,
        ..
    } = ctx;

    quote! {
        #[doc(hidden)]
        #vis struct #guard_ident #guard_use_generics #where_clause {
            inner: &'__s mut #ident #type_generics,
            backend: &'__s __B,
            location: ::kladde_traits::Location,
        }

        impl #guard_impl_generics ::kladde_traits::Guard
            for #guard_ident #guard_use_generics #where_clause
        {
            type Persistable = #ident #type_generics;
            type Backend = __B;

            fn as_persistable(&self) -> &#ident #type_generics {
                self.inner
            }
            fn as_persistable_mut(&mut self) -> &mut #ident #type_generics {
                self.inner
            }
            fn backend(&self) -> &__B {
                self.backend
            }
        }

        impl #guard_impl_generics ::std::ops::Deref
            for #guard_ident #guard_use_generics #where_clause
        {
            type Target = #ident #type_generics;
            fn deref(&self) -> &#ident #type_generics {
                self.inner
            }
        }

        impl #guard_impl_generics ::std::ops::DerefMut
            for #guard_ident #guard_use_generics #where_clause
        {
            fn deref_mut(&mut self) -> &mut #ident #type_generics {
                self.inner
            }
        }
    }
}

/// The `Guard` associated type and `guard()` constructor shared by every
/// derived `Persistable` impl -- the in-impl counterpart of
/// [`guard_scaffold`]'s out-of-impl items. Every derive kind builds the
/// same `{ inner, backend, location }` guard the same way.
fn guard_assoc(ctx: &Ctx, guard_ident: &syn::Ident) -> proc_macro2::TokenStream {
    let Ctx {
        guard_use_generics, ..
    } = ctx;
    quote! {
        type Guard<'__s, __B: ::kladde_traits::Backend>
            = #guard_ident #guard_use_generics
        where
            Self: '__s,
            __B: '__s;

        fn guard<'__s, __B: ::kladde_traits::Backend>(
            &'__s mut self,
            backend: &'__s __B,
            location: ::kladde_traits::Location,
        ) -> Self::Guard<'__s, __B> {
            #guard_ident {
                inner: self,
                backend,
                location,
            }
        }
    }
}

/// `#[kladde(transparent)]`, analogous to `#[serde(transparent)]`: a
/// single-field newtype (tuple `struct S(T)` or braced `struct S { x: T }`)
/// that is persisted *exactly* as its one field. The generated
/// [`Persistable`](::kladde_traits::Persistable) impl delegates its whole
/// representation to `T` -- same `INLINE_SIZE`, same bytes at the same
/// location -- and, crucially, is **schema-transparent**: it overrides
/// `describe` to reuse `T`'s descriptor rather than registering a node of
/// its own (leaving `describe_local` at its panicking default), so `S` and
/// `T` share one fingerprint. This is the derive-level front door to the
/// escape hatch documented on `Persistable::describe`.
///
/// The wrapper's guard exposes a single `get_mut()` returning the inner
/// type's own guard, so `T`'s full mutation API is reachable through it.
fn derive_transparent(input: &DeriveInput, ctx: &Ctx) -> proc_macro2::TokenStream {
    let ident = &input.ident;
    let vis = &input.vis;
    let guard_ident = format_ident!("{}Guard", ident);
    let Ctx {
        impl_generics,
        type_generics,
        where_clause,
        guard_impl_generics,
        guard_use_generics,
        ..
    } = ctx;

    let fields = match &input.data {
        Data::Struct(data) => &data.fields,
        Data::Enum(data) => {
            return syn::Error::new_spanned(
                data.enum_token,
                "#[kladde(transparent)] is only for single-field structs, not enums",
            )
            .to_compile_error();
        }
        Data::Union(data) => {
            return syn::Error::new_spanned(
                data.union_token,
                "#[kladde(transparent)] is only for single-field structs, not unions",
            )
            .to_compile_error();
        }
    };

    let field = match fields {
        Fields::Named(f) if f.named.len() == 1 => &f.named[0],
        Fields::Unnamed(f) if f.unnamed.len() == 1 => &f.unnamed[0],
        _ => {
            return syn::Error::new_spanned(
                fields,
                "#[kladde(transparent)] requires exactly one field",
            )
            .to_compile_error();
        }
    };

    let field_ty = &field.ty;
    // How the single field is named on `self` (`.0` for a tuple newtype,
    // the field ident for a braced one) and how the value is reassembled
    // in `load`.
    let (member, construct): (syn::Member, proc_macro2::TokenStream) = match &field.ident {
        Some(name) => (
            syn::Member::Named(name.clone()),
            quote! { #ident { #name: __value } },
        ),
        None => (
            syn::Member::Unnamed(syn::Index::from(0)),
            quote! { #ident(__value) },
        ),
    };

    let scaffold = guard_scaffold(ctx, ident, vis, &guard_ident);
    let guard_assoc = guard_assoc(ctx, &guard_ident);

    quote! {
        #scaffold

        impl #guard_impl_generics #guard_ident #guard_use_generics #where_clause {
            /// A mutable guard over the wrapped value. Since this is a
            /// `#[kladde(transparent)]` newtype, the inner value lives at
            /// the wrapper's own location, so this is a direct pass-through
            /// to the inner type's full mutation API.
            #vis fn get_mut(
                &mut self,
            ) -> <#field_ty as ::kladde_traits::Persistable>::Guard<'_, __B> {
                <#field_ty as ::kladde_traits::Persistable>::guard(
                    &mut self.inner.#member,
                    self.backend,
                    self.location,
                )
            }
        }

        impl #impl_generics ::kladde_traits::Persistable for #ident #type_generics #where_clause {
            // Transparent: the wrapper *is* its one field, so it owns no
            // storage of its own and forwards everything at offset 0.
            const INLINE_SIZE: usize =
                <#field_ty as ::kladde_traits::Persistable>::INLINE_SIZE;

            #guard_assoc

            fn store<__B: ::kladde_traits::Backend>(&mut self, backend: &__B, location: ::kladde_traits::Location) {
                <#field_ty as ::kladde_traits::Persistable>::store(
                    &mut self.#member,
                    backend,
                    location,
                );
            }

            fn load<__B: ::kladde_traits::Backend>(backend: &__B, location: ::kladde_traits::Location) -> Self {
                let __value = <#field_ty as ::kladde_traits::Persistable>::load(backend, location);
                #construct
            }

            // Schema-transparent: reuse the inner type's descriptor instead
            // of registering our own node, so `Self` and the field type
            // share one fingerprint. Overriding `describe` (and leaving
            // `describe_local` at its default) is exactly the transparency
            // escape hatch documented on `Persistable::describe`.
            fn describe(builder: &mut ::kladde_traits::SchemaBuilder) -> ::kladde_traits::TypeRef
            where
                Self: 'static,
            {
                <#field_ty as ::kladde_traits::Persistable>::describe(builder)
            }
        }
    }
}

fn derive_struct(
    input: &DeriveInput,
    data: &syn::DataStruct,
    ctx: &Ctx,
) -> proc_macro2::TokenStream {
    let ident = &input.ident;
    let vis = &input.vis;
    let guard_ident = format_ident!("{}Guard", ident);
    let Ctx {
        impl_generics,
        type_generics,
        where_clause,
        guard_impl_generics,
        guard_use_generics,
        ..
    } = ctx;

    // Both named (`struct S { x: T }`) and tuple (`struct S(T)`) structs
    // are laid out identically -- fields back to back -- so they share
    // this code path; only how each field is *named* differs (see
    // `StructField`).
    let fields: Vec<&syn::Field> = match &data.fields {
        Fields::Named(fields) => fields.named.iter().collect(),
        Fields::Unnamed(fields) => fields.unnamed.iter().collect(),
        Fields::Unit => {
            return derive_unit_like_struct(ctx, ident, vis, &guard_ident);
        }
    };

    let field_ty: Vec<syn::Type> = fields.iter().map(|f| f.ty.clone()).collect();
    // Per field: how it's accessed on `self` (`.name` or `.0`), the
    // `_mut()` accessor name, and its schema field name.
    let member: Vec<syn::Member> = fields
        .iter()
        .enumerate()
        .map(|(i, f)| match &f.ident {
            Some(name) => syn::Member::Named(name.clone()),
            None => syn::Member::Unnamed(syn::Index::from(i)),
        })
        .collect();
    let accessor_ident: Vec<syn::Ident> = fields
        .iter()
        .enumerate()
        .map(|(i, f)| match &f.ident {
            Some(name) => format_ident!("{}_mut", name),
            None => format_ident!("field_{}_mut", i),
        })
        .collect();
    let schema_name: Vec<String> = fields
        .iter()
        .enumerate()
        .map(|(i, f)| match &f.ident {
            Some(name) => name.to_string(),
            None => i.to_string(),
        })
        .collect();

    let field_offset = field_offsets(&field_ty);
    let total_size = total_size(&field_ty);

    // `load` reconstructs the value; the syntax differs between a braced
    // struct (`S { name: .. }`) and a tuple struct (`S(..)`).
    let is_tuple = matches!(&data.fields, Fields::Unnamed(_));
    let load_body = if is_tuple {
        quote! {
            #ident(
                #(
                    <#field_ty as ::kladde_traits::Persistable>::load(
                        backend,
                        ::kladde_traits::Location {
                            anchor: location.anchor,
                            offset: location.offset + #field_offset,
                        },
                    ),
                )*
            )
        }
    } else {
        let field_ident: Vec<&syn::Ident> =
            fields.iter().map(|f| f.ident.as_ref().unwrap()).collect();
        quote! {
            #ident {
                #(
                    #field_ident: <#field_ty as ::kladde_traits::Persistable>::load(
                        backend,
                        ::kladde_traits::Location {
                            anchor: location.anchor,
                            offset: location.offset + #field_offset,
                        },
                    ),
                )*
            }
        }
    };

    let scaffold = guard_scaffold(ctx, ident, vis, &guard_ident);
    let guard_assoc = guard_assoc(ctx, &guard_ident);

    quote! {
        #scaffold

        impl #guard_impl_generics #guard_ident #guard_use_generics #where_clause {
            #(
                #vis fn #accessor_ident(
                    &mut self,
                ) -> <#field_ty as ::kladde_traits::Persistable>::Guard<'_, __B> {
                    <#field_ty as ::kladde_traits::Persistable>::guard(
                        &mut self.inner.#member,
                        self.backend,
                        ::kladde_traits::Location {
                            anchor: self.location.anchor,
                            offset: self.location.offset + #field_offset,
                        },
                    )
                }
            )*
        }

        impl #impl_generics ::kladde_traits::Persistable for #ident #type_generics #where_clause {
            // A struct never owns an allocation of its own -- it's just
            // the sum of its fields' inline representations, threaded
            // through at static offsets.
            const INLINE_SIZE: usize = #total_size;

            #guard_assoc

            fn store<__B: ::kladde_traits::Backend>(&mut self, backend: &__B, location: ::kladde_traits::Location) {
                #(
                    ::kladde_traits::Persistable::store(
                        &mut self.#member,
                        backend,
                        ::kladde_traits::Location {
                            anchor: location.anchor,
                            offset: location.offset + #field_offset,
                        },
                    );
                )*
            }

            fn load<__B: ::kladde_traits::Backend>(backend: &__B, location: ::kladde_traits::Location) -> Self {
                #load_body
            }

            fn describe_local(
                __builder: &mut ::kladde_traits::SchemaBuilder,
            ) -> ::kladde_traits::TypeDescriptor
            where
                Self: 'static,
            {
                ::kladde_traits::TypeDescriptor::Struct {
                    name: ::std::string::ToString::to_string(::std::stringify!(#ident)),
                    fields: ::std::vec![
                        #(
                            ::kladde_traits::Field {
                                name: ::std::string::ToString::to_string(#schema_name),
                                ty: <#field_ty as ::kladde_traits::Persistable>::describe(
                                    __builder,
                                ),
                            },
                        )*
                    ],
                }
            }
        }
    }
}

/// A unit struct (`struct Foo;`) has no fields to mutate, load, or store
/// at all -- same shape as the struct case but with empty bodies
/// everywhere and `INLINE_SIZE = 0`.
fn derive_unit_like_struct(
    ctx: &Ctx,
    ident: &syn::Ident,
    vis: &syn::Visibility,
    guard_ident: &syn::Ident,
) -> proc_macro2::TokenStream {
    let Ctx {
        impl_generics,
        type_generics,
        where_clause,
        ..
    } = ctx;
    let scaffold = guard_scaffold(ctx, ident, vis, guard_ident);
    let guard_assoc = guard_assoc(ctx, guard_ident);

    quote! {
        #scaffold

        impl #impl_generics ::kladde_traits::Persistable for #ident #type_generics #where_clause {
            const INLINE_SIZE: usize = 0;

            #guard_assoc

            fn store<__B: ::kladde_traits::Backend>(&mut self, _backend: &__B, _location: ::kladde_traits::Location) {}

            fn load<__B: ::kladde_traits::Backend>(_backend: &__B, _location: ::kladde_traits::Location) -> Self {
                #ident
            }

            fn describe_local(
                _builder: &mut ::kladde_traits::SchemaBuilder,
            ) -> ::kladde_traits::TypeDescriptor
            where
                Self: 'static,
            {
                ::kladde_traits::TypeDescriptor::Struct {
                    name: ::std::string::ToString::to_string(::std::stringify!(#ident)),
                    fields: ::std::vec![],
                }
            }
        }
    }
}

/// Inline layout: a 4-byte discriminant followed by whichever variant's
/// own fields, laid out exactly like a struct's (see
/// `field_offsets`/`total_size`) but based at offset 4 instead of 0. Sized
/// to fit the *largest* variant, since the same bytes have to be able to
/// hold any of them -- unused tail bytes for a smaller variant are simply
/// never read, the same way a `union`'s would be.
///
/// The discriminant value follows Rust's own rule: the explicit value where
/// the author wrote one (`A = 42`), otherwise `predecessor + 1`. So the
/// value stored on disk (and reported in the schema) equals the enum's real
/// Rust discriminant, an author's pinned values stay stable, and uniqueness
/// is inherited from Rust's own discriminant check -- a colliding enum never
/// compiles. `store`, `load`, and `describe` all read it from one generated
/// `const` chain.
///
/// Supports unit, tuple (`Fields::Unnamed`), and named-field variants,
/// all in the same enum. Positional (tuple) fields get synthetic
/// `field_0`, `field_1`, ... bindings in generated match patterns, since
/// they have no identifier of their own to reuse.
fn derive_enum(input: &DeriveInput, data: &syn::DataEnum, ctx: &Ctx) -> proc_macro2::TokenStream {
    let ident = &input.ident;
    let vis = &input.vis;
    let guard_ident = format_ident!("{}Guard", ident);
    let Ctx {
        impl_generics,
        type_generics,
        where_clause,
        guard_impl_generics,
        guard_use_generics,
        ..
    } = ctx;

    if data.variants.is_empty() {
        return syn::Error::new_spanned(
            &data.variants,
            "#[derive(Persistable)] does not support enums with no variants",
        )
        .to_compile_error();
    }

    let variant_ident: Vec<_> = data.variants.iter().map(|v| v.ident.clone()).collect();
    let variant_count = data.variants.len();
    let variant_index: Vec<usize> = (0..variant_count).collect();

    // Per-variant discriminant, emitted as a `const` chain: the explicit
    // Rust discriminant where the author wrote one, otherwise
    // `predecessor + 1` (Rust's own rule, so the values coincide with the
    // enum's real discriminants and inherit its uniqueness check). Reused
    // verbatim by `store`, `load`, and `describe`.
    let disc_assign: Vec<proc_macro2::TokenStream> = data
        .variants
        .iter()
        .enumerate()
        .map(|(i, variant)| {
            let value = if let Some((_, expr)) = &variant.discriminant {
                quote! { (#expr) as u32 }
            } else if i == 0 {
                quote! { 0u32 }
            } else {
                let prev = i - 1;
                quote! { d[#prev] + 1 }
            };
            quote! { d[#i] = #value; }
        })
        .collect();
    let discriminants = quote! {
        const DISC: [u32; #variant_count] = {
            let mut d = [0u32; #variant_count];
            #(#disc_assign)*
            d
        };
    };

    // Each variant's own field types (for its own offset/size
    // computation) and the binding names generated code uses to refer to
    // them in a match pattern -- the variant's own field idents for a
    // named variant, synthetic `field_N` for a tuple variant, none for a
    // unit variant.
    let variant_field_ty: Vec<Vec<syn::Type>> = data
        .variants
        .iter()
        .map(|variant| match &variant.fields {
            Fields::Named(f) => f.named.iter().map(|f| f.ty.clone()).collect(),
            Fields::Unnamed(f) => f.unnamed.iter().map(|f| f.ty.clone()).collect(),
            Fields::Unit => Vec::new(),
        })
        .collect();
    let variant_binding: Vec<Vec<syn::Ident>> = data
        .variants
        .iter()
        .map(|variant| match &variant.fields {
            Fields::Named(f) => f.named.iter().map(|f| f.ident.clone().unwrap()).collect(),
            Fields::Unnamed(f) => (0..f.unnamed.len())
                .map(|i| format_ident!("field_{}", i))
                .collect(),
            Fields::Unit => Vec::new(),
        })
        .collect();
    let variant_field_offset: Vec<Vec<proc_macro2::TokenStream>> = variant_field_ty
        .iter()
        .map(|tys| field_offsets(tys))
        .collect();
    let variant_size: Vec<proc_macro2::TokenStream> =
        variant_field_ty.iter().map(|tys| total_size(tys)).collect();

    // The *schema* field names (as opposed to the match-binding idents):
    // a named variant's field idents, a tuple variant's decimal positions,
    // none for a unit variant.
    let variant_field_name: Vec<Vec<String>> = data
        .variants
        .iter()
        .map(|variant| match &variant.fields {
            Fields::Named(f) => f
                .named
                .iter()
                .map(|f| f.ident.as_ref().unwrap().to_string())
                .collect(),
            Fields::Unnamed(f) => (0..f.unnamed.len()).map(|i| i.to_string()).collect(),
            Fields::Unit => Vec::new(),
        })
        .collect();

    // A match pattern binding a variant's own fields (if any) -- shared
    // by `store` (matched against `&mut Self`, so bindings come out as
    // `&mut FieldTy` via match ergonomics).
    let variant_pattern: Vec<proc_macro2::TokenStream> = data
        .variants
        .iter()
        .zip(&variant_binding)
        .map(|(variant, bindings)| {
            let v_ident = &variant.ident;
            match &variant.fields {
                Fields::Named(_) => quote! { #ident::#v_ident { #(#bindings),* } },
                Fields::Unnamed(_) => quote! { #ident::#v_ident(#(#bindings),*) },
                Fields::Unit => quote! { #ident::#v_ident },
            }
        })
        .collect();

    // `store`'s per-variant match arm: write the discriminant, then each
    // field at its static offset (base 4). Recurses into
    // `Persistable::store` on each field's `&mut` binding, same as
    // `derive_struct`'s `store`.
    let variant_store_arm: Vec<proc_macro2::TokenStream> = (0..data.variants.len())
        .map(|i| {
            let pattern = &variant_pattern[i];
            let bindings = &variant_binding[i];
            let field_offset = &variant_field_offset[i];
            quote! {
                #pattern => {
                    ::kladde_traits::Allocator::write(
                        backend,
                        location.anchor,
                        location.offset,
                        &DISC[#i].to_le_bytes(),
                    );
                    #(
                        ::kladde_traits::Persistable::store(
                            #bindings,
                            backend,
                            ::kladde_traits::Location {
                                anchor: location.anchor,
                                offset: location.offset + 4 + #field_offset,
                            },
                        );
                    )*
                }
            }
        })
        .collect();

    // `load`'s per-discriminant reconstruction expression: read each
    // field back from its static offset and rebuild the variant.
    let variant_load_expr: Vec<proc_macro2::TokenStream> = (0..data.variants.len())
        .map(|i| {
            let v_ident = &variant_ident[i];
            let field_ty = &variant_field_ty[i];
            let field_offset = &variant_field_offset[i];
            match &data.variants[i].fields {
                Fields::Named(f) => {
                    let field_ident: Vec<_> =
                        f.named.iter().map(|f| f.ident.clone().unwrap()).collect();
                    quote! {
                        #ident::#v_ident {
                            #(
                                #field_ident: <#field_ty as ::kladde_traits::Persistable>::load(
                                    backend,
                                    ::kladde_traits::Location {
                                        anchor: location.anchor,
                                        offset: location.offset + 4 + #field_offset,
                                    },
                                ),
                            )*
                        }
                    }
                }
                Fields::Unnamed(_) => quote! {
                    #ident::#v_ident(
                        #(
                            <#field_ty as ::kladde_traits::Persistable>::load(
                                backend,
                                ::kladde_traits::Location {
                                    anchor: location.anchor,
                                    offset: location.offset + 4 + #field_offset,
                                },
                            ),
                        )*
                    )
                },
                Fields::Unit => quote! { #ident::#v_ident },
            }
        })
        .collect();

    // Each variant's `Vec<Field>` expression for `describe`: a field's
    // schema name paired with a recursive `describe` of its type.
    let variant_describe: Vec<proc_macro2::TokenStream> = (0..variant_count)
        .map(|i| {
            let v_ident = &variant_ident[i];
            let field_name = &variant_field_name[i];
            let field_ty = &variant_field_ty[i];
            quote! {
                ::kladde_traits::Variant {
                    discriminant: DISC[#i] as u64,
                    name: ::std::string::ToString::to_string(::std::stringify!(#v_ident)),
                    fields: ::std::vec![
                        #(
                            ::kladde_traits::Field {
                                name: ::std::string::ToString::to_string(#field_name),
                                ty: <#field_ty as ::kladde_traits::Persistable>::describe(
                                    __builder,
                                ),
                            },
                        )*
                    ],
                }
            }
        })
        .collect();

    let scaffold = guard_scaffold(ctx, ident, vis, &guard_ident);
    let guard_assoc = guard_assoc(ctx, &guard_ident);

    quote! {
        #scaffold

        impl #guard_impl_generics #guard_ident #guard_use_generics #where_clause {
            /// Replaces the whole value with `value`. See `spec.md`'s
            /// Future Work for the deferred fine-grained alternative
            /// (mutating a field within the current variant in place,
            /// and/or matching directly on this guard).
            #vis fn set(&mut self, mut value: #ident #type_generics) {
                ::kladde_traits::Persistable::store(&mut value, self.backend, self.location);
                *self.inner = value;
            }
        }

        impl #impl_generics ::kladde_traits::Persistable for #ident #type_generics #where_clause {
            const INLINE_SIZE: usize = 4 + {
                let variant_sizes = [#(#variant_size),*];
                let mut max = 0usize;
                let mut i = 0usize;
                while i < variant_sizes.len() {
                    if variant_sizes[i] > max {
                        max = variant_sizes[i];
                    }
                    i += 1;
                }
                max
            };

            #guard_assoc

            fn store<__B: ::kladde_traits::Backend>(&mut self, backend: &__B, location: ::kladde_traits::Location) {
                #discriminants
                match self {
                    #(#variant_store_arm)*
                }
            }

            fn load<__B: ::kladde_traits::Backend>(backend: &__B, location: ::kladde_traits::Location) -> Self {
                #discriminants
                let discriminant_bytes = ::kladde_traits::Allocator::read(backend, location.anchor, location.offset, 4);
                let discriminant = u32::from_le_bytes(discriminant_bytes.try_into().unwrap());
                #(
                    if discriminant == DISC[#variant_index] {
                        return #variant_load_expr;
                    }
                )*
                panic!(
                    "corrupt persisted {}: unknown discriminant {}",
                    ::std::stringify!(#ident),
                    discriminant,
                );
            }

            fn describe_local(
                __builder: &mut ::kladde_traits::SchemaBuilder,
            ) -> ::kladde_traits::TypeDescriptor
            where
                Self: 'static,
            {
                #discriminants
                ::kladde_traits::TypeDescriptor::Enum {
                    name: ::std::string::ToString::to_string(::std::stringify!(#ident)),
                    discriminant_width: 4,
                    variants: ::std::vec![
                        #(#variant_describe),*
                    ],
                }
            }
        }
    }
}
