//! ONC RPC version 2 (RFC 1831/5531): the call and reply envelope every NFS
//! message travels in, and the record marking that frames it on TCP.
//!
//! # Record marking, and why it is the part that goes wrong
//!
//! RPC was designed for UDP, where a datagram *is* a message. On TCP there
//! are no message boundaries, so RFC 1831 section 10 adds a four-byte header
//! before each fragment: the top bit says whether this is the last fragment
//! of the message, the low 31 bits say how many bytes of payload follow.
//!
//! That is the whole format, and it is where a hand-rolled client usually
//! breaks, in two ways this module exists to make impossible:
//!
//! - **A `read()` is not a message.** TCP will hand back half a header, or
//!   two replies in one buffer, whenever it feels like it. Code that assumes
//!   "one read, one reply" works against a loopback server on an idle link
//!   and then fails on real hardware under load, which is the worst way for
//!   it to fail. [`RecordAssembler`] is fed whatever arrives, in whatever
//!   sizes, and hands back a message only once it has all of it.
//! - **A message may be several fragments.** Linux's client sends one
//!   fragment per message, and so does this code ([`record_mark`]), so it is
//!   tempting to assume replies come that way too. Nothing in the protocol
//!   promises it. The assembler concatenates until it sees the last-fragment
//!   bit.
//!
//! Both are held down by tests that feed the assembler one byte at a time
//! (`rpc_tests::reply_arrives_one_byte_at_a_time`) and in fragments
//! (`rpc_tests::multi_fragment_message_is_concatenated`), because a test that
//! feeds it a whole reply in one call cannot tell this implementation from
//! the broken one.
//!
//! # Credentials
//!
//! NFSv3 with `AUTH_UNIX` (RFC 1831 appendix A) is the only flavour here. It
//! is worth being clear-eyed about what it is: the client *states* its uid,
//! gid and hostname, and the server believes it. It is not authentication,
//! and a client that claims uid 0 gets uid 0 unless the export says
//! `root_squash` — which is the server's decision, not ours. Eclipse sends
//! the calling process's real credentials because that is what makes
//! permissions on the server mean anything at all, not because the server has
//! any way to check them.
//!
//! `AUTH_NULL` is still needed, and not as a fallback: the portmap and MOUNT
//! handshake uses it, and so does every call whose answer does not depend on
//! who is asking.

use alloc::vec::Vec;

use super::xdr::{XdrDecoder, XdrEncoder, XdrError, XdrResult};

/// The only RPC version this speaks (RFC 5531 section 9).
pub const RPC_VERSION: u32 = 2;

/// `CALL`, the `msg_type` of a request.
pub const MSG_TYPE_CALL: u32 = 0;
/// `REPLY`, the `msg_type` of a response.
pub const MSG_TYPE_REPLY: u32 = 1;

/// `AUTH_NULL`: no credential at all.
pub const AUTH_NULL: u32 = 0;
/// `AUTH_UNIX`: the client states its uid, gid and groups. See the module
/// docs on what that is and is not worth.
pub const AUTH_UNIX: u32 = 1;

/// The largest message this client will assemble, in bytes.
///
/// A reply's size is the server's choice and its length fields are hostile
/// (see [`super::xdr`]), so without a cap a server could name a length that
/// makes this client buffer without bound — in the *kernel's* heap, where
/// running out is not an allocation failure in one process but a dead
/// machine. One mebibyte is well above anything NFSv3 sends: the largest
/// reply is a READ, and the transfer size this client negotiates is far
/// below it.
///
/// Linux caps the same thing (`RPCSVC_MAXPAYLOAD`), for the same reason.
pub const MAX_MESSAGE_LEN: usize = 1024 * 1024;

/// The bit in a record-marking header that says "last fragment of this
/// message".
const LAST_FRAGMENT: u32 = 0x8000_0000;

/// The 31-bit mask under that bit: the fragment's payload length.
const FRAGMENT_LEN_MASK: u32 = 0x7fff_ffff;

