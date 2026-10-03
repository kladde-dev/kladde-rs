//! Checks SVG files' round trip through kladde, and compares sizes.
//!
//! ```text
//! svg-roundtrip [--csv <table.csv>] [--render] <file or directory>...
//! ```
//!
//! For every `.svg` file under the given paths:
//!
//! 1. its canonical form, `write(parse(text))`, must be idempotent;
//! 2. storing it in a kladde file, closing, reopening and writing it out again
//!    must reproduce the canonical form byte for byte;
//! 3. the sizes of the original, the canonical form, both gzipped, and the
//!    kladde file are recorded, with the store's live allocation bytes,
//!    allocations and statements, and the closed file's pages by kind. The
//!    kladde file's size is taken twice: as first closed, after one explicit
//!    flush, and *settled*, once two more sessions have opened and closed it.
//!    A new file holds free pages ahead of its data, which `close` cannot
//!    truncate, and consolidation returns them only over the following
//!    flushes;
//! 4. so are how many numbers changed their shortest decimal by being stored
//!    as a `Number`, and how many attribute bytes the model kept verbatim;
//! 5. with `--render` (and the crate's `render` feature), the original and the
//!    canonical form are rendered with resvg and their pixels compared: a
//!    difference is a gap in the model, not a kladde bug.
//!
//! One row per file goes to the CSV table (default `svg-roundtrip.csv`), and
//! a summary to stderr. Exits non-zero if any canonical form was not
//! idempotent or any kladde round trip differed.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use kladde::Kladde;
use kladde_svg::{canon, parse, parse_with_stats, write, Document, ParseStats};

const COLUMNS: &str = "file,status,orig_bytes,orig_gz,canon_bytes,canon_gz,kladde_bytes,\
settled_bytes,alloc_bytes,allocations,statements,data_pages,table_pages,free_pages,numbers,numbers_changed,\
attrs,other_attrs,attr_bytes,other_attr_bytes,idempotent,kladde_ok,render_max_diff,\
render_diff_share,parse_us,open_us";

/// What one file's checks found.
#[derive(Default)]
struct Row {
    file: String,
    status: &'static str,
    orig_bytes: u64,
    orig_gz: u64,
    canon_bytes: u64,
    canon_gz: u64,
    kladde_bytes: u64,
    /// The file's size after two more sessions opened and closed it.
    settled_bytes: u64,
    alloc_bytes: u64,
    allocations: u64,
    statements: u64,
    /// The first-closed file's pages, by kind, as the reopened store counts
    /// them.
    data_pages: u64,
    table_pages: u64,
    free_pages: u64,
    stats: ParseStats,
    idempotent: bool,
    kladde_ok: bool,
    /// The largest difference of any channel of any pixel, and the share of
    /// pixels that differ by more than 1 in some channel; `None` if not
    /// rendered.
    render: Option<(u8, f64)>,
    parse_us: u128,
    open_us: u128,
}

impl Row {
    fn csv(&self) -> String {
        let file = if self.file.contains([',', '"']) {
            format!("\"{}\"", self.file.replace('"', "\"\""))
        } else {
            self.file.clone()
        };
        let (max_diff, share) = match self.render {
            Some((m, s)) => (m.to_string(), format!("{s:.6}")),
            None => (String::new(), String::new()),
        };
        format!(
            "{file},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{max_diff},{share},{},{}",
            self.status,
            self.orig_bytes,
            self.orig_gz,
            self.canon_bytes,
            self.canon_gz,
            self.kladde_bytes,
            self.settled_bytes,
            self.alloc_bytes,
            self.allocations,
            self.statements,
            self.data_pages,
            self.table_pages,
            self.free_pages,
            self.stats.numbers,
            self.stats.numbers_changed,
            self.stats.attrs,
            self.stats.other_attrs,
            self.stats.attr_bytes,
            self.stats.other_attr_bytes,
            self.idempotent as u8,
            self.kladde_ok as u8,
            self.parse_us,
            self.open_us,
        )
    }
}

fn gzip_len(bytes: &[u8]) -> u64 {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap().len() as u64
}

fn collect(path: &Path, files: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let mut entries: Vec<PathBuf> = match std::fs::read_dir(path) {
            Ok(dir) => dir.filter_map(|e| e.ok().map(|e| e.path())).collect(),
            Err(e) => {
                eprintln!("{}: {e}", path.display());
                return;
            }
        };
        entries.sort();
        for entry in entries {
            collect(&entry, files);
        }
    } else if path.extension().is_some_and(|ext| ext == "svg") {
        files.push(path.to_path_buf());
    }
}

