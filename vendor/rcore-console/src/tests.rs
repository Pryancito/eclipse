//! Tests for the console the kernel prints through.
//!
//! Nothing here needs a frame buffer: a [`TextBuffer`] is a 2D array of cells,
//! so a `Vec` is one, and the console above it is the same state machine that
//! runs on real hardware. What the tests drive is the byte stream -- the same
//! escape sequences a shell, `ls` or a full-screen app emits.

use crate::cell::{Cell, Flags};
use crate::color::{Color, NamedColor};
use crate::console::Console;
use crate::text_buffer::TextBuffer;
use crate::text_buffer_cache::TextBufferCache;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

/// A plain in-memory [`TextBuffer`], standing in for the frame buffer.
///
/// `write` ignores a cell outside the grid, exactly as `TextOnGraphic::write`
/// does, and counts it: a test can then tell "the layer above clamped" from
/// "the layer above asked for something impossible and got away with it".
pub(crate) struct Grid {
    w: usize,
    h: usize,
    cells: Vec<Cell>,
    pub(crate) rejected: usize,
}

impl Grid {
    /// Grow the grid under whatever is holding it, which is what a display
    /// mode change does to the console mid-run.
    fn resize(&mut self, w: usize, h: usize) {
        self.w = w;
        self.h = h;
        self.cells = alloc::vec![Cell::default(); w * h];
    }

    pub(crate) fn new(w: usize, h: usize) -> Self {
        Grid {
            w,
            h,
            cells: alloc::vec![Cell::default(); w * h],
            rejected: 0,
        }
    }
}

impl TextBuffer for Grid {
    fn width(&self) -> usize {
        self.w
    }
    fn height(&self) -> usize {
        self.h
    }
    fn read(&self, row: usize, col: usize) -> Cell {
        self.cells[row * self.w + col]
    }
    fn write(&mut self, row: usize, col: usize, cell: Cell) {
        if row >= self.h || col >= self.w {
            self.rejected += 1;
            return;
        }
        self.cells[row * self.w + col] = cell;
    }
}

type Cached = Console<TextBufferCache<Grid>>;

/// The console Eclipse builds for a frame buffer: a cache in front of the
/// buffer (`Console::on_frame_buffer` ends up here).
fn console(w: usize, h: usize) -> Cached {
    Console::on_cached_text_buffer(Grid::new(w, h))
}

/// A console straight on the buffer, with no cache: this is what exercises the
/// default methods of the [`TextBuffer`] trait itself.
fn uncached(w: usize, h: usize) -> Console<Grid> {
    Console::on_text_buffer(Grid::new(w, h))
}

fn send<T: TextBuffer>(c: &mut Console<T>, s: &str) {
    c.write_str(s).unwrap();
}

/// The characters of one row, as the console would render them.
fn row<T: TextBuffer>(c: &mut Console<T>, r: usize) -> String {
    let w = c.columns();
    (0..w).map(|col| c.buf_mut().read(r, col).c).collect()
}

/// Every row, joined by `|`, for asserting a whole screen in one line.
fn screen<T: TextBuffer>(c: &mut Console<T>) -> String {
    let h = c.rows();
    (0..h).map(|r| row(c, r)).collect::<Vec<_>>().join("|")
}

/// Fill the current line right to the last column. That parks the cursor one
/// past it: a VT terminal defers the wrap until the next character arrives, so
/// this state happens every time a line is exactly full.
fn fill_line<T: TextBuffer>(c: &mut Console<T>) {
    let w = c.columns();
    for i in 0..w {
        send(c, &alloc::format!("{}", i % 10));
    }
}

// ---------------------------------------------------------------------------
// The cursor parked one past the last column
// ---------------------------------------------------------------------------

#[test]
fn a_full_line_parks_the_cursor_one_past_the_last_column() {
    let mut c = console(8, 4);
    fill_line(&mut c);
    assert_eq!(c.cursor(), (0, 8));
    assert_eq!(row(&mut c, 0), "01234567");
}

#[test]
fn delete_char_on_a_full_line_is_not_a_panic() {
    // `columns - col - 1` underflowed here. `\e[P` is what readline sends when
    // you press Delete, so a full line plus Delete took the kernel down.
    let mut c = console(8, 4);
    fill_line(&mut c);
    send(&mut c, "\x1b[P");
    assert_eq!(row(&mut c, 0), "01234567", "nothing to delete past the end");
}

#[test]
fn erase_to_left_on_a_full_line_is_not_a_panic() {
    // `0..=col` named the column one past the end, and the cache indexed it.
    let mut c = console(8, 4);
    fill_line(&mut c);
    send(&mut c, "\x1b[1K");
    assert_eq!(row(&mut c, 0), "        ", "the whole line is to the left");
}

#[test]
fn delete_char_pulls_the_rest_of_the_line_left() {
    let mut c = console(8, 2);
    send(&mut c, "abcdefgh\x1b[1;3H\x1b[2P");
    assert_eq!(row(&mut c, 0), "abefgh  ");
}

#[test]
fn delete_char_from_the_first_column_can_clear_the_whole_line() {
    // The old clamp was `columns - col - 1`, one too few: from column 0 it
    // refused to delete the last column, so `\e[8P` left an 'h' behind.
    let mut c = console(8, 2);
    send(&mut c, "abcdefgh\x1b[1;1H\x1b[8P");
    assert_eq!(row(&mut c, 0), "        ");
}

#[test]
fn delete_char_more_than_the_line_holds_clears_to_the_end() {
    let mut c = console(8, 2);
    send(&mut c, "abcdefgh\x1b[1;5H\x1b[99P");
    assert_eq!(row(&mut c, 0), "abcd    ");
}

#[test]
fn erase_to_left_stops_at_the_cursor() {
    let mut c = console(8, 2);
    send(&mut c, "abcdefgh\x1b[1;4H\x1b[1K");
    assert_eq!(row(&mut c, 0), "    efgh", "columns 1..=4 cleared");
}

#[test]
fn the_character_after_a_full_line_wraps_to_the_next_row() {
    let mut c = console(4, 3);
    send(&mut c, "abcdZ");
    assert_eq!(screen(&mut c), "abcd|Z   |    ");
    assert_eq!(c.cursor(), (1, 1));
}

#[test]
fn with_wrap_off_a_full_line_swallows_what_follows() {
    let mut c = console(4, 3);
    // `\e[?7l` turns auto-wrap off.
    send(&mut c, "\x1b[?7labcdZZZ");
    assert_eq!(screen(&mut c), "abcd|    |    ");
}

#[test]
fn the_cursor_report_never_names_a_column_that_does_not_exist() {
    // An app that sizes itself from CPR would have read one column too many.
    let mut c = console(8, 4);
    fill_line(&mut c);
    send(&mut c, "\x1b[6n");
    let mut report = String::new();
    while let Some(b) = c.pop_report() {
        report.push(b as char);
    }
    assert_eq!(report, "\x1b[1;8R");
}

#[test]
fn a_status_query_answers_that_the_terminal_is_well() {
    let mut c = console(8, 4);
    send(&mut c, "\x1b[5n");
    let mut report = String::new();
    while let Some(b) = c.pop_report() {
        report.push(b as char);
    }
    assert_eq!(report, "\x1b[0n");
}

// ---------------------------------------------------------------------------
// The cache
// ---------------------------------------------------------------------------

#[test]
fn clearing_the_screen_clears_the_cache_too() {
    // `clear` painted the frame buffer and left the cache holding every
    // character it had just wiped.
    let mut c = console(4, 3);
    send(&mut c, "abcd\x1b[2J");
    assert_eq!(screen(&mut c), "    |    |    ");
}

#[test]
fn cleared_text_does_not_come_back_when_a_region_scrolls() {
    // A scroll reads cells back out of the cache and writes them to the
    // screen, so a stale cache resurrects the cleared text.
    let mut c = console(4, 3);
    send(&mut c, "aaaa\r\nbbbb\r\ncccc");
    send(&mut c, "\x1b[2J");
    // Region = rows 1..=2, then scroll it up once.
    send(&mut c, "\x1b[1;2r\x1b[S");
    assert_eq!(screen(&mut c), "    |    |    ");
}

