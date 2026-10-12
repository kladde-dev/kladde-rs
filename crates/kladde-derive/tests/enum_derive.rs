mod support;

use kladde::{Fixed, Persistable, TypeDescriptor, TypeRef};
use support::Fixture;

#[derive(Persistable, Debug, PartialEq, Clone)]
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

#[derive(Persistable, Debug, PartialEq, Clone, Copy)]
enum Unit {
    A,
    B,
    C = 127,
}

#[derive(Persistable, Debug, PartialEq, Clone, Copy)]
enum Wide {
    A,
    B = 128,
}

#[derive(Persistable, Debug, PartialEq, Clone, Copy)]
#[repr(u16)]
enum Pinned {
    A = 3,
    B,
}

#[test]
fn discriminants_are_rusts_and_fix_the_size_below_128() {
    assert_eq!(<Unit as Persistable>::FIXED_SIZE, Some(1));
    assert_eq!(Unit::C.to_bytes(), [127]);
    assert_eq!(<Wide as Persistable>::FIXED_SIZE, None);
    assert_eq!(Wide::B.to_bytes(), [0x80, 0x01]);
    assert_eq!(<Wide as Persistable>::FIXED_FORM_SIZE, Some(1));
    assert_eq!(Fixed(Wide::B).to_bytes(), [128]);
    assert_eq!(<Pinned as Persistable>::FIXED_FORM_SIZE, Some(2));
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
    #[derive(Persistable)]
    enum Same {
        A(f32),
        B(u8, u8, u8, u8),
    }
    assert_eq!(<Same as Persistable>::FIXED_SIZE, Some(5));
}

/// A list held inline, through a box.
#[derive(Persistable, Debug, PartialEq)]
enum List {
    Nil,
    Cons(u8, Box<List>),
}

#[test]
fn a_recursive_enum_holds_itself_inline() {
    let list = List::Cons(1, Box::new(List::Cons(2, Box::new(List::Nil))));
    assert_eq!(list.to_bytes(), [1, 1, 1, 2, 0]);
    assert_eq!(<List as Persistable>::FIXED_SIZE, None);
    assert_eq!(<List as Persistable>::FIXED_FORM_SIZE, None);
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
