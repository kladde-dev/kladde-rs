mod support;

use kladde::{Bounded, Durable, Fixed};
use support::Fixture;

#[derive(Durable, Debug, PartialEq, Clone)]
struct Point {
    x: f32,
    y: f32,
}

#[test]
fn a_guard_has_a_mut_accessor_per_field() {
    let mut f = Fixture::new(Point { x: 1.0, y: 2.0 });
    {
        let mut guard = f.kladde.get_mut();
        guard.x_mut().set(10.0).unwrap();
        guard.y_mut().set(20.0).unwrap();
        // Deref gives read-only access to the plain value.
        assert_eq!(guard.x, 10.0);
    }
    assert_eq!(*f.kladde.get(), Point { x: 10.0, y: 20.0 });
    assert_eq!(<Point as Durable>::FIXED_SIZE, Some(8));
    f.check();
}

#[test]
fn set_replaces_the_whole_value() {
    let mut f = Fixture::new(Point { x: 1.0, y: 2.0 });
    f.kladde.get_mut().set(Point { x: 3.0, y: 4.0 }).unwrap();
    assert_eq!(*f.kladde.get(), Point { x: 3.0, y: 4.0 });
    f.check();
}

#[derive(Durable)]
struct Empty;

#[test]
fn a_unit_struct_takes_no_bytes() {
    let mut f = Fixture::new(Empty);
    let _guard = f.kladde.get_mut();
    assert_eq!(<Empty as Durable>::FIXED_SIZE, Some(0));
    assert!(Empty.to_bytes().is_empty());
}

#[derive(Durable)]
struct Nested {
    label: u16,
    point: Point,
}

#[test]
fn nested_fields_find_their_starts() {
    let mut f = Fixture::new(Nested {
        label: 1,
        point: Point { x: 0.0, y: 0.0 },
    });
    f.kladde.get_mut().point_mut().y_mut().set(7.0).unwrap();
    // The label grows to two bytes, which moves the point.
    f.kladde.get_mut().label_mut().set(1000).unwrap();
    f.kladde.get_mut().point_mut().x_mut().set(5.0).unwrap();
    assert_eq!(f.kladde.get().point, Point { x: 5.0, y: 7.0 });
    f.check();
}

/// Integers whose encodings are varints.
#[derive(Durable, Debug, PartialEq, Clone, Copy)]
#[kladde(bounded)]
struct Counts {
    small: u32,
    signed: i64,
    flag: bool,
}

#[test]
fn a_struct_is_its_fields_back_to_back() {
    let counts = Counts {
        small: 5,
        signed: -3,
        flag: true,
    };
    assert_eq!(<Counts as Durable>::FIXED_SIZE, None);
    assert_eq!(<Counts as Bounded>::FIXED_FORM_SIZE, 4 + 8 + 1);
    assert_eq!(counts.encoded_len(), 3);
    assert_eq!(counts.to_bytes(), [5, 5, 1]);
    assert_eq!(Counts::from_bytes(&[5, 5, 1]).unwrap(), counts);
    // `Fixed<Counts>` lays every field out at its full width.
    let fixed = Fixed(counts).to_bytes();
    assert_eq!(fixed.len(), 13);
    assert_eq!(<Fixed<Counts>>::from_bytes(&fixed).unwrap(), Fixed(counts));
}

#[test]
fn fields_made_together_find_themselves_after_a_sibling_grows() {
    let mut f = Fixture::new(Counts {
        small: 5,
        signed: -3,
        flag: false,
    });
    {
        let mut guard = f.kladde.get_mut();
        let mut parts = guard.parts();
        parts.small.set(1 << 30).unwrap(); // five bytes now
        parts.flag.set(true).unwrap();
        parts.signed.set(-1000).unwrap(); // two bytes now
        parts.small.set(1).unwrap();
    }
    f.kladde.get_mut().signed_mut().set(i64::MIN).unwrap();
    f.kladde.get_mut().flag_mut().set(false).unwrap();
    assert_eq!(
        *f.kladde.get(),
        Counts {
            small: 1,
            signed: i64::MIN,
            flag: false
        }
    );
    f.check();
}

