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
//! - A derived enum is treated as an "owning" type instead (fixed 8-byte
//!   header, like `String`/`PersistedVec`): only whole-value replacement
//!   is supported (`guard.set(new_value)`), not mutating a field within
//!   the current variant in place, so its content is just the
//!   `postcard`-serialized whole value. See `spec.md`'s Future Work.
//! - Generic types and types with where-clauses are not yet supported.
//!
//! **Dependency note:** generated code references `::kladde_traits::...`,
//! `::serde::...`, and (for enums) `::postcard::...` paths directly, so
//! any crate using this macro needs `kladde-traits`, `serde`, and (if it
//! derives on any enum) `postcard` as *direct* dependencies too --
//! re-exports (e.g. via `kladde-types`) aren't enough to make
//! `::kladde_traits` resolve. The same reason `#[derive(serde::Serialize)]`
//! requires a direct `serde` dependency, not just `serde_derive`.

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

    // Field `i`'s static byte offset within this struct's own inline
    // representation is the sum of every *earlier* field's `INLINE_SIZE`
    // -- computable purely from the field types, so it's a `const`-like
    // expression the compiler evaluates, not something tracked at
    // runtime.
    let field_offset: Vec<_> = (0..field_ty.len())
        .map(|i| {
            let earlier = &field_ty[..i];
            quote! {
                (0usize #( + <#earlier as ::kladde_traits::Persistable>::INLINE_SIZE )*) as u32
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
            const INLINE_SIZE: usize =
                0usize #( + <#field_ty as ::kladde_traits::Persistable>::INLINE_SIZE )*;

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

            fn store<B: ::kladde_traits::Backend>(&self, backend: &B, location: ::kladde_traits::Location) {
                #(
                    ::kladde_traits::Persistable::store(
                        &self.#field_ident,
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

            fn store<B: ::kladde_traits::Backend>(&self, _backend: &B, _location: ::kladde_traits::Location) {}

            fn load<B: ::kladde_traits::Backend>(_backend: &B, _location: ::kladde_traits::Location) -> Self {
                #ident
            }
        }
    }
}

fn derive_enum(input: &DeriveInput, _data: &syn::DataEnum) -> proc_macro2::TokenStream {
    let ident = &input.ident;
    let vis = &input.vis;
    let guard_ident = format_ident!("{}Guard", ident);

    quote! {
        // v1 scope (see spec.md's Future Work): the only mutation is
        // replacing the whole value, so there's no per-variant,
        // per-field granularity yet, and an enum is treated as an
        // "owning" type (fixed 8-byte header, content elsewhere) rather
        // than trying to give each variant its own static byte layout --
        // see this module's doc comment.
        #[doc(hidden)]
        #vis struct #guard_ident<'s, B> {
            inner: &'s mut #ident,
            backend: &'s B,
            location: ::kladde_traits::Location,
        }

        impl<'s, B: ::kladde_traits::Backend> #guard_ident<'s, B> {
            /// Replaces the whole value with `value`. See spec.md's
            /// Future Work for the deferred fine-grained alternative
            /// (mutating a field within the current variant in place,
            /// and/or matching directly on this guard).
            #vis fn set(&mut self, value: #ident) {
                ::kladde_traits::Persistable::store(&value, self.backend, self.location);
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
            /// A fixed 8-byte `{ target, len }` header -- see
            /// `String`/`PersistedVec`'s identical layout note. Note the
            /// same interim limitation `String` has: a plain derived
            /// enum has no room for a persistent `pointer` field of its
            /// own, so every `store` allocates fresh rather than
            /// resizing/reusing, leaking the previous allocation (see
            /// `spec.md`).
            const INLINE_SIZE: usize = 8;

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

            fn store<B: ::kladde_traits::Backend>(&self, backend: &B, location: ::kladde_traits::Location) {
                let bytes = ::postcard::to_allocvec(self)
                    .expect("postcard serialization of an in-memory value should not fail");
                let pointer = ::kladde_traits::Allocator::alloc::<#ident>(backend, bytes.len());
                ::kladde_traits::Allocator::write(
                    backend,
                    ::kladde_traits::UniquePointer::raw(&pointer),
                    0,
                    &bytes,
                );
                ::kladde_traits::write_header(backend, location, ::kladde_traits::UniquePointer::index(&pointer), bytes.len() as u32);
            }

            fn load<B: ::kladde_traits::Backend>(backend: &B, location: ::kladde_traits::Location) -> Self {
                let (target, len) = ::kladde_traits::read_header(backend, location);
                let target = target.expect(concat!(
                    "no ", stringify!(#ident), " stored at this location -- load() called \
                     before this value was ever set"
                ));
                let bytes = ::kladde_traits::Allocator::read(
                    backend,
                    ::kladde_traits::RawPointer::from_index(target),
                    0,
                    len,
                );
                ::postcard::from_bytes(&bytes).expect("corrupt persisted enum bytes")
            }
        }
    }
}
