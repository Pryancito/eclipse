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
pub mod esp;
#[cfg(any(feature = "boot-ui", test))]
pub mod fb;
#[cfg(any(feature = "boot-ui", test))]
pub mod frames;
#[cfg(any(feature = "boot-ui", test))]
pub mod logo;
#[cfg(any(feature = "boot-ui", test))]
pub mod page_table;
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
    /// Raw TSC reading at each of rboot's boot progress marks (0..51), `0` for a
    /// mark not reached. The loader half of the boot timeline: the kernel
    /// converts these against a frequency it has checked against the ACPI PM
    /// timer and prints them above its own marks, so one table covers the whole
    /// boot. See `rboot::marks` (in the loader binary) and
    /// `kernel_hal::boot_marks`.
    ///
    /// Raw and not nanoseconds because the only clock the firmware offers rboot
    /// is `BootServices::stall`, and calibrating against it would mean spending
    /// ten or twenty milliseconds of the boot to measure it.
    ///
    /// After `edid`, like everything new here: growing the tail never shifts an
    /// existing field's offset.
    pub loader_marks: [u64; LOADER_MARKS],
    /// What `resolution=` asked for and what the firmware actually did about
    /// it. Here because rboot's own log cannot answer "why did my
    /// `resolution=` do nothing?" on the machine where it happens: the
    /// `warn!` goes to the firmware's text console and the splash wipes the
    /// screen two calls later, and the shipped `cmdline=LOG=error` discards
    /// it before that. The kernel prints this record to the klog instead, so
    /// `dmesg | grep gop` answers it after boot.
    ///
    /// After `loader_marks`, like everything new here: growing the tail never
    /// shifts an existing field's offset.
    pub gop: GopReport,
}

/// One mode the firmware offered, as [`GopReport`] records it.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
#[repr(C)]
pub struct OfferedMode {
    pub width: u16,
    pub height: u16,
    /// Whether the mode has a linear framebuffer (not `BltOnly`).
    pub direct: bool,
}

/// How many offered modes [`GopReport`] keeps. A GOP that lists more says so
/// in `offered_len`; the point of the list is to show whether the requested
/// mode is in it, and 48 covers every firmware seen so far.
pub const GOP_OFFERED_MAX: usize = 48;

/// `resolution=` as the loader parsed it, for [`GopReport::asked_kind`].
pub const GOP_ASKED_KEEP: u8 = 0;
/// `resolution=auto`.
pub const GOP_ASKED_AUTO: u8 = 1;
/// `resolution=WxH`.
pub const GOP_ASKED_EXACT: u8 = 2;

/// The boot-time record of the graphic mode decision: what was asked, what
/// the firmware offered, what was set, and on which GOP.
///
/// Every "my resolution= is ignored" report has three possible causes and
/// this record separates them without a serial cable: the firmware does not
/// enumerate the requested mode (`offered` has no such entry), it enumerates
/// it and refuses to set it (`set_called && !set_ok`), or the mode was set on
/// a GPU that is not the one driving the monitor (`gop_count > 1`, see
/// [`video::pick_gop`]).
#[derive(Debug, Copy, Clone)]
#[repr(C)]
pub struct GopReport {
    /// One of [`GOP_ASKED_KEEP`], [`GOP_ASKED_AUTO`], [`GOP_ASKED_EXACT`].
    pub asked_kind: u8,
    /// Whether `set_mode` was called at all, and whether it returned success.
    pub set_called: bool,
    pub set_ok: bool,
    /// Whether the GOP rboot used has an EDID of its own, i.e. a display is
    /// attached to *that* GPU.
    pub gop_has_edid: bool,
    /// The `WxH` asked for, `(0, 0)` unless `asked_kind` is
    /// [`GOP_ASKED_EXACT`].
    pub asked_width: u32,
    pub asked_height: u32,
    /// The display's EDID-preferred mode, `(0, 0)` when there is no usable
    /// EDID.
    pub edid_width: u32,
    pub edid_height: u32,
    /// The mode the kernel was handed, i.e. what the firmware was in when
    /// rboot left.
    pub final_width: u32,
    pub final_height: u32,
    /// GOP handles the firmware exposed, and which of them rboot used.
    pub gop_count: u32,
    pub gop_index: u32,
    /// Graphics cards actually on offer: drawable handles with distinct
    /// framebuffers. Not `gop_count`, which counts the firmware's own console
    /// GOP over the same framebuffer as a handle of its own -- see
    /// `video::distinct_cards`.
    pub card_count: u32,
    /// Modes that GOP offered, and how many of them are in `offered`.
    pub offered_len: u32,
    pub offered: [OfferedMode; GOP_OFFERED_MAX],
}

