//! The multicast hash filter that every Ethernet MAC in this tree computes
//! the same way.
//!
//! A MAC with more multicast groups than it has exact-match address slots
//! keeps a bitmap instead: each address hashes to one bucket, and a frame
//! whose address lands in a set bucket is let through. The hash is not a
//! choice the driver makes --- the hardware computes the same thing on the
//! way in, so the driver has to reproduce it exactly. The XGMAC, the
//! BCM2711's GENET and Cadence's macb all use the Ethernet CRC-32 for it,
//! which is why this lives here and not in one of them.
//!
//! Nothing here has run against hardware.

/// The number of bits a MAC address has, which is what the CRC is taken
/// over.
pub const ETH_ALEN: usize = 6;

/// The Ethernet CRC-32 of an address, complemented --- which is to say the
/// ordinary CRC-32 that `crc32fast`, zip and PNG all produce.
///
/// Linux's hash filters are written in terms of `crc32_le(~0, addr, 6)`,
/// which is the SAME algorithm with the final complement left off, and then
/// complement it themselves. So a driver ported from Linux that reaches for
/// a CRC-32 crate has already had one of the steps done for it, and keeping
/// Linux's `~` on top of it gives a different bucket --- in range, stable,
/// and wrong.
#[inline]
pub fn ether_crc32(addr: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(addr);
    hasher.finalize()
}

/// The hash bucket an address falls in, for a table of `buckets` bits.
///
/// The CRC's bits are reversed and the top `log2(buckets)` of the result
/// kept --- which are the BOTTOM bits of the CRC, so the low end of the
/// checksum is what moves the bucket. Skipping the reversal still gives a
/// number in range, so the mistake shows up only as an interface that drops
/// exactly the groups it was asked to join.
///
/// `buckets` must be a power of two of at most 2^32; anything else gets the
/// whole reversed word, which is what a caller asking for a nonsense table
/// deserves rather than a panic in a driver.
#[inline]
pub fn hash_bucket_of(addr: &[u8], buckets: u32) -> u32 {
    let reversed = ether_crc32(addr).reverse_bits();
    match buckets.checked_ilog2() {
        Some(bits) if bits < 32 => reversed >> (32 - bits),
        _ => reversed,
    }
}

/// Set an address's bit in a hash table held as 32-bit words.
///
/// The table is indexed word-first: bucket N is bit N % 32 of word N / 32.
#[inline]
pub fn hash_table_set(table: &mut [u32], addr: &[u8], buckets: u32) {
    let bucket = hash_bucket_of(addr, buckets);
    if let Some(word) = table.get_mut((bucket >> 5) as usize) {
        *word |= 1 << (bucket & 0x1f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `crc32_le` as Linux defines it: the reflected CRC-32, polynomial
    /// 0xedb88320, with NO final complement. Every Linux hash filter is
    /// written against this, so it is the oracle here.
    fn crc32_le(mut crc: u32, data: &[u8]) -> u32 {
        for &byte in data {
            crc ^= byte as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        crc
    }

    #[test]
    fn the_crate_agrees_with_the_standard_vector() {
        // The CRC-32 of "123456789" is this in every table that lists one.
        assert_eq!(ether_crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn the_crate_is_linuxs_crc_with_the_complement_already_done() {
        // This is the whole reason a port from Linux can drop a step: the
        // crate finishes with the complement that Linux applies by hand.
        for addr in [
            [0x01u8, 0x00, 0x5e, 0x00, 0x00, 0x01],
            [0x33, 0x33, 0x00, 0x00, 0x00, 0x01],
            [0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            [0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            [0xde, 0xad, 0xbe, 0xef, 0x00, 0x01],
        ] {
            assert_eq!(
                ether_crc32(&addr),
                !crc32_le(!0, &addr),
                "address {:02x?}",
                addr
            );
        }
    }

    #[test]
    fn the_bucket_matches_what_linux_computes() {
        // bitrev32(~crc32_le(~0, addr, 6)) >> (32 - log2(buckets)).
        for addr in [
            [0x01u8, 0x00, 0x5e, 0x12, 0x34, 0x56],
            [0x33, 0x33, 0xff, 0x00, 0x00, 0x02],
            [0x02, 0x11, 0x22, 0x33, 0x44, 0x55],
        ] {
            for (buckets, shift) in [(64u32, 26u32), (128, 25), (256, 24)] {
                let linux = (!crc32_le(!0, &addr)).reverse_bits() >> shift;
                assert_eq!(
                    hash_bucket_of(&addr, buckets),
                    linux,
                    "address {:02x?}, {} buckets",
                    addr,
                    buckets
                );
            }
        }
    }

    #[test]
    fn the_bucket_always_falls_inside_its_table() {
        for buckets in [64u32, 128, 256] {
            for i in 0..64u8 {
                let addr = [0x01, 0x00, 0x5e, 0, 0, i];
                assert!(hash_bucket_of(&addr, buckets) < buckets);
            }
        }
    }

    #[test]
    fn the_low_end_of_the_checksum_is_what_moves_the_bucket() {
        // The reversal puts the CRC's bottom bits at the top, which is why
        // two addresses differing in one byte usually land apart.
        let a = hash_bucket_of(&[0x01, 0x00, 0x5e, 0, 0, 1], 256);
        let b = hash_bucket_of(&[0x01, 0x00, 0x5e, 0, 0, 2], 256);
        assert_ne!(a, b);
        // Without the reversal the shift would keep the CRC's TOP bits, and
        // the whole thing would still be a number in range.
        let unreversed = ether_crc32(&[0x01, 0x00, 0x5e, 0, 0, 1]) >> 24;
        assert!(unreversed < 256);
        assert_ne!(a, unreversed);
    }

    #[test]
    fn a_table_entry_is_one_bit_of_one_word() {
        let mut table = [0u32; 8];
        let addr = [0x01, 0x00, 0x5e, 0, 0, 1];
        hash_table_set(&mut table, &addr, 256);
        assert_eq!(table.iter().map(|w| w.count_ones()).sum::<u32>(), 1);
        let bucket = hash_bucket_of(&addr, 256);
        assert_ne!(table[(bucket >> 5) as usize] & (1 << (bucket & 0x1f)), 0);
        // Adding it again is idempotent.
        hash_table_set(&mut table, &addr, 256);
        assert_eq!(table.iter().map(|w| w.count_ones()).sum::<u32>(), 1);
    }

    #[test]
    fn a_table_too_small_for_the_bucket_is_left_alone_rather_than_panicking() {
        // A two-word table with a 256-bucket hash: most addresses fall off
        // the end. Dropping them beats a panic inside a driver.
        let mut table = [0u32; 2];
        for i in 0..64u8 {
            hash_table_set(&mut table, &[0x01, 0x00, 0x5e, 0, 0, i], 256);
        }
        assert!(table.iter().map(|w| w.count_ones()).sum::<u32>() <= 64);
    }
}
