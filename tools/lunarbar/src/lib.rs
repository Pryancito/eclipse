//! Shared internals of Eclipse OS's native desktop clients.
//!
//! Five binaries live on top of this library, and they all exist for the same
//! reason: everything off-the-shelf in this space is a GTK or Qt application
//! that registers on the session D-Bus at startup, and Eclipse OS has no
//! session bus (see `DBUS_SESSION_BUS_ADDRESS` in the labwc session
//! environment) — waybar prints "Could not connect: Connection refused" and
//! exits before it maps, and Plasma's krunner *is* a D-Bus service.
//!
//! - `lunarbar` — the panel (taskbar, clock, indicators, app menu).
//! - `lunarrun` — the KRunner stand-in: a centred search overlay, plus
//!   `--toggle-desktop` (KDE's Super+D) over wlr-foreign-toplevel-management.
//! - `lunarfile` — the file browser behind Super+E, and the `--pick` chooser
//!   the XDG portal shows.
//! - `lunarclip` — the clipboard manager over wlr-data-control: the daemon
//!   that keeps a copy alive after the window that made it closes, and the
//!   Super+V history picker.
//! - `lunarhunter` — hunter's window, opened from the bar's shield menu:
//!   configuration, the event ring, scans, quarantine and signatures.
//!
//! All of them are single static musl binaries with their own bitmap font and
//! /proc readers — the first three over wlr-layer-shell + wl_shm, lunarclip
//! over wlr-data-control, lunarhunter over xdg-shell + wl_shm: no GTK, no
//! D-Bus, no gdk-pixbuf, no fontconfig, no locale.

pub mod apps;
pub mod clip;
pub mod draw;
pub mod files;
pub mod hunter;
pub mod hunter_firewall;
pub mod hunter_report;
pub mod hunter_scan;
pub mod fill_guard;
pub mod i18n;
pub mod icons;
pub mod keys;
pub mod list;
pub mod look;
pub mod obmenu;
pub mod open;
pub mod par;
pub mod proc;
pub mod sysinfo;
