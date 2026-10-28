//! `/proc/<pid>/mountinfo` (`show_mountinfo`), and the escaping it shares
//! with `/proc/mounts`.
//!
//! One line per mount, in the order they were made:
//!
//! ```text
//! 36 35 0:2 / /sys rw,nosuid,nodev,noexec,relatime - sysfs sysfs rw
//! ```
//!
//! Its number; the number of the mount it is on; the `st_dev` of its root;
//! the directory of its filesystem it shows, `/` unless it is a bind of a
//! directory inside a mount; where it is; the options of the mount itself; the
//! propagation fields, none because nothing here propagates, and the `-`
//! that ends them; the type; the source; and the options of the filesystem.
//!
//! libmount (`findmnt`, `mount`, `umount`), systemd and GIO read this file
//! first and `/proc/mounts` only when it is missing; `findmnt` draws its tree
//! from the first two numbers.

use super::search::Walker;
use super::{path_is_under, FileDesc, MountEntry, MOUNT_TABLE};
use crate::process::LinuxProcess;
use alloc::string::String;
use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};

/// The number of the mount `/` is on, which no reader sees. On Linux it is
/// the boot's `rootfs`, always 1, and it is left out of the listing because
/// it is above every process's root.
pub(super) const ROOT_PARENT: usize = 1;

static NEXT_ID: AtomicUsize = AtomicUsize::new(ROOT_PARENT + 1);

/// A number for a new mount, never handed out before.
pub(super) fn next_id() -> usize {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// What a mount at `target` is put on: the top mount at the longest mount
/// point that holds `target`, which is the last one recorded there (the rule
/// [`super::mount_flag_from`] reads the flags by), or [`ROOT_PARENT`] when
/// none does. A second mount on a directory goes on the first, as Linux
/// stacks it: init's `mount -t proc proc /proc` goes on the kernel's own.
pub(super) fn parent_of<'a>(target: &str, mounts: impl Iterator<Item = &'a MountEntry>) -> usize {
    mounts
        .filter(|m| path_is_under(target, &m.target))
        .max_by_key(|m| m.target.len())
        .map_or(ROOT_PARENT, |m| m.id)
}

/// `mangle()`: a space, a tab, a newline and a backslash as `\ooo`, which
/// is what `getmntent` and libmount turn back into the character.
pub(super) fn escape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    for c in field.chars() {
        match c {
            ' ' | '\t' | '\n' | '\\' => {
                let _ = write!(out, "\\{:03o}", c as u32);
            }
            _ => out.push(c),
        }
    }
    out
}

/// What `show_mnt_opts` prints: the flags of the mount, not of the
/// filesystem under it.
const MOUNT_OPTIONS: [&str; 7] = [
    "nosuid",
    "nodev",
    "noexec",
    "noatime",
    "nodiratime",
    "relatime",
    "nosymfollow",
];

/// The options of a mount as `show_mountinfo` splits them: `rw` or `ro` and
/// the mount's own flags, then `rw` or `ro` again and everything else, which
/// belongs to the filesystem (`sync`, `size=`, `mode=`).
fn split_options(options: &str) -> (String, String) {
    let access = if options.split(',').any(|w| w == "ro") {
        "ro"
    } else {
        "rw"
    };
    let (mut mount, mut filesystem) = (String::from(access), String::from(access));
    for word in options
        .split(',')
        .filter(|w| !w.is_empty() && *w != "rw" && *w != "ro")
    {
        let side = if MOUNT_OPTIONS.contains(&word) {
            &mut mount
        } else {
            &mut filesystem
        };
        side.push(',');
        side.push_str(word);
    }
    (mount, filesystem)
}

/// An `st_dev` taken apart as glibc's `major()` and `minor()` do, so the
/// field says what `stat` on the mount point says.
fn major_minor(dev: u64) -> (u64, u64) {
    let major = ((dev & 0x0000_0000_000f_ff00) >> 8) | ((dev & 0xffff_f000_0000_0000) >> 32);
    let minor = (dev & 0xff) | ((dev & 0x0000_0fff_fff0_0000) >> 12);
    (major, minor)
}

/// `/proc/mounts`, and every process's `mounts`: `show_vfsmnt`.
///
/// The source, the mount point and the options go through [`escape`], as
/// `mangle()` sends them. The line is split on spaces, so a mount point with
/// one in it printed raw is a line of seven fields, and `getmntent` took the
/// second half of its name for the type. The type needs nothing: it is one
/// of the names `mount` knows, and none has a space.
pub(super) fn mounts_text(mounts: &[MountEntry]) -> String {
    let mut out = String::new();
    for m in mounts {
        let _ = writeln!(
            out,
            "{} {} {} {} 0 0",
            escape(&m.source),
            escape(&m.target),
            m.fstype,
            escape(&m.options)
        );
    }
    out
}

/// The lines of `mounts`, each with the `st_dev` that `dev_of` finds at its
/// mount point.
fn render(mounts: &[MountEntry], dev_of: impl Fn(&str) -> u64) -> String {
    let mut out = String::new();
    for m in mounts {
        let (major, minor) = major_minor(dev_of(&m.target));
        let (mount, filesystem) = split_options(&m.options);
        let _ = writeln!(
            out,
            "{} {} {}:{} {} {} {} - {} {} {}",
            m.id,
            m.parent,
            major,
            minor,
            escape(&m.root),
            escape(&m.target),
            escape(&mount),
            m.fstype,
            escape(&m.source),
            escape(&filesystem)
        );
    }
    out
}

/// `/proc/<pid>/mountinfo` of `proc`.
pub(super) fn content(proc: Option<&LinuxProcess>) -> String {
    // A copy, so that no lookup runs with the table held: a mount takes the
    // tree first and the table after it.
    let mounts = MOUNT_TABLE.lock().clone();
    render(&mounts, |target| dev_at(proc, target))
}

/// What `stat` on `target` gives `proc` for `st_dev`, found by no one's
/// permission: Linux reads it off the superblock and walks nowhere. 0 when
/// nothing is there to ask.
fn dev_at(proc: Option<&LinuxProcess>, target: &str) -> u64 {
    proc.and_then(|p| {
        p.lookup_inode_at_by(FileDesc::CWD, target, true, Walker::Kernel)
            .ok()
    })
    .and_then(|inode| inode.metadata().ok())
    .map_or(0, |m| m.dev as u64)
}

#[cfg(test)]
mod tests;