fn check(file: &Path, work: &Path, render: bool) -> Row {
    let mut row = Row {
        file: file.display().to_string(),
        ..Row::default()
    };
    let bytes = match std::fs::read(file) {
        Ok(bytes) => bytes,
        Err(_) => {
            row.status = "unreadable";
            return row;
        }
    };
    row.orig_bytes = bytes.len() as u64;
    row.orig_gz = gzip_len(&bytes);
    let Ok(text) = String::from_utf8(bytes) else {
        row.status = "not-utf8";
        return row;
    };
    let t = Instant::now();
    let (doc, stats) = match parse_with_stats(&text) {
        Ok(parsed) => parsed,
        Err(_) => {
            row.status = "not-xml";
            return row;
        }
    };
    row.parse_us = t.elapsed().as_micros();
    row.stats = stats;
    let canonical = write(&doc);
    row.canon_bytes = canonical.len() as u64;
    row.canon_gz = gzip_len(canonical.as_bytes());
    row.idempotent = canon(&canonical).is_ok_and(|again| again == canonical);

    let path = work.join("roundtrip.kladde");
    let file_len = |path: &Path| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let stored = (|| -> kladde::Result<(kladde::Stats, kladde::Stats, String, u128, u64, u64)> {
        let mut drawing = Kladde::<Document>::create(&path, parse(&text).expect("parsed above"))?;
        drawing.flush()?;
        let stats = drawing.stats();
        drawing.close()?;
        let first = file_len(&path);
        let t = Instant::now();
        let reopened = Kladde::<Document>::open(&path)?;
        let open_us = t.elapsed().as_micros();
        let (pages, written) = (reopened.stats(), write(reopened.get()));
        reopened.close()?;
        Kladde::<Document>::open(&path)?.close()?;
        Ok((stats, pages, written, open_us, first, file_len(&path)))
    })();
    match stored {
        Ok((stats, pages, written, open_us, first, settled)) => {
            row.kladde_bytes = first;
            row.settled_bytes = settled;
            row.alloc_bytes = stats.allocation_bytes;
            row.allocations = stats.allocations;
            row.statements = stats.statements;
            row.data_pages = pages.data_pages;
            row.table_pages = pages.table_pages;
            row.free_pages = pages.free_pages;
            row.open_us = open_us;
            row.kladde_ok = written == canonical;
        }
        Err(e) => {
            eprintln!("{}: kladde: {e}", row.file);
        }
    }
    std::fs::remove_file(&path).ok();

    if render {
        row.render = render_diff(&text, &canonical);
    }
    row.status = match (row.idempotent, row.kladde_ok) {
        (true, true) => "ok",
        (false, _) => "not-idempotent",
        (true, false) => "kladde-mismatch",
    };
    row
}

#[cfg(feature = "render")]
fn render_diff(original: &str, canonical: &str) -> Option<(u8, f64)> {
    use resvg::{tiny_skia, usvg};
    let options = usvg::Options::default();
    let a = usvg::Tree::from_str(original, &options).ok()?;
    let b = usvg::Tree::from_str(canonical, &options).ok()?;
    let size = a.size();
    let scale = 256.0 / size.width().max(size.height());
    let (w, h) = (
        ((size.width() * scale).ceil() as u32).max(1),
        ((size.height() * scale).ceil() as u32).max(1),
    );
    let transform = tiny_skia::Transform::from_scale(scale, scale);
    let mut pa = tiny_skia::Pixmap::new(w, h)?;
    let mut pb = tiny_skia::Pixmap::new(w, h)?;
    resvg::render(&a, transform, &mut pa.as_mut());
    resvg::render(&b, transform, &mut pb.as_mut());
    let (mut max, mut differing) = (0u8, 0u64);
    for (x, y) in pa.data().chunks(4).zip(pb.data().chunks(4)) {
        let d = x
            .iter()
            .zip(y)
            .map(|(p, q)| p.abs_diff(*q))
            .max()
            .unwrap_or(0);
        max = max.max(d);
        differing += (d > 1) as u64;
    }
    Some((max, differing as f64 / (w as u64 * h as u64) as f64))
}

