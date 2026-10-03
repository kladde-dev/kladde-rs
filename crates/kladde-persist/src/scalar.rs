//! [`Persistable`] for the primitive types, so that `#[derive(Persistable)]`
//! can treat every field uniformly -- a `_mut()` accessor returning a nested
//! [`Guard`] -- rather than special-casing leaves.
//!
//! These live here, in the crate that defines `Persistable`: `impl Persistable
//! for i32` is `impl ForeignTrait for ForeignType` from anywhere else, which
//! the orphan rules forbid.
//!
//! Every scalar is pointer-free, so each implements `Persistable<P>` for **all**
//! `P: PointerRepr`. The guard types are generic over the backend and the
//! encoding, and read the pointer type back off the backend as `B::Pointer`.
//!
//! The two encodings:
//!
//! | type | fixed | packed |
//! | --- | --- | --- |
//! | `u8`, `i8`, `bool` | one byte | the same byte |
//! | `u16`, `u32`, `u64` | little-endian | unsigned LEB128 |
//! | `i16`, `i32`, `i64` | little-endian two's complement | zigzag, then LEB128 |
//! | `f32`, `f64` | IEEE-754, little-endian | the same bytes |
//! | `char` | the scalar value as a little-endian `u32` | UTF-8 |

use kladde_schema::{Primitive, TypeDescriptor};
use kladde_store::{Error, PointerRepr, ReadBackend, WriteBackend};

use crate::encoding::{Encoding, Slotted};
use crate::guard::Guard;
use crate::input::Input;
use crate::persistable::{Persistable, Slottable};
use crate::place::{write_encoded, Place};
use crate::schema::SchemaBuilder;

/// A scalar's guard: a whole-value `set`, and nothing else to mutate.
macro_rules! scalar_guard {
    ($ty:ty, $guard:ident, $sample:expr) => {
        #[doc = concat!("The [`Guard`] of `", stringify!($ty), "`: replaces the value with [`set`](", stringify!($guard), "::set).")]
        ///
        /// ```
        /// use kladde_persist::{Location, Persistable, Slotted};
        /// use kladde_store::{MemoryStorage, Store, WriteBackend};
        ///
        /// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
        #[doc = concat!("let size = <", stringify!($ty), " as Persistable>::SLOTTED_SIZE.unwrap();")]
        /// let p = store.alloc(size as u32)?;
        #[doc = concat!("let mut value: ", stringify!($ty), " = Default::default();")]
        #[doc = concat!("value.guard(&store, Slotted::at(Location::new(p.raw(), 0))).set(", stringify!($sample), ")?;")]
        #[doc = concat!("assert_eq!(value, ", stringify!($sample), ");")]
        /// # Ok::<(), kladde_store::Error>(())
        /// ```
        pub struct $guard<'s, B: WriteBackend, E: Encoding = Slotted> {
            inner: &'s mut $ty,
            backend: &'s B,
            place: Place<'s, B, E>,
        }

        impl<'s, B: WriteBackend, E: Encoding> $guard<'s, B, E> {
            /// Replaces the value. In a slotted place, or wherever the
            /// encoding keeps its size, that is one write; in a packed place
            /// where it does not, one splice that the enclosing values hear
            /// about, in one transaction.
            #[doc = concat!("See [`", stringify!($guard), "`] for an example.")]
            pub fn set(&mut self, value: $ty) -> Result<(), Error> {
                let mut bytes = Vec::with_capacity(8);
                <$ty as Persistable<B::Pointer>>::encode::<E>(&value, &mut bytes);
                let old = <$ty as Persistable<B::Pointer>>::encoded_size::<E>(self.inner);
                write_encoded(self.backend, &self.place, old, &bytes)?;
                *self.inner = value;
                Ok(())
            }
        }

        impl<'s, B: WriteBackend, E: Encoding> Guard for $guard<'s, B, E> {
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

        impl<'s, B: WriteBackend, E: Encoding> ::std::ops::Deref for $guard<'s, B, E> {
            type Target = $ty;
            fn deref(&self) -> &$ty {
                self.inner
            }
        }
    };
}

