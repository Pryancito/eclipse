//! The greeting: everything that happens **before** there is a filesystem.
//!
//! An NFSv3 mount is three conversations with three different programs on
//! three different ports, and only the last of them is NFS:
//!
//! 1. **portmap** (program 100000, always on port 111) is asked where the
//!    other two live. Its answer is a port number, and a port of zero is its
//!    way of saying "that program is not registered here".
//! 2. **`mountd`** (100005) is asked for the export by path, and answers with
//!    the **root filehandle** — the one opaque blob from which every other
//!    handle in the mount is reached. Without it there is nothing to call NFS
//!    about, which is why this is a separate step and not part of the session.
//! 3. **`nfsd`** (100003) is what the session of the previous batch then
//!    talks to, and this module only works out which port that is.
//!
//! # The traps, each of which is a mount that looks like it worked
//!
//! **A portmap answer of zero is not a port.** `GETPORT` answers `0` for a
//! program it has no registration for, and dialling port 0 is a different
//! failure in a different place: smoltcp refuses it as unaddressable, and on
//! a machine that accepted it the connection would be refused by the server's
//! stack. Reported here as `EOPNOTSUPP` — the server is answering, it just
//! does not run that program — which is what Linux reports for
//! `RPC: program not registered`.
//!
//! **`mountd`'s connection is not the mount's connection.** `MNT` is answered
//! and finished with; the mount then lives on the NFS port. Keeping the
//! `mountd` connection open holds a slot in a daemon that commonly has very
//! few, and the leak only shows up as the *next* mount hanging, so the
//! connection is dropped as soon as the handle is out of it.
//!
//! **A `MNT` that succeeded can still be a mount that cannot work.** The
//! reply lists the auth flavours the export accepts. A server wanting
//! Kerberos lists `RPCSEC_GSS` and nothing this client speaks, and every NFS
//! call afterwards is refused with an auth error — which arrives as `EACCES`
//! on the first `stat`, long after the mount said it was fine. Checked here,
//! where it can still be reported as a refusal of the mount. An **empty**
//! list is not a refusal: it means the server did not say, and plenty do not.
//!
//! **`UMNT` is advisory.** The export is unmounted locally whatever `mountd`
//! answers, because the alternative is a mount that cannot be removed because
//! the server that forgot about it will not confirm it. Linux's `nfs_umount`
//! does the same, for the same reason.
//!
//! **One deadline for the whole greeting**, not one per step. Three steps
//! with a 60 s timeout each is a `mount(2)` that can hang for three minutes
//! and then fail, and the caller asked for sixty.

use alloc::vec::Vec;

use crate::error::{LxError, LxResult};

use super::client::RpcClient;
use super::connect::Connector;
use super::mount3::{self, FileHandle, MountStatus};
use super::portmap;
use super::rpc::{AuthUnix, Credential};
use super::session::{call_error_to_lx, rpc_error_to_lx};
use super::source::{machine_name, NfsOptions, NfsSource};

/// `AUTH_NULL`, which is what `rpcbind` is asked with.
///
/// It does not care who asks — a port number is not privileged information —
/// and a credential it does not recognise is a refused call for no reason.
/// `mountd` is a different matter and gets `AUTH_UNIX`: it decides what the
/// export allows for *this* client, and some servers refuse `AUTH_NULL`
/// outright.
const PORTMAP_CRED: Credential = Credential::Null;

/// `AUTH_UNIX` flavour number, and `AUTH_NULL`'s.
///
/// The two this client can speak. A `MNT` reply that lists flavours and
/// neither of these among them is an export this client cannot use.
const AUTH_NULL_FLAVOR: u32 = 0;
const AUTH_UNIX_FLAVOR: u32 = 1;

/// What the greeting found out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Greeting {
    /// The port `nfsd` is on, which is where the session connects.
    pub nfs_port: u16,
    /// The port `mountd` answered on, kept for the `UMNT` at unmount time.
    pub mount_port: u16,
    /// The export's root filehandle.
    pub root: FileHandle,
    /// The auth flavours the server listed, as it listed them.
    pub auth_flavors: Vec<u32>,
}

