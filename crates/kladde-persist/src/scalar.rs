//! [`Durable`] and [`Bounded`] for the primitive types, whose `Mut`s are
//! [`SetMut`]s.
//!
//! | type | encoding | fixed form |
//! | --- | --- | --- |
//! | `u8`, `i8`, `bool` | one byte | the same byte |
//! | `u16`, `u32`, `u64` | unsigned LEB128 | little-endian |
//! | `i16`, `i32`, `i64` | zigzag, then LEB128 | little-endian two's complement |
//! | `f32`, `f64` | IEEE-754, little-endian | the same bytes |
//! | `char` | UTF-8 | the scalar value as a little-endian `u32` |
//!
//! These live here, in the crate that defines `Durable`, since the
//! orphan rules forbid `impl Durable for i32` anywhere else.

use kladde_schema::{Primitive, TypeDescriptor};
use kladde_store::Error;

use crate::codec::{Decoder, Encoder};
use crate::durable::{Bounded, Durable, SetMut};
use crate::region::{Backend, Region};
use crate::schema::SchemaBuilder;

/// The items every scalar spells the same way.
macro_rules! scalar {
    (
        $ty:ty, $code:expr, $fixed:expr, $form:expr,
        len: |$lv:ident| $len:expr,
        encode: |$ev:ident, $out:ident| $encode:expr,
        decode: |$input:ident| $decode:expr,
        encode_fixed: |$fv:ident, $fout:ident| $encode_fixed:expr,
        decode_fixed: |$finput:ident| $decode_fixed:expr $(,)?
    ) => {
        impl Bounded for $ty {
            const FIXED_FORM_SIZE: usize = $form;

            #[inline]
            fn encode_fixed(&self, $fout: &mut Encoder<'_>) {
                let $fv = *self;
                $encode_fixed
            }

            #[inline]
            fn decode_fixed($finput: &mut Decoder<'_>) -> Result<Self, Error> {
                $decode_fixed
            }
        }

        impl Durable for $ty {
            const FIXED_SIZE: Option<usize> = $fixed;

            #[inline]
            fn encoded_len(&self) -> usize {
                let $lv = *self;
                $len
            }

            #[inline]
            fn encode(&self, $out: &mut Encoder<'_>) {
                let $ev = *self;
                $encode
            }

            #[inline]
            fn decode($input: &mut Decoder<'_>) -> Result<Self, Error> {
                $decode
            }

            type Mut<'r, B: Backend + 'r> = SetMut<'r, $ty, B>;

            #[inline]
            fn mutate<'r, B: Backend + 'r>(
                &'r mut self,
                region: Region<'r, B>,
            ) -> Self::Mut<'r, B> {
                SetMut::new(self, region)
            }

            fn describe_local(_builder: &mut SchemaBuilder) -> TypeDescriptor {
                TypeDescriptor::Primitive($code)
            }
        }
    };
}

/// A scalar whose encoding and fixed form are its little-endian bytes: the
/// floats and the one-byte integers.
macro_rules! le_scalar {
    ($ty:ty, $code:expr) => {
        scalar!(
            $ty, $code,
            Some(::std::mem::size_of::<$ty>()),
            ::std::mem::size_of::<$ty>(),
            len: |_v| ::std::mem::size_of::<$ty>(),
            encode: |v, out| out.bytes(&v.to_le_bytes()),
            decode: |input| Ok(<$ty>::from_le_bytes(input.array()?)),
            encode_fixed: |v, out| out.bytes(&v.to_le_bytes()),
            decode_fixed: |input| Ok(<$ty>::from_le_bytes(input.array()?)),
        );
    };
}

/// An unsigned integer wider than a byte: a varint, and little-endian in
/// its fixed form.
macro_rules! unsigned_scalar {
    ($ty:ty, $code:expr) => {
        scalar!(
            $ty, $code,
            None,
            ::std::mem::size_of::<$ty>(),
            len: |v| kladde_varint::encoded_len(v as u64),
            encode: |v, out| out.varint(v as u64),
            decode: |input| input.varint_as::<$ty>(),
            encode_fixed: |v, out| out.bytes(&v.to_le_bytes()),
            decode_fixed: |input| Ok(<$ty>::from_le_bytes(input.array()?)),
        );
    };
}

/// A signed integer wider than a byte: zigzag and then a varint, so that a
/// number of small magnitude takes one byte whatever its sign, and
/// little-endian two's complement in its fixed form.
macro_rules! signed_scalar {
    ($ty:ty, $code:expr) => {
        scalar!(
            $ty, $code,
            None,
            ::std::mem::size_of::<$ty>(),
            len: |v| kladde_varint::encoded_len(kladde_varint::zigzag(v as i64)),
            encode: |v, out| out.zigzag(v as i64),
            decode: |input| {
                let wide = input.zigzag()?;
                <$ty>::try_from(wide).map_err(|_| {
                    input.corrupt(format!("{wide} is out of range for {}", stringify!($ty)))
                })
            },
            encode_fixed: |v, out| out.bytes(&v.to_le_bytes()),
            decode_fixed: |input| Ok(<$ty>::from_le_bytes(input.array()?)),
        );
    };
}

