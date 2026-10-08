//! Address-table statements and the `content` codec of address-table pages
//! (`spec/address-table.md#physical-format`).

use crate::consts::{MAX_INLINE, PAGE_SIZE};
use crate::error::{corrupt, Error};

/// The five statement types; `Ref`, `Zero`, and `Inline` come in a plain and
/// a sizing variant, told apart by [`Stmt::sizing`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Kind {
    Ref = 0,
    Zero = 1,
    Size = 2,
    Tombstone = 3,
    Inline = 4,
}

impl Kind {
    /// `Ref`, `Zero`, and `Inline` state content over a range.
    pub fn is_content(self) -> bool {
        matches!(self, Kind::Ref | Kind::Zero | Kind::Inline)
    }
}

/// A statement, without an inline payload's bytes.
///
/// `offset` is the statement's offset for `Ref`, `Zero` and `Inline`, the size
/// `n` for `Size`, and 0 for `Tombstone`. `size` is 0 for the two that carry
/// none. `address` is the file address of a `Ref`'s bytes. `sizing` marks the
/// sizing variant of a content statement, which also states that the
/// allocation ends where its content ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stmt {
    pub id: u32,
    pub kind: Kind,
    pub offset: u32,
    pub size: u32,
    pub address: u64,
    pub sizing: bool,
}

impl Stmt {
    pub fn tombstone(id: u32) -> Stmt {
        Stmt {
            id,
            kind: Kind::Tombstone,
            offset: 0,
            size: 0,
            address: 0,
            sizing: false,
        }
    }
    /// `Size(id, n)`.
    pub fn size_to(id: u32, n: u32) -> Stmt {
        Stmt {
            id,
            kind: Kind::Size,
            offset: n,
            size: 0,
            address: 0,
            sizing: false,
        }
    }
    pub fn zero(id: u32, offset: u32, size: u32) -> Stmt {
        Stmt {
            id,
            kind: Kind::Zero,
            offset,
            size,
            address: 0,
            sizing: false,
        }
    }
    pub fn reference(id: u32, offset: u32, size: u32, address: u64) -> Stmt {
        Stmt {
            id,
            kind: Kind::Ref,
            offset,
            size,
            address,
            sizing: false,
        }
    }
    pub fn inline(id: u32, offset: u32, size: u32) -> Stmt {
        Stmt {
            id,
            kind: Kind::Inline,
            offset,
            size,
            address: 0,
            sizing: false,
        }
    }

    /// The sizing variant of a content statement.
    pub fn sized(self) -> Stmt {
        debug_assert!(self.kind.is_content());
        Stmt {
            sizing: true,
            ..self
        }
    }

    /// Whether it states its allocation's size: a `Size`, a `Tombstone`, or
    /// a sizing content statement.
    pub fn states_size(&self) -> bool {
        self.sizing || matches!(self.kind, Kind::Size | Kind::Tombstone)
    }

    /// The size it states, if [`Stmt::states_size`].
    pub fn stated_size(&self) -> u32 {
        match self.kind {
            Kind::Size => self.offset,
            Kind::Tombstone => 0,
            _ => self.offset + self.size,
        }
    }

    /// Where the statement's match range starts, which is also its sort key.
    pub fn start(&self) -> u32 {
        self.offset
    }

    /// Where its match range ends: `offset + size` for content, and `offset`
    /// (an empty range) for `Size` and `Tombstone`, which match nothing.
    pub fn end(&self) -> u64 {
        self.offset as u64 + self.size as u64
    }

    /// The sort key within a page.
    pub fn sort_key(&self) -> u64 {
        (self.id as u64) << 32 | self.offset as u64
    }
}

/// A statement as decoded from a page: the statement, where its encoding
/// starts in the page, its framing length, and for an `Inline` where its
/// payload starts in the page (in `address`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decoded {
    pub stmt: Stmt,
    pub framing: u8,
}

fn varint_len(v: u64) -> usize {
    kladde_varint::encoded_len(v)
}

fn put_varint(buf: &mut Vec<u8>, v: u64) {
    kladde_varint::encode(v, buf);
}

/// Where the statements of a page stand in its delta encoding: whether the
/// child list is closed, and the cursors the next statement is encoded
/// against. [`TableWriter`] and [`TableLen`] share it, so that what one
/// encodes and the other counts cannot disagree.
#[derive(Clone, Copy, Debug, Default)]
struct Cursors {
    children_closed: bool,
    id_cursor: u32,
    offset_cursor: u32,
}

