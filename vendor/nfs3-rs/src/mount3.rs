//! The MOUNT protocol, version 3 (RFC 1813 appendix I): turns an export path
//! into the filehandle every later NFS call starts from.
//!
//! NFS itself has no "open this export" call and no notion of a path from
//! outside the filesystem. Every NFS procedure takes a filehandle, and the
//! only way to get the *first* one — the export's root — is to ask the MOUNT
//! service for it. That is the whole purpose of this protocol, and once
//! `MNT` has answered, nothing else here is on the path of any file
//! operation.
//!
//! Three procedures matter:
//!
//! - `MNT` (1) takes the export path and answers with the root filehandle
//!   and the auth flavours the server will accept for it.
//! - `UMNT` (3) tells the server this client is done, so it drops its entry
//!   from the export's list of clients (`/var/lib/nfs/rmtab`). It is
//!   advisory: nothing on the server depends on it, and nothing on the client
//!   breaks if it is lost, which is why it is sent once and not retried.
//! - `EXPORT` (5) lists the server's exports. Useful for diagnosing a mount
//!   that failed because the path was wrong, and it is a list of unbounded
//!   length from an untrusted server, so it is not implemented here.
//!
//! # The filehandle is a bearer token
//!
//! `MNT`'s answer is a handle of up to 64 opaque bytes, and RFC 1813 section
//! 2.6 is explicit that possession of it *is* the access: a server does not
//! check that the client asking a question about a handle is the one it gave
//! the handle to. That has two consequences this code has to respect.
//!
//! The first is that the export path is checked by the server, once, here —
//! and never again. After `MNT`, no NFS call mentions a path, so no NFS call
//! can be refused for being outside the export. A client that could be
//! tricked into using a handle from elsewhere would be outside the export
//! with the server's blessing.
//!
//! The second is that a handle must not be logged. It is a credential, and
//! `/var/log` is not where credentials go; [`FileHandle`]'s `Debug` prints
//! its length and not its bytes, for the same reason a password's would.

use alloc::vec::Vec;
use core::fmt;

use super::portmap::PROGRAM_MOUNT;
use super::rpc::{CallTarget, RpcError, RpcResult};
use super::xdr::{XdrDecoder, XdrEncoder, XdrError};

/// The MOUNT version this client speaks.
pub const MOUNT_VERSION: u32 = 3;
/// `MOUNTPROC3_MNT`.
pub const PROC_MNT: u32 = 1;
/// `MOUNTPROC3_UMNT`.
pub const PROC_UMNT: u32 = 3;

/// The largest an NFSv3 filehandle may be (`NFS3_FHSIZE`, RFC 1813 section
/// 2.3.1).
pub const FHSIZE3: usize = 64;

/// The longest export path this client will send (`MNTPATHLEN`, RFC 1813
/// appendix I).
pub const MNTPATHLEN: usize = 1024;

/// An NFS filehandle: the opaque bytes that name a file or directory on the
/// server.
///
/// The bytes mean nothing to this client — they are the server's own
/// encoding of an inode, a generation number and whatever else it keeps — and
/// they are a bearer token, so this type's `Debug` deliberately does not
/// print them.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct FileHandle(Vec<u8>);

impl FileHandle {
    /// A handle from bytes, refusing one longer than [`FHSIZE3`].
    ///
    /// The cap is the protocol's, so a longer handle is a malformed reply
    /// and not something to carry around: a server that sent one would get
    /// it back in the next call, where its own `FHSIZE3` check would refuse
    /// it, and the failure would surface far from here.
    pub fn new(bytes: &[u8]) -> Result<Self, XdrError> {
        if bytes.len() > FHSIZE3 {
            return Err(XdrError::LengthTooLarge);
        }
        Ok(Self(bytes.to_vec()))
    }

    /// The handle's bytes, to encode into a call.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// How many bytes the handle is.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the handle is empty, which no handle a server returns is.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for FileHandle {
    /// Prints the length, never the bytes: a handle is a credential (see the
    /// module docs), and a `Debug` that printed it would put it in every
    /// log line that logs a struct containing one.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FileHandle({} bytes)", self.0.len())
    }
}

