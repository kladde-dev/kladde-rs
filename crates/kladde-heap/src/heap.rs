//! The relocatable-heap traits: [`RelocatableHeap`] (geometry over stable ids)
//! and [`IncrementallyCompactableHeap`] (a marker that its compaction methods
//! actually do something).
//!
//! See `incremental-compaction.md`. The short version: because kladde's pointers
//! are stable ids, an `id -> (address, size)` table must exist in memory no
//! matter what -- so the component that chooses *where* things go should own that
//! table rather than shadow it (P1). That is what a `RelocatableHeap` is: it
//! manages the whole partition of `[0, end)` into allocations and gaps, and it
//! can move allocations to close the gaps, one bounded step at a time.
//!
//! The heap does **not** mint ids and never serializes them (P5) -- the backend
//! does both. The one thing it reads out of an id is its sizedness bit, via
//! [`AllocationId`], because that is placement-relevant.

use std::hash::Hash;

use crate::word::Word;

/// What the heap needs to know about an id it never mints.
///
/// Sizedness rides on the id rather than being stored per allocation: it is the
/// only per-allocation fact the heap has ever needed, so a separate `Meta`
/// channel would carry exactly this one bit and nothing else. See
/// [`Pointer::from_parts`](crate::Pointer::from_parts) for the concrete layout.
pub trait AllocationId: Copy + Eq + Hash {
    /// Whether the allocation this id names keeps its size for life.
    ///
    /// Fixed-size allocations are the ones kladde mints in bulk at a handful of
    /// distinct sizes, so they form dense size classes worth indexing; resizable
    /// ones scatter one-per-class and would have to move again on the next
    /// growth, so the heap indexes only the fixed-size ones as movers.
    fn is_fixed_size(&self) -> bool;
}

/// The heap's own errors. A bad id is *its* error now: unlike the free-space-only
/// allocator this replaces, the heap owns the `id -> address` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeapError {
    /// No free range large enough, and the address space cannot be extended.
    OutOfMemory,
    /// The id names no live allocation (freed, or never allocated).
    UnknownId,
    /// `alloc` was called with an id that is already live.
    DuplicateId,
}

impl std::fmt::Display for HeapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeapError::OutOfMemory => write!(f, "out of address space"),
            HeapError::UnknownId => write!(f, "no live allocation for this id"),
            HeapError::DuplicateId => write!(f, "id is already live"),
        }
    }
}
impl std::error::Error for HeapError {}

/// One compaction move: copy `[from, from + len)` down to `[to, to + len)`.
///
/// Deliberately **id-free**. A step is a contiguous *byte range*, which is what
/// lets one step describe a whole **run** of neighbouring allocations sliding
/// together -- the natural unit for I/O, since the fixed per-operation cost is
/// then paid once per run instead of once per allocation. Moving a single
/// allocation is just the one-element case of the same shape.
///
/// The caller copies the bytes and hands the step back to
/// [`RelocatableHeap::commit_compaction_step`], which re-keys every allocation in
/// the range. `to < from` always, so a source/destination overlap (which happens
/// whenever the gap is smaller than the run) is a *downward* move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step<A> {
    /// Start of the range being moved.
    pub from: A,
    /// Where it lands. Always strictly below `from`.
    pub to: A,
    /// How many bytes move. Spans whole allocations, never a partial one.
    pub len: A,
}

/// What a [`RelocatableHeap::resize`] did to an allocation's address: `None` if
/// it kept it, `Some((old, new))` if the bytes must be moved by the caller.
pub type Relocation<A> = Option<(A, A)>;

/// Geometry: which allocation lives where, where the free space is, and how to
/// squeeze it out.
///
/// Ids are minted by the caller and handed in; the heap only ever asks them
/// [`AllocationId::is_fixed_size`].
pub trait RelocatableHeap {
    /// The caller-minted id naming an allocation.
    type Id: AllocationId;
    /// A position in the address space. Also the unit of file length and of
    /// compaction budgets, since those are spans rather than allocation sizes.
    type Address: Word;
    /// The size of a single allocation.
    type Size: Word + Into<Self::Address>;

