//! Desktop shell features for Lunas — taskbar, app launcher, wallpaper,
//! notifications, and system tray management.

use std::prelude::v1::*;

/// Maximum number of notifications stored.
pub const MAX_NOTIFICATIONS: usize = 16;

/// Maximum number of pinned apps in the taskbar.
pub const MAX_PINNED_APPS: usize = 8;

/// A desktop notification.
#[derive(Clone)]
pub struct Notification {
    pub message: [u8; 64],
    pub priority: u8,
    pub timestamp: u64,
    pub read: bool,
}

impl Default for Notification {
    fn default() -> Self {
        Self {
            message: [0; 64],
            priority: 0,
            timestamp: 0,
            read: false,
        }
    }
}

impl Notification {
    pub fn new(msg: &str, priority: u8) -> Self {
        let mut n = Self::default();
        let bytes = msg.as_bytes();
        let len = bytes.len().min(64);
        n.message[..len].copy_from_slice(&bytes[..len]);
        n.priority = priority;
        n
    }

    pub fn message_str(&self) -> &str {
        let len = self.message.iter().position(|&b| b == 0).unwrap_or(64);
        core::str::from_utf8(&self.message[..len]).unwrap_or("")
    }
}

/// A pinned application in the taskbar.
#[derive(Clone)]
pub struct PinnedApp {
    pub name: [u8; 32],
    pub icon_color: (u8, u8, u8),
    pub running: bool,
    /// Executable path for launching this app (e.g. "/bin/terminal").
    pub exec_path: [u8; 64],
}

impl Default for PinnedApp {
    fn default() -> Self {
        Self {
            name: [0; 32],
            icon_color: (100, 100, 100),
            running: false,
            exec_path: [0; 64],
        }
    }
}

impl PinnedApp {
    pub fn new(name: &str, r: u8, g: u8, b: u8) -> Self {
        let mut app = Self::default();
        let bytes = name.as_bytes();
        let len = bytes.len().min(32);
        app.name[..len].copy_from_slice(&bytes[..len]);
        app.icon_color = (r, g, b);
        app
    }

    /// Create a pinned app with name, color, and executable path.
    pub fn with_exec(name: &str, r: u8, g: u8, b: u8, exec: &str) -> Self {
        let mut app = Self::new(name, r, g, b);
        let bytes = exec.as_bytes();
        let len = bytes.len().min(64);
        app.exec_path[..len].copy_from_slice(&bytes[..len]);
        app
    }

    pub fn name_str(&self) -> &str {
        let len = self.name.iter().position(|&b| b == 0).unwrap_or(32);
        core::str::from_utf8(&self.name[..len]).unwrap_or("")
    }

    /// Return the executable path as a string slice.
    pub fn exec_path_str(&self) -> &str {
        let len = self.exec_path.iter().position(|&b| b == 0).unwrap_or(64);
        core::str::from_utf8(&self.exec_path[..len]).unwrap_or("")
    }
}

/// Wallpaper mode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WallpaperMode {
    SolidColor,
    Gradient,
    CosmicTheme,
}

/// Desktop shell state — manages taskbar, notifications, launcher, and wallpaper.
pub struct DesktopShell {
    pub pinned_apps: [PinnedApp; MAX_PINNED_APPS],
    pub pinned_count: usize,
    pub notifications: [Notification; MAX_NOTIFICATIONS],
    pub notification_count: usize,
    pub wallpaper_mode: WallpaperMode,
    pub wallpaper_color: (u8, u8, u8),
    pub show_clock: bool,
    pub show_battery: bool,
    pub volume_level: u8,
    pub volume_muted: bool,
    pub brightness_level: u8,
    /// Current battery level (0-100). 0 = empty, 100 = full.
    pub battery_level: u8,
    /// Whether the device is currently charging.
    pub battery_charging: bool,
    /// Current clock hours (0-23), updated from system time.
    pub clock_hours: u8,
    /// Current clock minutes (0-59), updated from system time.
    pub clock_minutes: u8,
    /// Current day of month (1-31).
    pub clock_day: u8,
    /// Current month (1-12).
    pub clock_month: u8,
    /// Current year (e.g. 2026).
    pub clock_year: u16,
    /// Do Not Disturb mode — when active, incoming notifications are suppressed visually.
    pub do_not_disturb: bool,
    /// Night Light mode — when active, a warm tint is applied to reduce blue light.
    pub night_light_active: bool,
}

