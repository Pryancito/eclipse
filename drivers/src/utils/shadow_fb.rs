//! A double-buffered ("shadow") framebuffer for the graphic console.
//!
//! All console drawing happens into a CPU-side ARGB8888 buffer kept in normal,
//! cached RAM, which is cheap to both read and write. Dirty regions are then
//! pushed to the real display / GPU framebuffer in bulk via
//! [`DisplayScheme::blit_from`] followed by a single
//! [`DisplayScheme::flush`].
//!
//! Console dirty-rects live here (cached RAM → GOP). KMS `DIRTYFB` also
//! honours clip rects, but expands them to 64-byte write-combining lines
//! so a partial BAR1 store cannot smear neighbouring pixels.
//!
//! This avoids the two patterns that make a naive framebuffer console crawl on
//! real hardware:
//!  * per-pixel MMIO writes through the PCI BAR aperture, and
//!  * reading back VRAM during console scrolling (uncached/write-combining GPU
//!    memory is extremely slow to read).
//!
//! The same abstraction serves both backends equally: an NVIDIA GPU receives
//! the bulk blit straight into its BAR-mapped VRAM, while a virtio-gpu device
//! receives it into its host-shared framebuffer and the trailing `flush`
//! triggers the host transfer.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use lock::{HeldByCurrentCpu, Mutex};

use crate::scheme::DisplayScheme;

/// Inclusive-exclusive dirty bounding box in pixels: `[x0, y0, x1, y1)`.
type DirtyRect = (usize, usize, usize, usize);

struct ShadowInner {
    /// ARGB8888 pixels, row-major, `width` pixels per row.
    data: Vec<u32>,
    /// Smallest rectangle covering everything changed since the last present,
    /// or `None` when the shadow and the real framebuffer are in sync.
    dirty: Option<DirtyRect>,
    /// Pixel rectangle `(x, y, w, h)` where the inverted text cursor was last
    /// drawn directly to the device, so it can be erased on the next present.
    prev_cursor: Option<DirtyRect>,
}

/// A CPU-side shadow of the display framebuffer with dirty-region tracking.
///
/// Interior mutability lets the glyph renderer (which only has shared access
/// through the `DrawTarget`) and the console scroll/fill paths share one
/// buffer. Concurrency is not a concern in practice — the whole graphic console
/// is already serialized behind a single lock — but the internal [`Mutex`]
/// keeps the type `Send + Sync` and the accesses sound.
pub struct ShadowFramebuffer {
    width: usize,
    height: usize,
    inner: Mutex<ShadowInner>,
    /// One presenter at a time, so snapshots reach the device in order. A
    /// plain (non IRQ-disabling) spinlock that is only ever `try_lock`ed:
    /// held across the device blit, never spun on -- see [`Self::present`].
    blit_lock: spin::Mutex<()>,
}

impl ShadowFramebuffer {
    /// Create a black shadow buffer of `width` x `height` pixels.
    pub fn new(width: usize, height: usize) -> Arc<Self> {
        Arc::new(Self {
            width,
            height,
            inner: Mutex::new(ShadowInner {
                data: vec![0; width.saturating_mul(height)],
                dirty: None,
                prev_cursor: None,
            }),
            blit_lock: spin::Mutex::new(()),
        })
    }

    /// Width in pixels.
    #[inline]
    pub fn width(&self) -> usize {
        self.width
    }

    /// Height in pixels.
    #[inline]
    pub fn height(&self) -> usize {
        self.height
    }

