//! Allocation ids as pointers: the copyable [`Pointer`], the owned
//! [`UniquePointer`], the [`Word`] they are built from, and their on-file
//! encoding ([`PointerRepr`]).

use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::{Add, AddAssign, Sub, SubAssign};

/// An unsigned integer word: offsets and sizes within allocations, and the
/// raw width of a [`Pointer`].
///
/// Implemented for `u32` and `u64`; code generic over a backend uses it to
/// do arithmetic on sizes without knowing their width.
///
/// ```
/// use kladde_store::Word;
///
/// fn end<W: Word>(offset: W, len: W) -> W {
///     offset + len
/// }
/// assert_eq!(end(3u32, 4u32), 7);
/// assert_eq!(<u32 as Word>::try_from_usize(1 << 40), None);
/// assert_eq!(Word::to_bytes(0x0102u32), [2, 1, 0, 0]);
/// ```
///
/// # Safety
///
/// `NonZero` must be a genuine null-niche type for `Self`, with
/// `to_nonzero(x) == None` exactly when `x == zero()`, and `to_bytes` and
/// `from_bytes` must round-trip through the canonical little-endian form.
/// Downstream code relies on both for memory layout and on-file correctness.
pub unsafe trait Word:
    Copy + Ord + Hash + fmt::Debug + Add<Output = Self> + Sub<Output = Self> + AddAssign + SubAssign
{
    /// The null-niche counterpart of `Self`.
    type NonZero: Copy + Eq + Hash;
    /// The little-endian byte array of `Self`.
    type Bytes: Copy + AsRef<[u8]>;

    /// Zero.
    fn zero() -> Self;
    /// Truncating conversion from `usize`.
    fn from_usize(n: usize) -> Self;
    /// Conversion to `usize`, truncating where `usize` is narrower.
    fn to_usize(self) -> usize;
    /// Checked conversion from `usize`: `None` if `n` does not fit.
    fn try_from_usize(n: usize) -> Option<Self>;
    /// The nonzero counterpart, or `None` for zero.
    fn to_nonzero(self) -> Option<Self::NonZero>;
    /// The value of a nonzero counterpart.
    fn from_nonzero(nz: Self::NonZero) -> Self;
    /// The little-endian bytes.
    fn to_bytes(self) -> Self::Bytes;
    /// The value of little-endian bytes.
    fn from_bytes(bytes: Self::Bytes) -> Self;
    /// The value of the first `size_of::<Self::Bytes>()` bytes of `bytes`,
    /// little-endian. Panics if `bytes` is shorter.
    fn from_bytes_slice(bytes: &[u8]) -> Self;
}

macro_rules! impl_word {
    ($($int:ty => $nz:ty, $bytes:literal);* $(;)?) => {
        $(
            // SAFETY: `$nz` is the standard library's null-niche type for `$int`,
            // `new` returns `None` exactly at 0, and `to_le_bytes`/`from_le_bytes`
            // are the canonical little-endian round trip.
            unsafe impl Word for $int {
                type NonZero = $nz;
                type Bytes = [u8; $bytes];
                #[inline] fn zero() -> Self { 0 }
                #[inline] fn from_usize(n: usize) -> Self { n as $int }
                #[inline] fn to_usize(self) -> usize { self as usize }
                #[inline] fn try_from_usize(n: usize) -> Option<Self> { <$int>::try_from(n).ok() }
                #[inline] fn to_nonzero(self) -> Option<Self::NonZero> { <$nz>::new(self) }
                #[inline] fn from_nonzero(nz: Self::NonZero) -> Self { nz.get() }
                #[inline] fn to_bytes(self) -> Self::Bytes { self.to_le_bytes() }
                #[inline] fn from_bytes(bytes: Self::Bytes) -> Self { <$int>::from_le_bytes(bytes) }
                #[inline] fn from_bytes_slice(bytes: &[u8]) -> Self {
                    let mut arr = [0u8; $bytes];
                    arr.copy_from_slice(&bytes[..$bytes]);
                    <$int>::from_le_bytes(arr)
                }
            }
        )*
    };
}

