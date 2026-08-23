//! The journal: a totally ordered log of every mutation, plus the write-phase
//! geometry cache derived from it.
//!
//! See `journal-semantics.md` §2. The one rule everything else follows from:
//!
//! > **The log is the sole authority.** Everything else is derived from it and
//! > may be discarded and rebuilt.
//!
//! The predecessor of this module kept *two* records of a transaction -- an
//! unordered, self-annihilating `pending` map and a separate ordered write buffer
//! -- with no order relating them, so every operation that invalidated an earlier
//! buffered write had to repair it by hand. Only one did. See §1 of the design
//! note for the three bugs that followed, and `tests` below for the regressions.

use std::collections::HashMap;

use crate::pointer::Pointer;
use crate::word::Word;

/// Where a payload lives in the log's byte arena.
///
/// Ops carry a span rather than a `Vec<u8>` so that payload bytes are contiguous
/// and addressable: `journal-semantics.md`'s `Source::Literal(LogOffset)` is
/// exactly an index into [`Log::bytes`]. That is also what lets a literal survive
/// being partially overwritten -- the survivor simply names a position inside the
/// original payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Span {
    pub start: usize,
    pub len: usize,
}

impl Span {
    pub(crate) fn range(self) -> std::ops::Range<usize> {
        self.start..self.start + self.len
    }
}

/// One recorded mutation.
///
/// The six kinds are exactly the closure under "an entry whose absence would make
/// some other entry unreplayable or misinterpretable" (§2). In particular `Alloc`
/// is logged because otherwise a `Write` has no target *and* an id serialized into
/// a parent allocation would name something nothing ever created -- the failure
/// that has nothing to do with annihilation.
///
/// Sizedness rides on the id, so no op carries it separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op<W: Word, S: Word> {
    Alloc {
        id: Pointer<W>,
        size: S,
    },
    Free {
        id: Pointer<W>,
    },
    Resize {
        id: Pointer<W>,
        size: S,
    },
    /// Sizedness lives in the id's low bit, so a conversion cannot re-tag in
    /// place; it necessarily names a new id. See §4.3 -- this is a *relabel*, not
    /// an allocate-copy-free.
    ChangeSizedness {
        old: Pointer<W>,
        new: Pointer<W>,
    },
    Write {
        id: Pointer<W>,
        offset: S,
        payload: Span,
    },
    Splice {
        id: Pointer<W>,
        offset: S,
        old_len: S,
        payload: Span,
    },
    /// The only op that makes one allocation's content depend on another's, and
    /// therefore the only one that puts a cross-id `Storage` piece in a table
    /// (design note §7). Everything about hoisting and scheduling exists for it.
    Copy {
        src: Pointer<W>,
        src_offset: S,
        len: S,
        dst: Pointer<W>,
        dst_offset: S,
    },
}

/// The ordered log plus its payload arena.
#[derive(Debug)]
pub(crate) struct Log<W: Word, S: Word> {
    ops: Vec<Op<W, S>>,
    bytes: Vec<u8>,
}

impl<W: Word, S: Word> Default for Log<W, S> {
    fn default() -> Self {
        Self {
            ops: Vec::new(),
            bytes: Vec::new(),
        }
    }
}

impl<W: Word, S: Word> Log<W, S> {
    pub(crate) fn push(&mut self, op: Op<W, S>) {
        self.ops.push(op);
    }

    /// Append `bytes` to the arena and return the span naming them.
    pub(crate) fn intern(&mut self, bytes: &[u8]) -> Span {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(bytes);
        Span {
            start,
            len: bytes.len(),
        }
    }

    pub(crate) fn ops(&self) -> &[Op<W, S>] {
        &self.ops
    }

    pub(crate) fn payload(&self, span: Span) -> &[u8] {
        &self.bytes[span.range()]
    }

    /// Bytes of payload currently buffered -- the quantity an auto-checkpoint
    /// would eventually watch (§9.1).
    pub(crate) fn payload_len(&self) -> usize {
        self.bytes.len()
    }
}

/// What this transaction has done to one id, as a **delta** over the heap's
/// committed table (§3).
///
/// Not a snapshot: an id absent from the map is answered by the heap, which is
/// what makes a second write phase over an already-populated heap work at all.
/// The predecessor treated its map as exhaustive, so every allocation claimed by
/// an earlier checkpoint reported `DanglingPointer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pending<W: Word, S: Word> {
    /// Minted this transaction, not in the heap.
    New(S),
    /// In the heap from an earlier checkpoint, with a new size pending.
    Resized(S),
    /// In the heap under `from`; this id takes its place without moving bytes.
    ///
    /// Carries the *final* size so that a following `Resize` composes into it
    /// rather than needing a fifth state.
    Relabelled { from: Pointer<W>, size: S },
    /// To be released. May or may not be live in the heap -- an id minted and
    /// freed in the same transaction never reached it. This is `Freed` rather
    /// than simply absent so that the counter still gets recycled.
    Freed,
}

impl<W: Word, S: Word> Pending<W, S> {
    /// The size this id will have, or `None` if it is being released.
    pub(crate) fn size(self) -> Option<S> {
        match self {
            Pending::New(s) | Pending::Resized(s) | Pending::Relabelled { size: s, .. } => Some(s),
            Pending::Freed => None,
        }
    }
}

