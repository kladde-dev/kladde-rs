//! Description defragmentation
//! (`impl/consolidation.md#description-defragmentation-rides-the-rotating-window`):
//! the rotating window's walk finds ranges described by many statements that
//! one statement could describe, and later flushes rewrite the best of them.

use crate::consts::{MAX_INLINE, MAX_PAGE_CONTENT};
use crate::error::Error;
use crate::state::*;
use crate::statement::{Stmt, TableWriter};
use crate::store::Inner;

const C: u32 = MAX_PAGE_CONTENT as u32;

/// Framing estimates for a pending fragment, whose statement is not bound
/// yet: what it would take if stated alone.
const PENDING_REF: f64 = 8.0;
const PENDING_INLINE: f64 = 4.0;
const PENDING_ZERO: f64 = 5.0;

/// A fragment as description defragmentation weighs it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Weighed {
    start: u32,
    end: u32,
    /// The framing its rewrite retires: its statement's framing, shared
    /// among the statement's pins.
    share: f64,
    /// Net file bytes per byte a rewrite of it releases: 0 for bytes, which
    /// only move, and −1 for zeros, which become real bytes.
    per_byte: f64,
    /// Flushes since its statement was placed; `None` for a fragment no
    /// statement owns.
    age: Option<u64>,
}

/// A range that one statement could describe with profit.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Candidate {
    pub id: u32,
    pub start: u32,
    pub end: u32,
    /// `gain · age / written`, by which candidates compete.
    pub score: f64,
}

impl Candidate {
    fn len(&self) -> u32 {
        self.end - self.start
    }
}

impl Inner {
    /// Weighs the fragment `f` over `[start, end)`.
    pub(crate) fn weigh(&self, start: u32, end: u32, f: Fragment) -> Weighed {
        let owned = |s: StmtRef| {
            let i = s.idx();
            let share = self.state.slab.framing[i] as f64 / self.state.slab.pins[i].max(1) as f64;
            let placed = self.state.pages[self.state.slab.page(s) as usize].epoch;
            (share, Some(self.state.flush_epoch.saturating_sub(placed)))
        };
        let (share, per_byte, age) = match f {
            Fragment::Bytes { stmt, .. } => {
                let (share, age) = owned(stmt);
                (share, 0.0, age)
            }
            Fragment::ZeroExplicitly { stmt } => {
                let (share, age) = owned(stmt);
                (share, -1.0, age)
            }
            Fragment::ZeroByDefault => (0.0, -1.0, None),
            Fragment::Pending(p) => match self.state.pending[p as usize].place {
                Place::Zero => (PENDING_ZERO, -1.0, Some(0)),
                Place::Inline => (PENDING_INLINE, 0.0, Some(0)),
                _ => (PENDING_REF, 0.0, Some(0)),
            },
        };
        Weighed {
            start,
            end,
            share,
            per_byte,
            age,
        }
    }

    /// A fragment's weight in Kadane's search: net file bytes a rewrite
    /// releases, less `μ` per byte it writes.
    fn weight(&self, w: &Weighed) -> f64 {
        w.share + (w.per_byte - self.opts.mu) * (w.end - w.start) as f64
    }

    /// The framing of the one statement a rewrite of `len` bytes at `start`
    /// states: an `Inline` up to the threshold, a `Ref` beyond.
    fn new_framing(&self, id: u32, start: u32, len: u32) -> f64 {
        if len <= self.opts.inline_threshold.min(MAX_INLINE as u32) {
            (TableWriter::standalone_len(&Stmt::inline(id, start, len)) - len as usize) as f64
        } else {
            let far = address(self.file_pages, 0);
            TableWriter::standalone_len(&Stmt::reference(id, start, len, far)) as f64
        }
    }

    /// What rewriting `[start, end)` of `id` gains, net of the statement it
    /// adds, and how long its content has gone unwritten, from the weighed
    /// fragments `ws` overlapping it. A fragment's share is credited to the
    /// range it starts in.
    fn worth(&self, id: u32, start: u32, end: u32, ws: &[Weighed]) -> (f64, u64) {
        let mut gain = -self.new_framing(id, start, end - start);
        let mut youngest: Option<u64> = None;
        for w in ws {
            let (a, b) = (w.start.max(start), w.end.min(end));
            if a >= b {
                continue;
            }
            gain += (w.per_byte - self.opts.mu) * (b - a) as f64;
            if w.start >= start {
                gain += w.share;
            }
            if let Some(age) = w.age {
                youngest = Some(youngest.map_or(age, |y| y.min(age)));
            }
        }
        let content_age = self
            .state
            .allocs
            .get(&id)
            .map_or(0, |m| self.state.flush_epoch.saturating_sub(m.last_written));
        (gain, content_age.max(youngest.unwrap_or(0)))
    }

