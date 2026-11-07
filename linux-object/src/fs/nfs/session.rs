//! The session: the layer that actually makes NFSv3 calls.
//!
//! Everything below this point is a codec: bytes in, bytes out, no state.
//! Everything above it is the VFS, which asks for a file's attributes and
//! expects an `LxResult`. This is the layer in between, and it owns the four
//! things that are not in the codec and are not the VFS's business either:
//!
//! - **The errno.** A `nfsstat3` is not an errno (see
//!   [`Nfs3Status`](super::nfs3::Nfs3Status)), an [`RpcError`] is not one
//!   either, and neither is a [`TransportError`]. Three kinds of failure
//!   arrive here and one kind leaves.
//! - **The retries that are not the RPC client's.** The client retries a call
//!   that was never *answered*. `NFS3ERR_JUKEBOX` is a call that was answered,
//!   with "ask again later", and retrying it is this layer's job — see
//!   [`NfsSession::jukebox_retries`].
//! - **The loops the protocol makes mandatory.** A `READ` may come back short,
//!   a `WRITE` may be taken in part, and a `READDIR` is a sequence of calls
//!   and not one call. Each of those loops is a place where a plausible
//!   implementation silently loses data, so each is written once here rather
//!   than at every call site.
//! - **The negotiated sizes.** `FSINFO` says what the server will carry, the
//!   mount options say what was asked for, and the smaller of the two is what
//!   every subsequent call uses.
//!
//! # Why the loops are here and not in the VFS
//!
//! Three of them are the same bug with three faces, and all three faces look
//! like success:
//!
//! - A `READ` that returns fewer bytes than asked for has **not** reached the
//!   end of the file; only `eof` says that. A caller that stops at the short
//!   read truncates the file, with no error anywhere.
//! - A `WRITE` that the server took in part reports how much it took. A caller
//!   that assumes it all went writes a file with a hole in it.
//! - A `READDIR` that comes back with no entries and `eof` false has not
//!   finished the directory. A caller that stops there lists a prefix of it.
//!
//! None of those produce an error code, which is precisely why they belong in
//! one place with a test each rather than in every caller.
//!
//! # Bounded, because the server is not trusted
//!
//! Each loop here has an exit that does not depend on the server behaving.
//! A server that answers every `READ` with zero bytes and `eof` false, or
//! that hands back a `READDIR` cookie it has already given, or that takes
//! zero bytes of every `WRITE`, is not a hypothetical: it is the cheapest way
//! to wedge a client, and in this kernel a wedged VFS call holds the thread
//! that drives the network stack. So a loop that cannot make progress fails
//! with `EIO` rather than turning around again.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::{LxError, LxResult};

use super::client::{CallError, RetryPolicy, RpcClient, RpcTransport, TransportError};
use super::mount3::FileHandle;
use super::nfs3::WccData;
use super::nfs3::{self, Fattr3, FsInfo, FsStat, LookupReply, Nfs3Status, ReadReply};
use super::nfs3_dir::{self, CookieVerf, DirEntry, COOKIE_START, VERF_START};
use super::nfs3_write::{
    self, CommitReply, CreateHow, CreateReply, LinkReply, NodeKind, RenameWcc, SetAttr, StableHow,
    WriteReply,
};
use super::rpc::{AuthUnix, Credential, RpcError, RpcRejection};
use super::source::{machine_name, NfsOptions, MAX_TRANSFER_SIZE};
use super::xdr::XdrError;

/// How many times `NFS3ERR_JUKEBOX` is retried before it is reported.
///
/// Linux retries it with a growing delay and no limit while the mount is
/// hard. A limit here for the same reason the RPC client has a deadline: the
/// retry happens on a kernel thread that is driving the network stack, so
/// "forever" is not a thing this kernel can offer a caller.
pub const JUKEBOX_RETRIES: u32 = 10;

/// How long to wait between `NFS3ERR_JUKEBOX` retries, in milliseconds.
///
/// The status means the file is on tape, in cold storage, or otherwise not to
/// hand. Asking again immediately gets the same answer and only costs round
/// trips, so the pause is the point of the retry.
pub const JUKEBOX_PAUSE_MS: u64 = 1_000;

/// A bound on the pause loop's turns, in case the clock does not move.
///
/// The same lesson as the transport's `MAX_PARK_PASSES`: a loop whose only
/// exit is the clock does not terminate if the clock stops, and the thread it
/// stops is the one driving the network.
pub const MAX_PAUSE_PASSES: u32 = 5_000;

