//! The backend trait split: [`Backend`] (shared type carrier + read-only
//! heap queries), [`ReadBackend`] (stored-byte reads, `&mut self`), and
//! [`WriteBackend`] (stored-byte writes + address-hidden allocation, `&self`).
//!
//! A backend is **composed of** a [`RelocatableHeap`](crate::RelocatableHeap) and
//! a [`Storage`](crate::Storage) -- it does *not* extend `RelocatableHeap`. That
//! lets the two layers choose their `&self`/`&mut self` and their address
//! visibility independently: the reusable heap keeps a plain `&mut self` API,
//! while the backend presents the `&self` write facade the guard model needs (its
//! interior mutability lives in the concrete backend, never in the heap).
//!
//! ## Why the read/write split, and the `&self`/`&mut self` asymmetry
//!
//! `ReadBackend` and `WriteBackend` are split like `std::io::Read`/`Write`:
//! `ReadBackend` has *no* write or allocation surface at all, so a
//! `&mut impl ReadBackend` genuinely cannot mutate -- which is how the
//! read/write phases of a journaled backend are enforced (a shared `&Backend`
//! couldn't, because it would still carry `&self` write methods).
//!
//! - **write = `&self`**: a parent guard holds `&B` and hands each nested field
//!   guard the *same* `&B` by reborrow, so sibling guards can coexist.
//! - **read = `&mut self`**: `load` is *sequential* (one field/element after
//!   another), so a single `&mut` reborrowed down the recursion suffices. This
//!   dissolves the `Seek::seek`-takes-`&mut self` problem and lets the read path
//!   hand out the real seekable cursor with no `RefCell`. As a free side effect,
//!   the borrow checker forbids `load` (needs `&mut B`) while any guard (holds
//!   `&B`) is alive -- automatic "don't read stale data mid-write".

use std::io::{self, Read, Seek};

use crate::heap::CompactionProgress;
use crate::pointer::{ResolvedPointer, UniquePointerFixedSize, UniquePointerResizable};
use crate::word::Word;

/// Backend-layer error. A bad/corrupt *id* is the backend's error
/// (`DanglingPointer`/`WrongSizedness`) even though the heap owns the
/// `id -> address` table, because the backend is what mints ids and decodes them
/// off the file; it also touches `Storage`, hence `Io`. The heap's own
/// [`HeapError`](crate::HeapError) variants don't appear here -- the write path
/// treats `OutOfMemory` as impossible (in-memory) and unwraps, and
/// `UnknownId`/`DuplicateId` surface as `DanglingPointer`.
#[derive(Debug)]
pub enum BackendError {
    /// The id isn't a live allocation (freed / never existed / corrupt bytes).
    DanglingPointer,
    /// A `*_fixed_size`/`*_resizable` query found the *other* sizedness.
    WrongSizedness,
    Io(io::Error),
}

impl From<io::Error> for BackendError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendError::DanglingPointer => write!(f, "dangling pointer"),
            BackendError::WrongSizedness => write!(f, "wrong sizedness"),
            BackendError::Io(e) => write!(f, "storage I/O error: {e}"),
        }
    }
}
impl std::error::Error for BackendError {}

/// Shared type carrier **and** the always-safe read-only heap queries.
///
/// Every backend has exactly one `Pointer` and one `Size`, declared here once so
/// the two halves structurally can't disagree and `B::Pointer`/`B::Size` stay
/// unambiguous even under `ReadBackend + WriteBackend` (two sibling traits each
/// declaring a `Pointer` would make that shorthand an E0221 error).
///
/// The read-only *queries* (`size`, `resolve`, and the sizedness-specialized
/// forms) live here too, because they answer from the backend's own id table and
/// so are valid in *both* the read and write phases; only reads of *stored bytes*
/// need the read/write isolation. They fail only on a bad id, hence
/// `Result<_, BackendError>` with no I/O.
///
/// A bare `B: Backend` bound therefore guarantees the types plus these queries,
/// but *not* read or write access to stored bytes -- that is what the two halves
/// are for.
pub trait Backend {
    /// The concrete, serialized id (e.g. `Pointer<u32>`).
    ///
    /// Bounded by [`PointerRepr`](crate::PointerRepr) rather than merely `Copy`
    /// so that every downstream guard and container can serialize an
    /// `Option<Self::Pointer>` without restating the bound.
    type Pointer: crate::PointerRepr;
    /// Offsets and allocation sizes.
    type Size: Word;

    /// The allocation's size, or `Err(DanglingPointer)` for a non-live id.
    fn size(&self, p: Self::Pointer) -> Result<Self::Size, BackendError>;

    /// Recover the owned handle (with its sizedness) for `p`. By convention a
    /// `load`-time operation (it can mint a second owner of an owned region).
    fn resolve(&self, p: Self::Pointer) -> Result<ResolvedPointer<Self::Pointer>, BackendError>;

