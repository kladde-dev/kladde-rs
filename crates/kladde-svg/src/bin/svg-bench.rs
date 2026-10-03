//! Benchmarks a drawing editor's edits on an SVG drawing stored in kladde.
//!
//! ```text
//! svg-bench <drawing.svg> <output directory> [--quick] [--edits N] [--only KIND] [--seed N] [--variant NAME]
//! ```
//!
//! `--only Restack`, say, makes every edit a restack where the drawing allows
//! one, to measure what one kind of edit costs.
//!
//! The drawing is converted into a kladde file, which is not measured, and
//! then edited by a seeded mix of what a drawing editor does — dragging shapes,
//! moving, inserting and deleting path nodes, recoloring, duplicating,
//! deleting and restacking shapes, editing text — until the edits have stored
//! eight times the drawing's live size (once with `--quick`), or for exactly
//! `N` edits with `--edits`, which makes runs of the model's layouts (see
//! `kladde_svg::model`) make the same edits. Every 1000 edits
//! the file is flushed and one row is recorded in kladde-bench's format, to
//! `svg-<variant>.csv` in the output directory, so that `plot-evaluation.py`
//! in kladde-docs reads it like kladde-bench's tables. The variant is the
//! drawing's file name unless `--variant` says otherwise.
//!
//! `app_bytes` counts what each edit stores: the bytes of the numbers it sets,
//! or the encoding of a value it stores, in the place it stores it, plus
//! everything that value owns; deletions count nothing. `ops_us` counts the time spent in kladde's calls
//! only, not choosing the edits.
//!
//! At the end, the file is closed and reopened, and the drawing must write the
//! same canonical SVG as before. A summary on stderr compares the time to open
//! the SVG and the kladde file, and what kladde wrote per edit with what
//! saving the whole SVG would write.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use kladde::{Encoding, Kladde, Persistable, Pointer, WriteBackend};
use kladde_bench::{row, save, Rng};
use kladde_svg::model::{
    Attr, AttrParts, Color, Element, ElementGuard, ElementKind, ElementKindParts, ListEncoding,
    Node, NodeParts, Paint, PaintParts, PathSegment, PathSegmentParts, TransformOp,
    TransformOpParts,
};
use kladde_svg::size::Payload;
use kladde_svg::{parse, write, DeepCopy, Document, Number};

/// Edits between two explicit flushes, as in kladde-bench.
const OPS_PER_FLUSH: u64 = 1000;
const NUMBER: u64 = std::mem::size_of::<Number>() as u64;

/// The kinds of edit, with their share of the mix in percent.
const MIX: [(Edit, u64); 9] = [
    (Edit::Drag, 30),
    (Edit::MoveNode, 20),
    (Edit::InsertNode, 8),
    (Edit::DeleteNode, 7),
    (Edit::Recolor, 10),
    (Edit::Duplicate, 8),
    (Edit::Delete, 7),
    (Edit::Restack, 5),
    (Edit::Text, 5),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edit {
    Drag,
    MoveNode,
    InsertNode,
    DeleteNode,
    Recolor,
    Duplicate,
    Delete,
    Restack,
    Text,
}

/// Where the editable content of a drawing sits, by child indices from the
/// root element. Rebuilt after every edit that adds, removes or moves
/// elements.
#[derive(Default)]
struct Index {
    /// Shapes, groups, text and images outside definitions.
    movable: Vec<Vec<usize>>,
    /// Those of `movable` that contain no others: what duplicating and
    /// deleting pick from, so that neither doubles nor empties a drawing
    /// whose shapes all sit in one group.
    leaves: Vec<Vec<usize>>,
    /// Those of `movable` that are paths of at least two segments.
    paths: Vec<Vec<usize>>,
    /// Text nodes of text elements: the element, and the node's index.
    texts: Vec<(Vec<usize>, usize)>,
}

impl Index {
    fn of(doc: &Document) -> Index {
        let mut index = Index::default();
        index.walk(&doc.root, &mut Vec::new());
        index
    }

    fn walk(&mut self, e: &Element, path: &mut Vec<usize>) {
        use ElementKind as K;
        for (i, child) in e.children.iter().enumerate() {
            match child {
                Node::Element(c) => {
                    let hidden = matches!(
                        c.kind,
                        K::Defs
                            | K::ClipPath
                            | K::Mask
                            | K::Pattern
                            | K::Marker
                            | K::Symbol
                            | K::LinearGradient
                            | K::RadialGradient
                            | K::Filter
                            | K::Metadata
                            | K::Style
                            | K::Title
                            | K::Desc
                            | K::Script
                            | K::ForeignObject
                            | K::Other { .. }
                    );
                    if hidden {
                        continue;
                    }
                    path.push(i);
                    self.movable.push(path.clone());
                    if let K::Path { d } = &c.kind {
                        if d.len() >= 2 {
                            self.paths.push(path.clone());
                        }
                    }
                    let before = self.movable.len();
                    self.walk(c, path);
                    if self.movable.len() == before {
                        self.leaves.push(path.clone());
                    }
                    path.pop();
                }
                Node::Text(_) if matches!(e.kind, K::Text | K::Tspan) => {
                    self.texts.push((path.clone(), i));
                }
                _ => {}
            }
        }
    }
}

/// The element at `path` below `root`.
fn element<'a>(root: &'a Element, path: &[usize]) -> &'a Element {
    path.iter().fold(root, |e, &i| match &e.children[i] {
        Node::Element(child) => child,
        _ => unreachable!("the index names elements only"),
    })
}