/// How many `READDIR` calls one directory listing may take.
///
/// Generous: a directory of a million entries at a few hundred entries per
/// call is a few thousand rounds. It exists for the server that keeps handing
/// back cookies forever, not to limit a real directory.
pub const READDIR_MAX_ROUNDS: u32 = 8_192;

/// How many entries one listing may accumulate.
///
/// A listing is collected in memory, so this is a bound on the kernel heap a
/// single `ls` can ask for — and on what a server that generates entries
/// forever can make this kernel allocate. A caller that must handle a
/// directory larger than this streams it with
/// [`NfsSession::readdir_chunk`] instead.
pub const READDIR_MAX_ENTRIES: usize = 1 << 20;

/// What a `READDIR` asks the server to fit in one reply, when `FSINFO` has
/// not said otherwise.
pub const DEFAULT_DIR_COUNT: usize = 8 * 1024;

/// How much bigger a `READDIR` count gets when the server says nothing fit.
///
/// Growing rather than re-asking is the whole point: a client that re-sends
/// the same count gets the same empty answer forever, which is the hang
/// [`DirList::needs_a_bigger_count`](super::nfs3_dir::DirList::needs_a_bigger_count)
/// exists to make visible.
pub const DIR_COUNT_GROWTH: usize = 4;

/// The errno for a failure of the RPC machinery rather than of the procedure.
///
/// A soft mount's timeout is `EIO`, which is what Linux reports unless
/// `softerr` is set — the option exists because `ETIMEDOUT` is more
/// informative and also breaks programs that treat anything but `EIO` from a
/// read as fatal. Starting where Linux's default is means an `fstab` line
/// copied from a Linux box behaves the same way.
pub fn call_error_to_lx(e: CallError) -> LxError {
    match e {
        CallError::Transport(t) => transport_error_to_lx(t),
        CallError::Rpc(r) => rpc_error_to_lx(r),
        CallError::TimedOut => LxError::EIO,
        // A reply cut off part-way through is not a short reply: there is no
        // answer, and the connection cannot be trusted for the next call
        // either.
        CallError::TruncatedMessage => LxError::EIO,
    }
}

/// The errno for a transport failure.
pub fn transport_error_to_lx(e: TransportError) -> LxError {
    match e {
        TransportError::NotConnected => LxError::ENOTCONN,
        // The peer closed or reset. `EPIPE` would be right for a write to a
        // socket; for a filesystem call it is `EIO`, because what failed was
        // the call and not a pipe the caller holds.
        TransportError::Closed | TransportError::Reset => LxError::EIO,
        TransportError::Other => LxError::EIO,
    }
}

/// The errno for a failure of the RPC envelope or its decoding.
pub fn rpc_error_to_lx(e: RpcError) -> LxError {
    match e {
        RpcError::Xdr(x) => xdr_error_to_lx(x),
        // The stream is out of step, or the message was not a reply at all.
        // Either way the connection is unusable, and `EIO` is what the caller
        // can do something about.
        RpcError::XidMismatch { .. } | RpcError::NotAReply(_) | RpcError::BadFraming => {
            LxError::EIO
        }
        RpcError::Rejected(r) => rejection_to_lx(r),
    }
}

/// The errno for a server that refused the envelope.
fn rejection_to_lx(r: RpcRejection) -> LxError {
    match r {
        // The credential was refused. This is the one rejection that is about
        // *who asked* rather than about the protocol, and it is what a server
        // answers to a client whose uid it will not accept — so `EACCES`, the
        // same thing a local filesystem would say, and not a generic `EIO`
        // that hides a permissions problem behind an I/O error.
        RpcRejection::AuthError(_) => LxError::EACCES,
        // The program, version or procedure is not there. `EOPNOTSUPP` says
        // what happened: the server is answering, it just does not do this.
        RpcRejection::ProgramUnavailable
        | RpcRejection::ProgramMismatch { .. }
        | RpcRejection::ProcedureUnavailable
        | RpcRejection::VersionMismatch { .. } => LxError::EOPNOTSUPP,
        // The server could not decode what this client sent. Always a bug on
        // this side, and `EIO` is the honest report of it.
        RpcRejection::GarbageArgs => LxError::EIO,
        RpcRejection::SystemError | RpcRejection::Unknown(_) => LxError::EIO,
    }
}

