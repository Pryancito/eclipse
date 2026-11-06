//! The NFSv3 types (RFC 1813 section 2.5) and the procedures of the read
//! path.
//!
//! Everything here is still a pure function from bytes to bytes, so all of it
//! is tested on the host. What is *not* here is the write path and
//! `READDIRPLUS`, which are the next batch, and the socket, which is the one
//! after.
//!
//! # The shape of every NFSv3 reply
//!
//! Each procedure answers with a status word and then a union on it: the
//! results when it is `NFS3_OK`, and — in most cases — a *different*,
//! smaller set of fields when it is not. That second half is the part a
//! client is tempted to skip, and skipping it is how a decoder ends up one
//! field out of step with the server for the rest of the connection.
//!
//! Most replies also carry attributes the client did not ask for, as
//! `post_op_attr`: the server's current idea of the object, attached to
//! whatever else it was answering. They are not decoration. They are what
//! makes an NFS client's attribute cache work at all — every reply refreshes
//! what the client knows, so a `stat` after a `read` usually needs no call —
//! and they are optional on the wire, so a server that declines to send them
//! has to be handled rather than assumed away.
//!
//! # Mapping errors
//!
//! `nfsstat3` values are *mostly* errno numbers and then stop being them:
//! `NFS3ERR_NOT_SYNC` is 10002, `NFS3ERR_BADHANDLE` is 10001, and
//! `NFS3ERR_JUKEBOX` is 10008. Treating the lot as errno — which the first
//! dozen invite — sends numbers to userspace that no `errno` names. They stay
//! the protocol's own type here and are translated explicitly, once, in
//! [`Nfs3Status::to_lx_error`].

use alloc::string::String;
use alloc::vec::Vec;

use rcore_fs::vfs::{FileType, Metadata, Timespec};

use super::mount3::{FileHandle, FHSIZE3};
use super::rpc::{CallTarget, RpcError, RpcResult};
use super::xdr::{XdrDecoder, XdrEncoder, XdrError};
use crate::error::LxError;

/// The NFS program number.
pub const PROGRAM_NFS: u32 = 100_003;
/// The NFS version this client speaks.
pub const NFS_VERSION: u32 = 3;

/// `NFSPROC3_GETATTR`.
pub const PROC_GETATTR: u32 = 1;
/// `NFSPROC3_LOOKUP`.
pub const PROC_LOOKUP: u32 = 3;
/// `NFSPROC3_ACCESS`.
pub const PROC_ACCESS: u32 = 4;
/// `NFSPROC3_READLINK`.
pub const PROC_READLINK: u32 = 5;
/// `NFSPROC3_READ`.
pub const PROC_READ: u32 = 6;
/// `NFSPROC3_FSSTAT`.
pub const PROC_FSSTAT: u32 = 18;
/// `NFSPROC3_FSINFO`.
pub const PROC_FSINFO: u32 = 19;

/// The longest filename this client will send (`NFS3_MAXNAMLEN`).
pub const MAXNAMLEN: usize = 255;

/// The target of procedure `procedure` of NFS version 3.
pub fn target(procedure: u32) -> CallTarget {
    CallTarget::new(PROGRAM_NFS, NFS_VERSION, procedure)
}

/// `nfsstat3` (RFC 1813 section 2.6).
///
/// Kept as the protocol's own type rather than as a number, because the values
/// above 10000 are not errno and nothing should be able to pass one off as
/// one. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nfs3Status {
    /// `NFS3_OK`.
    Ok,
    /// Not owner.
    Perm,
    /// No such file or directory.
    NoEnt,
    /// I/O error.
    Io,
    /// No such device or address.
    NxIo,
    /// Permission denied.
    Acces,
    /// File exists.
    Exist,
    /// Attempt to do a cross-device hard link.
    XDev,
    /// No such device.
    NoDev,
    /// Not a directory.
    NotDir,
    /// Is a directory.
    IsDir,
    /// Invalid argument.
    Inval,
    /// File too large.
    FBig,
    /// No space left on device.
    NoSpc,
    /// Read-only file system.
    RoFs,
    /// Too many hard links.
    MLink,
    /// The filename was too long.
    NameTooLong,
    /// Directory not empty.
    NotEmpty,
    /// Quota exceeded.
    DQuot,
    /// The filehandle names a file that no longer exists.
    ///
    /// This is NFS's distinctive error and has no close errno: the handle was
    /// valid, the file it named is gone, and the client is holding a reference
    /// to nothing. `ESTALE` is what Linux reports, and what callers see.
    Stale,
    /// Too many levels of remote in path.
    Remote,
    /// The filehandle is not one this server issued (10001).
    BadHandle,
    /// A `SETATTR` guard did not match (10002).
    NotSync,
    /// A directory cookie is no longer valid (10003).
    BadCookie,
    /// The operation is not supported (10004).
    NotSupp,
    /// The reply would not fit the client's buffer (10005).
    TooSmall,
    /// The server failed for a reason of its own (10006).
    ServerFault,
    /// The object type is not supported (10007).
    BadType,
    /// The file is temporarily unavailable; retry later (10008).
    ///
    /// A server with the file on tape or in cold storage says this, and it
    /// means "ask again", not "failed". Linux retries it rather than reporting
    /// it, which is why it must not be collapsed into a generic error.
    JukeBox,
    /// A status this client does not know.
    Unknown(u32),
}

