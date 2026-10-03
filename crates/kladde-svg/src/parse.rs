//! Reading SVG text into the [model](crate::model).
//!
//! The XML goes through roxmltree, and every attribute the model types goes
//! through svgtypes. Nothing is rejected that is well-formed XML: an attribute
//! the model does not know, or whose value does not parse, is kept verbatim
//! as [`Attr::Other`], and an element it does not know as
//! [`ElementKind::Other`].
//!
//! What the model drops, and the canonical form therefore lacks: the XML
//! declaration, the DOCTYPE, entity references (which are expanded), and text
//! that is only whitespace outside text-content elements (`text`, `tspan`,
//! `textPath`, `title`, `desc`, `style`, `script` and what they contain).

use std::fmt;
use std::str::FromStr;

use kladde_types::PersistableVec;
use roxmltree as xml;

use crate::model::{
    Attr, Color, Document, Element, ElementKind, FillRule, Length, LengthUnit, LineCap, LineJoin,
    Node, Paint, PathSegment, Point, TransformOp,
};
use crate::Number;

pub(crate) const SVG_NS: &str = "http://www.w3.org/2000/svg";
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

/// Why a text is not an SVG document: it is not well-formed XML.
#[derive(Debug)]
pub struct ParseError(xml::Error);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not well-formed XML: {}", self.0)
    }
}

impl std::error::Error for ParseError {}

/// What a parse found, for judging how much of a document the model types
/// and how much precision [`Number`] keeps.
///
/// ```
/// let text = r#"<svg xmlns="http://www.w3.org/2000/svg"><rect x="1.23456789" display="none"/></svg>"#;
/// let (_, stats) = kladde_svg::parse_with_stats(text)?;
/// assert_eq!(stats.numbers, 1);
/// assert_eq!(stats.other_attrs, 1); // `display` is not typed
/// # Ok::<(), kladde_svg::ParseError>(())
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct ParseStats {
    /// Numbers stored as a [`Number`].
    pub numbers: u64,
    /// Numbers whose shortest decimal is not the same once stored as a
    /// [`Number`]: what `f32` loses of the source's precision. Always 0 with
    /// the `f64` feature.
    pub numbers_changed: u64,
    /// Attributes and inline style declarations, namespace declarations
    /// excluded.
    pub attrs: u64,
    /// Of those, the ones kept verbatim as [`Attr::Other`].
    pub other_attrs: u64,
    /// The bytes of all attributes' names and values.
    pub attr_bytes: u64,
    /// The bytes of the names and values of those kept as [`Attr::Other`].
    pub other_attr_bytes: u64,
}

/// Parses an SVG document.
///
/// Fails only on text that is not well-formed XML; see the [module
/// documentation](self) for what the model keeps and drops.
///
/// ```
/// use kladde_svg::model::ElementKind;
///
/// let doc = kladde_svg::parse(r#"<svg xmlns="http://www.w3.org/2000/svg"><path d="M0 0L10 10"/></svg>"#)?;
/// assert!(matches!(doc.root.kind, ElementKind::Svg));
/// assert_eq!(doc.root.children.len(), 1);
/// # Ok::<(), kladde_svg::ParseError>(())
/// ```
pub fn parse(text: &str) -> Result<Document, ParseError> {
    Parser::new(false).document(text)
}

/// Parses an SVG document, and reports what the parse found.
///
/// Slower than [`parse`], since it formats every number to compare it with
/// the source.
pub fn parse_with_stats(text: &str) -> Result<(Document, ParseStats), ParseError> {
    let mut parser = Parser::new(true);
    let doc = parser.document(text)?;
    Ok((doc, parser.stats))
}

/// The outcome of offering an attribute to an element's geometry.
enum Geometry {
    /// The attribute is not geometry of this element.
    No,
    /// The attribute was stored in the element's kind.
    Set,
    /// The attribute is geometry of this element, but its value did not
    /// parse.
    Invalid,
}

struct Parser {
    stats: ParseStats,
    count_precision: bool,
}

impl Parser {
    fn new(count_precision: bool) -> Parser {
        Parser {
            stats: ParseStats::default(),
            count_precision,
        }
    }

