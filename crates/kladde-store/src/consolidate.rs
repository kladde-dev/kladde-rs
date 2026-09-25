//! Consolidation (`impl/consolidation.md`): evacuating sparse data pages,
//! in the room of pages the flush writes anyway and on pages of their own,
//! and the controller that paces it.

use crate::consts::MAX_PAGE_CONTENT;
use crate::defrag::{Candidate, Weighed};
use crate::error::Error;
use crate::flush::{DataPage, Output};
use crate::hash::{IdMap, IdSet};
use crate::state::*;
use crate::stats::Stats;
use crate::store::Inner;

/// A page's content capacity.
const C: u32 = MAX_PAGE_CONTENT as u32;

/// How much more encoding restating a table page's live content takes than
/// its coverage: fragments that shadowing split need a statement each, and
/// statements lose the dense delta encoding of their old neighbours.
const RESTATE: f64 = 1.25;

/// What consolidation carries from flush to flush.
#[derive(Debug, Default)]
pub struct ConsState {
    /// The rotating window's cursor.
    pub cursor: Key,
    /// The per-flush budget in pages, as the controller moved it.
    pub budget: f64,
    /// Where sampling starts within a bucket; rotates so that no prefix of
    /// a bucket is examined forever.
    pub rot: usize,
    /// Victims this flush found unusable, so that it does not pick them again.
    pub skip: IdSet,
    /// Description defragmentation's candidates from the latest walk, for
    /// the next flush.
    pub candidates: Vec<Candidate>,
    /// This flush's candidates beyond its share, for free filling.
    pub spare: Vec<Candidate>,
}

/// A live fragment in a data page being evacuated.
#[derive(Clone, Copy, Debug)]
struct Survivor {
    id: u32,
    start: u32,
    end: u32,
    /// The statement stating it, or `None` if the flush took it already.
    stmt: Option<StmtRef>,
    /// Where its bytes are.
    addr: u64,
}

impl Survivor {
    fn len(&self) -> u32 {
        self.end - self.start
    }
}

/// What one budgeted data page would hold, and what it would reclaim.
#[derive(Debug)]
struct Offer {
    /// Victims evacuated whole into the page.
    whole: Vec<u32>,
    /// One more victim whose survivors fill the page and lead the next one.
    cut: Option<u32>,
    reclaimed: f64,
    written: f64,
    /// How full the page would close, as a fraction of its capacity.
    fill: f64,
}

impl Inner {
    pub(crate) fn seed_after_load(&mut self) {
        self.cons.budget = self.opts.budget_pages as f64;
        // Content ages fall back to the youngest page holding a fragment.
        let mut youngest: IdMap<u64> = Default::default();
        for (&k, &f) in &self.state.frags {
            let page = match f {
                Fragment::Bytes { page, .. } => page,
                Fragment::ZeroExplicitly { stmt } => self.state.slab.page(stmt),
                _ => continue,
            };
            let e = self.state.pages[page as usize].epoch;
            let y = youngest.entry(kid(k)).or_default();
            *y = (*y).max(e);
        }
        for (&id, m) in self.state.allocs.iter_mut() {
            m.last_written = youngest.get(&id).copied().unwrap_or(self.epoch);
        }
    }

    /// Resets what consolidation keeps for one flush only.
    pub(crate) fn begin_consolidation(&mut self) {
        self.cons.skip.clear();
    }

    // ------------------------------------------------------------ victims

    /// LFS's cost-benefit score: sparse and long untouched first.
    fn score(&self, p: u32) -> f64 {
        let info = &self.state.pages[p as usize];
        let u = info.coverage as f64 / C as f64;
        let age = self.state.flush_epoch.saturating_sub(info.epoch) as f64;
        (1.0 - u) * age / (1.0 + u)
    }

    /// Up to `sample` pages of a bucket, from a rotating start.
    fn sample(&mut self, data: bool, bucket: usize) -> Vec<u32> {
        let k = self.opts.sample.max(1);
        let buckets = if data {
            &self.state.data_buckets
        } else {
            &self.state.table_buckets
        };
        let list = &buckets.lists[bucket];
        let n = list.len();
        if n == 0 {
            return Vec::new();
        }
        let start = self.cons.rot % n;
        self.cons.rot = self.cons.rot.wrapping_add(1);
        (0..k.min(n)).map(|i| list[(start + i) % n]).collect()
    }

