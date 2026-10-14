//! The flush: fold the journal, map the new sequence onto spans, choose
//! which entries the header keeps and merge short spans as theirs leave it,
//! place new bytes in data pages, encode the span table, and commit.

use std::cmp::Reverse;
use std::collections::VecDeque;

use crate::consts::*;
use crate::entries::{children_len, estimate, Body, EntryOut, PageEncoder, Placed};
use crate::error::{corrupt, Error};
use crate::fold::{fold, Piece, Src};
use crate::hash::IdSet;
use crate::journal::{Cursor, JournalCuts};
use crate::page::{encode_page, new_page, seal_page, PageBuf};
use crate::state::*;
use crate::storage::Storage;
use crate::store::{take_page, Inner, PendingCommit, Tx};
use crate::treap::NIL;

/// A data page the flush fills.
pub(crate) struct DataPage {
    pub page: u32,
    pub buf: Box<PageBuf>,
    pub used: usize,
}

impl DataPage {
    pub fn room(&self) -> usize {
        MAX_PAGE_CONTENT - self.used
    }
}

/// What a span the flush made holds until it is placed: its bytes in the
/// flush's scratch, and whether its entry will hold them inline.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingBytes {
    pub start: u32,
    pub inline: bool,
}

/// What one flush keeps while it works.
#[derive(Default)]
pub(crate) struct Flush {
    /// Bytes of new spans and of runs that have no span yet.
    pub scratch: Vec<u8>,
    pub pending: Vec<PendingBytes>,
    /// Spans that get a new entry in this flush.
    pub stated: IdSet,
    /// Those of them whose entry goes to the header; the rest go to leaves.
    pub header: IdSet,
}

/// What the flush writes, gathered before the commit.
#[derive(Default)]
pub(crate) struct Output {
    pub data: Vec<DataPage>,
    /// Leaves: page and buffer.
    pub tables: Vec<(u32, Box<PageBuf>)>,
    pub header_content: Vec<u8>,
}

/// The largest run of bytes a span shows from a data page.
const C: u32 = MAX_PAGE_CONTENT as u32;