/// The `Persistable` items every scalar spells the same way: its guard and
/// its descriptor. `$size`, `$encode` and `$decode` supply the rest.
macro_rules! scalar_persistable {
    (
        $ty:ty, $guard:ident, $code:expr, $slotted:expr, $packed:expr,
        size: |$sv:ident, $sp:ident| $size:expr,
        encode: |$ev:ident, $ep:ident, $out:ident| $encode:expr,
        decode: |$dp:ident, $input:ident| $decode:expr $(,)?
    ) => {
        impl<P: PointerRepr> Slottable<P> for $ty {}

        impl<P: PointerRepr> Persistable<P> for $ty {
            const SLOTTED_SIZE: Option<usize> = Some($slotted);
            const PACKED_SIZE: Option<usize> = $packed;

            type RootEncoding = Slotted;

            type Guard<'s, B: WriteBackend<Pointer = P>, E: Encoding>
                = $guard<'s, B, E>
            where
                Self: 's,
                B: 's;

            #[inline]
            fn guard<'s, B: WriteBackend<Pointer = P>, E: Encoding>(
                &'s mut self,
                backend: &'s B,
                place: Place<'s, B, E>,
            ) -> Self::Guard<'s, B, E> {
                $guard {
                    inner: self,
                    backend,
                    place,
                }
            }

            #[inline]
            fn encoded_size<E: Encoding>(&self) -> usize {
                let $sv = *self;
                let $sp = E::PACKED;
                $size
            }

            #[inline]
            fn encode<E: Encoding>(&self, $out: &mut Vec<u8>) {
                let $ev = *self;
                let $ep = E::PACKED;
                $encode
            }

            #[inline]
            fn decode<B: ReadBackend<Pointer = P>, E: Encoding>(
                _backend: &mut B,
                $input: &mut Input<'_>,
            ) -> Result<Self, Error> {
                let $dp = E::PACKED;
                $decode
            }

            fn describe_local(_builder: &mut SchemaBuilder) -> TypeDescriptor {
                TypeDescriptor::Primitive($code)
            }
        }
    };
}

/// A scalar whose two encodings are its little-endian bytes: the floats and
/// the one-byte integers.
macro_rules! fixed_scalar {
    ($ty:ty, $guard:ident, $code:expr, $sample:expr) => {
        scalar_guard!($ty, $guard, $sample);
        scalar_persistable!(
            $ty, $guard, $code,
            ::std::mem::size_of::<$ty>(),
            Some(::std::mem::size_of::<$ty>()),
            size: |_v, _packed| ::std::mem::size_of::<$ty>(),
            encode: |v, _packed, out| out.extend_from_slice(&v.to_le_bytes()),
            decode: |_packed, input| Ok(<$ty>::from_le_bytes(input.array()?)),
        );
    };
}

/// An unsigned integer wider than a byte: little-endian when slotted, an
/// unsigned LEB128 varint when packed.
macro_rules! unsigned_scalar {
    ($ty:ty, $guard:ident, $code:expr, $sample:expr) => {
        scalar_guard!($ty, $guard, $sample);
        scalar_persistable!(
            $ty, $guard, $code,
            ::std::mem::size_of::<$ty>(),
            None,
            size: |v, packed| if packed {
                kladde_varint::encoded_len(v as u64)
            } else {
                ::std::mem::size_of::<$ty>()
            },
            encode: |v, packed, out| if packed {
                kladde_varint::encode(v as u64, out)
            } else {
                out.extend_from_slice(&v.to_le_bytes())
            },
            decode: |packed, input| if packed {
                let wide = input.varint()?;
                <$ty>::try_from(wide).map_err(|_| {
                    Error::Corrupt(format!("{wide} is out of range for {}", stringify!($ty)))
                })
            } else {
                Ok(<$ty>::from_le_bytes(input.array()?))
            },
        );
    };
}

