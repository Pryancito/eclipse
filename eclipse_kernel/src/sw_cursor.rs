//! Software cursor for real-hardware (non-VirtIO) EFI GOP framebuffer.
//!
//! Saves and restores the pixels behind the cursor, then draws a 16×24
//! arrow bitmap directly into the UEFI framebuffer using the kernel's
//! physical-to-virtual mapping (PHYS_MEM_OFFSET + phys_addr).
//!
//! Called from sys_set_cursor_position when no VirtIO GPU is present.

use spin::Mutex;
use crate::memory::PHYS_MEM_OFFSET;

/// Cursor bitmap dimensions in pixels
pub const CW: usize = 16;
pub const CH: usize = 24;

/// Number of rows used by the visible arrow shape (rows [ARROW_H..CH] are transparent)
const ARROW_H: usize = 16;

/// Combined cursor state and pixel save-buffer under one lock to avoid
/// potential deadlocks from acquiring two locks in sequence.
struct SoftCursor {
    x: i32,
    y: i32,
    /// True when the cursor has been drawn into the framebuffer.
    drawn: bool,
    /// Saved framebuffer pixels underneath the current cursor position.
    save: [u32; CW * CH],
}

static SOFT_CURSOR: Mutex<SoftCursor> = Mutex::new(SoftCursor {
    x: 0,
    y: 0,
    drawn: false,
    save: [0u32; CW * CH],
});

/// Returns the BGRA pixel colour for the arrow cursor at (col, row),
/// or `None` for a transparent pixel.
///
/// Arrow tip at (0, 0). Right edge column at row y is `y / 2` (1:2 slope:
/// 1 pixel right per 2 pixels down, matching the VirtIO cursor shape).
/// Black 1-pixel border; white interior; transparent outside.
#[inline(always)]
fn arrow_pixel(col: usize, row: usize) -> Option<u32> {
    if row >= ARROW_H {
        return None;
    }
    let right = row / 2;
    if col > right {
        return None;
    }
    // Black border: left edge, right diagonal, bottom row of the arrow
    let is_border = col == 0 || col == right || row == ARROW_H - 1;
    Some(if is_border { 0xFF000000 } else { 0xFFFFFFFF })
}

/// Restore the framebuffer pixels saved from a previous `draw_cursor` call.
/// Must be called with `cursor.drawn == true`.
unsafe fn erase_cursor(
    fb: *mut u32,
    pitch_px: usize,
    cx: i32,
    cy: i32,
    fb_w: i32,
    fb_h: i32,
    save: &[u32; CW * CH],
) {
    for row in 0..CH {
        let py = cy + row as i32;
        if py < 0 || py >= fb_h {
            continue;
        }
        for col in 0..CW {
            let px = cx + col as i32;
            if px < 0 || px >= fb_w {
                continue;
            }
            core::ptr::write_volatile(
                fb.add(py as usize * pitch_px + px as usize),
                save[row * CW + col],
            );
        }
    }
}

/// Save the pixels under the new cursor position, then draw the arrow bitmap.
unsafe fn draw_cursor(
    fb: *mut u32,
    pitch_px: usize,
    cx: i32,
    cy: i32,
    fb_w: i32,
    fb_h: i32,
    save: &mut [u32; CW * CH],
) {
    for row in 0..CH {
        let py = cy + row as i32;
        if py < 0 || py >= fb_h {
            continue;
        }
        for col in 0..CW {
            let px = cx + col as i32;
            if px < 0 || px >= fb_w {
                continue;
            }
            let idx = py as usize * pitch_px + px as usize;
            // Save the original pixel
            save[row * CW + col] = core::ptr::read_volatile(fb.add(idx));
            // Paint cursor pixel (only if not transparent)
            if let Some(color) = arrow_pixel(col, row) {
                core::ptr::write_volatile(fb.add(idx), color);
            }
        }
    }
}

/// Move the software cursor to `(new_x, new_y)`.
///
/// Reads framebuffer dimensions from the UEFI boot info.
/// No-op when no EFI GOP framebuffer is available (e.g. pure VirtIO boot).
pub fn update(new_x: u32, new_y: u32) {
    let bi = crate::boot::get_boot_info();
    let fi = &bi.framebuffer;

    // Skip if no valid EFI GOP framebuffer
    if fi.base_address == 0
        || fi.base_address == 0xDEAD_BEEF
        || fi.width == 0
        || fi.height == 0
        || fi.pixels_per_scan_line == 0
    {
        return;
    }

    // Resolve physical → kernel virtual address
    let fb_phys = if fi.base_address >= PHYS_MEM_OFFSET {
        fi.base_address - PHYS_MEM_OFFSET
    } else {
        fi.base_address
    };
    let fb_virt = (PHYS_MEM_OFFSET + fb_phys) as *mut u32;
    let pitch_px = fi.pixels_per_scan_line as usize;
    let fb_w = fi.width as i32;
    let fb_h = fi.height as i32;

    // Clamp requested position to screen bounds
    let nx = (new_x as i32).clamp(0, fb_w - 1);
    let ny = (new_y as i32).clamp(0, fb_h - 1);

    let mut cur = SOFT_CURSOR.lock();

    unsafe {
        // Erase cursor from its previous position
        if cur.drawn {
            erase_cursor(fb_virt, pitch_px, cur.x, cur.y, fb_w, fb_h, &cur.save);
        }
        // Draw cursor at new position (saves pixels underneath first)
        draw_cursor(fb_virt, pitch_px, nx, ny, fb_w, fb_h, &mut cur.save);
    }

    cur.x = nx;
    cur.y = ny;
    cur.drawn = true;
}
