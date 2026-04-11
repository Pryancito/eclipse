//! Alt-Tab window switcher overlay for labwc-style window cycling.
//!
//! The switcher shows a horizontal list of open windows with their titles.
//! It is activated by `Alt+Tab` / `Alt+Shift+Tab` and committed when the
//! `Alt` modifier is released.

use crate::compositor::{ShellWindow, WindowContent};
use crate::render::FramebufferState;
use embedded_graphics::{
    mono_font::{MonoTextStyle, ascii::FONT_6X12},
    pixelcolor::Rgb888,
    prelude::*,
    primitives::{Rectangle, PrimitiveStyleBuilder, Circle},
    text::Text,
};

// ── Constants ────────────────────────────────────────────────────────────────

/// Maximum number of entries shown in the switcher in one row.
const MAX_ENTRIES: usize = 16;
/// Width of each switcher entry card.
const ENTRY_W: i32 = 120;
/// Height of each switcher entry card.
const ENTRY_H: i32 = 80;
/// Horizontal gap between entry cards.
const ENTRY_GAP: i32 = 8;
/// Vertical padding above/below the switcher panel.
const PANEL_PADDING_V: i32 = 12;
/// Horizontal padding inside the panel.
const PANEL_PADDING_H: i32 = 16;

// ── State ────────────────────────────────────────────────────────────────────

/// State of the Alt-Tab window switcher.
///
/// This is kept in sync with `InputState::alt_tab_*` fields in `input.rs`.
/// The rendering function reads these fields directly from `InputState`.
#[derive(Debug, Clone, Copy)]
pub struct SwitcherState {
    pub active: bool,
    /// Index in `window_indices` that is currently highlighted.
    pub selected: usize,
    /// Ordered list of window indices to display (filled from `InputState`).
    pub window_indices: [usize; MAX_ENTRIES],
    pub window_count: usize,
}

impl SwitcherState {
    pub const fn new() -> Self {
        Self {
            active: false,
            selected: 0,
            window_indices: [0; MAX_ENTRIES],
            window_count: 0,
        }
    }
}

// ── Rendering ────────────────────────────────────────────────────────────────