impl Nfs3Status {
    /// The status a reply's `nfsstat3` names.
    pub fn from_wire(v: u32) -> Self {
        match v {
            0 => Self::Ok,
            1 => Self::Perm,
            2 => Self::NoEnt,
            5 => Self::Io,
            6 => Self::NxIo,
            13 => Self::Acces,
            17 => Self::Exist,
            18 => Self::XDev,
            19 => Self::NoDev,
            20 => Self::NotDir,
            21 => Self::IsDir,
            22 => Self::Inval,
            27 => Self::FBig,
            28 => Self::NoSpc,
            30 => Self::RoFs,
            31 => Self::MLink,
            63 => Self::NameTooLong,
            66 => Self::NotEmpty,
            69 => Self::DQuot,
            70 => Self::Stale,
            71 => Self::Remote,
            10001 => Self::BadHandle,
            10002 => Self::NotSync,
            10003 => Self::BadCookie,
            10004 => Self::NotSupp,
            10005 => Self::TooSmall,
            10006 => Self::ServerFault,
            10007 => Self::BadType,
            10008 => Self::JukeBox,
            other => Self::Unknown(other),
        }
    }

    /// Whether this status should be retried rather than reported.
    ///
    /// Only `NFS3ERR_JUKEBOX`, which means "the file is not to hand, ask
    /// again". Its own method because the decision belongs with the status and
    /// not with each caller, and because a caller that forgot it would turn a
    /// file on cold storage into an I/O error.
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::JukeBox)
    }

    /// The error userspace sees for this status.
    ///
    /// Explicit, one arm at a time, rather than casting the wire value. The
    /// values above 10000 are not errno, and a cast would send `10004` out as
    /// an error number that nothing names.
    pub fn to_lx_error(self) -> LxError {
        match self {
            // Not an error; a caller that reaches here asked the wrong
            // question, and `EUNDEF` is the honest answer to it.
            Self::Ok => LxError::EUNDEF,
            Self::Perm => LxError::EPERM,
            Self::NoEnt => LxError::ENOENT,
            Self::Io => LxError::EIO,
            Self::NxIo => LxError::ENXIO,
            Self::Acces => LxError::EACCES,
            Self::Exist => LxError::EEXIST,
            Self::XDev => LxError::EXDEV,
            Self::NoDev => LxError::ENODEV,
            Self::NotDir => LxError::ENOTDIR,
            Self::IsDir => LxError::EISDIR,
            Self::Inval => LxError::EINVAL,
            Self::FBig => LxError::EFBIG,
            Self::NoSpc => LxError::ENOSPC,
            Self::RoFs => LxError::EROFS,
            Self::MLink => LxError::EMLINK,
            Self::NameTooLong => LxError::ENAMETOOLONG,
            Self::NotEmpty => LxError::ENOTEMPTY,
            Self::DQuot => LxError::EDQUOT,
            // The handle names something that is gone. `ESTALE` exists for
            // exactly this and is what Linux reports.
            Self::Stale | Self::BadHandle => LxError::ESTALE,
            Self::Remote => LxError::EREMOTE,
            // A guard mismatch is a failed compare-and-set, not a bad
            // argument: `EINVAL` is what Linux's client reports.
            Self::NotSync => LxError::EINVAL,
            // A stale directory cookie. Linux restarts the readdir; a caller
            // that cannot sees `EINVAL`, as `lseek` on a bad offset does.
            Self::BadCookie => LxError::EINVAL,
            Self::NotSupp => LxError::EOPNOTSUPP,
            // The reply did not fit. Not the caller's fault and not a bad
            // argument: `EIO` is the only honest answer, and the client should
            // have asked for less.
            Self::TooSmall => LxError::EIO,
            Self::ServerFault => LxError::EIO,
            Self::BadType => LxError::EINVAL,
            // Should have been retried, not reported. If it reaches here the
            // retries ran out, and the file really was not available.
            Self::JukeBox => LxError::EAGAIN,
            Self::Unknown(_) => LxError::EIO,
        }
    }
}