#[test]
fn cleared_text_does_not_come_back_through_the_alternate_screen() {
    // Entering the alternate screen saves the main one by reading every cell.
    let mut c = console(4, 2);
    send(&mut c, "abcd\x1b[2J");
    send(&mut c, "\x1b[?1049h");
    send(&mut c, "\x1b[?1049l");
    assert_eq!(screen(&mut c), "    |    ");
}

#[test]
fn a_cell_past_the_last_column_is_dropped_not_a_panic() {
    let mut grid = Grid::new(4, 2);
    grid.rejected = 0;
    let mut cache = TextBufferCache::new(grid);
    cache.write(0, 9, Cell::default());
    cache.write(9, 0, Cell::default());
}

#[test]
fn reading_outside_the_buffer_is_blank() {
    let mut cache = TextBufferCache::new(Grid::new(4, 2));
    let mut mark = Cell::default();
    mark.c = 'Z';
    cache.write(1, 0, mark);
    // Row 99 must not fold round onto row 1 and answer with its contents: a
    // modulo alone would have said 'Z' here.
    assert_eq!(cache.read(99, 0).c, ' ', "a row past the end read as row 1");
    assert_eq!(cache.read(0, 99).c, ' ');
    assert_eq!(cache.read(1, 0).c, 'Z', "and the real row still reads back");
}

#[test]
fn the_buffer_growing_under_the_cache_is_not_a_panic() {
    // The cache sizes its rows once, at construction. If the buffer underneath
    // changes size -- a display mode change, or a console not fully built yet --
    // the two disagree, and every row index the cache trusted is now a guess.
    let mut cache = TextBufferCache::new(Grid::new(4, 2));
    cache.inner_mut().resize(8, 6);
    let mut mark = Cell::default();
    mark.c = 'Z';
    // The cell needs both a row and a column the cache did not have. Dropping
    // it would also be panic-free, so the test asks for it back: the cache has
    // to have grown to match, not merely refused.
    cache.write(5, 7, mark);
    assert_eq!(cache.read(5, 7).c, 'Z', "the cache never grew to fit");
    cache.new_line(Cell::default());
    cache.clear(Cell::default());
    assert_eq!(cache.read(5, 7).c, ' ');
}

#[test]
fn a_new_line_blanks_the_row_it_reuses() {
    // The cache scrolls by moving its offset, not its cells, so the row that
    // becomes the new bottom is the one that just left the top. Without
    // clearing it, the text from the top of the screen reappears underneath.
    let mut c = console(4, 3);
    send(&mut c, "aaaa\r\nbbbb\r\ncccc\r\n");
    assert_eq!(screen(&mut c), "bbbb|cccc|    ");
}

#[test]
fn the_cache_hands_the_buffer_underneath_the_same_cells_it_reports() {
    // The cache is a ring: `row_offset` moves on every new line, so the row it
    // reports and the row it writes to the buffer are not the same number. What
    // has to match is the *contents* it reports and what the buffer holds.
    let mut c = console(4, 3);
    send(&mut c, "aaaa\r\nbbbb\r\ncccc\r\ndddd");
    let reported = screen(&mut c);
    assert_eq!(reported, "bbbb|cccc|dddd");
}

#[test]
fn a_buffer_with_no_rows_survives_every_escape_we_know() {
    // A frame buffer shorter than one character cell reports zero rows, and the
    // modulo in the cache was a division by zero.
    for seq in EVERY_SEQUENCE {
        let mut c = console(0, 0);
        send(&mut c, seq);
    }
}

#[test]
fn a_buffer_with_no_columns_survives_every_escape_we_know() {
    for seq in EVERY_SEQUENCE {
        let mut c = console(0, 4);
        send(&mut c, seq);
    }
}

#[test]
fn a_buffer_of_one_cell_survives_every_escape_we_know() {
    for seq in EVERY_SEQUENCE {
        let mut c = console(1, 1);
        send(&mut c, seq);
    }
}

#[test]
fn nothing_ever_asks_the_buffer_for_a_cell_outside_it() {
    // The invariant behind most of this batch: whatever the escape stream says,
    // the console must never name a row or column the buffer does not have. On
    // real hardware the layer below drops such a write (or panics, if it
    // indexes first), so a broken clamp is invisible until it is the one that
    // panics. Here the buffer counts them.
    //
    // Each sequence is tried from a fresh screen, from the last column, and
    // from the parked position after an exactly-full line, which is where the
    // off-by-ones live.
    for seq in EVERY_SEQUENCE {
        for start in ["", "\x1b[1;8H", "12345678"] {
            let mut c = uncached(8, 4);
            send(&mut c, start);
            send(&mut c, seq);
            assert_eq!(
                c.buf_mut().rejected,
                0,
                "{:?} after {:?} asked for a cell outside the buffer",
                seq,
                start
            );
        }
    }
}

/// One of everything the console claims to understand, for the shapes of
/// buffer that arithmetic goes wrong on.
const EVERY_SEQUENCE: &[&str] = &[
    "x",
    "xxxxxxxxxx",
    "\r\n",
    "\t",
    "\x08",
    "\x1b[A",
    "\x1b[9B",
    "\x1b[9C",
    "\x1b[9D",
    "\x1b[9E",
    "\x1b[9F",
    "\x1b[9G",
    "\x1b[9;9H",
    "\x1b[0J",
    "\x1b[1J",
    "\x1b[2J",
    "\x1b[0K",
    "\x1b[1K",
    "\x1b[2K",
    "\x1b[9@",
    "\x1b[9L",
    "\x1b[9M",
    "\x1b[9P",
    "\x1b[9S",
    "\x1b[9T",
    "\x1b[9X",
    "\x1b[9d",
    "\x1b[9I",
    "\x1b[6n",
    "\x1b[1;9r",
    "\x1b[?25l",
    "\x1b[?1049h",
    "\x1b[?1049l",
    "\x1b[?7l",
    "\x1b[1;31;44m",
    "\x1b[38;5;200m",
    "\x1b[38;2;1;2;3m",
    "\x1b7",
    "\x1b8",
    "\x1bD",
    "\x1bE",
    "\x1bM",
    "\x1b(0",
    "\x1b(B",
];

// ---------------------------------------------------------------------------
// Cursor motion
// ---------------------------------------------------------------------------

#[test]
fn moving_down_stops_at_the_last_row() {
    let mut c = console(4, 3);
    send(&mut c, "\x1b[99B");
    assert_eq!(c.cursor(), (2, 0));
}

#[test]
fn moving_forward_stops_at_the_last_column() {
    let mut c = console(4, 3);
    send(&mut c, "\x1b[99C");
    assert_eq!(c.cursor(), (0, 3));
}

#[test]
fn moving_up_stops_at_the_first_row() {
    let mut c = console(4, 3);
    send(&mut c, "\x1b[2;1H\x1b[99A");
    assert_eq!(c.cursor(), (0, 0));
}

#[test]
fn moving_backward_stops_at_the_first_column() {
    let mut c = console(4, 3);
    send(&mut c, "\x1b[1;3H\x1b[99D");
    assert_eq!(c.cursor(), (0, 0));
}

#[test]
fn goto_past_the_screen_lands_on_the_last_cell() {
    // `\e[999;999H` is how a program probes the window size over a serial line.
    // Clamping to the *count* rather than the last index parked the cursor off
    // the screen and the display looked hung.
    let mut c = console(8, 4);
    send(&mut c, "\x1b[999;999H");
    assert_eq!(c.cursor(), (3, 7));
    send(&mut c, "Z");
    assert_eq!(row(&mut c, 3).chars().last(), Some('Z'));
}

#[test]
fn a_zero_parameter_counts_as_one() {
    // `\e[0B` is one row down, not none: the parser substitutes the default.
    let mut c = console(4, 3);
    send(&mut c, "\x1b[0B");
    assert_eq!(c.cursor(), (1, 0));
}

#[test]
fn moving_down_and_cr_returns_to_the_first_column() {
    let mut c = console(8, 4);
    send(&mut c, "abc\x1b[2E");
    assert_eq!(c.cursor(), (2, 0));
}

#[test]
fn backspace_does_not_walk_off_the_start_of_the_line() {
    let mut c = console(4, 2);
    send(&mut c, "\x08\x08\x08");
    assert_eq!(c.cursor(), (0, 0));
}