/// The errno for a malformed reply, or for arguments this client refused to
/// encode.
pub fn xdr_error_to_lx(e: XdrError) -> LxError {
    match e {
        // A name that is not one path component. The only one of these that
        // is about the *arguments* rather than about a reply, and the errno is
        // the one a local filesystem gives for the same name.
        XdrError::NotOneComponent => LxError::EINVAL,
        // Everything else is a reply this client could not parse.
        XdrError::Truncated
        | XdrError::LengthTooLarge
        | XdrError::BadPadding
        | XdrError::BadDiscriminant(_) => LxError::EIO,
    }
}

/// One mounted export's conversation with its server.
///
/// Holds the RPC client, the credential every call carries, the root
/// filehandle `MNT` returned, and the transfer sizes in force.
#[derive(Debug)]
pub struct NfsSession<T: RpcTransport> {
    rpc: RpcClient<T>,
    cred: Credential,
    root: FileHandle,
    rsize: usize,
    wsize: usize,
    dir_count: usize,
    read_only: bool,
    jukebox_retries: u32,
    jukebox_pause_ms: u64,
    /// The listing bounds in force. See [`READDIR_MAX_ROUNDS`] and
    /// [`READDIR_MAX_ENTRIES`].
    ///
    /// Fields and not constants for the reason the jukebox policy is a field:
    /// both are deliberately wide enough that no real directory reaches them,
    /// and a bound nothing can reach is a bound nothing measures. A test
    /// lowers them; nothing else should.
    readdir_max_rounds: u32,
    /// See [`READDIR_MAX_ENTRIES`].
    readdir_max_entries: usize,
    /// What `FSINFO` said, once it has been asked.
    fsinfo: Option<FsInfo>,
}

impl<T: RpcTransport> NfsSession<T> {
    /// A session over `transport`, rooted at the handle `MNT` returned.
    ///
    /// The transfer sizes start at what the options asked for and are lowered
    /// by [`negotiate`](Self::negotiate); nothing here assumes the server can
    /// carry what was asked.
    pub fn new(transport: T, first_xid: u32, root: FileHandle, opts: &NfsOptions) -> Self {
        let policy = RetryPolicy {
            timeout_ms: opts.timeout_ms,
            retrans: opts.retrans,
            soft: opts.soft,
            ..RetryPolicy::default()
        };
        Self {
            rpc: RpcClient::new(transport, first_xid, policy),
            // `AUTH_UNIX` as root until a caller says otherwise. The server
            // decides what that means — most export with `root_squash`, which
            // maps it to `nobody` — and this client does not pretend to know.
            cred: Credential::Unix(AuthUnix::new(&machine_name(), 0, 0)),
            root,
            rsize: opts.rsize,
            wsize: opts.wsize,
            dir_count: DEFAULT_DIR_COUNT,
            read_only: opts.read_only,
            jukebox_retries: JUKEBOX_RETRIES,
            jukebox_pause_ms: JUKEBOX_PAUSE_MS,
            readdir_max_rounds: READDIR_MAX_ROUNDS,
            readdir_max_entries: READDIR_MAX_ENTRIES,
            fsinfo: None,
        }
    }

    /// The root of the export.
    pub fn root(&self) -> &FileHandle {
        &self.root
    }

    /// The credential every call carries.
    pub fn credential(&self) -> &Credential {
        &self.cred
    }

    /// Make every later call claim `uid`/`gid`.
    ///
    /// The server is what enforces this; the client states it. An NFSv3
    /// `AUTH_UNIX` mount trusts the client to say who is asking, which is why
    /// `root_squash` exists on the other side.
    pub fn set_identity(&mut self, uid: u32, gid: u32) {
        self.cred = Credential::Unix(AuthUnix::new(&machine_name(), uid, gid));
    }

    /// Bytes per `READ`.
    pub fn read_size(&self) -> usize {
        self.rsize
    }

    /// Bytes per `WRITE`.
    pub fn write_size(&self) -> usize {
        self.wsize
    }

    /// Bytes a `READDIR` asks the server to fit in one reply.
    pub fn dir_count(&self) -> usize {
        self.dir_count
    }

    /// Whether the mount refuses to change anything.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// What `FSINFO` said, if it has been asked.
    pub fn fsinfo(&self) -> Option<&FsInfo> {
        self.fsinfo.as_ref()
    }

    /// Replies discarded as answers to calls already given up on.
    pub fn stale_replies(&self) -> u64 {
        self.rpc.stale_replies()
    }

    /// The transport, for a caller that has to ask it something.
    pub fn transport(&self) -> &T {
        self.rpc.transport()
    }

    /// The transport, mutably.
    pub fn transport_mut(&mut self) -> &mut T {
        self.rpc.transport_mut()
    }

