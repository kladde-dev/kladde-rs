//! The flush (`impl/flush.md`, `impl/consolidation.md`): take the header,
//! fold the journal, consolidate, pack data pages, cut the address table, and
//! commit.

use std::collections::VecDeque;

use crate::consts::*;
use crate::error::{corrupt, Error};
use crate::fold::{fold, Fold, Source};
use crate::hash::{IdMap, IdSet};
use crate::page::{decode_page, new_page, seal_page, PageBuf};
use crate::state::*;
use crate::statement::{Kind, Stmt, TableReader, TableWriter};
use crate::store::{take_page, Inner, Tx};

/// A data page the flush fills.
pub(crate) struct DataPage {
    pub page: u32,
    pub buf: Box<PageBuf>,
    pub used: usize,
    /// The bytes of its content expected to drain: what the prior on its
    /// chunks' class starts from.
    pub fast: f64,
}

impl DataPage {
    pub fn room(&self) -> usize {
        MAX_PAGE_CONTENT - self.used
    }
}

/// Per id, the fragments that the bytes it shifted within itself become:
/// each a destination range, and the pending content that states it there.
type Restated = IdMap<Vec<(u32, u32, Pending)>>;

/// What becomes of part of a range an id shifted within itself.
enum Shifted {
    /// A fragment, restated over `[a, b)` where its bytes lie.
    State(u32, u32, Pending),
    /// A run of fragments, written again over `[a, b)` as one chunk, from
    /// the committed bytes at `from`.
    Write { a: u32, b: u32, from: u32 },
}

/// About what a `Ref` statement's framing takes, in bytes: a tag, an offset,
/// a size and an address, each a short varint.
const REF_FRAMING: u64 = 9;

/// About what the framing of an `Inline` or a `Zero` statement takes.
const SHORT_FRAMING: u64 = 4;

/// Zeros, stated as such.
fn zero_pending() -> Pending {
    Pending {
        origin: Origin::None,
        place: Place::Zero,
        heat: 0,
        rewrite: false,
        from: None,
    }
}

/// Whether the cut's `derive` states `next` in one statement with `prev`,
/// which it directly follows: the same id and kind, adjacent, and for a
/// `Ref` adjacent bytes in one data page, for an `Inline` no longer than
/// `threshold` together.
fn continues(prev: &Stmt, next: &Stmt, threshold: u32) -> bool {
    prev.id == next.id
        && prev.kind == next.kind
        && prev.offset + prev.size == next.offset
        && match next.kind {
            Kind::Ref => {
                prev.address + prev.size as u64 == next.address
                    && split_address(prev.address).0 == split_address(next.address).0
            }
            Kind::Inline => prev.size + next.size <= threshold,
            _ => true,
        }
}

/// Splits the pieces of `table` that name `id`'s committed bytes where those
/// bytes enter or leave a range that more than one piece names, and returns
/// those ranges: so that a piece lies wholly inside them, to be written
/// again, or wholly outside, to be restated.
fn split_at_copies(id: u32, table: &mut crate::fold::Table) -> Vec<(u32, u32)> {
    let keys: Vec<(u32, Source)> = table.pieces.iter().map(|(&k, &s)| (k, s)).collect();
    let copied = copied_ranges(id, &keys, table.size);
    if copied.is_empty() {
        return copied;
    }
    for (i, &(k, s)) in keys.iter().enumerate() {
        let Source::Storage(src, so) = s else {
            continue;
        };
        if src != id {
            continue;
        }
        let end = keys.get(i + 1).map_or(table.size, |n| n.0);
        let se = so + (end - k);
        for &(a, b) in &copied {
            for cut in [a, b] {
                if so < cut && cut < se {
                    table
                        .pieces
                        .insert(k + (cut - so), Source::Storage(id, cut));
                }
            }
        }
    }
    copied
}