#[derive(Durable, Debug, PartialEq)]
struct Pair(u32, Fixed<u32>);

#[test]
fn a_tuple_struct_names_its_accessors_by_position() {
    let mut f = Fixture::new(Pair(1, Fixed(2)));
    f.kladde.get_mut().field_0_mut().set(300).unwrap();
    f.kladde.get_mut().field_1_mut().set(Fixed(400)).unwrap();
    {
        let mut guard = f.kladde.get_mut();
        let PairParts(mut a, mut b) = guard.parts();
        a.set(5).unwrap();
        b.set(Fixed(6)).unwrap();
    }
    assert_eq!(*f.kladde.get(), Pair(5, Fixed(6)));
    assert_eq!(f.kladde.get().to_bytes(), [5, 6, 0, 0, 0]);
    f.check();
}

// The nested setup of guards made together and on their own: two siblings of
// the root made by `parts`, two levels down in each on their own, two
// siblings of each made by `parts` again, and two more levels down in each of
// the four remote cousins. Every level holds a varint before the next, so
// that a write that changes size anywhere moves what follows it.

#[derive(Durable)]
struct Root {
    a: Level1,
    b: Level1,
}

#[derive(Durable)]
struct Level1 {
    pad: u32,
    down: Level2,
}

#[derive(Durable)]
struct Level2 {
    pad: u32,
    down: Level3,
}

#[derive(Durable)]
struct Level3 {
    x: Level4,
    y: Level4,
}

#[derive(Durable)]
struct Level4 {
    pad: u32,
    down: Level5,
}

#[derive(Durable)]
struct Level5 {
    pad: u32,
    value: u32,
}

fn level1(seed: u32) -> Level1 {
    let level4 = |s: u32| Level4 {
        pad: s,
        down: Level5 {
            pad: s + 1,
            value: s + 2,
        },
    };
    Level1 {
        pad: seed,
        down: Level2 {
            pad: seed + 1,
            down: Level3 {
                x: level4(seed + 2),
                y: level4(seed + 5),
            },
        },
    }
}

#[test]
fn remote_cousins_write_where_they_start_after_each_other_grows() {
    let mut f = Fixture::new(Root {
        a: level1(1),
        b: level1(100),
    });
    {
        let mut root = f.kladde.get_mut();
        let RootParts { mut a, mut b } = root.parts();
        let mut a1 = a.down_mut();
        let mut a2 = a1.down_mut();
        let mut a3 = a2.parts();
        let mut b1 = b.down_mut();
        let mut b2 = b1.down_mut();
        let mut b3 = b2.parts();
        let mut ax = a3.x.down_mut();
        let mut ay = a3.y.down_mut();
        let mut bx = b3.x.down_mut();
        let mut by = b3.y.down_mut();
        let mut axv = ax.value_mut();
        let mut ayv = ay.value_mut();
        let mut bxv = bx.value_mut();
        let mut byv = by.value_mut();
        // Each write changes the size of a value before the others, then
        // those write where they start now.
        axv.set(1 << 28).unwrap();
        byv.set(7).unwrap();
        ayv.set(1 << 20).unwrap();
        bxv.set(1 << 14).unwrap();
        axv.set(3).unwrap();
        byv.set(u32::MAX).unwrap();
        bxv.set(0).unwrap();
        ayv.set(200).unwrap();
    }
    let root = f.kladde.get();
    assert_eq!(root.a.down.down.x.down.value, 3);
    assert_eq!(root.a.down.down.y.down.value, 200);
    assert_eq!(root.b.down.down.x.down.value, 0);
    assert_eq!(root.b.down.down.y.down.value, u32::MAX);
    f.check();
    // The guards made afterwards find the same starts as a fresh walk.
    f.kladde.get_mut().b_mut().pad_mut().set(1 << 30).unwrap();
    f.kladde
        .get_mut()
        .b_mut()
        .down_mut()
        .down_mut()
        .y_mut()
        .down_mut()
        .value_mut()
        .set(9)
        .unwrap();
    f.check();
}
