//! Consolidation by ripeness (`drafts/ripeness.md`, on top of
//! `impl/consolidation.md`): evacuating the pages for which waiting no
//! longer pays, in the room of pages the flush writes anyway and on pages of
//! their own, and the controller that sets the price of space.

use crate::consts::MAX_PAGE_CONTENT;
use crate::defrag::{Candidate, Weighed};
use crate::error::Error;
use crate::flush::{DataPage, Output};
use crate::hash::{IdMap, IdSet};
use crate::ripeness::{Moments, Prior, BETA};
use crate::state::*;
use crate::stats::Stats;
use crate::store::Inner;

/// A page's content capacity.
const C: u32 = MAX_PAGE_CONTENT as u32;

/// How much more encoding restating a table page's live content takes than
/// its coverage: fragments that shadowing split need a statement each, and
/// statements lose the dense delta encoding of their old neighbours.
const RESTATE: f64 = 1.25;

/// The most ripe pages one offer considers.
const CANDIDATES: usize = 64;

/// The cursor gives up a page older than this many epochs, and leaves the
/// rest of it to ripeness.
const W: u64 = 8;

/// The expected static share at which a page counts, in the statistics, as
/// holding one.
const STATIC_SHARE: f64 = 0.1;

/// The bounds of the price of space.
const KAPPA_MIN: f64 = 1e-6;
const KAPPA_MAX: f64 = 1.0;

/// What consolidation carries from flush to flush.
#[derive(Debug, Default)]
pub struct ConsState {
    /// The rotating window's cursor.
    pub cursor: Key,
    /// The price of space, as the controller moved it: the page writes that
    /// a page of garbage kept for one flush is worth.
    pub kappa: f64,
    /// Victims this flush found unusable, so that it does not pick them again.
    pub skip: IdSet,
    /// Description defragmentation's candidates from the latest walk, for
    /// the next flush.
    pub candidates: Vec<Candidate>,
    /// Whether this flush runs in compaction mode.
    pub compaction: bool,
    /// Where the search for the highest live page resumes.
    pub tail: u32,
    /// Where the consolidator state is kept.
    pub kept: crate::constate::Kept,
    /// The data page whose survivors fill the room of the flush's own pages.
    pub cursor_page: Option<u32>,
    /// The data pages of the latest flush that wrote any, where the cursor
    /// finds its next page.
    pub prev_data_pages: Vec<u32>,
    /// The pages the latest flush wrote, whose starting estimates the next
    /// flush records.
    pub just_written: Vec<u32>,
    /// Pages in their first [`memory`](crate::ripeness::memory) epochs,
    /// whose losses so far the file's empirical prior takes in once they are
    /// that old, or gone.
    pub young: Vec<Young>,
    /// Per kind, data pages and leaves: the method of moments' sums over the
    /// early losses of the pages the file has written.
    pub moments: [Moments; 2],
    /// Per kind: the prior on the rate that the pages a flush writes start
    /// from, the file's empirical prior once it has one.
    pub prior: [Prior; 2],
    /// Pages whose estimate this flush changed, for the consolidator state.
    pub drained: Vec<u32>,
    /// Victims this flush took, anywhere.
    pub cleaned: u32,
    /// Whether this flush spent its whole budget.
    pub exhausted: bool,
}

/// A page in its first epochs, and what it has lost in them.
#[derive(Clone, Copy, Debug)]
pub struct Young {
    pub page: u32,
    /// The epoch it was written, which tells it from a later page in its slot.
    pub epoch: u64,
    pub data: bool,
    /// Loss events, and their exposure, in events.
    pub events: f64,
    pub exposure: f64,
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

/// What one budgeted page would hold.
#[derive(Debug, Default)]
struct Offer {
    /// Victims evacuated, or rewritten, whole.
    whole: Vec<u32>,
    /// One more victim whose survivors fill the page and lead the next one.
    cut: Option<u32>,
    /// Whether the offer holds compaction mode's tail, which the budget
    /// takes whatever the offer's fill.
    tail: bool,
    /// The bytes it would write.
    written: f64,
    /// How full the page would close, as a fraction of its capacity.
    fill: f64,
}

impl Inner {
    /// Resets what consolidation keeps for one flush only.
    pub(crate) fn begin_consolidation(&mut self) {
        self.cons.skip.clear();
        self.cons.cleaned = 0;
        self.cons.exhausted = false;
        // Compaction mode: while holes below the highest live page exceed a
        // share of the file, that page is every mechanism's first victim.
        self.cons.tail = self.file_pages.saturating_sub(1);
        let holes = match self.tail() {
            Some(t) => (2..t)
                .filter(|&p| self.state.pages[p as usize].state == PageState::Free)
                .count(),
            None => 0,
        };
        self.cons.compaction =
            self.opts.consolidate && holes as f64 > self.opts.hole_share * self.file_pages as f64;
        if self.cons.compaction {
            self.stats.compaction_flushes += 1;
        }
    }

