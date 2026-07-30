//! [`Persistable`] impls for tuples of `Persistable` types, `(A, B, ...)`
//! for arities up to 12 (plus the empty tuple `()`), so a tuple can be
//! used anywhere a `Persistable` field/element/value is expected -- e.g.
//! `PersistableVec<(i32, bool)>` or `PersistableHashMap<K, ()>`.
//!
//! A tuple is laid out exactly like a tuple struct with the same fields:
//! its components back to back, no header, `INLINE_SIZE` the sum of theirs.
//! Its schema descriptor is a [`TypeDescriptor::Struct`] with positional
//! field names (`"0"`, `"1"`, ...) -- structurally identical to what
//! `#[derive(Persistable)]` produces for a tuple struct `struct S(A, B)`,
//! differing only in the type *name* (`"Tuple2"` vs. `"S"`), so a tuple and
//! a named tuple struct get distinct (correct) fingerprints.
//!
//! These impls live in `kladde-traits` (where `Persistable` is defined)
//! rather than `kladde-types`, for the same orphan-rule reason the scalar
//! impls do: `impl Persistable for (A, B)` is only legal in the trait's own
//! crate.

use crate::{Backend, Field, Guard, Location, Persistable, SchemaBuilder, TypeDescriptor};
use std::ops::{Deref, DerefMut};

/// The [`Guard`] a tuple's [`Persistable`] impl hands out.
///
/// Generic over the whole tuple type `T`; its only mutation is a
/// whole-value [`set`](TupleGuard::set) (like a derived `enum`'s guard).
/// Per-component in-place accessors are not offered (a component-level
/// `_mut()` on an anonymous tuple would need macro-generated identifiers);
/// reach for a small `#[derive(Persistable)]` tuple struct if you need to
/// mutate one field of a large tuple in place.
pub struct TupleGuard<'s, T, B> {
    inner: &'s mut T,
    backend: &'s B,
    location: Location,
}

impl<'s, T: Persistable, B: Backend> TupleGuard<'s, T, B> {
    /// Replaces the whole tuple with `value` and persists it.
    pub fn set(&mut self, mut value: T) {
        Persistable::store(&mut value, self.backend, self.location);
        *self.inner = value;
    }
}

