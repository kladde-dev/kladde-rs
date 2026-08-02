//! Throwaway prototype for the *concrete-pointer* design decided in
//! `generic-allocator.md`, with the composition-not-extension restructure and
//! the `Size`-parametric `Location`. Validates that these compile and compose:
//!
//! - `Pointer<W = NonZeroU32>(W)` — the only type parameterized over the raw
//!   width integer `W`; owned handles parameterize over the *pointer type* `P`.
//! - `Location<P = Pointer, S = u32>` — anchor of type `P`, offset of type `S`.
//! - `Allocator`/`ReadBackend`/`WriteBackend` each carry an associated `Size`
//!   (a `Word`); offsets and allocation sizes are `Size`-typed, not `usize`.
//! - `Persistable<P>` stays *single-parameter*: its `store`/`load` name the
//!   size as `Location<P, B::Size>`, so `S` flows from the backend and never
//!   becomes a `Persistable` type parameter.
//! - `Allocator` is a `&mut self` API with addresses core to it; backends are
//!   *composed of* an `Allocator`, split into `ReadBackend` (read = `&mut
//!   self`) and `WriteBackend` (write = `&self`), both extending a shared
//!   `Backend` supertrait that carries the one `Pointer`/`Size` per backend.

use std::io::{self, Read, Seek, Write};
use std::marker::PhantomData;
use std::num::NonZeroU32;

#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::io::SeekFrom;

// ============================== word bound ==============================

/// The "unsigned integer" bound std doesn't provide, minimized to what the
/// prototype needs. In the real crate this is where `+`, `<`, `Into<Address>`,
/// etc. would live (see the Word-bound problem in the doc).
pub trait Word: Copy {
    fn from_usize(n: usize) -> Self;
    fn to_usize(self) -> usize;
}
impl Word for u32 {
    fn from_usize(n: usize) -> Self {
        n as u32
    }
    fn to_usize(self) -> usize {
        self as usize
    }
}
impl Word for u64 {
    fn from_usize(n: usize) -> Self {
        n as u64
    }
    fn to_usize(self) -> usize {
        self as usize
    }
}
impl Word for usize {
    fn from_usize(n: usize) -> Self {
        n
    }
    fn to_usize(self) -> usize {
        self
    }
}

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

/// `anchor` of type `P`, `offset` of type `S` (a `Size`). `S` defaults to `u32`
/// for standalone naming; in `Persistable` signatures it is `B::Size`.
#[derive(Clone, Copy)]
pub struct Location<P = Pointer, S = u32> {
    pub anchor: P,
    pub offset: S,
}

// ============================== allocator ==============================

/// Crate-local, Kladde-agnostic allocator error. `DanglingPointer`: the id
/// isn't a live allocation (freed / never existed / decoded from corrupt
/// bytes). `Exhausted`: a (re)allocation can't be satisfied. Fallible, not a
/// panic, because a persistence library must surface an invalid *deserialized*
/// id as a recoverable error rather than crash on a corrupt file.
#[derive(Debug, PartialEq, Eq)]
pub enum AllocError {
    DanglingPointer,
    Exhausted,
}

/// A resize's relocation report: `Some((old, new))` iff the bytes moved.
pub type Relocation<Addr> = Option<(Addr, Addr)>;

/// Pure address-range management: a *normal* `&mut self` mutating API, with no
/// knowledge of the bytes stored at those ranges. Addresses are **core** to the
/// `Allocator` (no separate `TransparentAllocator`); a `Backend` hides them.
pub trait Allocator {
    type Pointer: Copy;
    type Address: Word;
    type Size: Word;

    fn alloc_resizable(&mut self, size: Self::Size) -> UniquePointerResizable<Self::Pointer>;
    fn alloc_fixed(&mut self, size: Self::Size) -> UniquePointerFixedSize<Self::Pointer>;
    fn free_resizable(&mut self, p: UniquePointerResizable<Self::Pointer>);
    fn free_fixed(&mut self, p: UniquePointerFixedSize<Self::Pointer>);

