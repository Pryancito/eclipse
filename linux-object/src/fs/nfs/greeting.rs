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

use super::client::{CallError, RpcClient};
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

/// `AUTH_UNIX`'s flavour number, and the only one an export may offer that
/// lets this client mount it.
///
/// `AUTH_NULL` (flavour 0) is deliberately **not** here. `rpcbind` is asked
/// with it, but every NFS call a session makes carries `AUTH_UNIX`, so an
/// export that lists `AUTH_NULL` and nothing else is one whose NFS calls
/// would all be refused. See [`check_flavors`].
const AUTH_UNIX_FLAVOR: u32 = 1;

/// How long the **whole** greeting may take, by default.
///
/// One deadline and not one per step. Three steps with their own sixty
/// seconds is a `mount(2)` that can hang for three minutes and then fail,
/// and whoever asked for sixty asked for sixty. Each step — the TCP
/// handshake as much as the RPC — gets what is left of this and no more.
///
/// A caller whose per-call `timeo` is *longer* than this gets their number
/// instead: asking for more patience per call is not asking for a shorter
/// mount.
pub const GREETING_TIMEOUT_MS: u64 = 60_000;

/// What is left of the greeting's deadline.
///
/// Absolute and read fresh, for the reason the transport's waits are: a
/// budget kept as "how much is left" and decremented by what each step
/// *thought* it would take does not notice a step that overran.
struct Budget {
    deadline: u64,
}

impl Budget {
    fn new(total_ms: u64) -> Self {
        Self {
            deadline: now_ms().saturating_add(total_ms),
        }
    }

    /// The milliseconds the next step may take, or `ETIMEDOUT` if the
    /// greeting is already out of time.
    fn left(&self) -> LxResult<u64> {
        let now = now_ms();
        if now >= self.deadline {
            return Err(LxError::ETIMEDOUT);
        }
        Ok(self.deadline - now)
    }
}

fn now_ms() -> u64 {
    kernel_hal::timer::timer_now().as_millis() as u64
}

/// Why asking `rpcbind` did not produce a port.
///
/// The distinction is the whole reason this is a type and not an errno:
/// **only a `rpcbind` that never answered permits the fallback to the
/// well-known NFS port.** Filtering on the errno cannot do it, because an
/// RPC that timed out and a reply this client could not parse both come out
/// as `EIO` — so a server whose `rpcbind` refuses the credential, or answers
/// with something malformed, would be treated as a server with no `rpcbind`
/// at all, and the mount would go on to dial 2049 on its word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortmapFailure {
    /// Nothing came back: the connection could not be made, or the call ran
    /// out of time. Says nothing at all about the program asked for.
    Unreachable(LxError),
    /// `rpcbind` answered, and the answer was not a port: no registration, a
    /// refused credential, a truncated or malformed reply. It spoke, so its
    /// word stands.
    Answered(LxError),
    /// This client could not make the call: no free local port, no memory.
    /// Nothing was asked, so nothing may be assumed.
    Local(LxError),
}

impl PortmapFailure {
    /// The errno to report when this failure ends the mount.
    pub fn errno(self) -> LxError {
        match self {
            Self::Unreachable(e) | Self::Answered(e) | Self::Local(e) => e,
        }
    }

    /// Whether the well-known NFS port may be tried in spite of this.
    fn permits_fallback(self) -> bool {
        matches!(self, Self::Unreachable(_))
    }
}

/// How a failure to connect to `rpcbind` is classified.
///
/// `EADDRNOTAVAIL` and `ENOMEM` are this machine's problem and not the
/// server's; everything else the connector reports is a connection that did
/// not happen.
fn connect_failure(e: LxError) -> PortmapFailure {
    match e {
        LxError::EADDRNOTAVAIL | LxError::ENOMEM => PortmapFailure::Local(e),
        _ => PortmapFailure::Unreachable(e),
    }
}

