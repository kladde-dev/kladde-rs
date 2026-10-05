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
    /// Each data page a resolved `Ref` points into, and the bytes resolved
    /// to it: its coverage once its frame has been checked.
    data_pages: IdMap<u32>,
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
        self.state.cover(it.page, it.framing as u32);
        entries[idx].slot = Some(s);
        s
    }

    /// Emits `[start, end)` of `id`, won by the content statement `entries[idx]`.
    fn emit(
        &mut self,
        id: u32,
        start: u32,
        end: u32,
        idx: usize,
        entries: &mut [Entry],
        stmt_bytes: &mut u32,
    ) {
        let fresh = entries[idx].slot.is_none();
        let s = self.slot_of(entries, idx);
        if fresh {
            *stmt_bytes += entries[idx].item.framing as u32;
        }
        self.state.pin(s);
        let it = entries[idx].item;
        let d = start - it.stmt.offset;
        let f = match it.stmt.kind {
            Kind::Ref => {
                let (page, off) = split_address(it.stmt.address + d as u64);
                *self.data_pages.entry(page).or_default() += end - start;
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
                self.state.cover(it.page, end - start);
                Fragment::Bytes {
                    page: it.page,
                    offset: off,
                    stmt: s,
                }
            }
            _ => Fragment::Zero { stmt: s },
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
    let mut sizing: Option<usize> = None;
    let mut mentions = 0u32;
    // The envelope: where each run starts, and the content statement that
    // wins it, if any.
    let mut runs: Vec<(u64, Option<usize>)> = Vec::new();
    let mut pos: u64 = 0;
    let mut current: Option<usize> = None;
    let mut run_start: u64 = 0;

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
            if item.stmt.states_size() {
                match sizing.map(|z| entries[z].item.epoch) {
                    Some(e) if e == item.epoch => {
                        return Err(corrupt(format!(
                            "two statements of epoch {e} state the size of allocation {id}"
                        )))
                    }
                    Some(e) if e > item.epoch => {}
                    _ => sizing = Some(idx),
                }
            }
            if item.stmt.kind.is_content() && item.stmt.end() > pos {
                open.push(Open {
                    epoch: item.epoch,
                    end: item.stmt.end(),
                    idx,
                });
            }
        }
        let winner = open.peek().map(|e| e.idx);
        if winner != current {
            if pos > run_start {
                runs.push((run_start, current));
            }
            current = winner;
            run_start = pos;
        }
        let next_start = merge
            .peek()
            .filter(|s| s.stmt.id == id)
            .map(|s| s.stmt.start() as u64);
        if next_start.is_none() && winner.is_none() {
            break;
        }
        pos = next_start
            .unwrap_or(u64::MAX)
            .min(winner.map_or(u64::MAX, |w| entries[w].item.stmt.end()));
        if pos > u32::MAX as u64 {
            return Err(corrupt("an allocation reaches past 2^32 - 1"));
        }
    }
    runs.push((run_start, None));

    // The size is known only now, so the envelope is clipped to it here: runs
    // at or past it go, and every run below it must have a winner.
    let size = sizing.map_or(0, |z| entries[z].item.stmt.stated_size());
    let mut stmt_bytes = 0u32;
    let first_fragment = sink.emitted.len();
    for (i, &(start, winner)) in runs.iter().enumerate() {
        if start >= size as u64 {
            break;
        }
        let end = runs
            .get(i + 1)
            .map_or(size as u64, |r| r.0)
            .min(size as u64);
        let Some(w) = winner else {
            return Err(corrupt(format!(
                "byte {start} of allocation {id} is matched by no statement"
            )));
        };
        sink.emit(
            id,
            start as u32,
            end as u32,
            w,
            &mut entries,
            &mut stmt_bytes,
        );
    }
    let newest = newest.unwrap();
    let exists = entries[newest].item.stmt.kind != Kind::Tombstone;
    let fragment_count = (sink.emitted.len() - first_fragment) as u32;

    if exists {
        let mut meta = AllocationMeta {
            size,
            fragment_count,
            mentions,
            ..Default::default()
        };
        if let Some(z) = sizing {
            let fresh = entries[z].slot.is_none();
            let s = sink.slot_of(&mut entries, z);
            if fresh {
                stmt_bytes += entries[z].item.framing as u32;
            }
            sink.state.pin(s);
            meta.size_statement = Some(s);
        }
        meta.statement_bytes = stmt_bytes;
        sink.state.allocs.insert(id, meta);
    } else {
        debug_assert_eq!(size, 0);
        // The tombstone is the newest statement, so it holds `S` while
        // anything older still names the id.
        let tombstone = if mentions > 1 {
            let s = sink.slot_of(&mut entries, newest);
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
    let mut state = State {
        flush_epoch: header.epoch,
        ..State::default()
    };
    // Every page starts reusable, the other header slot included: the
    // caller's `fsync` retired the older world.
    state.ensure_page(file_pages.max(2) - 1);

    // 1. Walk the tree, keeping every table page resident.
    let mut table_pages: IdMap<Box<PageBuf>> = IdMap::default();
    let mut contents: Vec<(u32, u64, std::ops::Range<usize>)> = Vec::new();
    let mut seen = IdSet::default();
    let mut queue: VecDeque<u32> = VecDeque::new();
    {
        let d = decode_page(&header.page, true)
            .map_err(|_| corrupt("the governing header does not decode"))?;
        let (kids, _) = TableReader::open(&header.page[..], d.content.clone())?;
        let written = d.content.len() as u16;
        contents.push((header.slot, header.epoch, d.content));
        queue.extend(&kids);
        state.children.insert(header.slot, kids);
        state.pages[header.slot as usize] = Page::Live(Live {
            epoch: header.epoch,
            coverage: 0,
            kind: PageKind::Header,
            written,
            drain: Drain::default(),
        });
        seen.insert(header.slot);
    }
    while let Some(p) = queue.pop_front() {
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
        // Its child references are charged to it; its statements are added
        // as the merge resolves them.
        state.pages[p as usize] = Page::Live(Live {
            epoch: d.epoch,
            coverage: charged_child_bytes(&kids),
            kind: PageKind::Table,
            written: d.content.len() as u16,
            drain: Drain::default(),
        });
        queue.extend(&kids);
        contents.push((p, d.epoch, d.content));
        state.children.insert(p, kids);
        table_pages.insert(p, page);
    }

    // 2. Merge and resolve.
    let mut sink = Sink {
        state,
        emitted: Vec::new(),
        data_pages: IdMap::default(),
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

    // 3. Check every data page current state reaches against what the `Ref`s
    //    into it say it is, and record it with the epoch its frame gives.
    let mut buf = new_page();
    let mut data_pages: Vec<(u32, u32)> = data_pages.into_iter().collect();
    data_pages.sort_unstable();
    for (p, coverage) in data_pages {
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
        state.pages[p as usize] = Page::Live(Live {
            epoch: d.epoch,
            coverage,
            kind: PageKind::Data,
            written: d.content.len() as u16,
            drain: Drain::default(),
        });
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
        file_pages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::encode_page;
    use crate::statement::TableWriter;
    use crate::storage::MemoryStorage;

    /// A file of two table pages: the leaf at page 3, holding `older` at
    /// epoch 1, and the header, holding `newer` at epoch 2. `Inline`
    /// payloads are 7s.
    fn file(older: &[Stmt], newer: &[Stmt]) -> MemoryStorage {
        let encode = |stmts: &[Stmt], children: &[u32]| {
            let mut w = TableWriter::new();
            for &c in children {
                w.child(c);
            }
            w.end_children();
            for s in stmts {
                let len = if s.kind == Kind::Inline { s.size } else { 0 };
                w.push(s, &vec![7; len as usize]);
            }
            w.into_content()
        };
        let mut storage = MemoryStorage::new();
        storage.set_len(4 * PAGE_SIZE as u64).unwrap();
        let mut page = new_page();
        encode_page(&mut page, None, KIND_ADDRESS_TABLE, 1, &encode(older, &[]));
        storage.write_at(&page[..], 3 * PAGE_SIZE as u64).unwrap();
        let header = HeaderFields {
            journal_pointer: 2,
            ..Default::default()
        };
        encode_page(
            &mut page,
            Some(&header),
            KIND_ADDRESS_TABLE,
            2,
            &encode(newer, &[3]),
        );
        storage.write_at(&page[..], 0).unwrap();
        storage
    }

    fn open(storage: &MemoryStorage) -> Result<State, Error> {
        Ok(load(storage, pick_header(storage)?)?.state)
    }

    #[test]
    fn the_newest_statement_of_the_size_wins_over_older_extents() {
        let state = open(&file(
            &[Stmt::inline(5, 0, 100).sized()],
            &[Stmt::size_to(5, 30)],
        ))
        .unwrap();
        state.check();
        let m = &state.allocs[&5];
        assert_eq!(m.size, 30);
        assert_eq!(m.fragment_count, 1);
        // The older `Inline*` still wins `[0, 30)`, from the leaf, and the
        // `Size`, which owns nothing, holds its pin all the same.
        let (_, f, end) = state.resolve(5, 0).unwrap();
        assert!(matches!(f, Fragment::Bytes { page: 3, .. }));
        assert_eq!(end, 30);
        let s = m.size_statement.unwrap();
        assert_eq!(state.slab.kinds[s.idx()], Kind::Size);
        assert_eq!(state.slab.pins[s.idx()], 1);
    }

    #[test]
    fn a_newer_sizing_statement_grows_past_an_older_size() {
        let state = open(&file(
            &[Stmt::inline(5, 0, 10), Stmt::size_to(5, 10)],
            &[Stmt::zero(5, 10, 40).sized()],
        ))
        .unwrap();
        state.check();
        assert_eq!(state.allocs[&5].size, 50);
        assert!(matches!(
            state.resolve(5, 20).unwrap().1,
            Fragment::Zero { .. }
        ));
        // The older `Size` decides nothing any more, and gets no slot.
        assert_eq!(state.slab.live, 2);
    }

    #[test]
    fn a_byte_below_the_size_that_nothing_matches_is_an_error() {
        let err = open(&file(&[], &[Stmt::inline(5, 0, 10), Stmt::size_to(5, 20)]));
        assert!(matches!(err, Err(Error::Corrupt(_))), "{:?}", err.err());
    }

    #[test]
    fn two_statements_of_one_epoch_that_state_the_size_are_an_error() {
        let err = open(&file(
            &[],
            &[Stmt::inline(5, 0, 10).sized(), Stmt::size_to(5, 10)],
        ));
        assert!(matches!(err, Err(Error::Corrupt(_))), "{:?}", err.err());
    }

    #[test]
    fn a_tombstone_holds_its_pin_while_older_statements_name_its_id() {
        let state = open(&file(
            &[Stmt::inline(6, 0, 10).sized()],
            &[Stmt::tombstone(6)],
        ))
        .unwrap();
        state.check();
        assert!(!state.allocs.contains_key(&6));
        let r = state.recyclable[&6];
        assert_eq!(r.mentions, 2);
        let t = r.tombstone.unwrap();
        assert_eq!(state.slab.kinds[t.idx()], Kind::Tombstone);
        assert_eq!(state.slab.live, 1);
    }
}
