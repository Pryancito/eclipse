//! Removable media: every filesystem mounted from a block device after boot
//! -- a USB stick, an SD card, an external disk, a loop image -- is written
//! down here the moment it is mounted, scanned by the antivirus before anyone
//! has a reason to trust it, and (optionally) kept from running anything
//! until that scan comes back clean.
//!
//! ## Who does what
//!
//! The kernel only *notes* the mount ([`note_mount`], from `mount(2)`), and
//! keeps the media table that `/proc/hunter` prints as `media-device:` lines.
//! It does not walk the new tree itself: `mount(2)` would not return until
//! every file on a 64 GB stick had been read, and a scan on the kernel's
//! cooperative executor would starve every other task on that core for as
//! long. The walk is a userland agent's, the same `/proc/hunter_scan` batches
//! `hunterctl scan` already sends:
//!
//! * `lunarbar` sees a `state=pending` device on its next read of
//!   `/proc/hunter` and runs `hunterctl scan-media <id>` in the background;
//! * on a console without the desktop, `hunterctl scan-media` does the same
//!   for every pending device by hand.
//!
//! The agent reports back in the control language (`media <id> scanning`,
//! `media <id> done <files> <infected>`, `media <id> failed`), so the verdict
//! lives in the kernel next to everything else hunter knows, and every front
//! end reads the same one.
//!
//! ## The lock
//!
//! `media-lock on` makes [`check_exec`] refuse to run anything from a device
//! whose scan has not come back clean: pending, being scanned, failed or
//! infected. A stick that carries an autorun-style dropper cannot be
//! double-clicked into running in the second between plugging it in and the
//! scan reaching the dropper. It is off by default because without an agent
//! (no desktop, nobody running `hunterctl scan-media`) a device stays pending
//! for good, and so would its programs; `hunterctl lockdown` turns it on.
//!
//! ## The latch
//!
//! Under the tighten-only seal, `media-scan off` and `media-lock off` are
//! refused, and so is a `done` that would turn an infected device clean: once
//! sealed, a verdict only gets worse.

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use lazy_static::lazy_static;
use lock::Mutex;

use crate::event_log::{record, Severity};
use crate::policy;

/// Devices remembered at once. A finished entry is forgotten first when a new
/// mount needs room; nobody plugs in 32 sticks, but a script mounting loop
/// images in a loop must not grow the table without bound.
const MAX_DEVICES: usize = 32;

/// Where a device's scan stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Mounted, not scanned yet: waiting for an agent.
    Pending,
    /// An agent took it and is walking the tree.
    Scanning,
    /// Scanned, nothing found.
    Clean,
    /// Scanned, `infected` files matched a signature.
    Infected,
    /// The agent could not finish (the stick was pulled, the walk failed).
    Failed,
    /// Mounted while `media-scan off`: nobody will scan it.
    Skipped,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Scanning => "scanning",
            State::Clean => "clean",
            State::Infected => "infected",
            State::Failed => "failed",
            State::Skipped => "skipped",
        }
    }

    /// Still waiting for, or in the middle of, a scan.
    fn open(self) -> bool {
        matches!(self, State::Pending | State::Scanning)
    }
}

/// One mounted device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// Never reused while the kernel runs, so a `done` meant for a stick that
    /// was pulled cannot land on the next one mounted at the same place.
    pub id: u64,
    pub source: String,
    pub target: String,
    pub fstype: String,
    pub state: State,
    pub files: u64,
    pub infected: u64,
}

/// What an agent says about a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Report {
    /// It took the device and started walking it.
    Scanning,
    /// It finished: `files` scanned, `infected` of them matched.
    Done { files: u64, infected: u64 },
    /// It could not finish.
    Failed,
}

static AUTO_SCAN: AtomicBool = AtomicBool::new(true);
static LOCK: AtomicBool = AtomicBool::new(false);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

lazy_static! {
    static ref DEVICES: Mutex<Vec<Device>> = Mutex::new(Vec::new());
}

/// Whether new media are queued for a scan (`media-scan`, default on).
pub fn auto_scan() -> bool {
    AUTO_SCAN.load(Ordering::Relaxed)
}

/// Whether programs on media not yet scanned clean are refused (`media-lock`,
/// default off).
pub fn lock_enabled() -> bool {
    LOCK.load(Ordering::Relaxed)
}

/// `media-scan <on|off>`. Off is a relaxation, refused under the seal.
pub fn set_auto_scan(on: bool) {
    set_switch(&AUTO_SCAN, "media-scan", on);
}

