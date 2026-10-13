//! `#[kladde::dispatch]` on a trait, and `#[kladde(dispatch(Trait))]` on an
//! enum that derives `Durable`: the enum implements the trait by forwarding
//! every method to the value its current variant holds.
//!
//! A derive sees only the item it is attached to, never the trait's
//! methods, so the two macros meet through a third: the attribute on the
//! trait emits a `macro_rules!` macro, `__kladde_dispatch_{Trait}`, that
//! knows the trait's methods and writes the forwarding impl for whatever
//! enum it is handed, and re-exports it next to the trait with
//! `pub(crate) use`, so that the derive can invoke it by the trait's own
//! path, anywhere in the crate and in any order. That `use` is why the trait
//! and the enum must live in one crate: a `macro_rules!` macro can be public
//! only through `#[macro_export]`, which puts it at the crate root, where a
//! macro-expanded one may not be named by path from its own crate.
//!
//! The derive hands the macro, per variant, the pattern that binds the held
//! value, its type, and three expressions that reach the value from the
//! binding by reference, by mutable reference and by value: through the box
//! for a variant that holds a `Box<T>`, which forwards to `T`'s
//! implementation. The derive also writes the impl's `where` clause, with a
//! `Trait` bound on each variant's type spanned at that type, so a variant
//! whose type does not implement the trait is reported there, and a generic
//! enum gets the bounds its forwarding needs.

use proc_macro2::{Span, TokenStream as Tokens, TokenTree};
use quote::{format_ident, quote, quote_spanned};
use syn::spanned::Spanned;
use syn::{Data, DeriveInput, Fields, FnArg, ItemTrait, Pat, ReturnType, TraitItem, Type};

/// The name of the macro that `#[kladde::dispatch]` emits for `trait_ident`.
fn macro_ident(trait_ident: &syn::Ident) -> syn::Ident {
    format_ident!("__kladde_dispatch_{}", trait_ident)
}

/// How a method takes `self`, and so how a variant's value reaches it.
#[derive(Clone, Copy)]
enum Receiver {
    Ref,
    Mut,
    Value,
}

/// Whether `tokens` mention `Self` anywhere.
fn mentions_self(tokens: Tokens) -> bool {
    tokens.into_iter().any(|t| match t {
        TokenTree::Ident(i) => i == "Self",
        TokenTree::Group(g) => mentions_self(g.stream()),
        _ => false,
    })
}

/// Whether `tokens` hold the keyword `impl`, as an `impl Trait` type does.
fn mentions_impl(tokens: Tokens) -> bool {
    tokens.into_iter().any(|t| match t {
        TokenTree::Ident(i) => i == "impl",
        TokenTree::Group(g) => mentions_impl(g.stream()),
        _ => false,
    })
}

/// The receiver of a method, or why it cannot be forwarded.
fn receiver(sig: &syn::Signature) -> syn::Result<Receiver> {
    let Some(FnArg::Receiver(r)) = sig.inputs.first() else {
        return Err(syn::Error::new_spanned(
            &sig.ident,
            "#[kladde::dispatch] forwards only methods that take `self`; this one has no \
             receiver, so an enum could not choose a variant to forward it to",
        ));
    };
    let is_self =
        |ty: &Type| matches!(ty, Type::Path(p) if p.qself.is_none() && p.path.is_ident("Self"));
    match &*r.ty {
        Type::Reference(rf) if is_self(&rf.elem) => Ok(if rf.mutability.is_some() {
            Receiver::Mut
        } else {
            Receiver::Ref
        }),
        ty if is_self(ty) => Ok(Receiver::Value),
        ty => Err(syn::Error::new_spanned(
            ty,
            "#[kladde::dispatch] forwards methods taking `self`, `&self` or `&mut self` only",
        )),
    }
}

