//! The NFSv3 procedures that change something on the server: `SETATTR`,
//! `WRITE`, `COMMIT`, the four that create (`CREATE`, `MKDIR`, `SYMLINK`,
//! `MKNOD`) and the four that unmake or move (`REMOVE`, `RMDIR`, `RENAME`,
//! `LINK`).
//!
//! Still pure functions from bytes to bytes, so still tested whole on the
//! host. What changes from the read path is the *consequence* of getting one
//! wrong: a misdecoded `READ` reply reads the wrong bytes and a misencoded
//! `WRITE` writes them, so three things in here are not conveniences.
//!
//! # Why a retried write must not be re-executed
//!
//! An NFSv3 `WRITE` names its own offset, so retransmitting it is safe — it
//! writes the same bytes to the same place. [`CREATE`](encode_create) and the
//! rest are not: a `MKDIR` whose reply was lost has *already happened*, and
//! asking again gets `NFS3ERR_EXIST` for a directory the caller just made.
//! The protection is in the transport, not here — the retry carries the same
//! `xid`, which the server's reply cache matches (see
//! [`client`](super::client)) — and the one thing this module must do is not
//! undermine it. That is why [`CreateHow::Exclusive`] exists and why it
//! carries a verifier.
//!
//! # `sattr3` is all-optional, and that is the point
//!
//! Every field of a `sattr3` is a `set_*` option, so a `SETATTR` says what to
//! change and stays silent about the rest. A client that filled every field —
//! reading the current attributes and writing them back — would turn a
//! `chmod` into a lost update of whatever another client changed in between.
//! So [`SetAttr`] is `Option` all the way down and [`SetAttr::default`] sets
//! nothing.
//!
//! # `COMMIT` is not optional on an unstable write
//!
//! A `WRITE` with [`StableHow::Unstable`] may be in the server's memory and
//! nowhere else when it answers. The data is durable only after a `COMMIT`
//! whose `verf` equals the `verf` the writes came back with: a *different*
//! verifier means the server restarted, the unstable writes are gone, and
//! they have to be sent again. A client that ignored the verifier would
//! report `fsync` success over data that no longer exists, which is the one
//! failure a filesystem must never have.

use alloc::vec::Vec;

use rcore_fs::vfs::Timespec;

use super::mount3::{FileHandle, FHSIZE3};
use super::nfs3::{
    decode_post_op_attr, decode_wcc_data, is_one_component, Nfs3Status, PostOpAttr, WccData,
};
use super::rpc::{RpcError, RpcResult};
use super::xdr::{XdrDecoder, XdrEncoder, XdrError};

/// What a mutating call answered, when the reply carries the same payload
/// whether the call worked or not.
///
/// Which is every call in this module: the `wcc_data` is there *precisely*
/// for the case where the call did not do what the client expected, so the
/// failure arm carries it too and the decoders hand it back on both. See
/// [`WccData`].
pub type WccResult<T> = RpcResult<Result<T, (Nfs3Status, T)>>;

/// `NFSPROC3_SETATTR`.
pub const PROC_SETATTR: u32 = 2;
/// `NFSPROC3_WRITE`.
pub const PROC_WRITE: u32 = 7;
/// `NFSPROC3_CREATE`.
pub const PROC_CREATE: u32 = 8;
/// `NFSPROC3_MKDIR`.
pub const PROC_MKDIR: u32 = 9;
/// `NFSPROC3_SYMLINK`.
pub const PROC_SYMLINK: u32 = 10;
/// `NFSPROC3_MKNOD`.
pub const PROC_MKNOD: u32 = 11;
/// `NFSPROC3_REMOVE`.
pub const PROC_REMOVE: u32 = 12;
/// `NFSPROC3_RMDIR`.
pub const PROC_RMDIR: u32 = 13;
/// `NFSPROC3_RENAME`.
pub const PROC_RENAME: u32 = 14;
/// `NFSPROC3_LINK`.
pub const PROC_LINK: u32 = 15;
/// `NFSPROC3_COMMIT`.
pub const PROC_COMMIT: u32 = 21;

/// How long a write verifier is (`NFS3_WRITEVERFSIZE`), and a create
/// verifier with it (`NFS3_CREATEVERFSIZE`). Both are eight bytes.
pub const VERF_SIZE: usize = 8;

/// A server's write verifier: eight opaque bytes that change when it
/// restarts.
///
/// Opaque on purpose — nothing may interpret it, only compare it — so it is a
/// fixed array rather than a number, and the comparison is `==`.
pub type WriteVerf = [u8; VERF_SIZE];

