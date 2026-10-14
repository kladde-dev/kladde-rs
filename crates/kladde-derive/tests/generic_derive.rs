mod support;

use kladde::{Bounded, Durable, Fixed};
use support::Fixture;

#[derive(Durable, Debug, PartialEq)]
#[kladde(bounded)]
struct Tagged<T> {
    tag: u8,
    value: T,
}

#[derive(Durable, Debug, PartialEq)]
#[kladde(bounded)]
enum Either<L, R> {
    Left(L),
    Right(R),
}

#[test]
fn generic_types_take_their_parameters_sizes_and_schemas() {
    assert_eq!(<Tagged<f64> as Durable>::FIXED_SIZE, Some(9));
    assert_eq!(<Tagged<u64> as Durable>::FIXED_SIZE, None);
    assert_ne!(Tagged::<u8>::fingerprint(), Tagged::<u16>::fingerprint());
    assert_eq!(
        Either::<u8, f32>::Right(1.0).to_bytes(),
        [1, 0, 0, 0x80, 0x3f]
    );
}

#[test]
fn a_bounded_generic_type_has_a_fixed_form_where_its_parameters_have() {
    assert_eq!(<Tagged<u64> as Bounded>::FIXED_FORM_SIZE, 9);
    assert_eq!(<Either<u8, u32> as Bounded>::FIXED_FORM_SIZE, 1 + 4);
    let fixed = Fixed(Tagged {
        tag: 1,
        value: 7u32,
    });
    assert_eq!(fixed.to_bytes(), [1, 7, 0, 0, 0]);
    assert_eq!(
        <Fixed<Tagged<u32>>>::from_bytes(&fixed.to_bytes()).unwrap(),
        fixed
    );
    // A parameter without a fixed form leaves the type durable.
    let boxed = Tagged {
        tag: 1,
        value: Box::new(2u8),
    };
    assert_eq!(boxed.to_bytes(), [1, 2]);
}

#[test]
fn generic_guards_work_like_any_other() {
    let mut f = Fixture::new(Tagged {
        tag: 1,
        value: Either::<u32, bool>::Left(5),
    });
    if let EitherParts::Left(mut n) = f.kladde.get_mut().value_mut().parts() {
        n.set(1 << 20).unwrap();
    }
    f.kladde.get_mut().tag_mut().set(2).unwrap();
    f.kladde
        .get_mut()
        .value_mut()
        .set(Either::Right(true))
        .unwrap();
    assert_eq!(
        *f.kladde.get(),
        Tagged {
            tag: 2,
            value: Either::Right(true)
        }
    );
    f.check();
}

/// A parameter named like the generated code's own does not clash.
#[derive(Durable)]
struct Clash<B> {
    b: B,
}

#[test]
fn a_parameter_named_b_compiles() {
    let mut f = Fixture::new(Clash { b: 1u8 });
    f.kladde.get_mut().b_mut().set(2).unwrap();
    f.check();
}
