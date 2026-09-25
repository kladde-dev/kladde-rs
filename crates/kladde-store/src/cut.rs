//! The cut (`impl/consolidation.md#one-dirty-set-and-why-statements-are-derived-last`):
//! derive every statement from the dirty set, lay them out — the hottest in
//! the header, the rest in leaves in key order — give the room the last page
//! has left to the fillers, and bind.

use std::collections::BTreeMap;

use crate::consts::*;
use crate::error::{corrupt, Error};
use crate::flush::Output;
use crate::hash::IdMap;
use crate::page::decode_page;
use crate::state::*;
use crate::statement::{Kind, Stmt, TableReader, TableWriter};
use crate::store::{take_page, Inner};

/// A statement the cut derived, and what it states.
#[derive(Clone, Debug)]
pub(crate) struct Derived {
    pub stmt: Stmt,
    /// The pending fragments it states, for content statements.
    pub keys: Vec<Key>,
    pub payload: Vec<u8>,
    pub heat: u32,
}

/// How many child references the header keeps before an interior layer
/// takes them over.
const HEADER_CHILD_LIMIT: usize = 600;
/// Children per interior page.
const INTERIOR_FANOUT: usize = 800;
/// Header bytes the first layout keeps back, for leaves the fillers may add.
const HEADER_RESERVE: usize = 16;

/// Which page each derived statement goes to, by index into the statements.
struct Layout {
    header: Vec<usize>,
    leaves: Vec<Vec<usize>>,
}

/// The size statements a set of records calls for, and the ids whose old
/// grow witness they make redundant.
struct Sizes {
    stmts: Vec<Derived>,
    retire: Vec<u32>,
}

/// The encoded length of the statements `set`, without a child list.
fn statements_len(set: &[usize], derived: &[Derived]) -> usize {
    let mut w = TableWriter::new();
    w.end_children();
    for &i in set {
        w.push(&derived[i].stmt, &derived[i].payload);
    }
    w.len() - 1
}

/// A bound on the header's child list, with its delimiter, for the `kept`
/// leaves and `new` more.
fn children_len(kept: &[u32], new: usize) -> usize {
    let n = kept.len() + new;
    if n > HEADER_CHILD_LIMIT {
        5 * n.div_ceil(INTERIOR_FANOUT) + 1
    } else {
        TableWriter::children_len(kept) + 5 * new
    }
}

fn header_len(set: &[usize], derived: &[Derived], kept: &[u32], new_leaves: usize) -> usize {
    children_len(kept, new_leaves) + statements_len(set, derived)
}

fn sort_by_key(set: &mut [usize], derived: &[Derived]) {
    set.sort_by_key(|&i| derived[i].stmt.sort_key());
}

