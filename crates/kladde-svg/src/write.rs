//! Writing the [model](crate::model) as SVG text.
//!
//! The output is deterministic: an element's attributes come in a fixed
//! order — its geometry, then `transform`, then its other attributes in their
//! stored order, then `style` — and every number is written as the shortest
//! decimal that reads back as the same [`Number`]. Geometry
//! that is zero is left out, since zero is its initial value.

use std::fmt::Write as _;

use crate::model::{
    Attr, Color, Document, Element, ElementKind, FillRule, Length, LengthUnit, LineCap, LineJoin,
    Node, Paint, PathSegment, Point, TransformOp,
};
use crate::Number;

/// Writes a document compactly, with no whitespace between elements: the
/// canonical form.
///
/// ```
/// let doc = kladde_svg::parse(r#"<svg xmlns="http://www.w3.org/2000/svg">
///     <rect x="0" width="10" height="5"/>
/// </svg>"#)?;
/// assert_eq!(
///     kladde_svg::write(&doc),
///     r#"<svg xmlns="http://www.w3.org/2000/svg"><rect width="10" height="5"/></svg>"#
/// );
/// # Ok::<(), kladde_svg::ParseError>(())
/// ```
pub fn write(doc: &Document) -> String {
    Writer::new(false).document(doc)
}

/// Writes a document with each element on a line of its own, indented by
/// depth, except inside text-content elements, whose whitespace is content.
/// It parses to the same document as [`write()`]'s output.
///
/// ```
/// let text = r#"<svg xmlns="http://www.w3.org/2000/svg"><g><rect width="1"/></g></svg>"#;
/// let doc = kladde_svg::parse(text)?;
/// let indented = kladde_svg::write_indented(&doc);
/// assert!(indented.contains("\n  <g>\n    <rect"));
/// assert_eq!(kladde_svg::canon(&indented)?, text);
/// # Ok::<(), kladde_svg::ParseError>(())
/// ```
pub fn write_indented(doc: &Document) -> String {
    Writer::new(true).document(doc)
}

struct Writer {
    out: String,
    indent: bool,
}

impl Writer {
    fn new(indent: bool) -> Writer {
        Writer {
            out: String::new(),
            indent,
        }
    }

    fn document(mut self, doc: &Document) -> String {
        for node in doc.prolog.iter() {
            self.node(node, 0, false);
            self.out.push('\n');
        }
        self.element(&doc.root, 0, false);
        for node in doc.epilog.iter() {
            self.out.push('\n');
            self.node(node, 0, false);
        }
        if self.indent {
            self.out.push('\n');
        }
        self.out
    }

    fn node(&mut self, node: &Node, depth: usize, in_text: bool) {
        match node {
            Node::Element(e) => self.element(e, depth, in_text),
            Node::Text(t) => escape_text(&mut self.out, t),
            Node::Comment(c) => {
                self.out.push_str("<!--");
                self.out.push_str(c);
                self.out.push_str("-->");
            }
            Node::ProcessingInstruction { target, value } => {
                self.out.push_str("<?");
                self.out.push_str(target);
                if !value.is_empty() {
                    self.out.push(' ');
                    self.out.push_str(value);
                }
                self.out.push_str("?>");
            }
        }
    }