impl Cursors {
    /// The offset cursor `s` is delta-encoded against: the current one if
    /// `s` names the same id, else 0.
    #[inline]
    fn offset_base(&self, s: &Stmt) -> u32 {
        if s.id != self.id_cursor {
            0
        } else {
            self.offset_cursor
        }
    }

    /// The encoded length `s` adds, inline payload included, and the child
    /// list's delimiter if `s` is the first statement.
    #[inline]
    fn statement_len(&self, s: &Stmt) -> usize {
        let id_delta = s.id - self.id_cursor;
        let oc = self.offset_base(s);
        let mut len = varint_len(id_delta as u64) + 1 + usize::from(!self.children_closed);
        match s.kind {
            Kind::Ref => {
                len += varint_len((s.offset - oc) as u64)
                    + varint_len(s.size as u64)
                    + varint_len(s.address)
            }
            Kind::Zero => len += varint_len((s.offset - oc) as u64) + varint_len(s.size as u64),
            Kind::Size => len += varint_len((s.offset - oc) as u64),
            Kind::Tombstone => {}
            Kind::Inline => len += varint_len((s.offset - oc) as u64) + s.size as usize,
        }
        len
    }

    /// Moves past `s`, as its encoding leaves the cursors.
    #[inline]
    fn advance(&mut self, s: &Stmt) {
        self.children_closed = true;
        if s.id != self.id_cursor {
            self.offset_cursor = 0;
        }
        match s.kind {
            Kind::Ref | Kind::Zero | Kind::Inline => self.offset_cursor = s.offset + s.size,
            Kind::Size => self.offset_cursor = s.offset,
            Kind::Tombstone => {}
        }
        self.id_cursor = s.id;
    }
}

/// Encodes the `content` of an address-table page, child references first.
#[derive(Clone, Debug, Default)]
pub struct TableWriter {
    buf: Vec<u8>,
    page_cursor: u32,
    cur: Cursors,
}

/// Measures the `content` of an address-table page without a child list as
/// [`TableWriter`] would encode it, without encoding anything: for a layout
/// that needs to know what statements take before it writes a page.
#[derive(Clone, Copy, Debug, Default)]
pub struct TableLen {
    len: usize,
    cur: Cursors,
}

impl TableLen {
    pub fn new() -> Self {
        Self::default()
    }

    /// Counts the empty child list's delimiter, as
    /// [`TableWriter::end_children`] writes it. A first statement counts it
    /// too, if this was not called.
    pub fn end_children(&mut self) {
        debug_assert!(!self.cur.children_closed);
        self.len += 1;
        self.cur.children_closed = true;
    }

    /// The content length so far.
    pub fn len(&self) -> usize {
        self.len
    }

    /// The encoded length `s` would add, inline payload included, given what
    /// was counted before it.
    #[inline]
    pub fn statement_len(&self, s: &Stmt) -> usize {
        self.cur.statement_len(s)
    }

    /// Counts `s`, under the order [`TableWriter::push`] requires.
    #[inline]
    pub fn push(&mut self, s: &Stmt) {
        debug_assert!(s.id >= self.cur.id_cursor, "statements out of id order");
        debug_assert!(
            s.kind == Kind::Tombstone || s.offset >= self.cur.offset_base(s),
            "offset cursor moves backwards"
        );
        self.len += self.cur.statement_len(s);
        self.cur.advance(s);
    }
}

/// Where a pushed statement landed.
#[derive(Clone, Copy, Debug)]
pub struct Pushed {
    /// The encoded length without an inline payload.
    pub framing: u8,
    /// For an `Inline`, where its payload starts within the content.
    pub payload_pos: usize,
}

