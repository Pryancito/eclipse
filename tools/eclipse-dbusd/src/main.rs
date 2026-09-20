//! `eclipse-dbusd` — Eclipse OS's own D-Bus **session** message bus.
//!
//! Why this exists at all
//! ----------------------
//! Everything desktop-shaped assumes a session bus. SDL2 calls
//! `SDL_DBus_Init()` from `SDL_Init()` before it does anything else; GTK's
//! `GtkApplication` exits before drawing a single pixel without one; Qt, GIO,
//! portals and every "is another copy of me already running?" check are built
//! on `RequestName`. Eclipse's answer so far was to pin
//! `DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus` with **no daemon**
//! behind it, so that `connect()` fails instantly with `ECONNREFUSED` and
//! libdbus gives up rather than forking `dbus-launch` (which hung gzdoom). That
//! is a muzzle, not a bus.
//!
//! This is the daemon that address was always meant to point at. Alpine's
//! `dbus-daemon` is also installed by default (`xtask/src/linux/xorg.rs`) and
//! the `eclipse-dbus` wrapper prefers it when present; this implementation is
//! what makes the session bus work on an image built with no package mirror
//! within reach, and — because it is ours — what can be tested against the
//! Eclipse kernel in QEMU rather than only on hardware.
//!
//! What it implements
//! ------------------
//! * The `unix:path=` transport, the SASL handshake (`EXTERNAL` with
//!   `SO_PEERCRED`, plus `ANONYMOUS`), and `NEGOTIATE_UNIX_FD`.
//! * Unique-name assignment (`:1.N`), `Hello`, and the full well-known-name
//!   machinery: `RequestName`/`ReleaseName` with the replacement queue,
//!   `NameOwnerChanged`/`NameAcquired`/`NameLost`.
//! * Unicast routing by destination with a forged-proof `SENDER` field, error
//!   replies for unroutable calls, signal broadcast filtered by `AddMatch`
//!   rules, and `SCM_RIGHTS` pass-through for messages carrying file
//!   descriptors.
//!
//! What it does NOT implement, on purpose
//! --------------------------------------
//! * **Service activation.** There is no `.service` scanning and no forking of
//!   activatable services: `StartServiceByName` answers
//!   `ServiceUnknown` unless the name is already on the bus. Nothing in an
//!   Eclipse session is activated on demand — every daemon is an
//!   `eclipse-init` service — and activation is the part of a bus that most
//!   wants a policy engine behind it.
//! * **Policy (`<allow>`/`<deny>` in `session.conf`).** Eclipse is
//!   single-user root; the bus refuses connections whose `SO_PEERCRED` uid is
//!   not the uid the daemon runs as, and beyond that everything is allowed.
//! * **Eavesdropping / `BecomeMonitor`.** `dbus-monitor` will not work.
//!
//! Running it
//! ----------
//! ```text
//! eclipse-dbusd --session --address=unix:path=/run/user/0/bus   # foreground
//! eclipse-dbusd --selftest [--address=...]                      # probe a live bus
//! ```
//! It stays in the foreground: `eclipse-init` supervises it like every other
//! Eclipse service (see `/etc/eclipse/services/dbus.service`).

mod bus;
mod client;
mod message;

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::exit;
use std::time::{SystemTime, UNIX_EPOCH};

use bus::Bus;
use message::{Message, MAX_MESSAGE_LEN};

/// Where the session bus lives when nothing says otherwise. Matches the
/// `DBUS_SESSION_BUS_ADDRESS` every Eclipse session already exports.
const DEFAULT_ADDRESS: &str = "unix:path=/run/user/0/bus";

