mod support;

use kladde::Durable;
use support::Fixture;

// The enum comes before the trait, in another module, and names the trait
// by its path.
#[derive(Durable, Debug, PartialEq, Clone)]
#[kladde(dispatch(shapes::Shape, shapes::Scale))]
#[repr(u32)]
enum AnyShape {
    Circle(Circle) = 1,
    Rect(Rect) = 2,
    Frame(Box<Frame>) = 3,
}

#[derive(Durable, Debug, PartialEq, Clone)]
struct Circle {
    r: f32,
}

#[derive(Durable, Debug, PartialEq, Clone)]
struct Rect {
    w: f32,
    h: f32,
}

#[derive(Durable, Debug, PartialEq, Clone)]
struct Frame {
    outer: Rect,
    hole: Rect,
}

mod shapes {
    use super::{Circle, Frame, Rect};

    #[kladde::dispatch]
    pub trait Shape {
        fn area(&self) -> f32;
        /// Forwarded too, so that a variant's override wins.
        fn name(&self) -> String {
            "shape".into()
        }
        fn into_name(self) -> String
        where
            Self: Sized,
        {
            self.name()
        }
        /// A generic method, an argument bound by a pattern, and an
        /// `impl Trait` argument.
        fn weighted<W: Into<f32>>(&self, (a, b): (f32, f32), w: W) -> f32 {
            (self.area() + a + b) * w.into()
        }
        fn labelled(&self, label: impl AsRef<str>) -> String {
            format!("{} {}", label.as_ref(), self.name())
        }
        /// # Safety
        /// None needed; it checks forwarding an `unsafe fn`.
        unsafe fn raw(&self) -> u32 {
            7
        }
    }

    #[kladde::dispatch]
    pub trait Scale {
        fn scale(&mut self, by: f32);
    }

    impl Shape for Circle {
        fn area(&self) -> f32 {
            3.0 * self.r * self.r
        }
        fn name(&self) -> String {
            "circle".into()
        }
    }
    impl Shape for Rect {
        fn area(&self) -> f32 {
            self.w * self.h
        }
    }
    impl Shape for Frame {
        fn area(&self) -> f32 {
            self.outer.area() - self.hole.area()
        }
        fn name(&self) -> String {
            "frame".into()
        }
    }
    impl Scale for Circle {
        fn scale(&mut self, by: f32) {
            self.r *= by;
        }
    }
    impl Scale for Rect {
        fn scale(&mut self, by: f32) {
            self.w *= by;
            self.h *= by;
        }
    }
    impl Scale for Frame {
        fn scale(&mut self, by: f32) {
            self.outer.scale(by);
            self.hole.scale(by);
        }
    }
}

use shapes::{Scale, Shape};

fn frame() -> Frame {
    Frame {
        outer: Rect { w: 3.0, h: 3.0 },
        hole: Rect { w: 1.0, h: 1.0 },
    }
}

#[test]
fn every_method_forwards_to_the_current_variant() {
    let circle = AnyShape::from(Circle { r: 1.0 });
    let rect = AnyShape::from(Rect { w: 2.0, h: 3.0 });
    let boxed = AnyShape::from(frame());
    assert_eq!([circle.area(), rect.area(), boxed.area()], [3.0, 6.0, 8.0]);
    assert_eq!(circle.name(), "circle");
    assert_eq!(rect.name(), "shape", "the default, which `Rect` keeps");
    assert_eq!(
        boxed.clone().into_name(),
        "frame",
        "by value, out of the box"
    );
    assert_eq!(rect.weighted((1.0, 2.0), 2u8), 18.0);
    assert_eq!(boxed.labelled("a"), "a frame");
    assert_eq!(unsafe { boxed.raw() }, 7);

    let mut scaled = boxed;
    scaled.scale(2.0);
    assert_eq!(scaled.area(), 32.0, "by mutable reference, into the box");
}

#[test]
fn from_takes_a_variants_value_or_what_its_box_holds() {
    assert_eq!(AnyShape::from(frame()), AnyShape::Frame(Box::new(frame())));
    assert_eq!(
        AnyShape::from(Box::new(frame())),
        AnyShape::Frame(Box::new(frame()))
    );
}

/// The same enum without dispatch, without the box, and under another name.
#[derive(Durable)]
#[repr(u32)]
enum Plain {
    Circle(Circle) = 1,
    Rect(Rect) = 2,
    Frame(Frame) = 3,
}

#[test]
fn dispatch_and_a_box_change_nothing_on_file() {
    assert_eq!(AnyShape::fingerprint(), Plain::fingerprint());
    assert_eq!(
        AnyShape::from(frame()).to_bytes(),
        Plain::Frame(frame()).to_bytes()
    );
}

#[test]
fn a_dispatch_enum_is_durable_like_any_other() {
    let mut f = Fixture::new(AnyShape::from(Circle { r: 1.0 }));
    f.kladde.guard().set(frame().into()).unwrap();
    if let AnyShapeParts::Frame(mut fr) = f.kladde.guard().parts() {
        fr.hole_mut().w_mut().set(2.0).unwrap();
    }
    assert_eq!(f.kladde.get().area(), 7.0);
    f.check();
}

/// A generic enum: forwarding to `T` needs `T: Describe`, which the derive
/// adds.
#[kladde::dispatch]
trait Describe {
    fn describe(&self) -> String;
}

impl Describe for u32 {
    fn describe(&self) -> String {
        format!("{self}u32")
    }
}

impl Describe for Circle {
    fn describe(&self) -> String {
        format!("circle of {}", self.r)
    }
}

#[derive(Durable)]
#[kladde(dispatch(Describe))]
enum Either<T> {
    Value(T),
    Circle(Circle),
}

#[test]
fn a_generic_enum_forwards_to_its_parameter() {
    assert_eq!(Either::<u32>::Value(5).describe(), "5u32");
    assert_eq!(
        Either::<u32>::from(Circle { r: 2.0 }).describe(),
        "circle of 2"
    );
}

/// Two variants of one type: both forward, and neither gets `From`, which
/// would conflict.
#[derive(Durable)]
#[kladde(dispatch(Describe))]
enum Twice {
    First(u32),
    Second(u32),
    Circle(Circle),
}

impl From<u32> for Twice {
    fn from(n: u32) -> Self {
        Twice::Second(n)
    }
}

#[test]
fn a_type_two_variants_hold_gets_no_from() {
    assert_eq!(Twice::First(1).describe(), "1u32");
    assert_eq!(Twice::from(2).describe(), "2u32");
    assert!(matches!(Twice::from(Circle { r: 1.0 }), Twice::Circle(_)));
}
