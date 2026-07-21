//! Blanket [`Persistable`] impls for primitive/scalar types, so
//! `#[derive(Persistable)]`-generated code (see `kladde-derive`) can
//! treat every struct field uniformly -- a `_mut()` accessor returning a
//! nested `Guard` -- rather than special-casing "leaf" types with direct
//! setters. See `spec.md`'s "The Trait Layer" (Guards and the Backend)
//! for the rationale, and `V1_QUESTIONS.md` question 7 for the decision.
//!
//! These live here, in the crate that defines `Persistable`, rather than
//! in `kladde-types` (where `spec.md` originally described them) --
//! `impl Persistable for i32` is `impl ForeignTrait for ForeignType` from
//! `kladde-types`' point of view, which the orphan rules forbid. From
//! `kladde-traits`' point of view the trait is local, so it's allowed.
//! `kladde-types` re-exports these names for convenience.

use crate::{read_header, write_header, Backend, Guard, Location, Persistable, RawPointer};

/// Numeric scalars all have native `to_le_bytes`/`from_le_bytes` with a
/// fixed-width array, so one macro covers them; `bool`/`char`/`String`
/// don't fit that shape and are implemented by hand below.
macro_rules! impl_persistable_numeric_scalar {
    ($ty:ty, $guard:ident) => {
        #[doc = concat!("The `Guard` for `", stringify!($ty), "`.")]
        pub struct $guard<'s, B> {
            inner: &'s mut $ty,
            backend: &'s B,
            location: Location,
        }

        impl<'s, B: Backend> $guard<'s, B> {
            /// Replaces the value, writing its inline bytes at this
            /// guard's location.
            pub fn set(&mut self, value: $ty) {
                Persistable::store(&value, self.backend, self.location);
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

            fn store<B: Backend>(&self, backend: &B, location: Location) {
                backend.write(location.anchor, location.offset, &self.to_le_bytes());
            }

            fn load<B: Backend>(backend: &B, location: Location) -> Self {
                let bytes =
                    backend.read(location.anchor, location.offset, Self::INLINE_SIZE as u32);
                Self::from_le_bytes(bytes.try_into().unwrap())
            }
        }
    };
}

impl_persistable_numeric_scalar!(i8, I8Guard);
impl_persistable_numeric_scalar!(i16, I16Guard);
impl_persistable_numeric_scalar!(i32, I32Guard);
impl_persistable_numeric_scalar!(i64, I64Guard);
impl_persistable_numeric_scalar!(u8, U8Guard);
impl_persistable_numeric_scalar!(u16, U16Guard);
impl_persistable_numeric_scalar!(u32, U32Guard);
impl_persistable_numeric_scalar!(u64, U64Guard);
impl_persistable_numeric_scalar!(f32, F32Guard);
impl_persistable_numeric_scalar!(f64, F64Guard);

/// `bool` and `char` don't have `to_le_bytes`/`from_le_bytes`, so they're
/// encoded by hand (as one byte, and as `u32`, respectively) rather than
/// going through the numeric macro above.
macro_rules! impl_persistable_scalar_via {
    ($ty:ty, $guard:ident, $repr:ty, $to_repr:expr, $from_repr:expr) => {
        #[doc = concat!("The `Guard` for `", stringify!($ty), "`.")]
        pub struct $guard<'s, B> {
            inner: &'s mut $ty,
            backend: &'s B,
            location: Location,
        }

        impl<'s, B: Backend> $guard<'s, B> {
            pub fn set(&mut self, value: $ty) {
                Persistable::store(&value, self.backend, self.location);
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

            fn store<B: Backend>(&self, backend: &B, location: Location) {
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
        }
    };
}

impl_persistable_scalar_via!(bool, BoolGuard, u8, |v| v as u8, |b| b != 0);
impl_persistable_scalar_via!(char, CharGuard, u32, |v| v as u32, |b| {
    char::from_u32(b).expect("corrupt persisted char")
});

/// The `Guard` for [`String`]. Hand-written rather than going through
/// either macro above -- unlike the fixed-size scalars, `String` is
/// variably sized, so it needs its own content allocation (see below),
/// not just a `write` of a few fixed bytes.
pub struct StringGuard<'s, B> {
    inner: &'s mut String,
    backend: &'s B,
    location: Location,
}

