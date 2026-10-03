//! The canonical form is stable, the model types what it should, and a
//! document survives a trip through kladde, a reopen included, byte for byte.

use kladde::{Kladde, MemoryStorage, Options, Persistable};
use kladde_svg::model::{
    Attr, Document, ElementKind, Length, LengthUnit, Node, Paint, PathSegment,
};
use kladde_svg::{canon, parse, parse_with_stats, write, write_indented};

const NS: &str = r#"xmlns="http://www.w3.org/2000/svg""#;

fn svg(body: &str) -> String {
    format!("<svg {NS}>{body}</svg>")
}

/// `canon` is idempotent, `write_indented` parses to the same document, and
/// kladde reproduces the canonical form before and after a reopen.
fn check(text: &str) -> String {
    let canonical = canon(text).unwrap();
    assert_eq!(canon(&canonical).unwrap(), canonical, "canon is idempotent");
    let indented = write_indented(&parse(text).unwrap());
    assert_eq!(
        canon(&indented).unwrap(),
        canonical,
        "indenting changes nothing"
    );

    let storage = MemoryStorage::new();
    let drawing: Kladde<Document> = Kladde::create_in(
        Box::new(storage.clone()),
        parse(text).unwrap(),
        Options::default(),
    )
    .unwrap();
    assert_eq!(write(drawing.get()), canonical, "in memory");
    drawing.close().unwrap();
    let image = MemoryStorage::from_image(storage.image());
    let reopened: Kladde<Document> = Kladde::open_in(Box::new(image), Options::default()).unwrap();
    assert_eq!(write(reopened.get()), canonical, "after a reopen");
    canonical
}

