//! Reading the firmware's own description of the machine, byte by byte.
//!
//! The ACPI tables are a linked structure laid out in physical memory by
//! firmware: an `RSDP` names a `RSDT` or an `XSDT`, which is an array of
//! physical addresses of further tables, one of which is the `FADT` (signature
//! `FACP`) that names the power-management timer's I/O port. Walking it means
//! **dereferencing addresses the firmware supplied**, at boot, with no fault
//! handler worth the name yet -- so every step that can be checked before the
//! next dereference should be, and the checksums are the check the ACPI spec
//! puts there for exactly that reason.
//!
//! `bare/arch/x86_64/cpu.rs` walked it by hand with `read_unaligned` at
//! literal offsets, took the `RSD PTR ` signature on faith and never looked at
//! a checksum or at the `RSDT`/`XSDT` signature. A stale copy of the signature
//! string, or a table whose length survived the one bound that was checked, is
//! then a list of arbitrary physical addresses this code goes on to read.
//!
//! Nothing here touches hardware: a table is a slice of bytes, and what these
//! functions decide is what the bytes mean. The architecture file maps the
//! physical addresses and hands the slices over.

/// Every ACPI table begins with this much header: a four-byte signature, a
/// `u32` length, a revision and a checksum.
pub const SDT_HEADER_LEN: usize = 36;

/// The `RSDP` as ACPI 1.0 defined it: signature, checksum, OEM id, revision
/// and the 32-bit `RsdtAddress`.
pub const RSDP_V1_LEN: usize = 20;

/// The `RSDP` as ACPI 2.0 extended it: the 1.0 part plus a length, the 64-bit
/// `XsdtAddress` and an extended checksum.
///
/// Only worth reading once [`rsdp_revision`] has said so, which is why it is a
/// separate number from [`RSDP_V1_LEN`].
pub const RSDP_V2_LEN: usize = 36;

/// Offsets within the `RSDP`.
mod rsdp {
    pub const SIGNATURE: usize = 0;
    pub const REVISION: usize = 15;
    pub const RSDT_ADDRESS: usize = 16;
    pub const LENGTH: usize = 20;
    pub const XSDT_ADDRESS: usize = 24;
}

/// Offsets within the `FADT`.
mod fadt {
    /// `PM_TMR_BLK`, a 32-bit I/O port address.
    pub const PM_TMR_BLK: usize = 76;
    /// `PM_TMR_LEN`: four, or the block is not there.
    pub const PM_TMR_LEN: usize = 91;
    /// `Flags`, whose bit 8 is `TMR_VAL_EXT`.
    pub const FLAGS: usize = 112;
    /// `X_PM_TMR_BLK`, a 12-byte Generic Address Structure.
    pub const X_PM_TMR_BLK: usize = 208;
    /// Bit 8 of `Flags`: the counter is 32 bits wide rather than 24.
    pub const TMR_VAL_EXT: u32 = 1 << 8;
}

/// `AddressSpaceId` in a Generic Address Structure, for system I/O ports.
const GAS_SYSTEM_IO: u8 = 1;
/// Offset of the 64-bit address within a Generic Address Structure.
const GAS_ADDRESS: usize = 4;

fn u16_at(t: &[u8], off: usize) -> Option<u16> {
    let b = t.get(off..off + 2)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(t: &[u8], off: usize) -> Option<u32> {
    let b = t.get(off..off + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn u64_at(t: &[u8], off: usize) -> Option<u64> {
    let b = t.get(off..off + 8)?;
    let mut w = [0u8; 8];
    w.copy_from_slice(b);
    Some(u64::from_le_bytes(w))
}

/// Whether the bytes sum to zero, which is the only integrity check ACPI has.
///
/// An empty slice sums to zero and is *not* a valid table; every caller here
/// has already established a length, so this answers only the sum.
pub fn checksum_ok(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b)) == 0
}

/// Which table the `RSDP` points at, and whether its entries are 64 bits wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SdtPointer {
    /// Physical address of the `RSDT` or `XSDT`.
    pub phys: u64,
    /// `true` for an `XSDT` (eight-byte entries), `false` for an `RSDT`.
    pub wide: bool,
}

