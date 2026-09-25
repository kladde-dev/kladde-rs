//! [`Persistable`] for the primitive types, so that `#[derive(Persistable)]`
//! can treat every field uniformly -- a `_mut()` accessor returning a nested
//! [`Guard`] -- rather than special-casing leaves.
//!
//! These live here, in the crate that defines `Persistable`: `impl Persistable
//! for i32` is `impl ForeignTrait for ForeignType` from anywhere else, which
//! the orphan rules forbid.
//!
//! Every scalar is pointer-free, so each implements `Persistable<P>` for **all**
//! `P: PointerRepr`. The guard types are generic over the backend alone and read
//! the pointer type back off it as `B::Pointer`.

use kladde_schema::{Primitive, TypeDescriptor};
use kladde_store::{Error, PointerRepr, ReadBackend, Word, WriteBackend};
use std::io::Read;

use crate::guard::Guard;
use crate::location::Location;
use crate::persistable::Persistable;
use crate::schema::SchemaBuilder;

/// Reads exactly `N` bytes at `location` -- the read half of every scalar.
fn read_bytes<const N: usize, B: ReadBackend>(
    backend: &mut B,
    location: Location<B::Pointer, B::Size>,
) -> Result<[u8; N], Error> {
    let mut buf = [0u8; N];
    backend
        .read_at(location.anchor, location.offset)?
        .read_exact(&mut buf)?;
    Ok(buf)
}

/// A scalar's guard: a whole-value `set`, and nothing else to mutate.
macro_rules! scalar_guard {
    ($ty:ty, $guard:ident, $sample:expr) => {
        #[doc = concat!("The [`Guard`] of `", stringify!($ty), "`: replaces the value with [`set`](", stringify!($guard), "::set).")]
        ///
        /// ```
        /// use kladde_persist::{Location, Persistable};
        /// use kladde_store::{MemoryStorage, Store, WriteBackend};
        ///
        /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
        #[doc = concat!("let p = store.alloc(<", stringify!($ty), " as Persistable>::INLINE_SIZE as u32)?;")]
        #[doc = concat!("let mut value: ", stringify!($ty), " = Default::default();")]
        #[doc = concat!("value.guard(&store, Location::new(p.raw(), 0)).set(", stringify!($sample), ")?;")]
        #[doc = concat!("assert_eq!(value, ", stringify!($sample), ");")]
        /// # Ok::<(), kladde_store::Error>(())
        /// ```
        pub struct $guard<'s, B: WriteBackend> {
            inner: &'s mut $ty,
            backend: &'s B,
            location: Location<B::Pointer, B::Size>,
        }

        impl<'s, B: WriteBackend> $guard<'s, B> {
            /// Replaces the value, writing its inline bytes at this guard's
            /// location. A scalar is one write, so the change is atomic.
            #[doc = concat!("See [`", stringify!($guard), "`] for an example.")]
            pub fn set(&mut self, mut value: $ty) -> Result<(), Error> {
                <$ty as Persistable<B::Pointer>>::store(&mut value, self.backend, self.location)?;
                *self.inner = value;
                Ok(())
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
    };
}

/// `Persistable` for the numeric scalars, which have `to_le_bytes` and
/// `from_le_bytes` of a fixed width.
macro_rules! numeric_scalar {
    ($ty:ty, $guard:ident, $code:expr, $sample:expr) => {
        scalar_guard!($ty, $guard, $sample);

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
            ) -> Result<(), Error> {
                backend.write(location.anchor, location.offset, &self.to_le_bytes())
            }

            fn load<B: ReadBackend<Pointer = P>>(
                backend: &mut B,
                location: Location<P, B::Size>,
            ) -> Result<Self, Error> {
                Ok(Self::from_le_bytes(read_bytes::<
                    { ::std::mem::size_of::<$ty>() },
                    B,
                >(backend, location)?))
            }

            fn describe_local(_builder: &mut SchemaBuilder) -> TypeDescriptor {
                TypeDescriptor::Primitive($code)
            }
        }
    };
}