    /// How many times `NFS3ERR_JUKEBOX` is retried. See [`JUKEBOX_RETRIES`].
    pub fn jukebox_retries(&self) -> u32 {
        self.jukebox_retries
    }

    /// Set the jukebox retry budget and the pause between tries.
    ///
    /// A test wants a pause of zero; nothing else should change these.
    pub fn set_jukebox_policy(&mut self, retries: u32, pause_ms: u64) {
        self.jukebox_retries = retries;
        self.jukebox_pause_ms = pause_ms;
    }

    /// Lower the listing bounds so a test can reach them. See
    /// [`Self::readdir_max_rounds`].
    pub fn set_readdir_limits(&mut self, rounds: u32, entries: usize) {
        self.readdir_max_rounds = rounds;
        self.readdir_max_entries = entries;
    }

    /// Ask `FSINFO` and lower the transfer sizes to what the server will
    /// carry.
    ///
    /// Called once after the mount. The negotiation itself lives in
    /// [`FsInfo::read_size`] and [`FsInfo::write_size`], which round *down*
    /// to the server's multiple; see those for why down and not up.
    pub fn negotiate(&mut self) -> LxResult<FsInfo> {
        let info = self.fsinfo_of(&self.root.clone())?;
        self.rsize = info.read_size(self.rsize);
        self.wsize = info.write_size(self.wsize);
        // `dtpref` is the server's preferred READDIR size. Unlike the
        // transfer sizes it is not a limit the server enforces, so the client
        // takes the server's preference when there is one and keeps its own
        // default when the server says zero — which means "no opinion", not
        // "nothing".
        if info.dtpref != 0 {
            self.dir_count = (info.dtpref as usize).min(MAX_TRANSFER_SIZE);
        }
        self.fsinfo = Some(info);
        Ok(info)
    }

    /// `FSINFO` on a handle.
    pub fn fsinfo_of(&mut self, fh: &FileHandle) -> LxResult<FsInfo> {
        let args = nfs3::encode_fh_only(fh);
        self.call_decoding(nfs3::PROC_FSINFO, &args, |r| {
            nfs3::decode_fsinfo(r).map(|o| o.map(|(info, _)| info).map_err(|(s, _)| s))
        })
    }

    /// `FSSTAT`: the numbers `statfs(2)` wants.
    pub fn fsstat(&mut self, fh: &FileHandle) -> LxResult<FsStat> {
        let args = nfs3::encode_fh_only(fh);
        self.call_decoding(nfs3::PROC_FSSTAT, &args, |r| {
            nfs3::decode_fsstat(r).map(|o| o.map(|(st, _)| st).map_err(|(s, _)| s))
        })
    }

    /// `GETATTR`: a file's attributes, with no cache in front of it.
    pub fn getattr(&mut self, fh: &FileHandle) -> LxResult<Fattr3> {
        let args = nfs3::encode_fh_only(fh);
        self.call_decoding(nfs3::PROC_GETATTR, &args, nfs3::decode_getattr)
    }

    /// `LOOKUP`: one name in one directory.
    pub fn lookup(&mut self, dir: &FileHandle, name: &str) -> LxResult<LookupReply> {
        let args = nfs3::encode_dir_op(dir, name).map_err(xdr_error_to_lx)?;
        self.call_decoding(nfs3::PROC_LOOKUP, &args, |r| {
            nfs3::decode_lookup(r).map(|o| o.map_err(|(s, _)| s))
        })
    }

    /// `ACCESS`: which of `mask`'s `ACCESS3_*` bits the server allows.
    ///
    /// The answer is a *subset* of what was asked, and a bit the server left
    /// out is a bit it refuses. A caller that compares the answer to zero
    /// rather than to the mask grants everything as soon as one bit survives.
    pub fn access(&mut self, fh: &FileHandle, mask: u32) -> LxResult<u32> {
        let args = nfs3::encode_access(fh, mask);
        self.call_decoding(nfs3::PROC_ACCESS, &args, |r| {
            nfs3::decode_access(r).map(|o| o.map(|(bits, _)| bits).map_err(|(s, _)| s))
        })
    }

    /// `READLINK`: where a symbolic link points.
    ///
    /// The target is bytes on the wire and may hold a `/` — it is a path and
    /// not a `filename3`, which is the one place the one-component rule does
    /// not apply.
    pub fn readlink(&mut self, fh: &FileHandle) -> LxResult<String> {
        let args = nfs3::encode_fh_only(fh);
        let bytes = self.call_decoding(nfs3::PROC_READLINK, &args, |r| {
            nfs3::decode_readlink(r).map(|o| o.map(|(b, _)| b).map_err(|(s, _)| s))
        })?;
        Ok(nfs3::link_target_to_string(&bytes))
    }