impl DesktopShell {
    pub fn new() -> Self {
        let mut shell = Self {
            pinned_apps: core::array::from_fn(|_| PinnedApp::default()),
            pinned_count: 0,
            notifications: core::array::from_fn(|_| Notification::default()),
            notification_count: 0,
            wallpaper_mode: WallpaperMode::CosmicTheme,
            wallpaper_color: (10, 15, 30),
            show_clock: true,
            show_battery: true,
            volume_level: 75,
            volume_muted: false,
            brightness_level: 100,
            battery_level: 80,
            battery_charging: false,
            clock_hours: 0,
            clock_minutes: 0,
            clock_day: 1,
            clock_month: 1,
            clock_year: 2026,
            do_not_disturb: false,
            night_light_active: false,
        };

        // Default pinned apps with executable paths
        shell.pin_app_with_exec("Terminal", 0, 200, 100, "/bin/terminal");
        shell.pin_app_with_exec("Files", 100, 150, 255, "/bin/files");
        shell.pin_app_with_exec("Editor", 200, 160, 50, "/bin/editor");
        shell.pin_app_with_exec("Browser", 255, 100, 50, "/bin/browser");
        shell.pin_app_with_exec("Settings", 150, 150, 150, "/bin/settings");

        shell
    }

    pub fn pin_app(&mut self, name: &str, r: u8, g: u8, b: u8) {
        if self.pinned_count < MAX_PINNED_APPS {
            self.pinned_apps[self.pinned_count] = PinnedApp::new(name, r, g, b);
            self.pinned_count += 1;
        }
    }

    pub fn pin_app_with_exec(&mut self, name: &str, r: u8, g: u8, b: u8, exec: &str) {
        if self.pinned_count < MAX_PINNED_APPS {
            self.pinned_apps[self.pinned_count] = PinnedApp::with_exec(name, r, g, b, exec);
            self.pinned_count += 1;
        }
    }

    /// Remove a pinned app by index, shifting remaining apps down.
    /// Does nothing if the index is out of range.
    pub fn unpin_app(&mut self, idx: usize) {
        if idx >= self.pinned_count {
            return;
        }
        for i in idx..(self.pinned_count - 1) {
            self.pinned_apps[i] = self.pinned_apps[i + 1].clone();
        }
        self.pinned_apps[self.pinned_count - 1] = PinnedApp::default();
        self.pinned_count -= 1;
    }

    /// Swap two pinned apps by index (drag-and-drop reorder).
    /// Does nothing if either index is out of range or they are the same.
    pub fn swap_pinned_apps(&mut self, a: usize, b: usize) {
        if a != b && a < self.pinned_count && b < self.pinned_count {
            self.pinned_apps.swap(a, b);
        }
    }

    pub fn push_notification(&mut self, msg: &str, priority: u8) {
        if self.notification_count < MAX_NOTIFICATIONS {
            self.notifications[self.notification_count] = Notification::new(msg, priority);
            self.notification_count += 1;
        } else {
            // Shift notifications and add at end
            for i in 0..(MAX_NOTIFICATIONS - 1) {
                self.notifications[i] = self.notifications[i + 1].clone();
            }
            self.notifications[MAX_NOTIFICATIONS - 1] = Notification::new(msg, priority);
        }
    }

    pub fn clear_notifications(&mut self) {
        self.notification_count = 0;
    }

    pub fn unread_count(&self) -> usize {
        self.notifications[..self.notification_count]
            .iter()
            .filter(|n| !n.read)
            .count()
    }

