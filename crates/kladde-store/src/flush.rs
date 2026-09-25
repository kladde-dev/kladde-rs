//! The flush (`impl/flush.md`, `impl/consolidation.md`): take the header,
//! fold the journal, consolidate, pack data pages, cut the address table, and
//! commit.

use std::collections::VecDeque;

use crate::consts::*;
use crate::error::{corrupt, Error};
use crate::fold::{fold, Fold, Source};
use crate::page::{decode_page, new_page, seal_page, PageBuf};
use crate::state::*;
use crate::statement::{Kind, TableReader};
use crate::store::{take_page, Inner, Tx};

/// A data page the flush fills.
pub(crate) struct DataPage {
    pub page: u32,
    pub buf: Box<PageBuf>,
    pub used: usize,
    /// The bytes of its content that drain, and their drain rates summed
    /// over those bytes: what its estimate starts from.
    pub fast: f64,
    pub rate: f64,
}

impl DataPage {
    pub fn room(&self) -> usize {
        MAX_PAGE_CONTENT - self.used
    }
}

/// A pending fragment waiting for a data page.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Chunk {
    pub key: Key,
    pub len: u32,
    pub rewrite: bool,
}

/// What the flush writes, gathered before the commit.
#[derive(Default)]
pub(crate) struct Output {
    pub data: Vec<DataPage>,
    /// Address-table pages other than the header: page and content.
    pub tables: Vec<(u32, Vec<u8>, bool)>,
    pub header_content: Vec<u8>,
    /// Table pages this flush unlinked or replaced.
    pub dropped_tables: Vec<u32>,
}

