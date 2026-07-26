//! Unsigned variable-length integer encoding (LEB128), the length-prefix
//! and count encoding Kladde uses wherever a small, non-negative number
//! has to be written compactly and read back without a fixed width.
//!
//! An unsigned integer is split into 7-bit groups, least-significant group
//! first; every group but the last has its high bit set as a
//! "continuation" marker. Small values take one byte, and the encoding is
//! byte-for-byte identical on every platform, which is what makes it
//! suitable for an on-disk format that other-language implementations must
//! reproduce.
//!
//! This is a self-contained primitive with no dependencies: the schema
//! serialization ([`kladde-schema`](../kladde_schema/index.html)) uses it
//! today, and enum tags, journal records, and other compact encodings are
//! expected to reuse it.
//!
//! ```
//! let mut buf = Vec::new();
//! kladde_varint::encode(300, &mut buf);
//! assert_eq!(buf, [0xAC, 0x02]);
//!
//! let (value, rest) = kladde_varint::decode(&buf).unwrap();
//! assert_eq!(value, 300);
//! assert!(rest.is_empty());
//! ```

/// Why [`decode`] could not read a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The input ended in the middle of a value (the last byte still had
    /// its continuation bit set).
    Truncated,
    /// The encoded value does not fit in a `u64` (more groups than a
    /// 64-bit integer can hold).
    Overflow,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Truncated => f.write_str("varint is truncated"),
            Error::Overflow => f.write_str("varint does not fit in a u64"),
        }
    }
}

impl std::error::Error for Error {}

/// Appends the LEB128 encoding of `value` to `out`.
///
/// `out` is any byte sink (`impl Extend<u8>`): a `Vec<u8>` is the common
/// case, but a streaming hasher or other accumulator works just as well, so
/// the encoding can be fed somewhere without first collecting it into a
/// buffer.
pub fn encode(value: u64, out: &mut impl Extend<u8>) {
    let mut value = value;
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.extend([byte]);
            break;
        }
        out.extend([byte | 0x80]);
    }
}

/// The number of bytes [`encode`] would produce for `value` (always at
/// least 1).
pub fn encoded_len(value: u64) -> usize {
    let mut len = 1;
    let mut value = value >> 7;
    while value != 0 {
        len += 1;
        value >>= 7;
    }
    len
}

/// Decodes one LEB128 value from the front of `input`, returning the value
/// and the bytes that follow it. Fails if `input` ends mid-value
/// ([`Error::Truncated`]) or encodes a number too large for `u64`
/// ([`Error::Overflow`]).
pub fn decode(input: &[u8]) -> Result<(u64, &[u8]), Error> {
    let mut result = 0u64;
    let mut shift = 0u32;
    let mut i = 0;
    loop {
        let byte = *input.get(i).ok_or(Error::Truncated)?;
        // A `u64` holds ten 7-bit groups at most, and the tenth (shift 63)
        // has room for a single bit only.
        if shift >= 64 {
            return Err(Error::Overflow);
        }
        let low = (byte & 0x7f) as u64;
        if shift == 63 && low > 1 {
            return Err(Error::Overflow);
        }
        result |= low << shift;
        i += 1;
        if byte & 0x80 == 0 {
            return Ok((result, &input[i..]));
        }
        shift += 7;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(value: u64) -> Vec<u8> {
        let mut buf = Vec::new();
        encode(value, &mut buf);
        assert_eq!(
            buf.len(),
            encoded_len(value),
            "encoded_len disagrees for {value}"
        );
        let (decoded, rest) = decode(&buf).unwrap();
        assert_eq!(decoded, value);
        assert!(rest.is_empty());
        buf
    }

    #[test]
    fn known_encodings() {
        assert_eq!(round_trip(0), [0x00]);
        assert_eq!(round_trip(1), [0x01]);
        assert_eq!(round_trip(127), [0x7f]);
        assert_eq!(round_trip(128), [0x80, 0x01]);
        assert_eq!(round_trip(300), [0xac, 0x02]);
        assert_eq!(round_trip(16384), [0x80, 0x80, 0x01]);
    }

    #[test]
    fn round_trips_across_the_range() {
        for value in [0, 1, 63, 64, 127, 128, 255, 256, u32::MAX as u64, u64::MAX] {
            round_trip(value);
        }
        // A spread of larger values across every group boundary.
        let mut v = 1u64;
        while v < u64::MAX / 3 {
            round_trip(v);
            round_trip(v - 1);
            v = v.wrapping_mul(3).wrapping_add(7);
        }
    }

    #[test]
    fn u64_max_is_ten_bytes() {
        let mut buf = Vec::new();
        encode(u64::MAX, &mut buf);
        assert_eq!(
            buf,
            [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]
        );
    }

    #[test]
    fn decode_returns_the_remaining_bytes() {
        let mut buf = Vec::new();
        encode(300, &mut buf);
        buf.extend_from_slice(&[0xde, 0xad]);
        let (value, rest) = decode(&buf).unwrap();
        assert_eq!(value, 300);
        assert_eq!(rest, &[0xde, 0xad]);
    }

    #[test]
    fn truncated_input_fails() {
        assert_eq!(decode(&[]), Err(Error::Truncated));
        assert_eq!(decode(&[0x80]), Err(Error::Truncated));
        assert_eq!(decode(&[0x80, 0x80]), Err(Error::Truncated));
    }

    #[test]
    fn overflowing_input_fails() {
        // Eleven continuation bytes: more groups than a u64 can hold.
        let too_long = [0x80u8; 11];
        assert_eq!(decode(&too_long), Err(Error::Overflow));
        // Ten bytes whose tenth group sets more than the single available bit.
        let too_big = [0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02];
        assert_eq!(decode(&too_big), Err(Error::Overflow));
    }
}
