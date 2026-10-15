//! The journal: records, their encoding, the buffer that holds them between
//! flushes, transactions, the chain of journal pages, the CRC chain, and
//! recovery.
//!
//! The records act on positions of the one sequence: `Write`, `Splice`,
//! `Move`, and `Paste`, which inserts what a `Write` or `Splice` earlier in
//! the same journal cut out. On file each position is written as its
//! distance from where the record before it left off, and the record's kind
//! rides in the low two bits of that varint.
//!
//! In memory, a `Paste` names its cut by the index of the record that made
//! it, and holds its value's encoding as well: if the record that made the
//! cut belongs to a journal already folded when the paste is appended, the
//! cut has ended, and the paste is written as a `Splice` of those bytes
//! instead. The encoder and the fold apply the same rule, so they agree on
//! every record; on file, a `Paste` names its cut by how many cuts of the
//! journal were made after it.

use std::collections::{BTreeMap, HashMap, VecDeque};

use crate::consts::{CONTENT_OFFSET, JOURNAL_STREAM, KIND_DATA, MAX_PAGE_CONTENT, PAGE_SIZE};
use crate::crc::Crc32c;
use crate::error::{corrupt, Error};
use crate::page::{decode_page, new_page, PageBuf};
use crate::storage::Storage;

/// The escape that ends a journal with a seal.
const ESCAPE_SEAL: u64 = 0;
/// The escape that says the next record's bytes are external: in a payload
/// page, named by length, address and checksum.
const ESCAPE_EXTERNAL: u64 = 1;

/// A byte range of a [`Records`] arena, at a position counted from the start
/// of the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bytes {
    pub start: u64,
    pub len: u32,
}

/// One record: a byte-level effect on the sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Record {
    /// Overwrites `[pos, pos + bytes.len)`; what it overwrites is a cut of
    /// its journal.
    Write { pos: u64, bytes: Bytes },
    /// Replaces the `removed` bytes at `pos` with `bytes`; what it removes
    /// is a cut of its journal, if anything.
    Splice {
        pos: u64,
        removed: u64,
        bytes: Bytes,
    },
    /// Cuts `[from, from + len)` and inserts it at `to`, a position in the
    /// sequence without the cut bytes.
    Move { from: u64, len: u64, to: u64 },
    /// Replaces the `removed` bytes at `pos` with the cut that the record
    /// numbered `cut` made; the cut is `len` bytes long. `fallback` holds
    /// those bytes' encoding, which is what the paste writes if that cut has
    /// ended by the time the paste is appended; a paste recovered from the
    /// journal has none, since its cut is in the journal with it.
    Paste {
        pos: u64,
        removed: u64,
        cut: u64,
        len: u64,
        fallback: Option<Bytes>,
    },
}

impl Record {
    /// The kind, as the low two bits of the head encode it, for a record
    /// written as it is.
    fn kind(&self) -> u64 {
        match self {
            Record::Write { .. } => 0,
            Record::Splice { .. } => 1,
            Record::Move { .. } => 2,
            Record::Paste { .. } => 3,
        }
    }

    /// The record as written to a journal whose first record is numbered
    /// `first`: a `Paste` whose cut a record before `first` made has
    /// ended, and becomes a `Write` or `Splice` of its fallback bytes.
    pub fn as_written(&self, first: u64) -> Record {
        match *self {
            Record::Paste {
                pos,
                removed,
                cut,
                len,
                fallback: Some(bytes),
            } if cut < first => {
                debug_assert_eq!(bytes.len as u64, len);
                if removed == len {
                    Record::Write { pos, bytes }
                } else {
                    Record::Splice {
                        pos,
                        removed,
                        bytes,
                    }
                }
            }
            r => r,
        }
    }

    /// The length of the cut this record makes, as written: what a `Write`
    /// overwrites, or what a `Splice` removes, if anything.
    pub fn cut_len(&self) -> Option<u64> {
        match *self {
            Record::Write { bytes, .. } => Some(bytes.len as u64),
            Record::Splice { removed, .. } if removed > 0 => Some(removed),
            _ => None,
        }
    }

    /// The position the head's delta counts to.
    fn head_pos(&self) -> u64 {
        match *self {
            Record::Write { pos, .. } | Record::Splice { pos, .. } | Record::Paste { pos, .. } => {
                pos
            }
            Record::Move { from, .. } => from,
        }
    }

    /// Where the record leaves off: the cursor after it.
    pub fn end(&self) -> u64 {
        match *self {
            Record::Write { pos, bytes } | Record::Splice { pos, bytes, .. } => {
                pos + bytes.len as u64
            }
            Record::Move { len, to, .. } => to + len,
            Record::Paste { pos, len, .. } => pos + len,
        }
    }
}

/// A position in a [`Records`] buffer, counted from the start of the
/// session: the records before it, the arena bytes they stash, and the bytes
/// they encode to. Discarding a prefix of the buffer moves its front and
/// leaves every cursor into the rest valid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    /// Records before the position.
    pub at: u64,
    /// Arena bytes before the position.
    pub arena: u64,
    /// Encoded record bytes before the position, as their lengths were
    /// estimated when they were pushed.
    pub bytes: u64,
    /// Where the records before the position left off in the sequence, which
    /// the next record's length is estimated from.
    pub seq: u64,
}

/// The one buffer of every record since the last flush, with their payloads
/// in an arena beside them, and where each of the journal's transactions
/// starts among them.
///
/// Records are appended at the end and discarded from the front, so both the
/// records and the arena are deques, and positions count from the start of
/// the session: cursors, and the `Bytes` of a record, stay valid however much
/// of the front goes.
#[derive(Clone, Debug, Default)]
pub struct Records {
    records: VecDeque<Record>,
    arena: VecDeque<u8>,
    /// The record indices at which the journal's transactions start, for
    /// the records appended to it; ascending.
    starts: VecDeque<u64>,
    /// Runs of the arena that also lie in a payload page, by the arena
    /// position they start at (`spec/journal.md#external-bytes`). A run
    /// never reaches past the bytes of the record it belongs to.
    placed: BTreeMap<u64, Placement>,
    front: Cursor,
    end: Cursor,
}

/// Where a run of a record's bytes lies in a payload page: `len` bytes at
/// byte `offset` of page `page`, written in epoch `epoch`, whose CRC-32C is
/// `crc`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub len: u32,
    pub page: u32,
    pub offset: u16,
    pub crc: u32,
    pub epoch: u64,
}

impl Placement {
    /// The run's address in the file.
    pub fn address(&self) -> u64 {
        self.page as u64 * PAGE_SIZE as u64 + self.offset as u64
    }
}

impl Records {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    /// The position of the first record held.
    pub fn front(&self) -> Cursor {
        self.front
    }