/// How a server refused the envelope itself, before any NFS procedure ran.
///
/// These are *not* NFS errors and must not be mapped onto one: an
/// `AuthError` means the server never looked at the request, so retrying the
/// same call with the same credential cannot help, while an `ENOENT` from
/// NFS means the server looked and the file was not there. Collapsing the two
/// is what makes a client retry forever against a server that has rejected
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcRejection {
    /// `RPC_MISMATCH`: the server does not speak RPC version 2.
    VersionMismatch {
        /// Lowest RPC version the server supports.
        low: u32,
        /// Highest RPC version the server supports.
        high: u32,
    },
    /// `AUTH_ERROR`: the credential was refused, with the server's
    /// `auth_stat`.
    AuthError(u32),
    /// The program is not registered on the server (`PROG_UNAVAIL`).
    ProgramUnavailable,
    /// The program is there but not in this version range
    /// (`PROG_MISMATCH`).
    ProgramMismatch {
        /// Lowest program version the server supports.
        low: u32,
        /// Highest program version the server supports.
        high: u32,
    },
    /// The program and version are there but not this procedure
    /// (`PROC_UNAVAIL`).
    ProcedureUnavailable,
    /// The server could not decode the arguments (`GARBAGE_ARGS`). Always a
    /// bug on this side.
    GarbageArgs,
    /// `SYSTEM_ERR`: the server failed for a reason of its own.
    SystemError,
    /// An `accept_stat` or `reply_stat` this client does not know.
    Unknown(u32),
}

/// What went wrong around an RPC call, as distinct from what the NFS
/// procedure answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcError {
    /// The reply did not decode as RPC.
    Xdr(XdrError),
    /// A reply arrived whose `xid` is not the one that was sent.
    ///
    /// Worth its own variant rather than being treated as a transport error:
    /// over TCP this means the stream is out of step, and the connection has
    /// to be torn down rather than used for another call. Over UDP it would
    /// be an ordinary stale duplicate, which is one of several reasons this
    /// client is TCP-only.
    XidMismatch {
        /// The `xid` that was sent.
        expected: u32,
        /// The `xid` that came back.
        got: u32,
    },
    /// The message was a call where a reply was expected.
    NotAReply(u32),
    /// The server refused the envelope. See [`RpcRejection`].
    Rejected(RpcRejection),
    /// A record-marking header named a fragment length that would take the
    /// message over [`MAX_MESSAGE_LEN`], or a zero-length non-final
    /// fragment.
    BadFraming,
}

impl From<XdrError> for RpcError {
    fn from(e: XdrError) -> Self {
        Self::Xdr(e)
    }
}

/// A successful RPC step, or [`RpcError`].
pub type RpcResult<T> = Result<T, RpcError>;

/// The credential a call carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// `AUTH_NULL`: an empty credential, for the handshake and for calls
    /// whose answer does not depend on who asks.
    Null,
    /// `AUTH_UNIX`: the uid, gid and supplementary groups this client claims.
    Unix(AuthUnix),
}

/// The body of an `AUTH_UNIX` credential (RFC 1831 appendix A).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthUnix {
    /// A value the server may log; it has no protocol meaning.
    pub stamp: u32,
    /// The client's hostname, as the client chooses to state it.
    pub machine_name: Vec<u8>,
    /// The uid this client claims.
    pub uid: u32,
    /// The gid this client claims.
    pub gid: u32,
    /// The supplementary groups this client claims.
    ///
    /// RFC 1831 caps this at 16 and servers enforce it: a longer list is
    /// refused outright by some and silently truncated by others, so
    /// [`AuthUnix::encode`] truncates here, where the limit is visible,
    /// rather than letting the behaviour depend on which server answers.
    pub gids: Vec<u32>,
}

/// The most supplementary groups an `AUTH_UNIX` credential may carry
/// (RFC 1831 appendix A).
pub const AUTH_UNIX_MAX_GIDS: usize = 16;

impl AuthUnix {
    /// A credential for `uid`/`gid` with no supplementary groups.
    pub fn new(machine_name: &[u8], uid: u32, gid: u32) -> Self {
        Self {
            stamp: 0,
            machine_name: machine_name.to_vec(),
            uid,
            gid,
            gids: Vec::new(),
        }
    }

    /// Encode the credential body (not the `opaque_auth` wrapper around it).
    pub fn encode(&self) -> Vec<u8> {
        let mut enc = XdrEncoder::new();
        enc.u32(self.stamp);
        enc.opaque(&self.machine_name);
        enc.u32(self.uid);
        enc.u32(self.gid);
        let gids = &self.gids[..self.gids.len().min(AUTH_UNIX_MAX_GIDS)];
        enc.u32(gids.len() as u32);
        for &g in gids {
            enc.u32(g);
        }
        enc.into_bytes()
    }
}