/// Cuts `order`, sorted by key, into leaves, each closed when the next
/// statement does not fit.
fn split_leaves(order: &[usize], derived: &[Derived]) -> Vec<Vec<usize>> {
    let mut out = Vec::new();
    let mut w = TableWriter::new();
    w.end_children();
    let mut cur: Vec<usize> = Vec::new();
    for &i in order {
        let l = w.statement_len(&derived[i].stmt);
        if w.len() + l > MAX_PAGE_CONTENT && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            w = TableWriter::new();
            w.end_children();
        }
        w.push(&derived[i].stmt, &derived[i].payload);
        cur.push(i);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The header first, with the hottest statements it can hold; the rest in
/// leaves, in key order. `derived` is sorted by key.
fn layout(derived: &[Derived], kept: &[u32]) -> Layout {
    let all: Vec<usize> = (0..derived.len()).collect();
    if kept.len() <= HEADER_CHILD_LIMIT
        && header_len(&all, derived, kept, 0) + HEADER_RESERVE <= MAX_HEADER_CONTENT
    {
        return Layout {
            header: all,
            leaves: Vec::new(),
        };
    }
    let mut order = all.clone();
    order.sort_by_key(|&i| (derived[i].heat, derived[i].stmt.sort_key()));
    let total: usize = derived
        .iter()
        .map(|d| TableWriter::standalone_len(&d.stmt))
        .sum();
    let mut slack = 64usize;
    loop {
        let estimate = total / MAX_PAGE_CONTENT + 2;
        let cap = MAX_HEADER_CONTENT
            .saturating_sub(children_len(kept, estimate) + HEADER_RESERVE + slack);
        let mut used = 0usize;
        let mut chosen = vec![false; derived.len()];
        for &i in &order {
            let l = TableWriter::standalone_len(&derived[i].stmt);
            if used + l <= cap {
                used += l;
                chosen[i] = true;
            }
        }
        let header: Vec<usize> = all.iter().copied().filter(|&i| chosen[i]).collect();
        let rest: Vec<usize> = all.iter().copied().filter(|&i| !chosen[i]).collect();
        let leaves = split_leaves(&rest, derived);
        if header_len(&header, derived, kept, leaves.len()) + HEADER_RESERVE <= MAX_HEADER_CONTENT {
            return Layout { header, leaves };
        }
        slack += 256;
    }
}

/// Moves the header's coldest statements into leaves until it fits.
fn fit_header(lay: &mut Layout, derived: &[Derived], kept: &[u32]) {
    let mut spill: Vec<usize> = Vec::new();
    loop {
        let mut sorted = spill.clone();
        sort_by_key(&mut sorted, derived);
        let spill_leaves = split_leaves(&sorted, derived).len();
        let len = header_len(&lay.header, derived, kept, lay.leaves.len() + spill_leaves);
        if len <= MAX_HEADER_CONTENT || lay.header.is_empty() {
            break;
        }
        let coldest = (0..lay.header.len())
            .max_by_key(|&j| {
                (
                    derived[lay.header[j]].heat,
                    derived[lay.header[j]].stmt.sort_key(),
                )
            })
            .unwrap();
        spill.push(lay.header.remove(coldest));
    }
    if !spill.is_empty() {
        sort_by_key(&mut spill, derived);
        lay.leaves.extend(split_leaves(&spill, derived));
    }
}

/// Puts `extra` into the last page of `lay`: the last leaf, or the header if
/// there is none. What does not fit there spills into more leaves.
fn add_to_last(lay: &mut Layout, extra: Vec<usize>, derived: &[Derived], kept: &[u32]) {
    if extra.is_empty() {
        return;
    }
    match lay.leaves.pop() {
        Some(mut last) => {
            last.extend(extra);
            sort_by_key(&mut last, derived);
            lay.leaves.extend(split_leaves(&last, derived));
        }
        None => {
            lay.header.extend(extra);
            sort_by_key(&mut lay.header, derived);
        }
    }
    fit_header(lay, derived, kept);
}

impl Inner {
    /// Unlinks the leaves this flush rewrote, and those nothing keeps alive
    /// any more, whose statements are dead but still physically present
    /// until now. Returns the leaves that stay.
    fn unlink_leaves(&mut self, out: &mut Output) -> Result<Vec<u32>, Error> {
        let leaves = self.all_leaves();
        let mut kept = Vec::with_capacity(leaves.len());
        for p in leaves {
            if self.flush_rewritten.contains(&p) {
                out.dropped_tables.push(p);
            } else if self.state.pages[p as usize].state == PageState::Table
                && self.state.pages[p as usize].coverage == 0
            {
                self.drop_page_statements(p)?;
                out.dropped_tables.push(p);
            } else {
                kept.push(p);
            }
        }
        // The interior layer is rebuilt by every cut that needs one.
        let header_children = self
            .children
            .get(&self.header_slot)
            .cloned()
            .unwrap_or_default();
        for p in header_children {
            if self.state.pages[p as usize].state == PageState::Interior {
                out.dropped_tables.push(p);
            }
        }
        Ok(kept)
    }

    /// Every live leaf, however deep.
    pub(crate) fn all_leaves(&self) -> Vec<u32> {
        let mut out = Vec::new();
        let mut stack = self
            .children
            .get(&self.header_slot)
            .cloned()
            .unwrap_or_default();
        while let Some(p) = stack.pop() {
            match self.state.pages[p as usize].state {
                PageState::Interior => {
                    stack.extend(self.children.get(&p).cloned().unwrap_or_default())
                }
                _ => out.push(p),
            }
        }
        out.sort_unstable();
        out
    }

    /// The ids the statements of table page `p` name, in page order.
    pub(crate) fn page_ids(&self, p: u32) -> Result<Vec<u32>, Error> {
        let buf = self
            .table_pages
            .get(&p)
            .ok_or_else(|| corrupt("a table page is not resident"))?;
        let d = decode_page(buf, p < 2)
            .map_err(|_| corrupt("a resident table page does not decode"))?;
        let (_, mut r) = TableReader::open(&buf[..], d.content)?;
        let mut ids = Vec::new();
        while let Some(dec) = r.next_stmt()? {
            ids.push(dec.stmt.id);
        }
        Ok(ids)
    }

    /// Drops every statement of table page `p` physically.
    pub(crate) fn drop_page_statements(&mut self, p: u32) -> Result<(), Error> {
        for id in self.page_ids(p)? {
            self.state.drop_physically(id);
        }
        Ok(())
    }

    /// Derives the content statements stating the pending fragments in
    /// `ranges`: one per maximal run that states alike.
    fn derive(&mut self, ranges: &BTreeMap<Key, u32>) -> Result<Vec<Derived>, Error> {
        let threshold = self.opts.inline_threshold.clamp(1, MAX_INLINE as u32);
        let mut out: Vec<Derived> = Vec::new();
        for (&k, &end) in ranges {
            let id = kid(k);
            let end = end.min(self.state.size_of(id));
            if koff(k) >= end {
                continue;
            }
            let frags: Vec<(Key, Fragment)> = self
                .state
                .frags
                .range(k..key(id, end))
                .map(|(&a, &b)| (a, b))
                .collect();
            let mut run: Option<Derived> = None;
            for (i, &(fk, f)) in frags.iter().enumerate() {
                let fend = frags
                    .get(i + 1)
                    .map_or_else(|| self.state.frag_end(fk), |n| koff(n.0));
                let off = koff(fk);
                let len = fend - off;
                let Fragment::Pending(p) = f else {
                    out.extend(run.take());
                    continue;
                };
                let pe = self.state.pending[p as usize];
                let continues = |r: &Derived| r.stmt.offset + r.stmt.size == off;
                let start = |stmt: Stmt,
                             payload: Vec<u8>,
                             run: &mut Option<Derived>,
                             out: &mut Vec<Derived>| {
                    out.extend(run.take());
                    *run = Some(Derived {
                        stmt,
                        keys: vec![fk],
                        payload,
                        heat: pe.heat,
                    });
                };
                match pe.place {
                    Place::Zero => match &mut run {
                        Some(r) if r.stmt.kind == Kind::Zero && continues(r) => {
                            r.stmt.size += len;
                            r.keys.push(fk);
                            r.heat = r.heat.min(pe.heat);
                        }
                        _ => start(Stmt::zero(id, off, len), Vec::new(), &mut run, &mut out),
                    },
                    Place::Data(a) => match &mut run {
                        Some(r)
                            if r.stmt.kind == Kind::Ref
                                && continues(r)
                                && r.stmt.address + r.stmt.size as u64 == a
                                && split_address(r.stmt.address).0 == split_address(a).0 =>
                        {
                            r.stmt.size += len;
                            r.keys.push(fk);
                            r.heat = r.heat.min(pe.heat);
                        }
                        _ => start(
                            Stmt::reference(id, off, len, a),
                            Vec::new(),
                            &mut run,
                            &mut out,
                        ),
                    },
                    Place::Inline => {
                        let mut bytes = vec![0u8; len as usize];
                        self.read_fragment(f, 0, &mut bytes)?;
                        match &mut run {
                            Some(r)
                                if r.stmt.kind == Kind::Inline
                                    && continues(r)
                                    && r.stmt.size + len <= threshold =>
                            {
                                r.stmt.size += len;
                                r.keys.push(fk);
                                r.payload.extend_from_slice(&bytes);
                                r.heat = r.heat.min(pe.heat);
                            }
                            _ => {
                                if len as usize > MAX_INLINE {
                                    return Err(corrupt(
                                        "an inline run longer than the format allows",
                                    ));
                                }
                                start(Stmt::inline(id, off, len), bytes, &mut run, &mut out);
                            }
                        }
                    }
                    Place::Unplaced => {
                        return Err(corrupt("a chunk reached the cut without a data page"))
                    }
                }
            }
            out.extend(run.take());
        }
        Ok(out)
    }

    /// The statements the touched ids in `records` need for their size and
    /// existence (`impl/address-table-operations.md#what-the-cut-states-for-a-touched-id`).
    /// `content` holds every content statement of the flush.
    fn size_statements(&self, records: &IdMap<IdRecord>, content: &[&[Derived]]) -> Sizes {
        let mut reach: IdMap<u32> = IdMap::default();
        for d in content.iter().flat_map(|c| c.iter()) {
            if d.stmt.kind.is_content() {
                let e = reach.entry(d.stmt.id).or_default();
                *e = (*e).max(d.stmt.offset + d.stmt.size);
            }
        }
        let mut ids: Vec<u32> = records.keys().copied().collect();
        ids.sort_unstable();
        let mut sizes = Sizes {
            stmts: Vec::new(),
            retire: Vec::new(),
        };
        for id in ids {
            let rec = records[&id];
            let heat = if rec.written { 0 } else { self.heat_of(id) };
            let mk = |stmt| Derived {
                stmt,
                keys: Vec::new(),
                payload: Vec::new(),
                heat,
            };
            let Some(m) = self.state.allocs.get(&id) else {
                if (rec.freed || rec.replace_anchor) && self.state.mentions(id) > 0 {
                    sizes.stmts.push(mk(Stmt::tombstone(id)));
                }
                continue;
            };
            let size = m.size;
            let reaches = reach.get(&id).is_some_and(|&r| r >= size);
            let mut anchored = false;
            let mut grown = false;
            if rec.shrank {
                sizes.stmts.push(mk(Stmt::shrink(id, size)));
                anchored = true;
            } else if (rec.allocated || size > rec.size_before) && !reaches {
                sizes.stmts.push(mk(Stmt::grow(id, size)));
                grown = true;
            }
            if rec.replace_anchor && !anchored && m.anchor.is_some() {
                sizes.stmts.push(mk(Stmt::shrink(id, size)));
                anchored = true;
            }
            if rec.replace_grow && m.grow.is_some() {
                if !anchored && !grown && !reaches {
                    sizes.stmts.push(mk(Stmt::grow(id, size)));
                } else {
                    // Something else witnesses the size now; the old witness
                    // lives in a page the flush retires or restates, so it goes.
                    sizes.retire.push(id);
                }
            }
        }
        sizes
    }

    /// The room the last page of `lay` has left, in encoded bytes.
    fn last_room(&self, lay: &Layout, derived: &[Derived], kept: &[u32]) -> usize {
        match lay.leaves.last() {
            Some(last) => MAX_PAGE_CONTENT.saturating_sub(statements_len(last, derived) + 1),
            None => MAX_HEADER_CONTENT
                .saturating_sub(header_len(&lay.header, derived, kept, 0) + HEADER_RESERVE),
        }
    }

    /// Lays out and binds everything the flush states.
    pub(crate) fn cut(&mut self, dirty: &mut Dirty, out: &mut Output) -> Result<(), Error> {
        let mut kept = self.unlink_leaves(out)?;
        let mut derived = self.derive(&dirty.ranges)?;
        let sizes = self.size_statements(&dirty.records, &[&derived]);
        derived.extend(sizes.stmts);
        let mut retire = sizes.retire;
        derived.sort_by_key(|d| d.stmt.sort_key());
        let mut lay = layout(&derived, &kept);

        // The fillers: whole table victims that fit the last page's room,
        // then the rotating window. They state only ids the flush has not
        // touched otherwise, so their statements merge into the last page
        // without conflicting with anything laid out already.
        if self.opts.consolidate {
            let room = self.last_room(&lay, &derived, &kept);
            let mut fillers = Dirty::default();
            let victims = self.cut_fillers(room, dirty, &mut fillers)?;
            if !victims.is_empty() {
                kept.retain(|p| !victims.contains(p));
                out.dropped_tables.extend(victims);
            }
            let mut extra = self.derive(&fillers.ranges)?;
            let sizes = self.size_statements(&fillers.records, &[&derived, &extra]);
            extra.extend(sizes.stmts);
            retire.extend(sizes.retire);
            dirty.records.extend(fillers.records);
            let base = derived.len();
            derived.extend(extra);
            add_to_last(&mut lay, (base..derived.len()).collect(), &derived, &kept);
        }

        // Pages for the leaves, then the tree above them.
        let mut leaf_pages = Vec::with_capacity(lay.leaves.len());
        for _ in &lay.leaves {
            leaf_pages.push(self.take_table_page()?);
        }
        let mut all_leaves: Vec<u32> = kept
            .iter()
            .copied()
            .chain(leaf_pages.iter().copied())
            .collect();
        all_leaves.sort_unstable();
        let mut header_children = all_leaves.clone();
        let mut interiors: Vec<(u32, Vec<u32>)> = Vec::new();
        if all_leaves.len() > HEADER_CHILD_LIMIT {
            header_children.clear();
            for group in all_leaves.chunks(INTERIOR_FANOUT) {
                let p = self.take_table_page()?;
                interiors.push((p, group.to_vec()));
                header_children.push(p);
            }
            header_children.sort_unstable();
        }

        // Encode every page, noting where each statement landed.
        let new_slot = ((self.epoch + 1) % 2) as u32;
        let mut placed: Vec<(u32, u8, usize)> = vec![(0, 0, 0); derived.len()];
        let mut encode = |page: u32, children: &[u32], set: &[usize], base: usize| -> Vec<u8> {
            let mut w = TableWriter::new();
            for &c in children {
                w.child(c);
            }
            w.end_children();
            for &i in set {
                let pushed = w.push(&derived[i].stmt, &derived[i].payload);
                placed[i] = (page, pushed.framing, base + pushed.payload_pos);
            }
            w.into_content()
        };
        let mut contents: Vec<(u32, Vec<u8>, bool)> = Vec::new();
        for (set, &p) in lay.leaves.iter().zip(&leaf_pages) {
            contents.push((p, encode(p, &[], set, CONTENT_OFFSET), false));
        }
        for (p, kids) in &interiors {
            contents.push((*p, encode(*p, kids, &[], CONTENT_OFFSET), true));
        }
        let header_content = encode(
            new_slot,
            &header_children,
            &lay.header,
            HEADER_CONTENT_OFFSET,
        );
        if header_content.len() > MAX_HEADER_CONTENT {
            return Err(corrupt("the header overflowed its page"));
        }

        // The tree: parents and child lists.
        self.children.insert(new_slot, header_children.clone());
        for &p in &header_children {
            self.state.pages[p as usize].parent = new_slot;
        }
        for (p, kids) in &interiors {
            for &k in kids {
                self.state.pages[k as usize].parent = *p;
            }
            self.children.insert(*p, kids.clone());
            self.state.pages[*p as usize].coverage = TableWriter::children_len(kids) as u32;
        }
        if cfg!(debug_assertions) && self.state.pages[new_slot as usize].coverage != 0 {
            panic!(
                "the header slot being overwritten still holds live statements: {:?}",
                self.live_statements_in(new_slot)
            );
        }

        // Bind: every statement gets its slot, its fragments, and its pins.
        for id in retire {
            if let Some(g) = self.state.allocs.get_mut(&id).and_then(|m| m.grow.take()) {
                self.state.unpin(g);
            }
        }
        for (i, d) in derived.iter().enumerate() {
            let (page, framing, payload_pos) = placed[i];
            self.bind(d, page, framing, payload_pos);
        }
        out.tables.extend(contents);
        out.header_content = header_content;
        Ok(())
    }

    fn take_table_page(&mut self) -> Result<u32, Error> {
        take_page(
            &mut self.ready,
            &mut self.state,
            &mut self.file_pages,
            &mut *self.storage,
            &mut self.stats,
        )
    }

    /// A description of the live statements in page `p`, for diagnostics.
    pub(crate) fn live_statements_in(&self, p: u32) -> Vec<String> {
        (1..self.state.slab.pins.len())
            .filter(|&s| self.state.slab.pins[s] > 0 && self.state.slab.page_or_next[s] == p)
            .map(|s| {
                let id = self.state.slab.ids[s];
                format!(
                    "{:?} of id {id} with {} pins: {:?}",
                    self.state.slab.kinds[s],
                    self.state.slab.pins[s],
                    self.state.allocs.get(&id)
                )
            })
            .collect()
    }

    /// Gives `d` its slab slot, fragments, pins, and coverage.
    fn bind(&mut self, d: &Derived, page: u32, framing: u8, payload_pos: usize) {
        let id = d.stmt.id;
        let s = self.state.slab.alloc(page, framing, id, d.stmt.kind);
        self.state.cover(page, framing as u32);
        if let Some(m) = self.state.allocs.get_mut(&id) {
            m.mentions += 1;
            m.statement_bytes += framing as u32;
        } else if let Some(r) = self.state.recyclable.get_mut(&id) {
            r.mentions += 1;
        }
        match d.stmt.kind {
            Kind::Ref | Kind::Zero | Kind::Inline => {
                let first = d.keys[0];
                let f = match d.stmt.kind {
                    Kind::Ref => {
                        let (dp, off) = split_address(d.stmt.address);
                        Fragment::Bytes {
                            page: dp,
                            offset: off,
                            stmt: s,
                        }
                    }
                    Kind::Inline => {
                        self.state.cover(page, d.stmt.size);
                        Fragment::Bytes {
                            page,
                            offset: payload_pos as u16,
                            stmt: s,
                        }
                    }
                    _ => Fragment::ZeroExplicitly { stmt: s },
                };
                for &k in &d.keys[1..] {
                    self.state.frags.remove(&k);
                }
                self.state.frags.insert(first, f);
                self.state.pin(s);
                self.state.allocs.get_mut(&id).unwrap().fragment_count -= d.keys.len() as u32 - 1;
                if d.stmt.kind == Kind::Ref {
                    let (dp, _) = split_address(d.stmt.address);
                    self.state
                        .note_reverse(dp, id, d.stmt.offset, d.stmt.offset + d.stmt.size);
                }
            }
            Kind::Shrink => self.state.set_anchor(id, s, d.stmt.offset),
            Kind::Grow => self.state.set_grow_witness(id, s, d.stmt.offset),
            Kind::Tombstone => {
                self.state.pin(s);
                let r = self.state.recyclable.get_mut(&id).unwrap();
                if let Some(old) = r.tombstone.replace(s) {
                    self.state.unpin(old);
                }
            }
        }
    }
}