    fn element(&mut self, e: &Element, depth: usize, in_text: bool) {
        use ElementKind as K;
        let name = tag_name(&e.kind);
        self.out.push('<');
        self.out.push_str(name);

        let mut value = String::new();
        for (attr, length) in geometry(&e.kind) {
            if !length.is_zero() {
                value.clear();
                write_length(&mut value, length);
                self.attribute(attr, &value);
            }
        }
        match &e.kind {
            K::Path { d } if !d.is_empty() => {
                value.clear();
                write_path(&mut value, d);
                self.attribute("d", &value);
            }
            K::Polyline { points } | K::Polygon { points } if !points.is_empty() => {
                value.clear();
                write_points(&mut value, points);
                self.attribute("points", &value);
            }
            _ => {}
        }
        if !e.transform.is_empty() {
            value.clear();
            write_transform(&mut value, &e.transform);
            self.attribute("transform", &value);
        }
        for attr in e.attrs.iter() {
            value.clear();
            let name = write_attr(&mut value, attr);
            self.attribute(name, &value);
        }
        if !e.style.is_empty() {
            value.clear();
            for (i, decl) in e.style.iter().enumerate() {
                if i > 0 {
                    value.push(';');
                }
                let mut v = String::new();
                let name = write_attr(&mut v, decl);
                value.push_str(name);
                value.push(':');
                value.push_str(&v);
            }
            self.attribute("style", &value);
        }

        if e.children.is_empty() {
            self.out.push_str("/>");
            return;
        }
        self.out.push('>');
        let in_text = in_text
            || matches!(
                e.kind,
                K::Text | K::Tspan | K::TextPath | K::Title | K::Desc | K::Style | K::Script
            );
        // Indenting adds whitespace-only text, which is content in text and
        // next to other text.
        let indent =
            self.indent && !in_text && !e.children.iter().any(|c| matches!(c, Node::Text(_)));
        for child in e.children.iter() {
            if indent {
                self.newline(depth + 1);
            }
            self.node(child, depth + 1, in_text);
        }
        if indent {
            self.newline(depth);
        }
        self.out.push_str("</");
        self.out.push_str(name);
        self.out.push('>');
    }

    fn newline(&mut self, depth: usize) {
        self.out.push('\n');
        for _ in 0..depth {
            self.out.push_str("  ");
        }
    }

    fn attribute(&mut self, name: &str, value: &str) {
        self.out.push(' ');
        self.out.push_str(name);
        self.out.push_str("=\"");
        escape_attribute(&mut self.out, value);
        self.out.push('"');
    }
}

/// The name an element is written with.
pub(crate) fn tag_name(kind: &ElementKind) -> &str {
    use ElementKind as K;
    match kind {
        K::Svg => "svg",
        K::G => "g",
        K::Defs => "defs",
        K::Symbol => "symbol",
        K::Use { .. } => "use",
        K::Path { .. } => "path",
        K::Rect { .. } => "rect",
        K::Circle { .. } => "circle",
        K::Ellipse { .. } => "ellipse",
        K::Line { .. } => "line",
        K::Polyline { .. } => "polyline",
        K::Polygon { .. } => "polygon",
        K::Text => "text",
        K::Tspan => "tspan",
        K::TextPath => "textPath",
        K::LinearGradient => "linearGradient",
        K::RadialGradient => "radialGradient",
        K::Stop { .. } => "stop",
        K::ClipPath => "clipPath",
        K::Mask => "mask",
        K::Pattern => "pattern",
        K::Marker => "marker",
        K::Image { .. } => "image",
        K::Style => "style",
        K::Title => "title",
        K::Desc => "desc",
        K::Metadata => "metadata",
        K::Filter => "filter",
        K::Switch => "switch",
        K::A => "a",
        K::Script => "script",
        K::ForeignObject => "foreignObject",
        K::Other { name } => name,
    }
}

/// The length-valued geometry of an element kind, by attribute name, in the
/// order it is written.
fn geometry(kind: &ElementKind) -> Vec<(&'static str, &Length)> {
    use ElementKind as K;
    match kind {
        K::Use { x, y } | K::Image { x, y } => vec![("x", x), ("y", y)],
        K::Rect {
            x,
            y,
            width,
            height,
        } => vec![("x", x), ("y", y), ("width", width), ("height", height)],
        K::Circle { cx, cy, r } => vec![("cx", cx), ("cy", cy), ("r", r)],
        K::Ellipse { cx, cy } => vec![("cx", cx), ("cy", cy)],
        K::Line { x1, y1, x2, y2 } => vec![("x1", x1), ("y1", y1), ("x2", x2), ("y2", y2)],
        K::Stop { offset } => vec![("offset", offset)],
        _ => Vec::new(),
    }
}

