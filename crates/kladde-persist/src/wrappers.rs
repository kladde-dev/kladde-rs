//! [`Persistable`] for the wrappers: [`Option`], an enum; [`Box`], persisted
//! as what it holds; and [`Fixed`], a type's values at a fixed size.

use kladde_schema::{Field, TypeDescriptor, TypeRef, Variant};
use kladde_store::Error;

use crate::codec::{Decoder, Encoder};
use crate::persistable::{replace, Guard, Persistable, SetGuard};
use crate::region::{Backend, Region};
use crate::schema::SchemaBuilder;

/// `None` as the discriminant 0, `Some(v)` as 1 followed by `v`: an enum,
/// described as one.
impl<T: Persistable> Persistable for Option<T> {
    const FIXED_SIZE: Option<usize> = match T::FIXED_SIZE {
        Some(0) => Some(1),
        _ => None,
    };
    const FIXED_FORM_SIZE: Option<usize> = match T::FIXED_FORM_SIZE {
        Some(n) => Some(1 + n),
        None => None,
    };

    fn encoded_len(&self) -> usize {
        1 + self.as_ref().map_or(0, T::encoded_len)
    }

    fn encode(&self, out: &mut Encoder<'_>) {
        match self {
            None => out.byte(0),
            Some(v) => {
                out.byte(1);
                v.encode(out);
            }
        }
    }

    fn decode(input: &mut Decoder<'_>) -> Result<Self, Error> {
        match input.byte()? {
            0 => Ok(None),
            1 => T::decode(input).map(Some),
            d => Err(input.corrupt(format!("{d} is no variant of Option"))),
        }
    }

    fn encode_fixed(&self, out: &mut Encoder<'_>) {
        match self {
            None => {
                out.byte(0);
                out.zeros(T::FIXED_FORM_SIZE.expect("a fixed form"));
            }
            Some(v) => {
                out.byte(1);
                v.encode_fixed(out);
            }
        }
    }

    fn decode_fixed(input: &mut Decoder<'_>) -> Result<Self, Error> {
        match input.byte()? {
            0 => {
                for _ in 0..T::FIXED_FORM_SIZE.unwrap_or(0) {
                    if input.byte()? != 0 {
                        return Err(input.corrupt("padding that is not zero"));
                    }
                }
                Ok(None)
            }
            1 => T::decode_fixed(input).map(Some),
            d => Err(input.corrupt(format!("{d} is no variant of Option"))),
        }
    }

    type Guard<'r, B: Backend + 'r>
        = OptionGuard<'r, T, B>
    where
        Self: 'r;

    fn guard<'r, B: Backend + 'r>(&'r mut self, region: Region<'r, B>) -> Self::Guard<'r, B> {
        OptionGuard {
            inner: self,
            region,
        }
    }

    fn describe_local(builder: &mut SchemaBuilder) -> TypeDescriptor
    where
        Self: 'static,
    {
        TypeDescriptor::Enum {
            name: "Option".into(),
            discriminant_width: 1,
            variants: vec![
                Variant {
                    discriminant: 0,
                    name: "None".into(),
                    fields: vec![],
                },
                Variant {
                    discriminant: 1,
                    name: "Some".into(),
                    fields: vec![Field {
                        name: "0".into(),
                        ty: T::describe(builder),
                    }],
                },
            ],
        }
    }
}

/// The [`Guard`] of an [`Option`]: replace it whole, or mutate what it
/// holds.
///
/// ```
/// use kladde_persist::{Persistable, Region};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let mut size: Option<u32> = None;
/// store.splice(0, 0, &size.to_bytes())?;
/// let mut guard = size.guard(Region::root(&store));
/// guard.set(Some(7))?;
/// guard.as_mut().unwrap().set(700)?;
/// drop(guard);
/// assert_eq!(size, Some(700));
/// store.flush()?;
/// assert_eq!(store.read_all()?, Some(700u32).to_bytes());
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub struct OptionGuard<'r, T, B> {
    inner: &'r mut Option<T>,
    region: Region<'r, B>,
}

