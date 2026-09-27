//! The consolidator state (`impl/consolidator-state.md`, with what
//! `drafts/bayesian-ripeness.md` adds): what consolidation learned, kept in
//! an ordinary allocation that the header names, so that a session resumes
//! where the last one left off.
//!
//! The allocation's content, in this order, with every `f32` as its
//! little-endian bits:
//!
//! | bytes | what |
//! | --- | --- |
//! | 8 | [`TAG`]: this implementation, and the layout's version |
//! | 8 | `up_to_date`: the epoch of the flush that last wrote the state, little-endian |
//! | 4 | the price of space `κ`, an `f32` |
//! | 8 | the window: the id and offset at which the latest walk began, little-endian |
//! | 4 | the length of the snapshot that starts the records, little-endian |
//! | 16 | for data pages, then leaves: the sizes of natural loss events and their squares, summed and discounted, two `f32`s each |
//! | 40 | for data pages, then leaves: the five sums of the method of moments over pages' early losses ([`Moments`]), five `f32`s each |
//! | .. | records: the snapshot, then those of every flush since |
//!
//! A flush writes the state from its start in two pieces, the first 32 bytes
//! and the rest, so that the fixed fields, and a small state whole, are short
//! enough to be stated inline with the default options, in the address table
//! rather than in a data page.
//!
//! A record starts with its kind:
//!
//! - `0`, an age record: a varint epoch, a varint count, and that many
//!   varint id deltas, ascending from 0: the ids a flush wrote, or in the
//!   snapshot, the ids last written at that epoch.
//! - `1`, a drain record: a varint count, and that many entries of a varint
//!   page delta, ascending from 0, the page's epoch and coverage as varints,
//!   its estimate's sums ([`Drain`]'s `a`, `b`, and `drained`) as `f32`s,
//!   its prior's count of draining chunks in halves, `2·p0`, as a byte, and
//!   the estimate's epoch as a varint. A flush records the pages whose
//!   estimate it changed; the snapshot, every page emptier than the fill
//!   survivors are packed at. The draining share the ranking last found is
//!   not kept: the ranking finds it again, and a page it does not rank
//!   starts from its prior's.

use std::collections::BTreeMap;

use crate::error::Error;
use crate::hash::{IdMap, IdSet};
use crate::ripeness::{Loss, Moments, NU_PI};
use crate::state::*;
use crate::store::Inner;

/// Names this implementation and the layout's version.
pub(crate) const TAG: [u8; 8] = *b"kladbay1";
/// The bytes before the records.
const FIXED: usize = 88;
/// Where the second piece of the fixed fields starts.
const SPLIT: usize = 32;

const AGE: u8 = 0;
const DRAIN: u8 = 1;

/// One page's estimate, as recorded.
#[derive(Clone, Copy, Debug)]
struct DrainEntry {
    page: u32,
    epoch: u64,
    coverage: u32,
    drain: Drain,
}

/// What a parsed state holds.
struct Parsed {
    up_to_date: u64,
    kappa: f32,
    events: [(f64, f64); 2],
    moments: [Moments; 2],
    window: Key,
    snapshot_len: u32,
    ages: Vec<(u64, Vec<u32>)>,
    drains: Vec<DrainEntry>,
    records_len: u32,
}

fn put_varint(out: &mut Vec<u8>, v: u64) {
    kladde_varint::encode(v, out);
}

/// Encodes one age record.
fn age_record(out: &mut Vec<u8>, epoch: u64, ids: &[u32]) {
    out.push(AGE);
    put_varint(out, epoch);
    put_varint(out, ids.len() as u64);
    let mut prev = 0u32;
    for &id in ids {
        put_varint(out, (id - prev) as u64);
        prev = id;
    }
}

