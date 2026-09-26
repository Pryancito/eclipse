use crate::cell::Cell;
use crate::text_buffer::TextBuffer;
use alloc::vec::Vec;

/// Cache layer for [`TextBuffer`]
pub struct TextBufferCache<T: TextBuffer> {
    buf: Vec<Vec<Cell>>,
    row_offset: usize,
    inner: T,
}

impl<T: TextBuffer> TextBufferCache<T> {
    /// Create a cache layer for `inner` text buffer
    pub fn new(inner: T) -> Self {
        TextBufferCache {
            buf: vec![vec![Cell::default(); inner.width()]; inner.height()],
            row_offset: 0,
            inner,
        }
    }
    /// The buffer underneath, so a test can change its size out from under the
    /// cache. Not part of the public API.
    #[cfg(test)]
    pub(crate) fn inner_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    /// Resize `buf` to match the buffer underneath.
    ///
    /// The cache sizes its rows once, at construction, and then indexes them on
    /// every write -- but it asked **`inner`** how big it was, not its own
    /// array. The two disagree the moment the display changes mode, or while
    /// the console is not fully built, and then every bound the cache checked
    /// was the wrong one: `self.buf[row][col]` is an index out of bounds. That
    /// matters more than an ordinary panic, because the console is what the
    /// panic handler prints through, so it is a panic inside the panic handler
    /// and the report naming the original fault never reaches the screen.
    /// Eclipse's own scrollback buffer carries the same repair, for the same
    /// crash; this is the copy one layer down.
    fn ensure_dims(&mut self) {
        let height = self.inner.height();
        let width = self.inner.width();
        if self.buf.len() != height {
            self.buf.resize(height, Vec::new());
            self.row_offset = 0;
        }
        for row in self.buf.iter_mut() {
            if row.len() != width {
                row.resize(width, Cell::default());
            }
        }
    }

    /// Get the real row of `buf` a logical `row` lives in.
    ///
    /// `None` when `buf` has no such row -- including the case of no rows at
    /// all, which is what a frame buffer shorter than one character cell
    /// reports and where the modulo was a division by zero. The bound is
    /// `buf`'s own length, never `inner`'s: this is the array being indexed.
    fn real_row(&self, row: usize) -> Option<usize> {
        let rows = self.buf.len();
        if rows == 0 || row >= rows {
            return None;
        }
        Some((self.row_offset + row) % rows)
    }
    /// Clear line at `row`. `row` is a physical row of `buf`, already
    /// translated, so it is not passed through [`Self::real_row`] again.
    fn clear_line(&mut self, row: usize, cell: Cell) {
        if row >= self.buf.len() {
            return;
        }
        // `buf[row].len()` and `inner.width()` agree by the time anything gets
        // here, because the only caller runs `ensure_dims` first -- so mutation
        // reports the two as interchangeable, and for now they are. The array's
        // own length is still the right thing to ask: it is the one being
        // indexed, and that is the whole point of the repair above.
        for col in 0..self.buf[row].len() {
            self.buf[row][col] = cell;
            self.inner.write(row, col, cell);
        }
    }
}

impl<T: TextBuffer> TextBuffer for TextBufferCache<T> {
    #[inline]
    fn width(&self) -> usize {
        self.inner.width()
    }

    #[inline]
    fn height(&self) -> usize {
        self.inner.height()
    }

    /// A cell outside the buffer reads as blank rather than panicking.
    ///
    /// The cache used to index its rows straight away, so it was **stricter
    /// than the thing it caches**: `TextOnGraphic::write` has always ignored a
    /// row or column past the edge, and putting the cache in front of it turned
    /// that no-op into an index out of bounds inside the kernel. A cache must
    /// not be less forgiving than the buffer it fronts.
    /// `&self`, so it cannot repair the way [`Self::ensure_dims`] does: it
    /// bounds against `buf` and answers a blank.
    #[inline]
    fn read(&self, row: usize, col: usize) -> Cell {
        match self.real_row(row) {
            Some(row) if col < self.buf[row].len() => self.buf[row][col],
            _ => Cell::default(),
        }
    }

    #[inline]
    fn write(&mut self, row: usize, col: usize, cell: Cell) {
        self.ensure_dims();
        let row = match self.real_row(row) {
            Some(row) if col < self.buf[row].len() => row,
            _ => return,
        };
        self.buf[row][col] = cell;
        self.inner.write(row, col, cell);
    }

    #[inline]
    fn new_line(&mut self, cell: Cell) {
        self.ensure_dims();
        let rows = self.buf.len();
        if rows == 0 {
            return;
        }
        self.clear_line(self.row_offset, cell);
        self.row_offset = (self.row_offset + 1) % rows;
    }

    /// Clearing the screen has to clear the cache too.
    ///
    /// This used to paint the frame buffer and leave `buf` holding every
    /// character that had just been wiped, so the next thing that *reads* a
    /// cell -- `scroll_region_up`, `scroll_region_down`, saving the screen to
    /// enter the alternate buffer -- copied the cleared text back onto the
    /// screen. `clear` then `scroll`, which is what `clear` followed by any
    /// full-screen app does, brought the old contents back.
    #[inline]
    fn clear(&mut self, cell: Cell) {
        self.ensure_dims();
        self.row_offset = 0;
        for row in self.buf.iter_mut() {
            for slot in row.iter_mut() {
                *slot = cell;
            }
        }
        self.inner.clear(cell);
    }
}
