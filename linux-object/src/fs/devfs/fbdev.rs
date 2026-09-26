//! Implement INode for framebuffer

use alloc::sync::Arc;
use core::{any::Any, convert::From};

use kernel_hal::drivers::prelude::{ColorFormat, DisplayInfo};
use kernel_hal::drivers::scheme::DisplayScheme;
use kernel_hal::vm::{GenericPageTable, PageTable};
use rcore_fs::vfs::*;
use rcore_fs_devfs::DevFS;
use zircon_object::vm::{page_aligned, pages, VmObject, PAGE_SIZE};

use crate::error::{LxError, LxResult};

// IOCTLs
const FBIOGET_VSCREENINFO: u32 = 0x4600;
const FBIOPUT_VSCREENINFO: u32 = 0x4601;
const FBIOGET_FSCREENINFO: u32 = 0x4602;
const FBIOGETCMAP: u32 = 0x4604;
const FBIOPUTCMAP: u32 = 0x4605;
const FBIOPAN_DISPLAY: u32 = 0x4606;
const FBIOBLANK: u32 = 0x4611;

/// no hardware accelerator
const FB_ACCEL_NONE: u32 = 0;

/// Frambuffer type.
#[repr(u32)]
#[allow(dead_code)]
#[derive(Debug, Copy, Clone, Default)]
pub enum FbType {
    /// Packed Pixels
    #[default]
    PackedPixels = 0,
    /// Non interleaved planes
    Planes = 1,
    /// Interleaved planes
    InterleavedPlanes = 2,
    /// Text/attributes
    Text = 3,
    /// EGA/VGA planes
    VgaPlanes = 4,
    /// Type identified by a V4L2 FOURCC
    FourCC = 5,
}

/// Framebuffer visual type.
#[repr(u32)]
#[allow(dead_code)]
#[derive(Debug, Copy, Clone, Default)]
pub enum FbVisual {
    /// Monochr. 1=Black 0=White
    #[default]
    Mono01 = 0,
    /// Monochr. 1=White 0=Black
    Mono10 = 1,
    /// True color
    TrueColor = 2,
    /// Pseudo color (like atari)
    PseudoColor = 3,
    /// Direct color
    DirectColor = 4,
    /// Pseudo color readonly
    StaticPseudoColor = 5,
    /// Visual identified by a V4L2 FOURCC
    FourCC = 6,
}

/// Fixed screen info, defines the properties of a card that are created when a
/// mode is set and can’t be changed otherwise.
#[repr(C)]
#[derive(Debug, Default)]
pub struct FbFixScreeninfo {
    /// identification string eg "TT Builtin"
    id: [u8; 16],
    /// Start of frame buffer mem (physical address)
    smem_start: u64,
    /// Length of frame buffer mem
    smem_len: u32,
    /// see [`FbType`]
    fb_type: FbType,
    /// Interleave for interleaved Planes
    type_aux: u32,
    /// see [`FbVisual`]
    visual: FbVisual,
    /// zero if no hardware panning
    xpanstep: u16,
    /// zero if no hardware panning
    ypanstep: u16,
    /// zero if no hardware ywrap
    ywrapstep: u16,
    /// length of a line in bytes
    line_length: u32,
    /// Start of Memory Mapped I/O (physical address)
    mmio_start: u64,
    /// Length of Memory Mapped I/O
    mmio_len: u32,
    /// Indicate to driver which specific chip/card we have
    accel: u32,
    /// see FB_CAP_*
    capabilities: u16,
    /// Reserved for future compatibility
    _reserved: [u16; 2],
}

