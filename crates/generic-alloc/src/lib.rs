//! Throwaway prototype for the *concrete-pointer* design decided in
//! `generic-allocator.md`. Validates that these compile and compose:
//!
//! - `Pointer<W = NonZeroU32>(W)` — the only type parameterized over the raw
//!   width integer `W`;
//! - owned handles parameterized over the *pointer type* `P` (default
//!   `Pointer`), never over `W`, with inherent `.raw()` / `.into_fixed()`;
//! - `Persistable<P = Pointer>`, whose methods pin the backend's pointer type
//!   to `P` via `Backend<Pointer = P>`;
//! - a container `PersistableVec<T, P = Pointer>` and its impl, with an
//!   `INLINE_SIZE` that depends on `P`'s width via `size_of::<P>()`;
//! - the three implementor styles from the doc's guidance (all-`P`, generic
//!   container, and default-only);
//! - default type parameters making the common case parameter-free
//!   (`PersistableVec<i32>` round-trips through a `Pointer`-width backend).

use std::marker::PhantomData;
use std::num::NonZeroU32;

// ============================ pointer types ============================

/// A `Copy`, type- and size-erased identity: the serialized/at-rest form of a
/// pointer and the `anchor` of a [`Location`]. The *only* type parameterized
/// over the raw width integer `W` (default `NonZeroU32`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Pointer<W = NonZeroU32>(pub W);

/// Owned, single-owner handle to a resizable region. Parameterized over the
/// *pointer type* `P` (default `Pointer`), not over `W`.
#[derive(PartialEq, Eq, Debug)]
pub struct UniquePointerResizable<P = Pointer>(P);

/// Owned, single-owner handle to a fixed-size region.
#[derive(PartialEq, Eq, Debug)]
pub struct UniquePointerFixedSize<P = Pointer>(P);

/// The typed `Box<T>`: a fixed-size handle plus a phantom `T`.
pub struct UniquePointer<T, P = Pointer> {
    inner: UniquePointerFixedSize<P>,
    _marker: PhantomData<*const T>,
}

impl<P: Copy> UniquePointerResizable<P> {
    pub fn from_pointer(p: P) -> Self {
        Self(p)
    }
    /// Inherent `.raw()` — available because pointers are concrete.
    pub fn raw(&self) -> P {
        self.0
    }
}
impl<P: Copy> UniquePointerFixedSize<P> {
    pub fn from_pointer(p: P) -> Self {
        Self(p)
    }
    pub fn raw(&self) -> P {
        self.0
    }
}
impl<T, P: Copy> UniquePointer<T, P> {
    pub fn from_fixed(inner: UniquePointerFixedSize<P>) -> Self {
        Self {
            inner,
            _marker: PhantomData,
        }
    }
    pub fn into_fixed(self) -> UniquePointerFixedSize<P> {
        self.inner
    }
    pub fn raw(&self) -> P {
        self.inner.raw()
    }
}

// =============================== location ===============================

#[derive(Clone, Copy)]
pub struct Location<P = Pointer> {
    pub anchor: P,
    pub offset: u32,
}

// =========================== backend / allocator ===========================

/// The pointer-facing backend. `type Pointer` is a *concrete* `Pointer<W>`
/// (here `NonZeroU32`-wide by default); it is an associated type only so a
/// `Persistable<P>` can pin `P == B::Pointer`. Because it is a concrete type,
/// inherent pointer methods still work — no opacity.
pub trait Backend {
    type Pointer: Copy;

    fn alloc_resizable(&self, size: usize) -> UniquePointerResizable<Self::Pointer>;
    fn alloc_fixed(&self, size: usize) -> UniquePointerFixedSize<Self::Pointer>;
    fn free_resizable(&self, p: UniquePointerResizable<Self::Pointer>);
    fn free_fixed(&self, p: UniquePointerFixedSize<Self::Pointer>);
    fn resize(&self, p: &UniquePointerResizable<Self::Pointer>, new_size: usize);

