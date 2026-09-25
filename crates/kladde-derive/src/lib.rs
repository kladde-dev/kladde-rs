//! `#[derive(Persistable)]`: turns a struct or enum composed of
//! `Persistable` fields into a backed type. Applications reach it through the
//! `kladde` crate, which re-exports it.
//!
//! For a type `T`, it generates
//!
//! - a `Persistable` implementation: `INLINE_SIZE`, `store`, `load`, `free`,
//!   and the schema descriptor;
//! - a guard type `TGuard`, with a `{field}_mut()` accessor per field
//!   (`field_{i}_mut()` for a tuple struct's positional fields), a `parts()`
//!   method handing out a guard for every field at once, and a whole-value
//!   `set` that stores the new value and then frees the old one, in one
//!   transaction;
//! - `Deref` on the guard, so read-only methods stay available.
//!
//! Layout:
//!
//! - A struct's `INLINE_SIZE` is the sum of its fields', each field at the
//!   static offset of the fields before it. It owns no allocation of its own.
//! - An enum's `INLINE_SIZE` is a 4-byte discriminant plus its largest
//!   variant, each variant laid out like a struct based past the
//!   discriminant, and the bytes a smaller variant leaves unused written as
//!   zeros. Only whole-value replacement is supported (`guard.set(value)`).
//! - A single-field struct marked `#[kladde(transparent)]` is persisted exactly
//!   as its field: same bytes and same fingerprint. Its guard has a `get_mut()`
//!   returning the field's guard.
//! - Type parameters get a `Persistable` bound, as `#[derive(Debug)]` does;
//!   lifetime and const parameters are rejected.
//!
//! **Every field must be `Persistable`.** Plain `String` is not, and the
//! compile error is the point: a field that would silently persist nothing
//! does not compile.
//!
//! ```compile_fail
//! #[derive(kladde::Persistable)]
//! struct Contact {
//!     name: String, // error[E0277]: the trait bound `String: Persistable` is not satisfied
//! }
//! ```
//!
//! A type that does implement it, such as a scalar or a container of
//! `kladde-types`, compiles:
//!
//! ```
//! #[derive(kladde::Persistable)]
//! struct Contact {
//!     age: u16,
//! }
//! ```
//!
//! `#[kladde(transparent)]` requires exactly one field:
//!
//! ```compile_fail
//! #[derive(kladde::Persistable)]
//! #[kladde(transparent)]
//! struct TwoFields {
//!     a: i32,
//!     b: i32, // error: #[kladde(transparent)] requires exactly one field
//! }
//! ```
//!
//! ```
//! #[derive(kladde::Persistable)]
//! #[kladde(transparent)]
//! struct Meters(i32);
//! ```
//!
//! **Paths.** Generated code names everything through `::kladde`, which
//! re-exports what it needs. A library built on `kladde-persist` without the
//! facade redirects it with `#[kladde(crate = "kladde_persist")]`.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, parse_quote, Data, DeriveInput, Fields, GenericParam};

