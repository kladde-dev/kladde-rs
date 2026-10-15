//! Closing into a fully consolidated file
//! (`impl/flush.md#closing-into-a-consolidated-file`): the journal folded in
//! memory, the schema, the sequence and the private chain written back to
//! back into full data pages of a new file, one `Ref` for each run of a chain
//! within a page, and the new file renamed over the old one.
//!
//! Nothing is flushed into the old file, and the new file's bytes pass
//! through one page buffer: the sequence is never held whole.

use crate::consts::*;
use crate::entries::{children_len, Body, EntryOut, PageEncoder};
use crate::error::{corrupt, Error};
use crate::fold::{fold, Src};
use crate::page::{decode_page, encode_page, new_page, seal_page, HeaderFields, PageBuf};
use crate::state::{Content, RopeItem};
use crate::storage::Storage;
use crate::store::Inner;
use crate::treap::NIL;

/// Fills consecutive data pages from page 2 on, and records the spans the
/// bytes of the chain being written take: address and length.
struct Packer<'a> {
    out: &'a mut dyn Storage,
    epoch: u64,
    buf: Box<PageBuf>,
    page: u32,
    used: usize,
    spans: Vec<(u64, u32)>,
}

impl Packer<'_> {
    fn push(&mut self, mut bytes: &[u8]) -> Result<(), Error> {
        while !bytes.is_empty() {
            let n = bytes.len().min(MAX_PAGE_CONTENT - self.used);
            let at = CONTENT_OFFSET + self.used;
            self.buf[at..at + n].copy_from_slice(&bytes[..n]);
            let addr = self.page as u64 * PAGE_SIZE as u64 + at as u64;
            match self.spans.last_mut() {
                Some((a, len)) if *a + *len as u64 == addr => *len += n as u32,
                _ => self.spans.push((addr, n as u32)),
            }
            self.used += n;
            bytes = &bytes[n..];
            if self.used == MAX_PAGE_CONTENT {
                self.write_page()?;
            }
        }
        Ok(())
    }

    fn push_zeros(&mut self, mut n: u64) -> Result<(), Error> {
        static ZEROS: [u8; MAX_PAGE_CONTENT] = [0; MAX_PAGE_CONTENT];
        while n > 0 {
            let k = n.min(MAX_PAGE_CONTENT as u64);
            self.push(&ZEROS[..k as usize])?;
            n -= k;
        }
        Ok(())
    }

    fn write_page(&mut self) -> Result<(), Error> {
        seal_page(&mut self.buf, None, KIND_DATA, self.epoch, self.used);
        self.out
            .write_at(&self.buf[..], self.page as u64 * PAGE_SIZE as u64)?;
        self.page += 1;
        self.used = 0;
        Ok(())
    }

    /// The spans of the chain written since the last call.
    fn end_chain(&mut self) -> Vec<(u64, u32)> {
        std::mem::take(&mut self.spans)
    }

    /// Writes the last page, if it is partly filled; returns the page after
    /// the data pages.
    fn finish(mut self) -> Result<u32, Error> {
        if self.used > 0 {
            self.write_page()?;
        }
        Ok(self.page)
    }
}

impl Inner {
    /// Writes everything recorded outside an open transaction into a fully
    /// consolidated file that replaces this one.
    pub(crate) fn close_consolidated(&mut self) -> Result<(), Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        self.join_pending()?;
        let mut target = self.storage.sibling()?;
        // The old file keeps everything until the new one replaces it.
        let committed = self.append_ready()?;
        let pieces = fold(
            self.ops.range(self.ops.front(), committed),
            self.state.seq_len(),
            self.ops.front().at,
        )?;
        let schema = self.schema_pending.as_ref().unwrap_or(&self.schema);
        let private = self.private_pending.as_ref().unwrap_or(&self.private);
        let seq_len: u64 = pieces.iter().map(|p| p.len).sum();
        let total = schema.len() as u64 + seq_len + private.len() as u64;
        let data_pages = total.div_ceil(MAX_PAGE_CONTENT as u64);
        if 2 + data_pages >= u32::MAX as u64 {
            return Err(Error::OutOfBounds);
        }
        // Even, so that the header goes to slot 0, and page 1 is left blank.
        let epoch = (self.epoch + 2) & !1;
        target.set_len((2 + data_pages) * PAGE_SIZE as u64)?;

        // 1. The data pages.
        let mut p = Packer {
            out: &mut *target,
            epoch,
            buf: new_page(),
            page: 2,
            used: 0,
            spans: Vec::new(),
        };
        p.push(schema)?;
        let schema_spans = p.end_chain();
        let mut cache = None;
        for piece in &pieces {
            match piece.src {
                Src::Committed(s) => self.copy_committed(s, piece.len, &mut p, &mut cache)?,
                Src::Arena(a) => {
                    let (x, y) = self.ops.slices(a, piece.len as usize);
                    p.push(x)?;
                    p.push(y)?;
                }
            }
        }
        let seq_spans = p.end_chain();
        p.push(private)?;
        let private_spans = p.end_chain();
        let leaf0 = p.finish()?;

