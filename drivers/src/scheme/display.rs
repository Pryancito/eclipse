use super::Scheme;
use crate::DeviceResult;
use core::sync::atomic::{AtomicBool, Ordering};

/// When set, GOP scanout is stored left-right reversed. Opt-in (`FB_MIRROR_X`);
/// default is left-to-right. Do not use this to compensate for mirrored glyphs.
static SCANOUT_MIRROR_X: AtomicBool = AtomicBool::new(false);

/// Flip GOP writes on X. Call once at boot if the firmware presents a mirror.
pub fn set_scanout_mirror_x(on: bool) {
    SCANOUT_MIRROR_X.store(on, Ordering::SeqCst);
}

#[inline]
fn scanout_mirror_x() -> bool {
    SCANOUT_MIRROR_X.load(Ordering::Relaxed)
}

/// Map a logical X coordinate onto the scanout when [`set_scanout_mirror_x`]
/// is active.
#[inline]
fn map_x(x: u32, width: u32) -> u32 {
    if scanout_mirror_x() {
        width.saturating_sub(1).saturating_sub(x)
    } else {
        x
    }
}

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgbColor(u32);

/// Color format for one pixel. `RGB888` means R in bits 16-23, G in bits 8-15 and B in bits 0-7.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorFormat {
    RGB332,
    RGB565,
    RGB888,
    ARGB8888,
}

#[derive(Debug)]
pub struct Rectangle {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// 2D acceleration capabilities advertised by a display / GPU driver.
///
/// All capabilities default to `false`, meaning the generic software
/// implementations in [`DisplayScheme`] are used. Drivers that can offload
/// these operations to GPU-mapped memory (NVIDIA VRAM over the PCI BAR) or to
/// a host-shared framebuffer (virtio-gpu in QEMU/VirtualBox) override the
/// corresponding methods and set the matching flag, so callers (the graphic
/// console, DRM, ...) can prefer the accelerated 2D path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AccelCaps {
    /// Bulk rectangle fill is accelerated.
    pub fill: bool,
    /// Framebuffer-to-framebuffer copy (e.g. console scroll) is accelerated.
    pub copy: bool,
    /// CPU-buffer-to-framebuffer blit (double buffering) is accelerated.
    pub blit: bool,
}

pub struct FrameBuffer<'a> {
    raw: &'a mut [u8],
}

#[derive(Debug, Clone, Copy)]
pub struct DisplayInfo {
    /// visible width
    pub width: u32,
    /// visible height
    pub height: u32,
    /// Number of bytes between each row of the frame buffer.
    pub pitch: u32,
    /// color encoding format of RGBA
    pub format: ColorFormat,
    /// frame buffer base virtual address
    pub fb_base_vaddr: usize,
    /// frame buffer size
    pub fb_size: usize,
}

impl RgbColor {
    #[inline]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self(((r as u32) << 16) | ((g as u32) << 8) | b as u32)
    }

    #[inline]
    pub const fn r(self) -> u8 {
        (self.0 >> 16) as u8
    }

    #[inline]
    pub const fn g(self) -> u8 {
        (self.0 >> 8) as u8
    }

    #[inline]
    pub const fn b(self) -> u8 {
        self.0 as u8
    }

    #[inline]
    pub const fn raw_value(self) -> u32 {
        self.0
    }
}

impl ColorFormat {
    /// Number of bits per pixel.
    #[inline]
    pub const fn depth(self) -> u8 {
        match self {
            Self::RGB332 => 8,
            Self::RGB565 => 16,
            Self::RGB888 => 24,
            Self::ARGB8888 => 32,
        }
    }

    /// Number of bytes per pixel.
    #[inline]
    pub const fn bytes(self) -> u8 {
        self.depth() / 8
    }
}

