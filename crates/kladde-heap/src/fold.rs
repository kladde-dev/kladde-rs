//! Phase A of the flush optimizer: fold the log into per-id geometry and a
//! **piece table** over each touched allocation's bytes.
//!
//! `journal-semantics.md` §4. Per id, linear, no graph. The fold is a pure
//! function of the log, which is what makes it checkable against the naive
//! replayer (§8) and what would let it be recomputed after a crash (§9.4).
//!
//! Two properties fall out of it rather than being coded as special cases:
//! repeated writes to the same range emit **one** write, because the table holds
//! exactly one source per byte; and an allocation created and freed inside one
//! transaction never touches storage at all.

use std::collections::BTreeMap;

use crate::journal::{Log, Op, Pending, Span};
use crate::pointer::Pointer;
use crate::word::Word;

/// Where a byte comes from.
///
/// Offset-relative, so advancing a source by `k` is meaningful -- that is what
/// lets a literal survive being partially overwritten, the survivor naming a
/// position inside the original payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source<W: Word> {
    /// Bytes carried by a record in the log: an index into its payload arena.
    Literal(usize),
    /// Bytes that must be read from the file, at `id`'s address plus the offset.
    Storage(Pointer<W>, usize),
    /// Uninitialized. May legally be anything (§2.1), so it is never written.
    Undefined,
}

impl<W: Word> Source<W> {
    /// This source advanced by `k` bytes.
    fn advance(self, k: usize) -> Self {
        match self {
            Source::Literal(p) => Source::Literal(p + k),
            Source::Storage(id, off) => Source::Storage(id, off + k),
            Source::Undefined => Source::Undefined,
        }
    }

    /// Whether two adjacent segments are one contiguous run of the same source,
    /// so the boundary between them can be dropped.
    ///
    /// This is *source* contiguity, which saves memory but no I/O (§4.1.1). Two
    /// literals merge only when their log positions are adjacent, which
    /// consecutive `Write` records generally are not once the log is framed --
    /// hence the emitter gathers runs rather than relying on this.
    fn continues(self, gap: usize, next: Self) -> bool {
        self.advance(gap) == next
    }
}

/// One allocation's byte sources.
///
/// Every table starts with exactly one segment -- `Undefined` for a fresh
/// allocation, an identity `Storage(self, 0)` for one that already exists -- so
/// the no-allocation case is the common one rather than a special case (§7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Content<W: Word> {
    /// A single segment covering `[0, size)`. No allocation at all.
    Uniform(Source<W>),
    /// Segments live in the flush-wide spill map, keyed by `(id, offset)`.
    Spilled,
}

/// What the fold decided about one id.
#[derive(Debug)]
struct Entry<W: Word, S: Word> {
    geometry: Pending<W, S>,
    content: Content<W>,
}

/// The fold's output: geometry and content for every id the log touched.
///
/// Ordered rather than hashed, because the flush iterates it and
/// `journal-semantics.md` §9.4 makes the resulting layout's determinism
/// load-bearing.
#[derive(Debug)]
pub(crate) struct Folded<W: Word, S: Word> {
    entries: BTreeMap<Pointer<W>, Entry<W, S>>,
    /// One tree for the whole flush, not one per allocation.
    spill: BTreeMap<(Pointer<W>, usize), Source<W>>,
}

impl<W: Word, S: Word> Default for Folded<W, S> {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            spill: BTreeMap::new(),
        }
    }
}

/// What the flush must do about one id before any byte is written.
pub(crate) enum Action<W: Word, S: Word> {
    Claim(Pointer<W>, S),
    Reshape(Pointer<W>, S),
    /// Rekey in place: no bytes move (design note §4.3).
    Relabel {
        from: Pointer<W>,
        to: Pointer<W>,
        size: S,
    },
}

impl<W: Word, S: Word> Folded<W, S> {
    fn size_of(&self, id: Pointer<W>) -> Option<S> {
        self.entries.get(&id).and_then(|e| match e.geometry {
            Pending::New(s) | Pending::Resized(s) | Pending::Relabelled { size: s, .. } => Some(s),
            Pending::Freed => None,
        })
    }

