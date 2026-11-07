//! The scripted connector the mount path is measured against.
//!
//! What has to be measured on this path is not a decoding -- the codecs have
//! their own suites -- but **which port was dialled, what program was asked
//! for down it, with whose credential, and whether the connection was let go
//! of**. So the connector writes every dial into a shared log, and the server
//! behind it decodes each call's program, version, procedure and credential
//! flavour out of the envelope and writes those down too. A mount that asks
//! `mountd` for the NFS program, or holds the mount daemon's connection open,
//! is a perfectly decodable conversation and wrong, and only the log shows
//! it.
//!
//! Shared by the greeting's suite and the mount's rather than copied into
//! each, for the reason [`fake`](super::fake) is shared: two copies of a fake
//! drift, and the one that drifts is the one whose suite stops measuring what
//! it says it measures.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use lock::Mutex;

use crate::error::{LxError, LxResult};

use super::client::{RpcTransport, TransportError};
use super::connect::Connector;
use super::portmap;
use super::rpc::{frame_message, MSG_TYPE_REPLY};
use super::source::{NfsOptions, NfsSource};
use super::xdr::{XdrDecoder, XdrEncoder};

/// One call, as the server saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Call {
    pub(crate) port: u16,
    pub(crate) program: u32,
    pub(crate) version: u32,
    pub(crate) procedure: u32,
    /// The credential's flavour: 0 is `AUTH_NULL`, 1 is `AUTH_UNIX`.
    pub(crate) cred_flavor: u32,
    pub(crate) args: Vec<u8>,
}

/// What the test reads afterwards. Shared, because the connector hands its
/// transports away and the test cannot follow them.
#[derive(Debug, Default)]
pub(crate) struct Log {
    /// Every port dialled, in order.
    pub(crate) dialled: Vec<u16>,
    /// The timeout each dial was given, in the same order.
    ///
    /// What makes the greeting's single deadline measurable at all: a step
    /// handed a fresh full timeout and a step handed what is left of one
    /// behave identically when nothing times out, and only the number
    /// passed in tells them apart.
    pub(crate) budgets: Vec<u64>,
    /// Every call made, in order.
    pub(crate) calls: Vec<Call>,
    /// Connections made and not yet dropped.
    pub(crate) open: i32,
    /// The high-water mark of `open`.
    pub(crate) most_open: i32,
}

pub(crate) type Shared = Arc<Mutex<Log>>;

