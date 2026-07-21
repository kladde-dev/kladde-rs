//! `#[derive(Persistable)]` for application-level `struct`s and `enum`s
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
//!   based at offset 4 instead of 0), so an enum never owns an
//!   allocation of its own either, and needs no `serde`/`postcard` at
//!   all (unlike an earlier version of this macro, which treated a
//!   derived enum as a `postcard`-serialized "owning blob"). Only
//!   whole-value replacement is supported for now (`guard.set(new_value)`)
//!   -- mutating a field within the current variant in place, and/or
//!   matching directly on a generated `Guard`, is deferred (see
//!   `spec.md`'s Future Work).
//! - Generic types and types with where-clauses are not yet supported.
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

#[proc_macro_derive(Persistable)]
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

    let expanded = match &input.data {
        Data::Struct(data) => derive_struct(&input, data),
        Data::Enum(data) => derive_enum(&input, data),
        Data::Union(data) => syn::Error::new_spanned(
            data.union_token,
            "#[derive(Persistable)] does not support unions",
        )
        .to_compile_error(),
    };

    expanded.into()
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

    quote! {
        #[doc(hidden)]
        #vis struct #guard_ident<'s, B> {
            inner: &'s mut #ident,
            backend: &'s B,
            location: ::kladde_traits::Location,
        }

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

        impl ::kladde_traits::Persistable for #ident {
            // A struct never owns an allocation of its own -- it's just
            // the sum of its fields' inline representations, threaded
            // through at static offsets.
            const INLINE_SIZE: usize = #total_size;

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

        impl ::kladde_traits::Persistable for #ident {
            const INLINE_SIZE: usize = 0;

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

            fn store<B: ::kladde_traits::Backend>(&mut self, _backend: &B, _location: ::kladde_traits::Location) {}

            fn load<B: ::kladde_traits::Backend>(_backend: &B, _location: ::kladde_traits::Location) -> Self {
                #ident
            }
        }
    }
}

/// Inline layout: a 4-byte discriminant (this macro's own, assigned in
/// declaration order -- unrelated to any `#[repr]`/explicit discriminant
/// on the source enum) followed by whichever variant's own fields, laid
/// out exactly like a struct's (see `field_offsets`/`total_size`) but
/// based at offset 4 instead of 0. Sized to fit the *largest* variant,
/// since the same bytes have to be able to hold any of them -- unused
/// tail bytes for a smaller variant are simply never read, the same way
/// a `union`'s would be.
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
    let discriminant: Vec<u32> = (0..data.variants.len() as u32).collect();

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
            let disc = discriminant[i];
            quote! {
                #pattern => {
                    ::kladde_traits::Allocator::write(
                        backend,
                        location.anchor,
                        location.offset,
                        &(#disc as u32).to_le_bytes(),
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

    quote! {
        #[doc(hidden)]
        #vis struct #guard_ident<'s, B> {
            inner: &'s mut #ident,
            backend: &'s B,
            location: ::kladde_traits::Location,
        }

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

            fn store<B: ::kladde_traits::Backend>(&mut self, backend: &B, location: ::kladde_traits::Location) {
                match self {
                    #(#variant_store_arm)*
                }
            }

            fn load<B: ::kladde_traits::Backend>(backend: &B, location: ::kladde_traits::Location) -> Self {
                let discriminant_bytes = ::kladde_traits::Allocator::read(backend, location.anchor, location.offset, 4);
                let discriminant = u32::from_le_bytes(discriminant_bytes.try_into().unwrap());
                match discriminant {
                    #(#discriminant => #variant_load_expr,)*
                    other => panic!(
                        "corrupt persisted {}: unknown discriminant {}",
                        stringify!(#ident),
                        other,
                    ),
                }
            }
        }
    }
}
