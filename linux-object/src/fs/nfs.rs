//! The NFS client: mounting a remote export over NFSv3.
//!
//! The protocol below the VFS is the `nfs3` crate (`vendor/nfs3-rs`), which
//! the process that serves an NFS mount (`tools/eclipse-nfs`) links too; its
//! modules are re-exported here under the names they had when they lived
//! here. What is the kernel's:
//!
//! - [`fs`]: the VFS over a session.
//! - [`connect`] and [`served`]: the connection, a stream socket of the
//!   network stack that runs as a process.
//! - [`mount`]: the greeting, the session, the filesystem, in that order.
//!
//! # Only a client, and only NFSv3
//!
//! No server: Eclipse does not export its own directories, by decision. The
//! protocol is symmetric enough that most of this module would serve one,
//! which is why the codec is not shaped around the client's needs only, but
//! nothing here listens.
//!
//! NFSv3 and not v4. They are not versions of one protocol so much as two
//! protocols: v4 folds the MOUNT protocol in, adds compound operations,
//! state, leases, delegations and a mandatory security framework, and its
//! client is several times the size of a v3 one. v3 is what gets a remote
//! export mounted, it is still what most NAS boxes serve by default, and it
//! is stateless — which matters more here than anywhere, because a stateless
//! protocol has nothing to recover after the kind of reboot this kernel is
//! still having.
//!
//! # The server is not trusted
//!
//! Every byte parsed here came from another machine, and the parsing happens
//! in kernel context before anything has checked who sent it. Two rules run
//! through the whole module, and both are enforced in the decoder rather than
//! asked of each caller: no length from the wire is ever used to allocate,
//! and no arithmetic on one may wrap. [`xdr`] documents the reasoning and the
//! tests hold it at the boundary.

pub use ::nfs3::{
    client, greeting, mount3, nfs3, nfs3_dir, nfs3_write, portmap, rpc, session, source, transport,
    xdr,
};
#[cfg(test)]
pub(crate) use ::nfs3::{dialer, fake};

pub mod connect;
pub mod fs;
pub mod mount;
#[cfg(test)]
mod real_server_tests;
pub mod served;

pub use ::nfs3::errno::Errno;

/// The kernel's errno for one of the NFS client's.
impl From<Errno> for crate::error::LxError {
    fn from(e: Errno) -> Self {
        crate::error::LxError::from_errno(e.0)
    }
}

/// The NFS client's errno for one of the kernel's.
pub fn errno_of(e: crate::error::LxError) -> Errno {
    Errno(e as i32)
}
