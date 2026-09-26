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

/// How logical X maps onto the scanout, for one operation.
///
/// Every 2D primitive below has to apply the mirror, and each one used to spell
/// it out again: `draw_pixel` and `blit_argb_over` per pixel, `fill_rect` by
/// flipping a half-open range, `copy_rect` by flipping two rectangle origins,
/// `blit_from` with its own `screen_w - 1 - x`. Five hand-written copies of one
/// decision that happened to agree, with nothing checking that they did -- and
/// the mirror is opt-in at boot, so no test and no CI run ever exercised a
/// single one of them.
///
/// Reading the flag once into this also means one operation cannot see it change
/// half way through, which is what made a per-pixel `scanout_mirror_x()` a
/// question asked `width * height` times per fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct XMap {
    mirror: bool,
    width: u32,
}

impl XMap {
    /// The mapping in force right now for a screen `width` pixels wide.
    #[inline]
    fn of(width: u32) -> Self {
        Self {
            mirror: scanout_mirror_x(),
            width,
        }
    }

    /// One pixel's column.
    #[inline]
    fn px(self, x: u32) -> u32 {
        if self.mirror {
            self.width.saturating_sub(1).saturating_sub(x)
        } else {
            x
        }
    }

    /// A half-open column range `[left, right)`, as a half-open range again.
    ///
    /// This is [`px`](Self::px) applied to a whole span, and it has to stay
    /// exactly that: the mirror of `{ px(i) : i in [left, right) }` is
    /// `[width - right, width - left)`, because reversing a half-open range
    /// moves both ends. Callers hold `left <= right <= width`, which every one
    /// of them gets from clamping against the visible width first.
    #[inline]
    fn range(self, left: u32, right: u32) -> (u32, u32) {
        if self.mirror {
            (
                self.width.saturating_sub(right),
                self.width.saturating_sub(left),
            )
        } else {
            (left, right)
        }
    }
}