    /// The position past the last record held.
    pub fn end(&self) -> Cursor {
        self.end
    }

    /// Stores `bytes` in the arena, as the payload of the record pushed next.
    pub fn stash(&mut self, bytes: &[u8]) -> Bytes {
        let start = self.front.arena + self.arena.len() as u64;
        self.arena.extend(bytes);
        Bytes {
            start,
            len: bytes.len() as u32,
        }
    }

    /// Appends `r`, whose payload, if any, [`stash`](Self::stash) stored.
    pub fn push(&mut self, r: Record) {
        self.end.bytes += estimated_len(&r, self.end.seq, self.end.at) as u64;
        self.end.at += 1;
        self.end.seq = r.end();
        self.end.arena = self.front.arena + self.arena.len() as u64;
        self.records.push_back(r);
    }

    /// Records that the arena's bytes from `start` on lie in a payload page
    /// as `p` says.
    pub fn place(&mut self, start: u64, p: Placement) {
        self.placed.insert(start, p);
    }

    /// The placed runs that overlap the arena's bytes `[start, start + len)`,
    /// in order, each with the arena position it starts at.
    pub fn placed_in(&self, start: u64, len: u64) -> impl Iterator<Item = (u64, Placement)> + '_ {
        let before = self
            .placed
            .range(..start)
            .next_back()
            .filter(|(&s, p)| s + p.len as u64 > start);
        before
            .into_iter()
            .chain(self.placed.range(start..start + len))
            .map(|(&s, &p)| (s, p))
    }

    /// Marks the record at `at` as the first of a journal transaction.
    pub fn mark_start(&mut self, at: u64) {
        if self.starts.back() != Some(&at) {
            debug_assert!(self.starts.back().is_none_or(|&s| s < at));
            self.starts.push_back(at);
        }
    }

    /// The records in `[from, to)`, each with whether a journal transaction
    /// starts at it, and its number in the session.
    pub fn range(&self, from: Cursor, to: Cursor) -> impl Iterator<Item = (bool, u64, &Record)> {
        let base = self.front.at;
        let starts = &self.starts;
        self.records
            .range((from.at - base) as usize..(to.at - base) as usize)
            .enumerate()
            .map(move |(i, r)| {
                let at = from.at + i as u64;
                (starts.binary_search(&at).is_ok(), at, r)
            })
    }

    /// The record numbered `at` in the session, if the buffer still holds
    /// it.
    pub fn get(&self, at: u64) -> Option<&Record> {
        let i = at.checked_sub(self.front.at)?;
        self.records.get(usize::try_from(i).ok()?)
    }

    /// Every record held.
    pub fn iter(&self) -> impl Iterator<Item = &Record> {
        self.records.iter()
    }

    /// The arena's bytes `[start, start + len)`, in at most two slices.
    pub fn slices(&self, start: u64, len: usize) -> (&[u8], &[u8]) {
        let i = (start - self.front.arena) as usize;
        let (a, b) = self.arena.as_slices();
        if i >= a.len() {
            (&b[i - a.len()..i - a.len() + len], &[])
        } else if i + len <= a.len() {
            (&a[i..i + len], &[])
        } else {
            (&a[i..], &b[..i + len - a.len()])
        }
    }

    /// Copies the arena's bytes from position `start` into `out`.
    pub fn copy_bytes(&self, start: u64, out: &mut [u8]) {
        let (a, b) = self.slices(start, out.len());
        out[..a.len()].copy_from_slice(a);
        out[a.len()..].copy_from_slice(b);
    }

    /// Appends the bytes of `b` to `out`.
    pub fn extend_bytes(&self, b: Bytes, out: &mut Vec<u8>) {
        let (x, y) = self.slices(b.start, b.len as usize);
        out.extend_from_slice(x);
        out.extend_from_slice(y);
    }

    /// Encodes the records in `[from, to)` as a transaction's record bytes,
    /// their positions relative to `cursor`, which ends where they leave
    /// off, for the journal whose cuts so far `cuts` holds.
    pub fn encode(
        &self,
        from: Cursor,
        to: Cursor,
        cursor: &mut u64,
        cuts: &mut JournalCuts,
        out: &mut Vec<u8>,
    ) {
        for (_, at, r) in self.range(from, to) {
            let w = r.as_written(cuts.first);
            let distance = match w {
                Record::Paste { cut, .. } => cuts.distance(cut),
                _ => 0,
            };
            self.encode_written(&w, *cursor, distance, out);
            if w.cut_len().is_some() {
                cuts.made.push(at);
            }
            *cursor = w.end();
        }
    }

    /// Encodes `r`, as written, after a record that left off at `cursor`: a
    /// `Write` or `Splice` whose bytes are partly or wholly placed becomes
    /// one record per run, external where the run is placed, the first
    /// removing what `r` removes and the rest removing nothing, so that it
    /// still makes one cut.
    fn encode_written(&self, r: &Record, cursor: u64, distance: u32, out: &mut Vec<u8>) {
        let (pos, removed, bytes, is_write) = match *r {
            Record::Write { pos, bytes } => (pos, bytes.len as u64, bytes, true),
            Record::Splice {
                pos,
                removed,
                bytes,
            } => (pos, removed, bytes, false),
            _ => return encode_record(r, cursor, distance, self, out),
        };
        let (start, len) = (bytes.start, bytes.len as u64);
        let mut parts: Vec<(u64, u64, Option<Placement>)> = Vec::new();
        let mut at = start;
        for (s, p) in self.placed_in(start, len) {
            debug_assert!(s >= start && s + p.len as u64 <= start + len);
            if s > at {
                parts.push((at, s - at, None));
            }
            parts.push((s, p.len as u64, Some(p)));
            at = s + p.len as u64;
        }
        if parts.is_empty() {
            return encode_record(r, cursor, distance, self, out);
        }
        if at < start + len {
            parts.push((at, start + len - at, None));
        }
        let mut cur = cursor;
        let mut here = pos;
        for (i, &(s, n, placed)) in parts.iter().enumerate() {
            let b = Bytes {
                start: s,
                len: n as u32,
            };
            let part = match (i, is_write && parts.len() == 1) {
                (0, true) => Record::Write {
                    pos: here,
                    bytes: b,
                },
                (0, false) => Record::Splice {
                    pos: here,
                    removed,
                    bytes: b,
                },
                _ => Record::Splice {
                    pos: here,
                    removed: 0,
                    bytes: b,
                },
            };
            match placed {
                None => encode_record(&part, cur, 0, self, out),
                Some(p) => {
                    out.extend_from_slice(&[0x00, 0x00]);
                    put(out, ESCAPE_EXTERNAL);
                    put(out, head(&part, cur));
                    if let Record::Splice { removed, .. } = part {
                        put(out, removed);
                    }
                    put(out, n);
                    put(out, p.address());
                    out.extend_from_slice(&p.crc.to_le_bytes());
                }
            }
            here += n;
            cur = here;
        }
    }

    /// Discards the records from `to` on: the end moves back to `to`.
    pub fn truncate(&mut self, to: Cursor) {
        debug_assert!(self.front.at <= to.at && to.at <= self.end.at);
        self.records.truncate((to.at - self.front.at) as usize);
        self.arena.truncate((to.arena - self.front.arena) as usize);
        while self.starts.back().is_some_and(|&s| s >= to.at) {
            self.starts.pop_back();
        }
        self.placed.split_off(&to.arena);
        self.end = to;
    }

    /// Discards the records before `to`: the front moves up to `to`.
    pub fn discard_to(&mut self, to: Cursor) {
        debug_assert!(self.front.at <= to.at && to.at <= self.end.at);
        self.records.drain(..(to.at - self.front.at) as usize);
        self.arena.drain(..(to.arena - self.front.arena) as usize);
        while self.starts.front().is_some_and(|&s| s < to.at) {
            self.starts.pop_front();
        }
        self.placed = self.placed.split_off(&to.arena);
        self.front = to;
    }

    /// The bytes of `b`, copied.
    #[cfg(test)]
    pub fn to_vec(&self, b: Bytes) -> Vec<u8> {
        let mut out = Vec::new();
        self.extend_bytes(b, &mut out);
        out
    }
}

