//! `#[derive(Persistable)]`: turns a struct or enum composed of
//! `Persistable` fields into a backed type. Applications reach it through the
//! `kladde` crate, which re-exports it.
//!
//! For a type `T`, it generates
//!
//! - a `Persistable` implementation: both encodings (`encoded_size`, `encode`,
//!   `decode`), `prepare`, `free`, and the schema descriptor;
//! - a guard type `TGuard`, with a `{field}_mut()` accessor per field
//!   (`field_{i}_mut()` for a tuple struct's positional fields), a `parts()`
//!   method handing out a guard for every field at once, and a whole-value
//!   `set` that stores the new value and then frees the old one, in one
//!   transaction;
//! - `Deref` on the guard, so read-only methods stay available.
//!
//! The guard's last type parameter is the encoding of the place the value
//! sits in, `Slotted` by default: `TGuard<'_, B>` is the guard of a value in a
//! slotted place, `TGuard<'_, B, Packed>` that of one in a packed place. Every
//! field inherits it.
//!
//! Layout:
//!
//! - A struct's fixed encoding is its fields' fixed encodings back to back,
//!   each field at the static offset of the fields before it; its packed
//!   encoding is its fields' packed encodings back to back. It owns no
//!   allocation of its own.
//! - An enum's fixed encoding is its discriminant, its variant's fields laid
//!   out like a struct's, and zeros up to the size of the largest variant;
//!   its packed encoding is the discriminant as a varint followed by the
//!   variant's fields, with no padding. The discriminant's fixed width is
//!   what an integer `#[repr(u8 | u16 | u32 | u64)]` says, or else the
//!   smallest of 1, 2, 4 and 8 bytes that holds the largest discriminant
//!   value. Its guard's `parts()` returns a generated `{Enum}Parts` enum
//!   holding the guards of the current variant's fields, for mutating them in
//!   place; `set` replaces the whole value, which is how the variant changes.
//! - A single-field struct marked `#[kladde(transparent)]` is persisted exactly
//!   as its field: same bytes and same fingerprint. Its guard has a `get_mut()`
//!   returning the field's guard.
//! - Type parameters get a `Persistable` bound, as `#[derive(Debug)]` does;
//!   lifetime and const parameters are rejected.
//!
//! In a packed place, a field whose encoding changes size -- an enum that
//! switches variant, an integer whose varint grows -- splices its new
//! encoding in and tells its parent, so the guards of its siblings, and of
//! everything around it, find themselves at their new offsets.
//!
//! **Slotted fields and packed-only types.** A field marked
//! `#[kladde(slotted)]` keeps its fixed encoding inside a packed value, so
//! that it never changes size: for a counter, say, whose varint would grow
//! at every power of 128. Its type must be `Slottable`. The derive implements
//! `Slottable` for every type whose fields all have a fixed encoding; a type
//! with a field that has none, such as a small string, is marked
//! `#[kladde(packed_only)]`, gets no `Slottable` impl, and stands only in
//! packed places.
//!
//! ```
//! use kladde::{Kladde, Packed, Persistable};
//! use kladde_types::{PackedPersistableVec, SmallPersistableString};
//!
//! #[derive(Persistable)]
//! #[kladde(packed_only)]
//! struct Page {
//!     title: SmallPersistableString,
//!     #[kladde(slotted)]
//!     visits: u32,
//! }
//!
//! let page = Page { title: "home".into(), visits: 300 };
//! // A tag and four bytes of text, then the count's four fixed bytes.
//! assert_eq!(page.encoded_size::<Packed>(), 1 + 4 + 4);
//! let mut pages = Kladde::new(PackedPersistableVec::<Page>::new());
//! pages.guard().push(page)?;
//! pages.guard().get_mut(0).unwrap().visits_mut().set(301)?;
//! # Ok::<(), kladde::Error>(())
//! ```
//!
//! A type with such a field that is not marked does not compile:
//!
//! ```compile_fail
//! use kladde_types::SmallPersistableString;
//!
//! #[derive(kladde::Persistable)]
//! struct Page {
//!     title: SmallPersistableString, // error: `Page` has a field without a fixed encoding, `title`: mark `Page` #[kladde(packed_only)], ...
//! }
//! ```
//!
//! Nor does a slotted field without a fixed encoding:
//!
//! ```compile_fail
//! use kladde_types::SmallPersistableString;
//!
//! #[derive(kladde::Persistable)]
//! #[kladde(packed_only)]
//! struct Page {
//!     #[kladde(slotted)]
//!     title: SmallPersistableString, // error[E0277]: `SmallPersistableString` has no fixed encoding, so it cannot stand in a slotted place
//! }
//! ```
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
//! **Enums.** `parts()` reaches into the current variant:
//!
//! ```
//! use kladde::{Kladde, Persistable};
//!
//! #[derive(Persistable)]
//! enum Shape {
//!     Origin,
//!     Circle(i32),
//!     Rectangle { width: i32, height: i32 },
//! }
//!
//! let mut shape = Kladde::new(Shape::Rectangle { width: 1, height: 2 });
//! match shape.guard().parts() {
//!     ShapeParts::Rectangle { mut width, .. } => width.set(3).unwrap(),
//!     ShapeParts::Circle(mut radius) => radius.set(3).unwrap(),
//!     ShapeParts::Origin => {}
//! }
//! assert!(matches!(shape.get(), Shape::Rectangle { width: 3, height: 2 }));
//!
//! // Three variants fit a one-byte discriminant.
//! assert_eq!(<Shape as kladde::Persistable>::SLOTTED_SIZE, Some(1 + 8));
//! ```
//!
//! The variant cannot be switched while a field's guard is alive:
//!
//! ```compile_fail
//! use kladde::{Kladde, Persistable};
//!
//! #[derive(Persistable)]
//! enum Shape {
//!     Origin,
//!     Circle(i32),
//! }
//!
//! let mut shape = Kladde::new(Shape::Circle(1));
//! let mut guard = shape.guard();
//! if let ShapeParts::Circle(mut radius) = guard.parts() {
//!     guard.set(Shape::Origin).unwrap(); // error[E0499]: cannot borrow `guard` as mutable more than once
//!     radius.set(2).unwrap();
//! }
//! ```
//!
//! An integer `#[repr]` fixes the discriminant's width, and only unsigned
//! ones of up to 64 bits are accepted, since discriminants are stored
//! unsigned:
//!
//! ```
//! #[derive(kladde::Persistable)]
//! #[repr(u32)]
//! enum Pinned {
//!     A,
//!     B,
//! }
//! assert_eq!(<Pinned as kladde::Persistable>::SLOTTED_SIZE, Some(4));
//! ```
//!
//! ```compile_fail
//! #[derive(kladde::Persistable)]
//! #[repr(i8)] // error: #[derive(Persistable)] supports only the integer reprs `u8`, ...
//! enum Signed {
//!     A,
//!     B,
//! }
//! ```
//!
//! ```compile_fail
//! #[derive(kladde::Persistable)]
//! enum Negative {
//!     A = -1, // error: #[derive(Persistable)] does not support negative discriminants
//!     B,
//! }
//! ```
//!
//! **Paths.** Generated code names everything through `::kladde`, which
//! re-exports what it needs. A library built on `kladde-persist` without the
//! facade redirects it with `#[kladde(crate = "kladde_persist")]`.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, parse_quote, Data, DeriveInput, Fields, GenericParam};

