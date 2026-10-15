//! `kladde-inspect`: where the bytes of a kladde file go.
//!
//! Reads any kladde file, whichever implementation wrote it, and reports
//!
//! - every page by what it is to the governing header — header, span table,
//!   data, journal, or unreached — and every live page's bytes split into
//!   live content, dead content, slack and framing;
//! - how large a fully consolidated copy of the file is, and what the
//!   difference consists of;
//! - the spans, the data pages' live fractions and ages, and the journal;
//! - the schema, and, decoded by it, which fields, variants and types the
//!   sequence's bytes belong to, and its largest values.
//!
//! It reads the file into memory and never writes to it.
//!
//! ```text
//! kladde-inspect [--top N] [--types] [--map] [--no-decode] FILE
//! ```

mod decode;

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::ExitCode;

use kladde_schema::{TypeDescriptor, TypeTable};
use kladde_store::inspect::{Form, Frame, Inspection, PageInfo, Role, SpanInfo};
use kladde_store::{MemoryStorage, Store, MAX_PAGE_CONTENT, PAGE_SIZE};

const USAGE: &str = "\
usage: kladde-inspect [OPTIONS] FILE

Reports where the bytes of the kladde file FILE go, what a fully consolidated
copy of it would take, and which values of its content take the space. Reads
FILE into memory and never writes to it.

options:
  --top N       list the N largest values (default 15; 0 for none)
  --types       list every type of the schema
  --map         draw a map of the file, one character per page
  --no-decode   skip decoding the sequence by the schema
  -h, --help    print this help";

const MAP_LEGEND: &str = "\
  H governing header   h other header   T span-table page   J journal page
  0-9 data page, by its live fraction in tenths (9: 90 % or more)
  f unreached, but kept for the other header   s unreached, stale but framed
  . unreached, zeros   ? unreached, unframed (an old journal page, say)";

struct Args {
    path: PathBuf,
    top: usize,
    types: bool,
    map: bool,
    decode: bool,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut path = None;
    let mut args = Args {
        path: PathBuf::new(),
        top: 15,
        types: false,
        map: false,
        decode: true,
    };
    let mut it = std::env::args_os().skip(1);
    while let Some(a) = it.next() {
        match a.to_str() {
            Some("-h" | "--help") => return Ok(None),
            Some("--top") => {
                let n = it.next().ok_or("--top needs a number")?;
                args.top = n
                    .to_str()
                    .and_then(|n| n.parse().ok())
                    .ok_or("--top needs a number")?;
            }
            Some("--types") => args.types = true,
            Some("--map") => args.map = true,
            Some("--no-decode") => args.decode = false,
            Some(o) if o.starts_with('-') && o.len() > 1 => {
                return Err(format!("unknown option {o}"))
            }
            _ if path.is_none() => path = Some(PathBuf::from(a)),
            _ => return Err("more than one FILE".into()),
        }
    }
    args.path = path.ok_or("no FILE given")?;
    Ok(Some(args))
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("kladde-inspect: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kladde-inspect: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Bytes as a reader takes them in: exact below 10 KiB, else in KiB or MiB.
fn human(n: u64) -> String {
    let n = n as f64;
    if n < 10.0 * 1024.0 {
        format!("{n} B")
    } else if n < 10.0 * 1024.0 * 1024.0 {
        format!("{:.1} KiB", n / 1024.0)
    } else {
        format!("{:.2} MiB", n / (1024.0 * 1024.0))
    }
}

fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "-".into();
    }
    format!("{:.1} %", 100.0 * part as f64 / whole as f64)
}

/// One row of a table of pages and bytes: a label, pages (if they count),
/// bytes, and the bytes' share of `whole`.
fn row(label: &str, pages: Option<u64>, bytes: u64, whole: u64) {
    let pages = pages.map_or(String::new(), |p| p.to_string());
    println!(
        "  {label:<44} {pages:>7} {:>12} {:>8}",
        human(bytes),
        percent(bytes, whole)
    );
}

fn heading(title: &str) {
    println!("\n{title}");
}