/// Physical address of a framebuffer whose base need not start on a page
/// boundary.
///
/// `PageTable::query` takes a *page*: libos `debug_assert!`s the alignment and
/// the bare-metal walk only ever looks at the page indices. So handing it a
/// vaddr that carries an offset either kills a debug kernel from inside an
/// ioctl any process can issue on `/dev/fb0`, or answers with the containing
/// page's base -- a `smem_start` short by up to `PAGE_SIZE - 1` bytes, which
/// is up to 1023 pixels of shift at 32 bpp for every client that `mmap`s the
/// framebuffer through [`FbDev::get_vmo`]. Query the page, then put the offset
/// back.
fn fb_phys_addr(vaddr: usize) -> Option<u64> {
    fb_phys_addr_with(vaddr, |page| {
        PageTable::from_current()
            .query(page)
            .ok()
            .map(|(paddr, _, _)| paddr as u64)
    })
}

/// [`fb_phys_addr`] with the page-table lookup handed in, so a test can check
/// both halves of the contract without a page table: that what gets looked up
/// is a page, and that the offset within it survives the round trip.
fn fb_phys_addr_with(vaddr: usize, query: impl Fn(usize) -> Option<u64>) -> Option<u64> {
    let offset = vaddr % PAGE_SIZE;
    query(vaddr - offset).map(|page_paddr| page_paddr + offset as u64)
}

/// The driver name as the 16-byte, NUL-padded `fb_fix_screeninfo.id`.
///
/// Truncated rather than refused: the field is a fixed 16 bytes in the uAPI and
/// a long name is a cosmetic problem, where a missing one is a client that
/// cannot tell which device it opened.
fn fb_id_string(name: &str) -> [u8; 16] {
    let mut id = [0u8; 16];
    let bytes = name.as_bytes();
    let n = bytes.len().min(id.len());
    id[..n].copy_from_slice(&bytes[..n]);
    id
}

/// The screen's physical size in millimetres for `FBIOGET_VSCREENINFO`.
///
/// `/dev/fb0` is the GOP framebuffer, so the display that owns it is the one
/// whose EDID the bootloader captured -- the same block the DRM connector
/// answers `mm_width`/`mm_height` from. This used to report a flat zero, which
/// is not "unknown" but "zero millimetres": every DPI-aware client that
/// divides by it gets an infinite DPI, the same way the DRM side used to hand
/// out `600x0` mm. One question, one answer, in both places: the EDID when
/// there is one, else the ~96 DPI estimate, never zero.
fn size_mm(info: DisplayInfo, edid: Option<[u8; 128]>) -> (u32, u32) {
    edid.and_then(|e| zcore_drivers::display::edid::physical_size_mm(&e))
        .unwrap_or_else(|| zcore_drivers::display::edid::estimated_size_mm(info.width, info.height))
}

impl From<DisplayInfo> for FbFixScreeninfo {
    fn from(info: DisplayInfo) -> Self {
        let smem_start = fb_phys_addr(info.fb_base_vaddr).unwrap_or(u64::MAX);
        Self {
            smem_start,
            smem_len: info.fb_size as u32,
            fb_type: FbType::PackedPixels,
            visual: FbVisual::TrueColor,
            // Bytes per scanline: the GOP/BAR1 pitch, not width×bpp. Padding
            // (e.g. 8192 vs 1920×4) is common on NVIDIA; Xorg ShadowFB writes
            // `row * line_length` and a short stride shears the picture into
            // leftover squares and lines.
            line_length: info.pitch(),
            mmio_start: 0,
            mmio_len: 0,
            accel: FB_ACCEL_NONE,
            ..Default::default()
        }
    }
}

/// Interpretation of offset for color fields: All offsets are from the right,
/// inside a "pixel" value, which is exactly 'bits_per_pixel' wide (means: you
/// can use the offset as right argument to <<). A pixel afterwards is a bit
/// stream and is written to video memory as that unmodified.
#[repr(C)]
#[derive(Debug, Default)]
pub struct FbBitfield {
    /// beginning of bitfield
    offset: u32,
    /// length of bitfield
    length: u32,
    /// != 0 : Most significant bit is right
    msb_right: u32,
}

