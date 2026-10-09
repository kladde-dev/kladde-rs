//! The journal (`spec/journal.md`): records, their encoding, the buffer that
//! holds them between flushes, transactions, the chain of journal pages, the
//! CRC chain, and recovery.

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

/// One record: a byte-level effect on allocations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Record {
    Free {
        id: u32,
    },
    Resize {
        id: u32,
        size: u32,
    },
    Write {
        id: u32,
        offset: u32,
        bytes: Bytes,
    },
    Splice {
        id: u32,
        offset: u32,
        old_len: u32,
        bytes: Bytes,
    },
    Copy {
        src: u32,
        src_offset: u32,
        len: u32,
        dst: u32,
        dst_offset: u32,
    },
    Move {
        src: u32,
        src_offset: u32,
        len: u32,
        dst: u32,
        dst_offset: u32,
    },
}

impl Record {
    /// The ids the record names: one, or a source and a destination.
    pub fn ids(&self) -> impl Iterator<Item = u32> {
        let (a, b) = match *self {
            Record::Free { id }
            | Record::Resize { id, .. }
            | Record::Write { id, .. }
            | Record::Splice { id, .. } => (id, None),
            Record::Copy { src, dst, .. } | Record::Move { src, dst, .. } => (src, Some(dst)),
        };
        std::iter::once(a).chain(b)
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
    /// Encoded record bytes before the position.
    pub bytes: u64,
}

/// The one buffer of every record since the last flush, with their payloads
/// in an arena beside them (`impl/transactions-and-batches.md`).
///
/// Records are appended at the end and discarded from the front, so both the
/// records and the arena are deques, and positions count from the start of
/// the session: cursors, and the `Bytes` of a record, stay valid however much
/// of the front goes. A flush may also store bytes of its own past the end of
/// the arena, with [`scratch`](Self::scratch), which
/// [`drop_scratch`](Self::drop_scratch) drops again.
#[derive(Clone, Debug, Default)]
pub struct Records {
    records: VecDeque<Record>,
    arena: VecDeque<u8>,
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
        self.end.bytes += record_len(&r) as u64;
        self.end.arena = self.front.arena + self.arena.len() as u64;
        self.records.push_back(r);
    }

    /// The records in `[from, to)`.
    pub fn range(&self, from: Cursor, to: Cursor) -> impl Iterator<Item = &Record> {
        let base = self.front.at;
        self.records
            .range((from.at - base) as usize..(to.at - base) as usize)
    }

    /// Every record held.
    pub fn iter(&self) -> impl Iterator<Item = &Record> {
        self.records.iter()
    }

