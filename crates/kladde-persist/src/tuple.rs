//! [`Persistable`] for tuples of `Persistable` types, `(A, B, ...)`, up to
//! arity 12 and including the empty tuple `()`, so a tuple can be used
//! wherever a `Persistable` field, element, or value is expected -- e.g.
//! `PersistableVec<(i32, bool)>` or `PersistableHashMap<K, ()>`.
//!
//! A tuple is laid out exactly like a tuple struct with the same fields: its
//! components back to back, each in the tuple's own encoding, with no header.
//! Its schema descriptor is a [`TypeDescriptor::Struct`] with positional field
//! names (`"0"`, `"1"`, ...) -- structurally identical to what
//! `#[derive(Persistable)]` produces for a tuple struct `struct S(A, B)`,
//! differing only in the type *name* (`"Tuple2"` vs. `"S"`).
//!
//! These impls live here, where `Persistable` is defined, for the same
//! orphan-rule reason the scalar impls do.

use kladde_schema::{Field, TypeDescriptor};
use kladde_store::{Error, PointerRepr, ReadBackend, WriteBackend};

use crate::encoding::{Encoding, Slotted};
use crate::guard::Guard;
use crate::input::Input;
use crate::persistable::{replace, slot_size, Persistable, Slottable};
use crate::place::{FieldOffsets, Place};
use crate::schema::SchemaBuilder;
use crate::sizes::sum_sizes;

/// One more than the largest arity: the [`FieldOffsets`] a tuple guard keeps.
const MAX_FIELDS: usize = 13;

/// The [`Guard`] of a tuple: a whole-value [`set`](TupleGuard::set), and
/// [`parts`](TupleGuard::parts) to mutate the components one by one.
///
/// ```
/// use kladde_persist::{Location, Persistable, Slotted};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let p = store.alloc(5)?;
/// let mut pair = (0u32, false);
/// let mut guard = pair.guard(&store, Slotted::at(Location::new(p.raw(), 0)));
/// guard.set((1, false))?;
/// let (_, mut flag) = guard.parts();
/// flag.set(true)?;
/// assert_eq!(pair, (1, true));
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub struct TupleGuard<'s, T, B: WriteBackend, E: Encoding = Slotted> {
    inner: &'s mut T,
    backend: &'s B,
    place: Place<'s, B, E>,
    fields: FieldOffsets<MAX_FIELDS>,
}

