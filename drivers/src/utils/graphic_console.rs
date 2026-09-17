use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::Infallible;
use core::ops::{Deref, DerefMut};

use rcore_console::embedded_graphics::prelude::RgbColor as _;
use rcore_console::{
    Cell, Console, DrawTarget, Flags, OriginDimensions, Pixel, Rgb888, Size, TextBuffer,
    TextOnGraphic,
};

use super::shadow_fb::ShadowFramebuffer;
use crate::scheme::display::DisplayScheme;

/// Height in pixels of one text row (matches `rcore_console`'s `FONT_9X18`).
const CHAR_HEIGHT: usize = 18;
/// Width in pixels of one character cell (matches `rcore_console`'s `FONT_9X18`).
const CHAR_WIDTH: usize = 9;

/// Convert an `rcore_console` glyph color to a packed `0x00RRGGBB` value, matching
/// the byte layout previously written straight to the ARGB8888 framebuffer.
#[inline]
fn rgb888_to_argb(color: Rgb888) -> u32 {
    ((color.r() as u32) << 16) | ((color.g() as u32) << 8) | (color.b() as u32)
}

/// A `DrawTarget` that renders into a CPU-side [`ShadowFramebuffer`] instead of
/// writing pixels straight to GPU memory. Glyph rendering therefore touches only
/// cached RAM; the dirty region is later pushed to the device in bulk.
pub struct ShadowDraw {
    shadow: Arc<ShadowFramebuffer>,
    width: u32,
    height: u32,
}

impl DrawTarget for ShadowDraw {
    type Color = Rgb888;
    type Error = Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        self.shadow.put_pixels(pixels.into_iter().filter_map(|p| {
            let (x, y) = (p.0.x, p.0.y);
            if x < 0 || y < 0 {
                return None;
            }
            Some((x as usize, y as usize, rgb888_to_argb(p.1)))
        }));
        Ok(())
    }
}

impl OriginDimensions for ShadowDraw {
    fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }
}

/// Set once the kernel is dying, so the console stops trusting its own cache.
///
/// Three boots in a row ended as a panic INSIDE the panic handler, each time
/// on this path: `rust_begin_unwind` -> the console -> a `buf` that the fault
/// being reported had already corrupted. Bounds checks fixed the panics
/// (#1219, #1220) but not the premise — `ensure_dims` then *resized* a `Vec`
/// whose pointer was garbage, and wrote through it:
///
///     [KERNEL PAGE FAULT] vaddr=0xffffff00002184fd flags=WRITE
///         rip=<LinearScrollbackBuffer::ensure_dims+0x3f0>
///
/// A console cannot defend against its own memory being scribbled. So once a
/// panic starts it stops trying: no resizing, no repainting from the cache,
/// and every cell goes straight to the glyph renderer. The report reaches the
/// screen through the one path that needs nothing but the framebuffer.
static PANICKING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Tell the graphic console a panic is in progress. See [`PANICKING`].
pub fn note_panicking() {
    PANICKING.store(true, core::sync::atomic::Ordering::Relaxed);
}

#[inline]
fn panicking() -> bool {
    PANICKING.load(core::sync::atomic::Ordering::Relaxed)
}

pub struct LinearScrollbackBuffer {
    buf: Vec<Vec<Cell>>,
    history: VecDeque<Vec<Cell>>,
    scrollback_offset: Option<usize>,
    inner: TextOnGraphic<ShadowDraw>,
    shadow: Arc<ShadowFramebuffer>,
    display: Arc<dyn DisplayScheme>,
    /// Best-effort text cursor position (cell coords) for the block cursor.
    /// Tracks where the next character will be drawn.
    cursor_row: usize,
    cursor_col: usize,
}

impl LinearScrollbackBuffer {
    pub fn new(display: Arc<dyn DisplayScheme>) -> Self {
        let info = display.info();
        let shadow = ShadowFramebuffer::new(info.width as usize, info.height as usize);
        let draw = ShadowDraw {
            shadow: shadow.clone(),
            width: info.width,
            height: info.height,
        };
        let inner = TextOnGraphic::new(draw, info.width, info.height);
        let width = inner.width();
        let height = inner.height();
        Self {
            buf: vec![vec![Cell::default(); width]; height],
            history: VecDeque::new(),
            scrollback_offset: None,
            inner,
            shadow,
            display,
            cursor_row: 0,
            cursor_col: 0,
        }
    }