/// Encodes one drain record, of `entries` in ascending page order.
fn drain_record(out: &mut Vec<u8>, entries: &[DrainEntry]) {
    out.push(DRAIN);
    put_varint(out, entries.len() as u64);
    let mut prev = 0u32;
    for e in entries {
        put_varint(out, (e.page - prev) as u64);
        prev = e.page;
        put_varint(out, e.epoch);
        put_varint(out, e.coverage as u64);
        let d = &e.drain;
        for v in [d.a, d.b, d.drained] {
            out.extend_from_slice(&v.to_bits().to_le_bytes());
        }
        out.push((2.0 * d.p0).round() as u8);
        put_varint(out, d.at);
    }
}

fn parse(bytes: &[u8]) -> Option<Parsed> {
    if bytes.len() < FIXED || bytes[..8] != TAG {
        return None;
    }
    let u32_at = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
    let up_to_date = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let f32_at = |i: usize| f32::from_bits(u32_at(i)) as f64;
    let kappa = f32::from_bits(u32_at(16));
    let window = key(u32_at(20), u32_at(24));
    let snapshot_len = u32_at(28);
    let events = [0, 1].map(|k| (f32_at(SPLIT + 8 * k), f32_at(SPLIT + 4 + 8 * k)));
    let moments = [0, 1].map(|k| {
        let at = |j: usize| f32_at(SPLIT + 16 + 20 * k + 4 * j);
        Moments {
            n: at(0),
            k: at(1),
            e: at(2),
            e2: at(3),
            k2e: at(4),
        }
    });
    let mut rest = &bytes[FIXED..];
    let get = |rest: &mut &[u8]| -> Option<u64> {
        let (v, r) = kladde_varint::decode(rest).ok()?;
        *rest = r;
        Some(v)
    };
    let (mut ages, mut drains) = (Vec::new(), Vec::new());
    while let Some((&kind, r)) = rest.split_first() {
        rest = r;
        match kind {
            AGE => {
                let epoch = get(&mut rest)?;
                let n = get(&mut rest)?;
                let mut ids = Vec::with_capacity((n as usize).min(rest.len()));
                let mut id = 0u64;
                for _ in 0..n {
                    id += get(&mut rest)?;
                    ids.push(u32::try_from(id).ok()?);
                }
                ages.push((epoch, ids));
            }
            DRAIN => {
                let n = get(&mut rest)?;
                let mut page = 0u64;
                for _ in 0..n {
                    page += get(&mut rest)?;
                    let epoch = get(&mut rest)?;
                    let coverage = u32::try_from(get(&mut rest)?).ok()?;
                    let mut f = [0f32; 3];
                    for v in &mut f {
                        let (bits, r) = rest.split_first_chunk::<4>()?;
                        rest = r;
                        *v = f32::from_bits(u32::from_le_bytes(*bits));
                    }
                    let (&halves, r) = rest.split_first()?;
                    rest = r;
                    let at = get(&mut rest)?;
                    let [a, b, drained] = f;
                    let p0 = (halves as f32 / 2.0).clamp(0.5, NU_PI as f32 + 0.5);
                    drains.push(DrainEntry {
                        page: u32::try_from(page).ok()?,
                        epoch,
                        coverage,
                        drain: Drain {
                            a,
                            b,
                            drained,
                            p0,
                            share: ((p0 as f64 - 0.5) / NU_PI) as f32,
                            at,
                        },
                    });
                }
            }
            _ => return None,
        }
    }
    let records_len = (bytes.len() - FIXED) as u32;
    Some(Parsed {
        up_to_date,
        kappa,
        events,
        moments,
        window,
        snapshot_len,
        ages,
        drains,
        records_len,
    })
}

/// Where consolidation's state lives, and how much of it is snapshot.
#[derive(Debug, Default)]
pub(crate) struct Kept {
    /// The allocation holding the state, or 0 before the first flush writes one.
    pub id: u32,
    /// Whether the next flush must write a fresh snapshot: the allocation
    /// holds another implementation's state, or one that is not current.
    pub fresh: bool,
    /// Bytes of snapshot, and of records appended since.
    pub snapshot_len: u32,
    pub appended_len: u32,
}