    /// The best-scoring of the pages sampled from buckets `from..` that pass
    /// `ok`, stopping at the first bucket that yields one, or at a bucket
    /// whose pages all hold more than `max` bytes.
    fn pick_victim(
        &mut self,
        data: bool,
        from: usize,
        max: u32,
        ok: &dyn Fn(&Inner, u32) -> bool,
    ) -> Option<u32> {
        for b in from..BUCKETS {
            let lower = (b as u64 * (C as u64 + 1)).div_ceil(BUCKETS as u64) as u32;
            if lower > max {
                return None;
            }
            let best = self
                .sample(data, b)
                .into_iter()
                .filter(|&p| {
                    let cov = self.state.pages[p as usize].coverage;
                    cov > 0 && cov <= max && !self.cons.skip.contains(&p) && ok(self, p)
                })
                .max_by(|&x, &y| self.score(x).total_cmp(&self.score(y)));
            if best.is_some() {
                return best;
            }
        }
        None
    }

    /// The live fragments pointing into data page `v`, in key order. Narrows
    /// the reverse index's windows to what it finds.
    fn survivors(&mut self, v: u32) -> Vec<Survivor> {
        let entries = self.state.reverse.remove(&v).unwrap_or_default();
        let mut out = Vec::new();
        let mut kept = Vec::new();
        for (id, lo, hi) in entries {
            let size = self.state.size_of(id);
            let hi = hi.min(size);
            if lo >= hi {
                continue;
            }
            let first = match self.state.frags.range(..=key(id, lo)).next_back() {
                Some((&k, _)) if kid(k) == id => k,
                _ => key(id, lo),
            };
            let mut span: Option<(u32, u32)> = None;
            let mut it = self.state.frags.range(first..key(id, hi)).peekable();
            while let Some((&k, &f)) = it.next() {
                let addr = match f {
                    Fragment::Bytes { page, offset, .. } if page == v => address(page, offset),
                    Fragment::Pending(p) => match self.state.pending[p as usize].place {
                        Place::Data(a) if split_address(a).0 == v => a,
                        _ => continue,
                    },
                    _ => continue,
                };
                let end = match it.peek() {
                    Some((&n, _)) => koff(n),
                    None => self.state.frag_end(k),
                };
                out.push(Survivor {
                    id,
                    start: koff(k),
                    end,
                    stmt: f.stmt(),
                    addr,
                });
                span = Some(span.map_or((koff(k), end), |(a, b)| (a.min(koff(k)), b.max(end))));
            }
            if let Some((a, b)) = span {
                kept.push((id, a, b));
            }
        }
        if !kept.is_empty() {
            self.state.reverse.insert(v, kept);
        }
        out.sort_by_key(|s| key(s.id, s.start));
        out
    }

    /// The survivors of victim `v`, if they account for all of its coverage;
    /// otherwise the reverse index lost a referrer, and `v` is skipped.
    fn all_survivors(&mut self, v: u32) -> Option<Vec<Survivor>> {
        let surv = self.survivors(v);
        let total: u64 = surv.iter().map(|s| s.len() as u64).sum();
        if total != self.state.pages[v as usize].coverage as u64 {
            debug_assert!(false, "the reverse index lost a referrer of page {v}");
            self.cons.skip.insert(v);
            return None;
        }
        Some(surv)
    }

    /// Takes `s` for the flush, as a chunk whose bytes stay where they are
    /// until something places them.
    fn take_survivor(&mut self, s: &Survivor, dirty: &mut Dirty) {
        let heat = self.heat_of(s.id);
        let p = Pending {
            origin: Origin::File(s.addr),
            place: Place::Unplaced,
            heat,
            rewrite: false,
        };
        self.state.take(s.id, s.start, s.end, p, dirty);
    }

    /// Relocates `survivors` into `dp`, which has room for them.
    fn relocate(
        &mut self,
        survivors: &[Survivor],
        dp: &mut DataPage,
        dirty: &mut Dirty,
    ) -> Result<(), Error> {
        for s in survivors {
            self.take_survivor(s, dirty);
            self.place(dp, key(s.id, s.start), s.len())?;
        }
        Ok(())
    }