/// The cuts of the journal being appended to: which records are its own,
/// and which of them made a cut, in order, so that a `Paste` can be written
/// with how many cuts were made after its own.
#[derive(Clone, Debug, Default)]
pub struct JournalCuts {
    /// The number of the journal's first record: a cut that a record before
    /// it made has ended.
    pub first: u64,
    /// The numbers of the records appended to the journal that made a cut,
    /// ascending.
    pub made: Vec<u64>,
}

impl JournalCuts {
    /// The cuts of an empty journal whose first record is numbered `first`.
    pub fn starting_at(first: u64) -> Self {
        JournalCuts {
            first,
            made: Vec::new(),
        }
    }

    /// How many of the journal's cuts were made after the one record `cut`
    /// made.
    fn distance(&self, cut: u64) -> u32 {
        let i = self
            .made
            .binary_search(&cut)
            .expect("a paste is appended to the journal of its cut");
        (self.made.len() - 1 - i) as u32
    }
}

fn zz(v: i64) -> u64 {
    kladde_varint::zigzag(v)
}

/// The head of `r`, written after a record that left off at `cursor`.
fn head(r: &Record, cursor: u64) -> u64 {
    let delta = r.head_pos() as i64 - cursor as i64;
    zz((delta << 2) | r.kind() as i64)
}

/// The bytes the encoding of `r` takes after a record that left off at
/// `cursor`, if a `Paste` among them names its cut `distance` cuts back,
/// computed without encoding it.
pub fn record_len(r: &Record, cursor: u64, distance: u32) -> usize {
    let v = |x: u64| kladde_varint::encoded_len(x);
    v(head(r, cursor))
        + match *r {
            Record::Write { bytes, .. } => v(bytes.len as u64) + bytes.len as usize,
            Record::Splice { removed, bytes, .. } => {
                v(removed) + v(bytes.len as u64) + bytes.len as usize
            }
            Record::Move { from, len, to } => v(len) + v(zz(to as i64 - from as i64)),
            Record::Paste { removed, .. } => v(removed) + v(distance as u64),
        }
}

/// What the encoding of the record numbered `at` is estimated to take when
/// it is pushed: a `Paste` as a paste, its distance at most the number of
/// records since its cut's. The encoding itself can differ, if the paste's
/// cut ends before it is appended; the estimates only steer when to append
/// and flush.
fn estimated_len(r: &Record, cursor: u64, at: u64) -> usize {
    let distance = match *r {
        Record::Paste { cut, .. } => u32::try_from(at.saturating_sub(cut)).unwrap_or(u32::MAX),
        _ => 0,
    };
    record_len(r, cursor, distance)
}

/// The stream bytes a transaction of `n` record bytes takes: its length
/// prefix, its records, and its `crc`.
pub fn framed_len(n: u64) -> u64 {
    kladde_varint::encoded_len(n) as u64 + n + 4
}

fn put(out: &mut Vec<u8>, v: u64) {
    kladde_varint::encode(v, out);
}

fn encode_record(r: &Record, cursor: u64, distance: u32, recs: &Records, out: &mut Vec<u8>) {
    put(out, head(r, cursor));
    match *r {
        Record::Write { bytes, .. } => {
            put(out, bytes.len as u64);
            recs.extend_bytes(bytes, out);
        }
        Record::Splice { removed, bytes, .. } => {
            put(out, removed);
            put(out, bytes.len as u64);
            recs.extend_bytes(bytes, out);
        }
        Record::Move { from, len, to } => {
            put(out, len);
            put(out, zz(to as i64 - from as i64));
        }
        Record::Paste { removed, .. } => {
            put(out, removed);
            put(out, distance as u64);
        }
    }
}

use crate::consts::MAX_SEQUENCE;

/// The cuts of a journal as recovery decodes it: for each, the number of
/// the record that made it, its length, and whether it was pasted, oldest
/// first.
pub type DecodedCuts = Vec<(u64, u64, bool)>;

/// The bytes of an external reference as recovery reads them: its address,
/// length and checksum in, the bytes and where they lie out, or `None` if
/// the page does not hold them (`spec/journal.md#recovery`).
pub type Fetch<'a> = dyn FnMut(u64, u32, u32) -> Result<Option<(Vec<u8>, Placement)>, Error> + 'a;

/// Decodes the records of one transaction in `input` into `into`, their
/// positions relative to `cursor`, which ends where they leave off, adding
/// the cuts they make to the journal's `cuts`; checks the bounds and the
/// cuts a `Paste` names, but not whether the positions lie inside the
/// sequence, which only the fold knows. External bytes come from `fetch`.
///
/// Returns `false`, having taken nothing of the transaction into `into`,
/// `cursor` or `cuts`, if `fetch` finds an external reference's bytes
/// missing: the transaction belongs to the torn tail.
pub fn decode_records(
    input: &[u8],
    cursor: &mut u64,
    cuts: &mut DecodedCuts,
    into: &mut Records,
    fetch: &mut Fetch,
) -> Result<bool, Error> {
    let (mark, cursor0, cuts0) = (into.end(), *cursor, cuts.len());
    let mut pasted_now: Vec<usize> = Vec::new();
    match decode_into(input, cursor, cuts, into, fetch, &mut pasted_now)? {
        true => Ok(true),
        false => {
            into.truncate(mark);
            *cursor = cursor0;
            cuts.truncate(cuts0);
            for i in pasted_now {
                if i < cuts.len() {
                    cuts[i].2 = false;
                }
            }
            Ok(false)
        }
    }
}

