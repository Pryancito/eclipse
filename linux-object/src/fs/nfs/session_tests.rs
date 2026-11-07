//! The session, against a scripted server.
//!
//! The server here is not a stub that returns a canned blob: it reads the
//! procedure number and the arguments out of each call the session sent, so a
//! test can assert on *what was asked*, which is where the loops go wrong. A
//! `READ` loop that re-reads offset zero forever, a `WRITE` loop that sends
//! the same chunk twice, a `READDIR` loop that re-sends a cookie — each of
//! those is a correct-looking result built from wrong calls, and only the
//! calls show it.

use super::*;
use crate::fs::nfs::nfs3::{Ftype3, PreOpAttr};
use crate::fs::nfs::nfs3_dir::COOKIEVERF_SIZE;
use crate::fs::nfs::rpc::{frame_message, MSG_TYPE_REPLY};
use crate::fs::nfs::xdr::{XdrDecoder, XdrEncoder};
use alloc::collections::VecDeque;
use alloc::vec;
use rcore_fs::vfs::Timespec;

/// One call the session made, as the server saw it.
#[derive(Debug, Clone)]
struct Call {
    xid: u32,
    procedure: u32,
    args: Vec<u8>,
}

/// A server the test writes the answers for.
///
/// Answers are consumed in order, one per call. Running out is deliberate:
/// a loop that makes one call more than the test scripted gets no reply,
/// times out, and fails — rather than being handed the previous answer again
/// and looping on it.
#[derive(Debug)]
struct Server {
    answers: VecDeque<Vec<u8>>,
    calls: Vec<Call>,
    pending: VecDeque<Vec<u8>>,
    now: u64,
    ms_per_poll: u64,
    /// How many of the next calls get no answer at all.
    silent: usize,
    /// Whether the next call is answered with `MSG_DENIED`/`AUTH_ERROR`.
    deny_next: bool,
}

/// The most calls this server will take before refusing.
///
/// A bound and not a behaviour, for the reason the RPC client's fake has one:
/// an unbounded loop would otherwise grow [`Server::calls`] until the test
/// process is killed for its memory, and a killed process names no test.
const MAX_CALLS: usize = 100_000;

impl Server {
    fn new() -> Self {
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
    fn answer(&mut self, results: Vec<u8>) -> &mut Self {
        self.answers.push_back(results);
        self
    }

    /// Queue `n` copies of one answer.
    fn answer_times(&mut self, n: usize, results: Vec<u8>) -> &mut Self {
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
fn denied_reply(xid: u32) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(xid);
    enc.u32(MSG_TYPE_REPLY);
    enc.u32(1); // MSG_DENIED
    enc.u32(1); // AUTH_ERROR
    enc.u32(1); // AUTH_BADCRED
    frame_message(&enc.into_bytes())
}

/// An accepted, successful RPC reply to `xid` carrying `results`.
fn reply(xid: u32, results: &[u8]) -> Vec<u8> {
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

fn a_handle() -> FileHandle {
    FileHandle::new(&[1, 2, 3, 4]).unwrap()
}

/// Options with short timeouts: a test that means to reach a timeout should
/// reach it in a few thousand polls, not a few million.
fn quick_options() -> NfsOptions {
    NfsOptions {
        timeout_ms: 500,
        retrans: 0,
        rsize: 1024,
        wsize: 1024,
        ..NfsOptions::default()
    }
}

fn session(server: Server) -> NfsSession<Server> {
    let mut s = NfsSession::new(server, 1, a_handle(), &quick_options());
    // No pause between jukebox retries: the pause is tested on its own, and
    // every other test would otherwise spend it.
    s.set_jukebox_policy(JUKEBOX_RETRIES, 0);
    s
}

fn put_time(enc: &mut XdrEncoder, sec: u32, nsec: u32) {
    enc.u32(sec);
    enc.u32(nsec);
}

/// A `fattr3` of `size` bytes, as a file.
fn put_fattr3(enc: &mut XdrEncoder, ftype: Ftype3, size: u64, fileid: u64) {
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
fn put_post_op(enc: &mut XdrEncoder, size: u64) {
    enc.u32(1);
    put_fattr3(enc, Ftype3::Reg, size, 42);
}

/// A `post_op_attr` the server declined to send.
fn put_no_post_op(enc: &mut XdrEncoder) {
    enc.u32(0);
}

/// A `wcc_data` with neither half present, which is the legal short form.
fn put_empty_wcc(enc: &mut XdrEncoder) {
    enc.u32(0);
    enc.u32(0);
}

/// `GETATTR` answering `size`.
fn getattr_ok(size: u64) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_fattr3(&mut enc, Ftype3::Reg, size, 42);
    enc.into_bytes()
}

/// A failure carrying only a status and a `post_op_attr`.
fn failure(status: u32) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(status);
    put_no_post_op(&mut enc);
    enc.into_bytes()
}

/// `READ` answering `data`, with `eof` as given.
fn read_ok(data: &[u8], eof: bool) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_post_op(&mut enc, 4096);
    enc.u32(data.len() as u32);
    enc.u32(u32::from(eof));
    enc.opaque(data);
    enc.into_bytes()
}

/// `WRITE` reporting `count` bytes taken at `committed` durability.
fn write_ok(count: u32, committed: u32) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_empty_wcc(&mut enc);
    enc.u32(count);
    enc.u32(committed);
    enc.opaque_fixed(&[9u8; 8]);
    enc.into_bytes()
}