/// The write-phase geometry cache: `id -> Pending`, absent meaning "ask the heap".
#[derive(Debug)]
pub(crate) struct Deltas<W: Word, S: Word> {
    map: HashMap<Pointer<W>, Pending<W, S>>,
}

impl<W: Word, S: Word> Default for Deltas<W, S> {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
        }
    }
}

/// What a geometry query found.
pub(crate) enum Geometry<S> {
    /// The transaction has an opinion; use it without consulting the heap.
    Known(S),
    /// The transaction has released this id.
    Dead,
    /// The transaction has not touched this id; the heap is authoritative.
    AskHeap,
}

impl<W: Word, S: Word> Deltas<W, S> {
    pub(crate) fn clear(&mut self) {
        self.map.clear();
    }

    /// Fold agreement (design note §2): the geometry the fold derives from the
    /// log must equal what this cache recorded incrementally.
    ///
    /// The redundancy is the point -- every divergence bug in the predecessor
    /// would have tripped this, and it is asymptotically free next to the fold
    /// that produced the other side.
    ///
    /// One-directional: the fold also carries ids this map never mentions, namely
    /// persistent allocations that were only *written* and so have no geometry
    /// delta at all.
    pub(crate) fn agrees_with(
        &self,
        folded: impl Fn(Pointer<W>) -> Option<Pending<W, S>>,
    ) -> bool {
        self.map
            .iter()
            .all(|(id, pending)| folded(*id).as_ref() == Some(pending))
    }

    /// Answer a `size`/`resolve` query. Deltas first, heap second -- never the
    /// other way round, and never the typestate alone: `Backend::resolve` can
    /// mint a second owner of an owned region, so a stale duplicate handle can
    /// reach a freed id at runtime (§3).
    pub(crate) fn geometry(&self, id: Pointer<W>) -> Geometry<S> {
        match self.map.get(&id) {
            Some(p) => match p.size() {
                Some(s) => Geometry::Known(s),
                None => Geometry::Dead,
            },
            None => Geometry::AskHeap,
        }
    }

    /// `alloc`: the only source state is "not live anywhere" (§2.2 keeps counters
    /// out of the pool until checkpoint, so a fresh mint can never collide with a
    /// `Freed` entry).
    pub(crate) fn allocated(&mut self, id: Pointer<W>, size: S) {
        let previous = self.map.insert(id, Pending::New(size));
        debug_assert!(
            previous.is_none(),
            "a freshly minted id already had a pending entry: counters must not \
             return to the pool before checkpoint",
        );
    }

    /// `resize`. `Err(())` for an id this transaction has already released or
    /// that was never live -- the caller turns it into `DanglingPointer`.
    #[allow(clippy::result_unit_err)]
    pub(crate) fn resized(&mut self, id: Pointer<W>, size: S, live_in_heap: bool) -> Result<(), ()> {
        let next = match self.map.get(&id) {
            Some(Pending::New(_)) => Pending::New(size),
            Some(Pending::Resized(_)) => Pending::Resized(size),
            Some(Pending::Relabelled { from, .. }) => Pending::Relabelled { from: *from, size },
            Some(Pending::Freed) => return Err(()),
            None if live_in_heap => Pending::Resized(size),
            None => return Err(()),
        };
        self.map.insert(id, next);
        Ok(())
    }

    /// `free`. Always lands on `Freed`, including from `New`: going to absent
    /// would lose the fact that the counter still has to be recycled, which is
    /// the counter leak the predecessor had.
    pub(crate) fn freed(&mut self, id: Pointer<W>) {
        self.map.insert(id, Pending::Freed);
    }

    /// `ChangeSizedness` -- the **only** operation that mutates two entries in one
    /// call. The old id ends with no entry at all rather than a `Freed` one:
    /// making the relabel own both halves removes an ordering coupling between
    /// two entries that would otherwise have to be processed in sequence.
    pub(crate) fn sizedness_changed(
        &mut self,
        old: Pointer<W>,
        new: Pointer<W>,
        size: S,
        live_in_heap: bool,
    ) {
        let entry = match self.map.remove(&old) {
            // Never reached the heap, so there is nothing to relabel: the entry
            // moves to the new id. The old id's counter is still owed, so it is
            // marked `Freed` rather than dropped -- the release is lookup-guarded
            // and will recycle without touching the heap.
            Some(Pending::New(_)) => {
                self.map.insert(old, Pending::Freed);
                Pending::New(size)
            }
            // The relabel owns both halves, including recycling `from`, which is
            // why the old id ends with no entry at all.
            Some(Pending::Resized(_)) => Pending::Relabelled { from: old, size },
            Some(Pending::Relabelled { from, .. }) => Pending::Relabelled { from, size },
            Some(Pending::Freed) => {
                debug_assert!(false, "sizedness change of a released id");
                Pending::New(size)
            }
            None if live_in_heap => Pending::Relabelled { from: old, size },
            None => Pending::New(size),
        };
        self.map.insert(new, entry);
    }
}