        // 2. The span table: ids in chain order, each linking to the next.
        let mut entries = Vec::new();
        let mut heads = [0u32; 3];
        for (c, spans) in [schema_spans, seq_spans, private_spans].iter().enumerate() {
            for (i, &(addr, len)) in spans.iter().enumerate() {
                let id = entries.len() as u32 + 1;
                if i == 0 {
                    heads[c] = id;
                }
                let next = if i + 1 < spans.len() { id + 1 } else { 0 };
                entries.push(EntryOut {
                    id,
                    next,
                    body: Body::Ref { len, addr },
                });
            }
        }
        let mut header = PageEncoder::new(&[], MAX_HEADER_CONTENT);
        let mut leaves: Vec<Vec<u8>> = Vec::new();
        if !entries.iter().all(|e| header.push(e)) {
            let mut leaf = PageEncoder::new(&[], MAX_PAGE_CONTENT);
            for e in &entries {
                if !leaf.push(e) {
                    leaves.push(
                        std::mem::replace(&mut leaf, PageEncoder::new(&[], MAX_PAGE_CONTENT)).out,
                    );
                    assert!(leaf.push(e), "an entry fits an empty leaf");
                }
            }
            leaves.push(leaf.out);
            let children: Vec<u32> = (leaf0..leaf0 + leaves.len() as u32).collect();
            if children_len(&children) > MAX_HEADER_CONTENT {
                return Err(Error::OutOfBounds);
            }
            header = PageEncoder::new(&children, MAX_HEADER_CONTENT);
        }
        let file_pages = leaf0 + leaves.len() as u32;
        let mut buf = new_page();
        if !leaves.is_empty() {
            target.set_len(file_pages as u64 * PAGE_SIZE as u64)?;
        }
        for (i, content) in leaves.iter().enumerate() {
            encode_page(&mut buf, None, KIND_TABLE, epoch, content);
            target.write_at(&buf[..], (leaf0 as u64 + i as u64) * PAGE_SIZE as u64)?;
        }
        let fields = HeaderFields {
            format_version: FORMAT_VERSION,
            min_reader_version: MIN_READER_VERSION,
            schema_head: heads[0],
            sequence_head: heads[1],
            private_head: heads[2],
            root_fingerprint: self.header.root_fingerprint,
            // Past the file: the journal is empty.
            journal_pointer: file_pages,
        };
        encode_page(&mut buf, Some(&fields), KIND_TABLE, epoch, &header.out);
        target.write_at(&buf[..], 0)?;

        // 3. Durable, then in the old file's place.
        target.sync()?;
        self.storage.replace_with(target)?;
        // What is in memory describes the old file.
        self.poisoned = true;
        Ok(())
    }

    /// Writes `len` bytes of the committed sequence, from position `pos` on.
    fn copy_committed(
        &self,
        pos: u64,
        len: u64,
        p: &mut Packer,
        cache: &mut Option<(u32, Box<PageBuf>)>,
    ) -> Result<(), Error> {
        let past = || corrupt("a piece reaches past the committed sequence");
        let (mut node, mut off) = self
            .state
            .rope
            .find(self.state.root, pos)
            .ok_or_else(past)?;
        let mut left = len;
        while left > 0 {
            if node == NIL {
                return Err(past());
            }
            let RopeItem::Span { id, len: n } = *self.state.rope.item(node) else {
                unreachable!("a literal in the rope between flushes");
            };
            let take = (n as u64 - off).min(left);
            let (at, k) = (off as usize, take as usize);
            match self.state.span(id).content {
                Content::Ref { page, offset } => {
                    let buf = self.committed_page(page, cache)?;
                    let a = offset as usize + at;
                    p.push(&buf[a..a + k])?;
                }
                Content::Inline { page, offset } => {
                    let a = offset as usize + at;
                    p.push(&self.table_pages[&page][a..a + k])?;
                }
                Content::Zero => p.push_zeros(take)?,
                other => unreachable!("span {id} shows {other:?} between flushes"),
            }
            left -= take;
            off = 0;
            node = self.state.rope.next(node);
        }
        Ok(())
    }

    /// A data page of the committed file: from the mirror, or read into
    /// `cache`, which keeps the last page read.
    fn committed_page<'a>(
        &'a self,
        page: u32,
        cache: &'a mut Option<(u32, Box<PageBuf>)>,
    ) -> Result<&'a PageBuf, Error> {
        if let Some(buf) = self.mirror.get(&page) {
            return Ok(buf);
        }
        if cache.as_ref().is_none_or(|(p, _)| *p != page) {
            let mut buf = match cache.take() {
                Some((_, buf)) => buf,
                None => new_page(),
            };
            self.storage
                .read_at(&mut buf[..], page as u64 * PAGE_SIZE as u64)?;
            decode_page(&buf, false)
                .map_err(|_| corrupt(format!("data page {page} fails its checksum")))?;
            *cache = Some((page, buf));
        }
        Ok(&cache.as_ref().expect("just filled").1)
    }
}