impl_word! {
    u32 => std::num::NonZeroU32, 4;
    u64 => std::num::NonZeroU64, 8;
}

/// An allocation's id: the serialized form of a pointer, and the anchor of a
/// location. `Copy`, and it confers no rights: owning an allocation is what
/// [`UniquePointer`] is for.
///
/// Every `Pointer` is nonzero, so `Option<Pointer>` is the size of `Pointer`,
/// and on file the all-zero bytes are free to mean `None`.
///
/// ```
/// use kladde_store::Pointer;
///
/// let p = Pointer::<u32>::from_raw(7).unwrap();
/// assert_eq!(p.raw(), 7);
/// assert!(Pointer::<u32>::from_raw(0).is_none());
/// assert_eq!(std::mem::size_of::<Option<Pointer>>(), 4);
/// ```
pub struct Pointer<W: Word = u32>(W::NonZero);

impl<W: Word> Pointer<W> {
    /// The pointer with id `raw`, or `None` for zero, which is never an id.
    #[inline]
    pub fn from_raw(raw: W) -> Option<Self> {
        raw.to_nonzero().map(Self)
    }

    /// The pointer with the nonzero id `nz`.
    #[inline]
    pub fn from_nonzero(nz: W::NonZero) -> Self {
        Self(nz)
    }

    /// The id.
    #[inline]
    pub fn raw(self) -> W {
        W::from_nonzero(self.0)
    }
}

impl<W: Word> Clone for Pointer<W> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
impl<W: Word> Copy for Pointer<W> {}
impl<W: Word> PartialEq for Pointer<W> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl<W: Word> Eq for Pointer<W> {}
impl<W: Word> PartialOrd for Pointer<W> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<W: Word> Ord for Pointer<W> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.raw().cmp(&other.raw())
    }
}
impl<W: Word> Hash for Pointer<W> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state)
    }
}
impl<W: Word> fmt::Debug for Pointer<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pointer({:?})", self.raw())
    }
}

/// The owned handle to an allocation: single-owner and not `Copy`.
///
/// Holding one is the claim to the allocation, which is why resizing and
/// freeing take it. It is untyped: the value that holds it knows what the
/// allocation's bytes mean. Application code never holds one directly; the
/// containers and derived types do.
///
/// ```
/// use kladde_store::{Pointer, UniquePointer};
///
/// let owned = UniquePointer::from_pointer(Pointer::<u32>::from_raw(3).unwrap());
/// assert_eq!(owned.raw().raw(), 3);
/// ```
#[derive(PartialEq, Eq, Debug)]
pub struct UniquePointer<P = Pointer>(P);

impl<P: Copy> UniquePointer<P> {
    /// Claims the allocation `p`. Only a backend minting a new allocation, or
    /// a `load` recovering the owner of one it reads, should call this: a
    /// second owner of the same allocation breaks the ownership tree.
    #[inline]
    pub fn from_pointer(p: P) -> Self {
        Self(p)
    }

    /// The allocation's id.
    #[inline]
    pub fn raw(&self) -> P {
        self.0
    }
}

