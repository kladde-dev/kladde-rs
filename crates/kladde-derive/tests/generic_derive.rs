mod support;

use kladde::Persistable;
use support::Fixture;

#[derive(Persistable, Debug, PartialEq)]
struct Tagged<T> {
    tag: u8,
    value: T,
}

#[derive(Persistable, Debug, PartialEq)]
enum Either<L, R> {
    Left(L),
    Right(R),
}

#[test]
fn generic_types_take_their_parameters_sizes_and_schemas() {
    assert_eq!(<Tagged<f64> as Persistable>::FIXED_SIZE, Some(9));
    assert_eq!(<Tagged<u64> as Persistable>::FIXED_SIZE, None);
    assert_ne!(Tagged::<u8>::fingerprint(), Tagged::<u16>::fingerprint());
    assert_eq!(
        Either::<u8, f32>::Right(1.0).to_bytes(),
        [1, 0, 0, 0x80, 0x3f]
    );
}

#[test]
fn generic_guards_work_like_any_other() {
    let mut f = Fixture::new(Tagged {
        tag: 1,
        value: Either::<u32, bool>::Left(5),
    });
    if let EitherParts::Left(mut n) = f.kladde.guard().value_mut().parts() {
        n.set(1 << 20).unwrap();
    }
    f.kladde.guard().tag_mut().set(2).unwrap();
    f.kladde
        .guard()
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
#[derive(Persistable)]
struct Clash<B> {
    b: B,
}

#[test]
fn a_parameter_named_b_compiles() {
    let mut f = Fixture::new(Clash { b: 1u8 });
    f.kladde.guard().b_mut().set(2).unwrap();
    f.check();
}
