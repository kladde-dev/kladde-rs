//! Shared `#[cfg(test)]`-only fixture used across this crate's unit
//! tests: a real, flush-capable backend rather than a hand-rolled mock,
//! since these tests need actual `alloc`/`resize`/`write`/`copy`/`flush`
//! behavior (not just "did something get recorded"), and `kladde` is
//! already a dependency (for the `DefaultBackend` default type
//! parameter).

pub use kladde::DefaultBackend as MockBackend;
