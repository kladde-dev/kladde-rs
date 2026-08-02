//! The backend trait split: [`Backend`] (shared type carrier + read-only
//! allocator queries), [`ReadBackend`] (stored-byte reads, `&mut self`), and
//! [`WriteBackend`] (stored-byte writes + address-hidden allocation, `&self`).
//!
//! A backend is **composed of** an [`Allocator`](crate::Allocator) and a
//! [`Storage`](crate::Storage) -- it does *not* extend `Allocator`. That lets the
//! two layers choose their `&self`/`&mut self` and their address visibility
//! independently: the reusable `Allocator` keeps a plain `&mut self` API, while
//! the backend presents the `&self` write facade the guard model needs (its
//! interior mutability lives in the concrete backend, never in the allocator).
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

use crate::allocator::AllocError;
use crate::pointer::{ResolvedPointer, UniquePointerFixedSize, UniquePointerResizable};
use crate::word::Word;

/// Backend-layer error: an [`AllocError`] (dangling/corrupt id) *or* storage I/O.
/// The backend touches `Storage`, so its errors are a superset of the pure
/// allocator's. (Could become an associated `type Error` if backends need to
/// differ; a concrete enum is enough for now.)
#[derive(Debug)]
pub enum BackendError {
    Alloc(AllocError),
    Io(io::Error),
}

impl From<AllocError> for BackendError {
    fn from(e: AllocError) -> Self {
        Self::Alloc(e)
    }
}
impl From<io::Error> for BackendError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendError::Alloc(e) => write!(f, "allocator error: {e:?}"),
            BackendError::Io(e) => write!(f, "storage I/O error: {e}"),
        }
    }
}
impl std::error::Error for BackendError {}

/// Shared type carrier **and** the always-safe read-only allocator queries.
///
/// Every backend has exactly one `Pointer` and one `Size`, declared here once so
/// the two halves structurally can't disagree and `B::Pointer`/`B::Size` stay
/// unambiguous even under `ReadBackend + WriteBackend` (two sibling traits each
/// declaring a `Pointer` would make that shorthand an E0221 error).
///
/// The read-only *queries* (`size`, `resolve`, and the sizedness-specialized
/// forms) live here too, because they answer from the always-up-to-date
/// in-memory allocator and so are valid in *both* the read and write phases;
/// only reads of *stored bytes* need the read/write isolation. They never expose
/// per-allocation `meta` (that stays backend-internal) and never touch storage,
/// so they fail only on a dangling id -- hence `Result<_, AllocError>`, not
/// `BackendError`.
///
/// A bare `B: Backend` bound therefore guarantees the types plus these queries,
/// but *not* read or write access to stored bytes -- that is what the two halves
/// are for.
pub trait Backend {
    /// The concrete, `Copy` serialized id (e.g. `Pointer<u32>`).
    type Pointer: Copy;
    /// Offsets and allocation sizes.
    type Size: Word;

    /// The allocation's size, or `Err(DanglingPointer)` for a non-live id.
    fn size(&self, p: Self::Pointer) -> Result<Self::Size, AllocError>;

    /// Recover the owned handle (with its sizedness) for `p`. By convention a
    /// `load`-time operation (it can mint a second owner of an owned region).
    fn resolve(&self, p: Self::Pointer) -> Result<ResolvedPointer<Self::Pointer>, AllocError>;

    /// Like [`Backend::resolve`] but returns the fixed-size handle directly, with
    /// a single error path (`WrongSizedness` if `p` is resizable). Convenient in
    /// a `load` that already knows the sizedness.
    fn resolve_fixed_size(
        &self,
        p: Self::Pointer,
    ) -> Result<UniquePointerFixedSize<Self::Pointer>, AllocError> {
        match self.resolve(p)? {
            ResolvedPointer::Fixed(h) => Ok(h),
            ResolvedPointer::Resizable(_) => Err(AllocError::WrongSizedness),
        }
    }
    /// The resizable counterpart of [`Backend::resolve_fixed_size`].
    fn resolve_resizable(
        &self,
        p: Self::Pointer,
    ) -> Result<UniquePointerResizable<Self::Pointer>, AllocError> {
        match self.resolve(p)? {
            ResolvedPointer::Resizable(h) => Ok(h),
            ResolvedPointer::Fixed(_) => Err(AllocError::WrongSizedness),
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
/// needs lives inside the concrete backend, never in the reusable `Allocator`.
/// Addresses never surface here (contrast `Allocator::resize`, which reports a
/// `Relocation`): the backend consumes the relocation internally to move bytes.
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

    /// Convert a fixed-size allocation to resizable (address-hidden counterpart
    /// of `Allocator::make_resizable`), moving bytes if it relocates.
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