    /// Push the dirty region of the shadow buffer to the real display, drawing
    /// the text cursor when `visible`.
    ///
    /// Called once per batch of writes (per `write_str` / scroll) so a whole
    /// line of output becomes a single bulk transfer to the GPU. The cursor is
    /// hidden while viewing scrollback history.
    pub fn present(&self, visible: bool) {
        let cursor = if visible && self.scrollback_offset.is_none() {
            Some((self.cursor_col, self.cursor_row))
        } else {
            None
        };
        self.shadow
            .present_with_cursor(&*self.display, cursor, CHAR_WIDTH, CHAR_HEIGHT);
    }

    /// Set the text cursor position (cell coords) used to draw the block cursor.
    ///
    /// Driven from the owning [`Console`]'s authoritative cursor so the block
    /// cursor follows `goto`/`move_*` escape sequences, not just the last cell
    /// written (which is where full-screen editors like nano leave it).
    pub fn set_cursor(&mut self, row: usize, col: usize) {
        self.cursor_row = row;
        self.cursor_col = col;
    }

    /// Repaint the whole screen from the backing buffer into the shadow.
    ///
    /// Used when this VT becomes the active one (a different VT or a graphics
    /// client may have left arbitrary pixels on screen), so the shadow is first
    /// cleared to black and then every cell is redrawn.
    pub fn repaint_all(&mut self) {
        self.shadow.clear(0x0000_0000);
        self.redraw();
    }

    pub fn scroll_history(&mut self, direction: i32) {
        let height = self.height();
        let scroll_amount = (height as i32 - 2).max(1);
        let delta = direction * scroll_amount;
        let history_len = self.history.len();

        if delta > 0 {
            // Scroll up (back in history)
            let current_offset = self.scrollback_offset.unwrap_or(0);
            let new_offset = (current_offset + delta as usize).min(history_len);
            if new_offset > 0 {
                self.scrollback_offset = Some(new_offset);
            }
        } else if delta < 0 {
            // Scroll down (forward in history)
            if let Some(current_offset) = self.scrollback_offset {
                let steps = (-delta) as usize;
                if current_offset <= steps {
                    self.scrollback_offset = None;
                } else {
                    self.scrollback_offset = Some(current_offset - steps);
                }
            }
        }

        self.redraw();
    }

    /// Make `buf` match the display before anything indexes it.
    ///
    /// `width()` and `height()` come from the display (`self.inner`); `buf` is
    /// a separately-sized `Vec<Vec<Cell>>`. Nothing keeps the two in step, and
    /// every raw `self.buf[r][c]` in this file is bounded by the display's
    /// dimensions — so any disagreement is a panic, in the one place where a
    /// panic is unsurvivable: the panic handler prints THROUGH here
    /// (`rust_begin_unwind` -> `graphic_console_write_fmt_spin` ->
    /// rcore-console -> `TextBuffer`). Two of them landed in successive boots,
    /// at lines 244 and 205, and both times the KERNEL STOP screen came up with
    /// a banner and nothing under it: the report naming the original fault was
    /// lost to a panic inside the panic handler.
    ///
    /// Resizing is better than dropping the write: a console that repairs
    /// itself keeps printing the crash report, which is the entire reason this
    /// path exists. Reports once so a silent mismatch cannot hide.
    fn ensure_dims(&mut self) {
        if panicking() {
            // Resizing means writing through `buf`'s pointer, and the fault
            // being reported may be exactly what corrupted it. `write` draws
            // straight to the glyphs when a cell is missing, so standing down
            // costs the cache, not the report.
            return;
        }
        let (h, w) = (self.inner.height(), self.inner.width());
        if self.buf.len() == h && self.buf.iter().all(|r| r.len() == w) {
            return;
        }
        Self::report_dims_mismatch(
            self.buf.len(),
            h,
            self.buf.first().map_or(0, |r| r.len()),
            w,
        );
        self.buf.resize_with(h, || vec![Cell::default(); w]);
        for row in self.buf.iter_mut() {
            row.resize(w, Cell::default());
        }
    }

