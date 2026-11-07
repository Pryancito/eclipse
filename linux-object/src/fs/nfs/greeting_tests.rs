//! The greeting, against a scripted server reached through a scripted
//! connector.
//!
//! What has to be measured here is not a decoding — the codecs have their own
//! suites — but **which port was dialled, what program was asked for down it,
//! with whose credential, and whether the connection was let go of**. So the
//! connector writes every dial into a shared log, and the server behind it
//! decodes each call's program, version, procedure and credential flavour out
//! of the envelope and writes those down too. A mount that asks `mountd` for
//! the NFS program, or holds the mount daemon's connection open, is a
//! perfectly decodable conversation and wrong, and only the log shows it.

use super::*;
use crate::fs::nfs::client::{RpcTransport, TransportError};
use crate::fs::nfs::nfs3;
use crate::fs::nfs::rpc::{frame_message, MSG_TYPE_REPLY};
use crate::fs::nfs::xdr::{XdrDecoder, XdrEncoder};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::rc::Rc;
use alloc::vec;
use core::cell::RefCell;

/// One call, as the server saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Call {
    port: u16,
    program: u32,
    version: u32,
    procedure: u32,
    /// The credential's flavour: 0 is `AUTH_NULL`, 1 is `AUTH_UNIX`.
    cred_flavor: u32,
    args: Vec<u8>,
}

/// What the test reads afterwards. Shared, because the connector hands its
/// transports away and the test cannot follow them.
#[derive(Debug, Default)]
struct Log {
    /// Every port dialled, in order.
    dialled: Vec<u16>,
    /// Every call made, in order.
    calls: Vec<Call>,
    /// Connections made and not yet dropped.
    open: i32,
    /// The high-water mark of `open`.
    most_open: i32,
}

type Shared = Rc<RefCell<Log>>;

/// A server on one port, with the answers the test wrote for it.
struct Server {
    port: u16,
    /// Shared with every other connection to the same port, so the answers
    /// the test wrote are consumed **in order across connections**. Giving
    /// each connection its own copy would hand the second one the first
    /// one's answers, which is a fake that cannot tell a greeting asking
    /// portmap twice from one asking it once -- and that is the thing being
    /// measured.
    answers: Rc<RefCell<VecDeque<Vec<u8>>>>,
    pending: VecDeque<Vec<u8>>,
    now: u64,
    log: Shared,
    /// How many of the next calls get no answer at all.
    silent: usize,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.log.borrow_mut().open -= 1;
    }
}

impl RpcTransport for Server {
    fn send(&mut self, data: &[u8]) -> Result<(), TransportError> {
        let mut dec = XdrDecoder::new(&data[4..]);
        let xid = dec.u32().expect("xid");
        let _msg = dec.u32().expect("msg type");
        let _rpcvers = dec.u32().expect("rpc version");
        let program = dec.u32().expect("program");
        let version = dec.u32().expect("version");
        let procedure = dec.u32().expect("procedure");
        let cred_flavor = dec.u32().expect("cred flavour");
        dec.opaque().expect("cred body");
        let _verf_flavor = dec.u32().expect("verifier flavour");
        dec.opaque().expect("verifier body");
        self.log.borrow_mut().calls.push(Call {
            port: self.port,
            program,
            version,
            procedure,
            cred_flavor,
            args: dec.rest().to_vec(),
        });
        if self.silent > 0 {
            self.silent -= 1;
            return Ok(());
        }
        let next = self.answers.borrow_mut().pop_front();
        if let Some(results) = next {
            self.pending.push_back(reply(xid, &results));
        }
        Ok(())
    }

    fn recv(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        self.now = self.now.wrapping_add(50);
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
        self.now = self.now.wrapping_add(50);
    }
}

/// A connector with a script per port.
///
/// A port with no script is **refused**, which is what a server with no
/// `rpcbind` looks like from here: the connection, not the call, is what
/// fails.
struct Dialer {
    scripts: BTreeMap<u16, Rc<RefCell<VecDeque<Vec<u8>>>>>,
    silent: BTreeMap<u16, usize>,
    log: Shared,
}

