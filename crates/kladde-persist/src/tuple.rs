//! [`Persistable`] impls for tuples of `Persistable` types, `(A, B, ...)` for
//! arities up to 12 (plus the empty tuple `()`), so a tuple can be used anywhere
//! a `Persistable` field/element/value is expected -- e.g. `PersistableVec<(i32,
//! bool)>` or `PersistableHashMap<K, ()>`.
//!
//! A tuple is laid out exactly like a tuple struct with the same fields: its
//! components back to back, no header, `INLINE_SIZE` the sum of theirs. Its
//! schema descriptor is a [`TypeDescriptor::Struct`] with positional field names
//! (`"0"`, `"1"`, ...) -- structurally identical to what
//! `#[derive(Persistable)]` produces for a tuple struct `struct S(A, B)`,
//! differing only in the type *name* (`"Tuple2"` vs. `"S"`), so a tuple and a
//! named tuple struct get distinct (correct) fingerprints.
//!
//! These impls live here (where `Persistable` is defined) rather than in
//! `kladde-types`, for the same orphan-rule reason the scalar impls do.

use kladde_heap::{ReadBackend, Word, WriteBackend};
use kladde_schema::{Field, TypeDescriptor};

use crate::guard::Guard;
use crate::location::Location;
use crate::persistable::Persistable;
use crate::scalar::advance;
use crate::schema::SchemaBuilder;
use kladde_heap::PointerRepr;

/// The [`Guard`] a tuple's [`Persistable`] impl hands out.
///
/// Generic over the whole tuple type `T`; its only blanket mutation is a
/// whole-value [`set`](TupleGuard::set) (like a derived `enum`'s guard). Each
/// arity additionally gets `parts()`, which splits into one guard per component.
pub struct TupleGuard<'s, T, B: WriteBackend> {
    inner: &'s mut T,
    backend: &'s B,
    location: Location<B::Pointer, B::Size>,
}

impl<'s, T: Persistable<B::Pointer>, B: WriteBackend> TupleGuard<'s, T, B> {
    /// Replaces the whole tuple with `value` and persists it.
    pub fn set(&mut self, mut value: T) {
        <T as Persistable<B::Pointer>>::store(&mut value, self.backend, self.location);
        *self.inner = value;
    }
}

impl<'s, T, B: WriteBackend> Guard for TupleGuard<'s, T, B> {
    type Persistable = T;
    type Backend = B;

    fn as_persistable(&self) -> &T {
        self.inner
    }
    fn as_persistable_mut(&mut self) -> &mut T {
        self.inner
    }
    fn backend(&self) -> &B {
        self.backend
    }
}

impl<'s, T, B: WriteBackend> std::ops::Deref for TupleGuard<'s, T, B> {
    type Target = T;
    fn deref(&self) -> &T {
        self.inner
    }
}

impl<'s, T, B: WriteBackend> std::ops::DerefMut for TupleGuard<'s, T, B> {
    fn deref_mut(&mut self) -> &mut T {
        self.inner
    }
}