    #[cold]
    #[inline(never)]
    fn report_dims_mismatch(rows: usize, h: usize, cols: usize, w: usize) {
        use core::sync::atomic::{AtomicBool, Ordering};
        static REPORTED: AtomicBool = AtomicBool::new(false);
        if REPORTED.swap(true, Ordering::Relaxed) {
            return;
        }
        log::warn!(
            "[gcon] scrollback buffer is {}x{} but the display is {}x{} — \
             resizing. Every raw index in this file is bounded by the display, \
             so a mismatch would panic inside the panic handler.",
            rows,
            cols,
            h,
            w,
        );
    }

    pub fn redraw(&mut self) {
        if panicking() {
            // A repaint replays the whole cell cache, which is the least
            // trustworthy thing in the kernel at this moment. The panic text
            // is being written cell by cell anyway.
            return;
        }
        self.ensure_dims();
        let height = self.height();
        let width = self.width();

        if let Some(offset) = self.scrollback_offset {
            let history_len = self.history.len();
            for r in 0..height {
                let index = (history_len as isize)
                    - (offset as isize)
                    - ((height as isize) - 1 - (r as isize));
                if index < 0 {
                    let bg_cell = Cell::default();
                    for col in 0..width {
                        self.inner.write(r, col, bg_cell);
                    }
                } else if index < history_len as isize {
                    let blank = Cell::default();
                    let line = self.history.get(index as usize);
                    for col in 0..width {
                        let cell = line.and_then(|l| l.get(col)).copied().unwrap_or(blank);
                        self.inner.write(r, col, cell);
                    }
                } else {
                    let active_row = (index - history_len as isize) as usize;
                    if let Some(line) = self.buf.get(active_row).filter(|_| active_row < height) {
                        for col in 0..width {
                            let cell = line.get(col).copied().unwrap_or_default();
                            self.inner.write(r, col, cell);
                        }
                    } else {
                        let bg_cell = Cell::default();
                        for col in 0..width {
                            self.inner.write(r, col, bg_cell);
                        }
                    }
                }
            }
        } else {
            for r in 0..height {
                let line = self.buf.get(r);
                for col in 0..width {
                    let cell = line.and_then(|l| l.get(col)).copied().unwrap_or_default();
                    self.inner.write(r, col, cell);
                }
            }
        }
    }
}

impl TextBuffer for LinearScrollbackBuffer {
    #[inline]
    fn width(&self) -> usize {
        self.inner.width()
    }

    #[inline]
    fn height(&self) -> usize {
        self.inner.height()
    }

    #[inline]
    fn read(&self, row: usize, col: usize) -> Cell {
        // `&self`, so it cannot repair the way `ensure_dims` does; answer with
        // a blank rather than panic on the console's own print path.
        self.buf
            .get(row)
            .and_then(|r| r.get(col))
            .copied()
            .unwrap_or_default()
    }

    #[inline]
    fn write(&mut self, row: usize, col: usize, cell: Cell) {
        self.ensure_dims();
        let height = self.height();
        let width = self.width();
        if row >= height || col >= width {
            return;
        }
        // Skip the glyph rasterization when the cell already holds this exact
        // value: full-screen TUI repaints (htop, vim) rewrite >90 % identical
        // cells per refresh, and each `inner.write` is a per-pixel
        // embedded-graphics draw. `buf` always mirrors the last value pushed
        // to the pixels (including transient cursor-inversion writes), so
        // equality here guarantees the pixels are already correct. Cursor
        // tracking below still runs — position advances even over unchanged
        // cells.
        //
        // Bound by the BUFFER's own dimensions, not the console's. `width()`
        // and `height()` come from the display (`self.inner`), while `buf` is
        // resized separately, so the two can disagree — and when they do, this
        // line panics:
        //
        //   panic at drivers/src/utils/graphic_console.rs:244:38
        //   index out of bounds: the len is 0 but the index is 0
        //
        // That is fatal in a way an ordinary bounds check is not, because the
        // panic handler prints THROUGH this path: `rust_begin_unwind` ->
        // `graphic_console_write_fmt_spin` -> rcore-console -> here. A panic
        // here is therefore a panic inside the panic handler, which is exactly
        // why the KERNEL STOP screen came up blank — the report that would have
        // named the original fault never got rendered.
        //
        // Dropping the cell is the right failure: the console is mid-resize or
        // not yet built, and losing a character beats losing the crash report.
        let unchanged = match self.buf.get_mut(row).and_then(|r| r.get_mut(col)) {
            Some(slot) => {
                let same = *slot == cell;
                *slot = cell;
                same
            }
            // No cache slot (buffer short, or standing down mid-panic): draw
            // unconditionally rather than drop the character. Returning here
            // would have silenced the crash report, which is the one thing
            // this path exists to deliver.
            None => false,
        };

        if self.scrollback_offset.is_none() {
            if !unchanged {
                self.inner.write(row, col, cell);
            }
            // Track the cursor as the position just after the written cell.
            self.cursor_row = row;
            self.cursor_col = col + 1;
            if self.cursor_col >= width {
                self.cursor_col = 0;
                self.cursor_row = (self.cursor_row + 1).min(height.saturating_sub(1));
            }
        }
    }

