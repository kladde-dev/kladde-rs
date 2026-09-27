//! Cleaning by ripeness, Bayesian (`drafts/bayesian-ripeness.md`, as of
//! kladde-docs commit `369ae0e`, on its branch `ripeness2`): a posterior over
//! how much of each page still drains and how fast, and the pages ranked by
//! the price of space at which cleaning them starts to pay.
//!
//! A page's *chunks* are what it holds of its statements: each `Ref`'s
//! payload on a data page, each statement on a leaf. A chunk drains with
//! probability `π`, else it is static; a draining chunk loses its bytes at
//! the rate `r`, in loss events whose sizes have the dispersion
//! `σ = E[s²]/E[s]`. A chunk that has lost bytes is known to drain, and the
//! posterior is a mixture over `j`, how many of the `n` untouched chunks drain
//! too, each component with a Gamma posterior on the rate. All evidence is
//! discounted by `e^(−β)` per epoch, the prior's drift.
//!
//! Fills count relative to the fill `u₀` survivors are packed at. Cleaning a
//! page of fill `x` whose draining share `a` drains at `r` now rather than one
//! epoch later gains `κ·[(1 − x) − a·φ(r/κ)]` per epoch, with `φ` solving
//! `φ − ln(1 + φ) = y`; a page's index is the `1/κ` at which its expected
//! gain turns positive, rule (a), or its gain net of what the option to wait
//! is worth, rule (c′). Between a page's losses its index grows by the same
//! factor per epoch as every other page's, so the order of pages changes only
//! when a page's own content does.

use std::cmp::Ordering;
use std::collections::{BTreeSet, BinaryHeap};
use std::sync::OnceLock;

use crate::consts::MAX_PAGE_CONTENT;
use crate::hash::{IdMap, IdSet};
pub use crate::options::RipenessRule as Rule;

/// The prior's drift: how fast a page's posterior forgets its evidence, per
/// epoch.
pub const BETA: f64 = 0.1;
/// The slowest rate content is assumed to drain at, per epoch: no page is
/// riper than it would be if all of its live content drained at this rate.
pub const R_MIN: f64 = 1e-4;
/// The weight of the prior on a chunk's class, in chunks.
pub const NU_PI: f64 = 10.0;
/// The least shape a posterior is taken at.
const A_MIN: f64 = 0.05;
/// The range of `ln κ` an index is sought in.
const LN_KAPPA: (f64, f64) = (-24.0, 12.0);
/// Bisection steps for an index, which pin `ln κ` down to `36 / 2^32`.
const STEPS: usize = 32;

/// `e^(−β)`: what one epoch discounts evidence by.
fn delta() -> f64 {
    (-BETA).exp()
}

/// The discounted age of a byte live for `t` epochs: its exposure, as the
/// posterior remembers it.
pub fn remembered(t: f64) -> f64 {
    let d = delta();
    (1.0 - d.powf(t)) / (1.0 - d)
}

/// How many epochs the posterior remembers: `1/(1 − e^(−β))`.
pub fn memory() -> f64 {
    1.0 / (1.0 - delta())
}

/// The dispersion loss events are taken at before any has been seen, in
/// bytes.
pub const SIGMA0: f64 = 64.0;

/// A Gamma prior on the rate, in loss events: `a` events over an exposure of
/// `b`, which has mean `a/b`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Prior {
    pub a: f64,
    pub b: f64,
}

impl Default for Prior {
    /// The prior before the file has any evidence: next to none, so that a
    /// page's first losses decide.
    fn default() -> Prior {
        Prior { a: A_MIN, b: 1.0 }
    }
}

/// The sums of the method of moments over pages' losses in their first
/// epochs, discounted, from which the file's empirical prior follows: pages,
/// their loss events `k`, their exposures `E` in events, and `E²` and `k²/E`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Moments {
    pub n: f64,
    pub k: f64,
    pub e: f64,
    pub e2: f64,
    pub k2e: f64,
}

impl Moments {
    /// Adds a page that lost `k` events over an exposure of `e`.
    pub fn add(&mut self, k: f64, e: f64) {
        if e > 0.0 {
            self.n += 1.0;
            self.k += k;
            self.e += e;
            self.e2 += e * e;
            self.k2e += k * k / e;
        }
    }

    pub fn discount(&mut self, by: f64) {
        for v in [
            &mut self.n,
            &mut self.k,
            &mut self.e,
            &mut self.e2,
            &mut self.k2e,
        ] {
            *v *= by;
        }
    }