impl Credential {
    /// The `flavor` number this credential goes on the wire as.
    pub fn flavor(&self) -> u32 {
        match self {
            Self::Null => AUTH_NULL,
            Self::Unix(_) => AUTH_UNIX,
        }
    }

    /// Write the credential as an `opaque_auth`: the flavour, then the body
    /// as a variable-length opaque.
    fn encode_into(&self, enc: &mut XdrEncoder) {
        enc.u32(self.flavor());
        match self {
            Self::Null => enc.opaque(&[]),
            Self::Unix(u) => enc.opaque(&u.encode()),
        };
    }
}

/// Which procedure of which program a call is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallTarget {
    /// The RPC program number (100003 for NFS, 100005 for MOUNT, 100000 for
    /// portmap).
    pub program: u32,
    /// The program version.
    pub version: u32,
    /// The procedure number within the program.
    pub procedure: u32,
}

impl CallTarget {
    /// A target naming `program` version `version`, procedure `procedure`.
    pub const fn new(program: u32, version: u32, procedure: u32) -> Self {
        Self {
            program,
            version,
            procedure,
        }
    }
}

/// Build a complete RPC call message — ready to be framed by
/// [`record_mark`] — from its envelope and already-encoded arguments.
///
/// The verifier is always `AUTH_NULL`: `AUTH_UNIX` has no verifier, which is
/// another way of saying it proves nothing.
pub fn encode_call(xid: u32, target: CallTarget, cred: &Credential, args: &[u8]) -> Vec<u8> {
    let mut enc = XdrEncoder::with_capacity(args.len() + 64);
    enc.u32(xid);
    enc.u32(MSG_TYPE_CALL);
    enc.u32(RPC_VERSION);
    enc.u32(target.program);
    enc.u32(target.version);
    enc.u32(target.procedure);
    cred.encode_into(&mut enc);
    // The verifier. `AUTH_UNIX` carries none, so it is always null here.
    Credential::Null.encode_into(&mut enc);
    let mut out = enc.into_bytes();
    out.extend_from_slice(args);
    out
}

/// Frame `message` as a single final RPC record (RFC 1831 section 10),
/// returning the four header bytes.
///
/// One fragment per message, as Linux's client does. A caller writes these
/// four bytes and then the message.
pub fn record_mark(message_len: usize) -> [u8; 4] {
    debug_assert!(message_len as u32 & LAST_FRAGMENT == 0);
    (LAST_FRAGMENT | (message_len as u32 & FRAGMENT_LEN_MASK)).to_be_bytes()
}

/// Frame `message` as a single final record, header and payload in one
/// buffer.
pub fn frame_message(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + 4);
    out.extend_from_slice(&record_mark(message.len()));
    out.extend_from_slice(message);
    out
}

/// Reassembles RPC messages out of a TCP byte stream.
///
/// Fed whatever `read()` returned, in whatever sizes — a single byte, half a
/// header, three replies at once — and hands back one complete message at a
/// time. See the module docs for why this cannot be skipped.
#[derive(Debug, Default)]
pub struct RecordAssembler {
    /// Bytes received and not yet consumed as a message.
    pending: Vec<u8>,
    /// Payload of the fragments of the message being assembled, in order.
    message: Vec<u8>,
}

impl RecordAssembler {
    /// An assembler with nothing buffered.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add bytes that arrived from the connection.
    pub fn feed(&mut self, data: &[u8]) {
        self.pending.extend_from_slice(data);
    }

    /// How many bytes are buffered but not yet part of a complete message.
    ///
    /// For tests and for the "connection torn down mid-message" check: bytes
    /// left here when the peer closes mean a truncated reply, which must not
    /// be reported as a clean end of stream.
    pub fn buffered(&self) -> usize {
        self.pending.len() + self.message.len()
    }

    /// Whether a message is part-assembled.
    ///
    /// A connection that closes while this is true closed mid-message.
    pub fn in_progress(&self) -> bool {
        self.buffered() != 0
    }