/// One method of the impl the macro writes: its signature, with its
/// arguments renamed, and a `match` that forwards it to each variant.
fn forward(method: &syn::TraitItemFn) -> syn::Result<Tokens> {
    let sig = &method.sig;
    let recv = receiver(sig)?;
    for input in sig.inputs.iter().skip(1) {
        if let FnArg::Typed(arg) = input {
            if mentions_self(quote!(#arg)) {
                return Err(syn::Error::new_spanned(
                    arg,
                    "#[kladde::dispatch] cannot forward a method that takes `Self` other than \
                     as its receiver: each variant would need a value of its own type",
                ));
            }
        }
    }
    if let ReturnType::Type(_, ty) = &sig.output {
        if mentions_self(quote!(#ty)) {
            return Err(syn::Error::new_spanned(
                ty,
                "#[kladde::dispatch] cannot forward a method that returns `Self`: each variant \
                 would return a value of its own type",
            ));
        }
        if mentions_impl(quote!(#ty)) {
            return Err(syn::Error::new_spanned(
                ty,
                "#[kladde::dispatch] cannot forward a method that returns `impl Trait`: each \
                 variant would return a different type",
            ));
        }
    }

    // The signature, with every argument bound to a name of its own, so that
    // patterns such as `(a, b): (u8, u8)` forward as one value.
    let mut sig = sig.clone();
    let mut args = Vec::new();
    for (i, input) in sig.inputs.iter_mut().skip(1).enumerate() {
        if let FnArg::Typed(arg) = input {
            let name = format_ident!("__kladde_arg{}", i);
            *arg.pat = Pat::Verbatim(quote!(#name));
            arg.attrs.clear();
            args.push(name);
        }
    }

    // Type and const parameters are named in the call unless an argument's
    // type is `impl Trait`, which forbids naming them.
    let impl_args = sig.inputs.iter().any(|a| match a {
        FnArg::Typed(arg) => mentions_impl({
            let ty = &arg.ty;
            quote!(#ty)
        }),
        FnArg::Receiver(_) => false,
    });
    let params: Vec<Tokens> = sig
        .generics
        .params
        .iter()
        .filter_map(|p| match p {
            syn::GenericParam::Type(t) => Some({
                let i = &t.ident;
                quote!(#i)
            }),
            syn::GenericParam::Const(c) => Some({
                let i = &c.ident;
                quote!(#i)
            }),
            syn::GenericParam::Lifetime(_) => None,
        })
        .collect();
    let turbofish = (!impl_args && !params.is_empty()).then(|| quote!(::<#(#params),*>));

    let name = &sig.ident;
    let value = match recv {
        Receiver::Ref => quote!($r),
        Receiver::Mut => quote!($m),
        Receiver::Value => quote!($v),
    };
    let mut call = quote! { <$ty as $tr>::#name #turbofish(#value, #(#args),*) };
    if sig.asyncness.is_some() {
        call = quote!(#call.await);
    }
    if sig.unsafety.is_some() {
        call = quote!(unsafe { #call });
    }
    let cfgs = method.attrs.iter().filter(|a| a.path().is_ident("cfg"));
    Ok(quote! {
        #(#cfgs)*
        #[inline]
        #sig {
            match self {
                $( $($pat)* => #call, )*
            }
        }
    })
}

/// `#[kladde::dispatch]`: the trait unchanged, and the macro that writes its
/// forwarding impl for an enum.
pub fn dispatch_trait(attr: Tokens, item: ItemTrait) -> Tokens {
    if !attr.is_empty() {
        return syn::Error::new_spanned(attr, "#[kladde::dispatch] takes no arguments")
            .to_compile_error();
    }
    if !item.generics.params.is_empty() {
        return syn::Error::new_spanned(
            &item.generics,
            "#[kladde::dispatch] does not support generic traits",
        )
        .to_compile_error();
    }
    let mut methods = Vec::new();
    let mut errors = Tokens::new();
    for it in &item.items {
        match it {
            TraitItem::Fn(method) => match forward(method) {
                Ok(m) => methods.push(m),
                Err(e) => errors.extend(e.to_compile_error()),
            },
            TraitItem::Type(t) => errors.extend(
                syn::Error::new_spanned(
                    t,
                    "#[kladde::dispatch] cannot forward an associated type: the variants \
                     may each choose another",
                )
                .to_compile_error(),
            ),
            TraitItem::Const(c) => errors.extend(
                syn::Error::new_spanned(
                    c,
                    "#[kladde::dispatch] cannot forward an associated constant: the variants \
                     may each choose another",
                )
                .to_compile_error(),
            ),
            other => errors.extend(
                syn::Error::new_spanned(other, "#[kladde::dispatch] cannot forward this item")
                    .to_compile_error(),
            ),
        }
    }
    let macro_name = macro_ident(&item.ident);
    let unsafety = &item.unsafety;
    quote! {
        #item
        #errors

        #[doc(hidden)]
        macro_rules! #macro_name {
            (
                impl [$($g:tt)*] $tr:path => [$enum_:ty] where [$($w:tt)*] {
                    $( [$($pat:tt)*] [$ty:ty] [$r:expr] [$m:expr] [$v:expr] ),* $(,)?
                }
            ) => {
                #unsafety impl $($g)* $tr for $enum_ where $($w)* {
                    #(#methods)*
                }
            };
        }
        #[doc(hidden)]
        #[allow(unused_imports)]
        pub(crate) use #macro_name;
    }
}

/// The type a variant forwards to, and whether it is held in a `Box`: the
/// `T` of a field spelled `Box<T>`, `std::boxed::Box<T>` or
/// `alloc::boxed::Box<T>`, and the field's own type otherwise.
fn unboxed(ty: &Type) -> (&Type, bool) {
    if let Type::Path(p) = ty {
        if let Some(last) = p.path.segments.last() {
            if last.ident == "Box" {
                if let syn::PathArguments::AngleBracketed(a) = &last.arguments {
                    if let [syn::GenericArgument::Type(inner)] =
                        a.args.iter().collect::<Vec<_>>()[..]
                    {
                        return (inner, true);
                    }
                }
            }
        }
    }
    (ty, false)
}

/// For an enum marked `#[kladde(dispatch(Trait, ...))]`: an implementation of
/// each trait that forwards to the current variant's value, and `From` for
/// every variant's type, and for the `T` of a variant that holds a `Box<T>`.
pub fn dispatch_impls(input: &DeriveInput, traits: &[syn::Path]) -> Tokens {
    let Data::Enum(data) = &input.data else {
        return syn::Error::new(
            Span::call_site(),
            "#[kladde(dispatch(...))] is only for enums",
        )
        .to_compile_error();
    };
    let ident = &input.ident;
    let (impl_generics, type_generics, where_clause) = input.generics.split_for_impl();
    let where_preds = where_clause.map(|w| {
        let p = &w.predicates;
        quote!(#p,)
    });

    let mut arms = Vec::new();
    let mut targets: Vec<&Type> = Vec::new();
    let mut froms: Vec<(Tokens, Tokens)> = Vec::new();
    for v in &data.variants {
        let field = match &v.fields {
            Fields::Unnamed(f) if f.unnamed.len() == 1 => &f.unnamed[0],
            _ => {
                return syn::Error::new_spanned(
                    v,
                    "every variant of an enum with `#[kladde(dispatch(...))]` holds one value, \
                     as `Circle(Circle)` does, which the variant forwards the trait's methods to",
                )
                .to_compile_error();
            }
        };
        let v_ident = &v.ident;
        let field_ty = &field.ty;
        let (target, boxed) = unboxed(field_ty);
        let bind = quote!(__kladde_inner);
        let (r, m, o) = if boxed {
            (quote!(&**#bind), quote!(&mut **#bind), quote!(*#bind))
        } else {
            (quote!(#bind), quote!(#bind), quote!(#bind))
        };
        arms.push(quote! {
            [#ident::#v_ident(#bind)] [#target] [#r] [#m] [#o]
        });
        targets.push(target);
        froms.push((quote!(#field_ty), quote!(#ident::#v_ident(__kladde_value))));
        if boxed {
            froms.push((
                quote!(#target),
                quote!(#ident::#v_ident(::std::boxed::Box::new(__kladde_value))),
            ));
        }
    }

    let impls = traits.iter().map(|tr| {
        let mut macro_path = tr.clone();
        let last = macro_path
            .segments
            .last_mut()
            .expect("a path has a segment");
        last.ident = macro_ident(&last.ident);
        last.arguments = syn::PathArguments::None;
        // Spanned at each variant's type, so that a type that does not
        // implement the trait is reported there.
        let bounds = targets
            .iter()
            .map(|target| quote_spanned!(target.span()=> #target: #tr,));
        quote! {
            #macro_path! {
                impl [#impl_generics] #tr => [#ident #type_generics]
                where [#where_preds #(#bounds)*] {
                    #(#arms),*
                }
            }
        }
    });

    // `From` for a type that two variants hold would conflict, so such a
    // type gets none; nor does a type that mentions a type parameter, whose
    // `From` could conflict with another variant's for some argument, as
    // `From<T>` does with every other.
    let params: Vec<String> = input
        .generics
        .type_params()
        .map(|p| p.ident.to_string())
        .collect();
    let mentions_param = |tokens: &Tokens| {
        fn walk(tokens: Tokens, params: &[String]) -> bool {
            tokens.into_iter().any(|t| match t {
                TokenTree::Ident(i) => params.contains(&i.to_string()),
                TokenTree::Group(g) => walk(g.stream(), params),
                _ => false,
            })
        }
        walk(tokens.clone(), &params)
    };
    let keys: Vec<String> = froms.iter().map(|(t, _)| t.to_string()).collect();
    let from_impls = froms.iter().enumerate().filter_map(|(i, (ty, build))| {
        let unique = keys.iter().filter(|k| **k == keys[i]).count() == 1;
        (unique && !mentions_param(ty)).then(|| {
            quote! {
                impl #impl_generics ::core::convert::From<#ty> for #ident #type_generics #where_clause {
                    #[inline]
                    fn from(__kladde_value: #ty) -> Self {
                        #build
                    }
                }
            }
        })
    });

    quote! {
        #(#impls)*
        #(#from_impls)*
    }
}