/// The on-file encoding of a pointer: its id, little-endian, at its width.
///
/// ```
/// use kladde_store::{Pointer, PointerRepr};
///
/// let p = Pointer::<u32>::from_raw(258).unwrap();
/// assert_eq!(p.to_bytes(), [2, 1, 0, 0]);
/// assert_eq!(<Pointer as PointerRepr>::from_slice(&[2, 1, 0, 0, 9]), p);
/// assert_eq!(<Pointer as PointerRepr>::BYTE_LEN, 4);
/// ```
pub trait PointerRepr: Copy + Eq + Hash + fmt::Debug {
    /// `[u8; BYTE_LEN]`.
    type Bytes: Copy + AsRef<[u8]>;
    /// The encoding's width in bytes.
    const BYTE_LEN: usize;
    /// The encoding of a pointer.
    fn to_bytes(self) -> Self::Bytes;
    /// Decodes a nonzero id; the all-zero pattern belongs to `None`, see
    /// [`decode_option`].
    fn from_bytes(bytes: Self::Bytes) -> Self;
    /// Decodes from the first `BYTE_LEN` bytes of a slice.
    fn from_slice(bytes: &[u8]) -> Self;
    /// The all-zero encoding of `None`.
    fn zeroed_bytes() -> Self::Bytes;
    /// The id as a `u32`, which is what the store names allocations by.
    fn to_u32(self) -> u32;
    /// The pointer with id `id`, which must be nonzero.
    fn from_u32(id: u32) -> Self;
}

impl<W: Word> PointerRepr for Pointer<W> {
    type Bytes = W::Bytes;
    const BYTE_LEN: usize = std::mem::size_of::<W::Bytes>();

    #[inline]
    fn to_bytes(self) -> W::Bytes {
        self.raw().to_bytes()
    }
    #[inline]
    fn from_bytes(bytes: W::Bytes) -> Self {
        Pointer::from_raw(W::from_bytes(bytes)).expect("a nonzero pointer id")
    }
    #[inline]
    fn from_slice(bytes: &[u8]) -> Self {
        Pointer::from_raw(W::from_bytes_slice(bytes)).expect("a nonzero pointer id")
    }
    #[inline]
    fn zeroed_bytes() -> W::Bytes {
        W::zero().to_bytes()
    }
    #[inline]
    fn to_u32(self) -> u32 {
        self.raw().to_usize() as u32
    }
    #[inline]
    fn from_u32(id: u32) -> Self {
        Pointer::from_raw(W::from_usize(id as usize)).expect("a nonzero pointer id")
    }
}

/// Encodes `Option<P>`, with all-zero bytes for `None`.
///
/// ```
/// use kladde_store::{encode_option, decode_option, Pointer};
///
/// let p = Pointer::<u32>::from_raw(7);
/// assert_eq!(encode_option(p), [7, 0, 0, 0]);
/// assert_eq!(encode_option::<Pointer>(None), [0, 0, 0, 0]);
/// assert_eq!(decode_option::<Pointer>([7, 0, 0, 0]), p);
/// ```
#[inline]
pub fn encode_option<P: PointerRepr>(p: Option<P>) -> P::Bytes {
    p.map_or_else(P::zeroed_bytes, P::to_bytes)
}

/// Decodes `Option<P>`: all-zero bytes are `None`.
///
/// ```
/// use kladde_store::{decode_option, Pointer};
///
/// assert_eq!(decode_option::<Pointer>([0, 0, 0, 0]), None);
/// assert_eq!(decode_option::<Pointer>([1, 0, 0, 0]), Pointer::from_raw(1));
/// ```
#[inline]
pub fn decode_option<P: PointerRepr>(bytes: P::Bytes) -> Option<P> {
    if bytes.as_ref().iter().all(|&b| b == 0) {
        None
    } else {
        Some(P::from_bytes(bytes))
    }
}

/// Like [`decode_option`], from the first `P::BYTE_LEN` bytes of a slice.
/// Panics if the slice is shorter.
///
/// ```
/// use kladde_store::{decode_option_slice, Pointer};
///
/// let record = [5, 0, 0, 0, 0xff];
/// assert_eq!(decode_option_slice::<Pointer>(&record), Pointer::from_raw(5));
/// ```
#[inline]
pub fn decode_option_slice<P: PointerRepr>(bytes: &[u8]) -> Option<P> {
    let bytes = &bytes[..P::BYTE_LEN];
    if bytes.iter().all(|&b| b == 0) {
        None
    } else {
        Some(P::from_slice(bytes))
    }
}