impl<'s, T, B: WriteBackend, E: Encoding> Guard for TupleGuard<'s, T, B, E> {
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

impl<'s, T, B: WriteBackend, E: Encoding> std::ops::Deref for TupleGuard<'s, T, B, E> {
    type Target = T;
    fn deref(&self) -> &T {
        self.inner
    }
}

/// The [`Encoding::Join`] of `$acc` and every following encoding type.
macro_rules! join_encodings {
    ($acc:ty;) => { $acc };
    ($acc:ty; $first:ty $(, $rest:ty)*) => {
        join_encodings!(<$acc as Encoding>::Join<$first>; $($rest),*)
    };
}

/// Generates one `impl Persistable<__P> for (T0, T1, ...)`. `$name` is the
/// schema type name; each `$T $idx` pair is a component's type parameter and
/// its tuple index.
macro_rules! impl_persistable_tuple {
    ($name:literal; $($T:ident $idx:tt),*) => {
        // `__P`, `__B` and `__E`, so that none collides with a component type
        // parameter literally named `P`, `B` or `E`.
        impl<__P: PointerRepr, $($T: Slottable<__P>,)*> Slottable<__P> for ($($T,)*) {}

        impl<__P: PointerRepr, $($T: Persistable<__P>,)*> Persistable<__P> for ($($T,)*) {
            const SLOTTED_SIZE: Option<usize> =
                sum_sizes(&[$(<$T as Persistable<__P>>::SLOTTED_SIZE,)*]);
            const PACKED_SIZE: Option<usize> =
                sum_sizes(&[$(<$T as Persistable<__P>>::PACKED_SIZE,)*]);

            type RootEncoding = join_encodings!(Slotted; $(<$T as Persistable<__P>>::RootEncoding),*);

            type Guard<'s, __B: WriteBackend<Pointer = __P>, __E: Encoding>
                = TupleGuard<'s, Self, __B, __E>
            where
                Self: 's,
                __B: 's;

            #[inline]
            fn guard<'s, __B: WriteBackend<Pointer = __P>, __E: Encoding>(
                &'s mut self,
                backend: &'s __B,
                place: Place<'s, __B, __E>,
            ) -> Self::Guard<'s, __B, __E> {
                let guard = TupleGuard {
                    inner: self,
                    backend,
                    place,
                    fields: FieldOffsets::new(),
                };
                guard.refresh();
                guard
            }

            #[allow(unused_mut)]
            fn encoded_size<__E: Encoding>(&self) -> usize {
                if !__E::PACKED {
                    return slot_size::<Self, __P>();
                }
                let mut size = 0usize;
                $( size += <$T as Persistable<__P>>::encoded_size::<__E>(&self.$idx); )*
                size
            }

            #[allow(unused_variables)]
            fn encode<__E: Encoding>(&self, out: &mut Vec<u8>) {
                $( <$T as Persistable<__P>>::encode::<__E>(&self.$idx, out); )*
            }

            // `non_snake_case`: each component is bound to a local named after
            // its own (uppercase) type parameter, deliberately.
            #[allow(unused_variables, non_snake_case, clippy::unused_unit)]
            fn decode<__B: ReadBackend<Pointer = __P>, __E: Encoding>(
                backend: &mut __B,
                input: &mut Input<'_>,
            ) -> Result<Self, Error> {
                $( let $T = <$T as Persistable<__P>>::decode::<__B, __E>(backend, input)?; )*
                Ok(($($T,)*))
            }

            #[allow(unused_variables)]
            fn prepare<__B: WriteBackend<Pointer = __P>>(&mut self, backend: &__B) -> Result<(), Error> {
                $( <$T as Persistable<__P>>::prepare(&mut self.$idx, backend)?; )*
                Ok(())
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

        // A per-arity inherent impl: what needs the component types.
        impl<'s, $($T,)* __B: WriteBackend, __E: Encoding> TupleGuard<'s, ($($T,)*), __B, __E>
        where
            $($T: Persistable<__B::Pointer>,)*
        {
            /// Replaces the whole tuple with `value`: stores it, then frees
            /// what the old components owned, in one transaction. See
            /// [`TupleGuard`] for an example.
            pub fn set(&mut self, value: ($($T,)*)) -> Result<(), Error> {
                replace(self.inner, value, self.backend, &self.place)?;
                self.refresh();
                Ok(())
            }

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
            ) -> ( $(<$T as Persistable<__B::Pointer>>::Guard<'_, __B, __E>,)* ) {
                let mut fixed = 0usize;
                $(
                    // Each component guard borrows a disjoint part of the tuple.
                    let $T = {
                        let place = self.place.field::<__E, MAX_FIELDS>(&self.fields, $idx, fixed);
                        fixed += <$T as Persistable<__B::Pointer>>::SLOTTED_SIZE.unwrap_or(0);
                        <$T as Persistable<__B::Pointer>>::guard(
                            &mut self.inner.$idx,
                            self.backend,
                            place,
                        )
                    };
                )*
                ( $($T,)* )
            }

            /// Records the components' offsets, if their places link to them.
            fn refresh(&self) {
                if self.place.links_fields() {
                    self.fields.fill(0, &[
                        $( <$T as Persistable<__B::Pointer>>::encoded_size::<__E>(&self.inner.$idx), )*
                    ]);
                }
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
    use crate::encoding::Packed;
    use crate::location::Location;
    use kladde_store::{MemoryStorage, Pointer, Store};

    fn setup(size: usize) -> (Store, Location<Pointer, u32>) {
        let store = Store::create(Box::new(MemoryStorage::new()), Default::default()).unwrap();
        let p = store.alloc(size as u32).unwrap();
        (store, Location::new(p.raw(), 0))
    }

    #[test]
    fn pair_round_trips_through_a_backend() {
        let (mut store, location) = setup(<(i32, bool) as Persistable>::SLOTTED_SIZE.unwrap());
        let mut value = (-7i32, true);
        value.store::<_, Slotted>(&store, location).unwrap();
        store.flush().unwrap();
        assert_eq!(
            <(i32, bool)>::load::<_, Slotted>(&mut store, location).unwrap(),
            (-7, true)
        );
    }

    #[test]
    fn parts_hands_out_one_guard_per_component() {
        let (mut store, location) = setup(<(i32, u8) as Persistable>::SLOTTED_SIZE.unwrap());
        let mut value = (0i32, 0u8);
        {
            let mut guard = value.guard(&store, Slotted::at(location));
            let (mut first, mut second) = guard.parts();
            first.set(9).unwrap();
            second.set(3).unwrap();
        }
        assert_eq!(value, (9, 3));
        store.flush().unwrap();
        assert_eq!(
            <(i32, u8)>::load::<_, Slotted>(&mut store, location).unwrap(),
            (9, 3)
        );
    }

    #[test]
    fn empty_tuple_occupies_no_bytes() {
        assert_eq!(<() as Persistable>::SLOTTED_SIZE, Some(0));
        assert_eq!(<() as Persistable>::PACKED_SIZE, Some(0));
    }

    #[test]
    fn packed_parts_find_themselves_after_a_sibling_grows() {
        let (mut store, location) = setup(0);
        let mut value = (1u32, 2u32, 3u32);
        value.store::<_, Packed>(&store, location).unwrap();
        {
            let mut guard = value.guard(&store, Packed::at(location));
            let (mut a, mut b, mut c) = guard.parts();
            a.set(1_000).unwrap(); // two bytes now: b and c move
            c.set(300).unwrap();
            b.set(20_000_000).unwrap();
            a.set(5).unwrap();
        }
        assert_eq!(value, (5, 20_000_000, 300));
        store.flush().unwrap();
        assert_eq!(store.read_all(location.anchor).unwrap().len(), 1 + 4 + 2);
        assert_eq!(
            <(u32, u32, u32)>::load::<_, Packed>(&mut store, location).unwrap(),
            value
        );
    }
}