/// How durable the server must make a `WRITE` before it answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StableHow {
    /// The server may answer with the data still in its memory.
    ///
    /// The fast path, and the one that needs a [`COMMIT`](encode_commit)
    /// afterwards. Linux writes this way for ordinary buffered writes and
    /// commits on `fsync` and on writeback.
    Unstable,
    /// The data, but not necessarily the metadata, is on stable storage
    /// before the server answers.
    DataSync,
    /// Data and metadata are both on stable storage before the server
    /// answers. What `O_SYNC` means.
    FileSync,
}

impl StableHow {
    /// The `stable_how` value this is.
    pub fn to_wire(self) -> u32 {
        match self {
            Self::Unstable => 0,
            Self::DataSync => 1,
            Self::FileSync => 2,
        }
    }

    /// The `stable_how` a reply's `committed` field names.
    ///
    /// A server is allowed to answer with *more* durability than was asked
    /// for, never less, so the reply's value is what the client must believe:
    /// a `FileSync` answer to an `Unstable` write needs no `COMMIT`, and
    /// skipping that check would commit for nothing on every write.
    pub fn from_wire(v: u32) -> Option<Self> {
        Some(match v {
            0 => Self::Unstable,
            1 => Self::DataSync,
            2 => Self::FileSync,
            _ => return None,
        })
    }

    /// Whether data written with this much durability still needs a
    /// [`COMMIT`](encode_commit).
    ///
    /// Only `Unstable` does. Its own method because the decision belongs with
    /// the value and not with each caller.
    pub fn needs_commit(self) -> bool {
        matches!(self, Self::Unstable)
    }
}

/// A `sattr3`: the attributes a call changes, and only those.
///
/// Every field is optional and `None` means "leave it alone". See the module
/// docs on why a client must not fill in the fields it is not changing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SetAttr {
    /// The permission bits to set, without type bits.
    pub mode: Option<u32>,
    /// The owner to set.
    pub uid: Option<u32>,
    /// The group to set.
    pub gid: Option<u32>,
    /// The size to set, which is how a file is truncated or extended.
    pub size: Option<u64>,
    /// What to do with the access time.
    pub atime: Option<SetTime>,
    /// What to do with the modification time.
    pub mtime: Option<SetTime>,
}

/// A `time_how`: whose clock a time comes from.
///
/// `SET_TO_SERVER_TIME` and `SET_TO_CLIENT_TIME` are two arms of one union
/// and not two ways of saying the same thing: a client's clock may be well
/// off the server's, and asking the *server* for "now" is what keeps a
/// `touch` from stamping a file in the future for everyone else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetTime {
    /// `SET_TO_SERVER_TIME`: whatever the server's clock says when it runs
    /// the call.
    Server,
    /// `SET_TO_CLIENT_TIME`: this exact time.
    Client(Timespec),
}

impl SetAttr {
    /// Nothing to change.
    ///
    /// The same as [`Default`], and named because `SetAttr::nothing()` at a
    /// call site says what an empty struct literal does not.
    pub fn nothing() -> Self {
        Self::default()
    }

    /// Whether this would change anything at all.
    ///
    /// A `SETATTR` with nothing set is a legal call that does nothing, and
    /// worth not sending: it is a round trip to the server for no effect.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Append this `sattr3`.
    fn encode(&self, enc: &mut XdrEncoder) {
        enc.optional(self.mode, |e, v| e.u32(v));
        enc.optional(self.uid, |e, v| e.u32(v));
        enc.optional(self.gid, |e, v| e.u32(v));
        enc.optional(self.size, |e, v| e.u64(v));
        enc.optional(self.atime, |e, v| encode_set_time(e, v));
        enc.optional(self.mtime, |e, v| encode_set_time(e, v));
    }
}

/// Append a `set_atime`/`set_mtime` body: the `time_how`, then the time only
/// for `SET_TO_CLIENT_TIME`.
///
/// Careful with the discriminants: `DONT_CHANGE` is 0, `SET_TO_SERVER_TIME`
/// is 1 and `SET_TO_CLIENT_TIME` is 2. The outer `Option` has already said
/// "change it", so only 1 and 2 are ever written here — writing 0 would be a
/// call that silently changed nothing.
fn encode_set_time(enc: &mut XdrEncoder, when: SetTime) -> &mut XdrEncoder {
    match when {
        SetTime::Server => enc.u32(1),
        SetTime::Client(t) => {
            enc.u32(2);
            enc.u32(t.sec as u32);
            enc.u32(t.nsec as u32)
        }
    }
}

