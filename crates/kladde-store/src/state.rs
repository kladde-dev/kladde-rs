//! The committed state of a store in memory: the span table, the rope over
//! the sequence's spans, the short chains of the schema and the private
//! state, the page table with its victim buckets, and the reverse lists from
//! a page to the spans it holds.

use crate::consts::*;
use crate::hash::{IdMap, IdSet};
use crate::treap::{Item, Treap, NIL};

/// No page.
pub const NONE: u32 = u32::MAX;

/// Which chain a span belongs to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Chain {
    /// The span is free.
    #[default]
    None,
    Sequence,
    Schema,
    Private,
}

/// What a span shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Content {
    /// The span id is free.
    #[default]
    Free,
    /// Bytes in a data page, at a byte offset within the page.
    Ref { page: u32, offset: u16 },
    /// The payload of its entry, at a byte offset within the table page that
    /// holds the entry.
    Inline { page: u32, offset: u16 },
    /// Zeros.
    Zero,
    /// Bytes the flush in progress holds and has not placed yet: an index
    /// into its pending table.
    Pending(u32),
}

/// A span's record in the span table.
#[derive(Clone, Copy, Debug, Default)]
pub struct Span {
    pub len: u32,
    /// The next span of its chain, or 0.
    pub next: u32,
    pub content: Content,
    pub chain: Chain,
    /// The table page holding its live entry, or [`NONE`] while it has none.
    pub entry_page: u32,
    /// The bytes its live entry takes in that page, payload included.
    pub entry_len: u16,
    /// Its place in that page's entry list.
    pub tab: u32,
    /// Its place in its data page's span list, for a `Ref`.
    pub rev: u32,
    /// Its node in the rope, for a span of the sequence.
    pub node: u32,
    /// The flush that last wrote its bytes for the application, which ranks
    /// it for the header.
    pub heat: u64,
}

/// What the rope holds: a span, or during a flush a run of bytes the flush
/// holds that has no span yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RopeItem {
    Span {
        id: u32,
        len: u32,
    },
    /// `len` bytes of the flush's scratch, from `start`.
    Lit {
        start: u32,
        len: u32,
    },
}

impl Default for RopeItem {
    fn default() -> Self {
        RopeItem::Span { id: 0, len: 0 }
    }
}

impl Item for RopeItem {
    fn len(&self) -> u64 {
        match *self {
            RopeItem::Span { len, .. } | RopeItem::Lit { len, .. } => len as u64,
        }
    }
}

/// Hands out span ids lowest first, from a bitmap of ids in use with a
/// summary bit per word that is full.
#[derive(Debug)]
pub struct Ids {
    used: Vec<u64>,
    full: Vec<u64>,
    count: u32,
}

impl Default for Ids {
    fn default() -> Self {
        let mut ids = Ids {
            used: Vec::new(),
            full: Vec::new(),
            count: 0,
        };
        // Id 0 is never a span.
        ids.mark(0);
        ids.count = 0;
        ids
    }
}

impl Ids {
    pub fn is_used(&self, id: u32) -> bool {
        let w = (id / 64) as usize;
        w < self.used.len() && self.used[w] & (1 << (id % 64)) != 0
    }

    /// Marks `id` in use.
    pub fn mark(&mut self, id: u32) {
        let w = (id / 64) as usize;
        if w >= self.used.len() {
            self.used.resize(w + 1, 0);
            self.full.resize(self.used.len().div_ceil(64), 0);
        }
        debug_assert!(self.used[w] & (1 << (id % 64)) == 0, "span id {id} in use");
        self.used[w] |= 1 << (id % 64);
        if self.used[w] == u64::MAX {
            self.full[w / 64] |= 1 << (w % 64);
        }
        self.count += 1;
    }

    /// Frees `id`.
    pub fn free(&mut self, id: u32) {
        let w = (id / 64) as usize;
        debug_assert!(self.is_used(id), "freeing span id {id}, which is free");
        self.used[w] &= !(1 << (id % 64));
        self.full[w / 64] &= !(1 << (w % 64));
        self.count -= 1;
    }