#[test]
fn path_data_keeps_every_command_and_its_case() {
    let d = "M10 20l5 5H1h2V3v4C1 2 3 4 5 6c1 2 3 4 5 6S1 2 3 4s1 2 3 4Q1 2 3 4q1 2 3 4T1 2t3 4\
             A5 6 30 1 0 7 8a5 6 30 0 1 7 8Zz";
    let canonical = check(&svg(&format!(r#"<path d="{d}"/>"#)));
    assert_eq!(canonical, svg(&format!(r#"<path d="{d}"/>"#)));
    let doc = parse(&canonical).unwrap();
    let Node::Element(path) = &doc.root.children[0] else {
        panic!()
    };
    let ElementKind::Path { d } = &path.kind else {
        panic!()
    };
    assert_eq!(d.len(), 18);
    assert_eq!(
        d[1],
        PathSegment::LineTo {
            abs: false,
            x: 5.0,
            y: 5.0
        }
    );
}

#[test]
fn implicit_repetitions_and_separators_are_normalized() {
    let canonical = check(&svg(r#"<path d="M 10,20 30,40 L.5-.5 1e1 2e-1 z"/>"#));
    assert_eq!(
        canonical,
        svg(r#"<path d="M10 20L30 40L0.5 -0.5L10 0.2z"/>"#)
    );
}

#[test]
fn geometry_is_typed_and_zero_is_left_out() {
    let canonical = check(&svg(
        r#"<rect x="0" y="1.5" width="10%" height="2em" rx="3"/><circle cx="1" cy="0" r="5mm"/><line x2="4in"/>"#,
    ));
    assert_eq!(
        canonical,
        svg(
            r#"<rect y="1.5" width="10%" height="2em" rx="3"/><circle cx="1" r="5mm"/><line x2="4in"/>"#
        )
    );
    let doc = parse(&canonical).unwrap();
    let Node::Element(rect) = &doc.root.children[0] else {
        panic!()
    };
    assert!(matches!(
        rect.kind,
        ElementKind::Rect {
            width: Length {
                unit: LengthUnit::Percent,
                ..
            },
            ..
        }
    ));
    assert!(matches!(rect.attrs[0], Attr::Rx(_)));
}

#[test]
fn transforms_colors_and_paint() {
    let canonical = check(&svg(
        r##"<g transform="translate(10) rotate(45 5 5) scale(2)" fill="red" stroke="url(#grad)" opacity="0.5" stroke-dasharray="1, 2 3"><stop stop-color="rgba(0,0,255,0.5)" offset="50%"/></g>"##,
    ));
    assert_eq!(
        canonical,
        svg(
            r##"<g transform="translate(10 0) translate(5 5) rotate(45) translate(-5 -5) scale(2 2)" fill="#ff0000" stroke="url(#grad)" opacity="0.5" stroke-dasharray="1 2 3"><stop offset="50%" stop-color="#0000ff80"/></g>"##
        )
    );
    let doc = parse(&canonical).unwrap();
    let Node::Element(g) = &doc.root.children[0] else {
        panic!()
    };
    assert_eq!(g.transform.len(), 5);
    assert!(matches!(&g.attrs[0], Attr::Fill(Paint::Color(c)) if c.r == 255));
    assert!(matches!(&g.attrs[1], Attr::Stroke(Paint::Url(id)) if id == "grad"));
}

#[test]
fn inline_style_is_parsed_into_declarations() {
    let canonical = check(&svg(
        r#"<path style="fill:#00f; stroke-width : 2px;font-family:'A;B', serif;display:inline"/>"#,
    ));
    assert_eq!(
        canonical,
        svg(
            r#"<path style="fill:#0000ff;stroke-width:2px;font-family:'A;B', serif;display:inline"/>"#
        )
    );
    let doc = parse(&canonical).unwrap();
    let Node::Element(path) = &doc.root.children[0] else {
        panic!()
    };
    assert_eq!(path.style.len(), 4);
    assert!(matches!(path.style[0], Attr::Fill(_)));
    assert!(matches!(path.style[3], Attr::Other { .. }));
}

#[test]
fn a_style_that_is_not_plain_declarations_is_kept_verbatim() {
    let text = svg(r#"<path style="fill:red !important; /* x */"/>"#);
    let canonical = check(&text);
    assert_eq!(canonical, text);
}

#[test]
fn values_that_do_not_parse_are_kept_verbatim() {
    let text = svg(
        r#"<rect x="auto" fill="url(other.svg#a)" stroke-width="calc(1px + 2px)"/><path d="M0 0 L"/>"#,
    );
    let canonical = check(&text);
    assert_eq!(canonical, text);
}

#[test]
fn namespaces_and_foreign_content_survive() {
    let text = format!(
        r##"<svg {NS} xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:inkscape="http://www.inkscape.org/namespaces/inkscape" inkscape:version="1.3"><g inkscape:label="Layer 1" inkscape:groupmode="layer"><use xlink:href="#a" x="5"/></g><metadata><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="">hi</rdf:Description></rdf:RDF></metadata></svg>"##
    );
    let canonical = check(&text);
    let doc = parse(&canonical).unwrap();
    let Node::Element(g) = &doc.root.children[0] else {
        panic!()
    };
    let Node::Element(use_) = &g.children[0] else {
        panic!()
    };
    assert!(matches!(&use_.attrs[0], Attr::XlinkHref(href) if href == "#a"));
    assert_eq!(
        canonical,
        format!(
            r##"<svg {NS} xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:inkscape="http://www.inkscape.org/namespaces/inkscape" inkscape:version="1.3"><g inkscape:label="Layer 1" inkscape:groupmode="layer"><use x="5" xlink:href="#a"/></g><metadata><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="">hi</rdf:Description></rdf:RDF></metadata></svg>"##
        )
    );
}

#[test]
fn prefixes_are_kept_as_the_source_spelled_them() {
    // The SVG namespace bound to the default and to `svg:` at once, and
    // XLink bound to a prefix other than `xlink:`.
    let text = format!(
        r##"<svg {NS} xmlns:svg="http://www.w3.org/2000/svg" xmlns:dahut="http://www.w3.org/1999/xlink"><linearGradient href="#a" svg:href="#b"/><a dahut:href="c">x</a></svg>"##
    );
    let canonical = check(&text);
    assert_eq!(canonical, text);
    let doc = parse(&canonical).unwrap();
    let Node::Element(a) = &doc.root.children[1] else {
        panic!()
    };
    assert!(matches!(&a.attrs[0], Attr::Other { name, .. } if name == "dahut:href"));
}

#[test]
fn an_svg_element_with_a_prefix_keeps_it() {
    // `s:rect` is an SVG rect, `rect` in this scope is not.
    let text = format!(
        r#"<svg {NS}><s:g xmlns="http://www.example.org/notsvg" xmlns:s="http://www.w3.org/2000/svg"><s:rect width="1"/><rect width="2"/></s:g></svg>"#
    );
    let canonical = check(&text);
    assert_eq!(canonical, text);
    let doc = parse(&canonical).unwrap();
    let Node::Element(g) = &doc.root.children[0] else {
        panic!()
    };
    assert!(matches!(&g.kind, ElementKind::Other { name } if name == "s:g"));
    let Node::Element(rect) = &g.children[1] else {
        panic!()
    };
    assert!(matches!(&rect.kind, ElementKind::Other { name } if name == "rect"));
}

#[test]
fn text_keeps_its_whitespace_and_the_rest_loses_it() {
    let text = format!(
        "<?xml version=\"1.0\"?>\n<!-- before -->\n<svg {NS}>\n  <text x=\"1\"> a <tspan> b </tspan>\n c &amp; &lt;d&gt; </text>\n  <style><![CDATA[rect {{ fill: red }}]]></style>\n</svg>\n<!-- after -->\n"
    );
    let canonical = check(&text);
    assert_eq!(
        canonical,
        format!(
            "<!-- before -->\n<svg {NS}><text x=\"1\"> a <tspan> b </tspan>\n c &amp; &lt;d&gt; </text><style>rect {{ fill: red }}</style></svg>\n<!-- after -->"
        )
    );
}

#[test]
fn entities_from_the_doctype_are_expanded() {
    let text = r#"<!DOCTYPE svg [<!ENTITY ns_svg "http://www.w3.org/2000/svg">]><svg xmlns="&ns_svg;"><rect width="1"/></svg>"#;
    assert_eq!(check(text), svg(r#"<rect width="1"/>"#));
}

#[test]
fn attribute_whitespace_is_escaped_rather_than_normalized() {
    let text = svg("<g data-x=\"a&#10;b&#9;c\"/>");
    assert_eq!(check(&text), text);
}

#[test]
fn statistics_count_precision_and_coverage() {
    let text = svg(r#"<rect x="1.5" y="1.23456789" display="none"/>"#);
    let (_, stats) = parse_with_stats(&text).unwrap();
    assert_eq!(stats.numbers, 2);
    #[cfg(not(feature = "f64"))]
    assert_eq!(
        stats.numbers_changed, 1,
        "1.23456789 has more digits than f32 keeps"
    );
    #[cfg(feature = "f64")]
    assert_eq!(stats.numbers_changed, 0);
    assert_eq!(stats.attrs, 3);
    assert_eq!(stats.other_attrs, 1);
    assert_eq!(
        stats.other_attr_bytes,
        ("display".len() + "none".len()) as u64
    );
}

#[cfg(not(feature = "f64"))]
#[test]
fn inline_sizes_are_what_the_plan_estimated() {
    assert_eq!(<PathSegment as Persistable>::INLINE_SIZE, 26);
    assert_eq!(<Length as Persistable>::INLINE_SIZE, 5);
    assert_eq!(
        <kladde_svg::model::Element as Persistable>::INLINE_SIZE,
        1 + 20 + 4 * 4
    );
    assert_eq!(<Node as Persistable>::INLINE_SIZE, 1 + 37);
}

#[cfg(feature = "f64")]
#[test]
fn inline_sizes_with_f64() {
    assert_eq!(<PathSegment as Persistable>::INLINE_SIZE, 50);
    assert_eq!(<Length as Persistable>::INLINE_SIZE, 9);
}