    /// The highest page with live data or statements that is no victim of
    /// this flush yet: where compaction mode works down from. Journal pages
    /// are passed over, since the next flush moves the journal by itself.
    fn tail(&mut self) -> Option<u32> {
        let mut p = self.cons.tail;
        while p >= 2 && (p as usize) < self.state.pages.len() {
            let info = &self.state.pages[p as usize];
            let live =
                matches!(info.state, PageState::Data | PageState::Table) && info.coverage > 0;
            if live && !self.cons.skip.contains(&p) && !self.flush_rewritten.contains(&p) {
                self.cons.tail = p;
                return Some(p);
            }
            p -= 1;
        }
        self.cons.tail = p.min(self.cons.tail);
        None
    }

    /// The tail, if compaction mode is on and the tail is a page of the kind
    /// asked for.
    fn compaction_tail(&mut self, data: bool) -> Option<u32> {
        if !self.cons.compaction {
            return None;
        }
        let want = if data {
            PageState::Data
        } else {
            PageState::Table
        };
        self.tail()
            .filter(|&t| self.state.pages[t as usize].state == want)
    }

    /// The compaction tail, if a page that is reusable now lies below it for
    /// its content: moved to a page the file grows by, it would only become
    /// the next tail. Right after a mass free, the holes are still in
    /// quarantine.
    fn compaction_victim(&mut self, data: bool) -> Option<u32> {
        let lowest = self.ready.first().copied();
        self.compaction_tail(data)
            .filter(|&t| lowest.is_some_and(|l| l < t))
    }

    // ------------------------------------------------------------ victims

    /// Up to `limit` pages ripe at the current price, highest index first:
    /// of kind `kind` if given, holding at most `max` bytes, and neither in
    /// `exclude` nor taken or found unusable by this flush already.
    fn ripe_pages(
        &mut self,
        kind: Option<PageState>,
        max: u32,
        exclude: &IdSet,
        limit: usize,
    ) -> Vec<u32> {
        let now = self.state.flush_epoch;
        let packed = crate::ripeness::packed_fill(self.opts.theta);
        let sigma = self.state.sigma();
        self.state
            .refresh_ranking(now, packed, sigma, self.opts.ripeness_rule);
        let pages = &self.state.pages;
        let (skip, rewritten) = (&self.cons.skip, &self.flush_rewritten);
        let mut ok = |p: u32| {
            let info = &pages[p as usize];
            kind.is_none_or(|k| info.state == k)
                && info.coverage <= max
                && !exclude.contains(&p)
                && !skip.contains(&p)
                && !rewritten.contains(&p)
        };
        self.state
            .ripeness
            .ripe(now, self.cons.kappa, &mut ok, limit)
    }

    /// The cursor page, which the room of the flush's own pages takes
    /// survivors from: the current one while it has content and is at most
    /// `W` epochs old, else the least-filled data page of the latest earlier
    /// flush that wrote any.
    fn cursor_page(&mut self) -> Option<u32> {
        let now = self.state.flush_epoch;
        // A truncation may have taken the page away since.
        let usable = |p: u32, i: &Inner| {
            i.state.pages.get(p as usize).is_some_and(|info| {
                info.state == PageState::Data
                    && info.coverage > 0
                    && info.epoch < now
                    && now - info.epoch <= W
                    && !i.cons.skip.contains(&p)
            })
        };
        if let Some(p) = self.cons.cursor_page.filter(|&p| usable(p, self)) {
            return Some(p);
        }
        let next = self
            .cons
            .prev_data_pages
            .iter()
            .copied()
            .filter(|&p| usable(p, self))
            .min_by_key(|&p| self.state.pages[p as usize].coverage);
        self.cons.cursor_page = next;
        next
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
        self.cons.cleaned += 1;
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
        self.cons.cleaned += 1;
        Ok(rest)
    }

    // ------------------------------------------------------------ free filling