    /// The shadow lock, or `None` when **this CPU already holds it**.
    ///
    /// Every drawing entry point goes through here instead of `inner.lock()`.
    /// The lock is an IRQ-disabling ticket mutex, so no interrupt can re-enter
    /// it — but a *fault or panic taken mid-draw* can, and does: the panic
    /// handler prints to the graphic console, and instantiating or clearing a
    /// VT comes straight back in here. A ticket mutex is not re-entrant, so
    /// that second acquire waits, with interrupts off, for a release only this
    /// CPU could perform. The detector caught it exactly:
    ///
    ///     cpu=1 at drivers/src/utils/shadow_fb.rs:161   <- clear, waiting
    ///     HOLDER cpu=1 at drivers/src/utils/shadow_fb.rs:102 <- put_pixels, holding
    ///
    /// one CPU, both ends, while three other cores were busy panicking. Two of
    /// those panics were contained and the system would have survived; this is
    /// what killed it.
    ///
    /// Declining costs some console pixels on a path that is already printing a
    /// crash. Blocking costs the machine, and the crash report with it.
    #[inline]
    fn lock_inner(&self) -> Option<lock::MutexGuard<'_, ShadowInner>> {
        if self.inner.held_by_current_cpu() {
            return None;
        }
        Some(self.inner.lock())
    }

    #[inline]
    fn mark(inner: &mut ShadowInner, x0: usize, y0: usize, x1: usize, y1: usize) {
        inner.dirty = Some(match inner.dirty {
            Some((ax0, ay0, ax1, ay1)) => (ax0.min(x0), ay0.min(y0), ax1.max(x1), ay1.max(y1)),
            None => (x0, y0, x1, y1),
        });
    }

    /// Write a batch of `(x, y, argb)` pixels (used by the glyph renderer).
    ///
    /// Taking an iterator lets a whole glyph be rendered under a single lock.
    pub fn put_pixels(&self, pixels: impl Iterator<Item = (usize, usize, u32)>) {
        let (w, h) = (self.width, self.height);
        let Some(mut g) = self.lock_inner() else {
            return;
        };
        for (x, y, argb) in pixels {
            if x >= w || y >= h {
                continue;
            }
            g.data[y * w + x] = argb;
            Self::mark(&mut g, x, y, x + 1, y + 1);
        }
    }

    /// Fill a rectangle (pixel coordinates) with a single ARGB8888 color.
    pub fn fill_rect(&self, x: usize, y: usize, w: usize, h: usize, argb: u32) {
        let x1 = x.saturating_add(w).min(self.width);
        let y1 = y.saturating_add(h).min(self.height);
        if x >= x1 || y >= y1 {
            return;
        }
        let width = self.width;
        let Some(mut g) = self.lock_inner() else {
            return;
        };
        for yy in y..y1 {
            for px in &mut g.data[yy * width + x..yy * width + x1] {
                *px = argb;
            }
        }
        Self::mark(&mut g, x, y, x1, y1);
    }

    /// Copy a rectangle within the shadow buffer (`memmove` semantics), used for
    /// fast console scrolling entirely in cached RAM.
    pub fn copy_rect(&self, sx: usize, sy: usize, dx: usize, dy: usize, w: usize, h: usize) {
        let w = w
            .min(self.width.saturating_sub(sx))
            .min(self.width.saturating_sub(dx));
        let h = h
            .min(self.height.saturating_sub(sy))
            .min(self.height.saturating_sub(dy));
        if w == 0 || h == 0 {
            return;
        }
        let width = self.width;
        let Some(mut g) = self.lock_inner() else {
            return;
        };
        if dy <= sy {
            for r in 0..h {
                let s = (sy + r) * width + sx;
                let d = (dy + r) * width + dx;
                g.data.copy_within(s..s + w, d);
            }
        } else {
            for r in (0..h).rev() {
                let s = (sy + r) * width + sx;
                let d = (dy + r) * width + dx;
                g.data.copy_within(s..s + w, d);
            }
        }
        Self::mark(&mut g, dx, dy, dx + w, dy + h);
    }

    /// Clear the whole shadow buffer to `argb` and mark it fully dirty.
    pub fn clear(&self, argb: u32) {
        let Some(mut g) = self.lock_inner() else {
            return;
        };
        for px in g.data.iter_mut() {
            *px = argb;
        }
        g.dirty = Some((0, 0, self.width, self.height));
    }

    /// Push the dirty region to the real display and flush it.
    ///
    /// Does nothing when nothing changed since the last present. The dirty
    /// sub-rectangle is copied out of the shadow (cached RAM, cheap) under the
    /// shadow lock and then blitted with the lock RELEASED. The shadow lock is
    /// an IRQ-disabling spinlock, and the blit is the slow part: a full-height
    /// console scroll pushes ~8 MB through the GOP/BAR aperture, which real
    /// hardware serves at tens of MB/s -- holding interrupts off for the
    /// whole copy (as this did) stalled the timer, keyboard and xHCI for
    /// hundreds of milliseconds per scroll and made other CPUs spin IRQ-off on
    /// the same lock. A single [`DisplayScheme::flush`] follows for devices
    /// that need it (virtio-gpu).
    ///
    /// Presents are serialized by [`Self::blit_lock`] so two snapshots can
    /// never reach the device out of order; a presenter that finds it taken
    /// leaves its dirty rectangle in place for the next present instead of
    /// spinning (the timer-tick cursor blink runs in IRQ context on the CPU
    /// that may be mid-blit, so it must never wait).
    pub fn present(&self, display: &dyn DisplayScheme) {
        let Some(_blit) = self.blit_lock.try_lock() else {
            return;
        };
        let snap = {
            let Some(mut g) = self.lock_inner() else {
                return;
            };
            self.take_dirty(&mut g)
        };
        let Some(((x, y, w, h), pixels)) = snap else {
            return;
        };
        display.blit_from(x as u32, y as u32, &pixels, w, w as u32, h as u32);
        if display.need_flush() {
            let _ = display.flush();
        }
    }

    /// Present the dirty region and overlay a blinking text cursor.
    ///
    /// `cursor` is the cell to highlight `(col, row)` or `None` to hide it;
    /// `cw`/`ch` are the character cell size in pixels. The cursor is drawn as
    /// an inverted block straight to the device (never stored in the shadow, so
    /// it can be erased cleanly) by reading the cell's pixels from the shadow,
    /// XOR-ing them and blitting them back. Because it is always rebuilt from
    /// the clean shadow, redrawing the same cell is idempotent. Same locking
    /// rules as [`Self::present`]: everything is snapshotted under the shadow
    /// lock, every device write happens with it released.
    pub fn present_with_cursor(
        &self,
        display: &dyn DisplayScheme,
        cursor: Option<(usize, usize)>,
        cw: usize,
        ch: usize,
    ) {
        let Some(_blit) = self.blit_lock.try_lock() else {
            return;
        };

        // Pixel rectangle of the requested cursor cell, clamped to the screen.
        let new_rect = cursor.and_then(|(cx, cy)| {
            let x = (cx * cw).min(self.width);
            let y = (cy * ch).min(self.height);
            let w = cw.min(self.width - x);
            let h = ch.min(self.height - y);
            if w == 0 || h == 0 {
                None
            } else {
                Some((x, y, w, h))
            }
        });

        let (dirty, erase, draw) = {
            let Some(mut g) = self.lock_inner() else {
                return;
            };
            // 1. The dirty content region.
            let dirty = self.take_dirty(&mut g);
            // Widen a cell blit to whole write-combining lines, keeping any
            // inversion on the cell's own columns.
            let cell_blit = |rect: DirtyRect, invert: bool| {
                let (x, y, w, h) = rect;
                let (x0, x1) = Self::wc_expand_x(x, x + w, self.width);
                let window = if invert { x..x + w } else { 0..0 };
                let wide = (x0, y, x1 - x0, h);
                (wide, Self::cell_pixels(&g.data, self.width, wide, window))
            };
            // 2. The previously drawn cursor, if it moved or is hidden.
            let erase = match g.prev_cursor {
                Some(prev) if Some(prev) != new_rect => Some(cell_blit(prev, false)),
                _ => None,
            };
            // 3. The cursor (inverted) at its new position.
            let draw = new_rect.map(|rect| cell_blit(rect, true));
            g.prev_cursor = new_rect;
            (dirty, erase, draw)
        };

        for ((x, y, w, h), pixels) in dirty.into_iter().chain(erase).chain(draw) {
            display.blit_from(x as u32, y as u32, &pixels, w, w as u32, h as u32);
        }
        if display.need_flush() {
            let _ = display.flush();
        }
    }

    /// Widen `[x0, x1)` to whole write-combining lines: 16 XRGB8888 pixels are
    /// one 64-byte PCIe burst, and a blit whose left or right edge sits
    /// mid-line makes the aperture flush a half-full combine buffer over the
    /// neighbouring pixels -- leftover squares and stripes.
    ///
    /// The console needs this more than anything else on the machine and had no
    /// equivalent at all (the DRM present path has `expand_x_for_wc`). A text
    /// cell is 9 px = 36 bytes wide, so column *c* starts at byte `36 * c`,
    /// which is a multiple of 64 only every sixteenth column: virtually every
    /// console blit degenerated into scalar head and tail stores in
    /// `nt_store_row`. Rounding the left edge down fixes that unconditionally.
    ///
    /// `limit` is the shadow's own width, since unlike the DRM path there is no
    /// off-screen padding here to park a right-edge tail in -- so the right edge
    /// only reaches a boundary when the screen width is itself a multiple of 16.
    /// Widening never loses pixels: the extra columns are read from the same
    /// clean shadow and are identical to what is already on screen.
    fn wc_expand_x(x0: usize, x1: usize, limit: usize) -> (usize, usize) {
        const WC_PX: usize = 16;
        if x0 >= x1 || limit == 0 {
            return (x0, x1);
        }
        let lo = x0 - (x0 % WC_PX);
        let hi = x1.div_ceil(WC_PX).saturating_mul(WC_PX).min(limit);
        (lo, hi.max(x1))
    }

    /// Take the dirty rectangle, clamped to the screen and widened to whole
    /// write-combining lines, as `((x, y, w, h), pixels)` with the rows tightly
    /// packed (`w` pixels per row). `None` when nothing is dirty. Must be called
    /// with the shadow lock held.
    fn take_dirty(&self, g: &mut ShadowInner) -> Option<(DirtyRect, Vec<u32>)> {
        let (x0, y0, x1, y1) = g.dirty.take()?;
        let x0 = x0.min(self.width);
        let y0 = y0.min(self.height);
        let x1 = x1.min(self.width);
        let y1 = y1.min(self.height);
        if x0 >= x1 || y0 >= y1 {
            return None;
        }
        let (x0, x1) = Self::wc_expand_x(x0, x1, self.width);
        let (w, h) = (x1 - x0, y1 - y0);
        let mut pixels = Vec::with_capacity(w * h);
        for r in y0..y1 {
            let start = r * self.width + x0;
            pixels.extend_from_slice(&g.data[start..start + w]);
        }
        Some(((x0, y0, w, h), pixels))
    }

    /// Copy a strip of the shadow, inverting only the columns in `invert` (an
    /// absolute x range, empty for none). `rect` is `(x, y, w, h)` in pixels;
    /// the result is tightly packed.
    ///
    /// The invert window is separate from the rect because the blit is widened
    /// to whole write-combining lines (see [`Self::wc_expand_x`]) while the
    /// inversion must stay on the cursor's own cell -- otherwise the blink would
    /// flip up to 15 neighbouring columns with it.
    fn cell_pixels(
        data: &[u32],
        width: usize,
        rect: DirtyRect,
        invert: core::ops::Range<usize>,
    ) -> Vec<u32> {
        let (x, y, w, h) = rect;
        let mut out = Vec::with_capacity(w * h);
        for r in 0..h {
            let base = (y + r) * width + x;
            out.extend(data[base..base + w].iter().enumerate().map(|(c, px)| {
                if invert.contains(&(x + c)) {
                    px ^ 0x00FF_FFFF
                } else {
                    *px
                }
            }));
        }
        out
    }
}

