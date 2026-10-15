//! A read-only, type-agnostic look at a kladde file's storage: its headers,
//! its span-table pages and the entries in them, its three chains, its data
//! pages, its journal, and what every page of the file holds
//! (`spec/tooling.md#level-1--storage-analysis`).
//!
//! The inspection reads the file as the format defines it, independently of
//! what a [`Store`](crate::Store) keeps in memory: it follows a span-table
//! tree of any depth, and it reports the entries a page holds whether or not
//! they are still live. It writes nothing.

use crate::consts::*;
use crate::entries::{decode, KindIn};
use crate::error::{corrupt, Error};
use crate::hash::{IdMap, IdSet};
use crate::journal::{recover, Record};
use crate::page::{decode_page, new_page, PageBuf};
use crate::storage::Storage;

/// One of the three chains a header names.
///
/// ```
/// use kladde_store::inspect::ChainKind;
///
/// assert_eq!(ChainKind::ALL[0], ChainKind::Sequence);
/// assert_eq!(ChainKind::Schema.name(), "schema");
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChainKind {
    /// The root value's encoding.
    Sequence,
    /// The descriptor table of the root type.
    Schema,
    /// The writing implementation's private state.
    Private,
}

impl ChainKind {
    /// The three chains, in the order [`Inspection::chains`] holds them.
    pub const ALL: [ChainKind; 3] = [ChainKind::Sequence, ChainKind::Schema, ChainKind::Private];

    /// A lowercase name for reports: `sequence`, `schema` or `private`.
    ///
    /// See [`ChainKind`] for an example.
    pub fn name(self) -> &'static str {
        match self {
            ChainKind::Sequence => "sequence",
            ChainKind::Schema => "schema",
            ChainKind::Private => "private",
        }
    }
}

/// A valid header slot.
///
/// See [`Inspection`] for an example.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// The slot, 0 or 1, which is also the page number.
    pub slot: u32,
    /// The epoch of the flush that wrote it.
    pub epoch: u64,
    /// The specification version the writer used.
    pub format_version: u16,
    /// The oldest specification version that can read the file.
    pub min_reader_version: u16,
    /// The first span of the sequence, or 0 for an empty one.
    pub sequence_head: u32,
    /// The first span of the schema chain, or 0.
    pub schema_head: u32,
    /// The first span of the private chain, or 0.
    pub private_head: u32,
    /// The schema fingerprint of the root type.
    pub root_fingerprint: [u8; 16],
    /// The first page of the journal that follows this header.
    pub journal_pointer: u32,
}

/// What a page is to the governing header.
///
/// See [`Inspection`] for an example.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// A header slot: the governing one, or the other.
    Header {
        /// Whether this is the governing header.
        governing: bool,
    },
    /// A span-table page the governing header reaches.
    Table,
    /// A data page a span of a chain shows bytes of.
    Data,
    /// A page of the journal that follows the governing header.
    Journal,
    /// A payload page that a complete transaction of that journal names:
    /// a data page the journal wrote, which the flush folding the journal
    /// takes into the file.
    Payload,
    /// Nothing the governing header reaches: reusable, or kept for the
    /// fallback header (see [`PageInfo::fallback`]).
    Unreached,
}

/// What a page's bytes look like on their own, before anything reaches it.
///
/// See [`Inspection`] for an example.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frame {
    /// A page whose framing is valid.
    Framed {
        /// `Table` (`0x01`) or `Data` (`0x02`), or another byte a writer
        /// should not have used.
        kind: u8,
        /// The epoch the page carries.
        epoch: u64,
        /// The size of its `content`.
        content_size: u16,
    },
    /// A page of zeros: never written, or deallocated.
    Zeros,
    /// Neither: a journal page, a torn write, or stale bytes.
    Unframed,
}