    fn new_line(&mut self, cell: Cell) {
        self.ensure_dims();
        let height = self.height();
        let width = self.width();
        if height == 0 {
            return;
        }

        // 1+2. Rotate the rows instead of cloning each one: `rotate_left`
        // moves row 0 (the row scrolling into history) to the end as pure
        // pointer swaps, where the per-row `clone()` loop this replaces did
        // ~`height` heap alloc + memcpy + free pairs per newline. The old top
        // row's content goes to history; the row allocation recycled from a
        // retired history line (once scrollback is at capacity) becomes the
        // fresh bottom row, so a steady scroll allocates nothing at all.
        let bg_cell = Cell {
            c: ' ',
            bg: cell.bg,
            fg: cell.fg,
            flags: Flags::empty(),
        };
        self.buf.rotate_left(1);
        let mut new_bottom = if self.history.len() >= 1000 {
            self.history.pop_front().unwrap_or_default()
        } else {
            Vec::with_capacity(width)
        };
        new_bottom.clear();
        new_bottom.resize(width, bg_cell);
        let old_top = core::mem::replace(&mut self.buf[height - 1], new_bottom);
        self.history.push_back(old_top);

        // 3. Handle scrollback offset and scrolling
        if let Some(offset) = self.scrollback_offset {
            let max_offset = self.history.len();
            self.scrollback_offset = Some((offset + 1).min(max_offset));
            self.redraw();
        } else {
            // Scroll the shadow buffer up by one text row, entirely in cached
            // RAM — no read-back from GPU memory.
            let width_px = self.shadow.width();
            let text_h = height * CHAR_HEIGHT;
            if text_h > CHAR_HEIGHT {
                self.shadow
                    .copy_rect(0, CHAR_HEIGHT, 0, 0, width_px, text_h - CHAR_HEIGHT);
            }
            let bg_argb = rgb888_to_argb(cell.bg.to_rgb());
            self.shadow.fill_rect(
                0,
                (height - 1) * CHAR_HEIGHT,
                width_px,
                CHAR_HEIGHT,
                bg_argb,
            );
        }
        // After a scroll the next character lands at the bottom-left.
        self.cursor_row = height - 1;
        self.cursor_col = 0;
    }