fn run(args: &Args) -> Result<(), String> {
    let image = std::fs::read(&args.path).map_err(|e| format!("{}: {e}", args.path.display()))?;
    let storage = MemoryStorage::from_image(image.clone());
    let i = Inspection::read(&storage).map_err(|e| e.to_string())?;
    let file_len = i.file_len;
    let page = PAGE_SIZE as u64;

    // The file.
    println!("{}", args.path.display());
    println!(
        "  length       {} = {} pages of {} B{}",
        human(file_len),
        i.pages.len(),
        PAGE_SIZE,
        match file_len % page {
            0 => String::new(),
            r => format!(", and {r} bytes past the last whole page"),
        }
    );
    let h = &i.header;
    print!(
        "  header       slot {}, epoch {} (governing)",
        h.slot, h.epoch
    );
    if let Some(o) = &i.other_header {
        print!("; slot {}, epoch {}", o.slot, o.epoch);
    } else {
        print!("; the other slot is not a valid header");
    }
    println!();
    println!(
        "  format       version {}, readable from version {}",
        h.format_version, h.min_reader_version
    );
    let schema_bytes = i.chain_content(&storage, 1).map_err(|e| e.to_string())?;
    let table = TypeTable::decode(&schema_bytes);
    match &table {
        Ok(t) => {
            let fp = t.fingerprint();
            println!(
                "  root type    {}, fingerprint {} ({})",
                decode::type_name(t, 0, 0),
                fp.to_hex(),
                if *fp.as_bytes() == h.root_fingerprint {
                    "the header's"
                } else {
                    "the header records another"
                }
            );
        }
        Err(e) if schema_bytes.is_empty() => {
            let _ = e;
            println!("  root type    no schema");
        }
        Err(e) => println!("  root type    the schema chain does not decode: {e}"),
    }
    for (c, name) in i.chains.iter().zip(["sequence", "schema", "private"]) {
        println!(
            "  {name:<12} {} in {} span{}",
            human(c.len),
            c.spans.len(),
            if c.spans.len() == 1 { "" } else { "s" }
        );
    }
    let j = &i.journal;
    if j.transactions == 0 {
        if h.journal_pointer as usize >= i.pages.len() {
            println!(
                "  journal      empty; it would begin at page {}, past the end of the file",
                h.journal_pointer
            );
        } else {
            println!(
                "  journal      empty; it begins at page {}",
                h.journal_pointer
            );
        }
    } else {
        println!(
            "  journal      {} transaction{} ({} seal{}) in {} on {} page{}: \
             {} writes, {} splices, {} moves, {} pastes, carrying {}",
            j.transactions,
            if j.transactions == 1 { "" } else { "s" },
            j.seals,
            if j.seals == 1 { "" } else { "s" },
            human(j.stream_bytes),
            j.pages.len(),
            if j.pages.len() == 1 { "" } else { "s" },
            j.writes,
            j.splices,
            j.moves,
            j.pastes,
            human(j.payload_bytes)
        );
    }

    let mut acc = Accounts::of(&i);
    let (chained, chains) = trailer_chains(&i, &image);
    acc.chained = chained;
    acc.unframed -= acc.chained;
    acc.report(file_len);
    if chained > 0 {
        println!(
            "  ({chained} unframed pages form {chains} chain{} of journal trailers, each naming \
             the next page: most likely\n   journals of earlier epochs; their checksums cannot be \
             checked once a journal's first page is reused)",
            if chains == 1 { "" } else { "s" }
        );
    }

    // A consolidated copy, made the way a writer closing the file into one
    // would make it.
    heading("A fully consolidated copy (kladde-rs's close_consolidated, run on a copy in memory)");
    let sequence = match consolidated(image) {
        Ok((c, sequence)) => {
            let cacc = Accounts::of(&c);
            println!(
                "  length       {} = {} pages: 2 header slots, {} data pages, {} leaves",
                human(c.file_len),
                c.pages.len(),
                cacc.data.pages,
                cacc.table.pages - 1
            );
            if c.file_len > 0 {
                println!(
                    "  this file    {:.2} × the copy's length",
                    file_len as f64 / c.file_len as f64
                );
            }
            acc.compare(&cacc, file_len.saturating_sub(c.file_len));
            Some(sequence)
        }
        Err(e) => {
            println!("  could not be made: {e}");
            None
        }
    };

    report_data_pages(&i);
    report_spans(&i);

    // The schema, and the sequence decoded by it.
    let Ok(table) = table else {
        return Ok(());
    };
    report_schema(&table, args.types);
    if args.decode {
        let replayed = j.transactions > j.seals;
        let sequence = match (replayed, sequence) {
            (true, Some(s)) => s,
            _ => i.chain_content(&storage, 0).map_err(|e| e.to_string())?,
        };
        report_content(&i, &table, &sequence, args.top, replayed);
    }
    if args.map {
        report_map(&i);
    }
    Ok(())
}

