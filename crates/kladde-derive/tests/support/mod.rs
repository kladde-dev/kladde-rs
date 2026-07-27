//! Shared test fixtures for exercising `#[derive(Persistable)]`-generated
//! code: a minimal hand-written "leaf" `Persistable` type (standing in
//! for what `kladde-traits`' primitive blanket impls provide) and a real,
//! flush-capable mock `Backend`.
//!
//! Not every item here is used by every test binary that includes this
//! module (each `tests/*.rs` file compiles as its own crate) -- allowed
//! rather than split further, since it's `#[cfg(test)]`-only fixture code.
#![allow(dead_code)]

use kladde_traits::{
    Allocator, Guard, Location, Persistable, RawPointer, ResolvedPointer, UniqueArrayPointer,
    UniquePointer,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::num::NonZeroU32;

#[derive(Clone, Debug, PartialEq)]
pub struct Number(pub i32);

impl Persistable for Number {
    const INLINE_SIZE: usize = 4;

    type Guard<'s, B: kladde_traits::Backend>
        = NumberGuard<'s, B>
    where
        Self: 's,
        B: 's;

    fn guard<'s, B: kladde_traits::Backend>(
        &'s mut self,
        backend: &'s B,
        location: Location,
    ) -> Self::Guard<'s, B> {
        NumberGuard {
            inner: self,
            backend,
            location,
        }
    }

    fn store<B: kladde_traits::Backend>(&mut self, backend: &B, location: Location) {
        backend.write(location.anchor, location.offset, &self.0.to_le_bytes());
    }

    fn load<B: kladde_traits::Backend>(backend: &B, location: Location) -> Self {
        let bytes = backend.read(location.anchor, location.offset, 4);
        Number(i32::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn describe_local(
        _builder: &mut kladde_traits::SchemaBuilder,
    ) -> kladde_traits::TypeDescriptor {
        kladde_traits::TypeDescriptor::Primitive(kladde_traits::Primitive::I32)
    }
}

pub struct NumberGuard<'s, B> {
    inner: &'s mut Number,
    backend: &'s B,
    location: Location,
}

impl<'s, B: kladde_traits::Backend> NumberGuard<'s, B> {
    pub fn set(&mut self, value: i32) {
        self.backend.write(
            self.location.anchor,
            self.location.offset,
            &value.to_le_bytes(),
        );
        self.inner.0 = value;
    }
}

impl<'s, B: kladde_traits::Backend> Guard for NumberGuard<'s, B> {
    type Persistable = Number;
    type Backend = B;

    fn as_persistable(&self) -> &Number {
        self.inner
    }
    fn as_persistable_mut(&mut self) -> &mut Number {
        self.inner
    }
    fn backend(&self) -> &B {
        self.backend
    }
}

/// A real, in-memory-materialized (not deferred-to-flush) backend for
/// tests: `alloc` both mints an index *and* immediately materializes it,
/// so tests can `write`/`read` and check results without a separate
/// flush step. `root_location` hands out a location backed by a real
/// allocation, sized for whatever root type a test uses.
#[derive(Default)]
pub struct MockBackend {
    regions: RefCell<HashMap<NonZeroU32, Vec<u8>>>,
    next_index: std::cell::Cell<u32>,
}

impl MockBackend {
    pub fn root_location(&self, size: usize) -> Location {
        let pointer = self.alloc::<()>(size);
        Location {
            anchor: pointer.raw(),
            offset: 0,
        }
    }
}

impl Allocator for MockBackend {
    fn alloc<T>(&self, size: usize) -> UniquePointer<T> {
        let raw = self.next_index.get() + 1;
        self.next_index.set(raw);
        let index = NonZeroU32::new(raw).unwrap();
        self.regions.borrow_mut().insert(index, vec![0u8; size]);
        UniquePointer::from_index(index)
    }
    fn free<T>(&self, pointer: UniquePointer<T>) {
        self.regions.borrow_mut().remove(&pointer.index());
    }
    fn alloc_array<T>(&self, byte_size: usize) -> UniqueArrayPointer<T> {
        let raw = self.next_index.get() + 1;
        self.next_index.set(raw);
        let index = NonZeroU32::new(raw).unwrap();
        self.regions
            .borrow_mut()
            .insert(index, vec![0u8; byte_size]);
        UniqueArrayPointer::from_index(index)
    }
    fn free_array<T>(&self, pointer: UniqueArrayPointer<T>) {
        self.regions.borrow_mut().remove(&pointer.index());
    }
    fn resolve<'a, T>(&'a self, pointer: &UniquePointer<T>) -> Option<ResolvedPointer<'a, T>> {
        self.regions
            .borrow()
            .contains_key(&pointer.index())
            .then(|| ResolvedPointer::from_target(pointer.index()))
    }
    fn read(&self, target: RawPointer, offset: u32, len: u32) -> Vec<u8> {
        let regions = self.regions.borrow();
        let region = &regions[&target.index()];
        region[offset as usize..(offset + len) as usize].to_vec()
    }
    fn write(&self, target: RawPointer, offset: u32, bytes: &[u8]) {
        let mut regions = self.regions.borrow_mut();
        let region = regions.get_mut(&target.index()).unwrap();
        let start = offset as usize;
        if region.len() < start + bytes.len() {
            region.resize(start + bytes.len(), 0);
        }
        region[start..start + bytes.len()].copy_from_slice(bytes);
    }
    fn copy(&self, src: RawPointer, src_offset: u32, len: u32, dst: RawPointer, dst_offset: u32) {
        let bytes = self.read(src, src_offset, len);
        self.write(dst, dst_offset, &bytes);
    }
    fn resize_array<T>(&self, pointer: &UniqueArrayPointer<T>, new_byte_size: usize) {
        let mut regions = self.regions.borrow_mut();
        let region = regions.get_mut(&pointer.index()).unwrap();
        region.resize(new_byte_size, 0);
    }
    fn array_capacity<T>(&self, pointer: &UniqueArrayPointer<T>) -> Option<usize> {
        self.regions.borrow().get(&pointer.index()).map(Vec::len)
    }
    fn splice<T>(&self, pointer: &UniqueArrayPointer<T>, offset: u32, old_len: u32, new: &[u8]) {
        let mut regions = self.regions.borrow_mut();
        let region = regions.get_mut(&pointer.index()).unwrap();
        let start = offset as usize;
        region.splice(start..start + old_len as usize, new.iter().copied());
    }
}