impl Inner {
    /// Appends what a batch holds back outside any open transaction, then
    /// flushes. Poisons the store if the flush fails.
    pub fn flush_now(&mut self) -> Result<(), Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        self.must_flush_first = false;
        let ready = match self.tx {
            Tx::InBatch { .. } => self.ops.len(),
            Tx::InTxInBatch { ready, .. } => ready,
            _ => 0,
        };
        if ready > 0 {
            self.append_ops(ready)?;
        }
        match self.flush_inner() {
            Ok(()) => {
                self.repopulate_geometry();
                Ok(())
            }
            Err(e) => {
                self.poisoned = true;
                Err(e)
            }
        }
    }

    fn flush_inner(&mut self) -> Result<(), Error> {
        let e = self.epoch + 1;
        self.state.flush_epoch = e;
        self.state.pending.clear();
        self.state.dropped_candidates.clear();
        self.flush_rewritten.clear();
        self.begin_consolidation();
        let mut dirty = Dirty::default();

        // 1. The header's content, taken whole.
        self.take_header(&mut dirty)?;

        // 2. The fold, with every read from elsewhere hoisted into the arena.
        //    What it supersedes is what pages lose naturally, and so is what
        //    the consolidator state's own rewrite supersedes.
        let mut f = fold(&self.segment_records, |id| {
            self.state.allocs.get(&id).map(|m| m.size)
        });
        self.hoist(&mut f)?;
        self.state.natural = true;
        let folded = self.apply_fold(f, &mut dirty);
        self.state.natural = false;
        folded?;
        self.apply_losses(true);
        if self.opts.consolidate {
            if self.opts.consolidator_state {
                self.state.natural = true;
                let written = self.write_consolidator_state(&mut dirty);
                self.state.natural = false;
                written?;
                self.apply_losses(false);
            }
            // Description defragmentation's share: rewrites chosen now are
            // written like the flush's own.
            self.defrag_share(&mut dirty)?;
        }

        // 3. Data pages, with victims riding along, then budgeted pages.
        let mut out = Output::default();
        self.pack(&mut dirty, &mut out)?;
        if self.opts.consolidate {
            self.budget_loop(&mut dirty, &mut out)?;
        }

        // 4. The address table.
        self.cut(&mut dirty, &mut out)?;

        // 5. Write, fsync, commit.
        self.commit(e, &dirty, out)
    }

    // ---------------------------------------------------------------- taking

    /// Takes the content of fragment `f` over `[start, end)` of `id` in
    /// place: its bytes stay where they are, and the cut states it again.
    pub(crate) fn take_in_place(
        &mut self,
        id: u32,
        start: u32,
        end: u32,
        f: Fragment,
        heat: u32,
        dirty: &mut Dirty,
    ) {
        let p = match f {
            Fragment::Bytes { page, offset, stmt } => {
                let a = address(page, offset);
                if self.state.slab.kinds[stmt.idx()] == Kind::Inline {
                    Pending {
                        origin: Origin::File(a),
                        place: Place::Inline,
                        heat,
                        rewrite: false,
                    }
                } else {
                    Pending {
                        origin: Origin::File(a),
                        place: Place::Data(a),
                        heat,
                        rewrite: false,
                    }
                }
            }
            Fragment::ZeroExplicitly { .. } => Pending {
                origin: Origin::None,
                place: Place::Zero,
                heat,
                rewrite: false,
            },
            _ => return,
        };
        self.state.take(id, start, end, p, dirty);
    }

    /// The fragments over `[start, end)` of `id` owned by statements in `page`.
    pub(crate) fn owned_in(
        &self,
        id: u32,
        start: u32,
        end: u32,
        page: u32,
    ) -> Vec<(u32, u32, Fragment)> {
        let size = self.state.size_of(id);
        let end = end.min(size);
        let mut out = Vec::new();
        if start >= end {
            return out;
        }
        let first = self
            .state
            .frags
            .range(..=key(id, start))
            .next_back()
            .map(|(&k, _)| k)
            .filter(|&k| kid(k) == id)
            .unwrap_or(key(id, start));
        let mut it = self.state.frags.range(first..key(id, end)).peekable();
        while let Some((&k, &f)) = it.next() {
            let fend = match it.peek() {
                Some((&n, _)) => koff(n),
                None => self.state.frag_end(k),
            };
            if let Some(s) = f.stmt() {
                if self.state.slab.page(s) == page {
                    out.push((koff(k).max(start), fend.min(end), f));
                }
            }
        }
        out
    }

    /// Flushes untouched since `id` was last written: the heat the header's
    /// split ranks by.
    pub(crate) fn heat_of(&self, id: u32) -> u32 {
        let lw = self.state.allocs.get(&id).map_or(0, |m| m.last_written);
        (self.state.flush_epoch.saturating_sub(lw)).min(u32::MAX as u64) as u32
    }

    /// Takes the live content of the address-table page `page` in place, and
    /// drops its statements physically. The header's take and the page
    /// rewrite are this, for different pages.
    pub(crate) fn rewrite_table_page(&mut self, page: u32, dirty: &mut Dirty) -> Result<(), Error> {
        let buf = self
            .table_pages
            .get(&page)
            .ok_or_else(|| corrupt("a table page is not resident"))?
            .clone();
        let is_header = page < 2;
        let d = decode_page(&buf, is_header)
            .map_err(|_| corrupt("a resident table page does not decode"))?;
        let (_, mut r) = TableReader::open(&buf[..], d.content)?;
        while let Some(dec) = r.next_stmt()? {
            let s = dec.stmt;
            let id = s.id;
            let heat = self.heat_of(id);
            let (start, end) = match s.kind {
                Kind::Ref | Kind::Zero | Kind::Inline => {
                    (s.offset, s.offset.saturating_add(s.size))
                }
                Kind::Shrink => (s.offset, u32::MAX),
                Kind::Tombstone => (0, u32::MAX),
                Kind::Grow => (0, 0),
            };
            for (a, b, f) in self.owned_in(id, start, end, page) {
                self.take_in_place(id, a, b, f, heat, dirty);
            }
            let in_page =
                |s: Option<StmtRef>, st: &State| s.is_some_and(|s| st.slab.page(s) == page);
            if let Some(m) = self.state.allocs.get(&id) {
                let (ra, rg) = (in_page(m.anchor, &self.state), in_page(m.grow, &self.state));
                if ra || rg {
                    let rec = self.touch(id, false, dirty);
                    rec.replace_anchor |= ra;
                    rec.replace_grow |= rg;
                }
            } else if let Some(r) = self.state.recyclable.get(&id) {
                if in_page(r.tombstone, &self.state) {
                    let rec = self.touch(id, false, dirty);
                    rec.replace_anchor = true;
                }
            }
            self.state.drop_physically(id, page);
        }
        Ok(())
    }

    fn take_header(&mut self, dirty: &mut Dirty) -> Result<(), Error> {
        self.flush_rewritten.insert(self.header_slot);
        self.rewrite_table_page(self.header_slot, dirty)
    }

    /// Records that the flush changed `id`, the first time it does.
    pub(crate) fn touch<'a>(
        &mut self,
        id: u32,
        written: bool,
        dirty: &'a mut Dirty,
    ) -> &'a mut IdRecord {
        let size_before = self.state.size_of(id);
        if written {
            if let Some(m) = self.state.allocs.get_mut(&id) {
                m.last_written = self.state.flush_epoch;
            }
        }
        let rec = dirty.records.entry(id).or_insert(IdRecord {
            size_before,
            ..Default::default()
        });
        rec.written |= written;
        rec
    }

    // ------------------------------------------------------------ the fold

    /// Reads every piece that the fold sources from committed content other
    /// than where it already lies, and turns it into a literal.
    fn hoist(&mut self, f: &mut Fold) -> Result<(), Error> {
        let mut reads: Vec<(u32, u32, u32, u32, u32)> = Vec::new();
        for (&id, e) in &f.ids {
            if !e.exists {
                continue;
            }
            let keys: Vec<(u32, Source)> = e.table.pieces.iter().map(|(&k, &s)| (k, s)).collect();
            for (i, &(k, s)) in keys.iter().enumerate() {
                if let Source::Storage(src, so) = s {
                    let identity = src == id && so == k && e.committed_live;
                    if !identity {
                        let end = keys.get(i + 1).map_or(e.table.size, |n| n.0);
                        reads.push((id, k, src, so, end - k));
                    }
                }
            }
        }
        for (id, k, src, so, len) in reads {
            let mut bytes = vec![0u8; len as usize];
            self.read_committed(src, so, &mut bytes)?;
            let pos = self.segment_records.arena.len() as u64;
            self.segment_records.arena.extend_from_slice(&bytes);
            f.ids
                .get_mut(&id)
                .unwrap()
                .table
                .pieces
                .insert(k, Source::Literal(pos));
        }
        Ok(())
    }

    /// Takes the pieces of `table` that change `id`'s content: literals,
    /// grouped into runs that go `Inline` when short; and zeros below
    /// `zero_limit`, which deny content the id had before.
    fn take_pieces(
        &mut self,
        id: u32,
        table: &crate::fold::Table,
        identity: bool,
        zero_limit: u32,
        dirty: &mut Dirty,
    ) {
        let keys: Vec<(u32, Source)> = table.pieces.iter().map(|(&k, &s)| (k, s)).collect();
        let mut i = 0;
        while i < keys.len() {
            let (k, s) = keys[i];
            let end = keys.get(i + 1).map_or(table.size, |n| n.0);
            match s {
                Source::Storage(src, so) if identity && src == id && so == k => i += 1,
                Source::Storage(..) => unreachable!("hoisted"),
                Source::Zero => {
                    let e = end.min(zero_limit);
                    if k < e {
                        let p = Pending {
                            origin: Origin::None,
                            place: Place::Zero,
                            heat: 0,
                            rewrite: false,
                        };
                        self.state.take(id, k, e, p, dirty);
                    }
                    i += 1;
                }
                Source::Literal(_) => {
                    // A run of adjacent literal pieces.
                    let mut j = i;
                    let mut run_end = end;
                    while j + 1 < keys.len() && matches!(keys[j + 1].1, Source::Literal(_)) {
                        j += 1;
                        run_end = keys.get(j + 1).map_or(table.size, |n| n.0);
                    }
                    let place = if run_end - k <= self.opts.inline_threshold {
                        Place::Inline
                    } else {
                        Place::Unplaced
                    };
                    for (t, &(pk, ps)) in keys.iter().enumerate().take(j + 1).skip(i) {
                        let pe = keys.get(t + 1).map_or(table.size, |n| n.0);
                        let Source::Literal(pos) = ps else {
                            unreachable!()
                        };
                        let p = Pending {
                            origin: Origin::Arena(pos),
                            place,
                            heat: 0,
                            rewrite: false,
                        };
                        self.state.take(id, pk, pe, p, dirty);
                        self.stats.fresh_bytes += (pe - pk) as u64;
                    }
                    i = j + 1;
                }
            }
        }
    }

    /// Grows `id` to `new`. The range a growth exposes resolves through the
    /// anchor, unless the anchor lives in a page this flush retires: the
    /// statement the cut restates in its place anchors at the new size and
    /// covers none of that range, so the range is taken and stated as zeros.
    pub(crate) fn grow(&mut self, id: u32, old: u32, new: u32, dirty: &mut Dirty) {
        self.state.grow_size_to(id, new);
        let retired_anchor = dirty.records.get(&id).is_some_and(|r| r.replace_anchor);
        if retired_anchor && self.state.allocs[&id].anchor.is_some() {
            let p = Pending {
                origin: Origin::None,
                place: Place::Zero,
                heat: 0,
                rewrite: false,
            };
            self.state.take(id, old, new, p, dirty);
        }
    }

    fn apply_fold(&mut self, f: Fold, dirty: &mut Dirty) -> Result<(), Error> {
        for (id, e) in f.ids {
            match (e.existed, e.ended, e.exists) {
                (true, false, true) => {
                    self.touch(id, e.written, dirty);
                    let old = self.state.size_of(id);
                    let new = e.table.size;
                    if new < old {
                        self.state.shrink_size_to(id, new);
                        dirty.records.get_mut(&id).unwrap().shrank = true;
                    } else if new > old {
                        self.grow(id, old, new, dirty);
                    }
                    self.take_pieces(id, &e.table, true, old.min(new), dirty);
                }
                (true, true, false) => self.free(id, dirty),
                (true, true, true) => {
                    let rec = self.touch(id, true, dirty);
                    rec.freed = true;
                    rec.allocated = true;
                    let old = self.state.size_of(id);
                    let new = e.table.size;
                    if new < old {
                        self.state.shrink_size_to(id, new);
                        dirty.records.get_mut(&id).unwrap().shrank = true;
                    } else if new > old {
                        self.grow(id, old, new, dirty);
                    }
                    self.take_pieces(id, &e.table, false, old.min(new), dirty);
                }
                (false, _, true) => {
                    self.allocate(id, e.table.size, dirty);
                    self.take_pieces(id, &e.table, false, 0, dirty);
                }
                (false, _, false) => {
                    let live = self
                        .state
                        .recyclable
                        .get(&id)
                        .is_some_and(|r| r.tombstone.is_some());
                    self.ids.release(id, live);
                }
                (true, false, false) => unreachable!(),
            }
        }
        Ok(())
    }

    pub(crate) fn allocate(&mut self, id: u32, n: u32, dirty: &mut Dirty) {
        let mut meta = AllocationMeta {
            last_written: self.state.flush_epoch,
            ..Default::default()
        };
        if let Some(r) = self.state.recyclable.remove(&id) {
            meta.mentions = r.mentions;
            // A live tombstone becomes the new incarnation's anchor; its pin
            // moves with it.
            if let Some(t) = r.tombstone {
                meta.anchor = Some(t);
                meta.anchor_n = 0;
            }
        }
        self.state.allocs.insert(id, meta);
        let rec = self.touch(id, true, dirty);
        rec.allocated = true;
        self.grow(id, 0, n, dirty);
    }

    fn free(&mut self, id: u32, dirty: &mut Dirty) {
        let rec = self.touch(id, true, dirty);
        rec.freed = true;
        self.state.shrink_size_to(id, 0);
        let meta = self.state.allocs.remove(&id).unwrap();
        self.state.recyclable.insert(
            id,
            Recyclable {
                mentions: meta.mentions,
                tombstone: None,
            },
        );
        if let Some(a) = meta.anchor {
            self.state.unpin(a);
        }
        if let Some(g) = meta.grow {
            self.state.unpin(g);
        }
    }

    // ------------------------------------------------------------ packing

    /// The pending fragments in `dirty` that still need a data page, in key
    /// order.
    pub(crate) fn chunks(&self, dirty: &Dirty) -> Vec<Chunk> {
        let mut out = Vec::new();
        for (&k, &end) in &dirty.ranges {
            let id = kid(k);
            let mut it = self.state.frags.range(k..key(id, end)).peekable();
            while let Some((&fk, &f)) = it.next() {
                if let Fragment::Pending(p) = f {
                    let pe = self.state.pending[p as usize];
                    if pe.place == Place::Unplaced {
                        let fend = match it.peek() {
                            Some((&n, _)) => koff(n),
                            None => self.state.frag_end(fk),
                        };
                        out.push(Chunk {
                            key: fk,
                            len: fend - koff(fk),
                            rewrite: pe.rewrite,
                        });
                    }
                }
            }
        }
        out
    }

    pub(crate) fn new_data_page(&mut self) -> Result<DataPage, Error> {
        let page = take_page(
            &mut self.ready,
            &mut self.state,
            &mut self.file_pages,
            &mut *self.storage,
            &mut self.stats,
        )?;
        Ok(DataPage {
            page,
            buf: new_page(),
            used: 0,
            fast: 0.0,
            rate: 0.0,
        })
    }

    /// Places the chunk at `key` into `dp`, copying its bytes. Moved content
    /// brings its source page's split along, draining at the source's rate;
    /// fresh content drains at the rate fresh pages have been losing at; a
    /// description defragmentation's rewrite, cold by selection, is static.
    pub(crate) fn place(&mut self, dp: &mut DataPage, key: Key, len: u32) -> Result<(), Error> {
        let Some(Fragment::Pending(p)) = self.state.frags.get(&key).copied() else {
            return Err(corrupt("placing a chunk that is not pending"));
        };
        let pending = self.state.pending[p as usize];
        let (share, rate) = match pending.origin {
            _ if pending.rewrite => (0.0, 0.0),
            Origin::File(a) => {
                let src = &self.state.pages[split_address(a).0 as usize].drain;
                (src.share as f64, src.rate(self.state.flush_epoch))
            }
            _ => (1.0, self.fresh_rate(true)),
        };
        let fast = share * len as f64;
        dp.fast += fast;
        dp.rate += rate * fast;
        let at = CONTENT_OFFSET + dp.used;
        let mut bytes = vec![0u8; len as usize];
        self.read_fragment(Fragment::Pending(p), 0, &mut bytes)?;
        dp.buf[at..at + len as usize].copy_from_slice(&bytes);
        let a = address(dp.page, at as u16);
        self.state.pending[p as usize].place = Place::Data(a);
        self.state.cover(dp.page, len);
        self.state
            .note_reverse(dp.page, kid(key), koff(key), koff(key) + len);
        dp.used += len as usize;
        Ok(())
    }

    /// Packs the flush's chunks into data pages in key order with a bounded
    /// look-ahead, filling room its own chunks cannot use with victims, and
    /// cutting only to keep a page from closing more than `θ` empty.
    fn pack(&mut self, dirty: &mut Dirty, out: &mut Output) -> Result<(), Error> {
        let c = MAX_PAGE_CONTENT as u32;
        let chunks = self.chunks(dirty);
        let mut rem: VecDeque<Chunk> = VecDeque::new();
        // Whole pages first: the one cut that is forced.
        for ch in chunks {
            let (id, mut off, mut len) = (kid(ch.key), koff(ch.key), ch.len);
            while len >= c {
                self.state.split(id, off + c);
                let mut dp = self.new_data_page()?;
                self.place(&mut dp, key(id, off), c)?;
                out.data.push(dp);
                off += c;
                len -= c;
            }
            if len > 0 {
                rem.push_back(Chunk {
                    key: key(id, off),
                    len,
                    rewrite: ch.rewrite,
                });
            }
        }
        if rem.is_empty() {
            return Ok(());
        }
        let theta = (self.opts.theta * c as f64) as usize;
        let mut open = self.new_data_page()?;
        loop {
            let room = open.room() as u32;
            // 1. The first chunk that fits, within reach.
            if let Some(i) = rem
                .iter()
                .take(self.opts.lookahead)
                .position(|ch| ch.len <= room)
            {
                let ch = rem.remove(i).unwrap();
                self.place(&mut open, ch.key, ch.len)?;
                continue;
            }
            // 2. Free filling: victims that fit the room ride for free.
            if self.opts.consolidate {
                self.free_fill(&mut open, dirty)?;
            }
            if rem.is_empty() {
                break;
            }
            // 3. Still more than θ empty: cut a chunk so the page closes full,
            //    a genuine one of the flush's own before a rewrite.
            if open.room() > theta {
                let i = rem
                    .iter()
                    .take(self.opts.lookahead)
                    .position(|ch| !ch.rewrite)
                    .unwrap_or(0);
                let ch = rem.remove(i).unwrap();
                let head = open.room() as u32;
                let (id, off) = (kid(ch.key), koff(ch.key));
                self.state.split(id, off + head);
                self.place(&mut open, ch.key, head)?;
                let tail = Chunk {
                    key: key(id, off + head),
                    len: ch.len - head,
                    rewrite: ch.rewrite,
                };
                if tail.len <= self.opts.inline_threshold {
                    // Too short to be worth a chunk: the cut states it inline.
                    if let Some(Fragment::Pending(p)) = self.state.frags.get(&tail.key) {
                        self.state.pending[*p as usize].place = Place::Inline;
                    }
                } else {
                    rem.push_front(tail);
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

    // ------------------------------------------------------------ commit

    fn commit(&mut self, e: u64, dirty: &Dirty, out: Output) -> Result<(), Error> {
        // The next segment starts in the lowest reusable page, or past the end.
        let jp = match self.ready.pop_first() {
            Some(p) => p,
            None => self.file_pages,
        };
        let mut written = Vec::with_capacity(out.data.len() + out.tables.len());
        for mut dp in out.data {
            seal_page(&mut dp.buf, None, KIND_DATA, e, dp.used);
            self.storage
                .write_at(&dp.buf[..], dp.page as u64 * PAGE_SIZE as u64)?;
            let info = &mut self.state.pages[dp.page as usize];
            info.state = PageState::Data;
            info.epoch = e;
            info.written = dp.used as u16;
            let r0 = if dp.fast > 0.0 {
                dp.rate / dp.fast
            } else {
                0.0
            };
            info.drain = Drain::start(dp.fast, r0, dp.used as f64, e);
            self.stats.data_pages_written += 1;
            self.state.rerank(dp.page);
            written.push((dp.page, true));
        }
        let leaf_rate = self.fresh_rate(false);
        for (p, content, interior) in &out.tables {
            let mut buf = new_page();
            crate::page::encode_page(&mut buf, None, KIND_ADDRESS_TABLE, e, content);
            self.storage
                .write_at(&buf[..], *p as u64 * PAGE_SIZE as u64)?;
            let info = &mut self.state.pages[*p as usize];
            info.state = if *interior {
                PageState::Interior
            } else {
                PageState::Table
            };
            info.epoch = e;
            info.written = content.len() as u16;
            let len = content.len() as f64;
            info.drain = Drain::start(len, leaf_rate, len, e);
            self.table_pages.insert(*p, buf);
            self.stats.table_pages_written += 1;
            self.state.rerank(*p);
            if !*interior {
                written.push((*p, false));
            }
        }
        // A session's first flush clears the page it names for the next
        // segment (`spec/journal.md#the-start-of-a-session`).
        if self.first_flush && jp < self.file_pages {
            self.storage
                .write_at(&new_page()[..], jp as u64 * PAGE_SIZE as u64)?;
        }
        self.storage.sync()?;
        let slot = (e % 2) as u32;
        self.header.journal_pointer = jp;
        self.header.format_version = FORMAT_VERSION;
        self.header.min_reader_version = MIN_READER_VERSION;
        let mut hbuf = new_page();
        crate::page::encode_page(
            &mut hbuf,
            Some(&self.header),
            KIND_ADDRESS_TABLE,
            e,
            &out.header_content,
        );
        self.storage
            .write_at(&hbuf[..], slot as u64 * PAGE_SIZE as u64)?;
        self.stats.headers_written += 1;

        // Bookkeeping: the rotation of reusable pages first.
        for p in std::mem::take(&mut self.retiring) {
            self.state.pages[p as usize].state = PageState::Free;
            self.state.pages[p as usize].coverage = 0;
            self.ready.insert(p);
        }
        let mut retiring = Vec::new();
        let candidates: Vec<u32> = self.state.dropped_candidates.drain().collect();
        for p in candidates {
            let info = self.state.pages[p as usize];
            if info.state == PageState::Data && info.coverage == 0 {
                self.state.unrank(p);
                self.state.pages[p as usize].state = PageState::Retiring;
                self.state.reverse.remove(&p);
                retiring.push(p);
            }
        }
        for p in out.dropped_tables {
            if cfg!(debug_assertions) && self.state.pages[p as usize].state == PageState::Table {
                let live = self.live_statements_in(p);
                assert!(
                    live.is_empty(),
                    "unlinked leaf {p} still holds live statements: {live:?}"
                );
            }
            self.state.unrank(p);
            self.state.pages[p as usize].state = PageState::Retiring;
            self.table_pages.remove(&p);
            self.children.remove(&p);
            retiring.push(p);
        }
        for &p in &self.segment.pages {
            if p < self.file_pages && self.state.pages[p as usize].state == PageState::Journal {
                self.state.pages[p as usize].state = PageState::Retiring;
                retiring.push(p);
            }
        }
        self.retiring = retiring;
        self.table_pages.insert(slot, hbuf);
        self.state.pages[slot as usize].epoch = e;
        self.header_slot = slot;
        self.epoch = e;
        if jp < self.file_pages {
            self.state.pages[jp as usize].state = PageState::Journal;
        }
        self.segment = crate::journal::SegmentWriter::new(e + 1, jp);
        self.segment_records = crate::journal::Records::new();
        debug_assert!(self
            .state
            .frags
            .values()
            .all(|f| !matches!(f, Fragment::Pending(_))));
        self.state.pending.clear();
        self.first_flush = false;
        self.cache.clear();
        for (&id, rec) in &dirty.records {
            if rec.freed && !self.state.allocs.contains_key(&id) {
                let live = self
                    .state
                    .recyclable
                    .get(&id)
                    .is_some_and(|r| r.tombstone.is_some());
                self.ids.release(id, live);
            }
        }
        self.stats.flushes += 1;
        // What the next flush's fold takes from these pages is what fresh
        // pages lose; the cursor looks for its next page among the data pages.
        self.cons.just_written = written
            .iter()
            .map(|&(p, data)| (p, data, self.state.pages[p as usize].coverage))
            .collect();
        let data_pages: Vec<u32> = written
            .iter()
            .filter(|&&(_, data)| data)
            .map(|&(p, _)| p)
            .collect();
        if !data_pages.is_empty() {
            self.cons.prev_data_pages = data_pages;
        }
        self.after_commit()?;
        self.truncate(false)
    }

    /// Shrinks the file to end after its highest page that is not reusable,
    /// once the reusable tail reaches the threshold, or always if `force`.
    pub(crate) fn truncate(&mut self, force: bool) -> Result<(), Error> {
        let mut top = self.file_pages;
        while top > 2 {
            let p = top - 1;
            let st = self.state.pages[p as usize].state;
            let empty_journal = st == PageState::Journal
                && self.segment.transactions == 0
                && self.segment.pages == [p];
            if st == PageState::Free || (force && empty_journal) {
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
            self.state.pages[p as usize] = PageInfo::default();
        }
        self.state.pages.truncate(top.max(2) as usize);
        self.file_pages = top;
        self.stats.truncations += 1;
        Ok(())
    }
}