impl Inner {
    /// The youngest page each allocation's content is in, the fallback for
    /// an allocation without a current age record; and the youngest page
    /// holding a statement naming it, which says whether a record is still
    /// current.
    fn youngest_pages(&self) -> (IdMap<u64>, IdMap<u64>) {
        let (mut content, mut statements): (IdMap<u64>, IdMap<u64>) = Default::default();
        let epoch_of = |page: u32| self.state.pages[page as usize].epoch;
        let raise = |m: &mut IdMap<u64>, id: u32, e: u64| {
            let y = m.entry(id).or_default();
            *y = (*y).max(e);
        };
        for (&k, &f) in &self.state.frags {
            let id = kid(k);
            match f {
                Fragment::Bytes { page, stmt, .. } => {
                    raise(&mut content, id, epoch_of(page));
                    raise(&mut statements, id, epoch_of(self.state.slab.page(stmt)));
                }
                Fragment::ZeroExplicitly { stmt } => {
                    let e = epoch_of(self.state.slab.page(stmt));
                    raise(&mut content, id, e);
                    raise(&mut statements, id, e);
                }
                _ => {}
            }
        }
        for (&id, m) in &self.state.allocs {
            for s in [m.anchor, m.grow].into_iter().flatten() {
                let e = epoch_of(self.state.slab.page(s));
                raise(&mut statements, id, e);
                content.entry(id).or_insert(e);
            }
        }
        (content, statements)
    }

    /// Seeds each page's estimate from its fill and its age, assuming that
    /// all of it has drained at one rate since it was written: from its
    /// content size then to its coverage now, over the epochs until the
    /// session's first flush. Its past counts as watched, and a data page's
    /// `Ref` payloads that have lost bytes as draining since then.
    fn seed_drains(&mut self) {
        let first = self.epoch + 1;
        let sigma = self.state.sigma();
        let mut touched: IdMap<(u32, f64)> = IdMap::default();
        let mut seen = IdSet::default();
        for &f in self.state.frags.values() {
            if let Fragment::Bytes { page, stmt, .. } = f {
                let i = stmt.idx();
                if self.state.slab.touched[i] && seen.insert(i as u32) {
                    let t = touched.entry(page).or_default();
                    t.0 += 1;
                    t.1 += self.state.slab.size[i] as f64;
                }
            }
        }
        for (p, info) in self.state.pages.iter_mut().enumerate().skip(2) {
            if !matches!(info.state, PageState::Data | PageState::Table) {
                continue;
            }
            let (written, live) = (info.written as f64, info.coverage as f64);
            let past = self.epoch.saturating_sub(info.epoch);
            let age = first.saturating_sub(info.epoch).max(1) as f64;
            let s = sigma[usize::from(info.state != PageState::Data)];
            let (n, bytes) = touched.get(&(p as u32)).copied().unwrap_or_default();
            info.drain = Drain::seed(written, live, past, age, s, n, bytes, self.epoch);
        }
    }