impl<'a> FrameBuffer<'a> {
    /// # Safety
    ///
    /// This function is unsafe because it created the `FrameBuffer` structure
    /// from the raw pointer.
    pub unsafe fn from_raw_parts_mut(ptr: *mut u8, len: usize) -> Self {
        unsafe {
            Self {
                raw: core::slice::from_raw_parts_mut(ptr, len),
            }
        }
    }

    pub fn from_slice(slice: &'a mut [u8]) -> Self {
        Self { raw: slice }
    }

    /// # Safety
    ///
    /// This function is unsafe because the caller must ensure `offset` does
    /// not exceed the frame buffer size.
    pub unsafe fn write_color(&mut self, offset: usize, color: RgbColor, format: ColorFormat) {
        unsafe {
            const fn pack_channel(
                r_val: u8,
                _r_bits: u8,
                g_val: u8,
                g_bits: u8,
                b_val: u8,
                b_bits: u8,
            ) -> u32 {
                ((r_val as u32) << (g_bits + b_bits)) | ((g_val as u32) << b_bits) | b_val as u32
            }

            let (r, g, b) = (color.r(), color.g(), color.b());
            let ptr = self.raw.as_mut_ptr().add(offset);
            let dst = core::slice::from_raw_parts_mut(ptr, 4);
            match format {
                ColorFormat::RGB332 => {
                    *ptr = pack_channel(r >> (8 - 3), 3, g >> (8 - 3), 3, b >> (8 - 2), 2) as u8
                }
                ColorFormat::RGB565 => {
                    *(ptr as *mut u16) =
                        pack_channel(r >> (8 - 5), 5, g >> (8 - 6), 6, b >> (8 - 5), 5) as u16
                }
                ColorFormat::RGB888 => {
                    dst[2] = r;
                    dst[1] = g;
                    dst[0] = b;
                }
                ColorFormat::ARGB8888 => *(ptr as *mut u32) = color.raw_value(),
            }
        }
    }
}

impl<'a> core::ops::Deref for FrameBuffer<'a> {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        self.raw
    }
}

impl<'a> core::ops::DerefMut for FrameBuffer<'a> {
    #[allow(clippy::needless_borrow)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.raw
    }
}

impl DisplayInfo {
    /// Number of bytes between each row of the frame buffer.
    #[inline]
    pub const fn pitch(self) -> u32 {
        if self.pitch != 0 {
            self.pitch
        } else {
            self.width * self.format.bytes() as u32
        }
    }
}

pub trait DisplayScheme: Scheme {
    fn info(&self) -> DisplayInfo;

    /// Returns the framebuffer.
    fn fb(&self) -> FrameBuffer<'_>;

    /// 2D acceleration advertised by this display. Defaults to software-only.
    #[inline]
    fn accel_caps(&self) -> AccelCaps {
        AccelCaps::default()
    }

    /// True when the scanout framebuffer is write-combining (UEFI GOP / GPU
    /// BAR1). The generic [`blit_from`](Self::blit_from) then uses
    /// `MOVNTDQ` stores. Leave `false` for RAM framebuffers (virtio-gpu,
    /// QEMU software KMS on a host-shared buffer) so the scalar copy stays
    /// cache-friendly.
    #[inline]
    fn fb_write_combining(&self) -> bool {
        false
    }

    /// Write pixel color.
    #[inline]
    fn draw_pixel(&self, x: u32, y: u32, color: RgbColor) {
        let info = self.info();
        if x >= info.width || y >= info.height {
            return;
        }
        let x = map_x(x, info.width);
        let offset =
            (y as usize * info.pitch() as usize) + (x as usize * info.format.bytes() as usize);
        if offset < info.fb_size {
            unsafe { self.fb().write_color(offset, color, info.format) };
        }
    }

