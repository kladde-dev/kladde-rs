//! Writes, or checks, the file of the cross-implementation test: kladde-rs
//! and kladde-cpp each write one and read the other's
//! (`kladde-cpp/tools/kladde-cross.cpp` holds the same types in C++).
//!
//! ```sh
//! cargo run -p kladde-types --example cross_impl -- write drawing.kladde
//! cargo run -p kladde-types --example cross_impl -- check drawing.kladde
//! ```

use kladde::{Durable, Kladde};
use kladde_types::{DurableMap, DurableVec};

#[derive(Durable, Debug, PartialEq, Clone, Copy)]
struct Point {
    x: f32,
    y: f32,
}

#[derive(Durable, Debug, PartialEq)]
enum Shape {
    Circle { r: f32 },
    Poly(Vec<Point>),
    Label(String),
    Empty,
}

#[derive(Durable)]
struct Drawing {
    name: String,
    shapes: DurableVec<Shape>,
    tags: DurableMap<String, u32>,
    origin: Option<Point>,
    scale: u16,
}

/// The drawing both implementations write: created, then edited, so that
/// the file holds a flushed state and a journal.
fn write(path: &str) -> kladde::Result<()> {
    let mut d = Kladde::create(
        path,
        Drawing {
            name: "cross".into(),
            shapes: DurableVec::new(),
            tags: DurableMap::new(),
            origin: None,
            scale: 1,
        },
    )?;
    let mut m = d.get_mut();
    m.name_mut().push_str(" implementation")?;
    m.shapes_mut().push(Shape::Circle { r: 1.5 })?;
    m.shapes_mut().push(Shape::Poly(vec![
        Point { x: 0.0, y: 0.0 },
        Point { x: 3.0, y: 4.0 },
    ]))?;
    m.shapes_mut().push(Shape::Label("ünïcödé".into()))?;
    m.shapes_mut().push(Shape::Empty)?;
    m.shapes_mut().move_element(3, 0)?;
    m.tags_mut().insert("layer".into(), 7)?;
    m.tags_mut().insert("z".into(), 70_000)?;
    m.origin_mut().set(Some(Point { x: -1.0, y: 2.5 }))?;
    m.scale_mut().set(300)?;
    drop(m);
    d.flush()?;
    // Left in the journal, for the reader to recover.
    d.get_mut().tags_mut().remove(&"layer".into())?;
    Ok(())
}

fn check(path: &str) -> kladde::Result<()> {
    let d = Kladde::<Drawing>::open(path)?;
    let got = d.get();
    assert_eq!(got.name, "cross implementation");
    assert_eq!(
        got.shapes.as_slice(),
        [
            Shape::Empty,
            Shape::Circle { r: 1.5 },
            Shape::Poly(vec![Point { x: 0.0, y: 0.0 }, Point { x: 3.0, y: 4.0 }]),
            Shape::Label("ünïcödé".into()),
        ]
    );
    let tags: Vec<(&String, &u32)> = got.tags.iter().collect();
    assert_eq!(tags, [(&"z".to_string(), &70_000)]);
    assert_eq!(got.origin, Some(Point { x: -1.0, y: 2.5 }));
    assert_eq!(got.scale, 300);
    println!("{path} reads as expected");
    Ok(())
}

fn main() -> kladde::Result<()> {
    println!("fingerprint {}", Drawing::fingerprint().to_hex());
    let args: Vec<String> = std::env::args().collect();
    match (args.get(1).map(String::as_str), args.get(2)) {
        (Some("write"), Some(path)) => write(path),
        (Some("check"), Some(path)) => check(path),
        _ => {
            eprintln!("usage: cross_impl (write | check) <file>");
            std::process::exit(2);
        }
    }
}