impl GopReport {
    /// The record of a boot that never got as far as asking the firmware.
    pub const fn empty() -> Self {
        Self {
            asked_kind: GOP_ASKED_KEEP,
            set_called: false,
            set_ok: false,
            gop_has_edid: false,
            asked_width: 0,
            asked_height: 0,
            edid_width: 0,
            edid_height: 0,
            final_width: 0,
            final_height: 0,
            gop_count: 0,
            gop_index: 0,
            card_count: 0,
            offered_len: 0,
            offered: [OfferedMode {
                width: 0,
                height: 0,
                direct: false,
            }; GOP_OFFERED_MAX],
        }
    }

    /// Whether the firmware enumerated the exact mode `resolution=` asked
    /// for -- the first of the three causes to rule out, and the only one
    /// visible in the mode list alone.
    pub fn asked_mode_was_offered(&self) -> bool {
        self.asked_kind == GOP_ASKED_EXACT
            && self.offered[..(self.offered_len as usize).min(GOP_OFFERED_MAX)]
                .iter()
                .any(|m| {
                    m.direct
                        && m.width as u32 == self.asked_width
                        && m.height as u32 == self.asked_height
                })
    }

    /// Which of the causes explains a `resolution=` that did not take effect,
    /// or `None` when the machine is in the mode that was asked for.
    ///
    /// Reported even when the mode did change, in the multi-card case: on a
    /// two-card machine where no GOP reports a display, the mode may well
    /// have been set on the card the monitor is not plugged into, and from
    /// the loader's side that is indistinguishable from success.
    pub fn why_not(&self) -> Option<&'static str> {
        // The concrete causes first: they name what to change. The multi-card
        // caveat is a maybe, and it used to shadow them -- on plain QEMU,
        // where no GOP exposes an EDID, it answered every question with
        // "several GPUs".
        if self.asked_kind != GOP_ASKED_KEEP && !self.asked_mode_is_live() {
            if self.set_called && !self.set_ok {
                return Some("the firmware listed the mode and then refused to set it");
            }
            if self.asked_kind == GOP_ASKED_EXACT {
                return Some(
                    "the firmware does not enumerate that mode, and rboot only sets a mode it offers",
                );
            }
            return Some(
                "nothing the firmware offers matches the panel's own mode (which may also be above the 4K cap `auto` honours)",
            );
        }
        // The mode is the one that was asked for -- as far as the loader can
        // tell. With two cards and no display reported on the one whose mode
        // was set, that is exactly what "set on the card the monitor is not
        // plugged into" also looks like from here.
        if self.card_count > 1 && !self.gop_has_edid {
            return Some(
                "more than one graphics card and none of them reported an attached display: the mode may have been set on the card the monitor is not plugged into",
            );
        }
        None
    }

    /// Whether the mode the kernel was handed is the one that was asked for.
    pub fn asked_mode_is_live(&self) -> bool {
        match self.asked_kind {
            GOP_ASKED_EXACT => {
                self.final_width == self.asked_width && self.final_height == self.asked_height
            }
            GOP_ASKED_AUTO => self.edid_width == 0 || self.final_width == self.edid_width,
            _ => true,
        }
    }
}

impl Default for GopReport {
    fn default() -> Self {
        Self::empty()
    }
}

/// One slot per percentage rboot can pass to the progress bar, so a mark is
/// recorded by indexing. rboot owns 0..=51 of the bar and the kernel takes over
/// at 52; the array is sized for the whole of rboot's range plus the handoff
/// mark, and a percentage outside it is dropped rather than trusted.
pub const LOADER_MARKS: usize = 52;

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

#[cfg(test)]
mod gop_report_tests {
    use super::*;

    fn exact(w: u32, h: u32) -> GopReport {
        let mut r = GopReport::empty();
        r.asked_kind = GOP_ASKED_EXACT;
        r.asked_width = w;
        r.asked_height = h;
        r.gop_count = 1;
        r.card_count = 1;
        r.gop_has_edid = true;
        r
    }

    fn offer(r: &mut GopReport, modes: &[(u16, u16)]) {
        r.offered_len = modes.len() as u32;
        for (slot, &(w, h)) in r.offered.iter_mut().zip(modes) {
            *slot = OfferedMode {
                width: w,
                height: h,
                direct: true,
            };
        }
    }

