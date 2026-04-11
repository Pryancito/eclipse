//! labwc-style compositor configuration.
//!
//! Provides configuration structures analogous to labwc's rc.xml / menu.xml,
//! with defaults that match labwc's out-of-the-box behaviour.

use std::prelude::v1::*;

// ── Button identifiers ──────────────────────────────────────────────────────

/// Window button identifiers used in the button layout string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonId {
    None,
    /// Close button.
    Close,
    /// Maximize / restore button.
    Maximize,
    /// Minimize / iconify button.
    Minimize,
    /// Shade (roll-up) button.
    Shade,
    /// Window-menu button (sometimes called the "frame menu" button).
    WindowMenu,
}

// ── Button layout ───────────────────────────────────────────────────────────

/// Describes which window buttons appear on the left and right sides of the
/// title bar, and in what order.
///
/// Analogous to the `<theme><titleLayout>` setting in labwc (which mirrors
/// Openbox's button layout string format).
///
/// labwc default: nothing on the left, Close + Maximize + Minimize on the right.
#[derive(Debug, Clone, Copy)]
pub struct ButtonLayout {
    pub left_order: [ButtonId; 4],
    pub left_count: usize,
    pub right_order: [ButtonId; 4],
    pub right_count: usize,
}

impl ButtonLayout {
    /// labwc / Openbox default: `":"CMI"` — no left buttons,
    /// Close then Maximize then Minimize on the right.
    pub fn labwc_default() -> Self {
        Self {
            left_order: [ButtonId::None; 4],
            left_count: 0,
            right_order: [
                ButtonId::Close,
                ButtonId::Maximize,
                ButtonId::Minimize,
                ButtonId::None,
            ],
            right_count: 3,
        }
    }

    /// macOS / GNOME-style: Minimize + Maximize + Close on the left.
    pub fn left_cmi() -> Self {
        Self {
            left_order: [
                ButtonId::Close,
                ButtonId::Maximize,
                ButtonId::Minimize,
                ButtonId::None,
            ],
            left_count: 3,
            right_order: [ButtonId::None; 4],
            right_count: 0,
        }
    }
}

// ── Focus policy ────────────────────────────────────────────────────────────

/// Window focus policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusPolicy {
    /// Window must be clicked to receive focus (labwc default).
    Click,
    /// Focus follows the mouse pointer.
    FollowMouse,
}

// ── Title alignment ─────────────────────────────────────────────────────────

/// Alignment of the window title text within the title bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleAlign {
    Left,
    Center,
    Right,
}

// ── Keyboard / mouse bindings ───────────────────────────────────────────────

/// Actions that can be triggered by key or mouse bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindAction {
    /// Close the focused window (default: A-F4).
    Close,
    /// Toggle maximize / restore.
    Maximize,
    /// Minimize / iconify the window.
    Iconify,
    /// Toggle shade (roll window up to titlebar only).
    Shade,
    /// Toggle fullscreen.
    Fullscreen,
    /// Begin interactive move.
    Move,
    /// Begin interactive resize.
    Resize,
    /// Raise window to top of stack.
    Raise,
    /// Focus (or raise) the window under the cursor.
    Focus,
    /// Toggle always-on-top.
    ToggleAlwaysOnTop,
    /// Switch to Alt-Tab window list (forward).
    NextWindow,
    /// Switch to Alt-Tab window list (backward).
    PrevWindow,
    /// Show the window action menu (Alt+Space in Openbox / labwc).
    ShowWindowMenu,
    /// Show the root / desktop menu.
    ShowMenu,
    /// Snap window to left half.
    SnapLeft,
    /// Snap window to right half.
    SnapRight,
    /// Snap to top-left quarter.
    SnapTopLeft,
    /// Snap to top-right quarter.
    SnapTopRight,
    /// Snap to bottom-left quarter.
    SnapBottomLeft,
    /// Snap to bottom-right quarter.
    SnapBottomRight,
    /// Toggle show-desktop mode.
    ShowDesktop,
    /// Open the application launcher.
    ToggleLauncher,
    /// Switch to workspace N (0-indexed).
    GoToWorkspace(u8),
    /// Move focused window to workspace N.
    MoveToWorkspace(u8),
    /// Reconfigure the compositor.
    Reconfigure,
    /// Exit the compositor.
    Exit,
}