#[proc_macro_derive(Persistable, attributes(kladde))]
pub fn derive_persistable(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    let (transparent, krate) = match kladde_attrs(&input.attrs) {
        Ok(parsed) => parsed,
        Err(err) => return err.to_compile_error().into(),
    };

    let ctx = match Ctx::build(&input, krate) {
        Ok(ctx) => ctx,
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

/// The generics plumbing every derive path shares, precomputed once from the
/// input type's own generic parameters:
///
/// - the *type*'s own `impl`/type/where fragments, with a `Persistable` bound
///   added to each type parameter (the `#[derive(Debug)]` heuristic); and
/// - the generated *guard*'s generic lists, which extend the type's own
///   parameters with a fresh lifetime `'__s` and backend `__B`, given
///   underscore-prefixed names so that a user type like `struct Foo<B>` does
///   not clash with them -- the same hygiene trick serde's derive uses.
///
/// Only type parameters are supported; lifetime and const parameters are
/// rejected in [`build`](Ctx::build).
struct Ctx {
    /// The `impl` generics of the `Persistable` impl: the type's own
    /// parameters, each bounded by `Persistable<Pointer>`.
    impl_generics: proc_macro2::TokenStream,
    /// The type's type generics, e.g. `<T>` (empty for a non-generic type).
    type_generics: proc_macro2::TokenStream,
    /// The type's own `where`-clause, verbatim (empty if none).
    where_clause: proc_macro2::TokenStream,
    /// Generic list for *declaring* the guard struct and for every `impl`
    /// block on it.
    guard_impl_generics: proc_macro2::TokenStream,
    /// Generic list for *naming* the guard type (bare parameter names).
    guard_use_generics: proc_macro2::TokenStream,
    /// The type's type parameters rendered with their bounds, for building
    /// further generic lists (the `Parts` struct) that need a different
    /// lifetime than the guard's.
    bounded_params: Vec<proc_macro2::TokenStream>,
    /// The type's type parameters as bare idents.
    param_idents: Vec<syn::Ident>,
    /// The crate every generated path is rooted at -- `::kladde` unless
    /// `#[kladde(crate = "...")]` says otherwise.
    krate: syn::Path,
}

impl Ctx {
    fn build(input: &DeriveInput, krate: syn::Path) -> syn::Result<Ctx> {
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
                        "#[derive(Persistable)] does not support const generic parameters",
                    ));
                }
            }
        }

        let mut bounded = input.generics.clone();
        for tp in bounded.type_params_mut() {
            tp.bounds.push(parse_quote!(
                #krate::Persistable<#krate::Pointer>
            ));
        }
        let (_, type_generics, where_clause) = bounded.split_for_impl();

        let bounded_params: Vec<proc_macro2::TokenStream> =
            bounded.type_params().map(|tp| quote!(#tp)).collect();
        let param_idents: Vec<syn::Ident> = input
            .generics
            .type_params()
            .map(|tp| tp.ident.clone())
            .collect();

        Ok(Ctx {
            impl_generics: quote! { <#(#bounded_params,)*> },
            type_generics: quote!(#type_generics),
            where_clause: quote!(#where_clause),
            guard_impl_generics: quote! {
                <'__s, #(#bounded_params,)* __B: #krate::WriteBackend<Pointer = #krate::Pointer>>
            },
            guard_use_generics: quote! { <'__s, #(#param_idents,)* __B> },
            bounded_params,
            param_idents,
            krate,
        })
    }
}

/// Parses the type's `#[kladde(...)]` attributes: whether it is
/// `transparent`, and the `crate` generated paths are rooted at. Errors on
/// anything else, so that a typo is a compile error rather than a no-op.
fn kladde_attrs(attrs: &[syn::Attribute]) -> syn::Result<(bool, syn::Path)> {
    let mut transparent = false;
    let mut krate: syn::Path = syn::parse_quote!(::kladde);
    for attr in attrs {
        if !attr.path().is_ident("kladde") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("transparent") {
                transparent = true;
                Ok(())
            } else if meta.path.is_ident("crate") {
                let lit: syn::LitStr = meta.value()?.parse()?;
                krate = lit.parse()?;
                Ok(())
            } else {
                Err(meta.error(
                    "unknown `#[kladde(...)]` option; supported are `transparent` and `crate`",
                ))
            }
        })?;
    }
    Ok((transparent, krate))
}

/// For fields laid out back to back starting at `base` bytes into the value
/// (0 for a struct, 4 for an enum variant), each field's static offset: the
/// sum of every earlier field's `INLINE_SIZE`, as the backend's `Size`.
fn field_offsets(
    field_ty: &[syn::Type],
    base: usize,
    krate: &syn::Path,
) -> Vec<proc_macro2::TokenStream> {
    (0..field_ty.len())
        .map(|i| {
            let earlier = &field_ty[..i];
            quote! {
                <<__B as #krate::Backend>::Size as #krate::Word>::from_usize(
                    #base #( + <#earlier as #krate::Persistable<#krate::Pointer>>::INLINE_SIZE )*
                )
            }
        })
        .collect()
}

/// The total inline size of fields laid out back to back.
fn total_size(field_ty: &[syn::Type], krate: &syn::Path) -> proc_macro2::TokenStream {
    quote! {
        0usize #( + <#field_ty as #krate::Persistable<#krate::Pointer>>::INLINE_SIZE )*
    }
}

/// The guard type every derive kind shares: a `{ inner, backend, location }`
/// struct, its `Guard` and `Deref` impls, and the whole-value `set`. Each kind
/// adds accessors of its own on top.
fn guard_scaffold(
    ctx: &Ctx,
    ident: &syn::Ident,
    vis: &syn::Visibility,
    guard_ident: &syn::Ident,
) -> proc_macro2::TokenStream {
    let krate = &ctx.krate;
    let Ctx {
        type_generics,
        where_clause,
        guard_impl_generics,
        guard_use_generics,
        ..
    } = ctx;
    let doc =
        format!("The guard of [`{ident}`]: records and applies mutations of a backed `{ident}`.");

    quote! {
        #[doc = #doc]
        #vis struct #guard_ident #guard_impl_generics #where_clause {
            inner: &'__s mut #ident #type_generics,
            backend: &'__s __B,
            location: #krate::Location<#krate::Pointer, <__B as #krate::Backend>::Size>,
        }

        impl #guard_impl_generics #guard_ident #guard_use_generics #where_clause {
            /// Replaces the whole value: stores `value`, which publishes it,
            /// then frees what the old value owned, in one transaction. If
            /// anything fails, the value is left as it was.
            #vis fn set(
                &mut self,
                value: #ident #type_generics,
            ) -> ::std::result::Result<(), #krate::Error> {
                #krate::replace(&mut *self.inner, value, self.backend, self.location)
            }
        }

        impl #guard_impl_generics #krate::Guard
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
    }
}