/// `ftype3` (RFC 1813 section 2.5): what kind of object a filehandle names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ftype3 {
    /// A regular file.
    Reg,
    /// A directory.
    Dir,
    /// A block special device.
    Blk,
    /// A character special device.
    Chr,
    /// A symbolic link.
    Lnk,
    /// A socket.
    Sock,
    /// A named pipe.
    Fifo,
}

impl Ftype3 {
    /// The type a `fattr3`'s `type` field names, or `None` for a value no
    /// NFSv3 server should send.
    pub fn from_wire(v: u32) -> Option<Self> {
        Some(match v {
            1 => Self::Reg,
            2 => Self::Dir,
            3 => Self::Blk,
            4 => Self::Chr,
            5 => Self::Lnk,
            6 => Self::Sock,
            7 => Self::Fifo,
            _ => return None,
        })
    }

    /// The VFS type this is.
    pub fn to_vfs(self) -> FileType {
        match self {
            Self::Reg => FileType::File,
            Self::Dir => FileType::Dir,
            Self::Blk => FileType::BlockDevice,
            Self::Chr => FileType::CharDevice,
            Self::Lnk => FileType::SymLink,
            Self::Sock => FileType::Socket,
            Self::Fifo => FileType::NamedPipe,
        }
    }
}

/// `fattr3` (RFC 1813 section 2.5): everything the server knows about an
/// object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fattr3 {
    /// What kind of object this is.
    pub ftype: Ftype3,
    /// The permission bits, without the type bits: NFSv3 keeps the type in its
    /// own field, so this is the low twelve bits only.
    pub mode: u32,
    /// How many hard links the object has.
    pub nlink: u32,
    /// The owning user.
    pub uid: u32,
    /// The owning group.
    pub gid: u32,
    /// The object's size in bytes.
    pub size: u64,
    /// How much space it occupies on the server, which for a sparse file is
    /// less than `size`.
    pub used: u64,
    /// The device this is, for a block or character special file.
    pub rdev: (u32, u32),
    /// Which filesystem on the server holds it.
    pub fsid: u64,
    /// The object's number within that filesystem.
    pub fileid: u64,
    /// Last access.
    pub atime: Timespec,
    /// Last modification.
    pub mtime: Timespec,
    /// Last attribute change.
    pub ctime: Timespec,
}

impl Fattr3 {
    /// Decode a `fattr3`, which is twenty-one words and always this size.
    pub fn decode(dec: &mut XdrDecoder<'_>) -> RpcResult<Self> {
        let ftype =
            Ftype3::from_wire(dec.u32()?).ok_or(RpcError::Xdr(XdrError::BadDiscriminant(0)))?;
        let mode = dec.u32()?;
        let nlink = dec.u32()?;
        let uid = dec.u32()?;
        let gid = dec.u32()?;
        let size = dec.u64()?;
        let used = dec.u64()?;
        let rdev = (dec.u32()?, dec.u32()?);
        let fsid = dec.u64()?;
        let fileid = dec.u64()?;
        let atime = decode_time(dec)?;
        let mtime = decode_time(dec)?;
        let ctime = decode_time(dec)?;
        Ok(Self {
            ftype,
            mode,
            nlink,
            uid,
            gid,
            size,
            used,
            rdev,
            fsid,
            fileid,
            atime,
            mtime,
            ctime,
        })
    }