    /// Place a new allocation of `size` bytes and record it under `id`.
    ///
    /// `Err(DuplicateId)` if `id` is already live, `Err(OutOfMemory)` if nothing
    /// fits.
    fn alloc(&mut self, id: Self::Id, size: Self::Size) -> Result<Self::Address, HeapError>;

    /// Release the allocation named by `id`, merging its range into the
    /// neighbouring gaps.
    fn free(&mut self, id: Self::Id) -> Result<(), HeapError>;

    /// Resize the allocation named by `id`.
    ///
    /// `Ok(None)` if it kept its address; `Ok(Some((old, new)))` if it had to
    /// move, in which case the caller must copy `min(old_size, new_size)` bytes
    /// from `old` to `new`.
    fn resize(
        &mut self,
        id: Self::Id,
        new_size: Self::Size,
    ) -> Result<Relocation<Self::Address>, HeapError>;

    /// Where `id` currently lives, and how big it is.
    fn lookup(&self, id: Self::Id) -> Option<(Self::Address, Self::Size)>;

    /// One past the highest live byte -- the length the store needs right now.
    fn len(&self) -> Self::Address;

    /// Total bytes across all live allocations. Equals [`len`](Self::len)
    /// exactly when the heap is compact; the difference is the fragmentation
    /// debt compaction is working off.
    fn live_bytes(&self) -> Self::Address;

    /// Whether nothing is allocated.
    fn is_empty(&self) -> bool {
        self.live_bytes() == Self::Address::zero()
    }

    /// Number of live allocations. Useful for leak checks; override if the
    /// implementation can answer it without walking.
    fn live_count(&self) -> usize {
        self.iter().count()
    }

    /// Every live allocation, in ascending address order. For snapshotting the
    /// table to the persistent format.
    fn iter(&self) -> impl Iterator<Item = (Self::Id, Self::Address, Self::Size)> + '_;

    /// Propose the next incremental compaction step, preferring one that costs at
    /// most `budget` bytes. `None` means compact, or nothing left worth doing.
    ///
    /// May return a step costing **more** than `budget` when no worthwhile
    /// cheaper step exists -- otherwise a heap whose only useful move is one big
    /// slide would claim quiescence and never compact. `budget` is therefore a
    /// ranking input, not a cap; the caller can price the returned step itself
    /// (its `len`) and decide whether to run it, chunk it, or drop it.
    ///
    /// Defaults to `None`: this heap does not compact.
    fn propose_compaction_step(&self, _budget: Self::Address) -> Option<Step<Self::Address>> {
        None
    }

    /// Apply a step previously obtained from
    /// [`propose_compaction_step`](Self::propose_compaction_step), re-keying
    /// every allocation in the moved range. Copying the bytes is the caller's
    /// job, and must already have happened (or be journaled to happen).
    ///
    /// The default panics rather than doing nothing: a heap that never proposes a
    /// step can never legitimately be handed one, and a silent no-op here would
    /// turn "overrode `propose` but forgot `commit`" into copied bytes that are
    /// never pointed at -- corruption with no compile error.
    fn commit_compaction_step(&mut self, _step: Step<Self::Address>) {
        unreachable!("commit_compaction_step on a heap that proposes no steps")
    }
}

/// What one round of incremental compaction achieved. Returned by the backends'
/// `compact_incrementally`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CompactionProgress {
    /// How many steps were executed.
    pub steps: usize,
    /// How many bytes were copied in total.
    pub bytes_moved: u64,
    /// Whether the heap reported quiescence (rather than the budget running
    /// out). When true there is nothing left to gain from further rounds.
    pub quiesced: bool,
}

/// Marker: this heap's [`RelocatableHeap::propose_compaction_step`] really
/// proposes steps.
///
/// Compare `ExactSizeIterator`, which marks `size_hint` as meaning something.
/// The compaction methods live on the base trait (defaulted) so that a shared
/// flush path can call them without a bound; this marker is what gates the
/// *user-facing* compaction controls, which should not exist at all on a backend
/// whose heap does not compact.
pub trait IncrementallyCompactableHeap: RelocatableHeap {}