impl<'s, B: Backend> StringGuard<'s, B> {
    pub fn set(&mut self, value: String) {
        value.store(self.backend, self.location);
        *self.inner = value;
    }
}

impl<'s, B: Backend> Guard for StringGuard<'s, B> {
    type Persistable = String;
    type Backend = B;

    fn as_persistable(&self) -> &String {
        self.inner
    }
    fn as_persistable_mut(&mut self) -> &mut String {
        self.inner
    }
    fn backend(&self) -> &B {
        self.backend
    }
}

impl<'s, B> ::std::ops::Deref for StringGuard<'s, B> {
    type Target = String;
    fn deref(&self) -> &String {
        self.inner
    }
}

impl<'s, B> ::std::ops::DerefMut for StringGuard<'s, B> {
    fn deref_mut(&mut self) -> &mut String {
        self.inner
    }
}

impl Persistable for String {
    /// A fixed 8-byte `{ target, len }` header -- see [`write_header`].
    const INLINE_SIZE: usize = 8;

    type Guard<'s, B: Backend>
        = StringGuard<'s, B>
    where
        Self: 's,
        B: 's;

    fn guard<'s, B: Backend>(
        &'s mut self,
        backend: &'s B,
        location: Location,
    ) -> Self::Guard<'s, B> {
        StringGuard {
            inner: self,
            backend,
            location,
        }
    }

    /// Note on an interim limitation (see `spec.md`): unlike
    /// `PersistedVec`/`PersistedHashMap`, a plain `String` has no room
    /// for a persistent `pointer` field of its own (it's a foreign,
    /// `std`-defined type), so there's nowhere in memory to cache the
    /// index of a previous call's content allocation. Every `store` call
    /// therefore allocates a *fresh* region rather than resizing an
    /// existing one, leaving any previous allocation at this `location`
    /// unreferenced (never freed). Harmless for the in-memory mock (it
    /// only ever lives for one process), but a real, file-backed
    /// `Allocator` would leak space this way -- revisit if/when a
    /// `String`-like wrapper type with its own `pointer` field (the same
    /// shape as `PersistedVec`) is introduced.
    fn store<B: Backend>(&self, backend: &B, location: Location) {
        let bytes = self.as_bytes();
        let pointer = backend.alloc::<String>(bytes.len());
        backend.write(pointer.raw(), 0, bytes);
        write_header(backend, location, pointer.index(), bytes.len() as u32);
    }

    fn load<B: Backend>(backend: &B, location: Location) -> Self {
        let (target, len) = read_header(backend, location);
        match target {
            None => String::new(),
            Some(target) => {
                let bytes = backend.read(RawPointer::from_index(target), 0, len);
                String::from_utf8(bytes).expect("corrupt UTF-8 in persisted String")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Allocator, ResolvedPointer, UniquePointer};
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
        fn resize<T>(&self, pointer: &UniquePointer<T>, new_size: usize) {
            let mut regions = self.regions.borrow_mut();
            let region = regions.get_mut(&pointer.index()).unwrap();
            region.resize(new_size, 0);
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

    #[test]
    fn string_guard_records_and_mutates() {
        let backend = MockBackend::default();
        let location = root_location(&backend, String::INLINE_SIZE);
        let mut value = String::from("hello");
        let mut guard = value.guard(&backend, location);
        guard.set(String::from("a longer string than before"));
        assert_eq!(*guard, "a longer string than before");
        assert_eq!(value, "a longer string than before");
        assert_eq!(
            String::load(&backend, location),
            "a longer string than before"
        );
    }

    #[test]
    fn string_loads_as_empty_before_ever_being_set() {
        let backend = MockBackend::default();
        let location = root_location(&backend, String::INLINE_SIZE);
        // Nothing has been written here at all yet (header bytes are
        // still zeroed) -- `load` should see "no allocation" and produce
        // an empty string rather than panicking.
        assert_eq!(String::load(&backend, location), "");
    }
}