/// Runs `f` on the guard of the element at `rest` below child `i` of the
/// element `g` guards. Every element below the root stands in a list of
/// children, so its guard takes the lists' encoding, whatever `g`'s is.
fn below<B, E, R>(
    g: &mut ElementGuard<'_, B, E>,
    i: usize,
    rest: &[usize],
    f: impl FnOnce(&mut ElementGuard<'_, B, ListEncoding>) -> R,
) -> R
where
    B: WriteBackend<Pointer = Pointer>,
    E: Encoding,
{
    let mut children = g.children_mut();
    let mut node = children
        .get_mut(i)
        .expect("an index from the element index");
    match node.parts() {
        NodeParts::Element(mut child) => match rest.split_first() {
            None => f(&mut child),
            Some((&j, rest)) => below(&mut child, j, rest, f),
        },
        _ => unreachable!("the index names elements only"),
    }
}

/// Evaluates `$body` with `$e` bound to the guard of the element at `$path`
/// below the root element guarded by `$root`. A macro rather than a
/// function: the root element's guard and those below it take different
/// encodings in some layouts, and the body is type-checked against each.
macro_rules! on_element {
    ($root:expr, $path:expr, |$e:ident| $body:expr) => {{
        let root = $root;
        let path: &[usize] = $path;
        match path.split_first() {
            None => (|$e: &mut ElementGuard<'_, _, _>| $body)(root),
            Some((&i, rest)) => below(root, i, rest, |$e| $body),
        }
    }};
}

/// Times one mutation of the element at `$path`, made by `$body`.
macro_rules! mutate {
    ($bench:expr, $path:expr, |$e:ident| $body:expr) => {{
        let t = Instant::now();
        {
            let mut g = $bench.drawing.guard();
            let mut root = g.root_mut();
            on_element!(&mut root, $path, |$e| $body).expect("an edit");
        }
        $bench.kladde_time += t.elapsed();
    }};
}

/// A path segment's end point, as the coordinates it has.
fn end_point(segment: &PathSegment) -> (Option<Number>, Option<Number>) {
    use PathSegment as S;
    match *segment {
        S::MoveTo { x, y, .. }
        | S::LineTo { x, y, .. }
        | S::CurveTo { x, y, .. }
        | S::SmoothCurveTo { x, y, .. }
        | S::Quadratic { x, y, .. }
        | S::SmoothQuadratic { x, y, .. }
        | S::EllipticalArc { x, y, .. } => (Some(x), Some(y)),
        S::HorizontalLineTo { x, .. } => (Some(x), None),
        S::VerticalLineTo { y, .. } => (None, Some(y)),
        S::ClosePath { .. } => (None, None),
    }
}

/// Where an element's fill is set: its index in `attrs` or `style`, and
/// whether it is a plain color.
enum Fill {
    Attr(usize, bool),
    Style(usize, bool),
    Unset,
}

struct Bench {
    drawing: Kladde<Document>,
    rng: Rng,
    index: Index,
    stale: bool,
    /// How many leaves the drawing started with, which duplicating and
    /// deleting keep it near.
    initial: usize,
    ops: u64,
    app_bytes: u64,
    /// Time spent in kladde's calls since the last row.
    kladde_time: Duration,
    counts: Vec<(Edit, u64)>,
    /// The one kind of edit to make, if `--only` names one.
    only: Option<Edit>,
}

impl Bench {
    fn delta(&mut self) -> Number {
        (self.rng.range(0, 40) as Number - 20.0) / 4.0
    }

    fn pick<T: Clone>(&mut self, list: &[T]) -> T {
        list[self.rng.below(list.len() as u64) as usize].clone()
    }

    /// One edit, chosen from the mix.
    fn edit(&mut self) {
        if self.stale {
            self.index = Index::of(self.drawing.get());
            self.stale = false;
        }
        let mut r = self.rng.below(100);
        let mut edit = MIX[0].0;
        for (e, share) in MIX {
            if r < share {
                edit = e;
                break;
            }
            r -= share;
        }
        if let Some(only) = self.only {
            edit = only;
        }
        // Keep the drawing near its starting size, and fall back where the
        // drawing has nothing to edit.
        let n = self.index.leaves.len();
        edit = match edit {
            Edit::Duplicate if n > self.initial + self.initial / 5 => Edit::Delete,
            Edit::Delete if n < self.initial - self.initial / 5 || n < 2 => Edit::Duplicate,
            Edit::MoveNode | Edit::InsertNode | Edit::DeleteNode if self.index.paths.is_empty() => {
                Edit::Drag
            }
            Edit::Text if self.index.texts.is_empty() => Edit::Recolor,
            e => e,
        };
        match edit {
            Edit::Drag => self.drag(),
            Edit::MoveNode => self.move_node(),
            Edit::InsertNode => self.insert_node(),
            Edit::DeleteNode => self.delete_node(),
            Edit::Recolor => self.recolor(),
            Edit::Duplicate => self.duplicate(),
            Edit::Delete => self.delete(),
            Edit::Restack => self.restack(),
            Edit::Text => self.edit_text(),
        }
        self.counts.iter_mut().find(|(e, _)| *e == edit).unwrap().1 += 1;
        self.ops += 1;
    }

    /// Moves a shape by a few units, mostly one of the first tenth of the
    /// shapes: a drag sets the translation that leads its transform list.
    fn drag(&mut self) {
        let n = self.index.movable.len() as u64;
        let i = if self.rng.chance(0.9) {
            self.rng.below((n / 10).max(1))
        } else {
            self.rng.below(n)
        };
        let path = self.index.movable[i as usize].clone();
        let (dx, dy) = (self.delta(), self.delta());
        let first = element(&self.drawing.get().root, &path)
            .transform
            .first()
            .copied();
        let new = TransformOp::Translate { tx: dx, ty: dy };
        match first {
            Some(TransformOp::Translate { tx, ty }) => {
                let (tx, ty) = (tx + dx, ty + dy);
                mutate!(self, &path, |e| {
                    let mut transform = e.transform_mut();
                    let mut op = transform.get_mut(0).unwrap();
                    let TransformOpParts::Translate {
                        tx: mut gx,
                        ty: mut gy,
                    } = op.parts()
                    else {
                        unreachable!()
                    };
                    gx.set(tx)?;
                    gy.set(ty)
                });
                self.app_bytes += 2 * NUMBER;
            }
            Some(_) => {
                mutate!(self, &path, |e| e.transform_mut().insert(0, new));
                self.app_bytes += new.payload_bytes::<ListEncoding>();
            }
            None => {
                mutate!(self, &path, |e| e.transform_mut().push(new));
                self.app_bytes += new.payload_bytes::<ListEncoding>();
            }
        }
    }

    fn path_data<'a>(drawing: &'a Kladde<Document>, path: &[usize]) -> &'a [PathSegment] {
        match &element(&drawing.get().root, path).kind {
            ElementKind::Path { d } => d,
            _ => unreachable!("the index lists paths"),
        }
    }

    /// Moves a path node's end point.
    fn move_node(&mut self) {
        let path = self.pick(&self.index.paths.clone());
        let len = Self::path_data(&self.drawing, &path).len() as u64;
        let i = self.rng.below(len) as usize;
        let (x, y) = end_point(&Self::path_data(&self.drawing, &path)[i]);
        let (dx, dy) = (self.delta(), self.delta());
        let (x, y) = (x.map(|x| x + dx), y.map(|y| y + dy));
        mutate!(self, &path, |e| {
            let mut kind = e.kind_mut();
            let ElementKindParts::Path { mut d } = kind.parts() else {
                unreachable!()
            };
            let mut segment = d.get_mut(i).unwrap();
            use PathSegmentParts as S;
            let (gx, gy) = match segment.parts() {
                S::MoveTo { x, y, .. }
                | S::LineTo { x, y, .. }
                | S::CurveTo { x, y, .. }
                | S::SmoothCurveTo { x, y, .. }
                | S::Quadratic { x, y, .. }
                | S::SmoothQuadratic { x, y, .. }
                | S::EllipticalArc { x, y, .. } => (Some(x), Some(y)),
                S::HorizontalLineTo { x, .. } => (Some(x), None),
                S::VerticalLineTo { y, .. } => (None, Some(y)),
                S::ClosePath { .. } => (None, None),
            };
            if let (Some(mut g), Some(x)) = (gx, x) {
                g.set(x)?;
            }
            if let (Some(mut g), Some(y)) = (gy, y) {
                g.set(y)?;
            }
            Ok::<(), kladde::Error>(())
        });
        self.app_bytes += (x.is_some() as u64 + y.is_some() as u64) * NUMBER;
    }

    /// Inserts a relative line after some node of a path.
    fn insert_node(&mut self) {
        let path = self.pick(&self.index.paths.clone());
        let len = Self::path_data(&self.drawing, &path).len() as u64;
        let i = 1 + self.rng.below(len) as usize;
        let segment = PathSegment::LineTo {
            abs: false,
            x: self.delta(),
            y: self.delta(),
        };
        mutate!(self, &path, |e| {
            let mut kind = e.kind_mut();
            let ElementKindParts::Path { mut d } = kind.parts() else {
                unreachable!()
            };
            d.insert(i, segment)
        });
        self.app_bytes += segment.payload_bytes::<ListEncoding>();
    }

    /// Deletes a node of a path, never its first, and never below two.
    fn delete_node(&mut self) {
        let path = self.pick(&self.index.paths.clone());
        let len = Self::path_data(&self.drawing, &path).len() as u64;
        if len <= 2 {
            return self.insert_node();
        }
        let i = 1 + self.rng.below(len - 1) as usize;
        mutate!(self, &path, |e| {
            let mut kind = e.kind_mut();
            let ElementKindParts::Path { mut d } = kind.parts() else {
                unreachable!()
            };
            d.delete(i)
        });
    }

    /// Sets a shape's fill to a new color, where it is set or, if nowhere, as
    /// a new attribute.
    fn recolor(&mut self) {
        let path = self.pick(&self.index.movable.clone());
        let next = self.rng.next();
        let color = Color {
            r: next as u8,
            g: (next >> 8) as u8,
            b: (next >> 16) as u8,
            a: 255,
        };
        let e = element(&self.drawing.get().root, &path);
        let is_fill = |a: &Attr| match a {
            Attr::Fill(p) => Some(matches!(p, Paint::Color(_))),
            _ => None,
        };
        let fill = if let Some((k, c)) = e
            .attrs
            .iter()
            .enumerate()
            .find_map(|(k, a)| is_fill(a).map(|c| (k, c)))
        {
            Fill::Attr(k, c)
        } else if let Some((k, c)) = e
            .style
            .iter()
            .enumerate()
            .find_map(|(k, a)| is_fill(a).map(|c| (k, c)))
        {
            Fill::Style(k, c)
        } else {
            Fill::Unset
        };
        match fill {
            Fill::Attr(k, is_color) | Fill::Style(k, is_color) => {
                let in_style = matches!(fill, Fill::Style(..));
                mutate!(self, &path, |e| {
                    let mut list = if in_style {
                        e.style_mut()
                    } else {
                        e.attrs_mut()
                    };
                    let mut attr = list.get_mut(k).unwrap();
                    let AttrParts::Fill(mut paint) = attr.parts() else {
                        unreachable!()
                    };
                    if is_color {
                        let PaintParts::Color(mut c) = paint.parts() else {
                            unreachable!()
                        };
                        c.set(color)
                    } else {
                        paint.set(Paint::Color(color))
                    }
                });
                self.app_bytes += if is_color {
                    color.payload_bytes::<ListEncoding>()
                } else {
                    Paint::Color(color).payload_bytes::<ListEncoding>()
                };
            }
            Fill::Unset => {
                let attr = Attr::Fill(Paint::Color(color));
                self.app_bytes += attr.payload_bytes::<ListEncoding>();
                mutate!(self, &path, |e| e.attrs_mut().push(attr));
            }
        }
    }

    /// Duplicates a shape, placing the copy right above the original.
    fn duplicate(&mut self) {
        let path = self.pick(&self.index.leaves.clone());
        let (&i, parent) = path.split_last().unwrap();
        let copy = Node::Element(element(&self.drawing.get().root, &path).deep_copy());
        self.app_bytes += copy.payload_bytes::<ListEncoding>();
        mutate!(self, parent, |p| p.children_mut().insert(i + 1, copy));
        self.stale = true;
    }

    /// Deletes a shape.
    fn delete(&mut self) {
        let path = self.pick(&self.index.leaves.clone());
        let (&i, parent) = path.split_last().unwrap();
        mutate!(self, parent, |p| p.children_mut().delete(i));
        self.stale = true;
    }

    /// Moves a shape to another place among its siblings, in one transaction,
    /// since removing it and inserting it again belong together.
    fn restack(&mut self) {
        let path = self.pick(&self.index.movable.clone());
        let (&i, parent) = path.split_last().unwrap();
        let siblings = element(&self.drawing.get().root, parent).children.len() as u64;
        let j = self.rng.below(siblings) as usize;
        let moved =
            element(&self.drawing.get().root, parent).children[i].encoded_size::<ListEncoding>();
        let t = Instant::now();
        let mut tx = self.drawing.transaction();
        {
            let mut g = tx.guard();
            let mut root = g.root_mut();
            on_element!(&mut root, parent, |p| {
                let mut children = p.children_mut();
                let node = children.remove(i)?;
                children.insert(j, node)
            })
            .expect("an edit");
        }
        tx.commit().expect("a commit");
        self.kladde_time += t.elapsed();
        // The node's encoding moves; what it owns stays where it is.
        self.app_bytes += moved as u64;
        self.stale = true;
    }

    /// Types into a text, or replaces it once it has grown long.
    fn edit_text(&mut self) {
        let (path, child) = self.pick(&self.index.texts.clone());
        let len = self.rng.range(1, 12);
        let typed: String = (0..len)
            .map(|_| (b'a' + self.rng.below(26) as u8) as char)
            .collect();
        let long = match &element(&self.drawing.get().root, &path).children[child] {
            Node::Text(s) => s.len() > 64,
            _ => unreachable!("the index lists text nodes"),
        };
        self.app_bytes += typed.len() as u64;
        mutate!(self, &path, |e| {
            let mut children = e.children_mut();
            let mut node = children.get_mut(child).unwrap();
            let NodeParts::Text(mut s) = node.parts() else {
                unreachable!()
            };
            if long {
                s.set(typed.as_str())
            } else {
                s.push_str(&typed)
            }
        });
    }
}