    fn write(&self, anchor: Self::Pointer, offset: u32, bytes: &[u8]);
    fn read(&self, anchor: Self::Pointer, offset: u32, len: u32) -> Vec<u8>;
}

/// Typed conveniences (would live in the `Persistable` crate). Blanket-impl'd.
pub trait BackendExt: Backend {
    fn alloc_typed<T: Persistable<Self::Pointer>>(&self) -> UniquePointer<T, Self::Pointer> {
        UniquePointer::from_fixed(self.alloc_fixed(T::INLINE_SIZE))
    }
    fn free_typed<T>(&self, p: UniquePointer<T, Self::Pointer>) {
        self.free_fixed(p.into_fixed())
    }
}
impl<B: Backend + ?Sized> BackendExt for B {}

// ============================== persistable ==============================

/// Parameterized over the *pointer type* `P` (default `Pointer`). A type's
/// `store`/`load` only accept backends whose pointer type is `P`.
pub trait Persistable<P = Pointer>: Sized {
    const INLINE_SIZE: usize;
    fn store<B: Backend<Pointer = P>>(&mut self, backend: &B, location: Location<P>);
    fn load<B: Backend<Pointer = P>>(backend: &B, location: Location<P>) -> Self;
}

// Style (1) from the doc: a type that stores no pointers can be `Persistable`
// for *every* `P` -- works with any pointer width.
impl<P> Persistable<P> for i32 {
    const INLINE_SIZE: usize = 4;
    fn store<B: Backend<Pointer = P>>(&mut self, backend: &B, location: Location<P>) {
        backend.write(location.anchor, location.offset, &self.to_le_bytes());
    }
    fn load<B: Backend<Pointer = P>>(backend: &B, location: Location<P>) -> Self {
        let bytes = backend.read(location.anchor, location.offset, 4);
        i32::from_le_bytes(bytes.try_into().unwrap())
    }
}

// Style (2): a container that *does* store pointers is generic over `P`
// (default `Pointer`), and stores `P`-typed owned handles.
pub struct PersistableVec<T, P = Pointer> {
    data: Vec<T>,
    pointer: Option<UniquePointerResizable<P>>,
}

impl<T, P: Copy> PersistableVec<T, P> {
    pub fn new() -> Self {
        Self {
            data: Vec::new(),
            pointer: None,
        }
    }
    pub fn len(&self) -> usize {
        self.data.len()
    }
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
    pub fn push_in_memory(&mut self, value: T) {
        self.data.push(value);
    }
    pub fn get(&self, i: usize) -> Option<&T> {
        self.data.get(i)
    }
}

impl<T: Persistable<P>, P: Copy> Persistable<P> for PersistableVec<T, P> {
    // Just the pointer id -- the length/size is owned by the allocator (query
    // `size`), not stored inline. Empty is the null pointer, free via
    // `Option<P>`'s niche, so this is `size_of::<P>()` for a NonZero id.
    const INLINE_SIZE: usize = std::mem::size_of::<P>();

    fn store<B: Backend<Pointer = P>>(&mut self, backend: &B, location: Location<P>) {
        let byte_size = self.data.len() * T::INLINE_SIZE;
        let pointer = self
            .pointer
            .take()
            .unwrap_or_else(|| backend.alloc_resizable(byte_size));
        for (i, item) in self.data.iter_mut().enumerate() {
            item.store(
                backend,
                Location {
                    anchor: pointer.raw(),
                    offset: (i * T::INLINE_SIZE) as u32,
                },
            );
        }
        // inline header: just the target pointer id (INLINE_SIZE bytes), no len
        backend.write(location.anchor, location.offset, &vec![0u8; Self::INLINE_SIZE]);
        self.pointer = Some(pointer);
    }

    fn load<B: Backend<Pointer = P>>(_backend: &B, _location: Location<P>) -> Self {
        // (reconstruction elided; the shape is what we're checking)
        Self::new()
    }
}