    #[test]
    fn a_mode_that_was_asked_for_and_is_live_has_no_complaint() {
        let mut r = exact(1920, 1080);
        offer(&mut r, &[(1024, 768), (1920, 1080)]);
        r.set_called = true;
        r.set_ok = true;
        r.final_width = 1920;
        r.final_height = 1080;
        assert!(r.asked_mode_was_offered());
        assert!(r.asked_mode_is_live());
        assert_eq!(r.why_not(), None);
    }

    #[test]
    fn a_mode_the_firmware_never_offered_says_so() {
        // The common "resolution= does nothing": the GOP list has no such
        // mode, so rboot kept what the firmware had set.
        let mut r = exact(1920, 1080);
        offer(&mut r, &[(1024, 768), (1280, 800)]);
        r.final_width = 1280;
        r.final_height = 800;
        assert!(!r.asked_mode_was_offered());
        assert!(r.why_not().unwrap().contains("does not enumerate"));
    }

    #[test]
    fn a_mode_listed_as_bltonly_only_does_not_count_as_offered() {
        let mut r = exact(1920, 1080);
        r.offered_len = 1;
        r.offered[0] = OfferedMode {
            width: 1920,
            height: 1080,
            direct: false,
        };
        assert!(!r.asked_mode_was_offered());
    }

    #[test]
    fn a_firmware_that_refuses_the_mode_it_listed_says_so() {
        let mut r = exact(1920, 1080);
        offer(&mut r, &[(1920, 1080)]);
        r.set_called = true;
        r.set_ok = false;
        r.final_width = 1280;
        r.final_height = 800;
        assert!(r.why_not().unwrap().contains("refused"));
    }

    #[test]
    fn two_gpus_with_no_display_reported_is_flagged_even_when_the_mode_changed() {
        // The symptom this cannot tell from success: SetMode succeeded on
        // some card, and the monitor is on the other one.
        let mut r = exact(800, 600);
        offer(&mut r, &[(800, 600)]);
        r.set_called = true;
        r.set_ok = true;
        r.final_width = 800;
        r.final_height = 600;
        r.gop_count = 3; // two cards plus the firmware's own console GOP
        r.card_count = 2;
        r.gop_has_edid = false;
        assert!(r.why_not().unwrap().contains("more than one graphics card"));
        // Same machine, but the GOP we used does report a display: nothing to
        // complain about.
        r.gop_has_edid = true;
        assert_eq!(r.why_not(), None);
    }

    #[test]
    fn the_concrete_cause_is_reported_before_the_multi_card_maybe() {
        // The regression this ordering fixes: on plain QEMU every GOP is
        // EDID-less, so the "several cards" maybe answered every question and
        // the real cause (the mode is not in the list) never got printed.
        let mut r = exact(1234, 567);
        offer(&mut r, &[(800, 600)]);
        r.card_count = 2;
        r.gop_has_edid = false;
        r.final_width = 800;
        r.final_height = 600;
        assert!(r.why_not().unwrap().contains("does not enumerate"));
    }

    #[test]
    fn no_resolution_key_is_never_a_complaint() {
        let mut r = GopReport::empty();
        r.gop_count = 1;
        r.card_count = 1;
        r.final_width = 1280;
        r.final_height = 800;
        assert_eq!(r.asked_kind, GOP_ASKED_KEEP);
        assert_eq!(r.why_not(), None);
    }

    #[test]
    fn auto_that_did_not_land_on_the_panels_own_mode_says_so() {
        let mut r = GopReport::empty();
        r.asked_kind = GOP_ASKED_AUTO;
        r.gop_count = 1;
        r.card_count = 1;
        r.gop_has_edid = true;
        r.edid_width = 2560;
        r.edid_height = 1440;
        offer(&mut r, &[(1280, 800)]);
        r.set_called = true;
        r.set_ok = true;
        r.final_width = 1280;
        r.final_height = 800;
        assert!(r.why_not().unwrap().contains("nothing the firmware offers"));
        // And when it did land there, no complaint.
        r.final_width = 2560;
        r.final_height = 1440;
        assert_eq!(r.why_not(), None);
    }

    #[test]
    fn an_offered_list_longer_than_the_record_is_not_read_past_its_end() {
        let mut r = exact(640, 480);
        r.offered_len = 999;
        r.offered[0] = OfferedMode {
            width: 640,
            height: 480,
            direct: true,
        };
        assert!(r.asked_mode_was_offered());
    }
}
