//! `#[kladde(crate = "...")]` -- the escape hatch for code that builds on
//! `kladde-persist` without the `kladde` facade.
//!
//! Generated code is rooted at `::kladde` by default, because an application
//! should need only that one dependency. A library one layer down has no reason
//! to pull the facade in, so it redirects the macro at `kladde-persist` instead.

mod support;

use kladde_derive::Persistable;
use kladde_persist::Persistable;
use support::{Fixture, Number};

#[derive(Persistable)]
#[kladde(crate = "kladde_persist")]
struct Redirected {
    x: Number,
    y: Number,
}

#[test]
fn a_type_rooted_at_kladde_persist_round_trips() {
    let mut f = Fixture::new(<Redirected as Persistable>::INLINE_SIZE);
    let mut value = Redirected {
        x: Number(7),
        y: Number(9),
    };
    value.store(&f.store, f.location).unwrap();
    let loaded: Redirected = f.reload();
    assert_eq!(loaded.x, Number(7));
    assert_eq!(loaded.y, Number(9));
}

#[test]
fn the_inline_size_is_the_sum_of_the_fields() {
    assert_eq!(
        <Redirected as Persistable>::INLINE_SIZE,
        2 * <Number as Persistable>::INLINE_SIZE
    );
}
