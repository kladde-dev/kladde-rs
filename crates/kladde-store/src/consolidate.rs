//! Consolidation: evacuating sparse data pages, in the room of pages the
//! flush writes anyway and on pages of their own; draining sparse leaves;
//! compaction mode; and the controller that paces it.
//!
//! The policy is today's churn floor, unchanged: a budgeted victim must free
//! `churn_floor` bytes of space per byte it writes. Its survivors are whole
//! spans, which keep their ids and are stated with their new addresses.

use crate::consts::MAX_PAGE_CONTENT;
use crate::error::Error;
use crate::flush::{DataPage, Flush, Output};
use crate::hash::IdSet;
use crate::state::*;
use crate::stats::Stats;
use crate::store::Inner;

/// A page's content capacity.
const C: u32 = MAX_PAGE_CONTENT as u32;

/// What consolidation carries from flush to flush.
#[derive(Debug, Default)]
pub struct ConsState {
    /// The per-flush budget in pages, as the controller moved it.
    pub budget: f64,
    /// Where sampling starts within a bucket; rotates so that no prefix of
    /// a bucket is examined forever.
    pub rot: usize,
    /// Victims this flush found unusable or took already, so that it does
    /// not pick them again.
    pub skip: IdSet,
    /// Whether this flush runs in compaction mode.
    pub compaction: bool,
    /// Where the search for the highest live page resumes.
    pub tail: u32,
    /// Where the rotating walk resumes: a span of the sequence, or 0 for its
    /// start.
    pub walk: u32,
}

/// What one budgeted data page would hold, or what table victims would
/// restate, and what either would reclaim.
#[derive(Debug, Default)]
struct Offer {
    whole: Vec<u32>,
    cut: Option<u32>,
    tail: bool,
    reclaimed: f64,
    written: f64,
    fill: f64,
}

impl Inner {
    /// Resets what consolidation keeps for one flush only.
    pub(crate) fn begin_consolidation(&mut self) {
        self.cons.skip.clear();
        self.cons.tail = self.file_pages.saturating_sub(1);
        let holes = match self.tail() {
            Some(t) => (2..t)
                .filter(|&p| self.state.pages[p as usize] == Page::Reusable)
                .count(),
            None => 0,
        };
        self.cons.compaction =
            self.opts.consolidate && holes as f64 > self.opts.hole_share * self.file_pages as f64;
        if self.cons.compaction {
            self.stats.compaction_flushes += 1;
        }
    }

    /// The highest page that may be a victim and is none of this flush yet.
    fn tail(&mut self) -> Option<u32> {
        let mut p = self.cons.tail;
        while p >= 2 && (p as usize) < self.state.pages.len() {
            if self.state.victim(p).is_some() && !self.cons.skip.contains(&p) {
                self.cons.tail = p;
                return Some(p);
            }
            p -= 1;
        }
        self.cons.tail = p.min(self.cons.tail);
        None
    }

    fn compaction_tail(&mut self, data: bool) -> Option<u32> {
        if !self.cons.compaction {
            return None;
        }
        self.tail().filter(|&t| {
            let is_data = self.state.live(t).kind == PageKind::Data;
            is_data == data
        })
    }

    /// The compaction tail, if a page that is reusable now lies below it.
    fn compaction_victim(&mut self, data: bool) -> Option<u32> {
        let lowest = self.ready.first().copied();
        self.compaction_tail(data)
            .filter(|&t| lowest.is_some_and(|l| l < t))
    }

    // ------------------------------------------------------------ victims