fn log(msg: &str) {
    println!("[eclipse-dbusd] {msg}");
    let _ = io::Write::flush(&mut io::stdout());
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut address: Option<String> = None;
    let mut selftest = false;
    let mut print_address = false;
    let mut hold = false;

    for a in &args {
        if let Some(v) = a.strip_prefix("--address=") {
            address = Some(v.to_string());
        } else {
            match a.as_str() {
                // Accepted for command-line compatibility with dbus-daemon so
                // the wrapper can pass the same argv to either binary.
                "--session" | "--nofork" | "--nopidfile" | "--syslog" | "--syslog-only" => {}
                "--print-address" => print_address = true,
                "--selftest" => selftest = true,
                // Same convention as eclipse-sdl-probe: keep the window (here,
                // the terminal) open so the result can be read when the probe
                // was launched from the desktop menu.
                "--hold" => hold = true,
                "--version" => {
                    println!("eclipse-dbusd {}", env!("CARGO_PKG_VERSION"));
                    return;
                }
                "--help" | "-h" => {
                    println!(
                        "usage: eclipse-dbusd [--session] [--address=unix:path=PATH] \
                         [--print-address] [--selftest] [--hold]"
                    );
                    return;
                }
                "--system" => {
                    eprintln!("eclipse-dbusd: there is no system bus on Eclipse OS");
                    exit(2);
                }
                other => {
                    eprintln!("eclipse-dbusd: unknown argument {other}");
                    exit(2);
                }
            }
        }
    }

    let address = address
        .or_else(|| std::env::var("DBUS_SESSION_BUS_ADDRESS").ok())
        .unwrap_or_else(|| DEFAULT_ADDRESS.to_string());
    let path = match socket_path(&address) {
        Some(p) => p,
        None => {
            eprintln!("eclipse-dbusd: only unix:path= addresses are supported (got {address})");
            exit(2);
        }
    };

    if selftest {
        let outcome = selftest::run(&path);
        match &outcome {
            Ok(()) => println!("DBUSPROBE: PASS session bus at {path}"),
            Err(e) => println!("DBUSPROBE: FAIL {e}"),
        }
        if hold {
            println!("(pulsa Intro para cerrar)");
            let mut line = String::new();
            let _ = std::io::BufRead::read_line(&mut io::stdin().lock(), &mut line);
        }
        if outcome.is_err() {
            exit(1);
        }
        return;
    }

    // SIGPIPE would kill the daemon the first time a client disappears
    // mid-write. Every write path here already handles EPIPE.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    if let Err(e) = serve(&path, print_address) {
        eprintln!("eclipse-dbusd: {e}");
        exit(1);
    }
}

