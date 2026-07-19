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

use crate::{Backend, Guard, Persistable};

macro_rules! impl_persistable_scalar {
    ($ty:ty, $guard:ident, $op:ident) => {
        #[doc = concat!("The `Op` recorded by [`", stringify!($guard), "::set`].")]
        #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
        pub enum $op {
            Set($ty),
        }

        #[doc = concat!("The `Guard` for `", stringify!($ty), "`.")]
        pub struct $guard<'s, B> {
            inner: &'s mut $ty,
            backend: &'s B,
        }

        impl<'s, B: Backend> $guard<'s, B> {
            /// Replaces the value, recording a single `Set` op.
            pub fn set(&mut self, value: $ty) {
                self.backend.record::<$ty>(&$op::Set(value.clone()));
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
            type Op = $op;
            type Guard<'s, B: Backend>
                = $guard<'s, B>
            where
                Self: 's,
                B: 's;

            fn guard<'s, B: Backend>(&'s mut self, backend: &'s B) -> Self::Guard<'s, B> {
                $guard {
                    inner: self,
                    backend,
                }
            }
        }
    };
}

impl_persistable_scalar!(bool, BoolGuard, BoolOp);
impl_persistable_scalar!(char, CharGuard, CharOp);
impl_persistable_scalar!(i8, I8Guard, I8Op);
impl_persistable_scalar!(i16, I16Guard, I16Op);
impl_persistable_scalar!(i32, I32Guard, I32Op);
impl_persistable_scalar!(i64, I64Guard, I64Op);
impl_persistable_scalar!(u8, U8Guard, U8Op);
impl_persistable_scalar!(u16, U16Guard, U16Op);
impl_persistable_scalar!(u32, U32Guard, U32Op);
impl_persistable_scalar!(u64, U64Guard, U64Op);
impl_persistable_scalar!(f32, F32Guard, F32Op);
impl_persistable_scalar!(f64, F64Guard, F64Op);
impl_persistable_scalar!(String, StringGuard, StringOp);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Allocator, Journal, ResolvedPointer, UniquePointer};
    use std::cell::RefCell;

    #[derive(Default)]
    struct MockBackend {
        recorded: RefCell<usize>,
    }

    impl Journal for MockBackend {
        fn record<T: Persistable>(&self, _op: &T::Op) {
            *self.recorded.borrow_mut() += 1;
        }
    }

    impl Allocator for MockBackend {
        fn alloc<T>(&self, _size: usize) -> UniquePointer<T> {
            unimplemented!()
        }
        fn free<T: Persistable>(&self, _pointer: UniquePointer<T>) {}
        fn resolve<'a, T>(&'a self, _pointer: &UniquePointer<T>) -> Option<ResolvedPointer<'a, T>> {
            None
        }
    }

    #[test]
    fn i32_guard_records_and_mutates() {
        let backend = MockBackend::default();
        let mut value: i32 = 1;
        let mut guard = value.guard(&backend);
        guard.set(42);
        assert_eq!(*guard, 42);
        assert_eq!(*backend.recorded.borrow(), 1);
        assert_eq!(value, 42);
    }

    #[test]
    fn string_guard_records_and_mutates() {
        let backend = MockBackend::default();
        let mut value = String::from("hello");
        let mut guard = value.guard(&backend);
        guard.set(String::from("world"));
        assert_eq!(*guard, "world");
        assert_eq!(value, "world");
    }
}