    fn document(&mut self, text: &str) -> Result<Document, ParseError> {
        let options = xml::ParsingOptions {
            allow_dtd: true,
            nodes_limit: u32::MAX,
            ..Default::default()
        };
        let doc = xml::Document::parse_with_options(text, options).map_err(ParseError)?;
        let root = doc.root_element();
        let (mut prolog, mut epilog) = (Vec::new(), Vec::new());
        let mut after_root = false;
        for child in doc.root().children() {
            if child == root {
                after_root = true;
            } else if let Some(node) = self.misc(child) {
                if after_root {
                    epilog.push(node);
                } else {
                    prolog.push(node);
                }
            }
        }
        Ok(Document {
            prolog: PersistableVec::from_iter(prolog),
            root: self.element(root, false),
            epilog: PersistableVec::from_iter(epilog),
        })
    }

    /// A comment or processing instruction; `None` for anything else.
    fn misc(&mut self, node: xml::Node) -> Option<Node> {
        match node.node_type() {
            xml::NodeType::Comment => Some(Node::Comment(node.text().unwrap_or("").into())),
            xml::NodeType::PI => {
                let pi = node.pi()?;
                Some(Node::ProcessingInstruction {
                    target: pi.target.into(),
                    value: pi.value.unwrap_or("").into(),
                })
            }
            _ => None,
        }
    }

    fn element(&mut self, node: xml::Node, in_text: bool) -> Element {
        let tag = node.tag_name();
        let source = node.document().input_text()[node.range().start..]
            .strip_prefix('<')
            .and_then(|rest| {
                rest.split(|c: char| c.is_whitespace() || c == '/' || c == '>')
                    .next()
            });
        let name = qualified(node, source, tag.namespace(), tag.name());
        // A typed element is written without a prefix, so only one the
        // source wrote without a prefix is typed: an SVG element with a
        // prefix, in a scope whose default namespace may be another, keeps
        // its name.
        let in_svg = matches!(tag.namespace(), None | Some(SVG_NS)) && !name.contains(':');
        let mut kind = in_svg
            .then(|| kind_for(tag.name()))
            .flatten()
            .unwrap_or_else(|| ElementKind::Other { name: name.into() });
        use ElementKind as K;
        let in_text = in_text
            || matches!(
                kind,
                K::Text | K::Tspan | K::TextPath | K::Title | K::Desc | K::Style | K::Script
            );

        let mut attrs = Vec::new();
        // Namespace declarations: those in scope here and not in the parent.
        let inherited: Vec<(Option<&str>, &str)> = node
            .parent()
            .map(|p| p.namespaces().map(|ns| (ns.name(), ns.uri())).collect())
            .unwrap_or_default();
        for ns in node.namespaces() {
            if ns.uri() == XML_NS || inherited.contains(&(ns.name(), ns.uri())) {
                continue;
            }
            let name = match ns.name() {
                Some(prefix) => format!("xmlns:{prefix}"),
                None => "xmlns".to_string(),
            };
            attrs.push(Attr::Other {
                name: name.into(),
                value: ns.uri().into(),
            });
        }

        let mut transform = Vec::new();
        let mut style = Vec::new();
        let input = node.document().input_text();
        for a in node.attributes() {
            let (name, value) = (a.name(), a.value());
            let attr = match a.namespace() {
                Some(uri) => {
                    let source = input.get(a.range_qname());
                    let name = qualified(node, source, Some(uri), name);
                    self.count(&name, value);
                    if uri == XLINK_NS && name == "xlink:href" {
                        Some(Attr::XlinkHref(value.into()))
                    } else {
                        Some(self.other(&name, value))
                    }
                }
                None => match name {
                    "transform" => {
                        self.count(name, value);
                        match self.transform_list(value) {
                            Some(ops) => {
                                transform = ops;
                                None
                            }
                            None => Some(self.other(name, value)),
                        }
                    }
                    "style" => match self.style(value) {
                        Some(decls) => {
                            style = decls;
                            None
                        }
                        None => {
                            self.count(name, value);
                            Some(self.other(name, value))
                        }
                    },
                    _ => {
                        self.count(name, value);
                        match self.geometry(&mut kind, name, value) {
                            Geometry::Set => None,
                            Geometry::Invalid => Some(self.other(name, value)),
                            Geometry::No => Some(self.typed(name, value)),
                        }
                    }
                },
            };
            attrs.extend(attr);
        }

        let mut children = Vec::new();
        for child in node.children() {
            match child.node_type() {
                xml::NodeType::Element => {
                    children.push(Node::Element(self.element(child, in_text)))
                }
                xml::NodeType::Text => {
                    let text = child.text().unwrap_or("");
                    if in_text || !text.trim().is_empty() {
                        children.push(Node::Text(text.into()));
                    }
                }
                _ => children.extend(self.misc(child)),
            }
        }

        Element {
            kind,
            transform: PersistableVec::from_iter(transform),
            attrs: PersistableVec::from_iter(attrs),
            style: PersistableVec::from_iter(style),
            children: PersistableVec::from_iter(children),
        }
    }