    /// Fill a given rectangle with `color`.
    ///
    /// The generic implementation acquires the framebuffer once and writes it
    /// row by row, instead of going through [`draw_pixel`](Self::draw_pixel) per
    /// pixel (which re-derives the framebuffer slice and re-checks bounds on
    /// every pixel). For the common ARGB8888 format this becomes a tight
    /// word-store loop, which on GPU-mapped (write-combining) memory is several
    /// times faster than the per-pixel path.
    fn fill_rect(&self, rect: &Rectangle, color: RgbColor) {
        let info = self.info();
        let left = rect.x.min(info.width);
        let right = rect.x.saturating_add(rect.width).min(info.width);
        let top = rect.y.min(info.height);
        let bottom = rect.y.saturating_add(rect.height).min(info.height);
        if left >= right || top >= bottom {
            return;
        }

        if info.format == ColorFormat::ARGB8888 {
            let (left, right) = if scanout_mirror_x() {
                (info.width - right, info.width - left)
            } else {
                (left, right)
            };
            let pitch = info.pitch() as usize;
            let px = color.raw_value().to_ne_bytes();
            let mut fb = self.fb();
            let buf: &mut [u8] = &mut fb;
            for y in top..bottom {
                let mut off = y as usize * pitch + left as usize * 4;
                let end = y as usize * pitch + right as usize * 4;
                if end > buf.len() {
                    break;
                }
                while off < end {
                    buf[off..off + 4].copy_from_slice(&px);
                    off += 4;
                }
            }
            self.drain_if_write_combining();
        } else {
            for j in top..bottom {
                for i in left..right {
                    self.draw_pixel(i, j, color);
                }
            }
            self.drain_if_write_combining();
        }
    }

    /// Copy a rectangle within the framebuffer (`memmove` semantics).
    ///
    /// This is the primitive behind console scrolling. The generic version does
    /// a per-row copy honoring the framebuffer pitch and the vertical overlap
    /// direction. Crucially it never reads back already-displayed pixels through
    /// a slow GPU aperture more than the move strictly requires. Drivers with a
    /// hardware 2D blit engine can override this.
    fn copy_rect(&self, src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, width: u32, height: u32) {
        let info = self.info();
        let w = width
            .min(info.width.saturating_sub(src_x))
            .min(info.width.saturating_sub(dst_x)) as usize;
        let h = height
            .min(info.height.saturating_sub(src_y))
            .min(info.height.saturating_sub(dst_y)) as usize;
        if w == 0 || h == 0 {
            return;
        }
        let (src_x, dst_x) = if scanout_mirror_x() {
            (
                info.width.saturating_sub(src_x + w as u32),
                info.width.saturating_sub(dst_x + w as u32),
            )
        } else {
            (src_x, dst_x)
        };
        let pitch = info.pitch() as usize;
        let bpp = info.format.bytes() as usize;
        let row_bytes = w * bpp;
        let mut fb = self.fb();
        let buf: &mut [u8] = &mut fb;

        let mut copy_row = |r: usize| {
            let s = (src_y as usize + r) * pitch + src_x as usize * bpp;
            let d = (dst_y as usize + r) * pitch + dst_x as usize * bpp;
            if s + row_bytes <= buf.len() && d + row_bytes <= buf.len() {
                buf.copy_within(s..s + row_bytes, d);
            }
        };

        if dst_y > src_y {
            for r in (0..h).rev() {
                copy_row(r);
            }
        } else {
            for r in 0..h {
                copy_row(r);
            }
        }
        self.drain_if_write_combining();
    }