/// `mountstat3`: how the MOUNT service answered.
///
/// The numbers are not NFS's and not errno's: RFC 1813 appendix I gives
/// MOUNT its own status values, which happen to use the same numbers as
/// errno for the first few and then diverge (`MNT3ERR_NOTSUPP` is 10004).
/// They are kept as the protocol's own type here, and translated where they
/// meet the kernel's errors, so nothing silently reads 10004 as an errno.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountStatus {
    /// `MNT3_OK`.
    Ok,
    /// `MNT3ERR_PERM` (1): not owner.
    Perm,
    /// `MNT3ERR_NOENT` (2): no such file or directory — the usual answer to
    /// an export path the server does not have.
    NoEnt,
    /// `MNT3ERR_IO` (5).
    Io,
    /// `MNT3ERR_ACCES` (13): permission denied, which for a mount usually
    /// means this client's address is not in the export's allowed list.
    Acces,
    /// `MNT3ERR_NOTDIR` (20).
    NotDir,
    /// `MNT3ERR_INVAL` (22).
    Inval,
    /// `MNT3ERR_NAMETOOLONG` (63).
    NameTooLong,
    /// `MNT3ERR_NOTSUPP` (10004): the server does not support this
    /// procedure.
    NotSupp,
    /// `MNT3ERR_SERVERFAULT` (10006).
    ServerFault,
    /// A status this client does not know.
    Unknown(u32),
}

impl MountStatus {
    /// The status a reply's `mountstat3` names.
    pub fn from_wire(v: u32) -> Self {
        match v {
            0 => Self::Ok,
            1 => Self::Perm,
            2 => Self::NoEnt,
            5 => Self::Io,
            13 => Self::Acces,
            20 => Self::NotDir,
            22 => Self::Inval,
            63 => Self::NameTooLong,
            10004 => Self::NotSupp,
            10006 => Self::ServerFault,
            other => Self::Unknown(other),
        }
    }
}

/// What `MNT` answered: the export's root handle and the auth flavours the
/// server will accept for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountReply {
    /// The export's root filehandle.
    pub root: FileHandle,
    /// The `auth_flavor`s the server lists as acceptable.
    ///
    /// Advisory: the server enforces what it enforces whatever it listed
    /// here, and an empty list is not a refusal. It is worth keeping because
    /// it is how a client can tell *before* the first NFS call that a server
    /// wanting Kerberos will refuse `AUTH_UNIX` — which this client only
    /// speaks — and say so, instead of reporting the eventual `AuthError` as
    /// if the mount path were at fault.
    pub auth_flavors: Vec<u32>,
}

/// The target of a `MNT` call.
pub fn mnt_target() -> CallTarget {
    CallTarget::new(PROGRAM_MOUNT, MOUNT_VERSION, PROC_MNT)
}

/// The target of a `UMNT` call.
pub fn umnt_target() -> CallTarget {
    CallTarget::new(PROGRAM_MOUNT, MOUNT_VERSION, PROC_UMNT)
}

/// Encode the export path argument `MNT` and `UMNT` both take.
///
/// Refuses a path over [`MNTPATHLEN`]: the protocol caps it, and a server
/// would answer `MNT3ERR_NAMETOOLONG` after a round trip.
pub fn encode_path(path: &str) -> Result<Vec<u8>, XdrError> {
    if path.len() > MNTPATHLEN {
        return Err(XdrError::LengthTooLarge);
    }
    let mut enc = XdrEncoder::with_capacity(path.len() + 8);
    enc.string(path);
    Ok(enc.into_bytes())
}

/// Decode a `MNT` reply.
///
/// `Ok(Err(status))` is a server that answered and refused; `Err(_)` is a
/// reply that did not decode. They are different things — the first is the
/// server's verdict on the export path, the second is a broken or hostile
/// server — so they are different layers of the return type rather than one
/// flattened error.
pub fn decode_mnt(results: &[u8]) -> RpcResult<Result<MountReply, MountStatus>> {
    let mut dec = XdrDecoder::new(results);
    let status = MountStatus::from_wire(dec.u32()?);
    if status != MountStatus::Ok {
        return Ok(Err(status));
    }
    let root = FileHandle::new(dec.opaque_max(FHSIZE3)?).map_err(RpcError::Xdr)?;
    // The flavour list's length comes off the wire, so it is hostile: it is
    // never used to reserve. Each flavour is four bytes, so a length longer
    // than the remaining buffer runs out of bytes on the first iteration
    // past the end and fails there — the loop can do no more work than the
    // buffer has bytes for.
    let count = dec.u32()? as usize;
    let mut auth_flavors = Vec::new();
    for _ in 0..count {
        auth_flavors.push(dec.u32()?);
    }
    Ok(Ok(MountReply { root, auth_flavors }))
}

#[cfg(test)]
#[path = "mount3_tests.rs"]
mod mount3_tests;
