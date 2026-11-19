//! `READDIR` and `READDIRPLUS` (RFC 1813 sections 3.3.16 and 3.3.17): reading
//! a directory, and reading it with the attributes and handles attached.
//!
//! Still pure functions from bytes to bytes, so still tested whole on the
//! host. Three things here have no counterpart anywhere else in the protocol.
//!
//! # The entry list is a chain, not an array
//!
//! Nothing on the wire says how many entries there are. Each entry is
//! *preceded* by a boolean saying whether one follows, the way XDR writes an
//! optional pointer, and the list ends with a `false` followed by the `eof`
//! flag:
//!
//! ```text
//! (true, entry)* false eof
//! ```
//!
//! A decoder that looked for a count would be one field out of step on the
//! first entry and stay there. It also means the length of the list is not
//! known until it has been read, so a reply claiming a huge number of entries
//! costs exactly as much memory as the bytes it actually sent — there is no
//! count to trust and therefore none to be lied to about.
//!
//! # `eof` is the only end of the directory, and an empty list is not it
//!
//! This is the [`READ`](super::nfs3::decode_read) trap again, in the one place
//! it bites harder. A server may answer with **zero entries and `eof` false**,
//! and that is legal: it means the `count` the client offered was too small to
//! encode even one entry, so nothing fit. The answer is to ask again with a
//! bigger `count` — *not* to conclude the directory ended.
//!
//! A client that read an empty list as the end of the directory would make
//! every `ls` of a large directory return a prefix of it, silently, with no
//! error anywhere. Which entries went missing would depend on the server's
//! buffer sizes, so it would look like file loss rather than like a bug. The
//! same mistake also hides behind [`Nfs3Status::TooSmall`], which is the other
//! way a server can say the same thing.
//!
//! So [`DirList::eof`] is the only thing that ever means "the directory is
//! finished", and [`DirList::needs_a_bigger_count`] names the empty-and-not-
//! finished case so a caller cannot reach it by accident. Looping on it is the
//! caller's job (see the note on that method: a caller that re-asks with the
//! same `count` spins forever).
//!
//! # The cookie is opaque, and it travels with its verifier
//!
//! A `cookie3` is a 64-bit number the server chose. It is *not* an index, an
//! offset, or a hash of the name: arithmetic on it, sorting by it, or
//! inventing one are all wrong, and the only valid use of one is handing back
//! the exact value the server gave. The first call sends zero.
//!
//! The `cookieverf3` is how NFSv3 fixed version 2's unverifiable cookies. The
//! server stamps each reply with one, and the client must send back the
//! verifier *that came with the cookie it is resuming from*. If the directory
//! was reorganised in between, the server sees a stale verifier and answers
//! [`Nfs3Status::BadCookie`] rather than quietly skipping or repeating
//! entries, which is what version 2 did. A verifier of eight zero bytes is
//! not "missing": RFC 1813 says a server whose cookies are always valid may
//! use zero as its verifier forever, so it is a value to echo like any other.

use alloc::string::String;
use alloc::vec::Vec;

use super::mount3::{FileHandle, FHSIZE3};
use super::nfs3::{decode_post_op_attr, is_one_component, Failure, Nfs3Status, PostOpAttr};
use super::rpc::{RpcError, RpcResult};
use super::xdr::{XdrDecoder, XdrEncoder, XdrError};

/// `NFSPROC3_READDIR`.
pub const PROC_READDIR: u32 = 16;
/// `NFSPROC3_READDIRPLUS`.
pub const PROC_READDIRPLUS: u32 = 17;

/// The size of a `cookieverf3` (`NFS3_COOKIEVERFSIZE`).
pub const COOKIEVERF_SIZE: usize = 8;

/// A `cookieverf3`: eight opaque bytes, with no length prefix on the wire.
pub type CookieVerf = [u8; COOKIEVERF_SIZE];

/// The cookie and verifier a first call sends.
///
/// Both are zero, and the verifier's zero is a real value rather than an
/// absence — see the module note.
pub const COOKIE_START: u64 = 0;
/// The verifier a first call sends.
pub const VERF_START: CookieVerf = [0; COOKIEVERF_SIZE];

/// Encode `READDIR`'s arguments.
///
/// `count` is the cap on the whole encoded reply, XDR overhead included, and
/// RFC 1813 says it should be at least the `dtpref` from
/// [`FSINFO`](super::nfs3::decode_fsinfo). A count below that is not an error
/// — the server simply may not fit an entry in it, which comes back as an
/// empty list or as [`Nfs3Status::TooSmall`].
pub fn encode_readdir(dir: &FileHandle, cookie: u64, verf: &CookieVerf, count: u32) -> Vec<u8> {
    let mut enc = XdrEncoder::with_capacity(dir.len() + COOKIEVERF_SIZE + 20);
    enc.opaque(dir.as_bytes());
    enc.u64(cookie);
    enc.opaque_fixed(verf);
    enc.u32(count);
    enc.into_bytes()
}

