//! The btrfs and EclipseFS filesystems of Eclipse OS that run as processes.
//!
//! `eclipse-btrfs DEVICE DIR` (or `eclipse-eclipsefs`) mounts the volume on
//! `DEVICE` at `DIR` and serves it, as `eclipse-fatfs` serves a FAT volume:
//! the kernel asks by path, on a FIFO, for a directory's entries, a file's
//! bytes, or a change, and this answers through a window of memory both
//! have. The mount is a POSIX one (`SERVE_POSIX`): each entry comes with
//! its owner, its permissions and its number of names, and a file may be a
//! symbolic link, a pipe or a socket.
//!
//! The filesystem code is the kernel's own (`vendor/btrfs-rs`,
//! `vendor/eclipsefs-rs`, behind [`tree::Tree`]). It keeps the trees it
//! changes in memory; here it writes them out every so many changes
//! ([`volume::Volume`]), into the disk the FAT server keeps in memory the
//! mount holds ([`fatserve::disk`]), which reaches the device a second
//! later, and the changes since are written down in that memory too
//! ([`log`]). So what the kernel was told is done is always there: a
//! server that dies takes nothing with it, and the next one mounts the
//! volume from there and makes those changes again. A write that only adds
//! to the end of a file waits in that memory too ([`tail`]), and is handed
//! to the filesystem a mebibyte at a time.

pub mod dev;
pub mod log;
pub mod serve;
pub mod tail;
pub mod tree;
pub mod volume;
