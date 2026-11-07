//! Turning a `host:/export` device string into a mounted filesystem: the
//! four steps `mount(2)` takes between the syscall and a usable directory.
//!
//! The greeting ([`greeting`](super::greeting)) finds the ports and the root
//! filehandle. This adds what the greeting deliberately does not do: open the
//! connection the session will live on, ask `FSINFO`, and wrap the result in
//! the VFS types. It is one function because the four steps share one
//! deadline and because nothing else should be able to build a session that
//! skipped one of them.
//!
//! # Why the session's connection is not the greeting's
//!
//! The greeting opens a connection per step and closes each one: `rpcbind`
//! and `mountd` are asked once and then have no further part, and `mountd`
//! in particular has few slots and holding one is the leak the greeting's
//! docs describe. The session's connection to `nfsd` is the opposite — it
//! lives as long as the mount — so it is opened last, after everything that
//! could fail cheaply has had its chance to.
//!
//! It still comes out of the **same** deadline. A fourth step with its own
//! sixty seconds is exactly the arithmetic the one-deadline rule exists to
//! stop: whoever asked for a minute asked for a minute, and a mount that
//! fails after two is a mount whose timeout means nothing.
//!
//! # `FSINFO` is asked before the mount is called a success
//!
//! Linux's `nfs_probe_fsinfo` failing fails the mount, and the same applies
//! here for a reason beyond matching it: `FSINFO` is the first NFS call this
//! client makes with the session's own credential, on the handle `mountd`
//! handed over. A root handle that `nfsd` will not accept, a credential it
//! refuses, a port that answers RPC but is not `nfsd` — all of them are
//! invisible to the greeting and all of them surface here. Mounting first
//! and finding out on the first `ls` would turn a mount-time error into an
//! I/O error in whatever process touched the directory next.
//!
//! It is also what lowers the transfer sizes to what the server will carry,
//! so the first full-size `READ` is not the thing that discovers the
//! server's limit.

use alloc::sync::Arc;

use crate::error::LxResult;

use super::client::RpcTransport;
use super::connect::Connector;
use super::fs::NfsFileSystem;
use super::greeting::{self, Budget};
use super::session::NfsSession;
use super::source::{NfsOptions, NfsSource};

/// A mounted export: the filesystem, and what the greeting learned that
/// outlives it.
pub struct MountedExport<T: RpcTransport + Send + 'static> {
    /// What goes into the mount table.
    pub fs: Arc<NfsFileSystem<T>>,
    /// The port the session is talking to.
    pub nfs_port: u16,
    /// The port `mountd` answered on.
    ///
    /// Kept because `UMNT` at unmount time needs it and `rpcbind` may not be
    /// there to ask again — and because, `mountd`'s port being arbitrary,
    /// there is nothing to guess if it is lost.
    pub mount_port: u16,
}

/// Greet the server, open the session's connection, and hand back a
/// filesystem.
///
/// Every step inside one deadline; see the module docs for why that includes
/// the connection and why `FSINFO` is asked before this returns.
pub fn connect_export<C: Connector>(
    connector: &mut C,
    source: &NfsSource,
    opts: &NfsOptions,
) -> LxResult<MountedExport<C::Transport>>
where
    C::Transport: Send + 'static,
{
    let budget = Budget::new(greeting::greeting_budget(opts));
    let greeting = greeting::greet_with_deadline(connector, source, opts, &budget)?;
    let transport = connector.connect(greeting.nfs_port, budget.left()?)?;
    let mut session = NfsSession::new(
        transport,
        greeting::first_xid(),
        greeting.root.clone(),
        opts,
    );
    session.negotiate()?;
    Ok(MountedExport {
        fs: NfsFileSystem::new(session, greeting.root),
        nfs_port: greeting.nfs_port,
        mount_port: greeting.mount_port,
    })
}

#[cfg(test)]
#[path = "mount_tests.rs"]
mod mount_tests;