    /// LFS's cost-benefit score: sparse and long untouched first.
    fn score(&self, p: u32) -> f64 {
        let live = self.state.live(p);
        let u = live.coverage as f64 / C as f64;
        let age = self.state.flush_epoch.saturating_sub(live.epoch) as f64;
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
                    let cov = self.state.live(p).coverage;
                    cov > 0 && cov <= max && !self.cons.skip.contains(&p) && ok(self, p)
                })
                .max_by(|&x, &y| self.score(x).total_cmp(&self.score(y)));
            if best.is_some() {
                return best;
            }
        }
        None
    }

    /// The spans whose bytes lie in data page `v`, in the order they lie.
    fn survivors(&self, v: u32) -> Vec<u32> {
        let mut out = self.state.data_spans.get(&v).cloned().unwrap_or_default();
        out.sort_by_key(|&id| match self.state.span(id).content {
            Content::Ref { offset, .. } => offset,
            _ => 0,
        });
        out
    }

    /// Evacuates all of `v` into `dp`, if it fits.
    fn evacuate_into(&mut self, f: &mut Flush, v: u32, dp: &mut DataPage) -> Result<bool, Error> {
        self.cons.skip.insert(v);
        let total = self.state.live(v).coverage;
        if total as usize > dp.room() {
            return Ok(false);
        }
        for id in self.survivors(v) {
            self.place(f, dp, id)?;
        }
        self.stats.evacuated_pages += 1;
        self.stats.evacuated_bytes += total as u64;
        Ok(true)
    }

    /// Evacuates `v` into what room `dp` has left, cutting the span that
    /// crosses the boundary; returns the rest, not placed yet.
    fn evacuate_across(
        &mut self,
        f: &mut Flush,
        v: u32,
        dp: &mut DataPage,
    ) -> Result<Vec<u32>, Error> {
        self.cons.skip.insert(v);
        let total = self.state.live(v).coverage;
        let mut rest = Vec::new();
        for id in self.survivors(v) {
            let room = dp.room() as u32;
            let len = self.state.span(id).len;
            if !rest.is_empty() || room == 0 {
                rest.push(id);
            } else if len <= room {
                self.place(f, dp, id)?;
            } else if self.state.span(id).chain == Chain::Sequence {
                let t = self.split_span(f, id, room)?;
                self.place(f, dp, id)?;
                rest.push(t);
            } else {
                rest.push(id);
            }
        }
        self.stats.evacuated_pages += 1;
        self.stats.evacuated_bytes += total as u64;
        Ok(rest)
    }

    // ------------------------------------------------------------ free filling

    /// Fills the room of `open`, a page the flush writes anyway, with what
    /// costs nothing to move there: whole victims that fit, then as many of
    /// one victim's spans as fit.
    pub(crate) fn free_fill(&mut self, f: &mut Flush, open: &mut DataPage) -> Result<(), Error> {
        while let Some(b) = self.state.data_buckets.lowest_non_empty() {
            let room = open.room() as u32;
            if room == 0 {
                return Ok(());
            }
            let tail = self
                .compaction_tail(true)
                .filter(|&t| open.page < t && self.state.live(t).coverage <= room);
            let Some(v) = tail.or_else(|| self.pick_victim(true, b, room, &|_, _| true)) else {
                break;
            };
            if self.evacuate_into(f, v, open)? {
                self.stats.free_filled_pages += 1;
            }
        }
        if let Some(b) = self.state.data_buckets.lowest_non_empty() {
            if open.room() > 0 {
                if let Some(v) = self.pick_victim(true, b, C, &|_, _| true) {
                    for id in self.survivors(v) {
                        let len = self.state.span(id).len;
                        if len as usize <= open.room() {
                            self.place(f, open, id)?;
                            self.stats.evacuated_bytes += len as u64;
                        }
                    }
                }
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
        let mut offer = Offer::default();
        let mut chosen = IdSet::default();
        if let Some(t) = self.compaction_victim(true) {
            let cov = self.state.live(t).coverage;
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
                offer.reclaimed += C as f64;
                offer.written += cov as f64;
            }
        }
        while let Some(b) = self.state.data_buckets.lowest_non_empty() {
            if offer.cut.is_some() {
                break;
            }
            let not_chosen = |_: &Inner, p: u32| !chosen.contains(&p);
            let Some(v) = self.pick_victim(true, b, room.min(max), &not_chosen) else {
                break;
            };
            let cov = self.state.live(v).coverage;
            chosen.insert(v);
            offer.whole.push(v);
            offer.reclaimed += (C - cov) as f64;
            offer.written += cov as f64;
            room -= cov;
        }
        if may_cut && room as f64 > self.opts.theta * C as f64 && room < max {
            let from = Buckets::bucket_of(room + 1) as usize;
            let too_big =
                |i: &Inner, p: u32| !chosen.contains(&p) && i.state.live(p).coverage > room;
            if let Some(v) = self.pick_victim(true, from, max, &too_big) {
                let cov = self.state.live(v).coverage;
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

    /// What rewriting leaves would restate and reclaim: the tail first in
    /// compaction mode, then whole victims within the churn floor, best
    /// score first, while their entries fit a leaf and the `pages` of
    /// budget left. A leaf's entries are priced at its coverage.
    fn table_offer(&mut self, pages: f64) -> Option<Offer> {
        let max = (C as f64 / (1.0 + self.opts.churn_floor)) as u32;
        let mut room = pages.min(1.0) * C as f64;
        let mut offer = Offer::default();
        let mut chosen = IdSet::default();
        if let Some(t) = self.compaction_victim(false) {
            let cov = self.state.live(t).coverage;
            chosen.insert(t);
            offer.whole.push(t);
            offer.tail = true;
            offer.reclaimed += C as f64;
            offer.written += cov as f64;
            room -= cov as f64;
        }
        while let Some(b) = self.state.table_buckets.lowest_non_empty() {
            let fresh = |_: &Inner, p: u32| !chosen.contains(&p);
            let limit = (room.max(0.0) as u32).min(max);
            let Some(v) = self.pick_victim(false, b, limit, &fresh) else {
                break;
            };
            let cov = self.state.live(v).coverage;
            chosen.insert(v);
            offer.whole.push(v);
            offer.reclaimed += (C - cov) as f64;
            offer.written += cov as f64;
            room -= cov as f64;
        }
        if offer.whole.is_empty() {
            return None;
        }
        Some(offer)
    }

    /// Rewrites leaves: every live entry they hold is stated again by the
    /// cut, which drains them.
    fn rewrite_table_victims(&mut self, f: &mut Flush, victims: &[u32]) {
        for &v in victims {
            self.cons.skip.insert(v);
            for id in self.state.table_spans.get(&v).cloned().unwrap_or_default() {
                f.stated.insert(id);
            }
            self.stats.table_rewrites += 1;
        }
    }

    /// Spends the flush's budget, in pages written. Data pages are opened
    /// only to reclaim, one at a time, each only if its offer passes the
    /// churn floor and fills it. Leaves open no page of their own, since
    /// their entries join the leaves the cut writes anyway: their offer
    /// passes on the churn floor alone, and costs the budget the pages its
    /// entries are estimated to take.
    pub(crate) fn budget_loop(&mut self, f: &mut Flush, out: &mut Output) -> Result<(), Error> {
        let budget = self.cons.budget.round().max(0.0);
        let (lambda, theta) = (self.opts.churn_floor, self.opts.theta);
        let pays = |o: &Offer| o.tail || o.reclaimed >= lambda * o.written;
        let fills = |o: &Offer| o.tail || o.fill >= 1.0 - theta;
        let ratio = |o: &Offer| o.reclaimed / o.written.max(1.0);
        let mut spent = 0.0;
        let mut carry: Vec<u32> = Vec::new();
        while spent < budget {
            let carry_len: u32 = carry.iter().map(|&id| self.state.span(id).len).sum();
            let data = if spent + 1.0 <= budget {
                self.data_offer(carry_len, spent + 2.0 <= budget)
            } else {
                None
            };
            let offer = if !carry.is_empty() {
                data
            } else {
                let data = data.filter(|o| pays(o) && fills(o));
                let table = self.table_offer(budget - spent).filter(pays);
                match (data, table) {
                    (Some(d), Some(t)) if d.tail || (!t.tail && ratio(&d) >= ratio(&t)) => Some(d),
                    (_, Some(t)) => {
                        spent += t.written / C as f64;
                        self.rewrite_table_victims(f, &t.whole);
                        continue;
                    }
                    (Some(d), None) => Some(d),
                    (None, None) => break,
                }
            };
            spent += 1.0;
            let mut dp = self.new_data_page()?;
            for id in std::mem::take(&mut carry) {
                self.place(f, &mut dp, id)?;
            }
            if let Some(o) = offer {
                for v in o.whole {
                    self.evacuate_into(f, v, &mut dp)?;
                }
                if let Some(v) = o.cut {
                    carry = self.evacuate_across(f, v, &mut dp)?;
                }
            }
            self.stats.budget_pages += 1;
            out.data.push(dp);
        }
        if !carry.is_empty() {
            // The budget's last page cut a span: its rest takes a page of
            // its own.
            let mut dp = self.new_data_page()?;
            for id in carry {
                self.place(f, &mut dp, id)?;
            }
            out.data.push(dp);
        }
        Ok(())
    }

    /// Whole leaves whose entries fit `room` bytes of the cut's last leaf,
    /// sparsest first; at most a page's worth.
    pub(crate) fn cut_fillers(&mut self, f: &Flush, room: usize, _leaf: &[u32]) -> Vec<u32> {
        // Entries restated among others take a little more than they did:
        // keep a margin.
        let mut room = (room as f64 * 0.9) as u32;
        let mut out = Vec::new();
        while let Some(b) = self.state.table_buckets.lowest_non_empty() {
            let ok = |i: &Inner, p: u32| {
                i.state
                    .table_spans
                    .get(&p)
                    .is_some_and(|l| l.iter().all(|id| !f.stated.contains(id)))
            };
            let Some(v) = self.pick_victim(false, b, room, &ok) else {
                break;
            };
            let cov = self.state.live(v).coverage;
            self.cons.skip.insert(v);
            out.push(v);
            room -= cov;
        }
        out
    }

    /// Moves the budget toward the target fill, after each commit. The fill
    /// is measured against the whole file, so that holes count against it as
    /// much as sparse pages do.
    pub(crate) fn after_commit(&mut self) -> Result<(), Error> {
        if !self.opts.consolidate {
            return Ok(());
        }
        let mut live = 0u64;
        for page in self.state.pages.iter().skip(2) {
            if let Page::Live(l) = page {
                live += l.coverage as u64;
            }
        }
        let pages = self.file_pages.saturating_sub(2) as u64;
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
        for page in self.state.pages.iter().skip(2) {
            match page {
                Page::Live(Live {
                    coverage,
                    kind: PageKind::Data,
                    ..
                }) => {
                    s.data_pages += 1;
                    s.live_data_bytes += *coverage as u64;
                }
                Page::Live(Live {
                    coverage,
                    kind: PageKind::Table,
                    ..
                }) => {
                    s.table_pages += 1;
                    s.live_table_bytes += *coverage as u64;
                }
                Page::Reusable | Page::Fallback => s.free_pages += 1,
                Page::Journal
                | Page::Live(Live {
                    kind: PageKind::Header,
                    ..
                }) => {}
                Page::Filling(fl) => unreachable!("a page is filling between flushes: {fl:?}"),
            }
        }
        s.sequence_bytes = self.state.seq_len();
        s.schema_bytes = self.schema.len() as u64;
        s.spans = self.state.ids.count() as u64;
        s.budget = self.cons.budget as u64;
    }
}