/// Extract the filesystem path from a `unix:path=…` (or `unix:abstract=…`,
/// which Eclipse's kernel has no abstract namespace for) address.
fn socket_path(address: &str) -> Option<String> {
    for part in address.trim_start_matches("unix:").split(',') {
        if let Some(p) = part.strip_prefix("path=") {
            return Some(p.to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// Where a connection is in the SASL handshake.
#[derive(PartialEq)]
enum Auth {
    /// Waiting for the leading NUL byte.
    Nul,
    /// Exchanging auth lines.
    Lines,
    /// `AUTH EXTERNAL` with no initial response: waiting for `DATA <hex-uid>`.
    WantData,
    /// `BEGIN` seen: everything from here is messages.
    Done,
}

/// One byte stream plus everything the daemon must remember about it.
struct Peer {
    fd: RawFd,
    auth: Auth,
    /// Bytes read but not yet consumed (auth lines, then messages).
    inbuf: Vec<u8>,
    /// Queued writes. Each chunk carries the fds that must ride with its FIRST
    /// `sendmsg`, so a partially written chunk never sends them twice.
    out: VecDeque<OutChunk>,
    /// How much of `out.front()` has already gone out.
    out_done: usize,
    /// File descriptors received with `SCM_RIGHTS` and not yet handed to a
    /// decoded message, oldest first.
    recv_fds: VecDeque<RawFd>,
    /// The peer agreed to `SCM_RIGHTS` transfer.
    fds_ok: bool,
}

struct OutChunk {
    data: Vec<u8>,
    fds: Vec<RawFd>,
}

impl Peer {
    fn queue(&mut self, data: Vec<u8>, fds: Vec<RawFd>) {
        self.out.push_back(OutChunk { data, fds });
    }
    fn wants_write(&self) -> bool {
        !self.out.is_empty()
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        // Anything still in flight belongs to nobody now.
        for fd in self.recv_fds.drain(..) {
            unsafe { libc::close(fd) };
        }
        for chunk in self.out.drain(..) {
            for fd in chunk.fds {
                unsafe { libc::close(fd) };
            }
        }
        unsafe { libc::close(self.fd) };
    }
}

fn serve(path: &str, print_address: bool) -> Result<(), String> {
    // The socket's directory is the only access control a `unix:path=` bus
    // has, so create it 0700 when it is missing.
    if let Some(dir) = Path::new(path).parent() {
        let _ = fs::create_dir_all(dir);
        set_mode(dir, 0o700);
    }
    // A stale socket from a previous boot makes bind() fail with EADDRINUSE
    // even though nothing is listening. init clears /run at boot, but a
    // restart within one boot does not.
    if Path::new(path).exists() {
        if connectable(path) {
            return Err(format!("another bus is already listening on {path}"));
        }
        let _ = fs::remove_file(path);
    }

    let listener = UnixListener::bind(path).map_err(|e| format!("bind {path}: {e}"))?;
    set_mode(Path::new(path), 0o600);
    set_nonblocking(listener.as_raw_fd());

    let machine_id = read_machine_id();
    let guid = bus_guid(&machine_id);
    let mut b = Bus::new(guid.clone(), machine_id);

    if print_address {
        println!("unix:path={path},guid={guid}");
        let _ = io::Write::flush(&mut io::stdout());
    }
    log(&format!("session bus listening on {path} (guid {guid})"));

    let my_uid = unsafe { libc::getuid() };
    let mut peers: Vec<(u64, Peer)> = Vec::new();
    let mut next_id: u64 = 1;

    loop {
        // One pollfd for the listener, one per peer.
        let mut fds: Vec<libc::pollfd> = Vec::with_capacity(peers.len() + 1);
        fds.push(libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        for (_, p) in peers.iter() {
            let mut events = libc::POLLIN;
            if p.wants_write() {
                events |= libc::POLLOUT;
            }
            fds.push(libc::pollfd {
                fd: p.fd,
                events,
                revents: 0,
            });
        }
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("poll: {err}"));
        }

        // New connections.
        if fds[0].revents & libc::POLLIN != 0 {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let fd = stream.as_raw_fd();
                        std::mem::forget(stream); // the Peer owns the fd now
                        set_nonblocking(fd);
                        let (pid, uid) = peer_cred(fd);
                        if uid != my_uid {
                            log(&format!(
                                "refused a connection from uid {uid} (bus runs as {my_uid})"
                            ));
                            unsafe { libc::close(fd) };
                            continue;
                        }
                        let id = next_id;
                        next_id += 1;
                        b.add_connection(id, pid, uid);
                        peers.push((
                            id,
                            Peer {
                                fd,
                                auth: Auth::Nul,
                                inbuf: Vec::new(),
                                out: VecDeque::new(),
                                out_done: 0,
                                recv_fds: VecDeque::new(),
                                fds_ok: false,
                            },
                        ));
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        log(&format!("accept: {e}"));
                        break;
                    }
                }
            }
        }

        // Readable / writable peers. `fds[i + 1]` lines up with `peers[i]`
        // because the vector is only mutated after this loop.
        let mut dead: Vec<u64> = Vec::new();
        // Only the peers that were in THIS poll set line up with `fds`; any
        // connection accepted a few lines above is appended after them and is
        // polled on the next turn.
        let polled = fds.len() - 1;
        for (i, (id, peer)) in peers.iter_mut().take(polled).enumerate() {
            let re = fds[i + 1].revents;
            if re & libc::POLLOUT != 0 && !flush(peer) {
                dead.push(*id);
                continue;
            }
            if re & libc::POLLIN != 0 {
                match read_into(peer) {
                    Ok(true) => {}
                    Ok(false) | Err(_) => {
                        dead.push(*id);
                        continue;
                    }
                }
                if let Err(e) = consume(*id, peer, &mut b, &guid) {
                    log(&format!("connection {id}: {e}"));
                    dead.push(*id);
                    continue;
                }
            }
            if re & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                dead.push(*id);
            }
        }

        // Anything the bus produced goes into the owning peer's queue.
        drain_outbox(&mut b, &mut peers);

        // Writes that can go out now, go out now: one round-trip fewer.
        for (id, peer) in peers.iter_mut() {
            if peer.wants_write() && !flush(peer) {
                dead.push(*id);
            }
        }

        if !dead.is_empty() {
            dead.sort_unstable();
            dead.dedup();
            for id in &dead {
                b.remove_connection(*id);
            }
            peers.retain(|(id, _)| !dead.contains(id));
            // Removing a connection emits NameOwnerChanged to the survivors.
            drain_outbox(&mut b, &mut peers);
            for (_, peer) in peers.iter_mut() {
                if peer.wants_write() {
                    let _ = flush(peer);
                }
            }
        }
    }
}

