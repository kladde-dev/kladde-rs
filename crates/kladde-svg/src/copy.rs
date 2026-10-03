//! Deep copies of model values.
//!
//! kladde's containers do not implement `Clone`, because a clone would share
//! the original's allocation, so duplicating a shape means building a fresh
//! value that owns no allocation yet. [`DeepCopy`] does that for every type of
//! the model.

use kladde_types::{PersistableString, PersistableVec};

use crate::model::{
    Attr, Color, Document, Element, ElementKind, FillRule, Length, LengthUnit, LineCap, LineJoin,
    Node, Paint, PathSegment, Point, TransformOp,
};

/// A copy of a value that shares nothing with the original: it owns no
/// allocation until it is stored somewhere.
///
/// ```
/// use kladde::Kladde;
/// use kladde_svg::DeepCopy;
///
/// let text = r#"<svg xmlns="http://www.w3.org/2000/svg"><rect width="1"/></svg>"#;
/// let mut drawing = Kladde::new(kladde_svg::parse(text)?);
/// let copy = drawing.get().root.deep_copy();
/// drawing
///     .guard()
///     .root_mut()
///     .children_mut()
///     .push(kladde_svg::model::Node::Element(copy))
///     .unwrap();
/// assert_eq!(drawing.get().root.children.len(), 2);
/// # Ok::<(), kladde_svg::ParseError>(())
/// ```
pub trait DeepCopy {
    /// A copy of `self` that owns no allocation.
    fn deep_copy(&self) -> Self;
}

impl DeepCopy for PersistableString {
    fn deep_copy(&self) -> Self {
        PersistableString::from(&**self)
    }
}

impl<T: DeepCopy> DeepCopy for PersistableVec<T> {
    fn deep_copy(&self) -> Self {
        self.iter().map(DeepCopy::deep_copy).collect()
    }
}

macro_rules! copy_by_value {
    ($($ty:ty),*) => {
        $(impl DeepCopy for $ty {
            fn deep_copy(&self) -> Self {
                *self
            }
        })*
    };
}