/// Encode `SETATTR`'s arguments.
///
/// `guard` is a compare-and-set: when it is `Some(ctime)` the server runs the
/// call only if the object's `ctime` is still that, and answers
/// `NFS3ERR_NOT_SYNC` otherwise. It is how a read-modify-write of the
/// attributes is made safe against another client, and it is the reason
/// `NFS3ERR_NOT_SYNC` maps to `EINVAL` rather than to an I/O error.
pub fn encode_setattr(fh: &FileHandle, attr: &SetAttr, guard: Option<Timespec>) -> Vec<u8> {
    let mut enc = XdrEncoder::with_capacity(fh.len() + 64);
    enc.opaque(fh.as_bytes());
    attr.encode(&mut enc);
    enc.optional(guard, |e, t| {
        e.u32(t.sec as u32);
        e.u32(t.nsec as u32)
    });
    enc.into_bytes()
}

/// Decode a `SETATTR` reply: the object's `wcc_data`, on either arm.
pub fn decode_setattr(results: &[u8]) -> WccResult<WccData> {
    let mut dec = XdrDecoder::new(results);
    let status = Nfs3Status::from_wire(dec.u32()?);
    let wcc = decode_wcc_data(&mut dec)?;
    if status != Nfs3Status::Ok {
        return Ok(Err((status, wcc)));
    }
    Ok(Ok(wcc))
}

/// Encode `WRITE`'s arguments.
///
/// The `count` field is written from `data.len()` rather than taken as an
/// argument: they are the same number, and a function that let a caller pass
/// them separately would let a caller pass them differently. A server that
/// trusted the count over the data would then write bytes nobody sent.
pub fn encode_write(fh: &FileHandle, offset: u64, stable: StableHow, data: &[u8]) -> Vec<u8> {
    let mut enc = XdrEncoder::with_capacity(fh.len() + data.len() + 32);
    enc.opaque(fh.as_bytes());
    enc.u64(offset);
    enc.u32(data.len() as u32);
    enc.u32(stable.to_wire());
    enc.opaque(data);
    enc.into_bytes()
}

/// What `WRITE` answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteReply {
    /// The file's `wcc_data`, which is how a writer keeps the rest of its
    /// cached pages instead of dropping them after every write.
    pub wcc: WccData,
    /// How many bytes the server wrote, which may be fewer than were sent.
    ///
    /// A short write is not an error and not the end of anything: the caller
    /// sends the rest at the new offset. Treating it as a failure would turn
    /// a server's flow control into an I/O error.
    pub count: u32,
    /// How durable the data actually is, which may be *more* than was asked
    /// for. See [`StableHow::from_wire`].
    pub committed: StableHow,
    /// The server's write verifier, to be compared with a later
    /// [`COMMIT`](decode_commit)'s.
    pub verf: WriteVerf,
}

/// Decode a `WRITE` reply.
pub fn decode_write(results: &[u8]) -> RpcResult<Result<WriteReply, (Nfs3Status, WccData)>> {
    let mut dec = XdrDecoder::new(results);
    let status = Nfs3Status::from_wire(dec.u32()?);
    let wcc = decode_wcc_data(&mut dec)?;
    if status != Nfs3Status::Ok {
        return Ok(Err((status, wcc)));
    }
    let count = dec.u32()?;
    let committed =
        StableHow::from_wire(dec.u32()?).ok_or(RpcError::Xdr(XdrError::BadDiscriminant(0)))?;
    Ok(Ok(WriteReply {
        wcc,
        count,
        committed,
        verf: read_verf(&mut dec)?,
    }))
}

/// Encode `COMMIT`'s arguments: the handle, and the byte range to make
/// durable.
///
/// `count = 0` means "to the end of the file", which is what `fsync` wants
/// and what Linux sends. It is not "commit nothing": a client that read it
/// that way and skipped the call would report `fsync` success over data still
/// in the server's memory.
pub fn encode_commit(fh: &FileHandle, offset: u64, count: u32) -> Vec<u8> {
    let mut enc = XdrEncoder::with_capacity(fh.len() + 20);
    enc.opaque(fh.as_bytes());
    enc.u64(offset);
    enc.u32(count);
    enc.into_bytes()
}

/// What `COMMIT` answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitReply {
    /// The verifier to compare against the one the writes came back with.
    ///
    /// A *different* verifier means the server restarted and the unstable
    /// writes are gone; they have to be sent again. See the module docs.
    pub verf: WriteVerf,
    /// The file's `wcc_data`.
    pub wcc: WccData,
}