    /// The Gamma prior with the mean and the spread of the pages' rates, the
    /// spread being what is left of their losses' scatter once the Poisson
    /// noise is taken out, floored at a thousandth of the mean squared; once
    /// two pages' worth are in.
    pub fn prior(&self) -> Option<Prior> {
        if self.n < 2.0 || self.e <= 0.0 {
            return None;
        }
        let mean = (self.k / self.e).max(R_MIN);
        let q = self.k2e - 2.0 * mean * self.k + mean * mean * self.e;
        let over = self.e - self.e2 / self.e;
        let spread = if over > 0.0 {
            (q - (self.n - 1.0) * mean) / over
        } else {
            0.0
        };
        let spread = spread.max(mean * mean / 1000.0);
        Some(Prior {
            a: mean * mean / spread,
            b: mean / spread,
        })
    }
}

/// What a page's losses say about it, as sums that the posterior follows
/// from, and what the ranking last found.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Drain {
    /// The rate's shape: loss events, bytes lost over the dispersion, with
    /// the prior's as pseudo-observations, discounted.
    pub a: f32,
    /// The rate's scale: the exposure of the bytes known to drain, those of
    /// chunks that have lost bytes since the page was written, in events,
    /// with the prior's, discounted.
    pub b: f32,
    /// Chunks known to drain, discounted.
    pub drained: f32,
    /// The prior on a chunk's class, in chunks, a multiple of one half: `p0`
    /// draining, and `NU_PI + 1 − p0` static.
    pub p0: f32,
    /// The posterior mean draining share, as a fraction of the coverage, as
    /// the ranking last found it; moved content brings it along.
    pub share: f32,
    /// The epoch the sums are as of: the page's last natural loss, its
    /// writing, or the open that seeded it.
    pub at: u64,
}

/// A page's natural losses in one flush, as the fold records them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Loss {
    /// Bytes lost.
    pub lost: u32,
    /// The page's coverage before the first of them.
    pub live: u32,
    /// The live bytes of chunks touched before the flush: known to drain.
    pub known_live: u32,
    /// Chunks the flush touched first, and their bytes when written.
    pub new: u32,
    pub new_bytes: u32,
}

/// What the page table knows of a page's chunks, beside its estimate.
#[derive(Clone, Copy, Debug)]
pub struct View {
    /// Live bytes.
    pub live: f64,
    /// Chunks that have not lost a byte, and their bytes.
    pub untouched: u32,
    pub untouched_bytes: f64,
    /// The epoch the page was written.
    pub written: u64,
}

/// A page's posterior, as of its estimate's epoch: the rate's shape, shared,
/// and per number `j` of untouched chunks that drain, the rate's scale, the
/// weight, and the draining share in bytes.
struct Mixture {
    shape: f64,
    scale: Vec<f64>,
    weight: Vec<f64>,
    draining: Vec<f64>,
}

impl Drain {
    /// A new page's estimate, written at epoch `at`: the rate's prior, and the
    /// share `pi` of what it holds that drains.
    pub fn start(prior: Prior, pi: f64, at: u64) -> Drain {
        let pi = pi.clamp(0.0, 1.0);
        Drain {
            a: prior.a as f32,
            b: prior.b as f32,
            p0: ((2.0 * NU_PI * pi).round() / 2.0 + 0.5) as f32,
            share: pi as f32,
            at,
            ..Drain::default()
        }
    }

    /// The prior's count of static chunks.
    pub fn q0(&self) -> f64 {
        NU_PI + 1.0 - self.p0 as f64
    }

    /// The estimate of a page of which nothing is known but that it was
    /// written with `written` bytes, `past` epochs before `now`, holds `live`
    /// bytes, and has `touched` chunks that have lost bytes, first
    /// `touched_bytes` long, with loss events of dispersion `sigma`: its past
    /// counts as watched, losing bytes at the one rate that takes `written`
    /// to `live` in `age` epochs, and all of it drains.
    #[allow(clippy::too_many_arguments)]
    pub fn seed(
        written: f64,
        live: f64,
        past: u64,
        age: f64,
        sigma: f64,
        touched: u32,
        touched_bytes: f64,
        now: u64,
    ) -> Drain {
        let r = if live > 0.0 && written > live {
            (written / live).ln() / age
        } else {
            R_MIN
        };
        // The bytes it would have held k epochs ago, discounted by k.
        let d = delta();
        let b: f64 = (0..past.max(1))
            .map(|k| d.powi(k as i32) * live * (r * k as f64).exp())
            .sum::<f64>()
            / sigma;
        let t = remembered(past as f64);
        Drain {
            a: (r * b) as f32,
            b: (b + touched_bytes * t / sigma) as f32,
            drained: touched as f32,
            p0: (NU_PI + 0.5) as f32,
            share: 1.0,
            at: now,
        }
    }

