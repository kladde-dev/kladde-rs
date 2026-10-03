//! The durable SVG document model: one `#[derive(Persistable)]` type per kind
//! of SVG object.
//!
//! An [`Element`] carries its tag's core geometry in [`ElementKind`], its
//! transform list, its other attributes as an ordered list of typed [`Attr`]s,
//! its inline `style` declarations parsed into the same [`Attr`] type, and its
//! children. Anything the model does not type is kept verbatim as
//! [`Attr::Other`] or [`ElementKind::Other`], so every SVG document fits.
//!
//! # Layouts
//!
//! The containers of the model are type aliases, which the crate's features
//! choose among, in three cumulative steps, so that one code base measures
//! each:
//!
//! - by default, every list is a `PersistableVec`, whose elements are
//!   slotted, and every string a `PersistableString`;
//! - with `packed`, the lists of enums whose variants differ widely in size
//!   — children, transforms, attributes, path data — are
//!   `PackedPersistableVec`s, and everything inside their elements is packed
//!   too: element kinds, lengths, pointers;
//! - with `small-ids`, an element's `id` is a `SmallPersistableString`,
//!   inline in its attribute while it is short;
//! - with `small-attrs`, attribute lists are `SmallPersistableVec`s and every
//!   string a `SmallPersistableString`, so that short attribute lists and
//!   strings live inline in their element, and the document is a packed root;
//! - with `small`, every other list is a `SmallPersistableVec` too: transform
//!   lists, path data, points, and child lists.

#[cfg(feature = "packed")]
use kladde::Packed;
use kladde::Persistable;
#[cfg(not(feature = "packed"))]
use kladde::Slotted;
#[cfg(all(feature = "packed", not(feature = "small")))]
use kladde_types::PackedPersistableVec;
#[cfg(not(feature = "small-attrs"))]
use kladde_types::PersistableString;
#[cfg(not(feature = "small"))]
use kladde_types::PersistableVec;
#[cfg(feature = "small-ids")]
use kladde_types::SmallPersistableString;
#[cfg(feature = "small-attrs")]
use kladde_types::SmallPersistableVec;

use crate::Number;

/// A list of child nodes.
#[cfg(not(feature = "packed"))]
pub type Nodes = PersistableVec<Node>;
/// A list of child nodes.
#[cfg(all(feature = "packed", not(feature = "small")))]
pub type Nodes = PackedPersistableVec<Node>;
/// A list of child nodes.
#[cfg(feature = "small")]
pub type Nodes = SmallPersistableVec<Node>;

/// A transform list.
#[cfg(not(feature = "packed"))]
pub type Transforms = PersistableVec<TransformOp>;
/// A transform list.
#[cfg(all(feature = "packed", not(feature = "small")))]
pub type Transforms = PackedPersistableVec<TransformOp>;
/// A transform list.
#[cfg(feature = "small")]
pub type Transforms = SmallPersistableVec<TransformOp>;

/// A list of attributes.
#[cfg(not(feature = "packed"))]
pub type Attrs = PersistableVec<Attr>;
/// A list of attributes.
#[cfg(all(feature = "packed", not(feature = "small-attrs")))]
pub type Attrs = PackedPersistableVec<Attr>;
/// A list of attributes.
#[cfg(feature = "small-attrs")]
pub type Attrs = SmallPersistableVec<Attr>;

/// Path data.
#[cfg(not(feature = "packed"))]
pub type PathData = PersistableVec<PathSegment>;
/// Path data.
#[cfg(all(feature = "packed", not(feature = "small")))]
pub type PathData = PackedPersistableVec<PathSegment>;
/// Path data.
#[cfg(feature = "small")]
pub type PathData = SmallPersistableVec<PathSegment>;

/// A list of points, whose elements all have one size.
#[cfg(not(feature = "small"))]
pub type Points = PersistableVec<Point>;
/// A list of points, whose elements all have one size.
#[cfg(feature = "small")]
pub type Points = SmallPersistableVec<Point>;

/// A list of lengths, whose elements all have one size.
#[cfg(not(feature = "small"))]
pub type Lengths = PersistableVec<Length>;
/// A list of lengths, whose elements all have one size.
#[cfg(feature = "small")]
pub type Lengths = SmallPersistableVec<Length>;

/// An element's `id`.
#[cfg(not(feature = "small-ids"))]
pub type Id = PersistableString;
/// An element's `id`.
#[cfg(feature = "small-ids")]
pub type Id = SmallPersistableString;

/// Every other string: text, names, values, URLs.
#[cfg(not(feature = "small-attrs"))]
pub type Str = PersistableString;
/// Every other string: text, names, values, URLs.
#[cfg(feature = "small-attrs")]
pub type Str = SmallPersistableString;

