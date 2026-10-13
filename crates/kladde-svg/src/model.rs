//! The durable SVG document model: one `#[derive(Durable)]` type per kind
//! of SVG object.
//!
//! An [`Element`] carries its tag's core geometry in [`ElementKind`], its
//! transform list, its other attributes as an ordered list of typed [`Attr`]s,
//! its inline `style` declarations parsed into the same [`Attr`] type, and its
//! children. Anything the model does not type is kept verbatim as
//! [`Attr::Other`] or [`ElementKind::Other`], so every SVG document fits.
//!
//! Every list is a `DurableVec` and every string a `DurableString`,
//! inline in its parent, as everything is in the file's one sequence. Lists
//! of points and lengths have fixed-size elements and keep no index; lists of
//! children, attributes, transforms and path data keep a Fenwick tree of
//! their elements' sizes.

use kladde::Durable;
use kladde_types::{DurableString, DurableVec};

use crate::Number;

/// A list of child nodes.
pub type Nodes = DurableVec<Node>;

/// A transform list.
pub type Transforms = DurableVec<TransformOp>;

/// A list of attributes.
pub type Attrs = DurableVec<Attr>;

/// Path data.
pub type PathData = DurableVec<PathSegment>;

/// A list of points, whose elements all have one size.
pub type Points = DurableVec<Point>;

/// A list of lengths, whose elements all have one size.
pub type Lengths = DurableVec<Length>;

/// An element's `id`.
pub type Id = DurableString;

/// Every other string: text, names, values, URLs.
pub type Str = DurableString;

/// A whole SVG document: the root element, and the comments and processing
/// instructions around it.
#[derive(Durable, Debug, Clone)]
pub struct Document {
    /// Comments and processing instructions before the root element.
    pub prolog: Nodes,
    /// The root element, normally an `<svg>`.
    pub root: Element,
    /// Comments and processing instructions after the root element.
    pub epilog: Nodes,
}

/// A child of an element.
// An element is far larger in memory than a string; but an element's
// children are mostly elements anyway.
#[allow(clippy::large_enum_variant)]
#[derive(Durable, Debug, Clone)]
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
#[derive(Durable, Debug, Clone)]
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
#[derive(Durable, Debug, Clone)]
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
#[derive(Durable, Debug, Clone, Copy, PartialEq)]
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
#[derive(Durable, Debug, Clone, Copy, PartialEq)]
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
#[derive(Durable, Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: Number,
    pub y: Number,
}

/// A length or coordinate: a number and its unit.
#[derive(Durable, Debug, Clone, Copy, PartialEq)]
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
#[derive(Durable, Debug, Clone, Copy, PartialEq)]
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
#[derive(Durable, Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

/// The value of `fill` or `stroke`.
#[derive(Durable, Debug, Clone)]
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
#[derive(Durable, Debug, Clone, Copy, PartialEq)]
pub enum LineCap {
    Butt,
    Round,
    Square,
}

/// The value of `stroke-linejoin`.
#[derive(Durable, Debug, Clone, Copy, PartialEq)]
pub enum LineJoin {
    Miter,
    MiterClip,
    Round,
    Bevel,
    Arcs,
}

/// The value of `fill-rule` or `clip-rule`.
#[derive(Durable, Debug, Clone, Copy, PartialEq)]
pub enum FillRule {
    NonZero,
    EvenOdd,
}

/// An attribute, or a declaration of an inline `style`: typed where the model
/// knows the attribute and its value parses, [`Attr::Other`] otherwise.
#[derive(Durable, Debug, Clone)]
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
