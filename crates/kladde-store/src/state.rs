//! The in-memory structures of `impl/in-memory-state.md` and the
//! fragment-map primitives of `impl/address-table-operations.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;

use crate::consts::PAGE_SIZE;
use crate::hash::{IdMap, IdSet};
pub use crate::ripeness::{Drain, Ripeness};
use crate::statement::Kind;

/// A fragment-map key: `(id, offset)` packed with the id in the high half, so
/// that integer order is `(id, offset)` order.
pub type Key = u64;

#[inline]
pub fn key(id: u32, offset: u32) -> Key {
    (id as u64) << 32 | offset as u64
}
#[inline]
pub fn kid(k: Key) -> u32 {
    (k >> 32) as u32
}
#[inline]
pub fn koff(k: Key) -> u32 {
    k as u32
}

/// A statement's slab slot. Slot 0 is never handed out.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct StmtRef(NonZeroU32);

impl StmtRef {
    #[inline]
    pub fn idx(self) -> usize {
        self.0.get() as usize
    }
}

/// The resolved content of `[key.offset, next key or size)` of one id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fragment {
    /// Bytes at `offset` within `page`, stated by `stmt` (a `Ref` or `Inline`).
    Bytes {
        page: u32,
        offset: u16,
        stmt: StmtRef,
    },
    /// Zeros stated by a `Zero`, `Shrink`, or `Tombstone`.
    ZeroExplicitly { stmt: StmtRef },
    /// Zeros because no statement matches.
    ZeroByDefault,
    /// Taken by the flush in progress; an index into its pending table.
    Pending(u32),
}

impl Fragment {
    #[inline]
    pub fn stmt(&self) -> Option<StmtRef> {
        match *self {
            Fragment::Bytes { stmt, .. } | Fragment::ZeroExplicitly { stmt } => Some(stmt),
            _ => None,
        }
    }
}

/// Where a pending fragment's bytes are now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// In the flush's arena, at this position.
    Arena(u64),
    /// In the file, at this address.
    File(u64),
    /// Zeros: nothing to read.
    None,
}

/// Where a pending fragment will be stated from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// A chunk that `pack` gives a data page.
    Unplaced,
    /// In a data page, at this address.
    Data(u64),
    /// In the address table, as an `Inline` payload.
    Inline,
    /// A `Zero` statement's range.
    Zero,
}

/// A fragment the flush in progress has taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pending {
    pub origin: Origin,
    pub place: Place,
    /// Flushes untouched, for the header's split; 0 for what this flush wrote.
    pub heat: u32,
    /// Whether it is a description defragmentation rewrite, which `pack` cuts
    /// only when nothing else is within reach.
    pub rewrite: bool,
}

impl Pending {
    /// The same pending content `d` bytes further in.
    pub fn advanced(&self, d: u32) -> Pending {
        let origin = match self.origin {
            Origin::Arena(p) => Origin::Arena(p + d as u64),
            Origin::File(a) => Origin::File(a + d as u64),
            Origin::None => Origin::None,
        };
        let place = match self.place {
            Place::Data(a) => Place::Data(a + d as u64),
            other => other,
        };
        Pending {
            origin,
            place,
            ..*self
        }
    }
}

/// One record per live statement, as parallel arrays.
#[derive(Debug)]
pub struct Slab {
    /// The table page holding the statement, or the next free slot.
    pub page_or_next: Vec<u32>,
    pub pins: Vec<u32>,
    pub framing: Vec<u8>,
    pub ids: Vec<u32>,
    pub kinds: Vec<Kind>,
    free_head: u32,
    pub live: u32,
}

impl Default for Slab {
    fn default() -> Self {
        Slab {
            page_or_next: vec![0],
            pins: vec![0],
            framing: vec![0],
            ids: vec![0],
            kinds: vec![Kind::Tombstone],
            free_head: 0,
            live: 0,
        }
    }
}