fn decode_into(
    input: &[u8],
    cursor: &mut u64,
    cuts: &mut DecodedCuts,
    into: &mut Records,
    fetch: &mut Fetch,
    pasted_now: &mut Vec<usize>,
) -> Result<bool, Error> {
    let mut rest = input;
    let get = |rest: &mut &[u8]| -> Result<u64, Error> {
        let (v, r) =
            kladde_varint::decode(rest).map_err(|_| corrupt("a journal record is truncated"))?;
        *rest = r;
        Ok(v)
    };
    let bounded = |v: u64| -> Result<u64, Error> {
        if v >= MAX_SEQUENCE {
            Err(corrupt("a journal record reaches past 2^48 bytes"))
        } else {
            Ok(v)
        }
    };
    while !rest.is_empty() {
        let at = into.end().at;
        let mut x = kladde_varint::unzigzag(get(&mut rest)?);
        let mut external = false;
        if x == 0 && rest.first() == Some(&0) {
            // A `Write` of no bytes at delta 0: the escape.
            rest = &rest[1..];
            match get(&mut rest)? {
                ESCAPE_EXTERNAL => {}
                ESCAPE_SEAL => return Err(corrupt("a seal shares its transaction")),
                _ => return Err(corrupt("a journal record uses an unknown escape")),
            }
            external = true;
            x = kladde_varint::unzigzag(get(&mut rest)?);
            if x & 3 > 1 {
                return Err(corrupt(
                    "an escape for external bytes precedes no write or splice",
                ));
            }
        }
        let kind = x & 3;
        let pos = *cursor as i64 + (x >> 2);
        if pos < 0 {
            return Err(corrupt("a journal record names a negative position"));
        }
        let pos = bounded(pos as u64)?;
        let rec = match kind {
            0 | 1 => {
                let removed = if kind == 1 {
                    bounded(get(&mut rest)?)?
                } else {
                    0
                };
                let len = get(&mut rest)?;
                let bytes = if external {
                    let address = get(&mut rest)?;
                    let Some(crc) = rest.get(..4) else {
                        return Err(corrupt("a journal record is truncated"));
                    };
                    let crc = u32::from_le_bytes(crc.try_into().unwrap());
                    rest = &rest[4..];
                    let offset = address % PAGE_SIZE as u64;
                    if len == 0
                        || len > MAX_PAGE_CONTENT as u64
                        || address / (PAGE_SIZE as u64) < 2
                        || address / (PAGE_SIZE as u64) >= u32::MAX as u64
                        || offset < CONTENT_OFFSET as u64
                        || offset + len > (PAGE_SIZE - 4) as u64
                    {
                        return Err(corrupt("an external reference lies outside a data page"));
                    }
                    let Some((payload, placement)) = fetch(address, len as u32, crc)? else {
                        return Ok(false);
                    };
                    let b = into.stash(&payload);
                    into.place(b.start, placement);
                    b
                } else {
                    if (rest.len() as u64) < len {
                        return Err(corrupt("a journal record's bytes are truncated"));
                    }
                    let b = into.stash(&rest[..len as usize]);
                    rest = &rest[len as usize..];
                    b
                };
                if kind == 0 {
                    if len == 0 {
                        return Err(corrupt("a journal record uses the reserved escape"));
                    }
                    cuts.push((at, len, false));
                    Record::Write { pos, bytes }
                } else {
                    if removed > 0 {
                        cuts.push((at, removed, false));
                    }
                    Record::Splice {
                        pos,
                        removed,
                        bytes,
                    }
                }
            }
            2 => {
                let len = bounded(get(&mut rest)?)?;
                let to = pos as i64 + kladde_varint::unzigzag(get(&mut rest)?);
                if to < 0 {
                    return Err(corrupt("a move names a negative position"));
                }
                Record::Move {
                    from: pos,
                    len,
                    to: bounded(to as u64)?,
                }
            }
            _ => {
                let removed = bounded(get(&mut rest)?)?;
                let cut = get(&mut rest)?;
                if cut >= cuts.len() as u64 {
                    return Err(corrupt("a paste names a cut its journal did not make"));
                }
                let i = cuts.len() - 1 - cut as usize;
                if cuts[i].2 {
                    return Err(corrupt("a paste names a cut already pasted"));
                }
                cuts[i].2 = true;
                pasted_now.push(i);
                Record::Paste {
                    pos,
                    removed,
                    cut: cuts[i].0,
                    len: cuts[i].1,
                    fallback: None,
                }
            }
        };
        *cursor = rec.end();
        into.push(rec);
    }
    Ok(true)
}

/// The records of a seal's transaction: the escape (a `Write` of no bytes
/// at delta 0), escape 0, and the page where the next epoch's journal
/// begins (`spec/journal.md#seals`).
pub fn seal_payload(page: u32) -> Vec<u8> {
    let mut out = vec![0x00, 0x00, 0x00];
    kladde_varint::encode(page as u64, &mut out);
    out
}

/// The page a transaction's records name if they are a seal.
fn parse_seal(payload: &[u8]) -> Option<u32> {
    let rest = payload.strip_prefix(&[0x00, 0x00, 0x00])?;
    let (page, rest) = kladde_varint::decode(rest).ok()?;
    (rest.is_empty() && page <= u32::MAX as u64).then_some(page as u32)
}

/// The CRC chain of the journal, seeded with its epoch.
fn seeded_chain(epoch: u64) -> Crc32c {
    let mut c = Crc32c::new();
    c.update(&epoch.to_le_bytes());
    c
}

/// Appends transactions to the journal's chain of pages.
pub struct JournalWriter {
    /// The journal pages in chain order; the first is the header's
    /// `journal_pointer`.
    pub pages: Vec<u32>,
    buf: Box<PageBuf>,
    pos: usize,
    dirty_from: usize,
    chain: Crc32c,
    /// Where the last record appended left off in the sequence, which the
    /// next record's positions are written relative to.
    pub cursor: u64,
    /// Stream bytes appended so far.
    pub bytes: u64,
    /// Transactions appended so far.
    pub transactions: u64,
    /// The journal of an earlier session, which nothing may be appended to:
    /// it names the pages that the flush folding it retires, and counts as
    /// full, so that the flush rules fold it before anything is appended.
    recovered: bool,
    /// The journal's epoch, which its payload pages' framing carries.
    pub epoch: u64,
    /// The payload pages its transactions name, in the order they were
    /// taken.
    pub payload: Vec<u32>,
    /// The last payload page, while it has room: its buffer, and the
    /// content bytes it holds.
    pub open: Option<(u32, Box<PageBuf>, usize)>,
}