    /// Like [`Backend::resolve`] but returns the fixed-size handle directly, with
    /// a single error path (`WrongSizedness` if `p` is resizable). Convenient in
    /// a `load` that already knows the sizedness.
    fn resolve_fixed_size(
        &self,
        p: Self::Pointer,
    ) -> Result<UniquePointerFixedSize<Self::Pointer>, BackendError> {
        match self.resolve(p)? {
            ResolvedPointer::Fixed(h) => Ok(h),
            ResolvedPointer::Resizable(_) => Err(BackendError::WrongSizedness),
        }
    }
    /// The resizable counterpart of [`Backend::resolve_fixed_size`].
    fn resolve_resizable(
        &self,
        p: Self::Pointer,
    ) -> Result<UniquePointerResizable<Self::Pointer>, BackendError> {
        match self.resolve(p)? {
            ResolvedPointer::Resizable(h) => Ok(h),
            ResolvedPointer::Fixed(_) => Err(BackendError::WrongSizedness),
        }
    }
}

/// Read access to *stored bytes*. Takes `&mut self` so it can hand out the real
/// seekable cursor (see the module docs). No write, alloc, or resize surface at
/// all -- that is what enforces the journaled read/write phase separation.
pub trait ReadBackend: Backend {
    /// A reader positioned at `anchor + offset`, borrowing the backend.
    ///
    /// (A fuller pass would make this `io::Result<impl Read + Seek + '_>`, since
    /// seeking a real file can fail; the in-memory backends here never fail to
    /// seek, so the simpler infallible signature keeps `load` clean for now.)
    fn read_at(&mut self, anchor: Self::Pointer, offset: Self::Size) -> impl Read + Seek + '_;
}

/// Write access to *stored bytes*, plus **address-hidden** allocation. All
/// methods take `&self` (the guard-reborrow model); the interior mutability this
/// needs lives inside the concrete backend, never in the reusable heap.
/// Addresses never surface here: the backend consumes the heap's relocation
/// report ([`RelocatableHeap::resize`](crate::RelocatableHeap::resize)'s
/// [`Relocation`](crate::Relocation)) internally to move bytes.
pub trait WriteBackend: Backend {
    fn alloc_resizable(&self, size: Self::Size) -> UniquePointerResizable<Self::Pointer>;
    fn alloc_fixed_size(&self, size: Self::Size) -> UniquePointerFixedSize<Self::Pointer>;
    fn free_resizable(&self, p: UniquePointerResizable<Self::Pointer>);
    fn free_fixed_size(&self, p: UniquePointerFixedSize<Self::Pointer>);

    /// Resize a resizable allocation, moving bytes in storage if it relocates.
    /// Fallible because the byte move is I/O.
    fn resize(
        &self,
        p: &UniquePointerResizable<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<(), BackendError>;

    /// Convert a fixed-size allocation to resizable, **minting a new id**:
    /// sizedness rides on the id itself, so there is nothing to re-tag in place.
    /// The backend allocates a fresh range of the new sizedness, copies
    /// `min(old_size, new_size)` bytes across, and frees the old one; the single
    /// owner is handed the new id back, which is what keeps this sound. There is
    /// no heap-level `make_*`.
    fn make_resizable(
        &self,
        p: UniquePointerFixedSize<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<UniquePointerResizable<Self::Pointer>, BackendError>;
    /// The reverse of [`WriteBackend::make_resizable`].
    fn make_fixed_size(
        &self,
        p: UniquePointerResizable<Self::Pointer>,
        new_size: Self::Size,
    ) -> Result<UniquePointerFixedSize<Self::Pointer>, BackendError>;

    /// Overwrite `bytes.len()` bytes at `anchor + offset`. Takes the bytes (not a
    /// returned `impl Write`) so a journaled backend keeps control of its op
    /// frame -- handing out a writer could tear a length-prefixed, checksummed
    /// journal frame and silently lose the write. (Infallible for the in-memory
    /// backends; a fuller pass returns `Result<(), BackendError>`.)
    fn write(&self, anchor: Self::Pointer, offset: Self::Size, bytes: &[u8]);

    /// Atomic resize + tail-shift + content overwrite of one region: replace the
    /// `old_len` bytes at `offset` with `new`, shifting the trailing bytes.
    fn splice(
        &self,
        p: &UniquePointerResizable<Self::Pointer>,
        offset: Self::Size,
        old_len: Self::Size,
        new: &[u8],
    );
}

/// A backend whose heap can be asked to spend a bounded amount of work squeezing
/// fragmentation out of the address space.
///
/// Sits alongside the read/write split rather than inside it: compaction is
/// neither reading nor writing *stored values*, it relocates them behind the
/// stable ids, so it is available in both phases and to `&self` holders. Only
/// backends over an
/// [`IncrementallyCompactableHeap`](crate::IncrementallyCompactableHeap)
/// implement it -- a backend over a non-compacting heap simply doesn't have the
/// method, rather than having one that silently does nothing.
pub trait CompactingBackend: Backend {
    /// Run compaction steps until the heap quiesces or `budget` bytes have been
    /// copied, then truncate the store.
    ///
    /// `budget` is in bytes and is a *ranking* input rather than a hard cap: a
    /// single step larger than the whole budget is still executed when nothing
    /// else has moved, so one oversized slide can never be starved forever.
    fn compact_incrementally(&self, budget: usize) -> CompactionProgress;
}