impl Slab {
    /// A slot for a statement in `page`, with no pins yet.
    pub fn alloc(&mut self, page: u32, framing: u8, id: u32, kind: Kind) -> StmtRef {
        self.live += 1;
        if self.free_head != 0 {
            let s = self.free_head as usize;
            self.free_head = self.page_or_next[s];
            self.page_or_next[s] = page;
            self.pins[s] = 0;
            self.framing[s] = framing;
            self.ids[s] = id;
            self.kinds[s] = kind;
            return StmtRef(NonZeroU32::new(s as u32).unwrap());
        }
        let s = self.page_or_next.len();
        assert!(s < u32::MAX as usize, "more than 2^32 - 1 statements");
        self.page_or_next.push(page);
        self.pins.push(0);
        self.framing.push(framing);
        self.ids.push(id);
        self.kinds.push(kind);
        StmtRef(NonZeroU32::new(s as u32).unwrap())
    }

    #[inline]
    pub fn page(&self, s: StmtRef) -> u32 {
        self.page_or_next[s.idx()]
    }

    fn release(&mut self, s: StmtRef) {
        self.live -= 1;
        self.page_or_next[s.idx()] = self.free_head;
        self.free_head = s.idx() as u32;
    }
}

/// Per-allocation metadata.
#[derive(Clone, Debug, Default)]
pub struct AllocationMeta {
    pub size: u32,
    pub fragment_count: u32,
    pub statement_bytes: u32,
    pub mentions: u32,
    /// The epoch of the flush that last wrote the id.
    pub last_written: u64,
    /// The newest `Shrink` or `Tombstone`, and its bound.
    pub anchor: Option<StmtRef>,
    pub anchor_n: u32,
    /// A `Grow` with `n == size` that may be the size's sole witness.
    pub grow: Option<StmtRef>,
    pub grow_n: u32,
}

/// What a non-existent id still carries.
#[derive(Clone, Copy, Debug, Default)]
pub struct Recyclable {
    pub mentions: u32,
    pub tombstone: Option<StmtRef>,
}

/// What a page is, as far as the store is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageState {
    /// Reusable: in the ready pool.
    Free,
    /// Dropped by the last commit; reusable after the next one.
    Retiring,
    /// A live data page.
    Data,
    /// A live address-table leaf.
    Table,
    /// A live interior address-table page, holding child references only.
    Interior,
    /// Part of the current journal segment.
    Journal,
    /// Page 0 or 1.
    Header,
    /// Taken by the flush in progress, and not written yet.
    Claimed,
}

/// Per-page state.
#[derive(Clone, Copy, Debug)]
pub struct PageInfo {
    pub state: PageState,
    pub epoch: u64,
    /// Bytes current state relies on.
    pub coverage: u32,
    /// For a table page, the page naming it.
    pub parent: u32,
    /// Its content size when written.
    pub written: u16,
    /// How fast its content still dies.
    pub drain: Drain,
}

impl Default for PageInfo {
    fn default() -> Self {
        PageInfo {
            state: PageState::Free,
            epoch: 0,
            coverage: 0,
            parent: 0,
            written: 0,
            drain: Drain::default(),
        }
    }
}

/// Every key range the flush has taken, merged and sorted, plus per touched id
/// what the fragment map cannot say.
#[derive(Debug, Default)]
pub struct Dirty {
    /// `key(id, start) -> end`, with overlapping or touching ranges merged.
    pub ranges: BTreeMap<Key, u32>,
    pub records: IdMap<IdRecord>,
}

/// How an id changed during the flush.
#[derive(Clone, Copy, Debug, Default)]
pub struct IdRecord {
    /// Its size when the flush first touched it.
    pub size_before: u32,
    /// It existed and was freed.
    pub freed: bool,
    /// It exists now after not existing, or after being freed.
    pub allocated: bool,
    /// Its size went down.
    pub shrank: bool,
    /// A page the flush retires held its anchor.
    pub replace_anchor: bool,
    /// A page the flush retires held its grow witness.
    pub replace_grow: bool,
    /// The application wrote it.
    pub written: bool,
}

impl Dirty {
    /// Adds `[start, end)` of `id`.
    pub fn add(&mut self, id: u32, start: u32, end: u32) {
        if start >= end {
            return;
        }
        let (mut s, mut e) = (start, end);
        // Merge with a predecessor that reaches `start`.
        if let Some((&k, &pe)) = self.ranges.range(..=key(id, start)).next_back() {
            if kid(k) == id && pe >= start {
                s = koff(k);
                e = e.max(pe);
                self.ranges.remove(&k);
            }
        }
        // Absorb successors that start within `[s, e]`.
        loop {
            let next = self
                .ranges
                .range(key(id, s)..=key(id, e))
                .next()
                .map(|(&k, &v)| (k, v));
            match next {
                Some((k, v)) => {
                    e = e.max(v);
                    self.ranges.remove(&k);
                }
                None => break,
            }
        }
        self.ranges.insert(key(id, s), e);
    }
}

