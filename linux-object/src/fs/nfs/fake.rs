//! The scripted NFSv3 server the session's and the filesystem's tests share.
//!
//! Not a stub that returns a canned blob: it reads the procedure number and
//! the arguments out of each call that was sent, so a test can assert on
//! *what was asked*, which is where the loops and the caches go wrong. A
//! `READ` loop that re-reads offset zero forever, a `READDIR` loop that
//! re-sends a cookie, a `get_entry` that re-lists the directory once per
//! index -- each of those is a correct-looking result built from wrong calls,
//! and only the calls show it.
//!
//! Shared rather than copied because a fake that drifts between two suites
//! measures two different servers: the copy that was not updated keeps
//! passing tests about a protocol nobody speaks.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use super::client::{RpcTransport, TransportError};
use super::mount3::FileHandle;
use super::nfs3::Ftype3;
use super::nfs3_dir::COOKIEVERF_SIZE;
use super::rpc::{frame_message, MSG_TYPE_REPLY};
use super::session::{NfsSession, JUKEBOX_RETRIES};
use super::source::NfsOptions;
use super::xdr::{XdrDecoder, XdrEncoder};

/// One call the session made, as the server saw it.
#[derive(Debug, Clone)]
pub(crate) struct Call {
    pub(crate) xid: u32,
    pub(crate) procedure: u32,
    pub(crate) args: Vec<u8>,
}

/// A server the test writes the answers for.
///
/// Answers are consumed in order, one per call. Running out is deliberate:
/// a loop that makes one call more than the test scripted gets no reply,
/// times out, and fails — rather than being handed the previous answer again
/// and looping on it.
#[derive(Debug)]
pub(crate) struct Server {
    pub(crate) answers: VecDeque<Vec<u8>>,
    pub(crate) calls: Vec<Call>,
    pub(crate) pending: VecDeque<Vec<u8>>,
    pub(crate) now: u64,
    pub(crate) ms_per_poll: u64,
    /// How many of the next calls get no answer at all.
    pub(crate) silent: usize,
    /// Whether the next call is answered with `MSG_DENIED`/`AUTH_ERROR`.
    pub(crate) deny_next: bool,
}

/// The most calls this server will take before refusing.
///
/// A bound and not a behaviour, for the reason the RPC client's fake has one:
/// an unbounded loop would otherwise grow [`Server::calls`] until the test
/// process is killed for its memory, and a killed process names no test.
pub(crate) const MAX_CALLS: usize = 100_000;

impl Server {
    pub(crate) fn new() -> Self {
        Self {
            answers: VecDeque::new(),
            calls: Vec::new(),
            pending: VecDeque::new(),
            now: 0,
            // Enough that a 60 s timeout is reachable in a few thousand
            // polls rather than a few million.
            ms_per_poll: 50,
            silent: 0,
            deny_next: false,
        }
    }

    /// Queue one procedure result, to be wrapped in a successful RPC reply.
    pub(crate) fn answer(&mut self, results: Vec<u8>) -> &mut Self {
        self.answers.push_back(results);
        self
    }

    /// Queue `n` copies of one answer.
    pub(crate) fn answer_times(&mut self, n: usize, results: Vec<u8>) -> &mut Self {
        for _ in 0..n {
            self.answers.push_back(results.clone());
        }
        self
    }
}

impl RpcTransport for Server {
    fn send(&mut self, data: &[u8]) -> Result<(), TransportError> {
        if self.calls.len() >= MAX_CALLS {
            return Err(TransportError::Other);
        }
        // Strip the record header and read the call's envelope.
        let body = &data[4..];
        let mut dec = XdrDecoder::new(body);
        let xid = dec.u32().expect("xid");
        let _msg_type = dec.u32().expect("msg type");
        let _rpcvers = dec.u32().expect("rpc version");
        let _prog = dec.u32().expect("program");
        let _vers = dec.u32().expect("version");
        let procedure = dec.u32().expect("procedure");
        // The credential and the verifier, each a flavour and an opaque.
        for _ in 0..2 {
            let _flavor = dec.u32().expect("auth flavour");
            dec.opaque().expect("auth body");
        }
        let args = dec.rest().to_vec();
        self.calls.push(Call {
            xid,
            procedure,
            args,
        });
        if self.silent > 0 {
            self.silent -= 1;
            return Ok(());
        }
        if self.deny_next {
            self.deny_next = false;
            self.pending.push_back(denied_reply(xid));
            return Ok(());
        }
        if let Some(results) = self.answers.pop_front() {
            self.pending.push_back(reply(xid, &results));
        }
        Ok(())
    }