/// Render the Alt-Tab switcher overlay onto the framebuffer.
///
/// `indices` — ordered list of window indices to display.
/// `selected` — index into `indices` of the currently highlighted entry.
pub fn draw_switcher(
    fb: &mut FramebufferState,
    windows: &[ShellWindow],
    indices: &[usize],
    selected: usize,
) {
    if indices.is_empty() {
        return;
    }

    let fb_w = fb.info.width as i32;
    let fb_h = fb.info.height as i32;

    let count = indices.len().min(MAX_ENTRIES);
    let total_w = count as i32 * ENTRY_W + (count as i32 - 1) * ENTRY_GAP + PANEL_PADDING_H * 2;
    let total_h = ENTRY_H + PANEL_PADDING_V * 2;

    // Centre the panel on screen
    let panel_x = ((fb_w - total_w) / 2).max(0);
    let panel_y = (fb_h / 2 - total_h / 2).max(0);

    // ── Panel background ─────────────────────────────────────────────────────
    let bg_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(20, 22, 35))
        .build();
    let _ = Rectangle::new(
        Point::new(panel_x, panel_y),
        Size::new(total_w as u32, total_h as u32),
    )
    .into_styled(bg_style)
    .draw(fb);

    // Panel border
    let border_style = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb888::new(60, 80, 140))
        .stroke_width(1)
        .build();
    let _ = Rectangle::new(
        Point::new(panel_x, panel_y),
        Size::new(total_w as u32, total_h as u32),
    )
    .into_styled(border_style)
    .draw(fb);

    // ── Entry cards ──────────────────────────────────────────────────────────
    for (slot, &win_idx) in indices[..count].iter().enumerate() {
        let ex = panel_x + PANEL_PADDING_H + slot as i32 * (ENTRY_W + ENTRY_GAP);
        let ey = panel_y + PANEL_PADDING_V;

        let is_selected = slot == selected;

        // Card background
        let card_color = if is_selected {
            Rgb888::new(40, 80, 160)
        } else {
            Rgb888::new(28, 30, 48)
        };
        let card_style = PrimitiveStyleBuilder::new().fill_color(card_color).build();
        let _ = Rectangle::new(Point::new(ex, ey), Size::new(ENTRY_W as u32, ENTRY_H as u32))
            .into_styled(card_style)
            .draw(fb);

        // Selection highlight border
        if is_selected {
            let sel_border = PrimitiveStyleBuilder::new()
                .stroke_color(Rgb888::new(100, 150, 255))
                .stroke_width(2)
                .build();
            let _ = Rectangle::new(Point::new(ex, ey), Size::new(ENTRY_W as u32, ENTRY_H as u32))
                .into_styled(sel_border)
                .draw(fb);
        }

        // Window thumbnail area (small preview or placeholder icon)
        if win_idx < windows.len() {
            draw_entry_preview(fb, &windows[win_idx], ex + 4, ey + 4, ENTRY_W - 8, ENTRY_H - 24);

            // Title text below the preview
            let title = windows[win_idx].title_str();
            let title_style = MonoTextStyle::new(
                &FONT_6X12,
                if is_selected { Rgb888::WHITE } else { Rgb888::new(180, 190, 220) },
            );
            // Truncate title to at most `max_chars` characters to fit the card width
            // (FONT_6X12 = 6 px/char).  When the title is longer than the card we
            // reserve the last position for the Unicode ellipsis '…' (U+2026, 3 bytes
            // in UTF-8) so the user can see the title was truncated.
            let max_chars = (ENTRY_W / 6) as usize;
            // Scratch buffer for the possibly-truncated title.
            let truncated_buf: alloc::string::String;
            let display: &str = if let Some((byte_idx, _)) = title.char_indices().nth(max_chars) {
                // Title is longer than the card — cut at a safe UTF-8 boundary and
                // append an ellipsis in the reserved last character slot.
                let cut = max_chars.saturating_sub(1);
                let safe_idx = title.char_indices().nth(cut).map_or(byte_idx, |(i, _)| i);
                truncated_buf = alloc::format!("{}\u{2026}", &title[..safe_idx]);
                &truncated_buf
            } else {
                // Title fits entirely — no allocation needed.
                title
            };
            let text_x = ex + 4;
            let text_y = ey + ENTRY_H - 8;
            let _ = Text::new(display, Point::new(text_x, text_y), title_style).draw(fb);
        }
    }

    // ── "Alt+Tab" hint at the bottom of the panel ────────────────────────────
    let hint_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(80, 90, 120));
    let hint = "Release Alt to focus";
    let hint_x = panel_x + (total_w - hint.len() as i32 * 6) / 2;
    let hint_y = panel_y + total_h + 14;
    if hint_y < fb_h {
        let _ = Text::new(hint, Point::new(hint_x, hint_y), hint_style).draw(fb);
    }
}

/// Draw a small window preview (or placeholder icon) in the given area.
fn draw_entry_preview(
    fb: &mut FramebufferState,
    window: &ShellWindow,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) {
    if w <= 0 || h <= 0 {
        return;
    }

    // Placeholder: draw a miniature "window frame" icon
    let bg_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(15, 20, 35))
        .build();
    let _ = Rectangle::new(Point::new(x, y), Size::new(w as u32, h as u32))
        .into_styled(bg_style)
        .draw(fb);

    // Mini title bar
    let tb_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(37, 38, 51))
        .build();
    let tb_h = (h / 5).max(4);
    let _ = Rectangle::new(Point::new(x, y), Size::new(w as u32, tb_h as u32))
        .into_styled(tb_style)
        .draw(fb);

    // Mini close button dot
    let dot_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(220, 60, 60))
        .build();
    let _ = Circle::new(Point::new(x + w - 6, y + 2), 4)
        .into_styled(dot_style)
        .draw(fb);

    // Content area placeholder lines
    if window.content != WindowContent::None {
        let line_style = PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::new(40, 50, 80))
            .build();
        let line_y_start = y + tb_h + 3;
        let line_spacing = (h - tb_h - 6) / 4;
        for i in 0..3i32 {
            let ly = line_y_start + i * line_spacing;
            if ly >= y + h { break; }
            let lw = if i == 2 { w * 2 / 3 } else { w - 4 };
            let _ = Rectangle::new(Point::new(x + 2, ly), Size::new(lw as u32, 2))
                .into_styled(line_style)
                .draw(fb);
        }
    }

    // Window frame border
    let frame_style = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb888::new(50, 55, 75))
        .stroke_width(1)
        .build();
    let _ = Rectangle::new(Point::new(x, y), Size::new(w as u32, h as u32))
        .into_styled(frame_style)
        .draw(fb);
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_switcher_state_defaults() {
        let s = SwitcherState::new();
        assert!(!s.active);
        assert_eq!(s.window_count, 0);
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn test_max_entries_constant() {
        assert_eq!(MAX_ENTRIES, 16);
    }
}
