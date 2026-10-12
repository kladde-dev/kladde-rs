//! The encoding of a span-table page's content: child references, then
//! entries sorted by span id.
//!
//! ```text
//! table_page := child_ref* 0:byte entry*
//! entry      := id_delta:varint tag:byte body next?
//! ; bit 0 of tag: 1 if the next span is id + 1, and no `next` field follows
//! ; tag >> 1 = 0:      Ref     body := length:varint address_delta:varint
//! ; tag >> 1 = 1:      Zero    body := length:varint
//! ; tag >> 1 = 2..127: Inline  body := payload:byte{(tag >> 1) - 1}
//! next       := varint ; 0: the chain ends here; else zigzag(next - id)
//! ```

use crate::consts::*;
use crate::error::{corrupt, Error};

/// What an entry says a span shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Body<'a> {
    /// `len` bytes at file address `addr`.
    Ref {
        len: u32,
        addr: u64,
    },
    Zero {
        len: u32,
    },
    Inline(&'a [u8]),
}

/// An entry to encode.
#[derive(Clone, Copy, Debug)]
pub struct EntryOut<'a> {
    pub id: u32,
    pub next: u32,
    pub body: Body<'a>,
}

/// Where an encoded entry landed in the content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placed {
    pub id: u32,
    /// The bytes the entry takes, payload included.
    pub len: u16,
    /// Where an `Inline` entry's payload starts within the content.
    pub payload: usize,
}

fn put(out: &mut Vec<u8>, v: u64) {
    kladde_varint::encode(v, out);
}

fn vlen(v: u64) -> usize {
    kladde_varint::encoded_len(v)
}

fn zz(v: i64) -> u64 {
    kladde_varint::zigzag(v)
}

/// The bytes the child references take, the delimiter included.
pub fn children_len(children: &[u32]) -> usize {
    let mut prev = 0u32;
    let mut n = 1;
    for &c in children {
        n += vlen((c - prev) as u64);
        prev = c;
    }
    n
}

/// Encodes child references, in ascending order, then entries, sorted by
/// id, into `out`; returns where each entry landed.
pub fn encode(children: &[u32], entries: &[EntryOut], out: &mut Vec<u8>) -> Vec<Placed> {
    let mut prev = 0u32;
    for &c in children {
        debug_assert!(c > prev, "child references out of order");
        put(out, (c - prev) as u64);
        prev = c;
    }
    out.push(0);
    let mut placed = Vec::with_capacity(entries.len());
    let mut prev_id = 0u32;
    let mut prev_end = 0u64;
    for e in entries {
        let start = out.len();
        let payload = encode_one(e, prev_id, prev_end, out);
        prev_id = e.id;
        if let Body::Ref { len, addr } = e.body {
            prev_end = addr + len as u64;
        }
        placed.push(Placed {
            id: e.id,
            len: (out.len() - start) as u16,
            payload,
        });
    }
    placed
}

/// Encodes entries one at a time into a page's content, refusing one that
/// would not fit.
pub struct PageEncoder {
    pub out: Vec<u8>,
    capacity: usize,
    prev_id: u32,
    prev_end: u64,
    pub placed: Vec<Placed>,
}

impl PageEncoder {
    /// A page whose content, child references and delimiter included, may
    /// take `capacity` bytes.
    pub fn new(children: &[u32], capacity: usize) -> Self {
        let mut out = Vec::with_capacity(capacity);
        encode(children, &[], &mut out);
        PageEncoder {
            out,
            capacity,
            prev_id: 0,
            prev_end: 0,
            placed: Vec::new(),
        }
    }

    /// Appends `e`, whose id must exceed every id before it, if it fits.
    pub fn push(&mut self, e: &EntryOut) -> bool {
        let start = self.out.len();
        let mut tmp = std::mem::take(&mut self.out);
        // Encode as if after the previous entry: a one-entry run with the
        // cursors carried over.
        let placed = encode_one(e, self.prev_id, self.prev_end, &mut tmp);
        if tmp.len() > self.capacity {
            tmp.truncate(start);
            self.out = tmp;
            return false;
        }
        self.out = tmp;
        self.prev_id = e.id;
        if let Body::Ref { len, addr } = e.body {
            self.prev_end = addr + len as u64;
        }
        self.placed.push(Placed {
            id: e.id,
            len: (self.out.len() - start) as u16,
            payload: placed,
        });
        true
    }
}