impl Dialer {
    fn new() -> Self {
        Self {
            scripts: BTreeMap::new(),
            silent: BTreeMap::new(),
            log: Rc::new(RefCell::new(Log::default())),
        }
    }

    /// Queue one procedure result for `port`.
    fn answer(&mut self, port: u16, results: Vec<u8>) -> &mut Self {
        self.scripts
            .entry(port)
            .or_default()
            .borrow_mut()
            .push_back(results);
        self
    }

    /// Make `port` reachable but silent for its next `n` calls.
    fn silent(&mut self, port: u16, n: usize) -> &mut Self {
        self.scripts.entry(port).or_default();
        self.silent.insert(port, n);
        self
    }

    fn log(&self) -> Shared {
        Rc::clone(&self.log)
    }
}

impl Connector for Dialer {
    type Transport = Server;

    fn connect(&mut self, port: u16) -> LxResult<Self::Transport> {
        {
            let mut log = self.log.borrow_mut();
            log.dialled.push(port);
        }
        let answers = match self.scripts.get(&port) {
            Some(a) => Rc::clone(a),
            // Nothing is listening there.
            None => return Err(LxError::ECONNREFUSED),
        };
        let mut log = self.log.borrow_mut();
        log.open += 1;
        if log.open > log.most_open {
            log.most_open = log.open;
        }
        drop(log);
        Ok(Server {
            port,
            answers,
            pending: VecDeque::new(),
            now: 0,
            log: Rc::clone(&self.log),
            silent: self.silent.get(&port).copied().unwrap_or(0),
        })
    }
}

/// An accepted, successful RPC reply to `xid` carrying `results`.
fn reply(xid: u32, results: &[u8]) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(xid);
    enc.u32(MSG_TYPE_REPLY);
    enc.u32(0); // MSG_ACCEPTED
    enc.u32(0); // verifier: AUTH_NULL
    enc.u32(0); // verifier: empty body
    enc.u32(0); // SUCCESS
    let mut out = enc.into_bytes();
    out.extend_from_slice(results);
    frame_message(&out)
}

/// A `GETPORT` result.
fn getport(port: u16) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(u32::from(port));
    enc.into_bytes()
}

/// A successful `MNT` result: a root handle and a flavour list.
fn mnt_ok(handle: &[u8], flavors: &[u32]) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0); // MNT3_OK
    enc.opaque(handle);
    enc.u32(flavors.len() as u32);
    for &f in flavors {
        enc.u32(f);
    }
    enc.into_bytes()
}

/// A refused `MNT`.
fn mnt_err(status: u32) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(status);
    enc.into_bytes()
}

/// `UMNT` answers nothing at all: a void result.
fn umnt_ok() -> Vec<u8> {
    Vec::new()
}

const ROOT: &[u8] = &[1, 2, 3, 4, 5, 6, 7, 8];
const MOUNTD_PORT: u16 = 45_678;

fn quick_options() -> NfsOptions {
    NfsOptions {
        timeout_ms: 500,
        retrans: 0,
        ..NfsOptions::default()
    }
}

fn a_source() -> NfsSource {
    NfsSource::parse("nas:/srv/media").unwrap()
}

/// A dialer that answers the whole greeting: portmap for both programs, then
/// `mountd`.
fn a_working_server() -> Dialer {
    let mut d = Dialer::new();
    d.answer(portmap::PORTMAP_PORT, getport(MOUNTD_PORT));
    d.answer(portmap::PORTMAP_PORT, getport(2049));
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[1]));
    d
}

// --------------------------------------------------------------- portmap