type Tokens = proc_macro2::TokenStream;

#[proc_macro_derive(Persistable, attributes(kladde))]
pub fn derive_persistable(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    let TypeAttrs {
        transparent,
        packed_only,
        krate,
    } = match kladde_attrs(&input.attrs) {
        Ok(parsed) => parsed,
        Err(err) => return err.to_compile_error().into(),
    };

    let ctx = match Ctx::build(&input, krate, packed_only) {
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
///   parameters with a fresh lifetime `'__s`, backend `__B` and encoding
///   `__E`, given underscore-prefixed names so that a user type like
///   `struct Foo<B>` does not clash with them -- the same hygiene trick serde's
///   derive uses.
///
/// Only type parameters are supported; lifetime and const parameters are
/// rejected in [`build`](Ctx::build).
struct Ctx {
    /// The `impl` generics of the `Persistable` impl: the type's own
    /// parameters, each bounded by `Persistable<Pointer>`.
    impl_generics: Tokens,
    /// The type's type generics, e.g. `<T>` (empty for a non-generic type).
    type_generics: Tokens,
    /// The type's own `where`-clause, verbatim (empty if none).
    where_clause: Tokens,
    /// Generic list for *declaring* the guard struct, whose encoding
    /// parameter defaults to `Slotted`.
    guard_decl_generics: Tokens,
    /// Generic list for every `impl` block on the guard.
    guard_impl_generics: Tokens,
    /// Generic list for *naming* the guard type (bare parameter names).
    guard_use_generics: Tokens,
    /// The type's type parameters rendered with their bounds, for building
    /// further generic lists (the `Parts` types) that need a different
    /// lifetime than the guard's.
    bounded_params: Vec<Tokens>,
    /// The type's type parameters as bare idents.
    param_idents: Vec<syn::Ident>,
    /// The user's own `where` predicates, each followed by a comma.
    user_where_preds: Option<Tokens>,
    /// The crate every generated path is rooted at -- `::kladde` unless
    /// `#[kladde(crate = "...")]` says otherwise.
    krate: syn::Path,
    /// `#[kladde(packed_only)]`: no `Slottable` impl.
    packed_only: bool,
}

impl Ctx {
    fn build(input: &DeriveInput, krate: syn::Path, packed_only: bool) -> syn::Result<Ctx> {
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

        let bounded_params: Vec<Tokens> = bounded.type_params().map(|tp| quote!(#tp)).collect();
        let param_idents: Vec<syn::Ident> = input
            .generics
            .type_params()
            .map(|tp| tp.ident.clone())
            .collect();
        let user_where_preds = input.generics.where_clause.as_ref().map(|w| {
            let preds = &w.predicates;
            quote!(#preds,)
        });

        Ok(Ctx {
            impl_generics: quote! { <#(#bounded_params,)*> },
            type_generics: quote!(#type_generics),
            where_clause: quote!(#where_clause),
            guard_decl_generics: quote! {
                <
                    '__s,
                    #(#bounded_params,)*
                    __B: #krate::WriteBackend<Pointer = #krate::Pointer>,
                    __E: #krate::Encoding = #krate::Slotted
                >
            },
            guard_impl_generics: quote! {
                <
                    '__s,
                    #(#bounded_params,)*
                    __B: #krate::WriteBackend<Pointer = #krate::Pointer>,
                    __E: #krate::Encoding
                >
            },
            guard_use_generics: quote! { <'__s, #(#param_idents,)* __B, __E> },
            bounded_params,
            param_idents,
            user_where_preds,
            krate,
            packed_only,
        })
    }

    /// The `where` clause of the `Persistable` impl: the type's own
    /// predicates, and a `Slottable` bound on the type of every field
    /// declared `#[kladde(slotted)]`, so that a field without a fixed
    /// encoding there fails to compile at the type that declares it.
    fn persistable_where(&self, fields: &[&FieldInfo]) -> Tokens {
        let krate = &self.krate;
        let user = &self.user_where_preds;
        let slotted = fields.iter().filter(|f| f.slotted).map(|f| &f.ty);
        quote! {
            where #user #(#slotted: #krate::Slottable<#krate::Pointer>,)*
        }
    }

    /// The type's `Slottable` impl, or nothing for a type marked
    /// `#[kladde(packed_only)]`.
    ///
    /// It requires `Slottable` of every type parameter, as `#[derive(Debug)]`
    /// requires `Debug`, rather than of every field's type: a type that holds
    /// itself through a container, `struct Node { children:
    /// PersistableVec<Node> }`, would make that requirement circular. Fields
    /// whose types name no parameter are checked by an assertion instead,
    /// which names the attribute when it fails.
    fn slottable_impl(&self, ident: &syn::Ident, fields: &[&FieldInfo]) -> Tokens {
        if self.packed_only {
            return Tokens::new();
        }
        let krate = &self.krate;
        let (impl_generics, type_generics) = (&self.impl_generics, &self.type_generics);
        let user = &self.user_where_preds;
        let params = &self.param_idents;
        let checks = fields
            .iter()
            .filter(|f| !mentions_any(&f.ty, params))
            .map(|f| {
                let ty = persistable(&f.ty, krate);
                let message = format!(
                    "`{ident}` has a field without a fixed encoding, `{}`: mark `{ident}` \
                     #[kladde(packed_only)], so that it stands only in packed places",
                    f.schema_name
                );
                quote! {
                    const _: () = if #ty::SLOTTED_SIZE.is_none() {
                        ::std::panic!(#message)
                    };
                }
            });
        quote! {
            impl #impl_generics #krate::Slottable<#krate::Pointer> for #ident #type_generics
            where #user #(#params: #krate::Slottable<#krate::Pointer>,)*
            {}

            #(#checks)*
        }
    }

    /// The type's `RootEncoding`: `Slotted` joined with every field's.
    fn root_encoding(&self, fields: &[&FieldInfo]) -> Tokens {
        let krate = &self.krate;
        fields.iter().fold(quote!(#krate::Slotted), |acc, f| {
            let ty = persistable(&f.ty, krate);
            quote!(<#acc as #krate::Encoding>::Join<#ty::RootEncoding>)
        })
    }

    /// The generics of a generated `Parts` type, which holds field guards for
    /// a lifetime `'__f`, and the arguments that name it from a guard method.
    /// The encoding parameter is left out when no field inherits it, since
    /// an unused parameter would not compile.
    fn parts_generics(&self, uses_encoding: bool) -> (Tokens, Tokens, Tokens) {
        let krate = &self.krate;
        let bounded_params = &self.bounded_params;
        let param_idents = &self.param_idents;
        let user_where_preds = &self.user_where_preds;
        let (decl_e, use_e) = if uses_encoding {
            (
                quote!(, __E: #krate::Encoding = #krate::Slotted),
                quote!(, __E),
            )
        } else {
            (Tokens::new(), Tokens::new())
        };
        let decl = quote! {
            <'__f, #(#bounded_params,)* __B: #krate::WriteBackend<Pointer = #krate::Pointer> #decl_e>
        };
        let ret = quote! { <'_, #(#param_idents,)* __B #use_e> };
        // The `Parts` types hold `Guard<'__f, __B, _>` associated types, whose
        // GAT bounds need `Param: '__f` and `__B: '__f`.
        let where_ = quote! {
            where #user_where_preds #(#param_idents: '__f,)* __B: '__f
        };
        (decl, ret, where_)
    }
}

/// Parses the type's `#[kladde(...)]` attributes: whether it is
/// `transparent`, and the `crate` generated paths are rooted at. Errors on
/// anything else, so that a typo is a compile error rather than a no-op.
fn kladde_attrs(attrs: &[syn::Attribute]) -> syn::Result<TypeAttrs> {
    let mut parsed = TypeAttrs {
        transparent: false,
        packed_only: false,
        krate: syn::parse_quote!(::kladde),
    };
    for attr in attrs {
        if !attr.path().is_ident("kladde") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("transparent") {
                parsed.transparent = true;
                Ok(())
            } else if meta.path.is_ident("packed_only") {
                parsed.packed_only = true;
                Ok(())
            } else if meta.path.is_ident("crate") {
                let lit: syn::LitStr = meta.value()?.parse()?;
                parsed.krate = lit.parse()?;
                Ok(())
            } else {
                Err(meta.error(
                    "unknown `#[kladde(...)]` option; supported are `transparent`, \
                     `packed_only` and `crate`",
                ))
            }
        })?;
    }
    Ok(parsed)
}

/// The type's own `#[kladde(...)]` options.
struct TypeAttrs {
    transparent: bool,
    /// The type holds a field without a fixed encoding, so it gets no
    /// `Slottable` impl.
    packed_only: bool,
    krate: syn::Path,
}

/// Whether a field is marked `#[kladde(slotted)]`, the one option a field
/// takes.
fn field_slotted(attrs: &[syn::Attribute]) -> syn::Result<bool> {
    let mut slotted = false;
    for attr in attrs {
        if !attr.path().is_ident("kladde") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("slotted") {
                slotted = true;
                Ok(())
            } else {
                Err(meta
                    .error("unknown `#[kladde(...)]` option on a field; supported is `slotted`"))
            }
        })?;
    }
    Ok(slotted)
}

/// One field of a struct or of an enum variant, as the generated code needs
/// it.
struct FieldInfo {
    ty: syn::Type,
    /// Declared `#[kladde(slotted)]`: its place holds the fixed encoding,
    /// whatever the value around it holds.
    slotted: bool,
    /// The encoding the field's place holds: `Slotted` if declared so, and
    /// otherwise `__E`, inherited from the value around it.
    encoding: Tokens,
    /// The field's name in the schema: its identifier, or its position.
    schema_name: String,
}

impl FieldInfo {
    fn of(fields: &Fields, krate: &syn::Path) -> syn::Result<Vec<FieldInfo>> {
        let fields: Vec<&syn::Field> = match fields {
            Fields::Named(f) => f.named.iter().collect(),
            Fields::Unnamed(f) => f.unnamed.iter().collect(),
            Fields::Unit => Vec::new(),
        };
        fields
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let slotted = field_slotted(&f.attrs)?;
                Ok(FieldInfo {
                    ty: f.ty.clone(),
                    slotted,
                    encoding: if slotted {
                        quote!(#krate::Slotted)
                    } else {
                        quote!(__E)
                    },
                    schema_name: match &f.ident {
                        Some(name) => name.to_string(),
                        None => i.to_string(),
                    },
                })
            })
            .collect()
    }

    /// The reference to the field's descriptor: `Slotted(T)` for a field
    /// declared slotted, `T`'s own otherwise.
    fn describe(&self, krate: &syn::Path) -> Tokens {
        let ty = &self.ty;
        if self.slotted {
            quote!(__builder.slotted::<#krate::Pointer, #ty>())
        } else {
            let ty = persistable(ty, krate);
            quote!(#ty::describe(__builder))
        }
    }
}

/// Whether `ty` names any of `params`.
fn mentions_any(ty: &syn::Type, params: &[syn::Ident]) -> bool {
    fn walk(tokens: proc_macro2::TokenStream, params: &[syn::Ident]) -> bool {
        tokens.into_iter().any(|tree| match tree {
            proc_macro2::TokenTree::Ident(ident) => params.contains(&ident),
            proc_macro2::TokenTree::Group(group) => walk(group.stream(), params),
            _ => false,
        })
    }
    !params.is_empty() && walk(quote!(#ty), params)
}

/// `<ty as Persistable<Pointer>>`, the path every generated call goes
/// through.
fn persistable(ty: &syn::Type, krate: &syn::Path) -> Tokens {
    quote!(<#ty as #krate::Persistable<#krate::Pointer>>)
}

/// For fields laid out back to back starting at `base` bytes into the value's
/// fixed encoding, each field's static offset there: the sum of every earlier
/// field's slot. `base` is a `usize` expression. Only a slotted value at a
/// fixed location uses these offsets, so a field without a slot counts as
/// zero rather than failing to compile.
fn fixed_offsets(fields: &[FieldInfo], base: &Tokens, krate: &syn::Path) -> Vec<Tokens> {
    (0..fields.len())
        .map(|i| {
            let earlier = fields[..i].iter().map(|f| persistable(&f.ty, krate));
            quote! {
                #base #( + match #earlier::SLOTTED_SIZE {
                    ::std::option::Option::Some(size) => size,
                    ::std::option::Option::None => 0,
                } )*
            }
        })
        .collect()
}

/// The `SLOTTED_SIZE` and `PACKED_SIZE` contributions of fields laid out
/// back to back: `Option<usize>` constant expressions. A field declared
/// slotted takes its slot in either.
fn field_sizes(fields: &[FieldInfo], krate: &syn::Path) -> (Tokens, Tokens) {
    let paths: Vec<Tokens> = fields.iter().map(|f| persistable(&f.ty, krate)).collect();
    let packed: Vec<Tokens> = fields
        .iter()
        .zip(&paths)
        .map(|(f, path)| {
            if f.slotted {
                quote!(#path::SLOTTED_SIZE)
            } else {
                quote!(#path::PACKED_SIZE)
            }
        })
        .collect();
    (
        quote!(#krate::sum_sizes(&[#(#paths::SLOTTED_SIZE),*])),
        quote!(#krate::sum_sizes(&[#(#packed),*])),
    )
}

/// The guard type every derive kind shares: a `{ inner, backend, place }`
/// struct, plus the offsets of its fields when it has any (`offsets` is the
/// `FieldOffsets` capacity), and its `Guard` and `Deref` impls. Each kind adds
/// `set` and accessors of its own.
fn guard_scaffold(
    ctx: &Ctx,
    ident: &syn::Ident,
    vis: &syn::Visibility,
    guard_ident: &syn::Ident,
    offsets: Option<usize>,
) -> Tokens {
    let krate = &ctx.krate;
    let Ctx {
        type_generics,
        where_clause,
        guard_decl_generics,
        guard_impl_generics,
        guard_use_generics,
        ..
    } = ctx;
    let doc = format!(
        "The guard of [`{ident}`]: records and applies mutations of a backed `{ident}`, \
         in a place of encoding `__E`."
    );
    let offsets_field = offsets.map(|n| {
        quote! {
            /// Where each field starts, for fields whose places link to it.
            fields: #krate::FieldOffsets<#n>,
        }
    });

    quote! {
        #[doc = #doc]
        #vis struct #guard_ident #guard_decl_generics #where_clause {
            inner: &'__s mut #ident #type_generics,
            backend: &'__s __B,
            place: #krate::Place<'__s, __B, __E>,
            #offsets_field
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
/// `Persistable` impl shares. A guard with field offsets fills them as it is
/// made.
fn guard_assoc(ctx: &Ctx, guard_ident: &syn::Ident, with_offsets: bool) -> Tokens {
    let krate = &ctx.krate;
    let guard_use_generics = &ctx.guard_use_generics;
    let construct = if with_offsets {
        quote! {
            let guard = #guard_ident {
                inner: self,
                backend,
                place,
                fields: #krate::FieldOffsets::new(),
            };
            guard.__kladde_refresh();
            guard
        }
    } else {
        quote! {
            #guard_ident {
                inner: self,
                backend,
                place,
            }
        }
    };
    quote! {
        type Guard<'__s, __B: #krate::WriteBackend<Pointer = #krate::Pointer>, __E: #krate::Encoding>
            = #guard_ident #guard_use_generics
        where
            Self: '__s,
            __B: '__s;

        #[inline]
        fn guard<'__s, __B: #krate::WriteBackend<Pointer = #krate::Pointer>, __E: #krate::Encoding>(
            &'__s mut self,
            backend: &'__s __B,
            place: #krate::Place<'__s, __B, __E>,
        ) -> Self::Guard<'__s, __B, __E> {
            #construct
        }
    }
}

/// The signatures every derive kind spells the same way.
fn encoded_size_sig(krate: &syn::Path) -> Tokens {
    quote! {
        fn encoded_size<__E: #krate::Encoding>(&self) -> usize
    }
}

fn encode_sig(krate: &syn::Path) -> Tokens {
    quote! {
        fn encode<__E: #krate::Encoding>(&self, __out: &mut ::std::vec::Vec<u8>)
    }
}

// The three below allow unused variables: an enum whose variants have no
// fields never reads its backend.
fn decode_sig(krate: &syn::Path) -> Tokens {
    quote! {
        #[allow(unused_variables)]
        fn decode<__B: #krate::ReadBackend<Pointer = #krate::Pointer>, __E: #krate::Encoding>(
            __backend: &mut __B,
            __input: &mut #krate::Input<'_>,
        ) -> ::std::result::Result<Self, #krate::Error>
    }
}

fn prepare_sig(krate: &syn::Path) -> Tokens {
    quote! {
        #[allow(unused_variables)]
        fn prepare<__B: #krate::WriteBackend<Pointer = #krate::Pointer>>(
            &mut self,
            __backend: &__B,
        ) -> ::std::result::Result<(), #krate::Error>
    }
}

fn free_sig(krate: &syn::Path) -> Tokens {
    quote! {
        #[allow(unused_variables)]
        fn free<__B: #krate::WriteBackend<Pointer = #krate::Pointer>>(
            &mut self,
            __backend: &__B,
        ) -> ::std::result::Result<(), #krate::Error>
    }
}

/// The whole-value `set` of a guard: `replace`, then refilling the field
/// offsets for the new value.
fn set_method(ctx: &Ctx, ident: &syn::Ident, vis: &syn::Visibility, with_offsets: bool) -> Tokens {
    let krate = &ctx.krate;
    let type_generics = &ctx.type_generics;
    let refresh = with_offsets.then(|| quote!(self.__kladde_refresh();));
    quote! {
        /// Replaces the whole value: stores `value`, which publishes it,
        /// then frees what the old value owned, in one transaction. If
        /// anything fails, the value is left as it was.
        #vis fn set(
            &mut self,
            value: #ident #type_generics,
        ) -> ::std::result::Result<(), #krate::Error> {
            #krate::replace(&mut *self.inner, value, self.backend, &self.place)?;
            #refresh
            ::std::result::Result::Ok(())
        }
    }
}

/// `#[kladde(transparent)]`, analogous to `#[serde(transparent)]`: a
/// single-field newtype persisted *exactly* as its one field. The impl
/// delegates everything to the field, and is **schema-transparent**: it
/// overrides `describe` to reuse the field's descriptor rather than
/// registering a node of its own, so the two share one fingerprint.
fn derive_transparent(input: &DeriveInput, ctx: &Ctx) -> Tokens {
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

    match field_slotted(&field.attrs) {
        Ok(false) => {}
        Ok(true) => {
            return syn::Error::new_spanned(
                field,
                "#[kladde(transparent)] persists the field exactly as it is; it cannot be \
                 declared slotted",
            )
            .to_compile_error();
        }
        Err(err) => return err.to_compile_error(),
    }
    let info = FieldInfo {
        ty: field.ty.clone(),
        slotted: false,
        encoding: quote!(__E),
        schema_name: String::new(),
    };
    let slottable_impl = ctx.slottable_impl(ident, &[&info]);
    let root_encoding = ctx.root_encoding(&[&info]);

    let field_ty = persistable(&field.ty, krate);
    let (member, construct): (syn::Member, Tokens) = match &field.ident {
        Some(name) => (
            syn::Member::Named(name.clone()),
            quote! { #ident { #name: __value } },
        ),
        None => (
            syn::Member::Unnamed(syn::Index::from(0)),
            quote! { #ident(__value) },
        ),
    };

    let scaffold = guard_scaffold(ctx, ident, vis, &guard_ident, None);
    let guard_assoc = guard_assoc(ctx, &guard_ident, false);
    let set = set_method(ctx, ident, vis, false);
    let (encoded_size_sig, encode_sig, decode_sig) = (
        encoded_size_sig(krate),
        encode_sig(krate),
        decode_sig(krate),
    );
    let (prepare_sig, free_sig) = (prepare_sig(krate), free_sig(krate));

    quote! {
        #scaffold

        impl #guard_impl_generics #guard_ident #guard_use_generics #where_clause {
            #set

            /// The guard of the wrapped value, which lives at the wrapper's
            /// own place: its whole mutation API, passed through.
            #[inline]
            #vis fn get_mut(&mut self) -> #field_ty::Guard<'_, __B, __E> {
                #field_ty::guard(&mut self.inner.#member, self.backend, self.place)
            }
        }

        #slottable_impl

        impl #impl_generics #krate::Persistable<#krate::Pointer> for #ident #type_generics #where_clause {
            const SLOTTED_SIZE: ::std::option::Option<usize> = #field_ty::SLOTTED_SIZE;
            const PACKED_SIZE: ::std::option::Option<usize> = #field_ty::PACKED_SIZE;

            type RootEncoding = #root_encoding;

            #guard_assoc

            #[inline]
            #encoded_size_sig {
                #field_ty::encoded_size::<__E>(&self.#member)
            }

            #encode_sig {
                #field_ty::encode::<__E>(&self.#member, __out)
            }

            #decode_sig {
                let __value = #field_ty::decode::<__B, __E>(__backend, __input)?;
                ::std::result::Result::Ok(#construct)
            }

            #prepare_sig {
                #field_ty::prepare(&mut self.#member, __backend)
            }

            #free_sig {
                #field_ty::free(&mut self.#member, __backend)
            }

            fn describe(builder: &mut #krate::SchemaBuilder) -> #krate::TypeRef
            where
                Self: 'static,
            {
                #field_ty::describe(builder)
            }
        }
    }
}

fn derive_struct(input: &DeriveInput, data: &syn::DataStruct, ctx: &Ctx) -> Tokens {
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

    if matches!(data.fields, Fields::Unit) {
        return derive_unit_like_struct(ctx, ident, vis, &guard_ident);
    }
    // Named and tuple structs are laid out identically; only how each field
    // is *named* differs.
    let raw_fields: Vec<&syn::Field> = match &data.fields {
        Fields::Named(fields) => fields.named.iter().collect(),
        Fields::Unnamed(fields) => fields.unnamed.iter().collect(),
        Fields::Unit => unreachable!(),
    };
    let fields = match FieldInfo::of(&data.fields, krate) {
        Ok(fields) => fields,
        Err(err) => return err.to_compile_error(),
    };
    let field_refs: Vec<&FieldInfo> = fields.iter().collect();
    let persistable_where = ctx.persistable_where(&field_refs);
    let slottable_impl = ctx.slottable_impl(ident, &field_refs);
    let root_encoding = ctx.root_encoding(&field_refs);
    let field_describe: Vec<Tokens> = fields.iter().map(|f| f.describe(krate)).collect();
    let field_count = fields.len();
    let capacity = field_count + 1;

    let ty: Vec<Tokens> = fields.iter().map(|f| persistable(&f.ty, krate)).collect();
    let enc: Vec<&Tokens> = fields.iter().map(|f| &f.encoding).collect();
    let member: Vec<syn::Member> = raw_fields
        .iter()
        .enumerate()
        .map(|(i, f)| match &f.ident {
            Some(name) => syn::Member::Named(name.clone()),
            None => syn::Member::Unnamed(syn::Index::from(i)),
        })
        .collect();
    let accessor_ident: Vec<syn::Ident> = raw_fields
        .iter()
        .enumerate()
        .map(|(i, f)| match &f.ident {
            Some(name) => format_ident!("{}_mut", name),
            None => format_ident!("field_{}_mut", i),
        })
        .collect();
    let accessor_doc: Vec<String> = fields
        .iter()
        .map(|f| format!("The guard of field `{}`.", f.schema_name))
        .collect();
    let schema_name: Vec<&String> = fields.iter().map(|f| &f.schema_name).collect();

    let fixed_offset = fixed_offsets(&fields, &quote!(0usize), krate);
    let (slotted_size, packed_size) = field_sizes(&fields, krate);

    let is_tuple = matches!(&data.fields, Fields::Unnamed(_));
    let field_ident: Vec<&syn::Ident> =
        raw_fields.iter().filter_map(|f| f.ident.as_ref()).collect();
    let decode_body = if is_tuple {
        quote! {
            #ident(
                #( #ty::decode::<__B, #enc>(__backend, __input)?, )*
            )
        }
    } else {
        quote! {
            #ident {
                #( #field_ident: #ty::decode::<__B, #enc>(__backend, __input)?, )*
            }
        }
    };

    // The guard of a field: at its fixed offset in a slotted value at a fixed
    // location, and linked to the guard's field offsets everywhere else.
    let field_guard: Vec<Tokens> = (0..field_count)
        .map(|i| {
            let (ty, enc, member, offset) = (&ty[i], enc[i], &member[i], &fixed_offset[i]);
            quote! {
                #ty::guard(
                    &mut self.inner.#member,
                    self.backend,
                    self.place.field::<#enc, #capacity>(&self.fields, #i, #offset),
                )
            }
        })
        .collect();

    // A `{Ident}Parts` struct plus a `parts()` method handing out a guard for
    // *every* field at once, each borrowing a disjoint part of `self.inner`,
    // so that all fields can be mutated simultaneously.
    let parts_ident = format_ident!("{}Parts", ident);
    let uses_encoding = fields.iter().any(|f| !f.slotted);
    let (parts_decl_generics, parts_ret_generics, parts_where) = ctx.parts_generics(uses_encoding);
    let parts_doc = format!("The guards of every field of [`{ident}`] at once.");
    let (parts_struct, parts_ctor) = if is_tuple {
        let struct_def = quote! {
            #[doc = #parts_doc]
            #vis struct #parts_ident #parts_decl_generics (
                #( #vis #ty::Guard<'__f, __B, #enc>, )*
            ) #parts_where;
        };
        let ctor = quote! { #parts_ident( #( #field_guard, )* ) };
        (struct_def, ctor)
    } else {
        let struct_def = quote! {
            #[doc = #parts_doc]
            #vis struct #parts_ident #parts_decl_generics #parts_where {
                #(
                    #[doc = #accessor_doc]
                    #vis #field_ident: #ty::Guard<'__f, __B, #enc>,
                )*
            }
        };
        let ctor = quote! { #parts_ident { #( #field_ident: #field_guard, )* } };
        (struct_def, ctor)
    };

    let scaffold = guard_scaffold(ctx, ident, vis, &guard_ident, Some(capacity));
    let guard_assoc = guard_assoc(ctx, &guard_ident, true);
    let set = set_method(ctx, ident, vis, true);
    let (encoded_size_sig, encode_sig, decode_sig) = (
        encoded_size_sig(krate),
        encode_sig(krate),
        decode_sig(krate),
    );
    let (prepare_sig, free_sig) = (prepare_sig(krate), free_sig(krate));

    quote! {
        #scaffold

        #parts_struct

        impl #guard_impl_generics #guard_ident #guard_use_generics #where_clause {
            #set

            #(
                #[doc = #accessor_doc]
                #[inline]
                #vis fn #accessor_ident(&mut self) -> #ty::Guard<'_, __B, #enc> {
                    #field_guard
                }
            )*

            /// A guard for every field at once, so that all fields can be
            /// mutated simultaneously.
            #[inline]
            #vis fn parts(&mut self) -> #parts_ident #parts_ret_generics {
                #parts_ctor
            }

            /// Records where each field starts, if the fields' places link
            /// to the record.
            fn __kladde_refresh(&self) {
                if self.place.links_fields() {
                    self.fields.fill(0, &[
                        #( #ty::encoded_size::<#enc>(&self.inner.#member), )*
                    ]);
                }
            }
        }

        #slottable_impl

        impl #impl_generics #krate::Persistable<#krate::Pointer> for #ident #type_generics #persistable_where {
            const SLOTTED_SIZE: ::std::option::Option<usize> = #slotted_size;
            const PACKED_SIZE: ::std::option::Option<usize> = #packed_size;

            type RootEncoding = #root_encoding;

            #guard_assoc

            #[inline]
            #encoded_size_sig {
                if !<__E as #krate::Encoding>::PACKED {
                    return #krate::slot_size::<Self, #krate::Pointer>();
                }
                0usize #( + #ty::encoded_size::<#enc>(&self.#member) )*
            }

            #encode_sig {
                #( #ty::encode::<#enc>(&self.#member, __out); )*
            }

            #decode_sig {
                ::std::result::Result::Ok(#decode_body)
            }

            #prepare_sig {
                #( #ty::prepare(&mut self.#member, __backend)?; )*
                ::std::result::Result::Ok(())
            }

            #free_sig {
                #( #ty::free(&mut self.#member, __backend)?; )*
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
                                ty: #field_describe,
                            },
                        )*
                    ],
                }
            }
        }
    }
}