    /// Evacuates all of `v` into `dp`, if it fits.
    fn evacuate_into(
        &mut self,
        v: u32,
        dp: &mut DataPage,
        dirty: &mut Dirty,
    ) -> Result<bool, Error> {
        let Some(surv) = self.all_survivors(v) else {
            return Ok(false);
        };
        let total: u32 = surv.iter().map(Survivor::len).sum();
        if total as usize > dp.room() {
            self.cons.skip.insert(v);
            return Ok(false);
        }
        self.relocate(&surv, dp, dirty)?;
        self.stats.evacuated_pages += 1;
        self.stats.evacuated_bytes += total as u64;
        Ok(true)
    }

    /// Evacuates `v` into what room `dp` has left, cutting the survivor that
    /// crosses the boundary; returns the rest, taken but not placed.
    fn evacuate_across(
        &mut self,
        v: u32,
        dp: &mut DataPage,
        dirty: &mut Dirty,
    ) -> Result<Vec<Survivor>, Error> {
        let Some(surv) = self.all_survivors(v) else {
            return Ok(Vec::new());
        };
        let total: u64 = surv.iter().map(|s| s.len() as u64).sum();
        let mut rest = Vec::new();
        for s in surv {
            self.take_survivor(&s, dirty);
            let room = dp.room() as u32;
            if !rest.is_empty() || room == 0 {
                rest.push(s);
            } else if s.len() <= room {
                self.place(dp, key(s.id, s.start), s.len())?;
            } else {
                self.state.split(s.id, s.start + room);
                self.place(dp, key(s.id, s.start), room)?;
                rest.push(Survivor {
                    start: s.start + room,
                    ..s
                });
            }
        }
        self.stats.evacuated_pages += 1;
        self.stats.evacuated_bytes += total;
        Ok(rest)
    }

    // ------------------------------------------------------------ free filling

    /// Fills the room of `open`, a page the flush writes anyway, with what
    /// costs nothing to move there: whole victims that fit, then as many of
    /// one victim's statements as fit, a whole statement's survivors at a
    /// time.
    pub(crate) fn free_fill(
        &mut self,
        open: &mut DataPage,
        dirty: &mut Dirty,
    ) -> Result<(), Error> {
        while let Some(b) = self.state.data_buckets.lowest_non_empty() {
            let room = open.room() as u32;
            if room == 0 {
                return Ok(());
            }
            let Some(v) = self.pick_victim(true, b, room, &|_, _| true) else {
                break;
            };
            if self.evacuate_into(v, open, dirty)? {
                self.stats.free_filled_pages += 1;
            }
        }
        self.fill_with_candidates(open, dirty)?;
        match self.state.data_buckets.lowest_non_empty() {
            Some(b) if open.room() > 0 => self.fill_with_part(b, open, dirty),
            _ => Ok(()),
        }
    }

