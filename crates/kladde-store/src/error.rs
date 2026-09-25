//! The error type of the storage layer, and of everything built on it.

use std::fmt;
use std::io;

/// Why a store operation failed.
///
/// I/O failures convert into it, so `?` works on storage calls inside
/// functions returning [`Result`].
///
/// ```
/// use kladde_store::Error;
///
/// let e: Error = std::io::Error::other("disk on fire").into();
/// assert!(matches!(e, Error::Io(_)));
/// assert_eq!(e.to_string(), "I/O error: disk on fire");
/// ```
#[derive(Debug)]
pub enum Error {
    /// Reading or writing the file failed.
    Io(io::Error),
    /// The file violates the format.
    Corrupt(String),
    /// The file is not a kladde file.
    NotKladde,
    /// The file needs a newer reader than this one.
    UnsupportedVersion {
        /// The file's `min_reader_version`.
        min_reader_version: u16,
    },
    /// The file's root type differs from the one the application expects.
    SchemaMismatch {
        /// The fingerprint of the application's root type.
        expected: [u8; 16],
        /// The fingerprint the file records.
        found: [u8; 16],
    },
    /// An earlier failure made the store's in-memory state untrustworthy.
    /// Drop it and open the file again.
    Poisoned,
    /// A pointer names no live allocation.
    DanglingPointer(u32),
    /// An allocation would exceed the format's bounds.
    OutOfBounds,
    /// Every allocation id is in use.
    IdsExhausted,
}

/// `Result` with the storage layer's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

pub(crate) fn corrupt(msg: impl Into<String>) -> Error {
    Error::Corrupt(msg.into())
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Corrupt(m) => write!(f, "corrupt kladde file: {m}"),
            Error::NotKladde => write!(f, "not a kladde file"),
            Error::UnsupportedVersion { min_reader_version } => {
                write!(
                    f,
                    "the file needs reader version {min_reader_version} or newer"
                )
            }
            Error::SchemaMismatch { .. } => {
                write!(f, "the file's root type differs from the application's")
            }
            Error::Poisoned => write!(
                f,
                "the store is poisoned by an earlier failure; reopen the file"
            ),
            Error::DanglingPointer(id) => write!(f, "pointer {id} names no live allocation"),
            Error::OutOfBounds => write!(f, "an allocation would exceed 2^32 - 1 bytes"),
            Error::IdsExhausted => write!(f, "every allocation id is in use"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}