    /// One `READ`, of at most [`read_size`](Self::read_size) bytes.
    ///
    /// The reply may be shorter than `count` without that meaning anything is
    /// wrong or finished; [`read_into`](Self::read_into) is the loop that
    /// deals with it.
    pub fn read_once(&mut self, fh: &FileHandle, offset: u64, count: usize) -> LxResult<ReadReply> {
        let count = count.min(self.rsize).min(u32::MAX as usize) as u32;
        let args = nfs3::encode_read(fh, offset, count);
        self.call_decoding(nfs3::PROC_READ, &args, |r| {
            nfs3::decode_read(r).map(|o| o.map_err(|(s, _)| s))
        })
    }

    /// Fill `buf` from `offset`, as `read_at` wants it: as many bytes as are
    /// there, and fewer only at the end of the file.
    ///
    /// Returns how many bytes were read, which is short of `buf.len()` only
    /// when the file ended.
    ///
    /// # The loop, and why the short read is not the end
    ///
    /// A server may answer with fewer bytes than asked for whenever it likes
    /// — because the read crossed its preferred transfer size, because the
    /// file is in pieces, because it felt like it. Only `eof` means the file
    /// ended. So the loop continues past a short read and stops on `eof`, on
    /// a full buffer, or on a reply with no bytes in it and no `eof`, which
    /// is a server making no progress and is the one case that has to be an
    /// error rather than another turn.
    pub fn read_into(&mut self, fh: &FileHandle, offset: u64, buf: &mut [u8]) -> LxResult<usize> {
        let mut done = 0usize;
        while done < buf.len() {
            let want = buf.len() - done;
            let at = offset
                .checked_add(done as u64)
                // A read past the end of the 64-bit offset space. Not an I/O
                // failure: there is nothing there to read.
                .ok_or(LxError::EINVAL)?;
            let reply = self.read_once(fh, at, want)?;
            let n = reply.data.len().min(want);
            buf[done..done + n].copy_from_slice(&reply.data[..n]);
            done += n;
            if reply.eof {
                break;
            }
            if n == 0 {
                // No bytes and not the end of the file. A correct server does
                // not do this; one that does would spin this loop forever.
                return Err(LxError::EIO);
            }
        }
        Ok(done)
    }

    /// One `WRITE`. See [`write_all`](Self::write_all) for the loop.
    pub fn write_once(
        &mut self,
        fh: &FileHandle,
        offset: u64,
        data: &[u8],
        stable: StableHow,
    ) -> LxResult<WriteReply> {
        self.check_writable()?;
        let data = &data[..data.len().min(self.wsize)];
        let args = nfs3_write::encode_write(fh, offset, stable, data);
        let sent = data.len();
        let reply: WriteReply = self.call_decoding(nfs3_write::PROC_WRITE, &args, |r| {
            nfs3_write::decode_write(r).map(|o| o.map_err(|(s, _)| s))
        })?;
        // A server that claims to have written more than it was sent has
        // either lost track or is lying, and either way the offset the caller
        // would compute next is wrong. Refusing is the only answer that does
        // not corrupt the file.
        if reply.count as usize > sent {
            return Err(LxError::EIO);
        }
        Ok(reply)
    }

    /// Write all of `data` at `offset`, in [`write_size`](Self::write_size)
    /// chunks.
    ///
    /// Returns how durable the data is: the weakest
    /// [`StableHow`](super::nfs3_write::StableHow) any chunk came back with,
    /// because a caller deciding whether it still needs a `COMMIT` has to
    /// take the weakest answer and not the last one. The write verifier of
    /// the last chunk comes with it, for that `COMMIT` to compare against.
    ///
    /// The offset advances by what the **server** took, not by what was sent:
    /// a short write is flow control, and a caller that assumed the whole
    /// chunk landed would leave a hole in the file and never know.
    pub fn write_all(
        &mut self,
        fh: &FileHandle,
        offset: u64,
        data: &[u8],
        stable: StableHow,
    ) -> LxResult<(StableHow, nfs3_write::WriteVerf)> {
        self.check_writable()?;
        let mut done = 0usize;
        let mut weakest = StableHow::FileSync;
        let mut verf = [0u8; nfs3_write::VERF_SIZE];
        while done < data.len() {
            let at = offset.checked_add(done as u64).ok_or(LxError::EFBIG)?;
            let reply = self.write_once(fh, at, &data[done..], stable)?;
            let n = reply.count as usize;
            if n == 0 {
                // The server took nothing and did not fail. Another turn
                // would send the same bytes to the same answer.
                return Err(LxError::EIO);
            }
            done += n;
            weakest = weakest_of(weakest, reply.committed);
            verf = reply.verf;
        }
        Ok((weakest, verf))
    }

