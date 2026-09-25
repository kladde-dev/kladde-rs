//! Cleaning by ripeness (`drafts/ripeness.md`): how fast each page still
//! drains, estimated from its own losses, and the pages ranked by the price
//! of space at which cleaning them starts to pay.
//!
//! A page of fill `u` whose content dies at rate `r` is ripe at price `κ`
//! once `h(u) ≥ r / κ`, with `h(u) = (1 − u)/u − ln(1/u)`; its ripeness
//! index `h(u) / r` is the `1/κ` at which that happens. Every estimate decays
//! by the same factor per epoch, so every index grows by the same factor,
//! and the order of pages changes only when a page's own content does.

use std::cmp::Ordering;
use std::collections::{BTreeSet, BinaryHeap};

use crate::hash::{IdMap, IdSet};

/// How fast a page's estimate forgets old losses, per epoch.
pub const BETA: f64 = 0.1;
/// The slowest rate content is assumed to drain at, per epoch: without it,
/// an estimate that had decayed to nothing would make a page ripe at any
/// fill.
pub const R_MIN: f64 = 1e-4;

/// A page's drain rate estimate.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Drain {
    /// The rate at epoch `at`, as a fraction of live bytes per epoch.
    pub rho: f32,
    /// The epoch of the page's last natural loss, or of its writing.
    pub at: u64,
}

impl Drain {
    /// A flush's fold superseded `lost` of the page's `live` bytes at `now`.
    pub fn lose(&mut self, lost: u32, live: u32, now: u64) {
        if live == 0 {
            return;
        }
        let decayed = self.rho as f64 * decay(now, self.at);
        let rho = decayed + (1.0 - (-BETA).exp()) * lost as f64 / live as f64;
        self.rho = rho as f32;
        self.at = now;
    }

    /// The estimate at `now`, never below [`R_MIN`].
    pub fn rate(&self, now: u64) -> f64 {
        (self.rho as f64 * decay(now, self.at)).max(R_MIN)
    }
}

fn decay(now: u64, at: u64) -> f64 {
    (-BETA * now.saturating_sub(at) as f64).exp()
}

/// What keeping a page of fill `u` waiting costs, relative to what its
/// draining saves: the threshold `h(u*) = r / κ` of the draft.
pub fn h(u: f64) -> f64 {
    (1.0 - u) / u + u.ln()
}

/// A log-index, totally ordered.
#[derive(Clone, Copy, Debug)]
struct Ln(f64);

impl PartialEq for Ln {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Ln {}
impl PartialOrd for Ln {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Ln {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// The pages that could be ripe -- live data pages and leaves no fuller than
/// the cap -- ordered by their ripeness index.
#[derive(Debug, Default)]
pub struct Ripeness {
    /// `K = ln h(u) − ln rho − β·at`: the page's log-index is `K + β·now`
    /// while its estimate stays above the floor.
    draining: BTreeSet<(Ln, u32)>,
    /// `ln h(u) − ln R_MIN`: the log-index of a page at the floor, constant.
    settled: BTreeSet<(Ln, u32)>,
    /// Where each ranked page is: whether settled, and its key.
    at: IdMap<(bool, Ln)>,
    /// Pages whose fill, state, or estimate changed since they were ranked.
    pub stale: IdSet,
}

impl Ripeness {
    pub fn remove(&mut self, page: u32) {
        if let Some((settled, k)) = self.at.remove(&page) {
            if settled {
                self.settled.remove(&(k, page));
            } else {
                self.draining.remove(&(k, page));
            }
        }
    }

    /// Ranks `page` at fill `u` with estimate `d`, as of `now`.
    pub fn insert(&mut self, page: u32, u: f64, d: Drain, now: u64) {
        self.remove(page);
        let lh = h(u).ln();
        if d.rho as f64 * decay(now, d.at) > R_MIN {
            let k = Ln(lh - (d.rho as f64).ln() - BETA * d.at as f64);
            self.draining.insert((k, page));
            self.at.insert(page, (false, k));
        } else {
            let k = Ln(lh - R_MIN.ln());
            self.settled.insert((k, page));
            self.at.insert(page, (true, k));
        }
    }

    /// How many pages are ranked.
    pub fn len(&self) -> usize {
        self.at.len()
    }