/// The entries of a span-table page, counted.
///
/// See [`Inspection`] for an example.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TableStats {
    /// The child references, and the bytes they take with their delimiter.
    pub children: u32,
    pub child_bytes: u32,
    /// The entries the page holds, and the bytes they take.
    pub entries: u32,
    pub entry_bytes: u32,
    /// The entries that describe a span a chain reaches, and the bytes they
    /// take; the rest are superseded by a newer entry, or describe spans no
    /// chain reaches.
    pub live_entries: u32,
    pub live_entry_bytes: u32,
    /// The bytes of `Inline` payloads among the live entries.
    pub live_inline_bytes: u32,
}

/// One page of the file.
///
/// The bytes of a framed page split into 15 bytes of framing (and the 48
/// bytes of header fields, on a header page), its `content`, and slack up to
/// the page's end; `live_bytes` and `dead_bytes` split the content.
///
/// See [`Inspection`] for an example.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageInfo {
    /// The page number.
    pub number: u32,
    /// What the page is to the governing header.
    pub role: Role,
    /// What its bytes look like.
    pub frame: Frame,
    /// Whether the other valid header reaches the page, if the governing one
    /// does not: a page in that state is not reusable until the governing
    /// header is known to be durable.
    pub fallback: bool,
    /// The content bytes the governing header relies on: of a data page,
    /// those the reached spans show; of a table page, the live entries and
    /// the child references.
    pub live_bytes: u32,
    /// The rest of the content of a data or table page the governing header
    /// reaches.
    pub dead_bytes: u32,
    /// The entries of a table page the governing header reaches.
    pub table: Option<TableStats>,
}

/// Where a span's bytes are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// In a data page, at this address in the file.
    Ref {
        /// The byte offset of the span's first byte in the file.
        address: u64,
    },
    /// In its entry.
    Inline,
    /// Zeros, stated by length alone.
    Zero,
}

/// One span a chain reaches, as its newest entry describes it.
///
/// See [`Inspection`] for an example.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpanInfo {
    /// The span's id.
    pub id: u32,
    /// The bytes it shows.
    pub len: u32,
    /// Where they are.
    pub form: Form,
    /// The table page holding its entry, the epoch of that page, and the
    /// bytes the entry takes there.
    pub entry_page: u32,
    pub entry_epoch: u64,
    pub entry_len: u16,
}

/// One chain, in order.
///
/// See [`Inspection`] for an example.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Chain {
    /// The spans, from the head on.
    pub spans: Vec<SpanInfo>,
    /// The chain's content length, the sum of its spans' lengths.
    pub len: u64,
}

/// The journal that follows the governing header, as recovery reads it.
///
/// See [`Inspection`] for an example.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Journal {
    /// The pages its valid prefix occupies, in order, through any seals.
    pub pages: Vec<u32>,
    /// The payload pages its complete transactions name.
    pub payload_pages: Vec<u32>,
    /// Its complete transactions, seals included, and how many of those
    /// were seals.
    pub transactions: u64,
    pub seals: u64,
    /// The stream bytes the complete transactions take, framing included.
    pub stream_bytes: u64,
    /// Its records by kind.
    pub writes: u64,
    pub splices: u64,
    pub moves: u64,
    pub pastes: u64,
    /// The bytes `Write` and `Splice` records carry.
    pub payload_bytes: u64,
}

/// A read-only analysis of a kladde file's storage.
///
/// [`read`](Self::read) checks what the format requires of the governing
/// header's world — a valid header, a tree of table pages, chains that reach
/// each span once, spans inside data pages of the header's world — and fails
/// with [`Error::Corrupt`] where a [`Store`](crate::Store) would refuse to
/// open the file. It takes time linear in the file's length and holds the
/// governing world's table pages in memory, but not its data pages.
///
/// ```
/// use kladde_store::inspect::{Inspection, Role};
/// use kladde_store::{MemoryStorage, Store, WriteBackend};
///
/// let storage = MemoryStorage::new();
/// let store = Store::create(Box::new(storage.clone()), Default::default())?;
/// store.splice(0, 0, &[7; 10_000])?;
/// store.flush()?;
/// store.splice(0, 0, b"hello")?;
/// drop(store);
///
/// let i = Inspection::read(&storage)?;
/// assert_eq!(i.chains[0].len, 10_000);
/// assert_eq!(i.journal.transactions, 1);
/// let data = i.pages.iter().filter(|p| p.role == Role::Data).count();
/// assert_eq!(data, 3);
/// assert_eq!(i.chain_content(&storage, 0)?, vec![7; 10_000]);
/// # Ok::<(), kladde_store::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct Inspection {
    /// The file's length in bytes.
    pub file_len: u64,
    /// The governing header: the valid slot of highest epoch.
    pub header: Header,
    /// The other slot, if it is valid.
    pub other_header: Option<Header>,
    /// Every whole page of the file, by number.
    pub pages: Vec<PageInfo>,
    /// The sequence, the schema chain and the private chain, in the order of
    /// [`ChainKind::ALL`].
    pub chains: [Chain; 3],
    /// The journal that follows the governing header.
    pub journal: Journal,
    /// The payloads of the inline spans the chains reach.
    inline: IdMap<Vec<u8>>,
}