/// Tests for the shadow framebuffer's dirty-rectangle bookkeeping, and in
/// particular for the write-combining line alignment of what it hands the
/// display. The console is the heaviest user of the framebuffer aperture on a
/// text boot, and it had no alignment of any kind.
#[cfg(test)]
mod wc_dirty_tests {
    use super::*;
    use crate::scheme::display::{ColorFormat, DisplayInfo, FrameBuffer};
    use crate::scheme::Scheme;

    /// A display that records the `(dst_x, dst_y, width, height)` of every blit
    /// instead of drawing: what the console asks for IS the thing under test.
    struct RecordingDisplay {
        info: DisplayInfo,
        mem: Mutex<alloc::vec::Vec<u8>>,
        blits: Mutex<alloc::vec::Vec<(u32, u32, u32, u32)>>,
    }

    impl RecordingDisplay {
        fn new(width: u32, height: u32) -> Self {
            let size = (width * height * 4) as usize;
            Self {
                info: DisplayInfo {
                    width,
                    height,
                    pitch: width * 4,
                    format: ColorFormat::ARGB8888,
                    fb_base_vaddr: 0,
                    fb_size: size,
                },
                mem: Mutex::new(alloc::vec![0u8; size]),
                blits: Mutex::new(alloc::vec::Vec::new()),
            }
        }
        fn taken(&self) -> alloc::vec::Vec<(u32, u32, u32, u32)> {
            core::mem::take(&mut *self.blits.lock())
        }
    }