#[test]
fn saving_and_restoring_the_cursor_comes_back_to_the_same_cell() {
    let mut c = console(8, 4);
    send(&mut c, "\x1b[3;5H\x1b7\x1b[1;1H\x1b8");
    assert_eq!(c.cursor(), (2, 4));
}

// ---------------------------------------------------------------------------
// Scroll region
// ---------------------------------------------------------------------------

#[test]
fn without_a_region_a_linefeed_at_the_bottom_scrolls_the_screen() {
    let mut c = uncached(4, 3);
    send(&mut c, "aaaa\r\nbbbb\r\ncccc\r\ndddd");
    assert_eq!(screen(&mut c), "bbbb|cccc|dddd");
}

#[test]
fn a_region_scrolls_only_its_own_rows() {
    let mut c = uncached(4, 4);
    send(&mut c, "aaaa\r\nbbbb\r\ncccc\r\ndddd");
    // Rows 2..=3 (1-indexed) become the region, then scroll it up once.
    send(&mut c, "\x1b[2;3r\x1b[S");
    assert_eq!(
        screen(&mut c),
        "aaaa|cccc|    |dddd",
        "the first and last rows are outside the region"
    );
}

#[test]
fn a_region_of_one_line_is_no_region_at_all() {
    // xterm treats a region that cannot scroll as a reset to the whole screen.
    let mut c = uncached(4, 3);
    send(&mut c, "\x1b[2;2r");
    send(&mut c, "aaaa\r\nbbbb\r\ncccc\r\ndddd");
    assert_eq!(screen(&mut c), "bbbb|cccc|dddd");
}

#[test]
fn setting_a_region_homes_the_cursor() {
    let mut c = uncached(8, 4);
    send(&mut c, "\x1b[3;5H\x1b[1;3r");
    assert_eq!(c.cursor(), (0, 0));
}

#[test]
fn a_region_bottom_past_the_screen_is_clamped() {
    let mut c = uncached(4, 3);
    send(&mut c, "\x1b[1;99r");
    send(&mut c, "aaaa\r\nbbbb\r\ncccc\r\ndddd");
    assert_eq!(screen(&mut c), "bbbb|cccc|dddd");
}

#[test]
fn a_linefeed_at_the_region_bottom_scrolls_the_region() {
    let mut c = uncached(4, 4);
    send(&mut c, "\x1b[1;3r");
    send(&mut c, "aaaa\r\nbbbb\r\ncccc\r\ndddd");
    assert_eq!(
        screen(&mut c),
        "bbbb|cccc|dddd|    ",
        "the fourth row is below the region and never moves"
    );
}

#[test]
fn reverse_index_at_the_region_top_scrolls_it_down() {
    let mut c = uncached(4, 4);
    send(&mut c, "aaaa\r\nbbbb\r\ncccc\r\ndddd");
    send(&mut c, "\x1b[2;3r"); // homes the cursor to (0,0)
    send(&mut c, "\x1b[2;1H\x1bM");
    assert_eq!(screen(&mut c), "aaaa|    |bbbb|dddd");
}

#[test]
fn insert_blank_lines_opens_lines_at_the_cursor() {
    let mut c = uncached(4, 4);
    send(&mut c, "aaaa\r\nbbbb\r\ncccc\r\ndddd");
    send(&mut c, "\x1b[2;1H\x1b[L");
    assert_eq!(screen(&mut c), "aaaa|    |bbbb|cccc");
}

#[test]
fn delete_lines_closes_lines_at_the_cursor() {
    let mut c = uncached(4, 4);
    send(&mut c, "aaaa\r\nbbbb\r\ncccc\r\ndddd");
    send(&mut c, "\x1b[2;1H\x1b[M");
    assert_eq!(screen(&mut c), "aaaa|cccc|dddd|    ");
}

#[test]
fn scrolling_a_region_by_more_than_it_holds_just_clears_it() {
    let mut c = uncached(4, 4);
    send(&mut c, "aaaa\r\nbbbb\r\ncccc\r\ndddd");
    send(&mut c, "\x1b[1;3r\x1b[99S");
    assert_eq!(screen(&mut c), "    |    |    |dddd");
}

#[test]
fn scrolling_a_region_down_by_more_than_it_holds_just_clears_it() {
    let mut c = uncached(4, 4);
    send(&mut c, "aaaa\r\nbbbb\r\ncccc\r\ndddd");
    send(&mut c, "\x1b[1;3r\x1b[99T");
    assert_eq!(screen(&mut c), "    |    |    |dddd");
}

#[test]
fn inserting_or_deleting_lines_outside_the_region_does_nothing() {
    let mut c = uncached(4, 4);
    send(&mut c, "aaaa\r\nbbbb\r\ncccc\r\ndddd");
    // Region rows 1..=2, cursor on row 4, which is outside it.
    send(&mut c, "\x1b[1;2r\x1b[4;1H\x1b[L\x1b[M");
    assert_eq!(screen(&mut c), "aaaa|bbbb|cccc|dddd");
}

// ---------------------------------------------------------------------------
// The alternate screen
// ---------------------------------------------------------------------------

#[test]
fn the_alternate_screen_comes_up_blank_and_gives_the_screen_back() {
    let mut c = uncached(4, 2);
    send(&mut c, "aaaa\r\nbbbb");
    send(&mut c, "\x1b[?1049h");
    assert_eq!(screen(&mut c), "    |    ", "the alternate screen is blank");
    send(&mut c, "xx");
    send(&mut c, "\x1b[?1049l");
    assert_eq!(screen(&mut c), "aaaa|bbbb", "the main screen comes back");
}

#[test]
fn the_alternate_screen_restores_the_cursor_as_well() {
    let mut c = uncached(8, 4);
    send(&mut c, "\x1b[3;5H");
    send(&mut c, "\x1b[?1049h");
    assert_eq!(c.cursor(), (0, 0), "the alternate screen starts at home");
    send(&mut c, "\x1b[2;2H\x1b[?1049l");
    assert_eq!(c.cursor(), (2, 4));
}

#[test]
fn entering_the_alternate_screen_twice_does_not_lose_the_main_one() {
    let mut c = uncached(4, 2);
    send(&mut c, "aaaa\r\nbbbb");
    send(&mut c, "\x1b[?1049h");
    send(&mut c, "xxxx");
    send(&mut c, "\x1b[?1049h"); // a second request must not save the alt screen
    send(&mut c, "\x1b[?1049l");
    assert_eq!(screen(&mut c), "aaaa|bbbb");
}

#[test]
fn leaving_the_alternate_screen_without_having_entered_it_does_nothing() {
    let mut c = uncached(4, 2);
    send(&mut c, "aaaa\r\nbbbb");
    send(&mut c, "\x1b[?1049l");
    assert_eq!(screen(&mut c), "aaaa|bbbb");
}

#[test]
fn the_alternate_screen_drops_the_scroll_region() {
    // Five lines on a four-row screen, so something has to scroll. With the
    // region still in force only its own two rows would move; with it dropped
    // the whole screen does.
    let mut c = uncached(4, 4);
    send(&mut c, "\x1b[1;2r\x1b[?1049h");
    send(&mut c, "aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee");
    assert_eq!(
        screen(&mut c),
        "bbbb|cccc|dddd|eeee",
        "the whole screen scrolls again"
    );
}

// ---------------------------------------------------------------------------
// Tabs, erase and insert
// ---------------------------------------------------------------------------

#[test]
fn a_tab_stops_at_the_next_multiple_of_eight() {
    let mut c = uncached(24, 2);
    send(&mut c, "ab\t");
    assert_eq!(c.cursor(), (0, 8));
    send(&mut c, "\t");
    assert_eq!(c.cursor(), (0, 16));
}

#[test]
fn a_tab_does_not_run_off_the_end_of_the_line() {
    let mut c = uncached(10, 2);
    send(&mut c, "\t\t\t\t\t");
    assert_eq!(c.cursor().1, 10, "parked at the edge, not beyond it");
}