/// A header world: its table pages with their stats, its chains, the
/// payloads of the inline spans they reach, the live bytes of each data
/// page, and its journal's pages.
struct World {
    tables: Vec<(u32, u64, TableStats)>,
    chains: [Chain; 3],
    inline: IdMap<Vec<u8>>,
    data: IdMap<u32>,
    journal: Vec<u32>,
}

impl Inspection {
    /// Reads `storage` as a kladde file.
    ///
    /// Fails with [`Error::NotKladde`] if neither header slot carries the
    /// magic, and with [`Error::Corrupt`] if the governing header's world
    /// breaks the format. A fallback world that does not resolve is
    /// reported as reaching nothing. See [`Inspection`] for an example.
    pub fn read(storage: &dyn Storage) -> Result<Inspection, Error> {
        let file_len = storage.len()?;
        let file_pages = (file_len / PAGE_SIZE as u64) as u32;
        if file_pages == 0 {
            return Err(Error::NotKladde);
        }
        let mut headers: Vec<Header> = Vec::new();
        let mut saw_magic = false;
        let mut buf = new_page();
        for slot in 0..file_pages.min(2) {
            storage.read_at(&mut buf[..], slot as u64 * PAGE_SIZE as u64)?;
            saw_magic |= buf[..MAGIC.len()] == MAGIC;
            if let Some(h) = header_of(&buf, slot) {
                headers.push(h);
            }
        }
        headers.sort_by_key(|h| std::cmp::Reverse(h.epoch));
        let mut headers = headers.into_iter();
        let header = match headers.next() {
            Some(h) => h,
            None if saw_magic => return Err(corrupt("neither header slot is valid")),
            None => return Err(Error::NotKladde),
        };
        if header.min_reader_version > READER_VERSION {
            return Err(Error::UnsupportedVersion {
                min_reader_version: header.min_reader_version,
            });
        }
        let other_header = headers.next();

        let live = world(storage, &header, file_pages)?;
        let fallback = other_header
            .as_ref()
            .and_then(|h| world(storage, h, file_pages).ok());

        let recovered = recover(storage, header.epoch + 1, header.journal_pointer)?;
        let mut journal = Journal {
            pages: recovered.pages.clone(),
            payload_pages: recovered.payload_pages.clone(),
            transactions: recovered.transactions as u64,
            seals: recovered.seals as u64,
            stream_bytes: recovered.stream_bytes,
            ..Journal::default()
        };
        for r in recovered.records.iter() {
            match r {
                Record::Write { bytes, .. } => {
                    journal.writes += 1;
                    journal.payload_bytes += bytes.len as u64;
                }
                Record::Splice { bytes, .. } => {
                    journal.splices += 1;
                    journal.payload_bytes += bytes.len as u64;
                }
                Record::Move { .. } => journal.moves += 1,
                Record::Paste { .. } => journal.pastes += 1,
            }
        }

        // Every page: its role, its framing, and its share of live bytes.
        let mut role = vec![Role::Unreached; file_pages as usize];
        let mut table: IdMap<TableStats> = IdMap::default();
        role[header.slot as usize] = Role::Header { governing: true };
        if let Some(h) = &other_header {
            role[h.slot as usize] = Role::Header { governing: false };
        }
        for &(p, _, s) in &live.tables {
            if p >= 2 {
                role[p as usize] = Role::Table;
            }
            table.insert(p, s);
        }
        for &p in live.data.keys() {
            role[p as usize] = Role::Data;
        }
        for &p in &journal.pages {
            if role[p as usize] == Role::Unreached {
                role[p as usize] = Role::Journal;
            }
        }
        for &p in &journal.payload_pages {
            if role[p as usize] == Role::Unreached {
                role[p as usize] = Role::Payload;
            }
        }
        let mut in_fallback = IdSet::default();
        if let Some(f) = &fallback {
            in_fallback.extend(f.tables.iter().map(|t| t.0));
            in_fallback.extend(f.data.keys().copied());
            in_fallback.extend(f.journal.iter().copied());
        }
        let mut pages = Vec::with_capacity(file_pages as usize);
        for p in 0..file_pages {
            storage.read_at(&mut buf[..], p as u64 * PAGE_SIZE as u64)?;
            let frame = frame_of(&buf, p < 2);
            let content = match frame {
                Frame::Framed { content_size, .. } => content_size as u32,
                _ => 0,
            };
            let r = role[p as usize];
            let (live_bytes, t) = match r {
                Role::Data => (live.data[&p], None),
                Role::Table | Role::Header { governing: true } => {
                    let s = table[&p];
                    (s.child_bytes + s.live_entry_bytes, Some(s))
                }
                _ => (0, None),
            };
            let reached = matches!(
                r,
                Role::Data | Role::Table | Role::Header { governing: true }
            );
            pages.push(PageInfo {
                number: p,
                role: r,
                frame,
                fallback: r == Role::Unreached && in_fallback.contains(&p),
                live_bytes: if reached { live_bytes.min(content) } else { 0 },
                dead_bytes: if reached {
                    content.saturating_sub(live_bytes)
                } else {
                    0
                },
                table: t,
            });
        }
        Ok(Inspection {
            file_len,
            header,
            other_header,
            pages,
            chains: live.chains,
            journal,
            inline: live.inline,
        })
    }