/// The unreached, unframed pages outside the fallback world whose journal
/// trailers link them into chains: each page's `next` names another such
/// page, or such a page names it. Returns how many pages and how many chains.
///
/// A trailer's `crc` continues a chain that starts at the journal's first
/// page and skips the transactions' own checksums, so once that page is
/// reused, nothing can be checked without knowing where the transactions
/// begin; a `next` that names another page of the same kind is still good
/// evidence, since stale data would name one only by chance.
fn trailer_chains(i: &Inspection, image: &[u8]) -> (u64, u64) {
    let candidate = |p: u32| {
        i.pages.get(p as usize).is_some_and(|info| {
            info.role == Role::Unreached && !info.fallback && info.frame == Frame::Unframed
        })
    };
    let next = |p: u32| {
        let at = p as usize * PAGE_SIZE + PAGE_SIZE - 8;
        u32::from_le_bytes(image[at..at + 4].try_into().unwrap())
    };
    let mut linked = HashSet::new();
    let mut named = HashSet::new();
    for p in 2..i.pages.len() as u32 {
        if candidate(p) && next(p) != p && candidate(next(p)) {
            linked.insert(p);
            linked.insert(next(p));
            named.insert(next(p));
        }
    }
    let heads = linked.iter().filter(|p| !named.contains(p)).count();
    (
        linked.len() as u64,
        heads.max(usize::from(!linked.is_empty())) as u64,
    )
}

/// Copies the file into memory, opens it there, which replays its journal,
/// reads the sequence, and closes the copy into a consolidated file; returns
/// that file's inspection and the sequence.
fn consolidated(image: Vec<u8>) -> Result<(Inspection, Vec<u8>), String> {
    let s = MemoryStorage::from_image(image);
    let store = Store::open(Box::new(s.clone()), Default::default()).map_err(|e| e.to_string())?;
    let sequence = store.read_all().map_err(|e| e.to_string())?;
    store.close_consolidated().map_err(|e| e.to_string())?;
    let i = Inspection::read(&s).map_err(|e| e.to_string())?;
    Ok((i, sequence))
}

/// Pages and bytes of one kind of page.
#[derive(Default, Clone, Copy)]
struct Tally {
    pages: u64,
    /// Content the governing header relies on: data, live entries and child
    /// references.
    live: u64,
    /// Content it does not rely on: dead data, dead entries.
    dead: u64,
    /// Never-filled room after the content.
    slack: u64,
    /// Page framing, and the header fields of a header page.
    framing: u64,
    child_bytes: u64,
    inline_bytes: u64,
}

impl Tally {
    fn add(&mut self, p: &PageInfo) {
        self.pages += 1;
        let header = matches!(p.role, Role::Header { .. });
        let framing = (PAGE_SIZE - MAX_PAGE_CONTENT) as u64 + if header { 48 } else { 0 };
        let content = match p.frame {
            Frame::Framed { content_size, .. } => content_size as u64,
            _ => 0,
        };
        self.live += p.live_bytes as u64;
        self.dead += p.dead_bytes as u64;
        self.framing += framing;
        self.slack += PAGE_SIZE as u64 - framing - content;
        if let Some(t) = p.table {
            self.child_bytes += t.child_bytes as u64;
            self.inline_bytes += t.live_inline_bytes as u64;
        }
    }

    fn bytes(&self) -> u64 {
        self.pages * PAGE_SIZE as u64
    }
}