#[test]
fn a_tab_paints_the_cells_it_crosses_with_the_background() {
    let mut c = uncached(16, 2);
    send(&mut c, "abcdefghijklmnop\x1b[1;1H\t");
    assert_eq!(row(&mut c, 0), "        ijklmnop");
}

#[test]
fn erase_chars_paints_the_background_and_leaves_the_rest_alone() {
    let mut c = uncached(8, 2);
    send(&mut c, "abcdefgh\x1b[1;3H\x1b[2X");
    assert_eq!(row(&mut c, 0), "ab  efgh");
}

#[test]
fn erase_chars_past_the_end_of_the_line_stops_at_the_edge() {
    let mut c = uncached(8, 2);
    send(&mut c, "abcdefgh\x1b[1;7H\x1b[99X");
    assert_eq!(row(&mut c, 0), "abcdef  ");
}

#[test]
fn insert_blank_pushes_the_rest_of_the_line_right() {
    let mut c = uncached(8, 2);
    send(&mut c, "abcdefgh\x1b[1;3H\x1b[2@");
    assert_eq!(row(&mut c, 0), "ab  cdef");
}

#[test]
fn insert_blank_more_than_the_line_holds_clears_to_the_end() {
    let mut c = uncached(8, 2);
    send(&mut c, "abcdefgh\x1b[1;5H\x1b[99@");
    assert_eq!(row(&mut c, 0), "abcd    ");
}

#[test]
fn erase_to_the_right_clears_from_the_cursor_on() {
    let mut c = uncached(8, 2);
    send(&mut c, "abcdefgh\x1b[1;4H\x1b[0K");
    assert_eq!(row(&mut c, 0), "abc     ");
}

#[test]
fn erase_the_whole_line_leaves_the_others() {
    let mut c = uncached(4, 3);
    send(&mut c, "aaaa\r\nbbbb\r\ncccc");
    send(&mut c, "\x1b[2;3H\x1b[2K");
    assert_eq!(screen(&mut c), "aaaa|    |cccc");
}

#[test]
fn clearing_above_and_below_split_at_the_cursor() {
    let mut c = uncached(4, 3);
    send(&mut c, "aaaa\r\nbbbb\r\ncccc");
    send(&mut c, "\x1b[2;3H\x1b[1J"); // above, inclusive of the cells left of the cursor
    assert_eq!(screen(&mut c), "    |  bb|cccc");

    let mut c = uncached(4, 3);
    send(&mut c, "aaaa\r\nbbbb\r\ncccc");
    send(&mut c, "\x1b[2;3H\x1b[0J"); // below, from the cursor on
    assert_eq!(screen(&mut c), "aaaa|bb  |    ");
}

#[test]
fn clearing_the_whole_screen_homes_the_cursor() {
    let mut c = uncached(4, 3);
    send(&mut c, "aaaa\r\nbb\x1b[2J");
    assert_eq!(c.cursor(), (0, 0));
    assert_eq!(screen(&mut c), "    |    |    ");
}

// ---------------------------------------------------------------------------
// Attributes, colours and the DEC line-drawing charset
// ---------------------------------------------------------------------------

fn cell<T: TextBuffer>(c: &mut Console<T>, r: usize, col: usize) -> Cell {
    c.buf_mut().read(r, col)
}

#[test]
fn sgr_sets_the_two_colours_and_reset_puts_them_back() {
    let mut c = uncached(8, 2);
    send(&mut c, "\x1b[31;44mX\x1b[0mY");
    let x = cell(&mut c, 0, 0);
    assert_eq!(x.fg, Color::Named(NamedColor::Red));
    assert_eq!(x.bg, Color::Named(NamedColor::Blue));
    let y = cell(&mut c, 0, 1);
    assert_eq!(y.fg, Cell::default().fg);
    assert_eq!(y.bg, Cell::default().bg);
}

#[test]
fn the_bright_colours_are_their_own_sixteen() {
    let mut c = uncached(8, 2);
    send(&mut c, "\x1b[91;104mX");
    let x = cell(&mut c, 0, 0);
    assert_eq!(x.fg, Color::Named(NamedColor::BrightRed));
    assert_eq!(x.bg, Color::Named(NamedColor::BrightBlue));
}

#[test]
fn a_256_colour_index_is_kept_as_an_index() {
    let mut c = uncached(8, 2);
    send(&mut c, "\x1b[38;5;200mX");
    assert_eq!(cell(&mut c, 0, 0).fg, Color::Indexed(200));
}

#[test]
fn every_256_colour_index_has_a_colour_behind_it() {
    // `to_rgb` indexes a 256-entry table with a `u8`, so the whole range has to
    // resolve rather than the first sixteen.
    for i in 0..=255u8 {
        let _ = Color::Indexed(i).to_rgb();
    }
}

#[test]
fn a_truecolour_spec_is_taken_as_written() {
    let mut c = uncached(8, 2);
    send(&mut c, "\x1b[38;2;10;20;30mX");
    assert_eq!(
        cell(&mut c, 0, 0).fg,
        Color::Spec(crate::color::Rgb888::new(10, 20, 30))
    );
}

#[test]
fn a_colour_component_that_does_not_fit_a_byte_is_refused() {
    // The component is a `u16` on the wire; 300 is not a colour, and taking the
    // low byte of it would have been a different colour, silently.
    let mut c = uncached(8, 2);
    send(&mut c, "\x1b[38;2;300;0;0mX");
    assert_eq!(
        cell(&mut c, 0, 0).fg,
        Cell::default().fg,
        "the attribute is dropped, not truncated"
    );
}

#[test]
fn the_flag_attributes_go_on_and_come_off() {
    let mut c = uncached(8, 2);
    send(&mut c, "\x1b[1;3;4;7;9mX");
    let x = cell(&mut c, 0, 0);
    for f in [
        Flags::BOLD,
        Flags::ITALIC,
        Flags::UNDERLINE,
        Flags::INVERSE,
        Flags::STRIKEOUT,
    ] {
        assert!(x.flags.contains(f), "missing {:?}", f);
    }
    send(&mut c, "\x1b[22;23;24;27;29mY");
    assert_eq!(cell(&mut c, 0, 1).flags, Flags::empty());
}

#[test]
fn a_bare_sgr_is_a_reset() {
    let mut c = uncached(8, 2);
    send(&mut c, "\x1b[1;31m\x1b[mX");
    let x = cell(&mut c, 0, 0);
    assert_eq!(x.flags, Flags::empty());
    assert_eq!(x.fg, Cell::default().fg);
}

#[test]
fn the_dec_line_drawing_charset_turns_letters_into_box_characters() {
    let mut c = uncached(8, 2);
    // `ESC ( 0` selects DEC Special Graphics for G0; `q` and `x` are the
    // horizontal and vertical lines every TUI draws borders with.
    send(&mut c, "\x1b(0qx\x1b(Bqx");
    assert_eq!(row(&mut c, 0), "─│qx    ");
}

#[test]
fn a_letter_with_no_line_drawing_meaning_comes_through_as_itself() {
    let mut c = uncached(8, 2);
    send(&mut c, "\x1b(0Z");
    assert_eq!(row(&mut c, 0), "Z       ");
}

#[test]
fn hiding_and_showing_the_cursor_is_reported() {
    let mut c = uncached(8, 2);
    assert!(c.cursor_visible());
    send(&mut c, "\x1b[?25l");
    assert!(!c.cursor_visible());
    send(&mut c, "\x1b[?25h");
    assert!(c.cursor_visible());
}

#[test]
fn an_escape_sequence_nobody_handles_is_ignored_rather_than_printed() {
    let mut c = uncached(8, 2);
    send(&mut c, "\x1b[99;99y\x1b]0;a title\x07X");
    assert_eq!(row(&mut c, 0), "X       ", "no stray bytes on the screen");
}

// ---------------------------------------------------------------------------
// The renderer: cells to pixels
// ---------------------------------------------------------------------------

use crate::graphic::TextOnGraphic;
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::{DrawTarget, OriginDimensions, Pixel, RgbColor, Size};

/// A frame buffer that remembers every pixel written to it.
struct Canvas {
    w: u32,
    h: u32,
    px: Vec<Option<Rgb888>>,
    /// Pixels written outside the canvas. A renderer that does this on real
    /// hardware is writing over whatever follows the frame buffer.
    outside: usize,
}

