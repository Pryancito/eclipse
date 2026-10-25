//! UEFI Graphics Output Protocol

use crate::prelude::{DisplayInfo, FrameBuffer};
use crate::scheme::{DisplayScheme, Scheme};

pub struct UefiDisplay {
    info: DisplayInfo,
}

impl UefiDisplay {
    pub fn new(info: DisplayInfo) -> Self {
        Self { info }
    }
}

impl Scheme for UefiDisplay {
    fn name(&self) -> &str {
        "uefi-gop"
    }
}

impl DisplayScheme for UefiDisplay {
    #[inline]
    fn info(&self) -> DisplayInfo {
        self.info
    }

    #[inline]
    fn fb(&self) -> FrameBuffer<'_> {
        unsafe {
            FrameBuffer::from_raw_parts_mut(self.info.fb_base_vaddr as *mut u8, self.info.fb_size)
        }
    }

    /// GOP lives in the console GPU BAR1 (or a PAT-WC identity map of it).
    /// NT stores are the CPU blit fallback when the copy engine is wedged.
    #[inline]
    fn fb_write_combining(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prelude::ColorFormat;

    /// What the bootloader hands over is a mode and a (virtual address,
    /// size) pair: the display reports exactly those, its framebuffer is
    /// that memory whole, a write through it lands there, and it is a BAR,
    /// so write-combining.
    #[test]
    fn it_reports_the_mode_it_was_given_and_writes_into_its_framebuffer() {
        let buf: &'static mut [u8] = alloc::vec![0u8; 64 * 4 * 2].leak();
        let base = buf.as_mut_ptr() as usize;
        let d = UefiDisplay::new(DisplayInfo {
            width: 64,
            height: 2,
            pitch: 64 * 4,
            format: ColorFormat::ARGB8888,
            fb_base_vaddr: base,
            fb_size: 512,
        });
        assert_eq!(d.name(), "uefi-gop");
        let got = d.info();
        assert_eq!(
            (
                got.width,
                got.height,
                got.pitch,
                got.fb_base_vaddr,
                got.fb_size
            ),
            (64, 2, 256, base, 512)
        );
        {
            let mut fb = d.fb();
            assert_eq!(fb.len(), 512);
            fb[511] = 0x5a;
        }
        assert_eq!(unsafe { ((base + 511) as *const u8).read_volatile() }, 0x5a);
        assert!(d.fb_write_combining());
    }
}