// Style (3): a type that doesn't care about non-default widths implements
// `Persistable` only for the default `P = Pointer`, with no `P` generic.
#[allow(dead_code)] // compile-check only
struct DefaultOnly(i32);
impl Persistable for DefaultOnly {
    const INLINE_SIZE: usize = 4;
    fn store<B: Backend<Pointer = Pointer>>(&mut self, backend: &B, location: Location) {
        self.0.store(backend, location);
    }
    fn load<B: Backend<Pointer = Pointer>>(backend: &B, location: Location) -> Self {
        DefaultOnly(i32::load(backend, location))
    }
}

// ================================ mock ================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::num::NonZeroU64;

    #[derive(Default)]
    struct MockBackend {
        regions: RefCell<HashMap<NonZeroU32, Vec<u8>>>,
        next: Cell<u32>,
    }
    impl MockBackend {
        fn fresh(&self, size: usize) -> Pointer {
            let raw = self.next.get() + 1;
            self.next.set(raw);
            let id = NonZeroU32::new(raw).unwrap();
            self.regions.borrow_mut().insert(id, vec![0u8; size]);
            Pointer(id)
        }
    }
    impl Backend for MockBackend {
        type Pointer = Pointer; // = Pointer<NonZeroU32>

        fn alloc_resizable(&self, size: usize) -> UniquePointerResizable<Pointer> {
            UniquePointerResizable::from_pointer(self.fresh(size))
        }
        fn alloc_fixed(&self, size: usize) -> UniquePointerFixedSize<Pointer> {
            UniquePointerFixedSize::from_pointer(self.fresh(size))
        }
        fn free_resizable(&self, p: UniquePointerResizable<Pointer>) {
            self.regions.borrow_mut().remove(&p.raw().0);
        }
        fn free_fixed(&self, p: UniquePointerFixedSize<Pointer>) {
            self.regions.borrow_mut().remove(&p.raw().0);
        }
        fn resize(&self, p: &UniquePointerResizable<Pointer>, new_size: usize) {
            if let Some(r) = self.regions.borrow_mut().get_mut(&p.raw().0) {
                r.resize(new_size, 0);
            }
        }
        fn write(&self, anchor: Pointer, offset: u32, bytes: &[u8]) {
            let mut regions = self.regions.borrow_mut();
            let r = regions.get_mut(&anchor.0).unwrap();
            let start = offset as usize;
            if r.len() < start + bytes.len() {
                r.resize(start + bytes.len(), 0);
            }
            r[start..start + bytes.len()].copy_from_slice(bytes);
        }
        fn read(&self, anchor: Pointer, offset: u32, len: u32) -> Vec<u8> {
            let regions = self.regions.borrow();
            let r = &regions[&anchor.0];
            r[offset as usize..(offset + len) as usize].to_vec()
        }
    }

    #[test]
    fn default_pointer_common_case_is_parameter_free() {
        let backend = MockBackend::default();
        let root = backend.alloc_fixed(PersistableVec::<i32>::INLINE_SIZE);

        // `PersistableVec::<i32>` — `P` defaults to `Pointer`; no width in sight.
        let mut v = PersistableVec::<i32>::new();
        v.push_in_memory(10);
        v.push_in_memory(20);
        v.store(
            &backend,
            Location {
                anchor: root.raw(),
                offset: 0,
            },
        );
        assert_eq!(v.len(), 2);

        // scalar `Persistable<P>` for all P, and a typed box, both compile:
        let boxed = backend.alloc_typed::<i32>();
        assert_eq!(std::mem::size_of_val(&boxed.raw()), 4);
        backend.free_typed(boxed);
    }

    #[test]
    fn inline_size_is_just_the_pointer_width() {
        // The inline representation is just the pointer id -- the length/size
        // lives in the allocator, not inline. Default `Pointer<NonZeroU32>` is
        // 4 bytes; a wider `NonZeroU64` id is 8. (Per-`P` via `size_of::<P>()`.)
        assert_eq!(<PersistableVec<i32> as Persistable>::INLINE_SIZE, 4);
        assert_eq!(
            <PersistableVec<i32, Pointer<NonZeroU64>> as Persistable<Pointer<NonZeroU64>>>::INLINE_SIZE,
            8
        );
    }
}
