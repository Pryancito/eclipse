use super::expand_x_for_wc;

/// The byte range a CLFLUSH loop over `[start, start + len)` actually
/// evicts: whole 64-byte lines, so it reaches down to the line containing
/// `start` and up to the one containing the last byte.
fn flushed_lines(start: usize, len: usize) -> (usize, usize) {
    assert!(len > 0);
    (start - start % 64, (start + len).div_ceil(64) * 64)
}

/// Bytes `blit_cursor_patch` / `restore_rect` read on row `r`, given the
/// rect they were handed. Both widen x the same way before reading.
fn read_span(row: usize, stride_px: usize, x: u32, w: u32, pitch_px: u32) -> (usize, usize) {
    let (ex, ew) = expand_x_for_wc(x, w, pitch_px);
    let start = (row * stride_px + ex as usize) * 4;
    (start, start + ew as usize * 4)
}

/// Bytes the invalidate covers on row `r` for the rect it was handed.
fn sync_span(row: usize, stride_px: usize, x: u32, w: u32) -> (usize, usize) {
    let start = (row * stride_px + x as usize) * 4;
    (start, start + w as usize * 4)
}

/// Strides that are NOT a multiple of 16 pixels are the interesting ones:
/// there the row base is not 64-byte aligned, so widening x to the
/// write-combining boundary walks into cache lines the unexpanded rect
/// never touched. 1366 is the classic panel width (5464 bytes = 8 mod 16).
const STRIDES: [usize; 4] = [1366, 1367, 1376, 1920];
const CURSOR_W: u32 = 64;

#[test]
fn the_invalidate_covers_every_column_the_cursor_blit_reads() {
    for stride in STRIDES {
        let pitch_px = stride as u32;
        for x in 0..48u32 {
            for row in [0usize, 1, 2, 7, 33] {
                let (rd0, rd1) = read_span(row, stride, x, CURSOR_W, pitch_px);
                // What the fixed code invalidates: the same widened span.
                let (ex, ew) = expand_x_for_wc(x, CURSOR_W, pitch_px);
                let (sy0, sy1) = sync_span(row, stride, ex, ew);
                let (f0, f1) = flushed_lines(sy0, sy1 - sy0);
                assert!(
                    f0 <= rd0 && f1 >= rd1,
                    "stride={} x={} row={}: flushed [{},{}) does not cover read [{},{})",
                    stride,
                    x,
                    row,
                    f0,
                    f1,
                    rd0,
                    rd1
                );
            }
        }
    }
}

/// The bug this replaced: invalidating the rect as asked for, while the
/// blit reads the widened one. On a stride that is not a multiple of 16
/// pixels there are rows where the flushed lines fall short -- those are
/// the columns that came back as stale cache and got painted to screen.
#[test]
fn the_unexpanded_invalidate_left_columns_unflushed() {
    let mut short = 0;
    for stride in STRIDES {
        let pitch_px = stride as u32;
        for x in 0..48u32 {
            for row in 0..64usize {
                let (rd0, rd1) = read_span(row, stride, x, CURSOR_W, pitch_px);
                // What the old code invalidated: the rect as handed in.
                let (sy0, sy1) = sync_span(row, stride, x, CURSOR_W);
                let (f0, f1) = flushed_lines(sy0, sy1 - sy0);
                if f0 > rd0 || f1 < rd1 {
                    short += 1;
                }
            }
        }
    }
    assert!(
        short > 0,
        "expected the unexpanded invalidate to fall short somewhere; \
         if this fires, the widening is no longer load-bearing"
    );
}