impl<T: Persistable, B: Backend> OptionGuard<'_, T, B> {
    /// Replaces the whole value. See [`OptionGuard`] for an example.
    pub fn set(&mut self, value: Option<T>) -> Result<(), Error> {
        replace(self.inner, value, &self.region)
    }

    /// Takes the value out, leaving `None`.
    pub fn take(&mut self) -> Result<Option<T>, Error> {
        if self.inner.is_none() {
            return Ok(None);
        }
        self.region.replace(self.region.len(), &[0])?;
        Ok(self.inner.take())
    }

    /// The guard of the value inside, if there is one. See [`OptionGuard`]
    /// for an example.
    pub fn as_mut(&mut self) -> Option<T::Guard<'_, B>> {
        let value = self.inner.as_mut()?;
        let len = value.encoded_len() as u64;
        Some(value.guard(self.region.child(1, len, None, 0)))
    }
}

impl<T, B> Guard for OptionGuard<'_, T, B> {
    type Persistable = Option<T>;

    fn as_persistable(&self) -> &Option<T> {
        self.inner
    }
}

impl<T, B> std::ops::Deref for OptionGuard<'_, T, B> {
    type Target = Option<T>;

    fn deref(&self) -> &Option<T> {
        self.inner
    }
}

/// A `Box<T>` is persisted as the `T` it holds, with `T`'s encoding,
/// descriptor and fingerprint, so boxing a field changes nothing on file;
/// it is how a recursive type gets a size in Rust.
///
/// Its `FIXED_SIZE` is `None` whatever `T`'s, which costs a container of
/// boxes its multiplication and nothing else, and keeps a type that holds
/// itself through a box from asking its own size of itself.
///
/// ```
/// use kladde_persist::Persistable;
///
/// #[derive(Debug, PartialEq)]
/// enum List { Nil, Cons(u8, Box<List>) }
/// # impl Persistable for List {
/// #     const FIXED_SIZE: Option<usize> = None;
/// #     fn encoded_len(&self) -> usize { match self { List::Nil => 1, List::Cons(_, t) => 2 + t.encoded_len() } }
/// #     fn encode(&self, out: &mut kladde_persist::Encoder<'_>) {
/// #         match self { List::Nil => out.byte(0), List::Cons(h, t) => { out.byte(1); out.byte(*h); t.encode(out) } }
/// #     }
/// #     fn decode(input: &mut kladde_persist::Decoder<'_>) -> kladde_persist::Result<Self> {
/// #         Ok(match input.byte()? { 0 => List::Nil, _ => List::Cons(input.byte()?, Box::decode(input)?) })
/// #     }
/// #     type Guard<'r, B: kladde_persist::Backend + 'r> = kladde_persist::SetGuard<'r, Self, B>;
/// #     fn guard<'r, B: kladde_persist::Backend + 'r>(&'r mut self, region: kladde_persist::Region<'r, B>) -> Self::Guard<'r, B> {
/// #         kladde_persist::SetGuard::new(self, region)
/// #     }
/// # }
/// let list = List::Cons(1, Box::new(List::Cons(2, Box::new(List::Nil))));
/// assert_eq!(list.to_bytes(), [1, 1, 1, 2, 0]);
/// assert_eq!(Box::new(7u32).to_bytes(), 7u32.to_bytes());
/// ```
impl<T: Persistable> Persistable for Box<T> {
    const FIXED_SIZE: Option<usize> = None;

    fn encoded_len(&self) -> usize {
        (**self).encoded_len()
    }

    fn encode(&self, out: &mut Encoder<'_>) {
        (**self).encode(out)
    }

    fn decode(input: &mut Decoder<'_>) -> Result<Self, Error> {
        T::decode(input).map(Box::new)
    }

    type Guard<'r, B: Backend + 'r>
        = T::Guard<'r, B>
    where
        Self: 'r;

    fn guard<'r, B: Backend + 'r>(&'r mut self, region: Region<'r, B>) -> Self::Guard<'r, B> {
        (**self).guard(region)
    }

    fn describe(builder: &mut SchemaBuilder) -> TypeRef
    where
        Self: 'static,
    {
        T::describe(builder)
    }
}