    /// `Ok(Some((old, new)))` iff the bytes must move; `Ok(None)` = in-place
    /// resize or an unclaimed (address-less) reservation; `Err` = can't satisfy
    /// the request (or the handle's id isn't live).
    fn resize(
        &mut self,
        p: &UniquePointerResizable<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<Relocation<Self::Address>, AllocError>;

    /// `Err(DanglingPointer)` if `p` isn't a live allocation. A reserved-but-
    /// unclaimed pointer is *not* a valid argument: during journal replay
    /// `claim` always precedes any `address` call, so the reserved state never
    /// reaches here -- there is no valid "no address yet" case to model.
    fn address(&self, p: Self::Pointer) -> Result<Self::Address, AllocError>;
    /// `Err(DanglingPointer)` if `p` isn't a live allocation. Unlike `address`,
    /// a reserved-but-unclaimed pointer *is* a valid argument here (it already
    /// has a size), returning `Ok`; only a non-live id fails.
    fn size(&self, p: Self::Pointer) -> Result<Self::Size, AllocError>;
}

// ============================ backend split ============================

/// Backend-layer error: an [`AllocError`] (e.g. a dangling/corrupt id) *or* a
/// storage I/O failure. The backend touches `Storage`, so its errors are a
/// superset of the pure allocator's. (A real design might make this an
/// associated `type Error`; a concrete enum keeps the prototype simple.)
#[derive(Debug)]
pub enum BackendError {
    Alloc(AllocError),
    Io(io::Error),
}
impl From<AllocError> for BackendError {
    fn from(e: AllocError) -> Self {
        Self::Alloc(e)
    }
}
impl From<io::Error> for BackendError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Shared **type carrier**: every backend has exactly one `Pointer` and one
/// `Size`, declared here once. Because the two halves *extend* `Backend` rather
/// than each declaring their own copies, `B::Pointer`/`B::Size` stay
/// unambiguous even under `ReadBackend + WriteBackend`, and the two halves
/// structurally can't disagree. A bare `B: Backend` bound guarantees only these
/// *types*, not read or write *access* (that's what the two halves are for).
pub trait Backend {
    type Pointer: Copy;
    type Size: Word;
}

/// Read access. `read` is `&mut self`: `load` is *sequential*, so a `&mut`
/// reborrowed down the recursion never needs two live borrows — which is why
/// the read path can hand out the real seekable cursor with no `RefCell`.
pub trait ReadBackend: Backend {
    fn read(&mut self, anchor: Self::Pointer, offset: Self::Size, len: Self::Size) -> Vec<u8>;
    /// Fallible: an id decoded from corrupt bytes surfaces as `Err`, so a
    /// container's `load` propagates it instead of silently defaulting.
    fn size(&self, p: Self::Pointer) -> Result<Self::Size, BackendError>;
}

/// Write access. `write`/`alloc_*`/`resize` are `&self` (guard reborrow model);
/// interior mutability lives inside the backend. Addresses never surface here.
pub trait WriteBackend: Backend {
    fn alloc_resizable(&self, size: Self::Size) -> UniquePointerResizable<Self::Pointer>;
    fn alloc_fixed(&self, size: Self::Size) -> UniquePointerFixedSize<Self::Pointer>;
    fn free_resizable(&self, p: UniquePointerResizable<Self::Pointer>);
    fn free_fixed(&self, p: UniquePointerFixedSize<Self::Pointer>);
    /// Addresses never surface (contrast `Allocator::resize`); the relocation
    /// report is consumed internally to move bytes. Fallible: the move is I/O.
    fn resize(
        &self,
        p: &UniquePointerResizable<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<(), BackendError>;
    fn write(&self, anchor: Self::Pointer, offset: Self::Size, bytes: &[u8]);
    fn size(&self, p: Self::Pointer) -> Result<Self::Size, BackendError>;
}

// A caller needing both just writes `B: ReadBackend + WriteBackend` -- no
// parameterized convenience trait, and `B::Pointer`/`B::Size` stay unambiguous
// because they are declared once, on the shared `Backend` supertrait.

// ============================== persistable ==============================

/// Single-parameter over the *pointer type* `P` (default `Pointer`). The size
/// type is `B::Size`, named via `Location<P, B::Size>` — it never becomes a
/// `Persistable` type parameter.
pub trait Persistable<P = Pointer>: Sized {
    const INLINE_SIZE: usize;
    fn store<B: WriteBackend<Pointer = P>>(&mut self, backend: &B, location: Location<P, B::Size>);
    fn load<B: ReadBackend<Pointer = P>>(backend: &mut B, location: Location<P, B::Size>) -> Self;
}

// Style (1): a pointer-free type is `Persistable` for *every* `P` (any width).
impl<P> Persistable<P> for i32 {
    const INLINE_SIZE: usize = 4;
    fn store<B: WriteBackend<Pointer = P>>(&mut self, backend: &B, location: Location<P, B::Size>) {
        backend.write(location.anchor, location.offset, &self.to_le_bytes());
    }
    fn load<B: ReadBackend<Pointer = P>>(backend: &mut B, location: Location<P, B::Size>) -> Self {
        let len: B::Size = Word::from_usize(4);
        let bytes = backend.read(location.anchor, location.offset, len);
        i32::from_le_bytes(bytes.try_into().unwrap())
    }
}

// Style (2): a container that stores pointers is generic over `P` (default
// `Pointer`) and stores `P`-typed owned handles. `S` is still not a parameter.
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

impl<T, P: Copy> Default for PersistableVec<T, P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Persistable<P>, P: Copy> Persistable<P> for PersistableVec<T, P> {
    // Just the pointer id -- the length/size is owned by the allocator, not
    // stored inline. `size_of::<P>()` for a NonZero id (empty = null pointer).
    const INLINE_SIZE: usize = std::mem::size_of::<P>();