    /// Fills the room of `open`, a page the flush writes anyway: in
    /// compaction mode with the tail first, whenever it fits and `open` lies
    /// below it; then with whole ripe victims whose survivors take at most
    /// `θ` of a page, highest index first; then with the cursor page's
    /// survivors, a whole statement's at a time.
    pub(crate) fn free_fill(
        &mut self,
        open: &mut DataPage,
        dirty: &mut Dirty,
    ) -> Result<(), Error> {
        loop {
            let room = open.room() as u32;
            let tail = self
                .compaction_tail(true)
                .filter(|&t| open.page < t && self.state.pages[t as usize].coverage <= room);
            let Some(t) = tail else { break };
            if self.evacuate_into(t, open, dirty)? {
                self.stats.free_filled_pages += 1;
            }
        }
        let small = ((self.opts.theta * C as f64) as u32).min(open.room() as u32);
        if small > 0 {
            let none = IdSet::default();
            for v in self.ripe_pages(Some(PageState::Data), small, &none, CANDIDATES) {
                let cov = self.state.pages[v as usize].coverage;
                if cov as usize <= open.room() && self.evacuate_into(v, open, dirty)? {
                    self.stats.free_filled_pages += 1;
                }
            }
        }
        if open.room() > 0 {
            if let Some(c) = self.cursor_page() {
                self.fill_from(c, open, dirty)?;
            }
        }
        Ok(())
    }