/// The store's in-memory state: the committed file, plus what a flush in
/// progress has taken.
#[derive(Debug)]
pub struct State {
    pub frags: BTreeMap<Key, Fragment>,
    pub slab: Slab,
    pub allocs: IdMap<AllocationMeta>,
    pub recyclable: BTreeMap<u32, Recyclable>,
    pub pages: Vec<PageInfo>,
    /// Live data pages and leaves ranked by ripeness.
    pub ripeness: Ripeness,
    /// Whether coverage the flush releases now is a natural loss: one the
    /// application's writes, frees, and shrinks caused, as opposed to
    /// consolidation moving content.
    pub natural: bool,
    /// Page -> `(lost, live)`: the natural losses of the flush in progress,
    /// and the page's coverage before the first of them.
    pub losses: IdMap<(u32, u32)>,
    /// Data page -> `(id, lo, hi)`: ids that may have bytes in it, and the
    /// window of offsets they may occupy. A superset, pruned on use.
    pub reverse: IdMap<Vec<(u32, u32, u32)>>,
    /// The flush's pending table.
    pub pending: Vec<Pending>,
    /// Pages whose coverage fell during the flush.
    pub dropped_candidates: IdSet,
    /// The epoch of the flush in progress: pages written in it are never
    /// victims.
    pub flush_epoch: u64,
}

impl Default for State {
    fn default() -> Self {
        State {
            frags: BTreeMap::new(),
            slab: Slab::default(),
            allocs: IdMap::default(),
            recyclable: BTreeMap::new(),
            pages: Vec::new(),
            ripeness: Ripeness::default(),
            natural: false,
            losses: IdMap::default(),
            reverse: IdMap::default(),
            pending: Vec::new(),
            dropped_candidates: IdSet::default(),
            flush_epoch: 0,
        }
    }
}

/// The data page and in-page offset of a file address.
#[inline]
pub fn split_address(addr: u64) -> (u32, u16) {
    (
        (addr / PAGE_SIZE as u64) as u32,
        (addr % PAGE_SIZE as u64) as u16,
    )
}

#[inline]
pub fn address(page: u32, offset: u16) -> u64 {
    page as u64 * PAGE_SIZE as u64 + offset as u64
}

impl State {
    // ---------------------------------------------------------------- pages

    pub fn ensure_page(&mut self, page: u32) {
        if self.pages.len() <= page as usize {
            self.pages.resize(page as usize + 1, PageInfo::default());
        }
    }

    /// Marks `page` for ranking again: its coverage, state, or estimate
    /// changed.
    #[inline]
    pub fn rerank(&mut self, page: u32) {
        self.ripeness.stale.insert(page);
    }

    /// Takes `page` out of the ranking at once, before its state changes.
    pub fn unrank(&mut self, page: u32) {
        self.ripeness.remove(page);
        self.ripeness.stale.remove(&page);
    }

    /// Ranks every stale page again as of `now`: a live data page or leaf
    /// with coverage below `packed` bytes, the fill survivors are packed at,
    /// written before the flush in progress; anything else leaves the
    /// ranking. Its fill counts relative to `packed`, in `h`, or in the
    /// myopic `g` if `myopic`.
    pub fn refresh_ranking(&mut self, now: u64, packed: f64, myopic: bool) {
        let fill = if myopic {
            crate::ripeness::g
        } else {
            crate::ripeness::h
        };
        for page in std::mem::take(&mut self.ripeness.stale) {
            let Some(info) = self.pages.get(page as usize) else {
                self.ripeness.remove(page);
                continue;
            };
            let live = matches!(info.state, PageState::Data | PageState::Table);
            let coverage = info.coverage as f64;
            if live && info.coverage > 0 && coverage < packed && info.epoch < now {
                let (key, floor) = crate::ripeness::keys(&info.drain, coverage, packed, fill);
                self.ripeness.insert(page, key, floor, now);
            } else {
                self.ripeness.remove(page);
            }
        }
    }

