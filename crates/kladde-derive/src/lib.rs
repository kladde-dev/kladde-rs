//! `#[derive(Durable)]`: turns a struct or enum composed of
//! `Durable` fields into a durable type. Applications reach it
//! through the `kladde` crate, which re-exports it.
//!
//! For a type `T`, it generates
//!
//! - a `Durable` implementation: the encoding (`encoded_len`, `encode`,
//!   `decode`), the size `FIXED_SIZE`, and the schema descriptor;
//! - for a type marked `#[kladde(bounded)]`, a `Bounded` implementation:
//!   the fixed form (`encode_fixed`, `decode_fixed`) and its size
//!   `FIXED_FORM_SIZE`;
//! - a guard type `TGuard`, with a `{field}_mut()` accessor per field
//!   (`field_{i}_mut()` for a tuple struct's positional fields), a `parts()`
//!   method handing out a guard for every field at once, and a whole-value
//!   `set`;
//! - `Deref` on the guard, so read-only methods stay available.
//!
//! Encoding:
//!
//! - A struct is its fields' encodings back to back. It is fixed-size if
//!   every field is.
//! - An enum is its discriminant as a varint, then its variant's fields. It
//!   is fixed-size if every discriminant is below 128 and every variant
//!   takes the same fixed size. Its fixed form, if it is bounded, puts the
//!   discriminant at the width an integer `#[repr(u8 | u16 | u32 | u64)]`
//!   says, or else at the smallest of 1, 2, 4 and 8 bytes that holds the
//!   largest discriminant, and pads every variant with zeros to the
//!   largest. Its guard's `parts()` returns a generated `{Enum}Parts` enum
//!   holding the guards of the current variant's fields, for mutating them
//!   in place; `set` replaces the whole value, which is how the variant
//!   changes.
//! - A single-field struct marked `#[kladde(transparent)]` is persisted
//!   exactly as its field: same bytes and same fingerprint. Its guard has a
//!   `get_mut()` returning the field's guard.
//! - Type parameters get a `Durable` bound, as `#[derive(Debug)]` does,
//!   and a `Bounded` one in the `Bounded` implementation; lifetime and const
//!   parameters are rejected.
//!
//! A field guard made by its accessor holds where its field starts: at a
//! constant offset up to the first field whose size depends on its value,
//! and after that at the sum of the sizes of the fields before it. Guards
//! made together by `parts()` find where they start whenever they write, so
//! that a field before them may change size meanwhile:
//!
//! ```
//! use kladde::{Kladde, Durable};
//!
//! #[derive(Durable)]
//! struct Counts {
//!     small: u32, // a varint
//!     flag: bool,
//! }
//!
//! let mut counts = Kladde::new(Counts { small: 5, flag: false });
//! let mut guard = counts.guard();
//! let mut parts = guard.parts();
//! parts.small.set(1 << 30)?; // five bytes now, so `flag` moves
//! parts.flag.set(true)?;
//! assert_eq!(counts.get().small, 1 << 30);
//! # Ok::<(), kladde::Error>(())
//! ```
//!
//! **Every field must be `Durable`.** Plain `String` is not, and the
//! compile error is the point: a field that would silently persist nothing
//! does not compile.
//!
//! ```compile_fail
//! #[derive(kladde::Durable)]
//! struct Contact {
//!     name: String, // error[E0277]: the trait bound `String: Durable` is not satisfied
//! }
//! ```
//!
//! A type that does implement it, such as a scalar or a container of
//! `kladde-types`, compiles:
//!
//! ```
//! #[derive(kladde::Durable)]
//! struct Contact {
//!     age: u16,
//! }
//! ```
//!
//! `#[kladde(transparent)]` requires exactly one field:
//!
//! ```compile_fail
//! #[derive(kladde::Durable)]
//! #[kladde(transparent)]
//! struct TwoFields {
//!     a: i32,
//!     b: i32, // error: #[kladde(transparent)] requires exactly one field
//! }
//! ```
//!
//! ```
//! #[derive(kladde::Durable)]
//! #[kladde(transparent)]
//! struct Meters(i32);
//! ```
//!
//! **Fixed forms.** A type has a fixed form, and can be wrapped in
//! `Fixed<T>`, if it is marked `#[kladde(bounded)]`, which every one of its
//! fields' types must have too: a scalar, a `Fixed`, an `Option` or tuple of
//! such types, or another type marked so.
//!
//! ```
//! use kladde::{Bounded, Fixed, Durable};
//!
//! #[derive(Durable)]
//! #[kladde(bounded)]
//! struct Range {
//!     start: u32, // a varint, but four bytes in the fixed form
//!     len: u16,
//! }
//!
//! assert_eq!(<Range as Durable>::FIXED_SIZE, None);
//! assert_eq!(<Range as Bounded>::FIXED_FORM_SIZE, 6);
//! assert_eq!(<Fixed<Range> as Durable>::FIXED_SIZE, Some(6));
//! ```
//!
//! A field without a fixed form fails at the type's definition:
//!
//! ```compile_fail
//! #[derive(kladde::Durable)]
//! struct Inner {
//!     a: u32,
//! }
//!
//! #[derive(kladde::Durable)]
//! #[kladde(bounded)]
//! struct Outer {
//!     inner: Inner, // error[E0277]: `Inner` has no fixed form
//! }
//! ```
//!
//! and so does a `Fixed` of a type without one:
//!
//! ```compile_fail
//! #[derive(kladde::Durable)]
//! struct Inner {
//!     a: u32,
//! }
//!
//! #[derive(kladde::Durable)]
//! struct Holder {
//!     inner: kladde::Fixed<Inner>, // error[E0277]: `Inner` has no fixed form
//! }
//! ```
//!
//! **Enums.** `parts()` reaches into the current variant:
//!
//! ```
//! use kladde::{Bounded, Kladde, Durable};
//!
//! #[derive(Durable)]
//! #[kladde(bounded)]
//! enum Shape {
//!     Origin,
//!     Circle(f32),
//!     Rectangle { width: f32, height: f32 },
//! }
//!
//! let mut shape = Kladde::new(Shape::Rectangle { width: 1.0, height: 2.0 });
//! match shape.guard().parts() {
//!     ShapeParts::Rectangle { mut width, .. } => width.set(3.0)?,
//!     ShapeParts::Circle(mut radius) => radius.set(3.0)?,
//!     ShapeParts::Origin => {}
//! }
//! assert!(matches!(shape.get(), Shape::Rectangle { width: 3.0, height: 2.0 }));
//!
//! // The variants differ in size, so the enum is not fixed-size; its fixed
//! // form takes a one-byte discriminant and the largest variant.
//! assert_eq!(<Shape as Durable>::FIXED_SIZE, None);
//! assert_eq!(<Shape as Bounded>::FIXED_FORM_SIZE, 1 + 8);
//! # Ok::<(), kladde::Error>(())
//! ```
//!
//! The variant cannot be switched while a field's guard is alive:
//!
//! ```compile_fail
//! use kladde::{Kladde, Durable};
//!
//! #[derive(Durable)]
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
//! An integer `#[repr]` fixes the discriminant's width in the fixed form,
//! so that it stays when variants are added, which is the one way a
//! fixed-size type's fixed form can be larger than its encoding; only
//! unsigned ones of up to 64 bits are accepted, since discriminants are
//! stored unsigned:
//!
//! ```
//! #[derive(kladde::Durable)]
//! #[kladde(bounded)]
//! #[repr(u32)]
//! enum Pinned {
//!     A,
//!     B,
//! }
//! assert_eq!(<Pinned as kladde::Durable>::FIXED_SIZE, Some(1));
//! assert_eq!(<Pinned as kladde::Bounded>::FIXED_FORM_SIZE, 4);
//! ```
//!
//! ```compile_fail
//! #[derive(kladde::Durable)]
//! #[repr(i8)] // error: #[derive(Durable)] supports only the integer reprs `u8`, ...
//! enum Signed {
//!     A,
//!     B,
//! }
//! ```
//!
//! ```compile_fail
//! #[derive(kladde::Durable)]
//! enum Negative {
//!     A = -1, // error: #[derive(Durable)] does not support negative discriminants
//!     B,
//! }
//! ```
//!
//! **Trait objects, as an enum.** A file can hold only values whose types
//! it can name, so where an application would hold a `Box<dyn Shape>`, it
//! holds an enum of the shapes instead, which forwards the trait's methods
//! to its variants: mark the trait `#[kladde::dispatch]`, and the enum
//! `#[kladde(dispatch(Shape))]`. The enum is an ordinary durable enum, with
//! a variant per implementation, and converts `From` each variant's value.
//! A variant may hold its value in a `Box`, to keep a large variant from
//! making every value of the enum large: a box is persisted as what it
//! holds, so boxing a variant, or unboxing it, changes nothing on file.
//!
//! ```
//! use kladde::{Durable, Kladde};
//!
//! #[kladde::dispatch]
//! trait Shape {
//!     fn area(&self) -> f32;
//! }
//!
//! #[derive(Durable, Clone)]
//! struct Circle { r: f32 }
//! impl Shape for Circle {
//!     fn area(&self) -> f32 { 3.0 * self.r * self.r }
//! }
//!
//! #[derive(Durable, Clone)]
//! struct Rect { w: f32, h: f32 }
//! impl Shape for Rect {
//!     fn area(&self) -> f32 { self.w * self.h }
//! }
//!
//! #[derive(Durable, Clone)]
//! struct ComplicatedVariantWithManyFields {
//!     outer: Rect,
//!     hole: Rect,
//!     angle: f32,
//!     label: u32,
//! }
//! impl Shape for ComplicatedVariantWithManyFields {
//!     fn area(&self) -> f32 { self.outer.area() - self.hole.area() }
//! }
//!
//! #[derive(Durable)]
//! #[kladde(dispatch(Shape))]
//! #[repr(u32)]
//! enum AnyShape {
//!     Circle(Circle) = 1,
//!     Rect(Rect) = 2,
//!     ComplicatedVariantWithManyFields(Box<ComplicatedVariantWithManyFields>) = 3,
//! }
//!
//! let mut shape = Kladde::new(AnyShape::from(Rect { w: 2.0, h: 3.0 }));
//! assert_eq!(shape.get().area(), 6.0);
//!
//! // `From` the boxed variant's value, boxing it.
//! let frame = ComplicatedVariantWithManyFields {
//!     outer: Rect { w: 3.0, h: 3.0 },
//!     hole: Rect { w: 1.0, h: 1.0 },
//!     angle: 0.0,
//!     label: 7,
//! };
//! shape.guard().set(frame.into())?;
//! assert_eq!(shape.get().area(), 8.0);
//!
//! // The boxed variant's guard is its value's own.
//! if let AnyShapeParts::ComplicatedVariantWithManyFields(mut c) = shape.guard().parts() {
//!     c.hole_mut().w_mut().set(2.0)?;
//! }
//! assert_eq!(shape.get().area(), 7.0);
//! # Ok::<(), kladde::Error>(())
//! ```
//!
//! The discriminants are optional, as on every enum; explicit ones keep a
//! file readable when variants are added or reordered. Every variant holds
//! one value, whose type implements the trait, or fails to compile at the
//! enum:
//!
//! ```compile_fail
//! #[kladde::dispatch]
//! trait Shape { fn area(&self) -> f32; }
//!
//! #[derive(kladde::Durable)]
//! struct Square { side: f32 } // no `impl Shape for Square`
//!
//! #[derive(kladde::Durable)]
//! #[kladde(dispatch(Shape))]
//! enum AnyShape {
//!     Square(Square), // error[E0277]: the trait bound `Square: Shape` is not satisfied
//! }
//! ```
//!
//! The trait's methods take `self`, `&self` or `&mut self`, and mention
//! `Self` nowhere else in their signatures, since each variant would need a
//! value of its own type there:
//!
//! ```compile_fail
//! #[kladde::dispatch]
//! trait Merge {
//!     fn merge(&self, other: &Self) -> Self; // error: cannot forward a method that takes `Self` ...
//! }
//! ```
//!
//! The trait and the enum must be in one crate; the enum may be in any
//! module, before or after the trait.
//!
//! **Paths.** Generated code names everything through `::kladde`, which
//! re-exports what it needs. A library built on `kladde-persist` without the
//! facade redirects it with `#[kladde(crate = "kladde_persist")]`.

