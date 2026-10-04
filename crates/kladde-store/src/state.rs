//! The in-memory structures of `impl/in-memory-state.md` and the
//! fragment-map primitives of `impl/address-table-operations.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;

use crate::consts::{MAX_PAGE_CONTENT, PAGE_SIZE};
use crate::hash::{IdMap, IdSet};
use crate::statement::{Kind, TableWriter};

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
    /// Zeros stated by a `Zero`.
    Zero { stmt: StmtRef },
    /// Taken by the flush in progress; an index into its pending table.
    Pending(u32),
}

impl Fragment {
    #[inline]
    pub fn stmt(&self) -> Option<StmtRef> {
        match *self {
            Fragment::Bytes { stmt, .. } | Fragment::Zero { stmt } => Some(stmt),
            Fragment::Pending(_) => None,
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
    /// The newest statement that states the size, holding its `S` pin; for
    /// an id allocated again, its old tombstone until the cut binds the new
    /// incarnation's.
    pub size_statement: Option<StmtRef>,
}

/// What a non-existent id still carries.
#[derive(Clone, Copy, Debug, Default)]
pub struct Recyclable {
    pub mentions: u32,
    pub tombstone: Option<StmtRef>,
}

/// A page of the file, in the states of `spec/durability.md#page-states`,
/// plus one of the implementation's own: filling.
///
/// Only a filling or live page has a kind and coverage, and only a live page
/// an epoch and a place among the victims.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    /// In neither header's world: the flush may write it. The header slot
    /// the next commit overwrites is reusable after a load, but never joins
    /// the ready pool.
    Reusable,
    /// In the world of the older header only; reusable after the next commit.
    Fallback,
    /// Part of the current journal segment, or named by the header for it.
    /// Live in the sense of the spec, but the segment keeps what it holds.
    Journal,
    /// Taken by the flush in progress, which writes it at its commit: its
    /// epoch is that flush's. The spec counts it reusable until the commit;
    /// a failed flush poisons the store, so nothing ever reads a page that a
    /// flush took and did not commit.
    Filling(Filling),
    /// In the governing header's world.
    Live(Live),
}

/// What a page the flush in progress writes holds so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Filling {
    /// As [`Live::coverage`].
    pub coverage: u32,
    pub kind: PageKind,
}

/// What a live page holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Live {
    /// The epoch the page was written in.
    pub epoch: u64,
    /// Bytes current state relies on: data a fragment resolves to, statement
    /// framing and inline payloads, and a non-header page's child references.
    pub coverage: u32,
    pub kind: PageKind,
    /// Where it sits among the victims of its kind, if it is one.
    pub slot: Option<BucketSlot>,
}

/// The kind of a filling or live page, as its frame states it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageKind {
    Data,
    /// A non-header address-table page, with or without children; the tree
    /// is [`State::children`].
    Table,
    /// The governing header, or the slot the flush in progress writes.
    Header,
}

/// Where a page sits in the victim buckets of its kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BucketSlot {
    pub bucket: u8,
    pub index: u32,
}

impl Filling {
    /// A page of kind `kind` that the flush in progress takes to write.
    pub fn taken(kind: PageKind) -> Page {
        Page::Filling(Filling { coverage: 0, kind })
    }

    /// The page once the commit of epoch `epoch` has written it. It joins
    /// the victims, if it is one, when it is rebucketed.
    pub fn commit(self, epoch: u64) -> Live {
        Live {
            epoch,
            coverage: self.coverage,
            kind: self.kind,
            slot: None,
        }
    }
}

/// Buckets per page kind.
pub const BUCKETS: usize = 16;

/// Pages bucketed by live fraction, for `O(1)` victim selection.
#[derive(Debug, Default)]
pub struct Buckets {
    pub lists: [Vec<u32>; BUCKETS],
}

impl Buckets {
    pub fn bucket_of(coverage: u32) -> u8 {
        ((coverage as usize * BUCKETS) / (MAX_PAGE_CONTENT + 1)).min(BUCKETS - 1) as u8
    }

