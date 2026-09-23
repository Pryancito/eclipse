#![cfg_attr(not(test), no_std)]
#![deny(warnings)]

extern crate alloc;

use alloc::vec::Vec;
pub use uefi::proto::console::gop::ModeInfo;
pub use uefi::table::boot::{MemoryAttribute, MemoryDescriptor, MemoryType};

#[cfg(any(feature = "boot-ui", test))]
pub mod cmdline;
#[cfg(any(feature = "boot-ui", test))]
pub mod config;
#[cfg(any(feature = "boot-ui", test))]
pub mod fb;
#[cfg(any(feature = "boot-ui", test))]
pub mod logo;
#[cfg(any(feature = "boot-ui", test))]
pub mod progress;
#[cfg(any(feature = "boot-ui", test))]
pub mod video;

/// This structure represents the information that the bootloader passes to the kernel.
#[repr(C)]
#[derive(Debug)]
pub struct BootInfo {
    /// Referencias al buffer del mapa de memoria (vida `'static` vía `Box::leak` en main).
    pub memory_map: Vec<&'static MemoryDescriptor>,
    /// The offset into the virtual address space where the physical memory is mapped.
    pub physical_memory_offset: u64,
    /// The graphic output information
    pub graphic_info: GraphicInfo,
    /// Physical address of ACPI2 RSDP
    pub acpi2_rsdp_addr: u64,
    /// Physical address of SMBIOS
    pub smbios_addr: u64,
    /// The start physical address of initramfs
    pub initramfs_addr: u64,
    /// The size of initramfs
    pub initramfs_size: u64,
    /// Kernel command line
    pub cmdline: &'static str,
    /// Raw EDID (first 128-byte block) of the active display, read from the
    /// UEFI `EFI_EDID_ACTIVE_PROTOCOL` at boot. `edid_size` is 0 when the
    /// firmware exposed no EDID. Kept LAST so growing it never shifts any
    /// existing field's offset (ABI-stable across partial rebuilds).
    pub edid: [u8; 128],
    pub edid_size: u32,
}

/// Graphic output information
#[derive(Debug, Copy, Clone)]
#[repr(C)]
pub struct GraphicInfo {
    pub mode: ModeInfo,
    pub fb_addr: u64,
    pub fb_size: u64,
}

/// Stand-ins for the firmware, so the drawing and mode-selection code can be
/// checked on a host with no GOP.
#[cfg(test)]
pub(crate) mod testing {
    use alloc::vec;
    use alloc::vec::Vec;
    use uefi::proto::console::gop::{ModeInfo, PixelFormat};
    use uefi_raw::protocol::console::{
        GraphicsOutputModeInformation, GraphicsPixelFormat, PixelBitmask,
    };

    /// Exclusive use of the process-wide orientation flags.
    ///
    /// Re-entrant on purpose: a test that compares two renderings holds one
    /// canvas while it builds the next, and a plain `Mutex` would deadlock on
    /// the second. Nesting is per thread, so the exclusion still holds.
    pub struct ScreenGuard;

    static SCREEN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    std::thread_local! {
        static DEPTH: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
        static HELD: core::cell::RefCell<Option<std::sync::MutexGuard<'static, ()>>> =
            const { core::cell::RefCell::new(None) };
    }

    impl ScreenGuard {
        pub fn acquire() -> Self {
            DEPTH.with(|d| {
                if d.get() == 0 {
                    // A test that asserted and panicked poisons the mutex; the
                    // next test still needs the flags, not a second failure.
                    let g = SCREEN.lock().unwrap_or_else(|e| e.into_inner());
                    HELD.with(|h| *h.borrow_mut() = Some(g));
                }
                d.set(d.get() + 1);
            });
            ScreenGuard
        }
    }

    impl Drop for ScreenGuard {
        fn drop(&mut self) {
            DEPTH.with(|d| {
                let n = d.get() - 1;
                d.set(n);
                if n == 0 {
                    HELD.with(|h| *h.borrow_mut() = None);
                }
            });
        }
    }

    /// A `ModeInfo` describing a mode no firmware has to offer.
    pub fn fake_mode_info(sw: usize, sh: usize, stride: usize, fmt: PixelFormat) -> ModeInfo {
        let raw = GraphicsOutputModeInformation {
            version: 0,
            horizontal_resolution: sw as u32,
            vertical_resolution: sh as u32,
            pixel_format: GraphicsPixelFormat(fmt as u32),
            pixel_information: PixelBitmask {
                red: 0x00FF_0000,
                green: 0x0000_FF00,
                blue: 0x0000_00FF,
                reserved: 0xFF00_0000,
            },
            pixels_per_scan_line: stride as u32,
        };
        // SAFETY: `ModeInfo` is `#[repr(transparent)]` over exactly this type.
        unsafe { core::mem::transmute(raw) }
    }

    /// Ordinary memory standing in for the GOP framebuffer, with a tail the
    /// drawing must never touch.
    ///
    /// Holding a [`ScreenGuard`] for its whole life is what makes these tests
    /// safe to run in parallel: the orientation flags are process-wide, and
    /// CI's `--test-threads=1` would never catch the interference.
    pub struct Canvas {
        px: Vec<u32>,
        stride: usize,
        sw: usize,
        sh: usize,
        _guard: ScreenGuard,
    }

    impl Canvas {
        pub const UNTOUCHED: u32 = 0xDEAD_BEEF;
        const SLACK: usize = 4096;

        pub fn new(sw: usize, sh: usize, stride: usize) -> Self {
            let guard = ScreenGuard::acquire();
            crate::fb::set_rot180(false);
            crate::fb::set_mirror_x(false);
            Canvas {
                px: vec![Self::UNTOUCHED; stride * sh + Self::SLACK],
                stride,
                sw,
                sh,
                _guard: guard,
            }
        }

        pub fn addr(&mut self) -> u64 {
            self.px.as_mut_ptr() as u64
        }

        pub fn get(&self, x: usize, y: usize) -> u32 {
            self.px[y * self.stride + x]
        }

        pub fn raw(&self, i: usize) -> u32 {
            self.px[i]
        }

        pub fn pixels(&self) -> &[u32] {
            &self.px
        }

        /// How many of the `sw * sh` visible pixels were never stored to.
        pub fn untouched_visible(&self) -> usize {
            (0..self.sh)
                .flat_map(|y| (0..self.sw).map(move |x| (x, y)))
                .filter(|&(x, y)| self.get(x, y) == Self::UNTOUCHED)
                .count()
        }

        pub fn count(&self, color: u32) -> usize {
            (0..self.sh)
                .flat_map(|y| (0..self.sw).map(move |x| (x, y)))
                .filter(|&(x, y)| self.get(x, y) == color)
                .count()
        }

        /// Nothing was stored past the last scanline.
        pub fn assert_no_overrun(&self) {
            let end = self.stride * self.sh;
            assert!(
                self.px[end..].iter().all(|&p| p == Self::UNTOUCHED),
                "wrote past the end of the framebuffer"
            );
        }
    }
}
