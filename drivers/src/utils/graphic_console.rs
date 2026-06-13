use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::convert::Infallible;
use core::ops::{Deref, DerefMut};

use rcore_console::{Cell, Console, DrawTarget, Flags, OriginDimensions, Pixel, Rgb888, Size, TextBuffer, TextOnGraphic};
use rcore_console::embedded_graphics::prelude::RgbColor as _;

use crate::scheme::display::{DisplayScheme, RgbColor, Rectangle};

pub struct DisplayWrapper(Arc<dyn DisplayScheme>);

impl DrawTarget for DisplayWrapper {
    type Color = Rgb888;
    type Error = Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for p in pixels {
            let color = RgbColor::new(p.1.r(), p.1.g(), p.1.b());
            self.0.draw_pixel(p.0.x as u32, p.0.y as u32, color);
        }
        Ok(())
    }
}

impl OriginDimensions for DisplayWrapper {
    fn size(&self) -> Size {
        let info = self.0.info();
        Size::new(info.width, info.height)
    }
}

pub struct LinearScrollbackBuffer {
    buf: Vec<Vec<Cell>>,
    history: VecDeque<Vec<Cell>>,
    scrollback_offset: Option<usize>,
    inner: TextOnGraphic<DisplayWrapper>,
    display: Arc<dyn DisplayScheme>,
}

impl LinearScrollbackBuffer {
    pub fn new(display: Arc<dyn DisplayScheme>) -> Self {
        let display_wrapper = DisplayWrapper(display.clone());
        let info = display.info();
        let inner = TextOnGraphic::new(display_wrapper, info.width, info.height);
        let width = inner.width();
        let height = inner.height();
        Self {
            buf: vec![vec![Cell::default(); width]; height],
            history: VecDeque::new(),
            scrollback_offset: None,
            inner,
            display,
        }
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

    pub fn redraw(&mut self) {
        let height = self.height();
        let width = self.width();

        if let Some(offset) = self.scrollback_offset {
            let history_len = self.history.len();
            for r in 0..height {
                let index = (history_len as isize) - (offset as isize) - ((height as isize) - 1 - (r as isize));
                if index < 0 {
                    let bg_cell = Cell::default();
                    for col in 0..width {
                        self.inner.write(r, col, bg_cell);
                    }
                } else if index < history_len as isize {
                    let line = &self.history[index as usize];
                    for col in 0..width {
                        let cell = line[col];
                        self.inner.write(r, col, cell);
                    }
                } else {
                    let active_row = (index - history_len as isize) as usize;
                    if active_row < height {
                        let line = &self.buf[active_row];
                        for col in 0..width {
                            let cell = line[col];
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
                let line = &self.buf[r];
                for col in 0..width {
                    let cell = line[col];
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
        self.buf[row][col]
    }

    #[inline]
    fn write(&mut self, row: usize, col: usize, cell: Cell) {
        let height = self.height();
        let width = self.width();
        if row >= height || col >= width {
            return;
        }
        self.buf[row][col] = cell;

        if self.scrollback_offset.is_none() {
            self.inner.write(row, col, cell);
        }
    }

    fn new_line(&mut self, cell: Cell) {
        let height = self.height();
        let width = self.width();
        if height == 0 {
            return;
        }

        // 1. Save top row to history
        let top_row = self.buf[0].clone();
        self.history.push_back(top_row);
        if self.history.len() > 1000 {
            self.history.pop_front();
        }

        // 2. Shift active rows up
        for r in 1..height {
            self.buf[r - 1] = self.buf[r].clone();
        }
        let bg_cell = Cell {
            c: ' ',
            bg: cell.bg,
            fg: cell.fg,
            flags: Flags::empty(),
        };
        self.buf[height - 1] = vec![bg_cell; width];

        // 3. Handle scrollback offset and scrolling
        if let Some(offset) = self.scrollback_offset {
            let max_offset = self.history.len();
            self.scrollback_offset = Some((offset + 1).min(max_offset));
            self.redraw();
        } else {
            let info = self.display.info();
            let pitch = info.pitch() as usize;
            let char_height = 18;
            let text_height_pixels = height * char_height;

            if text_height_pixels > char_height {
                let src_start = char_height * pitch;
                let src_end = text_height_pixels * pitch;
                let dest_start = 0;

                let mut fb = self.display.fb();
                if src_end <= fb.len() {
                    fb.copy_within(src_start..src_end, dest_start);
                }
            }

            let rect = Rectangle {
                x: 0,
                y: ((height - 1) * char_height) as u32,
                width: info.width,
                height: char_height as u32,
            };
            let rgb = cell.bg.to_rgb();
            let color = RgbColor::new(rgb.r(), rgb.g(), rgb.b());
            self.display.fill_rect(&rect, color);
        }
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

        let rgb = cell.bg.to_rgb();
        let color = RgbColor::new(rgb.r(), rgb.g(), rgb.b());
        self.display.clear(color);
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