/// Move `bus.outbox` into the peers' write queues, handing each message the
/// file descriptors [`route`] parked for it.
fn drain_outbox(b: &mut Bus, peers: &mut [(u64, Peer)]) {
    let parked = PENDING_FDS.with(|p| std::mem::take(&mut *p.borrow_mut()));
    for (index, (to, msg)) in std::mem::take(&mut b.outbox).into_iter().enumerate() {
        let fds = parked.get(&index).cloned().unwrap_or_default();
        match peers.iter_mut().find(|(id, _)| *id == to) {
            Some((_, peer)) => {
                let fds = if peer.fds_ok || fds.is_empty() {
                    fds
                } else {
                    // The receiver never said AGREE_UNIX_FD: sending SCM_RIGHTS
                    // anyway would strand the descriptors in a socket nobody
                    // reads as ancillary data.
                    log("dropping file descriptors for a peer that did not negotiate them");
                    for fd in &fds {
                        unsafe { libc::close(*fd) };
                    }
                    Vec::new()
                };
                peer.queue(msg.encode(), fds);
            }
            None => {
                for fd in fds {
                    unsafe { libc::close(fd) };
                }
            }
        }
    }
}

use std::cell::RefCell;
use std::collections::BTreeMap;
thread_local! {
    /// File descriptors travelling with a message the bus is about to hand
    /// back, keyed by the message's INDEX in `Bus::outbox`. The [`Bus`] itself
    /// is transport-free and knows nothing about descriptors; this is the one
    /// place the two meet. Indices are stable because the outbox is only ever
    /// appended to between drains, and a serial is not usable as a key: a
    /// forwarded message keeps the ORIGINAL sender's serial, so two clients
    /// collide on it.
    static PENDING_FDS: RefCell<BTreeMap<usize, Vec<RawFd>>> = RefCell::new(BTreeMap::new());
}

/// Read whatever is available. `Ok(false)` means the peer closed its end.
fn read_into(peer: &mut Peer) -> io::Result<bool> {
    loop {
        if peer.inbuf.len() > MAX_MESSAGE_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "input buffer over the message size limit",
            ));
        }
        let mut chunk = [0u8; 8192];
        // Control buffer for SCM_RIGHTS: 16 fds per recvmsg is plenty (the
        // specification caps a message at 16 by default).
        let mut cmsg = [0u8; 256];
        let mut iov = libc::iovec {
            iov_base: chunk.as_mut_ptr() as *mut libc::c_void,
            iov_len: chunk.len(),
        };
        let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
        hdr.msg_iov = &mut iov;
        hdr.msg_iovlen = 1;
        hdr.msg_control = cmsg.as_mut_ptr() as *mut libc::c_void;
        hdr.msg_controllen = cmsg.len() as _;

        let n = unsafe { libc::recvmsg(peer.fd, &mut hdr, 0) };
        if n < 0 {
            let err = io::Error::last_os_error();
            return match err.kind() {
                io::ErrorKind::WouldBlock => Ok(true),
                io::ErrorKind::Interrupted => continue,
                _ => Err(err),
            };
        }
        if n == 0 {
            return Ok(false);
        }
        peer.inbuf.extend_from_slice(&chunk[..n as usize]);

        // Collect any descriptors that rode along.
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(&hdr);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let data = libc::CMSG_DATA(c);
                    let len = (*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                    let count = len / std::mem::size_of::<RawFd>();
                    for k in 0..count {
                        let mut fd: RawFd = 0;
                        std::ptr::copy_nonoverlapping(
                            data.add(k * std::mem::size_of::<RawFd>()),
                            &mut fd as *mut RawFd as *mut u8,
                            std::mem::size_of::<RawFd>(),
                        );
                        peer.recv_fds.push_back(fd);
                    }
                }
                c = libc::CMSG_NXTHDR(&hdr, c);
            }
        }
        if (n as usize) < chunk.len() {
            return Ok(true);
        }
    }
}

