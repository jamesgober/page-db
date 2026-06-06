//! CRC32C (Castagnoli) page checksums.
//!
//! Pages are protected with CRC32C — the Castagnoli polynomial (`0x1EDC6F41`,
//! reflected `0x82F63B78`), the same family used by iSCSI, ext4, and the sibling
//! `wal-db` crate. CRC32C is chosen over the IEEE CRC32 of zip/zlib because it is
//! the variant CPUs accelerate, which keeps the door open for a hardware path
//! later without changing a stored byte.
//!
//! The implementation here is a portable software slice-by-eight table — it
//! processes eight bytes per iteration and needs no `unsafe` and no CPU feature
//! detection, so it is correct and identical on every target. Checksumming a
//! 4 KiB page costs on the order of a microsecond, well under the cost of the
//! Direct I/O it accompanies, so this is not the page path's bottleneck; a
//! hardware-accelerated variant is a measured optimization for a later release,
//! not a guess made now.

/// CRC32C generator polynomial, bit-reflected for the right-shifting form.
const POLY: u32 = 0x82F6_3B78;

/// Slice-by-eight lookup tables, generated at compile time so no table data is
/// embedded as a magic constant and the build stays reproducible.
const TABLES: [[u32; 256]; 8] = generate_tables();

const fn generate_tables() -> [[u32; 256]; 8] {
    let mut tables = [[0u32; 256]; 8];

    // Table 0: the CRC of each single byte.
    let mut n = 0;
    while n < 256 {
        let mut crc = n as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        tables[0][n] = crc;
        n += 1;
    }

    // Tables 1..8: each extends the previous by one byte position.
    let mut slice = 1;
    while slice < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = tables[slice - 1][i];
            tables[slice][i] = (prev >> 8) ^ tables[0][(prev & 0xff) as usize];
            i += 1;
        }
        slice += 1;
    }

    tables
}

/// Advance the internal (post-init, pre-final-xor) CRC state over `data`.
#[inline]
fn process(mut crc: u32, data: &[u8]) -> u32 {
    let mut chunks = data.chunks_exact(8);
    for chunk in &mut chunks {
        let lo = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let hi = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        crc ^= lo;
        crc = TABLES[7][(crc & 0xff) as usize]
            ^ TABLES[6][((crc >> 8) & 0xff) as usize]
            ^ TABLES[5][((crc >> 16) & 0xff) as usize]
            ^ TABLES[4][((crc >> 24) & 0xff) as usize]
            ^ TABLES[3][(hi & 0xff) as usize]
            ^ TABLES[2][((hi >> 8) & 0xff) as usize]
            ^ TABLES[1][((hi >> 16) & 0xff) as usize]
            ^ TABLES[0][((hi >> 24) & 0xff) as usize];
    }
    for &byte in chunks.remainder() {
        crc = (crc >> 8) ^ TABLES[0][((crc ^ byte as u32) & 0xff) as usize];
    }
    crc
}

/// A streaming CRC32C accumulator.
///
/// Use this when a checksum must be computed over several non-contiguous byte
/// ranges — for example a page header and payload with the checksum field
/// itself skipped. Feeding the ranges through one accumulator gives the same
/// result as checksumming their concatenation, with no intermediate copy.
///
/// # Examples
///
/// ```
/// use page_db::checksum::Crc32c;
///
/// // Streaming two slices equals hashing their concatenation.
/// let mut h = Crc32c::new();
/// h.update(b"123");
/// h.update(b"456789");
/// assert_eq!(h.finalize(), page_db::crc32c(b"123456789"));
/// ```
#[derive(Debug, Clone)]
pub struct Crc32c {
    state: u32,
}

impl Default for Crc32c {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32c {
    /// Start a fresh checksum.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self { state: u32::MAX }
    }

    /// Fold `data` into the running checksum.
    #[inline]
    pub fn update(&mut self, data: &[u8]) {
        self.state = process(self.state, data);
    }

    /// Consume the accumulator and return the final CRC32C value.
    #[inline]
    #[must_use]
    pub fn finalize(self) -> u32 {
        self.state ^ u32::MAX
    }
}

/// Compute the CRC32C of a single contiguous buffer.
///
/// This is the one-shot form of [`Crc32c`]. The check value of the standard
/// `"123456789"` test vector is `0xE3069283`.
///
/// # Examples
///
/// ```
/// use page_db::crc32c;
///
/// assert_eq!(crc32c(b""), 0);
/// assert_eq!(crc32c(b"123456789"), 0xE306_9283);
/// ```
#[inline]
#[must_use]
pub fn crc32c(data: &[u8]) -> u32 {
    let mut h = Crc32c::new();
    h.update(data);
    h.finalize()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn test_crc32c_empty_is_zero() {
        assert_eq!(crc32c(b""), 0);
    }

    #[test]
    fn test_crc32c_check_vector_matches_standard() {
        // The canonical CRC-32C/iSCSI check value.
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn test_crc32c_streaming_equals_one_shot() {
        let data = b"the quick brown fox jumps over the lazy dog";
        for split in 0..=data.len() {
            let (a, b) = data.split_at(split);
            let mut h = Crc32c::new();
            h.update(a);
            h.update(b);
            assert_eq!(h.finalize(), crc32c(data), "split at {split}");
        }
    }

    #[test]
    fn test_crc32c_single_bit_flip_changes_value() {
        let mut data = vec![0u8; 4096];
        for (i, b) in data.iter_mut().enumerate() {
            *b = i as u8;
        }
        let base = crc32c(&data);
        let mut flipped = data.clone();
        flipped[2048] ^= 0x01;
        assert_ne!(crc32c(&flipped), base);
    }
}