#[test]
fn portmap_is_asked_on_port_111_for_the_two_programs_by_number() {
    let mut d = a_working_server();
    let log = d.log();
    greet(&mut d, &a_source(), &quick_options()).unwrap();

    let calls = &log.borrow().calls;
    let asks: Vec<(u16, u32, u32, u32)> = calls
        .iter()
        .filter(|c| c.program == portmap::PROGRAM_PORTMAP)
        .map(|c| {
            let mut dec = XdrDecoder::new(&c.args);
            let program = dec.u32().unwrap();
            let version = dec.u32().unwrap();
            let protocol = dec.u32().unwrap();
            (c.port, program, version, protocol)
        })
        .collect();
    assert_eq!(
        asks,
        vec![
            (
                portmap::PORTMAP_PORT,
                portmap::PROGRAM_MOUNT,
                mount3::MOUNT_VERSION,
                portmap::IPPROTO_TCP
            ),
            (
                portmap::PORTMAP_PORT,
                portmap::PROGRAM_NFS,
                nfs3::NFS_VERSION,
                portmap::IPPROTO_TCP
            ),
        ],
        "portmap was asked the wrong thing, or on the wrong port"
    );
}

#[test]
fn an_answer_of_zero_is_not_a_port_and_is_never_dialled() {
    // The trap. `GETPORT` answers 0 for a program it has no registration
    // for, and 0 is not a port: dialling it fails somewhere else as
    // something else, which is a mount error that points at the wrong thing.
    let mut d = Dialer::new();
    d.answer(portmap::PORTMAP_PORT, getport(0));
    let log = d.log();

    assert_eq!(
        greet(&mut d, &a_source(), &quick_options()),
        Err(LxError::EOPNOTSUPP)
    );
    assert!(
        !log.borrow().dialled.contains(&0),
        "port zero was dialled: {:?}",
        log.borrow().dialled
    );
}

#[test]
fn portmap_is_asked_with_auth_null_and_mountd_with_auth_unix() {
    // Two different programs with two different ideas about who is asking.
    // `rpcbind` does not care -- a port number is not privileged -- and a
    // credential it does not recognise is a refused call for nothing, while
    // `mountd` decides what the export allows for *this* client and some
    // refuse `AUTH_NULL` outright.
    let mut d = a_working_server();
    let log = d.log();
    greet(&mut d, &a_source(), &quick_options()).unwrap();

    let calls = log.borrow();
    for c in calls.calls.iter() {
        let want = if c.program == portmap::PROGRAM_PORTMAP {
            0
        } else {
            1
        };
        assert_eq!(
            c.cred_flavor, want,
            "program {} was asked with flavour {}",
            c.program, c.cred_flavor
        );
    }
}

// ------------------------------------------------------- the ports chosen

#[test]
fn a_port_in_the_options_skips_portmap_entirely() {
    let mut d = Dialer::new();
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[1]));
    let log = d.log();
    let opts = NfsOptions {
        port: Some(2049),
        mountport: Some(MOUNTD_PORT),
        ..quick_options()
    };

    let g = greet(&mut d, &a_source(), &opts).unwrap();

    assert_eq!(g.nfs_port, 2049);
    assert_eq!(g.mount_port, MOUNTD_PORT);
    assert_eq!(
        log.borrow().dialled,
        vec![MOUNTD_PORT],
        "portmap was asked for a port the mount already knew"
    );
}

#[test]
fn a_portmap_that_cannot_be_reached_falls_back_to_2049_for_nfs() {
    // Deliberate divergence, and the asymmetry is the point: `nfsd` on 2049
    // is near-universal and a growing number of servers run it with no
    // `rpcbind` at all, so a refused portmap must not fail a mount that
    // would have worked.
    let mut d = Dialer::new();
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[1]));
    let opts = NfsOptions {
        mountport: Some(MOUNTD_PORT),
        ..quick_options()
    };

    let g = greet(&mut d, &a_source(), &opts).unwrap();

    assert_eq!(g.nfs_port, portmap::NFS_PORT);
}

#[test]
fn a_portmap_that_cannot_be_reached_does_not_guess_a_port_for_mountd() {
    // The other half of the asymmetry. `mountd`'s port is genuinely
    // arbitrary, so a guess at it dials some other service -- and on a
    // machine that has one, sends it a `MNT`.
    let mut d = Dialer::new();
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[1]));
    let log = d.log();

    assert_eq!(
        greet(&mut d, &a_source(), &quick_options()),
        Err(LxError::ECONNREFUSED)
    );
    // Nothing but portmap: `does not contain mountd's port` would pass a
    // client that guessed some *other* port, which is the mistake.
    assert_eq!(
        log.borrow().dialled,
        vec![portmap::PORTMAP_PORT],
        "a port was guessed for mountd"
    );
}

