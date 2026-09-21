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
                // Scrollback reads `history ++ buf` as one stream, and `offset`
                // is how many lines back the TOP row is: at `offset` the top
                // row shows stream line `history_len - offset`, so at the cap
                // `scroll_history` uses (`history.len()`) the oldest line sits
                // on row 0, and at offset 0 the screen is the live buffer.
                //
                // This used to anchor the BOTTOM row instead
                // (`history_len - offset - (height - 1 - r)`), which shifts the
                // whole view back by a further `height - 1` lines: one press of
                // Shift+PgUp jumped almost two screens, and scrolling all the
                // way up left a blank screen with the oldest line alone on the
                // bottom row.
                let index = (history_len as isize) + (r as isize) - (offset as isize);
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

#[cfg(test)]
mod scrollback_tests {
    //! Host tests for the graphic console's cell cache and scrollback.
    //!
    //! Everything here is off-by-one country: `new_line` rotates the rows and
    //! moves a pixel band, `scroll_region_up`/`_down` do the same for a
    //! sub-region, and `redraw` maps a scrollback position onto
    //! `history ++ buf`. A one-row error in any of them is invisible to a
    //! compiler and obvious to whoever is reading the screen.
    //!
    //! Rows are tagged by their **background colour** (`Color::Indexed`), which
    //! survives into the history, into the pixels and back out of a
    //! `present()`, so one helper answers "which line is being shown on row r"
    //! for both the cache and the screen.

    use super::*;
    use crate::scheme::display::{ColorFormat, DisplayInfo, FrameBuffer};
    use crate::scheme::Scheme;
    use lock::Mutex;
    use rcore_console::Color;

    /// A display whose aperture is a heap buffer, sized in character cells so
    /// the console's `width()`/`height()` come out exactly as asked.
    struct FakeDisplay {
        info: DisplayInfo,
        mem: Mutex<Vec<u8>>,
    }

