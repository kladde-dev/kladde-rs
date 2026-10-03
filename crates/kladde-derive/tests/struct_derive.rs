mod support;

use kladde_derive::Persistable;
use kladde_persist::{Encoding, Input, Packed, Persistable, Place, Slotted};
use kladde_store::WriteBackend;
use support::{Fixture, Number};

#[derive(Persistable)]
struct Point {
    x: Number,
    y: Number,
}

#[test]
fn generated_guard_exposes_a_mut_accessor_per_field() {
    let mut f = Fixture::for_type::<Point>();
    let mut point = Point {
        x: Number(1),
        y: Number(2),
    };
    let mut guard = point.guard(&f.store, f.place());
    guard.x_mut().set(10).unwrap();
    guard.y_mut().set(20).unwrap();
    // Deref gives read-only access to the plain value.
    assert_eq!(guard.x.0, 10);
    assert_eq!(guard.y.0, 20);
    assert_eq!(point.x.0, 10);

    // `x` and `y` were written at static, non-overlapping offsets.
    let reloaded: Point = f.reload();
    assert_eq!(reloaded.x.0, 10);
    assert_eq!(reloaded.y.0, 20);
}

#[test]
fn set_replaces_the_whole_value() {
    let mut f = Fixture::for_type::<Point>();
    let mut point = Point {
        x: Number(1),
        y: Number(2),
    };
    point
        .guard(&f.store, f.place())
        .set(Point {
            x: Number(3),
            y: Number(4),
        })
        .unwrap();
    assert_eq!((point.x.0, point.y.0), (3, 4));
    let reloaded: Point = f.reload();
    assert_eq!((reloaded.x.0, reloaded.y.0), (3, 4));
}

#[derive(Persistable)]
struct Empty;

#[test]
fn unit_struct_derives_without_error() {
    let f = Fixture::for_type::<Empty>();
    let mut empty = Empty;
    let _guard = empty.guard(&f.store, f.place());
    assert_eq!(<Empty as Persistable>::SLOTTED_SIZE, Some(0));
    assert_eq!(<Empty as Persistable>::PACKED_SIZE, Some(0));
}

#[derive(Persistable)]
struct Nested {
    point: Point,
}

#[test]
fn nested_persistable_fields_reborrow_the_same_backend() {
    let mut f = Fixture::for_type::<Nested>();
    let mut nested = Nested {
        point: Point {
            x: Number(0),
            y: Number(0),
        },
    };
    nested
        .guard(&f.store, f.place())
        .point_mut()
        .x_mut()
        .set(7)
        .unwrap();
    assert_eq!(nested.point.x.0, 7);
    let reloaded: Nested = f.reload();
    assert_eq!(reloaded.point.x.0, 7);
}

/// Integers whose packed encodings are varints.
#[derive(Persistable, Debug, PartialEq)]
struct Counts {
    small: u32,
    signed: i64,
    flag: bool,
}

#[test]
fn a_struct_packs_its_fields() {
    let counts = Counts {
        small: 5,
        signed: -3,
        flag: true,
    };
    assert_eq!(<Counts as Persistable>::SLOTTED_SIZE, Some(4 + 8 + 1));
    assert_eq!(<Counts as Persistable>::PACKED_SIZE, None);
    assert_eq!(counts.encoded_size::<Packed>(), 3);
    assert_eq!(counts.to_bytes::<Packed>(), [5, 5, 1]);
    let bytes = counts.to_bytes::<Slotted>();
    assert_eq!(bytes.len(), 13);
    let mut f = Fixture::new(0);
    let mut input = Input::new(&bytes);
    assert_eq!(
        Counts::decode::<_, Slotted>(&mut f.store, &mut input).unwrap(),
        counts
    );
}

#[test]
fn packed_fields_find_themselves_after_a_sibling_grows() {
    let mut f = Fixture::new(0);
    let mut counts = Counts {
        small: 5,
        signed: -3,
        flag: false,
    };
    counts.store::<_, Packed>(&f.store, f.location).unwrap();
    {
        let mut guard = counts.guard(&f.store, f.packed());
        let mut parts = guard.parts();
        parts.small.set(1 << 30).unwrap(); // five bytes now
        parts.flag.set(true).unwrap();
        parts.signed.set(-1000).unwrap(); // two bytes now
        parts.small.set(1).unwrap();
    }
    {
        let mut guard = counts.guard(&f.store, f.packed());
        guard.signed_mut().set(i64::MIN).unwrap();
    }
    assert_eq!(f.bytes().len(), 1 + 10 + 1);
    let reloaded: Counts = f.reload_as::<_, Packed>();
    assert_eq!(reloaded, counts);
    assert_eq!(
        reloaded,
        Counts {
            small: 1,
            signed: i64::MIN,
            flag: true
        }
    );
}