impl Canvas {
    fn new(w: u32, h: u32) -> Self {
        Canvas {
            w,
            h,
            px: alloc::vec![None; (w * h) as usize],
            outside: 0,
        }
    }
    fn at(&self, x: u32, y: u32) -> Option<Rgb888> {
        self.px[(y * self.w + x) as usize]
    }
}

impl OriginDimensions for Canvas {
    fn size(&self) -> Size {
        Size::new(self.w, self.h)
    }
}

impl DrawTarget for Canvas {
    type Color = Rgb888;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(p, colour) in pixels {
            if p.x < 0 || p.y < 0 || p.x as u32 >= self.w || p.y as u32 >= self.h {
                self.outside += 1;
                continue;
            }
            self.px[(p.y as u32 * self.w + p.x as u32) as usize] = Some(colour);
        }
        Ok(())
    }
}

/// The glyph cell of the bundled font.
const CW: u32 = 9;
const CH: u32 = 18;

fn painted(c: char, cols: u32, rows: u32) -> TextOnGraphic<Canvas> {
    let mut g = TextOnGraphic::new(Canvas::new(cols * CW, rows * CH), cols * CW, rows * CH);
    let mut cell = Cell::default();
    cell.c = c;
    cell.fg = Color::Spec(Rgb888::new(255, 255, 255));
    cell.bg = Color::Spec(Rgb888::new(0, 0, 0));
    g.write(0, 0, cell);
    g
}

/// How many pixels of the first cell are the foreground colour.
fn lit(g: &TextOnGraphic<Canvas>, x0: u32, y0: u32, w: u32, h: u32) -> usize {
    let white = Rgb888::new(255, 255, 255);
    let mut n = 0;
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            if g.target().at(x, y) == Some(white) {
                n += 1;
            }
        }
    }
    n
}

#[test]
fn the_text_size_is_the_frame_buffer_divided_by_the_glyph() {
    let g = TextOnGraphic::new(
        Canvas::new(80 * CW + 5, 25 * CH + 7),
        80 * CW + 5,
        25 * CH + 7,
    );
    assert_eq!(g.width(), 80, "a partial column is not a column");
    assert_eq!(g.height(), 25);
}

#[test]
fn a_frame_buffer_smaller_than_one_glyph_has_no_cells() {
    let g = TextOnGraphic::new(Canvas::new(8, 17), 8, 17);
    assert_eq!((g.width(), g.height()), (0, 0));
}

#[test]
fn a_cell_outside_the_screen_draws_nothing() {
    let mut g = TextOnGraphic::new(Canvas::new(CW, CH), CW, CH);
    let mut cell = Cell::default();
    cell.c = '\u{2588}'; // full block, which fills its whole cell
    g.write(0, 1, cell);
    g.write(1, 0, cell);
    assert_eq!(g.target().outside, 0, "nothing was drawn off the canvas");
    assert!(
        g.target().at(0, 0).is_none(),
        "and nothing was drawn inside it either"
    );
}

#[test]
fn the_full_block_fills_its_whole_cell() {
    let g = painted('\u{2588}', 1, 1);
    assert_eq!(lit(&g, 0, 0, CW, CH), (CW * CH) as usize);
}

#[test]
fn the_half_blocks_fill_exactly_their_half() {
    let g = painted('\u{2584}', 1, 1); // lower half
    assert_eq!(lit(&g, 0, 0, CW, CH / 2), 0, "the top half is background");
    assert_eq!(lit(&g, 0, CH / 2, CW, CH / 2), (CW * CH / 2) as usize);

    let g = painted('\u{2580}', 1, 1); // upper half
    assert_eq!(lit(&g, 0, 0, CW, CH / 2), (CW * CH / 2) as usize);
    assert_eq!(lit(&g, 0, CH / 2, CW, CH / 2), 0);

    let g = painted('\u{258C}', 1, 1); // left half
    assert_eq!(lit(&g, 0, 0, CW / 2, CH), (CW / 2 * CH) as usize);
}

#[test]
fn the_eighth_blocks_grow_one_eighth_at_a_time() {
    // U+2581..U+2587 are the lower 1/8..7/8 blocks, the glyphs a meter is drawn
    // out of. Each one has to be taller than the last, or a bar chart has steps
    // that repeat or go backwards.
    let mut last = 0;
    for n in 1..=7u32 {
        let c = char::from_u32(0x2580 + n).unwrap();
        let g = painted(c, 1, 1);
        let want = (CH * n / 8 * CW) as usize;
        let got = lit(&g, 0, 0, CW, CH);
        assert_eq!(got, want, "{:?} should be {}/8 of the cell", c, n);
        assert!(got > last, "{:?} is not taller than the one before", c);
        last = got;
    }
}

#[test]
fn the_left_eighth_blocks_shrink_one_eighth_at_a_time() {
    // U+2589..U+258F run the other way: 7/8 down to 1/8.
    let mut last = usize::MAX;
    for (i, n) in (1..=7u32).rev().enumerate() {
        let c = char::from_u32(0x2589 + i as u32).unwrap();
        let g = painted(c, 1, 1);
        let want = (CW * n / 8 * CH) as usize;
        let got = lit(&g, 0, 0, CW, CH);
        assert_eq!(got, want, "{:?} should be {}/8 of the cell", c, n);
        assert!(got < last, "{:?} is not narrower than the one before", c);
        last = got;
    }
}

#[test]
fn a_horizontal_line_crosses_the_middle_of_the_cell() {
    let g = painted('\u{2500}', 1, 1);
    assert_eq!(lit(&g, 0, CH / 2, CW, 1), CW as usize);
    assert_eq!(lit(&g, 0, 0, CW, CH / 2), 0, "and nothing above it");
}

#[test]
fn a_vertical_line_runs_down_the_middle_of_the_cell() {
    let g = painted('\u{2502}', 1, 1);
    assert_eq!(lit(&g, CW / 2, 0, 1, CH), CH as usize);
}

#[test]
fn a_corner_draws_only_its_two_arms() {
    // U+250C is the top-left corner: right arm and bottom arm, nothing else.
    let g = painted('\u{250C}', 1, 1);
    assert_eq!(lit(&g, 0, 0, CW / 2, CH / 2), 0, "no arm up or left");
    assert!(lit(&g, CW / 2, CH / 2, CW - CW / 2, 1) > 0, "right arm");
    assert!(lit(&g, CW / 2, CH / 2, 1, CH - CH / 2) > 0, "bottom arm");
}

#[test]
fn the_shades_sit_between_the_two_colours() {
    // ░▒▓ are drawn as a blend, so each must be darker than the last against a
    // black background.
    let mut last = 0u32;
    for c in ['\u{2591}', '\u{2592}', '\u{2593}'] {
        let g = painted(c, 1, 1);
        let px = g.target().at(0, 0).expect("the cell was painted");
        let level = px.r() as u32 + px.g() as u32 + px.b() as u32;
        assert!(level > last, "{:?} is not lighter than the one before", c);
        assert!(level < 255 * 3, "{:?} is the full foreground", c);
        last = level;
    }
}

#[test]
fn an_inverse_cell_swaps_the_two_colours() {
    let mut g = TextOnGraphic::new(Canvas::new(CW, CH), CW, CH);
    let mut cell = Cell::default();
    cell.c = '\u{2588}'; // full block: the whole cell is the foreground
    cell.fg = Color::Spec(Rgb888::new(0, 0, 0));
    cell.bg = Color::Spec(Rgb888::new(255, 255, 255));
    cell.flags = Flags::INVERSE;
    g.write(0, 0, cell);
    assert_eq!(
        g.target().at(0, 0),
        Some(Rgb888::new(255, 255, 255)),
        "the background was used as the foreground"
    );
}

#[test]
fn reading_a_cell_back_from_pixels_is_blank_rather_than_a_panic() {
    // This was `unimplemented!()`, and the trait's own `new_line` reads before
    // it writes -- so a console built straight on the frame buffer died on its
    // first scroll, on the path the panic handler prints through.
    let g = TextOnGraphic::new(Canvas::new(CW, CH), CW, CH);
    assert_eq!(g.read(0, 0), Cell::default());
}