/// `media-lock <on|off>`. Off is a relaxation, refused under the seal.
pub fn set_lock(on: bool) {
    set_switch(&LOCK, "media-lock", on);
}

fn set_switch(flag: &AtomicBool, name: &str, on: bool) {
    if !on && policy::is_tighten_only() {
        record(
            0,
            Severity::Warning,
            "CONTROL",
            "WARNING",
            format!(
                "refused relaxing control change: {} off (tighten-only latch)",
                name
            ),
        );
        return;
    }
    flag.store(on, Ordering::Relaxed);
    record(
        0,
        Severity::Notice,
        "CONTROL",
        "CONFIG",
        format!("{} {}", name, if on { "on" } else { "off" }),
    );
}

/// `mount(2)` mounted the filesystem on `source` at `target`. Call it for
/// filesystems on a block device only: a pseudo-filesystem has no files of
/// anyone's to scan, and a bind or a move is no new device. Answers the id
/// the device got.
pub fn note_mount(source: &str, target: &str, fstype: &str) -> u64 {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let target = policy::canonicalize(target);
    let state = if auto_scan() {
        State::Pending
    } else {
        State::Skipped
    };
    let mut devs = DEVICES.lock();
    // A second mount at the same place hides the first: its files are no
    // longer reachable there, so only the new one is scanned or locked.
    devs.retain(|d| d.target != target);
    if devs.len() >= MAX_DEVICES {
        let victim = devs.iter().position(|d| !d.state.open()).unwrap_or(0);
        devs.remove(victim);
    }
    devs.push(Device {
        id,
        source: String::from(source),
        target: target.clone(),
        fstype: String::from(fstype),
        state,
        files: 0,
        infected: 0,
    });
    drop(devs);
    record(
        0,
        Severity::Notice,
        "MEDIA",
        "MOUNT",
        format!(
            "media #{} {} ({}) at {}: {}",
            id,
            source,
            fstype,
            target,
            if state == State::Pending {
                "waiting for a scan"
            } else {
                "not scanned (media-scan off)"
            }
        ),
    );
    id
}

/// `umount2` took `target` down, with whatever was mounted inside it.
pub fn note_umount(target: &str) {
    let target = policy::canonicalize(target);
    let mut devs = DEVICES.lock();
    devs.retain(|d| !is_under(&d.target, &target));
}

/// `mount --move from to` carried `from`, and what is mounted inside it, to
/// `to`. The verdict travels with the files.
pub fn note_move(from: &str, to: &str) {
    let from = policy::canonicalize(from);
    let to = policy::canonicalize(to);
    for d in DEVICES.lock().iter_mut() {
        if is_under(&d.target, &from) {
            let rest = &d.target[from.len()..];
            d.target = if to == "/" && rest.starts_with('/') {
                String::from(rest)
            } else {
                format!("{}{}", to, rest)
            };
        }
    }
}

/// An agent's report on device `id`. Answers whether there was such a device
/// to report on.
pub fn report(id: u64, r: Report) -> bool {
    let mut devs = DEVICES.lock();
    let Some(d) = devs.iter_mut().find(|d| d.id == id) else {
        return false;
    };
    if d.state == State::Infected && policy::is_tighten_only() {
        let target = d.target.clone();
        drop(devs);
        record(
            0,
            Severity::Warning,
            "CONTROL",
            "WARNING",
            format!(
                "refused relaxing control change: new verdict for infected media #{} at {} (tighten-only latch)",
                id, target
            ),
        );
        return true;
    }
    match r {
        Report::Scanning => d.state = State::Scanning,
        Report::Failed => d.state = State::Failed,
        Report::Done { files, infected } => {
            d.files = files;
            d.infected = infected;
            d.state = if infected == 0 {
                State::Clean
            } else {
                State::Infected
            };
        }
    }
    let d = d.clone();
    drop(devs);
    let (sev, action, what) = match d.state {
        State::Scanning => (Severity::Info, "SCAN", String::from("scan started")),
        State::Failed => (
            Severity::Warning,
            "FAILED",
            String::from("scan did not finish"),
        ),
        State::Clean => (
            Severity::Notice,
            "CLEAN",
            format!("{} file(s) scanned, nothing found", d.files),
        ),
        _ => (
            Severity::Critical,
            "INFECTED",
            format!("{} of {} file(s) matched a signature", d.infected, d.files),
        ),
    };
    record(
        0,
        sev,
        "MEDIA",
        action,
        format!("media #{} {} at {}: {}", d.id, d.source, d.target, what),
    );
    true
}

