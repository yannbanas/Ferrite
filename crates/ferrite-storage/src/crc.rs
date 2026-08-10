//! CRC-32C (Castagnoli) used for page and journal-record checksums.
//!
//! Implemented here rather than pulled from a crate so the on-disk
//! checksum algorithm is pinned by this repository and cannot drift with a
//! dependency upgrade.

const POLY: u32 = 0x82f6_3b78;

const TABLE: [u32; 256] = build_table();

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// CRC-32C of `bytes`, standard reflected form with pre/post inversion.
pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in bytes {
        crc = (crc >> 8) ^ TABLE[((crc ^ b as u32) & 0xff) as usize];
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
        assert_eq!(crc32c(b"a"), 0xc1d0_4330);
    }

    #[test]
    fn detects_single_bit_flip() {
        let mut data = vec![0u8; 512];
        data[100] = 0x5a;
        let before = crc32c(&data);
        data[100] ^= 0x01;
        assert_ne!(before, crc32c(&data));
    }
}
