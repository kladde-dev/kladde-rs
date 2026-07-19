//! `#[derive(Persistable)]` for application-level `struct`s and `enum`s
//! composed from already-`Persistable` fields. See `spec.md`'s "The Trait
//! Layer" and "Workspace Layout" for the pattern this generates, and
//! `V1_QUESTIONS.md` for the v1 scoping decisions this implements:
//!
//! - Every field is treated uniformly (including primitives, via blanket
//!   `Persistable` impls in `kladde-types`) -- no special-cased scalar
//!   setters, just a `{field}_mut()` accessor per field.
//! - Enums only support whole-value replacement (`guard.set(new_value)`),
//!   not mutating a field within the current variant in place.
//! - Generic types and types with where-clauses are not yet supported.
//!
//! **Dependency note:** generated code references `::kladde_traits::...`
//! and `::serde::...` paths directly, so any crate using this macro needs
//! `kladde-traits` and `serde` as *direct* dependencies too -- re-exports
//! (e.g. via `kladde-types`) aren't enough to make `::kladde_traits`
//! resolve. The same reason `#[derive(serde::Serialize)]` requires a
//! direct `serde` dependency, not just `serde_derive`.

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

    quote! {
        #[doc(hidden)]
        #vis struct #guard_ident<'s, B> {
            inner: &'s mut #ident,
            backend: &'s B,
        }

        impl<'s, B: ::kladde_traits::Backend> #guard_ident<'s, B> {
            #(
                #vis fn #accessor_ident(
                    &mut self,
                ) -> <#field_ty as ::kladde_traits::Persistable>::Guard<'_, B> {
                    <#field_ty as ::kladde_traits::Persistable>::guard(
                        &mut self.inner.#field_ident,
                        self.backend,
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
            // A struct never directly records an op of its own -- every
            // mutation is expressed as some leaf field's own `Op`,
            // recursively, so there's nothing for this type's `Op` to
            // carry. `()` is trivially `Serialize`/`Deserialize`.
            type Op = ();
            type Guard<'s, B: ::kladde_traits::Backend>
                = #guard_ident<'s, B>
            where
                Self: 's,
                B: 's;

            fn guard<'s, B: ::kladde_traits::Backend>(
                &'s mut self,
                backend: &'s B,
            ) -> Self::Guard<'s, B> {
                #guard_ident {
                    inner: self,
                    backend,
                }
            }
        }
    }
}

/// A unit struct (`struct Foo;`) has no fields to mutate at all -- same
/// shape as the struct case but with an empty accessor impl block.
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
            type Op = ();
            type Guard<'s, B: ::kladde_traits::Backend>
                = #guard_ident<'s, B>
            where
                Self: 's,
                B: 's;

            fn guard<'s, B: ::kladde_traits::Backend>(
                &'s mut self,
                backend: &'s B,
            ) -> Self::Guard<'s, B> {
                #guard_ident {
                    inner: self,
                    backend,
                }
            }
        }
    }
}

fn derive_enum(input: &DeriveInput, _data: &syn::DataEnum) -> proc_macro2::TokenStream {
    let ident = &input.ident;
    let vis = &input.vis;
    let guard_ident = format_ident!("{}Guard", ident);
    let op_ident = format_ident!("{}Op", ident);

    quote! {
        // v1 scope (see spec.md's Future Work): the only mutation is
        // replacing the whole value, so `Op` just carries the new value
        // wholesale -- no per-variant, per-field granularity yet.
        #[derive(::serde::Serialize, ::serde::Deserialize)]
        #[doc(hidden)]
        #vis enum #op_ident {
            Set(#ident),
        }

        #[doc(hidden)]
        #vis struct #guard_ident<'s, B> {
            inner: &'s mut #ident,
            backend: &'s B,
        }

        impl<'s, B: ::kladde_traits::Backend> #guard_ident<'s, B>
        where
            #ident: ::std::clone::Clone,
        {
            /// Replaces the whole value with `value`, recording a single
            /// `Set` op. See spec.md's Future Work for the deferred
            /// fine-grained alternative (mutating a field within the
            /// current variant in place, and/or matching directly on
            /// this guard).
            #vis fn set(&mut self, value: #ident) {
                ::kladde_traits::Journal::record::<#ident>(
                    self.backend,
                    &#op_ident::Set(::std::clone::Clone::clone(&value)),
                );
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

        impl ::kladde_traits::Persistable for #ident
        where
            #ident: ::serde::Serialize + ::serde::de::DeserializeOwned,
        {
            type Op = #op_ident;
            type Guard<'s, B: ::kladde_traits::Backend>
                = #guard_ident<'s, B>
            where
                Self: 's,
                B: 's;

            fn guard<'s, B: ::kladde_traits::Backend>(
                &'s mut self,
                backend: &'s B,
            ) -> Self::Guard<'s, B> {
                #guard_ident {
                    inner: self,
                    backend,
                }
            }
        }
    }
}
