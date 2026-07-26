//! composed from already-`Persistable` fields. See `spec.md`'s "The Trait
//! Layer" and "Workspace Layout" for the pattern this generates:
//!
//! - Every field is treated uniformly (including primitives, via blanket
//!   `Persistable` impls in `kladde-traits`) -- no special-cased scalar
//!   setters, just a `{field}_mut()` accessor per field.
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
//! - Generic types and types with where-clauses are not yet supported.
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
use syn::{parse_macro_input, Data, DeriveInput, Fields};

#[proc_macro_derive(Persistable, attributes(kladde))]
pub fn derive_persistable(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    if !input.generics.params.is_empty() || input.generics.where_clause.is_some() {
        return syn::Error::new_spanned(
            &input.generics,
            "#[derive(Persistable)] does not yet support generic types or where-clauses \
             (see spec.md's Open Questions)",
        )
        .to_compile_error()
        .into();
    }

    let transparent = match transparent_attr(&input.attrs) {
        Ok(transparent) => transparent,
        Err(err) => return err.to_compile_error().into(),
    };

    let expanded = if transparent {
        derive_transparent(&input)
    } else {
        match &input.data {
            Data::Struct(data) => derive_struct(&input, data),
            Data::Enum(data) => derive_enum(&input, data),
            Data::Union(data) => syn::Error::new_spanned(
                data.union_token,
                "#[derive(Persistable)] does not support unions",
            )
            .to_compile_error(),
        }
    };

    expanded.into()
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
/// `DerefMut` impls. These are byte-identical across the struct, unit,
/// enum, and transparent derives -- each of those differs only in the
/// *accessor* `impl` block it adds on top (per-field `_mut()`, an enum's
/// whole-value `set`, a transparent newtype's `get_mut`, ...) and in its
/// `Persistable` body. Paired with [`guard_assoc`], which emits the
/// matching items *inside* the `Persistable` impl.
fn guard_scaffold(
    ident: &syn::Ident,
    vis: &syn::Visibility,
    guard_ident: &syn::Ident,
) -> proc_macro2::TokenStream {
    quote! {
        #[doc(hidden)]
        #vis struct #guard_ident<'s, B> {
            inner: &'s mut #ident,
            backend: &'s B,
            location: ::kladde_traits::Location,
        }

        impl<'s, B: ::kladde_traits::Backend> ::kladde_traits::Guard for #guard_ident<'s, B> {
            type Persistable = #ident;
            type Backend = B;

            fn as_persistable(&self) -> &#ident {
                self.inner
            }
            fn as_persistable_mut(&mut self) -> &mut #ident {
                self.inner
            }
            fn backend(&self) -> &B {
                self.backend
            }
        }

        impl<'s, B> ::std::ops::Deref for #guard_ident<'s, B> {
            type Target = #ident;
            fn deref(&self) -> &#ident {
                self.inner
            }
        }

        impl<'s, B> ::std::ops::DerefMut for #guard_ident<'s, B> {
            fn deref_mut(&mut self) -> &mut #ident {
                self.inner
            }
        }
    }
}

