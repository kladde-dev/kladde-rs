//! A small, self-contained SHA-256, used to keep [`kladde-schema`](crate)
//! dependency-free and its fingerprints exactly reproducible from the
//! published algorithm (`type-descriptors.md` §4.2). This is the standard
//! FIPS 180-4 construction; it is not performance-tuned (fingerprinting
//! runs once per schema, over a few hundred bytes).
//!
//! The primitive is [`Sha256Hasher`], a streaming hasher: bytes are pushed
//! in and absorbed 64-byte block at a time, so the caller never has to
//! materialize the whole message in a buffer. [`sha256`] is the one-shot
//! convenience over it.

use std::borrow::Borrow;

const INITIAL: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

const ROUND_CONSTANTS: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// A streaming SHA-256 hasher. Feed it bytes with [`update`](Self::update)
/// (as many times as convenient) and read the digest with
/// [`finalize`](Self::finalize).
///
/// Only one 64-byte block is buffered at a time: each time the buffer
/// fills, its block is compressed into `state` and the buffer is reused. So
/// hashing a long, incrementally built message costs a fixed 64 bytes of
/// working memory rather than a growing `Vec` of the whole message — which
/// is exactly what schema fingerprinting wants, since it assembles each
/// node's hash input piecemeal from strings, varints, and child digests.
pub struct Sha256Hasher {
    /// The eight working hash words (the `H` values of FIPS 180-4).
    state: [u32; 8],
    /// The current partial block; only the first `len % 64` bytes are live.
    buffer: [u8; 64],
    /// Total number of message bytes absorbed so far, for the length pad.
    len: u64,
}

impl Default for Sha256Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256Hasher {
    /// A fresh hasher over the empty message.
    pub fn new() -> Self {
        Sha256Hasher {
            state: INITIAL,
            buffer: [0; 64],
            len: 0,
        }
    }

    /// Absorbs `bytes` into the running digest. Accepts anything yielding
    /// `u8`s — a `&[u8]`, an array, an iterator of `u8` or `&u8` — so
    /// callers can stream a slice or push a byte at a time without building
    /// an intermediate buffer.
    pub fn update<I>(&mut self, bytes: I)
    where
        I: IntoIterator,
        I::Item: Borrow<u8>,
    {
        for byte in bytes {
            self.absorb(*byte.borrow());
        }
    }

    /// Consumes the hasher and returns the 32-byte digest, applying the
    /// FIPS 180-4 padding (a `0x80` byte, zeros, then the 64-bit big-endian
    /// bit length) as it goes.
    pub fn finalize(mut self) -> [u8; 32] {
        let bit_len = self.len.wrapping_mul(8);
        self.absorb(0x80);
        while self.len % 64 != 56 {
            self.absorb(0);
        }
        for byte in bit_len.to_be_bytes() {
            self.absorb(byte);
        }
        debug_assert_eq!(self.len % 64, 0, "the length pad completes the final block");

        let mut digest = [0u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            digest[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        digest
    }

    /// Places one byte into `buffer` at the current offset (`len % 64`),
    /// advances `len`, and compresses the block once it fills. Drives both
    /// message bytes (via [`update`](Self::update)) and the final padding
    /// (via [`finalize`](Self::finalize)); `finalize` reads the message bit
    /// length off `len` *before* padding, so counting the pad bytes here too
    /// is harmless.
    fn absorb(&mut self, byte: u8) {
        self.buffer[self.len as usize % 64] = byte;
        self.len = self.len.wrapping_add(1);
        if self.len.is_multiple_of(64) {
            self.compress();
        }
    }

    /// The FIPS 180-4 compression function over the full `buffer`, folding
    /// it into `state`.
    fn compress(&mut self) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().enumerate().take(16) {
            let start = i * 4;
            *word = u32::from_be_bytes(self.buffer[start..start + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(ROUND_CONSTANTS[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        for (slot, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
}

/// Lets a [`Sha256Hasher`] be used as a byte sink, e.g. as the output of
/// [`kladde_varint::encode`] — a varint streams straight into the digest
/// with no intermediate buffer.
impl Extend<u8> for Sha256Hasher {
    fn extend<T: IntoIterator<Item = u8>>(&mut self, iter: T) {
        self.update(iter);
    }
}

/// The 32-byte SHA-256 digest of `data`, in one shot. A convenience over
/// [`Sha256Hasher`] used by the tests; the fingerprint path drives the
/// streaming hasher directly.
#[cfg(test)]
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256Hasher::new();
    hasher.update(data);
    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use super::{sha256, Sha256Hasher};

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn matches_known_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // 55 bytes: zero padding bytes
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnop"
            )),
            "aa353e009edbaebfc6e494c8d847696896cb8b398e0173a4b5c1b636292d87c7"
        );
        // 56 bytes: exercises the extra padding block.
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // 64 bytes: exactly one block.
        assert_eq!(
            hex(&sha256(
                b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmno"
            )),
            "2ff100b36c386c65a1afc462ad53e25479bec9498ed00aa5a04de584bc25301b"
        );
        // 112 bytes: longer than one block.
        assert_eq!(
            hex(&sha256(
                b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu"
            )),
            "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1"
        );
    }

    #[test]
    fn streaming_matches_one_shot() {
        // Feeding the message in arbitrary chunks — including byte at a time
        // and across block boundaries — must equal hashing it all at once.
        let message: Vec<u8> = (0u8..=250).cycle().take(1000).collect();
        let one_shot = sha256(&message);

        for chunk in [1usize, 7, 63, 64, 65, 128] {
            let mut hasher = Sha256Hasher::new();
            for part in message.chunks(chunk) {
                hasher.update(part);
            }
            assert_eq!(hasher.finalize(), one_shot, "chunk size {chunk}");
        }

        // Pushing byte-by-byte through the `u8` iterator path, too.
        let mut hasher = Sha256Hasher::new();
        for &byte in &message {
            hasher.update([byte]);
        }
        assert_eq!(hasher.finalize(), one_shot);
    }

    #[test]
    fn extend_feeds_the_hasher() {
        // The `Extend<u8>` impl is the path varints take on the hash input.
        let mut hasher = Sha256Hasher::new();
        hasher.extend(*b"abc");
        assert_eq!(hasher.finalize(), sha256(b"abc"));
    }
}