/// Write what is queued. `false` means the peer is gone.
fn flush(peer: &mut Peer) -> bool {
    while !peer.out.is_empty() {
        let (done, total, has_fds) = {
            let chunk = &peer.out[0];
            (peer.out_done, chunk.data.len(), !chunk.fds.is_empty())
        };
        if done >= total {
            let chunk = peer.out.pop_front();
            if let Some(c) = chunk {
                for fd in c.fds {
                    unsafe { libc::close(fd) };
                }
            }
            peer.out_done = 0;
            continue;
        }
        let send_fds = done == 0 && has_fds;
        let n = {
            let chunk = &peer.out[0];
            let remaining = &chunk.data[done..];
            if send_fds {
                sendmsg_with_fds(peer.fd, remaining, &chunk.fds)
            } else {
                unsafe {
                    libc::send(
                        peer.fd,
                        remaining.as_ptr() as *const libc::c_void,
                        remaining.len(),
                        libc::MSG_NOSIGNAL,
                    )
                }
            }
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            return match err.kind() {
                io::ErrorKind::WouldBlock => true,
                io::ErrorKind::Interrupted => continue,
                _ => false,
            };
        }
        if send_fds {
            // The descriptors belong to the receiver now.
            for fd in peer.out[0].fds.drain(..) {
                unsafe { libc::close(fd) };
            }
        }
        peer.out_done += n as usize;
    }
    peer.out_done = 0;
    true
}

fn sendmsg_with_fds(fd: RawFd, data: &[u8], fds: &[RawFd]) -> isize {
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    let space = unsafe { libc::CMSG_SPACE((fds.len() * std::mem::size_of::<RawFd>()) as u32) };
    let mut cbuf = vec![0u8; space as usize];
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;
    hdr.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
    hdr.msg_controllen = cbuf.len() as _;
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&hdr);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN((fds.len() * std::mem::size_of::<RawFd>()) as u32) as _;
        std::ptr::copy_nonoverlapping(
            fds.as_ptr() as *const u8,
            libc::CMSG_DATA(c),
            fds.len() * std::mem::size_of::<RawFd>(),
        );
        libc::sendmsg(fd, &hdr, libc::MSG_NOSIGNAL)
    }
}

/// Consume everything complete in the peer's input buffer: auth lines while
/// the handshake runs, messages afterwards.
fn consume(id: u64, peer: &mut Peer, b: &mut Bus, guid: &str) -> Result<(), String> {
    loop {
        match peer.auth {
            Auth::Nul => {
                if peer.inbuf.is_empty() {
                    return Ok(());
                }
                if peer.inbuf[0] != 0 {
                    return Err("first byte of the stream was not NUL".into());
                }
                peer.inbuf.remove(0);
                peer.auth = Auth::Lines;
            }
            Auth::Lines | Auth::WantData => {
                let pos = match peer.inbuf.windows(2).position(|w| w == b"\r\n") {
                    Some(p) => p,
                    None => {
                        // An auth line is short; anything long is an attack or
                        // a client that never sent BEGIN.
                        if peer.inbuf.len() > 16384 {
                            return Err("auth line too long".into());
                        }
                        return Ok(());
                    }
                };
                let line = String::from_utf8_lossy(&peer.inbuf[..pos]).to_string();
                peer.inbuf.drain(..pos + 2);
                auth_line(peer, &line, guid);
            }
            Auth::Done => {
                let (msg, used) = match Message::decode(&peer.inbuf)? {
                    Some(v) => v,
                    None => return Ok(()),
                };
                peer.inbuf.drain(..used);
                // Take the descriptors this message says it carries.
                let want = msg.unix_fds as usize;
                let mut fds = Vec::with_capacity(want);
                for _ in 0..want {
                    match peer.recv_fds.pop_front() {
                        Some(fd) => fds.push(fd),
                        None => break,
                    }
                }
                route(id, msg, fds, b);
            }
        }
    }
}