/// How a failed RPC to `rpcbind` is classified.
///
/// A timeout and a transport failure mean no answer arrived. Everything
/// else — a rejected credential, a truncated record, a reply that does not
/// decode — means bytes came back from `rpcbind`, and bytes are an answer.
fn call_failure(e: CallError) -> PortmapFailure {
    match e {
        CallError::TimedOut | CallError::Transport(_) => {
            PortmapFailure::Unreachable(call_error_to_lx(e))
        }
        CallError::Rpc(_) | CallError::TruncatedMessage => {
            PortmapFailure::Answered(call_error_to_lx(e))
        }
    }
}

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
/// `Ok(port)` is a registration. A failure keeps **why** it failed, as a
/// [`PortmapFailure`], because the caller's next move depends on it and an
/// errno cannot carry the difference: see [`greet`], where it is what
/// separates a server with no `rpcbind` from a server whose `rpcbind` said
/// something this client did not like.
///
/// `timeout_ms` is what is left of the greeting's deadline, and it bounds
/// the TCP handshake as well as the call.
pub fn ask_portmap<C: Connector>(
    connector: &mut C,
    program: u32,
    version: u32,
    timeout_ms: u64,
    opts: &NfsOptions,
) -> Result<u16, PortmapFailure> {
    let transport = connector
        .connect(portmap::PORTMAP_PORT, timeout_ms)
        .map_err(connect_failure)?;
    let mut client = RpcClient::new(transport, first_xid(), policy(opts, timeout_ms));
    let args = portmap::encode_getport(program, version, portmap::IPPROTO_TCP);
    let results = client
        .call(portmap::getport_target(), &PORTMAP_CRED, &args)
        .map_err(call_failure)?;
    match portmap::decode_getport(&results) {
        // Zero: "I have no registration for that program". An answer, and
        // dialling it would fail somewhere else, as something else.
        Ok(None) => Err(PortmapFailure::Answered(LxError::EOPNOTSUPP)),
        Ok(Some(port)) => Ok(port),
        // Bytes that do not decode still came from `rpcbind`.
        Err(e) => Err(PortmapFailure::Answered(rpc_error_to_lx(e))),
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
    timeout_ms: u64,
    opts: &NfsOptions,
) -> LxResult<(FileHandle, Vec<u32>)> {
    let args = mount3::encode_path(export).map_err(super::session::xdr_error_to_lx)?;
    let reply = {
        let transport = connector.connect(port, timeout_ms)?;
        let mut client = RpcClient::new(transport, first_xid(), policy(opts, timeout_ms));
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
    // Its own budget, and not the mount's: an unmount happens long after the
    // greeting, and there is nothing left of that deadline to spend.
    let budget = Budget::new(greeting_budget(opts));
    let transport = connector.connect(port, budget.left()?)?;
    let mut client = RpcClient::new(transport, first_xid(), policy(opts, budget.left()?));
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
///
/// # One deadline for the whole thing
///
/// [`GREETING_TIMEOUT_MS`] bounds the greeting end to end, and every step —
/// each TCP handshake and each RPC — is given what is left of it. Without
/// that, three steps each finishing just inside their own sixty-second
/// timeout is a `mount(2)` that returns a failure after three minutes, and
/// nothing in the code would be wrong by its own measure.
pub fn greet<C: Connector>(
    connector: &mut C,
    source: &NfsSource,
    opts: &NfsOptions,
) -> LxResult<Greeting> {
    greet_with_budget(connector, source, opts, greeting_budget(opts))
}

/// [`greet`] with the total budget given rather than derived.
///
/// Separate so the budget is something a test can set: the alternative is a
/// test that waits out a real minute, and a bound nobody can afford to
/// measure is a bound nobody measures.
pub fn greet_with_budget<C: Connector>(
    connector: &mut C,
    source: &NfsSource,
    opts: &NfsOptions,
    budget_ms: u64,
) -> LxResult<Greeting> {
    let budget = Budget::new(budget_ms);
    let mount_port = match opts.mountport {
        Some(port) => port,
        // No fallback: there is no well-known port for `mountd`, and a
        // `rpcbind` that would not answer about it has left nothing to guess
        // from. Whatever went wrong is reported as it was.
        None => ask_portmap(
            connector,
            portmap::PROGRAM_MOUNT,
            mount3::MOUNT_VERSION,
            budget.left()?,
            opts,
        )
        .map_err(PortmapFailure::errno)?,
    };
    let nfs_port = match opts.port {
        Some(port) => port,
        None => match ask_portmap(
            connector,
            portmap::PROGRAM_NFS,
            super::nfs3::NFS_VERSION,
            budget.left()?,
            opts,
        ) {
            Ok(port) => port,
            // The fallback to the well-known port is for one case only:
            // `rpcbind` never answered, which says nothing about `nfsd`.
            //
            // Everything else is reported. `rpcbind` answering "no
            // registration" is an answer -- it knows, and it says no -- and
            // so is a refused credential or a reply that does not decode:
            // bytes came back, and dialling 2049 against them is dialling a
            // port the server never offered. A local failure (no free port,
            // no memory) asked nothing at all. **None of these can be told
            // apart by errno**, which is why the failure carries its own
            // provenance: an RPC that timed out and a malformed reply are
            // both `EIO`.
            Err(failure) if failure.permits_fallback() => portmap::NFS_PORT,
            Err(failure) => return Err(failure.errno()),
        },
    };
    let (root, auth_flavors) =
        mount_export(connector, mount_port, &source.export, budget.left()?, opts)?;
    Ok(Greeting {
        nfs_port,
        mount_port,
        root,
        auth_flavors,
    })
}

/// The greeting's total budget: [`GREETING_TIMEOUT_MS`], or the mount's own
/// per-call timeout when that is longer.
///
/// A caller who raised `timeo` asked for more patience per call, which is
/// not a request for a shorter mount -- so their number becomes the floor
/// rather than being overridden by it.
fn greeting_budget(opts: &NfsOptions) -> u64 {
    opts.timeout_ms.max(GREETING_TIMEOUT_MS)
}

/// Whether this client can mount with any of the flavours the server listed.
///
/// An empty list passes: it means the server did not say, and many do not.
/// See the module docs for why a non-empty list is checked at mount time
/// rather than left to the first call to fail with `EACCES`.
///
/// **`AUTH_NULL` in the list does not count**, and that is the whole point of
/// the check. A session's calls are `AUTH_UNIX`: `NfsSession::new` installs
/// it and `set_identity` installs it again, and no API selects `AUTH_NULL`.
/// So an export that lists `AUTH_NULL` and nothing else is an export whose
/// every NFS call is refused — exactly the delayed `EACCES` this check
/// exists to turn into a mount that fails saying why. Accepting it because
/// this client *can* encode `AUTH_NULL` confuses "can send" with "can mount
/// with". If a session ever chooses its flavour, the flavour it chose is
/// what belongs here.
fn check_flavors(flavors: &[u32]) -> LxResult<()> {
    if flavors.is_empty() {
        return Ok(());
    }
    if flavors.contains(&AUTH_UNIX_FLAVOR) {
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
/// The retry policy for one step of the greeting, inside `timeout_ms`.
///
/// The per-call timeout is the **smaller** of what the mount asked for and
/// what is left of the greeting's deadline, and the retransmissions are
/// capped so that the retries cannot outlast the budget either: a policy
/// with the budget as its timeout and three retries still takes four
/// budgets.
fn policy(opts: &NfsOptions, timeout_ms: u64) -> super::client::RetryPolicy {
    let per_call = opts.timeout_ms.min(timeout_ms).max(1);
    // How many whole per-call timeouts the budget holds, minus the first
    // try. Zero retries when there is only room for one attempt.
    let affordable = (timeout_ms / per_call).saturating_sub(1);
    super::client::RetryPolicy {
        timeout_ms: per_call,
        retrans: opts.retrans.min(affordable as u32),
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