    /// Blit a CPU-side ARGB8888 buffer into the framebuffer at `(dst_x, dst_y)`.
    ///
    /// `src` is row-major with `src_stride` pixels per row; only the top-left
    /// `width` x `height` window is used. This is the workhorse of the
    /// double-buffered console: drawing happens in cached RAM and the dirty
    /// region is pushed here in bulk. On a write-combining GOP/BAR1 destination
    /// the generic path uses `MOVNTDQ` stores (see [`fb_write_combining`]);
    /// otherwise it copies whole rows with `copy_from_slice`. Drivers may
    /// override to use DMA / a copy engine.
    fn blit_from(
        &self,
        dst_x: u32,
        dst_y: u32,
        src: &[u32],
        src_stride: usize,
        width: u32,
        height: u32,
    ) {
        let info = self.info();
        let h = height.min(info.height.saturating_sub(dst_y)) as usize;
        let pitch = info.pitch() as usize;
        // Two different right limits, and picking the wrong one silently
        // disabled a mitigation. The *visible* width is the limit for anything
        // that addresses pixels individually. For the straight row copies
        // below the limit is the DESTINATION PITCH in pixels, because a
        // scanline is usually padded (a GOP reports `PixelsPerScanLine`, e.g.
        // 2048 for a 1920-wide mode) and the bytes past `width` in a row are
        // off-screen. `expand_x_for_wc` in `linux-object`'s DRM present path
        // deliberately rounds a blit's right edge UP to a 16-pixel (64-byte)
        // write-combining line and caps it at that pitch, so the tail lands in
        // that padding and the last combine buffer is completed rather than
        // flushed half-full over neighbouring pixels. Clamping here to
        // `info.width` truncated that tail back off and made the expansion
        // inert whenever the pitch was padded and `width % 16 != 0`.
        let visible_w = width.min(info.width.saturating_sub(dst_x)) as usize;
        let padded_w = width.min((pitch / 4).saturating_sub(dst_x as usize) as u32) as usize;
        if h == 0 || src_stride == 0 {
            return;
        }

        if scanout_mirror_x() && info.format == ColorFormat::ARGB8888 {
            let w = visible_w;
            if w == 0 {
                return;
            }
            let mut fb = self.fb();
            let buf: &mut [u8] = &mut fb;
            let screen_w = info.width as usize;
            for r in 0..h {
                let src_off = r * src_stride;
                if src_off + w > src.len() {
                    break;
                }
                let y = dst_y as usize + r;
                for c in 0..w {
                    let px = src[src_off + c].to_le_bytes();
                    let dx = screen_w - 1 - (dst_x as usize + c);
                    let d = y * pitch + dx * 4;
                    if d + 4 > buf.len() {
                        break;
                    }
                    buf[d..d + 4].copy_from_slice(&px);
                }
            }
            self.drain_if_write_combining();
            return;
        }

        if info.format == ColorFormat::ARGB8888 {
            // Row-at-a-time copies: the padding after the visible width is a
            // legal destination (see `padded_w` above).
            let w = padded_w;
            if w == 0 {
                return;
            }
            let mut fb = self.fb();
            let buf: &mut [u8] = &mut fb;
            let width_bytes = w * 4;
            let dst_base = dst_y as usize * pitch + dst_x as usize * 4;
            let last_src = (h - 1).saturating_mul(src_stride).saturating_add(w);
            let last_dst = dst_base
                .saturating_add((h - 1).saturating_mul(pitch))
                .saturating_add(width_bytes);
            if self.fb_write_combining()
                && crate::utils::dma_sync::has_nt_store()
                && last_src <= src.len()
                && last_dst <= buf.len()
                // SAFETY: `last_src`/`last_dst` were bounds-checked against
                // `src`/`buf` just above, and the framebuffer never aliases
                // the source pixel buffer.
                && unsafe {
                    crate::utils::dma_sync::nt_store_rows(
                        buf.as_mut_ptr().add(dst_base),
                        pitch,
                        src.as_ptr() as *const u8,
                        src_stride * 4,
                        width_bytes,
                        h,
                    )
                }
            {
                return;
            }
            for r in 0..h {
                let src_off = r * src_stride;
                if src_off + w > src.len() {
                    break;
                }
                let src_bytes = unsafe {
                    core::slice::from_raw_parts(src[src_off..].as_ptr() as *const u8, width_bytes)
                };
                let d = (dst_y as usize + r) * pitch + dst_x as usize * 4;
                let d_end = d + width_bytes;
                if d_end > buf.len() {
                    break;
                }
                buf[d..d_end].copy_from_slice(src_bytes);
            }
            self.drain_if_write_combining();
        } else {
            // Per-pixel `draw_pixel`, so the visible width is the limit.
            let w = visible_w;
            for r in 0..h {
                let src_off = r * src_stride;
                if src_off + w > src.len() {
                    break;
                }
                for c in 0..w {
                    self.draw_pixel(
                        dst_x + c as u32,
                        dst_y + r as u32,
                        RgbColor(src[src_off + c] & 0x00FF_FFFF),
                    );
                }
            }
            self.drain_if_write_combining();
        }
    }