    #[inline]
    pub fn cover(&mut self, page: u32, n: u32) {
        if n == 0 {
            return;
        }
        self.pages[page as usize].coverage += n;
        self.rerank(page);
    }

    #[inline]
    pub fn uncover(&mut self, page: u32, n: u32) {
        if n == 0 {
            return;
        }
        let info = &mut self.pages[page as usize];
        debug_assert!(info.coverage >= n, "coverage of page {page} underflows");
        if self.natural {
            let e = self.losses.entry(page).or_insert((0, info.coverage));
            e.0 += n;
        } else if info.coverage > n {
            // Content moved out: the estimate shrinks with the page. A page
            // emptied keeps its estimate, which its survivors carry along.
            let keep = (info.coverage - n) as f64 / info.coverage as f64;
            info.drain.shrink(keep);
        }
        info.coverage -= n;
        self.dropped_candidates.insert(page);
        self.rerank(page);
    }

    // ------------------------------------------------------------ allocations

    #[inline]
    pub fn size_of(&self, id: u32) -> u32 {
        self.allocs.get(&id).map_or(0, |m| m.size)
    }

    /// Where the fragment at `k` ends: the next key of its id, or the size.
    pub fn frag_end(&self, k: Key) -> u32 {
        let id = kid(k);
        match self.frags.range(k + 1..).next() {
            Some((&n, _)) if kid(n) == id => koff(n),
            _ => self.size_of(id),
        }
    }

    /// The fragment covering `(id, probe)` and its range.
    pub fn resolve(&self, id: u32, probe: u32) -> Option<(Key, Fragment, u32)> {
        let size = self.size_of(id);
        if probe >= size {
            return None;
        }
        let (&k, &f) = self.frags.range(..=key(id, probe)).next_back()?;
        debug_assert_eq!(kid(k), id, "the partition invariant");
        Some((k, f, self.frag_end(k)))
    }

    // ------------------------------------------------------------ pins

    #[inline]
    pub fn pin(&mut self, s: StmtRef) {
        self.slab.pins[s.idx()] += 1;
    }

    pub fn unpin(&mut self, s: StmtRef) {
        let i = s.idx();
        debug_assert!(self.slab.pins[i] > 0, "unpinning a dead statement");
        self.slab.pins[i] -= 1;
        if self.slab.pins[i] == 0 {
            // Read the page before the slot becomes a free-list link.
            let page = self.slab.page_or_next[i];
            let framing = self.slab.framing[i] as u32;
            let id = self.slab.ids[i];
            self.uncover(page, framing);
            if let Some(m) = self.allocs.get_mut(&id) {
                m.statement_bytes -= framing;
            }
            self.slab.release(s);
        }
    }

    fn release_fragment(&mut self, f: Fragment, len: u32) {
        match f {
            Fragment::Bytes { page, stmt, .. } => {
                self.uncover(page, len);
                self.unpin(stmt);
            }
            Fragment::ZeroExplicitly { stmt } => self.unpin(stmt),
            Fragment::ZeroByDefault => {}
            Fragment::Pending(p) => {
                if let Place::Data(a) = self.pending[p as usize].place {
                    self.uncover(split_address(a).0, len);
                }
            }
        }
    }

    fn charge_fragment(&mut self, f: Fragment, len: u32) {
        match f {
            Fragment::Bytes { page, stmt, .. } => {
                self.cover(page, len);
                self.pin(stmt);
            }
            Fragment::ZeroExplicitly { stmt } => self.pin(stmt),
            Fragment::ZeroByDefault => {}
            Fragment::Pending(p) => {
                if let Place::Data(a) = self.pending[p as usize].place {
                    self.cover(split_address(a).0, len);
                }
            }
        }
    }

    // ------------------------------------------------------------ primitives

