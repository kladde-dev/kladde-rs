//! Shared fixtures for exercising `#[derive(Persistable)]`-generated code: a
//! store in memory with a root allocation, and a minimal hand-written leaf
//! type standing in for what the scalar impls provide.
//!
//! Not every item is used by every test binary that includes this module
//! (each `tests/*.rs` file compiles as its own crate).
#![allow(dead_code)]

use kladde_persist::{
    Error, Guard, Location, Persistable, Pointer, PointerRepr, ReadBackend, WriteBackend,
};
use kladde_store::{MemoryStorage, Store};
use std::io::Read;

/// A store whose root allocation holds the value under test.
pub struct Fixture {
    pub store: Store,
    pub location: Location<Pointer, u32>,
}

impl Fixture {
    pub fn new(size: usize) -> Fixture {
        let store = Store::create(Box::new(MemoryStorage::new()), Default::default()).unwrap();
        let root = store.alloc(size as u32).unwrap();
        Fixture {
            store,
            location: Location::new(root.raw(), 0),
        }
    }

    /// Flushes, then loads a `T` from the root allocation.
    pub fn reload<T: Persistable>(&mut self) -> T {
        self.store.flush().unwrap();
        self.store.check();
        T::load(&mut self.store, self.location).unwrap()
    }

    /// Flushes, then returns the root allocation's bytes.
    pub fn bytes(&mut self) -> Vec<u8> {
        self.store.flush().unwrap();
        self.store.read_all(self.location.anchor).unwrap()
    }
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

    fn store<B: WriteBackend<Pointer = P>>(
        &mut self,
        backend: &B,
        location: Location<P, B::Size>,
    ) -> Result<(), Error> {
        backend.write(location.anchor, location.offset, &self.0.to_le_bytes())
    }

    fn load<B: ReadBackend<Pointer = P>>(
        backend: &mut B,
        location: Location<P, B::Size>,
    ) -> Result<Self, Error> {
        let mut bytes = [0u8; 4];
        backend
            .read_at(location.anchor, location.offset)?
            .read_exact(&mut bytes)?;
        Ok(Number(i32::from_le_bytes(bytes)))
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
    pub fn set(&mut self, value: i32) -> Result<(), Error> {
        self.backend.write(
            self.location.anchor,
            self.location.offset,
            &value.to_le_bytes(),
        )?;
        self.inner.0 = value;
        Ok(())
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