/// Variable screen info, describe the features of a video card that are user defined.
#[repr(C)]
#[derive(Debug, Default)]
pub struct FbVarScreeninfo {
    /// visible resolution x
    xres: u32,
    /// visible resolution y
    yres: u32,
    /// virtual resolution x
    xres_virtual: u32,
    /// virtual resolution y
    yres_virtual: u32,
    /// offset from virtual to visible x
    xoffset: u32,
    /// offset from virtual to visible y
    yoffset: u32,

    /// guess what
    bits_per_pixel: u32,
    /// 0 = color, 1 = grayscale, >1 = FOURCC
    grayscale: u32,
    /// red channel. bitfield in fb mem if true color, else only length is significant.
    red: FbBitfield,
    /// green channel
    green: FbBitfield,
    /// blue channel
    blue: FbBitfield,
    /// transparency
    transp: FbBitfield,

    /// != 0 Non standard pixel format
    nonstd: u32,

    /// see FB_ACTIVATE_*
    activate: u32,

    /// height of picture in mm
    height: u32,
    /// width of picture in mm
    width: u32,
    /// (OBSOLETE) see fb_info.flags
    accel_flags: u32,

    /* Timing: All values in pixclocks, except pixclock (of course) */
    /// pixel clock in ps (pico seconds)
    pixclock: u32,
    /// time from sync to picture
    left_margin: u32,
    /// time from picture to sync
    right_margin: u32,
    /// time from sync to picture
    upper_margin: u32,
    lower_margin: u32,
    /// length of horizontal sync
    hsync_len: u32,
    /// length of vertical sync
    vsync_len: u32,
    /// see FB_SYNC_*
    sync: u32,
    /// see FB_VMODE_*
    vmode: u32,
    /// angle we rotate counter clockwise
    rotate: u32,
    /// colorspace for FOURCC-based modes
    colorspace: u32,
    /// Reserved for future compatibility
    _reserved: [u32; 4],
}

impl From<DisplayInfo> for FbVarScreeninfo {
    fn from(info: DisplayInfo) -> Self {
        let (width_mm, height_mm) = size_mm(
            info,
            zcore_drivers::display::boot_edid()
                .and_then(|(block, len)| (len >= 128).then_some(block)),
        );
        let (rl, gl, bl, al, ro, go, bo, ao) = match info.format {
            ColorFormat::RGB332 => (3, 3, 2, 0, 5, 3, 0, 0),
            ColorFormat::RGB565 => (5, 6, 5, 0, 11, 5, 0, 0),
            ColorFormat::RGB888 => (8, 8, 8, 0, 16, 8, 0, 0),
            ColorFormat::ARGB8888 => (8, 8, 8, 8, 16, 8, 0, 24),
        };
        Self {
            xres: info.width,
            yres: info.height,
            xres_virtual: info.width,
            yres_virtual: info.height,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: info.format.depth() as u32,
            // Millimetres, not zero -- see `size_mm`.
            width: width_mm,
            height: height_mm,
            blue: FbBitfield {
                offset: bo,
                length: bl,
                msb_right: 0,
            },
            green: FbBitfield {
                offset: go,
                length: gl,
                msb_right: 0,
            },
            red: FbBitfield {
                offset: ro,
                length: rl,
                msb_right: 0,
            },
            transp: FbBitfield {
                offset: ao,
                length: al,
                msb_right: 0,
            },
            ..Default::default()
        }
    }
}

/// Framebuffer device
pub struct FbDev {
    display: Arc<dyn DisplayScheme>,
    inode_id: usize,
}

impl FbDev {
    pub fn new(display: Arc<dyn DisplayScheme>) -> Self {
        Self {
            display,
            inode_id: DevFS::new_inode_id(),
        }
    }

