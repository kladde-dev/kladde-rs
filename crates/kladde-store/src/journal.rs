//! The journal: records, their encoding, the buffer that holds them between
//! flushes, transactions, the chain of journal pages, the CRC chain, and
//! recovery.
//!
//! The records act on positions of the one sequence: `Write`, `Splice`,
//! `Move`, and `Paste`, which inserts what a `Write` or `Splice` earlier in
//! the same transaction cut out. On file each position is written as its
//! distance from where the record before it left off, and the record's kind
//! rides in the low two bits of that varint.

use std::collections::VecDeque;

use crate::consts::{JOURNAL_STREAM, PAGE_SIZE};
use crate::crc::Crc32c;
use crate::error::{corrupt, Error};
use crate::page::{new_page, PageBuf};
use crate::storage::Storage;

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
    /// its transaction.
    Write { pos: u64, bytes: Bytes },
    /// Replaces the `removed` bytes at `pos` with `bytes`; what it removes
    /// is a cut of its transaction, if anything.
    Splice {
        pos: u64,
        removed: u64,
        bytes: Bytes,
    },
    /// Cuts `[from, from + len)` and inserts it at `to`, a position in the
    /// sequence without the cut bytes.
    Move { from: u64, len: u64, to: u64 },
    /// Replaces the `removed` bytes at `pos` with the cut that `cut` cuts
    /// were made after, in this transaction; the cut is `len` bytes long.
    Paste {
        pos: u64,
        removed: u64,
        cut: u32,
        len: u64,
    },
}