impl<'s, T: Persistable, B: Backend> Guard for TupleGuard<'s, T, B> {
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

impl<'s, T, B> Deref for TupleGuard<'s, T, B> {
    type Target = T;
    fn deref(&self) -> &T {
        self.inner
    }
}

impl<'s, T, B> DerefMut for TupleGuard<'s, T, B> {
    fn deref_mut(&mut self) -> &mut T {
        self.inner
    }
}

/// Generates one `impl Persistable for (T0, T1, ...)`. `$name` is the
/// schema type name; each `$T $idx` pair is a component's type parameter
/// and its tuple index. Offsets are accumulated at runtime (each component
/// stored/loaded at the running `offset`, which then advances by that
/// component's `INLINE_SIZE`), which keeps the macro free of compile-time
/// prefix-sum gymnastics.
macro_rules! impl_persistable_tuple {
    ($name:literal; $($T:ident $idx:tt),*) => {
        impl<$($T: Persistable,)*> Persistable for ($($T,)*) {
            const INLINE_SIZE: usize = 0 $( + <$T as Persistable>::INLINE_SIZE )*;

            // `__B` (not `B`) for the backend generic, so it can't collide
            // with a component type parameter literally named `B`.
            type Guard<'s, __B: Backend>
                = TupleGuard<'s, Self, __B>
            where
                Self: 's,
                __B: 's;

            fn guard<'s, __B: Backend>(
                &'s mut self,
                backend: &'s __B,
                location: Location,
            ) -> Self::Guard<'s, __B> {
                TupleGuard {
                    inner: self,
                    backend,
                    location,
                }
            }

            #[allow(unused_variables, unused_mut, unused_assignments)]
            fn store<__B: Backend>(&mut self, backend: &__B, location: Location) {
                let mut offset = 0u32;
                $(
                    Persistable::store(&mut self.$idx, backend, location + offset);
                    offset += <$T as Persistable>::INLINE_SIZE as u32;
                )*
            }

            // `non_snake_case`: each component is bound to a local named
            // after its own (uppercase) type parameter, deliberately.
            // `clippy::unused_unit`: the arity-0 tuple reconstructs as `()`.
            #[allow(
                unused_variables,
                unused_mut,
                unused_assignments,
                non_snake_case,
                clippy::unused_unit
            )]
            fn load<__B: Backend>(backend: &__B, location: Location) -> Self {
                let mut offset = 0u32;
                $(
                    // Bind the component to a local named after its own
                    // type parameter (value/type namespaces don't clash).
                    let $T = <$T as Persistable>::load(backend, location + offset);
                    offset += <$T as Persistable>::INLINE_SIZE as u32;
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
                                ty: <$T as Persistable>::describe(builder),
                            },
                        )*
                    ],
                }
            }
        }

        // A per-arity inherent impl (distinct concrete tuple type each
        // time) adding `parts()` alongside the blanket `set`.
        impl<'s, $($T: Persistable,)* __B: Backend> TupleGuard<'s, ($($T,)*), __B> {
            /// Returns a guard for every component at once, so all components
            /// can be mutated simultaneously (the tuple analog of splitting
            /// `&mut (A, B)` into `&mut x.0` and `&mut x.1`).
            #[allow(
                unused_variables,
                unused_mut,
                unused_assignments,
                non_snake_case,
                clippy::unused_unit
            )]
            pub fn parts(&mut self) -> ( $(<$T as Persistable>::Guard<'_, __B>,)* ) {
                let mut offset = 0u32;
                $(
                    // Each component guard borrows a disjoint `&mut self.inner.$idx`.
                    let $T = {
                        let __g = <$T as Persistable>::guard(
                            &mut self.inner.$idx,
                            self.backend,
                            self.location + offset,
                        );
                        offset += <$T as Persistable>::INLINE_SIZE as u32;
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
    use crate::{Allocator, RawPointer, ResolvedPointer, UniquePointerResizable};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::num::NonZeroU32;

    #[derive(Default)]
    struct MockBackend {
        regions: RefCell<HashMap<NonZeroU32, Vec<u8>>>,
        next_index: std::cell::Cell<u32>,
    }

    impl MockBackend {
        fn root(&self, size: usize) -> Location {
            let pointer = self.alloc_fixed(size);
            Location {
                anchor: pointer.raw(),
                offset: 0,
            }
        }
    }

    impl Allocator for MockBackend {
        fn read(&self, target: RawPointer, offset: u32, len: u32) -> Vec<u8> {
            let regions = self.regions.borrow();
            regions[&target.index()][offset as usize..(offset + len) as usize].to_vec()
        }
        fn write(&self, target: RawPointer, offset: u32, bytes: &[u8]) {
            let mut regions = self.regions.borrow_mut();
            let region = regions.get_mut(&target.index()).unwrap();
            let start = offset as usize;
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
        fn alloc_resizable(&self, byte_size: usize) -> UniquePointerResizable {
            let raw = self.next_index.get() + 1;
            self.next_index.set(raw);
            let index = NonZeroU32::new(raw).unwrap();
            self.regions
                .borrow_mut()
                .insert(index, vec![0u8; byte_size]);
            UniquePointerResizable::from_index(index)
        }
        fn free_resizable(&self, pointer: UniquePointerResizable) {
            self.regions.borrow_mut().remove(&pointer.index());
        }
        fn resize(&self, pointer: &UniquePointerResizable, new_byte_size: usize) {
            let mut regions = self.regions.borrow_mut();
            regions
                .get_mut(&pointer.index())
                .unwrap()
                .resize(new_byte_size, 0);
        }
        fn splice(
            &self,
            pointer: &UniquePointerResizable,
            byte_offset: u32,
            old_byte_len: u32,
            new: &[u8],
        ) {
            let mut regions = self.regions.borrow_mut();
            let region = regions.get_mut(&pointer.index()).unwrap();
            let start = byte_offset as usize;
            region.splice(start..start + old_byte_len as usize, new.iter().copied());
        }
        fn capacity(&self, pointer: &UniquePointerResizable) -> Option<usize> {
            self.regions.borrow().get(&pointer.index()).map(Vec::len)
        }
        fn resolve(&self, pointer: RawPointer) -> Option<ResolvedPointer<'_>> {
            self.regions
                .borrow()
                .contains_key(&pointer.index())
                .then(|| ResolvedPointer::from_target(pointer.index()))
        }
    }

    #[test]
    fn inline_size_is_the_sum_of_components() {
        assert_eq!(<(i32, bool)>::INLINE_SIZE, 4 + 1);
        assert_eq!(<(u8, i64, u16)>::INLINE_SIZE, 1 + 8 + 2);
        assert_eq!(<()>::INLINE_SIZE, 0);
    }

    #[test]
    fn pair_round_trips_through_store_and_load() {
        let backend = MockBackend::default();
        let location = backend.root(<(i32, bool)>::INLINE_SIZE);

        let mut value = (7i32, true);
        value.store(&backend, location);

        let reloaded = <(i32, bool)>::load(&backend, location);
        assert_eq!(reloaded, (7, true));
    }

    #[test]
    fn guard_set_replaces_the_whole_tuple() {
        let backend = MockBackend::default();
        let location = backend.root(<(i32, u8)>::INLINE_SIZE);

        let mut value = (1i32, 2u8);
        value.guard(&backend, location).set((30, 40));
        assert_eq!(value, (30, 40));

        let reloaded = <(i32, u8)>::load(&backend, location);
        assert_eq!(reloaded, (30, 40));
    }

    #[test]
    fn parts_gives_simultaneous_component_guards() {
        let backend = MockBackend::default();
        let location = backend.root(<(i32, u8)>::INLINE_SIZE);

        let mut value = (0i32, 0u8);
        {
            let mut guard = value.guard(&backend, location);
            // Both component guards live at once (a is used after b).
            let (mut a, mut b) = guard.parts();
            b.set(9);
            a.set(7);
        }
        assert_eq!(value, (7, 9));

        let reloaded = <(i32, u8)>::load(&backend, location);
        assert_eq!(reloaded, (7, 9));
    }

    #[test]
    fn larger_tuple_lays_fields_out_at_cumulative_offsets() {
        let backend = MockBackend::default();
        let location = backend.root(<(u8, u32, i16, bool)>::INLINE_SIZE);

        let mut value = (9u8, 1234u32, -5i16, true);
        value.store(&backend, location);

        let reloaded = <(u8, u32, i16, bool)>::load(&backend, location);
        assert_eq!(reloaded, (9, 1234, -5, true));
    }

    #[test]
    fn empty_tuple_round_trips() {
        let backend = MockBackend::default();
        // `()` writes/reads nothing; the region size is immaterial.
        let location = backend.root(1);
        let mut value = ();
        value.store(&backend, location);
        assert_eq!(<()>::load(&backend, location), ());
    }

    #[test]
    fn schema_is_a_struct_with_positional_field_names() {
        let table = <(i32, bool)>::schema();
        match table.get(table.root()) {
            TypeDescriptor::Struct { name, fields } => {
                assert_eq!(name, "Tuple2");
                assert_eq!(fields[0].name, "0");
                assert_eq!(fields[1].name, "1");
            }
            other => panic!("expected Struct, got {other:?}"),
        }
    }
}