/// Hand one client message to the bus, arranging for any descriptors to follow
/// the copies the bus decides to send.
fn route(id: u64, msg: Message, fds: Vec<RawFd>, b: &mut Bus) {
    let before = b.outbox.len();
    b.dispatch(id, msg);
    if fds.is_empty() {
        return;
    }
    // Attach to every copy the dispatch produced that still declares fds,
    // duplicating for all but the first recipient.
    let carriers: Vec<usize> = b.outbox[before..]
        .iter()
        .enumerate()
        .filter(|(_, (_, m))| m.unix_fds > 0)
        .map(|(k, _)| before + k)
        .collect();
    if carriers.is_empty() {
        for fd in fds {
            unsafe { libc::close(fd) };
        }
        return;
    }
    PENDING_FDS.with(|p| {
        let mut map = p.borrow_mut();
        for (n, index) in carriers.iter().enumerate() {
            let copy: Vec<RawFd> = if n == 0 {
                fds.clone()
            } else {
                fds.iter().map(|fd| unsafe { libc::dup(*fd) }).collect()
            };
            map.insert(*index, copy);
        }
    });
}

/// One line of the SASL handshake. The daemon only ever replies; a client that
/// says something unexpected gets `ERROR`, which is what the specification
/// asks for and what libdbus copes with.
fn auth_line(peer: &mut Peer, line: &str, guid: &str) {
    let reply = |peer: &mut Peer, text: &str| {
        peer.queue(text.as_bytes().to_vec(), Vec::new());
    };
    let mut parts = line.split(' ');
    let cmd = parts.next().unwrap_or("").to_ascii_uppercase();
    let rest: Vec<&str> = parts.collect();
    // Copy the state out: the match below hands `peer` to `reply` mutably.
    let want_data = peer.auth == Auth::WantData;

    match (cmd.as_str(), want_data) {
        ("AUTH", _) => match rest.first().map(|m| m.to_ascii_uppercase()) {
            None => reply(peer, "REJECTED EXTERNAL ANONYMOUS\r\n"),
            Some(m) if m == "EXTERNAL" => {
                // The uid was already checked with SO_PEERCRED at accept time,
                // so whatever identity the client asserts here is redundant;
                // an EXTERNAL auth with no initial data is answered with DATA.
                if rest.len() > 1 {
                    peer.auth = Auth::Lines;
                    reply(peer, &format!("OK {guid}\r\n"));
                } else {
                    peer.auth = Auth::WantData;
                    reply(peer, "DATA\r\n");
                }
            }
            Some(m) if m == "ANONYMOUS" => {
                peer.auth = Auth::Lines;
                reply(peer, &format!("OK {guid}\r\n"));
            }
            Some(_) => reply(peer, "REJECTED EXTERNAL ANONYMOUS\r\n"),
        },
        ("DATA", true) => {
            peer.auth = Auth::Lines;
            reply(peer, &format!("OK {guid}\r\n"));
        }
        ("NEGOTIATE_UNIX_FD", _) => {
            peer.fds_ok = true;
            reply(peer, "AGREE_UNIX_FD\r\n");
        }
        ("BEGIN", _) => peer.auth = Auth::Done,
        ("CANCEL", _) => {
            peer.auth = Auth::Lines;
            reply(peer, "REJECTED EXTERNAL ANONYMOUS\r\n");
        }
        ("ERROR", _) => reply(peer, "REJECTED EXTERNAL ANONYMOUS\r\n"),
        _ => reply(peer, "ERROR Unknown command\r\n"),
    }
}

// ---------------------------------------------------------------------------
// Small system helpers
// ---------------------------------------------------------------------------