/// Decode a `COMMIT` reply.
pub fn decode_commit(results: &[u8]) -> RpcResult<Result<CommitReply, (Nfs3Status, WccData)>> {
    let mut dec = XdrDecoder::new(results);
    let status = Nfs3Status::from_wire(dec.u32()?);
    let wcc = decode_wcc_data(&mut dec)?;
    if status != Nfs3Status::Ok {
        return Ok(Err((status, wcc)));
    }
    Ok(Ok(CommitReply {
        verf: read_verf(&mut dec)?,
        wcc,
    }))
}

/// Read a verifier: eight opaque bytes, fixed-length, no padding.
fn read_verf(dec: &mut XdrDecoder<'_>) -> RpcResult<WriteVerf> {
    let bytes = dec.opaque_fixed(VERF_SIZE)?;
    let mut verf = [0u8; VERF_SIZE];
    verf.copy_from_slice(bytes);
    Ok(verf)
}

/// How `CREATE` should behave when the name already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateHow {
    /// `UNCHECKED`: create it, or truncate and reuse what is there.
    /// `open(O_CREAT)` without `O_EXCL`.
    Unchecked(SetAttr),
    /// `GUARDED`: fail with `NFS3ERR_EXIST` if the name exists.
    /// `open(O_CREAT | O_EXCL)` — but see [`Self::Exclusive`].
    Guarded(SetAttr),
    /// `EXCLUSIVE`: create it, and store this verifier on the new file so a
    /// *retry* of the same call can be told apart from a genuine collision.
    ///
    /// This is what `O_EXCL` over an unreliable network actually needs.
    /// `GUARDED`'s problem is the lost reply: the file was created, the answer
    /// never arrived, the client asks again and gets `NFS3ERR_EXIST` for the
    /// file it just made itself — so `O_EXCL` fails on a file that nobody else
    /// touched. With `EXCLUSIVE`, the server compares the verifier it stored
    /// against this one, sees its own earlier work, and answers success. The
    /// verifier must therefore be the *same* on every retry of one `open` and
    /// different between opens, which is why it is a parameter rather than
    /// generated here.
    Exclusive(WriteVerf),
}

impl CreateHow {
    /// The `createmode3` discriminant.
    fn mode(&self) -> u32 {
        match self {
            Self::Unchecked(_) => 0,
            Self::Guarded(_) => 1,
            Self::Exclusive(_) => 2,
        }
    }
}

/// Encode `CREATE`'s arguments.
///
/// Refuses the same names [`super::nfs3::encode_dir_op`] does, for the same
/// reason: a name is one component, and a server handed `a/b` would either
/// refuse it or resolve it, which would let a create escape the directory.
pub fn encode_create(dir: &FileHandle, name: &str, how: CreateHow) -> Result<Vec<u8>, XdrError> {
    let mut enc = XdrEncoder::with_capacity(dir.len() + name.len() + 80);
    encode_dir_and_name(&mut enc, dir, name)?;
    enc.u32(how.mode());
    match how {
        CreateHow::Unchecked(attr) | CreateHow::Guarded(attr) => attr.encode(&mut enc),
        CreateHow::Exclusive(verf) => {
            enc.opaque_fixed(&verf);
        }
    }
    Ok(enc.into_bytes())
}

/// Encode `MKDIR`'s arguments.
pub fn encode_mkdir(dir: &FileHandle, name: &str, attr: &SetAttr) -> Result<Vec<u8>, XdrError> {
    let mut enc = XdrEncoder::with_capacity(dir.len() + name.len() + 64);
    encode_dir_and_name(&mut enc, dir, name)?;
    attr.encode(&mut enc);
    Ok(enc.into_bytes())
}

/// Encode `SYMLINK`'s arguments.
///
/// The target is bytes, not a `str`: a path on the server is bytes, and a
/// client that insisted on UTF-8 could not create a link that the server, and
/// every other client, handles perfectly well. It is *not* checked for `/` —
/// unlike `name`, a link target is a whole path and slashes are the point.
pub fn encode_symlink(
    dir: &FileHandle,
    name: &str,
    attr: &SetAttr,
    target: &[u8],
) -> Result<Vec<u8>, XdrError> {
    let mut enc = XdrEncoder::with_capacity(dir.len() + name.len() + target.len() + 64);
    encode_dir_and_name(&mut enc, dir, name)?;
    attr.encode(&mut enc);
    enc.opaque(target);
    Ok(enc.into_bytes())
}

