mod support;

use kladde::{Bounded, Durable, Fixed};
use support::Fixture;

#[derive(Durable, Debug, PartialEq)]
#[kladde(transparent, bounded)]
struct Meters(i32);

#[derive(Durable, Debug, PartialEq)]
#[kladde(transparent)]
struct Named {
    inner: (u8, u16),
}

#[test]
fn a_transparent_type_is_its_field_on_file() {
    assert_eq!(Meters(-3).to_bytes(), (-3i32).to_bytes());
    assert_eq!(Meters::fingerprint(), i32::fingerprint());
    assert_eq!(Meters::schema(), i32::schema());
    assert_eq!(Named::fingerprint(), <(u8, u16)>::fingerprint());
    assert_eq!(<Meters as Bounded>::FIXED_FORM_SIZE, 4);
    assert_eq!(Fixed(Meters(-3)).to_bytes(), Fixed(-3i32).to_bytes());
}

#[test]
fn its_guard_passes_the_fields_through() {
    let mut f = Fixture::new(Meters(1));
    f.kladde.guard().get_mut().set(-70_000).unwrap();
    assert_eq!(*f.kladde.get(), Meters(-70_000));
    f.check();
    let mut f = Fixture::new(Named { inner: (1, 2) });
    {
        let mut guard = f.kladde.guard();
        let mut inner = guard.get_mut();
        let (mut a, mut b) = inner.parts();
        b.set(1000).unwrap();
        a.set(3).unwrap();
    }
    f.kladde.guard().set(Named { inner: (4, 5) }).unwrap();
    assert_eq!(f.kladde.get().inner, (4, 5));
    f.check();
}