/// A unit struct (`struct Foo;`): no fields, and no bytes in either encoding.
fn derive_unit_like_struct(
    ctx: &Ctx,
    ident: &syn::Ident,
    vis: &syn::Visibility,
    guard_ident: &syn::Ident,
) -> Tokens {
    let krate = &ctx.krate;
    let Ctx {
        impl_generics,
        type_generics,
        where_clause,
        guard_impl_generics,
        guard_use_generics,
        ..
    } = ctx;
    let scaffold = guard_scaffold(ctx, ident, vis, guard_ident, None);
    let guard_assoc = guard_assoc(ctx, guard_ident, false);
    let set = set_method(ctx, ident, vis, false);
    let (encoded_size_sig, encode_sig, decode_sig) = (
        encoded_size_sig(krate),
        encode_sig(krate),
        decode_sig(krate),
    );
    let slottable_impl = ctx.slottable_impl(ident, &[]);

    quote! {
        #scaffold

        impl #guard_impl_generics #guard_ident #guard_use_generics #where_clause {
            #set
        }

        #slottable_impl

        impl #impl_generics #krate::Persistable<#krate::Pointer> for #ident #type_generics #where_clause {
            const SLOTTED_SIZE: ::std::option::Option<usize> = ::std::option::Option::Some(0);
            const PACKED_SIZE: ::std::option::Option<usize> = ::std::option::Option::Some(0);

            type RootEncoding = #krate::Slotted;

            #guard_assoc

            #[inline]
            #encoded_size_sig {
                0
            }

            #encode_sig {
                let _ = __out;
            }

            #decode_sig {
                let _ = (__backend, __input);
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

