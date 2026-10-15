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
