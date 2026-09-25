//! The journal (`spec/journal.md`): records, their encoding, transactions,
//! the chain of pages a segment occupies, the CRC chain, and recovery.

use crate::consts::{JOURNAL_STREAM, PAGE_SIZE};
use crate::crc::Crc32c;
use crate::error::{corrupt, Error};
use crate::page::{new_page, PageBuf};
use crate::storage::Storage;

/// A byte range of a [`Records`] arena.
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

/// A sequence of records with their payloads in one arena.
#[derive(Clone, Debug, Default)]
pub struct Records {
    pub records: Vec<Record>,
    pub arena: Vec<u8>,
}

impl Records {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn clear(&mut self) {
        self.records.clear();
        self.arena.clear();
    }

    /// Stores `bytes` in the arena.
    pub fn stash(&mut self, bytes: &[u8]) -> Bytes {
        let start = self.arena.len() as u64;
        self.arena.extend_from_slice(bytes);
        Bytes {
            start,
            len: bytes.len() as u32,
        }
    }

    /// The bytes of `b`.
    pub fn bytes(&self, b: Bytes) -> &[u8] {
        &self.arena[b.start as usize..b.start as usize + b.len as usize]
    }

    /// Appends `r`, whose payload, if any, lives in `from`'s arena.
    pub fn push_from(&mut self, r: Record, from: &Records) {
        let r = match r {
            Record::Write { id, offset, bytes } => Record::Write {
                id,
                offset,
                bytes: self.stash(from.bytes(bytes)),
            },
            Record::Splice {
                id,
                offset,
                old_len,
                bytes,
            } => Record::Splice {
                id,
                offset,
                old_len,
                bytes: self.stash(from.bytes(bytes)),
            },
            other => other,
        };
        self.records.push(r);
    }

    /// Appends every record of `other`.
    pub fn extend_from(&mut self, other: &Records) {
        for r in &other.records {
            self.push_from(*r, other);
        }
    }

    /// Encodes records `range` as a transaction's record bytes.
    pub fn encode(&self, range: std::ops::Range<usize>, out: &mut Vec<u8>) {
        for r in &self.records[range] {
            encode_record(r, self, out);
        }
    }
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
            out.extend_from_slice(recs.bytes(bytes));
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
            out.extend_from_slice(recs.bytes(bytes));
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
        into.records.push(rec);
    }
    Ok(())
}

/// The CRC chain of a segment, seeded with its epoch.
fn seeded_chain(epoch: u64) -> Crc32c {
    let mut c = Crc32c::new();
    c.update(&epoch.to_le_bytes());
    c
}

/// Appends transactions to one segment's chain of pages.
pub struct SegmentWriter {
    /// The segment's epoch, which salts its chain.
    pub epoch: u64,
    /// The segment's pages in chain order; the first is the header's
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
}

impl SegmentWriter {
    /// An empty segment starting at `first_page`.
    pub fn new(epoch: u64, first_page: u32) -> Self {
        SegmentWriter {
            epoch,
            pages: vec![first_page],
            buf: new_page(),
            pos: 0,
            dirty_from: 0,
            chain: seeded_chain(epoch),
            bytes: 0,
            transactions: 0,
        }
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

/// What recovery found in a segment.
#[derive(Debug, Default)]
pub struct Recovered {
    /// The records of the complete transactions, in order.
    pub records: Records,
    /// How many complete transactions there were.
    pub transactions: usize,
    /// The pages the valid part of the segment occupies.
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

/// Reads the valid prefix of the segment of epoch `epoch` starting at
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
            r.records.push(Record::Write {
                id: 1 + i as u32,
                offset: i as u32,
                bytes,
            });
        }
        r.records.push(Record::Copy {
            src: 1,
            src_offset: 0,
            len: 3,
            dst: 2,
            dst_offset: 7,
        });
        r.records.push(Record::Free { id: 9 });
        r
    }

    #[test]
    fn transactions_across_many_pages_round_trip() {
        let mut storage = setup(3);
        let mut w = SegmentWriter::new(8, 2);
        let mut next = 3u32;
        let mut alloc = |s: &mut dyn Storage| -> Result<u32, Error> {
            let p = next;
            next += 1;
            s.set_len((p as u64 + 1) * PAGE_SIZE as u64)?;
            Ok(p)
        };
        let mut all = Records::new();
        for t in 0..20 {
            let recs = records(t % 5 + 1, 700 + t * 37);
            let mut payload = Vec::new();
            recs.encode(0..recs.len(), &mut payload);
            w.append(&payload, &mut storage, &mut alloc).unwrap();
            all.extend_from(&recs);
        }
        assert!(w.pages.len() > 3);
        let got = recover(&storage, 8, 2).unwrap();
        assert_eq!(got.transactions, 20);
        assert_eq!(got.records.records.len(), all.records.len());
        for (a, b) in got.records.records.iter().zip(&all.records) {
            match (a, b) {
                (
                    Record::Write {
                        id: i1,
                        offset: o1,
                        bytes: b1,
                    },
                    Record::Write {
                        id: i2,
                        offset: o2,
                        bytes: b2,
                    },
                ) => {
                    assert_eq!((i1, o1), (i2, o2));
                    assert_eq!(got.records.bytes(*b1), all.bytes(*b2));
                }
                (x, y) => assert_eq!(x, y),
            }
        }
        // A different epoch salt validates nothing.
        assert_eq!(recover(&storage, 9, 2).unwrap().transactions, 0);
    }

    #[test]
    fn a_torn_tail_keeps_the_longest_valid_prefix_and_skips_no_hole() {
        let mut storage = MemoryStorage::with_crash_tracking();
        storage.set_len(6 * PAGE_SIZE as u64).unwrap();
        storage.sync().unwrap();
        let mut w = SegmentWriter::new(3, 2);
        let mut next = 3u32;
        let mut alloc = |_: &mut dyn Storage| -> Result<u32, Error> {
            let p = next;
            next += 1;
            Ok(p)
        };
        for t in 0..8 {
            let recs = records(2, 1000 + t);
            let mut payload = Vec::new();
            recs.encode(0..recs.len(), &mut payload);
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