use proc_macro::TokenStream;
use quote::{format_ident, quote, quote_spanned};
use syn::spanned::Spanned;
use syn::{parse_macro_input, parse_quote, Data, DeriveInput, Fields, GenericParam};

type Tokens = proc_macro2::TokenStream;

mod dispatch;

#[proc_macro_derive(Durable, attributes(kladde))]
pub fn derive_durable(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    let TypeAttrs {
        transparent,
        bounded,
        krate,
        dispatch,
    } = match kladde_attrs(&input.attrs) {
        Ok(parsed) => parsed,
        Err(err) => return err.to_compile_error().into(),
    };

    let ctx = match Ctx::build(&input, krate, bounded) {
        Ok(ctx) => ctx,
        Err(err) => return err.to_compile_error().into(),
    };

    let mut expanded = if transparent {
        derive_transparent(&input, &ctx)
    } else {
        match &input.data {
            Data::Struct(data) => derive_struct(&input, data, &ctx),
            Data::Enum(data) => derive_enum(&input, data, &ctx),
            Data::Union(data) => syn::Error::new_spanned(
                data.union_token,
                "#[derive(Durable)] does not support unions",
            )
            .to_compile_error(),
        }
    };
    if !dispatch.is_empty() {
        expanded.extend(dispatch::dispatch_impls(&input, &dispatch));
    }

    expanded.into()
}

