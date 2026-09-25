//! The cut (`impl/consolidation.md#one-dirty-set-and-why-statements-are-derived-last`):
//! derive every statement from the dirty set, lay them out — the hottest in
//! the header, the rest in leaves in key order — and bind them.

use crate::consts::*;
use crate::error::{corrupt, Error};
use crate::flush::Output;
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

impl Inner {
    /// Unlinks table pages that nothing in them keeps alive. Their
    /// statements are dead, but still physically present until now.
    fn unlink_empty(&mut self, out: &mut Output) -> Result<Vec<u32>, Error> {
        let leaves = self.all_leaves();
        let mut kept = Vec::with_capacity(leaves.len());
        for p in leaves {
            let info = self.state.pages[p as usize];
            if info.state == PageState::Table && info.coverage == 0 {
                if !self.flush_rewritten.contains(&p) {
                    self.drop_page_statements(p)?;
                }
                out.dropped_tables.push(p);
            } else {
                kept.push(p);
            }
        }
        // The interior layer is rebuilt by every cut that needs one.
        let interiors: Vec<u32> = self
            .children
            .get(&self.header_slot)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|&c| self.state.pages[c as usize].state == PageState::Interior)
            .collect();
        for p in interiors {
            out.dropped_tables.push(p);
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

    /// Drops every statement of table page `p` physically.
    pub(crate) fn drop_page_statements(&mut self, p: u32) -> Result<(), Error> {
        let buf = self
            .table_pages
            .get(&p)
            .ok_or_else(|| corrupt("a table page is not resident"))?
            .clone();
        let d = decode_page(&buf, p < 2)
            .map_err(|_| corrupt("a resident table page does not decode"))?;
        let (_, mut r) = TableReader::open(&buf[..], d.content)?;
        while let Some(dec) = r.next_stmt()? {
            self.state.drop_physically(dec.stmt.id);
        }
        Ok(())
    }

    /// Derives the content statements stating the pending fragments in
    /// `dirty`: one per maximal run that states alike.
    pub(crate) fn derive(
        &mut self,
        dirty: &Dirty,
        from: Option<&std::collections::BTreeMap<Key, u32>>,
    ) -> Result<Vec<Derived>, Error> {
        let threshold = self.opts.inline_threshold.max(1).min(MAX_INLINE as u32);
        let mut out: Vec<Derived> = Vec::new();
        let ranges = from.unwrap_or(&dirty.ranges);
        for (&k, &end) in ranges {
            let id = kid(k);
            let size = self.state.size_of(id);
            let end = end.min(size);
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
                    if let Some(r) = run.take() {
                        out.push(r);
                    }
                    continue;
                };
                let pe = self.state.pending[p as usize];
                let continues = |r: &Derived| r.stmt.offset + r.stmt.size == off;
                match pe.place {
                    Place::Zero => match &mut run {
                        Some(r) if r.stmt.kind == Kind::Zero && continues(r) => {
                            r.stmt.size += len;
                            r.keys.push(fk);
                            r.heat = r.heat.min(pe.heat);
                        }
                        _ => {
                            if let Some(r) = run.take() {
                                out.push(r);
                            }
                            run = Some(Derived {
                                stmt: Stmt::zero(id, off, len),
                                keys: vec![fk],
                                payload: vec![],
                                heat: pe.heat,
                            });
                        }
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
                        _ => {
                            if let Some(r) = run.take() {
                                out.push(r);
                            }
                            run = Some(Derived {
                                stmt: Stmt::reference(id, off, len, a),
                                keys: vec![fk],
                                payload: vec![],
                                heat: pe.heat,
                            });
                        }
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
                                if let Some(r) = run.take() {
                                    out.push(r);
                                }
                                if len as usize > MAX_INLINE {
                                    return Err(corrupt(
                                        "an inline run longer than the format allows",
                                    ));
                                }
                                run = Some(Derived {
                                    stmt: Stmt::inline(id, off, len),
                                    keys: vec![fk],
                                    payload: bytes,
                                    heat: pe.heat,
                                });
                            }
                        }
                    }
                    Place::Unplaced => {
                        return Err(corrupt("a chunk reached the cut without a data page"))
                    }
                }
            }
            if let Some(r) = run.take() {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// The statements a touched id's size and existence call for
    /// (`impl/address-table-operations.md#what-the-cut-states-for-a-touched-id`).
    fn size_statements(&mut self, dirty: &Dirty, derived: &[Derived]) -> Vec<Derived> {
        let mut reach: crate::hash::IdMap<u32> = Default::default();
        for d in derived {
            let e = reach.entry(d.stmt.id).or_default();
            *e = (*e).max(d.stmt.offset + d.stmt.size);
        }
        let mut ids: Vec<u32> = dirty.records.keys().copied().collect();
        ids.sort_unstable();
        let mut out = Vec::new();
        let mut retire = Vec::new();
        for id in ids {
            let rec = dirty.records[&id];
            let heat = if rec.written { 0 } else { self.heat_of(id) };
            let mk = |stmt| Derived {
                stmt,
                keys: vec![],
                payload: vec![],
                heat,
            };
            let Some(m) = self.state.allocs.get(&id) else {
                if (rec.freed || rec.replace_anchor) && self.state.mentions(id) > 0 {
                    out.push(mk(Stmt::tombstone(id)));
                }
                continue;
            };
            let size = m.size;
            let reaches = reach.get(&id).is_some_and(|&r| r >= size);
            let mut anchored = false;
            let mut grown = false;
            if rec.shrank {
                out.push(mk(Stmt::shrink(id, size)));
                anchored = true;
            } else if (rec.allocated || size > rec.size_before) && !reaches {
                out.push(mk(Stmt::grow(id, size)));
                grown = true;
            }
            if rec.replace_anchor && !anchored && m.anchor.is_some() {
                out.push(mk(Stmt::shrink(id, size)));
                anchored = true;
            }
            if rec.replace_grow && m.grow.is_some() {
                if !anchored && !grown && !reaches {
                    out.push(mk(Stmt::grow(id, size)));
                } else {
                    // Something else witnesses the size now; the old witness
                    // lives in a page this flush retires, so it goes.
                    retire.push(id);
                }
            }
        }
        for id in retire {
            if let Some(g) = self.state.allocs.get_mut(&id).and_then(|m| m.grow.take()) {
                self.state.unpin(g);
            }
        }
        out
    }

    /// Lays out and binds everything the flush states.
    pub(crate) fn cut(&mut self, dirty: &mut Dirty, out: &mut Output) -> Result<(), Error> {
        let kept = self.unlink_empty(out)?;
        let mut derived = self.derive(dirty, None)?;
        let sizes = self.size_statements(dirty, &derived);
        derived.extend(sizes);
        derived.sort_by_key(|d| d.stmt.sort_key());

        // Header first: everything, if it fits.
        let new_slot = ((self.epoch + 1) % 2) as u32;
        let exact = |set: &[usize], derived: &[Derived], children: &[u32]| -> usize {
            let mut w = TableWriter::new();
            for &c in children {
                w.child(c);
            }
            w.end_children();
            for &i in set {
                w.push(&derived[i].stmt, &derived[i].payload);
            }
            w.len()
        };
        let all: Vec<usize> = (0..derived.len()).collect();
        let mut header_set: Vec<usize>;
        let mut leaf_sets: Vec<Vec<usize>> = Vec::new();
        if kept.len() <= HEADER_CHILD_LIMIT && exact(&all, &derived, &kept) <= MAX_HEADER_CONTENT {
            header_set = all;
        } else {
            // The hottest statements stay in the header; the rest go to leaves
            // in key order.
            let total: usize = derived
                .iter()
                .map(|d| TableWriter::standalone_len(&d.stmt))
                .sum();
            let mut reserve = 64usize;
            loop {
                let est_leaves = total / MAX_PAGE_CONTENT + 2;
                let child_budget = if kept.len() + est_leaves > HEADER_CHILD_LIMIT {
                    5 * ((kept.len() + est_leaves) / 800 + 2)
                } else {
                    TableWriter::children_len(&kept) + 5 * est_leaves
                };
                let cap = MAX_HEADER_CONTENT.saturating_sub(child_budget + reserve);
                let mut order: Vec<usize> = (0..derived.len()).collect();
                order.sort_by_key(|&i| (derived[i].heat, derived[i].stmt.sort_key()));
                let mut used = 0usize;
                let mut chosen = vec![false; derived.len()];
                for &i in &order {
                    let l = TableWriter::standalone_len(&derived[i].stmt);
                    if used + l <= cap {
                        used += l;
                        chosen[i] = true;
                    }
                }
                header_set = (0..derived.len()).filter(|&i| chosen[i]).collect();
                let rest: Vec<usize> = (0..derived.len()).filter(|&i| !chosen[i]).collect();
                leaf_sets.clear();
                let mut w = TableWriter::new();
                w.end_children();
                let mut cur: Vec<usize> = Vec::new();
                for i in rest {
                    let l = w.statement_len(&derived[i].stmt);
                    if w.len() + l > MAX_PAGE_CONTENT && !cur.is_empty() {
                        leaf_sets.push(std::mem::take(&mut cur));
                        w = TableWriter::new();
                        w.end_children();
                    }
                    w.push(&derived[i].stmt, &derived[i].payload);
                    cur.push(i);
                }
                if !cur.is_empty() {
                    leaf_sets.push(cur);
                }
                let leaves = kept.len() + leaf_sets.len();
                let children_len = if leaves > HEADER_CHILD_LIMIT {
                    5 * (leaves / 800 + 2)
                } else {
                    TableWriter::children_len(&kept) + 5 * leaf_sets.len()
                };
                if children_len + exact(&header_set, &derived, &[]) <= MAX_HEADER_CONTENT {
                    break;
                }
                reserve += 256;
            }
        }

        // Pages for the leaves, then the tree above them.
        let mut leaf_pages = Vec::with_capacity(leaf_sets.len());
        for _ in &leaf_sets {
            leaf_pages.push(take_page(
                &mut self.ready,
                &mut self.state,
                &mut self.file_pages,
                &mut *self.storage,
                &mut self.stats,
            )?);
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
            for group in all_leaves.chunks(800) {
                let p = take_page(
                    &mut self.ready,
                    &mut self.state,
                    &mut self.file_pages,
                    &mut *self.storage,
                    &mut self.stats,
                )?;
                interiors.push((p, group.to_vec()));
                header_children.push(p);
            }
            header_children.sort_unstable();
        }

        // Encode every page, noting where each statement landed.
        let mut placed: Vec<(u32, u8, usize)> = vec![(0, 0, 0); derived.len()];
        let mut encode = |page: u32,
                          children: &[u32],
                          set: &[usize],
                          base: usize,
                          derived: &[Derived]|
         -> Vec<u8> {
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
        for (set, &p) in leaf_sets.iter().zip(&leaf_pages) {
            contents.push((p, encode(p, &[], set, CONTENT_OFFSET, &derived), false));
        }
        for (p, kids) in &interiors {
            contents.push((*p, encode(*p, kids, &[], CONTENT_OFFSET, &derived), true));
        }
        let header_content = encode(
            new_slot,
            &header_children,
            &header_set,
            HEADER_CONTENT_OFFSET,
            &derived,
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
            let live: Vec<String> = (1..self.state.slab.pins.len())
                .filter(|&s| {
                    self.state.slab.pins[s] > 0 && self.state.slab.page_or_next[s] == new_slot
                })
                .map(|s| {
                    let id = self.state.slab.ids[s];
                    format!(
                        "{:?} id {} pins {} meta {:?} rec {:?}",
                        self.state.slab.kinds[s],
                        id,
                        self.state.slab.pins[s],
                        self.state.allocs.get(&id),
                        dirty.records.get(&id)
                    )
                })
                .collect();
            panic!("the header slot being overwritten still holds live statements: {live:#?}");
        }

        // Bind: every statement gets its slot, its fragments, and its pins.
        for (i, d) in derived.iter().enumerate() {
            let (page, framing, payload_pos) = placed[i];
            self.bind(d, page, framing, payload_pos);
        }
        out.tables.extend(contents);
        out.header_content = header_content;
        Ok(())
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