#[test]
fn a_portmap_that_says_nfs_is_not_registered_is_believed_rather_than_guessed() {
    // The difference between "cannot be reached" and "answered no". A
    // registration portmap denies is an answer: it knows, and falling back to
    // 2049 against its word would be dialling a port the server has just
    // said nothing is on.
    let mut d = Dialer::new();
    d.answer(portmap::PORTMAP_PORT, getport(MOUNTD_PORT));
    d.answer(portmap::PORTMAP_PORT, getport(0));
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[1]));
    let log = d.log();

    assert_eq!(
        greet(&mut d, &a_source(), &quick_options()),
        Err(LxError::EOPNOTSUPP)
    );
    assert!(
        !log.borrow().dialled.contains(&portmap::NFS_PORT),
        "2049 was dialled against portmap's word"
    );
}

// ----------------------------------------------------------------- MOUNT

#[test]
fn the_export_path_is_what_goes_to_mountd_and_not_the_whole_source() {
    let mut d = a_working_server();
    let log = d.log();
    greet(&mut d, &a_source(), &quick_options()).unwrap();

    let calls = log.borrow();
    let mnt = calls
        .calls
        .iter()
        .find(|c| c.program == portmap::PROGRAM_MOUNT)
        .expect("mountd was never asked");
    assert_eq!(mnt.procedure, mount3::PROC_MNT);
    assert_eq!(mnt.port, MOUNTD_PORT);
    let mut dec = XdrDecoder::new(&mnt.args);
    assert_eq!(
        core::str::from_utf8(dec.string_bytes().unwrap()).unwrap(),
        "/srv/media"
    );
}

#[test]
fn the_root_handle_is_the_one_the_server_sent() {
    let mut d = a_working_server();
    let g = greet(&mut d, &a_source(), &quick_options()).unwrap();
    assert_eq!(g.root.as_bytes(), ROOT);
}

#[test]
fn the_mount_daemons_connection_does_not_outlive_the_mnt() {
    // `mountd` commonly has very few slots, and a leak here shows up as the
    // *next* mount hanging -- a symptom with nothing to do with the mount
    // that caused it. The greeting ends holding no connection at all: the
    // one the session will use is opened by the session.
    let mut d = a_working_server();
    let log = d.log();
    greet(&mut d, &a_source(), &quick_options()).unwrap();

    let log = log.borrow();
    assert_eq!(log.open, 0, "the greeting ended holding a connection");
    assert_eq!(
        log.most_open, 1,
        "two connections were held at once, so one was kept across a step"
    );
}

#[test]
fn a_server_that_lists_only_flavours_this_client_cannot_speak_is_refused_at_mount_time() {
    // The trap that looks like a mount that worked. The export lists
    // `RPCSEC_GSS` and nothing else, so every NFS call afterwards is refused
    // with an auth error -- arriving as `EACCES` on the first `stat`, long
    // after the mount said it was fine.
    let mut d = Dialer::new();
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[6, 390_003]));
    let opts = NfsOptions {
        port: Some(2049),
        mountport: Some(MOUNTD_PORT),
        ..quick_options()
    };

    assert_eq!(greet(&mut d, &a_source(), &opts), Err(LxError::EACCES));
}

#[test]
fn an_empty_flavour_list_is_not_a_refusal() {
    // It means the server did not say, and plenty do not. Reading it as "no
    // flavours are acceptable" would refuse those servers outright.
    let mut d = Dialer::new();
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[]));
    let opts = NfsOptions {
        port: Some(2049),
        mountport: Some(MOUNTD_PORT),
        ..quick_options()
    };

    let g = greet(&mut d, &a_source(), &opts).unwrap();
    assert_eq!(g.root.as_bytes(), ROOT);
    assert!(g.auth_flavors.is_empty());
}