    /// Alpha-composite a premultiplied ARGB8888 source "over" the framebuffer at
    /// `(dst_x, dst_y)`, which may be negative (the source is clipped to the
    /// visible area). This is the kernel-composited hardware cursor: wlroots is
    /// forced onto the legacy KMS path, so it hands us the pointer bitmap via
    /// `DRM_IOCTL_MODE_CURSOR` and we draw it on top of every scanned-out frame
    /// instead of the compositor re-rendering the whole scene on each move.
    ///
    /// wlroots renders cursors with premultiplied alpha, so the "over" operator
    /// is `out = src + dst * (255 - a) / 255`. Fully transparent pixels are
    /// skipped and fully opaque pixels are copied without reading the (slow,
    /// PCIe-mapped) destination — so only the antialiased edge pays for a
    /// read-modify-write.
    fn blit_argb_over(
        &self,
        dst_x: i32,
        dst_y: i32,
        src: &[u32],
        src_stride: usize,
        width: u32,
        height: u32,
    ) {
        let info = self.info();
        if info.format != ColorFormat::ARGB8888 || src_stride == 0 {
            return;
        }
        let pitch = info.pitch() as usize;
        let (fw, fh) = (info.width as i32, info.height as i32);
        let mut fb = self.fb();
        let buf: &mut [u8] = &mut fb;
        for r in 0..height as i32 {
            let py = dst_y + r;
            if py < 0 || py >= fh {
                continue;
            }
            let src_row = r as usize * src_stride;
            for c in 0..width as i32 {
                let px = dst_x + c;
                if px < 0 || px >= fw {
                    continue;
                }
                let si = src_row + c as usize;
                if si >= src.len() {
                    break;
                }
                let s = src[si];
                let a = s >> 24;
                if a == 0 {
                    continue;
                }
                let d_off = py as usize * pitch + map_x(px as u32, info.width) as usize * 4;
                if d_off + 4 > buf.len() {
                    continue;
                }
                let out = if a == 0xff {
                    s & 0x00FF_FFFF
                } else {
                    let inv = 255 - a;
                    let (sr, sg, sb) = ((s >> 16) & 0xff, (s >> 8) & 0xff, s & 0xff);
                    let d = u32::from_ne_bytes([buf[d_off], buf[d_off + 1], buf[d_off + 2], 0]);
                    let (dr, dg, db) = ((d >> 16) & 0xff, (d >> 8) & 0xff, d & 0xff);
                    // premultiplied over; clamp guards against a non-premultiplied
                    // source (would otherwise bleed into the next channel).
                    let or = (sr + dr * inv / 255).min(0xff);
                    let og = (sg + dg * inv / 255).min(0xff);
                    let ob = (sb + db * inv / 255).min(0xff);
                    (or << 16) | (og << 8) | ob
                };
                buf[d_off..d_off + 4].copy_from_slice(&out.to_ne_bytes());
            }
        }
        self.drain_if_write_combining();
    }