    /// Counts an attribute or declaration towards the statistics.
    fn count(&mut self, name: &str, value: &str) {
        self.stats.attrs += 1;
        self.stats.attr_bytes += (name.len() + value.len()) as u64;
    }

    /// An attribute kept verbatim.
    fn other(&mut self, name: &str, value: &str) -> Attr {
        self.stats.other_attrs += 1;
        self.stats.other_attr_bytes += (name.len() + value.len()) as u64;
        Attr::Other {
            name: name.into(),
            value: value.into(),
        }
    }

    /// Stores `name="value"` in `kind` if it is geometry of that kind.
    fn geometry(&mut self, kind: &mut ElementKind, name: &str, value: &str) -> Geometry {
        use ElementKind as K;
        let slot: &mut Length = match (kind, name) {
            (K::Path { d }, "d") => {
                return match self.path(value) {
                    Some(segments) => {
                        *d = PersistableVec::from_iter(segments);
                        Geometry::Set
                    }
                    None => Geometry::Invalid,
                };
            }
            (K::Polyline { points } | K::Polygon { points }, "points") => {
                return match self.points(value) {
                    Some(list) => {
                        *points = PersistableVec::from_iter(list);
                        Geometry::Set
                    }
                    None => Geometry::Invalid,
                };
            }
            (K::Use { x, .. } | K::Image { x, .. } | K::Rect { x, .. }, "x") => x,
            (K::Use { y, .. } | K::Image { y, .. } | K::Rect { y, .. }, "y") => y,
            (K::Rect { width, .. }, "width") => width,
            (K::Rect { height, .. }, "height") => height,
            (K::Circle { cx, .. } | K::Ellipse { cx, .. }, "cx") => cx,
            (K::Circle { cy, .. } | K::Ellipse { cy, .. }, "cy") => cy,
            (K::Circle { r, .. }, "r") => r,
            (K::Line { x1, .. }, "x1") => x1,
            (K::Line { y1, .. }, "y1") => y1,
            (K::Line { x2, .. }, "x2") => x2,
            (K::Line { y2, .. }, "y2") => y2,
            (K::Stop { offset }, "offset") => offset,
            _ => return Geometry::No,
        };
        match self.length(value) {
            // A zero is left out on writing, so `x="0"` and no `x` are one.
            Some(length) => {
                *slot = length;
                Geometry::Set
            }
            None => Geometry::Invalid,
        }
    }

    /// An attribute or style declaration other than geometry, typed if the
    /// model knows it and its value parses.
    fn typed(&mut self, name: &str, value: &str) -> Attr {
        let t = value.trim();
        let typed = match name {
            "id" => Some(Attr::Id(value.into())),
            "class" => Some(Attr::Class(value.into())),
            "href" => Some(Attr::Href(value.into())),
            "font-family" => Some(Attr::FontFamily(value.into())),
            "fill" => self.paint(t).map(Attr::Fill),
            "stroke" => self.paint(t).map(Attr::Stroke),
            "opacity" => self.number(t).map(Attr::Opacity),
            "fill-opacity" => self.number(t).map(Attr::FillOpacity),
            "stroke-opacity" => self.number(t).map(Attr::StrokeOpacity),
            "stop-opacity" => self.number(t).map(Attr::StopOpacity),
            "stroke-miterlimit" => self.number(t).map(Attr::StrokeMiterlimit),
            "stroke-width" => self.length(t).map(Attr::StrokeWidth),
            "stroke-dashoffset" => self.length(t).map(Attr::StrokeDashoffset),
            "font-size" => self.length(t).map(Attr::FontSize),
            "x" => self.length(t).map(Attr::X),
            "y" => self.length(t).map(Attr::Y),
            "width" => self.length(t).map(Attr::Width),
            "height" => self.length(t).map(Attr::Height),
            "rx" => self.length(t).map(Attr::Rx),
            "ry" => self.length(t).map(Attr::Ry),
            "stroke-dasharray" => self
                .length_list(t)
                .map(|list| Attr::StrokeDasharray(PersistableVec::from_iter(list))),
            "stroke-linecap" => match t {
                "butt" => Some(LineCap::Butt),
                "round" => Some(LineCap::Round),
                "square" => Some(LineCap::Square),
                _ => None,
            }
            .map(Attr::StrokeLinecap),
            "stroke-linejoin" => match t {
                "miter" => Some(LineJoin::Miter),
                "miter-clip" => Some(LineJoin::MiterClip),
                "round" => Some(LineJoin::Round),
                "bevel" => Some(LineJoin::Bevel),
                "arcs" => Some(LineJoin::Arcs),
                _ => None,
            }
            .map(Attr::StrokeLinejoin),
            "fill-rule" => fill_rule(t).map(Attr::FillRule),
            "clip-rule" => fill_rule(t).map(Attr::ClipRule),
            "stop-color" => color(t).map(Attr::StopColor),
            "color" => color(t).map(Attr::Color),
            "viewBox" => self.view_box(t),
            "gradientTransform" => self
                .transform_list(value)
                .map(|ops| Attr::GradientTransform(PersistableVec::from_iter(ops))),
            "patternTransform" => self
                .transform_list(value)
                .map(|ops| Attr::PatternTransform(PersistableVec::from_iter(ops))),
            _ => None,
        };
        typed.unwrap_or_else(|| self.other(name, value))
    }

