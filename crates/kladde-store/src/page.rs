//! Page framing and the header fields (`spec/file-format.md`).

use crate::consts::*;
use crate::crc::Crc32c;

/// A page-sized buffer.
pub type PageBuf = [u8; PAGE_SIZE];

/// A fresh, zeroed page buffer on the heap.
pub fn new_page() -> Box<PageBuf> {
    vec![0u8; PAGE_SIZE]
        .into_boxed_slice()
        .try_into()
        .expect("PAGE_SIZE bytes")
}

/// The fields of a header page's `header`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct HeaderFields {
    pub format_version: u16,
    pub min_reader_version: u16,
    pub root_allocation: u32,
    pub schema_table: u32,
    pub consolidator_state: u32,
    pub root_fingerprint: [u8; 16],
    pub journal_pointer: u32,
}

impl HeaderFields {
    fn encode(&self, out: &mut [u8]) {
        out[0..11].copy_from_slice(&MAGIC);
        out[11] = LOG2_PAGE_SIZE;
        out[12..14].copy_from_slice(&self.format_version.to_le_bytes());
        out[14..16].copy_from_slice(&self.min_reader_version.to_le_bytes());
        out[16..20].copy_from_slice(&self.root_allocation.to_le_bytes());
        out[20..24].copy_from_slice(&self.schema_table.to_le_bytes());
        out[24..28].copy_from_slice(&self.consolidator_state.to_le_bytes());
        out[28..44].copy_from_slice(&self.root_fingerprint);
        out[44..48].copy_from_slice(&self.journal_pointer.to_le_bytes());
    }

    fn decode(bytes: &[u8]) -> Result<Self, PageError> {
        if bytes[0..11] != MAGIC {
            return Err(PageError::NotKladde);
        }
        if bytes[11] != LOG2_PAGE_SIZE {
            return Err(PageError::UnsupportedPageSize(bytes[11]));
        }
        let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        let mut root_fingerprint = [0u8; 16];
        root_fingerprint.copy_from_slice(&bytes[28..44]);
        Ok(HeaderFields {
            format_version: u16_at(12),
            min_reader_version: u16_at(14),
            root_allocation: u32_at(16),
            schema_table: u32_at(20),
            consolidator_state: u32_at(24),
            root_fingerprint,
            journal_pointer: u32_at(44),
        })
    }
}

/// Why a page did not decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageError {
    /// The CRC does not match, or `content_size` leaves no room for it.
    BadChecksum,
    /// A header page without the kladde magic.
    NotKladde,
    /// A header page declaring a page size this implementation cannot read.
    UnsupportedPageSize(u8),
}

/// A page that passed its checks.
#[derive(Debug, Clone)]
pub struct DecodedPage {
    pub header: Option<HeaderFields>,
    pub kind: u8,
    pub epoch: u64,
    /// Where `content` lies within the page.
    pub content: std::ops::Range<usize>,
}

/// Where `kind` sits in a header page or an ordinary one.
fn kind_offset(is_header: bool) -> usize {
    if is_header {
        HEADER_FIELDS
    } else {
        0
    }
}

/// Frames `content` into `buf` as a complete page, padding with zeros.
/// `header` is `Some` for a header page. Panics if `content` does not fit.
pub fn encode_page(
    buf: &mut PageBuf,
    header: Option<&HeaderFields>,
    kind: u8,
    epoch: u64,
    content: &[u8],
) {
    let k = kind_offset(header.is_some());
    if let Some(h) = header {
        h.encode(&mut buf[..HEADER_FIELDS]);
    }
    let start = k + CONTENT_OFFSET;
    assert!(
        start + content.len() + 4 <= PAGE_SIZE,
        "page content of {} bytes does not fit",
        content.len()
    );
    buf[k] = kind;
    buf[k + 1..k + 9].copy_from_slice(&epoch.to_le_bytes());
    buf[k + 9..k + 11].copy_from_slice(&(content.len() as u16).to_le_bytes());
    buf[start..start + content.len()].copy_from_slice(content);
    let crc = frame_crc(buf, k, content.len());
    let end = start + content.len();
    buf[end..end + 4].copy_from_slice(&crc.to_le_bytes());
    buf[end + 4..].fill(0);
}

