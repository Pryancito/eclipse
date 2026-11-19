//! The NFS client of Eclipse OS that runs as a process.
//!
//! `eclipse-nfs HOST:/EXPORT DIR` mounts the export at `DIR` and serves it,
//! as `eclipse-btrfs` serves a btrfs volume: the kernel asks by path, on a
//! FIFO, for a directory's entries, a file's bytes, or a change, and this
//! asks the NFS server. The mount is a POSIX one (`SERVE_POSIX`).
//!
//! The NFS code is the kernel's own (`vendor/nfs3-rs`): the same XDR, RPC,
//! portmap, MOUNT and NFSv3 calls, over a socket of this process
//! ([`net`]) instead of one of the kernel's. NFSv3 keeps no state on the
//! server a client has to hold on to, so a server that dies leaves nothing
//! behind: the next one greets the NFS server again and goes on from the
//! paths the kernel asks for ([`export`]).

pub mod export;
pub mod net;
