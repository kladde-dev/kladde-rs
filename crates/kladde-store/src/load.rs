//! Opening a file (`impl/address-table-operations.md#opening-a-file`): pick
//! the header, walk the address-table tree, merge every page's statements,
//! and resolve each id with the sweep.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};

use crate::consts::*;
use crate::error::{corrupt, Error};
use crate::hash::{IdMap, IdSet};
use crate::page::{decode_page, new_page, HeaderFields, PageBuf};
use crate::state::*;
use crate::statement::{Kind, Stmt, TableReader};
use crate::storage::Storage;

/// The governing header.
pub struct PickedHeader {
    pub slot: u32,
    pub epoch: u64,
    pub fields: HeaderFields,
    pub page: Box<PageBuf>,
}

/// Reads both header slots and picks the CRC-valid one with the higher epoch.
pub fn pick_header(storage: &dyn Storage) -> Result<PickedHeader, Error> {
    let len = storage.len()?;
    if len < PAGE_SIZE as u64 {
        return Err(Error::NotKladde);
    }
    let mut best: Option<PickedHeader> = None;
    let mut saw_magic = false;
    for slot in 0..2u32 {
        if (slot as u64 + 1) * PAGE_SIZE as u64 > len {
            break;
        }
        let mut page = new_page();
        storage.read_at(&mut page[..], slot as u64 * PAGE_SIZE as u64)?;
        if page[..11] == MAGIC {
            saw_magic = true;
        }
        let Ok(d) = decode_page(&page, true) else {
            continue;
        };
        if d.kind != KIND_ADDRESS_TABLE || d.epoch % 2 != slot as u64 {
            continue;
        }
        let fields = d.header.unwrap();
        if best.as_ref().is_none_or(|b| d.epoch > b.epoch) {
            best = Some(PickedHeader {
                slot,
                epoch: d.epoch,
                fields,
                page,
            });
        }
    }
    match best {
        Some(b) => {
            if b.fields.min_reader_version > READER_VERSION {
                return Err(Error::UnsupportedVersion {
                    min_reader_version: b.fields.min_reader_version,
                });
            }
            Ok(b)
        }
        None if saw_magic => Err(corrupt("neither header slot is valid")),
        None => Err(Error::NotKladde),
    }
}

/// Everything a load produces besides the [`State`].
pub struct Loaded {
    pub state: State,
    pub header: PickedHeader,
    /// Resident address-table pages, the governing header's included.
    pub table_pages: IdMap<Box<PageBuf>>,
    /// Each table page's child list, the header's under its slot.
    pub children: IdMap<Vec<u32>>,
    /// The number of pages in the file.
    pub file_pages: u32,
}

struct Run<'a> {
    reader: TableReader<'a>,
    page: u32,
    epoch: u64,
    cur: Option<(Stmt, u8)>,
}

/// A statement as the merge delivers it.
#[derive(Clone, Copy, Debug)]
struct Item {
    stmt: Stmt,
    framing: u8,
    page: u32,
    epoch: u64,
}

struct Merge<'a> {
    runs: Vec<Run<'a>>,
    heap: BinaryHeap<Reverse<(u64, usize)>>,
}

impl<'a> Merge<'a> {
    fn new(mut runs: Vec<Run<'a>>) -> Result<Self, Error> {
        let mut heap = BinaryHeap::new();
        for (i, r) in runs.iter_mut().enumerate() {
            if let Some(d) = r.reader.next_stmt()? {
                r.cur = Some((d.stmt, d.framing));
                heap.push(Reverse((d.stmt.sort_key(), i)));
            }
        }
        Ok(Merge { runs, heap })
    }

    fn peek(&self) -> Option<Item> {
        let Reverse((_, i)) = *self.heap.peek()?;
        let r = &self.runs[i];
        let (stmt, framing) = r.cur.unwrap();
        Some(Item {
            stmt,
            framing,
            page: r.page,
            epoch: r.epoch,
        })
    }

    fn next(&mut self) -> Result<Option<Item>, Error> {
        let Some(Reverse((_, i))) = self.heap.pop() else {
            return Ok(None);
        };
        let r = &mut self.runs[i];
        let (stmt, framing) = r.cur.take().unwrap();
        let item = Item {
            stmt,
            framing,
            page: r.page,
            epoch: r.epoch,
        };
        if let Some(d) = r.reader.next_stmt()? {
            r.cur = Some((d.stmt, d.framing));
            self.heap.push(Reverse((d.stmt.sort_key(), i)));
        }
        Ok(Some(item))
    }
}