/// Encode `READDIRPLUS`'s arguments.
///
/// The two counts are *different numbers* and that is the whole point of the
/// procedure. `dircount` is how many bytes of plain directory information the
/// client wants — names and cookies, not attributes and not handles — and
/// `maxcount` caps the encoded reply with everything in it. RFC 1813's
/// implementation note explains why: the server sizes its own directory read
/// from `dircount`, so a client that only sends the large number makes the
/// server read, stat and build handles for entries it will then have to throw
/// away. The ratio is "usually at least 8 to 1".
///
/// **Divergence from Linux.** Linux's `encode_readdirplus3args` sets both to
/// the same value (`fs/nfs/nfs3xdr.c`, with the comment "we just make sure we
/// make dircount big enough"), which gives up the optimization the fields
/// exist for. Keeping them as two arguments costs nothing here and lets the
/// caller ask for what it actually wants.
pub fn encode_readdirplus(
    dir: &FileHandle,
    cookie: u64,
    verf: &CookieVerf,
    dircount: u32,
    maxcount: u32,
) -> Vec<u8> {
    let mut enc = XdrEncoder::with_capacity(dir.len() + COOKIEVERF_SIZE + 24);
    enc.opaque(dir.as_bytes());
    enc.u64(cookie);
    enc.opaque_fixed(verf);
    enc.u32(dircount);
    enc.u32(maxcount);
    enc.into_bytes()
}

/// One directory entry.
///
/// One type serves both procedures. After a `READDIR` the last two fields are
/// always `None`, because `entry3` does not carry them; after a
/// `READDIRPLUS` they are whatever the server chose to attach, which may
/// still be nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// The `fileid3`. RFC 1813 asks servers to avoid sending zero and clients
    /// to map it, because a zero inode number means "no file" to a lot of
    /// userspace. It is kept exactly as sent here; substituting a number is a
    /// decision for the layer that has an inode to substitute.
    pub fileid: u64,
    /// The name of this entry, one path component.
    pub name: String,
    /// The cookie to resume *after* this entry. Opaque — see the module note.
    pub cookie: u64,
    /// `READDIRPLUS` only: the attributes, which save a `GETATTR` per entry.
    pub attr: PostOpAttr,
    /// `READDIRPLUS` only: the handle, which saves a `LOOKUP` per entry.
    ///
    /// `post_op_fh3` is optional, and a server declining to send one is not
    /// an error: it means this entry costs a `LOOKUP` after all, not that the
    /// entry is bad.
    pub fh: Option<FileHandle>,
}

impl DirEntry {
    /// Whether this is one of the two entries every directory has.
    ///
    /// They are ordinary entries on the wire and a server is expected to send
    /// them, so they are decoded like any other. A caller that is building
    /// paths rather than answering `getdents` wants to skip them.
    pub fn is_dot_or_dotdot(&self) -> bool {
        self.name == "." || self.name == ".."
    }
}

/// What one call returned: the entries it fit, and whether that was all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirList {
    /// The entries, in the order the server sent them.
    pub entries: Vec<DirEntry>,
    /// **The only signal that the directory has ended.** See the module note:
    /// an empty [`entries`](Self::entries) with this false means "nothing
    /// fit", not "nothing left".
    pub eof: bool,
}

impl DirList {
    /// The server fit nothing in the reply but the directory is not finished.
    ///
    /// The `count` was too small for even one entry. The call has to be made
    /// again **with the same cookie and a bigger count**: re-asking with the
    /// same count gets the same empty answer forever, which is the shape this
    /// method exists to make visible at the call site rather than discoverable
    /// in a hung `ls`.
    pub fn needs_a_bigger_count(&self) -> bool {
        self.entries.is_empty() && !self.eof
    }

    /// The cookie to resume from, or `None` when there is nothing to resume.
    ///
    /// It is the last entry's cookie, because a `cookie3` names the position
    /// *after* its entry. `None` at the end of the directory, and `None` when
    /// nothing fit — in which case the caller re-sends the cookie it already
    /// had, not this one.
    pub fn next_cookie(&self) -> Option<u64> {
        if self.eof {
            return None;
        }
        self.entries.last().map(|e| e.cookie)
    }
}

/// A successful `READDIR` or `READDIRPLUS` reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadDirReply {
    /// The directory's own attributes, refreshing the client's cache of it.
    pub dir_attr: PostOpAttr,
    /// The verifier to send back with any cookie from this reply.
    pub verf: CookieVerf,
    /// The entries and the end-of-directory flag.
    pub list: DirList,
}