    /// Drain the CPU's write-combining store buffers when this backend's
    /// framebuffer is write-combining, so the scanout engine cannot read a
    /// half-flushed combine buffer as the last line of what we just drew.
    ///
    /// Every generic 2D primitive ends with this. The non-temporal path inside
    /// [`dma_sync::nt_store_rows`] already fences itself, and it says why: "so
    /// scanout / cursor overlay cannot observe a torn last line". The scalar
    /// paths need exactly the same guarantee and had no barrier at all --
    /// `need_flush()` is `false` for both write-combining backends (UEFI GOP and
    /// NVIDIA BAR1), so nothing above supplied one either. A no-op on a
    /// write-back or virtio destination.
    #[inline]
    fn drain_if_write_combining(&self) {
        if self.fb_write_combining() {
            crate::utils::dma_sync::wc_store_drain();
        }
    }

    /// Clear the screen with `color`.
    fn clear(&self, color: RgbColor) {
        let info = self.info();
        self.fill_rect(
            &Rectangle {
                x: 0,
                y: 0,
                width: info.width,
                height: info.height,
            },
            color,
        )
    }

    /// Whether need to flush the frambuffer to screen.
    #[inline]
    fn need_flush(&self) -> bool {
        false
    }

    /// Flush framebuffer to screen.
    #[inline]
    fn flush(&self) -> DeviceResult {
        Ok(())
    }
}

/// Tests for the generic 2D primitives every framebuffer backend inherits.
///
/// These are the last hop of the present path: whatever `linux-object`'s DRM
/// code decides to blit ends up in [`DisplayScheme::blit_from`], writing into a
/// write-combining PCIe aperture. A fake backend over a heap buffer exercises
/// them with no hardware, and the buffer is then inspected byte by byte --
/// including the off-screen padding of each scanline, which is exactly where
/// the write-combining mitigation lives and where it was being discarded.
#[cfg(test)]
mod blit_tests {
    use super::*;
    use alloc::sync::Arc;
    use lock::Mutex;

    /// A display whose "aperture" is a heap buffer. `pitch` is independent of
    /// `width` so a padded scanline -- what a real GOP reports, e.g. 2048
    /// pixels per scanline for a 1920-wide mode -- can be modelled.
    struct FakeDisplay {
        info: DisplayInfo,
        mem: Mutex<alloc::vec::Vec<u8>>,
    }

    impl FakeDisplay {
        fn new(width: u32, height: u32, pitch_px: u32) -> Arc<Self> {
            let pitch = pitch_px * 4;
            let size = (pitch * height) as usize;
            Arc::new(Self {
                info: DisplayInfo {
                    width,
                    height,
                    pitch,
                    format: ColorFormat::ARGB8888,
                    fb_base_vaddr: 0,
                    fb_size: size,
                },
                mem: Mutex::new(alloc::vec![0u8; size]),
            })
        }

        /// The pixel at `(x, y)`, addressed through the pitch, so `x` may point
        /// into the off-screen padding past the visible width.
        fn px(&self, x: u32, y: u32) -> u32 {
            let off = (y * self.info.pitch + x * 4) as usize;
            let m = self.mem.lock();
            u32::from_ne_bytes([m[off], m[off + 1], m[off + 2], m[off + 3]])
        }
    }

    impl Scheme for FakeDisplay {
        fn name(&self) -> &str {
            "fake-display"
        }
    }