    /// The VFS metadata these attributes are.
    ///
    /// Two of the fields need thought rather than a cast:
    ///
    /// - **`blocks`.** The VFS counts 512-byte blocks, as `stat` does, and
    ///   NFSv3 reports `used` in *bytes*. Dividing by 512 rather than by the
    ///   block size is what makes `du` agree with the server about a sparse
    ///   file.
    /// - **`blk_size`.** NFSv3's `fattr3` does not carry one; it is a property
    ///   of the filesystem, not the file, and `FSINFO` is where it comes from.
    ///   The caller passes the negotiated transfer size, because that is what
    ///   a program reading this file should read in, and a wrong answer here
    ///   makes every buffered reader do short reads.
    pub fn to_metadata(&self, blk_size: usize) -> Metadata {
        Metadata {
            dev: self.fsid as usize,
            inode: self.fileid as usize,
            size: self.size as usize,
            blk_size,
            // 512-byte blocks, as `stat` counts them. `used` is in bytes.
            blocks: (self.used as usize).div_ceil(512),
            atime: self.atime,
            mtime: self.mtime,
            ctime: self.ctime,
            type_: self.ftype.to_vfs(),
            // The low twelve bits: NFSv3 keeps the type separately, so there
            // are no type bits here to mask off, but a server is free to set
            // bits above them and they are not permissions.
            mode: (self.mode & 0o7777) as u16,
            nlinks: self.nlink as usize,
            uid: self.uid as usize,
            gid: self.gid as usize,
            rdev: ((self.rdev.0 as usize) << 8) | (self.rdev.1 as usize),
        }
    }
}

/// An `nfstime3`: seconds and nanoseconds, both unsigned on the wire.
fn decode_time(dec: &mut XdrDecoder<'_>) -> RpcResult<Timespec> {
    let sec = dec.u32()?;
    let nsec = dec.u32()?;
    Ok(Timespec {
        sec: sec as i64,
        nsec: nsec as i32,
    })
}

/// A `post_op_attr`: the attributes a reply carries whether or not they were
/// asked for.
///
/// `None` is a server that declined to send them, which is allowed and is not
/// an error. See the module docs on why they matter.
pub type PostOpAttr = Option<Fattr3>;

/// What a reply that refused still carries: the status, and the attributes the
/// server attached to it anyway.
///
/// Every `*3resfail` arm in NFSv3 but `GETATTR3resfail` carries a
/// `post_op_attr`, and a client that threw it away would be dropping the one
/// thing that keeps its cache from going stale on exactly the calls where
/// something changed under it. So it is returned rather than skipped, and the
/// alias is here so that no decoder is tempted to return the status alone.
pub type Failure = (Nfs3Status, PostOpAttr);

/// Decode a `post_op_attr`.
pub fn decode_post_op_attr(dec: &mut XdrDecoder<'_>) -> RpcResult<PostOpAttr> {
    if dec.bool().map_err(RpcError::Xdr)? {
        Fattr3::decode(dec).map(Some)
    } else {
        Ok(None)
    }
}

/// Skip a `wcc_data`: the before-and-after attributes a modifying call
/// returns.
///
/// Read past rather than modelled, because nothing in the read path uses it
/// — but it has to be *consumed*, since a decoder that stopped short of it
/// would be however many fields out of step for whatever follows.
pub fn skip_wcc_data(dec: &mut XdrDecoder<'_>) -> RpcResult<()> {
    // pre_op_attr: size, mtime, ctime.
    if dec.bool().map_err(RpcError::Xdr)? {
        dec.u64().map_err(RpcError::Xdr)?;
        decode_time(dec)?;
        decode_time(dec)?;
    }
    decode_post_op_attr(dec)?;
    Ok(())
}

/// The arguments of a call that takes only a filehandle: `GETATTR`,
/// `READLINK`, `FSSTAT`, `FSINFO`.
pub fn encode_fh_only(fh: &FileHandle) -> Vec<u8> {
    let mut enc = XdrEncoder::with_capacity(fh.len() + 8);
    enc.opaque(fh.as_bytes());
    enc.into_bytes()
}

/// Decode a `GETATTR` reply.
///
/// `Ok(Err(status))` is a server that answered and refused; `Err(_)` is a
/// reply that did not decode. Every decoder here keeps those apart for the
/// same reason [`super::mount3::decode_mnt`] does.
pub fn decode_getattr(results: &[u8]) -> RpcResult<Result<Fattr3, Nfs3Status>> {
    let mut dec = XdrDecoder::new(results);
    let status = Nfs3Status::from_wire(dec.u32()?);
    if status != Nfs3Status::Ok {
        return Ok(Err(status));
    }
    Fattr3::decode(&mut dec).map(Ok)
}