#[cfg(not(feature = "render"))]
fn render_diff(_: &str, _: &str) -> Option<(u8, f64)> {
    None
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

fn summarize(rows: &[Row], render: bool) {
    let count = |status: &str| rows.iter().filter(|r| r.status == status).count();
    eprintln!(
        "{} files: {} ok, {} not idempotent, {} kladde mismatches, {} not XML, {} not UTF-8, {} unreadable",
        rows.len(),
        count("ok"),
        count("not-idempotent"),
        count("kladde-mismatch"),
        count("not-xml"),
        count("not-utf8"),
        count("unreadable"),
    );
    for r in rows
        .iter()
        .filter(|r| matches!(r.status, "not-idempotent" | "kladde-mismatch"))
        .take(10)
    {
        eprintln!("  {}: {}", r.status, r.file);
    }
    let parsed: Vec<&Row> = rows
        .iter()
        .filter(|r| r.stats.attrs > 0 || r.canon_bytes > 0)
        .collect();
    let sum = |f: fn(&Row) -> u64| parsed.iter().map(|r| f(r)).sum::<u64>();
    let (orig, orig_gz, canon_b, canon_gz, kladde_b, settled, alloc) = (
        sum(|r| r.orig_bytes),
        sum(|r| r.orig_gz),
        sum(|r| r.canon_bytes),
        sum(|r| r.canon_gz),
        sum(|r| r.kladde_bytes),
        sum(|r| r.settled_bytes),
        sum(|r| r.alloc_bytes),
    );
    let ratio = |a: u64, b: u64| a as f64 / b.max(1) as f64;
    eprintln!("total bytes, and relative to the originals:");
    eprintln!("  original            {orig:>12}");
    eprintln!(
        "  original, gzipped   {orig_gz:>12}  {:>6.3}",
        ratio(orig_gz, orig)
    );
    eprintln!(
        "  canonical           {canon_b:>12}  {:>6.3}",
        ratio(canon_b, orig)
    );
    eprintln!(
        "  canonical, gzipped  {canon_gz:>12}  {:>6.3}",
        ratio(canon_gz, orig)
    );
    eprintln!(
        "  kladde file         {kladde_b:>12}  {:>6.3}",
        ratio(kladde_b, orig)
    );
    eprintln!(
        "  kladde, settled     {settled:>12}  {:>6.3}",
        ratio(settled, orig)
    );
    eprintln!(
        "  kladde allocations  {alloc:>12}  {:>6.3}",
        ratio(alloc, orig)
    );
    eprintln!(
        "  per file, median: settled kladde file / original {:.2}, kladde allocations / canonical {:.2}",
        median(parsed.iter().map(|r| ratio(r.settled_bytes, r.orig_bytes)).collect()),
        median(parsed.iter().map(|r| ratio(r.alloc_bytes, r.canon_bytes)).collect()),
    );
    let numbers = sum(|r| r.stats.numbers);
    let changed = sum(|r| r.stats.numbers_changed);
    let attr_bytes = sum(|r| r.stats.attr_bytes);
    let other_bytes = sum(|r| r.stats.other_attr_bytes);
    eprintln!(
        "numbers: {numbers}, of which {changed} ({:.2} %) changed their shortest decimal",
        100.0 * ratio(changed, numbers)
    );
    eprintln!(
        "attribute bytes kept verbatim: {other_bytes} of {attr_bytes} ({:.1} %)",
        100.0 * ratio(other_bytes, attr_bytes)
    );
    if render {
        let rendered: Vec<&Row> = parsed
            .iter()
            .copied()
            .filter(|r| r.render.is_some())
            .collect();
        let differ = |share: f64| {
            rendered
                .iter()
                .filter(|r| r.render.is_some_and(|(_, s)| s > share))
                .count()
        };
        eprintln!(
            "rendered: {} of {}; pixels differing in more than 0 % / 0.1 % / 1 % of the image: {} / {} / {}",
            rendered.len(),
            parsed.len(),
            differ(0.0),
            differ(0.001),
            differ(0.01),
        );
        let mut worst: Vec<&Row> = rendered.clone();
        worst.sort_by(|a, b| b.render.unwrap().1.total_cmp(&a.render.unwrap().1));
        for r in worst.iter().take(10).filter(|r| r.render.unwrap().1 > 0.0) {
            let (max, share) = r.render.unwrap();
            eprintln!(
                "  {:.2} % of pixels, by up to {max}: {}",
                100.0 * share,
                r.file
            );
        }
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut csv = PathBuf::from("svg-roundtrip.csv");
    let mut render = false;
    let mut paths = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--csv" => csv = PathBuf::from(args.next().expect("--csv needs a path")),
            "--render" => render = true,
            _ => paths.push(PathBuf::from(arg)),
        }
    }
    if paths.is_empty() {
        eprintln!("usage: svg-roundtrip [--csv <table.csv>] [--render] <file or directory>...");
        return ExitCode::FAILURE;
    }
    if render && !cfg!(feature = "render") {
        eprintln!("--render needs the `render` feature: cargo run --features render ...");
        return ExitCode::FAILURE;
    }
    let mut files = Vec::new();
    for path in &paths {
        collect(path, &mut files);
    }
    let work = std::env::temp_dir().join(format!("svg-roundtrip-{}", std::process::id()));
    std::fs::create_dir_all(&work).expect("create a work directory");

    let t = Instant::now();
    let rows: Vec<Row> = files.iter().map(|f| check(f, &work, render)).collect();
    std::fs::remove_dir_all(&work).ok();

    let mut out = String::from(COLUMNS);
    out.push('\n');
    for row in &rows {
        out.push_str(&row.csv());
        out.push('\n');
    }
    std::fs::write(&csv, out).expect("write the table");
    eprintln!(
        "{}: {} rows in {:.1} s",
        csv.display(),
        rows.len(),
        t.elapsed().as_secs_f64()
    );
    summarize(&rows, render);

    let failed = rows
        .iter()
        .any(|r| matches!(r.status, "not-idempotent" | "kladde-mismatch"));
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