    pub fn mark_all_read(&mut self) {
        for n in &mut self.notifications[..self.notification_count] {
            n.read = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_desktop_shell_new() {
        let shell = DesktopShell::new();
        assert_eq!(shell.pinned_count, 5);
        assert_eq!(shell.notification_count, 0);
        assert_eq!(shell.wallpaper_mode, WallpaperMode::CosmicTheme);
    }

    #[test]
    fn test_push_notification() {
        let mut shell = DesktopShell::new();
        shell.push_notification("Test notification", 1);
        assert_eq!(shell.notification_count, 1);
        assert_eq!(shell.notifications[0].message_str(), "Test notification");
    }

    #[test]
    fn test_notification_overflow() {
        let mut shell = DesktopShell::new();
        for i in 0..MAX_NOTIFICATIONS + 5 {
            shell.push_notification("msg", 0);
        }
        assert_eq!(shell.notification_count, MAX_NOTIFICATIONS);
    }

    #[test]
    fn test_unread_count() {
        let mut shell = DesktopShell::new();
        shell.push_notification("a", 0);
        shell.push_notification("b", 0);
        assert_eq!(shell.unread_count(), 2);
        shell.mark_all_read();
        assert_eq!(shell.unread_count(), 0);
    }

    #[test]
    fn test_pinned_app() {
        let app = PinnedApp::new("Terminal", 0, 200, 100);
        assert_eq!(app.name_str(), "Terminal");
        assert_eq!(app.icon_color, (0, 200, 100));
        assert_eq!(app.exec_path_str(), "");
    }

    #[test]
    fn test_pinned_app_with_exec() {
        let app = PinnedApp::with_exec("Terminal", 0, 200, 100, "/bin/terminal");
        assert_eq!(app.name_str(), "Terminal");
        assert_eq!(app.exec_path_str(), "/bin/terminal");
    }

    #[test]
    fn test_desktop_shell_pinned_exec_paths() {
        let shell = DesktopShell::new();
        assert_eq!(shell.pinned_apps[0].exec_path_str(), "/bin/terminal");
        assert_eq!(shell.pinned_apps[1].exec_path_str(), "/bin/files");
        assert_eq!(shell.pinned_apps[4].exec_path_str(), "/bin/settings");
    }

    #[test]
    fn test_desktop_shell_clock_defaults() {
        let shell = DesktopShell::new();
        assert_eq!(shell.clock_hours, 0);
        assert_eq!(shell.clock_minutes, 0);
        assert!(!shell.volume_muted);
        assert!(shell.show_clock);
    }

    #[test]
    fn test_pin_app_at_capacity() {
        let mut shell = DesktopShell::new();
        // Already has 5 pinned; add more to fill
        for _ in 0..10 {
            shell.pin_app("Extra", 0, 0, 0);
        }
        assert_eq!(shell.pinned_count, MAX_PINNED_APPS);
    }

    #[test]
    fn test_unpin_app_removes_correctly() {
        let mut shell = DesktopShell::new();
        // Default has 5 apps: Terminal, Files, Editor, Browser, Settings
        assert_eq!(shell.pinned_count, 5);

        // Unpin the second app (Files at index 1)
        shell.unpin_app(1);
        assert_eq!(shell.pinned_count, 4);
        assert_eq!(shell.pinned_apps[0].name_str(), "Terminal");
        assert_eq!(shell.pinned_apps[1].name_str(), "Editor");
        assert_eq!(shell.pinned_apps[2].name_str(), "Browser");
        assert_eq!(shell.pinned_apps[3].name_str(), "Settings");
    }

    #[test]
    fn test_unpin_app_first() {
        let mut shell = DesktopShell::new();
        shell.unpin_app(0);
        assert_eq!(shell.pinned_count, 4);
        assert_eq!(shell.pinned_apps[0].name_str(), "Files");
    }

    #[test]
    fn test_unpin_app_last() {
        let mut shell = DesktopShell::new();
        let last_idx = shell.pinned_count - 1;
        shell.unpin_app(last_idx);
        assert_eq!(shell.pinned_count, 4);
        assert_eq!(shell.pinned_apps[3].name_str(), "Browser");
    }

    #[test]
    fn test_unpin_app_out_of_range() {
        let mut shell = DesktopShell::new();
        // Should do nothing for out-of-range index
        shell.unpin_app(10);
        assert_eq!(shell.pinned_count, 5);
    }
}