    /// Moves the survivors of as many of one victim's statements as fit into
    /// `open`, each statement's all or none; the victim is the best-scoring
    /// sample of bucket `b`, the sparsest. It stays a victim, only a sparser
    /// one.
    fn fill_with_part(
        &mut self,
        b: usize,
        open: &mut DataPage,
        dirty: &mut Dirty,
    ) -> Result<(), Error> {
        let Some(v) = self.pick_victim(true, b, C, &|_, _| true) else {
            return Ok(());
        };
        let Some(surv) = self.all_survivors(v) else {
            return Ok(());
        };
        let mut groups: Vec<Vec<Survivor>> = Vec::new();
        let mut group_of: IdMap<usize> = IdMap::default();
        for s in surv {
            match s.stmt {
                Some(st) => {
                    let g = *group_of.entry(st.idx() as u32).or_insert_with(|| {
                        groups.push(Vec::new());
                        groups.len() - 1
                    });
                    groups[g].push(s);
                }
                None => groups.push(vec![s]),
            }
        }
        for g in groups {
            let len: u32 = g.iter().map(Survivor::len).sum();
            if len as usize <= open.room() {
                self.relocate(&g, open, dirty)?;
                self.stats.evacuated_bytes += len as u64;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ the budget

    /// What one budgeted page could hold after `carry` bytes that must lead
    /// it: whole victims within the churn floor, best score first, then, if
    /// `may_cut` and the page would close more than θ empty, one more victim
    /// cut at the page boundary.
    fn data_offer(&mut self, carry: u32, may_cut: bool) -> Option<Offer> {
        let max = (C as f64 / (1.0 + self.opts.churn_floor)) as u32;
        let mut room = C - carry;
        let mut offer = Offer {
            whole: Vec::new(),
            cut: None,
            reclaimed: 0.0,
            written: 0.0,
            fill: 0.0,
        };
        let mut chosen = IdSet::default();
        while let Some(b) = self.state.data_buckets.lowest_non_empty() {
            let not_chosen = |_: &Inner, p: u32| !chosen.contains(&p);
            let Some(v) = self.pick_victim(true, b, room.min(max), &not_chosen) else {
                break;
            };
            let cov = self.state.pages[v as usize].coverage;
            chosen.insert(v);
            offer.whole.push(v);
            offer.reclaimed += (C - cov) as f64;
            offer.written += cov as f64;
            room -= cov;
        }
        if may_cut && room as f64 > self.opts.theta * C as f64 && room < max {
            let from = Buckets::bucket_of(room + 1) as usize;
            let too_big = |i: &Inner, p: u32| {
                !chosen.contains(&p) && i.state.pages[p as usize].coverage > room
            };
            if let Some(v) = self.pick_victim(true, from, max, &too_big) {
                let cov = self.state.pages[v as usize].coverage;
                offer.cut = Some(v);
                offer.reclaimed += (C - cov) as f64;
                offer.written += cov as f64;
                room = 0;
            }
        }
        if offer.whole.is_empty() && offer.cut.is_none() {
            return None;
        }
        offer.fill = (C - room) as f64 / C as f64;
        Some(offer)
    }

    /// Spends the flush's budget: pages opened only to reclaim, one at a
    /// time, each only if its offer passes the churn floor and fills it. A
    /// survivor cut at one page's end leads the next, which is opened
    /// whatever its own offer, so the budget's last page cuts nothing.
    pub(crate) fn budget_loop(&mut self, dirty: &mut Dirty, out: &mut Output) -> Result<(), Error> {
        let pages = self.cons.budget.round().max(0.0) as u32;
        let (lambda, theta) = (self.opts.churn_floor, self.opts.theta);
        let good = |o: &Offer| o.reclaimed >= lambda * o.written && o.fill >= 1.0 - theta;
        let ratio = |o: &Offer| o.reclaimed / o.written.max(1.0);
        let mut carry: Vec<Survivor> = Vec::new();
        for i in 0..pages {
            let carry_len: u32 = carry.iter().map(Survivor::len).sum();
            let data = self.data_offer(carry_len, i + 1 < pages);
            // A carried tail opens a data page whatever the offers.
            let offer = if !carry.is_empty() {
                data
            } else {
                let data = data.filter(good);
                let table = self.table_offer().filter(good);
                match (data, table) {
                    (Some(d), Some(t)) if ratio(&t) > ratio(&d) => {
                        self.rewrite_table_victims(&t.whole, dirty)?;
                        self.stats.budget_pages += 1;
                        continue;
                    }
                    (None, Some(t)) => {
                        self.rewrite_table_victims(&t.whole, dirty)?;
                        self.stats.budget_pages += 1;
                        continue;
                    }
                    (Some(d), _) => Some(d),
                    (None, None) => break,
                }
            };
            let mut dp = self.new_data_page()?;
            for s in std::mem::take(&mut carry) {
                self.place(&mut dp, key(s.id, s.start), s.len())?;
            }
            if let Some(o) = offer {
                for v in o.whole {
                    self.evacuate_into(v, &mut dp, dirty)?;
                }
                if let Some(v) = o.cut {
                    carry = self.evacuate_across(v, &mut dp, dirty)?;
                }
            }
            self.stats.budget_pages += 1;
            out.data.push(dp);
        }
        debug_assert!(carry.is_empty(), "the budget's last page cut a survivor");
        Ok(())
    }

    /// What rewriting table victims onto one budgeted leaf would hold and
    /// reclaim, without taking anything: whole victims within the churn
    /// floor, best score first, their restatements estimated from coverage.
    fn table_offer(&mut self) -> Option<Offer> {
        let max = (C as f64 / (1.0 + self.opts.churn_floor)) as u32;
        let mut room = C as f64;
        let mut offer = Offer {
            whole: Vec::new(),
            cut: None,
            reclaimed: 0.0,
            written: 0.0,
            fill: 0.0,
        };
        let mut chosen = IdSet::default();
        while let Some(b) = self.state.table_buckets.lowest_non_empty() {
            let fresh = |i: &Inner, p: u32| !chosen.contains(&p) && !i.flush_rewritten.contains(&p);
            let limit = ((room / RESTATE) as u32).min(max);
            let Some(v) = self.pick_victim(false, b, limit, &fresh) else {
                break;
            };
            let cov = self.state.pages[v as usize].coverage;
            chosen.insert(v);
            offer.whole.push(v);
            offer.reclaimed += (C - cov) as f64;
            offer.written += cov as f64 * RESTATE;
            room -= cov as f64 * RESTATE;
        }
        if offer.whole.is_empty() {
            return None;
        }
        offer.fill = offer.written / C as f64;
        Some(offer)
    }

    /// Rewrites table pages: their live content is taken in place, for the
    /// cut to state again, and they are unlinked.
    fn rewrite_table_victims(&mut self, victims: &[u32], dirty: &mut Dirty) -> Result<(), Error> {
        for &v in victims {
            self.rewrite_table_page(v, dirty)?;
            self.flush_rewritten.insert(v);
            self.stats.table_rewrites += 1;
        }
        Ok(())
    }

    // ------------------------------------------------------------ at the cut

    /// Whether the cut's fillers may rewrite table page `p`: its statements
    /// must name only ids the flush has not touched, so that what they
    /// restate cannot conflict with the statements laid out already.
    fn filler_eligible(&self, p: u32, main: &Dirty) -> bool {
        !self.flush_rewritten.contains(&p)
            && self
                .page_ids(p)
                .is_ok_and(|ids| ids.iter().all(|id| !main.records.contains_key(id)))
    }

    /// The fillers of the cut's last page: whole table victims that fit its
    /// `room`, then the rotating window. Takes into `fillers`, and returns
    /// the victims.
    pub(crate) fn cut_fillers(
        &mut self,
        room: usize,
        main: &Dirty,
        fillers: &mut Dirty,
    ) -> Result<Vec<u32>, Error> {
        let mut room = room as f64;
        let mut victims = Vec::new();
        while let Some(b) = self.state.table_buckets.lowest_non_empty() {
            let limit = (room / RESTATE) as u32;
            let eligible = |i: &Inner, p: u32| i.filler_eligible(p, main);
            let Some(v) = self.pick_victim(false, b, limit, &eligible) else {
                break;
            };
            room -= self.state.pages[v as usize].coverage as f64 * RESTATE;
            self.rewrite_table_victims(&[v], fillers)?;
            victims.push(v);
        }
        self.window(room.max(0.0) as usize, main, fillers);
        Ok(victims)
    }

    /// One step of the rotating window (`impl/consolidation.md#the-rotating-window`):
    /// restates live fragments from the cursor on, in place, until `room`
    /// bytes of encoding are used, and moves the cursor past `walk`
    /// fragments either way.
    fn window(&mut self, room: usize, main: &Dirty, fillers: &mut Dirty) {
        let n = self.opts.walk.min(self.state.frags.len());
        if n == 0 {
            return;
        }
        let start = self.cons.cursor;
        let items: Vec<(Key, Fragment)> = self
            .state
            .frags
            .range(start..)
            .chain(self.state.frags.range(..start))
            .take(n)
            .map(|(&k, &f)| (k, f))
            .collect();
        let zeros = [0u8; crate::consts::MAX_INLINE];
        let mut w = crate::statement::TableWriter::new();
        w.end_children();
        let mut prev: Option<Key> = None;
        let mut full = false;
        let mut take: Vec<(u32, u32, u32, Fragment)> = Vec::new();
        // Description defragmentation rides along: Kadane's search over each
        // id's walked fragments, reset at every id boundary.
        let mut found = Vec::new();
        let mut run: Vec<Weighed> = Vec::new();
        for &(k, f) in &items {
            let (id, off, end) = (kid(k), koff(k), self.state.frag_end(k));
            let wrapped = prev.is_some_and(|p| k < p);
            if wrapped || prev.is_some_and(|p| kid(p) != id) {
                self.candidates_of(prev.map_or(0, kid), &run, &mut found);
                run.clear();
            }
            if wrapped {
                // The encoding starts afresh.
                w = crate::statement::TableWriter::new();
                w.end_children();
            }
            prev = Some(k);
            run.push(self.weigh(off, end, f));
            let Some(s) = f.stmt() else { continue };
            if full {
                continue;
            }
            let len = end - off;
            let inline = self.state.slab.kinds[s.idx()] == crate::statement::Kind::Inline;
            let stmt = match f {
                Fragment::Bytes { .. } if inline => crate::statement::Stmt::inline(id, off, len),
                Fragment::Bytes { page, offset, .. } => {
                    crate::statement::Stmt::reference(id, off, len, address(page, offset))
                }
                _ => crate::statement::Stmt::zero(id, off, len),
            };
            if w.len() + w.statement_len(&stmt) > room {
                full = true;
                continue;
            }
            w.push(&stmt, &zeros[..if inline { len as usize } else { 0 }]);
            take.push((id, off, end, f));
        }
        self.candidates_of(prev.map_or(0, kid), &run, &mut found);
        self.cons.candidates = found;
        self.cons.cursor = items.last().map_or(start, |&(k, _)| k.wrapping_add(1));
        // The pages the window restates from, read before taking releases the
        // statements that name them.
        let touched: IdSet = take
            .iter()
            .map(|&(_, _, _, f)| self.state.slab.page(f.stmt().unwrap()))
            .collect();
        let mut ids: Vec<u32> = take.iter().map(|&(id, ..)| id).collect();
        ids.dedup();
        for &(id, off, end, f) in &take {
            let heat = self.heat_of(id);
            self.take_in_place(id, off, end, f, heat, fillers);
        }
        // An anchor or grow witness in a page the window restates from is
        // stated again too, where the flush has not touched the id already,
        // so that the page can empty.
        for id in ids {
            if main.records.contains_key(&id) {
                continue;
            }
            let Some(m) = self.state.allocs.get(&id) else {
                continue;
            };
            let in_touched =
                |s: Option<StmtRef>| s.is_some_and(|s| touched.contains(&self.state.slab.page(s)));
            let (ra, rg) = (in_touched(m.anchor), in_touched(m.grow));
            if ra || rg {
                let rec = self.touch(id, false, fillers);
                rec.replace_anchor |= ra;
                rec.replace_grow |= rg;
            }
        }
        self.stats.window_restated += take.len() as u64;
    }

    /// Moves the budget toward the target fill, after each commit.
    pub(crate) fn after_commit(&mut self) -> Result<(), Error> {
        if !self.opts.consolidate {
            return Ok(());
        }
        let (mut live, mut pages) = (0u64, 0u64);
        for info in self.state.pages.iter().skip(2) {
            if matches!(
                info.state,
                PageState::Data | PageState::Table | PageState::Interior
            ) {
                live += info.coverage as u64;
                pages += 1;
            }
        }
        if pages == 0 {
            return Ok(());
        }
        let fill = live as f64 / (pages as f64 * C as f64);
        let step = (4.0 * (self.opts.target_fill - fill)).exp();
        let (lo, hi) = (self.opts.budget_min as f64, self.opts.budget_max as f64);
        self.cons.budget = (self.cons.budget.max(1.0) * step).clamp(lo, hi);
        Ok(())
    }

    pub(crate) fn fill_stats(&self, s: &mut Stats) {
        s.file_pages = self.file_pages as u64;
        for (p, info) in self.state.pages.iter().enumerate() {
            if p < 2 {
                continue;
            }
            match info.state {
                PageState::Data => {
                    s.data_pages += 1;
                    s.live_data_bytes += info.coverage as u64;
                }
                PageState::Table | PageState::Interior => {
                    s.table_pages += 1;
                    s.live_table_bytes += info.coverage as u64;
                }
                PageState::Free | PageState::Retiring => s.free_pages += 1,
                _ => {}
            }
        }
        s.allocations = self.state.allocs.len() as u64;
        s.allocation_bytes = self.state.allocs.values().map(|m| m.size as u64).sum();
        s.fragments = self.state.frags.len() as u64;
        s.statements = self.state.slab.live as u64;
        s.budget = self.cons.budget as u64;
    }
}