    /// Seeds consolidation after a load: content ages and page estimates
    /// from the consolidator state where they are current, and from the
    /// pages elsewhere; the price of space, what loss events and pages' early
    /// losses have shown, and the window from the state; the cursor's pages
    /// from the governing header's epoch; and then a read-only walk that
    /// finds again the candidates the latest walk found.
    pub(crate) fn seed_after_load(&mut self) -> Result<(), Error> {
        self.cons.kappa = self.opts.kappa;
        self.cons.prev_data_pages = (2..self.file_pages)
            .filter(|&p| {
                let info = &self.state.pages[p as usize];
                info.state == PageState::Data && info.epoch == self.epoch
            })
            .collect();
        let (content, statements) = self.youngest_pages();
        for (&id, m) in self.state.allocs.iter_mut() {
            m.last_written = content.get(&id).copied().unwrap_or(self.epoch);
        }
        let id = self.header.consolidator_state;
        let parsed = match self.state.allocs.get(&id) {
            Some(m) if id != 0 => {
                let mut bytes = vec![0u8; m.size as usize];
                self.read_committed(id, 0, &mut bytes)?;
                self.cache.clear();
                Some(parse(&bytes))
            }
            _ => None,
        };
        // So that the session's first flush does not start its pages from
        // nothing, and the seeds weigh their pasts in the right events.
        if let Some(Some(p)) = &parsed {
            let finite = |v: f64| v.is_finite() && v >= 0.0;
            for (kind, &(s, s2)) in p.events.iter().enumerate() {
                if finite(s) && finite(s2) {
                    self.state.events[kind] = (s, s2);
                }
            }
            for (kind, m) in p.moments.iter().enumerate() {
                if [m.n, m.k, m.e, m.e2, m.k2e].into_iter().all(finite) {
                    self.cons.moments[kind] = *m;
                    if let Some(prior) = m.prior() {
                        self.cons.prior[kind] = prior;
                    }
                }
            }
        }
        self.seed_drains();
        let sigma = self.state.sigma();
        let mut window = None;
        match parsed {
            None => {}
            // Another implementation's, or unreadable: ours to replace.
            Some(None) => {
                self.cons.kept = Kept {
                    id,
                    fresh: true,
                    ..Default::default()
                }
            }
            Some(Some(p)) => {
                let current = p.up_to_date == self.epoch;
                for (epoch, ids) in &p.ages {
                    for &i in ids {
                        let fresh =
                            current || statements.get(&i).is_none_or(|&e| e <= p.up_to_date);
                        if let Some(m) = self.state.allocs.get_mut(&i).filter(|_| fresh) {
                            m.last_written = *epoch;
                        }
                    }
                }
                // A page's latest entry is current if the page is still the
                // one it describes; what it lost since is a loss in the gap.
                let mut latest: IdMap<DrainEntry> = IdMap::default();
                for e in p.drains {
                    latest.insert(e.page, e);
                }
                for e in latest.into_values() {
                    let Some(info) = self.state.pages.get_mut(e.page as usize) else {
                        continue;
                    };
                    let live = matches!(info.state, PageState::Data | PageState::Table);
                    if !live || info.epoch != e.epoch {
                        continue;
                    }
                    info.drain = e.drain;
                    if info.coverage < e.coverage {
                        let loss = Loss {
                            lost: e.coverage - info.coverage,
                            live: e.coverage,
                            known_live: info.known_live(),
                            ..Default::default()
                        };
                        let s = sigma[usize::from(info.state != PageState::Data)];
                        info.drain.lose(&loss, info.epoch, s, self.epoch);
                    }
                }
                if p.kappa.is_finite() && p.kappa > 0.0 {
                    self.cons.kappa = p.kappa as f64;
                }
                window = Some(p.window);
                self.cons.kept = Kept {
                    id,
                    // A state that is not current is rewritten whole, so that
                    // records it could not vouch for do not become current.
                    fresh: !current,
                    snapshot_len: p.snapshot_len,
                    appended_len: p.records_len.saturating_sub(p.snapshot_len),
                };
            }
        }
        for p in 2..self.file_pages {
            self.state.rerank(p);
        }
        self.cons.cursor = match window {
            Some(w) => w,
            None => self.seed_cursor()?,
        };
        if self.opts.consolidate && !self.state.frags.is_empty() {
            // The walk at open: read-only, it finds the candidates the
            // previous session's last walk found and no flush executed.
            self.state.flush_epoch = self.epoch + 1;
            self.window(0, &Dirty::default(), &mut Dirty::default());
        }
        Ok(())
    }