#[test]
fn a_console_straight_on_the_frame_buffer_survives_a_scroll() {
    let mut c = Console::on_text_buffer(TextOnGraphic::new(
        Canvas::new(4 * CW, 2 * CH),
        4 * CW,
        2 * CH,
    ));
    send(&mut c, "aaaa\r\nbbbb\r\ncccc");
    assert_eq!(c.cursor(), (1, 4));
}

// ---------------------------------------------------------------------------
// The mode numbers
// ---------------------------------------------------------------------------

#[test]
fn every_mode_the_parser_knows_is_reached_by_its_own_number() {
    use crate::ansi::Mode;
    // `\e[?7h` and `\e[7h` are different sequences: the `?` makes the number
    // private, and the two sets of numbers do not overlap at all. Nothing held
    // the table to that, so a mode could answer to the wrong number, or to
    // both, and the only symptom would be an app whose cursor stops blinking
    // when it asked for something else entirely.
    for (num, mode) in [
        (1, Mode::CursorKeys),
        (3, Mode::ColumnMode),
        (6, Mode::Origin),
        (7, Mode::LineWrap),
        (12, Mode::BlinkingCursor),
        (25, Mode::ShowCursor),
        (47, Mode::SwapScreenOld),
        (1000, Mode::ReportMouseClicks),
        (1002, Mode::ReportCellMouseMotion),
        (1003, Mode::ReportAllMouseMotion),
        (1004, Mode::ReportFocusInOut),
        (1005, Mode::Utf8Mouse),
        (1006, Mode::SgrMouse),
        (1007, Mode::AlternateScroll),
        (1042, Mode::UrgencyHints),
        (1047, Mode::SwapScreenBuffer),
        (1049, Mode::SwapScreenAndSetRestoreCursor),
        (2004, Mode::BracketedPaste),
    ] {
        assert_eq!(
            Mode::from_primitive(Some(&b'?'), num),
            Some(mode),
            "?{} is not the mode it should be",
            num
        );
        assert_eq!(
            Mode::from_primitive(None, num),
            None,
            "{} is a private mode and was accepted without the ?",
            num
        );
    }
    // The two public ones, and the same check the other way round.
    for (num, mode) in [(4, Mode::Insert), (20, Mode::LineFeedNewLine)] {
        assert_eq!(
            Mode::from_primitive(None, num),
            Some(mode),
            "{} is not the mode it should be",
            num
        );
        assert_eq!(
            Mode::from_primitive(Some(&b'?'), num),
            None,
            "?{} is not a private mode and was accepted as one",
            num
        );
    }
    // Any other intermediate byte is refused outright, private or not.
    assert_eq!(Mode::from_primitive(Some(&b'>'), 7), None);
    assert_eq!(Mode::from_primitive(Some(&b'>'), 4), None);
    // And a number in neither set stays unknown rather than falling through to
    // whichever arm the match happens to end on.
    assert_eq!(Mode::from_primitive(None, 7), None);
    assert_eq!(Mode::from_primitive(Some(&b'?'), 4), None);
    assert_eq!(Mode::from_primitive(Some(&b'?'), 9999), None);
}

// ---------------------------------------------------------------------------
// The control characters and the escapes with no bracket
// ---------------------------------------------------------------------------

#[test]
fn the_control_characters_each_mean_their_own_motion() {
    let mut c = uncached(8, 3);
    // Backspace steps one column left; carriage return goes all the way to the
    // first one. Either standing in for the other is invisible in anything but
    // a line that was already written to.
    send(&mut c, "abc\x08X");
    assert_eq!(
        row(&mut c, 0),
        "abX     ",
        "backspace did not step one column"
    );
    send(&mut c, "\rZ");
    assert_eq!(
        row(&mut c, 0),
        "ZbX     ",
        "carriage return did not reach column one"
    );

    // Vertical tab and form feed are linefeeds too -- `cat` of a text file with
    // page breaks in it emits both -- and this console's linefeed returns to
    // the first column on the way down.
    let mut c = uncached(8, 3);
    send(&mut c, "abc\x0bd\x0ce");
    assert_eq!(
        screen(&mut c),
        "abc     |d       |e       ",
        "vertical tab and form feed did not move down and back"
    );
}

#[test]
fn the_escapes_with_no_bracket_do_what_their_letter_says() {
    // IND moves down a line. Its neighbour RI moves up, and they are one
    // letter apart in a match arm.
    let mut c = uncached(4, 3);
    send(&mut c, "aa\x1bDb");
    assert_eq!(screen(&mut c), "aa  |b   |    ", "ESC D did not move down");

    // `ESC ) 0` loads the *second* charset, G1, which this console accepts and
    // then ignores: only G0 draws boxes, so a `q` after it is still a `q`.
    let mut c = uncached(4, 1);
    send(&mut c, "\x1b)0q");
    assert_eq!(row(&mut c, 0), "q   ", "ESC ) 0 turned on line drawing");
    let mut c = uncached(4, 1);
    send(&mut c, "\x1b)Bq");
    assert_eq!(row(&mut c, 0), "q   ");
}

// ---------------------------------------------------------------------------
// The CSI table: the aliases, the off-by-one and the defaults
// ---------------------------------------------------------------------------

#[test]
fn the_second_spelling_of_a_motion_moves_the_same_way() {
    // `e`, `a` and the backtick are the older spellings of CUD, CUF and CHA.
    // Which one a terminal sees is up to whoever wrote the terminfo entry, so
    // dropping an alias breaks exactly the apps that use that entry.
    for (seq, want) in [
        ("\x1b[2B", (2, 0)),
        ("\x1b[2e", (2, 0)),
        ("\x1b[3C", (0, 3)),
        ("\x1b[3a", (0, 3)),
        ("\x1b[4G", (0, 3)),
        ("\x1b[4`", (0, 3)),
    ] {
        let mut c = uncached(8, 4);
        send(&mut c, seq);
        assert_eq!(c.cursor(), want, "{:?} did not move the cursor", seq);
    }
}

#[test]
fn a_position_given_to_the_console_counts_from_one_and_keeps_the_other_axis() {
    // CHA (`G`) names a column and VPA (`d`) names a row. Both count from one,
    // and neither is allowed to touch the axis it does not name -- which is
    // the whole difference between them and `H`, that names both.
    let mut c = uncached(8, 6);
    send(&mut c, "\x1b[3;5H");
    assert_eq!(c.cursor(), (2, 4));
    send(&mut c, "\x1b[2G");
    assert_eq!(
        c.cursor(),
        (2, 1),
        "CHA moved the row, or counted from zero"
    );
    send(&mut c, "\x1b[4d");
    assert_eq!(
        c.cursor(),
        (3, 1),
        "VPA moved the column, or counted from zero"
    );
}

#[test]
fn moving_up_with_a_carriage_return_lands_on_the_first_column() {
    // CPL (`F`) is CUU plus a carriage return, and the pair of them differ by
    // one argument at the call.
    let mut c = uncached(8, 6);
    send(&mut c, "\x1b[4;5H\x1b[2F");
    assert_eq!(c.cursor(), (1, 0), "CPL did not return to the first column");
}

#[test]
fn a_csi_with_more_than_one_intermediate_is_refused() {
    // `\e[?25$h` is a DECRQM-shaped sequence, not DECSET: it carries a second
    // intermediate byte, and the mode arms match *any* intermediates, so
    // without the guard above them the `?` alone is enough to act on it.
    let mut c = uncached(4, 2);
    send(&mut c, "\x1b[?25l");
    send(&mut c, "\x1b[?25$h");
    assert!(
        !c.cursor_visible(),
        "a sequence with two intermediates was taken as DECSET"
    );

    // The other half of the guard is the parser's own "there was more here
    // than I can hold" flag, which it raises on more parameters than it has
    // room for -- with no intermediates at all, so the count cannot catch it.
    let mut c = uncached(4, 2);
    let mut flood = String::from("\x1b[");
    for _ in 0..32 {
        flood.push_str("1;");
    }
    flood.push_str("1m");
    send(&mut c, &flood);
    send(&mut c, "X");
    assert_eq!(
        cell(&mut c, 0, 0).flags,
        Flags::empty(),
        "an overflowing CSI was acted on anyway"
    );
}