    /// The sparsest non-empty bucket.
    pub fn lowest_non_empty(&self) -> Option<usize> {
        self.lists.iter().position(|l| !l.is_empty())
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
    /// A page the flush retires held its size statement, or, for an id that
    /// does not exist, its tombstone.
    pub replace: bool,
    /// The application wrote it.
    pub written: bool,
}

impl IdRecord {
    /// Folds in what another source recorded for the same id; the earlier
    /// size stays, since it is the size the id had when the flush began.
    pub fn merge(&mut self, other: IdRecord) {
        self.freed |= other.freed;
        self.allocated |= other.allocated;
        self.replace |= other.replace;
        self.written |= other.written;
    }
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
#[derive(Debug, Default)]
pub struct State {
    pub frags: BTreeMap<Key, Fragment>,
    pub slab: Slab,
    pub allocs: IdMap<AllocationMeta>,
    pub recyclable: BTreeMap<u32, Recyclable>,
    /// Every page of the file, by page number.
    pub pages: Vec<Page>,
    /// The address-table tree: each live table page's child references, the
    /// header's included; empty for a page without children.
    pub children: IdMap<Vec<u32>>,
    pub data_buckets: Buckets,
    pub table_buckets: Buckets,
    /// Data page -> `(id, lo, hi)`: ids that may have bytes in it, and the
    /// window of offsets they may occupy. A superset, pruned on use.
    pub reverse: IdMap<Vec<(u32, u32, u32)>>,
    /// The flush's pending table.
    pub pending: Vec<Pending>,
    /// Pages whose coverage fell during the flush.
    pub dropped_candidates: IdSet,
    /// The epoch of the flush in progress, and of every page it fills; or
    /// between flushes, the governing header's.
    pub flush_epoch: u64,
}

/// The bytes of a non-header table page's child list that its coverage
/// carries: the list with its delimiter, or nothing for a page without
/// children, whose delimiter alone must not keep it alive.
pub fn charged_child_bytes(children: &[u32]) -> u32 {
    if children.is_empty() {
        0
    } else {
        TableWriter::children_len(children) as u32
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

    /// Makes room for page records up to `page`, which the file now holds:
    /// a page that comes into existence is reusable.
    pub fn ensure_page(&mut self, page: u32) {
        if self.pages.len() <= page as usize {
            self.pages.resize(page as usize + 1, Page::Reusable);
        }
    }

    /// The record of `page`, which must be live.
    #[inline]
    pub fn live(&self, page: u32) -> &Live {
        match &self.pages[page as usize] {
            Page::Live(l) => l,
            other => panic!("page {page} is not live but {other:?}"),
        }
    }

    #[inline]
    pub fn live_mut(&mut self, page: u32) -> &mut Live {
        match &mut self.pages[page as usize] {
            Page::Live(l) => l,
            other => panic!("page {page} is not live but {other:?}"),
        }
    }

    /// The epoch of `page`, which holds content that current state refers
    /// to: its own if it is live, the flush's if it is filling.
    #[inline]
    pub fn epoch_of(&self, page: u32) -> u64 {
        match &self.pages[page as usize] {
            Page::Live(l) => l.epoch,
            Page::Filling(_) => self.flush_epoch,
            other => panic!("page {page} holds no content but is {other:?}"),
        }
    }

    /// The coverage of `page`, which must be filling or live.
    #[inline]
    fn coverage_mut(&mut self, page: u32) -> &mut u32 {
        match &mut self.pages[page as usize] {
            Page::Live(Live { coverage, .. }) | Page::Filling(Filling { coverage, .. }) => coverage,
            other => panic!("page {page} holds no content but is {other:?}"),
        }
    }

    pub fn is_filling(&self, page: u32) -> bool {
        matches!(self.pages[page as usize], Page::Filling(_))
    }

    /// Whether table page `page` names other pages.
    pub fn has_children(&self, page: u32) -> bool {
        self.children.get(&page).is_some_and(|k| !k.is_empty())
    }

    /// The record of `page` if it may be a consolidation victim: a live data
    /// page or leaf with live content. Pages with children are rebuilt by
    /// every cut that needs them, and so are never victims.
    pub fn victim(&self, page: u32) -> Option<&Live> {
        match &self.pages[page as usize] {
            Page::Live(l)
                if l.coverage > 0
                    && match l.kind {
                        PageKind::Data => true,
                        PageKind::Table => !self.has_children(page),
                        PageKind::Header => false,
                    } =>
            {
                Some(l)
            }
            _ => None,
        }
    }

    /// The bucket `page` belongs in: victims by coverage, and nothing for
    /// any other page.
    fn wanted_bucket(&self, page: u32) -> Option<u8> {
        self.victim(page).map(|l| Buckets::bucket_of(l.coverage))
    }

    fn current_bucket(&self, page: u32) -> Option<u8> {
        match &self.pages[page as usize] {
            Page::Live(l) => l.slot.map(|s| s.bucket),
            _ => None,
        }
    }

    fn buckets_of(&mut self, kind: PageKind) -> &mut Buckets {
        match kind {
            PageKind::Data => &mut self.data_buckets,
            PageKind::Table => &mut self.table_buckets,
            PageKind::Header => unreachable!("a header is never a victim"),
        }
    }

    /// Moves `page` to the bucket its coverage calls for, or out of buckets.
    pub fn rebucket(&mut self, page: u32) {
        let want = self.wanted_bucket(page);
        if self.current_bucket(page) == want {
            return;
        }
        self.unbucket(page);
        if let Some(bucket) = want {
            let kind = self.live(page).kind;
            let list = &mut self.buckets_of(kind).lists[bucket as usize];
            let index = list.len() as u32;
            list.push(page);
            self.live_mut(page).slot = Some(BucketSlot { bucket, index });
        }
    }

    /// Takes `page` out of its bucket, before it stops being live.
    pub fn unbucket(&mut self, page: u32) {
        let Page::Live(Live {
            kind,
            slot: Some(s),
            ..
        }) = self.pages[page as usize]
        else {
            return;
        };
        let list = &mut self.buckets_of(kind).lists[s.bucket as usize];
        let index = s.index as usize;
        list.swap_remove(index);
        if index < list.len() {
            let moved = list[index];
            if let Some(m) = &mut self.live_mut(moved).slot {
                m.index = index as u32;
            }
        }
        self.live_mut(page).slot = None;
    }

    #[inline]
    pub fn cover(&mut self, page: u32, n: u32) {
        if n == 0 {
            return;
        }
        *self.coverage_mut(page) += n;
        if self.wanted_bucket(page) != self.current_bucket(page) {
            self.rebucket(page);
        }
    }

    #[inline]
    pub fn uncover(&mut self, page: u32, n: u32) {
        if n == 0 {
            return;
        }
        let coverage = self.coverage_mut(page);
        debug_assert!(*coverage >= n, "coverage of page {page} underflows");
        *coverage -= n;
        self.dropped_candidates.insert(page);
        if self.wanted_bucket(page) != self.current_bucket(page) {
            self.rebucket(page);
        }
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
            Fragment::Zero { stmt } => self.unpin(stmt),
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
            Fragment::Zero { stmt } => self.pin(stmt),
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
            (Fragment::Zero { stmt: s }, Fragment::Zero { stmt: t }) => s == t,
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

    /// Makes `s` the id's size statement, which takes its `S` pin from the
    /// statement that held it before.
    pub fn set_size_statement(&mut self, id: u32, s: StmtRef) {
        self.pin(s);
        let old = self.allocs.get_mut(&id).unwrap().size_statement.replace(s);
        if let Some(o) = old {
            self.unpin(o);
        }
    }

    /// Raises `id`'s size to `n`. The range the growth exposes is taken for
    /// the flush as `p`, zeros unless the flush writes over them, since every
    /// byte an allocation gains is stated in the epoch that gains it.
    pub fn grow_size_to(&mut self, id: u32, n: u32, p: Pending, dirty: &mut Dirty) {
        let old = {
            let m = self.allocs.get_mut(&id).unwrap();
            if n <= m.size {
                return;
            }
            let old = m.size;
            m.size = n;
            m.fragment_count += 1;
            old
        };
        // Past the old end there is nothing to release, so this take only
        // appends.
        self.pending.push(p);
        let idx = self.pending.len() as u32 - 1;
        self.frags.insert(key(id, old), Fragment::Pending(idx));
        self.charge_fragment(Fragment::Pending(idx), n - old);
        dirty.add(id, old, n);
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

    /// A statement naming `id` stopped being physically present.
    ///
    /// When at most the tombstone is left naming a non-existent id, nothing
    /// is left for it to deny, so it is released. When the statement leaving
    /// is the tombstone itself, its page's rewrite has asked the cut to state
    /// it again while anything still names the id.
    pub fn drop_physically(&mut self, id: u32) {
        if let Some(m) = self.allocs.get_mut(&id) {
            m.mentions = m.mentions.saturating_sub(1);
            return;
        }
        let Some(r) = self.recyclable.get_mut(&id) else {
            return;
        };
        r.mentions = r.mentions.saturating_sub(1);
        if r.mentions <= 1 {
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
            if let Some(s) = m.size_statement {
                pins[s.idx()] += 1;
            }
        }
        for r in self.recyclable.values() {
            if let Some(t) = r.tombstone {
                pins[t.idx()] += 1;
            }
        }
        // `s` indexes four parallel arrays, the counts here and the slab's
        // pins, pages and framing, so it stays an index.
        #[allow(clippy::needless_range_loop)]
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
        for (p, page) in self.pages.iter().enumerate() {
            let p = p as u32;
            let want = coverage.get(&p).copied().unwrap_or(0);
            match page {
                Page::Live(l) => {
                    // A non-header page's child references are charged to
                    // it; the header's, which every flush rewrites, are not.
                    let kids = match l.kind {
                        PageKind::Header => 0,
                        _ => self.children.get(&p).map_or(0, |k| charged_child_bytes(k)),
                    };
                    assert_eq!(l.coverage, want + kids, "coverage of page {p} ({l:?})");
                    assert_eq!(
                        l.slot.map(|s| s.bucket),
                        self.wanted_bucket(p),
                        "bucket of page {p} ({l:?})"
                    );
                    if let Some(s) = l.slot {
                        let buckets = match l.kind {
                            PageKind::Data => &self.data_buckets,
                            _ => &self.table_buckets,
                        };
                        let list = &buckets.lists[s.bucket as usize];
                        assert_eq!(list.get(s.index as usize), Some(&p), "slot of page {p}");
                    }
                }
                Page::Filling(f) => panic!("page {p} is filling outside a flush: {f:?}"),
                other => assert_eq!(want, 0, "live content in page {p}, which is {other:?}"),
            }
        }
        let _: BTreeSet<u32> = BTreeSet::new();
    }
}