/// An open statement in the sweep, ordered by epoch.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Open {
    epoch: u64,
    end: u64,
    idx: usize,
}

struct Entry {
    item: Item,
    slot: Option<StmtRef>,
}

/// The per-load accumulators the sweep writes into.
struct Sink {
    state: State,
    emitted: Vec<(Key, Fragment)>,
    data_pages: IdSet,
    youngest: IdMap<u64>,
}

impl Sink {
    fn slot_of(&mut self, entries: &mut [Entry], idx: usize) -> StmtRef {
        if let Some(s) = entries[idx].slot {
            return s;
        }
        let it = entries[idx].item;
        let s = self
            .state
            .slab
            .alloc(it.page, it.framing, it.stmt.id, it.stmt.kind);
        // A chunk of its table page; a `Ref`'s payload is counted among its
        // data page's chunks once the fragments show whether it is whole.
        let stated = match it.stmt.kind {
            Kind::Ref | Kind::Inline => it.stmt.size,
            _ => 0,
        };
        self.state.add_chunk(s, it.page, None, stated);
        self.state.cover(it.page, it.framing as u32);
        entries[idx].slot = Some(s);
        s
    }

    fn emit(
        &mut self,
        id: u32,
        start: u32,
        end: u32,
        winner: Option<usize>,
        entries: &mut [Entry],
        stmt_bytes: &mut u32,
    ) {
        let f = match winner {
            None => Fragment::ZeroByDefault,
            Some(idx) => {
                let fresh = entries[idx].slot.is_none();
                let s = self.slot_of(entries, idx);
                if fresh {
                    *stmt_bytes += entries[idx].item.framing as u32;
                }
                self.state.pin(s);
                let it = entries[idx].item;
                let d = start - it.stmt.offset;
                match it.stmt.kind {
                    Kind::Ref => {
                        let (page, off) = split_address(it.stmt.address + d as u64);
                        self.state.ensure_page(page);
                        self.state.pages[page as usize].coverage += end - start;
                        self.data_pages.insert(page);
                        self.state.note_reverse(page, id, start, end);
                        let y = self.youngest.entry(id).or_default();
                        *y = (*y).max(u64::from(page) << 32);
                        Fragment::Bytes {
                            page,
                            offset: off,
                            stmt: s,
                        }
                    }
                    Kind::Inline => {
                        let (_, off) = split_address(it.stmt.address + d as u64);
                        self.state.pages[it.page as usize].coverage += end - start;
                        Fragment::Bytes {
                            page: it.page,
                            offset: off,
                            stmt: s,
                        }
                    }
                    _ => Fragment::ZeroExplicitly { stmt: s },
                }
            }
        };
        self.emitted.push((key(id, start), f));
    }
}