    fn store<B: WriteBackend<Pointer = P>>(&mut self, backend: &B, location: Location<P, B::Size>) {
        // Offsets/sizes are `B::Size`; build them from the usize byte counts.
        let byte_size: B::Size = Word::from_usize(self.data.len() * T::INLINE_SIZE);
        let pointer = self
            .pointer
            .take()
            .unwrap_or_else(|| backend.alloc_resizable(byte_size));
        for (i, item) in self.data.iter_mut().enumerate() {
            let offset: B::Size = Word::from_usize(i * T::INLINE_SIZE);
            item.store(
                backend,
                Location {
                    anchor: pointer.raw(),
                    offset,
                },
            );
        }
        // inline header: just the target pointer id (INLINE_SIZE bytes), no len
        backend.write(
            location.anchor,
            location.offset,
            &vec![0u8; Self::INLINE_SIZE],
        );
        self.pointer = Some(pointer);
    }

    fn load<B: ReadBackend<Pointer = P>>(backend: &mut B, location: Location<P, B::Size>) -> Self {
        // Read the inline pointer id (reconstruction elided). The loop shows the
        // sequential `&mut` read path reborrows cleanly, with `B::Size` offsets.
        let id_len: B::Size = Word::from_usize(Self::INLINE_SIZE);
        let _id_bytes = backend.read(location.anchor, location.offset, id_len);
        // `size` is now fallible: a dangling/corrupt id surfaces as `Err`
        // instead of silently defaulting to 0. (A real, fallible `load` would
        // `?`-propagate it; this prototype keeps `load -> Self`, so it panics
        // loudly rather than defaulting -- the point is that it can't be ignored.)
        let n = backend
            .size(location.anchor)
            .expect("prototype: root allocation must be live")
            .to_usize()
            / T::INLINE_SIZE.max(1);
        let mut data = Vec::new();
        for i in 0..n {
            let offset: B::Size = Word::from_usize(i * T::INLINE_SIZE);
            let item = T::load(
                backend,
                Location {
                    anchor: location.anchor,
                    offset,
                },
            );
            data.push(item);
        }
        Self {
            data,
            pointer: None,
        }
    }
}

// Style (3): a type that doesn't care about non-default widths implements
// `Persistable` only for the default `P = Pointer`, with no `P` generic.
#[allow(dead_code)] // compile-check only
struct DefaultOnly(i32);
impl Persistable for DefaultOnly {
    const INLINE_SIZE: usize = 4;
    fn store<B: WriteBackend<Pointer = Pointer>>(
        &mut self,
        backend: &B,
        location: Location<Pointer, B::Size>,
    ) {
        self.0.store(backend, location);
    }
    fn load<B: ReadBackend<Pointer = Pointer>>(
        backend: &mut B,
        location: Location<Pointer, B::Size>,
    ) -> Self {
        DefaultOnly(i32::load(backend, location))
    }
}

// ============================ storage (mock) ============================

/// Unstructured byte store: `Read + Write + Seek` plus resize/len.
pub trait Storage: Read + Write + Seek {
    fn resize(&mut self, new_len: u64) -> io::Result<()>;
    fn len(&self) -> io::Result<u64>;
    fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }
}

// test-only, in-memory: a Cursor gives it the Seek that a bare Vec<u8> lacks.
#[cfg(test)]
#[derive(Default)]
struct MockStorage(std::io::Cursor<Vec<u8>>);
#[cfg(test)]
impl Read for MockStorage {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}
#[cfg(test)]
impl Write for MockStorage {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
#[cfg(test)]
impl Seek for MockStorage {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.seek(pos)
    }
}
#[cfg(test)]
impl Storage for MockStorage {
    fn resize(&mut self, new_len: u64) -> io::Result<()> {
        self.0.get_mut().resize(new_len as usize, 0);
        Ok(())
    }
    fn len(&self) -> io::Result<u64> {
        Ok(self.0.get_ref().len() as u64)
    }
}

// ========================= composed backend (mock) =========================

/// A concrete `&mut self` bump `Allocator`. `Address = usize`, `Size = u32` --
/// a genuinely non-`usize` `Size`, exercising the `Word` conversions.
#[cfg(test)]
#[derive(Default)]
struct MockAllocator {
    table: HashMap<NonZeroU32, Row>,
    next_id: u32,
    bump: usize,
}
#[cfg(test)]
#[derive(Clone, Copy)]
struct Row {
    address: usize,
    size: usize,
}
#[cfg(test)]
impl MockAllocator {
    fn fresh(&mut self, size: usize) -> Pointer {
        self.next_id += 1;
        let id = NonZeroU32::new(self.next_id).unwrap();
        let address = self.bump;
        self.bump += size;
        self.table.insert(id, Row { address, size });
        Pointer(id)
    }
}
#[cfg(test)]
impl Allocator for MockAllocator {
    type Pointer = Pointer;
    type Address = usize;
    type Size = u32;
    fn alloc_resizable(&mut self, size: u32) -> UniquePointerResizable<Pointer> {
        UniquePointerResizable::from_pointer(self.fresh(size.to_usize()))
    }
    fn alloc_fixed(&mut self, size: u32) -> UniquePointerFixedSize<Pointer> {
        UniquePointerFixedSize::from_pointer(self.fresh(size.to_usize()))
    }
    fn free_resizable(&mut self, p: UniquePointerResizable<Pointer>) {
        self.table.remove(&p.raw().0);
    }
    fn free_fixed(&mut self, p: UniquePointerFixedSize<Pointer>) {
        self.table.remove(&p.raw().0);
    }
    fn resize(
        &mut self,
        p: &UniquePointerResizable<Pointer>,
        new_size: u32,
    ) -> Result<Relocation<usize>, AllocError> {
        let new_size = new_size.to_usize();
        let row = *self
            .table
            .get(&p.raw().0)
            .ok_or(AllocError::DanglingPointer)?;
        if new_size <= row.size {
            self.table.get_mut(&p.raw().0).unwrap().size = new_size;
            Ok(None) // shrink in place -- no move
        } else {
            let new_addr = self.bump; // bump allocator can't grow in place
            self.bump += new_size;
            let e = self.table.get_mut(&p.raw().0).unwrap();
            e.address = new_addr;
            e.size = new_size;
            Ok(Some((row.address, new_addr)))
        }
    }
    fn address(&self, p: Pointer) -> Result<usize, AllocError> {
        self.table
            .get(&p.0)
            .map(|r| r.address)
            .ok_or(AllocError::DanglingPointer)
    }
    fn size(&self, p: Pointer) -> Result<u32, AllocError> {
        self.table
            .get(&p.0)
            .map(|r| Word::from_usize(r.size))
            .ok_or(AllocError::DanglingPointer)
    }
}

/// Composition, not extension: holds a `Storage` and an `Allocator`. The
/// `RefCell` is the entire `&self`-write-facade cost, contained here.
#[cfg(test)]
struct UnjournaledBackend<S, A> {
    inner: RefCell<(S, A)>,
}
#[cfg(test)]
impl<S: Storage, A: Allocator<Pointer = Pointer>> UnjournaledBackend<S, A> {
    fn new(storage: S, alloc: A) -> Self {
        Self {
            inner: RefCell::new((storage, alloc)),
        }
    }
    /// The read path is `&mut self`, so it can hand out the real seekable cursor.
    fn read_at(&mut self, anchor: Pointer, offset: A::Size) -> &mut S {
        let (storage, alloc) = self.inner.get_mut();
        let addr = alloc.address(anchor).unwrap().to_usize();
        storage
            .seek(SeekFrom::Start((addr + offset.to_usize()) as u64))
            .unwrap();
        storage
    }
}
#[cfg(test)]
impl<S: Storage, A: Allocator<Pointer = Pointer>> Backend for UnjournaledBackend<S, A> {
    type Pointer = Pointer;
    type Size = A::Size;
}
#[cfg(test)]
impl<S: Storage, A: Allocator<Pointer = Pointer>> WriteBackend for UnjournaledBackend<S, A> {
    fn alloc_resizable(&self, size: A::Size) -> UniquePointerResizable<Pointer> {
        let mut g = self.inner.borrow_mut();
        let (storage, alloc) = &mut *g;
        let p = alloc.alloc_resizable(size);
        let end = (alloc.address(p.raw()).unwrap().to_usize() + size.to_usize()) as u64;
        if storage.len().unwrap() < end {
            storage.resize(end).unwrap();
        }
        p
    }
    fn alloc_fixed(&self, size: A::Size) -> UniquePointerFixedSize<Pointer> {
        let mut g = self.inner.borrow_mut();
        let (storage, alloc) = &mut *g;
        let p = alloc.alloc_fixed(size);
        let end = (alloc.address(p.raw()).unwrap().to_usize() + size.to_usize()) as u64;
        if storage.len().unwrap() < end {
            storage.resize(end).unwrap();
        }
        p
    }
    fn free_resizable(&self, p: UniquePointerResizable<Pointer>) {
        self.inner.borrow_mut().1.free_resizable(p);
    }
    fn free_fixed(&self, p: UniquePointerFixedSize<Pointer>) {
        self.inner.borrow_mut().1.free_fixed(p);
    }
    fn resize(
        &self,
        p: &UniquePointerResizable<Pointer>,
        new_size: A::Size,
    ) -> Result<(), BackendError> {
        let mut g = self.inner.borrow_mut();
        let (storage, alloc) = &mut *g;
        let old_size = alloc.size(p.raw())?.to_usize(); // AllocError -> BackendError
                                                        // Consume the allocator's address-level relocation report and translate
                                                        // it into a `Storage` byte move; users of the backend never see addrs.
                                                        // The byte move is I/O, hence the `?`s (io::Error -> BackendError).
        if let Some((old, new)) = alloc.resize(p, new_size)? {
            let (old, new) = (old.to_usize(), new.to_usize());
            let end = (new + new_size.to_usize()) as u64;
            if storage.len()? < end {
                storage.resize(end)?;
            }
            let mut buf = vec![0u8; old_size.min(new_size.to_usize())];
            storage.seek(SeekFrom::Start(old as u64))?;
            storage.read_exact(&mut buf)?;
            storage.seek(SeekFrom::Start(new as u64))?;
            storage.write_all(&buf)?;
        }
        Ok(())
    }
    fn write(&self, anchor: Pointer, offset: A::Size, bytes: &[u8]) {
        let mut g = self.inner.borrow_mut();
        let (storage, alloc) = &mut *g;
        let addr = alloc.address(anchor).unwrap().to_usize();
        storage
            .seek(SeekFrom::Start((addr + offset.to_usize()) as u64))
            .unwrap();
        storage.write_all(bytes).unwrap();
    }
    fn size(&self, p: Pointer) -> Result<A::Size, BackendError> {
        Ok(self.inner.borrow().1.size(p)?)
    }
}
#[cfg(test)]
impl<S: Storage, A: Allocator<Pointer = Pointer>> ReadBackend for UnjournaledBackend<S, A> {
    fn read(&mut self, anchor: Pointer, offset: A::Size, len: A::Size) -> Vec<u8> {
        let (storage, alloc) = self.inner.get_mut();
        let addr = alloc.address(anchor).unwrap().to_usize();
        storage
            .seek(SeekFrom::Start((addr + offset.to_usize()) as u64))
            .unwrap();
        let mut buf = vec![0u8; len.to_usize()];
        storage.read_exact(&mut buf).unwrap();
        buf
    }
    fn size(&self, p: Pointer) -> Result<A::Size, BackendError> {
        Ok(self.inner.borrow().1.size(p)?)
    }
}

// ================================ tests ================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;

