//! [`Encoder`] and [`Decoder`]: how a value writes its encoding into one
//! buffer, and reads it back from the sequence, which may lie in many
//! pieces.

use kladde_store::Error;

/// Appends a value's encoding to a buffer it borrows, with the primitives
/// the encoding is made of: bytes, varints, zigzag varints, and text with
/// its terminator.
///
/// ```
/// use kladde_persist::Encoder;
///
/// let mut out = Vec::new();
/// let mut e = Encoder::new(&mut out);
/// e.varint(300);
/// e.zigzag(-1);
/// e.text("hi");
/// assert_eq!(out, [0xac, 0x02, 0x01, b'h', b'i', 0xff]);
/// ```
pub struct Encoder<'a> {
    out: &'a mut Vec<u8>,
}

impl<'a> Encoder<'a> {
    /// An encoder appending to `out`. See [`Encoder`] for an example.
    pub fn new(out: &'a mut Vec<u8>) -> Self {
        Encoder { out }
    }

    /// Appends one byte.
    #[inline]
    pub fn byte(&mut self, byte: u8) {
        self.out.push(byte);
    }

    /// Appends `bytes` as they are.
    #[inline]
    pub fn bytes(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }

    /// Appends `n` zeros, as a `Fixed` value's padding.
    #[inline]
    pub fn zeros(&mut self, n: usize) {
        self.out.resize(self.out.len() + n, 0);
    }

    /// Appends `value` as an unsigned LEB128 varint, in
    /// [`varint_len`]`(value)` bytes.
    #[inline]
    pub fn varint(&mut self, value: u64) {
        kladde_varint::encode(value, self.out);
    }

    /// Appends `value` zigzag-encoded, then as a varint, so that a number of
    /// small magnitude takes one byte whatever its sign.
    #[inline]
    pub fn zigzag(&mut self, value: i64) {
        self.varint(kladde_varint::zigzag(value));
    }

    /// Appends `text` in UTF-8, then the byte `0xFF`, which UTF-8 never
    /// contains: the encoding of text, in `text.len() + 1` bytes.
    #[inline]
    pub fn text(&mut self, text: &str) {
        self.out.extend_from_slice(text.as_bytes());
        self.out.push(TERMINATOR);
    }

    /// The bytes the buffer holds, including any it held before this
    /// encoder appended to it.
    pub fn len(&self) -> usize {
        self.out.len()
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.out.is_empty()
    }
}

/// The byte that ends text: UTF-8 never contains it.
pub const TERMINATOR: u8 = 0xFF;

/// How many bytes [`Encoder::varint`] takes for `value`: one per seven bits.
///
/// ```
/// assert_eq!(kladde_persist::varint_len(127), 1);
/// assert_eq!(kladde_persist::varint_len(128), 2);
/// ```
pub const fn varint_len(value: u64) -> usize {
    let mut len = 1;
    let mut rest = value >> 7;
    while rest != 0 {
        len += 1;
        rest >>= 7;
    }
    len
}

/// Reads an encoding front to back, from one slice or from the sequence's
/// pieces as the store holds them, without copying them first.
///
/// Every method fails with [`Error::Corrupt`], naming the position, on bytes
/// that no value encodes to: input that ends early, a varint that is not
/// minimal or does not fit its type, text that is not UTF-8 or has no
/// terminator.
///
/// ```
/// use kladde_persist::Decoder;
///
/// let mut d = Decoder::from_chunks([&[0xac][..], &[0x02, b'h'], &[b'i', 0xff]].into_iter());
/// assert_eq!(d.varint()?, 300);
/// assert_eq!(d.text()?, "hi");
/// assert!(d.is_at_end());
/// # Ok::<(), kladde_store::Error>(())
/// ```
pub struct Decoder<'a> {
    current: &'a [u8],
    rest: Box<dyn Iterator<Item = &'a [u8]> + 'a>,
    /// Bytes consumed before `current`.
    consumed: u64,
}