impl TableWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The encoded length of the child list `children`, with its delimiter.
    pub fn children_len(children: &[u32]) -> usize {
        let mut cursor = 0u32;
        let mut len = 1;
        for &c in children {
            len += varint_len((c - cursor) as u64);
            cursor = c;
        }
        len
    }

    /// Appends a child reference; children must ascend.
    pub fn child(&mut self, page: u32) {
        assert!(!self.cur.children_closed && page > self.page_cursor && page >= 2);
        put_varint(&mut self.buf, (page - self.page_cursor) as u64);
        self.page_cursor = page;
    }

    /// Ends the child list. Called once, before the first statement.
    pub fn end_children(&mut self) {
        assert!(!self.cur.children_closed);
        self.buf.push(0);
        self.cur.children_closed = true;
    }

    /// The content length so far.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// The length `s` would take as the first statement of a page, which by
    /// subadditivity bounds what it adds anywhere.
    pub fn standalone_len(s: &Stmt) -> usize {
        Cursors {
            children_closed: true,
            ..Default::default()
        }
        .statement_len(s)
    }

    /// Appends `s`, whose inline payload, if any, is `payload`. Statements
    /// must be pushed in `(id, offset)` order and must never move the offset
    /// cursor backwards.
    pub fn push(&mut self, s: &Stmt, payload: &[u8]) -> Pushed {
        if !self.cur.children_closed {
            self.end_children();
        }
        assert!(s.id >= self.cur.id_cursor, "statements out of id order");
        let id_delta = s.id - self.cur.id_cursor;
        let start = self.buf.len();
        put_varint(&mut self.buf, id_delta as u64);
        let oc = self.cur.offset_base(s);
        let mut payload_pos = 0;
        let sizing = u8::from(s.sizing);
        debug_assert!(!s.sizing || s.kind.is_content());
        match s.kind {
            Kind::Ref => {
                assert!(s.offset >= oc, "offset cursor moves backwards");
                self.buf.push(sizing);
                put_varint(&mut self.buf, (s.offset - oc) as u64);
                put_varint(&mut self.buf, s.size as u64);
                put_varint(&mut self.buf, s.address);
            }
            Kind::Zero => {
                assert!(s.offset >= oc, "offset cursor moves backwards");
                self.buf.push(2 | sizing);
                put_varint(&mut self.buf, (s.offset - oc) as u64);
                put_varint(&mut self.buf, s.size as u64);
            }
            Kind::Tombstone => {
                self.buf.push(4);
            }
            Kind::Size => {
                assert!(s.offset >= oc, "offset cursor moves backwards");
                self.buf.push(5);
                put_varint(&mut self.buf, (s.offset - oc) as u64);
            }
            Kind::Inline => {
                assert!(s.offset >= oc, "offset cursor moves backwards");
                assert!(s.size >= 1 && s.size as usize <= MAX_INLINE);
                assert_eq!(payload.len(), s.size as usize);
                self.buf.push((4 + 2 * s.size) as u8 | sizing);
                put_varint(&mut self.buf, (s.offset - oc) as u64);
                payload_pos = self.buf.len();
                self.buf.extend_from_slice(payload);
            }
        }
        self.cur.advance(s);
        let framing = (self.buf.len()
            - start
            - if s.kind == Kind::Inline {
                s.size as usize
            } else {
                0
            }) as u8;
        Pushed {
            framing,
            payload_pos,
        }
    }

    /// The finished content.
    pub fn into_content(mut self) -> Vec<u8> {
        if !self.cur.children_closed {
            self.end_children();
        }
        self.buf
    }
}

/// Decodes the `content` of an address-table page. Positions are absolute
/// within the page, so a decoded `Inline`'s `address` is its payload's
/// offset in the page.
pub struct TableReader<'a> {
    page: &'a [u8],
    pos: usize,
    end: usize,
    id_cursor: u64,
    offset_cursor: u64,
}

fn get_varint(page: &[u8], pos: &mut usize, end: usize) -> Result<u64, Error> {
    let (v, rest) = kladde_varint::decode(&page[*pos..end])
        .map_err(|_| corrupt("truncated varint in an address-table page"))?;
    *pos = end - rest.len();
    Ok(v)
}

impl<'a> TableReader<'a> {
    /// Reads the child list of the page whose `content` is `page[content]`,
    /// and returns it with a reader positioned at the first statement.
    pub fn open(
        page: &'a [u8],
        content: std::ops::Range<usize>,
    ) -> Result<(Vec<u32>, Self), Error> {
        let mut pos = content.start;
        let end = content.end;
        let mut children = Vec::new();
        let mut cursor: u64 = 0;
        loop {
            if pos >= end {
                return Err(corrupt("address-table page ends inside its child list"));
            }
            let delta = get_varint(page, &mut pos, end)?;
            if delta == 0 {
                break;
            }
            cursor += delta;
            if cursor > u32::MAX as u64 || cursor < 2 {
                return Err(corrupt("child reference out of range"));
            }
            children.push(cursor as u32);
        }
        Ok((
            children,
            TableReader {
                page,
                pos,
                end,
                id_cursor: 0,
                offset_cursor: 0,
            },
        ))
    }

