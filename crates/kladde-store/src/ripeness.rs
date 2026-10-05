//! Cleaning by ripeness (`drafts/ripeness.md`, as of kladde-docs commit
//! `7c343b7`): how much of each page still drains and how fast, fitted to
//! the page's own losses, and the pages ranked by the price of space at
//! which cleaning them starts to pay.
//!
//! A page's live content is a share `a` that drains at rate `r` over a share
//! `s` that does not drain at all, both measured relative to the fill `u₀`
//! that survivors are packed at. The page is ripe at price `κ` once
//! `h(z) ≥ r / κ`, with `z = a/(1 − s)` and `h(x) = (1 − x)/x − ln(1/x)`.
//! Its ripeness index, `min(h(z)/r, h(x)/R_MIN)` with `x = a + s`, is the
//! `1/κ` at which that happens, and no page at or above `u₀` is ripe.
//! Between a page's losses its rate decays by the same factor per epoch as
//! every other page's, so the order of pages changes only when a page's own
//! content does.

use std::cmp::Ordering;
use std::collections::{BTreeSet, BinaryHeap};

use crate::consts::MAX_PAGE_CONTENT;
use crate::hash::{IdMap, IdSet};

/// How fast a page's fit forgets old losses, per epoch.
pub const BETA: f64 = 0.1;
/// The slowest rate content is assumed to drain at, per epoch: no page is
/// riper than it would be if all of its live content drained at this rate.
pub const R_MIN: f64 = 1e-4;
/// The weight of a page's starting estimate, in epochs of its losses.
pub const N0: u64 = 10;
/// How much better, in statements' worth of log-likelihood, a static share
/// must explain a page's losses than one share draining alone.
pub const STATIC_EVIDENCE: f64 = 3.0;
/// The fastest rate a fit considers, per epoch.
const R_MAX: f64 = 3.0;
/// Bisection steps per fit, which pin a rate down to `R_MAX / 2^50`.
const STEPS: usize = 50;

/// What a page's losses say about it: the discounted sums that a draining
/// share is fitted to, and the fit.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Drain {
    /// Natural losses, in bytes, each discounted by `e^(−β·lag)`, as of
    /// epoch `at`.
    pub s0: f32,
    /// The same, each also weighted by its lag.
    pub s1: f32,
    /// The draining share's rate at epoch `at`, a fraction of it per epoch.
    pub rate: f32,
    /// The draining share, as a fraction of the page's coverage; the rest is
    /// static.
    pub share: f32,
    /// The epoch the sums are as of: the page's last natural loss, its
    /// writing, or the open that seeded it.
    pub at: u64,
}

impl Drain {
    /// A new page's estimate, written at epoch `at`: `fast` of its `live`
    /// bytes draining at rate `r0`, and the rest static. The start enters the
    /// sums as the losses it predicts for the page's first [`N0`] epochs,
    /// which lie ahead.
    pub fn start(fast: f64, r0: f64, live: f64, at: u64) -> Drain {
        let (s0, s1) = predicted(fast, r0, -(N0 as i64), -1, 0);
        Drain {
            s0: s0 as f32,
            s1: s1 as f32,
            rate: r0 as f32,
            share: if live > 0.0 {
                (fast / live).clamp(0.0, 1.0) as f32
            } else {
                0.0
            },
            at,
        }
    }

    /// The estimate of a page of which nothing is known but that it was
    /// written with `written` bytes, `past` epochs before `now`, and holds
    /// `live` bytes: all of it draining, at the one rate that takes `written`
    /// to `live` in `age` epochs. Its past enters the sums as watched, and
    /// seen to lose what that rate predicts, and so does the start of a page
    /// with all of `written` draining.
    pub fn seed(written: f64, live: f64, past: u64, age: f64, now: u64) -> Drain {
        let r = if live > 0.0 && written > live {
            (written / live).ln() / age
        } else {
            0.0
        };
        let p = past as i64;
        let (a0, a1) = predicted(written, r, 0, p - 1, p);
        let (b0, b1) = predicted(written, r, p - N0 as i64, p - 1, p);
        Drain {
            s0: (a0 + b0) as f32,
            s1: (a1 + b1) as f32,
            rate: r as f32,
            share: 1.0,
            at: now,
        }
    }

