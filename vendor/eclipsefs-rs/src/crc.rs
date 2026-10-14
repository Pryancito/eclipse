//! CRC32 (IEEE 802.3, reflected polynomial 0xEDB88320), the checksum
//! eclipsefs-lib used for its header; here it covers every metadata block.

const fn make_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
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

static TABLE: [u32; 256] = make_table();

/// CRC32 of `data`.
pub fn crc32(data: &[u8]) -> u32 {
    crc32_update(0, data)
}

/// Continue a CRC32: `crc32_update(crc32(a), b) == crc32(a ++ b)`.
pub fn crc32_update(crc: u32, data: &[u8]) -> u32 {
    let mut crc = !crc;
    for &b in data {
        crc = TABLE[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::{crc32, crc32_update};

    #[test]
    fn incremental_matches_one_shot() {
        let data: alloc::vec::Vec<u8> = (0..1000u32).map(|i| (i * 31) as u8).collect();
        for cut in [0, 1, 7, 500, 999, 1000] {
            let (a, b) = data.split_at(cut);
            assert_eq!(crc32_update(crc32(a), b), crc32(&data));
        }
        assert_eq!(crc32_update(0, b""), 0);
    }

    #[test]
    fn known_vectors() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(
            crc32(b"The quick brown fox jumps over the lazy dog"),
            0x414F_A339
        );
    }

    #[test]
    fn every_single_bit_flip_changes_the_checksum() {
        let data: alloc::vec::Vec<u8> = (0..256u32).map(|i| (i * 31) as u8).collect();
        let base = crc32(&data);
        for byte in 0..data.len() {
            for bit in 0..8 {
                let mut d = data.clone();
                d[byte] ^= 1 << bit;
                assert_ne!(crc32(&d), base, "flip at {}:{}", byte, bit);
            }
        }
    }

    #[test]
    fn swapped_bytes_are_detected() {
        assert_ne!(crc32(b"ab"), crc32(b"ba"));
        assert_ne!(crc32(&[0u8; 4096]), crc32(&[0u8; 4095]));
    }

    #[test]
    fn matches_the_bitwise_reference_from_eclipsefs_lib() {
        // The loop eclipsefs-lib's `EclipseFSHeader::calculate_crc32` used.
        fn slow(data: &[u8]) -> u32 {
            let mut crc: u32 = 0xFFFF_FFFF;
            for &b in data {
                crc ^= b as u32;
                for _ in 0..8 {
                    crc = if crc & 1 != 0 {
                        (crc >> 1) ^ 0xEDB8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            crc ^ 0xFFFF_FFFF
        }
        for len in [0usize, 1, 7, 64, 1000, 4096] {
            let d: alloc::vec::Vec<u8> = (0..len).map(|i| (i * 13 + 5) as u8).collect();
            assert_eq!(crc32(&d), slow(&d), "len {}", len);
        }
    }
}