/// Whether a pixel of `bytes` bytes written at `offset` fits inside an aperture
/// of `fb_size` bytes.
///
/// The bound has to cover every byte the write touches. Checking only the first
/// (`offset < fb_size`) lets a pixel whose last byte falls past the end through,
/// and that write lands outside the mapping -- a device aperture on real
/// hardware. The addition is `checked_add` because `offset` comes from
/// `y * pitch + x * bytes`: an aperture whose reported size does not cover
/// `pitch * height` is exactly the case this exists for, and a wrap there would
/// answer "fits".
#[inline]
fn pixel_fits(offset: usize, bytes: usize, fb_size: usize) -> bool {
    matches!(offset.checked_add(bytes), Some(end) if end <= fb_size)
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
            // As wide as the format, not a flat four. A three-byte pixel at the
            // end of an unpadded scanline has only three bytes left, so a
            // four-byte slice there reaches one past the mapping -- nothing is
            // written to it, but forming the slice at all is not allowed.
            let dst = core::slice::from_raw_parts_mut(ptr, format.bytes() as usize);
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
        let x = XMap::of(info.width).px(x);
        let offset =
            (y as usize * info.pitch() as usize) + (x as usize * info.format.bytes() as usize);
        if pixel_fits(offset, info.format.bytes() as usize, info.fb_size) {
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
            let (left, right) = XMap::of(info.width).range(left, right);
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
        // Both origins through the same mapping: a run is mirrored by flipping
        // its whole span, so the new left edge is the old right edge's mirror.
        let xm = XMap::of(info.width);
        let (src_x, _) = xm.range(src_x, src_x + w as u32);
        let (dst_x, _) = xm.range(dst_x, dst_x + w as u32);
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
            // `visible_w`, not `padded_w`, and on purpose. The padded limit
            // exists so a ROW COPY's tail lands in the scanline's off-screen
            // padding and completes the last write-combining buffer. Mirrored,
            // this path writes pixel by pixel in decreasing address order, so
            // there is no row-copy tail to complete -- and the logical right
            // edge maps to the physical LEFT, so extending past it would walk
            // below column 0 into the previous row instead of into the padding.
            let w = visible_w;
            if w == 0 {
                return;
            }
            let mut fb = self.fb();
            let buf: &mut [u8] = &mut fb;
            for r in 0..h {
                let src_off = r * src_stride;
                if src_off + w > src.len() {
                    break;
                }
                let y = dst_y as usize + r;
                let xm = XMap::of(info.width);
                for c in 0..w {
                    let px = src[src_off + c].to_le_bytes();
                    let dx = xm.px(dst_x + c as u32) as usize;
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
        let xm = XMap::of(info.width);
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
                let d_off = py as usize * pitch + xm.px(px as u32) as usize * 4;
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

        /// The whole aperture, padding included. A blit that differs from
        /// another only in the off-screen bytes is still a blit that differs:
        /// those bytes are the next scanline's neighbours.
        fn snapshot(&self) -> alloc::vec::Vec<u8> {
            self.mem.lock().clone()
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

    /// A write-combining destination has **two** implementations of the same
    /// blit: `MOVNTDQ` stores when the CPU has SSE4.1, and `copy_from_slice`
    /// rows otherwise. Only the second one ever runs here or in QEMU --
    /// `HAS_NT_BLIT` is set by `probe_cpu_features` at boot, which no test
    /// calls, and the emulator's framebuffer is not write-combining anyway --
    /// so the fast path that Moebius's GOP takes on every single present was
    /// running untested while the slow path was the one under the microscope.
    ///
    /// If the two ever disagree, they disagree about pixels on a real screen
    /// and about nothing at all in CI. So: same blit, same source, both paths,
    /// byte for byte, over the geometries where they are most likely to part
    /// company -- a padded scanline, a width that is not a multiple of 16, an
    /// odd destination x, and a source window narrower than its stride.
    #[test]
    fn the_two_write_combining_paths_agree_byte_for_byte() {
        // (screen_w, screen_h, pitch_px, dst_x, dst_y, src_stride, w, h)
        let cases: &[(u32, u32, u32, u32, u32, usize, u32, u32)] = &[
            // A 1920 mode on a 2048-pixel pitch: 512 bytes of padding a row.
            (1920, 4, 2048, 0, 0, 1920, 1920, 4),
            // 1366 * 4 = 5464 bytes, which is 8 mod 16: every row ends inside
            // a step and leans on the bytewise tail.
            (1366, 4, 1536, 0, 0, 1366, 1366, 4),
            // The `expand_x_for_wc` shape: a 16-pixel window whose tail lands
            // in the off-screen padding.
            (1366, 4, 1536, 1360, 0, 16, 16, 2),
            // An odd destination x, so every row starts mid-16-byte-line.
            (800, 6, 800, 3, 1, 101, 101, 3),
            // A window of a wider source buffer: the bytes between `w` and the
            // stride must not be read.
            (640, 8, 704, 5, 2, 512, 200, 4),
            // One row, which is the cursor overlay and a one-line damage
            // rectangle both.
            (640, 8, 704, 0, 3, 640, 640, 1),
            // Clipped at the last visible row: the clip happens before either
            // path is chosen, so both must clip the same.
            (640, 4, 704, 0, 2, 640, 640, 9),
        ];

        for &(sw, sh, pitch, dx, dy, stride, w, h) in cases {
            let src = numbered(stride, h as usize + 1);
            let label = alloc::format!(
                "{}x{} pitch {} blit {}x{} at ({},{})",
                sw,
                sh,
                pitch,
                w,
                h,
                dx,
                dy
            );

            let slow = FakeDisplay::new(sw, sh, pitch);
            {
                let flag = crate::utils::dma_sync::test_flag::pinned(false);
                assert!(!flag.nt);
                slow.blit_from(dx, dy, &src, stride, w, h);
            }

            let fast = FakeDisplay::new(sw, sh, pitch);
            let took_fast_path = {
                let flag = crate::utils::dma_sync::test_flag::as_detected();
                let before = crate::utils::dma_sync::test_flag::nt_store_calls();
                fast.blit_from(dx, dy, &src, stride, w, h);
                let after = crate::utils::dma_sync::test_flag::nt_store_calls();
                if !flag.nt {
                    // No SSE4.1 here, so there is no second path to compare
                    // against and the case proves nothing. Say so rather than
                    // report a pass.
                    return;
                }
                after > before
            };
            assert!(
                took_fast_path,
                "{} never reached the non-temporal path, so comparing it with \
                 the scalar one compares the scalar one with itself",
                label
            );
            assert_eq!(
                fast.snapshot(),
                slow.snapshot(),
                "the non-temporal and scalar paths disagree for {}",
                label
            );
            // And not vacuously equal because neither wrote anything.
            assert_ne!(
                fast.snapshot(),
                FakeDisplay::new(sw, sh, pitch).snapshot(),
                "{} wrote nothing at all",
                label
            );
        }
    }

    // ------------------------------------------------------------------
    // The primitives `blit_from`'s tests above never reached. `fill_rect`,
    // `copy_rect`, `blit_argb_over`, `clear` and `draw_pixel` are the rest of
    // what every backend inherits: the console's own scroll and fill, and the
    // alpha composite the DRM path uses to put a cursor over a frame without
    // re-rendering it. Same fake aperture, same byte-for-byte inspection --
    // including the off-screen padding of each scanline, because a primitive
    // that spills into it is a primitive writing on the next row's neighbours.
    // ------------------------------------------------------------------

    /// Like [`FakeDisplay::new`], but the aperture is `short_by` bytes smaller
    /// than `pitch * height`, which is what a mode whose padded pitch was not
    /// accounted for in the reported size looks like. The last pixels of the
    /// last row then straddle the end of the buffer, and every primitive has to
    /// notice.
    fn truncated(width: u32, height: u32, pitch_px: u32, short_by: usize) -> Arc<FakeDisplay> {
        let pitch = pitch_px * 4;
        let size = (pitch * height) as usize - short_by;
        Arc::new(FakeDisplay {
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

    const RED: RgbColor = RgbColor(0x00FF_0000);
    const BLUE: RgbColor = RgbColor(0x0000_00FF);

    // ---- fill_rect ----

    #[test]
    fn a_fill_covers_exactly_the_rectangle_it_was_given() {
        let d = FakeDisplay::new(8, 4, 8);
        d.fill_rect(
            &Rectangle {
                x: 2,
                y: 1,
                width: 3,
                height: 2,
            },
            RED,
        );
        for y in 0..4 {
            for x in 0..8 {
                let inside = (2..5).contains(&x) && (1..3).contains(&y);
                let want = if inside { RED.raw_value() } else { 0 };
                assert_eq!(d.px(x, y), want, "pixel ({}, {})", x, y);
            }
        }
    }

    #[test]
    fn a_fill_is_clipped_at_every_edge_instead_of_wrapping_to_the_next_row() {
        // A rectangle that runs off the right edge must stop at the visible
        // width: the bytes past it are the next scanline's, and writing them is
        // how a fill turns into a diagonal stripe.
        let d = FakeDisplay::new(4, 3, 6); // 4 visible, 6 pitch
        d.fill_rect(
            &Rectangle {
                x: 2,
                y: 0,
                width: 100,
                height: 100,
            },
            RED,
        );
        for y in 0..3 {
            assert_eq!(d.px(0, y), 0);
            assert_eq!(d.px(1, y), 0);
            assert_eq!(d.px(2, y), RED.raw_value());
            assert_eq!(d.px(3, y), RED.raw_value());
            // The off-screen padding of the scanline stays untouched.
            assert_eq!(d.px(4, y), 0, "padding of row {}", y);
            assert_eq!(d.px(5, y), 0, "padding of row {}", y);
        }
    }

    #[test]
    fn a_rectangle_that_starts_past_the_edge_writes_nothing() {
        for rect in [
            Rectangle {
                x: 4,
                y: 0,
                width: 4,
                height: 4,
            },
            Rectangle {
                x: 0,
                y: 3,
                width: 4,
                height: 4,
            },
            Rectangle {
                x: 0,
                y: 0,
                width: 0,
                height: 4,
            },
            Rectangle {
                x: 0,
                y: 0,
                width: 4,
                height: 0,
            },
        ] {
            let d = FakeDisplay::new(4, 3, 4);
            let before = d.snapshot();
            d.fill_rect(&rect, RED);
            assert_eq!(d.snapshot(), before, "rect {:?} wrote something", rect);
        }
    }

    #[test]
    fn a_rectangle_whose_corner_overflows_a_u32_is_clipped_not_wrapped() {
        // `x + width` past `u32::MAX` wraps to a small number, which would turn
        // "entirely off screen" into "fills from 0". `saturating_add` is what
        // stops it, and this is the input that tells the two apart.
        let d = FakeDisplay::new(4, 2, 4);
        let before = d.snapshot();
        d.fill_rect(
            &Rectangle {
                x: u32::MAX - 1,
                y: 0,
                width: 8,
                height: 2,
            },
            RED,
        );
        assert_eq!(d.snapshot(), before);
        d.fill_rect(
            &Rectangle {
                x: 0,
                y: u32::MAX - 1,
                width: 4,
                height: 8,
            },
            RED,
        );
        assert_eq!(d.snapshot(), before);
    }

    #[test]
    fn a_fill_stops_at_the_end_of_a_truncated_aperture() {
        // The aperture is one pixel short of `pitch * height`, so the last row
        // does not fit. Writing it would run off the end of the mapping.
        let d = truncated(4, 3, 4, 4);
        d.fill_rect(
            &Rectangle {
                x: 0,
                y: 0,
                width: 4,
                height: 3,
            },
            RED,
        );
        let m = d.mem.lock();
        assert_eq!(m.len(), 4 * 4 * 3 - 4);
        // Rows 0 and 1 are filled; row 2 did not fit and was left alone.
        for i in 0..8 {
            let off = i * 4;
            assert_eq!(
                u32::from_ne_bytes([m[off], m[off + 1], m[off + 2], m[off + 3]]),
                RED.raw_value(),
                "pixel {}",
                i
            );
        }
        assert!(
            m[32..].iter().all(|&b| b == 0),
            "the row that did not fit was written anyway"
        );
    }

    #[test]
    fn the_two_fill_paths_paint_the_same_rectangle() {
        // ARGB8888 takes a word-store loop; every other format goes pixel by
        // pixel through `draw_pixel`. The same decision written twice, so the
        // set of pixels they cover has to be the same one -- each read in its
        // own units, because a 24-bit pixel is three bytes, not four.
        let painted = |format: ColorFormat| {
            let mut d = FakeDisplay::new(6, 4, 8);
            Arc::get_mut(&mut d).unwrap().info.format = format;
            d.fill_rect(
                &Rectangle {
                    x: 1,
                    y: 1,
                    width: 4,
                    height: 2,
                },
                RED,
            );
            let bytes = format.bytes() as usize;
            let pitch = d.info.pitch as usize;
            let m = d.mem.lock();
            let mut set = alloc::vec::Vec::new();
            for y in 0..4usize {
                for x in 0..8usize {
                    let off = y * pitch + x * bytes;
                    if off + bytes <= m.len() && m[off..off + bytes].iter().any(|&b| b != 0) {
                        set.push((x, y));
                    }
                }
            }
            set
        };
        let argb = painted(ColorFormat::ARGB8888);
        assert_eq!(
            argb,
            alloc::vec![
                (1, 1),
                (2, 1),
                (3, 1),
                (4, 1),
                (1, 2),
                (2, 2),
                (3, 2),
                (4, 2)
            ]
        );
        for format in [
            ColorFormat::RGB888,
            ColorFormat::RGB565,
            ColorFormat::RGB332,
        ] {
            assert_eq!(
                painted(format),
                argb,
                "{:?} covers a different rectangle than ARGB8888",
                format
            );
        }
    }

    #[test]
    fn clearing_the_screen_leaves_the_off_screen_padding_alone() {
        let d = FakeDisplay::new(4, 3, 6);
        d.clear(BLUE);
        for y in 0..3 {
            for x in 0..4 {
                assert_eq!(d.px(x, y), BLUE.raw_value(), "({}, {})", x, y);
            }
            assert_eq!(d.px(4, y), 0, "padding of row {}", y);
            assert_eq!(d.px(5, y), 0, "padding of row {}", y);
        }
    }

    // ---- copy_rect: the console scroll ----

    /// Paint row `y` with the colour `0x00_0y_0y_0y` so a row that lands in the
    /// wrong place names itself.
    fn rows(d: &Arc<FakeDisplay>) {
        for y in 0..d.info.height {
            d.fill_rect(
                &Rectangle {
                    x: 0,
                    y,
                    width: d.info.width,
                    height: 1,
                },
                RgbColor(0x0001_0101 * (y + 1)),
            );
        }
    }

    fn row_of(d: &Arc<FakeDisplay>, y: u32) -> u32 {
        d.px(0, y)
    }

    #[test]
    fn scrolling_up_moves_every_row_once_and_does_not_smear() {
        // The console scroll: copy rows 1..h up by one. Source and destination
        // overlap, and the copy runs top to bottom so a row is read before the
        // row above it is overwritten.
        let d = FakeDisplay::new(4, 5, 4);
        rows(&d);
        d.copy_rect(0, 1, 0, 0, 4, 4);
        for y in 0..4 {
            assert_eq!(
                row_of(&d, y),
                0x0001_0101 * (y + 2),
                "row {} after the scroll",
                y
            );
        }
        // The last row is left as it was, for the caller to clear.
        assert_eq!(row_of(&d, 4), 0x0001_0101 * 5);
    }

    #[test]
    fn scrolling_down_runs_bottom_to_top_so_it_does_not_smear_either() {
        // The other overlap direction. Walking top to bottom here would copy
        // row 0 down onto row 1 and then read it back as the source for row 2,
        // painting the whole region with row 0.
        let d = FakeDisplay::new(4, 5, 4);
        rows(&d);
        d.copy_rect(0, 0, 0, 1, 4, 4);
        for y in 1..5 {
            assert_eq!(row_of(&d, y), 0x0001_0101 * y, "row {} after the scroll", y);
        }
        assert_eq!(row_of(&d, 0), 0x0001_0101);
    }

    #[test]
    fn a_copy_that_overlaps_within_one_row_moves_the_pixels_not_the_first_one() {
        // Horizontal overlap: shifting a run right by one. A plain forward
        // byte copy would replicate the leftmost pixel across the whole run.
        let d = FakeDisplay::new(6, 1, 6);
        for x in 0..6u32 {
            d.draw_pixel(x, 0, RgbColor(0x0010_0000 + x));
        }
        d.copy_rect(0, 0, 1, 0, 5, 1);
        assert_eq!(d.px(0, 0), 0x0010_0000);
        for x in 1..6u32 {
            assert_eq!(d.px(x, 0), 0x0010_0000 + (x - 1), "pixel {}", x);
        }
    }

    #[test]
    fn a_copy_is_clipped_by_both_ends_of_the_move() {
        // The width is limited by whichever of source and destination runs out
        // of screen first, and the same for the height. Taking only the source
        // into account writes past the right edge of the destination row.
        let d = FakeDisplay::new(8, 2, 8);
        for x in 0..8u32 {
            d.draw_pixel(x, 0, RgbColor(0x0020_0000 + x));
        }
        // From x=0 to x=5, six pixels asked for: only three fit.
        d.copy_rect(0, 0, 5, 1, 6, 1);
        for x in 5..8u32 {
            assert_eq!(d.px(x, 1), 0x0020_0000 + (x - 5), "pixel {}", x);
        }
        // Nothing before the destination was touched...
        for x in 0..5u32 {
            assert_eq!(d.px(x, 1), 0, "pixel {} of row 1", x);
        }
        // ...and the source row is unchanged.
        for x in 0..8u32 {
            assert_eq!(d.px(x, 0), 0x0020_0000 + x);
        }
    }

    /// Like [`FakeDisplay::new`], but the aperture is two rows LARGER than the
    /// mode -- which is the normal case on real hardware, where the mode is
    /// whatever the firmware set and the aperture is the whole BAR. It matters
    /// because the slack below the visible screen is inside the mapping, so a
    /// primitive that clips by the buffer's length instead of by the screen's
    /// height writes there and nothing stops it.
    fn oversized(width: u32, height: u32, pitch_px: u32) -> Arc<FakeDisplay> {
        let pitch = pitch_px * 4;
        let size = (pitch * (height + 2)) as usize;
        Arc::new(FakeDisplay {
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

    #[test]
    fn a_copy_does_not_scroll_rows_below_the_visible_screen() {
        // The aperture holds two rows more than the mode. A copy whose
        // destination runs off the bottom has to be clipped by the SCREEN, not
        // by the buffer: the slack below is inside the mapping, so the bound
        // check cannot catch it and the rows land off screen.
        let d = oversized(2, 4, 2);
        rows(&d);
        // Three rows from y=0 down to y=2: only two are on screen.
        d.copy_rect(0, 0, 0, 2, 2, 3);
        assert_eq!(row_of(&d, 2), 0x0001_0101);
        assert_eq!(row_of(&d, 3), 0x0002_0202);
        assert_eq!(row_of(&d, 4), 0, "scrolled a row below the visible screen");
        assert_eq!(row_of(&d, 5), 0);
    }

    #[test]
    fn a_fill_does_not_paint_rows_below_the_visible_screen() {
        let d = oversized(2, 3, 2);
        d.fill_rect(
            &Rectangle {
                x: 0,
                y: 0,
                width: 2,
                height: 100,
            },
            RED,
        );
        for y in 0..3 {
            assert_eq!(d.px(0, y), RED.raw_value(), "row {}", y);
        }
        assert_eq!(d.px(0, 3), 0, "painted a row below the visible screen");
        assert_eq!(d.px(0, 4), 0);
    }

    #[test]
    fn a_copy_is_clipped_by_the_lower_of_the_two_bottom_edges() {
        // The height, like the width, is limited by whichever end runs out of
        // screen first. Taking only the source into account writes rows past
        // the bottom of the destination -- into the next thing in the aperture.
        let d = FakeDisplay::new(2, 4, 2);
        rows(&d);
        // Three rows asked for from y=0 to y=2: only two fit below y=2.
        d.copy_rect(0, 0, 0, 2, 2, 3);
        assert_eq!(row_of(&d, 2), 0x0001_0101, "row 0 did not land at y=2");
        assert_eq!(row_of(&d, 3), 0x0002_0202, "row 1 did not land at y=3");
        // The rows above the destination are untouched.
        assert_eq!(row_of(&d, 0), 0x0001_0101);
        assert_eq!(row_of(&d, 1), 0x0002_0202);
    }

    #[test]
    fn a_copy_with_nothing_to_move_writes_nothing() {
        for (sx, sy, dx, dy, w, h) in [
            (0u32, 0u32, 0u32, 0u32, 0u32, 4u32),
            (0, 0, 0, 0, 4, 0),
            (4, 0, 0, 0, 4, 4),
            (0, 4, 0, 0, 4, 4),
            (0, 0, 4, 0, 4, 4),
        ] {
            let d = FakeDisplay::new(4, 4, 4);
            rows(&d);
            let before = d.snapshot();
            d.copy_rect(sx, sy, dx, dy, w, h);
            assert_eq!(
                d.snapshot(),
                before,
                "copy_rect({}, {}, {}, {}, {}, {}) wrote something",
                sx,
                sy,
                dx,
                dy,
                w,
                h
            );
        }
    }

    // ---- blit_argb_over: the alpha composite ----

    /// A `width` x `height` source, every pixel `argb`.
    fn solid(argb: u32, width: usize, height: usize) -> alloc::vec::Vec<u32> {
        alloc::vec![argb; width * height]
    }

    #[test]
    fn a_fully_transparent_pixel_leaves_the_destination_alone() {
        // Laid down with `blit_from`, so the destination's alpha byte is 0xFF
        // and every byte of the pixel is a witness. Blending a transparent
        // source instead of skipping it gives the same three colour channels
        // back (`inv` is 255), so without a non-zero byte to watch the two are
        // indistinguishable -- and the alpha byte is that witness.
        let d = FakeDisplay::new(4, 2, 4);
        let frame = (0..8u32)
            .map(|n| 0xFF00_0000 | (0x1000 + n))
            .collect::<alloc::vec::Vec<_>>();
        d.blit_from(0, 0, &frame, 4, 4, 2);
        let before = d.snapshot();
        d.blit_argb_over(0, 0, &solid(0x0000_0000, 4, 2), 4, 4, 2);
        assert_eq!(
            d.snapshot(),
            before,
            "a transparent pixel touched the destination"
        );
    }

    /// The opaque fast path is a *performance* shortcut, not a different answer:
    /// with `a == 0xff` the blend's `inv` is 0, so `src + dst * 0 / 255` is the
    /// source either way. What the shortcut buys is not reading back through a
    /// PCIe aperture, and a heap buffer cannot show a read. So this fixes the
    /// result, and removing the shortcut is a mutation no test can catch --
    /// proven equivalent rather than left unexplained.
    #[test]
    fn a_fully_opaque_pixel_replaces_the_destination() {
        let d = FakeDisplay::new(4, 2, 4);
        d.clear(RED);
        d.blit_argb_over(1, 0, &solid(0xFF00_00FF, 2, 1), 2, 2, 1);
        assert_eq!(d.px(0, 0), RED.raw_value());
        assert_eq!(d.px(1, 0), 0x0000_00FF);
        assert_eq!(d.px(2, 0), 0x0000_00FF);
        assert_eq!(d.px(3, 0), RED.raw_value());
        assert_eq!(d.px(0, 1), RED.raw_value());
    }

    #[test]
    fn a_half_transparent_pixel_is_the_premultiplied_over_operator() {
        // wlroots hands over premultiplied alpha, so the operator is
        // `out = src + dst * (255 - a) / 255`. A source of 0x80404040 over a
        // destination of 0x00FF0000 gives 0x40C04040... computed, not asserted
        // from memory:
        //   r = 0x40 + 0xFF * 0x7F / 0xFF = 0x40 + 0x7F = 0xBF
        //   g = 0x40 + 0x00        = 0x40
        //   b = 0x40 + 0x00        = 0x40
        let d = FakeDisplay::new(1, 1, 1);
        d.clear(RED);
        d.blit_argb_over(0, 0, &solid(0x8040_4040, 1, 1), 1, 1, 1);
        assert_eq!(d.px(0, 0) & 0x00FF_FFFF, 0x00BF_4040);
    }

    #[test]
    fn a_source_that_is_not_premultiplied_clamps_instead_of_bleeding() {
        // A non-premultiplied source can push a channel past 255, and without
        // the clamp the carry lands in the channel above -- red bleeding out of
        // an overflowing green. `0x01FFFFFF` over white is that input.
        let d = FakeDisplay::new(1, 1, 1);
        d.clear(RgbColor(0x00FF_FFFF));
        d.blit_argb_over(0, 0, &solid(0x01FF_FFFF, 1, 1), 1, 1, 1);
        assert_eq!(d.px(0, 0) & 0x00FF_FFFF, 0x00FF_FFFF);
    }

    #[test]
    fn a_composite_partly_off_the_top_left_draws_only_what_is_on_screen() {
        // The cursor hotspot puts the blit at a negative origin every time it
        // touches the top or left edge, so this is the common case, not an
        // edge one.
        let d = FakeDisplay::new(3, 3, 3);
        let src = numbered(2, 2)
            .iter()
            .map(|p| 0xFF00_0000 | p)
            .collect::<alloc::vec::Vec<_>>();
        d.blit_argb_over(-1, -1, &src, 2, 2, 2);
        // Only the source's bottom-right pixel lands, at (0, 0).
        assert_eq!(d.px(0, 0), 0x1003);
        assert_eq!(d.px(1, 0), 0);
        assert_eq!(d.px(0, 1), 0);
    }

    #[test]
    fn a_composite_partly_off_the_bottom_right_draws_only_what_is_on_screen() {
        let d = FakeDisplay::new(3, 2, 4); // 3 visible, 4 pitch
        d.blit_argb_over(2, 1, &solid(0xFF00_00FF, 2, 2), 2, 2, 2);
        assert_eq!(d.px(2, 1), 0x0000_00FF);
        // Not into the scanline padding, and not off the bottom.
        assert_eq!(d.px(3, 1), 0, "wrote into the off-screen padding");
        assert_eq!(d.px(2, 0), 0);
    }

    #[test]
    fn a_composite_stops_at_the_end_of_a_truncated_aperture() {
        // Two bytes short of `pitch * height`, so the last pixel of the last
        // row starts inside the mapping and ends outside it.
        let d = truncated(4, 3, 4, 2);
        d.blit_argb_over(0, 0, &solid(0xFF00_00FF, 4, 3), 4, 4, 3);
        let m = d.mem.lock();
        assert_eq!(m.len(), 4 * 4 * 3 - 2);
        // The eleven pixels that fit are composited...
        for i in 0..11usize {
            let off = i * 4;
            assert_eq!(&m[off..off + 4], &[0xFF, 0x00, 0x00, 0x00], "pixel {}", i);
        }
        // ...and the two bytes of the twelfth that do fit are left alone.
        assert_eq!(&m[44..], &[0u8, 0], "wrote a pixel that does not fit");
    }

    #[test]
    fn a_composite_reads_no_further_than_the_source_it_was_given() {
        // A short source must not be read past its end: the row stride says
        // where a row starts, and the slice length is the only thing that says
        // where the pixels stop.
        let d = FakeDisplay::new(4, 4, 4);
        d.blit_argb_over(0, 0, &solid(0xFF00_00FF, 4, 1), 4, 4, 4);
        for x in 0..4 {
            assert_eq!(d.px(x, 0), 0x0000_00FF);
        }
        for y in 1..4 {
            for x in 0..4 {
                assert_eq!(d.px(x, y), 0, "({}, {}) came from past the source", x, y);
            }
        }
    }

    #[test]
    fn a_composite_onto_a_format_it_cannot_blend_writes_nothing() {
        // The blend reads and writes 32-bit words, so anything else is refused
        // rather than reinterpreted.
        let mut d = FakeDisplay::new(4, 2, 4);
        Arc::get_mut(&mut d).unwrap().info.format = ColorFormat::RGB565;
        let before = d.snapshot();
        d.blit_argb_over(0, 0, &solid(0xFF00_00FF, 4, 2), 4, 4, 2);
        assert_eq!(d.snapshot(), before);
        // And a source with no stride is not a source.
        let mut d = FakeDisplay::new(4, 2, 4);
        Arc::get_mut(&mut d).unwrap().info.format = ColorFormat::ARGB8888;
        let before = d.snapshot();
        d.blit_argb_over(0, 0, &solid(0xFF00_00FF, 4, 2), 0, 4, 2);
        assert_eq!(d.snapshot(), before);
    }

    #[test]
    fn the_composite_zeroes_the_alpha_byte_the_bulk_blit_preserves() {
        // Pinned, not endorsed. `blit_from` copies the source word whole, so a
        // wlroots frame lands with its alpha byte at 0xFF; `blit_argb_over`
        // builds its output from three channels and writes a fourth byte of
        // zero, so every pixel the cursor touches ends up with alpha 0. On an
        // XRGB scanout the byte is ignored and neither matters, which is why
        // this has never been noticed -- but the two primitives write the same
        // buffer and disagree about the same byte, and if a display plane is
        // ever configured to honour per-pixel alpha the difference is a
        // cursor-shaped hole. Changing it needs a real display to check
        // against, so for now the difference is written down here.
        let d = FakeDisplay::new(2, 1, 2);
        d.blit_from(0, 0, &[0xFF11_2233, 0xFF44_5566], 2, 2, 1);
        assert_eq!(d.px(0, 0) >> 24, 0xFF, "blit_from drops the source alpha");
        d.blit_argb_over(0, 0, &solid(0xFF00_00FF, 1, 1), 1, 1, 1);
        assert_eq!(
            d.px(0, 0) >> 24,
            0x00,
            "blit_argb_over no longer zeroes the alpha byte -- if that is on \
             purpose, `blit_from` and `fill_rect` need the same answer"
        );
        // The pixel the composite did not touch keeps its own alpha.
        assert_eq!(d.px(1, 0) >> 24, 0xFF);
    }

    // ---- the mirror ----

    /// Build a mapping without touching the process-wide flag, so the mirrored
    /// half of the primitives can be checked at all. `SCANOUT_MIRROR_X` is a
    /// boot-time opt-in that every test in this module reads, so a test that
    /// flipped it would depend on the execution order; this does not.
    fn xmap(mirror: bool, width: u32) -> XMap {
        XMap { mirror, width }
    }

    #[test]
    fn with_the_mirror_off_nothing_is_mapped_at_all() {
        // The default, and Moebius's machines: every primitive has to come out
        // byte-identical to no mapping at all.
        let m = xmap(false, 1920);
        for x in [0u32, 1, 959, 1919, 1920, u32::MAX] {
            assert_eq!(m.px(x), x);
        }
        assert_eq!(m.range(0, 1920), (0, 1920));
        assert_eq!(m.range(7, 9), (7, 9));
    }

    #[test]
    fn a_mirrored_pixel_is_its_distance_from_the_right_edge() {
        let m = xmap(true, 8);
        assert_eq!(m.px(0), 7);
        assert_eq!(m.px(7), 0);
        assert_eq!(m.px(3), 4);
        // Off the right edge saturates rather than wrapping to a huge column:
        // `0 - 1` as a `u32` is four billion, and four billion times four bytes
        // is where a blit would write if this were not saturating.
        assert_eq!(m.px(8), 0);
        assert_eq!(m.px(u32::MAX), 0);
    }

    #[test]
    fn a_mirrored_range_is_exactly_the_mirror_of_its_own_pixels() {
        // The invariant the five primitives silently shared and nothing
        // checked: `fill_rect` mirrors a half-open span in one step, the others
        // mirror pixel by pixel, and the two have to describe the same columns.
        // Reversing a half-open range moves BOTH ends, which is the part that
        // is easy to write as `width - left .. width - right` and get backwards.
        for width in 1..=24u32 {
            let m = xmap(true, width);
            for left in 0..=width {
                for right in left..=width {
                    let (ml, mr) = m.range(left, right);
                    let by_pixel: alloc::vec::Vec<u32> = (left..right).map(|x| m.px(x)).collect();
                    let by_range: alloc::vec::Vec<u32> = (ml..mr).collect();
                    let mut sorted = by_pixel.clone();
                    sorted.sort_unstable();
                    assert_eq!(
                        sorted, by_range,
                        "width {}, range {}..{} maps to {}..{} but its pixels are {:?}",
                        width, left, right, ml, mr, by_pixel
                    );
                }
            }
        }
    }

    #[test]
    fn a_mirrored_run_keeps_its_length_wherever_it_lands() {
        // `copy_rect` mirrors two origins of the same width and then copies
        // `w` pixels from each. If the mapping did not preserve the length the
        // two would address different amounts of pixels.
        let m = xmap(true, 16);
        for left in 0..16u32 {
            for w in 1..=(16 - left) {
                let (ml, mr) = m.range(left, left + w);
                assert_eq!(mr - ml, w, "run at {} of {} changed length", left, w);
                assert!(mr <= 16, "run at {} of {} ran off the screen", left, w);
            }
        }
    }

    #[test]
    fn mirroring_twice_is_not_mirroring() {
        // An involution: a primitive that applied the mapping to a coordinate
        // another primitive had already mapped would come out unmirrored, and
        // this is what says the two never compose.
        let m = xmap(true, 33);
        for x in 0..33u32 {
            assert_eq!(m.px(m.px(x)), x, "pixel {}", x);
        }
    }

    #[test]
    fn the_mapping_reads_the_flag_once_and_carries_it() {
        // Two mappings built from the same flag agree; a per-pixel read would
        // ask `width * height` times per fill and could see it change half way
        // through a frame.
        let a = XMap::of(64);
        let b = XMap::of(64);
        assert_eq!(a, b);
        assert_eq!(a.mirror, scanout_mirror_x());
        assert_eq!(a.width, 64);
    }

    // ---- draw_pixel ----

    #[test]
    fn a_pixel_outside_the_visible_area_is_not_drawn() {
        let d = FakeDisplay::new(4, 2, 6);
        let before = d.snapshot();
        d.draw_pixel(4, 0, RED);
        d.draw_pixel(0, 2, RED);
        d.draw_pixel(u32::MAX, u32::MAX, RED);
        assert_eq!(d.snapshot(), before);
    }

    #[test]
    fn a_pixel_is_only_drawn_when_the_whole_pixel_fits() {
        // The bound has to cover all the bytes the write touches, not just the
        // first: a pixel whose last byte is past the end of the mapping is a
        // write past the end of the mapping.
        assert!(pixel_fits(0, 4, 8));
        assert!(pixel_fits(4, 4, 8));
        assert!(!pixel_fits(5, 4, 8), "a pixel straddling the end fits?");
        assert!(!pixel_fits(8, 4, 8));
        assert!(!pixel_fits(usize::MAX, 4, 8), "no overflow, no wrap");
        // And the narrower formats get the bound their own width needs.
        assert!(pixel_fits(5, 3, 8));
        assert!(!pixel_fits(6, 3, 8));
    }

    #[test]
    fn a_pixel_that_straddles_the_end_of_the_aperture_is_not_drawn() {
        let d = truncated(4, 3, 4, 2);
        let before = d.snapshot();
        // The last pixel of the last row starts inside the mapping and ends
        // two bytes past it.
        d.draw_pixel(3, 2, RED);
        assert_eq!(d.snapshot(), before, "wrote past the end of the aperture");
        // The one before it fits and is drawn.
        d.draw_pixel(2, 2, RED);
        assert_ne!(d.snapshot(), before);
    }

    #[test]
    fn a_twenty_four_bit_pixel_writes_three_bytes_and_leaves_the_fourth() {
        // `write_color` used to build a four-byte slice for every format,
        // including the three-byte one, so the last pixel of an unpadded RGB888
        // scanline covered one byte past the mapping. Nothing is written there,
        // but the slice itself must not reach it either.
        let mut mem = alloc::vec![0xAAu8; 8];
        {
            let mut fb = FrameBuffer::from_slice(&mut mem);
            unsafe { fb.write_color(4, RgbColor(0x0011_2233), ColorFormat::RGB888) };
        }
        assert_eq!(&mem[4..7], &[0x33, 0x22, 0x11]);
        assert_eq!(
            mem[7], 0xAA,
            "the fourth byte of a 24-bit pixel was written"
        );
    }
}
