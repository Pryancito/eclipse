//! What the boot knows of the display, for a driver of the GPU that is not
//! the kernel's: the UEFI framebuffer, the monitor's EDID the firmware read
//! and the `resolution=WxH` asked for. The kernel's NVIDIA driver is told
//! these at boot ([`set_boot_fb_info`], [`set_boot_edid`],
//! [`set_requested_mode`]); the same driver in a process (`udrv-nvidia`)
//! asks the kernel for them (`ECLIPSE_BOOT_DISPLAY`) and is told the same.
//! Without them the GPU that scans out the console would not know it does
//! (`drives_boot_display`), and would be brought up as if it did not.
//!
//! On the wire, [`LEN`] bytes, little-endian: the framebuffer's physical
//! address (0 for none), width, height and pitch; the requested width and
//! height (0 for none); the EDID's length (0 for none) and its 128 bytes.

use core::convert::TryInto;

use super::{boot_edid, boot_fb_info, requested_mode};
use super::{set_boot_edid, set_boot_fb_info, set_requested_mode};

/// The bytes of one [`BootDisplay`].
pub const LEN: usize = 32 + 128;

/// What the boot knows of the display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BootDisplay {
    /// The UEFI framebuffer: `(phys, width, height, pitch)`.
    pub fb: Option<(u64, u32, u32, u32)>,
    /// The `resolution=WxH` asked for.
    pub requested: Option<(u32, u32)>,
    /// The monitor's EDID block, and how many of its bytes are.
    pub edid: Option<([u8; 128], u32)>,
}

impl BootDisplay {
    /// What this side was told at boot.
    pub fn this_boot() -> BootDisplay {
        BootDisplay {
            fb: boot_fb_info(),
            requested: requested_mode(),
            edid: boot_edid(),
        }
    }

    /// Tell this side's driver, as the kernel's is told at boot.
    pub fn install(&self) {
        if let Some((phys, width, height, pitch)) = self.fb {
            set_boot_fb_info(phys, width, height, pitch);
        }
        let (w, h) = self.requested.unwrap_or((0, 0));
        set_requested_mode(w, h);
        if let Some((edid, len)) = self.edid {
            set_boot_edid(&edid, len);
        }
    }

    pub fn encode(&self) -> [u8; LEN] {
        let mut out = [0u8; LEN];
        let (phys, width, height, pitch) = self.fb.unwrap_or((0, 0, 0, 0));
        let (rw, rh) = self.requested.unwrap_or((0, 0));
        let (edid, edid_len) = self.edid.unwrap_or(([0; 128], 0));
        out[..8].copy_from_slice(&phys.to_le_bytes());
        for (at, v) in [width, height, pitch, rw, rh, edid_len.min(128)]
            .iter()
            .copied()
            .enumerate()
        {
            out[8 + 4 * at..12 + 4 * at].copy_from_slice(&v.to_le_bytes());
        }
        out[32..].copy_from_slice(&edid);
        out
    }

    pub fn decode(bytes: &[u8; LEN]) -> BootDisplay {
        let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let phys = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        let (rw, rh) = (word(20), word(24));
        let edid_len = word(28).min(128);
        let mut edid = [0u8; 128];
        edid.copy_from_slice(&bytes[32..]);
        BootDisplay {
            fb: (phys != 0).then(|| (phys, word(8), word(12), word(16))),
            requested: (rw != 0 && rh != 0).then_some((rw, rh)),
            edid: (edid_len != 0).then_some((edid, edid_len)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_the_boot_knows_goes_whole() {
        let mut edid = [0u8; 128];
        edid[..8].copy_from_slice(&[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
        edid[127] = 0x5a;
        let all = BootDisplay {
            fb: Some((0xe000_0000, 2560, 1440, 10240)),
            requested: Some((1920, 1080)),
            edid: Some((edid, 128)),
        };
        assert_eq!(BootDisplay::decode(&all.encode()), all);
        let none = BootDisplay::default();
        assert_eq!(BootDisplay::decode(&none.encode()), none);
        assert_eq!(none.encode(), [0; LEN]);
    }

    #[test]
    fn a_length_past_the_block_is_the_block() {
        let mut bytes = BootDisplay::default().encode();
        bytes[28..32].copy_from_slice(&999u32.to_le_bytes());
        assert_eq!(BootDisplay::decode(&bytes).edid.map(|e| e.1), Some(128));
    }
}