    /// Where the rotation starts without a usable state: at the first
    /// statement of a table page chosen by the governing header's CRC, so
    /// that sessions spread over the key space and tests reproduce.
    fn seed_cursor(&self) -> Result<Key, Error> {
        let mut pages = self.all_leaves();
        pages.push(self.header_slot);
        let crc = crate::crc::crc32c(&self.table_pages[&self.header_slot][..]);
        let page = pages[crc as usize % pages.len()];
        let buf = &self.table_pages[&page];
        let d = crate::page::decode_page(buf, page < 2)
            .map_err(|_| crate::error::corrupt("a resident table page does not decode"))?;
        let (_, mut r) = crate::statement::TableReader::open(&buf[..], d.content)?;
        Ok(r.next_stmt()?.map_or(0, |s| key(s.stmt.id, s.stmt.offset)))
    }

    /// The current entries of `pages`, in ascending page order.
    fn drain_entries(&self, pages: impl IntoIterator<Item = u32>) -> Vec<DrainEntry> {
        let mut out: Vec<DrainEntry> = pages
            .into_iter()
            .filter_map(|p| {
                let info = self.state.pages.get(p as usize)?;
                matches!(info.state, PageState::Data | PageState::Table).then_some(DrainEntry {
                    page: p,
                    epoch: info.epoch,
                    coverage: info.coverage,
                    drain: info.drain,
                })
            })
            .collect();
        out.sort_unstable_by_key(|e| e.page);
        out.dedup_by_key(|e| e.page);
        out
    }

    /// Brings the consolidator state up to date with this flush, which has
    /// just folded its segment: appends the flush's age record and the
    /// estimates it changed, and rewrites the fixed fields, or writes a fresh
    /// snapshot when one is due.
    pub(crate) fn write_consolidator_state(&mut self, dirty: &mut Dirty) -> Result<(), Error> {
        let e = self.state.flush_epoch;
        let kept = std::mem::take(&mut self.cons.kept);
        let mut written: Vec<u32> = dirty
            .records
            .iter()
            .filter(|(&id, r)| r.written && id != kept.id && self.state.allocs.contains_key(&id))
            .map(|(&id, _)| id)
            .collect();
        written.sort_unstable();
        let mut rec = Vec::new();
        if !written.is_empty() {
            age_record(&mut rec, e, &written);
        }
        let drained = self.drain_entries(self.cons.drained.iter().copied());
        if !drained.is_empty() {
            drain_record(&mut rec, &drained);
        }
        let exists = kept.id != 0 && self.state.allocs.contains_key(&kept.id);
        let snapshot = !exists
            || kept.fresh
            || kept.appended_len + rec.len() as u32 > kept.snapshot_len.max(64);
        let id = if exists { kept.id } else { self.ids.mint()? };
        let mut fixed = Vec::with_capacity(FIXED);
        fixed.extend_from_slice(&TAG);
        fixed.extend_from_slice(&e.to_le_bytes());
        fixed.extend_from_slice(&(self.cons.kappa as f32).to_bits().to_le_bytes());
        fixed.extend_from_slice(&kid(self.cons.cursor).to_le_bytes());
        fixed.extend_from_slice(&koff(self.cons.cursor).to_le_bytes());
        let snap = snapshot.then(|| self.snapshot(id));
        let snapshot_len = snap.as_ref().map_or(kept.snapshot_len, |s| s.len() as u32);
        fixed.extend_from_slice(&snapshot_len.to_le_bytes());
        let events = self.state.events.iter().flat_map(|&(s, s2)| [s, s2]);
        let moments = self
            .cons
            .moments
            .iter()
            .flat_map(|m| [m.n, m.k, m.e, m.e2, m.k2e]);
        for v in events.chain(moments) {
            fixed.extend_from_slice(&(v as f32).to_bits().to_le_bytes());
        }
        if let Some(snap) = snap {
            fixed.extend_from_slice(&snap);
            if !exists {
                self.allocate(id, 0, dirty);
                self.header.consolidator_state = id;
            } else {
                self.touch(id, true, dirty);
                let old = self.state.size_of(id);
                if (fixed.len() as u32) < old {
                    self.state.shrink_size_to(id, fixed.len() as u32);
                    dirty.records.get_mut(&id).unwrap().shrank = true;
                }
            }
            self.put_bytes(id, 0, &fixed[..SPLIT], dirty);
            self.put_bytes(id, SPLIT as u32, &fixed[SPLIT..], dirty);
            self.cons.kept = Kept {
                id,
                fresh: false,
                snapshot_len,
                appended_len: 0,
            };
        } else {
            self.touch(id, true, dirty);
            let end = self.state.size_of(id);
            self.put_bytes(id, 0, &fixed[..SPLIT], dirty);
            self.put_bytes(id, SPLIT as u32, &fixed[SPLIT..], dirty);
            if !rec.is_empty() {
                self.put_bytes(id, end, &rec, dirty);
            }
            self.cons.kept = Kept {
                appended_len: kept.appended_len + rec.len() as u32,
                ..kept
            };
        }
        Ok(())
    }

