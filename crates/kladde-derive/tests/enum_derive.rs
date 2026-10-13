mod support;

use kladde::{Bounded, Durable, Fixed, TypeDescriptor, TypeRef};
use support::Fixture;

#[derive(Durable, Debug, PartialEq, Clone)]
#[kladde(bounded)]
enum Shape {
    Origin,
    Circle(f32),
    Rectangle { width: u32, height: u32 },
}

#[test]
fn an_enum_is_its_discriminant_then_its_fields() {
    assert_eq!(Shape::Origin.to_bytes(), [0]);
    assert_eq!(Shape::Circle(1.0).to_bytes(), [1, 0, 0, 0x80, 0x3f]);
    assert_eq!(
        Shape::Rectangle {
            width: 300,
            height: 2
        }
        .to_bytes(),
        [2, 0xac, 0x02, 2]
    );
    for shape in [
        Shape::Origin,
        Shape::Circle(2.5),
        Shape::Rectangle {
            width: 1,
            height: 1 << 20,
        },
    ] {
        assert_eq!(Shape::from_bytes(&shape.to_bytes()).unwrap(), shape);
        assert_eq!(shape.encoded_len(), shape.to_bytes().len());
        let fixed = Fixed(shape.clone()).to_bytes();
        assert_eq!(fixed.len(), 1 + 8);
        assert_eq!(<Fixed<Shape>>::from_bytes(&fixed).unwrap(), Fixed(shape));
    }
    assert!(Shape::from_bytes(&[3]).is_err());
}

#[test]
fn parts_reach_into_the_current_variant() {
    let mut f = Fixture::new(Shape::Rectangle {
        width: 1,
        height: 2,
    });
    match f.kladde.guard().parts() {
        ShapeParts::Rectangle {
            mut width,
            mut height,
        } => {
            width.set(1 << 21).unwrap(); // grows, and moves `height`
            height.set(5).unwrap();
        }
        _ => unreachable!(),
    }
    assert_eq!(
        *f.kladde.get(),
        Shape::Rectangle {
            width: 1 << 21,
            height: 5
        }
    );
    f.check();
    f.kladde.guard().set(Shape::Circle(3.0)).unwrap();
    if let ShapeParts::Circle(mut r) = f.kladde.guard().parts() {
        r.set(4.0).unwrap();
    }
    assert_eq!(*f.kladde.get(), Shape::Circle(4.0));
    f.check();
}

#[derive(Durable, Debug, PartialEq, Clone, Copy)]
enum Unit {
    A,
    B,
    C = 127,
}

#[derive(Durable, Debug, PartialEq, Clone, Copy)]
#[kladde(bounded)]
enum Wide {
    A,
    B = 128,
}

#[derive(Durable, Debug, PartialEq, Clone, Copy)]
#[kladde(bounded)]
#[repr(u16)]
enum Pinned {
    A = 3,
    B,
}

#[test]
fn discriminants_are_rusts_and_fix_the_size_below_128() {
    assert_eq!(<Unit as Durable>::FIXED_SIZE, Some(1));
    assert_eq!(Unit::C.to_bytes(), [127]);
    assert_eq!(<Wide as Durable>::FIXED_SIZE, None);
    assert_eq!(Wide::B.to_bytes(), [0x80, 0x01]);
    assert_eq!(<Wide as Bounded>::FIXED_FORM_SIZE, 1);
    assert_eq!(Fixed(Wide::B).to_bytes(), [128]);
    assert_eq!(<Pinned as Bounded>::FIXED_FORM_SIZE, 2);
    assert_eq!(Fixed(Pinned::B).to_bytes(), [4, 0]);
    let TypeDescriptor::Enum {
        discriminant_width,
        variants,
        ..
    } = Pinned::schema().get(TypeRef(0)).clone()
    else {
        panic!("an enum descriptor")
    };
    assert_eq!(discriminant_width, 2);
    assert_eq!(variants[1].discriminant, 4);
    // Every variant of one fixed size makes a fixed-size enum.
    #[derive(Durable)]
    enum Same {
        A(f32),
        B(u8, u8, u8, u8),
    }
    assert_eq!(<Same as Durable>::FIXED_SIZE, Some(5));
}

/// A list held inline, through a box.
#[derive(Durable, Debug, PartialEq)]
enum List {
    Nil,
    Cons(u8, Box<List>),
}

#[test]
fn a_recursive_enum_holds_itself_inline() {
    let list = List::Cons(1, Box::new(List::Cons(2, Box::new(List::Nil))));
    assert_eq!(list.to_bytes(), [1, 1, 1, 2, 0]);
    assert_eq!(<List as Durable>::FIXED_SIZE, None);
    let table = List::schema();
    assert_eq!(table.validate(), Ok(()));
    let mut f = Fixture::new(list);
    if let ListParts::Cons(mut head, mut tail) = f.kladde.guard().parts() {
        tail.set(List::Nil).unwrap();
        head.set(9).unwrap();
    }
    assert_eq!(*f.kladde.get(), List::Cons(9, Box::new(List::Nil)));
    f.check();
}

/// `Shape` again, with its rectangle's fields in a struct of their own.
#[derive(Durable, Debug, PartialEq, Clone)]
#[kladde(bounded)]
struct Rect {
    width: u32,
    height: u32,
}

#[derive(Durable, Debug, PartialEq, Clone)]
#[kladde(bounded)]
enum Unboxed {
    Origin,
    Circle(f32),
    Rectangle(Rect),
}

#[derive(Durable, Debug, PartialEq, Clone)]
#[kladde(bounded)]
enum Boxed {
    Origin,
    Circle(f32),
    Rectangle(Box<Rect>),
}

#[test]
fn boxing_a_variant_changes_nothing_on_file() {
    // Equal but for the enum's own name, which is not fingerprinted.
    assert_eq!(Boxed::fingerprint(), Unboxed::fingerprint());
    let mut schema = Boxed::schema().descriptors().to_vec();
    if let TypeDescriptor::Enum { name, .. } = &mut schema[0] {
        *name = "Unboxed".into();
    }
    assert_eq!(schema, Unboxed::schema().descriptors());
    let rect = Rect {
        width: 300,
        height: 2,
    };
    let boxed = Boxed::Rectangle(Box::new(rect.clone()));
    let unboxed = Unboxed::Rectangle(rect);
    assert_eq!(boxed.to_bytes(), unboxed.to_bytes());
    // The fixed form survives the box too.
    assert_eq!(
        <Boxed as Bounded>::FIXED_FORM_SIZE,
        <Unboxed as Bounded>::FIXED_FORM_SIZE
    );
    assert_eq!(
        Fixed(boxed.clone()).to_bytes(),
        Fixed(unboxed.clone()).to_bytes()
    );
    assert_eq!(
        <Fixed<Boxed>>::from_bytes(&Fixed(unboxed).to_bytes()).unwrap(),
        Fixed(boxed.clone())
    );
    // The boxed variant's guard is the struct's own.
    let mut f = Fixture::new(boxed);
    if let BoxedParts::Rectangle(mut r) = f.kladde.guard().parts() {
        r.width_mut().set(70_000).unwrap();
    }
    assert!(matches!(f.kladde.get(), Boxed::Rectangle(r) if r.width == 70_000));
    f.check();
}