    /// The declarations of an inline `style`, or `None` if it is more than a
    /// plain list of `name: value` declarations.
    fn style(&mut self, value: &str) -> Option<Vec<Attr>> {
        if value.contains("/*") || value.contains('!') || value.contains('\\') {
            return None;
        }
        let parts = split_declarations(value)?;
        let mut pairs = Vec::new();
        for part in parts {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (name, value) = part.split_once(':')?;
            let (name, value) = (name.trim(), value.trim());
            let plain = !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
            if !plain || value.is_empty() {
                return None;
            }
            pairs.push((name, value));
        }
        let mut decls = Vec::with_capacity(pairs.len());
        for (name, value) in pairs {
            self.count(name, value);
            decls.push(self.typed(name, value));
        }
        Some(decls)
    }

    /// A number as the model stores it, or `None` if it does not fit.
    fn num(&mut self, x: f64) -> Option<Number> {
        self.stats.numbers += 1;
        let n = x as Number;
        if !n.is_finite() {
            return None;
        }
        if self.count_precision && format!("{n}").parse::<f64>().ok() != Some(x) {
            self.stats.numbers_changed += 1;
        }
        Some(n)
    }

    fn number(&mut self, t: &str) -> Option<Number> {
        let n = svgtypes::Number::from_str(t).ok()?;
        self.num(n.0)
    }

    fn length(&mut self, t: &str) -> Option<Length> {
        let l = svgtypes::Length::from_str(t.trim()).ok()?;
        Some(Length {
            value: self.num(l.number)?,
            unit: unit(l.unit),
        })
    }

    fn length_list(&mut self, t: &str) -> Option<Vec<Length>> {
        let mut list = Vec::new();
        for l in svgtypes::LengthListParser::from(t) {
            let l = l.ok()?;
            list.push(Length {
                value: self.num(l.number)?,
                unit: unit(l.unit),
            });
        }
        (!list.is_empty()).then_some(list)
    }

    fn paint(&mut self, t: &str) -> Option<Paint> {
        Some(match svgtypes::Paint::from_str(t).ok()? {
            svgtypes::Paint::None => Paint::None,
            svgtypes::Paint::CurrentColor => Paint::CurrentColor,
            svgtypes::Paint::Inherit => Paint::Inherit,
            svgtypes::Paint::Color(c) => Paint::Color(from_svgtypes_color(c)),
            svgtypes::Paint::FuncIRI(id, None) => Paint::Url(id.into()),
            _ => return None,
        })
    }

    fn view_box(&mut self, t: &str) -> Option<Attr> {
        let vb = svgtypes::ViewBox::from_str(t).ok()?;
        Some(Attr::ViewBox {
            min_x: self.num(vb.x)?,
            min_y: self.num(vb.y)?,
            width: self.num(vb.w)?,
            height: self.num(vb.h)?,
        })
    }