    /// The snapshot: one age record per epoch, naming every allocation whose
    /// `last_written` the pages holding it after this flush would not give;
    /// then one drain record, of every page emptier than the fill survivors
    /// are packed at.
    fn snapshot(&self, own: u32) -> Vec<u8> {
        let e = self.state.flush_epoch;
        let mut fallback: IdMap<u64> = IdMap::default();
        for (&k, &f) in &self.state.frags {
            let page = match f {
                Fragment::Bytes { page, .. } => Some(page),
                Fragment::ZeroExplicitly { stmt } => Some(self.state.slab.page(stmt)),
                Fragment::Pending(p) => match self.state.pending[p as usize].place {
                    Place::Data(a) => Some(split_address(a).0)
                        .filter(|&pg| self.state.pages[pg as usize].state != PageState::Claimed),
                    _ => None,
                },
                Fragment::ZeroByDefault => continue,
            };
            // Anything this flush states lands in a page of this epoch.
            let epoch = page.map_or(e, |pg| self.state.pages[pg as usize].epoch);
            let y = fallback.entry(kid(k)).or_default();
            *y = (*y).max(epoch);
        }
        let mut by_epoch: BTreeMap<u64, Vec<u32>> = BTreeMap::new();
        for (&id, m) in &self.state.allocs {
            if id != own && fallback.get(&id) != Some(&m.last_written) {
                by_epoch.entry(m.last_written).or_default().push(id);
            }
        }
        let mut out = Vec::new();
        for (epoch, mut ids) in by_epoch {
            ids.sort_unstable();
            age_record(&mut out, epoch, &ids);
        }
        let packed = crate::ripeness::packed_fill(self.opts.theta);
        let below = (2..self.file_pages).filter(|&p| {
            let c = self.state.pages[p as usize].coverage;
            c > 0 && (c as f64) < packed
        });
        let entries = self.drain_entries(below);
        if !entries.is_empty() {
            drain_record(&mut out, &entries);
        }
        out
    }

    /// Writes `bytes` at `offset` of `id` as the flush's own content,
    /// growing `id` if they reach past its end.
    fn put_bytes(&mut self, id: u32, offset: u32, bytes: &[u8], dirty: &mut Dirty) {
        let end = offset + bytes.len() as u32;
        let old = self.state.size_of(id);
        if end > old {
            self.grow(id, old, end, dirty);
        }
        let pos = self.segment_records.arena.len() as u64;
        self.segment_records.arena.extend_from_slice(bytes);
        let place = if bytes.len() as u32 <= self.opts.inline_threshold {
            Place::Inline
        } else {
            Place::Unplaced
        };
        let p = Pending {
            origin: Origin::Arena(pos),
            place,
            heat: 0,
            rewrite: false,
        };
        self.state.take(id, offset, end, p, dirty);
    }
}