/// The arguments of a call that takes a directory handle and a name:
/// `LOOKUP`, and in a later batch `REMOVE`, `RMDIR` and the create calls.
///
/// Refuses a name over [`MAXNAMLEN`], and refuses `/` inside one: a name is
/// one component, and a server handed `a/b` would either refuse it or — worse
/// — resolve it, which would let a path escape the directory the client
/// thinks it is looking in.
pub fn encode_dir_op(dir: &FileHandle, name: &str) -> Result<Vec<u8>, XdrError> {
    if name.len() > MAXNAMLEN || name.is_empty() {
        return Err(XdrError::LengthTooLarge);
    }
    if name.contains('/') {
        return Err(XdrError::BadPadding);
    }
    let mut enc = XdrEncoder::with_capacity(dir.len() + name.len() + 16);
    enc.opaque(dir.as_bytes());
    enc.string(name);
    Ok(enc.into_bytes())
}

/// What `LOOKUP` answered: the handle of the name, and the attributes of it
/// and of the directory it was found in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupReply {
    /// The handle of the object found.
    pub fh: FileHandle,
    /// Its attributes, if the server sent them.
    pub attr: PostOpAttr,
    /// The directory's attributes, if the server sent them.
    pub dir_attr: PostOpAttr,
}

/// Decode a `LOOKUP` reply.
///
/// Both arms carry the directory's attributes — the failure arm too, which is
/// the detail worth getting right: a `LOOKUP` that returns `NFS3ERR_NOENT`
/// still refreshes what the client knows about the directory, and that is how
/// a negative lookup updates the cache instead of leaving it stale. Decoded
/// even on failure for that reason, and because not consuming it would leave
/// the decoder out of step.
pub fn decode_lookup(results: &[u8]) -> RpcResult<Result<LookupReply, Failure>> {
    let mut dec = XdrDecoder::new(results);
    let status = Nfs3Status::from_wire(dec.u32()?);
    if status != Nfs3Status::Ok {
        let dir_attr = decode_post_op_attr(&mut dec)?;
        return Ok(Err((status, dir_attr)));
    }
    let fh = FileHandle::new(dec.opaque_max(FHSIZE3)?).map_err(RpcError::Xdr)?;
    let attr = decode_post_op_attr(&mut dec)?;
    let dir_attr = decode_post_op_attr(&mut dec)?;
    Ok(Ok(LookupReply { fh, attr, dir_attr }))
}

/// `ACCESS3_*`: the permissions `ACCESS` asks about and answers with.
pub mod access {
    /// Read data from a file, or read a directory.
    pub const READ: u32 = 0x0001;
    /// Look up a name in a directory.
    pub const LOOKUP: u32 = 0x0002;
    /// Rewrite existing file data.
    pub const MODIFY: u32 = 0x0004;
    /// Extend a file, or add an entry to a directory.
    pub const EXTEND: u32 = 0x0008;
    /// Delete an entry from a directory.
    pub const DELETE: u32 = 0x0010;
    /// Execute a file.
    pub const EXECUTE: u32 = 0x0020;
}

/// Encode `ACCESS`'s arguments: the handle and the permissions asked about.
pub fn encode_access(fh: &FileHandle, mask: u32) -> Vec<u8> {
    let mut enc = XdrEncoder::with_capacity(fh.len() + 12);
    enc.opaque(fh.as_bytes());
    enc.u32(mask);
    enc.into_bytes()
}

/// Decode an `ACCESS` reply: the permissions the server *granted*, which may
/// be fewer than were asked for.
///
/// Fewer, and that is not an error: `ACCESS` answers "of these, which may
/// you", so a zero mask is a complete answer meaning "none of them". A client
/// that treated a short mask as a failure would refuse operations the server
/// permits.
pub fn decode_access(results: &[u8]) -> RpcResult<Result<(u32, PostOpAttr), Failure>> {
    let mut dec = XdrDecoder::new(results);
    let status = Nfs3Status::from_wire(dec.u32()?);
    // Before the branch, because both arms carry it. See [`Failure`].
    let attr = decode_post_op_attr(&mut dec)?;
    if status != Nfs3Status::Ok {
        return Ok(Err((status, attr)));
    }
    Ok(Ok((dec.u32()?, attr)))
}

