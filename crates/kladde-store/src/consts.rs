//! Format constants from `spec/file-format.md`.

/// Binary logarithm of the page size. Only 4 KiB pages are allowed.
pub const LOG2_PAGE_SIZE: u8 = 12;
/// The page size in bytes.
pub const PAGE_SIZE: usize = 1 << LOG2_PAGE_SIZE;
/// `kind` (1) + `epoch` (8) + `content_size` (2) + `crc` (4).
pub const FRAMING: usize = 15;
/// The largest `content` a non-header page holds.
pub const MAX_PAGE_CONTENT: usize = PAGE_SIZE - FRAMING;
/// The width of the `header` field of a header page.
pub const HEADER_FIELDS: usize = 48;
/// The largest `content` a header page holds.
pub const MAX_HEADER_CONTENT: usize = MAX_PAGE_CONTENT - HEADER_FIELDS;
/// Where `content` starts in a non-header page: after `kind`, `epoch`, and
/// `content_size`.
pub const CONTENT_OFFSET: usize = 11;
/// Where `content` starts in a header page.
pub const HEADER_CONTENT_OFFSET: usize = HEADER_FIELDS + CONTENT_OFFSET;
/// The bytes of a journal page that carry the transaction stream; the last 8
/// are the trailer.
pub const JOURNAL_STREAM: usize = PAGE_SIZE - 8;

/// `kind` of an address-table page, including the header pages.
pub const KIND_ADDRESS_TABLE: u8 = 0x01;
/// `kind` of a data page.
pub const KIND_DATA: u8 = 0x02;

/// The file magic, `\x8B K L A D D E \r \n \x1A \n`.
pub const MAGIC: [u8; 11] = [
    0x8B, 0x4B, 0x4C, 0x41, 0x44, 0x44, 0x45, 0x0D, 0x0A, 0x1A, 0x0A,
];
/// The specification version this implementation writes.
pub const FORMAT_VERSION: u16 = 0;
/// The oldest reader version that can read what this implementation writes.
pub const MIN_READER_VERSION: u16 = 0;
/// The newest `min_reader_version` this implementation can read.
pub const READER_VERSION: u16 = 0;

/// The format's ceiling on an `Inline` payload.
pub const MAX_INLINE: usize = 251;
/// The largest allocation size, `2^32 - 1`.
pub const MAX_ALLOCATION_SIZE: u64 = u32::MAX as u64;