/// A server on one port, with the answers the test wrote for it.
///
/// The sharing is `Arc`/`Mutex` and not `Rc`/`RefCell` for one reason:
/// [`NfsFileSystem`](super::fs::NfsFileSystem) holds its session behind a
/// `Mutex` and so requires its transport to be `Send`, which an `Rc` is not.
/// A harness that cannot be handed to the type the mount path produces is a
/// harness that cannot measure the mount path.
pub(crate) struct Server {
    pub(crate) port: u16,
    /// Shared with every other connection to the same port, so the answers
    /// the test wrote are consumed **in order across connections**. Giving
    /// each connection its own copy would hand the second one the first
    /// one's answers, which is a fake that cannot tell a greeting asking
    /// portmap twice from one asking it once -- and that is the thing being
    /// measured.
    pub(crate) answers: Arc<Mutex<VecDeque<Vec<u8>>>>,
    pub(crate) pending: VecDeque<Vec<u8>>,
    pub(crate) now: u64,
    pub(crate) log: Shared,
    /// How many of the next calls get no answer at all.
    pub(crate) silent: usize,
    /// Whether the next call is answered with `MSG_DENIED`/`AUTH_ERROR`.
    pub(crate) deny: bool,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.log.lock().open -= 1;
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
        self.log.lock().calls.push(Call {
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
        if self.deny {
            self.deny = false;
            self.pending.push_back(denied(xid));
            return Ok(());
        }
        let next = self.answers.lock().pop_front();
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
pub(crate) struct Dialer {
    pub(crate) scripts: BTreeMap<u16, Arc<Mutex<VecDeque<Vec<u8>>>>>,
    pub(crate) silent: BTreeMap<u16, usize>,
    pub(crate) deny: BTreeMap<u16, bool>,
    pub(crate) local: BTreeMap<u16, LxError>,
    /// Real milliseconds to burn inside a dial to `port`. See
    /// [`Dialer::slow`].
    pub(crate) burn: BTreeMap<u16, u64>,
    pub(crate) log: Shared,
}

impl Dialer {
    pub(crate) fn new() -> Self {
        Self {
            scripts: BTreeMap::new(),
            silent: BTreeMap::new(),
            deny: BTreeMap::new(),
            local: BTreeMap::new(),
            burn: BTreeMap::new(),
            log: Arc::new(Mutex::new(Log::default())),
        }
    }

    /// Queue one procedure result for `port`.
    pub(crate) fn answer(&mut self, port: u16, results: Vec<u8>) -> &mut Self {
        self.scripts
            .entry(port)
            .or_default()
            .lock()
            .push_back(results);
        self
    }

    /// Throw away whatever was queued for `port`, so a test can rewrite one
    /// step of a working server without rebuilding the other two.
    pub(crate) fn clear(&mut self, port: u16) -> &mut Self {
        self.scripts.entry(port).or_default().lock().clear();
        self
    }

    /// Make dialling `port` fail with `err` **before** any connection: what
    /// this machine running out of local ports or memory looks like.
    pub(crate) fn refuse_locally(&mut self, port: u16, err: LxError) -> &mut Self {
        self.local.insert(port, err);
        self
    }

    /// Make `port` refuse its next call with `MSG_DENIED`/`AUTH_ERROR`.
    ///
    /// Reachable, answering, and saying no -- which is the case the fallback
    /// to 2049 must not cover.
    pub(crate) fn deny_next(&mut self, port: u16) -> &mut Self {
        self.scripts.entry(port).or_default();
        self.deny.insert(port, true);
        self
    }

    /// Make `port` reachable but silent for its next `n` calls.
    pub(crate) fn silent(&mut self, port: u16, n: usize) -> &mut Self {
        self.scripts.entry(port).or_default();
        self.silent.insert(port, n);
        self
    }

    /// Make a dial to `port` take `ms` of **real** time.
    ///
    /// Real and not the fake's own clock, because the thing it is there to
    /// measure reads the real one: the greeting's deadline comes from
    /// `kernel_hal`'s timer, so a budget that is shared rather than restarted
    /// is only visible once that timer has moved. Every other wait in this
    /// harness is on the transport's clock and costs nothing; this one is the
    /// exception and is why it is asked for by name rather than applied
    /// everywhere.
    pub(crate) fn slow(&mut self, port: u16, ms: u64) -> &mut Self {
        self.burn.insert(port, ms);
        self
    }

    pub(crate) fn log(&self) -> Shared {
        Arc::clone(&self.log)
    }
}

impl Connector for Dialer {
    type Transport = Server;

    fn connect(&mut self, port: u16, timeout_ms: u64) -> LxResult<Self::Transport> {
        {
            let mut log = self.log.lock();
            log.dialled.push(port);
            log.budgets.push(timeout_ms);
        }
        if let Some(&ms) = self.burn.get(&port) {
            let until = now_ms().saturating_add(ms);
            while now_ms() < until {
                core::hint::spin_loop();
            }
        }
        if let Some(&err) = self.local.get(&port) {
            return Err(err);
        }
        let answers = match self.scripts.get(&port) {
            Some(a) => Arc::clone(a),
            // Nothing is listening there.
            None => return Err(LxError::ECONNREFUSED),
        };
        let mut log = self.log.lock();
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
            log: Arc::clone(&self.log),
            silent: self.silent.get(&port).copied().unwrap_or(0),
            deny: self.deny.get(&port).copied().unwrap_or(false),
        })
    }
}

/// A reply refusing `xid`'s credential: `MSG_DENIED` with `AUTH_BADCRED`.
pub(crate) fn denied(xid: u32) -> Vec<u8> {
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
    enc.u32(0); // verifier: AUTH_NULL
    enc.u32(0); // verifier: empty body
    enc.u32(0); // SUCCESS
    let mut out = enc.into_bytes();
    out.extend_from_slice(results);
    frame_message(&out)
}

/// A `GETPORT` result.
pub(crate) fn getport(port: u16) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(u32::from(port));
    enc.into_bytes()
}

/// A successful `MNT` result: a root handle and a flavour list.
pub(crate) fn mnt_ok(handle: &[u8], flavors: &[u32]) -> Vec<u8> {
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
pub(crate) fn mnt_err(status: u32) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(status);
    enc.into_bytes()
}

/// `UMNT` answers nothing at all: a void result.
pub(crate) fn umnt_ok() -> Vec<u8> {
    Vec::new()
}

pub(crate) const ROOT: &[u8] = &[1, 2, 3, 4, 5, 6, 7, 8];
pub(crate) const MOUNTD_PORT: u16 = 45_678;

pub(crate) fn quick_options() -> NfsOptions {
    NfsOptions {
        timeout_ms: 500,
        retrans: 0,
        ..NfsOptions::default()
    }
}

pub(crate) fn a_source() -> NfsSource {
    NfsSource::parse("nas:/srv/media").unwrap()
}

/// A dialer that answers the whole greeting: portmap for both programs, then
/// `mountd`.
pub(crate) fn a_working_server() -> Dialer {
    let mut d = Dialer::new();
    d.answer(portmap::PORTMAP_PORT, getport(MOUNTD_PORT));
    d.answer(portmap::PORTMAP_PORT, getport(2049));
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[1]));
    d
}

/// The clock the greeting's deadline reads.
fn now_ms() -> u64 {
    kernel_hal::timer::timer_now().as_millis() as u64
}