    fn backend() -> UnjournaledBackend<MockStorage, MockAllocator> {
        UnjournaledBackend::new(MockStorage::default(), MockAllocator::default())
    }

    #[test]
    fn default_pointer_common_case_is_parameter_free() {
        let backend = backend();
        let root = backend.alloc_fixed(4);

        // `PersistableVec::<i32>` — `P` defaults to `Pointer`; no width, no size.
        let mut v = PersistableVec::<i32>::new();
        v.push_in_memory(10);
        v.push_in_memory(20);
        v.store(
            &backend,
            Location {
                anchor: root.raw(),
                offset: 0, // inferred as B::Size = u32
            },
        );
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn store_via_shared_ref_then_load_via_mut_ref() {
        let mut backend = backend();
        let root = backend.alloc_fixed(4);

        // write path: `store` takes `&backend` (write = &self)
        let mut x = 7i32;
        x.store(
            &backend,
            Location {
                anchor: root.raw(),
                offset: 0,
            },
        );

        // read path: `load` takes `&mut backend` (read = &mut self)
        let y = i32::load(
            &mut backend,
            Location {
                anchor: root.raw(),
                offset: 0,
            },
        );
        assert_eq!(y, 7);
    }

    #[test]
    fn read_hands_out_a_real_seekable_cursor_via_mut_self() {
        let mut backend = backend();
        let root = backend.alloc_fixed(8);
        backend.write(root.raw(), 0, &[1, 2, 3, 4, 5, 6, 7, 8]);

        // `read_at` returns the real cursor because it borrows `&mut self`.
        let cursor = backend.read_at(root.raw(), 2);
        let mut buf = [0u8; 2];
        cursor.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [3, 4]); // read from offset 2..4
                                 // and it genuinely seeks (the whole point of `read = &mut self`) --
                                 // rewind by 1 and re-read to prove `Seek` works on the handed-out cursor
        cursor.seek(SeekFrom::Current(-1)).unwrap();
        let mut one = [0u8; 1];
        cursor.read_exact(&mut one).unwrap();
        assert_eq!(one, [4]); // we're now at offset 3
    }