/// The `Guard` associated type and `guard()` constructor every derived
/// `Persistable` impl shares.
fn guard_assoc(ctx: &Ctx, guard_ident: &syn::Ident) -> proc_macro2::TokenStream {
    let krate = &ctx.krate;
    let Ctx {
        guard_use_generics, ..
    } = ctx;
    quote! {
        type Guard<'__s, __B: #krate::WriteBackend<Pointer = #krate::Pointer>>
            = #guard_ident #guard_use_generics
        where
            Self: '__s,
            __B: '__s;

        #[inline]
        fn guard<'__s, __B: #krate::WriteBackend<Pointer = #krate::Pointer>>(
            &'__s mut self,
            backend: &'__s __B,
            location: #krate::Location<#krate::Pointer, <__B as #krate::Backend>::Size>,
        ) -> Self::Guard<'__s, __B> {
            #guard_ident {
                inner: self,
                backend,
                location,
            }
        }
    }
}

/// The `store`/`load`/`free` signatures, which every derive kind spells the
/// same way.
fn store_sig(krate: &syn::Path) -> proc_macro2::TokenStream {
    quote! {
        fn store<__B: #krate::WriteBackend<Pointer = #krate::Pointer>>(
            &mut self,
            backend: &__B,
            location: #krate::Location<#krate::Pointer, <__B as #krate::Backend>::Size>,
        ) -> ::std::result::Result<(), #krate::Error>
    }
}

fn load_sig(krate: &syn::Path) -> proc_macro2::TokenStream {
    quote! {
        fn load<__B: #krate::ReadBackend<Pointer = #krate::Pointer>>(
            backend: &mut __B,
            location: #krate::Location<#krate::Pointer, <__B as #krate::Backend>::Size>,
        ) -> ::std::result::Result<Self, #krate::Error>
    }
}

fn free_sig(krate: &syn::Path) -> proc_macro2::TokenStream {
    quote! {
        fn free<__B: #krate::WriteBackend<Pointer = #krate::Pointer>>(
            &mut self,
            backend: &__B,
        ) -> ::std::result::Result<(), #krate::Error>
    }
}

