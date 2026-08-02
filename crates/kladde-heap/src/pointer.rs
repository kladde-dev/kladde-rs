//! Concrete pointer types, shared across all allocators.
//!
//! Only [`Pointer`] carries the raw width `W`; everything above it (the owned
//! handles, and higher up `Persistable`/containers) parameterizes over the
//! *pointer type* `P`, defaulted to `Pointer`. See `generic-allocator.md`,
//! "Pointer types (concrete)".
//!
//! **Every `Pointer` is nonzero by construction** (its field is `W::NonZero`),
//! so `Option<Pointer>` gets the null niche in memory for free, and the on-file
//! null niche (see `kladde-persist`'s `PointerRepr`) is sound.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

use crate::word::Word;

/// A `Copy`, type- and size-erased identity: the serialized/at-rest form of a
/// pointer and the anchor of a `Location`. The **only** type parameterized over
/// the raw width `W` (a plain integer; the field is its `NonZero` form, so every
/// `Pointer` is nonzero).
///
/// The field is private: construct via [`Pointer::from_raw`] (checked) or
/// [`Pointer::from_nonzero`], read via [`Pointer::raw`]/[`Pointer::nonzero`].
/// Privacy buys representation independence -- e.g. later packing a
/// fixed/resizable flag bit into the id without touching call sites.
///
/// The trait impls below are hand-written rather than derived so they carry the
/// exactly-right bounds (`W: Word`) instead of the spurious `W: Clone`/`W: Eq`/…
/// that `#[derive]` would attach to the *width* parameter.
pub struct Pointer<W: Word = u32>(W::NonZero);

impl<W: Word> Pointer<W> {
    /// Construct from a raw width value; `None` if `raw` is zero (zero is the
    /// reserved null pattern and never a valid pointer).
    #[inline]
    pub fn from_raw(raw: W) -> Option<Self> {
        raw.to_nonzero().map(Self)
    }

    /// Construct directly from the nonzero form.
    #[inline]
    pub fn from_nonzero(nz: W::NonZero) -> Self {
        Self(nz)
    }

    /// The raw width value (always nonzero).
    #[inline]
    pub fn raw(self) -> W {
        W::from_nonzero(self.0)
    }

    /// The nonzero form of the id.
    #[inline]
    pub fn nonzero(self) -> W::NonZero {
        self.0
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
impl<W: Word> Hash for Pointer<W> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state)
    }
}
impl<W: Word> fmt::Debug for Pointer<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Format via `to_usize` so no `W::NonZero: Debug` bound is needed.
        write!(f, "Pointer({})", self.raw().to_usize())
    }
}

/// Owned, single-owner handle to a **resizable** region. Not `Copy` (single
/// owner). Parameterized over the pointer type `P` (default `Pointer`).
#[derive(PartialEq, Eq, Debug)]
pub struct UniquePointerResizable<P = Pointer>(P);

/// Owned, single-owner handle to a **fixed-size** region.
#[derive(PartialEq, Eq, Debug)]
pub struct UniquePointerFixedSize<P = Pointer>(P);

/// The typed `Box<T>`: a fixed-size handle plus a phantom `T`. This is just a
/// `UniquePointerFixedSize<P>` that remembers what it points at.
pub struct UniquePointer<T, P = Pointer> {
    inner: UniquePointerFixedSize<P>,
    _marker: PhantomData<*const T>,
}

/// The resolved kind of a `Pointer`, recovered by `Allocator::resolve` during
/// `load`. Carries the sizedness the raw id doesn't.
#[derive(PartialEq, Eq, Debug)]
pub enum ResolvedPointer<P = Pointer> {
    Resizable(UniquePointerResizable<P>),
    Fixed(UniquePointerFixedSize<P>),
}

impl<P: Copy> UniquePointerResizable<P> {
    /// Wrap a raw id as a resizable handle. By convention only the allocator (or
    /// a `load`) mints owners; see `Allocator::resolve`.
    #[inline]
    pub fn from_pointer(p: P) -> Self {
        Self(p)
    }
    /// The raw id (inherent, because pointers are concrete).
    #[inline]
    pub fn raw(&self) -> P {
        self.0
    }
}

impl<P: Copy> UniquePointerFixedSize<P> {
    #[inline]
    pub fn from_pointer(p: P) -> Self {
        Self(p)
    }
    #[inline]
    pub fn raw(&self) -> P {
        self.0
    }
}

impl<T, P: Copy> UniquePointer<T, P> {
    /// Promote a fixed-size handle to a typed one (the inherent replacement for
    /// the old `promote` extension method).
    #[inline]
    pub fn from_fixed(inner: UniquePointerFixedSize<P>) -> Self {
        Self {
            inner,
            _marker: PhantomData,
        }
    }
    /// Demote back to an untyped fixed-size handle.
    #[inline]
    pub fn into_fixed(self) -> UniquePointerFixedSize<P> {
        self.inner
    }
    #[inline]
    pub fn raw(&self) -> P {
        self.inner.raw()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    #[test]
    fn option_pointer_uses_the_null_niche_in_memory() {
        // The whole point of the nonzero field: `Option<Pointer>` is the same
        // size as `Pointer` at both supported widths.
        assert_eq!(size_of::<Option<Pointer>>(), size_of::<Pointer>());
        assert_eq!(size_of::<Pointer>(), 4);
        assert_eq!(size_of::<Option<Pointer<u64>>>(), size_of::<Pointer<u64>>());
        assert_eq!(size_of::<Pointer<u64>>(), 8);
    }

    #[test]
    fn from_raw_rejects_zero_and_round_trips_nonzero() {
        assert!(Pointer::<u32>::from_raw(0).is_none());
        let p = Pointer::<u32>::from_raw(42).unwrap();
        assert_eq!(p.raw(), 42);
    }

    #[test]
    fn handles_expose_raw_ids() {
        let p = Pointer::<u32>::from_raw(3).unwrap();
        assert_eq!(UniquePointerResizable::from_pointer(p).raw(), p);
        assert_eq!(UniquePointerFixedSize::from_pointer(p).raw(), p);
        let typed = UniquePointer::<i32, _>::from_fixed(UniquePointerFixedSize::from_pointer(p));
        assert_eq!(typed.raw(), p);
        assert_eq!(typed.into_fixed().raw(), p);
    }
}