    fn clear(&mut self, cell: Cell) {
        let width = self.width();
        let height = self.height();
        let bg_cell = Cell {
            c: ' ',
            bg: cell.bg,
            fg: cell.fg,
            flags: Flags::empty(),
        };
        self.buf = vec![vec![bg_cell; width]; height];
        self.history.clear();
        self.scrollback_offset = None;

        let bg_argb = rgb888_to_argb(cell.bg.to_rgb());
        self.shadow.clear(bg_argb);
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    /// Scroll a sub-region up by `n` lines, moving the surviving pixel band in
    /// one bulk `copy_rect` (cached RAM) instead of re-rendering every glyph —
    /// the fast path for full-screen TUIs (irssi/htop) that scroll a window.
    fn scroll_region_up(&mut self, top: usize, bottom: usize, n: usize, blank: Cell) {
        self.ensure_dims();
        let height = self.height();
        let width = self.width();
        if top > bottom || bottom >= height || n == 0 {
            return;
        }
        let n = n.min(bottom - top + 1);
        let blank_cell = Cell {
            c: ' ',
            bg: blank.bg,
            fg: blank.fg,
            flags: Flags::empty(),
        };
        // Shift the backing cells up within the region.
        for r in top..=bottom {
            self.buf[r] = if r + n <= bottom {
                self.buf[r + n].clone()
            } else {
                vec![blank_cell; width]
            };
        }
        if self.scrollback_offset.is_some() {
            self.redraw();
            return;
        }
        // Move the surviving pixels up, then clear the vacated band.
        let width_px = self.shadow.width();
        let survivors = (bottom - top + 1) - n;
        if survivors > 0 {
            self.shadow.copy_rect(
                0,
                (top + n) * CHAR_HEIGHT,
                0,
                top * CHAR_HEIGHT,
                width_px,
                survivors * CHAR_HEIGHT,
            );
        }
        let bg_argb = rgb888_to_argb(blank.bg.to_rgb());
        self.shadow.fill_rect(
            0,
            (bottom + 1 - n) * CHAR_HEIGHT,
            width_px,
            n * CHAR_HEIGHT,
            bg_argb,
        );
    }

    /// Scroll a sub-region down by `n` lines (bulk pixel copy + clear the top).
    fn scroll_region_down(&mut self, top: usize, bottom: usize, n: usize, blank: Cell) {
        self.ensure_dims();
        let height = self.height();
        let width = self.width();
        if top > bottom || bottom >= height || n == 0 {
            return;
        }
        let n = n.min(bottom - top + 1);
        let blank_cell = Cell {
            c: ' ',
            bg: blank.bg,
            fg: blank.fg,
            flags: Flags::empty(),
        };
        // Shift the backing cells down within the region (high rows first).
        for r in (top..=bottom).rev() {
            self.buf[r] = if r >= top + n {
                self.buf[r - n].clone()
            } else {
                vec![blank_cell; width]
            };
        }
        if self.scrollback_offset.is_some() {
            self.redraw();
            return;
        }
        let width_px = self.shadow.width();
        let survivors = (bottom - top + 1) - n;
        if survivors > 0 {
            self.shadow.copy_rect(
                0,
                top * CHAR_HEIGHT,
                0,
                (top + n) * CHAR_HEIGHT,
                width_px,
                survivors * CHAR_HEIGHT,
            );
        }
        let bg_argb = rgb888_to_argb(blank.bg.to_rgb());
        self.shadow
            .fill_rect(0, top * CHAR_HEIGHT, width_px, n * CHAR_HEIGHT, bg_argb);
    }
}

pub struct GraphicConsole {
    inner: Console<LinearScrollbackBuffer>,
}

impl GraphicConsole {
    pub fn new(display: Arc<dyn DisplayScheme>) -> Self {
        Self {
            inner: Console::on_text_buffer(LinearScrollbackBuffer::new(display)),
        }
    }

    /// Flush all pending console output to the display, showing the cursor.
    ///
    /// Drawing accumulates in the shadow buffer; this pushes the dirty region to
    /// the GPU in one bulk transfer. Call it after a batch of writes.
    pub fn present(&mut self) {
        let (row, col) = self.inner.cursor();
        // Honor DECTCEM (`?25`): a hidden cursor (full-screen TUIs while
        // repainting) is never drawn.
        let visible = self.inner.cursor_visible();
        let buf = self.inner.buf_mut();
        buf.set_cursor(row, col);
        buf.present(visible);
    }

    /// Redraw only the blinking cursor with the given visibility.
    ///
    /// Called from the timer tick (~2 Hz) so the cursor blinks while idle,
    /// without touching the text content. A cursor the application has hidden
    /// (`?25l`) must never blink back into view.
    pub fn set_cursor_blink(&mut self, visible: bool) {
        let (row, col) = self.inner.cursor();
        let visible = visible && self.inner.cursor_visible();
        let buf = self.inner.buf_mut();
        buf.set_cursor(row, col);
        buf.present(visible);
    }

    /// Repaint the entire screen from the backing buffer (e.g. on VT switch).
    pub fn repaint(&mut self) {
        self.inner.buf_mut().repaint_all();
        self.present();
    }
}

impl Deref for GraphicConsole {
    type Target = Console<LinearScrollbackBuffer>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for GraphicConsole {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