#[test]
fn a_list_that_holds_auth_unix_among_others_is_accepted() {
    let mut d = Dialer::new();
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[390_003, 1]));
    let opts = NfsOptions {
        port: Some(2049),
        mountport: Some(MOUNTD_PORT),
        ..quick_options()
    };

    assert!(greet(&mut d, &a_source(), &opts).is_ok());
}

#[test]
fn each_refusal_from_mountd_gets_its_own_errno() {
    // The export path's verdict is the one error a person reading a failed
    // mount can act on: "no such export" and "your address is not allowed"
    // ask for different things, and both as `EIO` would ask for nothing.
    for (status, want) in [
        (2u32, LxError::ENOENT),
        (13, LxError::EACCES),
        (20, LxError::ENOTDIR),
        (63, LxError::ENAMETOOLONG),
        (10_004, LxError::EOPNOTSUPP),
        (1, LxError::EPERM),
    ] {
        let mut d = Dialer::new();
        d.answer(MOUNTD_PORT, mnt_err(status));
        let opts = NfsOptions {
            port: Some(2049),
            mountport: Some(MOUNTD_PORT),
            ..quick_options()
        };
        assert_eq!(
            greet(&mut d, &a_source(), &opts),
            Err(want),
            "MNT3ERR {} came out as the wrong errno",
            status
        );
    }
}

#[test]
fn an_export_path_longer_than_the_protocol_allows_never_reaches_the_wire() {
    let mut d = Dialer::new();
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[1]));
    let log = d.log();
    let long = alloc::string::String::from_utf8(vec![b'a'; mount3::MNTPATHLEN + 1]).unwrap();
    let source = NfsSource {
        host: alloc::string::String::from("nas"),
        export: alloc::format!("/{}", long),
    };
    let opts = NfsOptions {
        port: Some(2049),
        mountport: Some(MOUNTD_PORT),
        ..quick_options()
    };

    assert!(greet(&mut d, &source, &opts).is_err());
    assert!(
        log.borrow().calls.is_empty(),
        "a path the protocol refuses was sent anyway"
    );
}

// ----------------------------------------------------------------- UMNT

#[test]
fn umnt_is_the_same_path_on_the_same_port_with_a_different_procedure() {
    let mut d = Dialer::new();
    d.answer(MOUNTD_PORT, umnt_ok());
    let log = d.log();

    unmount_export(&mut d, MOUNTD_PORT, "/srv/media", &quick_options()).unwrap();

    let log = log.borrow();
    let call = &log.calls[0];
    assert_eq!(call.port, MOUNTD_PORT);
    assert_eq!(call.program, portmap::PROGRAM_MOUNT);
    assert_eq!(call.version, mount3::MOUNT_VERSION);
    assert_eq!(call.procedure, mount3::PROC_UMNT);
    assert_ne!(mount3::PROC_UMNT, mount3::PROC_MNT);
    let mut dec = XdrDecoder::new(&call.args);
    assert_eq!(
        core::str::from_utf8(dec.string_bytes().unwrap()).unwrap(),
        "/srv/media"
    );
    assert_eq!(log.open, 0, "the UMNT's connection was kept");
}

// ------------------------------------------------------------ the timing

#[test]
fn the_greeting_is_soft_even_on_a_hard_mount() {
    // A `hard` mount means "wait for the server rather than fail the call",
    // and a mount that has not happened yet has no calls to protect: all a
    // hard greeting could do is hold `mount(2)` for ever, on the thread that
    // drives the network stack.
    let mut d = Dialer::new();
    d.silent(MOUNTD_PORT, 8);
    let opts = NfsOptions {
        port: Some(2049),
        mountport: Some(MOUNTD_PORT),
        soft: false,
        ..quick_options()
    };

    let log = d.log();
    assert!(
        greet(&mut d, &a_source(), &opts).is_err(),
        "a hard mount against a silent mountd was supposed to fail, not hang"
    );
    // The outcome alone measures nothing: a `hard` call still ends, at the
    // client's own ten-minute ceiling, so both versions fail and only the
    // *number of calls* says which policy was in force. `retrans: 0` means
    // one call and no retransmission.
    assert_eq!(
        log.borrow().calls.len(),
        1,
        "the greeting retransmitted, so it was not soft"
    );
}
