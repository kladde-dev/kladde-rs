//! Shared test fixtures for exercising `#[derive(Persistable)]`-generated code:
//! a minimal hand-written "leaf" `Persistable` type (standing in for what
//! `kladde-persist`'s primitive blanket impls provide) over the real
//! `kladde_persist::MockBackend`.
//!
//! Not every item here is used by every test binary that includes this module
//! (each `tests/*.rs` file compiles as its own crate) -- allowed rather than
//! split further, since it's `#[cfg(test)]`-only fixture code.
#![allow(dead_code)]

use kladde_persist::{
    Backend, Guard, Location, Persistable, PointerRepr, ReadBackend, Word, WriteBackend,
};
use std::io::Read;

pub use kladde_persist::MockBackend;

/// The pointer/size types the mock backend works in, so tests can name a
/// `Location` without spelling out associated types.
pub type MockLocation = Location<<MockBackend as Backend>::Pointer, <MockBackend as Backend>::Size>;

/// Hands out a location backed by a real allocation, sized for whatever root
/// type a test uses.
pub fn root_location(backend: &MockBackend, size: usize) -> MockLocation {
    let pointer = backend.alloc_fixed_size(Word::from_usize(size));
    Location::new(pointer.raw(), 0)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Number(pub i32);

impl<P: PointerRepr> Persistable<P> for Number {
    const INLINE_SIZE: usize = 4;

    type Guard<'s, B: WriteBackend<Pointer = P>>
        = NumberGuard<'s, B>
    where
        Self: 's,
        B: 's;

    fn guard<'s, B: WriteBackend<Pointer = P>>(
        &'s mut self,
        backend: &'s B,
        location: Location<P, B::Size>,
    ) -> Self::Guard<'s, B> {
        NumberGuard {
            inner: self,
            backend,
            location,
        }
    }

    fn store<B: WriteBackend<Pointer = P>>(&mut self, backend: &B, location: Location<P, B::Size>) {
        backend.write(location.anchor, location.offset, &self.0.to_le_bytes());
    }

    fn load<B: ReadBackend<Pointer = P>>(backend: &mut B, location: Location<P, B::Size>) -> Self {
        let mut bytes = [0u8; 4];
        backend
            .read_at(location.anchor, location.offset)
            .read_exact(&mut bytes)
            .expect("read Number");
        Number(i32::from_le_bytes(bytes))
    }

    fn describe_local(
        _builder: &mut kladde_persist::SchemaBuilder,
    ) -> kladde_persist::TypeDescriptor {
        kladde_persist::TypeDescriptor::Primitive(kladde_persist::Primitive::I32)
    }
}

pub struct NumberGuard<'s, B: WriteBackend> {
    inner: &'s mut Number,
    backend: &'s B,
    location: Location<B::Pointer, B::Size>,
}

impl<'s, B: WriteBackend> NumberGuard<'s, B> {
    pub fn set(&mut self, value: i32) {
        self.backend.write(
            self.location.anchor,
            self.location.offset,
            &value.to_le_bytes(),
        );
        self.inner.0 = value;
    }
}

impl<'s, B: WriteBackend> Guard for NumberGuard<'s, B> {
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