/// Ask `rpcbind` on port 111 where `program` version `version` is.
///
/// `Ok(port)` is a registration; `Err(EOPNOTSUPP)` is the answer zero, which
/// means there is no registration. A connection that cannot be made at all is
/// whatever the connector reported, and the caller decides what to do about
/// it — see [`greet`], where that is the difference between a server with no
/// `rpcbind` and a server that is not there.
pub fn ask_portmap<C: Connector>(
    connector: &mut C,
    program: u32,
    version: u32,
    opts: &NfsOptions,
) -> LxResult<u16> {
    let transport = connector.connect(portmap::PORTMAP_PORT)?;
    let mut client = RpcClient::new(transport, first_xid(), policy(opts));
    let args = portmap::encode_getport(program, version, portmap::IPPROTO_TCP);
    let results = client
        .call(portmap::getport_target(), &PORTMAP_CRED, &args)
        .map_err(call_error_to_lx)?;
    match portmap::decode_getport(&results).map_err(rpc_error_to_lx)? {
        Some(port) => Ok(port),
        // Zero: "I have no registration for that program". Dialling it would
        // fail somewhere else, as something else.
        None => Err(LxError::EOPNOTSUPP),
    }
}

/// Ask `mountd` for an export, and come away with its root handle.
///
/// The connection to `mountd` lives and dies inside this call. See the module
/// docs: holding it open leaks a slot in a daemon that has few.
pub fn mount_export<C: Connector>(
    connector: &mut C,
    port: u16,
    export: &str,
    opts: &NfsOptions,
) -> LxResult<(FileHandle, Vec<u32>)> {
    let args = mount3::encode_path(export).map_err(super::session::xdr_error_to_lx)?;
    let reply = {
        let transport = connector.connect(port)?;
        let mut client = RpcClient::new(transport, first_xid(), policy(opts));
        let results = client
            .call(mount3::mnt_target(), &unix_cred(), &args)
            .map_err(call_error_to_lx)?;
        mount3::decode_mnt(&results).map_err(rpc_error_to_lx)?
    };
    let reply = reply.map_err(mount_status_to_lx)?;
    check_flavors(&reply.auth_flavors)?;
    Ok((reply.root, reply.auth_flavors))
}

/// Tell `mountd` the export is no longer mounted.
///
/// Advisory, and the return type says so: `Err` here is information for the
/// log and never a reason to keep a mount the caller has finished with. See
/// the module docs.
pub fn unmount_export<C: Connector>(
    connector: &mut C,
    port: u16,
    export: &str,
    opts: &NfsOptions,
) -> LxResult<()> {
    let args = mount3::encode_path(export).map_err(super::session::xdr_error_to_lx)?;
    let transport = connector.connect(port)?;
    let mut client = RpcClient::new(transport, first_xid(), policy(opts));
    // `UMNT` is a void procedure: there is nothing to decode, and an empty
    // result is the success.
    client
        .call(mount3::umnt_target(), &unix_cred(), &args)
        .map_err(call_error_to_lx)?;
    Ok(())
}

/// The whole greeting: find the ports, mount the export, hand back what the
/// session needs.
///
/// Where the ports come from, in order of what the caller asked for:
///
/// - `mountport` or `port` in the options wins outright, and skips `rpcbind`
///   for that program. A server reachable on a known port need not run
///   `rpcbind` at all, and asking anyway is a round trip and a failure mode
///   for nothing.
/// - Otherwise `rpcbind` is asked.
/// - And if `rpcbind` itself cannot be reached, the **NFS** port falls back to
///   the well-known 2049, while `mountd` does not fall back at all. That
///   asymmetry is deliberate and is a divergence from Linux, which fails the
///   mount: `nfsd` on 2049 is near-universal and a growing number of servers
///   run it with no `rpcbind`, whereas `mountd`'s port is genuinely arbitrary
///   and a guess at it would dial some other service.
pub fn greet<C: Connector>(
    connector: &mut C,
    source: &NfsSource,
    opts: &NfsOptions,
) -> LxResult<Greeting> {
    let mount_port = match opts.mountport {
        Some(port) => port,
        // No fallback: there is no well-known port for `mountd`.
        None => ask_portmap(
            connector,
            portmap::PROGRAM_MOUNT,
            mount3::MOUNT_VERSION,
            opts,
        )?,
    };
    let nfs_port = match opts.port {
        Some(port) => port,
        None => match ask_portmap(
            connector,
            portmap::PROGRAM_NFS,
            super::nfs3::NFS_VERSION,
            opts,
        ) {
            Ok(port) => port,
            // `EOPNOTSUPP` is `rpcbind` saying there is no registration, and
            // that is an answer: it knows, and it says no. Anything else is
            // `rpcbind` not being reachable, which says nothing about
            // `nfsd` -- so the well-known port is tried rather than failing a
            // mount that would have worked.
            Err(LxError::EOPNOTSUPP) => return Err(LxError::EOPNOTSUPP),
            Err(_) => portmap::NFS_PORT,
        },
    };
    let (root, auth_flavors) = mount_export(connector, mount_port, &source.export, opts)?;
    Ok(Greeting {
        nfs_port,
        mount_port,
        root,
        auth_flavors,
    })
}