/// A field that owns an allocation of its own, to see `free` recurse.
#[derive(Persistable)]
struct Owner {
    id: Number,
    data: Boxed,
}

/// Owns one allocation of four bytes.
struct Boxed(Option<kladde_store::UniquePointer>);

/// A guard that offers nothing to mutate.
struct BoxedGuard<'s, B> {
    inner: &'s mut Boxed,
    backend: &'s B,
}

impl<'s, B: WriteBackend> kladde_persist::Guard for BoxedGuard<'s, B> {
    type Persistable = Boxed;
    type Backend = B;
    fn as_persistable(&self) -> &Boxed {
        self.inner
    }
    fn as_persistable_mut(&mut self) -> &mut Boxed {
        self.inner
    }
    fn backend(&self) -> &B {
        self.backend
    }
}

impl kladde_persist::Slottable for Boxed {}

impl Persistable for Boxed {
    const SLOTTED_SIZE: Option<usize> = Some(4);
    const PACKED_SIZE: Option<usize> = Some(4);

    type RootEncoding = Slotted;

    type Guard<'s, B: WriteBackend<Pointer = kladde_store::Pointer>, E: Encoding>
        = BoxedGuard<'s, B>
    where
        B: 's;

    fn guard<'s, B: WriteBackend<Pointer = kladde_store::Pointer>, E: Encoding>(
        &'s mut self,
        backend: &'s B,
        _place: Place<'s, B, E>,
    ) -> Self::Guard<'s, B, E> {
        BoxedGuard {
            inner: self,
            backend,
        }
    }

    fn encoded_size<E: Encoding>(&self) -> usize {
        4
    }

    fn encode<E: Encoding>(&self, out: &mut Vec<u8>) {
        let id = self.0.as_ref().map(|p| p.raw());
        out.extend_from_slice(&kladde_store::encode_option(id));
    }

    fn decode<B: kladde_store::ReadBackend<Pointer = kladde_store::Pointer>, E: Encoding>(
        _backend: &mut B,
        input: &mut Input<'_>,
    ) -> Result<Self, kladde_persist::Error> {
        Ok(Boxed(
            kladde_store::decode_option::<kladde_store::Pointer>(input.array()?)
                .map(kladde_store::UniquePointer::from_pointer),
        ))
    }

    fn prepare<B: WriteBackend<Pointer = kladde_store::Pointer>>(
        &mut self,
        backend: &B,
    ) -> Result<(), kladde_persist::Error> {
        if self.0.is_none() {
            self.0 = Some(backend.alloc(kladde_store::Word::from_usize(4))?);
        }
        Ok(())
    }

    fn free<B: WriteBackend<Pointer = kladde_store::Pointer>>(
        &mut self,
        backend: &B,
    ) -> Result<(), kladde_persist::Error> {
        match self.0.take() {
            Some(p) => backend.free(p),
            None => Ok(()),
        }
    }

    fn describe_local(_: &mut kladde_persist::SchemaBuilder) -> kladde_persist::TypeDescriptor {
        kladde_persist::TypeDescriptor::Primitive(kladde_persist::Primitive::U32)
    }
}

#[test]
fn set_frees_what_the_old_value_owned() {
    let mut f = Fixture::for_type::<Owner>();
    let mut owner = Owner {
        id: Number(1),
        data: Boxed(None),
    };
    owner.store::<_, Slotted>(&f.store, f.location).unwrap();
    f.store.flush().unwrap();
    assert_eq!(f.store.allocations().len(), 2, "the root and one box");
    owner
        .guard(&f.store, f.place())
        .set(Owner {
            id: Number(2),
            data: Boxed(None),
        })
        .unwrap();
    f.store.flush().unwrap();
    // The new value's box replaced the old one, which is gone.
    assert_eq!(f.store.allocations().len(), 2);
    let reloaded: Owner = f.reload();
    assert_eq!(reloaded.id.0, 2);
}
