//! [`Persistable`] for tuples of `Persistable` types, `(A, B, ...)`, up to
//! arity 12 and including the empty tuple `()`, so a tuple can be used
//! wherever a `Persistable` field, element, or value is expected -- e.g.
//! `PersistableVec<(i32, bool)>` or `PersistableMap<K, ()>`.
//!
//! A tuple is encoded exactly like a tuple struct with the same fields: its
//! components back to back. Its schema descriptor is a
//! [`TypeDescriptor::Struct`] with positional field names (`"0"`, `"1"`,
//! ...) -- structurally identical to what `#[derive(Persistable)]` produces
//! for a tuple struct `struct S(A, B)`, differing only in the type *name*
//! (`"Tuple2"` vs. `"S"`), which the fingerprint ignores.

use kladde_schema::{Field, TypeDescriptor};
use kladde_store::Error;

use crate::codec::{Decoder, Encoder};
use crate::persistable::{replace, Guard, Persistable};
use crate::region::{Backend, FieldSizes, Region};
use crate::schema::SchemaBuilder;
use crate::sizes::sum_sizes;

/// The [`Guard`] of a tuple: a whole-value [`set`](TupleGuard::set), and
/// `parts`, to mutate the components one by one.
///
/// ```
/// use kladde_persist::{Persistable, Region};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let mut pair = (0u32, false);
/// store.splice(0, 0, &pair.to_bytes())?;
/// {
///     let mut guard = pair.guard(Region::root(&store));
///     guard.set((1, false))?;
///     let (mut count, mut flag) = guard.parts();
///     count.set(1000)?; // grows by a byte, so `flag` moves
///     flag.set(true)?;
/// }
/// assert_eq!(pair, (1000, true));
/// store.flush()?;
/// assert_eq!(store.read_all()?, (1000u32, true).to_bytes());
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub struct TupleGuard<'r, T, B> {
    inner: &'r mut T,
    region: Region<'r, B>,
    fields: FieldSizes,
}

impl<T, B> Guard for TupleGuard<'_, T, B> {
    type Persistable = T;

    fn as_persistable(&self) -> &T {
        self.inner
    }
}

impl<T, B> std::ops::Deref for TupleGuard<'_, T, B> {
    type Target = T;

    fn deref(&self) -> &T {
        self.inner
    }
}

/// Generates one `impl Persistable for (T0, T1, ...)`. `$name` is the
/// schema type name; each `$T $idx $v` triple is a component's type
/// parameter, its tuple index, and a name for its guard.
macro_rules! impl_persistable_tuple {
    ($name:literal; $($T:ident $idx:tt $v:ident),*) => {
        impl<$($T: Persistable,)*> Persistable for ($($T,)*) {
            const FIXED_SIZE: Option<usize> = sum_sizes(&[$($T::FIXED_SIZE,)*]);
            const FIXED_FORM_SIZE: Option<usize> = sum_sizes(&[$($T::FIXED_FORM_SIZE,)*]);

            #[inline]
            fn encoded_len(&self) -> usize {
                0 $(+ self.$idx.encoded_len())*
            }

            #[inline]
            #[allow(unused_variables)]
            fn encode(&self, out: &mut Encoder<'_>) {
                $(self.$idx.encode(out);)*
            }

            #[inline]
            #[allow(unused_variables)]
            fn decode(input: &mut Decoder<'_>) -> Result<Self, Error> {
                Ok(($($T::decode(input)?,)*))
            }

            #[inline]
            #[allow(unused_variables)]
            fn encode_fixed(&self, out: &mut Encoder<'_>) {
                $(self.$idx.encode_fixed(out);)*
            }

            #[inline]
            #[allow(unused_variables)]
            fn decode_fixed(input: &mut Decoder<'_>) -> Result<Self, Error> {
                Ok(($($T::decode_fixed(input)?,)*))
            }

            type Guard<'r, B: Backend + 'r> = TupleGuard<'r, Self, B> where Self: 'r;

            fn guard<'r, B: Backend + 'r>(&'r mut self, region: Region<'r, B>) -> Self::Guard<'r, B> {
                TupleGuard { inner: self, region, fields: FieldSizes::new() }
            }

            #[allow(unused_variables)]
            fn describe_local(builder: &mut SchemaBuilder) -> TypeDescriptor
            where
                Self: 'static,
            {
                TypeDescriptor::Struct {
                    name: $name.into(),
                    fields: vec![$(Field {
                        name: stringify!($idx).into(),
                        ty: <$T as Persistable>::describe(builder),
                    },)*],
                }
            }
        }

        impl<'r, $($T: Persistable,)* B: Backend> TupleGuard<'r, ($($T,)*), B> {
            /// A guard for every component at once. Each finds where it
            /// starts whenever it writes, so a component before it may
            /// change size meanwhile. See [`TupleGuard`] for an example.
            #[allow(clippy::unused_unit, unused_variables)]
            pub fn parts(&mut self) -> ($($T::Guard<'_, B>,)*) {
                let TupleGuard { inner, region, fields } = self;
                fields.fill(0, &[$($T::FIXED_SIZE,)*], &[$(inner.$idx.encoded_len(),)*]);
                let (fields, region) = (&*fields, &*region);
                let ($($v,)*) = &mut **inner;
                ($($v.guard(region.field(fields, $idx)),)*)
            }

            /// Replaces the whole tuple. See [`TupleGuard`] for an example.
            pub fn set(&mut self, value: ($($T,)*)) -> Result<(), Error> {
                replace(self.inner, value, &self.region)
            }
        }
    };
}