/// Whether this client can speak any of the flavours the server listed.
///
/// An empty list passes: it means the server did not say. See the module
/// docs for why this is checked at mount time rather than left to the first
/// call to fail with `EACCES`.
fn check_flavors(flavors: &[u32]) -> LxResult<()> {
    if flavors.is_empty() {
        return Ok(());
    }
    if flavors
        .iter()
        .any(|&f| f == AUTH_UNIX_FLAVOR || f == AUTH_NULL_FLAVOR)
    {
        return Ok(());
    }
    Err(LxError::EACCES)
}

/// The errno for `mountd`'s verdict on an export path.
///
/// The `MNT3ERR_*` numbers are a subset of `nfsstat3` and mean here what they
/// mean there. The same mapping the session uses, written out again only
/// because `MountStatus` is its own type and a `match` on it is what makes a
/// new status a compile error rather than a silent `EIO`.
pub fn mount_status_to_lx(status: MountStatus) -> LxError {
    match status {
        // Never reached through `mount_export`, which only maps a refusal --
        // but a `match` and not a `_`, so a status added to the protocol type
        // is a compile error here rather than a silent `EIO`.
        MountStatus::Ok => LxError::EIO,
        MountStatus::Perm => LxError::EPERM,
        // The usual answer to an export path the server does not have.
        MountStatus::NoEnt => LxError::ENOENT,
        MountStatus::Io => LxError::EIO,
        // For a mount this usually means this client's address is not in the
        // export's allowed list, which is a refusal and not an I/O error.
        MountStatus::Acces => LxError::EACCES,
        MountStatus::NotDir => LxError::ENOTDIR,
        MountStatus::Inval => LxError::EINVAL,
        MountStatus::NameTooLong => LxError::ENAMETOOLONG,
        // The server does not do this procedure of MOUNTv3.
        MountStatus::NotSupp => LxError::EOPNOTSUPP,
        MountStatus::ServerFault => LxError::EIO,
        MountStatus::Unknown(_) => LxError::EIO,
    }
}

/// `AUTH_UNIX` as root, which is what `mountd` is asked with.
///
/// The same credential the session starts with, and for the same reason: the
/// server decides what root means for the export, and most map it to
/// `nobody`.
fn unix_cred() -> Credential {
    Credential::Unix(AuthUnix::new(&machine_name(), 0, 0))
}

/// The retry policy for a greeting call.
fn policy(opts: &NfsOptions) -> super::client::RetryPolicy {
    super::client::RetryPolicy {
        timeout_ms: opts.timeout_ms,
        retrans: opts.retrans,
        // The greeting is always soft, whatever the mount asked for. A `hard`
        // mount means "wait for the server rather than fail the call", and a
        // mount that has not happened yet has no calls to protect: all a
        // hard greeting could do is hold `mount(2)` for ever, on the thread
        // that drives the network stack.
        soft: true,
        ..super::client::RetryPolicy::default()
    }
}

/// A starting transaction id for a connection that lives for one or two
/// calls.
///
/// Derived from the clock for the reason [`RpcClient::new`] gives: a reply
/// from a previous connection to the same server, arriving on a reused local
/// port, must not carry an id this client is waiting for.
fn first_xid() -> u32 {
    kernel_hal::timer::timer_now().as_nanos() as u32
}

#[cfg(test)]
#[path = "greeting_tests.rs"]
mod greeting_tests;