    impl DisplayScheme for FakeDisplay {
        fn info(&self) -> DisplayInfo {
            self.info
        }
        fn fb(&self) -> FrameBuffer<'_> {
            // SAFETY: the `Vec` outlives the returned view (it is owned by
            // `self`), and the lock is released immediately -- the same shape
            // the real backends use, which hand out a raw aperture pointer.
            // Nothing in a single-threaded test aliases it.
            let mut m = self.mem.lock();
            unsafe { FrameBuffer::from_raw_parts_mut(m.as_mut_ptr(), m.len()) }
        }
        /// The two real backends (UEFI GOP and NVIDIA BAR1) both report true,
        /// which is what makes the edge alignment matter at all.
        fn fb_write_combining(&self) -> bool {
            true
        }
    }

    /// Source pixels numbered `0x1000 + n` so every one is distinguishable
    /// from the 0 the framebuffer starts at.
    fn numbered(stride: usize, rows: usize) -> alloc::vec::Vec<u32> {
        (0..stride * rows).map(|n| 0x1000 + n as u32).collect()
    }

    /// The regression this guards. `expand_x_for_wc` (linux-object's DRM
    /// present path) rounds a blit's right edge UP to a 16-pixel / 64-byte
    /// write-combining line, capped at the destination PITCH so the tail lands
    /// in the scanline's off-screen padding: that completes the last combine
    /// buffer instead of leaving it to be flushed half-full over neighbouring
    /// pixels ("leftover squares and stripes"). `blit_from` then clamped the
    /// width back to `info.width`, silently undoing it -- so on any mode whose
    /// width is not a multiple of 16 and whose pitch is padded, the mitigation
    /// did nothing. 1366 is such a width (1366 % 16 == 6) and is a real GOP
    /// mode.
    #[test]
    fn a_blit_may_write_the_off_screen_tail_of_a_padded_scanline() {
        let d = FakeDisplay::new(1366, 4, 1536); // 1366 visible, 1536 pitch
                                                 // What `expand_x_for_wc(1360, 6, 1536)` yields: start at 1360 (a
                                                 // 16-pixel boundary) and round 1366 up to 1376.
        let w = 16u32;
        let src = numbered(w as usize, 2);
        d.blit_from(1360, 0, &src, w as usize, w, 2);

        // The visible part landed.
        assert_eq!(d.px(1360, 0), 0x1000);
        assert_eq!(d.px(1365, 0), 0x1005, "last visible pixel");
        // And so did the tail past the visible width, up to the 16-pixel
        // boundary. This is the assertion that failed before the fix: the
        // clamp stopped the copy at x = 1366.
        assert_eq!(d.px(1366, 0), 0x1006, "first off-screen pixel of the line");
        assert_eq!(d.px(1375, 0), 0x100F, "the combine buffer is completed");
        // Second row, addressed through the pitch, not through the width.
        assert_eq!(d.px(1366, 1), 0x1016);
    }

    /// Why the drain in every 2D primitive is not optional. Both real
    /// write-combining backends advertise `fb_write_combining()` and neither
    /// overrides `need_flush()`, so before this there was no barrier anywhere
    /// between an ordinary store into the aperture and the scanout engine
    /// reading it -- the last line of a blit could reach the panel torn, or a
    /// frame late. The one backend that DOES flush (virtio-gpu) is the one that
    /// is not write-combining.
    #[test]
    fn a_write_combining_backend_offers_no_flush_of_its_own() {
        let uefi = crate::display::UefiDisplay::new(DisplayInfo {
            width: 1920,
            height: 1080,
            pitch: 1920 * 4,
            format: ColorFormat::ARGB8888,
            fb_base_vaddr: 0,
            fb_size: 1920 * 1080 * 4,
        });
        assert!(
            uefi.fb_write_combining(),
            "the GOP aperture is write-combining"
        );
        assert!(
            !uefi.need_flush(),
            "...and offers no flush, so the primitives must drain themselves"
        );
    }

    /// The drain runs whenever the destination says it is write-combining, and
    /// is skipped otherwise. Asserted through the trait so a backend cannot
    /// accidentally opt out of it.
    #[test]
    fn the_drain_follows_the_backends_own_answer() {
        struct Cached(DisplayInfo, Mutex<alloc::vec::Vec<u8>>);
        impl Scheme for Cached {
            fn name(&self) -> &str {
                "cached"
            }
        }
        impl DisplayScheme for Cached {
            fn info(&self) -> DisplayInfo {
                self.0
            }
            fn fb(&self) -> FrameBuffer<'_> {
                let mut m = self.1.lock();
                // SAFETY: owned by `self`, outlives the view.
                unsafe { FrameBuffer::from_raw_parts_mut(m.as_mut_ptr(), m.len()) }
            }
            fn fb_write_combining(&self) -> bool {
                false
            }
        }
        let wc = FakeDisplay::new(16, 2, 16);
        assert!(wc.fb_write_combining());
        // Both must complete without panicking, and both must draw: the drain
        // is a barrier, never a bail-out.
        wc.fill_rect(
            &Rectangle {
                x: 0,
                y: 0,
                width: 4,
                height: 1,
            },
            RgbColor(0x00AB_CDEF),
        );
        assert_eq!(wc.px(0, 0), 0x00AB_CDEF);

        let info = DisplayInfo {
            width: 16,
            height: 2,
            pitch: 64,
            format: ColorFormat::ARGB8888,
            fb_base_vaddr: 0,
            fb_size: 128,
        };
        let cached = Cached(info, Mutex::new(alloc::vec![0u8; 128]));
        cached.fill_rect(
            &Rectangle {
                x: 0,
                y: 0,
                width: 4,
                height: 1,
            },
            RgbColor(0x0012_3456),
        );
        assert_eq!(&cached.1.lock()[0..4], &0x0012_3456u32.to_ne_bytes());
    }

    /// The pitch is a hard limit even so: the padding belongs to this row, and
    /// a copy that ran past it would write the START of the next scanline --
    /// visible garbage on the left edge one line down.
    #[test]
    fn a_blit_never_runs_past_the_pitch_into_the_next_scanline() {
        let d = FakeDisplay::new(1366, 4, 1536);
        // Ask for far more than the pitch allows from x = 1520.
        let src = numbered(256, 2);
        d.blit_from(1520, 0, &src, 256, 256, 2);

        assert_eq!(d.px(1520, 0), 0x1000);
        assert_eq!(d.px(1535, 0), 0x100F, "last pixel of the padded row");
        // Row 1 got its own copy starting at its own x = 1520, and nothing
        // spilled into its x = 0.
        assert_eq!(d.px(0, 1), 0, "the next scanline's left edge is untouched");
        assert_eq!(d.px(1520, 1), 0x1100);
    }

    /// With no padding the two limits coincide, so an unpadded mode must
    /// behave exactly as it did before: clipped at the visible width, and never
    /// one pixel into the next row.
    #[test]
    fn an_unpadded_mode_is_still_clipped_at_the_visible_width() {
        let d = FakeDisplay::new(64, 4, 64); // pitch == width
        let src = numbered(32, 2);
        d.blit_from(56, 0, &src, 32, 32, 2);

        assert_eq!(d.px(56, 0), 0x1000);
        assert_eq!(d.px(63, 0), 0x1007, "last pixel of the row");
        assert_eq!(d.px(0, 1), 0, "no spill into the next row");
        assert_eq!(d.px(56, 1), 0x1020, "row 1 starts from its own source row");
    }

    /// A blit that starts at or past the right edge writes nothing, rather
    /// than wrapping a subtraction and painting somewhere else.
    #[test]
    fn a_blit_entirely_off_the_right_edge_writes_nothing() {
        let d = FakeDisplay::new(64, 4, 64);
        let src = numbered(8, 2);
        d.blit_from(64, 0, &src, 8, 8, 2);
        d.blit_from(200, 0, &src, 8, 8, 2);
        let m = d.mem.lock();
        assert!(m.iter().all(|&b| b == 0), "nothing should have been drawn");
    }

    /// Vertical clipping is against the visible height in every case: there is
    /// no "padding" below the last row, only the next thing in the aperture.
    #[test]
    fn a_blit_is_clipped_at_the_last_visible_row() {
        let d = FakeDisplay::new(16, 2, 16);
        let src = numbered(16, 8);
        d.blit_from(0, 1, &src, 16, 16, 8);
        assert_eq!(d.px(0, 1), 0x1000, "the one row that fits");
        // Nothing beyond: the buffer is exactly 2 rows, so a 3rd would have to
        // land outside it.
        assert_eq!(d.mem.lock().len(), 16 * 4 * 2);
    }
}