    fn set_size(&mut self, id: Pointer<W>, size: S) {
        if let Some(e) = self.entries.get_mut(&id) {
            e.geometry = match e.geometry {
                Pending::New(_) => Pending::New(size),
                Pending::Resized(_) => Pending::Resized(size),
                Pending::Relabelled { from, .. } => Pending::Relabelled { from, size },
                Pending::Freed => Pending::Freed,
            };
        }
    }

    /// The segments of `id`, normalized to `(start, source)` pairs in order.
    fn segments(&self, id: Pointer<W>) -> Vec<(usize, Source<W>)> {
        match self.entries.get(&id).map(|e| e.content) {
            Some(Content::Uniform(src)) => vec![(0, src)],
            Some(Content::Spilled) => self
                .spill
                .range((id, 0)..=(id, usize::MAX))
                .map(|((_, off), src)| (*off, *src))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Replace `id`'s segments wholesale, collapsing to `Uniform` when one
    /// segment suffices and coalescing adjacent runs of the same source.
    fn set_segments(&mut self, id: Pointer<W>, segments: Vec<(usize, Source<W>)>) {
        let mut merged: Vec<(usize, Source<W>)> = Vec::with_capacity(segments.len());
        for (start, src) in segments {
            if let Some(&(prev_start, prev_src)) = merged.last() {
                if prev_src.continues(start - prev_start, src) {
                    continue;
                }
            }
            merged.push((start, src));
        }

        self.spill
            .retain(|(other, _), _| *other != id);
        let content = if merged.len() == 1 && merged[0].0 == 0 {
            Content::Uniform(merged[0].1)
        } else {
            for (off, src) in merged {
                self.spill.insert((id, off), src);
            }
            Content::Spilled
        };
        if let Some(e) = self.entries.get_mut(&id) {
            e.content = content;
        }
    }

    /// The source of the byte at `offset`, or `None` past the end.
    fn source_at(&self, id: Pointer<W>, offset: usize, size: usize) -> Option<Source<W>> {
        if offset >= size {
            return None;
        }
        let segments = self.segments(id);
        let i = segments.partition_point(|(start, _)| *start <= offset);
        let (start, src) = segments[i - 1];
        Some(src.advance(offset - start))
    }

    /// Overwrite `[start, start + len)` with `src`.
    ///
    /// The only non-trivial primitive: materialize both boundaries from the
    /// segments they fall inside, drop everything strictly between, insert the new
    /// segment.
    fn overwrite(&mut self, id: Pointer<W>, size: usize, start: usize, len: usize, src: Source<W>) {
        if len == 0 {
            return;
        }
        let end = start + len;
        let right = self.source_at(id, end, size);
        let mut segments: Vec<(usize, Source<W>)> = self
            .segments(id)
            .into_iter()
            .filter(|(off, _)| *off < start || *off > end)
            .collect();
        segments.push((start, src));
        if let Some(r) = right {
            segments.push((end, r));
        }
        segments.sort_by_key(|(off, _)| *off);
        self.set_segments(id, segments);
    }

    /// Clip to `new_size`, or extend the tail with `Undefined`.
    fn resize_content(&mut self, id: Pointer<W>, old_size: usize, new_size: usize) {
        if new_size == old_size {
            return;
        }
        let mut segments: Vec<(usize, Source<W>)> = self
            .segments(id)
            .into_iter()
            .filter(|(off, _)| *off < new_size)
            .collect();
        if new_size > old_size {
            segments.push((old_size, Source::Undefined));
        }
        self.set_segments(id, segments);
    }

    /// Replace `[offset, offset + old_len)` with `src` of length `new_len`,
    /// shifting the tail. `splice` on a fresh allocation therefore costs no I/O
    /// at all -- it is a table edit, and the shift never touches bytes.
    fn splice_content(
        &mut self,
        id: Pointer<W>,
        size: usize,
        offset: usize,
        old_len: usize,
        src: Source<W>,
        new_len: usize,
    ) {
        let tail_start = offset + old_len;
        let tail_src = self.source_at(id, tail_start, size);
        let shift = |off: usize| off + new_len - old_len;

        let mut segments: Vec<(usize, Source<W>)> = self
            .segments(id)
            .into_iter()
            .filter(|(off, _)| *off < offset)
            .collect();
        if new_len > 0 {
            segments.push((offset, src));
        }
        if let Some(t) = tail_src {
            segments.push((offset + new_len, t));
        }
        for (off, s) in self.segments(id) {
            if off > tail_start {
                segments.push((shift(off), s));
            }
        }
        segments.sort_by_key(|(off, _)| *off);
        self.set_segments(id, segments);
    }

    /// The geometry that must be in place *before* any byte is written:
    /// relabels, then claims, then reshapes.
    pub(crate) fn before_writes(&self) -> Vec<Action<W, S>> {
        let mut claims = Vec::new();
        let mut reshapes = Vec::new();
        let mut relabels = Vec::new();
        for (id, entry) in &self.entries {
            match entry.geometry {
                Pending::New(s) => claims.push(Action::Claim(*id, s)),
                Pending::Resized(s) => reshapes.push(Action::Reshape(*id, s)),
                Pending::Relabelled { from, size } => relabels.push(Action::Relabel {
                    from,
                    to: *id,
                    size,
                }),
                Pending::Freed => {}
            }
        }
        relabels.into_iter().chain(claims).chain(reshapes).collect()
    }

    /// The releases, which run **last**.
    ///
    /// A piece may still name a released allocation's storage -- a sizedness
    /// conversion transfers its table to the new id, whose pieces read the old one
    /// -- and frees-last makes those reads trivially safe. It costs the placement
    /// benefit of frees-first, which read hoisting (§6.5) buys back by removing
    /// the reads that force the ordering.
    pub(crate) fn releases(&self) -> Vec<Pointer<W>> {
        self.entries
            .iter()
            .filter(|(_, e)| matches!(e.geometry, Pending::Freed))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Ids that will hold bytes after the flush, with their final sizes.
    pub(crate) fn survivors(&self) -> Vec<(Pointer<W>, S)> {
        self.entries
            .keys()
            .filter_map(|id| self.size_of(*id).map(|s| (*id, s)))
            .collect()
    }

    /// The runs of `id` that actually have to be written: maximal spans of
    /// segments that are neither `Undefined` nor already in the right place.
    ///
    /// An identity piece -- `Storage(self, k)` with `k` equal to the segment start
    /// -- emits nothing. Without that check every untouched region of every
    /// persistent allocation would emit a self-copy.
    pub(crate) fn runs(&self, id: Pointer<W>, size: usize) -> Vec<(usize, Vec<(usize, Source<W>)>)> {
        let segments = self.segments(id);
        let mut runs: Vec<(usize, Vec<(usize, Source<W>)>)> = Vec::new();
        let mut current: Option<(usize, Vec<(usize, Source<W>)>)> = None;

        for i in 0..segments.len() {
            let (start, src) = segments[i];
            if start >= size {
                break;
            }
            let end = segments.get(i + 1).map_or(size, |(s, _)| (*s).min(size));
            let len = end - start;
            if len == 0 {
                continue;
            }
            let skip = matches!(src, Source::Undefined)
                || matches!(src, Source::Storage(other, off) if other == id && off == start);
            if skip {
                if let Some(run) = current.take() {
                    runs.push(run);
                }
            } else {
                current
                    .get_or_insert_with(|| (start, Vec::new()))
                    .1
                    .push((len, src));
            }
        }
        if let Some(run) = current.take() {
            runs.push(run);
        }
        runs
    }

    /// What the fold decided about `id`, or `None` if the log never mentioned it.
    /// Feeds the fold-agreement assertion (§2).
    pub(crate) fn geometry_of(&self, id: Pointer<W>) -> Option<Pending<W, S>> {
        self.entries.get(&id).map(|e| e.geometry)
    }
}

/// Fold `log` against a heap in which `live` reports whether an id already exists.
pub(crate) fn fold<W: Word, S: Word>(
    log: &Log<W, S>,
    live: impl Fn(Pointer<W>) -> Option<S>,
) -> Folded<W, S> {
    let mut f = Folded::default();

    // Bring a persistent id into the fold on first mention: it exists, at its
    // committed size, and its bytes are already where they belong.
    let touch = |f: &mut Folded<W, S>, id: Pointer<W>| {
        if !f.entries.contains_key(&id) {
            if let Some(size) = live(id) {
                f.entries.insert(
                    id,
                    Entry {
                        geometry: Pending::Resized(size),
                        content: Content::Uniform(Source::Storage(id, 0)),
                    },
                );
            }
        }
    };

    for op in log.ops() {
        match *op {
            Op::Alloc { id, size } => {
                f.entries.insert(
                    id,
                    Entry {
                        geometry: Pending::New(size),
                        content: Content::Uniform(Source::Undefined),
                    },
                );
            }
            Op::Free { id } => {
                touch(&mut f, id);
                if let Some(e) = f.entries.get_mut(&id) {
                    e.geometry = Pending::Freed;
                }
                f.spill.retain(|(other, _), _| *other != id);
            }
            Op::Resize { id, size } => {
                touch(&mut f, id);
                let old = f.size_of(id).map_or(0, |s| s.to_usize());
                f.set_size(id, size);
                f.resize_content(id, old, size.to_usize());
            }
            Op::ChangeSizedness { old, new } => {
                touch(&mut f, old);
                let old_geometry = f.entries[&old].geometry;
                let content = f.entries[&old].content;

                // The bytes do not move, so a piece naming the old id names the
                // same physical bytes under the new one. Rewriting it keeps the
                // piece *identity*, which is what makes a conversion emit nothing
                // at all (§4.3). The allocate-copy-free form destroyed that
                // property, since the new id sat at a new address.
                let segments: Vec<(usize, Source<W>)> = f
                    .segments(old)
                    .into_iter()
                    .map(|(off, src)| {
                        let src = match src {
                            Source::Storage(id, k) if id == old => Source::Storage(new, k),
                            other => other,
                        };
                        (off, src)
                    })
                    .collect();

                let geometry = match old_geometry {
                    // Never reached the heap: nothing to relabel, the entry just
                    // moves. The old counter is still owed, hence the `Freed`
                    // below rather than dropping the entry.
                    Pending::New(size) => Pending::New(size),
                    Pending::Resized(size) => Pending::Relabelled { from: old, size },
                    // A chain of conversions: `from` stays whichever id the heap
                    // actually holds.
                    Pending::Relabelled { from, size } => Pending::Relabelled { from, size },
                    Pending::Freed => unreachable!("conversion of a released id"),
                };

                if matches!(old_geometry, Pending::New(_)) {
                    f.entries.get_mut(&old).expect("touched").geometry = Pending::Freed;
                    f.spill.retain(|(o, _), _| *o != old);
                } else {
                    // The relabel owns both halves, including recycling `from`.
                    f.entries.remove(&old);
                    f.spill.retain(|(o, _), _| *o != old);
                }

                f.entries.insert(new, Entry { geometry, content });
                f.set_segments(new, segments);
            }
            Op::Write {
                id,
                offset,
                payload,
            } => {
                touch(&mut f, id);
                let size = f.size_of(id).map_or(0, |s| s.to_usize());
                f.overwrite(
                    id,
                    size,
                    offset.to_usize(),
                    payload.len,
                    Source::Literal(payload.start),
                );
            }
            Op::Splice {
                id,
                offset,
                old_len,
                payload,
            } => {
                touch(&mut f, id);
                let size = f.size_of(id).map_or(0, |s| s.to_usize());
                let new_size = size + payload.len - old_len.to_usize();
                f.splice_content(
                    id,
                    size,
                    offset.to_usize(),
                    old_len.to_usize(),
                    Source::Literal(payload.start),
                    payload.len,
                );
                f.set_size(id, Word::from_usize(new_size));
            }
        }
    }
    f
}

/// Where the payload of a `Literal` lives, for the emitter.
pub(crate) fn literal<'a, W: Word, S: Word>(log: &'a Log<W, S>, at: usize, len: usize) -> &'a [u8] {
    log.payload(Span { start: at, len })
}