    /// Ensures a fragment boundary at `at`.
    pub fn split(&mut self, id: u32, at: u32) {
        if at >= self.size_of(id) {
            return;
        }
        let Some((&k, &f)) = self.frags.range(..=key(id, at)).next_back() else {
            return;
        };
        debug_assert_eq!(kid(k), id);
        if koff(k) == at {
            return;
        }
        let d = at - koff(k);
        let shifted = match f {
            Fragment::Bytes { page, offset, stmt } => Fragment::Bytes {
                page,
                offset: offset + d as u16,
                stmt,
            },
            Fragment::Pending(p) => {
                let q = self.pending[p as usize].advanced(d);
                self.pending.push(q);
                Fragment::Pending(self.pending.len() as u32 - 1)
            }
            other => other,
        };
        if let Some(s) = shifted.stmt() {
            self.pin(s);
        }
        self.frags.insert(key(id, at), shifted);
        self.allocs.get_mut(&id).unwrap().fragment_count += 1;
    }

    /// Replaces the content of `[start, end)` of `id` with `new`.
    pub fn overwrite(&mut self, id: u32, start: u32, end: u32, new: Fragment) {
        debug_assert!(start < end && end <= self.size_of(id));
        self.split(id, start);
        self.split(id, end);
        let olds: Vec<(Key, Fragment)> = self
            .frags
            .range(key(id, start)..key(id, end))
            .map(|(&k, &f)| (k, f))
            .collect();
        for (i, &(k, f)) in olds.iter().enumerate() {
            let e = olds.get(i + 1).map_or(end, |&(n, _)| koff(n));
            self.release_fragment(f, e - koff(k));
            self.frags.remove(&k);
        }
        let m = self.allocs.get_mut(&id).unwrap();
        m.fragment_count -= olds.len() as u32;
        m.fragment_count += 1;
        self.frags.insert(key(id, start), new);
        self.charge_fragment(new, end - start);
        self.coalesce_at(id, start);
        if end < self.size_of(id) {
            self.coalesce_at(id, end);
        }
    }

    /// Whether `b`, starting where `a` (of length `a_len`) ends, continues it.
    fn continues(&self, a: Fragment, a_len: u32, b: Fragment) -> bool {
        match (a, b) {
            (Fragment::ZeroByDefault, Fragment::ZeroByDefault) => true,
            (Fragment::ZeroExplicitly { stmt: s }, Fragment::ZeroExplicitly { stmt: t }) => s == t,
            (
                Fragment::Bytes {
                    page: p,
                    offset: o,
                    stmt: s,
                },
                Fragment::Bytes {
                    page: q,
                    offset: r,
                    stmt: t,
                },
            ) => s == t && p == q && o as u32 + a_len == r as u32,
            _ => false,
        }
    }

    /// Merges the fragment starting at `(id, at)` into its predecessor if it
    /// continues it.
    pub fn coalesce_at(&mut self, id: u32, at: u32) {
        if at == 0 {
            return;
        }
        let Some(&b) = self.frags.get(&key(id, at)) else {
            return;
        };
        let Some((&pk, &a)) = self.frags.range(..key(id, at)).next_back() else {
            return;
        };
        if kid(pk) != id {
            return;
        }
        if self.continues(a, at - koff(pk), b) {
            self.frags.remove(&key(id, at));
            if let Some(s) = b.stmt() {
                self.unpin(s);
            }
            self.allocs.get_mut(&id).unwrap().fragment_count -= 1;
        }
    }

    // ------------------------------------------------------------ sizes

    /// Anchors the id at `s`, a `Shrink` or `Tombstone` with bound `n`.
    pub fn set_anchor(&mut self, id: u32, s: StmtRef, n: u32) {
        let old = {
            let m = self.allocs.get_mut(&id).unwrap();
            let old = m.anchor.replace(s);
            m.anchor_n = n;
            old
        };
        self.pin(s);
        if let Some(o) = old {
            self.unpin(o);
        }
        self.retire_dead_grows(id);
    }

    /// Records `s`, a `Grow` with bound `n`, as a possible sole witness.
    pub fn set_grow_witness(&mut self, id: u32, s: StmtRef, n: u32) {
        let old = {
            let m = self.allocs.get_mut(&id).unwrap();
            let old = m.grow.replace(s);
            m.grow_n = n;
            old
        };
        self.pin(s);
        if let Some(o) = old {
            self.unpin(o);
        }
        self.retire_dead_grows(id);
    }