/// Decode a `READLINK` reply: where the symbolic link points.
///
/// The target is bytes, not a `str`: a path on the server is bytes, and this
/// client does not get to decide that a link whose target is not UTF-8 does
/// not exist. It is converted lossily only where the VFS demands a `String`.
pub fn decode_readlink(results: &[u8]) -> RpcResult<Result<(Vec<u8>, PostOpAttr), Failure>> {
    let mut dec = XdrDecoder::new(results);
    let status = Nfs3Status::from_wire(dec.u32()?);
    let attr = decode_post_op_attr(&mut dec)?;
    if status != Nfs3Status::Ok {
        return Ok(Err((status, attr)));
    }
    Ok(Ok((dec.string_bytes()?.to_vec(), attr)))
}

/// Encode `READ`'s arguments: the handle, the offset and how many bytes.
pub fn encode_read(fh: &FileHandle, offset: u64, count: u32) -> Vec<u8> {
    let mut enc = XdrEncoder::with_capacity(fh.len() + 20);
    enc.opaque(fh.as_bytes());
    enc.u64(offset);
    enc.u32(count);
    enc.into_bytes()
}

/// What `READ` answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadReply {
    /// The bytes read. May be shorter than asked for without that being an
    /// error or the end of the file: see [`Self::eof`].
    pub data: Vec<u8>,
    /// Whether the read reached the end of the file.
    ///
    /// The *only* reliable end-of-file signal, and the reason this field is
    /// not inferred from a short read. A server may return fewer bytes than
    /// asked for at any time — because the read crossed its preferred
    /// transfer size, because it felt like it — and a client that read a short
    /// reply as end-of-file truncates files silently. Linux loops until `eof`
    /// or a zero-length read, and so must anything built on this.
    pub eof: bool,
    /// The file's attributes, if the server sent them.
    pub attr: PostOpAttr,
}

/// Decode a `READ` reply.
///
/// The `count` field is read and then *checked against* the data rather than
/// trusted: it is the server's statement of how much it sent, the data that
/// follows carries its own length, and a client that believed `count` over
/// the data would read past the buffer or hand back bytes that were never
/// sent. Linux compares them too.
pub fn decode_read(results: &[u8]) -> RpcResult<Result<ReadReply, Failure>> {
    let mut dec = XdrDecoder::new(results);
    let status = Nfs3Status::from_wire(dec.u32()?);
    let attr = decode_post_op_attr(&mut dec)?;
    if status != Nfs3Status::Ok {
        return Ok(Err((status, attr)));
    }
    let count = dec.u32()? as usize;
    let eof = dec.bool().map_err(RpcError::Xdr)?;
    let data = dec.opaque()?;
    if data.len() != count {
        // The server contradicted itself. Refusing is the only safe answer:
        // taking either number means handing a caller a length the other half
        // of the reply does not support.
        return Err(RpcError::Xdr(XdrError::LengthTooLarge));
    }
    Ok(Ok(ReadReply {
        data: data.to_vec(),
        eof,
        attr,
    }))
}

/// What `FSSTAT` answered: the numbers `statfs` wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsStat {
    /// Total size of the filesystem, in bytes.
    pub tbytes: u64,
    /// Free bytes.
    pub fbytes: u64,
    /// Free bytes available to this user, which for a filesystem with a
    /// reserve is less than `fbytes`.
    pub abytes: u64,
    /// Total file slots.
    pub tfiles: u64,
    /// Free file slots.
    pub ffiles: u64,
    /// Free file slots available to this user.
    pub afiles: u64,
    /// How many seconds the server thinks these numbers are good for.
    pub invarsec: u32,
}

/// Decode an `FSSTAT` reply.
pub fn decode_fsstat(results: &[u8]) -> RpcResult<Result<(FsStat, PostOpAttr), Failure>> {
    let mut dec = XdrDecoder::new(results);
    let status = Nfs3Status::from_wire(dec.u32()?);
    let attr = decode_post_op_attr(&mut dec)?;
    if status != Nfs3Status::Ok {
        return Ok(Err((status, attr)));
    }
    Ok(Ok((
        FsStat {
            tbytes: dec.u64()?,
            fbytes: dec.u64()?,
            abytes: dec.u64()?,
            tfiles: dec.u64()?,
            ffiles: dec.u64()?,
            afiles: dec.u64()?,
            invarsec: dec.u32()?,
        },
        attr,
    )))
}