/// Generates one `impl Persistable<__P> for (T0, T1, ...)`. `$name` is the
/// schema type name; each `$T $idx` pair is a component's type parameter and its
/// tuple index. Offsets are accumulated at runtime (each component stored/loaded
/// at the running `offset`, which then advances by that component's
/// `INLINE_SIZE`), which keeps the macro free of compile-time prefix-sum
/// gymnastics.
macro_rules! impl_persistable_tuple {
    ($name:literal; $($T:ident $idx:tt),*) => {
        // `__P` (not `P`) for the pointer generic, and `__B` for the backend
        // one, so neither can collide with a component type parameter literally
        // named `P` or `B`.
        impl<__P: PointerRepr, $($T: Persistable<__P>,)*> Persistable<__P> for ($($T,)*) {
            const INLINE_SIZE: usize = 0 $( + <$T as Persistable<__P>>::INLINE_SIZE )*;

            type Guard<'s, __B: WriteBackend<Pointer = __P>>
                = TupleGuard<'s, Self, __B>
            where
                Self: 's,
                __B: 's;

            #[inline]
            fn guard<'s, __B: WriteBackend<Pointer = __P>>(
                &'s mut self,
                backend: &'s __B,
                location: Location<__P, __B::Size>,
            ) -> Self::Guard<'s, __B> {
                TupleGuard {
                    inner: self,
                    backend,
                    location,
                }
            }

            #[allow(unused_variables, unused_mut, unused_assignments)]
            fn store<__B: WriteBackend<Pointer = __P>>(
                &mut self,
                backend: &__B,
                location: Location<__P, __B::Size>,
            ) {
                let mut offset = <__B::Size as Word>::zero();
                $(
                    <$T as Persistable<__P>>::store(
                        &mut self.$idx,
                        backend,
                        location + offset,
                    );
                    offset = advance(offset, <$T as Persistable<__P>>::INLINE_SIZE);
                )*
            }

            // `non_snake_case`: each component is bound to a local named after
            // its own (uppercase) type parameter, deliberately.
            // `clippy::unused_unit`: the arity-0 tuple reconstructs as `()`.
            #[allow(
                unused_variables,
                unused_mut,
                unused_assignments,
                non_snake_case,
                clippy::unused_unit
            )]
            fn load<__B: ReadBackend<Pointer = __P>>(
                backend: &mut __B,
                location: Location<__P, __B::Size>,
            ) -> Self {
                let mut offset = <__B::Size as Word>::zero();
                $(
                    // Bind the component to a local named after its own type
                    // parameter (value/type namespaces don't clash).
                    let $T = <$T as Persistable<__P>>::load(backend, location + offset);
                    offset = advance(offset, <$T as Persistable<__P>>::INLINE_SIZE);
                )*
                ($($T,)*)
            }

            #[allow(unused_variables)]
            fn describe_local(builder: &mut SchemaBuilder) -> TypeDescriptor
            where
                Self: 'static,
            {
                TypeDescriptor::Struct {
                    name: ($name).to_string(),
                    fields: ::std::vec![
                        $(
                            Field {
                                name: ::std::stringify!($idx).to_string(),
                                ty: <$T as Persistable<__P>>::describe(builder),
                            },
                        )*
                    ],
                }
            }
        }

        // A per-arity inherent impl (distinct concrete tuple type each time)
        // adding `parts()` alongside the blanket `set`.
        impl<'s, $($T,)* __B: WriteBackend> TupleGuard<'s, ($($T,)*), __B>
        where
            $($T: Persistable<__B::Pointer>,)*
        {
            /// Returns a guard for every component at once, so all components
            /// can be mutated simultaneously (the tuple analog of splitting
            /// `&mut (A, B)` into `&mut x.0` and `&mut x.1`).
            #[inline]
            #[allow(
                unused_variables,
                unused_mut,
                unused_assignments,
                non_snake_case,
                clippy::unused_unit
            )]
            pub fn parts(
                &mut self,
            ) -> ( $(<$T as Persistable<__B::Pointer>>::Guard<'_, __B>,)* ) {
                let mut offset = <__B::Size as Word>::zero();
                $(
                    // Each component guard borrows a disjoint `&mut self.inner.$idx`.
                    let $T = {
                        let __g = <$T as Persistable<__B::Pointer>>::guard(
                            &mut self.inner.$idx,
                            self.backend,
                            self.location + offset,
                        );
                        offset = advance(
                            offset,
                            <$T as Persistable<__B::Pointer>>::INLINE_SIZE,
                        );
                        __g
                    };
                )*
                ( $($T,)* )
            }
        }
    };
}

impl_persistable_tuple!("Tuple0";);
impl_persistable_tuple!("Tuple1"; A 0);
impl_persistable_tuple!("Tuple2"; A 0, B 1);
impl_persistable_tuple!("Tuple3"; A 0, B 1, C 2);
impl_persistable_tuple!("Tuple4"; A 0, B 1, C 2, D 3);
impl_persistable_tuple!("Tuple5"; A 0, B 1, C 2, D 3, E 4);
impl_persistable_tuple!("Tuple6"; A 0, B 1, C 2, D 3, E 4, F 5);
impl_persistable_tuple!("Tuple7"; A 0, B 1, C 2, D 3, E 4, F 5, G 6);
impl_persistable_tuple!("Tuple8"; A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7);
impl_persistable_tuple!("Tuple9"; A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8);
impl_persistable_tuple!("Tuple10"; A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9);
impl_persistable_tuple!("Tuple11"; A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10);
impl_persistable_tuple!("Tuple12"; A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11);

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
    fn pair_round_trips_through_a_backend() {
        let mut b = MockBackend::new();
        let location = root(&b, <(i32, bool) as Persistable>::INLINE_SIZE);
        let mut value = (-7i32, true);
        value.store(&b, location);
        assert_eq!(<(i32, bool)>::load(&mut b, location), (-7, true));
    }

    #[test]
    fn parts_hands_out_one_guard_per_component() {
        let mut b = MockBackend::new();
        let location = root(&b, <(i32, u8) as Persistable>::INLINE_SIZE);
        let mut value = (0i32, 0u8);
        {
            let mut guard = value.guard(&b, location);
            let (mut first, mut second) = guard.parts();
            first.set(9);
            second.set(3);
        }
        assert_eq!(value, (9, 3));
        assert_eq!(<(i32, u8)>::load(&mut b, location), (9, 3));
    }

    #[test]
    fn empty_tuple_occupies_no_inline_bytes() {
        assert_eq!(<() as Persistable>::INLINE_SIZE, 0);
    }
}
