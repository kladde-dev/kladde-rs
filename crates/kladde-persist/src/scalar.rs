//! Blanket [`Persistable`] impls for primitive/scalar types, so
//! `#[derive(Persistable)]`-generated code can treat every struct field
//! uniformly -- a `_mut()` accessor returning a nested [`Guard`] -- rather than
//! special-casing "leaf" types with direct setters.
//!
//! These live here, in the crate that defines `Persistable`, rather than in
//! `kladde-types`: `impl Persistable for i32` is `impl ForeignTrait for
//! ForeignType` from `kladde-types`' point of view, which the orphan rules
//! forbid. From this crate's point of view the trait is local, so it is allowed.
//! `kladde-types` re-exports these names for convenience.
//!
//! Every scalar is pointer-free, so each implements `Persistable<P>` for **all**
//! `P: PointerRepr` -- usable at any pointer width. The guard types are generic
//! over the backend alone and read the pointer type back off it as `B::Pointer`,
//! which is why they need no `P` parameter of their own.

use kladde_heap::{ReadBackend, Word, WriteBackend};
use kladde_schema::{Primitive, TypeDescriptor};
use std::io::Read;

use crate::guard::Guard;
use crate::location::Location;
use crate::persistable::Persistable;
use crate::schema::SchemaBuilder;
use kladde_heap::PointerRepr;

/// Reads exactly `N` bytes at `location` -- the shared read half of every scalar
/// `load` below.
fn read_bytes<const N: usize, B: ReadBackend>(
    backend: &mut B,
    location: Location<B::Pointer, B::Size>,
) -> [u8; N] {
    let mut buf = [0u8; N];
    backend
        .read_at(location.anchor, location.offset)
        .read_exact(&mut buf)
        .expect("read scalar bytes");
    buf
}

/// Numeric scalars all have native `to_le_bytes`/`from_le_bytes` with a
/// fixed-width array, so one macro covers them; `bool`/`char` don't fit that
/// shape and are implemented by hand below. `String` doesn't implement
/// `Persistable` at all -- see the note further down.
macro_rules! impl_persistable_numeric_scalar {
    ($ty:ty, $guard:ident, $code:expr) => {
        #[doc = concat!("The [`Guard`] for `", stringify!($ty), "`.")]
        pub struct $guard<'s, B: WriteBackend> {
            inner: &'s mut $ty,
            backend: &'s B,
            location: Location<B::Pointer, B::Size>,
        }

        impl<'s, B: WriteBackend> $guard<'s, B> {
            /// Replaces the value, writing its inline bytes at this guard's
            /// location.
            pub fn set(&mut self, mut value: $ty) {
                <$ty as Persistable<B::Pointer>>::store(&mut value, self.backend, self.location);
                *self.inner = value;
            }
        }

        impl<'s, B: WriteBackend> Guard for $guard<'s, B> {
            type Persistable = $ty;
            type Backend = B;

            fn as_persistable(&self) -> &$ty {
                self.inner
            }
            fn as_persistable_mut(&mut self) -> &mut $ty {
                self.inner
            }
            fn backend(&self) -> &B {
                self.backend
            }
        }

        impl<'s, B: WriteBackend> ::std::ops::Deref for $guard<'s, B> {
            type Target = $ty;
            fn deref(&self) -> &$ty {
                self.inner
            }
        }

        impl<'s, B: WriteBackend> ::std::ops::DerefMut for $guard<'s, B> {
            fn deref_mut(&mut self) -> &mut $ty {
                self.inner
            }
        }

        impl<P: PointerRepr> Persistable<P> for $ty {
            const INLINE_SIZE: usize = ::std::mem::size_of::<$ty>();

            type Guard<'s, B: WriteBackend<Pointer = P>>
                = $guard<'s, B>
            where
                Self: 's,
                B: 's;

            #[inline]
            fn guard<'s, B: WriteBackend<Pointer = P>>(
                &'s mut self,
                backend: &'s B,
                location: Location<P, B::Size>,
            ) -> Self::Guard<'s, B> {
                $guard {
                    inner: self,
                    backend,
                    location,
                }
            }

            fn store<B: WriteBackend<Pointer = P>>(
                &mut self,
                backend: &B,
                location: Location<P, B::Size>,
            ) {
                backend.write(location.anchor, location.offset, &self.to_le_bytes());
            }

            fn load<B: ReadBackend<Pointer = P>>(
                backend: &mut B,
                location: Location<P, B::Size>,
            ) -> Self {
                Self::from_le_bytes(read_bytes::<{ ::std::mem::size_of::<$ty>() }, B>(
                    backend, location,
                ))
            }

            fn describe_local(_builder: &mut SchemaBuilder) -> TypeDescriptor {
                TypeDescriptor::Primitive($code)
            }
        }
    };
}

impl_persistable_numeric_scalar!(u8, U8Guard, Primitive::U8);
impl_persistable_numeric_scalar!(u16, U16Guard, Primitive::U16);
impl_persistable_numeric_scalar!(u32, U32Guard, Primitive::U32);
impl_persistable_numeric_scalar!(u64, U64Guard, Primitive::U64);
impl_persistable_numeric_scalar!(i8, I8Guard, Primitive::I8);
impl_persistable_numeric_scalar!(i16, I16Guard, Primitive::I16);
impl_persistable_numeric_scalar!(i32, I32Guard, Primitive::I32);
impl_persistable_numeric_scalar!(i64, I64Guard, Primitive::I64);
impl_persistable_numeric_scalar!(f32, F32Guard, Primitive::F32);
impl_persistable_numeric_scalar!(f64, F64Guard, Primitive::F64);

