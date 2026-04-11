//! Theming engine for Lunas desktop.
//!
//! Provides two layers of theming:
//!
//! * General UI components (panels, cards, buttons) using `StyleEngine`.
//! * Openbox / labwc-compatible **Server-Side Decoration** (SSD) colours and
//!   metrics via `SsdTheme`.  The property names mirror the `themerc` key
//!   names used by Openbox / labwc so they can easily be loaded from a config
//!   file in the future.

use crate::painter::SkiaPainter;
use tiny_skia::Color;

// ── SSD colour helper ────────────────────────────────────────────────────────

/// A packed 24-bit RGB colour used by the SSD theme (no alpha).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb24 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb24 {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Convert to the `0xFFRRGGBB` u32 format used by the framebuffer.
    pub fn to_u32(self) -> u32 {
        0xFF00_0000 | ((self.r as u32) << 16) | ((self.g as u32) << 8) | (self.b as u32)
    }
}

// ── SSD theme ────────────────────────────────────────────────────────────────

/// Openbox / labwc-compatible Server-Side Decoration theme.
///
/// Property names mirror the `themerc` convention so they can be driven
/// directly from a config file.
#[derive(Debug, Clone)]
pub struct SsdTheme {
    // ── Title bar colours ────────────────────────────────────────────────
    /// `window.active.title.bg.color`
    pub title_active_bg: Rgb24,
    /// `window.inactive.title.bg.color`
    pub title_inactive_bg: Rgb24,

    // ── Title text colours ───────────────────────────────────────────────
    /// `window.active.label.text.color`
    pub label_active_color: Rgb24,
    /// `window.inactive.label.text.color`
    pub label_inactive_color: Rgb24,

    // ── Window border colours ────────────────────────────────────────────
    /// `window.active.border.color`
    pub border_active_color: Rgb24,
    /// `window.inactive.border.color`
    pub border_inactive_color: Rgb24,

    // ── Button colours ───────────────────────────────────────────────────
    /// `window.active.button.unpressed.image.color` (close)
    pub button_close_active: Rgb24,
    /// `window.active.button.unpressed.image.color` (maximize)
    pub button_max_active: Rgb24,
    /// `window.active.button.unpressed.image.color` (minimize)
    pub button_min_active: Rgb24,
    /// `window.inactive.button.unpressed.image.color`
    pub button_inactive: Rgb24,
    /// Button hover tint colour.
    pub button_hover: Rgb24,

    // ── Metrics ──────────────────────────────────────────────────────────
    /// `border.width` — pixel width of the window border on all four sides.
    pub border_width: i32,
    /// `padding.height` — extra vertical padding inside the title bar.
    pub padding_height: i32,
    /// `window.handle.width` — pixel height of the bottom resize handle.
    pub handle_width: i32,
    /// Computed title-bar height (font_height + 2 * padding_height).
    pub titlebar_height: i32,
    /// Size (width = height) of each window button in pixels.
    pub button_size: i32,
    /// Gap between adjacent window buttons in pixels.
    pub button_gap: i32,
    /// Horizontal padding between the title-bar edge and the outermost button.
    pub button_margin: i32,
}

impl SsdTheme {
    /// labwc "default" theme — based on the Openbox "Clearlooks" look with
    /// dark colours suitable for Eclipse OS.
    pub fn labwc_default() -> Self {
        Self {
            // Dark navy title bars — active slightly lighter
            title_active_bg: Rgb24::new(37, 38, 51),
            title_inactive_bg: Rgb24::new(22, 23, 33),

            // Title text
            label_active_color: Rgb24::new(210, 215, 230),
            label_inactive_color: Rgb24::new(110, 115, 135),

            // Window borders
            border_active_color: Rgb24::new(70, 100, 160),
            border_inactive_color: Rgb24::new(40, 44, 60),

            // Buttons (traffic-light style)
            button_close_active: Rgb24::new(220, 60, 60),
            button_max_active: Rgb24::new(220, 180, 50),
            button_min_active: Rgb24::new(50, 200, 80),
            button_inactive: Rgb24::new(70, 75, 90),
            button_hover: Rgb24::new(255, 255, 255),

            // Metrics
            border_width: 1,
            padding_height: 4,
            handle_width: 6,
            titlebar_height: 24,
            button_size: 12,
            button_gap: 5,
            button_margin: 6,
        }
    }

    /// "Minimal" variant — flat monochrome style.
    pub fn minimal() -> Self {
        Self {
            title_active_bg: Rgb24::new(40, 42, 54),
            title_inactive_bg: Rgb24::new(30, 32, 42),
            label_active_color: Rgb24::new(220, 220, 220),
            label_inactive_color: Rgb24::new(100, 100, 100),
            border_active_color: Rgb24::new(100, 110, 140),
            border_inactive_color: Rgb24::new(50, 55, 70),
            button_close_active: Rgb24::new(180, 60, 60),
            button_max_active: Rgb24::new(140, 140, 140),
            button_min_active: Rgb24::new(140, 140, 140),
            button_inactive: Rgb24::new(60, 60, 60),
            button_hover: Rgb24::new(200, 200, 200),
            border_width: 1,
            padding_height: 3,
            handle_width: 4,
            titlebar_height: 22,
            button_size: 11,
            button_gap: 4,
            button_margin: 5,
        }
    }

