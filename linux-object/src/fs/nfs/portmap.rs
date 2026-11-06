//! Portmap / rpcbind version 2 (RFC 1833 section 3): asks a server which
//! port a program is listening on.
//!
//! An NFS server listens on three ports, and only one of them is fixed. NFS
//! itself is conventionally on 2049 — conventionally, not by standard, and
//! the convention is strong enough that Linux's `mount.nfs` defaults to it —
//! while `mountd` is wherever it happened to bind at boot, and finding it is
//! what portmap is for. Port 111 is the one port that is fixed, and it serves
//! the mapping.
//!
//! So a mount is: ask 111 for `mountd`'s port, ask `mountd` for the export's
//! root filehandle, then talk NFS. [`super::mount3`] is the second step.
//!
//! `GETPORT` is the only procedure here. `DUMP` (listing every registration)
//! would be useful for diagnosing a failed mount, but its reply is an
//! unbounded linked list from an untrusted server, and a client that can be
//! told its port can be told that much.
//!
//! # Versions
//!
//! This speaks portmap v2, not rpcbind v3/v4. A v4-only server would refuse
//! it with `PROG_MISMATCH`, which the error says plainly. In practice every
//! NFS server that answers on 111 answers v2 as well — Linux's own rpcbind
//! registers all three versions, and v2 is what `mount.nfs` asks first — and
//! v2 is the version that carries no address strings, so it is also the one
//! whose replies have nothing in them this client would have to parse out of
//! a universal address.

use alloc::vec::Vec;

use super::rpc::{CallTarget, RpcError, RpcResult};
use super::xdr::{XdrDecoder, XdrEncoder};

/// The portmap program number.
pub const PROGRAM_PORTMAP: u32 = 100_000;
/// The portmap version this client speaks.
pub const PORTMAP_VERSION: u32 = 2;
/// `PMAPPROC_GETPORT`.
pub const PROC_GETPORT: u32 = 3;

/// The fixed port the mapping itself is served on.
pub const PORTMAP_PORT: u16 = 111;

/// The NFS program number.
pub const PROGRAM_NFS: u32 = 100_003;
/// The MOUNT program number.
pub const PROGRAM_MOUNT: u32 = 100_005;

/// `IPPROTO_TCP`, as portmap's `prot` field spells it.
pub const IPPROTO_TCP: u32 = 6;
/// `IPPROTO_UDP`.
pub const IPPROTO_UDP: u32 = 17;

/// The conventional NFS port, and what this client assumes when it is not
/// told otherwise.
///
/// Asking portmap for it would be more correct and is what this code does
/// when asked to; defaulting to 2049 is what makes a mount work against a
/// server running NFS without rpcbind at all, which is now common (a
/// container image serving `nfsd` alone, and NFSv4 servers, which dropped
/// rpcbind entirely).
pub const NFS_PORT: u16 = 2049;

/// The target of a `GETPORT` call.
pub fn getport_target() -> CallTarget {
    CallTarget::new(PROGRAM_PORTMAP, PORTMAP_VERSION, PROC_GETPORT)
}

/// Encode `GETPORT`'s arguments: which program, version and transport the
/// port is wanted for.
///
/// The `port` field of the `mapping` struct is sent as zero — it is an output
/// of this call, and portmap ignores whatever is in it.
pub fn encode_getport(program: u32, version: u32, protocol: u32) -> Vec<u8> {
    let mut enc = XdrEncoder::with_capacity(16);
    enc.u32(program);
    enc.u32(version);
    enc.u32(protocol);
    enc.u32(0);
    enc.into_bytes()
}

/// Decode `GETPORT`'s reply: the port, or `None` where the program is not
/// registered.
///
/// Portmap answers an unregistered program with port 0 rather than an error,
/// so zero is the "not registered" case and not a port — mapping it to `None`
/// here keeps a caller from trying to connect to port 0 and reporting
/// whatever the stack says about that instead of "the server does not run
/// this".
///
/// A port above 65535 cannot be one; the field is 32 bits wide on the wire,
/// so the check has to happen somewhere, and doing it here means no caller
/// has to.
pub fn decode_getport(results: &[u8]) -> RpcResult<Option<u16>> {
    let mut dec = XdrDecoder::new(results);
    let port = dec.u32()?;
    match port {
        0 => Ok(None),
        p if p <= u32::from(u16::MAX) => Ok(Some(p as u16)),
        _ => Err(RpcError::Xdr(super::xdr::XdrError::LengthTooLarge)),
    }
}

#[cfg(test)]
#[path = "portmap_tests.rs"]
mod portmap_tests;