/// A keyboard shortcut binding.
#[derive(Debug, Clone, Copy)]
pub struct KeyBinding {
    /// PS/2 scancode for the key (without break bit).
    pub scancode: u8,
    /// Required modifier bitmask (bit 0=Shift, 1=Ctrl, 2=Alt, 3=Super).
    pub modifiers: u8,
    /// Action to execute when this binding fires.
    pub action: BindAction,
}

/// Mouse button codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    ScrollUp,
    ScrollDown,
}

/// Where a mouse binding applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseContext {
    /// Empty desktop area (no window under cursor).
    Root,
    /// On a window's title bar.
    Titlebar,
    /// On a window's content area.
    Client,
    /// On a window's resize border.
    Border,
    /// On the bottom resize handle.
    Handle,
}

/// A mouse binding.
#[derive(Debug, Clone, Copy)]
pub struct MouseBinding {
    pub button: MouseButton,
    pub context: MouseContext,
    /// Modifier bitmask (same encoding as KeyBinding::modifiers).
    pub modifiers: u8,
    pub action: BindAction,
}

// ── Menu items ───────────────────────────────────────────────────────────────

/// Action associated with a desktop-menu entry.
#[derive(Debug, Clone)]
pub enum MenuItemAction {
    /// Launch an application (exec path).
    Execute(heapless::String<64>),
    /// Visual separator line.
    Separator,
    /// Reconfigure the compositor.
    Reconfigure,
    /// Exit / logout.
    Exit,
}

/// One entry in the desktop / application menu.
#[derive(Debug, Clone)]
pub struct MenuItem {
    pub label: heapless::String<32>,
    pub action: MenuItemAction,
}

// ── Window rules ─────────────────────────────────────────────────────────────

/// Per-window placement and state rules applied at map time.
#[derive(Debug, Clone)]
pub struct WindowRule {
    /// Application identifier to match (empty = match any).
    pub identifier: heapless::String<32>,
    /// Optional title substring match (empty = match any).
    pub title: heapless::String<32>,
    /// Override initial x position.
    pub x: Option<i32>,
    /// Override initial y position.
    pub y: Option<i32>,
    /// Override initial width.
    pub width: Option<i32>,
    /// Override initial height.
    pub height: Option<i32>,
    /// Start maximized.
    pub maximized: bool,
    /// Start fullscreen.
    pub fullscreen: bool,
    /// Force floating (stacking) mode.
    pub floating: bool,
}

// ── Virtual desktops ─────────────────────────────────────────────────────────

/// Virtual desktop / workspace configuration.
#[derive(Debug, Clone, Copy)]
pub struct DesktopsConfig {
    /// Number of virtual desktops (labwc default: 1, we use 4 for Eclipse).
    pub count: u8,
    /// Desktop to activate at startup (0-indexed).
    pub start_desktop: u8,
}

impl Default for DesktopsConfig {
    fn default() -> Self {
        Self {
            count: 4,
            start_desktop: 0,
        }
    }
}

// ── Main configuration struct ────────────────────────────────────────────────

/// Full compositor configuration (analogous to labwc's rc.xml + menu.xml).
///
/// Created via `LabwcConfig::default_labwc()` which populates labwc-compatible
/// defaults.  In the future this struct can be populated by parsing
/// `~/.config/lunas/rc.xml` and `~/.config/lunas/menu.xml`.
pub struct LabwcConfig {
    /// Virtual desktop configuration.
    pub desktops: DesktopsConfig,
    /// Window title text alignment.
    pub title_align: TitleAlign,
    /// Window focus policy.
    pub focus_policy: FocusPolicy,
    /// Whether a window is raised to the top when it receives focus.
    pub raise_on_focus: bool,
    /// Pixel distance from screen edge that triggers snap-zone highlighting.
    pub snap_edge_threshold: i32,
    /// Minimum allowed window width in pixels.
    pub min_window_width: i32,
    /// Minimum allowed window height in pixels.
    pub min_window_height: i32,
    /// Title-bar button layout.
    pub button_layout: ButtonLayout,
    /// Per-window placement/state rules.
    pub window_rules: Vec<WindowRule>,
    /// Desktop right-click root menu entries.
    pub root_menu: Vec<MenuItem>,
    /// Keyboard bindings.
    pub key_bindings: Vec<KeyBinding>,
    /// Mouse bindings.
    pub mouse_bindings: Vec<MouseBinding>,
}

