//! How many bytes a model value takes in a kladde file: its inline bytes plus
//! the content of every allocation it owns.
//!
//! A benchmark counts what an edit stores with [`Payload::payload_bytes`], the
//! measure kladde-bench calls the bytes the application wrote.

use kladde::{Persistable, Slotted};
use kladde_types::{PersistableString, PersistableVec};

use crate::model::{
    Attr, Color, Document, Element, ElementKind, FillRule, Length, LengthUnit, LineCap, LineJoin,
    Node, Paint, PathSegment, Point, TransformOp,
};

/// The bytes a value occupies in a kladde file.
///
/// ```
/// use kladde_svg::size::Payload;
///
/// let doc = kladde_svg::parse(r#"<svg xmlns="http://www.w3.org/2000/svg"><path d="M0 0L1 1"/></svg>"#)?;
/// // Two path segments, of 26 bytes each with `f32`, owned by the path.
/// let path = &doc.root.children[0];
/// assert!(path.owned_bytes() >= 2 * 26);
/// # Ok::<(), kladde_svg::ParseError>(())
/// ```
pub trait Payload: Persistable {
    /// The content of every allocation this value owns, recursively, not
    /// counting its own inline bytes.
    fn owned_bytes(&self) -> u64;

    /// The value's inline bytes in a slotted place, plus everything it owns.
    fn payload_bytes(&self) -> u64 {
        self.encoded_size::<Slotted>() as u64 + self.owned_bytes()
    }
}

impl Payload for PersistableString {
    fn owned_bytes(&self) -> u64 {
        self.len() as u64
    }
}

impl<T: Payload> Payload for PersistableVec<T> {
    fn owned_bytes(&self) -> u64 {
        self.iter().map(Payload::payload_bytes).sum()
    }
}

macro_rules! owns_nothing {
    ($($ty:ty),*) => {
        $(impl Payload for $ty {
            fn owned_bytes(&self) -> u64 {
                0
            }
        })*
    };
}

owns_nothing!(
    PathSegment,
    TransformOp,
    Point,
    Length,
    LengthUnit,
    Color,
    LineCap,
    LineJoin,
    FillRule
);

impl Payload for Document {
    fn owned_bytes(&self) -> u64 {
        self.prolog.owned_bytes() + self.root.owned_bytes() + self.epilog.owned_bytes()
    }
}

impl Payload for Node {
    fn owned_bytes(&self) -> u64 {
        match self {
            Node::Element(e) => e.owned_bytes(),
            Node::Text(s) | Node::Comment(s) => s.owned_bytes(),
            Node::ProcessingInstruction { target, value } => {
                target.owned_bytes() + value.owned_bytes()
            }
        }
    }
}

impl Payload for Element {
    fn owned_bytes(&self) -> u64 {
        self.kind.owned_bytes()
            + self.transform.owned_bytes()
            + self.attrs.owned_bytes()
            + self.style.owned_bytes()
            + self.children.owned_bytes()
    }
}

impl Payload for ElementKind {
    fn owned_bytes(&self) -> u64 {
        match self {
            ElementKind::Path { d } => d.owned_bytes(),
            ElementKind::Polyline { points } | ElementKind::Polygon { points } => {
                points.owned_bytes()
            }
            ElementKind::Other { name } => name.owned_bytes(),
            _ => 0,
        }
    }
}

impl Payload for Paint {
    fn owned_bytes(&self) -> u64 {
        match self {
            Paint::Url(id) => id.owned_bytes(),
            _ => 0,
        }
    }
}

impl Payload for Attr {
    fn owned_bytes(&self) -> u64 {
        match self {
            Attr::Id(s)
            | Attr::Class(s)
            | Attr::Href(s)
            | Attr::XlinkHref(s)
            | Attr::FontFamily(s) => s.owned_bytes(),
            Attr::Fill(p) | Attr::Stroke(p) => p.owned_bytes(),
            Attr::StrokeDasharray(list) => list.owned_bytes(),
            Attr::GradientTransform(ops) | Attr::PatternTransform(ops) => ops.owned_bytes(),
            Attr::Other { name, value } => name.owned_bytes() + value.owned_bytes(),
            _ => 0,
        }
    }
}