    pub fn get_vmo(&self, offset: usize, len: usize) -> LxResult<Arc<VmObject>> {
        let info = self.display.info();
        if !page_aligned(offset) || offset >= info.fb_size {
            return Err(LxError::EINVAL);
        }
        let Some(paddr) = fb_phys_addr(info.fb_base_vaddr) else {
            return Err(LxError::ENOMEM);
        };
        let len = len.min(info.fb_size - offset);
        let vmo = VmObject::new_physical(paddr as usize + offset, pages(len));
        // The framebuffer is a device aperture (GOP / BAR): write-combining
        // like Linux's fbdev `fb_pgprotect`, not the physical VMO's default
        // Uncached, which made every Xorg `fbdev` store a serialized UC write.
        // (Falls back to UC on a core whose PAT has no WC entry.)
        let _ = vmo.set_cache_policy(kernel_hal::CachePolicy::WriteCombining);
        Ok(vmo)
    }
}

impl INode for FbDev {
    #[allow(unsafe_code)]
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        info!(
            "fbdev read_at: offset={:#x} buf_len={:#x}",
            offset,
            buf.len()
        );

        let info = self.display.info();
        if offset >= info.fb_size {
            return Ok(0);
        }
        let len = buf.len().min(info.fb_size - offset);
        let fb = self.display.fb();
        buf[..len].copy_from_slice(&fb[offset..offset + len]);
        Ok(len)
    }

    #[allow(unsafe_code)]
    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        info!(
            "fbdev write_at: offset={:#x} buf_len={:#x}",
            offset,
            buf.len()
        );

        let info = self.display.info();
        if offset >= info.fb_size {
            return Ok(0);
        }
        let len = buf.len().min(info.fb_size - offset);
        let mut fb = self.display.fb();
        fb[offset..offset + len].copy_from_slice(&buf[..len]);
        Ok(len)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            // A framebuffer is always ready both ways: it is an aperture, so
            // there is nothing to wait for and nothing to fill up. Linux's
            // fbdev has no `.poll` at all, which is how a char device ends up
            // reported ready for read AND write; answering `write: false` here
            // parks any client that polls before drawing -- and drawing is the
            // only reason to open this device.
            read: true,
            write: true,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 1,
            inode: self.inode_id,
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::CharDevice,
            mode: 0o660,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: make_rdev(0x1d, 0),
        })
    }

    #[allow(unsafe_code)]
    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        // The FBIO* numbers are old-style ioctls with no size encoded, so the
        // `access_ok()` check is per arm: the kernel writes a whole screeninfo
        // struct through `data`, and a NULL or kernel address must be EFAULT,
        // not a #PF or a write into kernel memory.
        fn ucheck<T>(addr: usize) -> Result<()> {
            if kernel_hal::user::user_range_ok(addr, core::mem::size_of::<T>()) {
                Ok(())
            } else {
                Err(FsError::BadAddress)
            }
        }
        match cmd {
            FBIOGET_FSCREENINFO => {
                ucheck::<FbFixScreeninfo>(data)?;
                let dst = unsafe { &mut *(data as *mut FbFixScreeninfo) };
                *dst = self.display.info().into();
                // Every fbdev driver names itself here ("uefi-gop", "nvidia"),
                // and it is what `fbset -i` and the Xorg log print to say which
                // device came up. An all-zero id says nothing came up at all.
                dst.id = fb_id_string(self.display.name());
                Ok(0)
            }
            FBIOGET_VSCREENINFO => {
                ucheck::<FbVarScreeninfo>(data)?;
                let dst = unsafe { &mut *(data as *mut FbVarScreeninfo) };
                *dst = self.display.info().into();
                Ok(0)
            }
            // The display runs at a single fixed mode, so we cannot honour an
            // arbitrary mode change. Accept the request and report back the
            // actual geometry, which is what X's fbdev driver needs to start.
            FBIOPUT_VSCREENINFO => {
                ucheck::<FbVarScreeninfo>(data)?;
                let dst = unsafe { &mut *(data as *mut FbVarScreeninfo) };
                *dst = self.display.info().into();
                Ok(0)
            }
            // Single, statically mapped framebuffer: no panning or blanking to
            // do, but X issues these during setup — accept them as no-ops.
            FBIOPAN_DISPLAY | FBIOBLANK => Ok(0),
            // TrueColor framebuffer: there is no hardware palette to program,
            // but X's fbdev driver still loads a colormap during setup. Accept
            // it so the screen comes up instead of failing the mode set.
            FBIOGETCMAP | FBIOPUTCMAP => Ok(0),
            _ => {
                warn!("use never support ioctl !");
                Err(FsError::NotSupported)
            }
        }
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// `/dev/fb0` end to end, with a framebuffer on the heap in place of a GOP
/// aperture.
///
/// This is the other way a client's pixels reach the screen: the DRM path has
/// `kms_emu`, but everything that goes through `/dev/fb0` -- the text console,
/// Xorg's `fbdev` driver, anything that opens the framebuffer directly -- comes
/// through this file, and it had no test at all. Nothing here needs a GPU: the
/// two screeninfo structs are pure functions of [`DisplayInfo`], and the ioctls
/// write through a user pointer that a host test can point at its own stack,
/// because `user_range_ok` accepts an ordinary buffer off bare metal.
#[cfg(test)]
mod fbdev_tests {
    extern crate std;