    impl Scheme for RecordingDisplay {
        fn name(&self) -> &str {
            "recording-display"
        }
    }

    impl DisplayScheme for RecordingDisplay {
        fn info(&self) -> DisplayInfo {
            self.info
        }
        fn fb(&self) -> FrameBuffer<'_> {
            let mut m = self.mem.lock();
            // SAFETY: the buffer is owned by `self` and outlives the view.
            unsafe { FrameBuffer::from_raw_parts_mut(m.as_mut_ptr(), m.len()) }
        }
        fn fb_write_combining(&self) -> bool {
            true
        }
        fn blit_from(
            &self,
            dst_x: u32,
            dst_y: u32,
            _src: &[u32],
            _src_stride: usize,
            width: u32,
            height: u32,
        ) {
            self.blits.lock().push((dst_x, dst_y, width, height));
        }
    }

    /// 16 XRGB8888 pixels are one 64-byte write-combining burst, so both edges
    /// have to sit on a 16-pixel boundary or the aperture flushes a half-full
    /// combine buffer over the neighbouring pixels.
    #[test]
    fn a_span_is_widened_to_whole_write_combining_lines() {
        // A single 9-px text cell in column 4: bytes 144..180, neither end on a
        // 64-byte boundary. Becomes pixels 32..48, i.e. bytes 128..192.
        assert_eq!(ShadowFramebuffer::wc_expand_x(36, 45, 1920), (32, 48));
        // Already aligned spans are left exactly as they are.
        assert_eq!(ShadowFramebuffer::wc_expand_x(0, 16, 1920), (0, 16));
        assert_eq!(ShadowFramebuffer::wc_expand_x(64, 128, 1920), (64, 128));
        // Empty and degenerate spans are not widened into existence.
        assert_eq!(ShadowFramebuffer::wc_expand_x(10, 10, 1920), (10, 10));
        assert_eq!(ShadowFramebuffer::wc_expand_x(0, 8, 0), (0, 8));
    }

    /// Unlike the DRM present path there is no off-screen padding here to park a
    /// right-edge tail in, so the right edge stops at the shadow's own width --
    /// and must never be pulled BELOW what was asked for, which would drop
    /// pixels the console just drew.
    #[test]
    fn widening_never_drops_a_requested_pixel() {
        for limit in [64usize, 720, 1366, 1920] {
            for x0 in 0..limit {
                for len in [1usize, 5, 9, 16, 33] {
                    let x1 = (x0 + len).min(limit);
                    if x0 >= x1 {
                        continue;
                    }
                    let (lo, hi) = ShadowFramebuffer::wc_expand_x(x0, x1, limit);
                    assert!(lo <= x0, "left edge moved right: {} > {}", lo, x0);
                    assert!(hi >= x1, "right edge moved left: {} < {}", hi, x1);
                    assert!(hi <= limit, "ran past the shadow: {} > {}", hi, limit);
                    assert_eq!(lo % 16, 0, "left edge {} is not on a WC line", lo);
                }
            }
        }
    }

    /// The regression this guards, end to end: a one-cell update used to be
    /// blitted at its raw pixel offset, so `nt_store_row` degenerated into
    /// scalar head and tail stores for all but 4 of every 64 bytes. Now the
    /// console asks for a 16-pixel-aligned span.
    #[test]
    fn a_one_cell_console_update_is_blitted_on_a_wc_boundary() {
        let d = RecordingDisplay::new(1920, 64);
        let shadow = ShadowFramebuffer::new(1920, 64);
        // Column 4 of a 9x18 cell grid: x = 36..45.
        shadow.fill_rect(36, 0, 9, 18, 0x00FF_FFFF);
        shadow.present(&d);

        let blits = d.taken();
        assert_eq!(blits.len(), 1, "one dirty region, one blit");
        let (x, _y, w, _h) = blits[0];
        assert_eq!(x % 16, 0, "blit starts mid-WC-line at x={}", x);
        assert_eq!((x + w) % 16, 0, "blit ends mid-WC-line at x={}", x + w);
        // And it still covers the cell it was asked to draw.
        assert!(
            x <= 36 && x + w >= 45,
            "cell 36..45 not covered by {}..{}",
            x,
            x + w
        );
    }

    /// The blinking cursor is its own blit, at cell granularity, running at
    /// 2 Hz forever -- so it needs the same alignment. But the INVERSION must
    /// stay on the cursor's own cell: widening the blit to a WC line must not
    /// flip up to 15 neighbouring columns with it.
    #[test]
    fn the_cursor_blit_is_aligned_but_only_its_own_cell_is_inverted() {
        let d = RecordingDisplay::new(1920, 64);
        let shadow = ShadowFramebuffer::new(1920, 64);
        shadow.clear(0x0000_0000);
        shadow.present(&d);
        let _ = d.taken();

        // Cursor at column 4, row 0 of a 9x18 grid.
        shadow.present_with_cursor(&d, Some((4, 0)), 9, 18);
        let blits = d.taken();
        assert!(!blits.is_empty(), "the cursor must be drawn");
        for (x, _, w, _) in &blits {
            assert_eq!(x % 16, 0, "cursor blit starts mid-WC-line at x={}", x);
            assert_eq!(
                (x + w) % 16,
                0,
                "cursor blit ends mid-WC-line at x={}",
                x + w
            );
        }

        // The inversion window: cells 36..45 flip, their WC-line neighbours do
        // not. Check against the pixels the shadow would hand over.
        let rect = (32usize, 0usize, 16usize, 18usize);
        let data = alloc::vec![0x0000_0000u32; 1920 * 64];
        let pixels = ShadowFramebuffer::cell_pixels(&data, 1920, rect, 36..45);
        for (i, px) in pixels.iter().take(16).enumerate() {
            let abs = 32 + i;
            if (36..45).contains(&abs) {
                assert_eq!(*px, 0x00FF_FFFF, "column {} should be inverted", abs);
            } else {
                assert_eq!(*px, 0, "column {} must NOT be inverted", abs);
            }
        }
    }
}