    fn transform_list(&mut self, value: &str) -> Option<Vec<TransformOp>> {
        use svgtypes::TransformListToken as T;
        let mut ops = Vec::new();
        for token in svgtypes::TransformListParser::from(value) {
            ops.push(match token.ok()? {
                T::Matrix { a, b, c, d, e, f } => TransformOp::Matrix {
                    a: self.num(a)?,
                    b: self.num(b)?,
                    c: self.num(c)?,
                    d: self.num(d)?,
                    e: self.num(e)?,
                    f: self.num(f)?,
                },
                T::Translate { tx, ty } => TransformOp::Translate {
                    tx: self.num(tx)?,
                    ty: self.num(ty)?,
                },
                T::Scale { sx, sy } => TransformOp::Scale {
                    sx: self.num(sx)?,
                    sy: self.num(sy)?,
                },
                T::Rotate { angle } => TransformOp::Rotate {
                    angle: self.num(angle)?,
                },
                T::SkewX { angle } => TransformOp::SkewX {
                    angle: self.num(angle)?,
                },
                T::SkewY { angle } => TransformOp::SkewY {
                    angle: self.num(angle)?,
                },
            });
        }
        Some(ops)
    }

    fn path(&mut self, value: &str) -> Option<Vec<PathSegment>> {
        use svgtypes::PathSegment as S;
        let mut segments = Vec::new();
        for segment in svgtypes::PathParser::from(value) {
            segments.push(match segment.ok()? {
                S::MoveTo { abs, x, y } => PathSegment::MoveTo {
                    abs,
                    x: self.num(x)?,
                    y: self.num(y)?,
                },
                S::LineTo { abs, x, y } => PathSegment::LineTo {
                    abs,
                    x: self.num(x)?,
                    y: self.num(y)?,
                },
                S::HorizontalLineTo { abs, x } => PathSegment::HorizontalLineTo {
                    abs,
                    x: self.num(x)?,
                },
                S::VerticalLineTo { abs, y } => PathSegment::VerticalLineTo {
                    abs,
                    y: self.num(y)?,
                },
                S::CurveTo {
                    abs,
                    x1,
                    y1,
                    x2,
                    y2,
                    x,
                    y,
                } => PathSegment::CurveTo {
                    abs,
                    x1: self.num(x1)?,
                    y1: self.num(y1)?,
                    x2: self.num(x2)?,
                    y2: self.num(y2)?,
                    x: self.num(x)?,
                    y: self.num(y)?,
                },
                S::SmoothCurveTo { abs, x2, y2, x, y } => PathSegment::SmoothCurveTo {
                    abs,
                    x2: self.num(x2)?,
                    y2: self.num(y2)?,
                    x: self.num(x)?,
                    y: self.num(y)?,
                },
                S::Quadratic { abs, x1, y1, x, y } => PathSegment::Quadratic {
                    abs,
                    x1: self.num(x1)?,
                    y1: self.num(y1)?,
                    x: self.num(x)?,
                    y: self.num(y)?,
                },
                S::SmoothQuadratic { abs, x, y } => PathSegment::SmoothQuadratic {
                    abs,
                    x: self.num(x)?,
                    y: self.num(y)?,
                },
                S::EllipticalArc {
                    abs,
                    rx,
                    ry,
                    x_axis_rotation,
                    large_arc,
                    sweep,
                    x,
                    y,
                } => PathSegment::EllipticalArc {
                    abs,
                    rx: self.num(rx)?,
                    ry: self.num(ry)?,
                    x_axis_rotation: self.num(x_axis_rotation)?,
                    large_arc,
                    sweep,
                    x: self.num(x)?,
                    y: self.num(y)?,
                },
                S::ClosePath { abs } => PathSegment::ClosePath { abs },
            });
        }
        Some(segments)
    }

    /// A `points` list: an even number of numbers.
    fn points(&mut self, value: &str) -> Option<Vec<Point>> {
        let mut numbers = Vec::new();
        for n in svgtypes::NumberListParser::from(value) {
            numbers.push(n.ok()?);
        }
        if numbers.len() % 2 != 0 {
            return None;
        }
        numbers
            .chunks(2)
            .map(|xy| {
                Some(Point {
                    x: self.num(xy[0])?,
                    y: self.num(xy[1])?,
                })
            })
            .collect()
    }
}