/// Encodes one entry after an entry of id `prev_id` and a reference that
/// ended at `prev_end`; returns where an inline payload starts.
fn encode_one(e: &EntryOut, prev_id: u32, prev_end: u64, out: &mut Vec<u8>) -> usize {
    debug_assert!(e.id > prev_id, "entries out of order");
    put(out, (e.id - prev_id) as u64);
    let implicit = e.next != 0 && e.next == e.id.wrapping_add(1);
    let mut payload = 0;
    match e.body {
        Body::Ref { len, addr } => {
            out.push(implicit as u8);
            put(out, len as u64);
            put(out, zz(addr as i64 - prev_end as i64));
        }
        Body::Zero { len } => {
            out.push(2 | implicit as u8);
            put(out, len as u64);
        }
        Body::Inline(p) => {
            debug_assert!(!p.is_empty() && p.len() <= MAX_INLINE);
            out.push((((p.len() + 1) << 1) | implicit as usize) as u8);
            payload = out.len();
            out.extend_from_slice(p);
        }
    }
    if !implicit {
        put(
            out,
            if e.next == 0 {
                0
            } else {
                zz(e.next as i64 - e.id as i64)
            },
        );
    }
    payload
}

/// About the bytes an entry takes among neighbours of nearby ids, for
/// choosing what goes where before anything is encoded.
pub fn estimate(id: u32, next: u32, body: &Body) -> usize {
    let next_len = if next != 0 && next == id.wrapping_add(1) {
        0
    } else if next == 0 {
        1
    } else {
        vlen(zz(next as i64 - id as i64)).min(3)
    };
    2 + next_len
        + match body {
            Body::Ref { len, .. } => vlen(*len as u64) + 2,
            Body::Zero { len } => vlen(*len as u64),
            Body::Inline(p) => p.len(),
        }
}

/// A decoded entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryIn {
    pub id: u32,
    pub next: u32,
    pub kind: KindIn,
    /// The bytes the entry takes, payload included.
    pub len: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KindIn {
    Ref {
        len: u32,
        addr: u64,
    },
    Zero {
        len: u32,
    },
    /// The payload of `len` bytes, starting `offset` bytes into the content.
    Inline {
        offset: usize,
        len: u32,
    },
}

/// A decoded page content.
#[derive(Debug, Default)]
pub struct Decoded {
    pub children: Vec<u32>,
    pub entries: Vec<EntryIn>,
}