    /// Moves the survivors of as many of `v`'s statements as fit into
    /// `open`, each statement's all or none. What stays behind is left to
    /// later flushes.
    fn fill_from(&mut self, v: u32, open: &mut DataPage, dirty: &mut Dirty) -> Result<(), Error> {
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
    /// it: in compaction mode the tail first, whatever its fill; then ripe
    /// data pages, highest index first, whole while they fit, and, if
    /// `may_cut` and the page would close more than θ empty, the first one
    /// that does not fit, cut at the page boundary.
    fn data_offer(&mut self, carry: u32, may_cut: bool) -> Option<Offer> {
        let mut room = C - carry;
        let mut offer = Offer::default();
        let mut chosen = IdSet::default();
        // In compaction mode, the tail first, whatever its fill: it returns a
        // whole page to the file system, not only to the pool.
        if let Some(t) = self.compaction_victim(true) {
            let cov = self.state.pages[t as usize].coverage;
            let taken = if cov <= room {
                offer.whole.push(t);
                room -= cov;
                true
            } else if may_cut {
                offer.cut = Some(t);
                room = 0;
                true
            } else {
                false
            };
            if taken {
                chosen.insert(t);
                offer.tail = true;
                offer.written += cov as f64;
            }
        }
        if offer.cut.is_none() && room > 0 {
            let theta = self.opts.theta * C as f64;
            for v in self.ripe_pages(Some(PageState::Data), C, &chosen, CANDIDATES) {
                let cov = self.state.pages[v as usize].coverage;
                if cov <= room {
                    offer.whole.push(v);
                    offer.written += cov as f64;
                    room -= cov;
                    if room == 0 {
                        break;
                    }
                } else if may_cut && room as f64 > theta {
                    offer.cut = Some(v);
                    offer.written += cov as f64;
                    room = 0;
                    break;
                }
            }
        }
        if offer.whole.is_empty() && offer.cut.is_none() {
            return None;
        }
        offer.fill = (C - room) as f64 / C as f64;
        Some(offer)
    }

    /// Spends the flush's budget, a cap on its work: pages opened only to
    /// take ripe victims, one at a time, each only if its offer fills it. A
    /// survivor cut at one page's end leads the next, which is opened
    /// whatever its own offer, so the budget's last page cuts nothing. The
    /// page kind of the highest ripe page goes first; an offer holding
    /// compaction mode's tail passes whatever its fill, and goes first, since
    /// the budget alone paces the mode.
    pub(crate) fn budget_loop(&mut self, dirty: &mut Dirty, out: &mut Output) -> Result<(), Error> {
        let pages = self.opts.budget_pages;
        let theta = self.opts.theta;
        let good = |o: &Offer| o.tail || o.fill >= 1.0 - theta;
        let mut carry: Vec<Survivor> = Vec::new();
        self.cons.exhausted = true;
        for i in 0..pages {
            let carry_len: u32 = carry.iter().map(Survivor::len).sum();
            let data = self.data_offer(carry_len, i + 1 < pages);
            // A carried tail opens a data page whatever the offers.
            let offer = if !carry.is_empty() {
                data
            } else {
                let data = data.filter(good);
                let table = self.table_offer().filter(good);
                let data_first = |i: &mut Inner| {
                    let none = IdSet::default();
                    let top = i.ripe_pages(None, C, &none, 1);
                    top.first()
                        .is_none_or(|&p| i.state.pages[p as usize].state == PageState::Data)
                };
                match (data, table) {
                    (Some(d), Some(t)) if d.tail || (!t.tail && data_first(self)) => Some(d),
                    (_, Some(t)) => {
                        self.rewrite_table_victims(&t.whole, dirty)?;
                        continue;
                    }
                    (Some(d), None) => Some(d),
                    (None, None) => {
                        self.cons.exhausted = false;
                        break;
                    }
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

    /// What rewriting table victims onto one budgeted leaf would hold, their
    /// restatements estimated from coverage, without taking anything: the
    /// tail first in compaction mode, whatever its size, then ripe leaves,
    /// highest index first, while they fit.
    fn table_offer(&mut self) -> Option<Offer> {
        let mut room = C as f64;
        let mut offer = Offer::default();
        let mut chosen = IdSet::default();
        if let Some(t) = self.compaction_victim(false) {
            let cov = self.state.pages[t as usize].coverage;
            chosen.insert(t);
            offer.whole.push(t);
            offer.tail = true;
            offer.written += cov as f64 * RESTATE;
            room -= cov as f64 * RESTATE;
        }
        if room > 0.0 {
            let limit = (room / RESTATE) as u32;
            for v in self.ripe_pages(Some(PageState::Table), limit, &chosen, CANDIDATES) {
                let w = self.state.pages[v as usize].coverage as f64 * RESTATE;
                if w <= room {
                    offer.whole.push(v);
                    offer.written += w;
                    room -= w;
                }
            }
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
            self.cons.cleaned += 1;
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

    /// The fillers of the cut's last page: in compaction mode the tail,
    /// whenever it fits its `room`, then ripe leaves that fit, highest index
    /// first, then the rotating window. Takes into `fillers`, and returns the
    /// victims.
    pub(crate) fn cut_fillers(
        &mut self,
        room: usize,
        main: &Dirty,
        fillers: &mut Dirty,
    ) -> Result<Vec<u32>, Error> {
        let mut room = room as f64;
        let mut victims = Vec::new();
        loop {
            let limit = (room / RESTATE) as u32;
            let tail = self.compaction_victim(false).filter(|&t| {
                self.state.pages[t as usize].coverage <= limit && self.filler_eligible(t, main)
            });
            let Some(v) = tail else { break };
            room -= self.state.pages[v as usize].coverage as f64 * RESTATE;
            self.rewrite_table_victims(&[v], fillers)?;
            victims.push(v);
        }
        let limit = (room / RESTATE) as u32;
        let none = IdSet::default();
        for v in self.ripe_pages(Some(PageState::Table), limit, &none, CANDIDATES) {
            let w = self.state.pages[v as usize].coverage as f64 * RESTATE;
            if w > room || !self.filler_eligible(v, main) {
                continue;
            }
            room -= w;
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
    pub(crate) fn window(&mut self, room: usize, main: &Dirty, fillers: &mut Dirty) {
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

    /// Moves the price of space toward the target fill, after each commit:
    /// up while the data pages and leaves are emptier than the target, which
    /// makes fuller pages ripe, and down while they are fuller. Those pages
    /// answer a change of price within a flush, where the whole file, whose
    /// freed pages stay free until new writes reuse them, would answer it
    /// only much later; holes are compaction mode's to return. The price
    /// holds where moving it could change nothing: it does not fall after a
    /// flush that cleaned nothing, nor rise after one that spent its whole
    /// budget.
    pub(crate) fn after_commit(&mut self) -> Result<(), Error> {
        if !self.opts.consolidate {
            return Ok(());
        }
        let (mut live, mut pages) = (0u64, 0u64);
        for info in self.state.pages.iter().skip(2) {
            if matches!(info.state, PageState::Data | PageState::Table) {
                live += info.coverage as u64;
                pages += 1;
            }
        }
        if pages == 0 {
            return Ok(());
        }
        let fill = live as f64 / (pages as f64 * C as f64);
        let miss = self.opts.target_fill - fill;
        let held = (miss < 0.0 && self.cons.cleaned == 0) || (miss > 0.0 && self.cons.exhausted);
        if !held {
            let step = (self.opts.kappa_gain * miss).exp();
            self.cons.kappa = (self.cons.kappa * step).clamp(KAPPA_MIN, KAPPA_MAX);
        }
        Ok(())
    }

    /// Adds the natural losses the flush has recorded since the last call to
    /// the estimates of the pages that lost them. On the flush's first call,
    /// what young pages have lost also goes into the file's empirical prior,
    /// and the evidence of loss events' sizes and of early losses is
    /// discounted, as a page's is, once per flush.
    pub(crate) fn apply_losses(&mut self, first: bool) {
        let now = self.state.flush_epoch;
        let sigma = self.state.sigma();
        let losses = std::mem::take(&mut self.state.losses);
        if first {
            self.learn_prior(&losses, sigma);
            // The estimates the pages of the previous flush started from are
            // recorded too.
            self.cons.drained = std::mem::take(&mut self.cons.just_written);
        }
        for (p, loss) in losses {
            let info = &mut self.state.pages[p as usize];
            if !matches!(info.state, PageState::Data | PageState::Table) {
                continue;
            }
            let s = sigma[usize::from(info.state != PageState::Data)];
            info.drain.lose(&loss, info.epoch, s, now);
            self.state.rerank(p);
            self.cons.drained.push(p);
        }
    }

    /// Adds this flush's `losses` of young pages to what they have lost, and
    /// a page that has been watched for the posterior's memory, or is gone,
    /// to the sums the file's empirical prior follows from; then sets the
    /// priors that the pages this flush writes start from.
    fn learn_prior(&mut self, losses: &IdMap<crate::ripeness::Loss>, sigma: [f64; 2]) {
        let now = self.state.flush_epoch;
        let delta = (-BETA).exp();
        for m in &mut self.cons.moments {
            m.discount(delta);
        }
        for e in &mut self.state.events {
            e.0 *= delta;
            e.1 *= delta;
        }
        let memory = crate::ripeness::memory() as u64;
        let mut young = std::mem::take(&mut self.cons.young);
        young.retain_mut(|y| {
            let kind = usize::from(!y.data);
            let info = self.state.pages.get(y.page as usize);
            let alive = info.is_some_and(|i| {
                i.epoch == y.epoch && matches!(i.state, PageState::Data | PageState::Table)
            });
            if let Some(info) = info.filter(|_| alive) {
                let (lost, live) = losses
                    .get(&y.page)
                    .map_or((0, info.coverage), |l| (l.lost, l.live));
                y.events += lost as f64 / sigma[kind];
                y.exposure += (live as f64 - lost as f64 / 2.0) / sigma[kind];
            }
            let done = !alive || now.saturating_sub(y.epoch) >= memory;
            if done {
                self.cons.moments[kind].add(y.events, y.exposure);
            }
            !done
        });
        for kind in 0..2 {
            // Until pages have been watched long enough, the young ones'
            // losses so far stand in.
            let mut m = self.cons.moments[kind];
            if m.prior().is_none() {
                for y in young.iter().filter(|y| usize::from(!y.data) == kind) {
                    m.add(y.events, y.exposure);
                }
            }
            if let Some(p) = m.prior() {
                self.cons.prior[kind] = p;
            }
        }
        self.cons.young = young;
    }

    pub(crate) fn fill_stats(&self, s: &mut Stats) {
        s.file_pages = self.file_pages as u64;
        for (p, info) in self.state.pages.iter().enumerate() {
            if p < 2 {
                continue;
            }
            if matches!(info.state, PageState::Data | PageState::Table) {
                let fixed = 1.0 - info.drain.share as f64;
                if fixed >= STATIC_SHARE {
                    s.static_pages += 1;
                }
                s.static_bytes += (fixed * info.coverage as f64) as u64;
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
        // The consolidator state is the store's own, not the application's.
        let own = self.header.consolidator_state;
        let apps = self.state.allocs.iter().filter(|(&id, _)| id != own);
        s.allocations = apps.clone().count() as u64;
        s.allocation_bytes = apps.map(|(_, m)| m.size as u64).sum();
        s.fragments = self.state.frags.len() as u64;
        s.statements = self.state.slab.live as u64;
        s.budget = self.opts.budget_pages as u64;
        s.kappa = self.cons.kappa;
        s.ripe_ranked = self.state.ripeness.len() as u64;
    }
}