/// The encoding of the places the lists' elements stand in: what a value
/// stored into a list of children, attributes, transforms or path segments
/// takes.
#[cfg(not(feature = "packed"))]
pub type ListEncoding = Slotted;
/// The encoding of the places the lists' elements stand in: what a value
/// stored into a list of children, attributes, transforms or path segments
/// takes.
#[cfg(feature = "packed")]
pub type ListEncoding = Packed;

/// A whole SVG document: the root element, and the comments and processing
/// instructions around it.
#[derive(Persistable, Debug)]
#[cfg_attr(feature = "small-attrs", kladde(packed_only))]
pub struct Document {
    /// Comments and processing instructions before the root element.
    pub prolog: Nodes,
    /// The root element, normally an `<svg>`.
    pub root: Element,
    /// Comments and processing instructions after the root element.
    pub epilog: Nodes,
}

/// A child of an element.
// An element is far larger in memory than a string, the more so with packed
// vectors, which keep their offsets; but a `Box` is not persistable, and an
// element's children are mostly elements anyway.
#[allow(clippy::large_enum_variant)]
#[derive(Persistable, Debug)]
#[cfg_attr(feature = "small-attrs", kladde(packed_only))]
pub enum Node {
    /// An element.
    Element(Element),
    /// Character data, with entities and character references resolved.
    Text(Str),
    /// A comment, without its `<!--` and `-->`.
    Comment(Str),
    /// A processing instruction, such as `<?xml-stylesheet ...?>`.
    ProcessingInstruction {
        /// The instruction's target, e.g. `xml-stylesheet`.
        target: Str,
        /// Everything after the target.
        value: Str,
    },
}

/// An element: its kind, its attributes, and its children.
#[derive(Persistable, Debug)]
#[cfg_attr(feature = "small-attrs", kladde(packed_only))]
pub struct Element {
    /// The tag, with the geometry specific to it.
    pub kind: ElementKind,
    /// The `transform` attribute; empty if there is none.
    pub transform: Transforms,
    /// Every other attribute, in source order, namespace declarations
    /// included.
    pub attrs: Attrs,
    /// The declarations of the inline `style` attribute, in source order;
    /// empty if there is none. A `style` that does not parse as plain
    /// declarations is kept in `attrs` instead, as [`Attr::Other`].
    pub style: Attrs,
    /// The element's children, in document order.
    pub children: Nodes,
}

/// An element's tag, and the core geometry the tag defines.
///
/// A geometry field holds its attribute's value, or the attribute's initial
/// value (zero) where the attribute is absent; the writer leaves out a field
/// that is zero. Geometry whose absence means something other than zero, such
/// as an ellipse's `rx` or a rect's `rx`, is an [`Attr`] instead.
#[derive(Persistable, Debug)]
#[cfg_attr(feature = "small-attrs", kladde(packed_only))]
pub enum ElementKind {
    Svg,
    G,
    Defs,
    Symbol,
    Use {
        x: Length,
        y: Length,
    },
    Path {
        /// The path data; empty if `d` is absent or did not parse, in which
        /// case its text is kept in [`Element::attrs`].
        d: PathData,
    },
    Rect {
        x: Length,
        y: Length,
        width: Length,
        height: Length,
    },
    Circle {
        cx: Length,
        cy: Length,
        r: Length,
    },
    Ellipse {
        cx: Length,
        cy: Length,
    },
    Line {
        x1: Length,
        y1: Length,
        x2: Length,
        y2: Length,
    },
    Polyline {
        points: Points,
    },
    Polygon {
        points: Points,
    },
    Text,
    Tspan,
    TextPath,
    LinearGradient,
    RadialGradient,
    Stop {
        offset: Length,
    },
    ClipPath,
    Mask,
    Pattern,
    Marker,
    Image {
        x: Length,
        y: Length,
    },
    Style,
    Title,
    Desc,
    Metadata,
    Filter,
    Switch,
    A,
    Script,
    ForeignObject,
    /// Any other element, by its qualified name (`prefix:local` for an
    /// element outside the SVG namespace).
    Other {
        name: Str,
    },
}

/// One segment of path data, as the `d` attribute writes it: absolute or
/// relative, and one of the ten commands.
#[derive(Persistable, Debug, Clone, Copy, PartialEq)]
pub enum PathSegment {
    MoveTo {
        abs: bool,
        x: Number,
        y: Number,
    },
    LineTo {
        abs: bool,
        x: Number,
        y: Number,
    },
    HorizontalLineTo {
        abs: bool,
        x: Number,
    },
    VerticalLineTo {
        abs: bool,
        y: Number,
    },
    CurveTo {
        abs: bool,
        x1: Number,
        y1: Number,
        x2: Number,
        y2: Number,
        x: Number,
        y: Number,
    },
    SmoothCurveTo {
        abs: bool,
        x2: Number,
        y2: Number,
        x: Number,
        y: Number,
    },
    Quadratic {
        abs: bool,
        x1: Number,
        y1: Number,
        x: Number,
        y: Number,
    },
    SmoothQuadratic {
        abs: bool,
        x: Number,
        y: Number,
    },
    EllipticalArc {
        abs: bool,
        rx: Number,
        ry: Number,
        x_axis_rotation: Number,
        large_arc: bool,
        sweep: bool,
        x: Number,
        y: Number,
    },
    ClosePath {
        abs: bool,
    },
}