    /// Ages the sums to epoch `to` through epochs without a loss, in which
    /// `known_live` events' worth of bytes known to drain stayed live.
    fn quiet(&mut self, to: u64, known_live: f64) {
        if to <= self.at {
            return;
        }
        let d = delta();
        let decay = d.powf((to - self.at) as f64);
        self.a = (decay * self.a as f64) as f32;
        self.b = (decay * self.b as f64 + known_live * (1.0 - decay) / (1.0 - d)) as f32;
        self.drained = (decay * self.drained as f64) as f32;
        self.at = to;
    }

    /// The flush of epoch `now` recorded `loss` on the page, written at epoch
    /// `written`; loss events on pages of its kind have the dispersion
    /// `sigma`, in bytes. A second record of the same flush adds to the
    /// first.
    pub fn lose(&mut self, loss: &Loss, written: u64, sigma: f64, now: u64) {
        let d = delta();
        if self.at < now {
            let known_live = loss.known_live as f64 / sigma;
            self.quiet(now - 1, known_live);
            self.a = (d * self.a as f64) as f32;
            self.b = (d * self.b as f64 + known_live) as f32;
            self.drained = (d * self.drained as f64) as f32;
            self.at = now;
        }
        // Chunks touched first are known to drain since the page's writing.
        let before = remembered(now.saturating_sub(1).saturating_sub(written) as f64);
        let (lost, new) = (loss.lost as f64 / sigma, loss.new_bytes as f64 / sigma);
        self.b = (self.b as f64 + new * (d * before + 1.0) - lost / 2.0).max(0.0) as f32;
        self.a += lost as f32;
        self.drained += loss.new as f32;
    }

    /// The posterior of a page `v`, as of the estimate's epoch.
    fn mixture(&self, v: &View, sigma: f64) -> Mixture {
        let n = v.untouched;
        let chunk = if n > 0 {
            v.untouched_bytes / n as f64
        } else {
            0.0
        };
        let known_live = (v.live - v.untouched_bytes).max(0.0);
        let t = remembered(self.at.saturating_sub(v.written) as f64);
        let shape = (self.a as f64).max(A_MIN);
        let base = self.b as f64;
        let per = chunk * t / sigma;
        let drained = self.drained as f64;
        let (p0, q0) = (self.p0 as f64, self.q0());
        let nf = n as f64;
        let ln_choose_n = ln_gamma(nf + 1.0);
        let mut ln_w = Vec::with_capacity(n as usize + 1);
        let mut scale = Vec::with_capacity(n as usize + 1);
        let mut draining = Vec::with_capacity(n as usize + 1);
        for j in 0..=n {
            let jf = j as f64;
            let b = (base + jf * per).max(1e-300);
            ln_w.push(
                ln_choose_n - ln_gamma(jf + 1.0) - ln_gamma(nf - jf + 1.0)
                    + ln_beta(p0 + drained + jf, q0 + nf - jf)
                    - shape * b.ln(),
            );
            scale.push(b);
            draining.push((known_live + jf * chunk).min(v.live));
        }
        let top = ln_w.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mut weight: Vec<f64> = ln_w.iter().map(|l| (l - top).exp()).collect();
        let sum: f64 = weight.iter().sum();
        for w in &mut weight {
            *w /= sum;
        }
        // Components that carry no weight cost time and nothing else.
        let keep: Vec<usize> = (0..weight.len()).filter(|&i| weight[i] > 1e-9).collect();
        Mixture {
            shape,
            scale: keep.iter().map(|&i| scale[i]).collect(),
            weight: keep.iter().map(|&i| weight[i]).collect(),
            draining: keep.iter().map(|&i| draining[i]).collect(),
        }
    }

    /// The draining share's rate, by the posterior mean of the component in
    /// which every untouched chunk drains: what the single-rate posterior
    /// would say. For tests and diagnostics.
    pub fn mean_rate(&self, v: &View, sigma: f64) -> f64 {
        let t = remembered(self.at.saturating_sub(v.written) as f64);
        let b = self.b as f64 + v.untouched_bytes * t / sigma;
        (self.a as f64).max(A_MIN) / b.max(1e-300)
    }
}