/// The ranges of `id`'s committed bytes that more than one piece of its
/// table names, merged: what the segment copied rather than moved.
fn copied_ranges(id: u32, keys: &[(u32, Source)], size: u32) -> Vec<(u32, u32)> {
    let mut sources: Vec<(u32, u32)> = keys
        .iter()
        .enumerate()
        .filter_map(|(i, &(k, s))| match s {
            Source::Storage(src, so) if src == id => {
                let end = keys.get(i + 1).map_or(size, |n| n.0);
                Some((so, so + (end - k)))
            }
            _ => None,
        })
        .collect();
    sources.sort_unstable();
    let mut copied: Vec<(u32, u32)> = Vec::new();
    let mut reach = 0u32;
    for &(a, b) in &sources {
        if a < reach {
            let overlap = (a, b.min(reach));
            match copied.last_mut() {
                Some(last) if last.1 >= overlap.0 => last.1 = last.1.max(overlap.1),
                _ => copied.push(overlap),
            }
        }
        reach = reach.max(b);
    }
    copied
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
    pub tables: Vec<(u32, Vec<u8>)>,
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
        self.state.carry.clear();
        self.flush_rewritten.clear();
        self.begin_consolidation();
        let mut dirty = Dirty::default();

        // 1. The header's content, taken whole.
        self.take_header(&mut dirty)?;

        // 2. The fold, with every read from elsewhere hoisted into the arena,
        //    but for the bytes an id shifted within itself, which it restates.
        //    What it supersedes is what pages lose naturally, and so is what
        //    the consolidator state's own rewrite supersedes; bytes restated
        //    where they lie are charged to their page again, and so are no
        //    loss.
        let mut f = fold(&self.segment_records, |id| {
            self.state.allocs.get(&id).map(|m| m.size)
        });
        let restated = self.hoist(&mut f)?;
        self.state.natural = true;
        let folded = self.apply_fold(f, restated, &mut dirty);
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
                        from: None,
                    }
                } else {
                    Pending {
                        origin: Origin::File(a),
                        place: Place::Data(a),
                        heat,
                        rewrite: false,
                        from: Some(stmt),
                    }
                }
            }
            Fragment::Zero { .. } => Pending {
                origin: Origin::None,
                place: Place::Zero,
                heat,
                rewrite: false,
                from: None,
            },
            Fragment::Pending(_) => return,
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
            if s.kind.is_content() {
                let (start, end) = (s.offset, s.offset.saturating_add(s.size));
                for (a, b, f) in self.owned_in(id, start, end, page) {
                    self.take_in_place(id, a, b, f, heat, dirty);
                }
            }
            // A size statement or tombstone that lived here is stated again.
            let held = match self.state.allocs.get(&id) {
                Some(m) => m.size_statement,
                None => self.state.recyclable.get(&id).and_then(|r| r.tombstone),
            };
            if held.is_some_and(|h| self.state.slab.page(h) == page) {
                self.touch(id, false, dirty).replace = true;
            }
            self.state.drop_physically(id);
        }
        Ok(())
    }

    /// The bytes that stating again what table page `page` holds would take,
    /// computed from the fragment map without taking anything: a statement
    /// per run of the fragments its live statements own, merged as the cut
    /// merges a run, and a `Size` or `Tombstone` for each id whose size
    /// statement or tombstone it holds, encoded as one run of their own.
    ///
    /// Merged into a page among other statements, they add at most this
    /// much to it: each statement's delta to its neighbour there is no
    /// larger than in the run, whose first statement already pays its whole
    /// id and offset, and merging with neighbours only drops statements.
    pub(crate) fn restatement_len(&self, page: u32) -> Result<usize, Error> {
        let buf = self
            .table_pages
            .get(&page)
            .ok_or_else(|| corrupt("a table page is not resident"))?;
        let d = decode_page(buf, page < 2)
            .map_err(|_| corrupt("a resident table page does not decode"))?;
        let (_, mut r) = TableReader::open(&buf[..], d.content)?;
        let threshold = self.opts.inline_threshold.clamp(1, MAX_INLINE as u32);
        let mut stmts: Vec<Stmt> = Vec::new();
        let mut held: Vec<u32> = Vec::new();
        while let Some(dec) = r.next_stmt()? {
            let s = dec.stmt;
            let id = s.id;
            if s.kind.is_content() {
                let (start, end) = (s.offset, s.offset.saturating_add(s.size));
                for (a, b, f) in self.owned_in(id, start, end, page) {
                    let next = match f {
                        Fragment::Bytes { stmt, .. }
                            if self.state.slab.kinds[stmt.idx()] == Kind::Inline =>
                        {
                            Stmt::inline(id, a, b - a)
                        }
                        Fragment::Bytes { page, offset, .. } => {
                            Stmt::reference(id, a, b - a, address(page, offset))
                        }
                        Fragment::Zero { .. } => Stmt::zero(id, a, b - a),
                        Fragment::Pending(_) => continue,
                    };
                    match stmts.last_mut() {
                        Some(prev) if continues(prev, &next, threshold) => prev.size += next.size,
                        _ => stmts.push(next),
                    }
                }
            }
            let h = match self.state.allocs.get(&id) {
                Some(m) => m.size_statement,
                None => self.state.recyclable.get(&id).and_then(|r| r.tombstone),
            };
            if h.is_some_and(|h| self.state.slab.page(h) == page) && held.last() != Some(&id) {
                held.push(id);
            }
        }
        // As the cut states them: a `Size` only where no content statement
        // ends at the size, and a tombstone wherever one may be needed.
        let ends: IdSet = stmts
            .iter()
            .filter(|s| s.offset + s.size == self.state.size_of(s.id))
            .map(|s| s.id)
            .collect();
        for id in held {
            match self.state.allocs.get(&id) {
                Some(m) if !ends.contains(&id) => stmts.push(Stmt::size_to(id, m.size)),
                Some(_) => {}
                None => stmts.push(Stmt::tombstone(id)),
            }
        }
        stmts.sort_by_key(Stmt::sort_key);
        let zeros = [0u8; MAX_INLINE];
        let mut w = TableWriter::new();
        w.end_children();
        for s in &stmts {
            let payload = if s.kind == Kind::Inline {
                &zeros[..s.size as usize]
            } else {
                &[][..]
            };
            w.push(s, payload);
        }
        Ok(w.len() - 1)
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
    /// than where it already lies, and turns it into a literal; except a
    /// piece that an id still in its committed incarnation shifted within
    /// itself, by a splice or a move, whose bytes no other piece names. Such
    /// a piece is restated instead, but for runs of fragments that are
    /// cheaper to write again than to describe at this and the later shifts
    /// the allocation's history predicts, which are read and written again
    /// as one chunk each (see [`restatement`](Self::restatement)): returned
    /// per id are the fragments it becomes, each a destination range and
    /// what states it there.
    ///
    /// Bytes that two pieces name were copied, and are written again, so that
    /// no data byte is referenced by two live statements.
    fn hoist(&mut self, f: &mut Fold) -> Result<Restated, Error> {
        let mut reads: Vec<(u32, u32, u32, u32, u32)> = Vec::new();
        let mut restated = Restated::default();
        let mut copies: IdMap<Vec<(u32, u32)>> = IdMap::default();
        for (&id, e) in f.ids.iter_mut() {
            if e.exists && e.existed && !e.ended && e.committed_live {
                let copied = split_at_copies(id, &mut e.table);
                if !copied.is_empty() {
                    copies.insert(id, copied);
                }
            }
        }
        // Runs of shifted bytes to write again, as (id, at, end, from).
        let mut merges: Vec<(u32, u32, u32, u32)> = Vec::new();
        let mut shifted_ids: Vec<u32> = Vec::new();
        let (epoch, decay) = (self.state.flush_epoch, self.opts.shift_decay);
        for (&id, e) in &f.ids {
            if !e.exists {
                continue;
            }
            let keys: Vec<(u32, Source)> = e.table.pieces.iter().map(|(&k, &s)| (k, s)).collect();
            let persistent = e.existed && !e.ended && e.committed_live;
            let copied = copies.get(&id).map_or(&[][..], |c| &c[..]);
            let horizon = self
                .state
                .allocs
                .get(&id)
                .map_or(0.0, |m| m.shifts_at(epoch, decay));
            let mut shifted_here = false;
            for (i, &(k, s)) in keys.iter().enumerate() {
                if let Source::Storage(src, so) = s {
                    let identity = src == id && so == k && e.committed_live;
                    if identity {
                        continue;
                    }
                    let end = keys.get(i + 1).map_or(e.table.size, |n| n.0);
                    let len = end - k;
                    let shifted = persistent && src == id;
                    if shifted && !copied.iter().any(|&(a, b)| a < so + len && so < b) {
                        if let Some(parts) = self.restatement(id, k, end, so, horizon) {
                            shifted_here = true;
                            for part in parts {
                                match part {
                                    Shifted::State(a, b, p) => {
                                        restated.entry(id).or_default().push((a, b, p));
                                    }
                                    Shifted::Write { a, b, from } => merges.push((id, a, b, from)),
                                }
                            }
                            continue;
                        }
                    }
                    reads.push((id, k, src, so, len));
                }
            }
            if shifted_here {
                shifted_ids.push(id);
            }
        }
        for id in shifted_ids {
            if let Some(m) = self.state.allocs.get_mut(&id) {
                m.shifts = (m.shifts_at(epoch, decay) + 1.0) as f32;
                m.shifted_epoch = epoch as u32;
            }
        }
        for (id, a, b, from) in merges {
            let mut bytes = vec![0u8; (b - a) as usize];
            self.read_committed(id, from, &mut bytes)?;
            let pos = self.segment_records.arena.len() as u64;
            self.segment_records.arena.extend_from_slice(&bytes);
            let p = Pending {
                origin: Origin::Arena(pos),
                place: Place::Unplaced,
                heat: 0,
                rewrite: false,
                from: None,
            };
            restated.entry(id).or_default().push((a, b, p));
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
        Ok(restated)
    }

    /// What becomes of the committed bytes `[so, so + end - k)` of `id`,
    /// shifted to `[k, end)`: in order, runs of the fragments they lie in,
    /// each restated where it lies, and runs written again as one chunk, to
    /// merge fragments whose statements would cost more than their bytes.
    ///
    /// The costs are estimates in bytes: a `Ref` statement takes about
    /// [`REF_FRAMING`] bytes, an `Inline` one its payload and
    /// [`SHORT_FRAMING`] more, and a `Zero` one [`SHORT_FRAMING`]; writing a
    /// run again costs its bytes and a `Ref` for every page it fills. A
    /// statement is paid not only now but at every later flush that shifts
    /// the bytes again, which `horizon` predicts from the allocation's past
    /// shifts; so a run is written again where its statements, times one
    /// plus the horizon, would cost more than its bytes plus its new `Ref`s,
    /// times the same.
    fn restatement(
        &self,
        id: u32,
        k: u32,
        end: u32,
        so: u32,
        horizon: f64,
    ) -> Option<Vec<Shifted>> {
        /// A fragment of the shifted range: where it goes, what restates
        /// it, what that costs per flush, and whether it may be merged.
        struct Piece {
            a: u32,
            b: u32,
            p: Pending,
            cost: u64,
            mergeable: bool,
        }
        let se = so + (end - k);
        let size = self.state.size_of(id);
        let mut pieces = Vec::new();
        let mut at = so;
        while at < se {
            if at >= size {
                // Past the committed size: zeros, below the old size never
                // stated, and so never worth merging.
                pieces.push(Piece {
                    a: k + at - so,
                    b: end,
                    p: zero_pending(),
                    cost: SHORT_FRAMING,
                    mergeable: false,
                });
                break;
            }
            let (fk, frag, fend) = self.state.resolve(id, at)?;
            let d = at - koff(fk);
            let to = fend.min(se);
            let (p, cost) = match frag {
                Fragment::Bytes { page, offset, stmt } => {
                    let a = address(page, offset + d as u16);
                    if self.state.slab.kinds[stmt.idx()] == Kind::Inline {
                        let p = Pending {
                            origin: Origin::File(a),
                            place: Place::Inline,
                            heat: 0,
                            rewrite: false,
                            from: None,
                        };
                        (p, SHORT_FRAMING + (to - at) as u64)
                    } else {
                        let p = Pending {
                            origin: Origin::File(a),
                            place: Place::Data(a),
                            heat: 0,
                            rewrite: false,
                            from: Some(stmt),
                        };
                        (p, REF_FRAMING)
                    }
                }
                Fragment::Zero { .. } => (zero_pending(), SHORT_FRAMING),
                Fragment::Pending(p) => {
                    let mut q = self.state.pending[p as usize].advanced(d);
                    q.heat = 0;
                    let cost = match q.place {
                        Place::Data(_) | Place::Unplaced => REF_FRAMING,
                        Place::Inline => SHORT_FRAMING + (to - at) as u64,
                        Place::Zero => SHORT_FRAMING,
                    };
                    (q, cost)
                }
            };
            pieces.push(Piece {
                a: k + at - so,
                b: k + to - so,
                p,
                cost,
                mergeable: true,
            });
            at = to;
        }

        // The runs worth writing again: in every stretch of mergeable pieces,
        // Kadane's maximum-weight run, where a piece weighs what restating
        // it costs over the horizon less its bytes, kept if it outweighs the
        // `Ref`s it would need over the same horizon.
        let m = 1.0 + horizon;
        let weight = |p: &Piece| m * p.cost as f64 - (p.b - p.a) as f64;
        let worth = |run: &[Piece]| {
            let len = (run[run.len() - 1].b - run[0].a) as u64;
            let refs = m * (REF_FRAMING * len.div_ceil(MAX_PAGE_CONTENT as u64)) as f64;
            run.iter().map(weight).sum::<f64>() > refs
        };
        let mut merge = vec![false; pieces.len()];
        let (mut cur, mut from) = (0.0f64, 0usize);
        let mut best: Option<(f64, usize, usize)> = None;
        let close = |best: &mut Option<(f64, usize, usize)>, merge: &mut [bool]| {
            if let Some((_, i, j)) = best.take() {
                if worth(&pieces[i..=j]) {
                    merge[i..=j].fill(true);
                }
            }
        };
        for (j, p) in pieces.iter().enumerate() {
            if !p.mergeable {
                close(&mut best, &mut merge);
                cur = 0.0;
                continue;
            }
            let x = weight(p);
            if cur <= 0.0 {
                close(&mut best, &mut merge);
                (cur, from) = (x, j);
            } else {
                cur += x;
            }
            if cur > 0.0 && best.is_none_or(|(s, _, _)| cur > s) {
                best = Some((cur, from, j));
            }
        }
        close(&mut best, &mut merge);

        let mut out = Vec::new();
        let mut i = 0;
        while i < pieces.len() {
            if merge[i] {
                let mut j = i;
                while j + 1 < pieces.len() && merge[j + 1] {
                    j += 1;
                }
                let (a, b) = (pieces[i].a, pieces[j].b);
                out.push(Shifted::Write {
                    a,
                    b,
                    from: so + (a - k),
                });
                i = j + 1;
            } else {
                let p = &pieces[i];
                out.push(Shifted::State(p.a, p.b, p.p));
                i += 1;
            }
        }
        Some(out)
    }

    /// Takes the pieces of `table` that change `id`'s content: literals,
    /// grouped into runs that go `Inline` when short; zeros below
    /// `zero_limit`, which deny content the id had before; and `restated`,
    /// the fragments of the pieces [`hoist`](Self::hoist) left to restate.
    fn take_pieces(
        &mut self,
        id: u32,
        table: &crate::fold::Table,
        identity: bool,
        zero_limit: u32,
        restated: Vec<(u32, u32, Pending)>,
        dirty: &mut Dirty,
    ) {
        let keys: Vec<(u32, Source)> = table.pieces.iter().map(|(&k, &s)| (k, s)).collect();
        let mut i = 0;
        while i < keys.len() {
            let (k, s) = keys[i];
            let end = keys.get(i + 1).map_or(table.size, |n| n.0);
            match s {
                Source::Storage(src, so) if identity && src == id && so == k => i += 1,
                Source::Storage(src, _) => {
                    debug_assert!(
                        identity && src == id && restated.iter().any(|t| t.0 == k),
                        "a piece from elsewhere that was neither hoisted nor restated"
                    );
                    i += 1;
                }
                Source::Zero => {
                    let e = end.min(zero_limit);
                    if k < e {
                        let p = Pending {
                            origin: Origin::None,
                            place: Place::Zero,
                            heat: 0,
                            rewrite: false,
                            from: None,
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
                    // A short run goes inline, unless it borders shifted
                    // bytes written again, which it joins in a data page,
                    // so that the cut can state both in one `Ref`.
                    let borders_merge = restated.iter().any(|&(a, b, p)| {
                        matches!(p.origin, Origin::Arena(_)) && (b == k || a == run_end)
                    });
                    let inline = run_end - k <= self.opts.inline_threshold.min(MAX_INLINE as u32);
                    let place = if inline && !borders_merge {
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
                            from: None,
                        };
                        self.state.take(id, pk, pe, p, dirty);
                        self.stats.fresh_bytes += (pe - pk) as u64;
                    }
                    i = j + 1;
                }
            }
        }
        for (a, b, p) in restated {
            // Zeros at or past the old size are pending already, as the
            // zeros the growth exposed.
            let b = if p.place == Place::Zero {
                b.min(zero_limit)
            } else {
                b
            };
            if a >= b {
                continue;
            }
            self.state.take(id, a, b, p, dirty);
            if let Origin::Arena(_) = p.origin {
                // A run written again to merge its fragments.
                self.stats.merged_bytes += (b - a) as u64;
                continue;
            }
            if let Place::Data(addr) = p.place {
                // The bytes lie where they did, at a new offset: widen the
                // reverse index's window, the one rule that keeps it true.
                self.state.note_reverse(split_address(addr).0, id, a, b);
            }
            self.stats.restated_bytes += (b - a) as u64;
        }
    }

    /// Grows `id` to `new`. The range the growth exposes is taken as zeros,
    /// which whatever the flush writes there replaces: every byte an
    /// allocation gains is stated in the epoch that gains it
    /// (`spec/address-table.md#the-coverage-rule`).
    pub(crate) fn grow(&mut self, id: u32, new: u32, dirty: &mut Dirty) {
        let p = Pending {
            origin: Origin::None,
            place: Place::Zero,
            heat: 0,
            rewrite: false,
            from: None,
        };
        self.state.grow_size_to(id, new, p, dirty);
    }

    fn apply_fold(
        &mut self,
        f: Fold,
        mut restated: Restated,
        dirty: &mut Dirty,
    ) -> Result<(), Error> {
        for (id, e) in f.ids {
            match (e.existed, e.ended, e.exists) {
                (true, false, true) => {
                    self.touch(id, e.written, dirty);
                    let old = self.state.size_of(id);
                    let new = e.table.size;
                    if new < old {
                        self.state.shrink_size_to(id, new);
                    } else if new > old {
                        self.grow(id, new, dirty);
                    }
                    let takes = restated.remove(&id).unwrap_or_default();
                    self.take_pieces(id, &e.table, true, old.min(new), takes, dirty);
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
                    } else if new > old {
                        self.grow(id, new, dirty);
                    }
                    self.take_pieces(id, &e.table, false, old.min(new), Vec::new(), dirty);
                }
                (false, _, true) => {
                    self.allocate(id, e.table.size, dirty);
                    self.take_pieces(id, &e.table, false, 0, Vec::new(), dirty);
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
            // A live tombstone stays the size statement, with its pin, until
            // the cut binds the new incarnation's.
            meta.size_statement = r.tombstone;
        }
        self.state.allocs.insert(id, meta);
        let rec = self.touch(id, true, dirty);
        rec.allocated = true;
        self.grow(id, n, dirty);
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
        if let Some(s) = meta.size_statement {
            self.state.unpin(s);
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
            Filling::taken(PageKind::Data),
        )?;
        Ok(DataPage {
            page,
            buf: new_page(),
            used: 0,
            fast: 0.0,
        })
    }

    /// Places the chunk at `key` into `dp`, copying its bytes. Moved content
    /// brings its source page's draining share along; fresh content drains;
    /// a description defragmentation's rewrite, cold by selection, is static.
    pub(crate) fn place(&mut self, dp: &mut DataPage, key: Key, len: u32) -> Result<(), Error> {
        let Some(Fragment::Pending(p)) = self.state.frags.get(&key).copied() else {
            return Err(corrupt("placing a chunk that is not pending"));
        };
        let pending = self.state.pending[p as usize];
        let share = match pending.origin {
            _ if pending.rewrite => 0.0,
            Origin::File(a) => match self.state.pages[split_address(a).0 as usize] {
                Page::Live(l) => l.drain.share as f64,
                _ => 1.0,
            },
            _ => 1.0,
        };
        dp.fast += share * len as f64;
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
        let slot = (e % 2) as u32;
        // Every page this flush filled, and nothing else, is written here.
        let written: Vec<u32> = (out.data.iter().map(|dp| dp.page))
            .chain(out.tables.iter().map(|&(p, _)| p))
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
        // What each page holds as it is written, and the rate its content
        // starts to drain at; and the data pages and leaves among them.
        let mut held: IdMap<(u16, Drain)> = IdMap::default();
        let mut fresh: Vec<(u32, bool)> = Vec::with_capacity(written.len());
        for mut dp in out.data {
            seal_page(&mut dp.buf, None, KIND_DATA, e, dp.used);
            self.storage
                .write_at(&dp.buf[..], dp.page as u64 * PAGE_SIZE as u64)?;
            let pi = dp.fast / (dp.used as f64).max(1.0);
            let drain = Drain::start(self.cons.prior[0], pi, e);
            held.insert(dp.page, (dp.used as u16, drain));
            self.stats.data_pages_written += 1;
            fresh.push((dp.page, true));
        }
        let leaf_prior = self.cons.prior[1];
        for (p, content) in &out.tables {
            let mut buf = new_page();
            crate::page::encode_page(&mut buf, None, KIND_ADDRESS_TABLE, e, content);
            self.storage
                .write_at(&buf[..], *p as u64 * PAGE_SIZE as u64)?;
            let drain = Drain::start(leaf_prior, 1.0, e);
            held.insert(*p, (content.len() as u16, drain));
            self.table_pages.insert(*p, buf);
            self.stats.table_pages_written += 1;
            if !self.state.has_children(*p) {
                fresh.push((*p, false));
            }
        }
        held.insert(slot, (out.header_content.len() as u16, Drain::default()));
        // A session's first flush clears the page it names for the next
        // segment (`spec/journal.md#the-start-of-a-session`).
        if self.first_flush && jp < self.file_pages {
            self.storage
                .write_at(&new_page()[..], jp as u64 * PAGE_SIZE as u64)?;
        }
        self.storage.sync()?;
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

        // The pages the flush filled are in the world now.
        for &p in &written {
            match self.state.pages[p as usize] {
                Page::Filling(f) => {
                    let (size, drain) = held[&p];
                    self.state.pages[p as usize] = Page::Live(f.commit(e, size, drain));
                }
                other => unreachable!("page {p} was written but is {other:?}"),
            }
        }

        // Bookkeeping: the rotation of reusable pages first.
        for p in std::mem::take(&mut self.retiring) {
            debug_assert_eq!(self.state.pages[p as usize], Page::Fallback);
            self.state.pages[p as usize] = Page::Reusable;
            self.ready.insert(p);
        }
        let mut retiring = Vec::new();
        let candidates: Vec<u32> = self.state.dropped_candidates.drain().collect();
        for p in candidates {
            if let Page::Live(Live {
                coverage: 0,
                kind: PageKind::Data,
                ..
            }) = self.state.pages[p as usize]
            {
                self.state.unrank(p);
                self.state.pages[p as usize] = Page::Fallback;
                self.state.reverse.remove(&p);
                retiring.push(p);
            }
        }
        for p in out.dropped_tables {
            if cfg!(debug_assertions) {
                let live = self.live_statements_in(p);
                assert!(
                    live.is_empty(),
                    "unlinked table page {p} still holds live statements: {live:?}"
                );
            }
            self.state.unrank(p);
            self.state.pages[p as usize] = Page::Fallback;
            self.table_pages.remove(&p);
            self.state.children.remove(&p);
            retiring.push(p);
        }
        for &p in &self.segment.pages {
            if p < self.file_pages && self.state.pages[p as usize] == Page::Journal {
                self.state.pages[p as usize] = Page::Fallback;
                retiring.push(p);
            }
        }
        self.retiring = retiring;
        // The header this commit replaced leaves the world; the flush took
        // everything it stated.
        let old = self.header_slot;
        if cfg!(debug_assertions) {
            let live = self.live_statements_in(old);
            assert!(
                live.is_empty(),
                "the replaced header still holds live statements: {live:?}"
            );
        }
        self.state.pages[old as usize] = Page::Fallback;
        self.state.children.remove(&old);
        self.table_pages.remove(&old);
        self.table_pages.insert(slot, hbuf);
        self.header_slot = slot;
        self.epoch = e;
        for p in written {
            self.state.rerank(p);
        }
        if jp < self.file_pages {
            debug_assert_eq!(self.state.pages[jp as usize], Page::Reusable);
            self.state.pages[jp as usize] = Page::Journal;
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
        // What these pages lose in their first epochs teaches the file's
        // empirical prior; the cursor looks for its next page among the data
        // pages.
        self.cons.just_written = fresh.iter().map(|&(p, _)| p).collect();
        self.cons
            .young
            .extend(fresh.iter().map(|&(page, data)| crate::consolidate::Young {
                page,
                epoch: e,
                data,
                events: 0.0,
                exposure: 0.0,
            }));
        let data_pages: Vec<u32> = fresh
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
            let page = self.state.pages[p as usize];
            let empty_journal = page == Page::Journal
                && self.segment.transactions == 0
                && self.segment.pages == [p];
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
