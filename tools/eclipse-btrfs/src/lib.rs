//! The btrfs filesystem of Eclipse OS that runs as a process.
//!
//! `eclipse-btrfs DEVICE DIR` mounts the btrfs volume on `DEVICE` at `DIR`
//! and serves it, as `eclipse-fatfs` serves a FAT volume: the kernel asks
//! by path, on a FIFO, for a directory's entries, a file's bytes, or a
//! change, and this answers through a window of memory both have. The
//! mount is a POSIX one (`SERVE_POSIX`): each entry comes with its owner,
//! its permissions and its number of names, and a file may be a symbolic
//! link, a pipe or a socket.
//!
//! The btrfs code is the kernel's own (`vendor/btrfs-rs`). It keeps the
//! tree blocks it changes in memory and writes them out every so many
//! changes; here it is made to write them out after each change the kernel
//! asks for ([`volume::Volume`]), into the disk the FAT server keeps in
//! memory the mount holds ([`fatserve::disk`]), which reaches the device a
//! second later. So what the kernel was told is done is always in that
//! memory or on the device, as a whole volume: a server that dies takes
//! nothing with it, and the next one mounts the volume from there. A write
//! that only adds to the end of a file waits in that memory too
//! ([`tail`]), and is handed to btrfs a mebibyte at a time.

pub mod dev;
pub mod log;
pub mod tail;
pub mod volume;