/// A signed integer wider than a byte: little-endian two's complement when
/// slotted, zigzag and then LEB128 when packed, so that a number of small
/// magnitude takes one byte whatever its sign.
macro_rules! signed_scalar {
    ($ty:ty, $guard:ident, $code:expr, $sample:expr) => {
        scalar_guard!($ty, $guard, $sample);
        scalar_persistable!(
            $ty, $guard, $code,
            ::std::mem::size_of::<$ty>(),
            None,
            size: |v, packed| if packed {
                kladde_varint::encoded_len(kladde_varint::zigzag(v as i64))
            } else {
                ::std::mem::size_of::<$ty>()
            },
            encode: |v, packed, out| if packed {
                kladde_varint::encode(kladde_varint::zigzag(v as i64), out)
            } else {
                out.extend_from_slice(&v.to_le_bytes())
            },
            decode: |packed, input| if packed {
                let wide = kladde_varint::unzigzag(input.varint()?);
                <$ty>::try_from(wide).map_err(|_| {
                    Error::Corrupt(format!("{wide} is out of range for {}", stringify!($ty)))
                })
            } else {
                Ok(<$ty>::from_le_bytes(input.array()?))
            },
        );
    };
}

fixed_scalar!(u8, U8Guard, Primitive::U8, 7);
fixed_scalar!(i8, I8Guard, Primitive::I8, -7);
fixed_scalar!(f32, F32Guard, Primitive::F32, 1.5);
fixed_scalar!(f64, F64Guard, Primitive::F64, 1.5);
unsigned_scalar!(u16, U16Guard, Primitive::U16, 7);
unsigned_scalar!(u32, U32Guard, Primitive::U32, 7);
unsigned_scalar!(u64, U64Guard, Primitive::U64, 7);
signed_scalar!(i16, I16Guard, Primitive::I16, -7);
signed_scalar!(i32, I32Guard, Primitive::I32, -7);
signed_scalar!(i64, I64Guard, Primitive::I64, -7);

scalar_guard!(bool, BoolGuard, true);
scalar_persistable!(
    bool, BoolGuard, Primitive::Bool, 1, Some(1),
    size: |_v, _packed| 1,
    encode: |v, _packed, out| out.push(v as u8),
    decode: |_packed, input| Ok(input.byte()? != 0),
);

scalar_guard!(char, CharGuard, 'k');
scalar_persistable!(
    char, CharGuard, Primitive::Char, 4, None,
    size: |v, packed| if packed { v.len_utf8() } else { 4 },
    encode: |v, packed, out| if packed {
        let mut buf = [0u8; 4];
        out.extend_from_slice(v.encode_utf8(&mut buf).as_bytes())
    } else {
        out.extend_from_slice(&(v as u32).to_le_bytes())
    },
    decode: |packed, input| if packed {
        decode_utf8_char(input)
    } else {
        let b = u32::from_le_bytes(input.array()?);
        char::from_u32(b).ok_or_else(|| Error::Corrupt(format!("{b:#x} is not a char")))
    },
);

/// One `char` in UTF-8, which must be well-formed: its shortest form, and no
/// surrogate, as UTF-8 itself requires.
fn decode_utf8_char(input: &mut Input<'_>) -> Result<char, Error> {
    let first = input.peek()?;
    let len = match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 0,
    };
    let bytes = input.take(len.max(1))?;
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|s| s.chars().next())
        .ok_or_else(|| Error::Corrupt(format!("{bytes:x?} is not a char in UTF-8")))
}

