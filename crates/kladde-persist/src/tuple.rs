//! [`Persistable`] for tuples of `Persistable` types, `(A, B, ...)`, up to
//! arity 12 and including the empty tuple `()`, so a tuple can be used
//! wherever a `Persistable` field, element, or value is expected -- e.g.
//! `PersistableVec<(i32, bool)>` or `PersistableHashMap<K, ()>`.
//!
//! A tuple is laid out exactly like a tuple struct with the same fields: its
//! components back to back, no header, `INLINE_SIZE` the sum of theirs. Its
//! schema descriptor is a [`TypeDescriptor::Struct`] with positional field names
//! (`"0"`, `"1"`, ...) -- structurally identical to what `#[derive(Persistable)]`
//! produces for a tuple struct `struct S(A, B)`, differing only in the type
//! *name* (`"Tuple2"` vs. `"S"`).
//!
//! These impls live here, where `Persistable` is defined, for the same
//! orphan-rule reason the scalar impls do.

use kladde_schema::{Field, TypeDescriptor};
use kladde_store::{Error, PointerRepr, ReadBackend, Word, WriteBackend};

use crate::guard::Guard;
use crate::location::Location;
use crate::persistable::{replace, Persistable};
use crate::scalar::advance;
use crate::schema::SchemaBuilder;

/// The [`Guard`] of a tuple: a whole-value [`set`](TupleGuard::set), and
/// [`parts`](TupleGuard::parts) to mutate the components one by one.
///
/// ```
/// use kladde_persist::{Location, Persistable};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let p = store.alloc(5)?;
/// let mut pair = (0u32, false);
/// let mut guard = pair.guard(&store, Location::new(p.raw(), 0));
/// guard.set((1, false))?;
/// let (_, mut flag) = guard.parts();
/// flag.set(true)?;
/// assert_eq!(pair, (1, true));
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub struct TupleGuard<'s, T, B: WriteBackend> {
    inner: &'s mut T,
    backend: &'s B,
    location: Location<B::Pointer, B::Size>,
}

impl<'s, T: Persistable<B::Pointer>, B: WriteBackend> TupleGuard<'s, T, B> {
    /// Replaces the whole tuple with `value`: stores it, then frees what the
    /// old components owned, in one transaction. See [`TupleGuard`] for an
    /// example.
    pub fn set(&mut self, value: T) -> Result<(), Error> {
        replace(self.inner, value, self.backend, self.location)
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

/// Generates one `impl Persistable<__P> for (T0, T1, ...)`. `$name` is the
/// schema type name; each `$T $idx` pair is a component's type parameter and
/// its tuple index. Offsets are accumulated at run time, which keeps the macro
/// free of compile-time prefix sums.
macro_rules! impl_persistable_tuple {
    ($name:literal; $($T:ident $idx:tt),*) => {
        // `__P` and `__B`, so that neither collides with a component type
        // parameter literally named `P` or `B`.
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
            ) -> Result<(), Error> {
                let mut offset = <__B::Size as Word>::zero();
                $(
                    <$T as Persistable<__P>>::store(&mut self.$idx, backend, location + offset)?;
                    offset = advance(offset, <$T as Persistable<__P>>::INLINE_SIZE);
                )*
                Ok(())
            }

            // `non_snake_case`: each component is bound to a local named after
            // its own (uppercase) type parameter, deliberately.
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
            ) -> Result<Self, Error> {
                let mut offset = <__B::Size as Word>::zero();
                $(
                    let $T = <$T as Persistable<__P>>::load(backend, location + offset)?;
                    offset = advance(offset, <$T as Persistable<__P>>::INLINE_SIZE);
                )*
                Ok(($($T,)*))
            }

            #[allow(unused_variables)]
            fn free<__B: WriteBackend<Pointer = __P>>(&mut self, backend: &__B) -> Result<(), Error> {
                $( <$T as Persistable<__P>>::free(&mut self.$idx, backend)?; )*
                Ok(())
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

        // A per-arity inherent impl adding `parts()` alongside the blanket
        // `set`.
        impl<'s, $($T,)* __B: WriteBackend> TupleGuard<'s, ($($T,)*), __B>
        where
            $($T: Persistable<__B::Pointer>,)*
        {
            /// A guard for every component at once, so that all of them can be
            /// mutated simultaneously -- the tuple analog of splitting
            /// `&mut (A, B)` into `&mut x.0` and `&mut x.1`. See [`TupleGuard`]
            /// for an example.
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
                    // Each component guard borrows a disjoint part of the tuple.
                    let $T = {
                        let guard = <$T as Persistable<__B::Pointer>>::guard(
                            &mut self.inner.$idx,
                            self.backend,
                            self.location + offset,
                        );
                        offset = advance(
                            offset,
                            <$T as Persistable<__B::Pointer>>::INLINE_SIZE,
                        );
                        guard
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
    use kladde_store::{MemoryStorage, Pointer, Store};

    fn setup(size: usize) -> (Store, Location<Pointer, u32>) {
        let store = Store::create(Box::new(MemoryStorage::new()), Default::default()).unwrap();
        let p = store.alloc(size as u32).unwrap();
        (store, Location::new(p.raw(), 0))
    }

    #[test]
    fn pair_round_trips_through_a_backend() {
        let (mut store, location) = setup(<(i32, bool) as Persistable>::INLINE_SIZE);
        let mut value = (-7i32, true);
        value.store(&store, location).unwrap();
        store.flush().unwrap();
        assert_eq!(
            <(i32, bool)>::load(&mut store, location).unwrap(),
            (-7, true)
        );
    }

    #[test]
    fn parts_hands_out_one_guard_per_component() {
        let (mut store, location) = setup(<(i32, u8) as Persistable>::INLINE_SIZE);
        let mut value = (0i32, 0u8);
        {
            let mut guard = value.guard(&store, location);
            let (mut first, mut second) = guard.parts();
            first.set(9).unwrap();
            second.set(3).unwrap();
        }
        assert_eq!(value, (9, 3));
        store.flush().unwrap();
        assert_eq!(<(i32, u8)>::load(&mut store, location).unwrap(), (9, 3));
    }

    #[test]
    fn empty_tuple_occupies_no_inline_bytes() {
        assert_eq!(<() as Persistable>::INLINE_SIZE, 0);
    }
}
