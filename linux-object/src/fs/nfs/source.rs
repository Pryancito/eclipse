//! The `host:/export` string `mount(8)` passes as an NFS mount's device, and
//! the `nfs` mount options Eclipse understands.
//!
//! `mount -t nfs 192.168.1.10:/srv/media /mnt` hands the kernel
//! `"192.168.1.10:/srv/media"` where a block filesystem would get
//! `"/dev/sda1"`. Splitting it is the first thing that happens on the mount
//! path and the first thing that can go wrong, so it is a separate, tested
//! function rather than an `rsplit` inline somewhere.
//!
//! # Where the separator is, which took three tries
//!
//! A naive `split(':')` works for every IPv4 address and hostname and breaks
//! on the first IPv6 one, because an IPv6 literal is *full* of colons.
//! `mount(8)` resolves that with brackets — `[fe80::1]:/export` — so the
//! parse has to look at the first character before it decides anything, or
//! `[fe80::1]:/export` yields host `"[fe80"` and reports a bad hostname for a
//! perfectly good one.
//!
//! For the unbracketed forms there are three candidate rules, and two of them
//! are wrong:
//!
//! - **The first colon**, which is what Linux does (`strchr` in
//!   `nfs_parse_devname`). Correct for every hostname and IPv4 literal, and
//!   for an export path with a colon in it — which is legal, since a colon is
//!   an ordinary character in a path. It breaks an unbracketed IPv6 literal,
//!   which Linux simply does not accept.
//! - **The last colon**, which this module did at first, to accept
//!   unbracketed IPv6 as well. It breaks a path with a colon in it, and
//!   breaks it *silently*: `nas:/srv/team:/data` splits into host
//!   `"nas:/srv/team"` and export `"/data"`, and that host is then resolved
//!   and mounted from. A mount against the wrong machine, with no error
//!   anywhere. Found by review.
//! - **The first colon that is followed by a slash**, which is what this does
//!   now. The export path is absolute, so the separator is always such a
//!   colon; taking the first means a colon later in the path stays in the
//!   path, and the colons inside an IPv6 literal are never followed by a
//!   slash, so they are never mistaken for it. `fe80::1:/export` and
//!   `nas:/srv/a:/b` both come out right.
//!
//! Accepting an unbracketed IPv6 literal at all is a deliberate divergence
//! from Linux. It costs nothing here — the rule that makes paths with colons
//! work handles it for free — and it is what someone types.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use smoltcp::wire::IpAddress;

use crate::error::LxError;

/// A parsed `host:/export` device string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NfsSource {
    /// The server, as written: a hostname, an IPv4 literal, or an IPv6
    /// literal with its brackets already stripped.
    ///
    /// Not resolved here. Turning a hostname into an address needs the
    /// kernel's resolver, which is `async`, and this is a pure function; the
    /// mount path resolves it.
    pub host: String,
    /// The export path on the server, which always starts with `/`.
    pub export: String,
}

impl NfsSource {
    /// Split a `host:/export` string.
    ///
    /// Returns `EINVAL` for anything that is not one, which is what Linux's
    /// `nfs_parse_devname` does: an empty host, an empty export, a string
    /// with no colon at all, or brackets that do not close.
    pub fn parse(source: &str) -> Result<Self, LxError> {
        // An IPv6 literal is bracketed, and everything inside the brackets
        // is the host however many colons it holds.
        let (host, rest) = if let Some(after) = source.strip_prefix('[') {
            let end = after.find(']').ok_or(LxError::EINVAL)?;
            (&after[..end], &after[end + 1..])
        } else {
            // The first colon followed by a slash. See the module docs: the
            // first colon alone keeps a path's own colons out of the host but
            // refuses an unbracketed IPv6 literal, and the last colon
            // silently mounts the wrong machine for a path that has one.
            let idx = source
                .as_bytes()
                .windows(2)
                .position(|w| w == b":/")
                .ok_or(LxError::EINVAL)?;
            (&source[..idx], &source[idx..])
        };
        let export = rest.strip_prefix(':').ok_or(LxError::EINVAL)?;
        if host.is_empty() || export.is_empty() {
            return Err(LxError::EINVAL);
        }
        // Linux refuses a relative export path: the server names its exports
        // absolutely, and a relative one has nothing to be relative to.
        if !export.starts_with('/') {
            return Err(LxError::EINVAL);
        }
        Ok(Self {
            host: host.to_string(),
            export: export.to_string(),
        })
    }
}