impl Record {
    /// The kind, as the low two bits of the head encode it.
    fn kind(&self) -> u64 {
        match self {
            Record::Write { .. } => 0,
            Record::Splice { .. } => 1,
            Record::Move { .. } => 2,
            Record::Paste { .. } => 3,
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
    front: Cursor,
    end: Cursor,
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
        self.end.at += 1;
        self.end.bytes += record_len(&r, self.end.seq) as u64;
        self.end.seq = r.end();
        self.end.arena = self.front.arena + self.arena.len() as u64;
        self.records.push_back(r);
    }

    /// Marks the record at `at` as the first of a journal transaction.
    pub fn mark_start(&mut self, at: u64) {
        if self.starts.back() != Some(&at) {
            debug_assert!(self.starts.back().is_none_or(|&s| s < at));
            self.starts.push_back(at);
        }
    }

    /// The records in `[from, to)`, each with whether a journal transaction
    /// starts at it.
    pub fn range(&self, from: Cursor, to: Cursor) -> impl Iterator<Item = (bool, &Record)> {
        let base = self.front.at;
        let starts = &self.starts;
        self.records
            .range((from.at - base) as usize..(to.at - base) as usize)
            .enumerate()
            .map(move |(i, r)| {
                let at = from.at + i as u64;
                (starts.binary_search(&at).is_ok(), r)
            })
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
    /// off.
    pub fn encode(&self, from: Cursor, to: Cursor, cursor: &mut u64, out: &mut Vec<u8>) {
        for (_, r) in self.range(from, to) {
            encode_record(r, *cursor, self, out);
            *cursor = r.end();
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

fn zz(v: i64) -> u64 {
    kladde_varint::zigzag(v)
}

/// The head of `r`, written after a record that left off at `cursor`.
fn head(r: &Record, cursor: u64) -> u64 {
    let delta = r.head_pos() as i64 - cursor as i64;
    zz((delta << 2) | r.kind() as i64)
}

/// The bytes the encoding of `r` takes after a record that left off at
/// `cursor`, computed without encoding it.
pub fn record_len(r: &Record, cursor: u64) -> usize {
    let v = |x: u64| kladde_varint::encoded_len(x);
    v(head(r, cursor))
        + match *r {
            Record::Write { bytes, .. } => v(bytes.len as u64) + bytes.len as usize,
            Record::Splice { removed, bytes, .. } => {
                v(removed) + v(bytes.len as u64) + bytes.len as usize
            }
            Record::Move { from, len, to } => v(len) + v(zz(to as i64 - from as i64)),
            Record::Paste { removed, cut, .. } => v(removed) + v(cut as u64),
        }
}

/// The stream bytes a transaction of `n` record bytes takes: its length
/// prefix, its records, and its `crc`.
pub fn framed_len(n: u64) -> u64 {
    kladde_varint::encoded_len(n) as u64 + n + 4
}

fn put(out: &mut Vec<u8>, v: u64) {
    kladde_varint::encode(v, out);
}

fn encode_record(r: &Record, cursor: u64, recs: &Records, out: &mut Vec<u8>) {
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
        Record::Paste { removed, cut, .. } => {
            put(out, removed);
            put(out, cut as u64);
        }
    }
}

use crate::consts::MAX_SEQUENCE;

/// Decodes the records of one transaction in `input` into `into`, their
/// positions relative to `cursor`, which ends where they leave off; checks
/// the bounds and the cuts a `Paste` names, but not whether the positions
/// lie inside the sequence, which only the fold knows.
pub fn decode_records(input: &[u8], cursor: &mut u64, into: &mut Records) -> Result<(), Error> {
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
    // The lengths of the cuts this transaction made, oldest first, and
    // whether each was pasted.
    let mut cuts: Vec<(u64, bool)> = Vec::new();
    while !rest.is_empty() {
        let x = kladde_varint::unzigzag(get(&mut rest)?);
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
                if (rest.len() as u64) < len {
                    return Err(corrupt("a journal record's bytes are truncated"));
                }
                let bytes = into.stash(&rest[..len as usize]);
                rest = &rest[len as usize..];
                if kind == 0 {
                    if len == 0 {
                        return Err(corrupt("a journal record uses the reserved escape"));
                    }
                    cuts.push((len, false));
                    Record::Write { pos, bytes }
                } else {
                    if removed > 0 {
                        cuts.push((removed, false));
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
                    return Err(corrupt("a paste names a cut its transaction did not make"));
                }
                let i = cuts.len() - 1 - cut as usize;
                if cuts[i].1 {
                    return Err(corrupt("a paste names a cut already pasted"));
                }
                cuts[i].1 = true;
                Record::Paste {
                    pos,
                    removed,
                    cut: cut as u32,
                    len: cuts[i].0,
                }
            }
        };
        *cursor = rec.end();
        into.push(rec);
    }
    Ok(())
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
        }
    }

    /// The non-empty journal that recovery found: `transactions` complete
    /// transactions in `pages`.
    pub fn recovered(epoch: u64, pages: Vec<u32>, transactions: u64) -> Self {
        JournalWriter {
            pages,
            transactions,
            recovered: true,
            ..JournalWriter::new(epoch, 0)
        }
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
            && self.bytes + framed_len(n) + ESTIMATE_SLACK <= budget as u64 * JOURNAL_STREAM as u64
    }

    /// Whether the journal has outgrown `budget` pages, which only a
    /// transaction too large for them on its own makes it do.
    pub fn over_budget(&self, budget: u32) -> bool {
        self.bytes > budget as u64 * JOURNAL_STREAM as u64
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
    /// How many complete transactions there were.
    pub transactions: usize,
    /// The pages the valid part of the journal occupies.
    pub pages: Vec<u32>,
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
/// `first_page`.
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
    let mut valid_pages = 1;
    let mut cursor = 0u64;
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
        let at = out.records.end().at;
        decode_records(&payload, &mut cursor, &mut out.records)?;
        out.records.mark_start(at);
        out.transactions += 1;
        valid_pages = r.pages.len();
    }
    out.pages = r.pages[..valid_pages].to_vec();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryStorage;

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

    /// A transaction of random records, with valid cuts and pastes.
    fn transaction(rng: &mut Rng, recs: &mut Records) {
        let mut cuts: Vec<(u64, bool)> = Vec::new();
        let start = recs.end().at;
        for _ in 0..rng.next(4) + 1 {
            let bits = rng.next(5) as u32 + 1;
            let r = match rng.next(3) {
                0 => {
                    let n = rng.next(8) as usize + 1;
                    let bytes = recs.stash(&vec![rng.next(8) as u8; n]);
                    cuts.push((n as u64, false));
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
                        cuts.push((removed, false));
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
                _ => match cuts.iter().rposition(|c| !c.1) {
                    Some(i) => {
                        cuts[i].1 = true;
                        Record::Paste {
                            pos: rng.next(bits),
                            removed: rng.next(bits),
                            cut: (cuts.len() - 1 - i) as u32,
                            len: cuts[i].0,
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
        for _ in 0..500 {
            let from = recs.end();
            transaction(&mut rng, &mut recs);
            txs.push((from, recs.end()));
        }
        let mut cursor = 0u64;
        let mut decoded = Records::new();
        let mut dcursor = 0u64;
        let mut total = 0u64;
        for &(from, to) in &txs {
            let mut out = Vec::new();
            let before = cursor;
            recs.encode(from, to, &mut cursor, &mut out);
            // The estimate assumed the same cursor, so it is exact here.
            assert_eq!(to.bytes - from.bytes, out.len() as u64, "from {before}");
            total += out.len() as u64;
            let at = decoded.end().at;
            decode_records(&out, &mut dcursor, &mut decoded).unwrap();
            decoded.mark_start(at);
        }
        assert_eq!(total, recs.end().bytes);
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
        assert_eq!(record_len(&r, 1000), 4);
        assert_eq!(record_len(&r, 999), 4);
        assert!(record_len(&r, 0) > 4);
    }

    #[test]
    fn pastes_of_unknown_or_pasted_cuts_and_the_escape_are_refused() {
        let mut out = Vec::new();
        // A paste in a transaction that cut nothing.
        put(&mut out, zz(3));
        put(&mut out, 0);
        put(&mut out, 0);
        assert!(decode_records(&out, &mut 0, &mut Records::new()).is_err());
        // A write of no bytes.
        let mut out = Vec::new();
        put(&mut out, zz(0));
        put(&mut out, 0);
        assert!(decode_records(&out, &mut 0, &mut Records::new()).is_err());
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
        assert!(decode_records(&out, &mut 0, &mut Records::new()).is_err());
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
        decode_records(&out, &mut 0, &mut recs).unwrap();
        assert_eq!(
            recs.iter().nth(1),
            Some(&Record::Paste {
                pos: 0,
                removed: 0,
                cut: 0,
                len: 2
            })
        );
    }

    #[test]
    fn the_buffer_keeps_positions_and_starts_as_its_front_moves() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let mut recs = Records::new();
        let mut marks = Vec::new();
        for _ in 0..40 {
            marks.push(recs.end());
            transaction(&mut rng, &mut recs);
        }
        let all = contents(&recs);
        let cut = marks[15];
        recs.discard_to(cut);
        let kept: Vec<_> = recs.range(recs.front(), recs.end()).collect();
        let starts: Vec<u64> = kept
            .iter()
            .enumerate()
            .filter(|(_, (s, _))| *s)
            .map(|(i, _)| cut.at + i as u64)
            .collect();
        assert_eq!(starts, marks[15..].iter().map(|c| c.at).collect::<Vec<_>>());
        assert_eq!(contents(&recs), all[(cut.at as usize)..]);
        recs.truncate(marks[30]);
        assert_eq!(recs.end(), marks[30]);
        assert_eq!(
            recs.range(recs.front(), recs.end())
                .filter(|(s, _)| *s)
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
        for _ in 0..200 {
            let from = recs.end();
            transaction(&mut rng, &mut recs);
            // Payloads large enough to cross pages.
            let bytes = recs.stash(&[7u8; 300]);
            recs.push(Record::Write { pos: 5, bytes });
            let mut payload = Vec::new();
            let mut c = w.cursor;
            recs.encode(from, recs.end(), &mut c, &mut payload);
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
        let w = JournalWriter::recovered(1, vec![2, 3], 4);
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
            recs.encode(from, recs.end(), &mut c, &mut payload);
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