impl<'a> Decoder<'a> {
    /// A decoder over one slice.
    ///
    /// ```
    /// use kladde_persist::Decoder;
    ///
    /// assert_eq!(Decoder::new(&[7]).byte()?, 7);
    /// # Ok::<(), kladde_store::Error>(())
    /// ```
    pub fn new(bytes: &'a [u8]) -> Self {
        Decoder {
            current: bytes,
            rest: Box::new(std::iter::empty()),
            consumed: 0,
        }
    }

    /// A decoder over consecutive pieces, such as a
    /// [`SequenceReader`](kladde_store::SequenceReader)'s. See [`Decoder`]
    /// for an example.
    pub fn from_chunks(chunks: impl Iterator<Item = &'a [u8]> + 'a) -> Self {
        Decoder {
            current: &[],
            rest: Box::new(chunks),
            consumed: 0,
        }
    }

    /// How many bytes the decoder has read so far.
    pub fn position(&self) -> u64 {
        self.consumed
    }

    /// Whether every byte has been read.
    pub fn is_at_end(&mut self) -> bool {
        self.fill()
    }

    /// An [`Error::Corrupt`] that says what was wrong at the current
    /// position, for a type's own checks.
    ///
    /// ```
    /// use kladde_persist::Decoder;
    ///
    /// let d = Decoder::new(&[]);
    /// assert_eq!(d.corrupt("no shape").to_string(), "corrupt kladde file: at byte 0: no shape");
    /// ```
    pub fn corrupt(&self, what: impl std::fmt::Display) -> Error {
        Error::Corrupt(format!("at byte {}: {what}", self.consumed))
    }

    /// Moves to the next non-empty piece if the current one is used up;
    /// `true` if there is none.
    #[inline]
    fn fill(&mut self) -> bool {
        while self.current.is_empty() {
            match self.rest.next() {
                Some(chunk) => self.current = chunk,
                None => return true,
            }
        }
        false
    }

    /// Reads one byte.
    #[inline]
    pub fn byte(&mut self) -> Result<u8, Error> {
        if self.fill() {
            return Err(self.corrupt("the input ends inside a value"));
        }
        let b = self.current[0];
        self.current = &self.current[1..];
        self.consumed += 1;
        Ok(b)
    }