/// The revision an `RSDP` declares, once its signature and its ACPI 1.0
/// checksum hold.
///
/// `None` means these bytes are not an `RSDP`, so nothing beyond them should be
/// read at all. Asking this with the [`RSDP_V1_LEN`] bytes ACPI 1.0 defines is
/// what lets a caller read the extended part **only** when the answer is 2 or
/// more, instead of dereferencing sixteen bytes it has no reason to believe are
/// there.
pub fn rsdp_revision(rsdp: &[u8]) -> Option<u8> {
    if rsdp.get(rsdp::SIGNATURE..8)? != b"RSD PTR " {
        return None;
    }
    if !checksum_ok(rsdp.get(..RSDP_V1_LEN)?) {
        return None;
    }
    Some(*rsdp.get(rsdp::REVISION)?)
}

/// Validate an `RSDP` and say which system description table it names.
///
/// Revision 2 and above carry a second length and a second checksum over the
/// whole structure, and the spec requires **both** to hold; the first twenty
/// bytes are checked either way. A revision-2 `RSDP` whose `XsdtAddress` is
/// zero falls back to the 32-bit `RsdtAddress`, which is what firmware that
/// fills in only one of them expects.
///
/// A slice of only [`RSDP_V1_LEN`] bytes is fine: every read past it is bounds
/// checked, so a revision-2 `RSDP` handed over short simply falls back to its
/// `RsdtAddress`.
pub fn rsdp_sdt(rsdp: &[u8]) -> Option<SdtPointer> {
    let revision = rsdp_revision(rsdp)?;
    if revision >= 2 {
        // Every read of the extended part is optional rather than fatal: the
        // extended checksum covers `length` bytes, and a length that does not
        // even reach the field it was read from is firmware talking nonsense,
        // not a table. A caller holding only the ACPI 1.0 bytes lands in the
        // same place -- the 32-bit `RsdtAddress` below -- instead of being
        // refused outright or having its slice read past.
        let wide = u32_at(rsdp, rsdp::LENGTH)
            .map(|len| len as usize)
            .filter(|len| *len >= rsdp::XSDT_ADDRESS + 8)
            .and_then(|len| rsdp.get(..len))
            .filter(|bytes| checksum_ok(bytes))
            .and_then(|_| u64_at(rsdp, rsdp::XSDT_ADDRESS))
            .filter(|phys| *phys != 0);
        if let Some(phys) = wide {
            return Some(SdtPointer { phys, wide: true });
        }
    }
    let phys = u32_at(rsdp, rsdp::RSDT_ADDRESS)? as u64;
    (phys != 0).then_some(SdtPointer { phys, wide: false })
}

/// The `length` an ACPI table's own header declares.
///
/// Refused when the header is short, when the length does not cover the header
/// or when it is larger than any table this kernel will walk. The caller reads
/// the header first, then maps this many bytes -- so a length that is not
/// checked here is a length that decides how much memory gets read.
pub fn table_length(header: &[u8]) -> Option<usize> {
    // A whole header, not just the four bytes the length field needs: the
    // caller is about to map `len` bytes on the strength of this answer, and a
    // slice too short to hold a header is not a table to take a length from.
    if header.len() < SDT_HEADER_LEN {
        return None;
    }
    let len = u32_at(header, 4)? as usize;
    (SDT_HEADER_LEN..=0x10000).contains(&len).then_some(len)
}

/// Whether a table carries the signature asked for and a sound checksum.
pub fn table_is(table: &[u8], signature: &[u8; 4]) -> bool {
    table.len() >= SDT_HEADER_LEN
        && &table[..4] == signature
        && table_length(table) == Some(table.len())
        && checksum_ok(table)
}

/// The physical addresses an `RSDT` or `XSDT` lists, in order.
///
/// Entries are four bytes in an `RSDT` and eight in an `XSDT`, and a trailing
/// partial entry -- a length that is not a whole number of entries past the
/// header -- is dropped rather than read half-way.
pub fn sdt_entries(sdt: &[u8], wide: bool) -> impl Iterator<Item = u64> + '_ {
    let step = if wide { 8 } else { 4 };
    let len = sdt.len();
    (SDT_HEADER_LEN..)
        .step_by(step)
        .take_while(move |off| off + step <= len)
        .filter_map(move |off| {
            if wide {
                u64_at(sdt, off)
            } else {
                u32_at(sdt, off).map(u64::from)
            }
        })
        .filter(|pa| *pa != 0)
}

