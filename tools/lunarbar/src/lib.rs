//! Shared internals of Eclipse OS's native desktop clients.
//!
//! Two binaries live on top of this library, and both exist for the same
//! reason: everything off-the-shelf in this space is a GTK or Qt application
//! that registers on the session D-Bus at startup, and Eclipse OS has no
//! session bus (see `DBUS_SESSION_BUS_ADDRESS` in the labwc session
//! environment) — waybar prints "Could not connect: Connection refused" and
//! exits before it maps, and Plasma's krunner *is* a D-Bus service.
//!
//! - `lunarbar` — the panel (taskbar, clock, indicators, app menu).
//! - `lunarrun` — the KRunner stand-in: a centred search overlay, plus
//!   `--toggle-desktop` (KDE's Super+D) over wlr-foreign-toplevel-management.
//!
//! Both are single static musl binaries over wlr-layer-shell + wl_shm with
//! their own bitmap font and /proc readers: no GTK, no D-Bus, no gdk-pixbuf,
//! no fontconfig, no locale.

pub mod apps;
pub mod draw;
pub mod fill_guard;
pub mod i18n;
pub mod icons;
pub mod keys;
pub mod look;
pub mod par;
pub mod proc;
pub mod sysinfo;