/// The file's pages, by what they are to the governing header.
#[derive(Default)]
struct Accounts {
    /// The governing header and the leaves it reaches.
    table: Tally,
    other_header: u64,
    data: Tally,
    journal: u64,
    fallback: u64,
    stale_data: u64,
    stale_table: u64,
    zeros: u64,
    /// Unframed pages that trailers chain as a journal's, and the rest.
    chained: u64,
    unframed: u64,
    /// Live bytes the chains keep in entries rather than data pages.
    inline: u64,
    zero_spans: u64,
    content: u64,
}

impl Accounts {
    fn of(i: &Inspection) -> Accounts {
        let mut a = Accounts::default();
        for p in &i.pages {
            match p.role {
                Role::Header { governing: true } | Role::Table => a.table.add(p),
                // A header slot stays reserved for the next header even
                // when it holds none.
                Role::Header { governing: false } => a.other_header += 1,
                Role::Unreached if p.number < 2 => a.other_header += 1,
                Role::Data => a.data.add(p),
                Role::Journal => a.journal += 1,
                Role::Unreached if p.fallback => a.fallback += 1,
                Role::Unreached => match p.frame {
                    Frame::Framed { kind: 2, .. } => a.stale_data += 1,
                    Frame::Framed { .. } => a.stale_table += 1,
                    Frame::Zeros => a.zeros += 1,
                    Frame::Unframed => a.unframed += 1,
                },
            }
        }
        for s in i.chains.iter().flat_map(|c| &c.spans) {
            match s.form {
                Form::Inline => a.inline += s.len as u64,
                Form::Zero => a.zero_spans += s.len as u64,
                Form::Ref { .. } => {}
            }
        }
        a.content = i.chains.iter().map(|c| c.len).sum();
        a
    }

    fn unreached(&self) -> u64 {
        self.fallback
            + self.stale_data
            + self.stale_table
            + self.zeros
            + self.chained
            + self.unframed
    }

    fn report(&self, file_len: u64) {
        let page = PAGE_SIZE as u64;
        heading("Where the file's bytes go");
        println!(
            "  {:<44} {:>7} {:>12} {:>8}",
            "", "pages", "bytes", "of file"
        );
        let t = &self.table;
        row(
            "span table (the governing header and leaves)",
            Some(t.pages),
            t.bytes(),
            file_len,
        );
        row("  live entries", None, t.live - t.child_bytes, file_len);
        row(
            "    of which inline payloads",
            None,
            t.inline_bytes,
            file_len,
        );
        row("  dead entries", None, t.dead, file_len);
        row("  child references", None, t.child_bytes, file_len);
        row("  slack", None, t.slack, file_len);
        row("  framing and header fields", None, t.framing, file_len);
        row(
            "the other header slot",
            Some(self.other_header),
            self.other_header * page,
            file_len,
        );
        let d = &self.data;
        row("live data pages", Some(d.pages), d.bytes(), file_len);
        row("  live bytes", None, d.live, file_len);
        row("  dead bytes", None, d.dead, file_len);
        row("  slack", None, d.slack, file_len);
        row("  framing", None, d.framing, file_len);
        row(
            "journal pages",
            Some(self.journal),
            self.journal * page,
            file_len,
        );
        row(
            "unreached pages",
            Some(self.unreached()),
            self.unreached() * page,
            file_len,
        );
        for (label, n) in [
            ("  kept for the other header", self.fallback),
            ("  stale data pages", self.stale_data),
            ("  stale span-table pages", self.stale_table),
            ("  zeros", self.zeros),
            ("  chained by journal trailers", self.chained),
            ("  other unframed pages", self.unframed),
        ] {
            if n > 0 {
                row(label, Some(n), n * page, file_len);
            }
        }
    }