le_scalar!(u8, Primitive::U8);
le_scalar!(i8, Primitive::I8);
le_scalar!(f32, Primitive::F32);
le_scalar!(f64, Primitive::F64);
unsigned_scalar!(u16, Primitive::U16);
unsigned_scalar!(u32, Primitive::U32);
unsigned_scalar!(u64, Primitive::U64);
signed_scalar!(i16, Primitive::I16);
signed_scalar!(i32, Primitive::I32);
signed_scalar!(i64, Primitive::I64);

fn decode_bool(input: &mut Decoder<'_>) -> Result<bool, Error> {
    match input.byte()? {
        0 => Ok(false),
        1 => Ok(true),
        b => Err(input.corrupt(format!("{b} is not a bool"))),
    }
}

scalar!(
    bool, Primitive::Bool, Some(1), 1,
    len: |_v| 1,
    encode: |v, out| out.byte(v as u8),
    decode: |input| decode_bool(input),
    encode_fixed: |v, out| out.byte(v as u8),
    decode_fixed: |input| decode_bool(input),
);

scalar!(
    char, Primitive::Char, None, 4,
    len: |v| v.len_utf8(),
    encode: |v, out| {
        let mut buf = [0u8; 4];
        out.bytes(v.encode_utf8(&mut buf).as_bytes())
    },
    decode: |input| input.utf8_char(),
    encode_fixed: |v, out| out.bytes(&(v as u32).to_le_bytes()),
    decode_fixed: |input| {
        let b = u32::from_le_bytes(input.array()?);
        char::from_u32(b).ok_or_else(|| input.corrupt(format!("{b:#x} is not a char")))
    },
);

#[cfg(test)]
mod tests {
    use super::*;
    use kladde_store::{MemoryStorage, Store, WriteBackend};

    fn fixed<T: Bounded>(value: T) -> Vec<u8> {
        let mut out = Vec::new();
        value.encode_fixed(&mut Encoder::new(&mut out));
        out
    }

    #[test]
    fn integers_are_varints_and_full_width_in_their_fixed_form() {
        assert_eq!(127u16.to_bytes(), [0x7f]);
        assert_eq!(128u32.to_bytes(), [0x80, 0x01]);
        assert_eq!(u64::MAX.to_bytes().len(), 10);
        assert_eq!(0i32.to_bytes(), [0]);
        assert_eq!((-1i64).to_bytes(), [1]);
        assert_eq!(1i16.to_bytes(), [2]);
        assert_eq!((-64i32).to_bytes(), [0x7f]);
        assert_eq!(i16::MIN.to_bytes().len(), 3);
        assert_eq!(200u8.to_bytes(), [200]);
        assert_eq!((-1i8).to_bytes(), [0xff]);
        assert_eq!(fixed(1u16), [1, 0]);
        assert_eq!(fixed(-1i32), [0xff; 4]);
        assert_eq!(fixed('a'), [0x61, 0, 0, 0]);
    }

    #[test]
    fn every_scalar_round_trips_in_both_forms() {
        fn both<T: Bounded + PartialEq + std::fmt::Debug + Copy + 'static>(values: &[T]) {
            for &v in values {
                assert_eq!(T::from_bytes(&v.to_bytes()).unwrap(), v);
                assert_eq!(v.to_bytes().len(), v.encoded_len());
                let f = fixed(v);
                assert_eq!(f.len(), T::FIXED_FORM_SIZE);
                assert_eq!(T::decode_fixed(&mut Decoder::new(&f)).unwrap(), v);
                assert_eq!(
                    T::schema().measure(kladde_schema::TypeRef(0), &v.to_bytes()),
                    Some(v.encoded_len())
                );
            }
        }
        both(&[0u8, 255]);
        both(&[i8::MIN, -1, 0, i8::MAX]);
        both(&[0u16, 300, u16::MAX]);
        both(&[0u32, 1 << 31, u32::MAX]);
        both(&[0u64, u64::MAX]);
        both(&[i16::MIN, -1, i16::MAX]);
        both(&[i32::MIN, -300, i32::MAX]);
        both(&[i64::MIN, i64::MAX]);
        both(&[0.0f32, -1.5, f32::MAX]);
        both(&[0.0f64, 1e300]);
        both(&[false, true]);
        both(&['a', 'é', '€', '𝄞']);
    }

    #[test]
    fn bad_bytes_are_corruption_not_a_panic() {
        assert!(bool::from_bytes(&[2]).is_err());
        assert!(
            u16::from_bytes(&[0x81, 0x00]).is_err(),
            "an overlong varint"
        );
        assert!(
            u16::from_bytes(&[0x80, 0x80, 0x04]).is_err(),
            "out of range"
        );
        assert!(char::decode_fixed(&mut Decoder::new(&0xD800u32.to_le_bytes())).is_err());
    }

    #[test]
    fn a_varint_that_grows_splices() {
        let store = Store::create(Box::new(MemoryStorage::new()), Default::default()).unwrap();
        let mut value: u32 = 100;
        store.splice(0, 0, &value.to_bytes()).unwrap();
        {
            let mut value_mut = value.mutate(Region::root(&store));
            value_mut.set(70_000).unwrap();
            value_mut.set(5).unwrap();
            value_mut.set(70_001).unwrap();
        }
        store.flush().unwrap();
        assert_eq!(store.read_all().unwrap(), 70_001u32.to_bytes());
        assert_eq!(value, 70_001);
    }
}
