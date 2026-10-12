//! Opening a file: pick the header, read every span-table page it reaches
//! and keep each span id's newest entry, walk the chains from their heads,
//! and read the data pages they show, in page order.

use crate::consts::*;
use crate::entries::{decode, KindIn};
use crate::error::{corrupt, Error};
use crate::hash::{IdMap, IdSet};
use crate::page::{decode_page, new_page, HeaderFields, PageBuf};
use crate::state::*;
use crate::storage::Storage;
use crate::treap::NIL;

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
        if d.kind != KIND_TABLE || d.epoch % 2 != slot as u64 {
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

/// Everything a load produces.
pub struct Loaded {
    pub state: State,
    pub header: PickedHeader,
    /// Resident span-table pages, the governing header's included.
    pub table_pages: IdMap<Box<PageBuf>>,
    /// Data pages the chains show.
    pub mirror: IdMap<Box<PageBuf>>,
    /// The number of pages in the file.
    pub file_pages: u32,
    /// The content of the schema's chain and of the private chain.
    pub schema: Vec<u8>,
    pub private: Vec<u8>,
}

/// A span id's newest entry so far.
#[derive(Clone, Copy)]
struct Newest {
    epoch: u64,
    page: u32,
    next: u32,
    kind: KindIn,
    len: u16,
    /// Where the page's content starts within the page.
    content_at: usize,
}

/// Loads the world of `header`.
pub fn load(storage: &dyn Storage, header: PickedHeader) -> Result<Loaded, Error> {
    let file_pages = (storage.len()? / PAGE_SIZE as u64) as u32;
    let mut state = State {
        flush_epoch: header.epoch,
        ..State::default()
    };
    state.ensure_page(file_pages.saturating_sub(1).max(1));
    let mut table_pages: IdMap<Box<PageBuf>> = IdMap::default();
    let mut newest: IdMap<Newest> = IdMap::default();
    let mut seen_page = IdSet::default();

    // The header and the leaves it names.
    let hd = decode_page(&header.page, true).map_err(|_| corrupt("the header fails its checks"))?;
    let content = &header.page[hd.content.clone()];
    let decoded = decode(content)?;
    let leaves = decoded.children.clone();
    let mut pages_to_read: Vec<(u32, u64, crate::entries::Decoded, usize)> =
        vec![(header.slot, header.epoch, decoded, HEADER_CONTENT_OFFSET)];
    for &leaf in &leaves {
        if leaf < 2 || leaf >= file_pages || !seen_page.insert(leaf) {
            return Err(corrupt(format!(
                "the header names leaf {leaf} out of bounds or twice"
            )));
        }
        let mut buf = new_page();
        storage.read_at(&mut buf[..], leaf as u64 * PAGE_SIZE as u64)?;
        let d = decode_page(&buf, false)
            .map_err(|_| corrupt(format!("leaf {leaf} fails its checks")))?;
        if d.kind != KIND_TABLE || d.epoch > header.epoch {
            return Err(corrupt(format!(
                "leaf {leaf} is not a table page of the header's world"
            )));
        }
        let dec = decode(&buf[d.content.clone()])?;
        if !dec.children.is_empty() {
            return Err(corrupt(format!(
                "leaf {leaf} names pages of its own, which this implementation does not read"
            )));
        }
        pages_to_read.push((leaf, d.epoch, dec, CONTENT_OFFSET));
        table_pages.insert(leaf, buf);
    }
    table_pages.insert(header.slot, header.page.clone());
    for (page, epoch, dec, content_at) in pages_to_read {
        for e in dec.entries {
            let n = Newest {
                epoch,
                page,
                next: e.next,
                kind: e.kind,
                len: e.len,
                content_at,
            };
            match newest.get(&e.id) {
                Some(old) if old.epoch == epoch => {
                    return Err(corrupt(format!(
                        "span {} has two entries of epoch {epoch}",
                        e.id
                    )))
                }
                Some(old) if old.epoch > epoch => {}
                _ => {
                    newest.insert(e.id, n);
                }
            }
        }
    }

    // The chains.
    let mut reached = IdSet::default();
    let mut walk = |head: u32, chain: Chain, state: &mut State| -> Result<Vec<u32>, Error> {
        let mut out = Vec::new();
        let mut id = head;
        while id != 0 {
            if !reached.insert(id) {
                return Err(corrupt(format!("span {id} is reached twice")));
            }
            let n = *newest.get(&id).ok_or_else(|| {
                corrupt(format!(
                    "the chain names span {id}, which no entry describes"
                ))
            })?;
            let (len, content) = match n.kind {
                KindIn::Ref { len, addr } => {
                    let (page, offset) = (
                        (addr / PAGE_SIZE as u64) as u32,
                        (addr % PAGE_SIZE as u64) as u16,
                    );
                    if page >= file_pages {
                        return Err(corrupt(format!("span {id} shows a page past the file")));
                    }
                    (len, Content::Ref { page, offset })
                }
                KindIn::Zero { len } => (len, Content::Zero),
                KindIn::Inline { offset, len } => (
                    len,
                    Content::Inline {
                        page: n.page,
                        offset: (n.content_at + offset) as u16,
                    },
                ),
            };
            if state.spans.len() <= id as usize {
                state.spans.resize(id as usize + 1, Span::default());
            }
            state.ids.mark(id);
            state.spans[id as usize] = Span {
                len,
                next: n.next,
                content,
                chain,
                entry_page: NONE,
                entry_len: 0,
                tab: 0,
                rev: 0,
                node: NIL,
                heat: n.epoch,
            };
            out.push(id);
            id = n.next;
        }
        Ok(out)
    };
    let seq = walk(header.fields.sequence_head, Chain::Sequence, &mut state)?;
    let schema = walk(header.fields.schema_head, Chain::Schema, &mut state)?;
    let private = walk(header.fields.private_head, Chain::Private, &mut state)?;

    // The pages: the header, the leaves, and the data pages the chains show.
    state.pages[header.slot as usize] = Page::Live(Live {
        epoch: header.epoch,
        coverage: 0,
        kind: PageKind::Header,
        slot: None,
    });
    for &leaf in &leaves {
        state.pages[leaf as usize] = Page::Live(Live {
            epoch: table_epoch(&table_pages[&leaf]),
            coverage: 0,
            kind: PageKind::Table,
            slot: None,
        });
        // A leaf that keeps no live entry is unlinked by the next flush.
        state.dropped_candidates.insert(leaf);
    }
    let mut data_pages: Vec<u32> = Vec::new();
    for &id in seq.iter().chain(&schema).chain(&private) {
        if let Content::Ref { page, .. } = state.spans[id as usize].content {
            if !matches!(state.pages[page as usize], Page::Live(_)) {
                data_pages.push(page);
                state.pages[page as usize] = Page::Live(Live {
                    epoch: 0,
                    coverage: 0,
                    kind: PageKind::Data,
                    slot: None,
                });
            } else if state.live(page).kind != PageKind::Data {
                return Err(corrupt(format!("span {id} shows a table page's bytes")));
            }
        }
    }
    data_pages.sort_unstable();
    let mut mirror: IdMap<Box<PageBuf>> = IdMap::default();
    for p in data_pages {
        let mut buf = new_page();
        storage.read_at(&mut buf[..], p as u64 * PAGE_SIZE as u64)?;
        let d = decode_page(&buf, false)
            .map_err(|_| corrupt(format!("data page {p} fails its checks")))?;
        if d.kind != KIND_DATA || d.epoch > header.epoch {
            return Err(corrupt(format!(
                "page {p} is not a data page of the header's world"
            )));
        }
        state.live_mut(p).epoch = d.epoch;
        mirror.insert(p, buf);
    }
    // Coverage, the reverse lists, and the rope.
    for &id in seq.iter().chain(&schema).chain(&private) {
        let n = newest[&id];
        let s = state.spans[id as usize];
        if let Content::Ref { offset, .. } = s.content {
            if offset as usize + s.len as usize > PAGE_SIZE - 4 {
                return Err(corrupt(format!("span {id} reaches past its page")));
            }
        }
        state.note_data(id);
        if let Content::Ref { page, .. } = s.content {
            state.cover(page, s.len);
        }
        state.note_entry(id, n.page, n.len);
    }
    let mut root = NIL;
    for &id in &seq {
        let len = state.spans[id as usize].len;
        let node = state.rope.node(RopeItem::Span { id, len });
        state.spans[id as usize].node = node;
        root = state.rope.merge(root, node);
    }
    state.root = root;
    state.schema = schema;
    state.private = private;
    for p in 0..state.pages.len() as u32 {
        if matches!(state.pages[p as usize], Page::Live(_)) {
            state.rebucket(p);
        }
    }
    let read_chain = |list: &[u32], state: &State| -> Vec<u8> {
        let mut out = Vec::new();
        for &id in list {
            let s = state.span(id);
            match s.content {
                Content::Ref { page, offset } => out.extend_from_slice(
                    &mirror[&page][offset as usize..offset as usize + s.len as usize],
                ),
                Content::Inline { page, offset } => out.extend_from_slice(
                    &table_pages[&page][offset as usize..offset as usize + s.len as usize],
                ),
                _ => out.resize(out.len() + s.len as usize, 0),
            }
        }
        out
    };
    let schema_bytes = read_chain(&state.schema, &state);
    let private_bytes = read_chain(&state.private, &state);
    Ok(Loaded {
        state,
        header,
        table_pages,
        mirror,
        file_pages,
        schema: schema_bytes,
        private: private_bytes,
    })
}

/// The epoch in a leaf's framing.
fn table_epoch(buf: &PageBuf) -> u64 {
    u64::from_le_bytes(buf[1..9].try_into().unwrap())
}
