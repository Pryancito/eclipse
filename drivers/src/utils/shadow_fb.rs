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
            // 2. The previously drawn cursor, if it moved or is hidden.
            let erase = match g.prev_cursor {
                Some(prev) if Some(prev) != new_rect => {
                    Some((prev, Self::cell_pixels(&g.data, self.width, prev, false)))
                }
                _ => None,
            };
            // 3. The cursor (inverted) at its new position.
            let draw =
                new_rect.map(|rect| (rect, Self::cell_pixels(&g.data, self.width, rect, true)));
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

    /// Take the dirty rectangle, clamped to the screen, as `((x, y, w, h),
    /// pixels)` with the rows tightly packed (`w` pixels per row). `None` when
    /// nothing is dirty. Must be called with the shadow lock held.
    fn take_dirty(&self, g: &mut ShadowInner) -> Option<(DirtyRect, Vec<u32>)> {
        let (x0, y0, x1, y1) = g.dirty.take()?;
        let x0 = x0.min(self.width);
        let y0 = y0.min(self.height);
        let x1 = x1.min(self.width);
        let y1 = y1.min(self.height);
        if x0 >= x1 || y0 >= y1 {
            return None;
        }
        let (w, h) = (x1 - x0, y1 - y0);
        let mut pixels = Vec::with_capacity(w * h);
        for r in y0..y1 {
            let start = r * self.width + x0;
            pixels.extend_from_slice(&g.data[start..start + w]);
        }
        Some(((x0, y0, w, h), pixels))
    }

    /// Copy one character cell out of the shadow, optionally inverting it (for
    /// the cursor). `rect` is `(x, y, w, h)` in pixels; the result is tightly
    /// packed.
    fn cell_pixels(data: &[u32], width: usize, rect: DirtyRect, invert: bool) -> Vec<u32> {
        let (x, y, w, h) = rect;
        let mut out = Vec::with_capacity(w * h);
        for r in 0..h {
            let base = (y + r) * width + x;
            if invert {
                out.extend(data[base..base + w].iter().map(|px| px ^ 0x00FF_FFFF));
            } else {
                out.extend_from_slice(&data[base..base + w]);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    //! Host tests for the console's dirty-region tracking.
    //!
    //! Nothing here needs a GPU: the device side is a recorder that keeps
    //! every `(x, y, w, h)` it was handed together with the pixels. That is
    //! the contract worth pinning down, because both ways of breaking it are
    //! silent on a developer's QEMU and obvious on a real screen — a dirty
    //! rectangle that is too small leaves stale pixels behind (the "visual
    //! corruption" class), and one that is too large turns a one-cell cursor
    //! blink into a full-screen blit through the BAR aperture.

    use super::*;
    use crate::scheme::display::{ColorFormat, DisplayInfo, FrameBuffer};
    use crate::scheme::Scheme;
    use lock::Mutex as IrqMutex;

    /// A display that records what it was asked to blit instead of drawing.
    struct Recorder {
        info: DisplayInfo,
        blits: IrqMutex<Vec<(u32, u32, usize, u32, u32, Vec<u32>)>>,
        mem: IrqMutex<Vec<u8>>,
        flushes: IrqMutex<usize>,
    }

    impl Recorder {
        fn new(width: u32, height: u32) -> Arc<Self> {
            let size = (width * height * 4) as usize;
            Arc::new(Self {
                info: DisplayInfo {
                    width,
                    height,
                    pitch: width * 4,
                    format: ColorFormat::ARGB8888,
                    fb_base_vaddr: 0,
                    fb_size: size,
                },
                blits: IrqMutex::new(Vec::new()),
                mem: IrqMutex::new(alloc::vec![0u8; size]),
                flushes: IrqMutex::new(0),
            })
        }

        /// The recorded blits as `(x, y, w, h)`, oldest first.
        fn rects(&self) -> Vec<(u32, u32, u32, u32)> {
            self.blits
                .lock()
                .iter()
                .map(|(x, y, _, w, h, _)| (*x, *y, *w, *h))
                .collect()
        }

        /// The pixels of the `n`-th recorded blit.
        fn pixels(&self, n: usize) -> Vec<u32> {
            self.blits.lock()[n].5.clone()
        }

        fn clear_log(&self) {
            self.blits.lock().clear();
        }
    }

    impl Scheme for Recorder {
        fn name(&self) -> &str {
            "recorder"
        }
    }

    impl DisplayScheme for Recorder {
        fn info(&self) -> DisplayInfo {
            self.info
        }
        fn fb(&self) -> FrameBuffer<'_> {
            // SAFETY: the `Vec` is owned by `self` and outlives the view; the
            // real backends hand out a raw aperture pointer the same way.
            let mut m = self.mem.lock();
            unsafe { FrameBuffer::from_raw_parts_mut(m.as_mut_ptr(), m.len()) }
        }
        fn blit_from(
            &self,
            dst_x: u32,
            dst_y: u32,
            src: &[u32],
            src_stride: usize,
            width: u32,
            height: u32,
        ) {
            self.blits
                .lock()
                .push((dst_x, dst_y, src_stride, width, height, src.to_vec()));
        }
        fn need_flush(&self) -> bool {
            true
        }
        fn flush(&self) -> crate::DeviceResult {
            *self.flushes.lock() += 1;
            Ok(())
        }
    }

    #[test]
    fn a_clean_shadow_presents_nothing() {
        let fb = ShadowFramebuffer::new(64, 32);
        let dev = Recorder::new(64, 32);
        fb.present(dev.as_ref());
        assert!(dev.rects().is_empty());
        assert_eq!(*dev.flushes.lock(), 0);
    }

    #[test]
    fn put_pixels_marks_the_bounding_box_and_clips_offscreen_writes() {
        let fb = ShadowFramebuffer::new(64, 32);
        let dev = Recorder::new(64, 32);
        // Two far-apart pixels plus two that fall off the screen entirely.
        fb.put_pixels(
            vec![
                (2usize, 3usize, 0x11u32),
                (5, 7, 0x22),
                (64, 0, 0x33), // x == width
                (0, 32, 0x44), // y == height
            ]
            .into_iter(),
        );
        fb.present(dev.as_ref());
        // The union of (2,3) and (5,7), exclusive on the far edge. The two
        // out-of-range pixels must not have widened it.
        assert_eq!(dev.rects(), vec![(2, 3, 4, 5)]);
        let px = dev.pixels(0);
        assert_eq!(px.len(), 4 * 5);
        // Rows are tightly packed at `w` pixels, NOT at the screen width.
        assert_eq!(px[0], 0x11); // (2,3) is the rect's origin
        assert_eq!(px[4 * 4 + 3], 0x22); // (5,7) is its far corner
        assert_eq!(*dev.flushes.lock(), 1);
    }

    #[test]
    fn a_present_consumes_the_dirty_region() {
        let fb = ShadowFramebuffer::new(64, 32);
        let dev = Recorder::new(64, 32);
        fb.fill_rect(0, 0, 8, 8, 0xFF00_0000);
        fb.present(dev.as_ref());
        assert_eq!(dev.rects(), vec![(0, 0, 8, 8)]);
        dev.clear_log();
        // Nothing changed since, so the second present is a no-op.
        fb.present(dev.as_ref());
        assert!(dev.rects().is_empty());
    }

    #[test]
    fn fill_rect_clamps_to_the_screen_and_ignores_empty_rects() {
        let fb = ShadowFramebuffer::new(16, 8);
        let dev = Recorder::new(16, 8);
        // Straddles the right and bottom edges.
        fb.fill_rect(12, 6, 100, 100, 0xABCD);
        fb.present(dev.as_ref());
        assert_eq!(dev.rects(), vec![(12, 6, 4, 2)]);
        assert!(dev.pixels(0).iter().all(|p| *p == 0xABCD));

        dev.clear_log();
        // Wholly offscreen, and a zero-sized rect: neither dirties anything.
        fb.fill_rect(16, 0, 4, 4, 1);
        fb.fill_rect(0, 0, 0, 4, 1);
        fb.present(dev.as_ref());
        assert!(dev.rects().is_empty());
    }

    #[test]
    fn clear_dirties_the_whole_screen() {
        let fb = ShadowFramebuffer::new(16, 8);
        let dev = Recorder::new(16, 8);
        fb.clear(0x0000_00FF);
        fb.present(dev.as_ref());
        assert_eq!(dev.rects(), vec![(0, 0, 16, 8)]);
        assert_eq!(dev.pixels(0).len(), 16 * 8);
        assert!(dev.pixels(0).iter().all(|p| *p == 0x0000_00FF));
    }

    #[test]
    fn copy_rect_scrolls_up_and_down_with_memmove_semantics() {
        // One distinct value per row so an overlapping copy in the wrong
        // direction shows up as a smear.
        let fb = ShadowFramebuffer::new(4, 4);
        let dev = Recorder::new(4, 4);
        for y in 0..4usize {
            fb.fill_rect(0, y, 4, 1, (y as u32) + 1);
        }
        fb.present(dev.as_ref());
        dev.clear_log();

        // Scroll up by one row (dy < sy): rows 1..4 move to 0..3.
        fb.copy_rect(0, 1, 0, 0, 4, 3);
        fb.present(dev.as_ref());
        assert_eq!(dev.rects(), vec![(0, 0, 4, 3)]);
        let px = dev.pixels(0);
        assert_eq!(&px[0..4], &[2, 2, 2, 2]);
        assert_eq!(&px[4..8], &[3, 3, 3, 3]);
        assert_eq!(&px[8..12], &[4, 4, 4, 4]);

        // Scroll back down by one row (dy > sy), overlapping the other way.
        dev.clear_log();
        fb.copy_rect(0, 0, 0, 1, 4, 3);
        fb.present(dev.as_ref());
        assert_eq!(dev.rects(), vec![(0, 1, 4, 3)]);
        let px = dev.pixels(0);
        assert_eq!(&px[0..4], &[2, 2, 2, 2]);
        assert_eq!(&px[4..8], &[3, 3, 3, 3]);
        assert_eq!(&px[8..12], &[4, 4, 4, 4]);
    }

    #[test]
    fn copy_rect_clamps_a_rectangle_that_would_run_off_the_screen() {
        let fb = ShadowFramebuffer::new(8, 4);
        let dev = Recorder::new(8, 4);
        fb.clear(0);
        fb.present(dev.as_ref());
        dev.clear_log();
        // Asks for 8 columns starting at x=4: only 4 are available at either
        // end, so the copy is trimmed rather than wrapping into the next row.
        fb.copy_rect(4, 0, 0, 0, 8, 10);
        fb.present(dev.as_ref());
        assert_eq!(dev.rects(), vec![(0, 0, 4, 4)]);
        // A copy with nothing left after clamping dirties nothing at all.
        dev.clear_log();
        fb.copy_rect(8, 0, 0, 0, 4, 4);
        fb.present(dev.as_ref());
        assert!(dev.rects().is_empty());
    }

    #[test]
    fn the_cursor_is_drawn_inverted_and_erased_when_it_moves() {
        let fb = ShadowFramebuffer::new(16, 8);
        let dev = Recorder::new(16, 8);
        fb.clear(0x0000_0000);
        // First present: content, then the cursor cell at (0, 0).
        fb.present_with_cursor(dev.as_ref(), Some((0, 0)), 8, 8);
        let rects = dev.rects();
        assert_eq!(rects, vec![(0, 0, 16, 8), (0, 0, 8, 8)]);
        // The cursor cell is the shadow's pixels XOR-ed with 0x00FF_FFFF.
        assert!(dev.pixels(1).iter().all(|p| *p == 0x00FF_FFFF));

        // Redrawing the SAME cell with a clean shadow is idempotent: one blit,
        // no erase, and it must look identical (the inversion is never stored
        // in the shadow, so it cannot compound).
        dev.clear_log();
        fb.present_with_cursor(dev.as_ref(), Some((0, 0)), 8, 8);
        assert_eq!(dev.rects(), vec![(0, 0, 8, 8)]);
        assert!(dev.pixels(0).iter().all(|p| *p == 0x00FF_FFFF));

        // Moving it erases the old cell (restored from the clean shadow) and
        // draws the new one.
        dev.clear_log();
        fb.present_with_cursor(dev.as_ref(), Some((1, 0)), 8, 8);
        assert_eq!(dev.rects(), vec![(0, 0, 8, 8), (8, 0, 8, 8)]);
        assert!(dev.pixels(0).iter().all(|p| *p == 0x0000_0000)); // erase
        assert!(dev.pixels(1).iter().all(|p| *p == 0x00FF_FFFF)); // draw

        // Hiding it erases the last cell and draws nothing.
        dev.clear_log();
        fb.present_with_cursor(dev.as_ref(), None, 8, 8);
        assert_eq!(dev.rects(), vec![(8, 0, 8, 8)]);
        assert!(dev.pixels(0).iter().all(|p| *p == 0x0000_0000));
    }

    #[test]
    fn a_cursor_cell_past_the_right_edge_is_dropped_not_clamped() {
        let fb = ShadowFramebuffer::new(16, 8);
        let dev = Recorder::new(16, 8);
        fb.clear(0);
        dev.clear_log();
        // Column 2 of an 8px cell starts at x=16, i.e. exactly off-screen.
        fb.present_with_cursor(dev.as_ref(), Some((2, 0)), 8, 8);
        // Only the content rect, no cursor blit.
        assert_eq!(dev.rects(), vec![(0, 0, 16, 8)]);
    }
}