    /// What this file holds beyond the consolidated copy `c`, which is
    /// `overhead` bytes shorter.
    fn compare(&self, c: &Accounts, overhead: u64) {
        let page = PAGE_SIZE as u64;
        println!(
            "  overhead     {} = {} pages, the file's length less the copy's",
            human(overhead),
            overhead / page
        );
        println!("  {:<44} {:>7} {:>12} {:>8}", "", "pages", "bytes", "of it");
        row(
            "unreached pages",
            Some(self.unreached()),
            self.unreached() * page,
            overhead,
        );
        row(
            "journal pages",
            Some(self.journal),
            self.journal * page,
            overhead,
        );
        let tables = self.table.pages as i64 - c.table.pages as i64;
        println!(
            "  {:<44} {:>7} {:>12} {:>8}",
            "span-table pages beyond the copy's",
            tables,
            human((tables * page as i64).max(0) as u64),
            percent((tables * page as i64).max(0) as u64, overhead)
        );
        let data = self.data.pages as i64 - c.data.pages as i64;
        println!(
            "  {:<44} {:>7} {:>12} {:>8}",
            "data pages beyond the copy's",
            data,
            human((data * page as i64).max(0) as u64),
            percent((data * page as i64).max(0) as u64, overhead)
        );
        row(
            "  dead bytes in live data pages",
            None,
            self.data.dead,
            overhead,
        );
        let slack = self.data.slack as i64 - c.data.slack as i64;
        println!(
            "  {:<44} {:>7} {:>12} {:>8}",
            "  slack, less the copy's",
            "",
            human(slack.max(0) as u64),
            percent(slack.max(0) as u64, overhead)
        );
        let framing = self.data.framing as i64 - c.data.framing as i64;
        println!(
            "  {:<44} {:>7} {:>12} {:>8}",
            "  framing of the extra pages",
            "",
            human(framing.max(0) as u64),
            percent(framing.max(0) as u64, overhead)
        );
        let elsewhere = (self.inline + self.zero_spans) as i64 - (c.inline + c.zero_spans) as i64
            + (self.content as i64 - c.content as i64);
        if elsewhere != 0 {
            println!(
                "  {:<44} {:>7} {:>12}",
                "  less content kept inline, as zeros, or in the journal",
                "",
                format!("-{}", human(elsewhere.max(0) as u64)),
            );
        }
    }
}

fn report_data_pages(i: &Inspection) {
    let data: Vec<&PageInfo> = i.pages.iter().filter(|p| p.role == Role::Data).collect();
    if data.is_empty() {
        return;
    }
    heading("Live data pages by live fraction (live bytes over a page's content capacity)");
    println!(
        "  {:<12} {:>7} {:>12} {:>12}",
        "live", "pages", "live bytes", "not live"
    );
    let mut buckets = [(0u64, 0u64, 0u64); 11];
    for p in &data {
        let f = p.live_bytes as f64 / MAX_PAGE_CONTENT as f64;
        let b = if p.live_bytes as usize >= MAX_PAGE_CONTENT {
            10
        } else {
            ((f * 10.0) as usize).min(9)
        };
        buckets[b].0 += 1;
        buckets[b].1 += p.live_bytes as u64;
        buckets[b].2 += (MAX_PAGE_CONTENT as u64).saturating_sub(p.live_bytes as u64);
    }
    for (b, &(n, live, not)) in buckets.iter().enumerate() {
        if n == 0 {
            continue;
        }
        let label = if b == 10 {
            "full".to_string()
        } else {
            format!("{:>3}–{:>3} %", b * 10, b * 10 + 10)
        };
        println!(
            "  {label:<12} {n:>7} {:>12} {:>12}",
            human(live),
            human(not)
        );
    }

    heading("Live pages by age (flushes since the page was written)");
    println!("  {:<12} {:>7} {:>7}", "age", "data", "table");
    let e = i.header.epoch;
    let ages = [
        (0, 0, "0"),
        (1, 9, "1–9"),
        (10, 99, "10–99"),
        (100, 999, "100–999"),
        (1000, u64::MAX, "1000 and up"),
    ];
    for (lo, hi, label) in ages {
        let count = |role: Role| {
            i.pages
                .iter()
                .filter(|p| p.role == role)
                .filter(|p| match p.frame {
                    Frame::Framed { epoch, .. } => (lo..=hi).contains(&(e - epoch.min(e))),
                    _ => false,
                })
                .count()
        };
        let (d, t) = (count(Role::Data), count(Role::Table));
        if d + t > 0 {
            println!("  {label:<12} {d:>7} {t:>7}");
        }
    }
}

