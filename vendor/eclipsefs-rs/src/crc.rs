//! Checksums.
//!
//! - CRC32 (IEEE 802.3, reflected polynomial 0xEDB88320), the checksum
//!   eclipsefs-lib used for its header, kept for the v0.3 format.
//! - CRC32C (Castagnoli, reflected 0x82F63B78), the one btrfs uses, for
//!   every v0.4 tree node and data block. Table-driven, eight bytes at a time.

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

const fn make_c_tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x82F6_3B78
            } else {
                crc >> 1
            };
            bit += 1;
        }
        t[0][i] = crc;
        i += 1;
    }
    let mut k = 1;
    while k < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = t[k - 1][i];
            t[k][i] = (prev >> 8) ^ t[0][(prev & 0xFF) as usize];
            i += 1;
        }
        k += 1;
    }
    t
}

static C_TABLES: [[u32; 256]; 8] = make_c_tables();

/// CRC32C of `data`.
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c_update(0, data)
}

/// Continue a CRC32C: `crc32c_update(crc32c(a), b) == crc32c(a ++ b)`.
/// Uses the SSE4.2 `crc32` instruction when the CPU has it (it works on
/// general-purpose registers, so it is fine in the kernel too).
pub fn crc32c_update(crc: u32, data: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    if hw::available() {
        // SAFETY: the CPU reports SSE4.2.
        return !unsafe { hw::update(!crc, data) };
    }
    crc32c_table(crc, data)
}

#[cfg(target_arch = "x86_64")]
mod hw {
    use core::sync::atomic::{AtomicU8, Ordering};

    /// 0: not probed yet, 1: SSE4.2, 2: none.
    static SSE42: AtomicU8 = AtomicU8::new(0);

    pub fn available() -> bool {
        match SSE42.load(Ordering::Relaxed) {
            1 => true,
            2 => false,
            _ => {
                #[allow(unused_unsafe)]
                // SAFETY: CPUID leaf 1 exists on every x86_64 CPU.
                let ecx = unsafe { core::arch::x86_64::__cpuid(1) }.ecx;
                let yes = ecx & (1 << 20) != 0;
                SSE42.store(if yes { 1 } else { 2 }, Ordering::Relaxed);
                yes
            }
        }
    }

    /// Raw (not inverted) CRC32C state over `data`.
    ///
    /// Inline assembly instead of the SSE4.2 intrinsics: those need the
    /// `sse4.2` target feature, which soft-float kernel targets (like
    /// zCore's, `-sse`) cannot enable, while `crc32` only uses
    /// general-purpose registers.
    ///
    /// # Safety
    ///
    /// The CPU must support SSE4.2 ([`available`]).
    pub unsafe fn update(crc: u32, data: &[u8]) -> u32 {
        // Three independent streams hide the instruction's latency (it
        // takes three cycles but one can start every cycle): every block
        // of 3 x STREAM bytes is three CRCs at once, joined by shifting.
        let mut c = crc;
        let mut data = data;
        while data.len() >= 3 * STREAM {
            let (a, rest) = data.split_at(STREAM);
            let (b, rest) = rest.split_at(STREAM);
            let (d, rest) = rest.split_at(STREAM);
            let (a, b, d) = (
                a.as_chunks::<8>().0,
                b.as_chunks::<8>().0,
                d.as_chunks::<8>().0,
            );
            let (mut x, mut y, mut z) = (c as u64, 0u64, 0u64);
            for i in 0..STREAM / 8 {
                // SAFETY: the caller checked for SSE4.2.
                unsafe {
                    core::arch::asm!(
                        "crc32 {x}, {u}",
                        "crc32 {y}, {v}",
                        "crc32 {z}, {w}",
                        x = inout(reg) x,
                        y = inout(reg) y,
                        z = inout(reg) z,
                        u = in(reg) u64::from_le_bytes(a[i]),
                        v = in(reg) u64::from_le_bytes(b[i]),
                        w = in(reg) u64::from_le_bytes(d[i]),
                        options(pure, nomem, nostack, preserves_flags),
                    );
                }
            }
            c = super::mulmod(SHIFT_2, x as u32) ^ super::mulmod(SHIFT_1, y as u32) ^ z as u32;
            data = rest;
        }
        let (chunks, rest) = data.as_chunks::<8>();
        let mut c = c as u64;
        for ch in chunks {
            // SAFETY: the caller checked for SSE4.2.
            unsafe {
                core::arch::asm!(
                    "crc32 {c}, {v}",
                    c = inout(reg) c,
                    v = in(reg) u64::from_le_bytes(*ch),
                    options(pure, nomem, nostack, preserves_flags),
                );
            }
        }
        let mut c = c as u32;
        for &b in rest {
            // SAFETY: as above.
            unsafe {
                core::arch::asm!(
                    "crc32 {c:e}, {v}",
                    c = inout(reg) c,
                    v = in(reg_byte) b,
                    options(pure, nomem, nostack, preserves_flags),
                );
            }
        }
        c
    }