/// `FSINFO` with the limits given and a `dtpref` of `dtpref`.
fn fsinfo_ok(rtmax: u32, rtpref: u32, rtmult: u32, wtmax: u32, dtpref: u32) -> Vec<u8> {
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
fn put_entry(enc: &mut XdrEncoder, fileid: u64, name: &str, cookie: u64) {
    enc.u32(1); // an entry follows
    enc.u64(fileid);
    enc.string(name);
    enc.u64(cookie);
}

/// A `READDIR` reply holding `entries`, ending with `eof`.
fn readdir_ok(entries: &[(u64, &str, u64)], eof: bool, verf: [u8; COOKIEVERF_SIZE]) -> Vec<u8> {
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
fn read_args(server: &Server, n: usize) -> (u64, u32) {
    let mut dec = XdrDecoder::new(&server.calls[n].args);
    dec.opaque().expect("handle");
    (dec.u64().expect("offset"), dec.u32().expect("count"))
}

/// The `WRITE` arguments of the n-th call: the offset and the data.
fn write_args(server: &Server, n: usize) -> (u64, Vec<u8>) {
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
fn readdir_args(server: &Server, n: usize) -> (u64, Vec<u8>, u32) {
    let mut dec = XdrDecoder::new(&server.calls[n].args);
    dec.opaque().expect("handle");
    let cookie = dec.u64().expect("cookie");
    let verf = dec
        .opaque_fixed(COOKIEVERF_SIZE)
        .expect("verifier")
        .to_vec();
    (cookie, verf, dec.u32().expect("count"))
}

// ------------------------------------------------------------- plain calls

#[test]
fn a_getattr_comes_back_as_the_attributes_the_server_sent() {
    let mut server = Server::new();
    server.answer(getattr_ok(1234));
    let mut s = session(server);
    let attr = s.getattr(&a_handle()).unwrap();
    assert_eq!(attr.size, 1234);
    assert_eq!(attr.ftype, Ftype3::Reg);
    assert_eq!(s.transport().calls[0].procedure, nfs3::PROC_GETATTR);
}

#[test]
fn a_status_the_server_returns_becomes_the_errno_for_it() {
    // NFS3ERR_ACCES is 13, and the errno a caller sees is EACCES. The point
    // is that it goes through the status type and not through a cast: see
    // the next test for the value where a cast is wrong.
    let mut server = Server::new();
    server.answer(failure(13));
    let mut s = session(server);
    assert_eq!(s.lookup(&a_handle(), "x"), Err(LxError::EACCES));
}

#[test]
fn a_status_above_ten_thousand_does_not_leave_as_an_errno() {
    // NFS3ERR_NOTSUPP is 10004. Cast, it would reach userspace as error
    // number 10004, which names nothing.
    let mut server = Server::new();
    server.answer(failure(10_004));
    let mut s = session(server);
    let got = s.lookup(&a_handle(), "x");
    assert_eq!(got, Err(LxError::EOPNOTSUPP));
    assert_ne!(got, Err(LxError::EUNDEF));
}

#[test]
fn a_stale_handle_is_estale_and_not_a_generic_io_error() {
    // The distinctive NFS error: the handle was valid and what it named is
    // gone. Reported as EIO it looks like a broken disk; as ESTALE a caller
    // knows to look the file up again.
    let mut server = Server::new();
    server.answer(failure(70));
    let mut s = session(server);
    assert_eq!(s.getattr(&a_handle()), Err(LxError::ESTALE));
}

#[test]
fn a_name_with_a_slash_in_it_never_reaches_the_wire() {
    let mut server = Server::new();
    server.answer(getattr_ok(1));
    let mut s = session(server);
    assert_eq!(s.lookup(&a_handle(), "a/b"), Err(LxError::EINVAL));
    assert!(
        s.transport().calls.is_empty(),
        "a name that is not one component was sent to the server anyway"
    );
}

#[test]
fn a_readlink_target_may_hold_a_slash() {
    // The one place the one-component rule does not apply: a link target is
    // a path, not a `filename3`.
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_no_post_op(&mut enc);
    enc.opaque(b"../elsewhere/file");
    let mut server = Server::new();
    server.answer(enc.into_bytes());
    let mut s = session(server);
    assert_eq!(s.readlink(&a_handle()).unwrap(), "../elsewhere/file");
}

#[test]
fn an_access_reply_is_the_subset_the_server_allows() {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_no_post_op(&mut enc);
    enc.u32(0x01); // READ only, of the three that were asked for
    let mut server = Server::new();
    server.answer(enc.into_bytes());
    let mut s = session(server);
    assert_eq!(s.access(&a_handle(), 0x07).unwrap(), 0x01);
}

// ------------------------------------------------------------ the jukebox

#[test]
fn a_jukebox_status_is_retried_and_not_reported() {
    // NFS3ERR_JUKEBOX is 10008: "the file is not to hand, ask again". A
    // client that reported it turns a file on cold storage into an error.
    let mut server = Server::new();
    server.answer(failure(10_008));
    server.answer(getattr_ok(77));
    let mut s = session(server);
    assert_eq!(s.getattr(&a_handle()).unwrap().size, 77);
    assert_eq!(s.transport().calls.len(), 2, "the call was not retried");
}

#[test]
fn a_jukebox_retry_sends_a_new_transaction_id() {
    // Unlike the RPC client's retransmit, which reuses the id so the
    // server's reply cache matches it, this is a *new call*: the previous one
    // was answered. Reusing the id would get the cached "jukebox" back.
    let mut server = Server::new();
    server.answer(failure(10_008));
    server.answer(getattr_ok(1));
    let mut s = session(server);
    s.getattr(&a_handle()).unwrap();
    let calls = &s.transport().calls;
    assert_ne!(calls[0].xid, calls[1].xid);
}

#[test]
fn a_jukebox_that_never_clears_runs_out_of_retries_and_is_reported() {
    let mut server = Server::new();
    server.answer_times(JUKEBOX_RETRIES as usize + 1, failure(10_008));
    let mut s = session(server);
    assert_eq!(s.getattr(&a_handle()), Err(LxError::EAGAIN));
    assert_eq!(
        s.transport().calls.len(),
        JUKEBOX_RETRIES as usize + 1,
        "the retry budget was not respected"
    );
}

#[test]
fn the_jukebox_pause_waits_on_the_clock_and_does_not_spin_forever() {
    let mut server = Server::new();
    server.answer(failure(10_008));
    server.answer(getattr_ok(1));
    let mut s = NfsSession::new(server, 1, a_handle(), &quick_options());
    s.set_jukebox_policy(2, 300);
    let before = s.transport().now_ms();
    s.getattr(&a_handle()).unwrap();
    assert!(
        s.transport().now_ms() - before >= 300,
        "the retry did not wait"
    );
}

#[test]
fn the_jukebox_pause_gives_up_on_a_clock_that_has_stopped() {
    // The pause's other exit, and the one the clock cannot provide. This
    // loop runs on the thread that drives the network stack, so a stalled
    // timer must not hold it: `now_ms() < until` is false forever and the
    // pass bound is the only way back. Without it this test does not fail,
    // it *hangs* -- which is the measurement.
    let mut server = Server::new();
    server.ms_per_poll = 0;
    server.answer_times(JUKEBOX_RETRIES as usize + 1, failure(10_008));
    let mut s = NfsSession::new(server, 1, a_handle(), &quick_options());
    s.set_jukebox_policy(JUKEBOX_RETRIES, JUKEBOX_PAUSE_MS);
    assert_eq!(s.getattr(&a_handle()), Err(LxError::EAGAIN));
    assert_eq!(
        s.transport().now_ms(),
        0,
        "the clock was supposed to be the frozen one"
    );
    assert_eq!(
        s.transport().calls.len(),
        JUKEBOX_RETRIES as usize + 1,
        "a bounded pause must return into the retry loop, not past it"
    );
}

// --------------------------------------------------------------- the read

#[test]
fn a_short_read_is_not_the_end_of_the_file() {
    // The trap. A server may answer with fewer bytes than asked for whenever
    // it likes, and only `eof` ends a file. A loop that stopped here would
    // truncate the file silently.
    let mut server = Server::new();
    server.answer(read_ok(b"abc", false));
    server.answer(read_ok(b"de", true));
    let mut s = session(server);
    let mut buf = [0u8; 8];
    let n = s.read_into(&a_handle(), 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"abcde");
}

#[test]
fn each_read_asks_from_where_the_last_one_ended() {
    let mut server = Server::new();
    server.answer(read_ok(b"abc", false));
    server.answer(read_ok(b"de", true));
    let mut s = session(server);
    let mut buf = [0u8; 8];
    s.read_into(&a_handle(), 100, &mut buf).unwrap();
    let srv = s.transport();
    assert_eq!(read_args(srv, 0), (100, 8));
    assert_eq!(
        read_args(srv, 1),
        (103, 5),
        "the second read did not advance by what the first returned"
    );
}

#[test]
fn a_read_stops_when_the_buffer_is_full_even_without_eof() {
    let mut server = Server::new();
    server.answer(read_ok(b"abcd", false));
    let mut s = session(server);
    let mut buf = [0u8; 4];
    assert_eq!(s.read_into(&a_handle(), 0, &mut buf).unwrap(), 4);
    assert_eq!(
        s.transport().calls.len(),
        1,
        "a full buffer did not end the loop"
    );
}

#[test]
fn a_read_of_nothing_without_eof_is_an_error_and_not_another_turn() {
    // A server that answers every read with zero bytes and `eof` false would
    // spin this loop forever, and in the kernel that loop holds the thread
    // driving the network stack.
    let mut server = Server::new();
    server.answer_times(4, read_ok(b"", false));
    let mut s = session(server);
    let mut buf = [0u8; 16];
    assert_eq!(s.read_into(&a_handle(), 0, &mut buf), Err(LxError::EIO));
    assert_eq!(s.transport().calls.len(), 1, "the loop went around again");
}

#[test]
fn a_read_is_capped_at_the_negotiated_size() {
    let mut server = Server::new();
    server.answer(read_ok(&[0u8; 1024], true));
    let mut s = session(server);
    let mut buf = [0u8; 4096];
    s.read_into(&a_handle(), 0, &mut buf).unwrap();
    assert_eq!(
        read_args(s.transport(), 0).1,
        1024,
        "the read asked for more than the mount negotiated"
    );
}

#[test]
fn a_read_that_would_run_off_the_offset_space_is_refused() {
    // A file cannot have a byte past offset 2^64-1, so the next read has
    // nowhere to be: `EINVAL`, not a wrapped offset that reads the start of
    // the file into the end of the buffer.
    let mut server = Server::new();
    server.answer_times(2, read_ok(b"x", false));
    let mut s = session(server);
    let mut buf = [0u8; 4];
    assert_eq!(
        s.read_into(&a_handle(), u64::MAX, &mut buf),
        Err(LxError::EINVAL)
    );
    assert_eq!(
        s.transport().calls.len(),
        1,
        "the offset wrapped instead of being refused"
    );
}

// -------------------------------------------------------------- the write

#[test]
fn a_write_longer_than_the_negotiated_size_goes_in_chunks() {
    let mut server = Server::new();
    server.answer(write_ok(1024, 2));
    server.answer(write_ok(500, 2));
    let mut s = session(server);
    let data = vec![7u8; 1524];
    s.write_all(&a_handle(), 0, &data, StableHow::FileSync)
        .unwrap();
    let srv = s.transport();
    assert_eq!(srv.calls.len(), 2);
    assert_eq!(write_args(srv, 0).0, 0);
    assert_eq!(write_args(srv, 0).1.len(), 1024);
    assert_eq!(write_args(srv, 1).0, 1024);
    assert_eq!(write_args(srv, 1).1.len(), 500);
}

#[test]
fn a_write_the_server_took_in_part_sends_the_rest_from_where_it_stopped() {
    // The server took 400 of the 1024 it was sent. The next call must start
    // at 400, not at 1024: a caller that assumed the whole chunk landed
    // leaves a hole in the file and never learns about it.
    let mut server = Server::new();
    server.answer(write_ok(400, 2));
    server.answer(write_ok(600, 2));
    let mut s = session(server);
    let data = vec![7u8; 1000];
    s.write_all(&a_handle(), 0, &data, StableHow::FileSync)
        .unwrap();
    let srv = s.transport();
    assert_eq!(write_args(srv, 1).0, 400);
    assert_eq!(write_args(srv, 1).1.len(), 600);
}

#[test]
fn a_write_the_server_took_nothing_of_is_an_error_and_not_another_turn() {
    let mut server = Server::new();
    server.answer_times(4, write_ok(0, 2));
    let mut s = session(server);
    let data = vec![7u8; 10];
    assert_eq!(
        s.write_all(&a_handle(), 0, &data, StableHow::FileSync),
        Err(LxError::EIO)
    );
    assert_eq!(s.transport().calls.len(), 1);
}

#[test]
fn a_server_that_claims_to_have_written_more_than_it_was_sent_is_refused() {
    // Not pedantry: the next offset is computed from this number, so a count
    // larger than the data skips bytes that were never written.
    let mut server = Server::new();
    server.answer(write_ok(99, 2));
    let mut s = session(server);
    let data = vec![7u8; 10];
    assert_eq!(
        s.write_once(&a_handle(), 0, &data, StableHow::FileSync),
        Err(LxError::EIO)
    );
}

#[test]
fn a_chunked_write_reports_the_weakest_durability_any_chunk_came_back_with() {
    // The weakest chunk comes FIRST, deliberately. With it last, a client
    // that simply kept the *latest* answer would pass this test and still be
    // wrong -- which is what the first version of this test did, and a
    // mutation that replaced the running minimum with the last answer
    // survived it. The order is the whole measurement.
    let mut server = Server::new();
    server.answer(write_ok(1024, 0)); // UNSTABLE
    server.answer(write_ok(100, 2)); // FILE_SYNC
    let mut s = session(server);
    let data = vec![7u8; 1124];
    let (committed, _verf) = s
        .write_all(&a_handle(), 0, &data, StableHow::Unstable)
        .unwrap();
    assert_eq!(committed, StableHow::Unstable);
    assert!(committed.needs_commit());
}

#[test]
fn the_weakest_of_two_durabilities_is_the_weaker_whichever_order_it_comes_in() {
    assert_eq!(
        weakest_of(StableHow::FileSync, StableHow::Unstable),
        StableHow::Unstable
    );
    assert_eq!(
        weakest_of(StableHow::Unstable, StableHow::FileSync),
        StableHow::Unstable
    );
    assert_eq!(
        weakest_of(StableHow::DataSync, StableHow::FileSync),
        StableHow::DataSync
    );
    assert_eq!(
        weakest_of(StableHow::FileSync, StableHow::FileSync),
        StableHow::FileSync
    );
}

#[test]
fn a_write_verifier_from_the_last_chunk_comes_back_for_the_commit_to_compare() {
    let mut server = Server::new();
    server.answer(write_ok(10, 0));
    let mut s = session(server);
    let (_c, verf) = s
        .write_all(&a_handle(), 0, &[1u8; 10], StableHow::Unstable)
        .unwrap();
    assert_eq!(verf, [9u8; 8]);
}

// ------------------------------------------------------------ read-only

#[test]
fn a_read_only_mount_refuses_a_write_before_the_wire_and_not_after_it() {
    let opts = NfsOptions {
        read_only: true,
        ..quick_options()
    };
    let mut s = NfsSession::new(Server::new(), 1, a_handle(), &opts);
    assert_eq!(
        s.write_once(&a_handle(), 0, b"x", StableHow::FileSync),
        Err(LxError::EROFS)
    );
    assert_eq!(s.remove(&a_handle(), "x"), Err(LxError::EROFS));
    assert_eq!(
        s.mkdir(&a_handle(), "d", &SetAttr::default()),
        Err(LxError::EROFS)
    );
    assert!(
        s.transport().calls.is_empty(),
        "a read-only mount reached the server"
    );
}

#[test]
fn a_read_only_mount_still_reads() {
    let opts = NfsOptions {
        read_only: true,
        ..quick_options()
    };
    let mut server = Server::new();
    server.answer(getattr_ok(5));
    let mut s = NfsSession::new(server, 1, a_handle(), &opts);
    assert_eq!(s.getattr(&a_handle()).unwrap().size, 5);
}

// ------------------------------------------------------------- the readdir

#[test]
fn a_listing_follows_the_cookie_of_the_last_entry_of_each_reply() {
    let mut server = Server::new();
    server.answer(readdir_ok(&[(1, "a", 11), (2, "b", 22)], false, [3; 8]));
    server.answer(readdir_ok(&[(3, "c", 33)], true, [3; 8]));
    let mut s = session(server);
    let entries = s.readdir_all(&a_handle(), false).unwrap();
    assert_eq!(
        entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
        vec!["a", "b", "c"]
    );
    let srv = s.transport();
    assert_eq!(readdir_args(srv, 0).0, COOKIE_START);
    assert_eq!(
        readdir_args(srv, 1).0,
        22,
        "the second call did not resume from the last entry's cookie"
    );
}

#[test]
fn the_verifier_that_came_with_a_cookie_goes_back_with_it() {
    // Including a verifier of eight zeroes, which is a value and not an
    // absence: a server whose cookies are always valid uses zero forever.
    let mut server = Server::new();
    server.answer(readdir_ok(&[(1, "a", 11)], false, [0xAB; 8]));
    server.answer(readdir_ok(&[], true, [0xAB; 8]));
    let mut s = session(server);
    s.readdir_all(&a_handle(), false).unwrap();
    let srv = s.transport();
    assert_eq!(readdir_args(srv, 0).1, vec![0u8; 8]);
    assert_eq!(
        readdir_args(srv, 1).1,
        vec![0xABu8; 8],
        "the verifier the server stamped was not echoed"
    );
}

#[test]
fn an_empty_reply_without_eof_asks_again_with_a_bigger_count() {
    // The legal "nothing fit" answer. Re-asking with the same count gets the
    // same answer forever; concluding the directory ended lists a prefix of
    // it with no error anywhere.
    let mut server = Server::new();
    server.answer(readdir_ok(&[], false, [1; 8]));
    server.answer(readdir_ok(&[(1, "a", 11)], true, [1; 8]));
    let mut s = session(server);
    let entries = s.readdir_all(&a_handle(), false).unwrap();
    assert_eq!(entries.len(), 1, "the empty reply was taken for the end");
    let srv = s.transport();
    let (first_cookie, _, first_count) = readdir_args(srv, 0);
    let (again_cookie, _, again_count) = readdir_args(srv, 1);
    assert_eq!(
        again_cookie, first_cookie,
        "the retry moved the cookie, so entries were skipped"
    );
    assert!(
        again_count > first_count,
        "the retry asked with the same count, which gets the same answer"
    );
}

#[test]
fn a_server_that_never_fits_an_entry_fails_rather_than_growing_forever() {
    let mut server = Server::new();
    server.answer_times(64, readdir_ok(&[], false, [1; 8]));
    let mut s = session(server);
    assert_eq!(s.readdir_all(&a_handle(), false), Err(LxError::EIO));
    assert!(
        s.transport().calls.len() < 64,
        "the count grew past every bound instead of giving up"
    );
}

#[test]
fn a_cookie_the_server_hands_back_unchanged_is_an_error_and_not_another_round() {
    // The cheapest way to wedge a client: answer every call with the same
    // cookie. The listing would never end and would never stop allocating.
    let mut server = Server::new();
    server.answer_times(4, readdir_ok(&[(1, "a", 0)], false, [1; 8]));
    let mut s = session(server);
    assert_eq!(s.readdir_all(&a_handle(), false), Err(LxError::EIO));
    assert_eq!(
        s.transport().calls.len(),
        1,
        "the listing asked again with the cookie it had already been given"
    );
}

#[test]
fn only_eof_ends_a_listing() {
    let mut server = Server::new();
    server.answer(readdir_ok(&[(1, "a", 11)], true, [1; 8]));
    let mut s = session(server);
    let entries = s.readdir_all(&a_handle(), false).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(s.transport().calls.len(), 1, "eof did not end the listing");
}

#[test]
fn a_bad_cookie_restarts_the_listing_once_and_then_gives_up() {
    // The directory changed under the listing. Linux starts over; so does
    // this, once. A second one is a directory changing faster than it can be
    // read, and is reported rather than retried forever.
    let mut server = Server::new();
    server.answer(failure(10_003)); // NFS3ERR_BAD_COOKIE
    server.answer(readdir_ok(&[(1, "a", 11)], true, [1; 8]));
    let mut s = session(server);
    assert_eq!(s.readdir_all(&a_handle(), false).unwrap().len(), 1);

    let mut server = Server::new();
    server.answer_times(4, failure(10_003));
    let mut s = session(server);
    assert_eq!(s.readdir_all(&a_handle(), false), Err(LxError::EINVAL));
    assert_eq!(
        s.transport().calls.len(),
        2,
        "the restart did not stop at one"
    );
}

#[test]
fn a_readdirplus_asks_for_less_in_names_than_in_the_whole_reply() {
    // `dircount` bounds the names and `maxcount` the whole reply. Sending the
    // same number for both asks the server to fit the attributes and the
    // handles in the space the names alone were budgeted for.
    let mut server = Server::new();
    server.answer(readdir_ok(&[], true, [1; 8]));
    let mut s = session(server);
    s.readdir_chunk(&a_handle(), 0, &VERF_START, 8192, true)
        .unwrap();
    let mut dec = XdrDecoder::new(&s.transport().calls[0].args);
    dec.opaque().unwrap();
    dec.u64().unwrap();
    dec.opaque_fixed(COOKIEVERF_SIZE).unwrap();
    let dircount = dec.u32().unwrap();
    let maxcount = dec.u32().unwrap();
    assert!(
        dircount < maxcount,
        "dircount was not smaller than maxcount"
    );
    assert_eq!(maxcount, 8192);
    assert_eq!(s.transport().calls[0].procedure, nfs3_dir::PROC_READDIRPLUS);
}

// ---------------------------------------------------------- the negotiation

#[test]
fn the_transfer_sizes_come_down_to_what_the_server_will_carry() {
    let opts = NfsOptions {
        rsize: 64 * 1024,
        wsize: 64 * 1024,
        ..quick_options()
    };
    let mut server = Server::new();
    server.answer(fsinfo_ok(32 * 1024, 32 * 1024, 4096, 16 * 1024, 4096));
    let mut s = NfsSession::new(server, 1, a_handle(), &opts);
    s.negotiate().unwrap();
    assert_eq!(s.read_size(), 32 * 1024);
    assert_eq!(s.write_size(), 16 * 1024);
    assert_eq!(s.dir_count(), 4096);
}

#[test]
fn a_server_that_offers_more_than_was_asked_for_does_not_raise_the_sizes() {
    // The mount option is a ceiling as well as a request: it bounds a kernel
    // allocation per in-flight call, and a server is not what decides it.
    let mut server = Server::new();
    server.answer(fsinfo_ok(1 << 20, 1 << 20, 1, 1 << 20, 0));
    let mut s = session(server);
    s.negotiate().unwrap();
    assert_eq!(s.read_size(), 1024);
    assert_eq!(s.write_size(), 1024);
}

#[test]
fn a_dtpref_of_zero_leaves_the_clients_own_count_alone() {
    // Zero is "no opinion", not "nothing". Taking it would ask the server to
    // fit a directory in a reply of no bytes.
    let mut server = Server::new();
    server.answer(fsinfo_ok(8192, 8192, 1, 8192, 0));
    let mut s = session(server);
    let before = s.dir_count();
    s.negotiate().unwrap();
    assert_eq!(s.dir_count(), before);
    assert!(s.dir_count() > 0);
}

#[test]
fn what_fsinfo_said_is_kept_for_the_layer_above_to_read() {
    let mut server = Server::new();
    server.answer(fsinfo_ok(8192, 8192, 512, 8192, 4096));
    let mut s = session(server);
    assert!(s.fsinfo().is_none());
    s.negotiate().unwrap();
    assert_eq!(s.fsinfo().unwrap().rtmult, 512);
}

// ---------------------------------------------------- the envelope failures

#[test]
fn a_server_that_refuses_the_credential_is_eacces_and_not_an_io_error() {
    // The one rejection that is about who asked rather than about the
    // protocol. Reported as EIO it hides a permissions problem behind a
    // broken disk.
    let mut server = Server::new();
    server.deny_next = true;
    let mut s = session(server);
    assert_eq!(s.getattr(&a_handle()), Err(LxError::EACCES));
}

#[test]
fn a_server_that_never_answers_fails_as_an_io_error_on_a_soft_mount() {
    // Linux's soft default is EIO, and `softerr` is the option that makes it
    // ETIMEDOUT. An fstab line copied from a Linux box behaves the same way.
    let mut server = Server::new();
    server.silent = 1;
    let mut s = session(server);
    assert_eq!(s.getattr(&a_handle()), Err(LxError::EIO));
}

#[test]
fn a_reply_this_client_cannot_parse_is_an_io_error() {
    let mut server = Server::new();
    server.answer(vec![0u8; 3]); // not even a status word
    let mut s = session(server);
    assert_eq!(s.getattr(&a_handle()), Err(LxError::EIO));
}

#[test]
fn a_transport_that_is_not_connected_says_so_rather_than_eio() {
    assert_eq!(
        transport_error_to_lx(TransportError::NotConnected),
        LxError::ENOTCONN
    );
    assert_eq!(transport_error_to_lx(TransportError::Reset), LxError::EIO);
}

// ------------------------------------------------------------- the identity

#[test]
fn the_credential_the_calls_carry_is_the_identity_that_was_set() {
    let mut server = Server::new();
    server.answer_times(2, getattr_ok(1));
    let mut s = session(server);
    s.getattr(&a_handle()).unwrap();
    s.set_identity(1000, 1001);
    s.getattr(&a_handle()).unwrap();
    match s.credential() {
        Credential::Unix(u) => {
            assert_eq!(u.uid, 1000);
            assert_eq!(u.gid, 1001);
        }
        other => panic!("not an AUTH_UNIX credential: {:?}", other),
    }
}

#[test]
fn a_fresh_session_claims_root_and_lets_the_server_squash_it() {
    let s = NfsSession::new(Server::new(), 1, a_handle(), &quick_options());
    match s.credential() {
        Credential::Unix(u) => {
            assert_eq!(u.uid, 0);
            assert_eq!(u.machine_name, machine_name());
        }
        other => panic!("not an AUTH_UNIX credential: {:?}", other),
    }
}

// -------------------------------------------------------------- the bounds

/// The defaults have to be wide enough that no real directory reaches them: a
/// bound lowered to something a large `ls` hits would turn that `ls` into an
/// I/O error. Checked when the kernel is built, because as a runtime
/// assertion it would be a constant.
const _: () = assert!(READDIR_MAX_ROUNDS >= 1_000);
const _: () = assert!(READDIR_MAX_ENTRIES >= 1 << 16);
const _: () = assert!(MAX_PAUSE_PASSES >= 1_000);
const _: () = assert!(DIR_COUNT_GROWTH >= 2);

#[test]
fn a_directory_that_never_ends_stops_at_the_entry_bound() {
    // A server is free to generate entries for ever, with a cookie that
    // advances every time, and a listing is collected in memory: without a
    // bound that is a kernel heap this kernel hands to whoever answered on
    // port 2049. The bound is lowered here because the real one is a million
    // entries, and a bound nothing can reach is a bound nothing measures.
    let mut server = Server::new();
    for i in 0..20u64 {
        server.answer(readdir_ok(&[(i + 1, "x", i + 1)], false, [1; 8]));
    }
    let mut s = session(server);
    s.set_readdir_limits(READDIR_MAX_ROUNDS, 4);
    assert_eq!(s.readdir_all(&a_handle(), false), Err(LxError::EIO));
    assert!(
        s.transport().calls.len() <= 6,
        "it kept asking past the entry bound: {} calls",
        s.transport().calls.len()
    );
}

#[test]
fn a_directory_that_never_ends_stops_at_the_round_bound_too() {
    // The other bound, for the server whose replies are empty of entries but
    // never `eof` once the count has stopped growing -- and for anything else
    // that would keep this loop turning. Each is a separate exit because each
    // is reached by a different kind of misbehaviour.
    let mut server = Server::new();
    for i in 0..20u64 {
        server.answer(readdir_ok(&[(i + 1, "x", i + 1)], false, [1; 8]));
    }
    let mut s = session(server);
    s.set_readdir_limits(3, READDIR_MAX_ENTRIES);
    assert_eq!(s.readdir_all(&a_handle(), false), Err(LxError::EIO));
    assert!(
        s.transport().calls.len() <= 5,
        "it turned past the round bound: {} calls",
        s.transport().calls.len()
    );
}

#[test]
fn a_wcc_data_the_server_sent_reaches_the_caller() {
    // The failure arms drop it on purpose — this layer has no cache to
    // refresh — but the success arm must not, because the layer above does.
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    enc.u32(1); // before present
    enc.u64(4096);
    put_time(&mut enc, 10, 11);
    put_time(&mut enc, 12, 13);
    put_no_post_op(&mut enc);
    let mut server = Server::new();
    server.answer(enc.into_bytes());
    let mut s = session(server);
    let wcc = s.remove(&a_handle(), "x").unwrap();
    assert_eq!(
        wcc.before,
        Some(PreOpAttr {
            size: 4096,
            mtime: Timespec { sec: 10, nsec: 11 },
            ctime: Timespec { sec: 12, nsec: 13 },
        })
    );
}

#[test]
fn rmdir_and_remove_are_the_same_arguments_and_different_procedures() {
    // One encoder, two procedure numbers: the server is what refuses a
    // directory to one and a file to the other.
    let mut server = Server::new();
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_empty_wcc(&mut enc);
    let ok = enc.into_bytes();
    server.answer_times(2, ok);
    let mut s = session(server);
    s.remove(&a_handle(), "f").unwrap();
    s.rmdir(&a_handle(), "d").unwrap();
    let srv = s.transport();
    assert_eq!(srv.calls[0].procedure, nfs3_write::PROC_REMOVE);
    assert_eq!(srv.calls[1].procedure, nfs3_write::PROC_RMDIR);
}