/// How many bytes a single NFS READ or WRITE may carry.
///
/// Linux's default is 1 MiB over TCP and it negotiates down to what the
/// server offers (`FSINFO`'s `rtpref`/`wtpref`). This client starts at 32 KiB
/// and will negotiate in a later batch: the size bounds a kernel-heap
/// allocation per in-flight call, and on a machine whose heap is the pressure
/// point a megabyte per reader is the wrong default to start from. It is a
/// mount option precisely so it need not be guessed.
pub const DEFAULT_TRANSFER_SIZE: usize = 32 * 1024;

/// The largest transfer size this client accepts, negotiated or asked for.
///
/// A READ reply of this size is buffered whole before it is parsed, so the
/// cap is a bound on kernel memory per in-flight call and not a performance
/// knob. It is below [`rpc::MAX_MESSAGE_LEN`](super::rpc::MAX_MESSAGE_LEN) by
/// enough for the RPC and NFS envelopes around the payload.
pub const MAX_TRANSFER_SIZE: usize = 512 * 1024;

/// A transfer size the record assembler would refuse is a mount option that
/// works until the first full-size read and then fails as a framing error, so
/// the relation between the two caps is checked when the kernel is built
/// rather than left to a test — raising either one on its own cannot compile.
const _: () = assert!(
    MAX_TRANSFER_SIZE < super::rpc::MAX_MESSAGE_LEN,
    "a full-size READ payload plus its RPC and NFS envelopes must fit in one \
     assembled message"
);

/// The `nfs` mount options Eclipse understands, after parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NfsOptions {
    /// The port NFS itself is on.
    ///
    /// `None` means "ask portmap"; the default is
    /// [`portmap::NFS_PORT`](super::portmap::NFS_PORT), because a growing
    /// number of servers run `nfsd` with no rpcbind at all.
    pub port: Option<u16>,
    /// The port `mountd` is on, or `None` to ask portmap — which is the
    /// default, since `mountd`'s port is genuinely arbitrary.
    pub mountport: Option<u16>,
    /// Bytes per READ.
    pub rsize: usize,
    /// Bytes per WRITE.
    pub wsize: usize,
    /// How long to wait for a reply before giving up on a call, in
    /// milliseconds.
    ///
    /// Linux's `timeo` is in tenths of a second and its default for TCP is
    /// 600 — a full minute — because a TCP retransmit already covers packet
    /// loss, so this timer only ever fires for a server that is gone or
    /// wedged. The same reasoning gives the same default here.
    pub timeout_ms: u64,
    /// How many times a timed-out call is retried before it fails.
    pub retrans: u32,
    /// Whether a call that keeps timing out eventually fails
    /// (`soft`) or is retried until the server comes back (`hard`).
    ///
    /// Linux defaults to `hard`, and this does not: a hard mount's blocked
    /// call is unkillable in Linux without `intr`, and in this kernel a
    /// VFS call blocks a kernel thread that is driving the network stack
    /// itself (see the batch that adds the transport). A hung server would
    /// take the machine, not the process. `soft` is a deliberate divergence
    /// and reversible with `hard` in the options.
    pub soft: bool,
    /// Mount read-only regardless of what the server would allow.
    pub read_only: bool,
    /// The server's address, when the caller already knew it.
    ///
    /// This is Linux's `addr=`, and it is there for the same reason: the
    /// kernel's NFS client does not resolve names. `mount.nfs(8)` resolves
    /// the hostname in userspace and passes the result in the option string,
    /// so the kernel is handed an address and a name it only has to record.
    /// Eclipse's `mount(2)` does resolve a name it is given -- there is no
    /// `mount.nfs` helper here -- but a caller that passes `addr=` is taken
    /// at its word and nothing is looked up, which is what makes a mount
    /// against a literal address work on a machine with no resolver.
    pub addr: Option<IpAddress>,
}

impl Default for NfsOptions {
    fn default() -> Self {
        Self {
            port: None,
            mountport: None,
            rsize: DEFAULT_TRANSFER_SIZE,
            wsize: DEFAULT_TRANSFER_SIZE,
            timeout_ms: 60_000,
            retrans: 3,
            soft: true,
            read_only: false,
            addr: None,
        }
    }
}

