//! A double-buffered ("shadow") framebuffer for the graphic console.
//!
//! All console drawing happens into a CPU-side ARGB8888 buffer kept in normal,
//! cached RAM, which is cheap to both read and write. Each present then pushes
//! the **whole** shadow to the real display / GPU framebuffer via
//! [`DisplayScheme::blit_from`] followed by a single
//! [`DisplayScheme::flush`].
//!
//! Partial clips on the WC GOP/BAR1 scanout smeared squares and lines into
//! neighboring pixels (the same class of artifact that made KMS DIRTYFB and
//! Wayland sub-rect damage unusable). There is no scene dirty-rect tracker:
//! a present is always a full-frame blit. A boolean skip flag still avoids
//! rewriting the aperture when nothing changed.
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
use lock::Mutex;

use crate::scheme::DisplayScheme;

/// Inclusive-exclusive pixel rectangle: `[x, y, w, h)`.
type CellRect = (usize, usize, usize, usize);

struct ShadowInner {
    /// ARGB8888 pixels, row-major, `width` pixels per row.
    data: Vec<u32>,
    /// Content changed since the last present.
    dirty: bool,
    /// Pixel rectangle `(x, y, w, h)` of the inverted text cursor last sent
    /// to the device, so a no-op present (same content, same cursor) can skip.
    prev_cursor: Option<CellRect>,
}

/// A CPU-side shadow of the display framebuffer.
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
}

impl ShadowFramebuffer {
    /// Create a black shadow buffer of `width` x `height` pixels.
    pub fn new(width: usize, height: usize) -> Arc<Self> {
        Arc::new(Self {
            width,
            height,
            inner: Mutex::new(ShadowInner {
                data: vec![0; width.saturating_mul(height)],
                dirty: false,
                prev_cursor: None,
            }),
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

    /// Write a batch of `(x, y, argb)` pixels (used by the glyph renderer).
    ///
    /// Taking an iterator lets a whole glyph be rendered under a single lock.
    pub fn put_pixels(&self, pixels: impl Iterator<Item = (usize, usize, u32)>) {
        let (w, h) = (self.width, self.height);
        let mut g = self.inner.lock();
        for (x, y, argb) in pixels {
            if x >= w || y >= h {
                continue;
            }
            g.data[y * w + x] = argb;
            g.dirty = true;
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
        let mut g = self.inner.lock();
        for yy in y..y1 {
            for px in &mut g.data[yy * width + x..yy * width + x1] {
                *px = argb;
            }
        }
        g.dirty = true;
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
        let mut g = self.inner.lock();
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
        g.dirty = true;
    }

    /// Clear the whole shadow buffer to `argb` and mark it for present.
    pub fn clear(&self, argb: u32) {
        let mut g = self.inner.lock();
        for px in g.data.iter_mut() {
            *px = argb;
        }
        g.dirty = true;
    }

    /// Push the whole shadow to the real display and flush it.
    ///
    /// Does nothing when nothing changed since the last present. One
    /// [`DisplayScheme::blit_from`] of the full buffer, then a single
    /// [`DisplayScheme::flush`] for devices that need it (virtio-gpu).
    pub fn present(&self, display: &dyn DisplayScheme) {
        let mut g = self.inner.lock();
        if !g.dirty && g.prev_cursor.is_none() {
            return;
        }
        g.dirty = false;
        g.prev_cursor = None;
        display.blit_from(
            0,
            0,
            &g.data,
            self.width,
            self.width as u32,
            self.height as u32,
        );
        drop(g);
        if display.need_flush() {
            let _ = display.flush();
        }
    }

    /// Present the full shadow and overlay a blinking text cursor.
    ///
    /// `cursor` is the cell to highlight `(col, row)` or `None` to hide it;
    /// `cw`/`ch` are the character cell size in pixels. The cursor is inverted
    /// in the shadow, the **whole** frame is blitted once (no WC sub-rect),
    /// then the invert is undone so the shadow stays clean. A present with
    /// unchanged content and the same cursor cell is a no-op.
    pub fn present_with_cursor(
        &self,
        display: &dyn DisplayScheme,
        cursor: Option<(usize, usize)>,
        cw: usize,
        ch: usize,
    ) {
        let mut g = self.inner.lock();

        let new_rect = cursor.and_then(|(cx, cy)| {
            let x = (cx * cw).min(self.width);
            let y = (cy * ch).min(self.height);
            let w = cw.min(self.width.saturating_sub(x));
            let h = ch.min(self.height.saturating_sub(y));
            if w == 0 || h == 0 {
                None
            } else {
                Some((x, y, w, h))
            }
        });

        if !g.dirty && g.prev_cursor == new_rect {
            return;
        }

        if let Some(rect) = new_rect {
            Self::invert_cell(&mut g.data, self.width, rect);
        }
        display.blit_from(
            0,
            0,
            &g.data,
            self.width,
            self.width as u32,
            self.height as u32,
        );
        if let Some(rect) = new_rect {
            Self::invert_cell(&mut g.data, self.width, rect);
        }
        g.dirty = false;
        g.prev_cursor = new_rect;

        drop(g);
        if display.need_flush() {
            let _ = display.flush();
        }
    }

    fn invert_cell(data: &mut [u32], width: usize, rect: CellRect) {
        let (x, y, w, h) = rect;
        for r in 0..h {
            let base = (y + r).saturating_mul(width).saturating_add(x);
            for c in 0..w {
                let i = base + c;
                if i < data.len() {
                    data[i] ^= 0x00FF_FFFF;
                }
            }
        }
    }
}