/// The exec gate's question about `resolved`, the file a program is about to
/// be run from: may it run, as far as removable media go? Only under
/// `media-lock on`, and only for a device whose scan has not come back clean.
pub fn check_exec(resolved: &str) -> bool {
    if !lock_enabled() {
        return true;
    }
    let path = policy::canonicalize(resolved);
    let devs = DEVICES.lock();
    // The innermost mount is the one the file is on.
    let Some(d) = devs
        .iter()
        .filter(|d| is_under(&path, &d.target))
        .max_by_key(|d| d.target.len())
    else {
        return true;
    };
    if d.state == State::Clean || d.state == State::Skipped {
        return true;
    }
    let (id, state) = (d.id, d.state);
    drop(devs);
    record(
        0,
        Severity::Critical,
        "MEDIA",
        "BLOCKED",
        format!(
            "blocked exec from media #{} ({}): {}",
            id,
            state.as_str(),
            path
        ),
    );
    warn!(
        "hunter: BLOCKED exec from {} media: {}",
        state.as_str(),
        path
    );
    false
}

/// The devices, oldest mount first.
pub fn devices() -> Vec<Device> {
    DEVICES.lock().clone()
}

/// The `/proc/hunter` lines: one summary, then one per device. The target
/// comes last on its line, so a mount point with a space in it is still the
/// whole rest of the line.
pub fn render() -> String {
    let devs = DEVICES.lock();
    let mut out = format!(
        "media: auto-scan={} lock={} devices={}\n",
        if auto_scan() { "on" } else { "off" },
        if lock_enabled() { "on" } else { "off" },
        devs.len()
    );
    for d in devs.iter() {
        out.push_str(&format!(
            "media-device: id={} state={} files={} infected={} fstype={} source={} target={}\n",
            d.id,
            d.state.as_str(),
            d.files,
            d.infected,
            d.fstype,
            d.source,
            d.target
        ));
    }
    out
}