/// `T`'s values at a fixed size: every integer at its full width, every
/// `char` in four bytes, and every enum with its discriminant at a fixed
/// width and its variant padded with zeros to the largest.
///
/// A `Fixed<T>` never changes size, so writing it is always an overwrite in
/// place, and a container of them finds its elements by multiplication. It
/// exists for every `T` with a
/// [fixed form](Persistable::FIXED_FORM_SIZE); using it with any other `T`
/// fails to compile. It applies to all of `T`: `Fixed<Option<u32>>` takes
/// five bytes for every value, while `Option<Fixed<u32>>` takes one for
/// `None` and five for `Some`.
///
/// Its guard replaces the value whole, since a guard of one of `T`'s fields
/// would have to write it in the fixed form.
///
/// ```
/// use kladde_persist::{Fixed, Persistable, Region};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// assert_eq!(<Fixed<Option<u32>> as Persistable>::FIXED_SIZE, Some(5));
/// assert_eq!(Fixed(1u32).to_bytes(), [1, 0, 0, 0]);
///
/// let store = Store::create(Box::new(MemoryStorage::new()), Default::default())?;
/// let mut counter = Fixed(0u32);
/// store.splice(0, 0, &counter.to_bytes())?;
/// counter.guard(Region::root(&store)).set(Fixed(1 << 20))?;
/// assert_eq!(store.len(), 4); // no splice: the size stays
/// # Ok::<(), kladde_store::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Fixed<T>(pub T);

impl<T: Persistable> Fixed<T> {
    /// The size of every value.
    const SIZE: usize = match T::FIXED_FORM_SIZE {
        Some(size) => size,
        None => panic!("Fixed<T> needs a T with a fixed form"),
    };
}

impl<T: Persistable> Persistable for Fixed<T> {
    const FIXED_SIZE: Option<usize> = Some(Self::SIZE);
    const FIXED_FORM_SIZE: Option<usize> = Some(Self::SIZE);

    fn encoded_len(&self) -> usize {
        Self::SIZE
    }

    fn encode(&self, out: &mut Encoder<'_>) {
        self.0.encode_fixed(out)
    }

    fn decode(input: &mut Decoder<'_>) -> Result<Self, Error> {
        T::decode_fixed(input).map(Fixed)
    }

    fn encode_fixed(&self, out: &mut Encoder<'_>) {
        self.0.encode_fixed(out)
    }

    fn decode_fixed(input: &mut Decoder<'_>) -> Result<Self, Error> {
        T::decode_fixed(input).map(Fixed)
    }

    type Guard<'r, B: Backend + 'r>
        = SetGuard<'r, Self, B>
    where
        Self: 'r;

    fn guard<'r, B: Backend + 'r>(&'r mut self, region: Region<'r, B>) -> Self::Guard<'r, B> {
        SetGuard::new(self, region)
    }

    fn describe_local(builder: &mut SchemaBuilder) -> TypeDescriptor
    where
        Self: 'static,
    {
        TypeDescriptor::Fixed(T::describe(builder))
    }
}

impl<T> std::ops::Deref for Fixed<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> std::ops::DerefMut for Fixed<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<T> From<T> for Fixed<T> {
    fn from(value: T) -> Self {
        Fixed(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_are_enums() {
        assert_eq!(None::<u32>.to_bytes(), [0]);
        assert_eq!(Some(300u32).to_bytes(), [1, 0xac, 0x02]);
        assert_eq!(<Option<()>>::FIXED_SIZE, Some(1));
        assert_eq!(<Option<u8>>::FIXED_SIZE, None);
        let table = <Option<u32>>::schema();
        assert_eq!(table.fixed_form_size(TypeRef(0)), Some(5));
        assert!(<Option<u8>>::from_bytes(&[2]).is_err());
    }

    #[test]
    fn fixed_values_take_their_largest_form() {
        assert_eq!(Fixed(None::<u16>).to_bytes(), [0, 0, 0]);
        assert_eq!(Fixed(Some(1u16)).to_bytes(), [1, 1, 0]);
        assert_eq!(
            <Fixed<Option<u16>>>::from_bytes(&[1, 1, 0]).unwrap(),
            Fixed(Some(1))
        );
        assert!(<Fixed<Option<u16>>>::from_bytes(&[0, 1, 0]).is_err());
        let table = <Fixed<(u32, char)>>::schema();
        assert_eq!(table.fixed_size(TypeRef(0)), Some(8));
        assert_eq!(
            table.measure(TypeRef(0), &Fixed((7u32, 'é')).to_bytes()),
            Some(8)
        );
    }

    #[test]
    fn a_box_is_what_it_holds() {
        assert_eq!(<Box<(u8, u8)>>::schema(), <(u8, u8)>::schema());
        assert_eq!(Box::new(-3i32).to_bytes(), (-3i32).to_bytes());
    }
}