#[test]
fn a_status_query_with_no_number_is_not_a_cursor_report() {
    // DSR's parameter has no default: `\e[n` asks nothing. Answering it with a
    // cursor report puts bytes in the input stream that the program on the
    // other end never asked for, and a shell reads them as typing.
    let mut c = uncached(8, 4);
    send(&mut c, "\x1b[n");
    assert_eq!(c.pop_report(), None, "an empty DSR was answered");
}

#[test]
fn the_scroll_region_defaults_are_the_whole_screen() {
    // DECSTBM is how a full-screen app gives the screen back: `\e[r` with no
    // parameters means "no region". A top that defaults to anything but the
    // first row leaves a band frozen under every app that resets this way.
    for seq in ["\x1b[r", "\x1b[0;0r"] {
        let mut c = uncached(4, 3);
        send(&mut c, "\x1b[2;3r");
        send(&mut c, seq);
        send(&mut c, "aaaa\r\nbbbb\r\ncccc\r\n");
        assert_eq!(
            screen(&mut c),
            "bbbb|cccc|    ",
            "{:?} left a region behind",
            seq
        );
    }

    // A bottom left out, or given as a zero, is the *last* row and not row one
    // -- a region of one line is no region at all, so getting this wrong is
    // again the whole screen.
    for seq in ["\x1b[2r", "\x1b[2;0r"] {
        let mut c = uncached(4, 3);
        send(&mut c, seq);
        send(&mut c, "aaaa\r\nbbbb\r\ncccc\r\n");
        assert_eq!(
            screen(&mut c),
            "aaaa|cccc|    ",
            "{:?} is not the rows from two to the bottom",
            seq
        );
    }
}

// ---------------------------------------------------------------------------
// The SGR table
// ---------------------------------------------------------------------------

#[test]
fn every_colour_parameter_names_its_own_colour() {
    // Sixty-four lines of table, each a number and a colour, and nothing read
    // more than six of them. The two halves are the same sixteen colours with
    // a different offset, which is exactly how a copy-paste slip survives.
    let mut c = uncached(16, 2);
    for (i, (fg_param, bg_param, want)) in [
        (30, 40, NamedColor::Black),
        (31, 41, NamedColor::Red),
        (32, 42, NamedColor::Green),
        (33, 43, NamedColor::Yellow),
        (34, 44, NamedColor::Blue),
        (35, 45, NamedColor::Magenta),
        (36, 46, NamedColor::Cyan),
        (37, 47, NamedColor::White),
        (90, 100, NamedColor::BrightBlack),
        (91, 101, NamedColor::BrightRed),
        (92, 102, NamedColor::BrightGreen),
        (93, 103, NamedColor::BrightYellow),
        (94, 104, NamedColor::BrightBlue),
        (95, 105, NamedColor::BrightMagenta),
        (96, 106, NamedColor::BrightCyan),
        (97, 107, NamedColor::BrightWhite),
    ]
    .iter()
    .copied()
    .enumerate()
    {
        send(&mut c, &alloc::format!("\x1b[{};{}mX", fg_param, bg_param));
        let x = cell(&mut c, 0, i);
        assert_eq!(
            x.fg,
            Color::Named(want),
            "SGR {} is the wrong foreground",
            fg_param
        );
        assert_eq!(
            x.bg,
            Color::Named(want),
            "SGR {} is the wrong background",
            bg_param
        );
    }
}

#[test]
fn the_two_parameters_that_mean_default_read_their_own_half_of_the_template() {
    // 39 and 49 each go back to a fresh cell's colour, and the two are *not*
    // the same colour, so reading the wrong field of the template is a visible
    // wrong colour rather than a no-op.
    assert_ne!(
        Cell::default().fg,
        Cell::default().bg,
        "the two defaults are one colour, so this test proves nothing"
    );
    let mut c = uncached(8, 2);
    send(&mut c, "\x1b[31;41m\x1b[39;49mX");
    let x = cell(&mut c, 0, 0);
    assert_eq!(x.fg, Cell::default().fg, "39 is not the default foreground");
    assert_eq!(x.bg, Cell::default().bg, "49 is not the default background");
}

#[test]
fn every_flag_attribute_is_set_and_cleared_by_its_own_parameter() {
    // Each row is the parameter that turns a flag on, the one that turns it
    // off, and the flag. The cancels are the half that matters: a cancel that
    // clears the wrong flag leaves an app bold forever, and the suite only
    // ever turned the whole lot on and off together.
    for (on, off, flag) in [
        (1, 21, Flags::BOLD),
        (1, 22, Flags::BOLD),
        (2, 22, Flags::DIM),
        (3, 23, Flags::ITALIC),
        (4, 24, Flags::UNDERLINE),
        (7, 27, Flags::INVERSE),
        (8, 28, Flags::HIDDEN),
        (9, 29, Flags::STRIKEOUT),
    ] {
        let mut c = uncached(8, 2);
        send(&mut c, &alloc::format!("\x1b[{}mX", on));
        assert!(
            cell(&mut c, 0, 0).flags.contains(flag),
            "SGR {} did not set {:?}",
            on,
            flag
        );
        send(&mut c, &alloc::format!("\x1b[{}mY", off));
        assert!(
            !cell(&mut c, 0, 1).flags.contains(flag),
            "SGR {} did not clear {:?}",
            off,
            flag
        );
    }

    // `4:0` and `4:2` are the subparameter spellings of "no underline" and
    // "double underline". The second one this console does not draw, so it
    // must not quietly draw a single one instead.
    let mut c = uncached(8, 2);
    send(&mut c, "\x1b[4m\x1b[4:0mX");
    assert!(
        !cell(&mut c, 0, 0).flags.contains(Flags::UNDERLINE),
        "4:0 did not clear the underline"
    );
    let mut c = uncached(8, 2);
    send(&mut c, "\x1b[4:2mX");
    assert!(
        !cell(&mut c, 0, 0).flags.contains(Flags::UNDERLINE),
        "4:2 drew a single underline"
    );
}

#[test]
fn the_three_spellings_of_a_truecolour_mean_the_same_colour() {
    // The semicolon form is one parameter and the components after it; the two
    // colon forms are one parameter with subparameters, and the longer of them
    // carries an empty colour-space id that has to be stepped over. All three
    // are in use -- the colon forms are what a modern `ls --color` emits -- and
    // the arm that reads them is written twice, once per half of the cell.
    for spec in ["38;2;1;2;3", "38:2:1:2:3", "38:2::1:2:3"] {
        let mut c = uncached(8, 2);
        send(&mut c, &alloc::format!("\x1b[{}mX", spec));
        assert_eq!(
            cell(&mut c, 0, 0).fg,
            Color::Spec(Rgb888::new(1, 2, 3)),
            "{} is not rgb(1,2,3) in the foreground",
            spec
        );
    }
    for spec in ["48;2;1;2;3", "48:2:1:2:3", "48:2::1:2:3"] {
        let mut c = uncached(8, 2);
        send(&mut c, &alloc::format!("\x1b[{}mX", spec));
        assert_eq!(
            cell(&mut c, 0, 0).bg,
            Color::Spec(Rgb888::new(1, 2, 3)),
            "{} is not rgb(1,2,3) in the background",
            spec
        );
    }
}

#[test]
fn a_colour_number_that_does_not_fit_a_byte_is_refused() {
    // An index is one byte wide. Truncating 300 instead of refusing it paints
    // colour 44, which is a colour, so nothing downstream ever complains.
    let mut c = uncached(8, 2);
    send(&mut c, "\x1b[31m\x1b[38;5;300mX");
    assert_eq!(
        cell(&mut c, 0, 0).fg,
        Color::Named(NamedColor::Red),
        "an index wider than a byte was taken anyway"
    );
}

#[test]
fn a_colour_specifier_nobody_knows_leaves_the_colour_alone() {
    // 2 is an rgb triple and 5 is an index; everything else is reported and
    // dropped. Falling through to an index of zero would paint the cell black.
    let mut c = uncached(8, 2);
    send(&mut c, "\x1b[31m\x1b[38;9;1mX");
    assert_eq!(
        cell(&mut c, 0, 0).fg,
        Color::Named(NamedColor::Red),
        "an unknown colour specifier changed the colour"
    );
}