/// One entry of a transform list. A `rotate` about a center is stored as the
/// translate, rotate, translate it stands for.
#[derive(Persistable, Debug, Clone, Copy, PartialEq)]
pub enum TransformOp {
    Matrix {
        a: Number,
        b: Number,
        c: Number,
        d: Number,
        e: Number,
        f: Number,
    },
    Translate {
        tx: Number,
        ty: Number,
    },
    Scale {
        sx: Number,
        sy: Number,
    },
    Rotate {
        angle: Number,
    },
    SkewX {
        angle: Number,
    },
    SkewY {
        angle: Number,
    },
}

/// A point of a `points` list.
#[derive(Persistable, Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: Number,
    pub y: Number,
}

/// A length or coordinate: a number and its unit.
#[derive(Persistable, Debug, Clone, Copy, PartialEq)]
pub struct Length {
    pub value: Number,
    pub unit: LengthUnit,
}

impl Length {
    /// Zero user units, the initial value of every geometry field.
    pub const ZERO: Length = Length {
        value: 0.0,
        unit: LengthUnit::None,
    };

    /// `value` user units.
    pub fn user(value: Number) -> Length {
        Length {
            value,
            unit: LengthUnit::None,
        }
    }

    /// Whether this is zero in any unit, which the writer leaves out.
    pub fn is_zero(&self) -> bool {
        self.value == 0.0
    }
}

/// The unit of a [`Length`].
#[derive(Persistable, Debug, Clone, Copy, PartialEq)]
pub enum LengthUnit {
    /// User units: a bare number.
    None,
    Em,
    Ex,
    Px,
    In,
    Cm,
    Mm,
    Pt,
    Pc,
    Percent,
}

/// An sRGB color with alpha.
#[derive(Persistable, Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

/// The value of `fill` or `stroke`.
#[derive(Persistable, Debug)]
#[cfg_attr(feature = "small-attrs", kladde(packed_only))]
pub enum Paint {
    None,
    CurrentColor,
    Inherit,
    Color(Color),
    /// A reference to a paint server, by its IRI, e.g. `#gradient`. A
    /// reference with a fallback color is an [`Attr::Other`] instead.
    Url(Str),
}

/// The value of `stroke-linecap`.
#[derive(Persistable, Debug, Clone, Copy, PartialEq)]
pub enum LineCap {
    Butt,
    Round,
    Square,
}

/// The value of `stroke-linejoin`.
#[derive(Persistable, Debug, Clone, Copy, PartialEq)]
pub enum LineJoin {
    Miter,
    MiterClip,
    Round,
    Bevel,
    Arcs,
}

/// The value of `fill-rule` or `clip-rule`.
#[derive(Persistable, Debug, Clone, Copy, PartialEq)]
pub enum FillRule {
    NonZero,
    EvenOdd,
}

/// An attribute, or a declaration of an inline `style`: typed where the model
/// knows the attribute and its value parses, [`Attr::Other`] otherwise.
#[derive(Persistable, Debug)]
#[cfg_attr(feature = "small-ids", kladde(packed_only))]
pub enum Attr {
    Id(Id),
    Class(Str),
    /// `href`.
    Href(Str),
    /// `xlink:href`.
    XlinkHref(Str),
    Fill(Paint),
    Stroke(Paint),
    Opacity(Number),
    FillOpacity(Number),
    StrokeOpacity(Number),
    StopOpacity(Number),
    StrokeWidth(Length),
    StrokeDashoffset(Length),
    StrokeMiterlimit(Number),
    StrokeDasharray(Lengths),
    StrokeLinecap(LineCap),
    StrokeLinejoin(LineJoin),
    FillRule(FillRule),
    ClipRule(FillRule),
    StopColor(Color),
    Color(Color),
    FontSize(Length),
    FontFamily(Str),
    ViewBox {
        min_x: Number,
        min_y: Number,
        width: Number,
        height: Number,
    },
    X(Length),
    Y(Length),
    Width(Length),
    Height(Length),
    Rx(Length),
    Ry(Length),
    GradientTransform(Transforms),
    PatternTransform(Transforms),
    /// Any other attribute, or one whose value did not parse, verbatim, by
    /// its qualified name.
    Other {
        name: Str,
        value: Str,
    },
}