/// Resolves every statement naming the id at the head of `merge`.
fn resolve_id(sink: &mut Sink, merge: &mut Merge) -> Result<(), Error> {
    let id = merge.peek().unwrap().stmt.id;
    let mut entries: Vec<Entry> = Vec::new();
    let mut open: BinaryHeap<Open> = BinaryHeap::new();
    let mut newest: Option<usize> = None;
    let mut grow: Option<usize> = None;
    let mut mentions = 0u32;
    let mut pos: u64 = 0;
    let mut current: Option<usize> = None;
    let mut run_start: u64 = 0;
    let mut stmt_bytes = 0u32;
    let first_fragment = sink.emitted.len();

    loop {
        while open.peek().is_some_and(|e| e.end <= pos) {
            open.pop();
        }
        while merge
            .peek()
            .is_some_and(|s| s.stmt.id == id && s.stmt.start() as u64 <= pos)
        {
            let item = merge.next()?.unwrap();
            let idx = entries.len();
            entries.push(Entry { item, slot: None });
            mentions += 1;
            if newest.is_none_or(|n| item.epoch > entries[n].item.epoch) {
                newest = Some(idx);
            }
            if item.stmt.end() > pos {
                open.push(Open {
                    epoch: item.epoch,
                    end: item.stmt.end(),
                    idx,
                });
            } else if item.stmt.kind == Kind::Grow
                && open.peek().is_none_or(|t| item.epoch > t.epoch)
            {
                grow = Some(idx);
            }
        }
        let winner = open.peek().map(|e| e.idx);
        if winner != current {
            if pos > run_start {
                sink.emit(
                    id,
                    run_start as u32,
                    pos as u32,
                    current,
                    &mut entries,
                    &mut stmt_bytes,
                );
            }
            current = winner;
            run_start = pos;
        }
        let next_start = merge
            .peek()
            .filter(|s| s.stmt.id == id)
            .map(|s| s.stmt.start() as u64);
        if next_start.is_none() && winner.is_none_or(|w| entries[w].item.stmt.kind.is_unbounded()) {
            break;
        }
        pos = next_start
            .unwrap_or(u64::MAX)
            .min(winner.map_or(u64::MAX, |w| entries[w].item.stmt.end()));
        if pos > u32::MAX as u64 {
            return Err(corrupt("an allocation reaches past 2^32 - 1"));
        }
    }

    let grow_n = grow.map_or(0, |g| entries[g].item.stmt.offset as u64);
    let size = run_start.max(grow_n);
    if size > u32::MAX as u64 {
        return Err(corrupt("an allocation is larger than 2^32 - 1 bytes"));
    }
    if run_start < size {
        sink.emit(
            id,
            run_start as u32,
            size as u32,
            current,
            &mut entries,
            &mut stmt_bytes,
        );
    }
    let size = size as u32;
    let newest = newest.unwrap();
    let exists = entries[newest].item.stmt.kind != Kind::Tombstone;
    let anchor = current.filter(|&c| entries[c].item.stmt.kind.is_unbounded());
    let fragment_count = (sink.emitted.len() - first_fragment) as u32;

    if exists {
        let mut meta = AllocationMeta {
            size,
            fragment_count,
            mentions,
            ..Default::default()
        };
        if let Some(a) = anchor {
            let fresh = entries[a].slot.is_none();
            let s = sink.slot_of(&mut entries, a);
            if fresh {
                stmt_bytes += entries[a].item.framing as u32;
            }
            sink.state.pin(s);
            meta.anchor = Some(s);
            meta.anchor_n = entries[a].item.stmt.offset;
        }
        if let Some(g) = grow {
            let n = entries[g].item.stmt.offset;
            if n as u64 > run_start || size == 0 {
                let fresh = entries[g].slot.is_none();
                let s = sink.slot_of(&mut entries, g);
                if fresh {
                    stmt_bytes += entries[g].item.framing as u32;
                }
                sink.state.pin(s);
                meta.grow = Some(s);
                meta.grow_n = n;
            }
        }
        meta.statement_bytes = stmt_bytes;
        sink.state.allocs.insert(id, meta);
    } else {
        debug_assert_eq!(size, 0);
        let tombstone = if mentions > 1 {
            let a = anchor.expect("a non-existent id is anchored by its tombstone");
            let s = sink.slot_of(&mut entries, a);
            sink.state.pin(s);
            Some(s)
        } else {
            None
        };
        sink.state.recyclable.insert(
            id,
            Recyclable {
                mentions,
                tombstone,
            },
        );
    }
    Ok(())
}