/// The power-management timer's I/O port, and whether its counter is 32 bits
/// wide rather than 24.
///
/// ACPI 2.0's `X_PM_TMR_BLK` wins when it is present *and* names a system I/O
/// port: a Generic Address Structure can describe memory-mapped registers too,
/// and reading its address as a port number would aim `in`/`out` at an
/// arbitrary sixteen bits of the I/O space. The legacy `PM_TMR_BLK` is used
/// otherwise, and only when `PM_TMR_LEN` says the block is the four bytes a
/// timer occupies.
pub fn pm_timer_from_fadt(fadt: &[u8]) -> Option<(u16, bool)> {
    // `Flags` is needed by both paths and sits below either of their fields.
    let wide = (u32_at(fadt, fadt::FLAGS)? & fadt::TMR_VAL_EXT) != 0;
    if fadt.get(fadt::X_PM_TMR_BLK).copied() == Some(GAS_SYSTEM_IO) {
        if let Some(addr) = u64_at(fadt, fadt::X_PM_TMR_BLK + GAS_ADDRESS) {
            if addr != 0 && addr <= u16::MAX as u64 {
                return Some((addr as u16, wide));
            }
        }
    }
    if *fadt.get(fadt::PM_TMR_LEN)? != 4 {
        return None;
    }
    let port = u32_at(fadt, fadt::PM_TMR_BLK)?;
    (port != 0 && port <= u16::MAX as u32).then_some((port as u16, wide))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    /// Make the bytes sum to zero by writing the difference into `slot`.
    fn seal(bytes: &mut [u8], slot: usize) {
        bytes[slot] = 0;
        let sum = bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b));
        bytes[slot] = sum.wrapping_neg();
    }

    fn rsdp_v1(rsdt: u32) -> Vec<u8> {
        let mut r = vec![0u8; RSDP_V1_LEN];
        r[..8].copy_from_slice(b"RSD PTR ");
        r[rsdp::RSDT_ADDRESS..rsdp::RSDT_ADDRESS + 4].copy_from_slice(&rsdt.to_le_bytes());
        seal(&mut r, 8);
        r
    }

    fn rsdp_v2(rsdt: u32, xsdt: u64) -> Vec<u8> {
        let mut r = vec![0u8; 36];
        r[..8].copy_from_slice(b"RSD PTR ");
        r[rsdp::REVISION] = 2;
        r[rsdp::RSDT_ADDRESS..rsdp::RSDT_ADDRESS + 4].copy_from_slice(&rsdt.to_le_bytes());
        r[rsdp::LENGTH..rsdp::LENGTH + 4].copy_from_slice(&36u32.to_le_bytes());
        r[rsdp::XSDT_ADDRESS..rsdp::XSDT_ADDRESS + 8].copy_from_slice(&xsdt.to_le_bytes());
        seal(&mut r[..RSDP_V1_LEN], 8);
        // ...and the extended checksum, over all 36 bytes, in its own slot.
        seal(&mut r, 32);
        r
    }

    fn table(signature: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut t = vec![0u8; SDT_HEADER_LEN];
        t[..4].copy_from_slice(signature);
        t.extend_from_slice(body);
        let len = t.len() as u32;
        t[4..8].copy_from_slice(&len.to_le_bytes());
        seal(&mut t, 9);
        t
    }

    fn fadt_bytes(len: usize) -> Vec<u8> {
        let mut f = vec![0u8; len.max(SDT_HEADER_LEN)];
        f[..4].copy_from_slice(b"FACP");
        f
    }

    // ── the checksum ────────────────────────────────────────────────────────

    #[test]
    fn a_table_is_sound_when_its_bytes_sum_to_zero() {
        assert!(checksum_ok(&[0x10, 0xf0]));
        assert!(!checksum_ok(&[0x10, 0xf1]));
        // A wrapping sum, not a saturating one: this is a one-byte field.
        assert!(checksum_ok(&[0xff, 0x01]));
        assert!(!checksum_ok(&[]));
    }

    // ── the RSDP ────────────────────────────────────────────────────────────

    #[test]
    fn a_revision_one_rsdp_names_its_rsdt() {
        let r = rsdp_v1(0x1234_5000);
        assert_eq!(
            rsdp_sdt(&r),
            Some(SdtPointer {
                phys: 0x1234_5000,
                wide: false
            })
        );
    }

    #[test]
    fn a_revision_two_rsdp_names_its_xsdt() {
        let r = rsdp_v2(0x1234_5000, 0x1_0000_9000);
        assert_eq!(
            rsdp_sdt(&r),
            Some(SdtPointer {
                phys: 0x1_0000_9000,
                wide: true
            })
        );
    }

    #[test]
    fn a_revision_two_rsdp_with_no_xsdt_falls_back_to_its_rsdt() {
        let r = rsdp_v2(0x1234_5000, 0);
        assert_eq!(
            rsdp_sdt(&r),
            Some(SdtPointer {
                phys: 0x1234_5000,
                wide: false
            })
        );
    }

    #[test]
    fn an_rsdp_whose_checksum_does_not_hold_is_refused() {
        // The check that was missing. What follows an accepted RSDP is a list
        // of physical addresses this kernel dereferences at boot, so a stale
        // or half-overwritten copy of the signature string is not something to
        // take on faith.
        let mut r = rsdp_v1(0x1234_5000);
        r[10] = r[10].wrapping_add(1);
        assert_eq!(rsdp_sdt(&r), None);
    }

    #[test]
    fn a_revision_two_rsdp_whose_extended_checksum_fails_still_has_an_rsdt() {
        // The spec asks for both sums. Failing the second one says the 64-bit
        // half is not to be trusted -- not that the 32-bit half, which passed
        // its own checksum, is gone.
        let mut r = rsdp_v2(0x1234_5000, 0x1_0000_9000);
        r[30] = r[30].wrapping_add(1);
        assert_eq!(
            rsdp_sdt(&r),
            Some(SdtPointer {
                phys: 0x1234_5000,
                wide: false
            })
        );
    }

    #[test]
    fn something_that_is_not_an_rsdp_at_all_is_refused() {
        assert_eq!(rsdp_sdt(b"RSD PTR"), None);
        assert_eq!(rsdp_sdt(&[0u8; 20]), None);
        assert_eq!(rsdp_sdt(&[]), None);
        let mut r = rsdp_v1(0x1000);
        r[3] = b'X';
        assert_eq!(rsdp_sdt(&r), None);
    }

    #[test]
    fn an_rsdp_that_names_nothing_is_refused() {
        assert_eq!(rsdp_sdt(&rsdp_v1(0)), None);
    }

    #[test]
    fn a_revision_two_rsdp_whose_length_stops_short_of_the_xsdt_is_not_wide() {
        let mut r = rsdp_v2(0x1234_5000, 0x1_0000_9000);
        r[rsdp::LENGTH..rsdp::LENGTH + 4].copy_from_slice(&24u32.to_le_bytes());
        seal(&mut r[..RSDP_V1_LEN], 8);
        assert_eq!(
            rsdp_sdt(&r),
            Some(SdtPointer {
                phys: 0x1234_5000,
                wide: false
            })
        );
    }

    #[test]
    fn the_revision_can_be_had_from_the_twenty_bytes_acpi_one_defined() {
        // The point of asking separately: a caller holding only the ACPI 1.0
        // RSDP can find out whether the extended part is worth reading, without
        // dereferencing it to find out.
        let v1 = rsdp_v1(0x1000);
        assert_eq!(v1.len(), RSDP_V1_LEN);
        assert_eq!(rsdp_revision(&v1), Some(0));

        let v2 = rsdp_v2(0x1000, 0x2000);
        assert_eq!(v2.len(), RSDP_V2_LEN);
        assert_eq!(rsdp_revision(&v2[..RSDP_V1_LEN]), Some(2));
    }

    #[test]
    fn the_revision_is_refused_by_whatever_refuses_the_rsdp() {
        let mut bad_sig = rsdp_v1(0x1000);
        bad_sig[0] = b'X';
        assert_eq!(rsdp_revision(&bad_sig), None);

        let mut bad_sum = rsdp_v1(0x1000);
        bad_sum[8] = bad_sum[8].wrapping_add(1);
        assert_eq!(rsdp_revision(&bad_sum), None);

        let short = rsdp_v1(0x1000);
        assert_eq!(rsdp_revision(&short[..RSDP_V1_LEN - 1]), None);
    }

    #[test]
    fn a_wide_rsdp_handed_over_short_falls_back_instead_of_reading_past_it() {
        // What the caller does when the revision says 2: it goes back and reads
        // the other sixteen bytes. If it did not, the walk still has to be
        // sound, and soundness here means the 32-bit RsdtAddress, never a
        // half-read XsdtAddress.
        let v2 = rsdp_v2(0x1000, 0x2000);
        let short = rsdp_sdt(&v2[..RSDP_V1_LEN]).unwrap();
        assert_eq!((short.phys, short.wide), (0x1000, false));
        let whole = rsdp_sdt(&v2).unwrap();
        assert_eq!((whole.phys, whole.wide), (0x2000, true));
    }

    // ── the system description table ────────────────────────────────────────

    #[test]
    fn a_table_declares_how_long_it_is() {
        let t = table(b"XSDT", &[0u8; 16]);
        assert_eq!(table_length(&t), Some(SDT_HEADER_LEN + 16));
        assert_eq!(
            table_length(&t[..SDT_HEADER_LEN]),
            Some(SDT_HEADER_LEN + 16)
        );
    }

    #[test]
    fn a_length_is_not_taken_from_fewer_bytes_than_a_header() {
        // Eight bytes are enough to *read* the length field, which is not the
        // same as being enough to believe it: the caller maps `len` bytes on
        // the strength of the answer, so anything shorter than a header is not
        // a table to take a length from.
        let t = table(b"XSDT", &[0u8; 16]);
        for short in [0, 3, 8, SDT_HEADER_LEN - 1] {
            assert_eq!(table_length(&t[..short]), None, "{short} bytes");
        }
    }

    #[test]
    fn a_length_that_does_not_cover_the_header_is_refused() {
        let mut t = table(b"XSDT", &[]);
        t[4..8].copy_from_slice(&8u32.to_le_bytes());
        assert_eq!(table_length(&t), None);
        t[4..8].copy_from_slice(&0x20_0000u32.to_le_bytes());
        assert_eq!(table_length(&t), None);
    }

    #[test]
    fn a_table_has_to_say_what_it_is_and_add_up() {
        // The `RSDT`/`XSDT` signature was never looked at: whatever the RSDP
        // named was walked as an array of physical addresses.
        let t = table(b"XSDT", &[0u8; 16]);
        assert!(table_is(&t, b"XSDT"));
        assert!(!table_is(&t, b"RSDT"));
        let mut bad = t.clone();
        bad[20] = bad[20].wrapping_add(1);
        assert!(!table_is(&bad, b"XSDT"));
        assert!(!table_is(&t[..SDT_HEADER_LEN - 1], b"XSDT"));
    }

    #[test]
    fn an_xsdt_lists_eight_byte_addresses_and_an_rsdt_four() {
        let mut body = Vec::new();
        for pa in [0x1000u64, 0x2000, 0x3000] {
            body.extend_from_slice(&pa.to_le_bytes());
        }
        let x = table(b"XSDT", &body);
        assert_eq!(
            sdt_entries(&x, true).collect::<Vec<_>>(),
            [0x1000, 0x2000, 0x3000]
        );

        let mut body = Vec::new();
        for pa in [0x1000u32, 0x2000] {
            body.extend_from_slice(&pa.to_le_bytes());
        }
        let r = table(b"RSDT", &body);
        assert_eq!(sdt_entries(&r, false).collect::<Vec<_>>(), [0x1000, 0x2000]);
    }

    #[test]
    fn an_entry_that_names_nothing_is_skipped_rather_than_dereferenced() {
        let mut body = Vec::new();
        for pa in [0x1000u64, 0, 0x3000] {
            body.extend_from_slice(&pa.to_le_bytes());
        }
        let x = table(b"XSDT", &body);
        assert_eq!(sdt_entries(&x, true).collect::<Vec<_>>(), [0x1000, 0x3000]);
    }

    #[test]
    fn a_trailing_half_entry_is_dropped_and_not_read_half_way() {
        let mut body = 0x1000u64.to_le_bytes().to_vec();
        body.extend_from_slice(&[0xAA; 5]);
        let x = table(b"XSDT", &body);
        assert_eq!(sdt_entries(&x, true).collect::<Vec<_>>(), [0x1000]);
    }

    #[test]
    fn a_table_with_no_entries_lists_none() {
        let x = table(b"XSDT", &[]);
        assert_eq!(sdt_entries(&x, true).count(), 0);
        assert_eq!(sdt_entries(&[], true).count(), 0);
    }

    // ── the FADT ────────────────────────────────────────────────────────────

    #[test]
    fn the_legacy_block_names_the_port() {
        let mut f = fadt_bytes(116);
        f[fadt::PM_TMR_LEN] = 4;
        f[fadt::PM_TMR_BLK..fadt::PM_TMR_BLK + 4].copy_from_slice(&0x408u32.to_le_bytes());
        assert_eq!(pm_timer_from_fadt(&f), Some((0x408, false)));
    }

    #[test]
    fn the_extended_block_wins_when_it_names_an_io_port() {
        let mut f = fadt_bytes(244);
        f[fadt::PM_TMR_LEN] = 4;
        f[fadt::PM_TMR_BLK..fadt::PM_TMR_BLK + 4].copy_from_slice(&0x408u32.to_le_bytes());
        f[fadt::X_PM_TMR_BLK] = GAS_SYSTEM_IO;
        let a = fadt::X_PM_TMR_BLK + GAS_ADDRESS;
        f[a..a + 8].copy_from_slice(&0x1808u64.to_le_bytes());
        assert_eq!(pm_timer_from_fadt(&f), Some((0x1808, false)));
    }

    #[test]
    fn the_offsets_are_the_ones_the_spec_gives_and_not_the_ones_the_code_says() {
        // Every other test here names the fields through this module's own
        // constants, so it goes on passing if a constant moves. These are the
        // numbers out of the ACPI spec, written out: `PM_TMR_BLK` at 76,
        // `PM_TMR_LEN` at 91, `Flags` at 112, and `X_PM_TMR_BLK` at 208 with
        // its address eight bytes into the twelve-byte GAS, four in.
        let mut f = fadt_bytes(244);
        f[91] = 4;
        f[76..80].copy_from_slice(&0x408u32.to_le_bytes());
        f[112..116].copy_from_slice(&0x0000_0100u32.to_le_bytes());
        assert_eq!(pm_timer_from_fadt(&f), Some((0x408, true)));
        f[208] = 1;
        f[212..220].copy_from_slice(&0x1808u64.to_le_bytes());
        assert_eq!(pm_timer_from_fadt(&f), Some((0x1808, true)));
    }

    #[test]
    fn a_memory_mapped_timer_is_not_an_io_port() {
        // A Generic Address Structure can describe memory too, and its address
        // read as a port number aims `in`/`out` at sixteen arbitrary bits of
        // the I/O space. Space id 0 is system memory.
        let mut f = fadt_bytes(244);
        f[fadt::PM_TMR_LEN] = 4;
        f[fadt::PM_TMR_BLK..fadt::PM_TMR_BLK + 4].copy_from_slice(&0x408u32.to_le_bytes());
        f[fadt::X_PM_TMR_BLK] = 0;
        let a = fadt::X_PM_TMR_BLK + GAS_ADDRESS;
        f[a..a + 8].copy_from_slice(&0xfed0_0008u64.to_le_bytes());
        assert_eq!(pm_timer_from_fadt(&f), Some((0x408, false)));
        // And a memory address that *would* fit in a port is the case that
        // matters: a wrong space id is then invisible in the answer's shape,
        // and `in`/`out` go to a port nothing owns.
        f[a..a + 8].copy_from_slice(&0x1808u64.to_le_bytes());
        assert_eq!(pm_timer_from_fadt(&f), Some((0x408, false)));
        // Every space id the spec defines that is not system I/O: memory,
        // PCI configuration, embedded controller, SMBus.
        for id in [0u8, 2, 3, 4, 0x0a, 0x7f] {
            f[fadt::X_PM_TMR_BLK] = id;
            assert_eq!(
                pm_timer_from_fadt(&f),
                Some((0x408, false)),
                "space id {}",
                id
            );
        }
    }

    #[test]
    fn an_extended_address_too_wide_for_a_port_falls_back() {
        let mut f = fadt_bytes(244);
        f[fadt::PM_TMR_LEN] = 4;
        f[fadt::PM_TMR_BLK..fadt::PM_TMR_BLK + 4].copy_from_slice(&0x408u32.to_le_bytes());
        f[fadt::X_PM_TMR_BLK] = GAS_SYSTEM_IO;
        let a = fadt::X_PM_TMR_BLK + GAS_ADDRESS;
        f[a..a + 8].copy_from_slice(&0x1_0000u64.to_le_bytes());
        assert_eq!(pm_timer_from_fadt(&f), Some((0x408, false)));
    }

    #[test]
    fn a_block_that_is_not_four_bytes_long_is_not_a_timer() {
        let mut f = fadt_bytes(116);
        f[fadt::PM_TMR_BLK..fadt::PM_TMR_BLK + 4].copy_from_slice(&0x408u32.to_le_bytes());
        for len in [0u8, 1, 2, 3, 5, 8] {
            f[fadt::PM_TMR_LEN] = len;
            assert_eq!(pm_timer_from_fadt(&f), None, "PM_TMR_LEN = {}", len);
        }
    }

    #[test]
    fn the_width_of_the_counter_comes_out_of_the_flags() {
        // Bit 8 of `Flags`, `TMR_VAL_EXT`. Get it wrong and the elapsed count
        // is taken modulo the wrong power of two, which shows up as a
        // frequency rather than as an error.
        let mut f = fadt_bytes(116);
        f[fadt::PM_TMR_LEN] = 4;
        f[fadt::PM_TMR_BLK..fadt::PM_TMR_BLK + 4].copy_from_slice(&0x408u32.to_le_bytes());
        f[fadt::FLAGS..fadt::FLAGS + 4].copy_from_slice(&fadt::TMR_VAL_EXT.to_le_bytes());
        assert_eq!(pm_timer_from_fadt(&f), Some((0x408, true)));
        // ...and no neighbouring bit is that flag.
        for bit in [7u32, 9] {
            f[fadt::FLAGS..fadt::FLAGS + 4].copy_from_slice(&(1u32 << bit).to_le_bytes());
            assert_eq!(pm_timer_from_fadt(&f), Some((0x408, false)), "bit {}", bit);
        }
    }

    #[test]
    fn a_fadt_too_short_to_hold_the_fields_is_refused_rather_than_read_past() {
        assert_eq!(pm_timer_from_fadt(&fadt_bytes(80)), None);
        assert_eq!(pm_timer_from_fadt(&fadt_bytes(SDT_HEADER_LEN)), None);
        assert_eq!(pm_timer_from_fadt(&[]), None);
        // Long enough for the legacy block but not for the extended one: the
        // extended read must not run off the end.
        let mut f = fadt_bytes(116);
        f[fadt::PM_TMR_LEN] = 4;
        f[fadt::PM_TMR_BLK..fadt::PM_TMR_BLK + 4].copy_from_slice(&0x408u32.to_le_bytes());
        assert_eq!(pm_timer_from_fadt(&f), Some((0x408, false)));
    }

    #[test]
    fn a_port_of_zero_is_no_port() {
        let mut f = fadt_bytes(116);
        f[fadt::PM_TMR_LEN] = 4;
        assert_eq!(pm_timer_from_fadt(&f), None);
    }
}
