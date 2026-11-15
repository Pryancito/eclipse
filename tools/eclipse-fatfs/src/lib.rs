//! The FAT filesystem of Eclipse OS that runs as a process.
//!
//! `eclipse-fatfs DEVICE DIR` mounts the FAT volume on `DEVICE` at `DIR`
//! and serves it: the kernel asks for a directory's entries or a file's
//! bytes, or for a change -- bytes written, a file made, removed, renamed,
//! cut -- on a FIFO, and this answers through a window of memory both have
//! ([`wire`]). The kernel knows nothing of FAT; it keeps what it was told
//! -- the listings, and the files' pages in its page cache -- so each
//! thing is asked for once.
//!
//! A request names its file by path, and nothing is kept per file that the
//! kernel would have to say to forget: what this keeps of the files it has
//! used ([`volume`]) and of the disk ([`disk`]) only saves work. What was
//! written and has not reached the device is in [`disk`], for a second at
//! most, in memory the mount holds ([`disk::Kept`]).
//!
//! So a server that dies takes nothing with it. The mount stays and waits
//! for the next server, which is given that memory and goes on from what
//! the dead one was told; the request the dead one was in the middle of is
//! made again. `eclipse-fatfs` leaves a second process behind that starts
//! the next server.
//!
//! What can be decided without a kernel -- the layout of a reply, what the
//! volume holds after a change -- is kept apart from the system calls, and
//! tested on the host with `cargo test`.

pub mod disk;
pub mod volume;
pub mod wire;