/// `#[kladde(transparent)]`, analogous to `#[serde(transparent)]`: a
/// single-field newtype persisted *exactly* as its one field. The impl
/// delegates everything to the field at the wrapper's own location, and is
/// **schema-transparent**: it overrides `describe` to reuse the field's
/// descriptor rather than registering a node of its own, so the two share one
/// fingerprint.
fn derive_transparent(input: &DeriveInput, ctx: &Ctx) -> proc_macro2::TokenStream {
    let krate = &ctx.krate;
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
    let (store_sig, load_sig, free_sig) = (store_sig(krate), load_sig(krate), free_sig(krate));

    quote! {
        #scaffold

        impl #guard_impl_generics #guard_ident #guard_use_generics #where_clause {
            /// The guard of the wrapped value, which lives at the wrapper's
            /// own location: its whole mutation API, passed through.
            #[inline]
            #vis fn get_mut(
                &mut self,
            ) -> <#field_ty as #krate::Persistable<#krate::Pointer>>::Guard<'_, __B> {
                <#field_ty as #krate::Persistable<#krate::Pointer>>::guard(
                    &mut self.inner.#member,
                    self.backend,
                    self.location,
                )
            }
        }

        impl #impl_generics #krate::Persistable<#krate::Pointer> for #ident #type_generics #where_clause {
            const INLINE_SIZE: usize =
                <#field_ty as #krate::Persistable<#krate::Pointer>>::INLINE_SIZE;

            #guard_assoc

            #store_sig {
                <#field_ty as #krate::Persistable<#krate::Pointer>>::store(
                    &mut self.#member,
                    backend,
                    location,
                )
            }

            #load_sig {
                let __value =
                    <#field_ty as #krate::Persistable<#krate::Pointer>>::load(backend, location)?;
                ::std::result::Result::Ok(#construct)
            }

            #free_sig {
                <#field_ty as #krate::Persistable<#krate::Pointer>>::free(&mut self.#member, backend)
            }

            fn describe(builder: &mut #krate::SchemaBuilder) -> #krate::TypeRef
            where
                Self: 'static,
            {
                <#field_ty as #krate::Persistable<#krate::Pointer>>::describe(builder)
            }
        }
    }
}