    /// `COMMIT`: make unstable writes durable.
    ///
    /// A verifier different from the one the writes came back with means the
    /// server restarted and those writes are gone; comparing it is the
    /// caller's job, because only the caller still has the data to send
    /// again.
    pub fn commit(&mut self, fh: &FileHandle, offset: u64, count: u32) -> LxResult<CommitReply> {
        self.check_writable()?;
        let args = nfs3_write::encode_commit(fh, offset, count);
        self.call_decoding(nfs3_write::PROC_COMMIT, &args, |r| {
            nfs3_write::decode_commit(r).map(|o| o.map_err(|(s, _)| s))
        })
    }

    /// `SETATTR`, with an optional `ctime` guard.
    ///
    /// The guard is a compare-and-set: the change is applied only if the
    /// file's `ctime` still matches, and `NFS3ERR_NOT_SYNC` means it did not.
    pub fn setattr(
        &mut self,
        fh: &FileHandle,
        attr: &SetAttr,
        guard: Option<rcore_fs::vfs::Timespec>,
    ) -> LxResult<WccData> {
        self.check_writable()?;
        let args = nfs3_write::encode_setattr(fh, attr, guard);
        self.call_decoding(nfs3_write::PROC_SETATTR, &args, |r| {
            nfs3_write::decode_setattr(r).map(|o| o.map_err(|(s, _)| s))
        })
    }

    /// `CREATE`.
    pub fn create(
        &mut self,
        dir: &FileHandle,
        name: &str,
        how: CreateHow,
    ) -> LxResult<CreateReply> {
        self.check_writable()?;
        let args = nfs3_write::encode_create(dir, name, how).map_err(xdr_error_to_lx)?;
        self.create_call(nfs3_write::PROC_CREATE, &args)
    }

    /// `MKDIR`.
    pub fn mkdir(&mut self, dir: &FileHandle, name: &str, attr: &SetAttr) -> LxResult<CreateReply> {
        self.check_writable()?;
        let args = nfs3_write::encode_mkdir(dir, name, attr).map_err(xdr_error_to_lx)?;
        self.create_call(nfs3_write::PROC_MKDIR, &args)
    }

    /// `SYMLINK`. The target is a path and may hold a `/`.
    pub fn symlink(
        &mut self,
        dir: &FileHandle,
        name: &str,
        target: &[u8],
        attr: &SetAttr,
    ) -> LxResult<CreateReply> {
        self.check_writable()?;
        let args = nfs3_write::encode_symlink(dir, name, attr, target).map_err(xdr_error_to_lx)?;
        self.create_call(nfs3_write::PROC_SYMLINK, &args)
    }

    /// `MKNOD`.
    pub fn mknod(
        &mut self,
        dir: &FileHandle,
        name: &str,
        kind: NodeKind,
        attr: &SetAttr,
    ) -> LxResult<CreateReply> {
        self.check_writable()?;
        let args = nfs3_write::encode_mknod(dir, name, kind, attr).map_err(xdr_error_to_lx)?;
        self.create_call(nfs3_write::PROC_MKNOD, &args)
    }

    /// The four procedures that answer `diropok3`, which is one shape.
    fn create_call(&mut self, procedure: u32, args: &[u8]) -> LxResult<CreateReply> {
        self.call_decoding(procedure, args, |r| {
            nfs3_write::decode_create(r).map(|o| o.map_err(|(s, _)| s))
        })
    }

    /// `REMOVE`: unlink a name that is not a directory.
    pub fn remove(&mut self, dir: &FileHandle, name: &str) -> LxResult<WccData> {
        self.check_writable()?;
        let args = nfs3_write::encode_remove(dir, name).map_err(xdr_error_to_lx)?;
        self.call_decoding(nfs3_write::PROC_REMOVE, &args, |r| {
            nfs3_write::decode_remove(r).map(|o| o.map_err(|(s, _)| s))
        })
    }

    /// `RMDIR`: remove an empty directory.
    ///
    /// The same arguments and the same reply shape as `REMOVE`, and a
    /// different procedure number: the server is what refuses a directory to
    /// one and a file to the other.
    pub fn rmdir(&mut self, dir: &FileHandle, name: &str) -> LxResult<WccData> {
        self.check_writable()?;
        let args = nfs3_write::encode_remove(dir, name).map_err(xdr_error_to_lx)?;
        self.call_decoding(nfs3_write::PROC_RMDIR, &args, |r| {
            nfs3_write::decode_remove(r).map(|o| o.map_err(|(s, _)| s))
        })
    }