    /// The death tests of `impl/liveness.md` for the grow witness.
    pub fn retire_dead_grows(&mut self, id: u32) {
        let Some(m) = self.allocs.get(&id) else {
            return;
        };
        let Some(g) = m.grow else { return };
        let mut dead = m.size > m.grow_n;
        if let Some(a) = m.anchor {
            let anchor_is_shrink = self.slab.kinds[a.idx()] == Kind::Shrink;
            // A tombstone anchor asserts no existence, so a `Grow` it bounds
            // may still be the only statement that does.
            if anchor_is_shrink && m.grow_n <= m.anchor_n {
                dead = true;
            }
            let ga = self.pages[self.slab.page(g) as usize].epoch;
            let aa = self.pages[self.slab.page(a) as usize].epoch;
            if ga < aa {
                dead = true;
            }
        }
        if dead {
            self.allocs.get_mut(&id).unwrap().grow = None;
            self.unpin(g);
        }
    }

    /// Inserts `f` over `[from, to)` at the end of `id`, extending its last
    /// fragment when that already resolves the same way.
    fn append_range(&mut self, id: u32, from: u32, to: u32, f: Fragment) {
        if from >= to {
            return;
        }
        if from > 0 {
            if let Some((&pk, &last)) = self.frags.range(..key(id, from)).next_back() {
                if kid(pk) == id && last == f && !matches!(f, Fragment::Pending(_)) {
                    // Extending: the fragment now reaches further, which for a
                    // statement-owned zero needs no new pin.
                    return;
                }
            }
        }
        self.frags.insert(key(id, from), f);
        if let Some(s) = f.stmt() {
            self.pin(s);
        }
        self.allocs.get_mut(&id).unwrap().fragment_count += 1;
    }

    /// Raises `id`'s size to `n`: the exposed range resolves through the
    /// anchor if there is one, else by default.
    pub fn grow_size_to(&mut self, id: u32, n: u32) {
        let (old, anchor) = {
            let m = self.allocs.get_mut(&id).unwrap();
            if n <= m.size {
                return;
            }
            let old = m.size;
            m.size = n;
            (old, m.anchor)
        };
        let f = match anchor {
            Some(a) => Fragment::ZeroExplicitly { stmt: a },
            None => Fragment::ZeroByDefault,
        };
        self.append_range(id, old, n, f);
        self.retire_dead_grows(id);
    }

    /// Lowers `id`'s size to `n`, dropping everything at or past it.
    pub fn shrink_size_to(&mut self, id: u32, n: u32) {
        let size = self.size_of(id);
        if n >= size {
            return;
        }
        self.split(id, n);
        let olds: Vec<(Key, Fragment)> = self
            .frags
            .range(key(id, n)..=key(id, u32::MAX))
            .map(|(&k, &f)| (k, f))
            .collect();
        for (i, &(k, f)) in olds.iter().enumerate() {
            let e = olds.get(i + 1).map_or(size, |&(nk, _)| koff(nk));
            self.release_fragment(f, e - koff(k));
            self.frags.remove(&k);
        }
        let m = self.allocs.get_mut(&id).unwrap();
        m.fragment_count -= olds.len() as u32;
        m.size = n;
        self.retire_dead_grows(id);
    }

    /// Re-owns `[start, end)` of `id` for the flush in progress.
    pub fn take(&mut self, id: u32, start: u32, end: u32, p: Pending, dirty: &mut Dirty) {
        if start >= end {
            return;
        }
        self.pending.push(p);
        let idx = self.pending.len() as u32 - 1;
        self.overwrite(id, start, end, Fragment::Pending(idx));
        dirty.add(id, start, end);
    }

    /// A statement naming `id`, in table page `page`, stopped being
    /// physically present.
    ///
    /// When only the tombstone is left naming a non-existent id, nothing is
    /// left for it to deny, so it is released. But when the statement leaving
    /// *is* the tombstone -- its page is being rewritten -- what remains are
    /// the statements it denies, and it stays, for the cut to restate: an id
    /// re-allocated in the same flush anchors on it.
    pub fn drop_physically(&mut self, id: u32, page: u32) {
        if let Some(m) = self.allocs.get_mut(&id) {
            m.mentions = m.mentions.saturating_sub(1);
            return;
        }
        let Some(r) = self.recyclable.get_mut(&id) else {
            return;
        };
        r.mentions = r.mentions.saturating_sub(1);
        let leaving = r.tombstone.is_some_and(|t| self.slab.page(t) == page);
        if r.mentions <= 1 && !leaving {
            if let Some(t) = r.tombstone.take() {
                self.unpin(t);
            }
        }
    }