fn median(mut v: Vec<u128>) -> u128 {
    v.sort_unstable();
    v.get(v.len() / 2).copied().unwrap_or(0)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut positional, mut quick, mut seed, mut variant) = (Vec::new(), false, 1u64, None);
    let (mut edits, mut only): (Option<u64>, Option<String>) = (None, None);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--quick" => quick = true,
            "--seed" => seed = args.next().and_then(|s| s.parse().ok()).expect("--seed N"),
            "--edits" => edits = Some(args.next().and_then(|s| s.parse().ok()).expect("--edits N")),
            "--only" => only = Some(args.next().expect("--only KIND")),
            "--variant" => variant = Some(args.next().expect("--variant NAME")),
            _ => positional.push(PathBuf::from(arg)),
        }
    }
    let [input, out] = positional.as_slice() else {
        eprintln!(
            "usage: svg-bench <drawing.svg> <output directory> [--quick] [--edits N] [--only KIND] [--seed N] [--variant NAME]"
        );
        std::process::exit(2);
    };
    let variant = variant.unwrap_or_else(|| {
        input
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "drawing".into())
    });
    std::fs::create_dir_all(out.join("files")).expect("create the output directory");
    let file: PathBuf = out.join("files").join(format!("svg-{variant}.kladde"));

    let text = std::fs::read_to_string(input).expect("read the drawing");
    let t = Instant::now();
    let doc = parse(&text).expect("parse the drawing");
    let parse_time = t.elapsed();
    let canonical_before = write(&doc).len();

    let mut drawing = Kladde::<Document>::create(&file, doc).expect("create the kladde file");
    drawing.flush().expect("flush");
    let base = drawing.stats();
    let live = base.allocation_bytes;
    let index = Index::of(drawing.get());
    let initial = index.leaves.len();
    assert!(initial > 0, "the drawing has nothing to edit");
    eprintln!(
        "{variant}: {} bytes of SVG, {live} live bytes in kladde, {} elements to edit ({initial} without children), {} paths, {} texts",
        text.len(),
        index.movable.len(),
        index.paths.len(),
        index.texts.len()
    );

    let mut bench = Bench {
        drawing,
        rng: Rng::new(seed),
        index,
        stale: false,
        initial,
        ops: 0,
        app_bytes: 0,
        kladde_time: Duration::ZERO,
        counts: MIX.iter().map(|&(e, _)| (e, 0)).collect(),
        only: only.map(|name| {
            MIX.iter()
                .map(|&(e, _)| e)
                .find(|e| format!("{e:?}").eq_ignore_ascii_case(&name))
                .unwrap_or_else(|| panic!("--only takes one of {:?}", MIX.map(|(e, _)| e)))
        }),
    };
    let target = if quick { live } else { 8 * live };
    let started = Instant::now();
    let (mut rows, mut flushes, mut flush_times) = (Vec::new(), 0u64, Vec::new());
    while match edits {
        Some(n) => bench.ops < n,
        None => bench.app_bytes < target,
    } {
        bench.edit();
        if bench.ops.is_multiple_of(OPS_PER_FLUSH) {
            let t = Instant::now();
            bench.drawing.flush().expect("flush");
            let flush_us = t.elapsed().as_micros();
            flushes += 1;
            flush_times.push(flush_us);
            let s = bench.drawing.stats();
            rows.push(row(
                "svg",
                &variant,
                live,
                flushes,
                bench.ops,
                bench.app_bytes,
                &s,
                &base,
                flush_us,
                bench.kladde_time.as_micros(),
            ));
            bench.kladde_time = Duration::ZERO;
        }
    }

    // What the run cost, before closing changes the counters.
    let s = bench.drawing.stats();
    let written = s.journal_bytes - base.journal_bytes
        + 4096
            * (s.data_pages_written - base.data_pages_written + s.table_pages_written
                - base.table_pages_written
                + s.headers_written
                - base.headers_written);
    let t = Instant::now();
    let canonical = write(bench.drawing.get());
    let write_time = t.elapsed();
    let file_bytes = s.file_pages * 4096;
    bench.drawing.close().expect("close");
    let t = Instant::now();
    let reopened = Kladde::<Document>::open(&file).expect("reopen");
    let open_time = t.elapsed();
    assert!(
        write(reopened.get()) == canonical,
        "{variant}: the drawing differs after reopening"
    );
    drop(reopened);
    std::fs::remove_file(&file).ok();
    std::fs::remove_dir(out.join("files")).ok();
    save(out, &format!("svg-{variant}"), &rows, started);

    let ops = bench.ops;
    let per_op = |bytes: f64| bytes / ops.max(1) as f64;
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    eprintln!(
        "{ops} edits storing {} bytes ({:.1} per edit), {flushes} flushes",
        bench.app_bytes,
        per_op(bench.app_bytes as f64)
    );
    let mix: Vec<String> = bench
        .counts
        .iter()
        .map(|(e, n)| format!("{e:?} {:.1} %", 100.0 * *n as f64 / ops.max(1) as f64))
        .collect();
    eprintln!("  mix: {}", mix.join(", "));
    eprintln!(
        "kladde wrote {written} bytes: {:.0} per edit, {:.2} per byte stored; the file is {file_bytes} bytes, {:.2} times its live size",
        per_op(written as f64),
        written as f64 / bench.app_bytes.max(1) as f64,
        file_bytes as f64 / s.allocation_bytes.max(1) as f64,
    );
    eprintln!(
        "saving the canonical SVG ({canonical_before} bytes before, {} after) would write per edit: {:.0} after every edit, {:.0} every 100, {:.0} every 1000",
        canonical.len(),
        canonical.len() as f64,
        canonical.len() as f64 / 100.0,
        canonical.len() as f64 / 1000.0,
    );
    eprintln!(
        "edits: {:.0} per second in kladde's calls; flushes take {:.1} ms at the median",
        ops as f64 / rows_time(&rows).max(1e-9),
        median(flush_times) as f64 / 1e3,
    );
    eprintln!(
        "opening: parsing the SVG took {:.1} ms, opening the kladde file {:.1} ms; writing the SVG took {:.1} ms",
        ms(parse_time),
        ms(open_time),
        ms(write_time),
    );
}

/// The seconds spent in kladde's calls, summed over the rows.
fn rows_time(rows: &[String]) -> f64 {
    rows.iter()
        .filter_map(|r| r.rsplit(',').next()?.parse::<f64>().ok())
        .sum::<f64>()
        / 1e6
}
