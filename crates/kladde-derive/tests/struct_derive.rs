mod support;

use kladde_derive::Persistable;
use kladde_persist::Persistable;
use kladde_store::WriteBackend;
use support::{Fixture, Number};

#[derive(Persistable)]
struct Point {
    x: Number,
    y: Number,
}

#[test]
fn generated_guard_exposes_a_mut_accessor_per_field() {
    let mut f = Fixture::new(<Point as Persistable>::INLINE_SIZE);
    let mut point = Point {
        x: Number(1),
        y: Number(2),
    };
    let mut guard = point.guard(&f.store, f.location);
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
    let mut f = Fixture::new(<Point as Persistable>::INLINE_SIZE);
    let mut point = Point {
        x: Number(1),
        y: Number(2),
    };
    point
        .guard(&f.store, f.location)
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
    let f = Fixture::new(<Empty as Persistable>::INLINE_SIZE);
    let mut empty = Empty;
    let _guard = empty.guard(&f.store, f.location);
    assert_eq!(<Empty as Persistable>::INLINE_SIZE, 0);
}

#[derive(Persistable)]
struct Nested {
    point: Point,
}

#[test]
fn nested_persistable_fields_reborrow_the_same_backend() {
    let mut f = Fixture::new(<Nested as Persistable>::INLINE_SIZE);
    let mut nested = Nested {
        point: Point {
            x: Number(0),
            y: Number(0),
        },
    };
    nested
        .guard(&f.store, f.location)
        .point_mut()
        .x_mut()
        .set(7)
        .unwrap();
    assert_eq!(nested.point.x.0, 7);
    let reloaded: Nested = f.reload();
    assert_eq!(reloaded.point.x.0, 7);
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

impl Persistable for Boxed {
    const INLINE_SIZE: usize = 4;
    type Guard<'s, B: WriteBackend<Pointer = kladde_store::Pointer>>
        = BoxedGuard<'s, B>
    where
        B: 's;

    fn guard<'s, B: WriteBackend<Pointer = kladde_store::Pointer>>(
        &'s mut self,
        backend: &'s B,
        _location: kladde_persist::Location<kladde_store::Pointer, B::Size>,
    ) -> Self::Guard<'s, B> {
        BoxedGuard {
            inner: self,
            backend,
        }
    }

    fn store<B: WriteBackend<Pointer = kladde_store::Pointer>>(
        &mut self,
        backend: &B,
        location: kladde_persist::Location<kladde_store::Pointer, B::Size>,
    ) -> Result<(), kladde_persist::Error> {
        if self.0.is_none() {
            self.0 = Some(backend.alloc(kladde_store::Word::from_usize(4))?);
        }
        let id = self.0.as_ref().unwrap().raw();
        backend.write(
            location.anchor,
            location.offset,
            &kladde_store::encode_option(Some(id)),
        )
    }

    fn load<B: kladde_store::ReadBackend<Pointer = kladde_store::Pointer>>(
        backend: &mut B,
        location: kladde_persist::Location<kladde_store::Pointer, B::Size>,
    ) -> Result<Self, kladde_persist::Error> {
        let mut bytes = [0u8; 4];
        std::io::Read::read_exact(
            &mut backend.read_at(location.anchor, location.offset)?,
            &mut bytes,
        )?;
        Ok(Boxed(
            kladde_store::decode_option::<kladde_store::Pointer>(bytes)
                .map(kladde_store::UniquePointer::from_pointer),
        ))
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
    let mut f = Fixture::new(<Owner as Persistable>::INLINE_SIZE);
    let mut owner = Owner {
        id: Number(1),
        data: Boxed(None),
    };
    owner.store(&f.store, f.location).unwrap();
    f.store.flush().unwrap();
    assert_eq!(f.store.allocations().len(), 2, "the root and one box");
    owner
        .guard(&f.store, f.location)
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
