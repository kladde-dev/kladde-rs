//! Shared fixtures for exercising `#[derive(Persistable)]`-generated code: a
//! store in memory with a root allocation, and a minimal hand-written leaf
//! type standing in for what the scalar impls provide.
//!
//! Not every item is used by every test binary that includes this module
//! (each `tests/*.rs` file compiles as its own crate).
#![allow(dead_code)]

use kladde_persist::{
    slot_size, write_encoded, Encoding, Error, Guard, Input, Location, Packed, Persistable, Place,
    Pointer, PointerRepr, ReadBackend, Slotted, WriteBackend,
};
use kladde_store::{MemoryStorage, Store};

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

    /// A store whose root allocation is a slot for a `T`.
    pub fn for_type<T: Persistable>() -> Fixture {
        Fixture::new(slot_size::<T, Pointer>())
    }

    /// The root allocation, as a slotted place.
    pub fn place(&self) -> Place<'static, Store, Slotted> {
        Slotted::at(self.location)
    }

    /// The root allocation, as a packed place.
    pub fn packed(&self) -> Place<'static, Store, Packed> {
        Packed::at(self.location)
    }

    /// Flushes, then loads a `T` from the root allocation.
    pub fn reload<T: Persistable>(&mut self) -> T {
        self.reload_as::<T, Slotted>()
    }

    /// Flushes, then loads a `T` in encoding `E` from the root allocation.
    pub fn reload_as<T: Persistable, E: Encoding>(&mut self) -> T {
        self.store.flush().unwrap();
        self.store.check();
        T::load::<_, E>(&mut self.store, self.location).unwrap()
    }

    /// Flushes, then returns the root allocation's bytes.
    pub fn bytes(&mut self) -> Vec<u8> {
        self.store.flush().unwrap();
        self.store.read_all(self.location.anchor).unwrap()
    }
}

/// A hand-written `i32` stand-in: four bytes in either encoding.
#[derive(Clone, Debug, PartialEq)]
pub struct Number(pub i32);

impl<P: PointerRepr> Persistable<P> for Number {
    const SLOTTED_SIZE: Option<usize> = Some(4);
    const PACKED_SIZE: Option<usize> = Some(4);

    type Guard<'s, B: WriteBackend<Pointer = P>, E: Encoding>
        = NumberGuard<'s, B, E>
    where
        Self: 's,
        B: 's;

    fn guard<'s, B: WriteBackend<Pointer = P>, E: Encoding>(
        &'s mut self,
        backend: &'s B,
        place: Place<'s, B, E>,
    ) -> Self::Guard<'s, B, E> {
        NumberGuard {
            inner: self,
            backend,
            place,
        }
    }

    fn encoded_size<E: Encoding>(&self) -> usize {
        4
    }

    fn encode<E: Encoding>(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0.to_le_bytes());
    }

    fn decode<B: ReadBackend<Pointer = P>, E: Encoding>(
        _backend: &mut B,
        input: &mut Input<'_>,
    ) -> Result<Self, Error> {
        Ok(Number(i32::from_le_bytes(input.array()?)))
    }

    fn describe_local(
        _builder: &mut kladde_persist::SchemaBuilder,
    ) -> kladde_persist::TypeDescriptor {
        kladde_persist::TypeDescriptor::Primitive(kladde_persist::Primitive::I32)
    }
}

pub struct NumberGuard<'s, B: WriteBackend, E: Encoding> {
    inner: &'s mut Number,
    backend: &'s B,
    place: Place<'s, B, E>,
}

impl<'s, B: WriteBackend, E: Encoding> NumberGuard<'s, B, E> {
    pub fn set(&mut self, value: i32) -> Result<(), Error> {
        write_encoded(self.backend, &self.place, 4, &value.to_le_bytes())?;
        self.inner.0 = value;
        Ok(())
    }
}

impl<'s, B: WriteBackend, E: Encoding> Guard for NumberGuard<'s, B, E> {
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