    /// Reads the content of the chain at `index` in [`chains`](Self::chains)
    /// from `storage`, which must be the storage this inspection read: the
    /// committed content, without the journal.
    ///
    /// Panics if `index` is not 0, 1 or 2. See [`Inspection`] for an
    /// example.
    pub fn chain_content(&self, storage: &dyn Storage, index: usize) -> Result<Vec<u8>, Error> {
        let chain = &self.chains[index];
        let mut out = Vec::with_capacity(chain.len as usize);
        let mut buf = new_page();
        let mut loaded = u32::MAX;
        for s in &chain.spans {
            match s.form {
                Form::Ref { address } => {
                    let page = (address / PAGE_SIZE as u64) as u32;
                    if page != loaded {
                        storage.read_at(&mut buf[..], page as u64 * PAGE_SIZE as u64)?;
                        loaded = page;
                    }
                    let at = (address % PAGE_SIZE as u64) as usize;
                    out.extend_from_slice(&buf[at..at + s.len as usize]);
                }
                Form::Inline => out.extend_from_slice(&self.inline[&s.id]),
                Form::Zero => out.resize(out.len() + s.len as usize, 0),
            }
        }
        Ok(out)
    }
}

/// The header in `buf`, read from slot `slot`, if it is valid.
fn header_of(buf: &PageBuf, slot: u32) -> Option<Header> {
    let d = decode_page(buf, true).ok()?;
    if d.kind != KIND_TABLE || d.epoch % 2 != slot as u64 {
        return None;
    }
    let f = d.header?;
    Some(Header {
        slot,
        epoch: d.epoch,
        format_version: f.format_version,
        min_reader_version: f.min_reader_version,
        sequence_head: f.sequence_head,
        schema_head: f.schema_head,
        private_head: f.private_head,
        root_fingerprint: f.root_fingerprint,
        journal_pointer: f.journal_pointer,
    })
}