/// Decodes a table page's content, checking what the format requires of
/// each entry on its own.
pub fn decode(content: &[u8]) -> Result<Decoded, Error> {
    let mut rest = content;
    let bad = |m: &str| corrupt(format!("a span-table page {m}"));
    let get = |rest: &mut &[u8]| -> Result<u64, Error> {
        let (v, r) = kladde_varint::decode(rest).map_err(|_| bad("is truncated"))?;
        *rest = r;
        Ok(v)
    };
    let mut out = Decoded::default();
    let mut page = 0u64;
    loop {
        let d = get(&mut rest)?;
        if d == 0 {
            break;
        }
        page += d;
        if page >= u32::MAX as u64 {
            return Err(bad("names a page past the file's bounds"));
        }
        out.children.push(page as u32);
    }
    let mut id = 0u64;
    let mut prev_end = 0i64;
    while !rest.is_empty() {
        let start = content.len() - rest.len();
        let d = get(&mut rest)?;
        if d == 0 {
            return Err(bad("holds entries out of order"));
        }
        id += d;
        if id >= u32::MAX as u64 {
            return Err(bad("names a span id past 2^32 - 1"));
        }
        let Some((&tag, r)) = rest.split_first() else {
            return Err(bad("is truncated"));
        };
        rest = r;
        let implicit = tag & 1 == 1;
        let kind = match tag >> 1 {
            0 => {
                let len = get(&mut rest)?;
                let addr = prev_end + kladde_varint::unzigzag(get(&mut rest)?);
                if len == 0 || len > MAX_PAGE_CONTENT as u64 || addr < 0 {
                    return Err(bad("holds a reference out of bounds"));
                }
                let (pg, off) = (
                    addr as u64 / PAGE_SIZE as u64,
                    addr as u64 % PAGE_SIZE as u64,
                );
                if pg < 2 || off < CONTENT_OFFSET as u64 || off + len > (PAGE_SIZE - 4) as u64 {
                    return Err(bad("holds a reference outside a data page"));
                }
                prev_end = addr + len as i64;
                KindIn::Ref {
                    len: len as u32,
                    addr: addr as u64,
                }
            }
            1 => {
                let len = get(&mut rest)?;
                if len == 0 || len > u32::MAX as u64 {
                    return Err(bad("holds an empty or overlong span of zeros"));
                }
                KindIn::Zero { len: len as u32 }
            }
            n => {
                let len = (n - 1) as usize;
                if rest.len() < len {
                    return Err(bad("is truncated"));
                }
                let offset = content.len() - rest.len();
                rest = &rest[len..];
                KindIn::Inline {
                    offset,
                    len: len as u32,
                }
            }
        };
        let next = if implicit {
            id + 1
        } else {
            match get(&mut rest)? {
                0 => 0,
                z => {
                    let n = id as i64 + kladde_varint::unzigzag(z);
                    if n <= 0 || n >= u32::MAX as i64 || n == id as i64 {
                        return Err(bad("links a span to itself or out of bounds"));
                    }
                    n as u64
                }
            }
        };
        if next >= u32::MAX as u64 {
            return Err(bad("links a span out of bounds"));
        }
        out.entries.push(EntryIn {
            id: id as u32,
            next: next as u32,
            kind,
            len: (content.len() - rest.len() - start) as u16,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_round_trip_and_estimates_are_close() {
        let payload = [7u8; 40];
        let a = 3 * PAGE_SIZE as u64 + CONTENT_OFFSET as u64;
        let entries = [
            EntryOut {
                id: 1,
                next: 2,
                body: Body::Ref { len: 100, addr: a },
            },
            EntryOut {
                id: 2,
                next: 3,
                body: Body::Ref {
                    len: 1000,
                    addr: a + 100,
                },
            },
            EntryOut {
                id: 3,
                next: 900,
                body: Body::Inline(&payload),
            },
            EntryOut {
                id: 900,
                next: 0,
                body: Body::Zero { len: 1 << 20 },
            },
            EntryOut {
                id: 901,
                next: 4,
                body: Body::Ref {
                    len: 4081,
                    addr: 7 * PAGE_SIZE as u64 + CONTENT_OFFSET as u64,
                },
            },
        ];
        let mut out = Vec::new();
        let placed = encode(&[2, 5, 9], &entries, &mut out);
        let d = decode(&out).unwrap();
        assert_eq!(d.children, [2, 5, 9]);
        assert_eq!(d.entries.len(), entries.len());
        for ((e, i), p) in entries.iter().zip(&d.entries).zip(&placed) {
            assert_eq!(e.id, i.id);
            assert_eq!(e.next, i.next);
            assert_eq!(p.len, i.len);
            match (e.body, i.kind) {
                (Body::Ref { len, addr }, KindIn::Ref { len: l, addr: b }) => {
                    assert_eq!((len, addr), (l, b))
                }
                (Body::Zero { len }, KindIn::Zero { len: l }) => assert_eq!(len, l),
                (Body::Inline(pl), KindIn::Inline { offset, len }) => {
                    assert_eq!(&out[offset..offset + len as usize], pl);
                    assert_eq!(offset, p.payload);
                }
                other => panic!("{other:?}"),
            }
            let est = estimate(e.id, e.next, &e.body);
            assert!(
                est.abs_diff(p.len as usize) <= 3,
                "{e:?}: {est} vs {}",
                p.len
            );
        }
        // A reference of a page's length that follows its predecessor in id
        // and in the page takes five bytes.
        assert_eq!(placed[1].len, 5);
    }

    #[test]
    fn malformed_pages_are_refused() {
        // A reference into the header slots.
        let mut out = Vec::new();
        encode(
            &[],
            &[EntryOut {
                id: 1,
                next: 0,
                body: Body::Ref { len: 4, addr: 100 },
            }],
            &mut out,
        );
        assert!(decode(&out).is_err());
        // An entry linking to itself.
        let mut bad = vec![0, 1, 2, 1];
        bad.push(0); // a Zero of one byte, then next = zigzag(0)...
        assert!(decode(&[0, 1, 2, 1, 0]).is_ok());
        bad.truncate(4);
        bad.push(kladde_varint::zigzag(0) as u8);
        // zigzag(0) is 0, the end of the chain, so this links nowhere.
        assert!(decode(&bad).is_ok());
        // A truncated inline payload.
        assert!(decode(&[0, 1, 20, 1, 2]).is_err());
    }
}