    fn recv(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        self.now = self.now.wrapping_add(self.ms_per_poll);
        match self.pending.pop_front() {
            Some(chunk) => {
                let n = chunk.len().min(buf.len());
                buf[..n].copy_from_slice(&chunk[..n]);
                if n < chunk.len() {
                    self.pending.push_front(chunk[n..].to_vec());
                }
                Ok(n)
            }
            None => Ok(0),
        }
    }

    fn now_ms(&self) -> u64 {
        self.now
    }

    fn poll(&mut self) {
        self.now = self.now.wrapping_add(self.ms_per_poll);
    }
}

/// A reply refusing `xid`'s credential: `MSG_DENIED` with `AUTH_BADCRED`.
pub(crate) fn denied_reply(xid: u32) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(xid);
    enc.u32(MSG_TYPE_REPLY);
    enc.u32(1); // MSG_DENIED
    enc.u32(1); // AUTH_ERROR
    enc.u32(1); // AUTH_BADCRED
    frame_message(&enc.into_bytes())
}

/// An accepted, successful RPC reply to `xid` carrying `results`.
pub(crate) fn reply(xid: u32, results: &[u8]) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(xid);
    enc.u32(MSG_TYPE_REPLY);
    enc.u32(0); // MSG_ACCEPTED
    enc.u32(0); // verifier flavour AUTH_NULL
    enc.opaque(&[]);
    enc.u32(0); // SUCCESS
    let mut out = enc.into_bytes();
    out.extend_from_slice(results);
    frame_message(&out)
}

// ---------------------------------------------------------------- fixtures

pub(crate) fn a_handle() -> FileHandle {
    FileHandle::new(&[1, 2, 3, 4]).unwrap()
}

/// Options with short timeouts: a test that means to reach a timeout should
/// reach it in a few thousand polls, not a few million.
pub(crate) fn quick_options() -> NfsOptions {
    NfsOptions {
        timeout_ms: 500,
        retrans: 0,
        rsize: 1024,
        wsize: 1024,
        ..NfsOptions::default()
    }
}

pub(crate) fn session(server: Server) -> NfsSession<Server> {
    let mut s = NfsSession::new(server, 1, a_handle(), &quick_options());
    // No pause between jukebox retries: the pause is tested on its own, and
    // every other test would otherwise spend it.
    s.set_jukebox_policy(JUKEBOX_RETRIES, 0);
    s
}

pub(crate) fn put_time(enc: &mut XdrEncoder, sec: u32, nsec: u32) {
    enc.u32(sec);
    enc.u32(nsec);
}

/// A `fattr3` of `size` bytes, as a file.
pub(crate) fn put_fattr3(enc: &mut XdrEncoder, ftype: Ftype3, size: u64, fileid: u64) {
    enc.u32(match ftype {
        Ftype3::Reg => 1,
        Ftype3::Dir => 2,
        Ftype3::Blk => 3,
        Ftype3::Chr => 4,
        Ftype3::Lnk => 5,
        Ftype3::Sock => 6,
        Ftype3::Fifo => 7,
    });
    enc.u32(0o644); // mode
    enc.u32(1); // nlink
    enc.u32(1000); // uid
    enc.u32(1001); // gid
    enc.u64(size);
    enc.u64(size); // used
    enc.u32(7); // rdev major
    enc.u32(8); // rdev minor
    enc.u64(0x5555); // fsid
    enc.u64(fileid);
    put_time(enc, 100, 1);
    put_time(enc, 200, 2);
    put_time(enc, 300, 3);
}

/// A `post_op_attr` that is present.
pub(crate) fn put_post_op(enc: &mut XdrEncoder, size: u64) {
    enc.u32(1);
    put_fattr3(enc, Ftype3::Reg, size, 42);
}

/// A `post_op_attr` the server declined to send.
pub(crate) fn put_no_post_op(enc: &mut XdrEncoder) {
    enc.u32(0);
}

/// A `wcc_data` with neither half present, which is the legal short form.
pub(crate) fn put_empty_wcc(enc: &mut XdrEncoder) {
    enc.u32(0);
    enc.u32(0);
}