/// What kind of special file `MKNOD` makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// A block special device, with its major and minor.
    Block(u32, u32),
    /// A character special device, with its major and minor.
    Char(u32, u32),
    /// A socket.
    Socket,
    /// A named pipe.
    Fifo,
}

impl NodeKind {
    /// The `ftype3` discriminant this is.
    fn ftype(&self) -> u32 {
        match self {
            Self::Block(..) => 3,
            Self::Char(..) => 4,
            Self::Socket => 6,
            Self::Fifo => 7,
        }
    }
}

/// Encode `MKNOD`'s arguments.
///
/// The union after the type is not uniform, and that is the trap: a device
/// carries attributes *and* a `specdata3`, while a socket or a FIFO carries
/// attributes alone. Writing a zero `specdata3` for the latter two would
/// leave the server two words out of step.
pub fn encode_mknod(
    dir: &FileHandle,
    name: &str,
    kind: NodeKind,
    attr: &SetAttr,
) -> Result<Vec<u8>, XdrError> {
    let mut enc = XdrEncoder::with_capacity(dir.len() + name.len() + 64);
    encode_dir_and_name(&mut enc, dir, name)?;
    enc.u32(kind.ftype());
    attr.encode(&mut enc);
    match kind {
        NodeKind::Block(major, minor) | NodeKind::Char(major, minor) => {
            enc.u32(major);
            enc.u32(minor);
        }
        NodeKind::Socket | NodeKind::Fifo => {}
    }
    Ok(enc.into_bytes())
}

/// What a create-shaped call answered: the new object's handle and
/// attributes, both of which the server may decline to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateReply {
    /// The new object's handle, if the server sent one.
    ///
    /// `None` is allowed, and means a `LOOKUP` is needed to get one. Rare but
    /// real, and a client that unwrapped this would panic on a legal reply.
    pub fh: Option<FileHandle>,
    /// Its attributes, if the server sent them.
    pub attr: PostOpAttr,
    /// The directory's `wcc_data`: how the client learns whether its cached
    /// listing of that directory is still good.
    pub dir_wcc: WccData,
}

/// Decode a reply from `CREATE`, `MKDIR`, `SYMLINK` or `MKNOD`.
///
/// All four share `diropok3`: a `post_op_fh3`, a `post_op_attr` and a
/// `wcc_data` for the directory. One decoder, because they are one shape and
/// four copies of it would be four chances to get the optional handle wrong.
pub fn decode_create(results: &[u8]) -> RpcResult<Result<CreateReply, (Nfs3Status, WccData)>> {
    let mut dec = XdrDecoder::new(results);
    let status = Nfs3Status::from_wire(dec.u32()?);
    if status != Nfs3Status::Ok {
        // The failure arm is the directory's wcc_data and nothing else: no
        // handle and no attributes, so a decoder that went looking for them
        // would read the wcc_data as a handle.
        return Ok(Err((status, decode_wcc_data(&mut dec)?)));
    }
    let fh = if dec.bool().map_err(RpcError::Xdr)? {
        Some(FileHandle::new(dec.opaque_max(FHSIZE3)?).map_err(RpcError::Xdr)?)
    } else {
        None
    };
    let attr = decode_post_op_attr(&mut dec)?;
    Ok(Ok(CreateReply {
        fh,
        attr,
        dir_wcc: decode_wcc_data(&mut dec)?,
    }))
}

/// Encode the arguments of `REMOVE` or `RMDIR`: a `diropargs3`.
pub fn encode_remove(dir: &FileHandle, name: &str) -> Result<Vec<u8>, XdrError> {
    let mut enc = XdrEncoder::with_capacity(dir.len() + name.len() + 16);
    encode_dir_and_name(&mut enc, dir, name)?;
    Ok(enc.into_bytes())
}

/// Decode a reply from `REMOVE` or `RMDIR`: the directory's `wcc_data`, on
/// either arm.
///
/// The same shape as [`decode_setattr`] — one status, one `wcc_data` — so it
/// is the same decoder. Two copies would be two chances to get the optional
/// halves wrong.
pub fn decode_remove(results: &[u8]) -> WccResult<WccData> {
    decode_setattr(results)
}