fn set_nonblocking(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

/// `(pid, uid)` of the process on the other end. Eclipse's kernel answers
/// `SO_PEERCRED` with the peer's pid and root's uid; a kernel that does not
/// answer at all leaves the bus assuming its own uid, which keeps a
/// single-user session working instead of refusing every client.
fn peer_cred(fd: RawFd) -> (u32, u32) {
    let mut ucred = [0u32; 3];
    let mut len = std::mem::size_of_val(&ucred) as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            ucred.as_mut_ptr() as *mut libc::c_void,
            &mut len,
        )
    };
    if rc == 0 && len as usize >= std::mem::size_of_val(&ucred) {
        (ucred[0], ucred[1])
    } else {
        (0, unsafe { libc::getuid() })
    }
}

/// Is something actually listening on `path`? Used to tell a stale socket file
/// from a live bus before unlinking it.
fn connectable(path: &str) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

/// `/etc/machine-id`, or a stable-looking fallback. dbus requires exactly 32
/// lowercase hex digits and clients do validate it.
fn read_machine_id() -> String {
    for p in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(s) = fs::read_to_string(p) {
            let s = s.trim().to_ascii_lowercase();
            if s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
                return s;
            }
        }
    }
    "00000000000000000000000000000000".to_string()
}

/// The bus GUID: 32 hex digits, different on each run (clients use it to tell
/// one bus instance from another across reconnects).
fn bus_guid(machine_id: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let mut seed = now ^ (pid << 32) ^ 0x9e37_79b9_7f4a_7c15;
    // Two rounds of splitmix64 give 16 bytes with no dependency on getrandom,
    // which is not what a GUID needs to be anyway (it is an identifier, not a
    // secret).
    let mut out = String::with_capacity(32);
    for _ in 0..2 {
        seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        out.push_str(&format!("{z:016x}"));
    }
    // Fold in the machine id so two boots of the same image still differ but a
    // GUID is recognisably from this host.
    if machine_id.len() == 32 {
        let a = u64::from_str_radix(&machine_id[..8], 16).unwrap_or(0);
        let head = u64::from_str_radix(&out[..16], 16).unwrap_or(0) ^ a;
        out.replace_range(..16, &format!("{head:016x}"));
    }
    out
}

// ---------------------------------------------------------------------------
// Selftest
// ---------------------------------------------------------------------------

mod selftest {
    use crate::client::Client;
    use crate::message::{Arg, Message, MSG_SIGNAL};

    /// Drive a live bus through everything a desktop client depends on. Run on
    /// the guest it is a kernel test as much as a bus test: every step here is
    /// a unix-socket round trip through `sendmsg`/`recvmsg`/`poll`.
    pub fn run(path: &str) -> Result<(), String> {
        let mut a = Client::connect(path)?;
        let mut b = Client::connect(path)?;
        if a.unique == b.unique {
            return Err("two connections got the same unique name".into());
        }

        // A watches for the service name appearing, then B claims it.
        a.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "AddMatch",
            &[Arg::Str(
                "type='signal',interface='org.freedesktop.DBus',member='NameOwnerChanged'".into(),
            )],
        )?;