/// What a journal transaction may be off by from the estimate its records'
/// lengths give, since the cursor the estimate assumed may have been reset
/// by a flush in between: a head's delta, at most, grows by this much.
const ESTIMATE_SLACK: u64 = 16;

impl JournalWriter {
    /// An empty journal of epoch `epoch`, starting at `first_page`.
    pub fn new(epoch: u64, first_page: u32) -> Self {
        JournalWriter {
            pages: vec![first_page],
            buf: new_page(),
            pos: 0,
            dirty_from: 0,
            chain: seeded_chain(epoch),
            cursor: 0,
            bytes: 0,
            transactions: 0,
            recovered: false,
            epoch,
            payload: Vec::new(),
            open: None,
        }
    }

    /// The non-empty journal that recovery found: `transactions` complete
    /// transactions in `pages`, naming the payload pages `payload`.
    pub fn recovered(epoch: u64, pages: Vec<u32>, payload: Vec<u32>, transactions: u64) -> Self {
        JournalWriter {
            pages,
            payload,
            transactions,
            recovered: true,
            ..JournalWriter::new(epoch, 0)
        }
    }

    /// What the journal holds against its budget: its stream bytes, and a
    /// page's worth of stream for each payload page.
    fn used(&self) -> u64 {
        self.bytes + self.payload.len() as u64 * JOURNAL_STREAM as u64
    }

    /// What the journal holds, in pages' worth of stream: for an idle flush's
    /// check against the budget.
    pub fn held(&self) -> u64 {
        self.used()
    }

    /// Whether this is the journal of an earlier session, which nothing may
    /// be appended to.
    pub fn is_recovered(&self) -> bool {
        self.recovered
    }

    /// Whether no transaction has been appended.
    pub fn is_empty(&self) -> bool {
        self.transactions == 0
    }

    /// Whether a transaction of about `n` record bytes, appended now, would
    /// keep the journal within `budget` pages. The next page is taken only
    /// once the stream continues past the current one, so a stream that
    /// ends within `budget` pages' worth of stream bytes takes at most
    /// `budget` pages.
    pub fn fits(&self, n: u64, budget: u32) -> bool {
        !self.recovered
            && self.used() + framed_len(n) + ESTIMATE_SLACK <= budget as u64 * JOURNAL_STREAM as u64
    }

    /// Whether the journal, with its payload pages, has outgrown `budget`
    /// pages, which only a transaction too large for them on its own makes
    /// it do.
    pub fn over_budget(&self, budget: u32) -> bool {
        self.used() > budget as u64 * JOURNAL_STREAM as u64
    }

    /// The page being appended to.
    pub fn current_page(&self) -> u32 {
        *self.pages.last().unwrap()
    }

    /// Appends one transaction whose record bytes are `payload`. `next_page`
    /// supplies a page, existing in the file, whenever the stream continues
    /// past the current one; the current page must exist too.
    pub fn append(
        &mut self,
        payload: &[u8],
        storage: &mut dyn Storage,
        next_page: &mut dyn FnMut(&mut dyn Storage) -> Result<u32, Error>,
    ) -> Result<(), Error> {
        debug_assert!(!self.recovered, "appending to a recovered journal");
        if payload.len() as u64 > u32::MAX as u64 {
            return Err(Error::OutOfBounds);
        }
        let mut len = Vec::with_capacity(5);
        kladde_varint::encode(payload.len() as u64, &mut len);
        self.stream(&len, true, storage, next_page)?;
        self.stream(payload, true, storage, next_page)?;
        let crc = self.chain.value();
        self.stream(&crc.to_le_bytes(), false, storage, next_page)?;
        self.write_range(storage, self.dirty_from, self.pos)?;
        self.dirty_from = self.pos;
        self.bytes += (len.len() + payload.len() + 4) as u64;
        self.transactions += 1;
        Ok(())
    }

    fn stream(
        &mut self,
        mut bytes: &[u8],
        feed: bool,
        storage: &mut dyn Storage,
        next_page: &mut dyn FnMut(&mut dyn Storage) -> Result<u32, Error>,
    ) -> Result<(), Error> {
        while !bytes.is_empty() {
            if self.pos == JOURNAL_STREAM {
                let next = next_page(storage)?;
                self.buf[JOURNAL_STREAM..JOURNAL_STREAM + 4].copy_from_slice(&next.to_le_bytes());
                self.chain.update(&next.to_le_bytes());
                let trailer = self.chain.value();
                self.buf[JOURNAL_STREAM + 4..].copy_from_slice(&trailer.to_le_bytes());
                self.write_range(storage, self.dirty_from, PAGE_SIZE)?;
                self.pages.push(next);
                self.pos = 0;
                self.dirty_from = 0;
            }
            let k = bytes.len().min(JOURNAL_STREAM - self.pos);
            self.buf[self.pos..self.pos + k].copy_from_slice(&bytes[..k]);
            if feed {
                self.chain.update(&bytes[..k]);
            }
            self.pos += k;
            bytes = &bytes[k..];
        }
        Ok(())
    }

    fn write_range(&self, storage: &mut dyn Storage, from: usize, to: usize) -> Result<(), Error> {
        if from < to {
            let page = self.current_page() as u64;
            storage.write_at(&self.buf[from..to], page * PAGE_SIZE as u64 + from as u64)?;
        }
        Ok(())
    }
}

/// What recovery found in the journal.
#[derive(Debug, Default)]
pub struct Recovered {
    /// The records of the complete transactions, in order, each
    /// transaction's start marked.
    pub records: Records,
    /// How many complete transactions there were, seals included.
    pub transactions: usize,
    /// How many of them were seals.
    pub seals: usize,
    /// The stream bytes the complete transactions take, framing included.
    pub stream_bytes: u64,
    /// The pages the valid part of the journal occupies.
    pub pages: Vec<u32>,
    /// The payload pages its complete transactions name, in the order they
    /// were first named.
    pub payload_pages: Vec<u32>,
}

/// A payload page as recovery read it: its bytes, the epoch its framing
/// carries, and where its content lies.
type ReadPage = (Box<PageBuf>, u64, std::ops::Range<usize>);

/// The payload pages recovery has read, each `None` if its framing failed,
/// it is no data page, or it lies past the end of the file.
struct PayloadReader<'a> {
    storage: &'a dyn Storage,
    file_pages: u64,
    pages: HashMap<u32, Option<ReadPage>>,
}