#[test]
fn an_unknown_sgr_parameter_changes_nothing() {
    // The table ends in a report-and-drop. Ending it in a reset instead would
    // wipe the colours an app set three sequences ago, from a parameter it
    // sent for a terminal that has it.
    let mut c = uncached(8, 2);
    send(&mut c, "\x1b[1;31m\x1b[99mX");
    let x = cell(&mut c, 0, 0);
    assert_eq!(
        x.fg,
        Color::Named(NamedColor::Red),
        "an unknown SGR reset the colour"
    );
    assert!(
        x.flags.contains(Flags::BOLD),
        "an unknown SGR reset the flags"
    );
}

// ---------------------------------------------------------------------------
// The line-drawing table
// ---------------------------------------------------------------------------

#[test]
fn every_line_drawing_letter_has_its_own_box_character() {
    // Seventeen letters, and two of them were read. Five of the seventeen all
    // mean the same horizontal line, which is what makes the table look
    // arbitrary enough to mistype: the one next to them means a vertical one.
    let mut c = uncached(1, 1);
    send(&mut c, "\x1b(0");
    for (letter, glyph) in [
        ('j', '┘'),
        ('k', '┐'),
        ('l', '┌'),
        ('m', '└'),
        ('n', '┼'),
        ('q', '─'),
        ('o', '─'),
        ('p', '─'),
        ('r', '─'),
        ('s', '─'),
        ('t', '├'),
        ('u', '┤'),
        ('v', '┴'),
        ('w', '┬'),
        ('x', '│'),
        ('a', '▒'),
        ('0', '█'),
    ] {
        send(&mut c, &alloc::format!("\r{}", letter));
        assert_eq!(
            cell(&mut c, 0, 0).c,
            glyph,
            "{} does not draw what it should",
            letter
        );
    }
}

// ---------------------------------------------------------------------------
// Scrolling, regions and the alternate screen: the rest
// ---------------------------------------------------------------------------

#[test]
fn a_linefeed_below_the_region_scrolls_nothing_and_stays_on_screen() {
    // A region does not pin the cursor into it, and `\e[r` leaves one behind
    // whose bottom is above the cursor. The row the cursor is on is then the
    // last of the screen but not the last of the region: scrolling the region
    // there would move text the app never asked to move, and walking down
    // would put the cursor on a row the buffer does not have.
    let mut c = uncached(4, 5);
    send(&mut c, "aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee");
    send(&mut c, "\x1b[2;4r\x1b[5;1H\nX");
    assert_eq!(
        screen(&mut c),
        "aaaa|bbbb|cccc|dddd|Xeee",
        "a linefeed below the region moved something"
    );
    assert_eq!(
        c.buf_mut().rejected,
        0,
        "the cursor walked off the bottom of the buffer"
    );
}

#[test]
fn scrolling_the_screen_down_moves_the_text_down() {
    // SU and SD are one letter apart and the suite only ever scrolled a region
    // by more than it holds, which clears it whichever way it went.
    let mut c = uncached(4, 4);
    send(&mut c, "aaaa\r\nbbbb\r\ncccc\r\ndddd");
    send(&mut c, "\x1b[2T");
    assert_eq!(
        screen(&mut c),
        "    |    |aaaa|bbbb",
        "SD scrolled the wrong way"
    );
    send(&mut c, "\x1b[1S");
    assert_eq!(
        screen(&mut c),
        "    |aaaa|bbbb|    ",
        "SU scrolled the wrong way"
    );
}

#[test]
fn a_screen_of_one_row_still_scrolls() {
    // With one row the top of the region and its bottom are the same number,
    // and the guard that rules out an upside-down region is one character away
    // from ruling out that one too -- which is a console that never scrolls at
    // all on a frame buffer shorter than two character cells.
    let mut c = uncached(4, 1);
    send(&mut c, "abcd\ncd");
    assert_eq!(row(&mut c, 0), "cd  ", "the only row was never blanked");
}

#[test]
fn a_full_screen_scroll_leaves_the_current_attributes_behind_it() {
    // There are two ways down: a region that is the whole screen goes through
    // the buffer's own `new_line`, which is what feeds the scrollback, and a
    // partial one goes through `scroll_region_up`. They fill the vacated row
    // from different cells -- the attribute template against its background
    // alone -- so which path ran is visible in the colour of the new row.
    let mut c = uncached(4, 2);
    send(&mut c, "\x1b[31maaaa\r\nbbbb\x1b[1S");
    assert_eq!(screen(&mut c), "bbbb|    ");
    assert_eq!(
        cell(&mut c, 1, 0).fg,
        Color::Named(NamedColor::Red),
        "the whole-screen scroll did not go through new_line"
    );
}

#[test]
fn scrolling_the_whole_screen_by_three_moves_three_rows() {
    // The whole-screen path scrolls one line at a time, in a loop over the
    // count, so the count is the one thing in it that can be dropped.
    let mut c = uncached(4, 4);
    send(&mut c, "aaaa\r\nbbbb\r\ncccc\r\ndddd");
    send(&mut c, "\x1b[3S");
    assert_eq!(screen(&mut c), "dddd|    |    |    ");
}

#[test]
fn the_buffer_shrinking_under_the_alternate_screen_is_not_a_panic() {
    // The alternate screen saves every cell of the main one and writes them
    // back on the way out. If the display mode changed in between -- which is
    // the whole reason the console asks the buffer for its size every time --
    // the saved screen is bigger than the one it is being written into, and
    // both of its dimensions have to be clamped, not just the rows.
    let mut c = uncached(4, 4);
    send(&mut c, "aaaa\r\nbbbb\r\ncccc\r\ndddd");
    send(&mut c, "\x1b[?1049h");
    c.buf_mut().resize(2, 2);
    send(&mut c, "\x1b[?1049l");
    assert_eq!(
        c.buf_mut().rejected,
        0,
        "the restore wrote outside the buffer"
    );
    assert_eq!(screen(&mut c), "aa|bb");
}

#[test]
fn opening_or_closing_lines_above_the_region_does_nothing() {
    // IL and DL are defined only inside the region. The row above it is the
    // case that gets through a guard written with the wrong connective: the
    // row below it is caught one layer down, by the buffer refusing an
    // upside-down range, but above it the range is merely *wider* than the
    // region, so the whole screen moves instead of nothing.
    let mut c = uncached(4, 4);
    send(&mut c, "aaaa\r\nbbbb\r\ncccc\r\ndddd");
    send(&mut c, "\x1b[3;4r\x1b[1;1H\x1b[L");
    assert_eq!(
        screen(&mut c),
        "aaaa|bbbb|cccc|dddd",
        "IL above the region opened a line anyway"
    );
    send(&mut c, "\x1b[M");
    assert_eq!(
        screen(&mut c),
        "aaaa|bbbb|cccc|dddd",
        "DL above the region closed a line anyway"
    );
}

#[test]
fn reverse_index_inside_the_region_just_moves_up() {
    // RI scrolls only at the top of the region; anywhere else it is a plain
    // step up, and the row right under the top is where the guard for "there
    // is a row above me" and the one for "I am at the top" overlap.
    let mut c = uncached(4, 3);
    send(&mut c, "aaaa\r\nbbbb\r\ncccc");
    send(&mut c, "\x1b[2;1H\x1bM");
    assert_eq!(c.cursor(), (0, 0), "RI did not step up");
    assert_eq!(screen(&mut c), "aaaa|bbbb|cccc", "RI scrolled from inside");
}

#[test]
fn line_wrap_can_be_turned_back_on() {
    // An app that turns wrapping off for its own drawing turns it back on when
    // it leaves. Only the off switch was ever read, so the on switch could set
    // the same flag and the symptom would be a shell that stops wrapping after
    // the first full-screen program exits.
    let mut c = uncached(4, 3);
    send(&mut c, "\x1b[?7l");
    send(&mut c, "aaaaX");
    assert_eq!(screen(&mut c), "aaaa|    |    ", "the X was not swallowed");
    send(&mut c, "\x1b[?7h");
    send(&mut c, "Y");
    assert_eq!(
        screen(&mut c),
        "aaaa|Y   |    ",
        "the wrap never came back on"
    );
}