    /// The lowest free id, marked in use; or `after + 1` if that is free,
    /// so that spans made one after another get consecutive ids.
    pub fn alloc(&mut self, after: u32) -> Option<u32> {
        if after != 0 && after < u32::MAX && !self.is_used(after + 1) {
            self.mark(after + 1);
            return Some(after + 1);
        }
        for (i, &f) in self.full.iter().enumerate() {
            if f != u64::MAX {
                let w = i * 64 + (!f).trailing_zeros() as usize;
                if w < self.used.len() {
                    let id = (w * 64) as u32 + (!self.used[w]).trailing_zeros();
                    self.mark(id);
                    return Some(id);
                }
                break;
            }
        }
        let id = (self.used.len() * 64) as u32;
        if id == u32::MAX {
            return None;
        }
        self.mark(id);
        Some(id)
    }

    /// Ids in use.
    pub fn count(&self) -> u32 {
        self.count
    }
}

/// The state of every page of the file.
///
/// Only a filling or live page has a kind and coverage, and only a live page
/// an epoch and a place among the victims.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    /// In neither header's world: the flush may write it.
    Reusable,
    /// In the world of the older header only; reusable after the next commit.
    Fallback,
    /// One of the current journal's pages, or named by the header for it.
    Journal,
    /// Taken by the flush in progress, which writes it at its commit.
    Filling(Filling),
    /// In the governing header's world.
    Live(Live),
}

/// What a page the flush in progress writes holds so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Filling {
    pub coverage: u32,
    pub kind: PageKind,
}