    /// "Neon" variant — cyan / teal accent style.
    pub fn neon() -> Self {
        Self {
            title_active_bg: Rgb24::new(0, 40, 60),
            title_inactive_bg: Rgb24::new(0, 25, 40),
            label_active_color: Rgb24::new(0, 240, 220),
            label_inactive_color: Rgb24::new(0, 100, 90),
            border_active_color: Rgb24::new(0, 240, 220),
            border_inactive_color: Rgb24::new(0, 60, 55),
            button_close_active: Rgb24::new(255, 80, 80),
            button_max_active: Rgb24::new(0, 240, 220),
            button_min_active: Rgb24::new(0, 180, 160),
            button_inactive: Rgb24::new(0, 50, 45),
            button_hover: Rgb24::new(0, 255, 255),
            border_width: 1,
            padding_height: 5,
            handle_width: 6,
            titlebar_height: 26,
            button_size: 13,
            button_gap: 5,
            button_margin: 6,
        }
    }

    /// Select theme variant by index (0 = default, 1 = minimal, 2 = neon).
    pub fn by_index(idx: u8) -> Self {
        match idx {
            1 => Self::minimal(),
            2 => Self::neon(),
            _ => Self::labwc_default(),
        }
    }
}

// ── General UI style engine ──────────────────────────────────────────────────

pub struct StyleEngine {
    pub panel_bg: Color,
    pub border_color: Color,
    pub accent_color: Color,
    pub text_color: Color,
    pub text_dim_color: Color,
    pub corner_radius: f32,
    pub animation_speed: f32,
    pub dark_mode: bool,
    /// Active SSD theme for window decorations.
    pub ssd: SsdTheme,
}

impl StyleEngine {
    pub fn new() -> Self {
        Self {
            panel_bg: SkiaPainter::color(15, 18, 35, 200),
            border_color: SkiaPainter::color(255, 255, 255, 25),
            accent_color: SkiaPainter::color(0, 128, 255, 255),
            text_color: SkiaPainter::color(220, 230, 240, 255),
            text_dim_color: SkiaPainter::color(100, 120, 160, 255),
            corner_radius: 12.0,
            animation_speed: 1.0,
            dark_mode: true,
            ssd: SsdTheme::labwc_default(),
        }
    }

    pub fn draw_panel(&self, painter: &mut SkiaPainter, x: f32, y: f32, w: f32, h: f32, radius: f32) {
        painter.fill_round_rect(x, y, w, h, radius, self.panel_bg);
        painter.stroke_round_rect(x, y, w, h, radius, 1.0, self.border_color);
    }

    pub fn draw_button(&self, painter: &mut SkiaPainter, x: f32, y: f32, w: f32, h: f32, active: bool) {
        let bg = if active { self.accent_color } else { SkiaPainter::color(255, 255, 255, 20) };
        painter.fill_round_rect(x, y, w, h, 8.0, bg);
    }

    pub fn draw_card(&self, painter: &mut SkiaPainter, x: f32, y: f32, w: f32, h: f32) {
        painter.fill_round_rect(x, y, w, h, self.corner_radius, SkiaPainter::color(20, 25, 45, 220));
        painter.stroke_round_rect(x, y, w, h, self.corner_radius, 1.0, self.border_color);
    }

    pub fn draw_input_field(&self, painter: &mut SkiaPainter, x: f32, y: f32, w: f32, h: f32, focused: bool) {
        let border = if focused { self.accent_color } else { self.border_color };
        painter.fill_round_rect(x, y, w, h, 6.0, SkiaPainter::color(25, 30, 50, 240));
        painter.stroke_round_rect(x, y, w, h, 6.0, 1.5, border);
    }

    pub fn draw_separator(&self, painter: &mut SkiaPainter, x: f32, y: f32, w: f32) {
        painter.draw_line(x, y, x + w, y, 1.0, self.border_color);
    }

    /// Apply a new SSD theme by decoration-style index (0=labwc, 1=minimal, 2=neon).
    pub fn set_ssd_theme(&mut self, idx: u8) {
        self.ssd = SsdTheme::by_index(idx);
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rgb24_to_u32() {
        let c = Rgb24::new(0xFF, 0x00, 0x80);
        let u = c.to_u32();
        assert_eq!(u & 0xFF0000, 0xFF0000);
        assert_eq!(u & 0x00FF00, 0x000000);
        assert_eq!(u & 0x0000FF, 0x000080);
        assert_eq!(u >> 24, 0xFF, "alpha should be 0xFF");
    }

    #[test]
    fn test_ssd_theme_by_index() {
        let t0 = SsdTheme::by_index(0);
        let t1 = SsdTheme::by_index(1);
        let t2 = SsdTheme::by_index(2);
        let t99 = SsdTheme::by_index(99);
        // Each variant should produce different active title colours
        assert_ne!(t0.title_active_bg.r, t1.title_active_bg.r);
        assert_ne!(t1.title_active_bg.r, t2.title_active_bg.r);
        // Unknown index falls back to default
        assert_eq!(t99.title_active_bg.r, t0.title_active_bg.r);
    }

    #[test]
    fn test_style_engine_has_ssd_theme() {
        let se = StyleEngine::new();
        // Should have a non-zero titlebar_height
        assert!(se.ssd.titlebar_height > 0);
    }

    #[test]
    fn test_set_ssd_theme_changes_variant() {
        let mut se = StyleEngine::new();
        // Default theme
        assert_eq!(se.ssd.titlebar_height, SsdTheme::labwc_default().titlebar_height);
        // Switch to minimal (index 1)
        se.set_ssd_theme(1);
        let minimal = SsdTheme::minimal();
        assert_eq!(se.ssd.titlebar_height, minimal.titlebar_height);
        // Switch to neon (index 2) — has a taller title bar than default
        se.set_ssd_theme(2);
        let neon = SsdTheme::neon();
        assert_eq!(se.ssd.titlebar_height, neon.titlebar_height);
        assert!(se.ssd.titlebar_height >= 24);
    }
}