// `String` deliberately does *not* implement `Persistable`: it is a foreign,
// `std`-defined type with no room for a pointer to its own content allocation,
// so a `store` would have nowhere to remember an earlier call's allocation and
// would leak a fresh one on every call. This absence *is* the enforcement
// mechanism the derive macro relies on: a struct field typed as plain `String`
// fails to compile, pointing application authors at
// `kladde_types::PersistableString` instead.

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

    fn packed<T: Persistable>(value: T) -> Vec<u8> {
        value.to_bytes::<Packed>()
    }

    #[test]
    fn i32_guard_records_and_mutates() {
        let (mut store, location) = setup(4);
        let mut value: i32 = 1;
        let mut guard = value.guard(&store, Slotted::at(location));
        guard.set(42).unwrap();
        assert_eq!(*guard, 42);
        assert_eq!(value, 42);
        store.flush().unwrap();
        assert_eq!(i32::load::<_, Slotted>(&mut store, location).unwrap(), 42);
    }

    #[test]
    fn bool_and_char_round_trip() {
        let (mut store, location) = setup(8);
        let mut b = false;
        b.guard(&store, Slotted::at(location)).set(true).unwrap();
        let mut c = 'a';
        c.guard(&store, Slotted::at(location + 4)).set('z').unwrap();
        store.flush().unwrap();
        assert!(bool::load::<_, Slotted>(&mut store, location).unwrap());
        assert_eq!(
            char::load::<_, Slotted>(&mut store, location + 4).unwrap(),
            'z'
        );
    }

    #[test]
    fn an_invalid_char_is_corruption_not_a_panic() {
        let (mut store, location) = setup(4);
        store
            .write(location.anchor, 0, &0xD800u32.to_le_bytes())
            .unwrap();
        store.flush().unwrap();
        assert!(matches!(
            char::load::<_, Slotted>(&mut store, location),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn packed_integers_are_varints() {
        assert_eq!(packed(127u16), [0x7f]);
        assert_eq!(packed(128u32), [0x80, 0x01]);
        assert_eq!(packed(u64::MAX).len(), 10);
        assert_eq!(packed(0i32), [0]);
        assert_eq!(packed(-1i64), [1]);
        assert_eq!(packed(1i16), [2]);
        assert_eq!(packed(-64i32), [0x7f]);
        assert_eq!(packed(i16::MIN).len(), 3);
        assert_eq!(packed(200u8), [200]);
        assert_eq!(packed(-1i8), [0xff]);
    }

    #[test]
    fn packed_chars_are_utf8() {
        for c in ['a', 'é', '€', '𝄞'] {
            let mut buf = [0u8; 4];
            assert_eq!(packed(c), c.encode_utf8(&mut buf).as_bytes());
            let bytes = packed(c);
            let mut input = Input::new(&bytes);
            let (mut store, _) = setup(0);
            assert_eq!(
                <char as Persistable>::decode::<_, Packed>(&mut store, &mut input).unwrap(),
                c
            );
        }
    }

    #[test]
    fn non_canonical_packed_encodings_are_refused() {
        let (mut store, _) = setup(0);
        let refuse = |bytes: &[u8], store: &mut Store| {
            let mut input = Input::new(bytes);
            u16::decode::<_, Packed>(store, &mut input).is_err()
        };
        assert!(refuse(&[0x81, 0x00], &mut store), "an overlong varint");
        assert!(refuse(&[0x80, 0x80, 0x04], &mut store), "out of range");
        for bytes in [&[0xc0, 0x80][..], &[0xed, 0xa0, 0x80], &[0xff]] {
            let mut input = Input::new(bytes);
            assert!(
                char::decode::<_, Packed>(&mut store, &mut input).is_err(),
                "{bytes:x?} is not UTF-8"
            );
        }
    }

    #[test]
    fn a_packed_integer_that_grows_splices() {
        let (mut store, location) = setup(1);
        let mut value: u32 = 100;
        let mut guard = value.guard(&store, Packed::at(location));
        guard.set(200).unwrap();
        guard.set(70_000).unwrap();
        store.flush().unwrap();
        assert_eq!(store.read_all(location.anchor).unwrap().len(), 3);
        assert_eq!(
            u32::load::<_, Packed>(&mut store, location).unwrap(),
            70_000
        );
    }
}