impl LabwcConfig {
    /// Create a configuration with labwc-compatible defaults.
    pub fn default_labwc() -> Self {
        // ── Root menu (equivalent to labwc's default menu.xml) ──
        let mut root_menu: Vec<MenuItem> = Vec::new();

        root_menu.push(MenuItem {
            label: {
                let mut s = heapless::String::new();
                let _ = s.push_str("Terminal");
                s
            },
            action: MenuItemAction::Execute({
                let mut s = heapless::String::new();
                let _ = s.push_str("/bin/terminal");
                s
            }),
        });

        root_menu.push(MenuItem {
            label: heapless::String::new(),
            action: MenuItemAction::Separator,
        });

        root_menu.push(MenuItem {
            label: {
                let mut s = heapless::String::new();
                let _ = s.push_str("Reconfigure");
                s
            },
            action: MenuItemAction::Reconfigure,
        });

        root_menu.push(MenuItem {
            label: {
                let mut s = heapless::String::new();
                let _ = s.push_str("Exit");
                s
            },
            action: MenuItemAction::Exit,
        });

        // ── Keyboard bindings (labwc defaults) ──
        let key_bindings: Vec<KeyBinding> = vec![
            // A-F4 → Close
            KeyBinding { scancode: 0x3E, modifiers: 0b0100, action: BindAction::Close },
            // A-Tab → NextWindow
            KeyBinding { scancode: 0x0F, modifiers: 0b0100, action: BindAction::NextWindow },
            // A-Shift-Tab → PrevWindow
            KeyBinding { scancode: 0x0F, modifiers: 0b0101, action: BindAction::PrevWindow },
            // A-Space → ShowWindowMenu
            KeyBinding { scancode: 0x39, modifiers: 0b0100, action: BindAction::ShowWindowMenu },
            // Super-Left → SnapLeft
            KeyBinding { scancode: 0x4B, modifiers: 0b1000, action: BindAction::SnapLeft },
            // Super-Right → SnapRight
            KeyBinding { scancode: 0x4D, modifiers: 0b1000, action: BindAction::SnapRight },
            // Super-Up → Maximize
            KeyBinding { scancode: 0x48, modifiers: 0b1000, action: BindAction::Maximize },
            // Super-Down → Iconify (when not maximized) or restore
            KeyBinding { scancode: 0x50, modifiers: 0b1000, action: BindAction::Iconify },
            // Super-D → ShowDesktop
            KeyBinding { scancode: 0x20, modifiers: 0b1000, action: BindAction::ShowDesktop },
            // Super-1..4 → GoToWorkspace
            KeyBinding { scancode: 0x02, modifiers: 0b1000, action: BindAction::GoToWorkspace(0) },
            KeyBinding { scancode: 0x03, modifiers: 0b1000, action: BindAction::GoToWorkspace(1) },
            KeyBinding { scancode: 0x04, modifiers: 0b1000, action: BindAction::GoToWorkspace(2) },
            KeyBinding { scancode: 0x05, modifiers: 0b1000, action: BindAction::GoToWorkspace(3) },
        ];

        // ── Mouse bindings (labwc defaults) ──
        let mouse_bindings: Vec<MouseBinding> = vec![
            // Left-drag titlebar → Move
            MouseBinding {
                button: MouseButton::Left,
                context: MouseContext::Titlebar,
                modifiers: 0,
                action: BindAction::Move,
            },
            // Right-click titlebar → ShowWindowMenu
            MouseBinding {
                button: MouseButton::Right,
                context: MouseContext::Titlebar,
                modifiers: 0,
                action: BindAction::ShowWindowMenu,
            },
            // Left-drag border → Resize
            MouseBinding {
                button: MouseButton::Left,
                context: MouseContext::Border,
                modifiers: 0,
                action: BindAction::Resize,
            },
            // Left-drag handle → Resize
            MouseBinding {
                button: MouseButton::Left,
                context: MouseContext::Handle,
                modifiers: 0,
                action: BindAction::Resize,
            },
            // Right-click desktop → ShowMenu
            MouseBinding {
                button: MouseButton::Right,
                context: MouseContext::Root,
                modifiers: 0,
                action: BindAction::ShowMenu,
            },
            // Alt + Left-drag → Move (on any part of the window)
            MouseBinding {
                button: MouseButton::Left,
                context: MouseContext::Client,
                modifiers: 0b0100, // Alt
                action: BindAction::Move,
            },
            // Alt + Right-drag → Resize (on any part of the window)
            MouseBinding {
                button: MouseButton::Right,
                context: MouseContext::Client,
                modifiers: 0b0100, // Alt
                action: BindAction::Resize,
            },
        ];

        Self {
            desktops: DesktopsConfig::default(),
            title_align: TitleAlign::Left,
            focus_policy: FocusPolicy::Click,
            raise_on_focus: false,
            snap_edge_threshold: 8,
            min_window_width: 100,
            min_window_height: 40,
            button_layout: ButtonLayout::labwc_default(),
            window_rules: Vec::new(),
            root_menu,
            key_bindings,
            mouse_bindings,
        }
    }