/// The framing of a page, tried as a header page for the two slots.
fn frame_of(buf: &PageBuf, slot: bool) -> Frame {
    match decode_page(buf, slot) {
        Ok(d) => Frame::Framed {
            kind: d.kind,
            epoch: d.epoch,
            content_size: d.content.len() as u16,
        },
        Err(_) if buf.iter().all(|&b| b == 0) => Frame::Zeros,
        Err(_) => Frame::Unframed,
    }
}

/// Resolves the world of `header`: the table tree, each span id's newest
/// entry, the chains, and the live bytes of each data page.
fn world(storage: &dyn Storage, header: &Header, file_pages: u32) -> Result<World, Error> {
    // The table tree, breadth first, and each entry of each page, with the
    // payload of an inline one.
    struct Entry {
        page: u32,
        epoch: u64,
        next: u32,
        kind: KindIn,
        len: u16,
        payload: Option<Vec<u8>>,
    }
    let mut newest: IdMap<Entry> = IdMap::default();
    let mut page_entries: Vec<(u32, u64, TableStats, Vec<u32>)> = Vec::new();
    let mut seen = IdSet::default();
    let mut queue = std::collections::VecDeque::from([header.slot]);
    while let Some(p) = queue.pop_front() {
        let mut buf = new_page();
        storage.read_at(&mut buf[..], p as u64 * PAGE_SIZE as u64)?;
        let d = decode_page(&buf, p == header.slot)
            .map_err(|_| corrupt(format!("table page {p} fails its checks")))?;
        if d.kind != KIND_TABLE || d.epoch > header.epoch {
            return Err(corrupt(format!(
                "page {p} is not a table page of the header's world"
            )));
        }
        let content = &buf[d.content.clone()];
        let dec = decode(content)?;
        for &c in &dec.children {
            if c < 2 || c >= file_pages || !seen.insert(c) {
                return Err(corrupt(format!(
                    "table page {p} names page {c}, out of bounds or twice"
                )));
            }
            queue.push_back(c);
        }
        let entry_bytes: u32 = dec.entries.iter().map(|e| e.len as u32).sum();
        let stats = TableStats {
            children: dec.children.len() as u32,
            child_bytes: d.content.len() as u32 - entry_bytes,
            entries: dec.entries.len() as u32,
            entry_bytes,
            ..TableStats::default()
        };
        let mut ids = Vec::with_capacity(dec.entries.len());
        for e in dec.entries {
            ids.push(e.id);
            match newest.get(&e.id) {
                Some(old) if old.epoch == d.epoch => {
                    return Err(corrupt(format!(
                        "span {} has two entries of epoch {}",
                        e.id, d.epoch
                    )))
                }
                Some(old) if old.epoch > d.epoch => {}
                _ => {
                    let payload = match e.kind {
                        KindIn::Inline { offset, len } => {
                            Some(content[offset..offset + len as usize].to_vec())
                        }
                        _ => None,
                    };
                    newest.insert(
                        e.id,
                        Entry {
                            page: p,
                            epoch: d.epoch,
                            next: e.next,
                            kind: e.kind,
                            len: e.len,
                            payload,
                        },
                    );
                }
            }
        }
        page_entries.push((p, d.epoch, stats, ids));
    }

    // The chains, and the entries and data bytes they rely on.
    let mut reached = IdSet::default();
    let mut data: IdMap<u32> = IdMap::default();
    let heads = [
        header.sequence_head,
        header.schema_head,
        header.private_head,
    ];
    let mut chains: [Chain; 3] = Default::default();
    let mut inline: IdMap<Vec<u8>> = IdMap::default();
    for (chain, head) in chains.iter_mut().zip(heads) {
        let mut id = head;
        while id != 0 {
            if !reached.insert(id) {
                return Err(corrupt(format!("span {id} is reached twice")));
            }
            let e = newest.get(&id).ok_or_else(|| {
                corrupt(format!("a chain names span {id}, which no entry describes"))
            })?;
            let (len, form) = match e.kind {
                KindIn::Ref { len, addr } => {
                    let page = (addr / PAGE_SIZE as u64) as u32;
                    if page >= file_pages {
                        return Err(corrupt(format!("span {id} shows a page past the file")));
                    }
                    *data.entry(page).or_default() += len;
                    (len, Form::Ref { address: addr })
                }
                KindIn::Zero { len } => (len, Form::Zero),
                KindIn::Inline { len, .. } => {
                    inline.insert(id, e.payload.clone().expect("an inline entry's payload"));
                    (len, Form::Inline)
                }
            };
            chain.spans.push(SpanInfo {
                id,
                len,
                form,
                entry_page: e.page,
                entry_epoch: e.epoch,
                entry_len: e.len,
            });
            chain.len += len as u64;
            id = e.next;
        }
    }

    // The data pages the chains show, checked.
    let mut buf = new_page();
    for &p in data.keys() {
        storage.read_at(&mut buf[..], p as u64 * PAGE_SIZE as u64)?;
        let d = decode_page(&buf, false)
            .map_err(|_| corrupt(format!("data page {p} fails its checks")))?;
        if d.kind != KIND_DATA || d.epoch > header.epoch {
            return Err(corrupt(format!(
                "page {p} is not a data page of the header's world"
            )));
        }
    }
    for chain in &chains {
        for s in &chain.spans {
            if let Form::Ref { address } = s.form {
                let offset = address % PAGE_SIZE as u64;
                if offset + s.len as u64 > (PAGE_SIZE - 4) as u64 {
                    return Err(corrupt(format!("span {} reaches past its page", s.id)));
                }
            }
        }
    }

    // The live entries of each table page.
    let mut tables = Vec::with_capacity(page_entries.len());
    for (p, epoch, mut stats, ids) in page_entries {
        for id in ids {
            let e = &newest[&id];
            if e.page == p && reached.contains(&id) {
                stats.live_entries += 1;
                stats.live_entry_bytes += e.len as u32;
                if let KindIn::Inline { len, .. } = e.kind {
                    stats.live_inline_bytes += len;
                }
            }
        }
        tables.push((p, epoch, stats));
    }
    let journal = recover(storage, header.epoch + 1, header.journal_pointer)
        .map(|r| r.pages.into_iter().chain(r.payload_pages).collect())
        .unwrap_or_default();
    Ok(World {
        tables,
        chains,
        inline,
        data,
        journal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemoryStorage, Store, WriteBackend};

    #[test]
    fn every_page_of_an_edited_file_is_accounted_for() {
        let storage = MemoryStorage::new();
        let store = Store::create(Box::new(storage.clone()), Default::default()).unwrap();
        store.set_schema(b"schema".to_vec());
        for i in 0..200u64 {
            store
                .splice(i * 37 % (i * 50 + 1), 0, &[i as u8; 50])
                .unwrap();
            if i % 20 == 19 {
                store.flush().unwrap();
            }
        }
        store.flush().unwrap();
        let expected = store.read_all().unwrap();
        drop(store);

        let i = Inspection::read(&storage).unwrap();
        assert_eq!(i.chain_content(&storage, 0).unwrap(), expected);
        assert_eq!(i.chain_content(&storage, 1).unwrap(), b"schema");
        assert_eq!(i.pages.len() as u64 * PAGE_SIZE as u64, i.file_len);
        let data_live: u64 = i
            .pages
            .iter()
            .filter(|p| p.role == Role::Data)
            .map(|p| p.live_bytes as u64)
            .sum();
        let refs: u64 = i
            .chains
            .iter()
            .flat_map(|c| &c.spans)
            .filter(|s| matches!(s.form, Form::Ref { .. }))
            .map(|s| s.len as u64)
            .sum();
        assert_eq!(data_live, refs);
        let live_entries: u32 = i
            .pages
            .iter()
            .filter_map(|p| p.table)
            .map(|t| t.live_entries)
            .sum();
        let spans: usize = i.chains.iter().map(|c| c.spans.len()).sum();
        assert_eq!(live_entries as usize, spans);
        assert!(i.other_header.is_some());
        assert_eq!(i.journal.transactions, 0);
    }
}