        let name = "org.eclipse.SelfTest";
        let r = b.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "RequestName",
            &[Arg::Str(name.into()), Arg::U32(0)],
        )?;
        match r.args().first() {
            Some(Arg::U32(1)) => {}
            other => return Err(format!("RequestName returned {other:?}, expected 1")),
        }

        let sig = a.wait_for(|m| {
            m.member.as_deref() == Some("NameOwnerChanged") && m.arg0_str().as_deref() == Some(name)
        })?;
        match sig.args().get(2) {
            Some(Arg::Str(owner)) if *owner == b.unique => {}
            other => return Err(format!("NameOwnerChanged new_owner was {other:?}")),
        }

        // The name resolves, and a call addressed to it reaches B with the
        // sender the bus stamped.
        let r = a.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetNameOwner",
            &[Arg::Str(name.into())],
        )?;
        match r.args().first() {
            Some(Arg::Str(o)) if *o == b.unique => {}
            other => return Err(format!("GetNameOwner returned {other:?}")),
        }

        let mut call = Message::method_call(
            name,
            "/org/eclipse/SelfTest",
            "org.eclipse.SelfTest",
            "Echo",
        );
        call.set_body(&[Arg::Str("ping".into())]);
        let serial = a.send(call)?;
        let got = b.wait_for(|m| m.member.as_deref() == Some("Echo"))?;
        if got.sender.as_deref() != Some(a.unique.as_str()) {
            return Err(format!("forwarded call had sender {:?}", got.sender));
        }
        if got.arg0_str().as_deref() != Some("ping") {
            return Err("forwarded call lost its body".into());
        }

        // B answers; the reply must find its way back to A by unique name.
        let mut reply = Message::method_return(&got);
        reply.destination = Some(a.unique.clone());
        reply.set_body(&[Arg::Str("pong".into())]);
        b.send(reply)?;
        let back = a.wait_for(|m| m.reply_serial == Some(serial))?;
        if back.arg0_str().as_deref() != Some("pong") {
            return Err("reply did not carry its body".into());
        }

        // A broadcast signal reaches a subscriber and nobody else.
        b.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "AddMatch",
            &[Arg::Str(
                "type='signal',interface='org.eclipse.SelfTest'".into(),
            )],
        )?;
        let mut sig = Message::signal("/org/eclipse/SelfTest", "org.eclipse.SelfTest", "Tick");
        sig.set_body(&[Arg::Str("tock".into())]);
        a.send(sig)?;
        let got = b.wait_for(|m| m.kind == MSG_SIGNAL && m.member.as_deref() == Some("Tick"))?;
        if got.arg0_str().as_deref() != Some("tock") {
            return Err("broadcast signal lost its body".into());
        }

        // Peer interface, and the name list the bus reports.
        a.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus.Peer",
            "Ping",
            &[],
        )?;
        let r = a.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "ListNames",
            &[],
        )?;
        match r.args().first() {
            Some(Arg::StrArray(names)) => {
                for want in [
                    "org.freedesktop.DBus".to_string(),
                    a.unique.clone(),
                    b.unique.clone(),
                    name.to_string(),
                ] {
                    if !names.contains(&want) {
                        return Err(format!("ListNames is missing {want}"));
                    }
                }
            }
            other => return Err(format!("ListNames returned {other:?}")),
        }

        // Dropping B must release the name and tell A about it.
        drop(b);
        let sig = a.wait_for(|m| {
            m.member.as_deref() == Some("NameOwnerChanged") && m.arg0_str().as_deref() == Some(name)
        })?;
        match sig.args().get(2) {
            Some(Arg::Str(owner)) if owner.is_empty() => {}
            other => return Err(format!("name was not released on disconnect: {other:?}")),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_parsing() {
        assert_eq!(
            socket_path("unix:path=/run/user/0/bus").as_deref(),
            Some("/run/user/0/bus")
        );
        assert_eq!(
            socket_path("unix:path=/tmp/x,guid=deadbeef").as_deref(),
            Some("/tmp/x")
        );
        assert_eq!(socket_path("tcp:host=localhost,port=1"), None);
    }

    #[test]
    fn guid_is_32_hex_digits() {
        let g = bus_guid("0123456789abcdef0123456789abcdef");
        assert_eq!(g.len(), 32);
        assert!(g.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(g, bus_guid("0123456789abcdef0123456789abcdef"));
    }

    /// End-to-end over a real socket: start the daemon in a thread and run the
    /// same selftest the guest runs.
    #[test]
    fn end_to_end_over_a_unix_socket() {
        let path = format!(
            "/tmp/eclipse-dbusd-test-{}-{}.sock",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let p = path.clone();
        std::thread::spawn(move || {
            let _ = serve(&p, false);
        });
        // Wait for the socket to appear rather than sleeping a fixed time.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !Path::new(&path).exists() {
            assert!(std::time::Instant::now() < deadline, "daemon never bound");
            std::thread::yield_now();
        }
        let result = selftest::run(&path);
        let _ = fs::remove_file(&path);
        result.expect("selftest");
    }
}
