//! The FAT filesystem of Eclipse OS that runs as a process.
//!
//! `eclipse-fatfs DEVICE DIR` mounts the FAT volume on `DEVICE` at `DIR`,
//! read-only, and serves it: the kernel asks for a directory's entries or a
//! file's bytes on a FIFO, and this answers through a window of memory
//! both have ([`wire`]). The kernel knows nothing of FAT; it keeps what it
//! was told -- the listings, and the files' pages in its page cache -- so
//! each thing is asked for once.
//!
//! A request names its file by path, and nothing is kept per file that the
//! kernel would have to say to forget: what this keeps of the files it has
//! read ([`volume`]) and of the disk ([`disk`]) only saves work.
//!
//! What can be decided without a kernel -- the layout of a reply, what the
//! volume holds -- is kept apart from the system calls, and tested on the
//! host with `cargo test`.

pub mod disk;
pub mod volume;
pub mod wire;