    use super::*;
    use alloc::{vec, vec::Vec};
    use kernel_hal::drivers::prelude::FrameBuffer;
    use kernel_hal::drivers::scheme::Scheme;
    use spin::Mutex;

    /// A display over a heap buffer. `info()` re-reads the buffer's address
    /// every time, the way the real backends read a live aperture base.
    struct Fake {
        name: &'static str,
        mem: Mutex<Vec<u8>>,
        info: DisplayInfo,
    }

    impl Scheme for Fake {
        fn name(&self) -> &str {
            self.name
        }
    }

    impl DisplayScheme for Fake {
        fn info(&self) -> DisplayInfo {
            let mut info = self.info;
            info.fb_base_vaddr = self.mem.lock().as_ptr() as usize;
            info
        }
        fn fb(&self) -> FrameBuffer<'_> {
            // SAFETY: the `Vec` is owned by `self` and outlives the view; a
            // single-threaded test never aliases it. Same shape as the real
            // backends, which hand out a raw aperture pointer.
            let mut m = self.mem.lock();
            unsafe { FrameBuffer::from_raw_parts_mut(m.as_mut_ptr(), m.len()) }
        }
    }

    /// A 1920x1080 ARGB display with `pitch` bytes per row (0 = "not stated").
    fn display(pitch: u32) -> Fake {
        let stride = if pitch != 0 { pitch } else { 1920 * 4 };
        let size = stride as usize * 1080;
        Fake {
            name: "uefi-gop",
            mem: Mutex::new(vec![0u8; size]),
            info: DisplayInfo {
                width: 1920,
                height: 1080,
                pitch,
                format: ColorFormat::ARGB8888,
                fb_base_vaddr: 0,
                fb_size: size,
            },
        }
    }

    fn info_of(pitch: u32) -> DisplayInfo {
        display(pitch).info()
    }

    // ---- the physical address handed to userspace ----

    #[test]
    fn the_looked_up_address_is_always_a_page_and_the_offset_comes_back() {
        // `PageTable::query` takes a page: libos `debug_assert!`s it and the
        // bare-metal walk only reads the page indices. So the lookup below
        // asserts what the real one assumes, and the arithmetic has to put the
        // in-page offset back afterwards or `smem_start` is short by it.
        const PAGE_PADDR: u64 = 0xF000_0000;
        let query = |page: usize| {
            assert!(
                page_aligned(page),
                "the page table was asked about {:#x}, which is not a page",
                page
            );
            Some(PAGE_PADDR)
        };
        // A framebuffer that starts 0x123 bytes into its page -- at 32 bpp that
        // is 72 pixels and change, which is what a client would be shifted by.
        let base = 0xFFFF_8000_0001_0000usize + 0x123;
        assert_eq!(
            fb_phys_addr_with(base, query),
            Some(PAGE_PADDR + 0x123),
            "the offset within the page has to survive the lookup"
        );
    }

    #[test]
    fn a_page_aligned_framebuffer_reports_the_page_itself() {
        let query = |page: usize| Some(0xF000_0000u64 + page as u64 % 4096);
        assert_eq!(
            fb_phys_addr_with(0xFFFF_8000_0001_0000usize, query),
            Some(0xF000_0000)
        );
    }

    #[test]
    fn a_framebuffer_that_is_not_mapped_has_no_address() {
        assert_eq!(fb_phys_addr_with(0x1234_5000, |_| None), None);
        // And the fixed info says so with the sentinel the callers check,
        // rather than a plausible-looking zero.
        let fix = FbFixScreeninfo::from(info_of(0));
        assert_eq!(fix.smem_start, u64::MAX);
    }

    // ---- the stride ----

    #[test]
    fn the_scanline_length_is_the_pitch_the_hardware_states() {
        // 8192 bytes for a 1920-wide mode is the padding NVIDIA's GOP reports;
        // answering width*4 (7680) here shears the picture into leftover
        // squares, which is the whole reason this field reads `info.pitch()`.
        assert_eq!(FbFixScreeninfo::from(info_of(8192)).line_length, 8192);
    }

    #[test]
    fn a_display_that_states_no_pitch_gets_width_times_bytes_per_pixel() {
        assert_eq!(FbFixScreeninfo::from(info_of(0)).line_length, 1920 * 4);
    }

    // ---- the physical size ----

    /// A valid EDID block: header, a detailed timing in slot `0`, and the
    /// checksum that makes the whole thing sum to zero.
    fn edid_block(f: impl FnOnce(&mut [u8; 128])) -> [u8; 128] {
        let mut b = [0u8; 128];
        b[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        f(&mut b);
        let sum = b[..127].iter().fold(0u8, |a, x| a.wrapping_add(*x));
        b[127] = 0u8.wrapping_sub(sum);
        b
    }

    /// Put a detailed timing stating `w_mm` x `h_mm` in descriptor slot 0.
    fn timing_with_size(b: &mut [u8; 128], w_mm: u32, h_mm: u32) {
        let d = 54;
        b[d] = 0x01; // non-zero pixel clock => a detailed timing
        b[d + 12] = (w_mm & 0xFF) as u8;
        b[d + 13] = (h_mm & 0xFF) as u8;
        b[d + 14] = (((w_mm >> 8) << 4) | (h_mm >> 8)) as u8;
    }

    #[test]
    fn the_screen_size_comes_from_the_monitors_own_edid() {
        // The 32" TV from the DRM side's comment: 885x497 mm, which a 96 DPI
        // guess would have called 508x285.
        let block = edid_block(|b| timing_with_size(b, 885, 497));
        assert_eq!(size_mm(info_of(0), Some(block)), (885, 497));
    }

    #[test]
    fn a_screen_with_no_edid_is_estimated_and_never_zero_millimetres() {
        // Zero is not "unknown", it is "zero millimetres", and every client
        // that divides by it gets an infinite DPI.
        let (w, h) = size_mm(info_of(0), None);
        assert!(w > 0 && h > 0, "got {}x{} mm", w, h);
        assert_eq!((w, h), (1920 * 254 / 960, 1080 * 254 / 960));
    }

    #[test]
    fn an_edid_that_states_no_size_falls_back_to_the_estimate() {
        // A block with no detailed timing and zeroed centimetre bytes: valid,
        // and says nothing about the size. The estimate has to take over.
        let block = edid_block(|_| {});
        let (w, h) = size_mm(info_of(0), Some(block));
        assert!(w > 0 && h > 0, "got {}x{} mm", w, h);
    }

    #[test]
    fn the_variable_info_reports_a_size_rather_than_zero() {
        let var = FbVarScreeninfo::from(info_of(8192));
        assert!(
            var.width > 0 && var.height > 0,
            "FBIOGET_VSCREENINFO reported {}x{} mm",
            var.width,
            var.height
        );
    }

    // ---- the pixel layout ----

    #[test]
    fn each_colour_format_states_its_own_channel_layout() {
        let layout = |format: ColorFormat| {
            let mut info = info_of(0);
            info.format = format;
            let v = FbVarScreeninfo::from(info);
            (
                v.bits_per_pixel,
                (v.red.offset, v.red.length),
                (v.green.offset, v.green.length),
                (v.blue.offset, v.blue.length),
                (v.transp.offset, v.transp.length),
            )
        };
        // Offsets are from the right of a `bits_per_pixel`-wide pixel, so each
        // channel has to sit above the ones below it and none may run off the
        // top: that is what an `fbdev` client packs its pixels by.
        assert_eq!(
            layout(ColorFormat::ARGB8888),
            (32, (16, 8), (8, 8), (0, 8), (24, 8))
        );
        assert_eq!(
            layout(ColorFormat::RGB888),
            (24, (16, 8), (8, 8), (0, 8), (0, 0))
        );
        assert_eq!(
            layout(ColorFormat::RGB565),
            (16, (11, 5), (5, 6), (0, 5), (0, 0))
        );
        assert_eq!(
            layout(ColorFormat::RGB332),
            (8, (5, 3), (3, 3), (0, 2), (0, 0))
        );
        for format in [
            ColorFormat::ARGB8888,
            ColorFormat::RGB888,
            ColorFormat::RGB565,
            ColorFormat::RGB332,
        ] {
            let (bpp, r, g, b, a) = layout(format);
            for (offset, length) in [r, g, b, a] {
                assert!(
                    offset + length <= bpp,
                    "{:?}: a channel at {}+{} runs past {} bits",
                    format,
                    offset,
                    length,
                    bpp
                );
            }
        }
    }

    // ---- the device's own identity ----

    #[test]
    fn the_device_names_itself_so_a_client_can_tell_what_came_up() {
        let dev = FbDev::new(Arc::new(display(8192)));
        let mut fix = FbFixScreeninfo::default();
        let ptr = &mut fix as *mut FbFixScreeninfo as usize;
        assert_eq!(dev.io_control(FBIOGET_FSCREENINFO, ptr), Ok(0));
        let id = &fix.id[..fix.id.iter().position(|&c| c == 0).unwrap_or(16)];
        assert_eq!(id, b"uefi-gop");
    }

    #[test]
    fn a_name_longer_than_the_field_is_truncated_not_overflowed() {
        let id = fb_id_string("a-display-with-a-very-long-name");
        assert_eq!(&id[..], b"a-display-with-a");
    }

    // ---- the ioctl surface ----

    #[test]
    fn a_user_pointer_outside_the_user_range_is_a_fault() {
        let dev = FbDev::new(Arc::new(display(0)));
        // NULL is the one every client hits by accident, and the kernel would
        // otherwise write a whole screeninfo struct through it.
        for cmd in [
            FBIOGET_FSCREENINFO,
            FBIOGET_VSCREENINFO,
            FBIOPUT_VSCREENINFO,
        ] {
            assert_eq!(
                dev.io_control(cmd, 0),
                Err(FsError::BadAddress),
                "ioctl {:#x} through a null pointer",
                cmd
            );
        }
    }

    #[test]
    fn setting_a_mode_reports_back_the_one_the_hardware_actually_runs() {
        // The display runs at a single fixed mode, so a mode change cannot be
        // honoured -- but X's fbdev driver will not start on a refusal. It is
        // accepted and answered with the real geometry, which is the contract
        // the arm's comment states.
        let dev = FbDev::new(Arc::new(display(8192)));
        let mut var = FbVarScreeninfo {
            xres: 640,
            yres: 480,
            bits_per_pixel: 16,
            ..Default::default()
        };
        let ptr = &mut var as *mut FbVarScreeninfo as usize;
        assert_eq!(dev.io_control(FBIOPUT_VSCREENINFO, ptr), Ok(0));
        assert_eq!((var.xres, var.yres), (1920, 1080));
        assert_eq!(var.bits_per_pixel, 32);
    }

    #[test]
    fn the_no_op_ioctls_x_issues_during_setup_are_accepted() {
        // Panning, blanking and the colormap have nothing to do on a static
        // truecolor aperture, but refusing them fails X's mode set.
        let dev = FbDev::new(Arc::new(display(0)));
        for cmd in [FBIOPAN_DISPLAY, FBIOBLANK, FBIOGETCMAP, FBIOPUTCMAP] {
            assert_eq!(dev.io_control(cmd, 0), Ok(0), "ioctl {:#x}", cmd);
        }
    }

    #[test]
    fn an_ioctl_this_device_does_not_have_is_refused() {
        let dev = FbDev::new(Arc::new(display(0)));
        assert_eq!(dev.io_control(0x4699, 0), Err(FsError::NotSupported));
    }

    #[test]
    fn a_framebuffer_is_ready_to_be_drawn_on() {
        // Drawing is the only reason to open this device, so a client that
        // polls for writability before its first store must not be parked.
        let dev = FbDev::new(Arc::new(display(0)));
        let poll = dev.poll().unwrap();
        assert!(poll.write, "a framebuffer reported as not writable");
        assert!(poll.read);
        assert!(!poll.error && !poll.hangup);
    }

    // ---- reading and writing the aperture ----

    #[test]
    fn a_read_at_the_end_of_the_aperture_is_end_of_file() {
        let d = display(0);
        let size = d.info().fb_size;
        let dev = FbDev::new(Arc::new(d));
        let mut buf = [0u8; 8];
        assert_eq!(dev.read_at(size, &mut buf), Ok(0));
        assert_eq!(dev.read_at(size + 4096, &mut buf), Ok(0));
    }

    #[test]
    fn a_transfer_that_runs_off_the_end_stops_at_the_end() {
        let d = display(0);
        let size = d.info().fb_size;
        let dev = FbDev::new(Arc::new(d));
        // Four bytes left, sixteen asked for: four move, and not one past the
        // aperture -- this is an indexing panic in the kernel if it is wrong.
        let src = [0xABu8; 16];
        assert_eq!(dev.write_at(size - 4, &src), Ok(4));
        let mut buf = [0u8; 16];
        assert_eq!(dev.read_at(size - 4, &mut buf), Ok(4));
        assert_eq!(&buf[..4], &[0xAB; 4]);
        assert_eq!(&buf[4..], &[0u8; 12], "nothing past the end was touched");
    }

    #[test]
    fn what_is_written_to_the_aperture_is_what_is_read_back() {
        let dev = FbDev::new(Arc::new(display(8192)));
        // One whole 32-bit pixel, one row down: 8192 bytes in, not 7680.
        let px = 0x11223344u32.to_ne_bytes();
        assert_eq!(dev.write_at(8192, &px), Ok(4));
        let mut buf = [0u8; 4];
        assert_eq!(dev.read_at(8192, &mut buf), Ok(4));
        assert_eq!(buf, px);
        // And the row above it is untouched.
        let mut above = [0xFFu8; 4];
        assert_eq!(dev.read_at(0, &mut above), Ok(4));
        assert_eq!(above, [0u8; 4]);
    }

    // ---- mmap ----

    #[test]
    fn an_unaligned_or_out_of_range_mapping_offset_is_refused() {
        let d = display(0);
        let size = d.info().fb_size;
        let dev = FbDev::new(Arc::new(d));
        // `VmObject::new_physical` maps whole pages, so a mapping cannot start
        // mid-page, and it cannot start past the aperture at all.
        assert_eq!(dev.get_vmo(0x123, 4096).err(), Some(LxError::EINVAL));
        assert_eq!(dev.get_vmo(size, 4096).err(), Some(LxError::EINVAL));
        assert_eq!(
            dev.get_vmo(size + PAGE_SIZE, 4096).err(),
            Some(LxError::EINVAL)
        );
    }
}