/// The log-index of page `v` with estimate `d` by `rule`, fills relative to
/// `packed` bytes, with loss events of dispersion `sigma` bytes, and the
/// posterior mean draining share as a fraction of the page's live bytes.
/// [`Rule::ExpectedGain`] is rule (a): the `1/κ` at which the expected gain of
/// cleaning now turns positive; [`Rule::OptionToWait`] is rule (c′): the
/// `1/κ` at which the gain of cleaning now, by the posterior means, covers
/// what the option to wait is worth.
pub fn ln_index(d: &Drain, v: &View, packed: f64, sigma: f64, rule: Rule) -> (f64, f64) {
    let m = d.mixture(v, sigma);
    let x = v.live / packed;
    let mean_share: f64 = m.weight.iter().zip(&m.draining).map(|(w, a)| w * a).sum();
    let share = if v.live > 0.0 {
        (mean_share / v.live).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let ln = match rule {
        Rule::ExpectedGain => {
            let table = phi_table();
            let gain = |lk: f64| {
                let kappa = lk.exp();
                let wait: f64 = (0..m.weight.len())
                    .map(|i| {
                        m.weight[i] * m.draining[i] / packed * table.at(m.shape, m.scale[i] * kappa)
                    })
                    .sum();
                (1.0 - x) - wait
            };
            -root(gain)
        }
        Rule::OptionToWait => {
            let rate: f64 = (0..m.weight.len())
                .map(|i| m.weight[i] * m.shape / m.scale[i])
                .sum();
            option_ln_index(m.shape, rate, mean_share / packed, x, sigma / packed)
        }
    };
    (ln, share)
}

/// The `ln κ` at which `f`, rising in `ln κ`, turns positive, within
/// [`LN_KAPPA`].
fn root(f: impl Fn(f64) -> f64) -> f64 {
    let (mut lo, mut hi) = LN_KAPPA;
    if f(lo) >= 0.0 {
        return lo;
    }
    if f(hi) < 0.0 {
        return hi;
    }
    for _ in 0..STEPS {
        let mid = (lo + hi) / 2.0;
        if f(mid) >= 0.0 {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    (lo + hi) / 2.0
}

/// Rule (c′): the log of the `1/κ` at which cleaning now gains, per epoch, at
/// least what the option to wait is worth, all by the posterior means: the
/// gain the page would forgo if the losses of the posterior's memory, drawn
/// from the negative-binomial predictive, left it unripe. The draining share
/// `a` drains at `rate` and a posterior resting on `shape` events; after `H`
/// epochs and `J` events of `sigma` each, the posterior is discounted by
/// `e^(−β·H)`, with the events and `H` epochs' exposure added. Fills
/// relative to `u₀`.
fn option_ln_index(shape: f64, rate: f64, a: f64, x: f64, sigma: f64) -> f64 {
    let h = memory();
    let d = delta();
    let decay = d.powf(h);
    let kept = (1.0 - decay) / (h * (1.0 - d));
    let scale = shape / rate.max(1e-300);
    let top = (a / sigma).floor().max(0.0) as usize;
    let exposure = a / sigma * h;
    let p = (scale / (scale + exposure)).min(1.0 - 1e-16);
    let mut weight = Vec::with_capacity(top + 1);
    let mut later = Vec::with_capacity(top + 1);
    for j in 0..=top {
        let jf = j as f64;
        let ln_p = ln_gamma(shape + jf) - ln_gamma(shape) - ln_gamma(jf + 1.0)
            + shape * p.ln()
            + jf * (-p).ln_1p();
        let aj = (a - jf * sigma).max(0.0);
        let shape_j = decay * shape + jf * kept;
        let scale_j = decay * scale + (a - jf * sigma / 2.0).max(0.0) * h * kept / sigma;
        weight.push(ln_p.exp());
        later.push((aj, x - (a - aj), shape_j / scale_j.max(1e-300)));
    }
    let sum: f64 = weight.iter().sum();
    if sum > 0.0 {
        for w in &mut weight {
            *w /= sum;
        }
    }
    let table = phi_table();
    let gain = |lk: f64| {
        let kappa = lk.exp();
        let now = (1.0 - x) - a * table.phi(rate / kappa);
        let option: f64 = weight
            .iter()
            .zip(&later)
            .map(|(w, &(aj, xj, rj))| w * (-((1.0 - xj) - aj * table.phi(rj / kappa))).max(0.0))
            .sum();
        now - option
    };
    -root(gain)
}

// ------------------------------------------------------------------ tables

/// `φ` and `Φ_A(m) = E φ(G/m)`, `G ~ Gamma(A, 1)`, tabulated once.
struct PhiTable {
    /// `ln φ` on a grid of `ln y`.
    ln_phi: Vec<f64>,
    /// `Φ`, by `ln A`, then `ln m`.
    big: Vec<f64>,
}

const LN_Y: (f64, f64, usize) = (-30.0, 12.0, 4201);
const LN_A: (f64, f64, usize) = (-3.0, 11.6, 147);
const LN_M: (f64, f64, usize) = (-24.0, 24.0, 481);

fn grid(g: (f64, f64, usize), i: usize) -> f64 {
    g.0 + (g.1 - g.0) * i as f64 / (g.2 - 1) as f64
}

/// Where `v` falls on grid `g`: the cell and the fraction into it.
fn locate(g: (f64, f64, usize), v: f64) -> (usize, f64) {
    let f = ((v - g.0) / (g.1 - g.0) * (g.2 - 1) as f64).clamp(0.0, (g.2 - 1) as f64 - 1e-9);
    (f as usize, f - f.floor())
}

/// `φ(y)`, solving `φ − ln(1 + φ) = y` by Newton's method.
fn phi_exact(y: f64) -> f64 {
    if y <= 0.0 {
        return 0.0;
    }
    if y < 1e-10 {
        return (2.0 * y).sqrt();
    }
    let mut p = if y < 1.0 {
        (2.0 * y).sqrt() + 2.0 * y / 3.0
    } else {
        y + (1.0 + y).ln()
    };
    for _ in 0..50 {
        let f = p - p.ln_1p() - y;
        let step = f * (1.0 + p) / p;
        p = (p - step).max(p / 2.0);
        if step.abs() <= 1e-14 * p {
            break;
        }
    }
    p
}

impl PhiTable {
    /// Integrates over `ln G`, whose density is `e^(A·t − e^t)/Γ(A)`, with
    /// 160 points where the integrand peaks and 40 to its left, where a
    /// small shape spreads `ln G` over some `1/A`: to within 0.4 % of the
    /// same with 25 times the points.
    fn build() -> PhiTable {
        let (tail, bulk) = (40, 160);
        let ln_phi: Vec<f64> = (0..LN_Y.2)
            .map(|i| phi_exact(grid(LN_Y, i).exp()).ln())
            .collect();
        let mut t = PhiTable {
            ln_phi,
            big: Vec::new(),
        };
        let mut big = vec![0.0; LN_A.2 * LN_M.2];
        for ia in 0..LN_A.2 {
            let shape = grid(LN_A, ia).exp();
            let lo = shape.ln() - 10.0 / shape.sqrt() - 40.0 / shape;
            let hi = (shape + 40.0 + 10.0 * shape.sqrt()).ln();
            let cut = (shape.ln().min(0.0) - 8.0).clamp(lo, hi);
            let mut ts: Vec<f64> = (0..tail)
                .map(|k| lo + (cut - lo) * k as f64 / tail as f64)
                .collect();
            ts.extend((0..bulk).map(|k| cut + (hi - cut) * k as f64 / (bulk - 1) as f64));
            let ln_density: Vec<f64> = ts
                .iter()
                .map(|&tk| shape * tk - tk.exp() - ln_gamma(shape))
                .collect();
            for im in 0..LN_M.2 {
                let ln_m = grid(LN_M, im);
                let f: Vec<f64> = ts
                    .iter()
                    .zip(&ln_density)
                    .map(|(&tk, &ld)| ld + t.ln_phi_at(tk - ln_m))
                    .collect();
                // Exact where the integrand is exponential between points, as
                // it nearly is in the tail; and beyond `lo`, where it falls as
                // `e^((A + 1/2)·t)`.
                let mut e = f[0].exp() / (shape + 0.5);
                for k in 1..ts.len() {
                    let (h, d) = (ts[k] - ts[k - 1], f[k] - f[k - 1]);
                    e += if d.abs() < 1e-6 {
                        h * (f[k].exp() + f[k - 1].exp()) / 2.0
                    } else {
                        h * (f[k].exp() - f[k - 1].exp()) / d
                    };
                }
                big[ia * LN_M.2 + im] = e;
            }
        }
        t.big = big;
        t
    }

    /// `φ(y)`, from the table, and from its asymptotes beyond it.
    fn phi(&self, y: f64) -> f64 {
        if y <= 0.0 {
            return 0.0;
        }
        self.ln_phi_at(y.ln()).exp()
    }

    /// `ln φ(e^ly)`, which stays finite where `e^ly` underflows.
    fn ln_phi_at(&self, ly: f64) -> f64 {
        if ly < LN_Y.0 {
            return 0.5 * (std::f64::consts::LN_2 + ly);
        }
        if ly > LN_Y.1 {
            let y = ly.exp();
            return (y + y.ln_1p()).ln();
        }
        let (i, f) = locate(LN_Y, ly);
        self.ln_phi[i] * (1.0 - f) + self.ln_phi[i + 1] * f
    }

    /// `Φ_A(m) = E φ(G/m)`, `G ~ Gamma(A, 1)`: from the table, and from its
    /// asymptotes in `m` beyond it.
    fn at(&self, shape: f64, m: f64) -> f64 {
        let (la, lm) = (shape.max(A_MIN).ln(), m.max(1e-300).ln());
        if lm > LN_M.1 {
            // E √(2G/m)
            return (2.0 / m).sqrt() * (ln_gamma(shape + 0.5) - ln_gamma(shape)).exp();
        }
        if lm < LN_M.0 {
            return shape / m + (shape / m).ln_1p();
        }
        let (ia, fa) = locate(LN_A, la);
        let (im, fm) = locate(LN_M, lm);
        let v = |a: usize, b: usize| self.big[a * LN_M.2 + b];
        let lo = v(ia, im) * (1.0 - fm) + v(ia, im + 1) * fm;
        let hi = v(ia + 1, im) * (1.0 - fm) + v(ia + 1, im + 1) * fm;
        lo * (1.0 - fa) + hi * fa
    }
}

fn phi_table() -> &'static PhiTable {
    static TABLE: OnceLock<PhiTable> = OnceLock::new();
    TABLE.get_or_init(PhiTable::build)
}

/// `ln Γ(x)`, for `x > 0`, by Lanczos's approximation.
pub fn ln_gamma(x: f64) -> f64 {
    const G: f64 = 7.0;
    const C: [f64; 9] = [
        0.999_999_999_999_809_9,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    if x < 0.5 {
        // Reflection: Γ(x) Γ(1 − x) = π / sin(πx).
        let pi = std::f64::consts::PI;
        return (pi / (pi * x).sin()).ln() - ln_gamma(1.0 - x);
    }
    let x = x - 1.0;
    let mut sum = C[0];
    for (i, &c) in C.iter().enumerate().skip(1) {
        sum += c / (x + i as f64);
    }
    let t = x + G + 0.5;
    0.5 * (2.0 * std::f64::consts::PI).ln() + (x + 0.5) * t.ln() - t + sum.ln()
}

/// `ln B(p, q)`.
fn ln_beta(p: f64, q: f64) -> f64 {
    ln_gamma(p) + ln_gamma(q) - ln_gamma(p + q)
}

// ----------------------------------------------------------------- ranking

/// The fill survivors are packed at, in bytes: `u₀ = 1 − θ` of a page's
/// capacity, which packing promises for every page but a flush's last. A
/// page's fill counts relative to it, and no page at or above it is ripe,
/// since its survivors would take a whole page or more.
pub fn packed_fill(theta: f64) -> f64 {
    (1.0 - theta) * MAX_PAGE_CONTENT as f64
}

/// What keeping a page at relative fill `x` waiting costs, relative to what
/// its draining saves: the threshold `h(x*) = r / κ` of the drafts.
pub fn h(x: f64) -> f64 {
    (1.0 - x) / x + x.ln()
}

/// The keys that rank page `v`, emptier than `packed` bytes, with estimate
/// `d`: its key while its index lies below its floor, its floor's, and its
/// posterior mean draining share.
pub fn keys(d: &Drain, v: &View, packed: f64, sigma: f64, rule: Rule) -> (Option<f64>, f64, f64) {
    let floor = h(v.live / packed).ln() - R_MIN.ln();
    let (ln, share) = ln_index(d, v, packed, sigma, rule);
    (Some(ln - BETA * d.at as f64), floor, share)
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
    /// `K = ln I − β·at`, with `I` the index as of the estimate's epoch
    /// `at`: while the index lies below its floor, the page's log-index is
    /// `K + β·now`.
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

    #[test]
    fn phi_solves_its_equation() {
        let t = phi_table();
        for y in [1e-8, 1e-4, 0.01, 0.3, 1.0, 10.0, 100.0, 1e4] {
            let p = phi_exact(y);
            assert!(
                (p - p.ln_1p() - y).abs() < 1e-9 * y.max(1.0),
                "y = {y}: {p}"
            );
            assert!(
                (t.phi(y) / p - 1.0).abs() < 1e-4,
                "table at {y}: {} against {p}",
                t.phi(y)
            );
        }
        // The draft's table.
        for (y, want) in [
            (0.0001, 0.014),
            (0.01, 0.15),
            (1.0, 2.15),
            (10.0, 12.6),
            (100.0, 104.7),
        ] {
            assert!(
                (phi_exact(y) - want).abs() < 0.006 * want.max(1.0),
                "φ({y})"
            );
        }
    }

    #[test]
    fn ln_gamma_matches_known_values() {
        for (x, want) in [
            (1.0, 0.0),
            (2.0, 0.0),
            (5.0, 24f64.ln()),
            (0.5, std::f64::consts::PI.sqrt().ln()),
            (0.05, 2.968_879_9),
            (100.5, 361.435_540_5),
        ] {
            assert!(
                (ln_gamma(x) - want).abs() < 1e-6,
                "ln Γ({x}) = {}",
                ln_gamma(x)
            );
        }
    }

    #[test]
    fn the_expected_phi_matches_a_direct_integration() {
        // G ~ Gamma(1, 1) is exponential: E φ(G/m) = ∫ φ(g/m) e^(−g) dg.
        let t = phi_table();
        for m in [0.01, 0.3, 1.0, 20.0, 500.0] {
            let n = 200_000;
            let top = 60.0;
            let direct: f64 = (0..n)
                .map(|k| {
                    let g = (k as f64 + 0.5) * top / n as f64;
                    phi_exact(g / m) * (-g).exp() * top / n as f64
                })
                .sum();
            assert!(
                (t.at(1.0, m) / direct - 1.0).abs() < 2e-3,
                "m = {m}: {} against {direct}",
                t.at(1.0, m)
            );
        }
        // Many events: the posterior is sure, and Φ is φ at the mean.
        let (a, m) = (5000.0, 400.0);
        assert!((t.at(a, m) / phi_exact(a / m) - 1.0).abs() < 1e-3);
        // Concave φ: an uncertain posterior expects less than φ at its mean.
        assert!(t.at(1.0, 1.0) < phi_exact(1.0));
        // Small shapes, which spread ln G over some 1/A, against adaptive
        // quadrature (SciPy's `quad`, to 10⁻¹⁰), on grid points and off.
        for (a, m, want) in [
            (0.05, 1.0, 0.153_640_795),
            (0.1, 0.01, 11.084_279_485),
            (0.3, 100.0, 0.057_060_878),
            (0.07, 1e-6, 70_004.816_133),
            (3.0, 0.5, 8.122_593_478),
        ] {
            let got = t.at(a, m);
            assert!(
                (got / want - 1.0).abs() < 5e-3,
                "Φ_{a}({m}) = {got}, not {want}"
            );
        }
    }

    /// A page of `chunks` chunks of `size` bytes each, `drain` of which lose
    /// `per` of their bytes each epoch in events of `sigma` bytes, the rest
    /// never: its estimate and view after `epochs` epochs, from the rate's
    /// `prior` and the draining share `pi` of what it was written with.
    #[allow(clippy::too_many_arguments)]
    fn watch(
        chunks: u32,
        size: f64,
        drain: u32,
        per: f64,
        sigma: f64,
        epochs: u64,
        prior: Prior,
        pi: f64,
    ) -> (Drain, View) {
        let mut d = Drain::start(prior, pi, 0);
        let mut live = chunks as f64 * size;
        let mut untouched = chunks;
        let mut untouched_bytes = live;
        let mut left = drain as f64 * size;
        for t in 1..=epochs {
            let lost = left * per;
            let new = if t == 1 { drain } else { 0 };
            let loss = Loss {
                lost: lost.round() as u32,
                live: live as u32,
                known_live: (live - untouched_bytes) as u32,
                new,
                new_bytes: (new as f64 * size) as u32,
            };
            d.lose(&loss, 0, sigma, t);
            if t == 1 {
                untouched -= drain;
                untouched_bytes -= drain as f64 * size;
            }
            live -= lost;
            left -= lost;
        }
        let v = View {
            live,
            untouched,
            untouched_bytes,
            written: 0,
        };
        (d, v)
    }

    #[test]
    fn untouched_chunks_that_outlive_the_draining_ones_are_found_static() {
        // Twenty chunks of 150 bytes: four lose 5 % a epoch, sixteen never
        // lose a byte. Written as half static, the page finds most of its
        // untouched chunks static after 30 epochs, and all of them once its
        // losses rest on many events, even written as fresh content.
        let prior = Prior { a: 0.8, b: 16.0 };
        let (d, v) = watch(20, 150.0, 4, 0.05, 30.0, 30, prior, 0.5);
        let (_, share) = ln_index(&d, &v, PAGE, 30.0, Rule::ExpectedGain);
        assert!(share < 0.25, "share {share}");
        let (d, v) = watch(20, 150.0, 4, 0.05, 2.0, 30, prior, 1.0);
        let (_, share) = ln_index(&d, &v, PAGE, 2.0, Rule::ExpectedGain);
        let known = (v.live - v.untouched_bytes) / v.live;
        assert!(
            (share - known).abs() < 0.05,
            "share {share}, known to drain {known}"
        );
    }

    #[test]
    fn fresh_content_keeps_draining_on_little_evidence() {
        // The same page as fresh content, whose losses rest on a few events:
        // the prior that all of it drains outweighs the untouched chunks'
        // survival.
        let prior = Prior { a: 0.8, b: 16.0 };
        let (d, v) = watch(20, 150.0, 4, 0.05, 30.0, 30, prior, 1.0);
        let (_, share) = ln_index(&d, &v, PAGE, 30.0, Rule::ExpectedGain);
        assert!(share > 0.5, "share {share}");
    }

    #[test]
    fn a_page_that_drains_throughout_is_found_draining() {
        // Every chunk loses a little every epoch: none is untouched, and the
        // whole page drains at the rate of its losses.
        let prior = Prior { a: 0.8, b: 16.0 };
        let (d, v) = watch(20, 150.0, 20, 0.05, 30.0, 30, prior, 1.0);
        assert_eq!(v.untouched, 0);
        let (_, share) = ln_index(&d, &v, PAGE, 30.0, Rule::ExpectedGain);
        assert!((share - 1.0).abs() < 1e-9, "share {share}");
        let r = d.mean_rate(&v, 30.0);
        assert!((r / 0.05 - 1.0).abs() < 0.15, "rate {r}");
    }

    #[test]
    fn a_static_share_makes_a_page_riper() {
        // The same losses, on a page written as mostly static content and
        // on one written as fresh content.
        let prior = Prior { a: 0.8, b: 16.0 };
        let (d1, v1) = watch(20, 150.0, 4, 0.05, 30.0, 30, prior, 0.2);
        let (d2, v2) = watch(20, 150.0, 4, 0.05, 30.0, 30, prior, 1.0);
        let i1 = ln_index(&d1, &v1, PAGE, 30.0, Rule::ExpectedGain).0;
        let i2 = ln_index(&d2, &v2, PAGE, 30.0, Rule::ExpectedGain).0;
        assert!(i1 > i2, "{i1} against {i2}");
    }

    #[test]
    fn the_option_to_wait_is_worth_little_once_losses_are_known() {
        // With the posterior resting on many events, (c′) ranks a page as the
        // posterior mean does, and (a) as nearly so.
        let prior = Prior { a: 0.8, b: 16.0 };
        let (d, v) = watch(20, 150.0, 20, 0.05, 30.0, 30, prior, 1.0);
        let a = ln_index(&d, &v, PAGE, 30.0, Rule::ExpectedGain).0;
        let c = ln_index(&d, &v, PAGE, 30.0, Rule::OptionToWait).0;
        let mean = (h(v.live / PAGE) / d.mean_rate(&v, 30.0)).ln();
        assert!(
            (c - mean).abs() < 0.05,
            "(c′) {c} against the mean's {mean}"
        );
        assert!((a - mean).abs() < 0.2, "(a) {a} against the mean's {mean}");
    }

    #[test]
    fn a_seeded_page_starts_from_the_one_rate_of_its_life() {
        // Written full 30 epochs ago, half left.
        let rate = 2f64.ln() / 30.0;
        let d = Drain::seed(PAGE, PAGE / 2.0, 30, 30.0, 100.0, 0, 0.0, 30);
        assert!((d.a as f64 / d.b as f64 / rate - 1.0).abs() < 1e-3);
    }

    #[test]
    fn ripe_pages_come_highest_index_first() {
        let mut r = Ripeness::default();
        let floor = |x: f64| h(x).ln() - R_MIN.ln();
        // Log-indexes at epoch 10: ln 11, ln 0.06; and a page at its floor.
        r.insert(1, Some(11f64.ln() - BETA * 10.0), floor(0.3), 10);
        r.insert(2, None, floor(0.8), 10);
        r.insert(3, Some(0.06f64.ln() - BETA * 10.0), floor(0.9), 10);
        assert_eq!(r.ripe(10, 0.01, &mut |_| true, 10), vec![2]);
        assert_eq!(r.ripe(10, 0.2, &mut |_| true, 10), vec![2, 1]);
    }

    #[test]
    fn a_page_whose_index_passes_its_floor_is_settled_and_still_found() {
        let mut r = Ripeness::default();
        let floor = h(0.5).ln() - R_MIN.ln();
        r.insert(7, Some(floor - 20.0), floor, 0);
        assert!(!r.at[&7].0, "draining");
        assert_eq!(r.ripe(500, 0.01, &mut |_| true, 10), vec![7]);
        assert!(r.at[&7].0, "settled");
    }
}
