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
}

impl Default for PinnedApp {
    fn default() -> Self {
        Self {
            name: [0; 32],
            icon_color: (100, 100, 100),
            running: false,
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

    pub fn name_str(&self) -> &str {
        let len = self.name.iter().position(|&b| b == 0).unwrap_or(32);
        core::str::from_utf8(&self.name[..len]).unwrap_or("")
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
    pub brightness_level: u8,
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
            brightness_level: 100,
        };

        // Default pinned apps
        shell.pin_app("Terminal", 0, 200, 100);
        shell.pin_app("Files", 100, 150, 255);
        shell.pin_app("Editor", 200, 160, 50);
        shell.pin_app("Browser", 255, 100, 50);
        shell.pin_app("Settings", 150, 150, 150);

        shell
    }

    pub fn pin_app(&mut self, name: &str, r: u8, g: u8, b: u8) {
        if self.pinned_count < MAX_PINNED_APPS {
            self.pinned_apps[self.pinned_count] = PinnedApp::new(name, r, g, b);
            self.pinned_count += 1;
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
}