fn report_spans(i: &Inspection) {
    heading("Spans of the sequence");
    let spans = &i.chains[0].spans;
    if spans.is_empty() {
        println!("  none");
        return;
    }
    let mut forms = [(0u64, 0u64, 0u64); 3];
    for s in spans {
        let f = match s.form {
            Form::Ref { .. } => 0,
            Form::Inline => 1,
            Form::Zero => 2,
        };
        forms[f].0 += 1;
        forms[f].1 += s.len as u64;
        forms[f].2 += s.entry_len as u64;
    }
    println!(
        "  {:<26} {:>8} {:>12} {:>12}",
        "", "spans", "content", "entries"
    );
    for (f, label) in ["in data pages (Ref)", "inline", "zeros"]
        .iter()
        .enumerate()
    {
        if forms[f].0 > 0 {
            println!(
                "  {label:<26} {:>8} {:>12} {:>12}",
                forms[f].0,
                human(forms[f].1),
                human(forms[f].2)
            );
        }
    }
    let entries: u64 = forms.iter().map(|f| f.2).sum();
    println!(
        "  entries take {:.2} % of the sequence's length, {:.1} B per span",
        100.0 * entries as f64 / i.chains[0].len.max(1) as f64,
        entries as f64 / spans.len() as f64
    );
    let bounds = [1u32, 8, 32, 128, 512, 2048, MAX_PAGE_CONTENT as u32];
    print!("  lengths     ");
    let mut lo = 1u32;
    for &hi in &bounds {
        let n = spans.iter().filter(|s| (lo..=hi).contains(&s.len)).count();
        if lo == hi {
            print!(" {hi}: {n};");
        } else {
            print!(" {lo}–{hi}: {n};");
        }
        lo = hi + 1;
    }
    println!();
    let mean = i.chains[0].len as f64 / spans.len() as f64;
    let mut sorted: Vec<u32> = spans.iter().map(|s| s.len).collect();
    sorted.sort_unstable();
    println!(
        "  mean length {mean:.0} B, median {} B",
        sorted[sorted.len() / 2]
    );
    // How a front-to-back read of the sequence moves through the file.
    let mut jumps = 0;
    let mut pages_visited = 0;
    let mut last: Option<(u64, u64)> = None;
    for s in spans {
        if let Form::Ref { address } = s.form {
            let page = address / PAGE_SIZE as u64;
            match last {
                Some((end, p)) => {
                    if end != address {
                        jumps += 1;
                    }
                    if p != page {
                        pages_visited += 1;
                    }
                }
                None => pages_visited += 1,
            }
            last = Some((address + s.len as u64, page));
        }
    }
    println!(
        "  a front-to-back read of the sequence jumps {jumps} times and enters a data page {pages_visited} times"
    );
}