    /// Up to `limit` pages that are ripe at price `kappa` and pass `ok`,
    /// highest index first. `drain` gives each page's estimate; a draining
    /// page whose estimate has reached the floor is overstated by its key,
    /// so it can surface early but never hide further down, and it is
    /// settled on the way.
    pub fn ripe(
        &mut self,
        now: u64,
        kappa: f64,
        drain: &dyn Fn(u32) -> Drain,
        ok: &mut dyn FnMut(u32) -> bool,
        limit: usize,
    ) -> Vec<u32> {
        let floor = -kappa.ln();
        let bn = BETA * now as f64;
        let mut out = Vec::new();
        let mut settle = Vec::new();
        {
            let mut d = self.draining.iter().rev().peekable();
            let mut s = self.settled.iter().rev().peekable();
            // Pages settled on the way, at their settled keys.
            let mut late: BinaryHeap<(Ln, u32)> = BinaryHeap::new();
            while out.len() < limit {
                let dk = d.peek().map(|&&(k, p)| (k.0 + bn, p, 0));
                let sk = s.peek().map(|&&(k, p)| (k.0, p, 1));
                let lk = late.peek().map(|&(k, p)| (k.0, p, 2));
                let best = [dk, sk, lk]
                    .into_iter()
                    .flatten()
                    .max_by(|a, b| a.0.total_cmp(&b.0));
                let Some((index, p, from)) = best else { break };
                if index < floor {
                    break;
                }
                match from {
                    0 => {
                        let &(k, _) = d.next().unwrap();
                        let e = drain(p);
                        if e.rho as f64 * decay(now, e.at) <= R_MIN {
                            let ks =
                                Ln(k.0 + (e.rho as f64).ln() + BETA * e.at as f64 - R_MIN.ln());
                            settle.push((p, k, ks));
                            late.push((ks, p));
                            continue;
                        }
                    }
                    1 => {
                        s.next();
                    }
                    _ => {
                        late.pop();
                    }
                }
                if ok(p) {
                    out.push(p);
                }
            }
        }
        for (p, k, ks) in settle {
            self.draining.remove(&(k, p));
            self.settled.insert((ks, p));
            self.at.insert(p, (true, ks));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_losing_a_steady_share_settles_at_that_rate() {
        let mut d = Drain::default();
        for now in 1..200 {
            d.lose(10, 1000, now);
        }
        assert!((d.rate(199) - 0.01).abs() < 1e-4, "{}", d.rate(199));
    }

    #[test]
    fn a_page_that_stops_losing_decays_to_the_floor() {
        let mut d = Drain::default();
        d.lose(500, 1000, 1);
        assert!(d.rate(2) > 0.01);
        assert_eq!(d.rate(1000), R_MIN);
    }

    #[test]
    fn the_threshold_matches_the_drafts_table() {
        // `u*` for `r / κ`, from `drafts/ripeness.md`.
        for (ratio, u) in [(0.001, 0.96), (0.01, 0.87), (0.1, 0.66), (1.0, 0.32)] {
            let (mut lo, mut hi) = (1e-9, 1.0 - 1e-9);
            for _ in 0..100 {
                let mid = (lo + hi) / 2.0;
                if h(mid) > ratio {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            assert!((lo - u).abs() < 0.01, "r/κ = {ratio}: u* = {lo}");
        }
    }

    #[test]
    fn ripe_pages_come_highest_index_first() {
        let mut r = Ripeness::default();
        let drains: IdMap<Drain> = [
            (1, Drain { rho: 0.1, at: 10 }), // hot: ripe only when sparse
            (2, Drain { rho: 0.0, at: 0 }),  // frozen
            (3, Drain { rho: 0.1, at: 10 }),
        ]
        .into_iter()
        .collect();
        r.insert(1, 0.3, drains[&1], 10);
        r.insert(2, 0.8, drains[&2], 10);
        r.insert(3, 0.9, drains[&3], 10);
        let ripe = r.ripe(10, 0.01, &|p| drains[&p], &mut |_| true, 10);
        // Frozen at 0.8: h = 0.027, index 270. Hot at 0.3: h = 1.13,
        // index 11. Hot at 0.9: h = 0.006, index 0.06, not ripe at 1/κ = 100.
        assert_eq!(ripe, vec![2]);
        let ripe = r.ripe(10, 0.2, &|p| drains[&p], &mut |_| true, 10);
        assert_eq!(ripe, vec![2, 1]);
    }

    #[test]
    fn a_decayed_page_is_settled_and_still_found() {
        let mut r = Ripeness::default();
        let d = Drain { rho: 0.01, at: 0 };
        r.insert(7, 0.5, d, 0);
        // Much later the estimate is at the floor: index h(0.5) / R_MIN.
        let ripe = r.ripe(500, 0.01, &|_| d, &mut |_| true, 10);
        assert_eq!(ripe, vec![7]);
        assert!(r.at[&7].0, "settled");
    }
}