copy_by_value!(
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

impl DeepCopy for Document {
    fn deep_copy(&self) -> Self {
        Document {
            prolog: self.prolog.deep_copy(),
            root: self.root.deep_copy(),
            epilog: self.epilog.deep_copy(),
        }
    }
}

impl DeepCopy for Node {
    fn deep_copy(&self) -> Self {
        match self {
            Node::Element(e) => Node::Element(e.deep_copy()),
            Node::Text(t) => Node::Text(t.deep_copy()),
            Node::Comment(c) => Node::Comment(c.deep_copy()),
            Node::ProcessingInstruction { target, value } => Node::ProcessingInstruction {
                target: target.deep_copy(),
                value: value.deep_copy(),
            },
        }
    }
}

impl DeepCopy for Element {
    fn deep_copy(&self) -> Self {
        Element {
            kind: self.kind.deep_copy(),
            transform: self.transform.deep_copy(),
            attrs: self.attrs.deep_copy(),
            style: self.style.deep_copy(),
            children: self.children.deep_copy(),
        }
    }
}

impl DeepCopy for ElementKind {
    fn deep_copy(&self) -> Self {
        use ElementKind as K;
        match self {
            K::Svg => K::Svg,
            K::G => K::G,
            K::Defs => K::Defs,
            K::Symbol => K::Symbol,
            K::Use { x, y } => K::Use { x: *x, y: *y },
            K::Path { d } => K::Path { d: d.deep_copy() },
            K::Rect {
                x,
                y,
                width,
                height,
            } => K::Rect {
                x: *x,
                y: *y,
                width: *width,
                height: *height,
            },
            K::Circle { cx, cy, r } => K::Circle {
                cx: *cx,
                cy: *cy,
                r: *r,
            },
            K::Ellipse { cx, cy } => K::Ellipse { cx: *cx, cy: *cy },
            K::Line { x1, y1, x2, y2 } => K::Line {
                x1: *x1,
                y1: *y1,
                x2: *x2,
                y2: *y2,
            },
            K::Polyline { points } => K::Polyline {
                points: points.deep_copy(),
            },
            K::Polygon { points } => K::Polygon {
                points: points.deep_copy(),
            },
            K::Text => K::Text,
            K::Tspan => K::Tspan,
            K::TextPath => K::TextPath,
            K::LinearGradient => K::LinearGradient,
            K::RadialGradient => K::RadialGradient,
            K::Stop { offset } => K::Stop { offset: *offset },
            K::ClipPath => K::ClipPath,
            K::Mask => K::Mask,
            K::Pattern => K::Pattern,
            K::Marker => K::Marker,
            K::Image { x, y } => K::Image { x: *x, y: *y },
            K::Style => K::Style,
            K::Title => K::Title,
            K::Desc => K::Desc,
            K::Metadata => K::Metadata,
            K::Filter => K::Filter,
            K::Switch => K::Switch,
            K::A => K::A,
            K::Script => K::Script,
            K::ForeignObject => K::ForeignObject,
            K::Other { name } => K::Other {
                name: name.deep_copy(),
            },
        }
    }
}

impl DeepCopy for Paint {
    fn deep_copy(&self) -> Self {
        match self {
            Paint::None => Paint::None,
            Paint::CurrentColor => Paint::CurrentColor,
            Paint::Inherit => Paint::Inherit,
            Paint::Color(c) => Paint::Color(*c),
            Paint::Url(id) => Paint::Url(id.deep_copy()),
        }
    }
}

impl DeepCopy for Attr {
    fn deep_copy(&self) -> Self {
        match self {
            Attr::Id(s) => Attr::Id(s.deep_copy()),
            Attr::Class(s) => Attr::Class(s.deep_copy()),
            Attr::Href(s) => Attr::Href(s.deep_copy()),
            Attr::XlinkHref(s) => Attr::XlinkHref(s.deep_copy()),
            Attr::Fill(p) => Attr::Fill(p.deep_copy()),
            Attr::Stroke(p) => Attr::Stroke(p.deep_copy()),
            Attr::Opacity(n) => Attr::Opacity(*n),
            Attr::FillOpacity(n) => Attr::FillOpacity(*n),
            Attr::StrokeOpacity(n) => Attr::StrokeOpacity(*n),
            Attr::StopOpacity(n) => Attr::StopOpacity(*n),
            Attr::StrokeWidth(l) => Attr::StrokeWidth(*l),
            Attr::StrokeDashoffset(l) => Attr::StrokeDashoffset(*l),
            Attr::StrokeMiterlimit(n) => Attr::StrokeMiterlimit(*n),
            Attr::StrokeDasharray(list) => Attr::StrokeDasharray(list.deep_copy()),
            Attr::StrokeLinecap(c) => Attr::StrokeLinecap(*c),
            Attr::StrokeLinejoin(j) => Attr::StrokeLinejoin(*j),
            Attr::FillRule(r) => Attr::FillRule(*r),
            Attr::ClipRule(r) => Attr::ClipRule(*r),
            Attr::StopColor(c) => Attr::StopColor(*c),
            Attr::Color(c) => Attr::Color(*c),
            Attr::FontSize(l) => Attr::FontSize(*l),
            Attr::FontFamily(s) => Attr::FontFamily(s.deep_copy()),
            Attr::ViewBox {
                min_x,
                min_y,
                width,
                height,
            } => Attr::ViewBox {
                min_x: *min_x,
                min_y: *min_y,
                width: *width,
                height: *height,
            },
            Attr::X(l) => Attr::X(*l),
            Attr::Y(l) => Attr::Y(*l),
            Attr::Width(l) => Attr::Width(*l),
            Attr::Height(l) => Attr::Height(*l),
            Attr::Rx(l) => Attr::Rx(*l),
            Attr::Ry(l) => Attr::Ry(*l),
            Attr::GradientTransform(ops) => Attr::GradientTransform(ops.deep_copy()),
            Attr::PatternTransform(ops) => Attr::PatternTransform(ops.deep_copy()),
            Attr::Other { name, value } => Attr::Other {
                name: name.deep_copy(),
                value: value.deep_copy(),
            },
        }
    }
}