/// `GETATTR` answering `size`.
pub(crate) fn getattr_ok(size: u64) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_fattr3(&mut enc, Ftype3::Reg, size, 42);
    enc.into_bytes()
}

/// A failure carrying only a status and a `post_op_attr`.
pub(crate) fn failure(status: u32) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(status);
    put_no_post_op(&mut enc);
    enc.into_bytes()
}

/// `READ` answering `data`, with `eof` as given.
pub(crate) fn read_ok(data: &[u8], eof: bool) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_post_op(&mut enc, 4096);
    enc.u32(data.len() as u32);
    enc.u32(u32::from(eof));
    enc.opaque(data);
    enc.into_bytes()
}

/// `WRITE` reporting `count` bytes taken at `committed` durability.
pub(crate) fn write_ok(count: u32, committed: u32) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_empty_wcc(&mut enc);
    enc.u32(count);
    enc.u32(committed);
    enc.opaque_fixed(&[9u8; 8]);
    enc.into_bytes()
}

/// `FSINFO` with the limits given and a `dtpref` of `dtpref`.
pub(crate) fn fsinfo_ok(rtmax: u32, rtpref: u32, rtmult: u32, wtmax: u32, dtpref: u32) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_no_post_op(&mut enc);
    enc.u32(rtmax);
    enc.u32(rtpref);
    enc.u32(rtmult);
    enc.u32(wtmax);
    enc.u32(wtmax); // wtpref
    enc.u32(1); // wtmult
    enc.u32(dtpref);
    enc.u64(1 << 40); // maxfilesize
    put_time(&mut enc, 0, 1);
    enc.u32(0); // properties
    enc.into_bytes()
}

/// One `READDIR` entry in the chain.
pub(crate) fn put_entry(enc: &mut XdrEncoder, fileid: u64, name: &str, cookie: u64) {
    enc.u32(1); // an entry follows
    enc.u64(fileid);
    enc.string(name);
    enc.u64(cookie);
}

/// A `READDIR` reply holding `entries`, ending with `eof`.
pub(crate) fn readdir_ok(
    entries: &[(u64, &str, u64)],
    eof: bool,
    verf: [u8; COOKIEVERF_SIZE],
) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_no_post_op(&mut enc);
    enc.opaque_fixed(&verf);
    for (fileid, name, cookie) in entries {
        put_entry(&mut enc, *fileid, name, *cookie);
    }
    enc.u32(0); // no more entries
    enc.u32(u32::from(eof));
    enc.into_bytes()
}

/// The `READ` arguments of the n-th call: the offset and the count.
pub(crate) fn read_args(server: &Server, n: usize) -> (u64, u32) {
    let mut dec = XdrDecoder::new(&server.calls[n].args);
    dec.opaque().expect("handle");
    (dec.u64().expect("offset"), dec.u32().expect("count"))
}

/// The `WRITE` arguments of the n-th call: the offset and the data.
pub(crate) fn write_args(server: &Server, n: usize) -> (u64, Vec<u8>) {
    let mut dec = XdrDecoder::new(&server.calls[n].args);
    dec.opaque().expect("handle");
    let offset = dec.u64().expect("offset");
    let _count = dec.u32().expect("count");
    let _stable = dec.u32().expect("stable");
    let data = dec.opaque().expect("data").to_vec();
    (offset, data)
}

/// The `READDIR` arguments of the n-th call: the cookie, the verifier, the
/// count.
pub(crate) fn readdir_args(server: &Server, n: usize) -> (u64, Vec<u8>, u32) {
    let mut dec = XdrDecoder::new(&server.calls[n].args);
    dec.opaque().expect("handle");
    let cookie = dec.u64().expect("cookie");
    let verf = dec
        .opaque_fixed(COOKIEVERF_SIZE)
        .expect("verifier")
        .to_vec();
    (cookie, verf, dec.u32().expect("count"))
}

// ------------------------------------------- the replies the VFS layer needs

/// `GETATTR` answering a node of `ftype`.
pub(crate) fn getattr_of(ftype: Ftype3, size: u64, fileid: u64) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_fattr3(&mut enc, ftype, size, fileid);
    enc.into_bytes()
}

