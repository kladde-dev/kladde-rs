//! Shared test fixtures for exercising `#[derive(Persistable)]`-generated
//! code: a minimal hand-written "leaf" `Persistable` type (standing in
//! for what `kladde-types`' primitive blanket impls will provide) and a
//! mock `Backend` that records how many ops were recorded.
//!
//! Not every item here is used by every test binary that includes this
//! module (each `tests/*.rs` file compiles as its own crate) -- allowed
//! rather than split further, since it's `#[cfg(test)]`-only fixture code.
#![allow(dead_code)]

use kladde_traits::{
    Allocator, Backend, Guard, Journal, Persistable, ResolvedPointer, UniquePointer,
};
use std::cell::RefCell;

#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
pub enum NumberOp {
    Set(i32),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Number(pub i32);

impl Persistable for Number {
    type Op = NumberOp;
    type Guard<'s, B: Backend>
        = NumberGuard<'s, B>
    where
        Self: 's,
        B: 's;

    fn guard<'s, B: Backend>(&'s mut self, backend: &'s B) -> Self::Guard<'s, B> {
        NumberGuard {
            inner: self,
            backend,
        }
    }
}

pub struct NumberGuard<'s, B> {
    inner: &'s mut Number,
    backend: &'s B,
}

impl<'s, B: Backend> NumberGuard<'s, B> {
    pub fn set(&mut self, value: i32) {
        self.backend.record::<Number>(&NumberOp::Set(value));
        self.inner.0 = value;
    }
}

impl<'s, B: Backend> Guard for NumberGuard<'s, B> {
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

#[derive(Default)]
pub struct MockBackend {
    pub record_count: RefCell<usize>,
}

impl Journal for MockBackend {
    fn record<T: Persistable>(&self, _op: &T::Op) {
        *self.record_count.borrow_mut() += 1;
    }
}

impl Allocator for MockBackend {
    fn alloc<T>(&self, _size: usize) -> UniquePointer<T> {
        unimplemented!("not exercised by these tests")
    }
    fn free<T: Persistable>(&self, _pointer: UniquePointer<T>) {
        unimplemented!("not exercised by these tests")
    }
    fn resolve<'a, T>(&'a self, _pointer: &UniquePointer<T>) -> Option<ResolvedPointer<'a, T>> {
        None
    }
}