/// The kind of an element in the SVG namespace, by its local name, with
/// geometry at its initial values; `None` for a name the model does not know.
fn kind_for(local: &str) -> Option<ElementKind> {
    use ElementKind as K;
    let z = Length::ZERO;
    Some(match local {
        "svg" => K::Svg,
        "g" => K::G,
        "defs" => K::Defs,
        "symbol" => K::Symbol,
        "use" => K::Use { x: z, y: z },
        "path" => K::Path {
            d: PersistableVec::new(),
        },
        "rect" => K::Rect {
            x: z,
            y: z,
            width: z,
            height: z,
        },
        "circle" => K::Circle { cx: z, cy: z, r: z },
        "ellipse" => K::Ellipse { cx: z, cy: z },
        "line" => K::Line {
            x1: z,
            y1: z,
            x2: z,
            y2: z,
        },
        "polyline" => K::Polyline {
            points: PersistableVec::new(),
        },
        "polygon" => K::Polygon {
            points: PersistableVec::new(),
        },
        "text" => K::Text,
        "tspan" => K::Tspan,
        "textPath" => K::TextPath,
        "linearGradient" => K::LinearGradient,
        "radialGradient" => K::RadialGradient,
        "stop" => K::Stop { offset: z },
        "clipPath" => K::ClipPath,
        "mask" => K::Mask,
        "pattern" => K::Pattern,
        "marker" => K::Marker,
        "image" => K::Image { x: z, y: z },
        "style" => K::Style,
        "title" => K::Title,
        "desc" => K::Desc,
        "metadata" => K::Metadata,
        "filter" => K::Filter,
        "switch" => K::Switch,
        "a" => K::A,
        "script" => K::Script,
        "foreignObject" => K::ForeignObject,
        _ => return None,
    })
}

/// The name an element or attribute is written with: as the source spelled
/// it, if `source` is that spelling, or else `prefix:local` when its
/// namespace is bound to a prefix where it occurs, and `local` otherwise.
///
/// The source spelling matters where one namespace is bound to several
/// prefixes, or to a prefix and the default namespace at once; roxmltree
/// resolves names to namespaces and keeps no prefixes. It is not available
/// for content expanded from an entity, whose source range lies elsewhere.
fn qualified(
    node: xml::Node,
    source: Option<&str>,
    namespace: Option<&str>,
    local: &str,
) -> String {
    if let Some(source) = source {
        let spelled_so = match source.split_once(':') {
            Some((prefix, rest)) => {
                rest == local && node.lookup_namespace_uri(Some(prefix)) == namespace
            }
            None => source == local && node.lookup_namespace_uri(None) == namespace,
        };
        if spelled_so {
            return source.to_string();
        }
    }
    let prefix = match namespace {
        None => None,
        Some(XML_NS) => Some("xml"),
        Some(uri) => node.lookup_prefix(uri),
    };
    match prefix {
        Some(prefix) if !prefix.is_empty() => format!("{prefix}:{local}"),
        _ => local.to_string(),
    }
}

/// Splits a style at the semicolons outside quotes and parentheses, or
/// `None` if its quotes or parentheses do not balance.
fn split_declarations(value: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    let (mut start, mut depth, mut quote) = (0, 0i32, None);
    for (i, c) in value.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
            }
            (None, ';') if depth == 0 => {
                parts.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if quote.is_some() || depth != 0 {
        return None;
    }
    parts.push(&value[start..]);
    Some(parts)
}

fn fill_rule(t: &str) -> Option<FillRule> {
    match t {
        "nonzero" => Some(FillRule::NonZero),
        "evenodd" => Some(FillRule::EvenOdd),
        _ => None,
    }
}

fn color(t: &str) -> Option<Color> {
    svgtypes::Color::from_str(t).ok().map(from_svgtypes_color)
}

fn from_svgtypes_color(c: svgtypes::Color) -> Color {
    Color {
        r: c.red,
        g: c.green,
        b: c.blue,
        a: c.alpha,
    }
}

fn unit(u: svgtypes::LengthUnit) -> LengthUnit {
    use svgtypes::LengthUnit as U;
    match u {
        U::None => LengthUnit::None,
        U::Em => LengthUnit::Em,
        U::Ex => LengthUnit::Ex,
        U::Px => LengthUnit::Px,
        U::In => LengthUnit::In,
        U::Cm => LengthUnit::Cm,
        U::Mm => LengthUnit::Mm,
        U::Pt => LengthUnit::Pt,
        U::Pc => LengthUnit::Pc,
        U::Percent => LengthUnit::Percent,
    }
}