    /// The arena's bytes `[start, start + len)`, in at most two slices.
    fn slices(&self, start: u64, len: usize) -> (&[u8], &[u8]) {
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

    /// Encodes the records in `[from, to)` as a transaction's record bytes.
    pub fn encode(&self, from: Cursor, to: Cursor, out: &mut Vec<u8>) {
        for r in self.range(from, to) {
            encode_record(r, self, out);
        }
    }

    /// Stores `bytes` past the end, for the flush, and returns their
    /// position.
    pub fn scratch(&mut self, bytes: &[u8]) -> u64 {
        let pos = self.front.arena + self.arena.len() as u64;
        self.arena.extend(bytes);
        pos
    }

    /// Drops what [`scratch`](Self::scratch) stored.
    pub fn drop_scratch(&mut self) {
        self.arena
            .truncate((self.end.arena - self.front.arena) as usize);
    }

    /// Discards the records from `to` on: the end moves back to `to`.
    pub fn truncate(&mut self, to: Cursor) {
        debug_assert!(self.front.at <= to.at && to.at <= self.end.at);
        self.records.truncate((to.at - self.front.at) as usize);
        self.arena.truncate((to.arena - self.front.arena) as usize);
        self.end = to;
    }

    /// Discards the records before `to`: the front moves up to `to`.
    pub fn discard_to(&mut self, to: Cursor) {
        debug_assert!(self.front.at <= to.at && to.at <= self.end.at);
        self.records.drain(..(to.at - self.front.at) as usize);
        self.arena.drain(..(to.arena - self.front.arena) as usize);
        self.front = to;
    }

    /// The byte at arena position `pos`.
    #[cfg(test)]
    pub fn byte(&self, pos: u64) -> u8 {
        self.arena[(pos - self.front.arena) as usize]
    }

    /// The bytes of `b`, copied.
    #[cfg(test)]
    pub fn to_vec(&self, b: Bytes) -> Vec<u8> {
        let mut out = Vec::new();
        self.extend_bytes(b, &mut out);
        out
    }
}

/// The bytes the encoding of `r` takes, computed without encoding it.
pub fn record_len(r: &Record) -> usize {
    let v = |x: u32| kladde_varint::encoded_len(x as u64);
    1 + match *r {
        Record::Free { id } => v(id),
        Record::Resize { id, size } => v(id) + v(size),
        Record::Write { id, offset, bytes } => {
            v(id) + v(offset) + v(bytes.len) + bytes.len as usize
        }
        Record::Splice {
            id,
            offset,
            old_len,
            bytes,
        } => v(id) + v(offset) + v(old_len) + v(bytes.len) + bytes.len as usize,
        Record::Copy {
            src,
            src_offset,
            len,
            dst,
            dst_offset,
        }
        | Record::Move {
            src,
            src_offset,
            len,
            dst,
            dst_offset,
        } => v(src) + v(src_offset) + v(len) + v(dst) + v(dst_offset),
    }
}

/// The stream bytes a transaction of `n` record bytes takes: its length
/// prefix, its records, and its `crc` (`spec/journal.md#framing`).
pub fn framed_len(n: u64) -> u64 {
    kladde_varint::encoded_len(n) as u64 + n + 4
}

fn put(out: &mut Vec<u8>, v: u64) {
    kladde_varint::encode(v, out);
}

fn encode_record(r: &Record, recs: &Records, out: &mut Vec<u8>) {
    match *r {
        Record::Free { id } => {
            out.push(1);
            put(out, id as u64);
        }
        Record::Resize { id, size } => {
            out.push(2);
            put(out, id as u64);
            put(out, size as u64);
        }
        Record::Write { id, offset, bytes } => {
            out.push(3);
            put(out, id as u64);
            put(out, offset as u64);
            put(out, bytes.len as u64);
            recs.extend_bytes(bytes, out);
        }
        Record::Splice {
            id,
            offset,
            old_len,
            bytes,
        } => {
            out.push(4);
            put(out, id as u64);
            put(out, offset as u64);
            put(out, old_len as u64);
            put(out, bytes.len as u64);
            recs.extend_bytes(bytes, out);
        }
        Record::Copy {
            src,
            src_offset,
            len,
            dst,
            dst_offset,
        }
        | Record::Move {
            src,
            src_offset,
            len,
            dst,
            dst_offset,
        } => {
            out.push(if matches!(r, Record::Copy { .. }) {
                5
            } else {
                6
            });
            put(out, src as u64);
            put(out, src_offset as u64);
            put(out, len as u64);
            put(out, dst as u64);
            put(out, dst_offset as u64);
        }
    }
}

/// Decodes every record in `input` into `into`, checking the bounds the
/// specification states.
pub fn decode_records(input: &[u8], into: &mut Records) -> Result<(), Error> {
    let mut rest = input;
    let get = |rest: &mut &[u8]| -> Result<u64, Error> {
        let (v, r) =
            kladde_varint::decode(rest).map_err(|_| corrupt("a journal record is truncated"))?;
        *rest = r;
        Ok(v)
    };
    let id_of = |v: u64| -> Result<u32, Error> {
        if v == 0 || v > u32::MAX as u64 {
            Err(corrupt("a journal record names an invalid id"))
        } else {
            Ok(v as u32)
        }
    };
    let bounded = |a: u64, b: u64| -> Result<(u32, u32), Error> {
        if a.checked_add(b).is_none_or(|s| s > u32::MAX as u64) {
            Err(corrupt("a journal record reaches past 2^32 - 1"))
        } else {
            Ok((a as u32, b as u32))
        }
    };
    while !rest.is_empty() {
        let tag = rest[0];
        rest = &rest[1..];
        let rec = match tag {
            1 => Record::Free {
                id: id_of(get(&mut rest)?)?,
            },
            2 => {
                let id = id_of(get(&mut rest)?)?;
                let (size, _) = bounded(get(&mut rest)?, 0)?;
                Record::Resize { id, size }
            }
            3 | 4 => {
                let id = id_of(get(&mut rest)?)?;
                let offset = get(&mut rest)?;
                let old_len = if tag == 4 { get(&mut rest)? } else { 0 };
                let len = get(&mut rest)?;
                if (rest.len() as u64) < len {
                    return Err(corrupt("a journal record's bytes are truncated"));
                }
                let (offset, _) = bounded(offset, if tag == 3 { len } else { old_len })?;
                if tag == 4 {
                    bounded(offset as u64, len)?;
                }
                let bytes = into.stash(&rest[..len as usize]);
                rest = &rest[len as usize..];
                if tag == 3 {
                    Record::Write { id, offset, bytes }
                } else {
                    Record::Splice {
                        id,
                        offset,
                        old_len: old_len as u32,
                        bytes,
                    }
                }
            }
            5 | 6 => {
                let src = id_of(get(&mut rest)?)?;
                let src_offset = get(&mut rest)?;
                let len = get(&mut rest)?;
                let dst = id_of(get(&mut rest)?)?;
                let dst_offset = get(&mut rest)?;
                let (src_offset, len32) = bounded(src_offset, len)?;
                let (dst_offset, _) = bounded(dst_offset, len)?;
                if tag == 5 {
                    Record::Copy {
                        src,
                        src_offset,
                        len: len32,
                        dst,
                        dst_offset,
                    }
                } else {
                    Record::Move {
                        src,
                        src_offset,
                        len: len32,
                        dst,
                        dst_offset,
                    }
                }
            }
            _ => return Err(corrupt("a journal record has a reserved tag")),
        };
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
    /// Stream bytes appended so far.
    pub bytes: u64,
    /// Transactions appended so far.
    pub transactions: u64,
    /// The journal of an earlier session, which nothing may be appended to
    /// (`spec/journal.md#the-start-of-a-session`): it names the pages that
    /// the flush folding it retires, and counts as full, so that the flush
    /// rules fold it before anything is appended.
    recovered: bool,
}

impl JournalWriter {
    /// An empty journal of epoch `epoch`, starting at `first_page`.
    pub fn new(epoch: u64, first_page: u32) -> Self {
        JournalWriter {
            pages: vec![first_page],
            buf: new_page(),
            pos: 0,
            dirty_from: 0,
            chain: seeded_chain(epoch),
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

    /// Whether a transaction of `n` record bytes, appended now, would keep
    /// the journal within `budget` pages. The next page is taken only once
    /// the stream continues past the current one, so a stream that ends
    /// within `budget` pages' worth of stream bytes takes at most `budget`
    /// pages.
    pub fn fits(&self, n: u64, budget: u32) -> bool {
        !self.recovered && self.bytes + framed_len(n) <= budget as u64 * JOURNAL_STREAM as u64
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
    /// The records of the complete transactions, in order.
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
        decode_records(&payload, &mut out.records)?;
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

    fn records(n: usize, size: usize) -> Records {
        let mut r = Records::new();
        for i in 0..n {
            let bytes = r.stash(&vec![i as u8; size]);
            r.push(Record::Write {
                id: 1 + i as u32,
                offset: i as u32,
                bytes,
            });
        }
        r.push(Record::Copy {
            src: 1,
            src_offset: 0,
            len: 3,
            dst: 2,
            dst_offset: 7,
        });
        r.push(Record::Free { id: 9 });
        r
    }

    fn encoded(recs: &Records) -> Vec<u8> {
        let mut payload = Vec::new();
        recs.encode(recs.front(), recs.end(), &mut payload);
        payload
    }

    /// Each record with its payload, for comparing buffers, the payload's
    /// position in the arena left out.
    fn contents(recs: &Records) -> Vec<(Record, Vec<u8>)> {
        recs.iter()
            .map(|&r| match r {
                Record::Write { id, offset, bytes } => (
                    Record::Write {
                        id,
                        offset,
                        bytes: Bytes { start: 0, ..bytes },
                    },
                    recs.to_vec(bytes),
                ),
                Record::Splice {
                    id,
                    offset,
                    old_len,
                    bytes,
                } => (
                    Record::Splice {
                        id,
                        offset,
                        old_len,
                        bytes: Bytes { start: 0, ..bytes },
                    },
                    recs.to_vec(bytes),
                ),
                _ => (r, Vec::new()),
            })
            .collect()
    }

    /// A record of every kind, with fields and payloads of random sizes.
    fn random_record(rng: &mut u64, recs: &mut Records) -> Record {
        let mut next = |bits: u32| {
            *rng ^= *rng << 13;
            *rng ^= *rng >> 7;
            *rng ^= *rng << 17;
            (*rng % (1u64 << bits.min(32))) as u32
        };
        let bits = next(5);
        let id = next(bits).max(1);
        match next(3) {
            0 => Record::Free { id },
            1 => Record::Resize {
                id,
                size: next(bits),
            },
            2 | 3 => {
                let len = next(9) as usize;
                let bytes = recs.stash(&vec![next(8) as u8; len]);
                if next(1) == 0 {
                    Record::Write {
                        id,
                        offset: next(bits),
                        bytes,
                    }
                } else {
                    Record::Splice {
                        id,
                        offset: next(bits),
                        old_len: next(bits),
                        bytes,
                    }
                }
            }
            k => {
                let (src, src_offset, len, dst_offset) =
                    (next(bits).max(1), next(bits), next(bits), next(bits));
                if k < 6 {
                    Record::Copy {
                        src,
                        src_offset,
                        len,
                        dst: id,
                        dst_offset,
                    }
                } else {
                    Record::Move {
                        src,
                        src_offset,
                        len,
                        dst: id,
                        dst_offset,
                    }
                }
            }
        }
    }

    #[test]
    fn record_len_counts_what_the_encoding_writes() {
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        let mut recs = Records::new();
        for _ in 0..2000 {
            let r = random_record(&mut rng, &mut recs);
            recs.push(r);
            let mut out = Vec::new();
            encode_record(&r, &recs, &mut out);
            assert_eq!(record_len(&r), out.len(), "{r:?}");
        }
        assert_eq!(recs.end().bytes, encoded(&recs).len() as u64);
    }

    #[test]
    fn the_buffer_keeps_positions_as_its_front_moves_and_its_deques_wrap() {
        let mut rng = 0x2545_F491_4F6C_DD1Du64;
        let mut recs = Records::new();
        let mut want: Vec<(Record, Vec<u8>)> = Vec::new();
        for round in 0..50 {
            for _ in 0..(round % 7) * 5 + 1 {
                let r = random_record(&mut rng, &mut recs);
                recs.push(r);
                want.extend(contents(&recs).pop());
            }
            // Scratch bytes past the end change nothing that is held.
            let pos = recs.scratch(&[0xAB; 300]);
            assert_eq!(pos, recs.end().arena);
            let mut got = [0u8; 300];
            recs.copy_bytes(pos, &mut got);
            assert_eq!(got, [0xAB; 300]);
            recs.drop_scratch();
            assert_eq!(contents(&recs), want);
            // Drop a prefix, and every cursor into the rest still holds.
            let keep = want.len() / 3;
            let mut cut = recs.front();
            for r in recs.iter().take(want.len() - keep) {
                cut.at += 1;
                cut.bytes += record_len(r) as u64;
                if let Record::Write { bytes, .. } | Record::Splice { bytes, .. } = r {
                    cut.arena = bytes.start + bytes.len as u64;
                }
            }
            recs.discard_to(cut);
            want.drain(..want.len() - keep);
            assert_eq!(contents(&recs), want);
            assert_eq!(
                recs.end().bytes - recs.front().bytes,
                encoded(&recs).len() as u64
            );
        }
        // Truncating drops the records and their payloads from the end.
        let mark = recs.end();
        let r = random_record(&mut rng, &mut recs);
        recs.push(r);
        recs.truncate(mark);
        assert_eq!(contents(&recs), want);
        assert_eq!(recs.stash(&[1]).start, mark.arena);
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
        let mut all = Vec::new();
        for t in 0..20 {
            let recs = records(t % 5 + 1, 700 + t * 37);
            w.append(&encoded(&recs), &mut storage, &mut alloc).unwrap();
            all.extend(contents(&recs));
        }
        assert!(w.pages.len() > 3);
        let got = recover(&storage, 8, 2).unwrap();
        assert_eq!(got.transactions, 20);
        assert_eq!(contents(&got.records), all);
        // A different epoch salt validates nothing.
        assert_eq!(recover(&storage, 9, 2).unwrap().transactions, 0);
    }

    #[test]
    fn a_transaction_fits_exactly_when_it_ends_within_the_budget() {
        let mut rng = 0x5851_F42D_4C95_7F2Du64;
        for budget in 1..5u32 {
            for _ in 0..50 {
                let mut storage = setup(3);
                let mut w = JournalWriter::new(1, 2);
                let mut next = 3u32;
                let mut alloc = |s: &mut dyn Storage| -> Result<u32, Error> {
                    let p = next;
                    next += 1;
                    s.set_len((p as u64 + 1) * PAGE_SIZE as u64)?;
                    Ok(p)
                };
                loop {
                    let mut recs = Records::new();
                    for _ in 0..rng % 40 + 1 {
                        let r = random_record(&mut rng, &mut recs);
                        recs.push(r);
                    }
                    let fits = w.fits(recs.end().bytes, budget);
                    w.append(&encoded(&recs), &mut storage, &mut alloc).unwrap();
                    assert_eq!(w.pages.len() as u32 <= budget, fits);
                    assert_eq!(w.over_budget(budget), !fits);
                    if !fits {
                        break;
                    }
                }
            }
        }
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
        for t in 0..8 {
            let recs = records(2, 1000 + t);
            w.append(&encoded(&recs), &mut storage, &mut alloc).unwrap();
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
