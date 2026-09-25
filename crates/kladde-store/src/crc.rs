//! CRC-32C (Castagnoli), as `spec/file-format.md` specifies it: reflected
//! polynomial `0x82F63B78`, initial value `0xFFFFFFFF`, final XOR
//! `0xFFFFFFFF`. Slicing-by-8, which is fast enough that the checksum never
//! shows next to the I/O it guards.

const POLY: u32 = 0x82F6_3B78;

const fn make_tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut s = 1;
        while s < 8 {
            t[s][i] = (t[s - 1][i] >> 8) ^ t[0][(t[s - 1][i] & 0xff) as usize];
            s += 1;
        }
        i += 1;
    }
    t
}

static TABLES: [[u32; 256]; 8] = make_tables();

/// A running CRC-32C.
///
/// [`value`](Crc32c::value) is the CRC of everything fed so far, and feeding
/// more afterwards continues the same computation, which is what the journal's
/// chain needs: every checksum in a segment is a value of one running CRC.
#[derive(Clone, Copy, Debug)]
pub struct Crc32c {
    state: u32,
}

impl Default for Crc32c {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32c {
    /// A CRC of the empty input.
    pub const fn new() -> Self {
        Crc32c { state: !0 }
    }

    /// Feeds `bytes`.
    pub fn update(&mut self, bytes: &[u8]) {
        let t = &TABLES;
        let mut crc = self.state;
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            let lo = u32::from_le_bytes([c[0], c[1], c[2], c[3]]) ^ crc;
            let hi = u32::from_le_bytes([c[4], c[5], c[6], c[7]]);
            crc = t[7][(lo & 0xff) as usize]
                ^ t[6][((lo >> 8) & 0xff) as usize]
                ^ t[5][((lo >> 16) & 0xff) as usize]
                ^ t[4][(lo >> 24) as usize]
                ^ t[3][(hi & 0xff) as usize]
                ^ t[2][((hi >> 8) & 0xff) as usize]
                ^ t[1][((hi >> 16) & 0xff) as usize]
                ^ t[0][(hi >> 24) as usize];
        }
        for &b in chunks.remainder() {
            crc = t[0][((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
        }
        self.state = crc;
    }

    /// The CRC-32C of everything fed so far.
    pub fn value(&self) -> u32 {
        !self.state
    }
}

/// The CRC-32C of `bytes`.
pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut c = Crc32c::new();
    c.update(bytes);
    c.value()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_check_value_from_the_specification() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(0xE306_9283u32.to_le_bytes(), [0x83, 0x92, 0x06, 0xE3]);
    }

    #[test]
    fn slicing_agrees_with_bytewise_at_every_split() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 31 + 7) as u8).collect();
        let whole = crc32c(&data);
        for split in [0, 1, 7, 8, 9, 63, 500, 999, 1000] {
            let mut c = Crc32c::new();
            c.update(&data[..split]);
            c.update(&data[split..]);
            assert_eq!(c.value(), whole, "split at {split}");
        }
        let mut c = Crc32c::new();
        for b in &data {
            c.update(std::slice::from_ref(b));
        }
        assert_eq!(c.value(), whole);
    }

    #[test]
    fn a_message_followed_by_its_own_crc_ends_in_a_constant_state() {
        // The reason the journal's chain is blind to its own checksums.
        let mut residues = Vec::new();
        for msg in [&b"a"[..], b"hello", b"kladde journal"] {
            let mut c = Crc32c::new();
            c.update(msg);
            let v = c.value();
            c.update(&v.to_le_bytes());
            residues.push(c.value());
        }
        assert!(residues.windows(2).all(|w| w[0] == w[1]));
    }
}