/// Writes an attribute's or declaration's value, and returns its name.
fn write_attr<'a>(out: &mut String, attr: &'a Attr) -> &'a str {
    match attr {
        Attr::Id(s) => {
            out.push_str(s);
            "id"
        }
        Attr::Class(s) => {
            out.push_str(s);
            "class"
        }
        Attr::Href(s) => {
            out.push_str(s);
            "href"
        }
        Attr::XlinkHref(s) => {
            out.push_str(s);
            "xlink:href"
        }
        Attr::FontFamily(s) => {
            out.push_str(s);
            "font-family"
        }
        Attr::Fill(p) => {
            write_paint(out, p);
            "fill"
        }
        Attr::Stroke(p) => {
            write_paint(out, p);
            "stroke"
        }
        Attr::Opacity(n) => {
            write_number(out, *n);
            "opacity"
        }
        Attr::FillOpacity(n) => {
            write_number(out, *n);
            "fill-opacity"
        }
        Attr::StrokeOpacity(n) => {
            write_number(out, *n);
            "stroke-opacity"
        }
        Attr::StopOpacity(n) => {
            write_number(out, *n);
            "stop-opacity"
        }
        Attr::StrokeMiterlimit(n) => {
            write_number(out, *n);
            "stroke-miterlimit"
        }
        Attr::StrokeWidth(l) => {
            write_length(out, l);
            "stroke-width"
        }
        Attr::StrokeDashoffset(l) => {
            write_length(out, l);
            "stroke-dashoffset"
        }
        Attr::FontSize(l) => {
            write_length(out, l);
            "font-size"
        }
        Attr::X(l) => {
            write_length(out, l);
            "x"
        }
        Attr::Y(l) => {
            write_length(out, l);
            "y"
        }
        Attr::Width(l) => {
            write_length(out, l);
            "width"
        }
        Attr::Height(l) => {
            write_length(out, l);
            "height"
        }
        Attr::Rx(l) => {
            write_length(out, l);
            "rx"
        }
        Attr::Ry(l) => {
            write_length(out, l);
            "ry"
        }
        Attr::StrokeDasharray(list) => {
            for (i, l) in list.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                write_length(out, l);
            }
            "stroke-dasharray"
        }
        Attr::StrokeLinecap(cap) => {
            out.push_str(match cap {
                LineCap::Butt => "butt",
                LineCap::Round => "round",
                LineCap::Square => "square",
            });
            "stroke-linecap"
        }
        Attr::StrokeLinejoin(join) => {
            out.push_str(match join {
                LineJoin::Miter => "miter",
                LineJoin::MiterClip => "miter-clip",
                LineJoin::Round => "round",
                LineJoin::Bevel => "bevel",
                LineJoin::Arcs => "arcs",
            });
            "stroke-linejoin"
        }
        Attr::FillRule(rule) => {
            write_fill_rule(out, *rule);
            "fill-rule"
        }
        Attr::ClipRule(rule) => {
            write_fill_rule(out, *rule);
            "clip-rule"
        }
        Attr::StopColor(c) => {
            write_color(out, c);
            "stop-color"
        }
        Attr::Color(c) => {
            write_color(out, c);
            "color"
        }
        Attr::ViewBox {
            min_x,
            min_y,
            width,
            height,
        } => {
            write_numbers(out, &[*min_x, *min_y, *width, *height]);
            "viewBox"
        }
        Attr::GradientTransform(ops) => {
            write_transform(out, ops);
            "gradientTransform"
        }
        Attr::PatternTransform(ops) => {
            write_transform(out, ops);
            "patternTransform"
        }
        Attr::Other { name, value } => {
            out.push_str(value);
            name
        }
    }
}

fn write_number(out: &mut String, n: Number) {
    write!(out, "{n}").unwrap();
}

/// Numbers separated by single spaces.
fn write_numbers(out: &mut String, numbers: &[Number]) {
    for (i, n) in numbers.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        write_number(out, *n);
    }
}

fn write_length(out: &mut String, l: &Length) {
    write_number(out, l.value);
    out.push_str(match l.unit {
        LengthUnit::None => "",
        LengthUnit::Em => "em",
        LengthUnit::Ex => "ex",
        LengthUnit::Px => "px",
        LengthUnit::In => "in",
        LengthUnit::Cm => "cm",
        LengthUnit::Mm => "mm",
        LengthUnit::Pt => "pt",
        LengthUnit::Pc => "pc",
        LengthUnit::Percent => "%",
    });
}