fn report_schema(table: &TypeTable, all: bool) {
    heading("Schema");
    let d = table.descriptors();
    let count = |f: fn(&TypeDescriptor) -> bool| d.iter().filter(|t| f(t)).count();
    println!(
        "  {} types: {} structs, {} enums, {} sequences, {} maps, {} arrays, {} opaque, {} fixed, {} primitives, {} text",
        d.len(),
        count(|t| matches!(t, TypeDescriptor::Struct { .. })),
        count(|t| matches!(t, TypeDescriptor::Enum { .. })),
        count(|t| matches!(t, TypeDescriptor::Sequence(_))),
        count(|t| matches!(t, TypeDescriptor::Map { .. })),
        count(|t| matches!(t, TypeDescriptor::Array { .. })),
        count(|t| matches!(t, TypeDescriptor::Opaque { .. })),
        count(|t| matches!(t, TypeDescriptor::Fixed(_))),
        count(|t| matches!(t, TypeDescriptor::Primitive(_))),
        count(|t| matches!(t, TypeDescriptor::Text)),
    );
    for t in d {
        if let TypeDescriptor::Opaque {
            library_name,
            type_name,
            version,
            fixed_size,
            ..
        } = t
        {
            println!(
                "  opaque {library_name}::{type_name} {}.{}.{}{}",
                version.major,
                version.minor,
                version.patch,
                fixed_size.map_or(String::new(), |n| format!(", {n} B"))
            );
        }
    }
    if !all {
        return;
    }
    for (n, t) in d.iter().enumerate() {
        let name = |r: &kladde_schema::TypeRef| decode::type_name(table, r.0, 1);
        let body = match t {
            TypeDescriptor::Struct { name: s, fields } => format!(
                "struct {s} {{ {} }}",
                fields
                    .iter()
                    .map(|f| format!("{}: {}", f.name, name(&f.ty)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            TypeDescriptor::Enum {
                name: e, variants, ..
            } => format!(
                "enum {e} {{ {} }}",
                variants
                    .iter()
                    .map(|v| format!("{} = {}: {}", v.name, v.discriminant, name(&v.payload)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            _ => decode::type_name(table, n, 0),
        };
        println!("  #{n:<4} {body}");
    }
}

fn report_content(i: &Inspection, table: &TypeTable, sequence: &[u8], top: usize, replayed: bool) {
    let len = sequence.len() as u64;
    heading(&format!(
        "The sequence's {} by what they lie in{}",
        human(len),
        if replayed {
            ", the journal replayed"
        } else {
            ""
        }
    ));
    let d = match decode::walk(table, sequence, top) {
        Ok(d) => d,
        Err(e) => {
            println!("  the sequence does not decode by the schema: {e}");
            return;
        }
    };
    println!(
        "  {:<52} {:>9} {:>12} {:>8}",
        "", "values", "bytes", "of it"
    );
    for g in d.groups.iter().take(30) {
        println!(
            "  {:<52} {:>9} {:>12} {:>8}",
            g.key,
            g.count,
            human(g.bytes),
            percent(g.bytes, len)
        );
    }
    if d.groups.len() > 30 {
        println!("  … and {} more", d.groups.len() - 30);
    }
    if d.largest.is_empty() {
        return;
    }
    heading("The largest values (texts, opaque values, and sequences of plain elements)");
    // Where each one lies on file, if the sequence is the committed one.
    let starts: Vec<u64> = i.chains[0]
        .spans
        .iter()
        .scan(0u64, |at, s| {
            let start = *at;
            *at += s.len as u64;
            Some(start)
        })
        .collect();
    let spans: &[SpanInfo] = &i.chains[0].spans;
    for leaf in &d.largest {
        println!(
            "  {:>10} {:>7}  {}: {}",
            human(leaf.len),
            percent(leaf.len, len),
            leaf.path,
            leaf.ty
        );
        let mut detail = format!("bytes {}–{}", leaf.start, leaf.start + leaf.len);
        if !replayed && leaf.len > 0 {
            let first = starts.partition_point(|&s| s <= leaf.start) - 1;
            let last = starts.partition_point(|&s| s < leaf.start + leaf.len) - 1;
            let mut pages: Vec<u64> = spans[first..=last]
                .iter()
                .filter_map(|s| match s.form {
                    Form::Ref { address } => Some(address / PAGE_SIZE as u64),
                    _ => None,
                })
                .collect();
            pages.sort_unstable();
            pages.dedup();
            detail += &format!(
                ", {} span{} on {} data page{}",
                last - first + 1,
                if last == first { "" } else { "s" },
                pages.len(),
                if pages.len() == 1 { "" } else { "s" }
            );
        }
        println!("  {:>19}  {detail}; {}", "", leaf.preview);
    }
}

fn report_map(i: &Inspection) {
    heading("Map of the file, 64 pages per line");
    println!("{MAP_LEGEND}");
    let mut line = String::new();
    for p in &i.pages {
        if p.number % 64 == 0 {
            if !line.is_empty() {
                println!("{line}");
            }
            line = format!("  {:>7} ", p.number);
        }
        let c = match p.role {
            Role::Header { governing: true } => 'H',
            Role::Header { governing: false } => 'h',
            Role::Table => 'T',
            Role::Journal => 'J',
            Role::Data => {
                let tenths = p.live_bytes as usize * 10 / MAX_PAGE_CONTENT;
                char::from_digit(tenths.min(9) as u32, 10).unwrap()
            }
            Role::Unreached if p.fallback => 'f',
            Role::Unreached => match p.frame {
                Frame::Framed { .. } => 's',
                Frame::Zeros => '.',
                Frame::Unframed => '?',
            },
        };
        line.push(c);
    }
    if !line.is_empty() {
        println!("{line}");
    }
}