    /// Bytes per stream: three of them fill a 4 KiB block but 16 bytes.
    const STREAM: usize = 1360;
    /// x^(8 * STREAM) and x^(16 * STREAM) mod P: what the first two streams
    /// are shifted by to join the third.
    const SHIFT_1: u32 = super::x8n(STREAM as u64);
    const SHIFT_2: u32 = super::x8n(2 * STREAM as u64);
}

/// Reflected polynomial of CRC32C.
const POLY_C: u32 = 0x82F6_3B78;

/// `a * b mod P` for CRC32C states (reflected: bit 31 is x^0).
const fn mulmod(a: u32, b: u32) -> u32 {
    let (mut m, mut b, mut p) = (1u32 << 31, b, 0u32);
    while m != 0 {
        if a & m != 0 {
            p ^= b;
        }
        b = if b & 1 != 0 {
            (b >> 1) ^ POLY_C
        } else {
            b >> 1
        };
        m >>= 1;
    }
    p
}

/// x^(8n) mod P: a raw CRC32C state followed by `n` zero bytes is the
/// state times this.
const fn x8n(n: u64) -> u32 {
    let (mut p, mut sq, mut k) = (1u32 << 31, 1u32 << 23, n);
    // sq walks x^8, x^16, x^32, ...
    while k != 0 {
        if k & 1 != 0 {
            p = mulmod(p, sq);
        }
        sq = mulmod(sq, sq);
        k >>= 1;
    }
    p
}

/// Table-driven CRC32C (slice-by-8), for CPUs without an instruction.
pub fn crc32c_table(crc: u32, data: &[u8]) -> u32 {
    let t = &C_TABLES;
    let mut crc = !crc;
    let (chunks, rest) = data.as_chunks::<8>();
    for c in chunks {
        let lo = u32::from_le_bytes([c[0], c[1], c[2], c[3]]) ^ crc;
        let hi = u32::from_le_bytes([c[4], c[5], c[6], c[7]]);
        crc = t[7][(lo & 0xFF) as usize]
            ^ t[6][((lo >> 8) & 0xFF) as usize]
            ^ t[5][((lo >> 16) & 0xFF) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][(hi & 0xFF) as usize]
            ^ t[2][((hi >> 8) & 0xFF) as usize]
            ^ t[1][((hi >> 16) & 0xFF) as usize]
            ^ t[0][(hi >> 24) as usize];
    }
    for &b in rest {
        crc = t[0][((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::{crc32, crc32_update, crc32c, crc32c_table, crc32c_update};

    #[test]
    fn crc32c_instruction_matches_the_tables() {
        let data: alloc::vec::Vec<u8> = (0..10_000u32).map(|i| (i * 31 + i / 7) as u8).collect();
        for len in [0, 1, 7, 8, 9, 63, 4096, 10_000] {
            for start in [0, 1, 3] {
                let d = &data[start..len.max(start)];
                assert_eq!(crc32c_update(0x1234, d), crc32c_table(0x1234, d));
                assert_eq!(crc32c(d), crc32c_table(0, d));
            }
        }
    }

    #[test]
    fn crc32c_streams_join_at_every_length() {
        let data: alloc::vec::Vec<u8> = (0..20_000u32).map(|i| (i * 97 + i / 13) as u8).collect();
        for len in [4079, 4080, 4081, 4096, 8160, 8192 + 7, 12_241, 20_000] {
            for start in [0, 5] {
                let d = &data[start..len];
                assert_eq!(
                    crc32c_update(0xDEAD_BEEF, d),
                    crc32c_table(0xDEAD_BEEF, d),
                    "len {len}"
                );
            }
        }
    }

    #[test]
    fn crc32c_known_vectors() {
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        // RFC 3720 (iSCSI) test vectors.
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
        assert_eq!(crc32c(&[0xFFu8; 32]), 0x62A8_AB43);
        let inc: alloc::vec::Vec<u8> = (0..32u8).collect();
        assert_eq!(crc32c(&inc), 0x46DD_794E);
        let dec: alloc::vec::Vec<u8> = (0..32u8).rev().collect();
        assert_eq!(crc32c(&dec), 0x113F_DB5C);
    }

    #[test]
    fn crc32c_fast_path_matches_bytewise() {
        fn slow(data: &[u8]) -> u32 {
            let mut crc: u32 = !0;
            for &b in data {
                crc ^= b as u32;
                for _ in 0..8 {
                    crc = if crc & 1 != 0 {
                        (crc >> 1) ^ 0x82F6_3B78
                    } else {
                        crc >> 1
                    };
                }
            }
            !crc
        }
        let data: alloc::vec::Vec<u8> = (0..5000u32).map(|i| (i * 131 + i / 7) as u8).collect();
        for len in [0usize, 1, 7, 8, 9, 15, 16, 63, 4096, 5000] {
            assert_eq!(crc32c(&data[..len]), slow(&data[..len]), "len {}", len);
        }
        for cut in [0, 3, 8, 4095] {
            let (a, b) = data.split_at(cut);
            assert_eq!(crc32c_update(crc32c(a), b), crc32c(&data));
        }
    }

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
