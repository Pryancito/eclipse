//! The EDID the UEFI bootloader captured (`EFI_EDID_ACTIVE_PROTOCOL`):
//! stashed at driver init, read by the DRM connector property, procfs and
//! the physical size. A child of `nvidia` with no logic change: the parent
//! is at its size ceiling (`xtask tamano`); the parent re-exports both
//! functions so `display::{boot_edid, set_boot_edid}` keep their paths.

use lock::Mutex;

/// Raw EDID of the active display captured by the UEFI bootloader
/// (`EFI_EDID_ACTIVE_PROTOCOL`), stashed at driver init. This is the real
/// panel on the GPU that drives the GOP console -- available with no GPU
/// display bring-up at all. `len` is the valid byte count (0 = none).
static BOOT_EDID: Mutex<Option<([u8; 128], u32)>> = Mutex::new(None);

/// Record the boot-time EDID (called from kernel-hal with the bootloader's
/// `GraphicInfo.edid`). A zero length is stored as "no EDID".
pub fn set_boot_edid(edid: &[u8], len: u32) {
    if len == 0 || edid.is_empty() {
        return;
    }
    let mut buf = [0u8; 128];
    let n = (len as usize).min(edid.len()).min(128);
    buf[..n].copy_from_slice(&edid[..n]);
    // Whatever the firmware last read off the DDC line lands here, and an
    // unpowered sink, a flaky line or a GOP that never filled its buffer all
    // produce something that looks like an EDID. Storing it would hand every
    // reader -- the DRM connector property, procfs, the physical size every
    // DPI-aware client scales by -- a panel invented out of line noise, so a
    // block that fails its own header or checksum is dropped here instead.
    if !crate::display::edid::block_valid(&buf[..n]) {
        log::warn!(
            "[edid] firmware handed over {} bytes that are not a valid EDID block; ignoring",
            n
        );
        return;
    }
    *BOOT_EDID.lock() = Some((buf, n as u32));
}

/// The captured UEFI EDID (bytes, valid length), if the firmware exposed one.
pub fn boot_edid() -> Option<([u8; 128], u32)> {
    *BOOT_EDID.lock()
}

/// Tests only: put `edid` in the bootloader's place and hand back what was
/// there, so a test that needs a boot EDID, or none, leaves the next test
/// what it found.
#[cfg(test)]
pub(super) fn swap_boot_edid(edid: Option<([u8; 128], u32)>) -> Option<([u8; 128], u32)> {
    core::mem::replace(&mut *BOOT_EDID.lock(), edid)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A whole block: the EDID header, `fill` in between, and the checksum
    /// byte that makes the 128 sum to zero.
    fn block(fill: u8) -> [u8; 128] {
        let mut b = [fill; 128];
        b[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        let sum = b[..127].iter().fold(0u8, |s, x| s.wrapping_add(*x));
        b[127] = 0u8.wrapping_sub(sum);
        b
    }

    /// Empties the stash for the scope and puts back what it held, so the
    /// readers (the connector property, procfs, the physical size) keep
    /// whatever another test left them.
    struct Stash(Option<([u8; 128], u32)>);
    impl Stash {
        fn empty() -> Self {
            Stash(BOOT_EDID.lock().take())
        }
    }
    impl Drop for Stash {
        fn drop(&mut self) {
            *BOOT_EDID.lock() = self.0;
        }
    }

    #[test]
    fn a_zero_length_or_an_empty_slice_stores_nothing() {
        let _serialised = crate::test_globals::LOCK.lock();
        let _stash = Stash::empty();
        set_boot_edid(&block(0x10), 0);
        assert_eq!(boot_edid(), None, "length 0 is \"no EDID\"");
        set_boot_edid(&[], 128);
        assert_eq!(boot_edid(), None, "and so is an empty slice");
    }

    #[test]
    fn a_block_that_fails_its_header_or_checksum_is_dropped() {
        let _serialised = crate::test_globals::LOCK.lock();
        let _stash = Stash::empty();
        let mut bad_header = block(0x10);
        bad_header[0] = 0xFF;
        set_boot_edid(&bad_header, 128);
        assert_eq!(boot_edid(), None, "line noise is not a panel");
        let mut bad_sum = block(0x10);
        bad_sum[127] ^= 1;
        set_boot_edid(&bad_sum, 128);
        assert_eq!(boot_edid(), None, "nor is a block whose checksum fails");
        // A partial block cannot carry its checksum: the 32-byte head a GOP
        // that filled only that much hands over is dropped too.
        set_boot_edid(&block(0x10)[..32], 32);
        assert_eq!(boot_edid(), None, "a partial block is not a block");
    }

    #[test]
    fn a_valid_block_is_stored_with_its_length_and_the_latest_wins() {
        let _serialised = crate::test_globals::LOCK.lock();
        let _stash = Stash::empty();
        set_boot_edid(&block(0x10), 128);
        assert_eq!(boot_edid(), Some((block(0x10), 128)));
        set_boot_edid(&block(0x20), 128);
        assert_eq!(boot_edid(), Some((block(0x20), 128)), "the latest capture");
    }

    #[test]
    fn the_length_is_clamped_to_the_slice_and_to_one_block() {
        let _serialised = crate::test_globals::LOCK.lock();
        let _stash = Stash::empty();
        // A length past the slice: only what is there is copied, so a
        // short slice with a long length is a partial block, not a panic.
        set_boot_edid(&block(0x30), 1000);
        assert_eq!(boot_edid(), Some((block(0x30), 128)));
        set_boot_edid(&block(0x30)[..64], 200);
        assert_eq!(
            boot_edid(),
            Some((block(0x30), 128)),
            "64 bytes however long it claims: dropped, the capture before it stays"
        );
        // Two blocks with their full length: the first is kept and the
        // stored length says one block, not two.
        let mut two = [0u8; 256];
        two[..128].copy_from_slice(&block(0x40));
        two[128..].copy_from_slice(&block(0x50));
        set_boot_edid(&two, 256);
        assert_eq!(boot_edid(), Some((block(0x40), 128)));
    }
}