impl Inner {
    /// Appends a batch's ready run, which lies outside any open transaction,
    /// then folds the journal, even an empty one. Poisons the store if the
    /// flush fails.
    pub fn flush_now(&mut self) -> Result<(), Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        let end = self.ops.end();
        let (committed, ready) = match self.tx {
            Tx::Immediate => (end, end),
            Tx::InBatch { committed, .. } => (committed, end),
            Tx::InTransaction { committed, .. } => (committed, committed),
            Tx::InTxInBatch {
                committed, ready, ..
            } => (committed, ready),
        };
        let committed = self.append(committed, ready)?;
        self.tx = self.tx.with_committed(committed);
        // An explicit flush returns once what it folded is durable.
        self.explicit_flush = true;
        let r = self.fold_journal(committed);
        self.explicit_flush = false;
        r?;
        self.join_pending()
    }

    /// Waits for the commit on its own thread, if one is running, and makes
    /// the pages it waited to free reusable. A failure poisons the store.
    pub(crate) fn join_pending(&mut self) -> Result<(), Error> {
        let Some(pending) = self.pending.take() else {
            return Ok(());
        };
        match pending.worker.join() {
            Ok(Ok(())) => {
                for p in pending.release {
                    debug_assert_eq!(self.state.pages[p as usize], Page::Fallback);
                    self.state.pages[p as usize] = Page::Reusable;
                    self.ready.insert(p);
                }
                Ok(())
            }
            Ok(Err(e)) => {
                self.poisoned = true;
                Err(e.into())
            }
            Err(_) => {
                self.poisoned = true;
                Err(Error::Io(std::io::Error::other(
                    "the commit's thread panicked",
                )))
            }
        }
    }

    /// Flushes as [`flush_now`](Self::flush_now) does if the journal pages,
    /// with what an explicit flush would append to them, hold at least
    /// `fraction` of the budget; returns whether it flushed.
    pub fn flush_if_journal_exceeds(&mut self, fraction: f64) -> Result<bool, Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        let end = self.ops.end();
        let (committed, ready) = match self.tx {
            Tx::Immediate => (end, end),
            Tx::InBatch { committed, .. } => (committed, end),
            Tx::InTransaction { committed, .. } => (committed, committed),
            Tx::InTxInBatch {
                committed, ready, ..
            } => (committed, ready),
        };
        let held = self.journal.bytes + (ready.bytes - committed.bytes);
        let budget = self.opts.journal_budget_pages as f64 * JOURNAL_STREAM as f64;
        if (held as f64) < fraction * budget {
            return Ok(false);
        }
        self.flush_now()?;
        Ok(true)
    }

    /// Folds the operations the journal pages hold, `ops[..committed]`, and
    /// drops them from the buffer. Poisons the store if the flush fails.
    pub(crate) fn fold_journal(&mut self, committed: Cursor) -> Result<(), Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        match self.flush_inner(committed) {
            Ok(()) => {
                // The next journal starts at `committed`; the cuts of the
                // folded one have ended.
                self.ops.discard_to(committed);
                self.journal_cuts = JournalCuts::starting_at(committed.at);
                self.pasted.retain(|&cut, _| cut >= committed.at);
                Ok(())
            }
            Err(e) => {
                self.poisoned = true;
                Err(e)
            }
        }
    }

    /// Ends the journal with a seal that names the page where the next
    /// journal begins (`spec/journal.md#seals`), which the flush's header
    /// names too. The page is taken now, so that the flush's own pages
    /// cannot be it, and a session's first flush clears it before the seal
    /// names it (`spec/journal.md#the-start-of-a-session`).
    ///
    /// Without a reusable page it does not seal: a page added at the end
    /// now would lie below every page the flush adds after it, and keep the
    /// file from being truncated to them.
    fn seal(&mut self) -> Result<(), Error> {
        let Some(jp) = self.ready.pop_first() else {
            return Ok(());
        };
        if self.first_flush {
            // Durable before the seal names it: a power cut that kept the
            // seal and lost the zeros would let a lost journal of the same
            // epoch left on the page validate after the seal.
            self.storage
                .write_at(&new_page()[..], jp as u64 * PAGE_SIZE as u64)?;
            self.storage.sync()?;
        }
        self.state.pages[jp as usize] = Page::Journal;
        let payload = crate::journal::seal_payload(jp);
        let Inner {
            storage,
            journal,
            ready,
            state,
            file_pages,
            stats,
            ..
        } = self;
        let mut alloc = |s: &mut dyn Storage| -> Result<u32, Error> {
            take_page(ready, state, file_pages, s, stats, Page::Journal)
        };
        let before = journal.bytes;
        journal.append(&payload, &mut **storage, &mut alloc)?;
        self.stats.journal_bytes += self.journal.bytes - before;
        self.sealed_next = Some(jp);
        Ok(())
    }

    fn flush_inner(&mut self, committed: Cursor) -> Result<(), Error> {
        // One commit at a time: the pages the last one freed become
        // reusable only through its `fsync`.
        self.join_pending()?;
        if self.opts.seal && !self.journal.is_recovered() && !self.journal.is_empty() {
            self.seal()?;
        }
        let e = self.epoch + 1;
        self.state.flush_epoch = e;
        let mut f = Flush::default();
        self.begin_consolidation();

        // 1. The fold, and the new sequence mapped onto spans.
        let n0 = self.state.seq_len();
        let first = self.ops.front().at;
        let pieces = fold(self.ops.range(self.ops.front(), committed), n0, first)?;
        self.apply_pieces(&pieces, &mut f)?;
        self.write_small_chains(&mut f)?;

        // 2. The header's entries, and merges of short spans as theirs leave
        //    it; then the rotating walk's merges of cold spans.
        self.choose_header(&mut f);
        self.merge_evicted(&mut f)?;
        if self.opts.consolidate {
            self.walk(&mut f)?;
        }

        // 3. Data pages, with victims riding along, then budgeted pages.
        let mut out = Output::default();
        self.pack(&mut f, &mut out)?;
        if self.opts.consolidate {
            self.budget_loop(&mut f, &mut out)?;
        }

        // 4. The span table.
        self.cut(&mut f, &mut out)?;

        // 5. Write, fsync, commit.
        self.commit(e, out)
    }

    // ---------------------------------------------------------------- spans

    /// Ends span `id` within the flush.
    pub(crate) fn kill_in(&mut self, f: &mut Flush, id: u32) {
        self.state.kill(id);
        f.stated.remove(&id);
        f.header.remove(&id);
    }

    /// Appends the bytes span `id` shows to the scratch, and returns where
    /// they start there.
    pub(crate) fn span_to_scratch(&mut self, f: &mut Flush, id: u32) -> Result<u32, Error> {
        let start = f.scratch.len() as u32;
        let s = *self.state.span(id);
        let n = s.len as usize;
        match s.content {
            Content::Ref { page, offset } | Content::Inline { page, offset } => {
                let buf = self.page_bytes(page)?;
                f.scratch
                    .extend_from_slice(&buf[offset as usize..offset as usize + n]);
            }
            Content::Zero => f.scratch.resize(f.scratch.len() + n, 0),
            Content::Pending(i) => {
                let p = f.pending[i as usize].start as usize;
                f.scratch.extend_from_within(p..p + n);
            }
            Content::Free => return Err(corrupt(format!("reading free span {id}"))),
        }
        Ok(start)
    }

    /// Makes new spans of `len` bytes of the scratch from `start`, of chain
    /// `chain`, whose bytes the application wrote in flush `heat`: one
    /// inline span if they are short, else spans of at most a page each,
    /// and a span of zeros for a run of them. Returns their ids, in order,
    /// linked to each other but not to what follows.
    fn new_spans(
        &mut self,
        f: &mut Flush,
        start: u32,
        len: u32,
        chain: Chain,
        heat: u64,
        after: u32,
    ) -> Result<Vec<u32>, Error> {
        let inline_max = self.opts.inline_threshold.min(MAX_INLINE as u32);
        let mut out = Vec::new();
        let mut at = 0u32;
        let mut prev = after;
        while at < len {
            let n = if len <= inline_max {
                len
            } else {
                (len - at).min(C)
            };
            let bytes = &f.scratch[(start + at) as usize..(start + at + n) as usize];
            let zeros = n >= 32 && bytes.iter().all(|&b| b == 0);
            let content = if zeros {
                Content::Zero
            } else {
                f.pending.push(PendingBytes {
                    start: start + at,
                    inline: len <= inline_max,
                });
                Content::Pending((f.pending.len() - 1) as u32)
            };
            let id = self.state.new_span(prev).ok_or(Error::OutOfBounds)?;
            *self.state.span_mut(id) = Span {
                len: n,
                next: 0,
                content,
                chain,
                entry_page: NONE,
                heat,
                node: NIL,
                ..Span::default()
            };
            if let Some(&p) = out.last() {
                self.state.span_mut(p).next = id;
            }
            f.stated.insert(id);
            out.push(id);
            prev = id;
            at += n;
        }
        Ok(out)
    }

    /// The span a rope node holds.
    fn span_at(&self, node: u32) -> u32 {
        match *self.state.rope.item(node) {
            RopeItem::Span { id, .. } => id,
            RopeItem::Lit { .. } => unreachable!("a literal where a span was expected"),
        }
    }

    // ---------------------------------------------------------------- mapping

    /// Maps the new sequence, as the fold's pieces describe it, onto spans:
    /// committed ranges keep the spans that showed them, split where a range
    /// ends inside one, and new bytes become new spans; spans no piece
    /// shows end. Every span whose length or successor changed is stated.
    fn apply_pieces(&mut self, pieces: &[Piece], f: &mut Flush) -> Result<(), Error> {
        let n0 = self.state.seq_len();
        let unchanged = match pieces {
            [] => n0 == 0,
            [p] => p.src == Src::Committed(0) && p.len == n0,
            _ => false,
        };
        if unchanged {
            return Ok(());
        }
        let mut cuts: Vec<u64> = Vec::new();
        for p in pieces {
            if let Src::Committed(s) = p.src {
                cuts.push(s);
                cuts.push(s + p.len);
            }
        }
        cuts.retain(|&x| x > 0 && x < n0);
        cuts.sort_unstable();
        cuts.dedup();
        for &x in &cuts {
            self.boundary(x, f)?;
        }
        // The committed sequence, cut into segments at those positions.
        let mut segs: Vec<(u64, u32, bool)> = Vec::new();
        if n0 > 0 {
            let mut rest = self.state.root;
            let mut prev = 0u64;
            for &x in cuts.iter().chain(std::iter::once(&n0)) {
                let (seg, r) = self.state.rope.split(rest, x - prev);
                segs.push((prev, seg, false));
                rest = r;
                prev = x;
            }
            debug_assert_eq!(rest, NIL);
        }
        let mut root = NIL;
        let mut touched: Vec<u32> = Vec::new();
        for p in pieces {
            match p.src {
                Src::Committed(s) => {
                    let i = segs
                        .binary_search_by_key(&s, |x| x.0)
                        .map_err(|_| corrupt("a piece starts inside a segment"))?;
                    let seg = segs[i].1;
                    debug_assert_eq!(self.state.rope.sum(seg), p.len);
                    segs[i].2 = true;
                    touched.push(self.state.rope.first(seg));
                    touched.push(self.state.rope.last(seg));
                    root = self.state.rope.merge(root, seg);
                }
                Src::Arena(a) => {
                    let mut done = 0u64;
                    while done < p.len {
                        let n = (p.len - done).min(1 << 30);
                        let start = f.scratch.len();
                        f.scratch.resize(start + n as usize, 0);
                        self.ops.copy_bytes(a + done, &mut f.scratch[start..]);
                        self.stats.fresh_bytes += n;
                        let node = self.state.rope.node(RopeItem::Lit {
                            start: start as u32,
                            len: n as u32,
                        });
                        touched.push(node);
                        root = self.state.rope.merge(root, node);
                        done += n;
                    }
                }
            }
        }
        for (_, seg, used) in segs {
            if used {
                continue;
            }
            for n in self.state.rope.nodes(seg) {
                if let RopeItem::Span { id, .. } = *self.state.rope.item(n) {
                    self.kill_in(f, id);
                }
            }
            self.state.rope.free_tree(seg);
        }
        self.state.root = root;
        self.settle(f, touched)
    }

    /// Makes position `x` of the committed sequence a boundary between rope
    /// items: a long span of a data page or of zeros is split, its left part
    /// keeping its id; a short or inline span becomes a run of literal bytes,
    /// to be written again with whatever lands next to it.
    fn boundary(&mut self, x: u64, f: &mut Flush) -> Result<(), Error> {
        loop {
            let Some((node, off)) = self.state.rope.find(self.state.root, x) else {
                return Ok(());
            };
            if off == 0 {
                return Ok(());
            }
            let off32 = off as u32;
            match *self.state.rope.item(node) {
                RopeItem::Lit { start, len } => {
                    self.state
                        .rope
                        .set_item(node, RopeItem::Lit { start, len: off32 });
                    let m = self.state.rope.node(RopeItem::Lit {
                        start: start + off32,
                        len: len - off32,
                    });
                    self.insert_at(x, m);
                    return Ok(());
                }
                RopeItem::Span { id, len } => {
                    let s = *self.state.span(id);
                    let dissolve = match s.content {
                        Content::Inline { .. } => true,
                        Content::Ref { .. } => len < self.opts.rewrite_below,
                        _ => false,
                    };
                    if dissolve {
                        let start = self.span_to_scratch(f, id)?;
                        self.stats.rewritten_bytes += len as u64;
                        self.kill_in(f, id);
                        self.state.rope.set_item(node, RopeItem::Lit { start, len });
                        continue;
                    }
                    let content = match s.content {
                        Content::Ref { page, offset } => Content::Ref {
                            page,
                            offset: offset + off as u16,
                        },
                        Content::Zero => Content::Zero,
                        other => unreachable!("splitting a span that shows {other:?}"),
                    };
                    let t = self.state.new_span(id).ok_or(Error::OutOfBounds)?;
                    *self.state.span_mut(t) = Span {
                        len: len - off32,
                        next: s.next,
                        content,
                        chain: Chain::Sequence,
                        entry_page: NONE,
                        heat: s.heat,
                        node: NIL,
                        ..Span::default()
                    };
                    self.state.span_mut(id).len = off32;
                    self.state.span_mut(id).next = t;
                    self.state.note_data(t);
                    self.state
                        .rope
                        .set_item(node, RopeItem::Span { id, len: off32 });
                    let m = self.state.rope.node(RopeItem::Span {
                        id: t,
                        len: len - off32,
                    });
                    self.state.span_mut(t).node = m;
                    self.insert_at(x, m);
                    f.stated.insert(id);
                    f.stated.insert(t);
                    self.stats.span_splits += 1;
                    return Ok(());
                }
            }
        }
    }

    /// Inserts the one-node tree `m` at position `x` of the rope, which must
    /// fall between items.
    fn insert_at(&mut self, x: u64, m: u32) {
        let rope = &mut self.state.rope;
        let (l, r) = rope.split(self.state.root, x);
        let l = rope.merge(l, m);
        self.state.root = rope.merge(l, r);
    }

    /// Turns every run of literal bytes that `touched` reaches into new
    /// spans, and links every span around a touched node to the span that
    /// now follows it, stating those whose successor changed.
    fn settle(&mut self, f: &mut Flush, touched: Vec<u32>) -> Result<(), Error> {
        let e = self.state.flush_epoch;
        let mut gone = IdSet::default();
        let mut link: Vec<u32> = Vec::new();
        for &n in &touched {
            if n == NIL || gone.contains(&n) {
                continue;
            }
            match *self.state.rope.item(n) {
                RopeItem::Span { .. } => link.push(n),
                RopeItem::Lit { .. } => {
                    let rope = &self.state.rope;
                    let is_lit = |m: u32| m != NIL && matches!(rope.item(m), RopeItem::Lit { .. });
                    let mut a = n;
                    while is_lit(rope.prev(a)) {
                        a = rope.prev(a);
                    }
                    let mut run = Vec::new();
                    let mut m = a;
                    while is_lit(m) {
                        run.push(m);
                        m = rope.next(m);
                    }
                    let pos = rope.pos_of(a);
                    // The run's bytes, contiguous in the scratch.
                    let mut total = 0u64;
                    let mut contiguous = true;
                    let mut expect = None;
                    for &r in &run {
                        let RopeItem::Lit { start, len } = *rope.item(r) else {
                            unreachable!()
                        };
                        if expect.is_some_and(|x| x != start) {
                            contiguous = false;
                        }
                        expect = Some(start + len);
                        total += len as u64;
                    }
                    let start = if contiguous {
                        let RopeItem::Lit { start, .. } = *rope.item(a) else {
                            unreachable!()
                        };
                        start
                    } else {
                        let at = f.scratch.len() as u32;
                        for &r in &run {
                            let RopeItem::Lit { start, len } = *self.state.rope.item(r) else {
                                unreachable!()
                            };
                            f.scratch
                                .extend_from_within(start as usize..(start + len) as usize);
                        }
                        at
                    };
                    for &r in &run {
                        gone.insert(r);
                    }
                    let rope = &mut self.state.rope;
                    let (l, rest) = rope.split(self.state.root, pos);
                    let (mid, r) = rope.split(rest, total);
                    rope.free_tree(mid);
                    self.state.root = rope.merge(l, r);
                    let total32 = u32::try_from(total).map_err(|_| Error::OutOfBounds)?;
                    let ids = self.new_spans(f, start, total32, Chain::Sequence, e, 0)?;
                    let mut at = pos;
                    for id in ids {
                        let len = self.state.span(id).len;
                        let node = self.state.rope.node(RopeItem::Span { id, len });
                        self.state.span_mut(id).node = node;
                        gone.remove(&node);
                        self.insert_at(at, node);
                        at += len as u64;
                        link.push(node);
                    }
                }
            }
        }
        for n in link {
            if gone.contains(&n) {
                continue;
            }
            let p = self.state.rope.prev(n);
            for m in [p, n] {
                if m == NIL {
                    continue;
                }
                let id = self.span_at(m);
                let nx = match self.state.rope.next(m) {
                    NIL => 0,
                    k => self.span_at(k),
                };
                if self.state.span(id).next != nx {
                    self.state.span_mut(id).next = nx;
                    f.stated.insert(id);
                }
            }
        }
        Ok(())
    }

    /// Writes the schema's chain and the private chain anew, if their
    /// content was set.
    fn write_small_chains(&mut self, f: &mut Flush) -> Result<(), Error> {
        let e = self.state.flush_epoch;
        for chain in [Chain::Schema, Chain::Private] {
            let pending = match chain {
                Chain::Schema => self.schema_pending.take(),
                _ => self.private_pending.take(),
            };
            let Some(bytes) = pending else { continue };
            let old = match chain {
                Chain::Schema => std::mem::take(&mut self.state.schema),
                _ => std::mem::take(&mut self.state.private),
            };
            for id in old {
                self.kill_in(f, id);
            }
            let start = f.scratch.len() as u32;
            f.scratch.extend_from_slice(&bytes);
            let ids = if bytes.is_empty() {
                Vec::new()
            } else {
                let len = u32::try_from(bytes.len()).map_err(|_| Error::OutOfBounds)?;
                self.new_spans(f, start, len, chain, e, 0)?
            };
            match chain {
                Chain::Schema => {
                    self.state.schema = ids;
                    self.schema = bytes;
                }
                _ => {
                    self.state.private = ids;
                    self.private = bytes;
                }
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- the header

    /// What the entry of span `id` will be, its bytes borrowed from where
    /// they lie.
    fn body<'a>(&'a self, f: &'a Flush, id: u32) -> Body<'a> {
        let s = self.state.span(id);
        match s.content {
            Content::Ref { page, offset } => Body::Ref {
                len: s.len,
                addr: page as u64 * PAGE_SIZE as u64 + offset as u64,
            },
            Content::Zero => Body::Zero { len: s.len },
            Content::Inline { page, offset } => Body::Inline(
                &self.table_pages[&page][offset as usize..offset as usize + s.len as usize],
            ),
            Content::Pending(i) => {
                let p = f.pending[i as usize];
                if p.inline {
                    Body::Inline(&f.scratch[p.start as usize..(p.start + s.len) as usize])
                } else {
                    // Not placed yet: an estimate stands for its address.
                    Body::Ref {
                        len: s.len,
                        addr: 0,
                    }
                }
            }
            Content::Free => unreachable!("an entry for free span {id}"),
        }
    }

    fn estimate_of(&self, f: &Flush, id: u32) -> usize {
        let s = self.state.span(id);
        estimate(id, s.next, &self.body(f, id))
    }

    /// The live leaves, by page number.
    fn leaves(&self) -> Vec<u32> {
        let mut out: Vec<u32> = self
            .table_pages
            .keys()
            .copied()
            .filter(|&p| {
                matches!(
                    self.state.pages[p as usize],
                    Page::Live(Live {
                        kind: PageKind::Table,
                        ..
                    })
                )
            })
            .collect();
        out.sort_unstable();
        out
    }

    /// Chooses which entries the new header holds: the hottest of those the
    /// flush states and those the old header holds, as many as fit, by an
    /// estimate of their size; every one of them is stated.
    fn choose_header(&mut self, f: &mut Flush) {
        if let Some(list) = self.state.table_spans.get(&self.header_slot) {
            for &id in list {
                f.stated.insert(id);
            }
        }
        let mut cands: Vec<u32> = f.stated.iter().copied().collect();
        cands.sort_by_key(|&id| (Reverse(self.state.span(id).heat), id));
        let ests: Vec<usize> = cands.iter().map(|&id| self.estimate_of(f, id)).collect();
        let total: usize = ests.iter().sum();
        let children = children_len(&self.leaves()) + 3 * (total / MAX_PAGE_CONTENT + 1);
        let capacity = MAX_HEADER_CONTENT.saturating_sub(children + 64);
        let mut used = 0;
        for (&id, &est) in cands.iter().zip(&ests) {
            if used + est > capacity {
                break;
            }
            used += est;
            f.header.insert(id);
        }
    }

    // ---------------------------------------------------------------- merges

    /// Whether span `id`'s entry lies, or will lie after this flush, in a
    /// leaf rather than in the header.
    fn leafward(&self, f: &Flush, id: u32) -> bool {
        self.state.span(id).chain == Chain::Sequence && !f.header.contains(&id)
    }

    /// Merges the nodes `group`, consecutive in the rope, into one new span,
    /// whose entry goes to a leaf; returns it.
    fn merge_group(&mut self, f: &mut Flush, group: &[u32]) -> Result<u32, Error> {
        let ids: Vec<u32> = group.iter().map(|&n| self.span_at(n)).collect();
        let pos = self.state.rope.pos_of(group[0]);
        let pred = self.state.rope.prev(group[0]);
        let succ = self.state.rope.next(*group.last().unwrap());
        let succ_id = if succ == NIL { 0 } else { self.span_at(succ) };
        let pred_id = if pred == NIL { 0 } else { self.span_at(pred) };
        let start = f.scratch.len() as u32;
        let mut heat = 0;
        let mut total = 0u32;
        for &id in &ids {
            self.span_to_scratch(f, id)?;
            heat = heat.max(self.state.span(id).heat);
            total += self.state.span(id).len;
        }
        for &id in &ids {
            self.kill_in(f, id);
        }
        let rope = &mut self.state.rope;
        let (l, rest) = rope.split(self.state.root, pos);
        let (mid, r) = rope.split(rest, total as u64);
        rope.free_tree(mid);
        self.state.root = rope.merge(l, r);
        let made = self.new_spans(f, start, total, Chain::Sequence, heat, pred_id)?;
        let mut at = pos;
        for &id in &made {
            let len = self.state.span(id).len;
            let node = self.state.rope.node(RopeItem::Span { id, len });
            self.state.span_mut(id).node = node;
            self.insert_at(at, node);
            at += len as u64;
        }
        let last = *made.last().unwrap();
        self.state.span_mut(last).next = succ_id;
        if pred_id != 0 {
            self.state.span_mut(pred_id).next = made[0];
            f.stated.insert(pred_id);
        }
        Ok(made[0])
    }

    /// Merges short spans whose entries leave the header for a leaf in this
    /// flush with their leafward neighbours, wherever two neighbours are
    /// shorter than `span_min` together.
    fn merge_evicted(&mut self, f: &mut Flush) -> Result<(), Error> {
        let l = self.opts.span_min;
        let mut todo: Vec<u32> = f
            .stated
            .iter()
            .copied()
            .filter(|&id| self.leafward(f, id) && self.state.span(id).len < l)
            .collect();
        todo.sort_unstable();
        let mut done = IdSet::default();
        for id in todo {
            if done.contains(&id) || self.state.span(id).chain != Chain::Sequence {
                continue;
            }
            let len_of = |i: &Inner, n: u32| i.state.span(i.span_at(n)).len;
            let mut first = self.state.span(id).node;
            let mut last = first;
            loop {
                let p = self.state.rope.prev(first);
                if p == NIL
                    || !self.leafward(f, self.span_at(p))
                    || len_of(self, p) + len_of(self, first) >= l
                {
                    break;
                }
                first = p;
            }
            loop {
                let n = self.state.rope.next(last);
                if n == NIL
                    || !self.leafward(f, self.span_at(n))
                    || len_of(self, last) + len_of(self, n) >= l
                {
                    break;
                }
                last = n;
            }
            let mut run = vec![first];
            let mut m = first;
            while m != last {
                m = self.state.rope.next(m);
                run.push(m);
            }
            for &n in &run {
                done.insert(self.span_at(n));
            }
            // Greedily, a group grows while it stays shorter than `l`.
            let mut groups: Vec<Vec<u32>> = Vec::new();
            let mut cur: Vec<u32> = Vec::new();
            let mut cur_len = 0u32;
            for &n in &run {
                let len = len_of(self, n);
                if !cur.is_empty() && cur_len + len >= l {
                    groups.push(std::mem::take(&mut cur));
                    cur_len = 0;
                }
                cur.push(n);
                cur_len += len;
            }
            groups.push(cur);
            for g in groups {
                if g.len() < 2 {
                    continue;
                }
                let bytes: u64 = g.iter().map(|&n| len_of(self, n) as u64).sum();
                let made = self.merge_group(f, &g)?;
                done.insert(made);
                self.stats.evict_merges += 1;
                self.stats.evict_merge_bytes += bytes;
            }
        }
        Ok(())
    }

    /// The rotating walk: looks at the next `walk` spans of the sequence and
    /// merges runs of cold spans in leaves where the entry bytes the merge
    /// retires outweigh `mu` per byte it writes.
    fn walk(&mut self, f: &mut Flush) -> Result<(), Error> {
        let e = self.state.flush_epoch;
        let cold_before = e.saturating_sub(self.opts.cold_flushes as u64);
        let mut id = self.cons.walk;
        if id == 0
            || (id as usize) >= self.state.spans.len()
            || self.state.span(id).chain != Chain::Sequence
        {
            id = self.state.seq_head();
        }
        let mut run: Vec<u32> = Vec::new();
        let mut seen = 0;
        while id != 0 && seen < self.opts.walk {
            let s = *self.state.span(id);
            let eligible = s.chain == Chain::Sequence
                && s.heat <= cold_before
                && s.entry_page != NONE
                && s.entry_page != self.header_slot
                && !f.stated.contains(&id);
            let next = s.next;
            if eligible {
                run.push(id);
            } else {
                self.merge_run(f, &run)?;
                run.clear();
            }
            id = next;
            seen += 1;
        }
        self.merge_run(f, &run)?;
        self.cons.walk = id;
        Ok(())
    }

    /// Merges the groups of `run`, spans consecutive in the sequence, that
    /// pay: Kadane's maximal sums of what each span's entry costs less `mu`
    /// per byte it shows, within a page.
    fn merge_run(&mut self, f: &mut Flush, run: &[u32]) -> Result<(), Error> {
        if run.len() < 2 {
            return Ok(());
        }
        let mu = self.opts.mu;
        let weight = |i: &Inner, id: u32| {
            let s = i.state.span(id);
            let framing = s.entry_len as f64
                - match s.content {
                    Content::Inline { .. } => s.len as f64,
                    _ => 0.0,
                };
            framing - mu * s.len as f64
        };
        // A merged span's own entry, about.
        const NEW_FRAMING: f64 = 6.0;
        let mut groups: Vec<Vec<u32>> = Vec::new();
        let mut cur: Vec<u32> = Vec::new();
        let (mut sum, mut len) = (0.0, 0u32);
        let close = |cur: &mut Vec<u32>, sum: f64, groups: &mut Vec<Vec<u32>>| {
            if cur.len() >= 2 && sum > NEW_FRAMING {
                groups.push(std::mem::take(cur));
            }
            cur.clear();
        };
        for &id in run {
            let (w, n) = (weight(self, id), self.state.span(id).len);
            if len + n > C {
                close(&mut cur, sum, &mut groups);
                sum = 0.0;
                len = 0;
            }
            sum += w;
            len += n;
            cur.push(id);
            if sum < 0.0 {
                cur.clear();
                sum = 0.0;
                len = 0;
            }
        }
        close(&mut cur, sum, &mut groups);
        for g in groups {
            let nodes: Vec<u32> = g.iter().map(|&id| self.state.span(id).node).collect();
            let bytes: u64 = g.iter().map(|&id| self.state.span(id).len as u64).sum();
            self.merge_group(f, &nodes)?;
            self.stats.walk_merges += 1;
            self.stats.walk_merge_bytes += bytes;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- placement

    pub(crate) fn new_data_page(&mut self) -> Result<DataPage, Error> {
        let page = take_page(
            &mut self.ready,
            &mut self.state,
            &mut self.file_pages,
            &mut *self.storage,
            &mut self.stats,
            Filling::taken(PageKind::Data),
        )?;
        Ok(DataPage {
            page,
            buf: new_page(),
            used: 0,
        })
    }

    /// Places the bytes of span `id`, pending or in a data page, into `dp`,
    /// which has room for them; the span is stated with its new address.
    pub(crate) fn place(&mut self, f: &mut Flush, dp: &mut DataPage, id: u32) -> Result<(), Error> {
        let s = *self.state.span(id);
        let n = s.len as usize;
        let at = CONTENT_OFFSET + dp.used;
        match s.content {
            Content::Pending(i) => {
                let p = f.pending[i as usize].start as usize;
                dp.buf[at..at + n].copy_from_slice(&f.scratch[p..p + n]);
            }
            Content::Ref { page, offset } => {
                let src = self.page_bytes(page)?;
                dp.buf[at..at + n].copy_from_slice(&src[offset as usize..offset as usize + n]);
                self.state.unnote_data(id);
                self.state.uncover(page, s.len);
            }
            other => return Err(corrupt(format!("placing span {id}, which shows {other:?}"))),
        }
        self.state.span_mut(id).content = Content::Ref {
            page: dp.page,
            offset: at as u16,
        };
        self.state.note_data(id);
        self.state.cover(dp.page, s.len);
        f.stated.insert(id);
        dp.used += n;
        Ok(())
    }

    /// Splits span `id`, `head` bytes into it: it keeps the first `head`,
    /// and a new span, which it returns, takes the rest. Both are stated.
    pub(crate) fn split_span(&mut self, f: &mut Flush, id: u32, head: u32) -> Result<u32, Error> {
        let s = *self.state.span(id);
        debug_assert!(head > 0 && head < s.len);
        let content = match s.content {
            Content::Pending(i) => {
                let p = f.pending[i as usize];
                f.pending.push(PendingBytes {
                    start: p.start + head,
                    inline: false,
                });
                Content::Pending((f.pending.len() - 1) as u32)
            }
            Content::Ref { page, offset } => Content::Ref {
                page,
                offset: offset + head as u16,
            },
            other => {
                return Err(corrupt(format!(
                    "splitting span {id}, which shows {other:?}"
                )))
            }
        };
        let t = self.state.new_span(id).ok_or(Error::OutOfBounds)?;
        *self.state.span_mut(t) = Span {
            len: s.len - head,
            next: s.next,
            content,
            chain: s.chain,
            entry_page: NONE,
            heat: s.heat,
            node: NIL,
            ..Span::default()
        };
        self.state.span_mut(id).len = head;
        self.state.span_mut(id).next = t;
        self.state.note_data(t);
        match s.chain {
            Chain::Sequence => {
                let pos = self.state.rope.pos_of(s.node);
                self.state
                    .rope
                    .set_item(s.node, RopeItem::Span { id, len: head });
                let m = self.state.rope.node(RopeItem::Span {
                    id: t,
                    len: s.len - head,
                });
                self.state.span_mut(t).node = m;
                self.insert_at(pos + head as u64, m);
            }
            Chain::Schema | Chain::Private => {
                let list = if s.chain == Chain::Schema {
                    &mut self.state.schema
                } else {
                    &mut self.state.private
                };
                let i = list.iter().position(|&x| x == id).unwrap();
                list.insert(i + 1, t);
            }
            Chain::None => unreachable!(),
        }
        f.stated.insert(id);
        f.stated.insert(t);
        if f.header.contains(&id) {
            f.header.insert(t);
        }
        Ok(t)
    }

    /// The flush's spans that still need a data page, the small chains'
    /// first, then the sequence's in order.
    fn chunks(&self, f: &Flush) -> Vec<u32> {
        let mut small = Vec::new();
        let mut seq: Vec<(u64, u32)> = Vec::new();
        for &id in &f.stated {
            let s = self.state.span(id);
            if let Content::Pending(i) = s.content {
                if !f.pending[i as usize].inline {
                    match s.chain {
                        Chain::Sequence => seq.push((self.state.rope.pos_of(s.node), id)),
                        _ => small.push(id),
                    }
                }
            }
        }
        small.sort_unstable();
        seq.sort_unstable();
        small.extend(seq.into_iter().map(|(_, id)| id));
        small
    }

    /// Packs the flush's chunks into data pages in sequence order with a
    /// bounded look-ahead, filling room its own chunks cannot use with
    /// victims, and cutting only to keep a page from closing more than `θ`
    /// empty.
    fn pack(&mut self, f: &mut Flush, out: &mut Output) -> Result<(), Error> {
        let mut rem: VecDeque<u32> = self.chunks(f).into();
        if rem.is_empty() {
            return Ok(());
        }
        let theta = (self.opts.theta * C as f64) as usize;
        let inline_max = self.opts.inline_threshold.min(MAX_INLINE as u32);
        let mut open = self.new_data_page()?;
        loop {
            let room = open.room() as u32;
            if let Some(i) = rem
                .iter()
                .take(self.opts.lookahead)
                .position(|&id| self.state.span(id).len <= room)
            {
                let id = rem.remove(i).unwrap();
                self.place(f, &mut open, id)?;
                continue;
            }
            if self.opts.consolidate {
                self.free_fill(f, &mut open)?;
            }
            if rem.is_empty() {
                break;
            }
            if open.room() > theta {
                let id = rem.pop_front().unwrap();
                let head = open.room() as u32;
                let t = self.split_span(f, id, head)?;
                self.place(f, &mut open, id)?;
                if self.state.span(t).len <= inline_max {
                    if let Content::Pending(i) = self.state.span(t).content {
                        f.pending[i as usize].inline = true;
                    }
                } else {
                    rem.push_front(t);
                }
            }
            if rem.is_empty() {
                break;
            }
            let full = std::mem::replace(&mut open, self.new_data_page()?);
            out.data.push(full);
        }
        out.data.push(open);
        Ok(())
    }

    // ---------------------------------------------------------------- the cut

    /// Encodes the entries of `ids`, sorted, into leaves; returns each
    /// leaf's content and where its entries landed.
    fn encode_leaves(&self, f: &Flush, ids: &[u32]) -> Vec<(Vec<u8>, Vec<Placed>)> {
        let mut pages = Vec::new();
        let mut enc = PageEncoder::new(&[], MAX_PAGE_CONTENT);
        for &id in ids {
            let e = EntryOut {
                id,
                next: self.state.span(id).next,
                body: self.body(f, id),
            };
            if !enc.push(&e) {
                let full = std::mem::replace(&mut enc, PageEncoder::new(&[], MAX_PAGE_CONTENT));
                pages.push((full.out, full.placed));
                assert!(enc.push(&e), "an entry larger than a page");
            }
        }
        if !enc.placed.is_empty() {
            pages.push((enc.out, enc.placed));
        }
        pages
    }

    /// Encodes the header's entries after `children`; `None` if they do not
    /// fit.
    fn encode_header(
        &self,
        f: &Flush,
        children: &[u32],
        ids: &[u32],
    ) -> Option<(Vec<u8>, Vec<Placed>)> {
        let mut enc = PageEncoder::new(children, MAX_HEADER_CONTENT);
        for &id in ids {
            let e = EntryOut {
                id,
                next: self.state.span(id).next,
                body: self.body(f, id),
            };
            if !enc.push(&e) {
                return None;
            }
        }
        Some((enc.out, enc.placed))
    }

    /// Writes every stated span's entry: the header's into the header, the
    /// rest into new leaves, the last of which takes whole sparse leaves
    /// whose entries fit its room.
    fn cut(&mut self, f: &mut Flush, out: &mut Output) -> Result<(), Error> {
        let stated: Vec<u32> = f.stated.iter().copied().collect();
        for &id in &stated {
            self.state.drop_entry(id);
        }
        let mut head: Vec<u32> = stated
            .iter()
            .copied()
            .filter(|id| f.header.contains(id))
            .collect();
        let mut leaf: Vec<u32> = stated
            .iter()
            .copied()
            .filter(|id| !f.header.contains(id))
            .collect();
        head.sort_unstable();
        leaf.sort_unstable();
        loop {
            let mut pages = self.encode_leaves(f, &leaf);
            // Fillers: whole leaves whose entries fit the last leaf's room.
            if self.opts.consolidate {
                if let Some((content, _)) = pages.last() {
                    let room = MAX_PAGE_CONTENT - content.len();
                    let victims = self.cut_fillers(f, room, &leaf);
                    if !victims.is_empty() {
                        for v in &victims {
                            for id in self.state.table_spans.get(v).cloned().unwrap_or_default() {
                                self.state.drop_entry(id);
                                f.stated.insert(id);
                                leaf.push(id);
                            }
                            self.stats.table_rewrites += 1;
                        }
                        leaf.sort_unstable();
                        pages = self.encode_leaves(f, &leaf);
                    }
                }
            }
            let mut taken = Vec::new();
            for _ in &pages {
                taken.push(take_page(
                    &mut self.ready,
                    &mut self.state,
                    &mut self.file_pages,
                    &mut *self.storage,
                    &mut self.stats,
                    Filling::taken(PageKind::Table),
                )?);
            }
            let mut children: Vec<u32> = self
                .leaves()
                .into_iter()
                .filter(|&p| self.state.coverage(p) > 0)
                .collect();
            children.extend(&taken);
            children.sort_unstable();
            match self.encode_header(f, &children, &head) {
                Some((content, placed)) => {
                    // Record where every entry landed.
                    for ((content, placed), &page) in pages.into_iter().zip(&taken) {
                        let mut buf = new_page();
                        encode_page(&mut buf, None, KIND_TABLE, self.state.flush_epoch, &content);
                        self.table_pages.insert(page, buf);
                        self.note_placed(f, page, CONTENT_OFFSET, &placed);
                        out.tables.push((page, self.table_pages[&page].clone()));
                    }
                    let slot = (self.state.flush_epoch % 2) as u32;
                    self.note_placed(f, slot, HEADER_CONTENT_OFFSET, &placed);
                    out.header_content = content;
                    self.stats.entries_stated += (head.len() + leaf.len()) as u64;
                    return Ok(());
                }
                None => {
                    for p in taken {
                        self.state.pages[p as usize] = Page::Reusable;
                        self.ready.insert(p);
                    }
                    if head.is_empty() {
                        // The references to leaves alone outgrow the header,
                        // which only interior table pages would relieve, and
                        // this implementation writes none.
                        return Err(Error::OutOfBounds);
                    }
                    // The coldest eighth of the header's entries go to leaves.
                    head.sort_by_key(|&id| (Reverse(self.state.span(id).heat), id));
                    let keep = head.len() - head.len().div_ceil(8);
                    for id in head.split_off(keep) {
                        f.header.remove(&id);
                        leaf.push(id);
                    }
                    head.sort_unstable();
                    leaf.sort_unstable();
                }
            }
        }
    }

    /// Records the entries `placed` in table page `page`, whose content
    /// starts `content_at` bytes into it.
    fn note_placed(&mut self, f: &Flush, page: u32, content_at: usize, placed: &[Placed]) {
        if page < 2 {
            // The header slot the flush writes is taken here, its old content
            // having been moved.
            if !matches!(self.state.pages[page as usize], Page::Filling(_)) {
                self.state.pages[page as usize] = Filling::taken(PageKind::Header);
            }
        }
        for p in placed {
            let s = *self.state.span(p.id);
            let inline = match s.content {
                Content::Inline { .. } => true,
                Content::Pending(i) => f.pending[i as usize].inline,
                _ => false,
            };
            if inline {
                self.state.span_mut(p.id).content = Content::Inline {
                    page,
                    offset: (content_at + p.payload) as u16,
                };
            }
            self.state.note_entry(p.id, page, p.len);
        }
    }

    // ---------------------------------------------------------------- commit

    fn commit(&mut self, e: u64, out: Output) -> Result<(), Error> {
        // The next journal starts where the seal said, or else in the lowest
        // reusable page, or past the end.
        let sealed = self.sealed_next.take();
        let jp = match sealed {
            Some(p) => p,
            None => match self.ready.pop_first() {
                Some(p) => p,
                None => self.file_pages,
            },
        };
        let slot = (e % 2) as u32;
        if !matches!(self.state.pages[slot as usize], Page::Filling(_)) {
            self.state.pages[slot as usize] = Filling::taken(PageKind::Header);
        }
        let written: Vec<u32> = (out.data.iter().map(|dp| dp.page))
            .chain(out.tables.iter().map(|(p, _)| *p))
            .chain([slot])
            .collect();
        debug_assert_eq!(
            written.len(),
            self.state
                .pages
                .iter()
                .filter(|p| matches!(p, Page::Filling(_)))
                .count(),
            "a page the flush took is not written"
        );
        for mut dp in out.data {
            seal_page(&mut dp.buf, None, KIND_DATA, e, dp.used);
            self.storage
                .write_at(&dp.buf[..], dp.page as u64 * PAGE_SIZE as u64)?;
            self.stats.data_pages_written += 1;
        }
        for (p, buf) in &out.tables {
            self.storage
                .write_at(&buf[..], *p as u64 * PAGE_SIZE as u64)?;
            self.stats.table_pages_written += 1;
        }
        // A session's first flush clears the page it names for the next
        // journal (`spec/journal.md#the-start-of-a-session`), unless the
        // seal did already.
        if self.first_flush && sealed.is_none() && jp < self.file_pages {
            self.storage
                .write_at(&new_page()[..], jp as u64 * PAGE_SIZE as u64)?;
        }
        self.header.journal_pointer = jp;
        self.header.format_version = FORMAT_VERSION;
        self.header.min_reader_version = MIN_READER_VERSION;
        self.header.sequence_head = self.state.seq_head();
        self.header.schema_head = self.state.schema.first().copied().unwrap_or(0);
        self.header.private_head = self.state.private.first().copied().unwrap_or(0);
        let mut hbuf = new_page();
        encode_page(
            &mut hbuf,
            Some(&self.header),
            KIND_TABLE,
            e,
            &out.header_content,
        );
        // The `fsync` and the header write, on a thread of their own when a
        // mutation triggered the flush and the journal is sealed: the next
        // journal is reachable through the seal whichever header governs,
        // so appends may go on meanwhile.
        let at = slot as u64 * PAGE_SIZE as u64;
        let deferred = sealed.is_some() && self.opts.sync_on_worker && !self.explicit_flush;
        let mut worker = None;
        if let Some(mut s) = deferred.then(|| self.storage.try_clone()).flatten() {
            let page = hbuf.clone();
            self.stats.commits_on_worker += 1;
            worker = Some(std::thread::spawn(move || {
                s.sync()?;
                s.write_at(&page[..], at)
            }));
        } else {
            self.storage.sync()?;
            self.storage.write_at(&hbuf[..], at)?;
        }
        self.stats.headers_written += 1;

        // The pages the flush filled are in the world now.
        for &p in &written {
            match self.state.pages[p as usize] {
                Page::Filling(fl) => self.state.pages[p as usize] = Page::Live(fl.commit(e)),
                other => unreachable!("page {p} was written but is {other:?}"),
            }
        }

        // Bookkeeping: the rotation of reusable pages first. The pages the
        // commit before freed are reusable once this commit's `fsync` is
        // done, which a worker reports when it joins.
        let release = std::mem::take(&mut self.retiring);
        match worker {
            Some(worker) => self.pending = Some(PendingCommit { worker, release }),
            None => {
                for p in release {
                    debug_assert_eq!(self.state.pages[p as usize], Page::Fallback);
                    self.state.pages[p as usize] = Page::Reusable;
                    self.ready.insert(p);
                }
            }
        }
        let mut retiring = Vec::new();
        let candidates: Vec<u32> = self.state.dropped_candidates.drain().collect();
        for p in candidates {
            if p == slot || p == self.header_slot {
                continue;
            }
            if let Page::Live(Live {
                coverage: 0, kind, ..
            }) = self.state.pages[p as usize]
            {
                debug_assert!(kind != PageKind::Header);
                self.state.unbucket(p);
                self.state.pages[p as usize] = Page::Fallback;
                self.table_pages.remove(&p);
                self.mirror.remove(&p);
                self.cache.remove(&p);
                retiring.push(p);
            }
        }
        for &p in &self.journal.pages {
            if p < self.file_pages && self.state.pages[p as usize] == Page::Journal {
                self.state.pages[p as usize] = Page::Fallback;
                retiring.push(p);
            }
        }
        // The header this commit replaced leaves the world; the flush moved
        // everything it held. A header slot is never handed out, so it does
        // not join the rotation of reusable pages.
        let old = self.header_slot;
        if old != slot {
            debug_assert!(
                !self.state.table_spans.contains_key(&old),
                "the replaced header still holds live entries"
            );
            self.state.pages[old as usize] = Page::Fallback;
        }
        self.retiring = retiring;
        self.table_pages.insert(slot, hbuf);
        if old != slot {
            self.table_pages.remove(&old);
        }
        self.header_slot = slot;
        self.epoch = e;
        for p in written {
            self.state.rebucket(p);
        }
        if jp < self.file_pages {
            debug_assert!(
                self.state.pages[jp as usize] == Page::Reusable
                    || (sealed.is_some() && self.state.pages[jp as usize] == Page::Journal)
            );
            self.state.pages[jp as usize] = Page::Journal;
        }
        self.journal = crate::journal::JournalWriter::new(e + 1, jp);
        self.first_flush = false;
        self.cache.clear();
        self.stats.flushes += 1;
        self.after_commit()?;
        self.truncate(false)
    }

    /// Shrinks the file to end after its highest page that is not reusable,
    /// once the reusable tail reaches the threshold, or always if `force`.
    pub(crate) fn truncate(&mut self, force: bool) -> Result<(), Error> {
        let mut top = self.file_pages;
        while top > 2 {
            let p = top - 1;
            let page = self.state.pages[p as usize];
            let empty_journal =
                page == Page::Journal && self.journal.is_empty() && self.journal.pages == [p];
            if page == Page::Reusable || (force && empty_journal) {
                top -= 1;
            } else {
                break;
            }
        }
        let tail = self.file_pages - top;
        if tail == 0 || (!force && tail < self.opts.truncate_tail) {
            return Ok(());
        }
        self.storage.set_len(top as u64 * PAGE_SIZE as u64)?;
        for p in top..self.file_pages {
            self.ready.remove(&p);
        }
        self.state.pages.truncate(top.max(2) as usize);
        self.file_pages = top;
        self.stats.truncations += 1;
        Ok(())
    }
}