    /// Take the next complete message, or `None` if more bytes are needed.
    ///
    /// Call it in a loop after each [`Self::feed`]: one `feed` can complete
    /// more than one message, and a message left in the buffer while the
    /// caller waits for more bytes is how a client deadlocks against a
    /// server that has already answered.
    pub fn next_message(&mut self) -> RpcResult<Option<Vec<u8>>> {
        loop {
            if self.pending.len() < 4 {
                return Ok(None);
            }
            let header = u32::from_be_bytes([
                self.pending[0],
                self.pending[1],
                self.pending[2],
                self.pending[3],
            ]);
            let last = header & LAST_FRAGMENT != 0;
            let len = (header & FRAGMENT_LEN_MASK) as usize;

            // A zero-length fragment that is not the last one advances
            // nothing: a server that sent them forever would spin this loop
            // without the buffer ever growing or shrinking. A zero-length
            // FINAL fragment is legal (it ends a message), so only the
            // non-final case is refused.
            if len == 0 && !last {
                return Err(RpcError::BadFraming);
            }
            // Checked before anything is copied, so a server cannot make this
            // buffer without bound. `checked_add` because `len` comes off the
            // wire.
            if self
                .message
                .len()
                .checked_add(len)
                .is_none_or(|total| total > MAX_MESSAGE_LEN)
            {
                return Err(RpcError::BadFraming);
            }
            if self.pending.len() < 4 + len {
                // The fragment's payload has not all arrived. Leave the
                // header in place: re-reading it next time is free, and
                // removing it would mean remembering it somewhere else.
                return Ok(None);
            }

            self.message.extend_from_slice(&self.pending[4..4 + len]);
            self.pending.drain(..4 + len);

            if last {
                return Ok(Some(core::mem::take(&mut self.message)));
            }
        }
    }
}

/// An accepted, successful reply: the `xid` it answers and the bytes of the
/// procedure's results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyBody<'a> {
    /// The transaction id the reply answers.
    pub xid: u32,
    /// The procedure's encoded results, positioned past the RPC envelope.
    pub results: &'a [u8],
}

/// Parse an RPC reply envelope, checking it answers `expected_xid`.
///
/// On success the returned [`ReplyBody`] borrows the results out of `message`:
/// the envelope has been verified and what follows is the procedure's own
/// encoding, which only the caller knows the shape of.
pub fn decode_reply(message: &[u8], expected_xid: u32) -> RpcResult<ReplyBody<'_>> {
    let mut dec = XdrDecoder::new(message);
    let xid = dec.u32()?;
    if xid != expected_xid {
        return Err(RpcError::XidMismatch {
            expected: expected_xid,
            got: xid,
        });
    }
    let msg_type = dec.u32()?;
    if msg_type != MSG_TYPE_REPLY {
        return Err(RpcError::NotAReply(msg_type));
    }
    // reply_stat: MSG_ACCEPTED (0) or MSG_DENIED (1).
    match dec.u32()? {
        0 => {}
        1 => return Err(RpcError::Rejected(decode_rejection(&mut dec)?)),
        other => return Err(RpcError::Rejected(RpcRejection::Unknown(other))),
    }
    // The server's verifier, which `AUTH_NULL` and `AUTH_UNIX` leave empty.
    // Read past it rather than assuming its size: `AUTH_SYS` servers that
    // answer with a flavour of their own still frame it as an `opaque_auth`.
    let _flavor = dec.u32()?;
    let _body = dec.opaque()?;
    // accept_stat.
    match dec.u32()? {
        0 => Ok(ReplyBody {
            xid,
            results: dec.rest(),
        }),
        1 => {
            let low = dec.u32()?;
            let high = dec.u32()?;
            Err(RpcError::Rejected(RpcRejection::ProgramMismatch {
                low,
                high,
            }))
        }
        2 => Err(RpcError::Rejected(RpcRejection::ProgramUnavailable)),
        3 => Err(RpcError::Rejected(RpcRejection::ProcedureUnavailable)),
        4 => Err(RpcError::Rejected(RpcRejection::GarbageArgs)),
        5 => Err(RpcError::Rejected(RpcRejection::SystemError)),
        other => Err(RpcError::Rejected(RpcRejection::Unknown(other))),
    }
}

/// The body of a `MSG_DENIED` reply, positioned just past the `reply_stat`.
fn decode_rejection(dec: &mut XdrDecoder<'_>) -> XdrResult<RpcRejection> {
    // reject_stat: RPC_MISMATCH (0) or AUTH_ERROR (1).
    match dec.u32()? {
        0 => {
            let low = dec.u32()?;
            let high = dec.u32()?;
            Ok(RpcRejection::VersionMismatch { low, high })
        }
        1 => Ok(RpcRejection::AuthError(dec.u32()?)),
        other => Ok(RpcRejection::Unknown(other)),
    }
}

#[cfg(test)]
#[path = "rpc_tests.rs"]
mod rpc_tests;