fn write_color(out: &mut String, c: &Color) {
    if c.a == 255 {
        write!(out, "#{:02x}{:02x}{:02x}", c.r, c.g, c.b).unwrap();
    } else {
        write!(out, "#{:02x}{:02x}{:02x}{:02x}", c.r, c.g, c.b, c.a).unwrap();
    }
}

fn write_paint(out: &mut String, p: &Paint) {
    match p {
        Paint::None => out.push_str("none"),
        Paint::CurrentColor => out.push_str("currentColor"),
        Paint::Inherit => out.push_str("inherit"),
        Paint::Color(c) => write_color(out, c),
        Paint::Url(id) => {
            out.push_str("url(#");
            out.push_str(id);
            out.push(')');
        }
    }
}

fn write_fill_rule(out: &mut String, rule: FillRule) {
    out.push_str(match rule {
        FillRule::NonZero => "nonzero",
        FillRule::EvenOdd => "evenodd",
    });
}

fn write_transform(out: &mut String, ops: &[TransformOp]) {
    for (i, op) in ops.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let (name, args): (&str, &[Number]) = match op {
            TransformOp::Matrix { a, b, c, d, e, f } => ("matrix", &[*a, *b, *c, *d, *e, *f]),
            TransformOp::Translate { tx, ty } => ("translate", &[*tx, *ty]),
            TransformOp::Scale { sx, sy } => ("scale", &[*sx, *sy]),
            TransformOp::Rotate { angle } => ("rotate", &[*angle]),
            TransformOp::SkewX { angle } => ("skewX", &[*angle]),
            TransformOp::SkewY { angle } => ("skewY", &[*angle]),
        };
        out.push_str(name);
        out.push('(');
        write_numbers(out, args);
        out.push(')');
    }
}

/// Path data with every command letter written out, segments back to back.
pub(crate) fn write_path(out: &mut String, d: &[PathSegment]) {
    let flag = |b: bool| if b { 1.0 } else { 0.0 };
    for segment in d {
        let (letter, abs, args): (u8, bool, &[Number]) = match segment {
            PathSegment::MoveTo { abs, x, y } => (b'm', *abs, &[*x, *y]),
            PathSegment::LineTo { abs, x, y } => (b'l', *abs, &[*x, *y]),
            PathSegment::HorizontalLineTo { abs, x } => (b'h', *abs, &[*x]),
            PathSegment::VerticalLineTo { abs, y } => (b'v', *abs, &[*y]),
            PathSegment::CurveTo {
                abs,
                x1,
                y1,
                x2,
                y2,
                x,
                y,
            } => (b'c', *abs, &[*x1, *y1, *x2, *y2, *x, *y]),
            PathSegment::SmoothCurveTo { abs, x2, y2, x, y } => (b's', *abs, &[*x2, *y2, *x, *y]),
            PathSegment::Quadratic { abs, x1, y1, x, y } => (b'q', *abs, &[*x1, *y1, *x, *y]),
            PathSegment::SmoothQuadratic { abs, x, y } => (b't', *abs, &[*x, *y]),
            PathSegment::EllipticalArc {
                abs,
                rx,
                ry,
                x_axis_rotation,
                large_arc,
                sweep,
                x,
                y,
            } => (
                b'a',
                *abs,
                &[
                    *rx,
                    *ry,
                    *x_axis_rotation,
                    flag(*large_arc),
                    flag(*sweep),
                    *x,
                    *y,
                ],
            ),
            PathSegment::ClosePath { abs } => (b'z', *abs, &[]),
        };
        out.push(if abs {
            letter.to_ascii_uppercase()
        } else {
            letter
        } as char);
        write_numbers(out, args);
    }
}

fn write_points(out: &mut String, points: &[Point]) {
    for (i, p) in points.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        write_number(out, p.x);
        out.push(',');
        write_number(out, p.y);
    }
}

fn escape_text(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#13;"),
            c => out.push(c),
        }
    }
}

/// Escapes an attribute value, including the whitespace characters that XML
/// would otherwise normalize to spaces when reading it back.
fn escape_attribute(out: &mut String, value: &str) {
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#9;"),
            c => out.push(c),
        }
    }
}