    /// A flush's fold superseded `bytes` of the page's content at `now`,
    /// leaving `live` bytes; the page was written `age` epochs ago, and the
    /// file's statements state `statement` bytes each on average, which is
    /// the unit its losses come in.
    pub fn lose(&mut self, bytes: u32, live: u32, age: u64, now: u64, statement: f64) {
        let d = now.saturating_sub(self.at) as f64;
        let decay = (-BETA * d).exp();
        let (s0, s1) = (self.s0 as f64, self.s1 as f64);
        let s1 = decay * (s1 + d * s0);
        let s0 = decay * s0 + bytes as f64;
        self.s0 = s0 as f32;
        self.s1 = s1 as f32;
        self.at = now;
        if live == 0 || s0 <= 0.0 {
            return;
        }
        let (live, age) = (live as f64, age.max(1) as f64);
        let (r, loss) = fit(s0, s1, age);
        let fast = if r > 1e-9 {
            loss / r.exp_m1()
        } else {
            f64::INFINITY
        };
        // A static share must earn its place against one share draining
        // alone, by what it adds to the log-likelihood.
        let (one, one_likelihood) = fit_one(s0, s1, age, live);
        let gain = s0 * loss.ln() + r * s1 - s0 - one_likelihood;
        if fast < live && gain >= STATIC_EVIDENCE * statement.max(1.0) {
            self.rate = r as f32;
            self.share = (fast / live) as f32;
        } else {
            self.rate = one as f32;
            self.share = 1.0;
        }
    }

    /// The draining share's rate at `now`: between losses, it decays as a
    /// discounted average of the losses would.
    pub fn rate(&self, now: u64) -> f64 {
        self.rate as f64 * (-BETA * now.saturating_sub(self.at) as f64).exp()
    }

    /// Consolidation moved content out of the page, leaving `keep` of it.
    /// Which share the content came from is unknown, so both shrink in
    /// proportion, and the rate stays where it was.
    pub fn shrink(&mut self, keep: f64) {
        self.s0 = (self.s0 as f64 * keep) as f32;
        self.s1 = (self.s1 as f64 * keep) as f32;
    }
}

/// `ln Σ_{k<n} e^(w·k)`, for `n ≥ 1`.
fn ln_geometric(w: f64, n: f64) -> f64 {
    if w.abs() < 1e-12 {
        n.ln()
    } else if w < 0.0 {
        (-(w * n).exp_m1()).ln() - (-w.exp_m1()).ln()
    } else {
        w * (n - 1.0) + (-(-w * n).exp_m1()).ln() - (-(-w).exp_m1()).ln()
    }
}

/// The mean of `k < n` under weights `e^(w·k)`.
fn mean_geometric(w: f64, n: f64) -> f64 {
    if (w * n).abs() < 1e-6 {
        (n - 1.0) / 2.0 + w * (n * n - 1.0) / 12.0
    } else {
        n / -(-w * n).exp_m1() - 1.0 / -(-w).exp_m1()
    }
}

/// The losses at each lag that a share draining at rate `r` predicts,
/// relative to what it loses now, summed with the discount: `ln E(r)`, and
/// the mean lag `E'(r)/E(r)`. The lags are those of the `age` epochs the page
/// has been watched, and those of the [`N0`] epochs its start stands for,
/// the page's first.
fn exposure(r: f64, age: f64) -> (f64, f64) {
    let w = r - BETA;
    let n0 = N0 as f64;
    let watched = ln_geometric(w, age);
    let start = w * (age - n0) + ln_geometric(w, n0);
    let top = watched.max(start);
    let ln_e = top + ((watched - top).exp() + (start - top).exp()).ln();
    let (p, q) = ((watched - ln_e).exp(), (start - ln_e).exp());
    let mean = p * mean_geometric(w, age) + q * (age - n0 + mean_geometric(w, n0));
    (ln_e, mean)
}