/// The `Guard` associated type and `guard()` constructor shared by every
/// derived `Persistable` impl -- the in-impl counterpart of
/// [`guard_scaffold`]'s out-of-impl items. Every derive kind builds the
/// same `{ inner, backend, location }` guard the same way.
fn guard_assoc(guard_ident: &syn::Ident) -> proc_macro2::TokenStream {
    quote! {
        type Guard<'s, B: ::kladde_traits::Backend>
            = #guard_ident<'s, B>
        where
            Self: 's,
            B: 's;

        fn guard<'s, B: ::kladde_traits::Backend>(
            &'s mut self,
            backend: &'s B,
            location: ::kladde_traits::Location,
        ) -> Self::Guard<'s, B> {
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
fn derive_transparent(input: &DeriveInput) -> proc_macro2::TokenStream {
    let ident = &input.ident;
    let vis = &input.vis;
    let guard_ident = format_ident!("{}Guard", ident);

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

    let scaffold = guard_scaffold(ident, vis, &guard_ident);
    let guard_assoc = guard_assoc(&guard_ident);

    quote! {
        #scaffold

        impl<'s, B: ::kladde_traits::Backend> #guard_ident<'s, B> {
            /// A mutable guard over the wrapped value. Since this is a
            /// `#[kladde(transparent)]` newtype, the inner value lives at
            /// the wrapper's own location, so this is a direct pass-through
            /// to the inner type's full mutation API.
            #vis fn get_mut(
                &mut self,
            ) -> <#field_ty as ::kladde_traits::Persistable>::Guard<'_, B> {
                <#field_ty as ::kladde_traits::Persistable>::guard(
                    &mut self.inner.#member,
                    self.backend,
                    self.location,
                )
            }
        }

        impl ::kladde_traits::Persistable for #ident {
            // Transparent: the wrapper *is* its one field, so it owns no
            // storage of its own and forwards everything at offset 0.
            const INLINE_SIZE: usize =
                <#field_ty as ::kladde_traits::Persistable>::INLINE_SIZE;

            #guard_assoc

            fn store<B: ::kladde_traits::Backend>(&mut self, backend: &B, location: ::kladde_traits::Location) {
                <#field_ty as ::kladde_traits::Persistable>::store(
                    &mut self.#member,
                    backend,
                    location,
                );
            }

            fn load<B: ::kladde_traits::Backend>(backend: &B, location: ::kladde_traits::Location) -> Self {
                let __value = <#field_ty as ::kladde_traits::Persistable>::load(backend, location);
                #construct
            }

            // Schema-transparent: reuse the inner type's descriptor instead
            // of registering our own node, so `Self` and the field type
            // share one fingerprint. Overriding `describe` (and leaving
            // `describe_local` at its default) is exactly the transparency
            // escape hatch documented on `Persistable::describe`.
            fn describe(builder: &mut ::kladde_traits::SchemaBuilder) -> ::kladde_traits::TypeRef {
                <#field_ty as ::kladde_traits::Persistable>::describe(builder)
            }
        }
    }
}