/// What `FSINFO` answered: the sizes and limits to negotiate against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsInfo {
    /// The largest READ the server will serve.
    pub rtmax: u32,
    /// The READ size the server prefers.
    pub rtpref: u32,
    /// The multiple a READ's size should be.
    pub rtmult: u32,
    /// The largest WRITE the server will take.
    pub wtmax: u32,
    /// The WRITE size the server prefers.
    pub wtpref: u32,
    /// The multiple a WRITE's size should be.
    pub wtmult: u32,
    /// The READDIR size the server prefers.
    pub dtpref: u32,
    /// The largest file the server supports.
    pub maxfilesize: u64,
    /// The granularity of the times the server keeps.
    pub time_delta: Timespec,
    /// `FSF3_*` properties.
    pub properties: u32,
}

/// `FSF3_*`: what the server says its filesystem can do.
pub mod fsf3 {
    /// Hard links are supported.
    pub const LINK: u32 = 0x0001;
    /// Symbolic links are supported.
    pub const SYMLINK: u32 = 0x0002;
    /// A filehandle stays valid across a server restart.
    pub const HOMOGENEOUS: u32 = 0x0008;
    /// The server can set a file's size with `SETATTR`.
    pub const CANSETTIME: u32 = 0x0010;
}

impl FsInfo {
    /// The transfer size to use for reads, given what was asked for.
    ///
    /// The negotiation, in one place: never above the server's maximum, never
    /// zero, and rounded *down* to the server's multiple rather than up —
    /// rounding up can exceed `rtmax`, which is the bound the server actually
    /// enforces, and a client that rounded up would have its largest reads
    /// refused.
    pub fn read_size(&self, wanted: usize) -> usize {
        negotiate(wanted, self.rtmax, self.rtpref, self.rtmult)
    }

    /// The transfer size to use for writes.
    pub fn write_size(&self, wanted: usize) -> usize {
        negotiate(wanted, self.wtmax, self.wtpref, self.wtmult)
    }
}

/// The shared body of [`FsInfo::read_size`] and [`FsInfo::write_size`].
fn negotiate(wanted: usize, max: u32, pref: u32, mult: u32) -> usize {
    // A server that reports zero for a limit has told us nothing, so the
    // client's own number stands. Treating zero as the limit would negotiate
    // every transfer down to nothing.
    let ceiling = match (max, pref) {
        (0, 0) => wanted,
        (0, p) => p as usize,
        (m, 0) => m as usize,
        (m, p) => (m as usize).min(p as usize),
    };
    let size = wanted.min(ceiling).max(1);
    if mult <= 1 {
        return size;
    }
    let mult = mult as usize;
    // Down, not up: `max` is what the server enforces.
    let rounded = (size / mult) * mult;
    if rounded == 0 {
        size.min(mult)
    } else {
        rounded
    }
}

/// Decode an `FSINFO` reply.
pub fn decode_fsinfo(results: &[u8]) -> RpcResult<Result<(FsInfo, PostOpAttr), Failure>> {
    let mut dec = XdrDecoder::new(results);
    let status = Nfs3Status::from_wire(dec.u32()?);
    let attr = decode_post_op_attr(&mut dec)?;
    if status != Nfs3Status::Ok {
        return Ok(Err((status, attr)));
    }
    Ok(Ok((
        FsInfo {
            rtmax: dec.u32()?,
            rtpref: dec.u32()?,
            rtmult: dec.u32()?,
            wtmax: dec.u32()?,
            wtpref: dec.u32()?,
            wtmult: dec.u32()?,
            dtpref: dec.u32()?,
            maxfilesize: dec.u64()?,
            time_delta: decode_time(&mut dec)?,
            properties: dec.u32()?,
        },
        attr,
    )))
}

/// A symbolic link's target as the VFS wants it: a `String`.
///
/// Lossy on purpose, and only here. A path on the server is bytes, so a link
/// whose target is not UTF-8 is a link that exists and that this kernel's VFS
/// has no way to spell; refusing to read it would be worse than reading it
/// with replacement characters, and the conversion is confined to this one
/// function so nothing else loses the bytes.
pub fn link_target_to_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
#[path = "nfs3_tests.rs"]
mod nfs3_tests;