    /// Apply a window rule matching `title` to fill in initial placement for a new window.
    /// Returns `Some((x, y, w, h, maximized, fullscreen))` if a matching rule exists.
    pub fn apply_window_rules(
        &self,
        title: &str,
        default_x: i32,
        default_y: i32,
        default_w: i32,
        default_h: i32,
    ) -> (i32, i32, i32, i32, bool, bool) {
        for rule in &self.window_rules {
            let id_match = rule.identifier.is_empty()
                || title
                    .len()
                    .min(rule.identifier.len())
                    .eq(&rule.identifier.len())
                    && title[..rule.identifier.len()]
                        .eq_ignore_ascii_case(rule.identifier.as_str());
            if !id_match {
                continue;
            }
            let title_match = rule.title.is_empty()
                || title.contains(rule.title.as_str());
            if !title_match {
                continue;
            }
            return (
                rule.x.unwrap_or(default_x),
                rule.y.unwrap_or(default_y),
                rule.width.unwrap_or(default_w),
                rule.height.unwrap_or(default_h),
                rule.maximized,
                rule.fullscreen,
            );
        }
        (default_x, default_y, default_w, default_h, false, false)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_has_root_menu() {
        let cfg = LabwcConfig::default_labwc();
        assert!(!cfg.root_menu.is_empty(), "root menu should not be empty");
    }

    #[test]
    fn test_default_button_layout_is_labwc() {
        let layout = ButtonLayout::labwc_default();
        assert_eq!(layout.left_count, 0, "labwc default: no left buttons");
        assert_eq!(layout.right_count, 3, "labwc default: 3 right buttons");
        assert_eq!(layout.right_order[0], ButtonId::Close);
        assert_eq!(layout.right_order[1], ButtonId::Maximize);
        assert_eq!(layout.right_order[2], ButtonId::Minimize);
    }

    #[test]
    fn test_left_cmi_layout() {
        let layout = ButtonLayout::left_cmi();
        assert_eq!(layout.left_count, 3);
        assert_eq!(layout.right_count, 0);
        assert_eq!(layout.left_order[0], ButtonId::Close);
    }

    #[test]
    fn test_default_has_key_bindings() {
        let cfg = LabwcConfig::default_labwc();
        // Alt+F4 should close
        let af4 = cfg.key_bindings.iter().find(|k| k.scancode == 0x3E && k.modifiers == 0b0100);
        assert!(af4.is_some(), "Alt+F4 binding should exist");
        assert_eq!(af4.unwrap().action, BindAction::Close);
    }

    #[test]
    fn test_window_rule_no_match_returns_defaults() {
        let cfg = LabwcConfig::default_labwc();
        let (x, y, w, h, max, fs) = cfg.apply_window_rules("SomeApp", 10, 20, 640, 480);
        assert_eq!(x, 10);
        assert_eq!(y, 20);
        assert_eq!(w, 640);
        assert_eq!(h, 480);
        assert!(!max);
        assert!(!fs);
    }
}