/// `LOOKUP` answering `fh`, with the entry's attributes and no directory
/// attributes.
pub(crate) fn lookup_ok(fh: &[u8], ftype: Ftype3, size: u64, fileid: u64) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    enc.opaque(fh);
    enc.u32(1);
    put_fattr3(&mut enc, ftype, size, fileid);
    put_no_post_op(&mut enc);
    enc.into_bytes()
}

/// `READLINK` answering `target`.
pub(crate) fn readlink_ok(target: &[u8]) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_no_post_op(&mut enc);
    enc.opaque(target);
    enc.into_bytes()
}

/// A reply that is only a `wcc_data`, which is what `REMOVE`, `RMDIR` and
/// `RENAME` answer with. `RENAME` carries two, so `wccs` says how many.
pub(crate) fn wcc_only(wccs: usize) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    for _ in 0..wccs {
        put_empty_wcc(&mut enc);
    }
    enc.into_bytes()
}

/// `SETATTR` answering a `wcc_data` whose `after` is present.
pub(crate) fn setattr_ok(size: u64) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    enc.u32(0); // no `before`
    put_post_op(&mut enc, size);
    enc.into_bytes()
}

/// One of the four `diropok3` replies: `CREATE`, `MKDIR`, `SYMLINK`, `MKNOD`.
pub(crate) fn created(fh: Option<&[u8]>, ftype: Ftype3, fileid: u64) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    match fh {
        Some(bytes) => {
            enc.u32(1);
            enc.opaque(bytes);
        }
        None => {
            enc.u32(0);
        }
    }
    enc.u32(1);
    put_fattr3(&mut enc, ftype, 0, fileid);
    put_empty_wcc(&mut enc);
    enc.into_bytes()
}

/// `LINK`: the file's attributes and the directory's `wcc_data`.
pub(crate) fn link_ok() -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_post_op(&mut enc, 0);
    put_empty_wcc(&mut enc);
    enc.into_bytes()
}

/// One `READDIRPLUS` entry: a name, a cookie, and what the server chose to
/// attach to it.
pub(crate) struct PlusEntry<'a> {
    pub(crate) fileid: u64,
    pub(crate) name: &'a str,
    pub(crate) cookie: u64,
    /// The type, when the server sent attributes. `None` is an entry sent
    /// bare, which the protocol allows and which the caller then has to look
    /// up for itself.
    pub(crate) ftype: Option<Ftype3>,
    /// The handle, when the server sent one.
    pub(crate) fh: Option<&'a [u8]>,
}

/// A `READDIRPLUS` reply holding `entries`, ending with `eof`.
pub(crate) fn readdirplus_ok(entries: &[PlusEntry<'_>], eof: bool) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_no_post_op(&mut enc);
    enc.opaque_fixed(&[0u8; COOKIEVERF_SIZE]);
    for e in entries {
        enc.u32(1); // an entry follows
        enc.u64(e.fileid);
        enc.string(e.name);
        enc.u64(e.cookie);
        match e.ftype {
            Some(ftype) => {
                enc.u32(1);
                put_fattr3(&mut enc, ftype, 0, e.fileid);
            }
            None => {
                enc.u32(0);
            }
        }
        match e.fh {
            Some(bytes) => {
                enc.u32(1);
                enc.opaque(bytes);
            }
            None => {
                enc.u32(0);
            }
        }
    }
    enc.u32(0); // no more entries
    enc.u32(u32::from(eof));
    enc.into_bytes()
}

/// `FSSTAT` with the byte and file counters given.
pub(crate) fn fsstat_ok(
    tbytes: u64,
    fbytes: u64,
    abytes: u64,
    tfiles: u64,
    ffiles: u64,
) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_no_post_op(&mut enc);
    enc.u64(tbytes);
    enc.u64(fbytes);
    enc.u64(abytes);
    enc.u64(tfiles);
    enc.u64(ffiles);
    enc.u64(0); // afiles
    enc.u32(0); // invarsec
    enc.into_bytes()
}

/// A session over `server` mounted `ro`, for the checks that have to happen
/// before anything reaches the wire.
pub(crate) fn read_only_session(server: Server) -> NfsSession<Server> {
    let opts = NfsOptions {
        read_only: true,
        ..quick_options()
    };
    let mut s = NfsSession::new(server, 1, a_handle(), &opts);
    s.set_jukebox_policy(JUKEBOX_RETRIES, 0);
    s
}