/// Encode `RENAME`'s arguments: two `diropargs3`, from and to.
pub fn encode_rename(
    from_dir: &FileHandle,
    from_name: &str,
    to_dir: &FileHandle,
    to_name: &str,
) -> Result<Vec<u8>, XdrError> {
    let mut enc = XdrEncoder::with_capacity(
        from_dir.len() + from_name.len() + to_dir.len() + to_name.len() + 32,
    );
    encode_dir_and_name(&mut enc, from_dir, from_name)?;
    encode_dir_and_name(&mut enc, to_dir, to_name)?;
    Ok(enc.into_bytes())
}

/// What `RENAME` answered: one `wcc_data` per directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenameWcc {
    /// The directory the name came out of.
    pub from_dir: WccData,
    /// The directory it went into. The same directory as `from_dir` for a
    /// rename inside one directory, and then the server sends that
    /// directory's `wcc_data` twice.
    pub to_dir: WccData,
}

/// Decode a `RENAME` reply.
///
/// Both arms carry *two* `wcc_data`, one per directory, and both have to be
/// consumed. Reading one would leave the decoder half a reply short — which
/// on a stream transport is not a decode error on this message but a
/// misparse of the next one.
pub fn decode_rename(results: &[u8]) -> WccResult<RenameWcc> {
    let mut dec = XdrDecoder::new(results);
    let status = Nfs3Status::from_wire(dec.u32()?);
    let wcc = RenameWcc {
        from_dir: decode_wcc_data(&mut dec)?,
        to_dir: decode_wcc_data(&mut dec)?,
    };
    if status != Nfs3Status::Ok {
        return Ok(Err((status, wcc)));
    }
    Ok(Ok(wcc))
}

/// Encode `LINK`'s arguments: the existing object, then where to link it.
pub fn encode_link(fh: &FileHandle, dir: &FileHandle, name: &str) -> Result<Vec<u8>, XdrError> {
    let mut enc = XdrEncoder::with_capacity(fh.len() + dir.len() + name.len() + 32);
    enc.opaque(fh.as_bytes());
    encode_dir_and_name(&mut enc, dir, name)?;
    Ok(enc.into_bytes())
}

/// What `LINK` answered: the linked file's attributes and the directory's
/// `wcc_data`, in that order and on either arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkReply {
    /// The file's attributes, if the server sent them. Its link count is the
    /// field that just changed.
    pub attr: PostOpAttr,
    /// The directory's `wcc_data`.
    pub dir_wcc: WccData,
}

/// Decode a `LINK` reply.
pub fn decode_link(results: &[u8]) -> WccResult<LinkReply> {
    let mut dec = XdrDecoder::new(results);
    let status = Nfs3Status::from_wire(dec.u32()?);
    // Both arms carry both fields, so both are read before the branch.
    let reply = LinkReply {
        attr: decode_post_op_attr(&mut dec)?,
        dir_wcc: decode_wcc_data(&mut dec)?,
    };
    if status != Nfs3Status::Ok {
        return Ok(Err((status, reply)));
    }
    Ok(Ok(reply))
}

/// Append a `diropargs3`, refusing a name that is not one component.
fn encode_dir_and_name(enc: &mut XdrEncoder, dir: &FileHandle, name: &str) -> Result<(), XdrError> {
    if !is_one_component(name) {
        return Err(XdrError::NotOneComponent);
    }
    enc.opaque(dir.as_bytes());
    enc.string(name);
    Ok(())
}

/// The `SetAttr` that a `truncate` to `size` is.
///
/// A convenience with a reason: `size` is the one `sattr3` field whose use is
/// not "change this attribute" but "change the file's contents", and a
/// truncate that also sent a mode or a time would quietly do two things.
pub fn truncate_to(size: u64) -> SetAttr {
    SetAttr {
        size: Some(size),
        ..SetAttr::default()
    }
}

/// The `SetAttr` a `chmod` is, and the attributes a freshly created object
/// should get.
///
/// One function for both because they are the same request, and the masking
/// is why it exists: a caller holding a VFS mode has the type bits in it, and
/// sending those would ask the server to set bits that are not permissions.
///
/// The uid and gid stay unset on a create on purpose. The server assigns them
/// from the credential, as a local filesystem does from the process, and a
/// client that set them would be asking for a `chown` that `AUTH_UNIX`
/// usually may not do — so the create would fail outright rather than the
/// ownership merely being wrong.
pub fn mode_to(mode: u32) -> SetAttr {
    SetAttr {
        mode: Some(mode & 0o7777),
        ..SetAttr::default()
    }
}

#[cfg(test)]
#[path = "nfs3_write_tests.rs"]
mod nfs3_write_tests;