    /// Reads `N` bytes.
    #[inline]
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        if let Some((head, tail)) = self.current.split_first_chunk::<N>() {
            self.current = tail;
            self.consumed += N as u64;
            return Ok(*head);
        }
        let mut out = [0; N];
        for b in &mut out {
            *b = self.byte()?;
        }
        Ok(out)
    }

    /// Appends the next `n` bytes to `out`.
    pub fn bytes_into(&mut self, mut n: usize, out: &mut Vec<u8>) -> Result<(), Error> {
        out.reserve(n);
        while n > 0 {
            if self.fill() {
                return Err(self.corrupt("the input ends inside a value"));
            }
            let k = n.min(self.current.len());
            out.extend_from_slice(&self.current[..k]);
            self.current = &self.current[k..];
            self.consumed += k as u64;
            n -= k;
        }
        Ok(())
    }

    /// Reads a minimal unsigned LEB128 varint.
    #[inline]
    pub fn varint(&mut self) -> Result<u64, Error> {
        if let Ok((value, rest)) = kladde_varint::decode_minimal(self.current) {
            self.consumed += (self.current.len() - rest.len()) as u64;
            self.current = rest;
            return Ok(value);
        }
        // Across pieces, or not a varint: gather its bytes and try again.
        let start = self.consumed;
        let mut raw = [0u8; 10];
        for i in 0..10 {
            raw[i] = self.byte()?;
            if raw[i] & 0x80 == 0 {
                return match kladde_varint::decode_minimal(&raw[..=i]) {
                    Ok((value, _)) => Ok(value),
                    Err(_) => Err(Error::Corrupt(format!(
                        "at byte {start}: a varint that is not minimal or too large"
                    ))),
                };
            }
        }
        Err(Error::Corrupt(format!(
            "at byte {start}: a varint longer than ten bytes"
        )))
    }

    /// Reads a varint that must fit `T`.
    #[inline]
    pub fn varint_as<T: TryFrom<u64>>(&mut self) -> Result<T, Error> {
        let value = self.varint()?;
        T::try_from(value).map_err(|_| {
            self.corrupt(format!(
                "{value} is out of range for {}",
                std::any::type_name::<T>()
            ))
        })
    }

    /// Reads a zigzag varint.
    #[inline]
    pub fn zigzag(&mut self) -> Result<i64, Error> {
        Ok(kladde_varint::unzigzag(self.varint()?))
    }

    /// Reads text: UTF-8 up to the byte `0xFF`, which it consumes too.
    pub fn text(&mut self) -> Result<String, Error> {
        let start = self.consumed;
        let mut bytes = Vec::new();
        loop {
            if self.fill() {
                return Err(Error::Corrupt(format!(
                    "at byte {start}: text without its terminator"
                )));
            }
            match self.current.iter().position(|&b| b == TERMINATOR) {
                Some(end) => {
                    bytes.extend_from_slice(&self.current[..end]);
                    self.current = &self.current[end + 1..];
                    self.consumed += end as u64 + 1;
                    break;
                }
                None => {
                    bytes.extend_from_slice(self.current);
                    self.consumed += self.current.len() as u64;
                    self.current = &[];
                }
            }
        }
        String::from_utf8(bytes)
            .map_err(|_| Error::Corrupt(format!("at byte {start}: text that is not UTF-8")))
    }

    /// Reads one `char` in UTF-8: its shortest form, and no surrogate.
    pub fn utf8_char(&mut self) -> Result<char, Error> {
        let first = self.byte()?;
        let len = match first {
            0x00..=0x7f => return Ok(first as char),
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => 0,
        };
        let mut buf = [first, 0, 0, 0];
        for b in buf.iter_mut().take(len).skip(1) {
            *b = self.byte()?;
        }
        std::str::from_utf8(&buf[..len.max(1)])
            .ok()
            .and_then(|s| s.chars().next())
            .ok_or_else(|| self.corrupt(format!("{:x?} is not a char in UTF-8", &buf[..len])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_cross_pieces_and_must_be_minimal() {
        let mut d = Decoder::from_chunks([&[0x80][..], &[0x80], &[0x01]].into_iter());
        assert_eq!(d.varint().unwrap(), 1 << 14);
        let mut d = Decoder::new(&[0x81, 0x00]);
        assert!(d.varint().is_err());
        let mut d = Decoder::from_chunks([&[0x81][..], &[0x00]].into_iter());
        assert!(d.varint().is_err());
        let mut d = Decoder::new(&[0x80, 0x80, 0x04]);
        assert!(d.varint_as::<u16>().is_err());
    }

    #[test]
    fn text_crosses_pieces_and_must_be_utf8() {
        let mut d = Decoder::from_chunks([&b"ab"[..], b"", b"c\xffd"].into_iter());
        assert_eq!(d.text().unwrap(), "abc");
        assert_eq!(d.byte().unwrap(), b'd');
        assert!(d.is_at_end());
        assert!(Decoder::new(b"\xc3\xff").text().is_err());
        assert!(Decoder::new(b"abc").text().is_err());
    }

    #[test]
    fn chars_must_be_well_formed() {
        for c in ['a', 'é', '€', '𝄞'] {
            let mut buf = [0u8; 4];
            let mut d = Decoder::new(c.encode_utf8(&mut buf).as_bytes());
            assert_eq!(d.utf8_char().unwrap(), c);
        }
        for bytes in [&[0xc0, 0x80][..], &[0xed, 0xa0, 0x80], &[0xff]] {
            assert!(Decoder::new(bytes).utf8_char().is_err(), "{bytes:x?}");
        }
    }
}