    /// `mentions` of an id, existing or not.
    pub fn mentions(&self, id: u32) -> u32 {
        self.allocs.get(&id).map_or_else(
            || self.recyclable.get(&id).map_or(0, |r| r.mentions),
            |m| m.mentions,
        )
    }

    /// Records in the reverse index that `id` may have bytes at `[lo, hi)`
    /// in data page `page`.
    pub fn note_reverse(&mut self, page: u32, id: u32, lo: u32, hi: u32) {
        let v = self.reverse.entry(page).or_default();
        for e in v.iter_mut() {
            if e.0 == id {
                e.1 = e.1.min(lo);
                e.2 = e.2.max(hi);
                return;
            }
        }
        v.push((id, lo, hi));
    }

    /// Checks the invariants tying the structures together. Expensive; for
    /// tests and debug assertions.
    pub fn check(&self) {
        let mut pins = vec![0u32; self.slab.pins.len()];
        let mut coverage: IdMap<u32> = IdMap::default();
        let mut per_id: IdMap<u32> = IdMap::default();
        let mut last: Option<(u32, u32)> = None;
        let keys: Vec<(Key, Fragment)> = self.frags.iter().map(|(&k, &f)| (k, f)).collect();
        for (i, &(k, f)) in keys.iter().enumerate() {
            let id = kid(k);
            let size = self.size_of(id);
            assert!(
                self.allocs.contains_key(&id),
                "fragment of a non-existent id {id}"
            );
            assert!(
                koff(k) < size,
                "fragment at {} past size {size} of id {id}",
                koff(k)
            );
            match last {
                Some((lid, _)) if lid == id => {}
                _ => assert_eq!(koff(k), 0, "id {id} does not start at 0"),
            }
            let end = keys
                .get(i + 1)
                .filter(|(n, _)| kid(*n) == id)
                .map_or(size, |(n, _)| koff(*n));
            *per_id.entry(id).or_default() += 1;
            if let Some(s) = f.stmt() {
                pins[s.idx()] += 1;
            }
            match f {
                Fragment::Bytes { page, .. } => *coverage.entry(page).or_default() += end - koff(k),
                Fragment::Pending(p) => {
                    if let Place::Data(a) = self.pending[p as usize].place {
                        *coverage.entry(split_address(a).0).or_default() += end - koff(k);
                    }
                }
                _ => {}
            }
            last = Some((id, end));
        }
        for (&id, m) in &self.allocs {
            assert_eq!(
                per_id.get(&id).copied().unwrap_or(0),
                m.fragment_count,
                "fragment_count of {id}"
            );
            if m.size > 0 {
                assert!(
                    per_id.contains_key(&id),
                    "id {id} of size {} has no fragments",
                    m.size
                );
            }
            if let Some(a) = m.anchor {
                pins[a.idx()] += 1;
            }
            if let Some(g) = m.grow {
                pins[g.idx()] += 1;
            }
        }
        for r in self.recyclable.values() {
            if let Some(t) = r.tombstone {
                pins[t.idx()] += 1;
            }
        }
        for s in 1..pins.len() {
            let live = self.slab.pins[s] > 0;
            if live {
                assert_eq!(pins[s], self.slab.pins[s], "pins of statement {s}");
                *coverage.entry(self.slab.page_or_next[s]).or_default() +=
                    self.slab.framing[s] as u32;
            } else {
                assert_eq!(pins[s], 0, "a dead statement {s} is referenced");
            }
        }
        for (p, info) in self.pages.iter().enumerate() {
            if matches!(info.state, PageState::Header) {
                continue;
            }
            let want = coverage.get(&(p as u32)).copied().unwrap_or(0);
            // Interior pages carry child references, which only they count.
            if !matches!(info.state, PageState::Interior) {
                assert_eq!(
                    info.coverage, want,
                    "coverage of page {p} ({:?})",
                    info.state
                );
            }
        }
        let _: BTreeSet<u32> = BTreeSet::new();
    }
}