numeric_scalar!(u8, U8Guard, Primitive::U8, 7);
numeric_scalar!(u16, U16Guard, Primitive::U16, 7);
numeric_scalar!(u32, U32Guard, Primitive::U32, 7);
numeric_scalar!(u64, U64Guard, Primitive::U64, 7);
numeric_scalar!(i8, I8Guard, Primitive::I8, -7);
numeric_scalar!(i16, I16Guard, Primitive::I16, -7);
numeric_scalar!(i32, I32Guard, Primitive::I32, -7);
numeric_scalar!(i64, I64Guard, Primitive::I64, -7);
numeric_scalar!(f32, F32Guard, Primitive::F32, 1.5);
numeric_scalar!(f64, F64Guard, Primitive::F64, 1.5);

/// `Persistable` for a scalar stored through another representation: `bool`
/// as one byte, `char` as a `u32`.
macro_rules! scalar_via {
    ($ty:ty, $guard:ident, $repr:ty, $to_repr:expr, $from_repr:expr, $code:expr, $sample:expr) => {
        scalar_guard!($ty, $guard, $sample);

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
            ) -> Result<(), Error> {
                let to_repr: fn($ty) -> $repr = $to_repr;
                backend.write(
                    location.anchor,
                    location.offset,
                    &to_repr(*self).to_le_bytes(),
                )
            }

            fn load<B: ReadBackend<Pointer = P>>(
                backend: &mut B,
                location: Location<P, B::Size>,
            ) -> Result<Self, Error> {
                let from_repr: fn($repr) -> Result<$ty, Error> = $from_repr;
                let bytes = read_bytes::<{ ::std::mem::size_of::<$repr>() }, B>(backend, location)?;
                from_repr(<$repr>::from_le_bytes(bytes))
            }

            fn describe_local(_builder: &mut SchemaBuilder) -> TypeDescriptor {
                TypeDescriptor::Primitive($code)
            }
        }
    };
}

scalar_via!(
    bool,
    BoolGuard,
    u8,
    |v| v as u8,
    |b| Ok(b != 0),
    Primitive::Bool,
    true
);
scalar_via!(
    char,
    CharGuard,
    u32,
    |v| v as u32,
    |b| char::from_u32(b).ok_or_else(|| Error::Corrupt(format!("{b:#x} is not a char"))),
    Primitive::Char,
    'k'
);

// `String` deliberately does *not* implement `Persistable`: it is a foreign,
// `std`-defined type with no room for a pointer to its own content allocation,
// so a `store` would have nowhere to remember an earlier call's allocation and
// would leak a fresh one on every call. This absence *is* the enforcement
// mechanism the derive macro relies on: a struct field typed as plain `String`
// fails to compile, pointing application authors at
// `kladde_types::PersistableString` instead.

/// The offset arithmetic every composite `store`/`load`/`guard` does, kept in
/// one place so the `usize`-to-`Size` conversion has one home.
#[inline]
pub(crate) fn advance<S: Word>(offset: S, by: usize) -> S {
    offset + S::from_usize(by)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kladde_store::{MemoryStorage, Pointer, Store};

    fn setup(size: usize) -> (Store, Location<Pointer, u32>) {
        let store = Store::create(Box::new(MemoryStorage::new()), Default::default()).unwrap();
        let p = store.alloc(size as u32).unwrap();
        (store, Location::new(p.raw(), 0))
    }

    #[test]
    fn i32_guard_records_and_mutates() {
        let (mut store, location) = setup(4);
        let mut value: i32 = 1;
        let mut guard = value.guard(&store, location);
        guard.set(42).unwrap();
        assert_eq!(*guard, 42);
        assert_eq!(value, 42);
        store.flush().unwrap();
        assert_eq!(i32::load(&mut store, location).unwrap(), 42);
    }

    #[test]
    fn bool_and_char_round_trip() {
        let (mut store, location) = setup(8);
        let mut b = false;
        b.guard(&store, location).set(true).unwrap();
        let mut c = 'a';
        c.guard(&store, location + 4).set('z').unwrap();
        store.flush().unwrap();
        assert!(bool::load(&mut store, location).unwrap());
        assert_eq!(char::load(&mut store, location + 4).unwrap(), 'z');
    }

    #[test]
    fn an_invalid_char_is_corruption_not_a_panic() {
        let (mut store, location) = setup(4);
        store
            .write(location.anchor, 0, &0xD800u32.to_le_bytes())
            .unwrap();
        store.flush().unwrap();
        assert!(matches!(
            char::load(&mut store, location),
            Err(Error::Corrupt(_))
        ));
    }
}