/// Decode a `READDIR` reply.
///
/// The failure arm carries the directory's `post_op_attr` and it is handed
/// back: a listing that failed still refreshes what the client knows about
/// the directory, and for [`Nfs3Status::BadCookie`] that refreshed attribute
/// is how the client learns the directory changed under it.
pub fn decode_readdir(results: &[u8]) -> RpcResult<Result<ReadDirReply, Failure>> {
    decode_any_readdir(results, false)
}

/// Decode a `READDIRPLUS` reply.
///
/// Identical in shape to [`decode_readdir`]; the entries carry two more
/// fields.
pub fn decode_readdirplus(results: &[u8]) -> RpcResult<Result<ReadDirReply, Failure>> {
    decode_any_readdir(results, true)
}

/// The body of both, because `READDIRPLUS3res` differs from `READDIR3res`
/// only inside the entry.
fn decode_any_readdir(results: &[u8], plus: bool) -> RpcResult<Result<ReadDirReply, Failure>> {
    let mut dec = XdrDecoder::new(results);
    let status = Nfs3Status::from_wire(dec.u32()?);
    let dir_attr = decode_post_op_attr(&mut dec)?;
    if status != Nfs3Status::Ok {
        return Ok(Err((status, dir_attr)));
    }

    let mut verf = VERF_START;
    verf.copy_from_slice(dec.opaque_fixed(COOKIEVERF_SIZE)?);

    let mut entries = Vec::new();
    while dec.bool().map_err(RpcError::Xdr)? {
        entries.push(decode_entry(&mut dec, plus)?);
    }
    let eof = dec.bool().map_err(RpcError::Xdr)?;

    Ok(Ok(ReadDirReply {
        dir_attr,
        verf,
        list: DirList { entries, eof },
    }))
}

/// Decode one `entry3`, or one `entryplus3` when `plus`.
///
/// The order is fixed and the two extra fields come *after* the cookie, with
/// the attributes before the handle. Reading them the other way round
/// mistakes the attributes' leading "are there any" boolean for the handle's
/// and desynchronizes the rest of the list.
fn decode_entry(dec: &mut XdrDecoder<'_>, plus: bool) -> RpcResult<DirEntry> {
    let fileid = dec.u64()?;
    let name = decode_name(dec)?;
    let cookie = dec.u64()?;

    let (attr, fh) = if plus {
        let attr = decode_post_op_attr(dec)?;
        let fh = if dec.bool().map_err(RpcError::Xdr)? {
            Some(FileHandle::new(dec.opaque_max(FHSIZE3)?).map_err(RpcError::Xdr)?)
        } else {
            None
        };
        (attr, fh)
    } else {
        (None, None)
    };

    Ok(DirEntry {
        fileid,
        name,
        cookie,
        attr,
        fh,
    })
}

/// Decode a `filename3` coming *from* the server, and check it is a name.
///
/// **Divergence from Linux, on purpose.** Linux's `decode_inline_filename3`
/// checks only the length. This also refuses an empty name and any name
/// holding `/`, the same two rules
/// [`encode_dir_op`](super::nfs3::encode_dir_op) applies on the way out.
///
/// The reason is that an NFS server is not a trusted peer. It is whatever
/// answered on port 2049, and a client that will later join these names onto
/// a path has handed path traversal to it: an entry named `../../etc` is a
/// directory listing that escapes the mount. Validating on the way out and
/// not on the way in checks the one side that was never the threat.
///
/// Refusing the reply rather than skipping the entry is deliberate too.
/// Dropping it would make a server able to hide a file from the client by
/// naming it badly, and the listing would look complete.
fn decode_name(dec: &mut XdrDecoder<'_>) -> RpcResult<String> {
    let name = String::from_utf8_lossy(dec.string_bytes()?).into_owned();
    if !is_one_component(&name) {
        return Err(RpcError::Xdr(XdrError::NotOneComponent));
    }
    Ok(name)
}

/// Whether this status means the cookie is no good and the listing has to
/// start over from [`COOKIE_START`].
///
/// It is not an I/O error and should not be reported as one: the directory is
/// fine, the client's place in it is not. The caller discards what it has and
/// re-reads from the beginning — which is also why a client should not hold
/// cookies across its own `REMOVE` and `CREATE` calls.
pub fn cookie_is_stale(status: Nfs3Status) -> bool {
    status == Nfs3Status::BadCookie
}

/// Whether this status means the client's `count` was too small, which is the
/// other face of [`DirList::needs_a_bigger_count`].
///
/// A server may answer either way for the same condition, so a caller that
/// handles only one of them still hangs or truncates on the servers that pick
/// the other.
pub fn count_was_too_small(status: Nfs3Status) -> bool {
    status == Nfs3Status::TooSmall
}

#[cfg(test)]
#[path = "nfs3_dir_tests.rs"]
mod nfs3_dir_tests;