/// One variant of an enum, as the generated code needs it.
struct VariantInfo {
    ident: syn::Ident,
    fields: Vec<FieldInfo>,
    /// The variant's fields bound in a match pattern, prefixed so that a
    /// field named like a parameter of the generated code does not shadow it.
    bindings: Vec<syn::Ident>,
    /// A match pattern binding the fields to `bindings`.
    pattern: Tokens,
    /// An expression building the variant from expressions `values`, one per
    /// field.
    kind: VariantKind,
}

enum VariantKind {
    Named(Vec<syn::Ident>),
    Unnamed,
    Unit,
}

impl VariantInfo {
    fn of(
        enum_ident: &syn::Ident,
        variant: &syn::Variant,
        krate: &syn::Path,
    ) -> syn::Result<VariantInfo> {
        let v_ident = &variant.ident;
        let fields = FieldInfo::of(&variant.fields, krate)?;
        let (bindings, pattern, kind) = match &variant.fields {
            Fields::Named(f) => {
                let names: Vec<syn::Ident> =
                    f.named.iter().map(|f| f.ident.clone().unwrap()).collect();
                let bindings: Vec<syn::Ident> = names
                    .iter()
                    .map(|n| format_ident!("__field_{}", n))
                    .collect();
                let pattern = quote! { #enum_ident::#v_ident { #(#names: #bindings),* } };
                (bindings, pattern, VariantKind::Named(names))
            }
            Fields::Unnamed(f) => {
                let bindings: Vec<syn::Ident> = (0..f.unnamed.len())
                    .map(|i| format_ident!("__field_{}", i))
                    .collect();
                let pattern = quote! { #enum_ident::#v_ident(#(#bindings),*) };
                (bindings, pattern, VariantKind::Unnamed)
            }
            Fields::Unit => (
                Vec::new(),
                quote! { #enum_ident::#v_ident },
                VariantKind::Unit,
            ),
        };
        Ok(VariantInfo {
            ident: v_ident.clone(),
            fields,
            bindings,
            pattern,
            kind,
        })
    }

    /// `Enum::Variant` built from one expression per field.
    fn construct(&self, enum_ident: &syn::Ident, values: &[Tokens]) -> Tokens {
        let v_ident = &self.ident;
        match &self.kind {
            VariantKind::Named(names) => quote! { #enum_ident::#v_ident { #(#names: #values),* } },
            VariantKind::Unnamed => quote! { #enum_ident::#v_ident(#(#values),*) },
            VariantKind::Unit => quote! { #enum_ident::#v_ident },
        }
    }
}

/// Fixed encoding: the discriminant at its width, the variant's fields laid
/// out like a struct's but based past the discriminant, and zeros up to the
/// size of the largest variant, so that every value takes the same slot.
/// Packed encoding: the discriminant as a varint, then the variant's fields.
///
/// The discriminant follows Rust's own rule: the explicit value where the
/// author wrote one (`A = 42`), otherwise `predecessor + 1`. So the value
/// stored on disk, and reported in the schema, equals the enum's real Rust
/// discriminant, pinned values stay stable, and uniqueness is inherited from
/// Rust's own check. Every generated item reads it from one generated
/// `const` chain.
fn derive_enum(input: &DeriveInput, data: &syn::DataEnum, ctx: &Ctx) -> Tokens {
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

    if data.variants.is_empty() {
        return syn::Error::new_spanned(
            &data.variants,
            "#[derive(Persistable)] does not support enums with no variants",
        )
        .to_compile_error();
    }

    let repr = match enum_repr(&input.attrs) {
        Ok(repr) => repr,
        Err(err) => return err.to_compile_error(),
    };

    let variants: Vec<VariantInfo> = match data
        .variants
        .iter()
        .map(|v| VariantInfo::of(ident, v, krate))
        .collect()
    {
        Ok(variants) => variants,
        Err(err) => return err.to_compile_error(),
    };
    let all_fields: Vec<&FieldInfo> = variants.iter().flat_map(|v| &v.fields).collect();
    let persistable_where = ctx.persistable_where(&all_fields);
    let slottable_impl = ctx.slottable_impl(ident, &all_fields);
    let root_encoding = ctx.root_encoding(&all_fields);
    let variant_count = variants.len();
    let capacity = variants.iter().map(|v| v.fields.len()).max().unwrap_or(0) + 1;
    let has_fields = capacity > 1;

    // The discriminant values, evaluated as Rust evaluates them: an explicit
    // expression at the `repr` type (`isize` without one), otherwise one more
    // than the previous variant's. Widened to `i128` so that a negative value
    // can be detected rather than wrapping.
    let disc_ty = match &repr {
        Some((ty, _)) => quote!(#ty),
        None => quote!(isize),
    };
    let disc_assign: Vec<Tokens> = data
        .variants
        .iter()
        .enumerate()
        .map(|(i, variant)| {
            let value = if let Some((_, expr)) = &variant.discriminant {
                quote! { { const __V: #disc_ty = #expr; __V as i128 } }
            } else if i == 0 {
                quote! { 0i128 }
            } else {
                let prev = i - 1;
                quote! { d[#prev] + 1 }
            };
            quote! { d[#i] = #value; }
        })
        .collect();
    let disc_values = quote! {
        {
            let mut d = [0i128; #variant_count];
            #(#disc_assign)*
            d
        }
    };
    let negative_msg = format!(
        "#[derive(Persistable)] does not support negative discriminants (in enum `{ident}`)"
    );
    let width_expr = match &repr {
        Some((_, width)) => quote!(#width),
        None => quote! {
            {
                let d = Self::__KLADDE_DISCRIMINANTS;
                let mut max = 0u64;
                let mut i = 0usize;
                while i < d.len() {
                    if d[i] > max {
                        max = d[i];
                    }
                    i += 1;
                }
                if max <= 0xFF {
                    1
                } else if max <= 0xFFFF {
                    2
                } else if max <= 0xFFFF_FFFF {
                    4
                } else {
                    8
                }
            }
        },
    };
    // Where every generated item names the discriminants and their width.
    let discriminants = quote!(<#ident #type_generics>::__KLADDE_DISCRIMINANTS);
    let disc_width = quote!(<#ident #type_generics>::__KLADDE_DISCRIMINANT_WIDTH);

    let mut slotted_sizes = Vec::new();
    let mut packed_sizes = Vec::new();
    let mut size_arms = Vec::new();
    let mut encode_arms = Vec::new();
    let mut decode_branches = Vec::new();
    let mut prepare_arms = Vec::new();
    let mut free_arms = Vec::new();
    let mut refresh_arms = Vec::new();
    let mut describe_variants = Vec::new();
    let mut parts_variants = Vec::new();
    let mut parts_arms = Vec::new();
    let parts_ident = format_ident!("{}Parts", ident);

    for (i, variant) in variants.iter().enumerate() {
        let VariantInfo {
            ident: v_ident,
            fields,
            bindings,
            pattern,
            ..
        } = variant;
        let ty: Vec<Tokens> = fields.iter().map(|f| persistable(&f.ty, krate)).collect();
        let enc: Vec<&Tokens> = fields.iter().map(|f| &f.encoding).collect();
        let (slotted, packed) = field_sizes(fields, krate);
        slotted_sizes.push(slotted);
        packed_sizes.push(packed);

        size_arms.push(quote! {
            #pattern => #krate::varint_len(#discriminants[#i])
                #( + #ty::encoded_size::<#enc>(#bindings) )*
        });
        encode_arms.push(quote! {
            #pattern => {
                if <__E as #krate::Encoding>::PACKED {
                    #krate::write_varint(#discriminants[#i], __out);
                } else {
                    __out.extend_from_slice(&#discriminants[#i].to_le_bytes()[..#disc_width]);
                }
                #( #ty::encode::<#enc>(#bindings, __out); )*
            }
        });
        let decoded: Vec<Tokens> = (0..fields.len())
            .map(|k| {
                let (ty, enc) = (&ty[k], enc[k]);
                quote!(#ty::decode::<__B, #enc>(__backend, __input)?)
            })
            .collect();
        let construct = variant.construct(ident, &decoded);
        decode_branches.push(quote! {
            if __discriminant == #discriminants[#i] {
                #construct
            }
        });
        prepare_arms.push(quote! {
            #pattern => { #( #ty::prepare(#bindings, __backend)?; )* }
        });
        free_arms.push(quote! {
            #pattern => { #( #ty::free(#bindings, __backend)?; )* }
        });
        refresh_arms.push(quote! {
            #pattern => {
                let __start = if <__E as #krate::Encoding>::PACKED {
                    #krate::varint_len(#discriminants[#i])
                } else {
                    #disc_width
                };
                self.fields.fill(__start, &[ #( #ty::encoded_size::<#enc>(#bindings), )* ]);
            }
        });
        let field_name: Vec<&String> = fields.iter().map(|f| &f.schema_name).collect();
        let field_describe: Vec<Tokens> = fields.iter().map(|f| f.describe(krate)).collect();
        describe_variants.push(quote! {
            #krate::Variant {
                discriminant: #discriminants[#i],
                name: ::std::string::ToString::to_string(::std::stringify!(#v_ident)),
                fields: ::std::vec![
                    #(
                        #krate::Field {
                            name: ::std::string::ToString::to_string(#field_name),
                            ty: #field_describe,
                        },
                    )*
                ],
            }
        });

        // `parts()`: each field guard at its fixed offset past the
        // discriminant in a slotted value at a fixed location, linked to the
        // guard's field offsets everywhere else.
        let fixed_offset = fixed_offsets(fields, &disc_width, krate);
        let guards: Vec<Tokens> = (0..fields.len())
            .map(|k| {
                let (ty, enc, binding, offset) = (&ty[k], enc[k], &bindings[k], &fixed_offset[k]);
                quote! {
                    #ty::guard(
                        #binding,
                        self.backend,
                        self.place.field::<#enc, #capacity>(&self.fields, #k, #offset),
                    )
                }
            })
            .collect();
        let doc = format!("The guards of the fields of [`{ident}::{v_ident}`].");
        match &variant.kind {
            VariantKind::Named(names) => {
                let field_docs = names
                    .iter()
                    .map(|name| format!("The guard of field `{name}`."));
                parts_variants.push(quote! {
                    #[doc = #doc]
                    #v_ident {
                        #(
                            #[doc = #field_docs]
                            #names: #ty::Guard<'__f, __B, #enc>,
                        )*
                    }
                });
                parts_arms.push(quote! {
                    #pattern => #parts_ident::#v_ident { #(#names: #guards,)* }
                });
            }
            VariantKind::Unnamed => {
                parts_variants.push(quote! {
                    #[doc = #doc]
                    #v_ident( #(#ty::Guard<'__f, __B, #enc>,)* )
                });
                parts_arms.push(quote! {
                    #pattern => #parts_ident::#v_ident(#(#guards,)*)
                });
            }
            VariantKind::Unit => {
                let doc = format!("[`{ident}::{v_ident}`], which has no fields.");
                parts_variants.push(quote! {
                    #[doc = #doc]
                    #v_ident
                });
                parts_arms.push(quote! { #pattern => #parts_ident::#v_ident });
            }
        }
    }

    // An enum guard's `parts()`: a generated `{Enum}Parts` enum with the same
    // variants, each carrying the guards of its fields, and the method that
    // matches on the current variant to hand them out. `parts()` borrows the
    // guard mutably, so `set` cannot switch the variant while a field guard
    // is alive. An enum without any fields gets neither: there is nothing to
    // mutate in place, and the `Parts` enum would not use its parameters.
    let parts = has_fields.then(|| {
        let uses_encoding = variants.iter().flat_map(|v| &v.fields).any(|f| !f.slotted);
        let (parts_decl_generics, parts_ret_generics, parts_where) =
            ctx.parts_generics(uses_encoding);
        let parts_doc = format!(
            "The guards of the fields of the current variant of a backed [`{ident}`], \
             handed out by its guard's `parts()`."
        );
        quote! {
            #[doc = #parts_doc]
            #vis enum #parts_ident #parts_decl_generics #parts_where {
                #(#parts_variants,)*
            }

            impl #guard_impl_generics #guard_ident #guard_use_generics #where_clause {
                /// The guards of the current variant's fields, for mutating them
                /// in place; match on the result to reach them. To switch to
                /// another variant, use `set`.
                #[inline]
                #vis fn parts(&mut self) -> #parts_ident #parts_ret_generics {
                    match &mut *self.inner {
                        #(#parts_arms,)*
                    }
                }
            }
        }
    });

    let refresh_body = if has_fields {
        quote! {
            if self.place.links_fields() {
                match &*self.inner {
                    #(#refresh_arms)*
                }
            }
        }
    } else {
        Tokens::new()
    };

    let scaffold = guard_scaffold(ctx, ident, vis, &guard_ident, Some(capacity));
    let guard_assoc = guard_assoc(ctx, &guard_ident, true);
    let set = set_method(ctx, ident, vis, true);
    let (encoded_size_sig, encode_sig, decode_sig) = (
        encoded_size_sig(krate),
        encode_sig(krate),
        decode_sig(krate),
    );
    let (prepare_sig, free_sig) = (prepare_sig(krate), free_sig(krate));

    quote! {
        #scaffold

        #parts

        impl #guard_impl_generics #guard_ident #guard_use_generics #where_clause {
            #set

            /// Records where each field of the current variant starts, if the
            /// fields' places link to the record.
            fn __kladde_refresh(&self) {
                #refresh_body
            }
        }

        // Rejected here rather than wrapped: a descriptor stores discriminants
        // unsigned. A free constant is evaluated even where nothing uses the
        // type, so the error cannot hide behind a generic that is never
        // instantiated.
        const _: () = {
            let d: [i128; #variant_count] = #disc_values;
            let mut i = 0usize;
            while i < d.len() {
                if d[i] < 0 {
                    ::std::panic!(#negative_msg);
                }
                i += 1;
            }
        };

        impl #impl_generics #ident #type_generics #where_clause {
            /// Each variant's discriminant, in declaration order.
            const __KLADDE_DISCRIMINANTS: [u64; #variant_count] = {
                let d: [i128; #variant_count] = #disc_values;
                let mut out = [0u64; #variant_count];
                let mut i = 0usize;
                while i < d.len() {
                    out[i] = d[i] as u64;
                    i += 1;
                }
                out
            };
            /// The discriminant's width in bytes in the fixed encoding: the
            /// integer `repr`'s, or else the smallest of 1, 2, 4 and 8 that
            /// holds every value.
            const __KLADDE_DISCRIMINANT_WIDTH: usize = #width_expr;
        }

        #slottable_impl

        impl #impl_generics #krate::Persistable<#krate::Pointer> for #ident #type_generics #persistable_where {
            const SLOTTED_SIZE: ::std::option::Option<usize> =
                #krate::enum_slotted_size(#disc_width, &[#(#slotted_sizes),*]);
            const PACKED_SIZE: ::std::option::Option<usize> =
                #krate::enum_packed_size(&#discriminants, &[#(#packed_sizes),*]);

            type RootEncoding = #root_encoding;

            #guard_assoc

            #encoded_size_sig {
                if !<__E as #krate::Encoding>::PACKED {
                    return #krate::slot_size::<Self, #krate::Pointer>();
                }
                match self {
                    #(#size_arms,)*
                }
            }

            #encode_sig {
                let __start = __out.len();
                match self {
                    #(#encode_arms)*
                }
                if !<__E as #krate::Encoding>::PACKED {
                    __out.resize(__start + #krate::slot_size::<Self, #krate::Pointer>(), 0);
                }
            }

            #decode_sig {
                let __start = __input.position();
                let __discriminant = if <__E as #krate::Encoding>::PACKED {
                    __input.varint()?
                } else {
                    let mut __buf = [0u8; 8];
                    __buf[..#disc_width].copy_from_slice(__input.take(#disc_width)?);
                    u64::from_le_bytes(__buf)
                };
                let __value = #(#decode_branches else)* {
                    return ::std::result::Result::Err(#krate::Error::Corrupt(::std::format!(
                        "unknown discriminant {} of {}",
                        __discriminant,
                        ::std::stringify!(#ident),
                    )));
                };
                if !<__E as #krate::Encoding>::PACKED {
                    __input.skip_to(__start + #krate::slot_size::<Self, #krate::Pointer>())?;
                }
                ::std::result::Result::Ok(__value)
            }

            #prepare_sig {
                match self {
                    #(#prepare_arms)*
                }
                ::std::result::Result::Ok(())
            }

            #free_sig {
                match self {
                    #(#free_arms)*
                }
                ::std::result::Result::Ok(())
            }

            fn describe_local(
                __builder: &mut #krate::SchemaBuilder,
            ) -> #krate::TypeDescriptor
            where
                Self: 'static,
            {
                #krate::TypeDescriptor::Enum {
                    name: ::std::string::ToString::to_string(::std::stringify!(#ident)),
                    discriminant_width: #disc_width as u8,
                    variants: ::std::vec![
                        #(#describe_variants),*
                    ],
                }
            }
        }
    }
}

/// An enum's integer `#[repr]`, if it has one: the discriminant's type and
/// its width in bytes. Other `repr` options (`C`, `align(..)`, ...) are
/// ignored; a signed, pointer-sized or 128-bit integer `repr` is an error,
/// because descriptors store discriminants unsigned in 1, 2, 4 or 8 bytes.
fn enum_repr(attrs: &[syn::Attribute]) -> syn::Result<Option<(syn::Ident, usize)>> {
    let mut repr = None;
    for attr in attrs {
        if !attr.path().is_ident("repr") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if let Some(ty) = meta.path.get_ident() {
                let width = match ty.to_string().as_str() {
                    "u8" => Some(1usize),
                    "u16" => Some(2),
                    "u32" => Some(4),
                    "u64" => Some(8),
                    "i8" | "i16" | "i32" | "i64" | "i128" | "u128" | "isize" | "usize" => {
                        return Err(meta.error(
                            "#[derive(Persistable)] supports only the integer reprs `u8`, `u16`, \
                             `u32` and `u64` on enums, since discriminants are stored unsigned \
                             and at a fixed width",
                        ));
                    }
                    _ => None,
                };
                if let Some(width) = width {
                    repr = Some((ty.clone(), width));
                }
            }
            // Skip the argument of options such as `align(8)`.
            if meta.input.peek(syn::token::Paren) {
                let content;
                syn::parenthesized!(content in meta.input);
                content.parse::<Tokens>()?;
            }
            Ok(())
        })?;
    }
    Ok(repr)
}