/// The maximum-likelihood fit of a falling loss rate, `ℓ·e^(r·lag)`, to the
/// sums: the rate at which the mean lag it predicts is the sums' own, and
/// the current loss `ℓ` in bytes per epoch.
fn fit(s0: f64, s1: f64, age: f64) -> (f64, f64) {
    let mean = s1 / s0;
    let (mut lo, mut hi) = (0.0, R_MAX);
    let r = if mean <= exposure(lo, age).1 {
        lo
    } else if mean >= exposure(hi, age).1 {
        hi
    } else {
        for _ in 0..STEPS {
            let mid = (lo + hi) / 2.0;
            if exposure(mid, age).1 < mean {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        (lo + hi) / 2.0
    };
    (r, s0 * (-exposure(r, age).0).exp())
}

/// The fit of one share draining alone, all `live` bytes of it, which loses
/// `live·(e^r − 1)` in the epoch just past: its rate, and its log-likelihood.
fn fit_one(s0: f64, s1: f64, age: f64, live: f64) -> (f64, f64) {
    // The log-likelihood is concave in the rate, so the sign of its slope
    // brackets the maximum. The slope's two terms are compared in logs where
    // both are positive, since the second can overflow.
    let rising = |r: f64| {
        let (ln_e, mean) = exposure(r, age);
        let up = s0 / -(-r).exp_m1() + s1;
        let bracket = r.exp() + r.exp_m1() * mean;
        if up > 0.0 && bracket > 0.0 {
            up.ln() > live.ln() + ln_e + bracket.ln()
        } else {
            up > live * ln_e.exp() * bracket
        }
    };
    let (mut lo, mut hi) = (1e-9, R_MAX);
    let r = if rising(hi) {
        hi
    } else {
        for _ in 0..STEPS {
            let mid = (lo + hi) / 2.0;
            if rising(mid) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        (lo + hi) / 2.0
    };
    let ln_loss = live.ln() + r.exp_m1().ln();
    let likelihood = s0 * ln_loss + r * s1 - (ln_loss + exposure(r, age).0).exp();
    (r, likelihood)
}

/// `(S0, S1)` of the losses that a share of `start` bytes draining at `r0`
/// predicts for the epochs at lags `lo..=hi`, as of `past` epochs after the
/// page was written: a lag of `k` is the page's epoch `past − k`.
fn predicted(start: f64, r0: f64, lo: i64, hi: i64, past: i64) -> (f64, f64) {
    if start <= 0.0 || r0 <= 0.0 || hi < lo {
        return (0.0, 0.0);
    }
    let w = r0 - BETA;
    let n = (hi - lo + 1) as f64;
    let ln_s0 = start.ln() + (-(-r0).exp_m1()).ln() - r0 * (past - 1) as f64
        + w * lo as f64
        + ln_geometric(w, n);
    let s0 = ln_s0.exp();
    (s0, s0 * (lo as f64 + mean_geometric(w, n)))
}

/// The fill survivors are packed at, in bytes: `u₀ = 1 − θ` of a page's
/// capacity, which packing promises for every page but a flush's last. A
/// page's fill counts relative to it, and no page at or above it is ripe,
/// since its survivors would take a whole page or more.
pub fn packed_fill(theta: f64) -> f64 {
    (1.0 - theta) * MAX_PAGE_CONTENT as f64
}

/// What keeping a page at relative fill `x` waiting costs, relative to what
/// its draining saves: the threshold `h(x*) = r / κ` of the draft.
pub fn h(x: f64) -> f64 {
    (1.0 - x) / x + x.ln()
}

/// The myopic rule's `g(x) = (1 − x)/x`, which the draft's ablation puts in
/// place of `h`: the space a cleaning frees per byte it writes, blind to
/// survivors that go on dying after they are moved.
pub fn g(x: f64) -> f64 {
    (1.0 - x) / x
}

/// The keys that rank a page of `live` bytes, fewer than `packed`, with
/// estimate `d`, its fill measured by `fill`, `h` or `g`: its key while its
/// index lies below its floor, if anything on it drains, and its floor's.
pub fn keys(d: &Drain, live: f64, packed: f64, fill: fn(f64) -> f64) -> (Option<f64>, f64) {
    let floor = fill(live / packed).ln() - R_MIN.ln();
    let fast = d.share as f64 * live;
    let key = (fast > 0.0 && d.rate > 0.0).then(|| {
        let z = fast / (packed - (live - fast));
        fill(z).ln() - (d.rate as f64).ln() - BETA * d.at as f64
    });
    (key, floor)
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

/// The pages that could be ripe -- live data pages and leaves emptier than
/// `u₀` -- ordered by their ripeness index.
#[derive(Debug, Default)]
pub struct Ripeness {
    /// `K = ln h(z) − ln rate − β·at`: while the index lies below its floor,
    /// the page's log-index is `K + β·now`.
    draining: BTreeSet<(Ln, u32)>,
    /// `ln h(x) − ln R_MIN`: the log-index of a page at its floor, constant.
    settled: BTreeSet<(Ln, u32)>,
    /// Where each ranked page is: whether settled, its key, and its floor's.
    at: IdMap<(bool, Ln, Ln)>,
    /// Pages whose fill, state, or estimate changed since they were ranked.
    pub stale: IdSet,
}

impl Ripeness {
    pub fn remove(&mut self, page: u32) {
        if let Some((settled, k, _)) = self.at.remove(&page) {
            if settled {
                self.settled.remove(&(k, page));
            } else {
                self.draining.remove(&(k, page));
            }
        }
    }

    /// Ranks `page` by its `key` and its `floor`'s, as [`keys`] gives them,
    /// as of `now`.
    pub fn insert(&mut self, page: u32, key: Option<f64>, floor: f64, now: u64) {
        self.remove(page);
        let floor = Ln(floor);
        match key.filter(|k| k + BETA * (now as f64) < floor.0) {
            Some(k) => {
                self.draining.insert((Ln(k), page));
                self.at.insert(page, (false, Ln(k), floor));
            }
            None => {
                self.settled.insert((floor, page));
                self.at.insert(page, (true, floor, floor));
            }
        }
    }

    /// How many pages are ranked.
    pub fn len(&self) -> usize {
        self.at.len()
    }

    /// Up to `limit` pages that are ripe at price `kappa` and pass `ok`,
    /// highest index first. A draining page whose index has passed its floor
    /// is overstated by its key, so it can surface early but never hide
    /// further down, and it is settled on the way.
    pub fn ripe(
        &mut self,
        now: u64,
        kappa: f64,
        ok: &mut dyn FnMut(u32) -> bool,
        limit: usize,
    ) -> Vec<u32> {
        let threshold = -kappa.ln();
        let bn = BETA * now as f64;
        let mut out = Vec::new();
        let mut settle = Vec::new();
        {
            let at = &self.at;
            let mut d = self.draining.iter().rev().peekable();
            let mut s = self.settled.iter().rev().peekable();
            // Pages settled on the way, at their floors.
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
                if index < threshold {
                    break;
                }
                match from {
                    0 => {
                        let &(k, _) = d.next().unwrap();
                        let floor = at[&p].2;
                        if index >= floor.0 {
                            settle.push((p, k, floor));
                            late.push((floor, p));
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
        for (p, k, floor) in settle {
            self.draining.remove(&(k, p));
            self.settled.insert((floor, p));
            self.at.insert(p, (true, floor, floor));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: f64 = 4096.0;

    /// The index of a page of `live` bytes with estimate `d` at `now`, fills
    /// relative to `packed`.
    fn index(d: &Drain, live: f64, packed: f64, now: u64) -> f64 {
        let (key, floor) = keys(d, live, packed, h);
        key.map_or(floor, |k| (k + BETA * now as f64).min(floor))
            .exp()
    }

    /// A page holding `shares` of `(fill, rate)`, starting from `start`'s
    /// `(fast, rate)`, that loses exactly what its shares predict each epoch;
    /// its estimate and live bytes at each epoch up to `epochs`.
    fn drain_exactly(
        shares: &[(f64, f64)],
        start: (f64, f64),
        statement: f64,
        epochs: u64,
    ) -> Vec<(Drain, f64)> {
        let live = |t: u64| {
            shares
                .iter()
                .map(|&(a, r)| a * (-r * t as f64).exp())
                .sum::<f64>()
                * PAGE
        };
        let mut d = Drain::start(start.0 * PAGE, start.1, live(0), 0);
        let mut out = vec![(d, live(0))];
        for t in 1..=epochs {
            let lost = (live(t - 1) - live(t)).round() as u32;
            d.lose(lost, live(t).round() as u32, t, t, statement);
            out.push((d, live(t)));
        }
        out
    }

    /// The `x` at which `f(x)` falls to `ratio`, for a decreasing `f`.
    fn threshold(f: fn(f64) -> f64, ratio: f64) -> f64 {
        let (mut lo, mut hi) = (1e-9, 1.0 - 1e-9);
        for _ in 0..100 {
            let mid = (lo + hi) / 2.0;
            if f(mid) > ratio {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }

    #[test]
    fn the_thresholds_match_the_drafts_table() {
        // `x*` for `r / κ`, and where the myopic rule would clean instead,
        // from `drafts/ripeness.md`.
        for (ratio, x, myopic) in [
            (0.001, 0.96, 0.999),
            (0.01, 0.87, 0.99),
            (0.1, 0.66, 0.91),
            (1.0, 0.32, 0.5),
            (10.0, 0.07, 0.09),
        ] {
            let (xh, xg) = (threshold(h, ratio), threshold(g, ratio));
            assert!((xh - x).abs() < 0.01, "r/κ = {ratio}: x* = {xh}");
            assert!((xg - myopic).abs() < 0.005, "r/κ = {ratio}: myopic {xg}");
        }
    }

    #[test]
    fn the_fit_follows_the_drafts_example() {
        // `drafts/ripeness.md`: 0.3 of the page dying at 0.5, 0.3 at 0.02,
        // 0.4 static, from the draft's start of 0.6 at 0.26, with statements
        // of 144 bytes. The draft's tested fit reads 40, 6, and 41, from a
        // simulation that fits rates on a grid of steps of 0.001; solved
        // exactly, the last is 43.
        let shares = [(0.3, 0.5), (0.3, 0.02), (0.4, 0.0)];
        let run = drain_exactly(&shares, (0.6, 0.26), 144.0, 50);
        for (t, want) in [(10, 40.0), (20, 6.0), (50, 43.0)] {
            let (d, live) = run[t];
            let got = index(&d, live, PAGE, t as u64);
            assert!(
                (got / want).ln().abs() < 0.05,
                "epoch {t}: {got}, not {want}"
            );
        }
        // While the fast share dies, the fit keeps most of the page static.
        let (d, live) = run[10];
        let stat = (1.0 - d.share as f64) * live / PAGE;
        assert!((stat - 0.62).abs() < 0.01, "static share {stat}");
        // Once it has died, the few losses cannot earn a static share.
        assert_eq!(run[20].0.share, 1.0);
    }

    #[test]
    fn a_static_share_is_found_where_losses_are_plentiful() {
        // 0.3 draining at 0.1 over 0.7 static: in small statements, the
        // losses soon show the fall, and the fit finds both exactly.
        let shares = [(0.3, 0.1), (0.7, 0.0)];
        let (d, live) = drain_exactly(&shares, (0.3, 0.1), 4.0, 20)[20];
        let stat = (1.0 - d.share as f64) * live / PAGE;
        assert!((stat - 0.7).abs() < 0.005, "static share {stat}");
        assert!((d.rate - 0.1).abs() < 0.002, "rate {}", d.rate);
        // In statements of 144 bytes, the same losses do not earn it: the
        // page is taken for one share draining slowly throughout.
        let (d, _) = drain_exactly(&shares, (0.3, 0.1), 144.0, 20)[20];
        assert_eq!(d.share, 1.0);
        assert!(d.rate < 0.02, "rate {}", d.rate);
    }

    #[test]
    fn a_page_losing_a_steady_share_fits_one_share_at_that_rate() {
        // From a start at a fifth of the true rate, the fit converges.
        let run = drain_exactly(&[(1.0, 0.05)], (1.0, 0.01), 144.0, 40);
        let (d, _) = run[40];
        assert_eq!(d.share, 1.0);
        assert!((d.rate - 0.05).abs() < 0.005, "rate {}", d.rate);
    }

    #[test]
    fn the_seed_starts_from_the_one_rate_of_the_pages_life() {
        // Written full at epoch 71, half left at the open at epoch 100: all
        // of it draining at ln 2 / 30, 30 epochs until the session's first
        // flush, and a first loss at that rate keeps it there.
        let rate = 2f64.ln() / 30.0;
        let mut d = Drain::seed(PAGE, PAGE / 2.0, 29, 30.0, 100);
        assert_eq!(d.share, 1.0);
        assert!((d.rate as f64 - rate).abs() < 1e-6);
        let lost = PAGE / 2.0 * -(-rate).exp_m1();
        d.lose(lost as u32, (PAGE / 2.0 - lost) as u32, 30, 101, 144.0);
        assert!((d.rate as f64 - rate).abs() < 0.1 * rate, "rate {}", d.rate);
    }

    #[test]
    fn moving_content_out_leaves_the_fit_alone() {
        let mut d = Drain::start(PAGE, 0.05, PAGE, 0);
        d.lose(200, 3896, 1, 1, 144.0);
        let mut moved = d;
        moved.shrink(0.5);
        let (mut a, mut b) = (d, moved);
        a.lose(190, 3706, 2, 2, 144.0);
        b.lose(95, 1853, 2, 2, 144.0);
        assert!(
            (a.rate - b.rate).abs() < 1e-4,
            "{} against {}",
            a.rate,
            b.rate
        );
        assert_eq!(a.share, b.share);
    }

    #[test]
    fn ripe_pages_come_highest_index_first() {
        let packed = PAGE;
        let rank = |r: &mut Ripeness, p: u32, d: Drain, fill: f64| {
            let (key, floor) = keys(&d, fill * packed, packed, h);
            r.insert(p, key, floor, 10);
        };
        let mut r = Ripeness::default();
        // Hot at 0.3: index h(0.3) / 0.1 = 11. Nothing draining at 0.8:
        // at its floor, h(0.8) / R_MIN = 270. Hot at 0.9: 0.06.
        let hot = Drain {
            rate: 0.1,
            share: 1.0,
            at: 10,
            ..Drain::default()
        };
        rank(&mut r, 1, hot, 0.3);
        rank(&mut r, 2, Drain::default(), 0.8);
        rank(&mut r, 3, hot, 0.9);
        assert_eq!(r.ripe(10, 0.01, &mut |_| true, 10), vec![2]);
        assert_eq!(r.ripe(10, 0.2, &mut |_| true, 10), vec![2, 1]);
    }

    #[test]
    fn a_page_whose_index_passes_its_floor_is_settled_and_still_found() {
        let mut r = Ripeness::default();
        let d = Drain {
            rate: 0.01,
            share: 1.0,
            at: 0,
            ..Drain::default()
        };
        let (key, floor) = keys(&d, PAGE / 2.0, PAGE, h);
        r.insert(7, key, floor, 0);
        assert!(!r.at[&7].0, "draining");
        // Much later its index would pass the floor, h(0.5) / R_MIN.
        assert_eq!(r.ripe(500, 0.01, &mut |_| true, 10), vec![7]);
        assert!(r.at[&7].0, "settled");
    }

    #[test]
    fn a_static_share_ranks_its_page_riper() {
        // Two pages at 0.8 that lose alike: 0.1 draining at r over 0.7
        // static, and all of it draining at r/8. The draft ranks the first
        // about four times higher: h(1/3) / (8 h(0.8)) = 4.2.
        let r = 0.02;
        let first = Drain {
            rate: r as f32,
            share: 0.125,
            at: 0,
            ..Drain::default()
        };
        let second = Drain {
            rate: (r / 8.0) as f32,
            share: 1.0,
            at: 0,
            ..Drain::default()
        };
        let (a, b) = (
            index(&first, 0.8 * PAGE, PAGE, 0),
            index(&second, 0.8 * PAGE, PAGE, 0),
        );
        assert!((a / b - 4.2).abs() < 0.01, "{a} against {b}");
    }
}