    /// `RENAME`.
    pub fn rename(
        &mut self,
        from_dir: &FileHandle,
        from: &str,
        to_dir: &FileHandle,
        to: &str,
    ) -> LxResult<RenameWcc> {
        self.check_writable()?;
        let args =
            nfs3_write::encode_rename(from_dir, from, to_dir, to).map_err(xdr_error_to_lx)?;
        self.call_decoding(nfs3_write::PROC_RENAME, &args, |r| {
            nfs3_write::decode_rename(r).map(|o| o.map_err(|(s, _)| s))
        })
    }

    /// `LINK`: a second name for an existing file.
    pub fn link(&mut self, fh: &FileHandle, dir: &FileHandle, name: &str) -> LxResult<LinkReply> {
        self.check_writable()?;
        let args = nfs3_write::encode_link(fh, dir, name).map_err(xdr_error_to_lx)?;
        self.call_decoding(nfs3_write::PROC_LINK, &args, |r| {
            nfs3_write::decode_link(r).map(|o| o.map_err(|(s, _)| s))
        })
    }

    /// One `READDIR` or `READDIRPLUS` call.
    ///
    /// The caller passes the cookie and the verifier that came with it, and
    /// gets back the reply as the server sent it — including the
    /// nothing-fit case, which is
    /// [`DirList::needs_a_bigger_count`](super::nfs3_dir::DirList::needs_a_bigger_count)
    /// and not the end of the directory.
    pub fn readdir_chunk(
        &mut self,
        dir: &FileHandle,
        cookie: u64,
        verf: &CookieVerf,
        count: usize,
        plus: bool,
    ) -> LxResult<nfs3_dir::ReadDirReply> {
        let count = count.min(MAX_TRANSFER_SIZE).min(u32::MAX as usize) as u32;
        if plus {
            // `dircount` bounds the names and `maxcount` the whole reply, so
            // the first is the smaller of the two. Sending the same number
            // for both asks the server to fit the attributes and handles in
            // the space the names alone were budgeted.
            let args = nfs3_dir::encode_readdirplus(dir, cookie, verf, count / 4, count);
            self.call_decoding(nfs3_dir::PROC_READDIRPLUS, &args, |r| {
                nfs3_dir::decode_readdirplus(r).map(|o| o.map_err(|(s, _)| s))
            })
        } else {
            let args = nfs3_dir::encode_readdir(dir, cookie, verf, count);
            self.call_decoding(nfs3_dir::PROC_READDIR, &args, |r| {
                nfs3_dir::decode_readdir(r).map(|o| o.map_err(|(s, _)| s))
            })
        }
    }

    /// Read a whole directory.
    ///
    /// # The three ways this loop goes wrong
    ///
    /// - **Stopping at an empty reply.** A reply with no entries and `eof`
    ///   false means the `count` was too small to encode even one entry. The
    ///   answer is the *same cookie with a bigger count*; re-asking with the
    ///   same count gets the same answer forever, and treating it as the end
    ///   lists a prefix of the directory with no error.
    /// - **Resuming from the wrong cookie.** The cookie to continue from is
    ///   the **last** entry's, and the verifier that came with that reply has
    ///   to go back with it. A verifier of eight zeroes is a value, not an
    ///   absence.
    /// - **Trusting the server to finish.** A server that keeps answering
    ///   with entries, or that hands back a cookie it has already given,
    ///   makes this loop run until the machine dies. So a cookie that does
    ///   not advance is `EIO`, and the rounds and the entries are both
    ///   bounded.
    ///
    /// `NFS3ERR_BAD_COOKIE` means the directory changed under the listing.
    /// Linux starts the listing again, and so does this — once. A second one
    /// is a directory changing faster than it can be read, and reported
    /// rather than retried forever.
    pub fn readdir_all(&mut self, dir: &FileHandle, plus: bool) -> LxResult<Vec<DirEntry>> {
        let mut restarted = false;
        loop {
            match self.readdir_pass(dir, plus) {
                Err(LxError::EINVAL) if !restarted => {
                    // `BAD_COOKIE` and `NOT_SYNC` both map to `EINVAL`, and
                    // only the first can happen here. Start over once.
                    restarted = true;
                }
                other => return other,
            }
        }
    }