impl_persistable_tuple!("Tuple0";);
impl_persistable_tuple!("Tuple1"; A 0 a);
impl_persistable_tuple!("Tuple2"; A 0 a, B1 1 b);
impl_persistable_tuple!("Tuple3"; A 0 a, B1 1 b, C 2 c);
impl_persistable_tuple!("Tuple4"; A 0 a, B1 1 b, C 2 c, D 3 d);
impl_persistable_tuple!("Tuple5"; A 0 a, B1 1 b, C 2 c, D 3 d, E 4 e);
impl_persistable_tuple!("Tuple6"; A 0 a, B1 1 b, C 2 c, D 3 d, E 4 e, F 5 f);
impl_persistable_tuple!("Tuple7"; A 0 a, B1 1 b, C 2 c, D 3 d, E 4 e, F 5 f, G 6 g);
impl_persistable_tuple!("Tuple8"; A 0 a, B1 1 b, C 2 c, D 3 d, E 4 e, F 5 f, G 6 g, H 7 h);
impl_persistable_tuple!("Tuple9"; A 0 a, B1 1 b, C 2 c, D 3 d, E 4 e, F 5 f, G 6 g, H 7 h, I 8 i);
impl_persistable_tuple!("Tuple10"; A 0 a, B1 1 b, C 2 c, D 3 d, E 4 e, F 5 f, G 6 g, H 7 h, I 8 i, J 9 j);
impl_persistable_tuple!("Tuple11"; A 0 a, B1 1 b, C 2 c, D 3 d, E 4 e, F 5 f, G 6 g, H 7 h, I 8 i, J 9 j, K 10 k);
impl_persistable_tuple!("Tuple12"; A 0 a, B1 1 b, C 2 c, D 3 d, E 4 e, F 5 f, G 6 g, H 7 h, I 8 i, J 9 j, K 10 k, L 11 l);

#[cfg(test)]
mod tests {
    use super::*;
    use kladde_schema::TypeRef;

    #[test]
    fn sizes_combine() {
        assert_eq!(<(u8, f32)>::FIXED_SIZE, Some(5));
        assert_eq!(<(u8, u32)>::FIXED_SIZE, None);
        assert_eq!(<(u8, u32)>::FIXED_FORM_SIZE, Some(5));
        assert_eq!(<()>::FIXED_SIZE, Some(0));
        assert!(().to_bytes().is_empty());
    }

    #[test]
    fn the_descriptor_matches_the_bytes() {
        let value = (300u32, -2i64, 'x', (true, 1.5f64));
        let table = <(u32, i64, char, (bool, f64))>::schema();
        assert_eq!(
            table.measure(TypeRef(0), &value.to_bytes()),
            Some(value.encoded_len())
        );
        assert_eq!(
            <(u32, i64, char, (bool, f64))>::from_bytes(&value.to_bytes()).unwrap(),
            value
        );
    }
}