impl PayloadReader<'_> {
    /// The `len` bytes at `address`, if a `Data` page of epoch `epoch`
    /// holds them in its content and their CRC-32C is `crc`.
    fn fetch(
        &mut self,
        epoch: u64,
        address: u64,
        len: u32,
        crc: u32,
    ) -> Result<Option<(Vec<u8>, Placement)>, Error> {
        let page = (address / PAGE_SIZE as u64) as u32;
        let offset = (address % PAGE_SIZE as u64) as usize;
        if !self.pages.contains_key(&page) {
            let read = if (page as u64) < self.file_pages {
                let mut buf = new_page();
                self.storage
                    .read_at(&mut buf[..], page as u64 * PAGE_SIZE as u64)?;
                match decode_page(&buf, false) {
                    Ok(d) if d.kind == KIND_DATA => Some((buf, d.epoch, d.content)),
                    _ => None,
                }
            } else {
                None
            };
            self.pages.insert(page, read);
        }
        let Some((buf, page_epoch, content)) = &self.pages[&page] else {
            return Ok(None);
        };
        let end = offset + len as usize;
        if *page_epoch != epoch || offset < content.start || end > content.end {
            return Ok(None);
        }
        let bytes = &buf[offset..end];
        let mut c = Crc32c::new();
        c.update(bytes);
        if c.value() != crc {
            return Ok(None);
        }
        Ok(Some((
            bytes.to_vec(),
            Placement {
                len,
                page,
                offset: offset as u16,
                crc,
                epoch,
            },
        )))
    }
}

struct StreamReader<'a> {
    storage: &'a dyn Storage,
    file_pages: u64,
    buf: Box<PageBuf>,
    pos: usize,
    chain: Crc32c,
    pages: Vec<u32>,
}

impl StreamReader<'_> {
    /// Reads `out.len()` stream bytes, following trailers. `false` means the
    /// stream ended before that: a trailer failed its check or pointed
    /// nowhere valid.
    fn read(&mut self, out: &mut [u8], feed: bool) -> Result<bool, Error> {
        let mut done = 0;
        while done < out.len() {
            if self.pos == JOURNAL_STREAM {
                let next_bytes: [u8; 4] = self.buf[JOURNAL_STREAM..JOURNAL_STREAM + 4]
                    .try_into()
                    .unwrap();
                let stored = u32::from_le_bytes(self.buf[JOURNAL_STREAM + 4..].try_into().unwrap());
                let mut c = self.chain;
                c.update(&next_bytes);
                if c.value() != stored {
                    return Ok(false);
                }
                let next = u32::from_le_bytes(next_bytes);
                if next < 2 || next as u64 >= self.file_pages || self.pages.contains(&next) {
                    return Ok(false);
                }
                self.chain = c;
                self.storage
                    .read_at(&mut self.buf[..], next as u64 * PAGE_SIZE as u64)?;
                self.pages.push(next);
                self.pos = 0;
            }
            let k = (out.len() - done).min(JOURNAL_STREAM - self.pos);
            out[done..done + k].copy_from_slice(&self.buf[self.pos..self.pos + k]);
            if feed {
                self.chain.update(&out[done..done + k]);
            }
            self.pos += k;
            done += k;
        }
        Ok(true)
    }
}