/// Loads the world of the governing header, after the caller's `fsync`.
pub fn load(storage: &dyn Storage, header: PickedHeader) -> Result<Loaded, Error> {
    let file_pages = (storage.len()? / PAGE_SIZE as u64).min(u32::MAX as u64) as u32;
    let mut state = State::default();
    state
        .pages
        .resize(file_pages.max(2) as usize, PageInfo::default());
    for slot in 0..2 {
        state.pages[slot].state = PageState::Header;
    }

    // 1. Walk the tree, keeping every table page resident.
    let mut table_pages: IdMap<Box<PageBuf>> = IdMap::default();
    let mut children: IdMap<Vec<u32>> = IdMap::default();
    let mut contents: Vec<(u32, u64, std::ops::Range<usize>)> = Vec::new();
    let mut seen = IdSet::default();
    let mut queue: VecDeque<(u32, u32)> = VecDeque::new();
    {
        let d = decode_page(&header.page, true)
            .map_err(|_| corrupt("the governing header does not decode"))?;
        let (kids, _) = TableReader::open(&header.page[..], d.content.clone())?;
        contents.push((header.slot, header.epoch, d.content));
        for &c in &kids {
            queue.push_back((c, header.slot));
        }
        children.insert(header.slot, kids);
        seen.insert(header.slot);
    }
    while let Some((p, parent)) = queue.pop_front() {
        if p < 2 || p >= file_pages || !seen.insert(p) {
            return Err(corrupt("the address-table pages do not form a tree"));
        }
        let mut page = new_page();
        storage.read_at(&mut page[..], p as u64 * PAGE_SIZE as u64)?;
        let d = decode_page(&page, false)
            .map_err(|_| corrupt(format!("address-table page {p} fails its check")))?;
        if d.kind != KIND_ADDRESS_TABLE {
            return Err(corrupt(format!("page {p} is not an address-table page")));
        }
        if d.epoch > header.epoch {
            return Err(corrupt(format!(
                "page {p} is newer than its header: the fsync contract was broken"
            )));
        }
        let (kids, _) = TableReader::open(&page[..], d.content.clone())?;
        let info = &mut state.pages[p as usize];
        info.epoch = d.epoch;
        info.parent = parent;
        info.written = (d.content.end - d.content.start) as u16;
        for &c in &kids {
            queue.push_back((c, p));
        }
        contents.push((p, d.epoch, d.content));
        children.insert(p, kids);
        table_pages.insert(p, page);
    }
    // A page with children is interior; it holds child references only, and
    // they count as its coverage.
    for (&p, kids) in &children {
        if p >= 2 && !kids.is_empty() {
            state.pages[p as usize].state = PageState::Interior;
        }
    }
    for &(p, _, _) in &contents {
        if p >= 2 && state.pages[p as usize].state != PageState::Interior {
            state.pages[p as usize].state = PageState::Table;
        }
    }
    for (&p, kids) in &children {
        if p >= 2 && !kids.is_empty() {
            state.pages[p as usize].coverage =
                crate::statement::TableWriter::children_len(kids) as u32;
        }
    }
    state.pages[header.slot as usize].epoch = header.epoch;

    // 2. Merge and resolve.
    let mut sink = Sink {
        state,
        emitted: Vec::new(),
        data_pages: IdSet::default(),
        youngest: IdMap::default(),
    };
    {
        let mut runs = Vec::with_capacity(contents.len());
        for (p, epoch, content) in &contents {
            let buf: &PageBuf = if *p == header.slot {
                &header.page
            } else {
                &table_pages[p]
            };
            let (_, reader) = TableReader::open(&buf[..], content.clone())?;
            runs.push(Run {
                reader,
                page: *p,
                epoch: *epoch,
                cur: None,
            });
        }
        let mut merge = Merge::new(runs)?;
        while merge.peek().is_some() {
            resolve_id(&mut sink, &mut merge)?;
        }
    }
    let Sink {
        mut state,
        emitted,
        data_pages,
        ..
    } = sink;
    state.frags = emitted.into_iter().collect();

    // 3. Mirror nothing, but check every data page current state reaches.
    let mut buf = new_page();
    for &p in &data_pages {
        if p < 2 || p >= file_pages {
            return Err(corrupt(format!(
                "a Ref points outside the file, into page {p}"
            )));
        }
        storage.read_at(&mut buf[..], p as u64 * PAGE_SIZE as u64)?;
        let d = decode_page(&buf, false)
            .map_err(|_| corrupt(format!("data page {p} fails its check")))?;
        if d.kind != KIND_DATA {
            return Err(corrupt(format!(
                "page {p} is referenced as data but is not a data page"
            )));
        }
        if d.epoch > header.epoch {
            return Err(corrupt(format!(
                "page {p} is newer than its header: the fsync contract was broken"
            )));
        }
        let info = &mut state.pages[p as usize];
        info.state = PageState::Data;
        info.epoch = d.epoch;
        info.written = (d.content.end - d.content.start) as u16;
    }
    // A `Ref` whose payload is whole is one of its data page's untouched
    // chunks; one that has lost bytes is known to drain.
    let mut refs: IdMap<(u32, u32)> = IdMap::default();
    for (&k, &f) in &state.frags {
        if let Fragment::Bytes { page, stmt, .. } = f {
            if state.slab.kinds[stmt.idx()] == Kind::Ref {
                let len = state.frag_end(k) - koff(k);
                refs.entry(stmt.idx() as u32).or_insert((page, 0)).1 += len;
            }
        }
    }
    for (&s, &(page, live)) in &refs {
        let size = state.slab.size[s as usize];
        if live >= size {
            let info = &mut state.pages[page as usize];
            info.untouched += 1;
            info.untouched_bytes += size;
        } else {
            state.slab.touched[s as usize] = true;
        }
    }
    for p in 2..file_pages {
        state.rerank(p);
    }
    state.dropped_candidates.clear();
    let mut header = header;
    table_pages.insert(header.slot, std::mem::replace(&mut header.page, new_page()));
    Ok(Loaded {
        state,
        header,
        table_pages,
        children,
        file_pages,
    })
}