/// Frames a page whose `content` is already in place at its offset: writes
/// `kind`, `epoch`, `content_size`, the CRC, and zero padding.
pub fn seal_page(
    buf: &mut PageBuf,
    header: Option<&HeaderFields>,
    kind: u8,
    epoch: u64,
    len: usize,
) {
    let k = kind_offset(header.is_some());
    if let Some(h) = header {
        h.encode(&mut buf[..HEADER_FIELDS]);
    }
    let start = k + CONTENT_OFFSET;
    assert!(start + len + 4 <= PAGE_SIZE);
    buf[k] = kind;
    buf[k + 1..k + 9].copy_from_slice(&epoch.to_le_bytes());
    buf[k + 9..k + 11].copy_from_slice(&(len as u16).to_le_bytes());
    let crc = frame_crc(buf, k, len);
    let end = start + len;
    buf[end..end + 4].copy_from_slice(&crc.to_le_bytes());
    buf[end + 4..].fill(0);
}

/// The CRC over everything before `content_size`, then the content.
fn frame_crc(buf: &PageBuf, k: usize, len: usize) -> u32 {
    let mut c = Crc32c::new();
    c.update(&buf[..k + 9]);
    let start = k + CONTENT_OFFSET;
    c.update(&buf[start..start + len]);
    c.value()
}

/// Checks a page's framing and decodes it.
pub fn decode_page(buf: &PageBuf, is_header: bool) -> Result<DecodedPage, PageError> {
    let k = kind_offset(is_header);
    let len = u16::from_le_bytes([buf[k + 9], buf[k + 10]]) as usize;
    let start = k + CONTENT_OFFSET;
    if start + len + 4 > PAGE_SIZE {
        return Err(PageError::BadChecksum);
    }
    let stored = u32::from_le_bytes(buf[start + len..start + len + 4].try_into().unwrap());
    if frame_crc(buf, k, len) != stored {
        return Err(PageError::BadChecksum);
    }
    let header = if is_header {
        Some(HeaderFields::decode(&buf[..HEADER_FIELDS])?)
    } else {
        None
    };
    Ok(DecodedPage {
        header,
        kind: buf[k],
        epoch: u64::from_le_bytes(buf[k + 1..k + 9].try_into().unwrap()),
        content: start..start + len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_data_page_round_trips() {
        let mut buf = new_page();
        encode_page(&mut buf, None, KIND_DATA, 7, b"hello");
        let d = decode_page(&buf, false).unwrap();
        assert_eq!(d.kind, KIND_DATA);
        assert_eq!(d.epoch, 7);
        assert_eq!(&buf[d.content.clone()], b"hello");
        assert_eq!(d.content.start, CONTENT_OFFSET);
    }

    #[test]
    fn a_header_page_round_trips() {
        let h = HeaderFields {
            root_allocation: 3,
            schema_table: 4,
            consolidator_state: 5,
            root_fingerprint: [9; 16],
            journal_pointer: 12,
            ..Default::default()
        };
        let mut buf = new_page();
        encode_page(&mut buf, Some(&h), KIND_ADDRESS_TABLE, 2, &[1, 2, 3]);
        let d = decode_page(&buf, true).unwrap();
        assert_eq!(d.header.as_ref(), Some(&h));
        assert_eq!(d.epoch, 2);
        assert_eq!(d.content.start, HEADER_CONTENT_OFFSET);
    }

    #[test]
    fn corruption_and_a_wild_content_size_are_rejected() {
        let mut buf = new_page();
        encode_page(&mut buf, None, KIND_DATA, 1, &[5; 100]);
        let mut bad = buf.clone();
        bad[CONTENT_OFFSET + 50] ^= 1;
        assert_eq!(
            decode_page(&bad, false).unwrap_err(),
            PageError::BadChecksum
        );
        let mut wild = buf.clone();
        wild[9..11].copy_from_slice(&4090u16.to_le_bytes());
        assert_eq!(
            decode_page(&wild, false).unwrap_err(),
            PageError::BadChecksum
        );
        // The padding is not covered.
        let mut padded = buf.clone();
        padded[PAGE_SIZE - 1] = 0xAA;
        assert!(decode_page(&padded, false).is_ok());
    }

    #[test]
    fn a_full_page_fits_exactly() {
        let mut buf = new_page();
        let content = vec![7u8; MAX_PAGE_CONTENT];
        encode_page(&mut buf, None, KIND_DATA, 1, &content);
        assert!(decode_page(&buf, false).is_ok());
        let content = vec![7u8; MAX_HEADER_CONTENT];
        encode_page(
            &mut buf,
            Some(&HeaderFields::default()),
            KIND_ADDRESS_TABLE,
            1,
            &content,
        );
        assert!(decode_page(&buf, true).is_ok());
    }
}