/// Reads the valid prefix of the journal of epoch `epoch` starting at
/// `first_page`, and of the journals of later epochs that seals lead to.
pub fn recover(storage: &dyn Storage, epoch: u64, first_page: u32) -> Result<Recovered, Error> {
    let file_pages = storage.len()? / PAGE_SIZE as u64;
    let mut out = Recovered::default();
    if first_page < 2 || first_page as u64 >= file_pages {
        return Ok(out);
    }
    let mut r = StreamReader {
        storage,
        file_pages,
        buf: new_page(),
        pos: 0,
        chain: seeded_chain(epoch),
        pages: vec![first_page],
    };
    storage.read_at(&mut r.buf[..], first_page as u64 * PAGE_SIZE as u64)?;
    let mut epoch = epoch;
    // The pages of the journals that seals ended, before the current one.
    let mut sealed_pages: Vec<u32> = Vec::new();
    let mut valid_pages = 1;
    let mut cursor = 0u64;
    let mut cuts = DecodedCuts::new();
    let mut payloads = PayloadReader {
        storage,
        file_pages,
        pages: HashMap::new(),
    };
    let mut seen_payload = std::collections::HashSet::new();
    let mut payload_mark = 0u64;
    'transactions: loop {
        // The length prefix, one byte at a time.
        let mut len: u64 = 0;
        let mut shift = 0;
        loop {
            let mut b = [0u8];
            if !r.read(&mut b, true)? || shift > 28 {
                break 'transactions;
            }
            len |= ((b[0] & 0x7f) as u64) << shift;
            if b[0] & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        if len > u32::MAX as u64 {
            break;
        }
        // The records, read in bounded pieces so that stale bytes claiming a
        // huge length cost nothing before their trailer fails.
        let mut payload = Vec::with_capacity((len as usize).min(1 << 20));
        let mut left = len as usize;
        while left > 0 {
            let k = left.min(1 << 16);
            let start = payload.len();
            payload.resize(start + k, 0);
            if !r.read(&mut payload[start..], true)? {
                break 'transactions;
            }
            left -= k;
        }
        let expected = r.chain.value();
        let mut crc = [0u8; 4];
        if !r.read(&mut crc, false)? || u32::from_le_bytes(crc) != expected {
            break;
        }
        if let Some(next) = parse_seal(&payload) {
            out.transactions += 1;
            out.stream_bytes += framed_len(len);
            out.seals += 1;
            // The journal ends here, and the next epoch's begins at `next`,
            // with a chain, a cursor and cuts of its own.
            sealed_pages.extend_from_slice(&r.pages);
            if next < 2 || next as u64 >= file_pages || sealed_pages.contains(&next) {
                valid_pages = 0;
                r.pages.clear();
                break;
            }
            epoch += 1;
            r.chain = seeded_chain(epoch);
            r.pages = vec![next];
            r.pos = 0;
            storage.read_at(&mut r.buf[..], next as u64 * PAGE_SIZE as u64)?;
            valid_pages = 1;
            cursor = 0;
            cuts = DecodedCuts::new();
            continue;
        }
        let at = out.records.end().at;
        let mut fetch = |address, len, crc| payloads.fetch(epoch, address, len, crc);
        if !decode_records(
            &payload,
            &mut cursor,
            &mut cuts,
            &mut out.records,
            &mut fetch,
        )? {
            // A payload page's write did not reach the disk: the
            // transaction never happened.
            break;
        }
        out.transactions += 1;
        out.stream_bytes += framed_len(len);
        valid_pages = r.pages.len();
        out.records.mark_start(at);
        let end = out.records.end().arena;
        for (_, p) in out.records.placed_in(payload_mark, end - payload_mark) {
            if seen_payload.insert(p.page) {
                out.payload_pages.push(p.page);
            }
        }
        payload_mark = out.records.end().arena;
    }
    out.pages = sealed_pages;
    out.pages.extend_from_slice(&r.pages[..valid_pages]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryStorage;

    /// Decodes a transaction that names no payload page.
    fn decode_inline(
        input: &[u8],
        cursor: &mut u64,
        cuts: &mut DecodedCuts,
        into: &mut Records,
    ) -> Result<bool, Error> {
        decode_records(input, cursor, cuts, into, &mut |_, _, _| {
            Err(corrupt("no payload pages here"))
        })
    }

    fn setup(pages: u64) -> MemoryStorage {
        let mut s = MemoryStorage::new();
        s.set_len(pages * PAGE_SIZE as u64).unwrap();
        s
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self, bits: u32) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % (1u64 << bits.min(48))
        }
    }

    /// A transaction of random records, with valid cuts and pastes; `cuts`
    /// are the journal's, which a paste may name whichever transaction made
    /// them.
    fn transaction(rng: &mut Rng, recs: &mut Records, cuts: &mut DecodedCuts) {
        let start = recs.end().at;
        for _ in 0..rng.next(4) + 1 {
            let bits = rng.next(5) as u32 + 1;
            let at = recs.end().at;
            let r = match rng.next(3) {
                0 => {
                    let n = rng.next(8) as usize + 1;
                    let bytes = recs.stash(&vec![rng.next(8) as u8; n]);
                    cuts.push((at, n as u64, false));
                    Record::Write {
                        pos: rng.next(bits),
                        bytes,
                    }
                }
                1 => {
                    let n = rng.next(8) as usize;
                    let bytes = recs.stash(&vec![rng.next(8) as u8; n]);
                    let removed = rng.next(bits);
                    if removed > 0 {
                        cuts.push((at, removed, false));
                    }
                    Record::Splice {
                        pos: rng.next(bits),
                        removed,
                        bytes,
                    }
                }
                2 => Record::Move {
                    from: rng.next(bits),
                    len: rng.next(bits),
                    to: rng.next(bits),
                },
                _ => match cuts.iter().rposition(|c| !c.2) {
                    Some(i) => {
                        cuts[i].2 = true;
                        Record::Paste {
                            pos: rng.next(bits),
                            removed: rng.next(bits),
                            cut: cuts[i].0,
                            len: cuts[i].1,
                            fallback: None,
                        }
                    }
                    None => Record::Move {
                        from: 1,
                        len: 2,
                        to: 3,
                    },
                },
            };
            recs.push(r);
        }
        recs.mark_start(start);
    }

    /// Each record with its payload, for comparing buffers, the payload's
    /// position in the arena left out.
    fn contents(recs: &Records) -> Vec<(Record, Vec<u8>)> {
        recs.iter()
            .map(|&r| match r {
                Record::Write { pos, bytes } => (
                    Record::Write {
                        pos,
                        bytes: Bytes { start: 0, ..bytes },
                    },
                    recs.to_vec(bytes),
                ),
                Record::Splice {
                    pos,
                    removed,
                    bytes,
                } => (
                    Record::Splice {
                        pos,
                        removed,
                        bytes: Bytes { start: 0, ..bytes },
                    },
                    recs.to_vec(bytes),
                ),
                _ => (r, Vec::new()),
            })
            .collect()
    }

    #[test]
    fn records_round_trip_and_record_len_counts_the_encoding() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut recs = Records::new();
        let mut txs = Vec::new();
        let mut cuts = DecodedCuts::new();
        for _ in 0..500 {
            let from = recs.end();
            transaction(&mut rng, &mut recs, &mut cuts);
            txs.push((from, recs.end()));
        }
        let mut cursor = 0u64;
        let mut journal = JournalCuts::starting_at(0);
        let mut decoded = Records::new();
        let mut dcursor = 0u64;
        let mut dcuts = DecodedCuts::new();
        let mut total = 0u64;
        for &(from, to) in &txs {
            let mut out = Vec::new();
            let before = cursor;
            recs.encode(from, to, &mut cursor, &mut journal, &mut out);
            // The estimate assumed the same cursor, and a paste's distance at
            // most the records since its cut, so it never falls short.
            assert!(to.bytes - from.bytes >= out.len() as u64, "from {before}");
            total += out.len() as u64;
            let at = decoded.end().at;
            decode_inline(&out, &mut dcursor, &mut dcuts, &mut decoded).unwrap();
            decoded.mark_start(at);
        }
        assert!(total <= recs.end().bytes);
        assert_eq!(contents(&decoded), contents(&recs));
        assert_eq!(dcursor, cursor);
    }

    #[test]
    fn a_keystroke_takes_four_bytes() {
        let mut recs = Records::new();
        let bytes = recs.stash(b"a");
        let r = Record::Splice {
            pos: 1000,
            removed: 0,
            bytes,
        };
        assert_eq!(record_len(&r, 1000, 0), 4);
        assert_eq!(record_len(&r, 999, 0), 4);
        assert!(record_len(&r, 0, 0) > 4);
    }

    #[test]
    fn pastes_of_unknown_or_pasted_cuts_and_the_escape_are_refused() {
        let mut out = Vec::new();
        let refused = |out: &[u8]| {
            decode_inline(out, &mut 0, &mut DecodedCuts::new(), &mut Records::new()).is_err()
        };
        // A paste in a journal that cut nothing.
        put(&mut out, zz(3));
        put(&mut out, 0);
        put(&mut out, 0);
        assert!(refused(&out));
        // A write of no bytes.
        let mut out = Vec::new();
        put(&mut out, zz(0));
        put(&mut out, 0);
        assert!(refused(&out));
        // A cut pasted twice.
        let mut out = Vec::new();
        put(&mut out, zz(1));
        put(&mut out, 2);
        put(&mut out, 0);
        for _ in 0..2 {
            put(&mut out, zz(3));
            put(&mut out, 0);
            put(&mut out, 0);
        }
        assert!(refused(&out));
    }

    #[test]
    fn a_paste_names_a_cut_of_an_earlier_transaction_of_its_journal() {
        // A splice that removes two bytes, then, in another transaction, a
        // paste of the cut it made.
        let mut first = Vec::new();
        put(&mut first, zz(1));
        put(&mut first, 2);
        put(&mut first, 0);
        let mut second = Vec::new();
        put(&mut second, zz(3));
        put(&mut second, 0);
        put(&mut second, 0);
        let (mut cursor, mut cuts, mut recs) = (0, DecodedCuts::new(), Records::new());
        decode_inline(&first, &mut cursor, &mut cuts, &mut recs).unwrap();
        decode_inline(&second, &mut cursor, &mut cuts, &mut recs).unwrap();
        assert_eq!(
            recs.iter().nth(1),
            Some(&Record::Paste {
                pos: 0,
                removed: 0,
                cut: 0,
                len: 2,
                fallback: None,
            })
        );
    }

    #[test]
    fn a_paste_whose_cut_has_ended_is_written_as_its_fallback() {
        let mut recs = Records::new();
        let gone = recs.stash(b"");
        recs.push(Record::Splice {
            pos: 0,
            removed: 2,
            bytes: gone,
        });
        let fallback = recs.stash(b"xy");
        recs.push(Record::Paste {
            pos: 3,
            removed: 0,
            cut: 0,
            len: 2,
            fallback: Some(fallback),
        });
        let paste = recs.range(recs.front(), recs.end()).nth(1).unwrap().2;
        // In the journal of its cut, it is a paste; in the next, a splice.
        assert!(matches!(paste.as_written(0), Record::Paste { .. }));
        assert_eq!(
            paste.as_written(1),
            Record::Splice {
                pos: 3,
                removed: 0,
                bytes: fallback
            }
        );
    }

    #[test]
    fn a_write_makes_a_cut_of_its_length() {
        // A write of two bytes, then a paste of the cut it made.
        let mut out = Vec::new();
        put(&mut out, zz(0));
        put(&mut out, 2);
        out.extend_from_slice(b"xy");
        put(&mut out, zz(3 - (2 << 2)));
        put(&mut out, 0);
        put(&mut out, 0);
        let mut recs = Records::new();
        decode_inline(&out, &mut 0, &mut DecodedCuts::new(), &mut recs).unwrap();
        assert_eq!(
            recs.iter().nth(1),
            Some(&Record::Paste {
                pos: 0,
                removed: 0,
                cut: 0,
                len: 2,
                fallback: None,
            })
        );
    }

    #[test]
    fn the_buffer_keeps_positions_and_starts_as_its_front_moves() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let mut recs = Records::new();
        let mut marks = Vec::new();
        let mut cuts = DecodedCuts::new();
        for _ in 0..40 {
            marks.push(recs.end());
            transaction(&mut rng, &mut recs, &mut cuts);
        }
        let all = contents(&recs);
        let cut = marks[15];
        recs.discard_to(cut);
        let kept: Vec<_> = recs.range(recs.front(), recs.end()).collect();
        let starts: Vec<u64> = kept
            .iter()
            .enumerate()
            .filter(|(_, (s, _, _))| *s)
            .map(|(i, _)| cut.at + i as u64)
            .collect();
        assert_eq!(starts, marks[15..].iter().map(|c| c.at).collect::<Vec<_>>());
        assert_eq!(contents(&recs), all[(cut.at as usize)..]);
        recs.truncate(marks[30]);
        assert_eq!(recs.end(), marks[30]);
        assert_eq!(
            recs.range(recs.front(), recs.end())
                .filter(|(s, _, _)| *s)
                .count(),
            15
        );
    }

    #[test]
    fn transactions_across_many_pages_round_trip() {
        let mut storage = setup(3);
        let mut w = JournalWriter::new(8, 2);
        let mut next = 3u32;
        let mut alloc = |s: &mut dyn Storage| -> Result<u32, Error> {
            let p = next;
            next += 1;
            s.set_len((p as u64 + 1) * PAGE_SIZE as u64)?;
            Ok(p)
        };
        let mut rng = Rng(77);
        let mut recs = Records::new();
        let mut cuts = DecodedCuts::new();
        let mut journal = JournalCuts::starting_at(0);
        for _ in 0..200 {
            let from = recs.end();
            transaction(&mut rng, &mut recs, &mut cuts);
            // Payloads large enough to cross pages.
            let bytes = recs.stash(&[7u8; 300]);
            cuts.push((recs.end().at, 300, false));
            recs.push(Record::Write { pos: 5, bytes });
            let mut payload = Vec::new();
            let mut c = w.cursor;
            recs.encode(from, recs.end(), &mut c, &mut journal, &mut payload);
            w.cursor = c;
            w.append(&payload, &mut storage, &mut alloc).unwrap();
        }
        assert!(w.pages.len() > 3);
        let got = recover(&storage, 8, 2).unwrap();
        assert_eq!(got.transactions, 200);
        assert_eq!(contents(&got.records), contents(&recs));
        // A different epoch salt validates nothing.
        assert_eq!(recover(&storage, 9, 2).unwrap().transactions, 0);
    }

    #[test]
    fn a_recovered_journal_never_fits_anything() {
        let w = JournalWriter::recovered(1, vec![2, 3], vec![], 4);
        assert!(!w.is_empty());
        assert!(!w.fits(0, u32::MAX));
    }

    #[test]
    fn a_torn_tail_keeps_the_longest_valid_prefix_and_skips_no_hole() {
        let mut storage = MemoryStorage::with_crash_tracking();
        storage.set_len(6 * PAGE_SIZE as u64).unwrap();
        storage.sync().unwrap();
        let mut w = JournalWriter::new(3, 2);
        let mut next = 3u32;
        let mut alloc = |_: &mut dyn Storage| -> Result<u32, Error> {
            let p = next;
            next += 1;
            Ok(p)
        };
        let mut recs = Records::new();
        for t in 0..8 {
            let from = recs.end();
            let bytes = recs.stash(&vec![t as u8; 1000 + t]);
            recs.push(Record::Write { pos: 0, bytes });
            let mut payload = Vec::new();
            let mut c = 0;
            let mut journal = JournalCuts::starting_at(0);
            recs.encode(from, recs.end(), &mut c, &mut journal, &mut payload);
            w.append(&payload, &mut storage, &mut alloc).unwrap();
        }
        let ops = storage.unsynced_ops();
        // Losing any single write leaves exactly the transactions before it.
        for lost in 0..ops {
            let img = storage.crash_image(|i| i != lost);
            let got = recover(&MemoryStorage::from_image(img), 3, 2).unwrap();
            assert!(got.transactions < 8);
            let img_prefix = storage.crash_image(|i| i < lost);
            let prefix = recover(&MemoryStorage::from_image(img_prefix), 3, 2).unwrap();
            assert_eq!(got.transactions, prefix.transactions, "losing write {lost}");
        }
    }

    #[test]
    fn a_journal_pointer_past_the_file_is_an_empty_journal() {
        let storage = setup(2);
        let got = recover(&storage, 1, 2).unwrap();
        assert_eq!(got.transactions, 0);
    }
}