impl NfsOptions {
    /// Parse `mount(2)`'s comma-separated option string.
    ///
    /// Unknown options are ignored, not refused. That is what every Linux
    /// filesystem does with the generic options the VFS has already consumed
    /// (`rw`, `relatime`, `nosuid`) and what makes an `fstab` line written
    /// for Linux work here: `mount(8)` passes along a dozen options this
    /// client has no opinion about, and refusing them would fail the mount
    /// over a flag that changes nothing.
    ///
    /// A *malformed* value is a different matter and is refused: `port=abc`
    /// or `rsize=0` is a mistake in the line, and silently mounting with a
    /// default instead hides it until the mount behaves unlike what was
    /// asked for.
    pub fn parse(data: &str) -> Result<Self, LxError> {
        let mut out = Self::default();
        for opt in data.split(',').filter(|s| !s.is_empty()) {
            let (key, value) = match opt.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (opt, None),
            };
            match key {
                "port" => out.port = Some(parse_port(value)?),
                "mountport" => out.mountport = Some(parse_port(value)?),
                "rsize" => out.rsize = parse_transfer_size(value)?,
                "wsize" => out.wsize = parse_transfer_size(value)?,
                // Linux's `timeo` is in tenths of a second, and an `fstab`
                // line copied from a Linux box says `timeo=600` meaning a
                // minute. Reading it as milliseconds would silently make
                // every timeout 600 ms, so the unit is converted rather than
                // the name reused for something else.
                "timeo" => out.timeout_ms = parse_u64(value)?.saturating_mul(100),
                "retrans" => out.retrans = parse_u64(value)?.min(u64::from(u32::MAX)) as u32,
                "soft" => out.soft = true,
                "hard" => out.soft = false,
                "addr" => out.addr = Some(parse_addr(value)?),
                "ro" => out.read_only = true,
                "rw" => out.read_only = false,
                // Everything else is the VFS's or another client's.
                _ => {}
            }
        }
        Ok(out)
    }
}

/// An `addr=` option: an address literal, bracketed or not.
///
/// Brackets are accepted because the device string takes them for an IPv6
/// literal (`[fe80::1]:/export`) and a person who writes them there writes
/// them here. A name is refused: `addr=` means "do not resolve", so a name
/// in it is a mistake in the line rather than something to look up, and
/// looking it up anyway would make `addr=` mean nothing.
fn parse_addr(value: Option<&str>) -> Result<IpAddress, LxError> {
    let s = value.ok_or(LxError::EINVAL)?;
    let s = match s.strip_prefix('[') {
        Some(rest) => rest.strip_suffix(']').ok_or(LxError::EINVAL)?,
        None => s,
    };
    crate::net::dns::parse_literal(s).ok_or(LxError::EINVAL)
}

/// A port number option: 1 to 65535. Zero is refused — it is not a port, and
/// `port=0` in Linux means "ask portmap", which is this client's default and
/// is better asked for by leaving the option out than by a value that looks
/// like a number.
fn parse_port(value: Option<&str>) -> Result<u16, LxError> {
    let v = parse_u64(value)?;
    if v == 0 || v > u64::from(u16::MAX) {
        return Err(LxError::EINVAL);
    }
    Ok(v as u16)
}

/// An `rsize`/`wsize` option, rounded as NFS requires and capped.
///
/// A transfer size has to be a multiple of 512 — Linux rounds it to a page
/// and servers assume a block multiple — and must be neither zero nor over
/// [`MAX_TRANSFER_SIZE`]. Rounding *up* a value below 512 rather than
/// refusing it is what Linux does with `rsize=100`, and the clamp is what
/// keeps `rsize=1073741824` from being a kernel allocation of a gibibyte.
fn parse_transfer_size(value: Option<&str>) -> Result<usize, LxError> {
    let v = parse_u64(value)?;
    if v == 0 {
        return Err(LxError::EINVAL);
    }
    let v = v.min(MAX_TRANSFER_SIZE as u64) as usize;
    Ok(v.next_multiple_of(512).min(MAX_TRANSFER_SIZE))
}

/// A decimal option value. A missing value, an empty one, or one with a
/// non-digit in it is `EINVAL`.
fn parse_u64(value: Option<&str>) -> Result<u64, LxError> {
    let s = value.ok_or(LxError::EINVAL)?;
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(LxError::EINVAL);
    }
    s.parse::<u64>().map_err(|_| LxError::EINVAL)
}

/// The hostname this client states in its `AUTH_UNIX` credential.
///
/// The server may log it and that is all it is for: nothing on the server
/// depends on it, and nothing checks it. A fixed name is enough, and is one
/// fewer thing that varies between machines when a mount is being debugged.
pub fn machine_name() -> Vec<u8> {
    b"eclipse".to_vec()
}

#[cfg(test)]
#[path = "source_tests.rs"]
mod source_tests;
