//! Blanket [`Persistable`] impls for primitive/scalar types, so
//! `#[derive(Persistable)]`-generated code (see `kladde-derive`) can
//! treat every struct field uniformly -- a `_mut()` accessor returning a
//! nested `Guard` -- rather than special-casing "leaf" types with direct
//! setters. See `spec.md`'s "The Trait Layer" (Guards and the Backend)
//! for the rationale.
//!
//! These live here, in the crate that defines `Persistable`, rather than
//! in `kladde-types` (where `spec.md` originally described them) --
//! `impl Persistable for i32` is `impl ForeignTrait for ForeignType` from
//! `kladde-types`' point of view, which the orphan rules forbid. From
//! `kladde-traits`' point of view the trait is local, so it's allowed.
//! `kladde-types` re-exports these names for convenience.

use crate::{Backend, Guard, Location, Persistable, SchemaBuilder};
use kladde_schema::{Primitive, TypeDescriptor};

/// Numeric scalars all have native `to_le_bytes`/`from_le_bytes` with a
/// fixed-width array, so one macro covers them; `bool`/`char` don't fit
/// that shape and are implemented by hand below. `String` doesn't
/// implement `Persistable` at all -- see the note further down.
macro_rules! impl_persistable_numeric_scalar {
    ($ty:ty, $guard:ident, $code:expr) => {
        #[doc = concat!("The `Guard` for `", stringify!($ty), "`.")]
        pub struct $guard<'s, B> {
            inner: &'s mut $ty,
            backend: &'s B,
            location: Location,
        }

        impl<'s, B: Backend> $guard<'s, B> {
            /// Replaces the value, writing its inline bytes at this
            /// guard's location.
            pub fn set(&mut self, mut value: $ty) {
                Persistable::store(&mut value, self.backend, self.location);
                *self.inner = value;
            }
        }

        impl<'s, B: Backend> Guard for $guard<'s, B> {
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

        impl<'s, B> ::std::ops::Deref for $guard<'s, B> {
            type Target = $ty;
            fn deref(&self) -> &$ty {
                self.inner
            }
        }

        impl<'s, B> ::std::ops::DerefMut for $guard<'s, B> {
            fn deref_mut(&mut self) -> &mut $ty {
                self.inner
            }
        }

        impl Persistable for $ty {
            const INLINE_SIZE: usize = ::std::mem::size_of::<$ty>();

            type Guard<'s, B: Backend>
                = $guard<'s, B>
            where
                Self: 's,
                B: 's;

            fn guard<'s, B: Backend>(
                &'s mut self,
                backend: &'s B,
                location: Location,
            ) -> Self::Guard<'s, B> {
                $guard {
                    inner: self,
                    backend,
                    location,
                }
            }

            fn store<B: Backend>(&mut self, backend: &B, location: Location) {
                backend.write(location.anchor, location.offset, &self.to_le_bytes());
            }

            fn load<B: Backend>(backend: &B, location: Location) -> Self {
                let bytes =
                    backend.read(location.anchor, location.offset, Self::INLINE_SIZE as u32);
                Self::from_le_bytes(bytes.try_into().unwrap())
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
/// encoded by hand (as one byte, and as `u32`, respectively) rather than
/// going through the numeric macro above.
macro_rules! impl_persistable_scalar_via {
    ($ty:ty, $guard:ident, $repr:ty, $to_repr:expr, $from_repr:expr, $code:expr) => {
        #[doc = concat!("The `Guard` for `", stringify!($ty), "`.")]
        pub struct $guard<'s, B> {
            inner: &'s mut $ty,
            backend: &'s B,
            location: Location,
        }

        impl<'s, B: Backend> $guard<'s, B> {
            pub fn set(&mut self, mut value: $ty) {
                Persistable::store(&mut value, self.backend, self.location);
                *self.inner = value;
            }
        }

        impl<'s, B: Backend> Guard for $guard<'s, B> {
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

        impl<'s, B> ::std::ops::Deref for $guard<'s, B> {
            type Target = $ty;
            fn deref(&self) -> &$ty {
                self.inner
            }
        }

        impl<'s, B> ::std::ops::DerefMut for $guard<'s, B> {
            fn deref_mut(&mut self) -> &mut $ty {
                self.inner
            }
        }

        impl Persistable for $ty {
            const INLINE_SIZE: usize = ::std::mem::size_of::<$repr>();

            type Guard<'s, B: Backend>
                = $guard<'s, B>
            where
                Self: 's,
                B: 's;

            fn guard<'s, B: Backend>(
                &'s mut self,
                backend: &'s B,
                location: Location,
            ) -> Self::Guard<'s, B> {
                $guard {
                    inner: self,
                    backend,
                    location,
                }
            }

            fn store<B: Backend>(&mut self, backend: &B, location: Location) {
                let to_repr: fn($ty) -> $repr = $to_repr;
                let repr = to_repr(*self);
                backend.write(location.anchor, location.offset, &repr.to_le_bytes());
            }

            fn load<B: Backend>(backend: &B, location: Location) -> Self {
                let from_repr: fn($repr) -> $ty = $from_repr;
                let bytes =
                    backend.read(location.anchor, location.offset, Self::INLINE_SIZE as u32);
                let repr = <$repr>::from_le_bytes(bytes.try_into().unwrap());
                from_repr(repr)
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

// `String` deliberately does *not* implement `Persistable`: it's a
// foreign, `std`-defined type with no room for a persistent `pointer`
// field of its own, so a `store` implementation would have nowhere to
// cache a previous call's content allocation and would leak a fresh one
// on every call (a real, file-backed `Allocator` would leak space this
// way, unlike the in-memory mock). This absence *is* the enforcement
// mechanism the derive macro relies on (see `spec.md`): a struct field
// typed as plain `String` simply fails to compile, pointing application
// authors at `kladde_types::PersistableString` -- a wrapper with its own
// `pointer` field, the same shape `PersistableVec` already has -- instead.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Allocator, RawPointer, ResolvedPointer, UniqueArrayPointer, UniquePointer};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::num::NonZeroU32;

    #[derive(Default)]
    struct MockBackend {
        regions: RefCell<HashMap<NonZeroU32, Vec<u8>>>,
        next_index: std::cell::Cell<u32>,
    }

    impl Allocator for MockBackend {
        fn alloc<T>(&self, size: usize) -> UniquePointer<T> {
            let raw = self.next_index.get() + 1;
            self.next_index.set(raw);
            let index = NonZeroU32::new(raw).unwrap();
            self.regions.borrow_mut().insert(index, vec![0u8; size]);
            UniquePointer::from_index(index)
        }
        fn free<T>(&self, pointer: UniquePointer<T>) {
            self.regions.borrow_mut().remove(&pointer.index());
        }
        fn alloc_array<T>(&self, byte_size: usize) -> UniqueArrayPointer<T> {
            let raw = self.next_index.get() + 1;
            self.next_index.set(raw);
            let index = NonZeroU32::new(raw).unwrap();
            self.regions
                .borrow_mut()
                .insert(index, vec![0u8; byte_size]);
            UniqueArrayPointer::from_index(index)
        }
        fn free_array<T>(&self, pointer: UniqueArrayPointer<T>) {
            self.regions.borrow_mut().remove(&pointer.index());
        }
        fn resolve<'a, T>(&'a self, pointer: &UniquePointer<T>) -> Option<ResolvedPointer<'a, T>> {
            self.regions
                .borrow()
                .contains_key(&pointer.index())
                .then(|| ResolvedPointer::from_target(pointer.index()))
        }
        fn read(&self, target: RawPointer, offset: u32, len: u32) -> Vec<u8> {
            let regions = self.regions.borrow();
            let region = &regions[&target.index()];
            region[offset as usize..(offset + len) as usize].to_vec()
        }
        fn write(&self, target: RawPointer, offset: u32, bytes: &[u8]) {
            let mut regions = self.regions.borrow_mut();
            let region = regions.get_mut(&target.index()).unwrap();
            let start = offset as usize;
            if region.len() < start + bytes.len() {
                region.resize(start + bytes.len(), 0);
            }
            region[start..start + bytes.len()].copy_from_slice(bytes);
        }
        fn copy(
            &self,
            src: RawPointer,
            src_offset: u32,
            len: u32,
            dst: RawPointer,
            dst_offset: u32,
        ) {
            let bytes = self.read(src, src_offset, len);
            self.write(dst, dst_offset, &bytes);
        }
        fn resize_array<T>(&self, pointer: &UniqueArrayPointer<T>, new_byte_size: usize) {
            let mut regions = self.regions.borrow_mut();
            let region = regions.get_mut(&pointer.index()).unwrap();
            region.resize(new_byte_size, 0);
        }
        fn array_capacity<T>(&self, pointer: &UniqueArrayPointer<T>) -> Option<usize> {
            self.regions.borrow().get(&pointer.index()).map(Vec::len)
        }
    }

    fn root_location(backend: &MockBackend, size: usize) -> Location {
        let pointer = backend.alloc::<()>(size);
        Location {
            anchor: pointer.raw(),
            offset: 0,
        }
    }

    #[test]
    fn i32_guard_records_and_mutates() {
        let backend = MockBackend::default();
        let location = root_location(&backend, i32::INLINE_SIZE);
        let mut value: i32 = 1;
        let mut guard = value.guard(&backend, location);
        guard.set(42);
        assert_eq!(*guard, 42);
        assert_eq!(value, 42);
        assert_eq!(i32::load(&backend, location), 42);
    }

    #[test]
    fn bool_and_char_round_trip() {
        let backend = MockBackend::default();
        let bool_location = root_location(&backend, bool::INLINE_SIZE);
        let mut b = false;
        b.guard(&backend, bool_location).set(true);
        assert!(bool::load(&backend, bool_location));

        let char_location = root_location(&backend, char::INLINE_SIZE);
        let mut c = 'a';
        c.guard(&backend, char_location).set('z');
        assert_eq!(char::load(&backend, char_location), 'z');
    }
}