/// Makes a trait one that an enum can implement by forwarding to its
/// variants, with `#[derive(Durable)]` and `#[kladde(dispatch(Trait))]`.
///
/// The trait itself is unchanged. Every method must take `self`, `&self` or
/// `&mut self`, and mention `Self` nowhere else in its signature; the trait
/// may not be generic or have associated types or constants. The enum must
/// be in the trait's crate. See the [crate documentation](crate) for an
/// example.
#[proc_macro_attribute]
pub fn dispatch(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as syn::ItemTrait);
    dispatch::dispatch_trait(attr.into(), item).into()
}

/// The generics plumbing every derive path shares, precomputed once from the
/// input type's own generic parameters:
///
/// - the *type*'s own `impl`/type/where fragments, with a `Durable` bound
///   added to each type parameter (the `#[derive(Debug)]` heuristic); and
/// - the generated *guard*'s generic lists, which extend the type's own
///   parameters with a fresh lifetime `'__r` and backend `__B`, given
///   underscore-prefixed names so that a user type like `struct Foo<B>` does
///   not clash with them -- the same hygiene trick serde's derive uses.
///
/// Only type parameters are supported; lifetime and const parameters are
/// rejected in [`build`](Ctx::build).
struct Ctx {
    /// The `impl` generics of the `Durable` impl: the type's own
    /// parameters, each bounded by `Durable`.
    impl_generics: Tokens,
    /// The type's type generics, e.g. `<T>` (empty for a non-generic type).
    type_generics: Tokens,
    /// The type's own `where`-clause, verbatim (empty if none).
    where_clause: Tokens,
    /// Generic list for declaring the guard struct and its `impl` blocks.
    guard_impl_generics: Tokens,
    /// Generic list for *naming* the guard type (bare parameter names).
    guard_use_generics: Tokens,
    /// The type's type parameters rendered with their bounds.
    bounded_params: Vec<Tokens>,
    /// The type's type parameters as bare idents.
    param_idents: Vec<syn::Ident>,
    /// The user's own `where` predicates, each followed by a comma.
    user_where_preds: Option<Tokens>,
    /// The crate every generated path is rooted at -- `::kladde` unless
    /// `#[kladde(crate = "...")]` says otherwise.
    krate: syn::Path,
    /// Whether the type is marked `#[kladde(bounded)]`, so that it gets a
    /// `Bounded` impl.
    bounded: bool,
    /// The `where`-clause of the `Bounded` impl: the user's own predicates,
    /// and `Bounded` for every type parameter, as `#[derive(Debug)]` asks
    /// `Debug` of each. A field's type that is not `Bounded` then fails
    /// where the impl's items name it, at the type's definition.
    bounded_where: Tokens,
}