fn derive_struct(input: &DeriveInput, data: &syn::DataStruct) -> proc_macro2::TokenStream {
    let ident = &input.ident;
    let vis = &input.vis;
    let guard_ident = format_ident!("{}Guard", ident);

    let fields = match &data.fields {
        Fields::Named(fields) => &fields.named,
        Fields::Unit => {
            return derive_unit_like_struct(ident, vis, &guard_ident);
        }
        Fields::Unnamed(fields) => {
            return syn::Error::new_spanned(
                fields,
                "#[derive(Persistable)] does not yet support tuple structs, only named fields",
            )
            .to_compile_error();
        }
    };

    let field_ident: Vec<_> = fields.iter().map(|f| f.ident.clone().unwrap()).collect();
    let field_ty: Vec<_> = fields.iter().map(|f| f.ty.clone()).collect();
    let accessor_ident: Vec<_> = field_ident
        .iter()
        .map(|f| format_ident!("{}_mut", f))
        .collect();

    let field_offset = field_offsets(&field_ty);
    let total_size = total_size(&field_ty);

    let scaffold = guard_scaffold(ident, vis, &guard_ident);
    let guard_assoc = guard_assoc(&guard_ident);

    quote! {
        #scaffold

        impl<'s, B: ::kladde_traits::Backend> #guard_ident<'s, B> {
            #(
                #vis fn #accessor_ident(
                    &mut self,
                ) -> <#field_ty as ::kladde_traits::Persistable>::Guard<'_, B> {
                    <#field_ty as ::kladde_traits::Persistable>::guard(
                        &mut self.inner.#field_ident,
                        self.backend,
                        ::kladde_traits::Location {
                            anchor: self.location.anchor,
                            offset: self.location.offset + #field_offset,
                        },
                    )
                }
            )*
        }

        impl ::kladde_traits::Persistable for #ident {
            // A struct never owns an allocation of its own -- it's just
            // the sum of its fields' inline representations, threaded
            // through at static offsets.
            const INLINE_SIZE: usize = #total_size;

            #guard_assoc

            fn store<B: ::kladde_traits::Backend>(&mut self, backend: &B, location: ::kladde_traits::Location) {
                #(
                    ::kladde_traits::Persistable::store(
                        &mut self.#field_ident,
                        backend,
                        ::kladde_traits::Location {
                            anchor: location.anchor,
                            offset: location.offset + #field_offset,
                        },
                    );
                )*
            }

            fn load<B: ::kladde_traits::Backend>(backend: &B, location: ::kladde_traits::Location) -> Self {
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

            fn describe_local(
                __builder: &mut ::kladde_traits::SchemaBuilder,
            ) -> ::kladde_traits::TypeDescriptor {
                ::kladde_traits::TypeDescriptor::Struct {
                    name: ::std::string::ToString::to_string(::std::stringify!(#ident)),
                    fields: ::std::vec![
                        #(
                            ::kladde_traits::Field {
                                name: ::std::string::ToString::to_string(
                                    ::std::stringify!(#field_ident),
                                ),
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
    ident: &syn::Ident,
    vis: &syn::Visibility,
    guard_ident: &syn::Ident,
) -> proc_macro2::TokenStream {
    let scaffold = guard_scaffold(ident, vis, guard_ident);
    let guard_assoc = guard_assoc(guard_ident);

    quote! {
        #scaffold

        impl ::kladde_traits::Persistable for #ident {
            const INLINE_SIZE: usize = 0;

            #guard_assoc

            fn store<B: ::kladde_traits::Backend>(&mut self, _backend: &B, _location: ::kladde_traits::Location) {}

            fn load<B: ::kladde_traits::Backend>(_backend: &B, _location: ::kladde_traits::Location) -> Self {
                #ident
            }

            fn describe_local(
                _builder: &mut ::kladde_traits::SchemaBuilder,
            ) -> ::kladde_traits::TypeDescriptor {
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
fn derive_enum(input: &DeriveInput, data: &syn::DataEnum) -> proc_macro2::TokenStream {
    let ident = &input.ident;
    let vis = &input.vis;
    let guard_ident = format_ident!("{}Guard", ident);

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
    // `&mut FieldTy` via match ergonomics) and, negated, doesn't apply to
    // `load` (which reconstructs a value rather than matching one).
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
    // `derive_struct`'s `store` -- see that trait method's own doc
    // comment for why `&mut self` matters (a field may need to learn its
    // own allocation pointer for the first time here).
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

    let scaffold = guard_scaffold(ident, vis, &guard_ident);
    let guard_assoc = guard_assoc(&guard_ident);

    quote! {
        #scaffold

        impl<'s, B: ::kladde_traits::Backend> #guard_ident<'s, B> {
            /// Replaces the whole value with `value`. See `spec.md`'s
            /// Future Work for the deferred fine-grained alternative
            /// (mutating a field within the current variant in place,
            /// and/or matching directly on this guard).
            #vis fn set(&mut self, mut value: #ident) {
                ::kladde_traits::Persistable::store(&mut value, self.backend, self.location);
                *self.inner = value;
            }
        }

        impl ::kladde_traits::Persistable for #ident {
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

            fn store<B: ::kladde_traits::Backend>(&mut self, backend: &B, location: ::kladde_traits::Location) {
                #discriminants
                match self {
                    #(#variant_store_arm)*
                }
            }

            fn load<B: ::kladde_traits::Backend>(backend: &B, location: ::kladde_traits::Location) -> Self {
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
            ) -> ::kladde_traits::TypeDescriptor {
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
