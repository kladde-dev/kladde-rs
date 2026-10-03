//! How many bytes a model value takes in a kladde file: its encoding in the
//! place that holds it, plus the content of every allocation it owns.
//!
//! A benchmark counts what an edit stores with [`Payload::payload_bytes`], the
//! measure kladde-bench calls the bytes the application wrote.

use kladde::{Encoding, Packed, Persistable, Slotted};
use kladde_types::{
    PackedPersistableVec, PersistableString, PersistableVec, SmallPersistableString,
    SmallPersistableVec,
};

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
/// // Two path segments, of 10 bytes each packed and 26 slotted with `f32`,
/// // owned by the path, or inline in its element.
/// let path = &doc.root.children[0];
/// assert!(path.payload_bytes::<kladde_svg::model::ListEncoding>() >= 2 * 10);
/// # Ok::<(), kladde_svg::ParseError>(())
/// ```
pub trait Payload: Persistable {
    /// The content of every allocation this value owns, recursively, not
    /// counting its own encoding.
    fn owned_bytes(&self) -> u64;

    /// The value's encoding in a place of encoding `E`, plus everything it
    /// owns.
    fn payload_bytes<E: Encoding>(&self) -> u64 {
        self.encoded_size::<E>() as u64 + self.owned_bytes()
    }
}

impl Payload for PersistableString {
    fn owned_bytes(&self) -> u64 {
        self.len() as u64
    }
}

/// Inline, the text is part of the string's own encoding.
impl Payload for SmallPersistableString {
    fn owned_bytes(&self) -> u64 {
        if self.is_inline() {
            0
        } else {
            self.len() as u64
        }
    }
}

impl<T: Payload + kladde::Slottable> Payload for PersistableVec<T> {
    fn owned_bytes(&self) -> u64 {
        self.iter().map(Payload::payload_bytes::<Slotted>).sum()
    }
}

impl<T: Payload> Payload for PackedPersistableVec<T> {
    fn owned_bytes(&self) -> u64 {
        self.iter().map(Payload::payload_bytes::<Packed>).sum()
    }
}

/// Inline, the elements' encodings are part of the vector's own; what they
/// own is not.
impl<T: Payload> Payload for SmallPersistableVec<T> {
    fn owned_bytes(&self) -> u64 {
        if self.is_inline() {
            self.iter().map(Payload::owned_bytes).sum()
        } else {
            self.iter().map(Payload::payload_bytes::<Packed>).sum()
        }
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
            Attr::Id(id) => id.owned_bytes(),
            Attr::Class(s) | Attr::Href(s) | Attr::XlinkHref(s) | Attr::FontFamily(s) => {
                s.owned_bytes()
            }
            Attr::Fill(p) | Attr::Stroke(p) => p.owned_bytes(),
            Attr::StrokeDasharray(list) => list.owned_bytes(),
            Attr::GradientTransform(ops) | Attr::PatternTransform(ops) => ops.owned_bytes(),
            Attr::Other { name, value } => name.owned_bytes() + value.owned_bytes(),
            _ => 0,
        }
    }
}
