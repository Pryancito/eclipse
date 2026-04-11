//! Window rules — apply per-application placement and state rules when a new
//! window is mapped.
//!
//! Rules are defined in `LabwcConfig::window_rules` (populated from the
//! `<windowRules>` section of `~/.config/lunas/rc.xml`).  When a new
//! `ShellWindow` is created the compositor calls `apply_rules` to adjust its
//! initial position, size, and state before inserting it into the window list.

use crate::config::LabwcConfig;
use crate::compositor::ShellWindow;

/// Apply all matching window rules from `config` to `window`.
///
/// Rules are evaluated in order; the **last** matching rule wins for each
/// attribute (same semantics as labwc / Openbox).
///
/// # Arguments
/// * `config`  — Active labwc configuration.
/// * `window`  — Newly-created window to mutate.
/// * `fb_w`    — Framebuffer width in pixels (used to clamp positions).
/// * `fb_h`    — Framebuffer height in pixels.
pub fn apply_rules(config: &LabwcConfig, window: &mut ShellWindow, fb_w: i32, fb_h: i32) {
    // Decode the window title from the fixed-length byte array.
    let title_len = window.title.iter().position(|&b| b == 0).unwrap_or(window.title.len());
    let title = core::str::from_utf8(&window.title[..title_len]).unwrap_or("");

    let (new_x, new_y, new_w, new_h, maximized, _fullscreen) =
        config.apply_window_rules(title, window.x, window.y, window.w, window.h);

    // Apply size first (so position clamping uses the updated dimensions).
    if new_w != window.w || new_h != window.h {
        window.w = new_w.max(config.min_window_width);
        window.h = new_h.max(config.min_window_height);
    }

    // Apply position after size so the clamp range is correct.
    if new_x != window.x || new_y != window.y {
        window.x = new_x.max(0).min((fb_w - window.w).max(0));
        window.y = new_y
            .max(ShellWindow::TITLE_H)
            .min((fb_h - window.h).max(ShellWindow::TITLE_H));
    }

    if maximized && !window.maximized {
        // Maximise to the full usable area (below title bar, above taskbar).
        let taskbar_h = crate::render::TASKBAR_HEIGHT;
        window.stored_rect = (window.x, window.y, window.w, window.h);
        window.x = 0;
        window.y = 0;
        window.w = fb_w;
        window.h = fb_h - taskbar_h;
        window.maximized = true;
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::WindowContent;

    fn default_window() -> ShellWindow {
        ShellWindow {
            x: 50, y: 50, w: 640, h: 480,
            content: WindowContent::InternalDemo,
            ..Default::default()
        }
    }

    #[test]
    fn test_no_rules_window_unchanged() {
        let config = LabwcConfig::load();
        let mut win = default_window();
        apply_rules(&config, &mut win, 1920, 1080);
        // With no rules the position/size should be unchanged.
        assert_eq!(win.x, 50);
        assert_eq!(win.y, 50);
        assert_eq!(win.w, 640);
        assert_eq!(win.h, 480);
        assert!(!win.maximized);
    }

    #[test]
    fn test_maximized_rule_sets_maximized_flag() {
        use crate::config::{WindowRule, LabwcConfig};
        let mut config = LabwcConfig::load();
        config.window_rules.push(WindowRule {
            identifier: heapless::String::new(), // matches all
            title: heapless::String::new(),
            maximized: true,
            fullscreen: false,
            floating: false,
            x: None,
            y: None,
            width: None,
            height: None,
        });
        let mut win = default_window();
        apply_rules(&config, &mut win, 1920, 1080);
        assert!(win.maximized);
        assert_eq!(win.x, 0);
        assert_eq!(win.y, 0);
    }
}