    /// The next statement, or `None` at the end of the content.
    pub fn next_stmt(&mut self) -> Result<Option<Decoded>, Error> {
        if self.pos >= self.end {
            return Ok(None);
        }
        let start = self.pos;
        let page = self.page;
        let end = self.end;
        let id_delta = get_varint(page, &mut self.pos, end)?;
        self.id_cursor += id_delta;
        if self.id_cursor == 0 || self.id_cursor > u32::MAX as u64 {
            return Err(corrupt("statement id out of range"));
        }
        if id_delta != 0 {
            self.offset_cursor = 0;
        }
        if self.pos >= end {
            return Err(corrupt("statement without a tag"));
        }
        let tag = page[self.pos];
        self.pos += 1;
        let id = self.id_cursor as u32;
        let bound = |v: u64| -> Result<u32, Error> {
            if v > u32::MAX as u64 {
                Err(corrupt("statement offset out of range"))
            } else {
                Ok(v as u32)
            }
        };
        let (mut stmt, payload) = match tag {
            0..=3 => {
                self.offset_cursor += get_varint(page, &mut self.pos, end)?;
                let offset = bound(self.offset_cursor)?;
                let size = get_varint(page, &mut self.pos, end)?;
                self.offset_cursor += size;
                bound(self.offset_cursor)?;
                if tag < 2 {
                    let address = get_varint(page, &mut self.pos, end)?;
                    (Stmt::reference(id, offset, size as u32, address), 0)
                } else {
                    (Stmt::zero(id, offset, size as u32), 0)
                }
            }
            4 => (Stmt::tombstone(id), 0),
            5 => {
                self.offset_cursor += get_varint(page, &mut self.pos, end)?;
                (Stmt::size_to(id, bound(self.offset_cursor)?), 0)
            }
            _ => {
                let size = ((tag - 4) >> 1) as u32;
                self.offset_cursor += get_varint(page, &mut self.pos, end)?;
                let offset = bound(self.offset_cursor)?;
                if self.pos + size as usize > end {
                    return Err(corrupt("inline payload runs past the page content"));
                }
                let mut s = Stmt::inline(id, offset, size);
                s.address = self.pos as u64;
                self.pos += size as usize;
                self.offset_cursor += size as u64;
                bound(self.offset_cursor)?;
                (s, size as usize)
            }
        };
        // Bit 0 of a content statement's tag is its sizing bit.
        stmt.sizing = stmt.kind.is_content() && tag & 1 == 1;
        if stmt.kind == Kind::Ref {
            let in_page = (stmt.address % PAGE_SIZE as u64) as usize;
            if in_page + stmt.size as usize > PAGE_SIZE {
                return Err(corrupt("a Ref spans a page boundary"));
            }
        }
        let framing = (self.pos - start - payload) as u8;
        Ok(Some(Decoded { stmt, framing }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::{CONTENT_OFFSET, KIND_ADDRESS_TABLE};
    use crate::page::{decode_page, encode_page, new_page};

    #[test]
    fn a_page_of_every_statement_kind_round_trips() {
        let stmts = [
            (Stmt::reference(3, 0, 100, 4096 * 5 + 11), vec![]),
            (Stmt::zero(3, 100, 20), vec![]),
            (Stmt::size_to(3, 120), vec![]),
            (Stmt::tombstone(7), vec![]),
            (Stmt::inline(9, 0, 3), vec![1, 2, 3]),
            (Stmt::zero(9, 3, 47).sized(), vec![]),
            (Stmt::reference(1000, 7, 1, 11).sized(), vec![]),
            (Stmt::inline(1001, 0, 1).sized(), vec![9]),
            (Stmt::inline(1002, 0, 125), vec![7; 125]),
            (Stmt::inline(1003, 0, 125).sized(), vec![8; 125]),
        ];
        let mut w = TableWriter::new();
        w.child(4);
        w.child(9);
        // Measured from after the child list, which the first statement
        // closes in both.
        let mut measure = TableLen::new();
        let mut lens = Vec::new();
        for (s, p) in &stmts {
            let predicted = measure.statement_len(s);
            let before = w.len();
            let pushed = w.push(s, p);
            measure.push(s);
            lens.push(pushed.framing);
            assert_eq!(w.len() - before, predicted);
        }
        assert_eq!(
            w.len(),
            TableWriter::children_len(&[4, 9]) - 1 + measure.len()
        );
        let content = w.into_content();
        let mut page = new_page();
        encode_page(&mut page, None, KIND_ADDRESS_TABLE, 4, &content);
        let d = decode_page(&page, false).unwrap();
        let (children, mut r) = TableReader::open(&page[..], d.content).unwrap();
        assert_eq!(children, [4, 9]);
        for (i, (s, p)) in stmts.iter().enumerate() {
            let got = r.next_stmt().unwrap().unwrap();
            let mut expect = *s;
            if s.kind == Kind::Inline {
                let pos = got.stmt.address as usize;
                assert_eq!(&page[pos..pos + p.len()], &p[..]);
                expect.address = got.stmt.address;
            }
            assert_eq!(got.stmt, expect);
            assert_eq!(got.framing, lens[i]);
        }
        assert!(r.next_stmt().unwrap().is_none());
        let _ = CONTENT_OFFSET;
    }

    /// A page's statements in the order a writer pushes them, chosen by a
    /// fixed xorshift sequence from `seed`: every kind, both variants, and
    /// deltas, sizes and addresses of every varint width.
    fn random_page(seed: u64) -> Vec<(Stmt, Vec<u8>)> {
        let mut x = seed | 1;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        // A number below 2^bits, for a bit count up to `max_bits`.
        let below = |max_bits: u64, next: &mut dyn FnMut() -> u64| {
            let bits = next() % (max_bits + 1);
            if bits == 0 {
                0
            } else {
                next() >> (64 - bits)
            }
        };
        let mut out = Vec::new();
        let mut id = below(8, &mut next) as u32;
        for i in 0..(next() % 300) {
            if i > 0 {
                let Some(n) = id.checked_add(1 + below(31, &mut next) as u32) else {
                    break;
                };
                id = n;
            }
            if next() % 8 == 0 {
                out.push((Stmt::tombstone(id), vec![]));
                continue;
            }
            let mut cursor = 0u64;
            for _ in 0..(1 + next() % 6) {
                let offset = cursor + below(24, &mut next);
                let (stmt, payload) = match next() % 3 {
                    0 => {
                        let size = 1 + next() % MAX_INLINE as u64;
                        let s = Stmt::inline(id, offset as u32, size as u32);
                        (s, vec![0xa5; size as usize])
                    }
                    1 => {
                        let size = 1 + below(16, &mut next);
                        (Stmt::zero(id, offset as u32, size as u32), vec![])
                    }
                    _ => {
                        let size = 1 + below(16, &mut next);
                        let address = below(49, &mut next);
                        (
                            Stmt::reference(id, offset as u32, size as u32, address),
                            vec![],
                        )
                    }
                };
                if stmt.end() > u32::MAX as u64 {
                    break;
                }
                cursor = stmt.end();
                out.push((stmt, payload));
            }
            if next() % 2 == 0 {
                let last = out.len() - 1;
                out[last].0 = out[last].0.sized();
            } else if cursor + 1 < u32::MAX as u64 {
                out.push((
                    Stmt::size_to(id, (cursor + below(8, &mut next)) as u32),
                    vec![],
                ));
            }
        }
        out
    }

    #[test]
    fn table_len_counts_what_table_writer_encodes() {
        for seed in 0..500u64 {
            let stmts = random_page(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            // With the child list closed first, and closed by the first
            // statement.
            for close_first in [true, false] {
                let mut w = TableWriter::new();
                let mut measure = TableLen::new();
                if close_first {
                    w.end_children();
                    measure.end_children();
                }
                for (s, p) in &stmts {
                    let predicted = measure.statement_len(s);
                    let before = w.len();
                    w.push(s, p);
                    measure.push(s);
                    assert_eq!(w.len() - before, predicted, "seed {seed}: {s:?}");
                    assert_eq!(w.len(), measure.len(), "seed {seed}: {s:?}");
                }
                assert_eq!(w.into_content().len(), measure.len().max(1), "seed {seed}");
            }
        }
    }

    #[test]
    fn the_framing_bound_from_the_specification_holds() {
        // 5 bytes of id, a tag, 5 of offset, 3 of size (64 KiB pages), 7 of address.
        let s = Stmt::reference(u32::MAX, u32::MAX - 65_521, 65_521, (1u64 << 49) - 1);
        assert_eq!(TableWriter::standalone_len(&s), 21);
    }
}