fn derive_struct(
    input: &DeriveInput,
    data: &syn::DataStruct,
    ctx: &Ctx,
) -> proc_macro2::TokenStream {
    let krate = &ctx.krate;
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

    // Named and tuple structs are laid out identically; only how each field
    // is *named* differs.
    let fields: Vec<&syn::Field> = match &data.fields {
        Fields::Named(fields) => fields.named.iter().collect(),
        Fields::Unnamed(fields) => fields.unnamed.iter().collect(),
        Fields::Unit => {
            return derive_unit_like_struct(ctx, ident, vis, &guard_ident);
        }
    };

    let field_ty: Vec<syn::Type> = fields.iter().map(|f| f.ty.clone()).collect();
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
    let accessor_doc: Vec<String> = fields
        .iter()
        .enumerate()
        .map(|(i, f)| match &f.ident {
            Some(name) => format!("The guard of field `{name}`."),
            None => format!("The guard of field `{i}`."),
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

    let field_offset = field_offsets(&field_ty, 0, krate);
    let total_size = total_size(&field_ty, krate);

    let is_tuple = matches!(&data.fields, Fields::Unnamed(_));
    let load_body = if is_tuple {
        quote! {
            #ident(
                #(
                    <#field_ty as #krate::Persistable<#krate::Pointer>>::load(
                        backend,
                        location + #field_offset,
                    )?,
                )*
            )
        }
    } else {
        let field_ident: Vec<&syn::Ident> =
            fields.iter().map(|f| f.ident.as_ref().unwrap()).collect();
        quote! {
            #ident {
                #(
                    #field_ident: <#field_ty as #krate::Persistable<#krate::Pointer>>::load(
                        backend,
                        location + #field_offset,
                    )?,
                )*
            }
        }
    };

    // A `{Ident}Parts` struct plus a `parts()` method handing out a guard for
    // *every* field at once, each borrowing a disjoint part of `self.inner`,
    // so that all fields can be mutated simultaneously.
    let parts_ident = format_ident!("{}Parts", ident);
    let bounded_params = &ctx.bounded_params;
    let param_idents = &ctx.param_idents;
    let parts_decl_generics = quote! {
        <'__f, #(#bounded_params,)* __B: #krate::WriteBackend<Pointer = #krate::Pointer>>
    };
    let parts_ret_generics = quote! { <'_, #(#param_idents,)* __B> };
    // The `Parts` struct holds `Guard<'__f, __B>` associated types, whose GAT
    // bounds need `Param: '__f` and `__B: '__f`.
    let user_where_preds = input.generics.where_clause.as_ref().map(|w| {
        let preds = &w.predicates;
        quote!(#preds,)
    });
    let parts_where = quote! {
        where #user_where_preds #(#param_idents: '__f,)* __B: '__f
    };
    let parts_doc = format!("The guards of every field of [`{ident}`] at once.");
    let (parts_struct, parts_ctor) = if is_tuple {
        let struct_def = quote! {
            #[doc = #parts_doc]
            #vis struct #parts_ident #parts_decl_generics (
                #(
                    #vis <#field_ty as #krate::Persistable<#krate::Pointer>>::Guard<'__f, __B>,
                )*
            ) #parts_where;
        };
        let ctor = quote! {
            #parts_ident(
                #(
                    <#field_ty as #krate::Persistable<#krate::Pointer>>::guard(
                        &mut self.inner.#member,
                        self.backend,
                        self.location + #field_offset,
                    ),
                )*
            )
        };
        (struct_def, ctor)
    } else {
        let field_ident: Vec<&syn::Ident> =
            fields.iter().map(|f| f.ident.as_ref().unwrap()).collect();
        let struct_def = quote! {
            #[doc = #parts_doc]
            #vis struct #parts_ident #parts_decl_generics #parts_where {
                #(
                    #[doc = #accessor_doc]
                    #vis #field_ident:
                        <#field_ty as #krate::Persistable<#krate::Pointer>>::Guard<'__f, __B>,
                )*
            }
        };
        let ctor = quote! {
            #parts_ident {
                #(
                    #field_ident: <#field_ty as #krate::Persistable<#krate::Pointer>>::guard(
                        &mut self.inner.#member,
                        self.backend,
                        self.location + #field_offset,
                    ),
                )*
            }
        };
        (struct_def, ctor)
    };

    let scaffold = guard_scaffold(ctx, ident, vis, &guard_ident);
    let guard_assoc = guard_assoc(ctx, &guard_ident);
    let (store_sig, load_sig, free_sig) = (store_sig(krate), load_sig(krate), free_sig(krate));

    quote! {
        #scaffold

        #parts_struct

        impl #guard_impl_generics #guard_ident #guard_use_generics #where_clause {
            #(
                #[doc = #accessor_doc]
                #[inline]
                #vis fn #accessor_ident(
                    &mut self,
                ) -> <#field_ty as #krate::Persistable<#krate::Pointer>>::Guard<'_, __B> {
                    <#field_ty as #krate::Persistable<#krate::Pointer>>::guard(
                        &mut self.inner.#member,
                        self.backend,
                        self.location + #field_offset,
                    )
                }
            )*

            /// A guard for every field at once, so that all fields can be
            /// mutated simultaneously.
            #[inline]
            #vis fn parts(&mut self) -> #parts_ident #parts_ret_generics {
                #parts_ctor
            }
        }

        impl #impl_generics #krate::Persistable<#krate::Pointer> for #ident #type_generics #where_clause {
            const INLINE_SIZE: usize = #total_size;

            #guard_assoc

            #store_sig {
                #(
                    <#field_ty as #krate::Persistable<#krate::Pointer>>::store(
                        &mut self.#member,
                        backend,
                        location + #field_offset,
                    )?;
                )*
                ::std::result::Result::Ok(())
            }

            #load_sig {
                ::std::result::Result::Ok(#load_body)
            }

            #free_sig {
                #(
                    <#field_ty as #krate::Persistable<#krate::Pointer>>::free(
                        &mut self.#member,
                        backend,
                    )?;
                )*
                ::std::result::Result::Ok(())
            }

            fn describe_local(
                __builder: &mut #krate::SchemaBuilder,
            ) -> #krate::TypeDescriptor
            where
                Self: 'static,
            {
                #krate::TypeDescriptor::Struct {
                    name: ::std::string::ToString::to_string(::std::stringify!(#ident)),
                    fields: ::std::vec![
                        #(
                            #krate::Field {
                                name: ::std::string::ToString::to_string(#schema_name),
                                ty: <#field_ty as #krate::Persistable<#krate::Pointer>>::describe(
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

/// A unit struct (`struct Foo;`): no fields, `INLINE_SIZE = 0`.
fn derive_unit_like_struct(
    ctx: &Ctx,
    ident: &syn::Ident,
    vis: &syn::Visibility,
    guard_ident: &syn::Ident,
) -> proc_macro2::TokenStream {
    let krate = &ctx.krate;
    let Ctx {
        impl_generics,
        type_generics,
        where_clause,
        ..
    } = ctx;
    let scaffold = guard_scaffold(ctx, ident, vis, guard_ident);
    let guard_assoc = guard_assoc(ctx, guard_ident);
    let (store_sig, load_sig) = (store_sig(krate), load_sig(krate));

    quote! {
        #scaffold

        impl #impl_generics #krate::Persistable<#krate::Pointer> for #ident #type_generics #where_clause {
            const INLINE_SIZE: usize = 0;

            #guard_assoc

            #[allow(unused_variables)]
            #store_sig {
                ::std::result::Result::Ok(())
            }

            #[allow(unused_variables)]
            #load_sig {
                ::std::result::Result::Ok(#ident)
            }

            fn describe_local(
                _builder: &mut #krate::SchemaBuilder,
            ) -> #krate::TypeDescriptor
            where
                Self: 'static,
            {
                #krate::TypeDescriptor::Struct {
                    name: ::std::string::ToString::to_string(::std::stringify!(#ident)),
                    fields: ::std::vec![],
                }
            }
        }
    }
}

/// Inline layout: a 4-byte discriminant followed by the variant's fields,
/// laid out like a struct's but based at offset 4, and zeros up to the size
/// of the largest variant, so that `store` always writes exactly
/// `INLINE_SIZE` bytes.
///
/// The discriminant follows Rust's own rule: the explicit value where the
/// author wrote one (`A = 42`), otherwise `predecessor + 1`. So the value
/// stored on disk, and reported in the schema, equals the enum's real Rust
/// discriminant, pinned values stay stable, and uniqueness is inherited from
/// Rust's own check. `store`, `load`, and `describe` read it from one
/// generated `const` chain.
fn derive_enum(input: &DeriveInput, data: &syn::DataEnum, ctx: &Ctx) -> proc_macro2::TokenStream {
    let krate = &ctx.krate;
    let ident = &input.ident;
    let vis = &input.vis;
    let guard_ident = format_ident!("{}Guard", ident);
    let Ctx {
        impl_generics,
        type_generics,
        where_clause,
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
        .map(|tys| field_offsets(tys, 4, krate))
        .collect();
    let variant_size: Vec<proc_macro2::TokenStream> = variant_field_ty
        .iter()
        .map(|tys| total_size(tys, krate))
        .collect();
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

    // A match pattern binding a variant's fields, matched against `&mut Self`
    // so that bindings come out as `&mut FieldTy`.
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

    // `store`: the discriminant, each field at its static offset, and zeros
    // for what the largest variant has beyond this one.
    let variant_store_arm: Vec<proc_macro2::TokenStream> = (0..variant_count)
        .map(|i| {
            let pattern = &variant_pattern[i];
            let bindings = &variant_binding[i];
            let field_offset = &variant_field_offset[i];
            let field_ty = &variant_field_ty[i];
            let size = &variant_size[i];
            quote! {
                #pattern => {
                    #krate::WriteBackend::write(
                        backend,
                        location.anchor,
                        location.offset,
                        &DISC[#i].to_le_bytes(),
                    )?;
                    #(
                        <#field_ty as #krate::Persistable<#krate::Pointer>>::store(
                            #bindings,
                            backend,
                            location + #field_offset,
                        )?;
                    )*
                    let __used = 4 + #size;
                    let __padding = <Self as #krate::Persistable<#krate::Pointer>>::INLINE_SIZE - __used;
                    if __padding > 0 {
                        let __at = location
                            + <<__B as #krate::Backend>::Size as #krate::Word>::from_usize(__used);
                        #krate::WriteBackend::write(
                            backend,
                            __at.anchor,
                            __at.offset,
                            &::std::vec![0u8; __padding],
                        )?;
                    }
                }
            }
        })
        .collect();

    let variant_free_arm: Vec<proc_macro2::TokenStream> = (0..variant_count)
        .map(|i| {
            let pattern = &variant_pattern[i];
            let bindings = &variant_binding[i];
            let field_ty = &variant_field_ty[i];
            quote! {
                #pattern => {
                    #(
                        <#field_ty as #krate::Persistable<#krate::Pointer>>::free(#bindings, backend)?;
                    )*
                }
            }
        })
        .collect();

    let variant_load_expr: Vec<proc_macro2::TokenStream> = (0..variant_count)
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
                                #field_ident: <#field_ty as #krate::Persistable<#krate::Pointer>>::load(
                                    backend,
                                    location + #field_offset,
                                )?,
                            )*
                        }
                    }
                }
                Fields::Unnamed(_) => quote! {
                    #ident::#v_ident(
                        #(
                            <#field_ty as #krate::Persistable<#krate::Pointer>>::load(
                                backend,
                                location + #field_offset,
                            )?,
                        )*
                    )
                },
                Fields::Unit => quote! { #ident::#v_ident },
            }
        })
        .collect();

    let variant_describe: Vec<proc_macro2::TokenStream> = (0..variant_count)
        .map(|i| {
            let v_ident = &variant_ident[i];
            let field_name = &variant_field_name[i];
            let field_ty = &variant_field_ty[i];
            quote! {
                #krate::Variant {
                    discriminant: DISC[#i] as u64,
                    name: ::std::string::ToString::to_string(::std::stringify!(#v_ident)),
                    fields: ::std::vec![
                        #(
                            #krate::Field {
                                name: ::std::string::ToString::to_string(#field_name),
                                ty: <#field_ty as #krate::Persistable<#krate::Pointer>>::describe(
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
    let (store_sig, load_sig, free_sig) = (store_sig(krate), load_sig(krate), free_sig(krate));

    quote! {
        #scaffold

        impl #impl_generics #krate::Persistable<#krate::Pointer> for #ident #type_generics #where_clause {
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

            #store_sig {
                #discriminants
                match self {
                    #(#variant_store_arm)*
                }
                ::std::result::Result::Ok(())
            }

            #load_sig {
                #discriminants
                let discriminant = {
                    let mut __buf = [0u8; 4];
                    let mut __cursor = #krate::ReadBackend::read_at(
                        backend,
                        location.anchor,
                        location.offset,
                    )?;
                    ::std::io::Read::read_exact(&mut __cursor, &mut __buf)?;
                    u32::from_le_bytes(__buf)
                };
                #(
                    if discriminant == DISC[#variant_index] {
                        return ::std::result::Result::Ok(#variant_load_expr);
                    }
                )*
                ::std::result::Result::Err(#krate::Error::Corrupt(::std::format!(
                    "unknown discriminant {} of {}",
                    discriminant,
                    ::std::stringify!(#ident),
                )))
            }

            #free_sig {
                match self {
                    #(#variant_free_arm)*
                }
                ::std::result::Result::Ok(())
            }

            fn describe_local(
                __builder: &mut #krate::SchemaBuilder,
            ) -> #krate::TypeDescriptor
            where
                Self: 'static,
            {
                #discriminants
                #krate::TypeDescriptor::Enum {
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