/// `bool` and `char` don't have `to_le_bytes`/`from_le_bytes`, so they're
/// encoded by hand (as one byte, and as `u32`, respectively) rather than going
/// through the numeric macro above.
macro_rules! impl_persistable_scalar_via {
    ($ty:ty, $guard:ident, $repr:ty, $to_repr:expr, $from_repr:expr, $code:expr) => {
        #[doc = concat!("The [`Guard`] for `", stringify!($ty), "`.")]
        pub struct $guard<'s, B: WriteBackend> {
            inner: &'s mut $ty,
            backend: &'s B,
            location: Location<B::Pointer, B::Size>,
        }

        impl<'s, B: WriteBackend> $guard<'s, B> {
            /// Replaces the value, writing its inline bytes at this guard's
            /// location.
            pub fn set(&mut self, mut value: $ty) {
                <$ty as Persistable<B::Pointer>>::store(&mut value, self.backend, self.location);
                *self.inner = value;
            }
        }

        impl<'s, B: WriteBackend> Guard for $guard<'s, B> {
            type Persistable = $ty;
            type Backend = B;

            fn as_persistable(&self) -> &$ty {
                self.inner
            }
            fn as_persistable_mut(&mut self) -> &mut $ty {
                self.inner
            }
            fn backend(&self) -> &B {
                self.backend
            }
        }

        impl<'s, B: WriteBackend> ::std::ops::Deref for $guard<'s, B> {
            type Target = $ty;
            fn deref(&self) -> &$ty {
                self.inner
            }
        }

        impl<'s, B: WriteBackend> ::std::ops::DerefMut for $guard<'s, B> {
            fn deref_mut(&mut self) -> &mut $ty {
                self.inner
            }
        }

        impl<P: PointerRepr> Persistable<P> for $ty {
            const INLINE_SIZE: usize = ::std::mem::size_of::<$repr>();

            type Guard<'s, B: WriteBackend<Pointer = P>>
                = $guard<'s, B>
            where
                Self: 's,
                B: 's;

            #[inline]
            fn guard<'s, B: WriteBackend<Pointer = P>>(
                &'s mut self,
                backend: &'s B,
                location: Location<P, B::Size>,
            ) -> Self::Guard<'s, B> {
                $guard {
                    inner: self,
                    backend,
                    location,
                }
            }

            fn store<B: WriteBackend<Pointer = P>>(
                &mut self,
                backend: &B,
                location: Location<P, B::Size>,
            ) {
                let to_repr: fn($ty) -> $repr = $to_repr;
                let repr = to_repr(*self);
                backend.write(location.anchor, location.offset, &repr.to_le_bytes());
            }

            fn load<B: ReadBackend<Pointer = P>>(
                backend: &mut B,
                location: Location<P, B::Size>,
            ) -> Self {
                let from_repr: fn($repr) -> $ty = $from_repr;
                let bytes = read_bytes::<{ ::std::mem::size_of::<$repr>() }, B>(backend, location);
                from_repr(<$repr>::from_le_bytes(bytes))
            }

            fn describe_local(_builder: &mut SchemaBuilder) -> TypeDescriptor {
                TypeDescriptor::Primitive($code)
            }
        }
    };
}

impl_persistable_scalar_via!(
    bool,
    BoolGuard,
    u8,
    |v| v as u8,
    |b| b != 0,
    Primitive::Bool
);
impl_persistable_scalar_via!(
    char,
    CharGuard,
    u32,
    |v| v as u32,
    |b| { char::from_u32(b).expect("corrupt persisted char") },
    Primitive::Char
);

// `String` deliberately does *not* implement `Persistable`: it's a foreign,
// `std`-defined type with no room for a persistent `pointer` field of its own,
// so a `store` implementation would have nowhere to cache a previous call's
// content allocation and would leak a fresh one on every call. This absence *is*
// the enforcement mechanism the derive macro relies on: a struct field typed as
// plain `String` simply fails to compile, pointing application authors at
// `kladde_types::PersistableString` instead.

/// The offset arithmetic every composite `store`/`load`/`guard` does. Kept here
/// (rather than open-coded) so the `usize`-to-`Size` conversion has one home.
#[inline]
pub(crate) fn advance<S: Word>(offset: S, by: usize) -> S {
    offset + S::from_usize(by)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kladde_heap::{Backend, MockBackend};

    fn root(
        backend: &MockBackend,
        size: usize,
    ) -> Location<<MockBackend as Backend>::Pointer, u32> {
        let p = backend.alloc_fixed_size(size as u32);
        Location::new(p.raw(), 0)
    }

    #[test]
    fn i32_guard_records_and_mutates() {
        let mut backend = MockBackend::new();
        let location = root(&backend, <i32 as Persistable>::INLINE_SIZE);
        let mut value: i32 = 1;
        let mut guard = value.guard(&backend, location);
        guard.set(42);
        assert_eq!(*guard, 42);
        assert_eq!(value, 42);
        assert_eq!(i32::load(&mut backend, location), 42);
    }

    #[test]
    fn bool_and_char_round_trip() {
        let mut backend = MockBackend::new();
        let bool_location = root(&backend, <bool as Persistable>::INLINE_SIZE);
        let mut b = false;
        b.guard(&backend, bool_location).set(true);
        assert!(bool::load(&mut backend, bool_location));

        let char_location = root(&backend, <char as Persistable>::INLINE_SIZE);
        let mut c = 'a';
        c.guard(&backend, char_location).set('z');
        assert_eq!(char::load(&mut backend, char_location), 'z');
    }
}