impl Ctx {
    fn build(input: &DeriveInput, krate: syn::Path, is_bounded: bool) -> syn::Result<Ctx> {
        for param in &input.generics.params {
            match param {
                GenericParam::Type(_) => {}
                GenericParam::Lifetime(lt) => {
                    return Err(syn::Error::new_spanned(
                        lt,
                        "#[derive(Durable)] does not support lifetime parameters",
                    ));
                }
                GenericParam::Const(c) => {
                    return Err(syn::Error::new_spanned(
                        c,
                        "#[derive(Durable)] does not support const generic parameters",
                    ));
                }
            }
        }

        let mut bounded = input.generics.clone();
        for tp in bounded.type_params_mut() {
            tp.bounds.push(parse_quote!(#krate::Durable));
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
        let bounded_where = quote! {
            where #user_where_preds #(#param_idents: #krate::Bounded,)*
        };

        Ok(Ctx {
            impl_generics: quote! { <#(#bounded_params,)*> },
            type_generics: quote!(#type_generics),
            where_clause: quote!(#where_clause),
            guard_impl_generics: quote! {
                <'__r, #(#bounded_params,)* __B: #krate::Backend>
            },
            guard_use_generics: quote! { <'__r, #(#param_idents,)* __B> },
            bounded_params,
            param_idents,
            user_where_preds,
            krate,
            bounded: is_bounded,
            bounded_where,
        })
    }

    /// The generics of a generated `Parts` type, which holds field guards for
    /// a lifetime `'__f`, the arguments that name it from a guard method,
    /// and its `where` clause.
    fn parts_generics(&self) -> (Tokens, Tokens, Tokens) {
        let krate = &self.krate;
        let bounded_params = &self.bounded_params;
        let param_idents = &self.param_idents;
        let user_where_preds = &self.user_where_preds;
        let decl = quote! { <'__f, #(#bounded_params,)* __B: #krate::Backend + '__f> };
        let ret = quote! { <'_, #(#param_idents,)* __B> };
        // The `Parts` types hold `Guard<'__f, __B>` associated types, whose
        // GAT bounds need `Param: '__f`.
        let where_ = quote! {
            where #user_where_preds #(#param_idents: '__f,)*
        };
        (decl, ret, where_)
    }
}

/// Parses the type's `#[kladde(...)]` attributes: whether it is
/// `transparent`, whether it is `bounded`, the `crate` generated paths are
/// rooted at, and the traits it `dispatch`es. Errors on anything else, so
/// that a typo is a compile error rather than a no-op.
fn kladde_attrs(attrs: &[syn::Attribute]) -> syn::Result<TypeAttrs> {
    let mut parsed = TypeAttrs {
        transparent: false,
        bounded: false,
        krate: syn::parse_quote!(::kladde),
        dispatch: Vec::new(),
    };
    for attr in attrs {
        if !attr.path().is_ident("kladde") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("transparent") {
                parsed.transparent = true;
                Ok(())
            } else if meta.path.is_ident("bounded") {
                parsed.bounded = true;
                Ok(())
            } else if meta.path.is_ident("crate") {
                let lit: syn::LitStr = meta.value()?.parse()?;
                parsed.krate = lit.parse()?;
                Ok(())
            } else if meta.path.is_ident("dispatch") {
                meta.parse_nested_meta(|tr| {
                    parsed.dispatch.push(tr.path.clone());
                    Ok(())
                })
            } else {
                Err(meta.error(
                    "unknown `#[kladde(...)]` option; supported are `transparent`, `bounded`, \
                     `dispatch(...)` and `crate`",
                ))
            }
        })?;
    }
    Ok(parsed)
}

/// The type's own `#[kladde(...)]` options.
struct TypeAttrs {
    transparent: bool,
    bounded: bool,
    krate: syn::Path,
    /// The traits the enum implements by forwarding to its variants.
    dispatch: Vec<syn::Path>,
}

/// Refuses `#[kladde(...)]` on a field: fields take no options.
fn no_field_attrs(fields: &Fields) -> syn::Result<()> {
    for f in fields.iter() {
        if let Some(attr) = f.attrs.iter().find(|a| a.path().is_ident("kladde")) {
            return Err(syn::Error::new_spanned(
                attr,
                "fields take no `#[kladde(...)]` options; for a fixed-size field, use `Fixed<T>`",
            ));
        }
    }
    Ok(())
}

/// One field of a struct or of an enum variant, as the generated code needs
/// it.
struct FieldInfo {
    ty: syn::Type,
    /// The field's name in the schema: its identifier, or its position.
    schema_name: String,
}

impl FieldInfo {
    fn of(fields: &Fields) -> syn::Result<Vec<FieldInfo>> {
        no_field_attrs(fields)?;
        Ok(fields
            .iter()
            .enumerate()
            .map(|(i, f)| FieldInfo {
                ty: f.ty.clone(),
                schema_name: match &f.ident {
                    Some(name) => name.to_string(),
                    None => i.to_string(),
                },
            })
            .collect())
    }
}

/// `<ty as Durable>`, the path every generated call goes through.
fn durable(ty: &syn::Type, krate: &syn::Path) -> Tokens {
    quote!(<#ty as #krate::Durable>)
}

/// `<ty as Bounded>`, spanned at the field's type, so that a field whose
/// type has no fixed form is reported where it is written.
fn bounded(ty: &syn::Type, krate: &syn::Path) -> Tokens {
    quote_spanned!(ty.span()=> <#ty as #krate::Bounded>)
}

/// The `Option<usize>` constant expression summing the fields' `FIXED_SIZE`.
fn fixed_size(fields: &[FieldInfo], krate: &syn::Path) -> Tokens {
    let paths: Vec<Tokens> = fields.iter().map(|f| durable(&f.ty, krate)).collect();
    quote!(#krate::sum_sizes(&[#(#paths::FIXED_SIZE),*]))
}

/// The `usize` constant expression summing the fields' `FIXED_FORM_SIZE`.
fn fixed_form_size(fields: &[FieldInfo], krate: &syn::Path) -> Tokens {
    let paths: Vec<Tokens> = fields.iter().map(|f| bounded(&f.ty, krate)).collect();
    quote!(0usize #(+ #paths::FIXED_FORM_SIZE)*)
}

/// The guard type every derive kind shares: a `{ inner, region, fields }`
/// struct, and its `Guard` and `Deref` impls. Each kind adds `set` and
/// accessors of its own.
fn guard_scaffold(
    ctx: &Ctx,
    ident: &syn::Ident,
    vis: &syn::Visibility,
    guard_ident: &syn::Ident,
) -> Tokens {
    let krate = &ctx.krate;
    let Ctx {
        type_generics,
        where_clause,
        guard_impl_generics,
        guard_use_generics,
        ..
    } = ctx;
    let doc = format!(
        "The guard of [`{ident}`]: records and applies mutations of a persisted `{ident}`."
    );

    quote! {
        #[doc = #doc]
        #vis struct #guard_ident #guard_impl_generics #where_clause {
            inner: &'__r mut #ident #type_generics,
            region: #krate::Region<'__r, __B>,
            /// The fields' sizes, for guards that `parts` makes together.
            #[allow(dead_code)]
            fields: #krate::FieldSizes,
        }

        impl #guard_impl_generics #krate::Guard
            for #guard_ident #guard_use_generics #where_clause
        {
            type Durable = #ident #type_generics;

            fn as_durable(&self) -> &#ident #type_generics {
                self.inner
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
/// `Durable` impl shares.
fn guard_assoc(ctx: &Ctx, guard_ident: &syn::Ident) -> Tokens {
    let krate = &ctx.krate;
    let guard_use_generics = &ctx.guard_use_generics;
    quote! {
        type Guard<'__r, __B: #krate::Backend + '__r>
            = #guard_ident #guard_use_generics
        where
            Self: '__r;

        #[inline]
        fn guard<'__r, __B: #krate::Backend + '__r>(
            &'__r mut self,
            region: #krate::Region<'__r, __B>,
        ) -> Self::Guard<'__r, __B> {
            #guard_ident {
                inner: self,
                region,
                fields: #krate::FieldSizes::new(),
            }
        }
    }
}

/// The signatures every derive kind spells the same way: `encode` and
/// `decode`, and `encode_fixed` and `decode_fixed`.
fn codec_sigs(krate: &syn::Path) -> [Tokens; 4] {
    [
        quote! {
            #[allow(unused_variables)]
            fn encode(&self, __out: &mut #krate::Encoder<'_>)
        },
        quote! {
            #[allow(unused_variables)]
            fn decode(__input: &mut #krate::Decoder<'_>) -> ::std::result::Result<Self, #krate::Error>
        },
        quote! {
            #[allow(unused_variables)]
            fn encode_fixed(&self, __out: &mut #krate::Encoder<'_>)
        },
        quote! {
            #[allow(unused_variables)]
            fn decode_fixed(__input: &mut #krate::Decoder<'_>) -> ::std::result::Result<Self, #krate::Error>
        },
    ]
}

/// The whole-value `set` of a guard.
fn set_method(ctx: &Ctx, ident: &syn::Ident, vis: &syn::Visibility) -> Tokens {
    let krate = &ctx.krate;
    let type_generics = &ctx.type_generics;
    quote! {
        /// Replaces the whole value: an overwrite in place if its encoding
        /// keeps its size, a splice otherwise. If the write fails, the value
        /// is left as it was.
        #vis fn set(
            &mut self,
            value: #ident #type_generics,
        ) -> ::std::result::Result<(), #krate::Error> {
            #krate::replace(&mut *self.inner, value, &self.region)
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
    if let Err(err) = no_field_attrs(fields) {
        return err.to_compile_error();
    }

    let field_ty = durable(&field.ty, krate);
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

    let scaffold = guard_scaffold(ctx, ident, vis, &guard_ident);
    let guard_assoc = guard_assoc(ctx, &guard_ident);
    let set = set_method(ctx, ident, vis);
    let [encode_sig, decode_sig, encode_fixed_sig, decode_fixed_sig] = codec_sigs(krate);
    let bounded_impl = ctx.bounded.then(|| {
        let field_bounded = bounded(&field.ty, krate);
        let bounded_where = &ctx.bounded_where;
        quote! {
            impl #impl_generics #krate::Bounded for #ident #type_generics #bounded_where {
                const FIXED_FORM_SIZE: usize = #field_bounded::FIXED_FORM_SIZE;

                #encode_fixed_sig {
                    #field_bounded::encode_fixed(&self.#member, __out)
                }

                #decode_fixed_sig {
                    let __value = #field_bounded::decode_fixed(__input)?;
                    ::std::result::Result::Ok(#construct)
                }
            }
        }
    });

    quote! {
        #scaffold

        impl #guard_impl_generics #guard_ident #guard_use_generics #where_clause {
            #set

            /// The guard of the wrapped value, which fills the wrapper's own
            /// region: its whole mutation API, passed through.
            #[inline]
            #vis fn get_mut(&mut self) -> #field_ty::Guard<'_, __B> {
                let __len = self.region.len();
                #field_ty::guard(&mut self.inner.#member, self.region.child(0, __len, None, 0))
            }
        }

        #bounded_impl

        impl #impl_generics #krate::Durable for #ident #type_generics #where_clause {
            const FIXED_SIZE: ::std::option::Option<usize> = #field_ty::FIXED_SIZE;

            #guard_assoc

            #[inline]
            fn encoded_len(&self) -> usize {
                #field_ty::encoded_len(&self.#member)
            }

            #encode_sig {
                #field_ty::encode(&self.#member, __out)
            }

            #decode_sig {
                let __value = #field_ty::decode(__input)?;
                ::std::result::Result::Ok(#construct)
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

    let fields = match FieldInfo::of(&data.fields) {
        Ok(fields) => fields,
        Err(err) => return err.to_compile_error(),
    };
    let ty: Vec<Tokens> = fields.iter().map(|f| durable(&f.ty, krate)).collect();
    let member: Vec<syn::Member> = data
        .fields
        .iter()
        .enumerate()
        .map(|(i, f)| match &f.ident {
            Some(name) => syn::Member::Named(name.clone()),
            None => syn::Member::Unnamed(syn::Index::from(i)),
        })
        .collect();
    let binding: Vec<syn::Ident> = (0..fields.len())
        .map(|i| format_ident!("__field_{}", i))
        .collect();
    let accessor_ident: Vec<syn::Ident> = data
        .fields
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
    let fixed_size = fixed_size(&fields, krate);

    // `S { a, b }`, `S(a, b)` or `S`, built from or bound to `values`.
    let shape = |values: &[Tokens]| match &data.fields {
        Fields::Named(_) => {
            let names = data.fields.iter().map(|f| f.ident.as_ref().unwrap());
            quote! { #ident { #(#names: #values),* } }
        }
        Fields::Unnamed(_) => quote! { #ident( #(#values),* ) },
        Fields::Unit => quote! { #ident },
    };
    let decoded: Vec<Tokens> = ty.iter().map(|t| quote!(#t::decode(__input)?)).collect();
    let decode_body = shape(&decoded);
    let bindings: Vec<Tokens> = binding.iter().map(|b| quote!(#b)).collect();
    let pattern = shape(&bindings);

    // A field's accessor: a guard made on its own, at the sum of the sizes
    // of the fields before it, constant up to the first variable-size one.
    let accessor_body: Vec<Tokens> = (0..fields.len())
        .map(|i| {
            let (t, m) = (&ty[i], &member[i]);
            let (earlier_ty, earlier_member) = (&ty[..i], &member[..i]);
            quote! {
                let __offset = 0usize #(
                    + match #earlier_ty::FIXED_SIZE {
                        ::std::option::Option::Some(__n) => __n,
                        ::std::option::Option::None => #earlier_ty::encoded_len(&self.inner.#earlier_member),
                    }
                )*;
                let __len = #t::encoded_len(&self.inner.#m);
                let Self { inner, region, .. } = self;
                #t::guard(&mut inner.#m, region.child(__offset as u64, __len as u64, None, #i))
            }
        })
        .collect();

    // A `{Ident}Parts` struct plus a `parts()` method handing out a guard for
    // *every* field at once, each borrowing a disjoint part of `self.inner`.
    let parts_ident = format_ident!("{}Parts", ident);
    let (parts_decl_generics, parts_ret_generics, parts_where) = ctx.parts_generics();
    let parts_doc = format!("The guards of every field of [`{ident}`] at once.");
    let field_guard: Vec<Tokens> = (0..fields.len())
        .map(|i| {
            let (t, b) = (&ty[i], &binding[i]);
            quote!(#t::guard(#b, __region.field(__fields, #i)))
        })
        .collect();
    let (parts_struct, parts_ctor) = match &data.fields {
        Fields::Named(_) => {
            let names: Vec<&syn::Ident> = data
                .fields
                .iter()
                .map(|f| f.ident.as_ref().unwrap())
                .collect();
            (
                quote! {
                    #[doc = #parts_doc]
                    #vis struct #parts_ident #parts_decl_generics #parts_where {
                        #(
                            #[doc = #accessor_doc]
                            #vis #names: #ty::Guard<'__f, __B>,
                        )*
                    }
                },
                quote! { #parts_ident { #( #names: #field_guard, )* } },
            )
        }
        Fields::Unnamed(_) => (
            quote! {
                #[doc = #parts_doc]
                #vis struct #parts_ident #parts_decl_generics (
                    #( #vis #ty::Guard<'__f, __B>, )*
                ) #parts_where;
            },
            quote! { #parts_ident( #( #field_guard, )* ) },
        ),
        Fields::Unit => (Tokens::new(), Tokens::new()),
    };
    let parts_method = (!matches!(data.fields, Fields::Unit)).then(|| {
        quote! {
            /// A guard for every field at once, so that all fields can be
            /// mutated simultaneously; each finds where it starts whenever
            /// it writes.
            #[inline]
            #vis fn parts(&mut self) -> #parts_ident #parts_ret_generics {
                let Self { inner, region, fields } = self;
                fields.fill(
                    0,
                    &[#( #ty::FIXED_SIZE, )*],
                    &[#( #ty::encoded_len(&inner.#member), )*],
                );
                let (__fields, __region) = (&*fields, &*region);
                let #pattern = &mut **inner;
                #parts_ctor
            }
        }
    });

    let scaffold = guard_scaffold(ctx, ident, vis, &guard_ident);
    let guard_assoc = guard_assoc(ctx, &guard_ident);
    let set = set_method(ctx, ident, vis);
    let [encode_sig, decode_sig, encode_fixed_sig, decode_fixed_sig] = codec_sigs(krate);
    let field_describe = ty.iter().map(|t| quote!(#t::describe(__builder)));
    let bounded_impl = ctx.bounded.then(|| {
        let bty: Vec<Tokens> = fields.iter().map(|f| bounded(&f.ty, krate)).collect();
        let decoded_fixed: Vec<Tokens> = bty
            .iter()
            .map(|t| quote!(#t::decode_fixed(__input)?))
            .collect();
        let decode_fixed_body = shape(&decoded_fixed);
        let fixed_form_size = fixed_form_size(&fields, krate);
        let bounded_where = &ctx.bounded_where;
        quote! {
            impl #impl_generics #krate::Bounded for #ident #type_generics #bounded_where {
                const FIXED_FORM_SIZE: usize = #fixed_form_size;

                #encode_fixed_sig {
                    #( #bty::encode_fixed(&self.#member, __out); )*
                }

                #decode_fixed_sig {
                    ::std::result::Result::Ok(#decode_fixed_body)
                }
            }
        }
    });

    quote! {
        #scaffold

        #parts_struct

        impl #guard_impl_generics #guard_ident #guard_use_generics #where_clause {
            #set

            #(
                #[doc = #accessor_doc]
                #[inline]
                #vis fn #accessor_ident(&mut self) -> #ty::Guard<'_, __B> {
                    #accessor_body
                }
            )*

            #parts_method
        }

        #bounded_impl

        impl #impl_generics #krate::Durable for #ident #type_generics #where_clause {
            const FIXED_SIZE: ::std::option::Option<usize> = #fixed_size;

            #guard_assoc

            #[inline]
            fn encoded_len(&self) -> usize {
                0usize #( + #ty::encoded_len(&self.#member) )*
            }

            #encode_sig {
                #( #ty::encode(&self.#member, __out); )*
            }

            #decode_sig {
                ::std::result::Result::Ok(#decode_body)
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

/// One variant of an enum, as the generated code needs it.
struct VariantInfo {
    ident: syn::Ident,
    fields: Vec<FieldInfo>,
    /// The variant's fields bound in a match pattern, prefixed so that a
    /// field named like a parameter of the generated code does not shadow it.
    bindings: Vec<syn::Ident>,
    /// A match pattern binding the fields to `bindings`.
    pattern: Tokens,
    kind: VariantKind,
}

enum VariantKind {
    Named(Vec<syn::Ident>),
    Unnamed,
    Unit,
}

impl VariantInfo {
    fn of(enum_ident: &syn::Ident, variant: &syn::Variant) -> syn::Result<VariantInfo> {
        let v_ident = &variant.ident;
        let fields = FieldInfo::of(&variant.fields)?;
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

/// The encoding: the discriminant as a varint, then the variant's fields.
/// The fixed form: the discriminant at its width, little-endian, the
/// variant's fields in their fixed forms, and zeros up to the size of the
/// largest variant.
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
            "#[derive(Durable)] does not support enums with no variants",
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
        .map(|v| VariantInfo::of(ident, v))
        .collect()
    {
        Ok(variants) => variants,
        Err(err) => return err.to_compile_error(),
    };
    let variant_count = variants.len();
    let has_fields = variants.iter().any(|v| !v.fields.is_empty());

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
    let negative_msg =
        format!("#[derive(Durable)] does not support negative discriminants (in enum `{ident}`)");
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
                #krate::discriminant_width(max)
            }
        },
    };
    // Where every generated item names the discriminants and their width.
    let discriminants = quote!(<#ident #type_generics>::__KLADDE_DISCRIMINANTS);
    let disc_width = quote!(<#ident #type_generics>::__KLADDE_DISCRIMINANT_WIDTH);

    let mut fixed_sizes = Vec::new();
    let mut fixed_form_sizes = Vec::new();
    let mut len_arms = Vec::new();
    let mut encode_arms = Vec::new();
    let mut encode_fixed_arms = Vec::new();
    let mut decode_arms = Vec::new();
    let mut decode_fixed_arms = Vec::new();
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
        let ty: Vec<Tokens> = fields.iter().map(|f| durable(&f.ty, krate)).collect();
        let bty: Vec<Tokens> = fields.iter().map(|f| bounded(&f.ty, krate)).collect();
        let form = fixed_form_size(fields, krate);
        fixed_sizes.push(fixed_size(fields, krate));
        fixed_form_sizes.push(form.clone());

        len_arms.push(quote! {
            #pattern => #krate::varint_len(#discriminants[#i]) #( + #ty::encoded_len(#bindings) )*
        });
        encode_arms.push(quote! {
            #pattern => {
                __out.varint(#discriminants[#i]);
                #( #ty::encode(#bindings, __out); )*
            }
        });
        encode_fixed_arms.push(quote! {
            #pattern => {
                __out.bytes(&#discriminants[#i].to_le_bytes()[..#disc_width]);
                #( #bty::encode_fixed(#bindings, __out); )*
                let __used = #disc_width + #form;
                __out.zeros(<Self as #krate::Bounded>::FIXED_FORM_SIZE - __used);
            }
        });
        let decoded: Vec<Tokens> = ty.iter().map(|t| quote!(#t::decode(__input)?)).collect();
        let construct = variant.construct(ident, &decoded);
        decode_arms.push(quote! {
            __d if __d == #discriminants[#i] => #construct
        });
        let decoded_fixed: Vec<Tokens> = bty
            .iter()
            .map(|t| quote!(#t::decode_fixed(__input)?))
            .collect();
        let construct_fixed = variant.construct(ident, &decoded_fixed);
        decode_fixed_arms.push(quote! {
            __d if __d == #discriminants[#i] => {
                let __value = #construct_fixed;
                let __used = #disc_width + #form;
                for _ in __used..<Self as #krate::Bounded>::FIXED_FORM_SIZE {
                    if __input.byte()? != 0 {
                        return ::std::result::Result::Err(__input.corrupt("padding that is not zero"));
                    }
                }
                __value
            }
        });
        let field_name: Vec<&String> = fields.iter().map(|f| &f.schema_name).collect();
        describe_variants.push(quote! {
            #krate::Variant {
                discriminant: #discriminants[#i],
                name: ::std::string::ToString::to_string(::std::stringify!(#v_ident)),
                fields: ::std::vec![
                    #(
                        #krate::Field {
                            name: ::std::string::ToString::to_string(#field_name),
                            ty: #ty::describe(__builder),
                        },
                    )*
                ],
            }
        });

        // `parts()`: the guards of the current variant's fields, made
        // together, past the discriminant.
        let guards: Vec<Tokens> = (0..fields.len())
            .map(|k| {
                let (t, b) = (&ty[k], &bindings[k]);
                quote!(#t::guard(#b, __region.field(__fields, #k)))
            })
            .collect();
        let fill = quote! {
            fields.fill(
                #krate::varint_len(#discriminants[#i]) as u64,
                &[#( #ty::FIXED_SIZE, )*],
                &[#( #ty::encoded_len(#bindings), )*],
            );
            let (__fields, __region) = (&*fields, &*region);
        };
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
                            #names: #ty::Guard<'__f, __B>,
                        )*
                    }
                });
                parts_arms.push(quote! {
                    #pattern => { #fill #parts_ident::#v_ident { #(#names: #guards,)* } }
                });
            }
            VariantKind::Unnamed => {
                parts_variants.push(quote! {
                    #[doc = #doc]
                    #v_ident( #(#ty::Guard<'__f, __B>,)* )
                });
                parts_arms.push(quote! {
                    #pattern => { #fill #parts_ident::#v_ident(#(#guards,)*) }
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
        let (parts_decl_generics, parts_ret_generics, parts_where) = ctx.parts_generics();
        let parts_doc = format!(
            "The guards of the fields of the current variant of a persisted [`{ident}`], \
             handed out by its guard's `parts()`."
        );
        quote! {
            #[doc = #parts_doc]
            #vis enum #parts_ident #parts_decl_generics #parts_where {
                #(#parts_variants,)*
            }

            impl #guard_impl_generics #guard_ident #guard_use_generics #where_clause {
                /// The guards of the current variant's fields, for mutating
                /// them in place; match on the result to reach them. To
                /// switch to another variant, use `set`.
                #[inline]
                #[allow(unused_variables)]
                #vis fn parts(&mut self) -> #parts_ident #parts_ret_generics {
                    let Self { inner, region, fields } = self;
                    match &mut **inner {
                        #(#parts_arms,)*
                    }
                }
            }
        }
    });

    let scaffold = guard_scaffold(ctx, ident, vis, &guard_ident);
    let guard_assoc = guard_assoc(ctx, &guard_ident);
    let set = set_method(ctx, ident, vis);
    let [encode_sig, decode_sig, encode_fixed_sig, decode_fixed_sig] = codec_sigs(krate);
    let bounded_impl = ctx.bounded.then(|| {
        let bounded_where = &ctx.bounded_where;
        quote! {
            impl #impl_generics #krate::Bounded for #ident #type_generics #bounded_where {
                const FIXED_FORM_SIZE: usize =
                    #krate::enum_fixed_form_size(#disc_width, &[#(#fixed_form_sizes),*]);

                #encode_fixed_sig {
                    match self {
                        #(#encode_fixed_arms)*
                    }
                }

                #decode_fixed_sig {
                    let mut __buf = [0u8; 8];
                    for __b in &mut __buf[..#disc_width] {
                        *__b = __input.byte()?;
                    }
                    let __discriminant = u64::from_le_bytes(__buf);
                    ::std::result::Result::Ok(match __discriminant {
                        #(#decode_fixed_arms,)*
                        _ => return ::std::result::Result::Err(__input.corrupt(::std::format!(
                            "unknown discriminant {} of {}",
                            __discriminant,
                            ::std::stringify!(#ident),
                        ))),
                    })
                }
            }
        }
    });

    quote! {
        #scaffold

        #parts

        impl #guard_impl_generics #guard_ident #guard_use_generics #where_clause {
            #set
        }

        #bounded_impl

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
            /// The discriminant's width in bytes in the fixed form: the
            /// integer `repr`'s, or else the smallest of 1, 2, 4 and 8 that
            /// holds every value.
            const __KLADDE_DISCRIMINANT_WIDTH: usize = #width_expr;
        }

        impl #impl_generics #krate::Durable for #ident #type_generics #where_clause {
            const FIXED_SIZE: ::std::option::Option<usize> =
                #krate::enum_fixed_size(&#discriminants, &[#(#fixed_sizes),*]);

            #guard_assoc

            #[inline]
            fn encoded_len(&self) -> usize {
                match self {
                    #(#len_arms,)*
                }
            }

            #encode_sig {
                match self {
                    #(#encode_arms)*
                }
            }

            #decode_sig {
                let __discriminant = __input.varint()?;
                ::std::result::Result::Ok(match __discriminant {
                    #(#decode_arms,)*
                    _ => return ::std::result::Result::Err(__input.corrupt(::std::format!(
                        "unknown discriminant {} of {}",
                        __discriminant,
                        ::std::stringify!(#ident),
                    ))),
                })
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
                            "#[derive(Durable)] supports only the integer reprs `u8`, `u16`, \
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