    /// One attempt at a whole listing, from the start of the directory.
    fn readdir_pass(&mut self, dir: &FileHandle, plus: bool) -> LxResult<Vec<DirEntry>> {
        let mut out: Vec<DirEntry> = Vec::new();
        let mut cookie = COOKIE_START;
        let mut verf = VERF_START;
        let mut count = self.dir_count;
        let mut rounds = 0u32;
        loop {
            rounds += 1;
            if rounds > self.readdir_max_rounds {
                return Err(LxError::EIO);
            }
            let reply = self.readdir_chunk(dir, cookie, &verf, count, plus)?;
            // The verifier travels with the cookie taken from this reply.
            verf = reply.verf;
            if reply.list.needs_a_bigger_count() {
                // Nothing fit. Same cookie, bigger count — and if the count
                // cannot grow any further the server will never fit an entry
                // in a reply this client can ask for.
                if count >= MAX_TRANSFER_SIZE {
                    return Err(LxError::EIO);
                }
                count = count
                    .saturating_mul(DIR_COUNT_GROWTH)
                    .min(MAX_TRANSFER_SIZE);
                continue;
            }
            if out.len().saturating_add(reply.list.entries.len()) > self.readdir_max_entries {
                return Err(LxError::EIO);
            }
            let next = reply.list.next_cookie();
            out.extend(reply.list.entries);
            match next {
                // End of the directory: the only thing that means it.
                None => return Ok(out),
                // A cookie this listing has already been given. The server is
                // not advancing, and another round sends the same request.
                Some(c) if c == cookie => return Err(LxError::EIO),
                Some(c) => cookie = c,
            }
        }
    }

    /// Refuse a change on a mount that was asked to make none.
    ///
    /// Before the wire, not after it: a read-only mount should not be able to
    /// reach a server with a `WRITE` at all, and the server's `EROFS` — if it
    /// even has one — would arrive after the call was made.
    fn check_writable(&self) -> LxResult<()> {
        if self.read_only {
            return Err(LxError::EROFS);
        }
        Ok(())
    }

    /// Make one call, decode it, and retry `NFS3ERR_JUKEBOX`.
    ///
    /// The decoder is handed in as a closure that flattens whatever the
    /// procedure's failure arm carries down to the status, because the
    /// `post_op_attr` and `wcc_data` those arms hold are a cache refresh for
    /// a layer that has a cache — which this one does not.
    fn call_decoding<R, F>(&mut self, procedure: u32, args: &[u8], decode: F) -> LxResult<R>
    where
        F: Fn(&[u8]) -> Result<Result<R, Nfs3Status>, RpcError>,
    {
        let mut tries = 0u32;
        loop {
            let results = {
                // Split the borrow: the call needs the client mutably and the
                // credential by reference, and they are two fields.
                let Self { rpc, cred, .. } = self;
                rpc.call(nfs3::target(procedure), cred, args)
                    .map_err(call_error_to_lx)?
            };
            match decode(&results).map_err(rpc_error_to_lx)? {
                Ok(v) => return Ok(v),
                Err(status) if status.is_retryable() && tries < self.jukebox_retries => {
                    tries += 1;
                    self.pause();
                }
                Err(status) => return Err(status.to_lx_error()),
            }
        }
    }

    /// Wait before retrying a `JUKEBOX`, driving the stack while waiting.
    ///
    /// Bounded in passes as well as in time, for the reason the transport's
    /// park is: if the clock does not move, the only other way out is a
    /// counter.
    fn pause(&mut self) {
        if self.jukebox_pause_ms == 0 {
            return;
        }
        let until = self
            .rpc
            .transport()
            .now_ms()
            .saturating_add(self.jukebox_pause_ms);
        let mut passes = 0u32;
        while self.rpc.transport().now_ms() < until && passes < MAX_PAUSE_PASSES {
            passes += 1;
            self.rpc.transport_mut().poll();
        }
    }
}

/// The weaker of two durability levels.
///
/// A caller that wrote a file in chunks and is deciding whether it still owes
/// a `COMMIT` has to take the weakest answer any chunk gave. Taking the last
/// one — or the strongest — is how a file comes back missing its middle after
/// the server reboots.
pub fn weakest_of(a: StableHow, b: StableHow) -> StableHow {
    fn rank(s: StableHow) -> u8 {
        match s {
            StableHow::Unstable => 0,
            StableHow::DataSync => 1,
            StableHow::FileSync => 2,
        }
    }
    if rank(a) <= rank(b) {
        a
    } else {
        b
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod session_tests;