/// What a live page holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Live {
    /// The epoch the page was written in.
    pub epoch: u64,
    /// The bytes current state relies on: for a data page those its spans
    /// show, for a leaf its live entries' bytes.
    pub coverage: u32,
    pub kind: PageKind,
    pub slot: Option<BucketSlot>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageKind {
    Data,
    /// A leaf of the span table: a table page the header names.
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

    /// The page once the commit of epoch `epoch` has written it.
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

/// Pages bucketed by coverage, for `O(1)` victim selection.
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

/// The committed state.
#[derive(Debug, Default)]
pub struct State {
    /// Every span, by id; free slots have `Content::Free`.
    pub spans: Vec<Span>,
    pub ids: Ids,
    /// The rope over the sequence's spans, and its root.
    pub rope: Treap<RopeItem>,
    pub root: u32,
    /// The spans of the schema's chain and of the private chain, in order.
    pub schema: Vec<u32>,
    pub private: Vec<u32>,
    /// Every page of the file, by page number.
    pub pages: Vec<Page>,
    pub data_buckets: Buckets,
    pub table_buckets: Buckets,
    /// Data page -> the spans whose bytes lie in it.
    pub data_spans: IdMap<Vec<u32>>,
    /// Table page -> the spans whose live entry it holds.
    pub table_spans: IdMap<Vec<u32>>,
    /// Pages whose coverage fell since the last commit.
    pub dropped_candidates: IdSet,
    /// The epoch of the flush in progress, and of every page it fills; or
    /// between flushes, the governing header's.
    pub flush_epoch: u64,
}

impl State {
    // ---------------------------------------------------------------- pages

    /// Makes room for page records up to `page`, which the file now holds.
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

    #[inline]
    fn coverage_mut(&mut self, page: u32) -> &mut u32 {
        match &mut self.pages[page as usize] {
            Page::Live(Live { coverage, .. }) | Page::Filling(Filling { coverage, .. }) => coverage,
            other => panic!("page {page} holds no content but is {other:?}"),
        }
    }

    pub fn coverage(&self, page: u32) -> u32 {
        match &self.pages[page as usize] {
            Page::Live(Live { coverage, .. }) | Page::Filling(Filling { coverage, .. }) => {
                *coverage
            }
            _ => 0,
        }
    }

    /// The record of `page` if it may be a consolidation victim: a live data
    /// page or leaf with live content.
    pub fn victim(&self, page: u32) -> Option<&Live> {
        match &self.pages[page as usize] {
            Page::Live(l) if l.coverage > 0 && l.kind != PageKind::Header => Some(l),
            _ => None,
        }
    }

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

    // ---------------------------------------------------------------- spans

    #[inline]
    pub fn span(&self, id: u32) -> &Span {
        &self.spans[id as usize]
    }

    #[inline]
    pub fn span_mut(&mut self, id: u32) -> &mut Span {
        &mut self.spans[id as usize]
    }

    /// A new span id, preferably `after + 1`, with a fresh record.
    pub fn new_span(&mut self, after: u32) -> Option<u32> {
        let id = self.ids.alloc(after)?;
        if self.spans.len() <= id as usize {
            self.spans.resize(id as usize + 1, Span::default());
        }
        self.spans[id as usize] = Span {
            entry_page: NONE,
            node: NIL,
            ..Span::default()
        };
        Some(id)
    }

    /// The length of the sequence.
    pub fn seq_len(&self) -> u64 {
        self.rope.sum(self.root)
    }

    /// The first span of the sequence, or 0.
    pub fn seq_head(&self) -> u32 {
        match self.rope.first(self.root) {
            NIL => 0,
            n => match *self.rope.item(n) {
                RopeItem::Span { id, .. } => id,
                RopeItem::Lit { .. } => unreachable!("a literal in the rope between flushes"),
            },
        }
    }

    /// Adds span `id`, a `Ref`, to its data page's list.
    pub fn note_data(&mut self, id: u32) {
        let Content::Ref { page, .. } = self.spans[id as usize].content else {
            return;
        };
        let list = self.data_spans.entry(page).or_default();
        self.spans[id as usize].rev = list.len() as u32;
        list.push(id);
    }

    /// Takes span `id`, a `Ref`, out of its data page's list.
    pub fn unnote_data(&mut self, id: u32) {
        let Content::Ref { page, .. } = self.spans[id as usize].content else {
            return;
        };
        let rev = self.spans[id as usize].rev as usize;
        let list = self.data_spans.get_mut(&page).expect("a data page's list");
        debug_assert_eq!(list[rev], id);
        list.swap_remove(rev);
        if rev < list.len() {
            let moved = list[rev];
            self.spans[moved as usize].rev = rev as u32;
        }
        if list.is_empty() {
            self.data_spans.remove(&page);
        }
    }

    /// Records that span `id`'s live entry now lies in `page`, taking
    /// `len` bytes.
    pub fn note_entry(&mut self, id: u32, page: u32, len: u16) {
        debug_assert_eq!(self.spans[id as usize].entry_page, NONE);
        let list = self.table_spans.entry(page).or_default();
        let s = &mut self.spans[id as usize];
        s.tab = list.len() as u32;
        s.entry_page = page;
        s.entry_len = len;
        list.push(id);
        self.cover(page, len as u32);
    }

    /// Ends span `id`'s live entry, if it has one: it no longer counts
    /// toward its page.
    pub fn drop_entry(&mut self, id: u32) {
        let s = self.spans[id as usize];
        if s.entry_page == NONE {
            return;
        }
        let list = self
            .table_spans
            .get_mut(&s.entry_page)
            .expect("a table page's list");
        let tab = s.tab as usize;
        debug_assert_eq!(list[tab], id);
        list.swap_remove(tab);
        if tab < list.len() {
            let moved = list[tab];
            self.spans[moved as usize].tab = tab as u32;
        }
        if list.is_empty() {
            self.table_spans.remove(&s.entry_page);
        }
        self.uncover(s.entry_page, s.entry_len as u32);
        self.spans[id as usize].entry_page = NONE;
    }

    /// Ends span `id`: its entry dies, its bytes stop counting, and its id
    /// is free.
    pub fn kill(&mut self, id: u32) {
        self.drop_entry(id);
        let s = self.spans[id as usize];
        if let Content::Ref { page, .. } = s.content {
            self.unnote_data(id);
            self.uncover(page, s.len);
        }
        self.spans[id as usize] = Span::default();
        self.ids.free(id);
    }

    /// The spans of the sequence, in order.
    pub fn seq_spans(&self) -> Vec<u32> {
        self.rope
            .nodes(self.root)
            .into_iter()
            .map(|n| match *self.rope.item(n) {
                RopeItem::Span { id, .. } => id,
                RopeItem::Lit { .. } => unreachable!("a literal in the rope between flushes"),
            })
            .collect()
    }

    /// Checks every invariant between flushes, panicking at the first
    /// violation: the rope against the span table, the chains' links, the
    /// reverse lists, and every page's coverage.
    pub fn check(&self, header_slot: u32) {
        self.rope.check(self.root);
        let mut data_cov: IdMap<u64> = IdMap::default();
        let mut table_cov: IdMap<u64> = IdMap::default();
        let mut reached = IdSet::default();
        let chains = [
            (Chain::Sequence, self.seq_spans()),
            (Chain::Schema, self.schema.clone()),
            (Chain::Private, self.private.clone()),
        ];
        for (chain, list) in chains {
            for (i, &id) in list.iter().enumerate() {
                assert!(reached.insert(id), "span {id} is reached twice");
                let s = self.span(id);
                assert_eq!(s.chain, chain, "span {id} is in another chain");
                assert!(s.len > 0, "span {id} is empty");
                let next = list.get(i + 1).copied().unwrap_or(0);
                assert_eq!(s.next, next, "span {id} links to the wrong span");
                if chain == Chain::Sequence {
                    assert_eq!(
                        *self.rope.item(s.node),
                        RopeItem::Span { id, len: s.len },
                        "span {id}'s rope node"
                    );
                }
                assert!(s.entry_page != NONE, "span {id} has no entry");
                *table_cov.entry(s.entry_page).or_default() += s.entry_len as u64;
                assert_eq!(
                    self.table_spans[&s.entry_page][s.tab as usize], id,
                    "span {id} is not in its table page's list"
                );
                match s.content {
                    Content::Ref { page, offset } => {
                        assert!(
                            offset as usize + s.len as usize <= PAGE_SIZE - 4,
                            "span {id} reaches past its page"
                        );
                        *data_cov.entry(page).or_default() += s.len as u64;
                        assert_eq!(
                            self.data_spans[&page][s.rev as usize], id,
                            "span {id} is not in its data page's list"
                        );
                    }
                    Content::Inline { page, .. } => {
                        assert_eq!(page, s.entry_page, "span {id}'s payload is elsewhere");
                        assert!(s.len as usize <= MAX_INLINE);
                    }
                    Content::Zero => {}
                    other => panic!("span {id} shows {other:?} between flushes"),
                }
            }
        }
        assert_eq!(
            reached.len() as u32,
            self.ids.count(),
            "spans nobody reaches"
        );
        for (&page, list) in &self.data_spans {
            assert_eq!(
                list.len(),
                list.iter().filter(|id| reached.contains(id)).count()
            );
            let _ = page;
        }
        for (p, page) in self.pages.iter().enumerate() {
            let p = p as u32;
            if let Page::Live(l) = page {
                match l.kind {
                    PageKind::Data => assert_eq!(
                        l.coverage as u64,
                        data_cov.get(&p).copied().unwrap_or(0),
                        "coverage of data page {p}"
                    ),
                    PageKind::Table => assert_eq!(
                        l.coverage as u64,
                        table_cov.get(&p).copied().unwrap_or(0),
                        "coverage of leaf {p}"
                    ),
                    PageKind::Header => assert_eq!(p, header_slot, "a stale header"),
                }
                assert_eq!(
                    l.slot.map(|s| s.bucket),
                    self.wanted_bucket(p),
                    "bucket of {p}"
                );
            } else {
                assert!(
                    !data_cov.contains_key(&p) && !table_cov.contains_key(&p),
                    "page {p} is {page:?} but holds live content"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_go_lowest_first_and_follow_their_predecessor() {
        let mut ids = Ids::default();
        let a: Vec<u32> = (0..200).map(|_| ids.alloc(0).unwrap()).collect();
        assert_eq!(a, (1..201).collect::<Vec<_>>());
        for id in [5, 70, 130] {
            ids.free(id);
        }
        assert_eq!(ids.alloc(0), Some(5));
        assert_eq!(ids.alloc(69), Some(70));
        assert_eq!(ids.alloc(0), Some(130));
        assert_eq!(ids.alloc(0), Some(201));
        assert_eq!(ids.count(), 201);
    }
}