/// `path` is `dir` or inside it (both canonical).
fn is_under(path: &str, dir: &str) -> bool {
    if dir == "/" {
        return true;
    }
    path == dir || (path.starts_with(dir) && path.as_bytes().get(dir.len()) == Some(&b'/'))
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    AUTO_SCAN.store(true, Ordering::Relaxed);
    LOCK.store(false, Ordering::Relaxed);
    DEVICES.lock().clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_globals;

    fn reset() {
        policy::reset_for_test();
        crate::event_log::reset_for_test();
        reset_for_test();
    }

    fn state_of(id: u64) -> Option<State> {
        devices().into_iter().find(|d| d.id == id).map(|d| d.state)
    }

    #[test]
    fn a_new_stick_waits_for_a_scan_and_the_verdict_sticks() {
        let _g = test_globals::lock();
        reset();
        let id = note_mount("/dev/sdb1", "/media/usb/", "vfat");
        assert_eq!(state_of(id), Some(State::Pending));
        assert!(render().contains(&format!(
            "media-device: id={} state=pending files=0 infected=0 fstype=vfat source=/dev/sdb1 target=/media/usb\n",
            id
        )));
        assert!(report(id, Report::Scanning));
        assert_eq!(state_of(id), Some(State::Scanning));
        assert!(report(
            id,
            Report::Done {
                files: 12,
                infected: 0
            }
        ));
        assert_eq!(state_of(id), Some(State::Clean));
        assert!(!report(id + 100, Report::Failed));
    }

    #[test]
    fn an_infected_stick_is_a_critical_event() {
        let _g = test_globals::lock();
        reset();
        let id = note_mount("/dev/sdb1", "/media/usb", "vfat");
        let before = crate::event_log::stats().criticals;
        report(
            id,
            Report::Done {
                files: 40,
                infected: 2,
            },
        );
        assert_eq!(state_of(id), Some(State::Infected));
        assert_eq!(crate::event_log::stats().criticals, before + 1);
    }

    #[test]
    fn with_scanning_off_a_stick_is_skipped() {
        let _g = test_globals::lock();
        reset();
        set_auto_scan(false);
        let id = note_mount("/dev/sdc", "/mnt", "vfat");
        assert_eq!(state_of(id), Some(State::Skipped));
        assert!(render().starts_with("media: auto-scan=off lock=off devices=1\n"));
    }

    #[test]
    fn the_lock_holds_programs_on_media_until_they_scan_clean() {
        let _g = test_globals::lock();
        reset();
        let id = note_mount("/dev/sdb1", "/media/usb", "vfat");
        // Off by default.
        assert!(check_exec("/media/usb/setup"));
        set_lock(true);
        assert!(!check_exec("/media/usb/setup"));
        assert!(!check_exec("/media/usb/../usb/bin/tool"));
        // Not on the stick: a sibling whose name only starts the same.
        assert!(check_exec("/media/usb2/setup"));
        assert!(check_exec("/usr/bin/ls"));
        report(
            id,
            Report::Done {
                files: 3,
                infected: 1,
            },
        );
        assert!(!check_exec("/media/usb/setup"));
        report(
            id,
            Report::Done {
                files: 3,
                infected: 0,
            },
        );
        assert!(check_exec("/media/usb/setup"));
        report(id, Report::Failed);
        assert!(!check_exec("/media/usb/setup"));
    }

    #[test]
    fn the_innermost_mount_decides() {
        let _g = test_globals::lock();
        reset();
        set_lock(true);
        let outer = note_mount("/dev/sdb1", "/media/disk", "btrfs");
        let inner = note_mount("/dev/sdc1", "/media/disk/card", "vfat");
        report(
            outer,
            Report::Done {
                files: 1,
                infected: 0,
            },
        );
        assert!(check_exec("/media/disk/run"));
        assert!(!check_exec("/media/disk/card/run"));
        report(
            inner,
            Report::Done {
                files: 1,
                infected: 0,
            },
        );
        assert!(check_exec("/media/disk/card/run"));
    }

    #[test]
    fn unmounting_forgets_the_device_and_what_was_inside_it() {
        let _g = test_globals::lock();
        reset();
        note_mount("/dev/sdb1", "/media/disk", "btrfs");
        note_mount("/dev/sdc1", "/media/disk/card", "vfat");
        let other = note_mount("/dev/sdd1", "/media/diskette", "vfat");
        note_umount("/media/disk");
        let left: Vec<u64> = devices().iter().map(|d| d.id).collect();
        assert_eq!(left, [other]);
    }

    #[test]
    fn a_remount_at_the_same_place_replaces_the_old_entry() {
        let _g = test_globals::lock();
        reset();
        let old = note_mount("/dev/sdb1", "/media/usb", "vfat");
        let new = note_mount("/dev/sdc1", "/media/usb", "vfat");
        assert_eq!(state_of(old), None);
        assert_eq!(state_of(new), Some(State::Pending));
    }

    #[test]
    fn a_moved_mount_keeps_its_verdict() {
        let _g = test_globals::lock();
        reset();
        set_lock(true);
        let id = note_mount("/dev/sdb1", "/mnt/tmp", "vfat");
        note_move("/mnt/tmp", "/media/usb");
        assert!(!check_exec("/media/usb/x"));
        assert!(check_exec("/mnt/tmp/x"));
        assert_eq!(devices()[0].target, "/media/usb");
        report(
            id,
            Report::Done {
                files: 1,
                infected: 0,
            },
        );
        assert!(check_exec("/media/usb/x"));
    }

    #[test]
    fn the_table_is_bounded_and_drops_finished_devices_first() {
        let _g = test_globals::lock();
        reset();
        let first = note_mount("/dev/loop0", "/mnt/0", "vfat");
        let done = note_mount("/dev/loop1", "/mnt/1", "vfat");
        report(
            done,
            Report::Done {
                files: 0,
                infected: 0,
            },
        );
        for i in 2..MAX_DEVICES + 1 {
            note_mount("/dev/loop", &format!("/mnt/{}", i), "vfat");
        }
        let devs = devices();
        assert_eq!(devs.len(), MAX_DEVICES);
        assert!(devs.iter().any(|d| d.id == first));
        assert!(!devs.iter().any(|d| d.id == done));
    }

    #[test]
    fn once_sealed_a_verdict_only_gets_worse() {
        let _g = test_globals::lock();
        reset();
        set_lock(true);
        let id = note_mount("/dev/sdb1", "/media/usb", "vfat");
        report(
            id,
            Report::Done {
                files: 5,
                infected: 1,
            },
        );
        policy::seal_tighten_only();
        report(
            id,
            Report::Done {
                files: 5,
                infected: 0,
            },
        );
        assert_eq!(state_of(id), Some(State::Infected));
        set_lock(false);
        set_auto_scan(false);
        assert!(lock_enabled());
        assert!(auto_scan());
    }
}