    impl FakeDisplay {
        fn new(cols: usize, rows: usize) -> Arc<Self> {
            let width = (cols * CHAR_WIDTH) as u32;
            let height = (rows * CHAR_HEIGHT) as u32;
            let pitch = width * 4;
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
                mem: Mutex::new(vec![0u8; size]),
            })
        }

        fn px(&self, x: usize, y: usize) -> u32 {
            let off = y * self.info.pitch as usize + x * 4;
            let m = self.mem.lock();
            u32::from_ne_bytes([m[off], m[off + 1], m[off + 2], m[off + 3]]) & 0x00ff_ffff
        }

        fn poke(&self, x: usize, y: usize, argb: u32) {
            let off = y * self.info.pitch as usize + x * 4;
            let mut m = self.mem.lock();
            m[off..off + 4].copy_from_slice(&argb.to_ne_bytes());
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
            // SAFETY: the `Vec` is owned by `self` and outlives the view; the
            // real backends hand out a raw aperture pointer the same way.
            let mut m = self.mem.lock();
            unsafe { FrameBuffer::from_raw_parts_mut(m.as_mut_ptr(), m.len()) }
        }
    }

    fn console(cols: usize, rows: usize) -> (LinearScrollbackBuffer, Arc<FakeDisplay>) {
        let d = FakeDisplay::new(cols, rows);
        let lsb = LinearScrollbackBuffer::new(d.clone() as Arc<dyn DisplayScheme>);
        assert_eq!((lsb.width(), lsb.height()), (cols, rows));
        (lsb, d)
    }

    /// A blank cell tagged by its background. Indices 17.. are the 6x6x6 colour
    /// cube, so every tag is a different pixel value (index 16 is black, which
    /// is also the default background — keep off it).
    fn tagged(tag: u8) -> Cell {
        Cell {
            c: ' ',
            fg: Cell::default().fg,
            bg: Color::Indexed(tag),
            flags: Flags::empty(),
        }
    }

    fn tag_argb(tag: u8) -> u32 {
        rgb888_to_argb(Color::Indexed(tag).to_rgb())
    }

    fn blank_argb() -> u32 {
        rgb888_to_argb(Cell::default().bg.to_rgb())
    }

    /// Push the shadow's dirty region to the fake display.
    fn flush(lsb: &LinearScrollbackBuffer) {
        lsb.shadow.present(&*lsb.display);
    }

    /// The colour actually on screen for text row `r`, sampled inside the
    /// character cell (a blank glyph is background all the way across).
    fn on_screen(d: &FakeDisplay, r: usize) -> u32 {
        d.px(3, r * CHAR_HEIGHT + 8)
    }

    fn screen(d: &FakeDisplay, rows: usize) -> Vec<u32> {
        (0..rows).map(|r| on_screen(d, r)).collect()
    }

    /// The tag cached for row `r` (all columns are painted alike).
    fn cached(lsb: &LinearScrollbackBuffer, r: usize) -> u32 {
        rgb888_to_argb(lsb.read(r, 0).bg.to_rgb())
    }

    fn cached_rows(lsb: &LinearScrollbackBuffer, rows: usize) -> Vec<u32> {
        (0..rows).map(|r| cached(lsb, r)).collect()
    }

    fn paint_row(lsb: &mut LinearScrollbackBuffer, row: usize, tag: u8) {
        let cell = tagged(tag);
        for c in 0..lsb.width() {
            lsb.write(row, c, cell);
        }
    }

    /// Write a tagged line on the bottom row and scroll it up, the way a
    /// program printing a line of output does.
    fn feed(lsb: &mut LinearScrollbackBuffer, tag: u8) {
        let bottom = lsb.height() - 1;
        paint_row(lsb, bottom, tag);
        lsb.new_line(Cell::default());
    }

    // ---------------------------------------------------------------- write

    #[test]
    fn a_written_cell_reaches_both_the_cache_and_the_pixels() {
        let (mut lsb, d) = console(4, 3);
        paint_row(&mut lsb, 1, 20);
        flush(&lsb);
        assert_eq!(lsb.read(1, 0).bg, Color::Indexed(20));
        assert_eq!(on_screen(&d, 1), tag_argb(20));
        // Neighbouring rows were not touched.
        assert_eq!(on_screen(&d, 0), blank_argb());
        assert_eq!(on_screen(&d, 2), blank_argb());
    }

    #[test]
    fn a_write_outside_the_screen_is_dropped_rather_than_panicking() {
        let (mut lsb, d) = console(4, 3);
        // The panic handler prints through this path, so an out-of-range cell
        // has to be dropped, not asserted on.
        lsb.write(3, 0, tagged(20));
        lsb.write(0, 4, tagged(20));
        lsb.write(usize::MAX, usize::MAX, tagged(20));
        flush(&lsb);
        assert_eq!(screen(&d, 3), vec![blank_argb(); 3]);
        // And `read` answers blank rather than panicking, since it cannot
        // repair the way `write` can.
        assert_eq!(lsb.read(99, 99), Cell::default());
    }

    #[test]
    fn rewriting_an_identical_cell_skips_the_glyph_but_still_moves_the_cursor() {
        let (mut lsb, d) = console(4, 3);
        lsb.write(0, 0, tagged(20));
        flush(&lsb);
        assert_eq!(on_screen(&d, 0), tag_argb(20));

        // Scribble on the display behind the console's back, then write the
        // very same cell: the cache says the pixels are already right, so
        // nothing is redrawn and the scribble survives.
        d.poke(3, 8, 0x00_1234);
        lsb.write(0, 0, tagged(20));
        flush(&lsb);
        assert_eq!(on_screen(&d, 0), 0x00_1234, "an unchanged cell was redrawn");
        // The cursor advanced anyway — position must not depend on the cache.
        assert_eq!((lsb.cursor_row, lsb.cursor_col), (0, 1));

        // A different cell does redraw.
        lsb.write(0, 0, tagged(21));
        flush(&lsb);
        assert_eq!(on_screen(&d, 0), tag_argb(21));
    }

    #[test]
    fn the_cursor_wraps_at_the_right_edge_and_stops_on_the_last_row() {
        let (mut lsb, _d) = console(4, 3);
        lsb.write(0, 2, tagged(20));
        assert_eq!((lsb.cursor_row, lsb.cursor_col), (0, 3));
        // Past the last column the cursor moves to the start of the next row.
        lsb.write(0, 3, tagged(20));
        assert_eq!((lsb.cursor_row, lsb.cursor_col), (1, 0));
        // On the last row it stays put instead of running off the screen.
        lsb.write(2, 3, tagged(20));
        assert_eq!((lsb.cursor_row, lsb.cursor_col), (2, 0));
    }

    #[test]
    fn a_write_while_scrolled_back_updates_the_cache_but_not_the_screen() {
        let (mut lsb, d) = console(4, 3);
        feed(&mut lsb, 20);
        flush(&lsb);
        lsb.scrollback_offset = Some(1);
        let before = screen(&d, 3);

        paint_row(&mut lsb, 0, 21);
        flush(&lsb);
        // The history view the user is reading must not be overwritten by a
        // background program's output...
        assert_eq!(screen(&d, 3), before);
        // ...but the cell is remembered, so returning to the live screen shows
        // it.
        assert_eq!(lsb.read(0, 0).bg, Color::Indexed(21));
        // And the cursor did not move behind the user's back either.
        assert_eq!((lsb.cursor_row, lsb.cursor_col), (2, 0));
    }

    // ------------------------------------------------------------- new_line

    #[test]
    fn new_line_pushes_the_top_row_into_history_and_blanks_the_bottom() {
        let (mut lsb, _d) = console(4, 3);
        paint_row(&mut lsb, 0, 20);
        paint_row(&mut lsb, 1, 21);
        paint_row(&mut lsb, 2, 22);

        lsb.new_line(Cell::default());

        assert_eq!(
            cached_rows(&lsb, 3),
            vec![tag_argb(21), tag_argb(22), blank_argb()]
        );
        assert_eq!(lsb.history.len(), 1);
        assert_eq!(lsb.history[0][0].bg, Color::Indexed(20));
        assert_eq!((lsb.cursor_row, lsb.cursor_col), (2, 0));
    }

    #[test]
    fn new_line_moves_the_pixels_up_by_exactly_one_row() {
        let (mut lsb, d) = console(4, 4);
        for (r, tag) in [20u8, 21, 22, 23].iter().enumerate() {
            paint_row(&mut lsb, r, *tag);
        }
        flush(&lsb);
        assert_eq!(
            screen(&d, 4),
            vec![tag_argb(20), tag_argb(21), tag_argb(22), tag_argb(23)]
        );

        lsb.new_line(tagged(30));
        flush(&lsb);
        // Rows 1..3 slid into 0..2 and the vacated band took the new
        // background — one row of pixels, not two and not none.
        assert_eq!(
            screen(&d, 4),
            vec![tag_argb(21), tag_argb(22), tag_argb(23), tag_argb(30)]
        );
        // The very last pixel line of the screen is part of the blanked band.
        assert_eq!(d.px(3, 4 * CHAR_HEIGHT - 1), tag_argb(30));
        // And the last line of the row above is not.
        assert_eq!(d.px(3, 3 * CHAR_HEIGHT - 1), tag_argb(23));
    }

    #[test]
    fn the_new_bottom_row_is_blank_in_the_requested_colour() {
        let (mut lsb, _d) = console(4, 3);
        // A program that set a background colour keeps it on the line it
        // scrolls in, but never inherits the old line's characters.
        lsb.new_line(Cell {
            c: 'X',
            bg: Color::Indexed(30),
            fg: Cell::default().fg,
            flags: Flags::INVERSE,
        });
        for c in 0..lsb.width() {
            let cell = lsb.read(2, c);
            assert_eq!(cell.c, ' ');
            assert_eq!(cell.bg, Color::Indexed(30));
            assert_eq!(cell.flags, Flags::empty());
        }
    }

    #[test]
    fn history_stops_growing_at_its_cap_and_the_recycled_row_is_clean() {
        let (mut lsb, _d) = console(4, 3);
        paint_row(&mut lsb, 0, 20);
        paint_row(&mut lsb, 1, 21);
        paint_row(&mut lsb, 2, 22);
        for _ in 0..1100 {
            lsb.new_line(Cell::default());
            assert!(lsb.history.len() <= 1000, "scrollback grew past its cap");
        }
        assert_eq!(lsb.history.len(), 1000);
        // The first lines are long gone.
        assert!(lsb
            .history
            .iter()
            .all(|line| line[0].bg != Color::Indexed(20)));

        // Past the cap every new bottom row reuses a retired allocation. It
        // must come back the right width and fully repainted, not carrying the
        // cells of the line it used to be.
        lsb.new_line(tagged(31));
        assert_eq!(lsb.buf[2].len(), 4);
        for c in 0..4 {
            assert_eq!(lsb.read(2, c).bg, Color::Indexed(31));
            assert_eq!(lsb.read(2, c).c, ' ');
        }
    }

    #[test]
    fn a_console_with_no_rows_survives_a_new_line() {
        // A display whose mode is not set yet reports zero height; the panic
        // handler may still print through here.
        let (mut lsb, _d) = console(4, 0);
        lsb.new_line(Cell::default());
        lsb.clear(Cell::default());
        lsb.write(0, 0, tagged(20));
        lsb.scroll_region_up(0, 0, 1, Cell::default());
        lsb.scroll_region_down(0, 0, 1, Cell::default());
        assert_eq!(lsb.height(), 0);
    }

    // ---------------------------------------------------------------- clear

    #[test]
    fn clear_wipes_the_screen_the_history_and_the_scrollback_position() {
        let (mut lsb, d) = console(4, 3);
        for tag in 20..26u8 {
            feed(&mut lsb, tag);
        }
        lsb.scrollback_offset = Some(2);
        assert!(!lsb.history.is_empty());

        lsb.clear(tagged(30));
        flush(&lsb);

        assert!(lsb.history.is_empty(), "clear left scrollback behind");
        assert_eq!(lsb.scrollback_offset, None);
        assert_eq!(cached_rows(&lsb, 3), vec![tag_argb(30); 3]);
        assert_eq!(screen(&d, 3), vec![tag_argb(30); 3]);
        assert_eq!((lsb.cursor_row, lsb.cursor_col), (0, 0));
    }

    // ------------------------------------------------------- scroll regions

    #[test]
    fn scroll_region_up_shifts_the_region_and_leaves_the_rest_alone() {
        let (mut lsb, d) = console(4, 6);
        for (r, tag) in (20u8..26).enumerate() {
            paint_row(&mut lsb, r, tag);
        }
        flush(&lsb);

        // Scroll rows 1..=4 up by one; rows 0 and 5 are outside the region.
        lsb.scroll_region_up(1, 4, 1, tagged(30));
        flush(&lsb);

        let want = vec![
            tag_argb(20), // untouched, above the region
            tag_argb(22),
            tag_argb(23),
            tag_argb(24),
            tag_argb(30), // the vacated row
            tag_argb(25), // untouched, below the region
        ];
        assert_eq!(cached_rows(&lsb, 6), want, "cells");
        assert_eq!(screen(&d, 6), want, "pixels");
    }

    #[test]
    fn scroll_region_up_blanks_exactly_the_vacated_band() {
        let (mut lsb, d) = console(4, 6);
        for (r, tag) in (20u8..26).enumerate() {
            paint_row(&mut lsb, r, tag);
        }
        flush(&lsb);

        lsb.scroll_region_up(1, 4, 2, tagged(30));
        flush(&lsb);

        assert_eq!(
            screen(&d, 6),
            vec![
                tag_argb(20),
                tag_argb(23),
                tag_argb(24),
                tag_argb(30),
                tag_argb(30),
                tag_argb(25),
            ]
        );
        // The band edges, to the pixel: row 2 ends where row 3 begins.
        assert_eq!(d.px(3, 3 * CHAR_HEIGHT - 1), tag_argb(24));
        assert_eq!(d.px(3, 3 * CHAR_HEIGHT), tag_argb(30));
        assert_eq!(d.px(3, 5 * CHAR_HEIGHT - 1), tag_argb(30));
        assert_eq!(d.px(3, 5 * CHAR_HEIGHT), tag_argb(25));
    }

    #[test]
    fn scrolling_a_region_by_more_than_its_height_just_clears_it() {
        let (mut lsb, d) = console(4, 5);
        for (r, tag) in (20u8..25).enumerate() {
            paint_row(&mut lsb, r, tag);
        }
        flush(&lsb);

        lsb.scroll_region_up(1, 3, 99, tagged(30));
        flush(&lsb);

        let want = vec![
            tag_argb(20),
            tag_argb(30),
            tag_argb(30),
            tag_argb(30),
            tag_argb(24),
        ];
        assert_eq!(cached_rows(&lsb, 5), want, "cells");
        assert_eq!(screen(&d, 5), want, "pixels");
    }

    #[test]
    fn a_degenerate_scroll_region_is_a_no_op() {
        let (mut lsb, d) = console(4, 4);
        for (r, tag) in (20u8..24).enumerate() {
            paint_row(&mut lsb, r, tag);
        }
        flush(&lsb);
        let before = screen(&d, 4);

        for (top, bottom, n) in [(2usize, 1usize, 1usize), (0, 4, 1), (1, 2, 0), (0, 99, 1)] {
            lsb.scroll_region_up(top, bottom, n, tagged(30));
            lsb.scroll_region_down(top, bottom, n, tagged(30));
        }
        flush(&lsb);

        assert_eq!(screen(&d, 4), before);
        assert_eq!(
            cached_rows(&lsb, 4),
            vec![tag_argb(20), tag_argb(21), tag_argb(22), tag_argb(23)]
        );
    }

    #[test]
    fn scroll_region_down_shifts_the_region_and_leaves_the_rest_alone() {
        let (mut lsb, d) = console(4, 6);
        for (r, tag) in (20u8..26).enumerate() {
            paint_row(&mut lsb, r, tag);
        }
        flush(&lsb);

        lsb.scroll_region_down(1, 4, 1, tagged(30));
        flush(&lsb);

        let want = vec![
            tag_argb(20),
            tag_argb(30), // the vacated row, at the top of the region
            tag_argb(21),
            tag_argb(22),
            tag_argb(23),
            tag_argb(25),
        ];
        assert_eq!(cached_rows(&lsb, 6), want, "cells");
        assert_eq!(screen(&d, 6), want, "pixels");
    }

    #[test]
    fn scroll_region_down_blanks_exactly_the_vacated_band() {
        let (mut lsb, d) = console(4, 6);
        for (r, tag) in (20u8..26).enumerate() {
            paint_row(&mut lsb, r, tag);
        }
        flush(&lsb);

        lsb.scroll_region_down(1, 4, 2, tagged(30));
        flush(&lsb);

        assert_eq!(
            screen(&d, 6),
            vec![
                tag_argb(20),
                tag_argb(30),
                tag_argb(30),
                tag_argb(21),
                tag_argb(22),
                tag_argb(25),
            ]
        );
        assert_eq!(d.px(3, CHAR_HEIGHT - 1), tag_argb(20));
        assert_eq!(d.px(3, CHAR_HEIGHT), tag_argb(30));
        assert_eq!(d.px(3, 3 * CHAR_HEIGHT - 1), tag_argb(30));
        assert_eq!(d.px(3, 3 * CHAR_HEIGHT), tag_argb(21));
    }

    #[test]
    fn scroll_region_down_by_more_than_its_height_just_clears_it() {
        let (mut lsb, d) = console(4, 5);
        for (r, tag) in (20u8..25).enumerate() {
            paint_row(&mut lsb, r, tag);
        }
        flush(&lsb);

        lsb.scroll_region_down(1, 3, 99, tagged(30));
        flush(&lsb);

        let want = vec![
            tag_argb(20),
            tag_argb(30),
            tag_argb(30),
            tag_argb(30),
            tag_argb(24),
        ];
        assert_eq!(cached_rows(&lsb, 5), want, "cells");
        assert_eq!(screen(&d, 5), want, "pixels");
    }

    // ------------------------------------------------------- scrollback view

    /// Feed tags 20.. so the combined stream (`history ++ buf`) is known, and
    /// return it.
    fn stream(lsb: &mut LinearScrollbackBuffer, lines: u8) -> Vec<u32> {
        for tag in 20..20 + lines {
            feed(lsb, tag);
        }
        let mut s: Vec<u32> = lsb
            .history
            .iter()
            .map(|l| rgb888_to_argb(l[0].bg.to_rgb()))
            .collect();
        s.extend(cached_rows(lsb, lsb.height()));
        flush(lsb);
        s
    }

    #[test]
    fn one_line_back_shows_one_more_line_of_history() {
        // The regression this guards: `redraw` anchored the BOTTOM row at
        // `history_len - offset`, so the first line of scrollback jumped a
        // whole extra screen backwards — one press of Shift+PgUp skipped
        // `height` lines of output that were never shown.
        let (mut lsb, d) = console(4, 4);
        let s = stream(&mut lsb, 8);
        let live = screen(&d, 4);
        assert_eq!(
            live,
            s[s.len() - 4..].to_vec(),
            "the live screen is the tail"
        );

        lsb.scrollback_offset = Some(1);
        lsb.redraw();
        flush(&lsb);
        // Scrolled back by one line: the window slides by exactly one.
        assert_eq!(screen(&d, 4), s[s.len() - 5..s.len() - 1].to_vec());

        lsb.scrollback_offset = Some(3);
        lsb.redraw();
        flush(&lsb);
        assert_eq!(screen(&d, 4), s[s.len() - 7..s.len() - 3].to_vec());
    }

    #[test]
    fn scrolled_fully_back_puts_the_oldest_line_on_the_top_row() {
        let (mut lsb, d) = console(4, 4);
        let s = stream(&mut lsb, 8);
        let history_len = lsb.history.len();

        // `scroll_history` caps the offset at the number of history lines,
        // which is only the top of the buffer if the top row is the anchor.
        lsb.scrollback_offset = Some(history_len);
        lsb.redraw();
        flush(&lsb);
        assert_eq!(
            screen(&d, 4),
            s[..4].to_vec(),
            "the oldest line should be at the top, not at the bottom over a blank screen"
        );
    }

    #[test]
    fn a_page_back_and_a_page_forward_return_to_the_live_screen() {
        let (mut lsb, d) = console(4, 6);
        let s = stream(&mut lsb, 20);
        let live = screen(&d, 6);
        assert_eq!(live, s[s.len() - 6..].to_vec());

        lsb.scroll_history(1);
        flush(&lsb);
        // A page is `height - 2` lines, so two rows of the old screen stay on
        // display to give the reader an anchor.
        let page = lsb.height() - 2;
        assert_eq!(lsb.scrollback_offset, Some(page));
        assert_eq!(
            screen(&d, 6),
            s[s.len() - 6 - page..s.len() - page].to_vec()
        );

        lsb.scroll_history(-1);
        flush(&lsb);
        assert_eq!(lsb.scrollback_offset, None, "still stuck in scrollback");
        assert_eq!(screen(&d, 6), live);
    }

    #[test]
    fn holding_page_up_stops_with_the_oldest_line_on_the_top_row() {
        let (mut lsb, d) = console(4, 5);
        let s = stream(&mut lsb, 30);

        // Page up until the view stops moving, the way a reader holding
        // Shift+PgUp does. Where it stops is the whole point: the cap
        // `scroll_history` applies has to leave the oldest line reachable, on
        // the top row rather than one line past it.
        let mut last = None;
        for _ in 0..50 {
            lsb.scroll_history(1);
            if lsb.scrollback_offset == last {
                break;
            }
            last = lsb.scrollback_offset;
        }
        flush(&lsb);

        assert_eq!(lsb.scrollback_offset, Some(lsb.history.len()));
        assert_eq!(
            screen(&d, 5),
            s[..5].to_vec(),
            "the top of the scrollback is not reachable"
        );
    }

    #[test]
    fn rows_above_the_oldest_line_are_blank_rather_than_wrapped() {
        let (mut lsb, d) = console(4, 6);
        // Only four lines of history, so a full page back cannot fill the
        // screen; the gap must be blank, not the newest lines wrapped around.
        let s = stream(&mut lsb, 4);
        let history_len = lsb.history.len();
        lsb.scrollback_offset = Some(history_len);
        lsb.redraw();
        flush(&lsb);

        let got = screen(&d, 6);
        assert_eq!(got.len(), 6);
        assert_eq!(&got[..4], &s[..4], "the oldest history is at the top");
        // Below it comes the live buffer, never a wrap back to the newest.
        assert_eq!(&got[4..], &s[4..6]);
    }

    #[test]
    fn scrolling_back_with_no_history_stays_live() {
        let (mut lsb, d) = console(4, 4);
        paint_row(&mut lsb, 0, 20);
        flush(&lsb);
        let before = screen(&d, 4);

        lsb.scroll_history(1);
        flush(&lsb);
        assert_eq!(lsb.scrollback_offset, None);
        assert_eq!(screen(&d, 4), before);

        // And scrolling forward from the live screen is not an underflow.
        lsb.scroll_history(-1);
        flush(&lsb);
        assert_eq!(lsb.scrollback_offset, None);
        assert_eq!(screen(&d, 4), before);
    }

    #[test]
    fn output_arriving_while_scrolled_back_leaves_the_view_still() {
        let (mut lsb, d) = console(4, 5);
        stream(&mut lsb, 12);
        lsb.scrollback_offset = Some(4);
        lsb.redraw();
        flush(&lsb);
        let view = screen(&d, 5);

        // A background job prints three more lines. The reader is looking at
        // history and must not be yanked around.
        for tag in 60..63u8 {
            feed(&mut lsb, tag);
        }
        flush(&lsb);
        assert_eq!(screen(&d, 5), view);
        assert_eq!(lsb.scrollback_offset, Some(7));

        // Returning to the live screen shows the new output.
        lsb.scrollback_offset = None;
        lsb.redraw();
        flush(&lsb);
        assert_eq!(
            screen(&d, 5),
            vec![
                tag_argb(31), // the last line printed before the three new ones
                tag_argb(60),
                tag_argb(61),
                tag_argb(62),
                blank_argb(),
            ]
        );
    }

    // ----------------------------------------------------------- ensure_dims

    #[test]
    fn a_cache_that_lost_its_rows_is_rebuilt_instead_of_panicking() {
        let (mut lsb, d) = console(4, 3);
        // Every raw `buf[r][c]` in this file is bounded by the DISPLAY's
        // dimensions, so a short cache is a panic on the path the panic
        // handler prints through. It has to repair itself.
        lsb.buf.clear();
        lsb.write(2, 0, tagged(20));
        flush(&lsb);
        assert_eq!(lsb.buf.len(), 3);
        assert!(lsb.buf.iter().all(|r| r.len() == 4));
        assert_eq!(on_screen(&d, 2), tag_argb(20));

        // A row that is too short is widened the same way -- including its
        // last column, which is what the raw indexing would have run past.
        lsb.buf[0].clear();
        lsb.write(0, 3, tagged(21));
        flush(&lsb);
        assert_eq!(lsb.buf[0].len(), 4);
        assert_eq!(d.px(3 * CHAR_WIDTH + 3, 8), tag_argb(21));

        // And the scroll paths repair before they index.
        lsb.buf.truncate(1);
        lsb.scroll_region_up(0, 2, 1, tagged(22));
        lsb.buf.truncate(1);
        lsb.scroll_region_down(0, 2, 1, tagged(22));
        lsb.buf.truncate(1);
        lsb.new_line(tagged(22));
        assert_eq!(lsb.buf.len(), 3);
    }
}
