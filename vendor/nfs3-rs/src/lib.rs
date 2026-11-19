//! The NFSv3 client: everything below the socket and above it up to the
//! session, as pure functions from bytes to bytes.
//!
//! Shared by the kernel's NFS (`linux-object/src/fs/nfs`, which adds the
//! VFS and the kernel's sockets) and the process that serves an NFS mount
//! (`tools/eclipse-nfs`, which adds `std` sockets and the served
//! filesystem protocol). Moved here from the kernel unchanged but for the
//! errno, which is a number ([`errno::Errno`]) and not the kernel's type,
//! and the clock, which the [`connect::Connector`] keeps.
//!
//! - [`xdr`]: the XDR encoding (RFC 4506) every ONC RPC protocol uses.
//! - [`rpc`]: the RPC v2 call and reply envelope (RFC 5531) and the TCP
//!   record marking that frames it (RFC 1831 section 10).
//! - [`portmap`]: `GETPORT` against port 111, which is how a client finds
//!   `mountd`.
//! - [`mount3`]: the MOUNT v3 `MNT` call, which turns an export path into the
//!   root filehandle every NFS call starts from.
//! - [`nfs3`]: the NFSv3 types (RFC 1813) and the procedures of the read
//!   path.
//! - [`nfs3_write`]: the NFSv3 procedures that change something on the
//!   server.
//! - [`nfs3_dir`]: `READDIR` and `READDIRPLUS`, which read a directory.
//! - [`client`]: one call out, one reply back, with transaction ids,
//!   timeouts and retries, over a four-method transport trait.
//! - [`transport`]: what every transport shares.
//! - [`session`]: one mounted export's conversation with its server: the
//!   errno every failure becomes, and the loops `READ`, `WRITE` and `READDIR`
//!   make mandatory.
//! - [`source`]: the `host:/export` string `mount(8)` passes as the device,
//!   and the options.
//! - [`connect`] and [`greeting`]: portmap, `mountd` and the NFS port,
//!   before there is a filesystem.
//!
//! # The server is not trusted
//!
//! Every byte parsed here came from another machine, and in the kernel the
//! parsing happens before anything has checked who sent it. Two rules run
//! through the whole crate, and both are enforced in the decoder rather than
//! asked of each caller: no length from the wire is ever used to allocate,
//! and no arithmetic on one may wrap. [`xdr`] documents the reasoning and the
//! tests hold it at the boundary.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod client;
pub mod connect;
#[doc(hidden)]
pub mod dialer;
pub mod errno;
#[doc(hidden)]
pub mod fake;
pub mod greeting;
pub mod mount3;
pub mod nfs3;
pub mod nfs3_dir;
pub mod nfs3_write;
pub mod portmap;
pub mod rpc;
pub mod session;
pub mod source;
pub mod transport;
pub mod xdr;

/// The time every NFS attribute carries, as the kernel's VFS has it.
pub use rcore_fs::vfs::Timespec;