    #[test]
    fn resize_relocation_moves_bytes_in_storage() {
        let backend = backend();
        let p = backend.alloc_resizable(4);
        backend.write(p.raw(), 0, &[9, 8, 7, 6]);
        // grow -> the bump allocator relocates -> WriteBackend copies the bytes
        backend.resize(&p, 8).unwrap();
        // read back through a fresh &mut borrow
        let mut backend = backend;
        let got = backend.read(p.raw(), 0, 4);
        assert_eq!(got, vec![9, 8, 7, 6]);
    }

    #[test]
    fn size_of_a_dangling_pointer_is_an_error_not_a_default() {
        let backend = backend();
        let bogus = Pointer(NonZeroU32::new(999).unwrap()); // never allocated
                                                            // `size` (both halves) takes `&self`; disambiguate via ReadBackend.
        assert!(matches!(
            <UnjournaledBackend<_, _> as ReadBackend>::size(&backend, bogus),
            Err(BackendError::Alloc(AllocError::DanglingPointer))
        ));
    }

    #[test]
    fn location_size_flows_from_the_backend() {
        // `Location<P, B::Size>` means the offset type is the backend's `Size`.
        let loc: Location<Pointer, <MockAllocator as Allocator>::Size> = Location {
            anchor: Pointer(NonZeroU32::new(1).unwrap()),
            offset: 5u32, // must be u32 here, not usize
        };
        assert_eq!(loc.offset, 5u32);
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
