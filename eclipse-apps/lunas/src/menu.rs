//! Root (desktop) menu and window action menu for labwc-style operation.
//!
//! Both menus are built on top of the existing `ContextMenu` / `ContextAction`
//! infrastructure already present in `input.rs`.  This module provides builder
//! functions that populate a `ContextMenu` with the appropriate items.

use crate::input::{ContextMenu, ContextAction};
use crate::compositor::ShellWindow;

// ── Root / Desktop menu ──────────────────────────────────────────────────────

/// Populate `menu` with the labwc-style **root menu** entries shown when the
/// user right-clicks on the empty desktop.
///
/// The items mirror the default `menu.xml` shipped with labwc.
pub fn build_root_menu(menu: &mut ContextMenu, x: i32, y: i32, fb_w: i32, fb_h: i32) {
    menu.show(x, y);
    menu.add_item("New Window",     ContextAction::NewWindow);
    menu.add_separator();
    menu.add_item("Change Wallpaper", ContextAction::CycleWallpaper);
    menu.add_separator();
    menu.add_item("Show Desktop",   ContextAction::ShowDesktop);
    menu.add_separator();
    menu.add_item("Launcher",       ContextAction::ToggleLauncher);
    menu.add_separator();
    menu.add_item("Exit",           ContextAction::ShowDesktop); // placeholder — labwc Exit action
    menu.clamp_to_screen(fb_w, fb_h);
}

// ── Window action menu ───────────────────────────────────────────────────────

/// Populate `menu` with the labwc / Openbox-style **window action menu** for
/// the window at `window_idx`.
///
/// Shown when the user right-clicks the title bar or presses `Alt+Space`.
pub fn build_window_menu(
    menu: &mut ContextMenu,
    x: i32,
    y: i32,
    window_idx: usize,
    window: &ShellWindow,
    fb_w: i32,
    fb_h: i32,
) {
    menu.show(x, y);

    // Maximize — checked when already maximized
    menu.add_checked_item(
        "Maximize",
        ContextAction::MaximizeWindow(window_idx),
        window.maximized,
    );

    // Minimize / Iconify
    menu.add_item("Minimize", ContextAction::MinimizeWindow(window_idx));

    // Shade (roll up to title bar) — checked when shaded
    menu.add_checked_item(
        "Shade",
        ContextAction::ShadeWindow(window_idx),
        window.shaded,
    );

    menu.add_separator();

    // Move / Resize (triggers interactive mode via pending action)
    menu.add_item("Move",   ContextAction::MoveWindow(window_idx));
    menu.add_item("Resize", ContextAction::ResizeWindow(window_idx));

    menu.add_separator();

    // Always on top — checked when active
    menu.add_checked_item(
        "Always on Top",
        ContextAction::ToggleAlwaysOnTop(window_idx),
        window.above,
    );

    menu.add_separator();

    // Close
    menu.add_item("Close", ContextAction::CloseWindow(window_idx));

    menu.clamp_to_screen(fb_w, fb_h);
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::WindowContent;

    fn make_menu() -> ContextMenu {
        ContextMenu::new()
    }

    #[test]
    fn test_root_menu_has_items() {
        let mut m = make_menu();
        build_root_menu(&mut m, 100, 100, 1920, 1080);
        assert!(m.visible);
        assert!(m.item_count > 0);
        // First item should be NewWindow
        assert_eq!(m.items[0].action, ContextAction::NewWindow);
    }

    #[test]
    fn test_window_menu_has_close() {
        let win = ShellWindow {
            x: 100, y: 100, w: 400, h: 300,
            content: WindowContent::InternalDemo,
            ..Default::default()
        };
        let mut m = make_menu();
        build_window_menu(&mut m, 100, 100, 0, &win, 1920, 1080);
        assert!(m.visible);
        let has_close = (0..m.item_count).any(|i| {
            matches!(m.items[i].action, ContextAction::CloseWindow(_))
        });
        assert!(has_close, "window menu should contain Close action");
    }

    #[test]
    fn test_window_menu_shade_checked_when_shaded() {
        let mut win = ShellWindow {
            x: 0, y: 0, w: 400, h: 300,
            content: WindowContent::InternalDemo,
            ..Default::default()
        };
        win.shaded = true;

        let mut m = make_menu();
        build_window_menu(&mut m, 0, 0, 0, &win, 1920, 1080);
        let shade_item = (0..m.item_count)
            .find(|&i| matches!(m.items[i].action, ContextAction::ShadeWindow(_)));
        assert!(shade_item.is_some(), "Shade item should be present");
        assert!(m.items[shade_item.unwrap()].checked, "Shade should be checked when window is shaded");
    }

    #[test]
    fn test_window_menu_maximize_unchecked_when_not_maximized() {
        let win = ShellWindow {
            x: 0, y: 0, w: 400, h: 300,
            content: WindowContent::InternalDemo,
            ..Default::default()
        };
        let mut m = make_menu();
        build_window_menu(&mut m, 0, 0, 0, &win, 1920, 1080);
        let max_item = (0..m.item_count)
            .find(|&i| matches!(m.items[i].action, ContextAction::MaximizeWindow(_)));
        assert!(max_item.is_some());
        assert!(!m.items[max_item.unwrap()].checked);
    }

    #[test]
    fn test_root_menu_clamped_to_screen() {
        let mut m = make_menu();
        // Place near the right/bottom edge — should be clamped
        build_root_menu(&mut m, 1900, 1060, 1920, 1080);
        assert!(m.x >= 0);
        assert!(m.y >= 0);
        assert!(m.x + crate::render::CONTEXT_MENU_W <= 1920);
    }
}