    /// Adds the candidates of `id`, whose walked fragments, adjacent and in
    /// offset order, are `ws`: Kadane's maximum-weight run, tiled into
    /// ranges of at most a page, each kept if it gains on its own.
    pub(crate) fn candidates_of(&self, id: u32, ws: &[Weighed], out: &mut Vec<Candidate>) {
        let (mut best, mut run) = (0.0f64, None);
        let (mut cur, mut from) = (0.0f64, 0usize);
        for (j, w) in ws.iter().enumerate() {
            let x = self.weight(w);
            if cur <= 0.0 {
                (cur, from) = (x, j);
            } else {
                cur += x;
            }
            if cur > best {
                (best, run) = (cur, Some((from, j)));
            }
        }
        let Some((i, j)) = run else { return };
        let (a, b) = (ws[i].start, ws[j].end);
        let mut start = a;
        while start < b {
            let end = (start as u64 + C as u64).min(b as u64) as u32;
            let (gain, age) = self.worth(id, start, end, &ws[i..=j]);
            if gain > 0.0 {
                let score = gain * age as f64 / (end - start) as f64;
                out.push(Candidate {
                    id,
                    start,
                    end,
                    score,
                });
            }
            start = end;
        }
    }

    /// Re-checks `c` against the fragment map as it now stands: its gain and
    /// age, if it still gains, has gone unwritten for at least a flush, and
    /// holds no byte this flush is writing to a new place.
    fn recheck(&self, c: &Candidate) -> Option<(f64, u64)> {
        let size = self.state.allocs.get(&c.id)?.size;
        if c.end > size || c.start >= c.end {
            return None;
        }
        let first = match self.state.frags.range(..=key(c.id, c.start)).next_back() {
            Some((&k, _)) if kid(k) == c.id => k,
            _ => return None,
        };
        let mut ws = Vec::new();
        let mut it = self.state.frags.range(first..key(c.id, c.end)).peekable();
        while let Some((&k, &f)) = it.next() {
            if let Fragment::Pending(p) = f {
                match self.state.pending[p as usize].place {
                    Place::Unplaced => return None,
                    Place::Data(a)
                        if self.state.pages[split_address(a).0 as usize].state
                            == PageState::Claimed =>
                    {
                        return None
                    }
                    _ => {}
                }
            }
            let end = match it.peek() {
                Some((&n, _)) => koff(n),
                None => self.state.frag_end(k),
            };
            ws.push(self.weigh(koff(k), end, f));
        }
        let (gain, age) = self.worth(c.id, c.start, c.end, &ws);
        (gain > 0.0 && age >= 1).then_some((gain, age))
    }

    /// Takes all of `c` as one pending fragment holding its current bytes,
    /// with the gaps as real zeros: a chunk, or, if it is short enough, an
    /// `Inline`. Leaves `last_written` alone, since the content does not
    /// change.
    fn rewrite(&mut self, c: &Candidate, age: u64, dirty: &mut Dirty) -> Result<(), Error> {
        let mut bytes = vec![0u8; c.len() as usize];
        self.read_committed(c.id, c.start, &mut bytes)?;
        let pos = self.segment_records.arena.len() as u64;
        self.segment_records.arena.extend_from_slice(&bytes);
        let inline = c.len() <= self.opts.inline_threshold.min(MAX_INLINE as u32);
        let place = if inline {
            Place::Inline
        } else {
            Place::Unplaced
        };
        let heat = age.min(u32::MAX as u64) as u32;
        let p = Pending {
            origin: Origin::Arena(pos),
            place,
            heat,
            rewrite: true,
        };
        self.state.take(c.id, c.start, c.end, p, dirty);
        self.stats.defrag_rewrites += 1;
        self.stats.defrag_bytes += c.len() as u64;
        Ok(())
    }

    /// Spends description defragmentation's reserved share, before anything
    /// is packed: the best of the candidates the latest walk found, re-checked,
    /// become writes of the flush's own.
    pub(crate) fn defrag_share(&mut self, dirty: &mut Dirty) -> Result<(), Error> {
        let found = std::mem::take(&mut self.cons.candidates);
        let mut ok: Vec<(Candidate, u64)> = found
            .into_iter()
            .filter_map(|c| {
                let (gain, age) = self.recheck(&c)?;
                Some((
                    Candidate {
                        score: gain * age as f64 / c.len() as f64,
                        ..c
                    },
                    age,
                ))
            })
            .collect();
        ok.sort_by(|x, y| y.0.score.total_cmp(&x.0.score));
        let mut share = self.opts.defrag_share as u64 * C as u64;
        for (c, age) in ok {
            if c.len() as u64 <= share {
                share -= c.len() as u64;
                self.rewrite(&c, age, dirty)?;
            }
        }
        Ok(())
    }
}
