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

/// Where the system bus lives when nothing says otherwise: the one path
/// libdbus compiles in, so every client that asks for the system bus without
/// an address finds it here. This daemon used to answer `--system` with
/// "there is no system bus on Eclipse OS" and exit 2, which is what left
/// pulseaudio logging "Unable to contact D-Bus system bus: Failed to connect
/// to socket /run/dbus/system_bus_socket: No such file or directory" on every
/// boot. Nothing in the daemon is session-specific --- it is the same router
/// either way --- so the only difference is the default path and that
/// `DBUS_SESSION_BUS_ADDRESS` must NOT be consulted here: in a desktop session
/// that variable points at the session bus, and honouring it would make
/// `--system` quietly serve (or collide with) the session socket.
const DEFAULT_SYSTEM_ADDRESS: &str = "unix:path=/run/dbus/system_bus_socket";

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
    let mut system = false;

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
                        "usage: eclipse-dbusd [--session|--system] \
                         [--address=unix:path=PATH] [--print-address] \
                         [--selftest] [--hold]"
                    );
                    return;
                }
                "--system" => system = true,
                other => {
                    eprintln!("eclipse-dbusd: unknown argument {other}");
                    exit(2);
                }
            }
        }
    }

    let address = chosen_address(
        address,
        system,
        std::env::var("DBUS_SESSION_BUS_ADDRESS").ok(),
    );
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

    if let Err(e) = serve(&path, print_address, system) {
        eprintln!("eclipse-dbusd: {e}");
        exit(1);
    }
}

/// Which address this run serves. `--address=` always wins. Otherwise a
/// session bus follows `DBUS_SESSION_BUS_ADDRESS` and falls back to
/// [`DEFAULT_ADDRESS`], while a system bus ignores that variable entirely and
/// uses [`DEFAULT_SYSTEM_ADDRESS`]: inside a desktop session the variable
/// names the session socket, and honouring it under `--system` would have the
/// two buses fight over one path.
fn chosen_address(explicit: Option<String>, system: bool, session_env: Option<String>) -> String {
    if let Some(a) = explicit {
        return a;
    }
    if system {
        return DEFAULT_SYSTEM_ADDRESS.to_string();
    }
    session_env.unwrap_or_else(|| DEFAULT_ADDRESS.to_string())
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

    /// A peer with no socket behind it. The handshake and the message framing
    /// never look at the descriptor -- they only ever touch the buffers -- so
    /// this is enough to drive them, and `-1` is what `Drop` hands `close`,
    /// which refuses it harmlessly.
    #[cfg(test)]
    fn detached() -> Peer {
        Peer {
            fd: -1,
            auth: Auth::Nul,
            inbuf: Vec::new(),
            out: VecDeque::new(),
            out_done: 0,
            recv_fds: VecDeque::new(),
            fds_ok: false,
        }
    }

    /// Everything queued for writing, as text. The handshake is lines of
    /// ASCII, so this is what a client would read off the socket.
    #[cfg(test)]
    fn queued(&self) -> String {
        self.out
            .iter()
            .map(|c| String::from_utf8_lossy(&c.data).into_owned())
            .collect()
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

/// The directory and socket modes a bus of this kind is created with.
///
/// A session bus belongs to one uid and refusing the rest is its whole access
/// control, so 0700/0600. The system bus is the opposite by definition: its
/// clients run as other uids (pulseaudio drops to its own), and 0700/0600
/// would hand them EACCES instead of the "no such file" they used to get,
/// which is no better.
fn socket_modes(system: bool) -> (u32, u32) {
    if system {
        (0o755, 0o666)
    } else {
        (0o700, 0o600)
    }
}

/// May a connection from `uid` stay?
///
/// A session bus belongs to one uid. A system bus runs as root precisely so
/// that clients under other uids can reach it, so the same check there turns
/// the socket the daemon just created into a closed door.
fn accepts_uid(system: bool, uid: u32, my_uid: u32) -> bool {
    system || uid == my_uid
}

/// Clear the socket file at `path` if it is a leftover, and refuse to start if
/// it is a live bus.
///
/// A stale socket from a previous boot makes `bind()` fail with EADDRINUSE
/// even though nothing is listening. init clears `/run` at boot, but a restart
/// within one boot does not. Whether the file is stale is decided by trying to
/// connect to it, which is why this is split from `serve`: the test can put a
/// real listener on one path and a plain file on another.
fn clear_stale_socket(path: &str) -> Result<(), String> {
    if Path::new(path).exists() {
        if connectable(path) {
            return Err(format!("another bus is already listening on {path}"));
        }
        // Report a removal that fails instead of walking into `bind()`, which
        // answers EADDRINUSE and names neither the file nor the reason -- the
        // same message a live bus gives, which is the one case this function
        // exists to tell apart. A read-only /run, a socket owned by another
        // uid, or something that is not a file at all all land here.
        if let Err(e) = fs::remove_file(path) {
            return Err(format!("cannot clear the stale socket {path}: {e}"));
        }
    }
    Ok(())
}

/// `system` only changes who may reach the socket. A session bus belongs to one
/// uid, so its directory is 0700 and the socket 0600. The system bus is the
/// opposite by definition: `--system` clients run as other uids (pulseaudio
/// drops to its own), so 0700/0600 would hand them EACCES instead of the
/// "no such file" they used to get, which is no better. Everything else ---
/// routing, names, signals --- is identical.
fn serve(path: &str, print_address: bool, system: bool) -> Result<(), String> {
    let (dir_mode, sock_mode) = socket_modes(system);
    // The socket's directory is part of the access control a `unix:path=` bus
    // has, so create it with that mode when it is missing.
    if let Some(dir) = Path::new(path).parent() {
        let _ = fs::create_dir_all(dir);
        set_mode(dir, dir_mode);
    }
    clear_stale_socket(path)?;

    let listener = UnixListener::bind(path).map_err(|e| format!("bind {path}: {e}"))?;
    set_mode(Path::new(path), sock_mode);
    set_nonblocking(listener.as_raw_fd());

    let machine_id = read_machine_id();
    let guid = bus_guid(&machine_id);
    let mut b = Bus::new(guid.clone(), machine_id);

    if print_address {
        println!("unix:path={path},guid={guid}");
        let _ = io::Write::flush(&mut io::stdout());
    }
    log(&format!(
        "{} bus listening on {path} (guid {guid})",
        if system { "system" } else { "session" }
    ));

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
                        // A session bus belongs to one uid and refusing the
                        // rest is its whole access control. A system bus is
                        // the opposite: it runs as root precisely so that
                        // clients under other uids can reach it (pulseaudio
                        // drops to its own), so this check would turn the
                        // socket we just created into a closed door.
                        if !accepts_uid(system, uid, my_uid) {
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
                // The `is_empty` half changes no descriptor: the branch
                // below closes nothing and answers with an empty vector too.
                // It is there so a peer that negotiated nothing and was sent
                // nothing does not get the line in the log.
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

/// The most descriptors one message may carry. The specification's default,
/// and what `read_into`'s control buffer is sized for.
/// Longest the unterminated head of the auth handshake may get before the
/// connection is dropped. An `AUTH EXTERNAL <hex uid>` line is tens of bytes;
/// anything approaching this is a client that never sent `BEGIN` or one
/// feeding the daemon a buffer to grow. Named because the test that exercises
/// the limit used to repeat the number, so moving it here moved the limit for
/// production only and the test went on passing.
const MAX_AUTH_LINE: usize = 16384;

const MAX_MESSAGE_FDS: usize = 16;

/// The most descriptors the daemon will hold for one peer before dropping it.
///
/// A peer may legitimately have pipelined several messages before `consume`
/// runs, so this is a generous multiple of the per-message ceiling -- and
/// still far below a default `RLIMIT_NOFILE`, which is the number that
/// matters: past it the daemon cannot `accept`, and a session bus that
/// cannot accept is a dead desktop.
const MAX_QUEUED_FDS: usize = MAX_MESSAGE_FDS * 16;

/// Has this peer queued more descriptors than the daemon will hold for it?
///
/// Nothing consumed them before: `read_into` collects whatever rides on a
/// `recvmsg` -- before the handshake is even finished -- and `consume` only
/// pops them once a whole message has decoded. So a client that attaches
/// descriptors to bytes that never complete a message has the daemon hold
/// them for as long as it stays connected. Measured with a client that never
/// got past the opening NUL: 960 sent, 962 more descriptors open in the
/// daemon, none of them ever asked for.
fn fd_queue_overflowed(queued: usize) -> bool {
    queued > MAX_QUEUED_FDS
}

/// How many descriptors to hand a message that declares `declared` of them,
/// given `queued` are actually waiting.
///
/// `declared` is a `u32` copied straight out of the `UNIX_FDS` header field,
/// so it is whatever the client wrote there. Reserving room for it was an
/// abort: `u32::MAX` asks for 16 GiB, the allocator says no and Rust calls
/// `handle_alloc_error`, which kills the daemon -- from one method call of a
/// hundred bytes, sent by any client allowed on the bus.
///
/// The answer cannot be more than is really queued, which is the bound: the
/// loop that follows already stopped at the end of the queue, so this changes
/// no behaviour at all, only how much room is set aside for it.
fn fds_to_take(declared: u32, queued: usize) -> usize {
    (declared as usize).min(queued)
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
        if fd_queue_overflowed(peer.recv_fds.len()) {
            // Dropping the peer is what closes them: `Peer::drop` drains the
            // queue. Answering `Err` here is how that happens.
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "peer queued more file descriptors than the bus will hold",
            ));
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
        // `done == 0` cannot be false while `has_fds` is true -- the first
        // send with descriptors drains them out of the chunk below -- so it
        // is the sentence and not the condition that needs the first half.
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
            // The descriptors belong to the receiver now. The pop above
            // closes whatever is left on a chunk, so dropping this would
            // still close them in the end -- just not until the rest of a
            // partially written chunk has gone out.
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
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of_val(fds) as u32) };
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
        (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds) as u32) as _;
        std::ptr::copy_nonoverlapping(
            fds.as_ptr() as *const u8,
            libc::CMSG_DATA(c),
            std::mem::size_of_val(fds),
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
                        if peer.inbuf.len() > MAX_AUTH_LINE {
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
                // Take the descriptors this message says it carries -- as
                // many as it really has, which is not the same number.
                let want = fds_to_take(msg.unix_fds, peer.recv_fds.len());
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
    // Linux fills all three words or fails outright, so the length is only
    // ever short on a kernel that answers `SO_PEERCRED` partially -- which is
    // the case this is here for, and the one no test on a Linux host can
    // reach.
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

/// Where a machine id may live, in the order dbus looks for it. The second is
/// where dbus itself used to write one, and an image that has it there and not
/// in /etc is still a valid host.
const MACHINE_ID_PATHS: [&str; 2] = ["/etc/machine-id", "/var/lib/dbus/machine-id"];

/// The machine id all-zeros, for a host that has none. dbus requires exactly
/// 32 hex digits, so there is no "empty" answer to give.
const NO_MACHINE_ID: &str = "00000000000000000000000000000000";

/// `/etc/machine-id`, or a stable-looking fallback.
fn read_machine_id() -> String {
    read_machine_id_from(&MACHINE_ID_PATHS)
}

/// `read_machine_id` over the paths it is given. Split from the constant
/// because the real paths are absolute and a test can put a file at neither.
fn read_machine_id_from(paths: &[&str]) -> String {
    for p in paths {
        if let Ok(s) = fs::read_to_string(p) {
            if let Some(id) = machine_id_of(&s) {
                return id;
            }
        }
    }
    NO_MACHINE_ID.to_string()
}

/// The machine id a file's contents name, if they name one.
///
/// dbus requires exactly 32 lowercase hex digits and clients do validate it,
/// so a file holding anything else is not an id -- the next path is tried and
/// then the fallback. The trailing newline `systemd-machine-id-setup` writes
/// is not part of it.
fn machine_id_of(text: &str) -> Option<String> {
    let s = text.trim().to_ascii_lowercase();
    (s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())).then_some(s)
}

/// The bus GUID: 32 hex digits, different on each run (clients use it to tell
/// one bus instance from another across reconnects).
fn bus_guid(machine_id: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    guid_from(now, std::process::id() as u64, machine_id)
}

/// `bus_guid` from the clock reading and the pid it would have read. Split out
/// because with those two inside, no two calls can be compared: the pid goes
/// into the high half of the seed and the machine id is folded into the
/// result, and neither is visible from a single GUID.
fn guid_from(now: u64, pid: u64, machine_id: &str) -> String {
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
    fn the_system_bus_lands_on_the_path_libdbus_compiles_in() {
        // pulseaudio and every other `--system` client with no address asks
        // libdbus, which has exactly this path built in. Getting it wrong is
        // indistinguishable, from the client's side, from the daemon not
        // running at all: "Failed to connect to socket
        // /run/dbus/system_bus_socket: No such file or directory".
        assert_eq!(
            chosen_address(None, true, None),
            "unix:path=/run/dbus/system_bus_socket"
        );
    }

    #[test]
    fn a_session_address_in_the_environment_never_reaches_the_system_bus() {
        // The regression this guards: inside a desktop session
        // DBUS_SESSION_BUS_ADDRESS is always set, so a `--system` daemon that
        // read it would bind the session socket --- either colliding with the
        // session bus or silently serving it as if it were the system one.
        let session = Some("unix:path=/run/user/0/bus".to_string());
        assert_eq!(
            chosen_address(None, true, session.clone()),
            "unix:path=/run/dbus/system_bus_socket"
        );
        // And the session bus does follow it, which is why it is read at all.
        assert_eq!(
            chosen_address(None, false, session),
            "unix:path=/run/user/0/bus"
        );
    }

    #[test]
    fn an_explicit_address_wins_over_both_defaults() {
        let want = "unix:path=/tmp/some-other-bus";
        for system in [false, true] {
            assert_eq!(
                chosen_address(Some(want.to_string()), system, None),
                want,
                "--address= must win with system={system}"
            );
        }
        assert_eq!(chosen_address(None, false, None), DEFAULT_ADDRESS);
    }

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
            let _ = serve(&p, false, false);
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

    // ---- descriptors the client says it sent, and the ones it really did ---

    #[test]
    fn a_message_only_gets_room_for_the_descriptors_that_are_really_there() {
        // The count comes out of the `UNIX_FDS` header field, so it is
        // whatever the client wrote. Reserving for it aborted the daemon.
        assert_eq!(fds_to_take(u32::MAX, 3), 3);
        assert_eq!(fds_to_take(u32::MAX, 0), 0);
        assert_eq!(fds_to_take(1_000_000, 2), 2);
    }

    #[test]
    fn a_message_takes_no_more_than_it_declares_even_when_more_are_waiting() {
        // The other half of the bound, and the half that must not change:
        // descriptors are queued per peer and handed out per message, so
        // taking a queued one that this message did not declare would give it
        // to the wrong recipient.
        assert_eq!(fds_to_take(2, 5), 2);
        assert_eq!(fds_to_take(0, 5), 0);
        assert_eq!(fds_to_take(5, 5), 5);
    }

    #[test]
    fn the_queue_holds_more_than_a_message_needs_and_much_less_than_a_process_may_open() {
        // Several messages' worth still fit: a peer may legitimately have
        // pipelined a few before `consume` runs.
        assert!(
            !fd_queue_overflowed(MAX_MESSAGE_FDS * 4),
            "cuatro mensajes legitimos ya desbordan la cola"
        );
        // A default soft `RLIMIT_NOFILE` is 1024. Past it the daemon cannot
        // `accept`, and a session bus that cannot accept is a dead desktop.
        assert!(
            fd_queue_overflowed(1024),
            "un solo cliente puede agotar los descriptores del demonio"
        );
        assert!(!fd_queue_overflowed(MAX_QUEUED_FDS));
        assert!(fd_queue_overflowed(MAX_QUEUED_FDS + 1));
        assert!(!fd_queue_overflowed(0));
    }

    /// Start a daemon on its own socket and wait for it to bind.
    fn a_bus(tag: &str) -> String {
        let path = format!(
            "/tmp/eclipse-dbusd-test-{}-{}-{}.sock",
            std::process::id(),
            tag,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let p = path.clone();
        std::thread::spawn(move || {
            let _ = serve(&p, false, false);
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !Path::new(&path).exists() {
            assert!(std::time::Instant::now() < deadline, "daemon never bound");
            std::thread::yield_now();
        }
        path
    }

    /// A method call the bus itself answers, so a live bus is a reply.
    fn get_id(c: &mut client::Client) -> Result<Message, String> {
        c.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetId",
            &[],
        )
    }

    /// The failure here is not an assertion: it is `SIGABRT`, from the
    /// allocator refusing 16 GiB inside `consume`. Measured on master.
    #[test]
    fn a_message_declaring_four_billion_descriptors_does_not_kill_the_bus() {
        let path = a_bus("fdcount");
        let mut c = client::Client::connect(&path).expect("connect");
        let mut m = Message::method_call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetId",
        );
        m.unix_fds = u32::MAX;
        c.send(m).expect("send");
        // The bus is still there, and still answering -- including this very
        // client, whose message carried no descriptors at all.
        let mut other = client::Client::connect(&path).expect("el bus murio");
        get_id(&mut other).expect("el bus ya no contesta");
        let _ = fs::remove_file(&path);
    }

    /// Descriptors arrive before the handshake even finishes and are only
    /// consumed by a decoded message, so a client that never completes one
    /// had the daemon hold them until it ran out.
    #[test]
    fn a_client_that_hoards_descriptors_is_dropped_before_the_bus_runs_out() {
        fn open_fds() -> usize {
            fs::read_dir("/proc/self/fd")
                .map(|d| d.count())
                .unwrap_or(0)
        }
        const PER: usize = 8;
        let path = a_bus("fdflood");
        let base = open_fds();
        let sock = std::os::unix::net::UnixStream::connect(&path).expect("connect");
        let fd = std::os::unix::io::AsRawFd::as_raw_fd(&sock);
        // The opening NUL, with nothing attached.
        assert_eq!(unsafe { libc::write(fd, [0u8].as_ptr() as *const _, 1) }, 1);
        let dev_null = fs::File::open("/dev/null").expect("/dev/null");
        let src = std::os::unix::io::AsRawFd::as_raw_fd(&dev_null);

        // One byte of an auth line that never ends, with descriptors hanging
        // off every send. The loop ends when the daemon drops us, which is
        // the whole assertion: a failed send means the peer is gone.
        //
        // What the client managed to send is not the measure -- the kernel
        // holds descriptors in flight until the daemon reads them, and the
        // client runs ahead. What the daemon has actually installed is, and
        // that is what `/proc/self/fd` counts: the daemon runs in a thread of
        // this very process.
        let slack = 64;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut sent = 0;
        while send_with_fds(fd, b"A", src, PER) {
            sent += PER;
            let held = open_fds().saturating_sub(base);
            assert!(
                held <= MAX_QUEUED_FDS + slack,
                "el demonio retiene {held} descriptores de un cliente que no ha \
                 mandado ni un mensaje"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "el demonio no ha soltado al cliente tras {sent} descriptores"
            );
            // Give it the chance to read them.
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
        assert!(sent > MAX_QUEUED_FDS, "no llegamos ni al tope: {sent}");

        // And the bus itself is unharmed: it dropped the peer, not its socket.
        let mut c = client::Client::connect(&path).expect("el bus ya no acepta");
        get_id(&mut c).expect("el bus ya no contesta");
        let _ = fs::remove_file(&path);
    }

    /// `sendmsg` of `bytes` with `count` copies of `src` attached. `false`
    /// means the write failed, which is what a dropped peer looks like.
    fn send_with_fds(fd: RawFd, bytes: &[u8], src: RawFd, count: usize) -> bool {
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr() as *mut libc::c_void,
            iov_len: bytes.len(),
        };
        let space = unsafe { libc::CMSG_SPACE((count * 4) as u32) } as usize;
        let mut cmsg = vec![0u8; space];
        let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
        hdr.msg_iov = &mut iov;
        hdr.msg_iovlen = 1;
        hdr.msg_control = cmsg.as_mut_ptr() as *mut libc::c_void;
        hdr.msg_controllen = space as _;
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&hdr);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN((count * 4) as u32) as _;
            let data = libc::CMSG_DATA(c) as *mut RawFd;
            for k in 0..count {
                *data.add(k) = src;
            }
            libc::sendmsg(fd, &hdr, libc::MSG_NOSIGNAL) > 0
        }
    }

    /// A connected pair of non-blocking unix sockets: a `Peer` on one end and
    /// the raw descriptor of the other, to play the client with.
    fn socket_pair() -> (Peer, RawFd) {
        let mut fds = [0 as libc::c_int; 2];
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        assert_eq!(rc, 0, "socketpair: {}", io::Error::last_os_error());
        set_nonblocking(fds[0]);
        set_nonblocking(fds[1]);
        let mut p = Peer::detached();
        p.fd = fds[0];
        (p, fds[1])
    }

    fn is_open(fd: RawFd) -> bool {
        unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
    }

    /// Send `bytes` with `fds` riding along as `SCM_RIGHTS`, each descriptor
    /// its own. The helper further down sends one descriptor `count` times,
    /// which cannot show what ORDER they arrive in.
    fn send_each_fd(fd: RawFd, bytes: &[u8], fds: &[RawFd]) -> bool {
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr() as *mut libc::c_void,
            iov_len: bytes.len(),
        };
        let space = unsafe { libc::CMSG_SPACE(std::mem::size_of_val(fds) as u32) } as usize;
        let mut cmsg = vec![0u8; space];
        let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
        hdr.msg_iov = &mut iov;
        hdr.msg_iovlen = 1;
        hdr.msg_control = cmsg.as_mut_ptr() as *mut libc::c_void;
        hdr.msg_controllen = space as _;
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&hdr);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds) as u32) as _;
            let data = libc::CMSG_DATA(c) as *mut RawFd;
            for (k, fd) in fds.iter().enumerate() {
                *data.add(k) = *fd;
            }
            libc::sendmsg(fd, &hdr, libc::MSG_NOSIGNAL) > 0
        }
    }

    /// Which `Auth` state a row of the table below names.
    fn state(name: &str) -> Auth {
        match name {
            "Nul" => Auth::Nul,
            "Lines" => Auth::Lines,
            "WantData" => Auth::WantData,
            "Done" => Auth::Done,
            other => panic!("no such auth state: {other}"),
        }
    }

    /// Every line of the SASL handshake and what the daemon answers to it.
    ///
    /// The handshake had no test of its own, and it is the one part of the
    /// daemon every client walks through before anything else: a client that
    /// cannot authenticate never reaches the bus, and all it can report is
    /// that the connection failed.
    #[test]
    fn every_line_of_the_handshake_is_answered_the_way_the_specification_says() {
        // (state it is in, the line it gets, the state it ends in, what goes back)
        let cases: &[(&str, &str, &str, &str)] = &[
            // The uid was already checked with SO_PEERCRED at accept time, so
            // EXTERNAL with an initial response is simply accepted.
            ("Lines", "AUTH EXTERNAL 30", "Lines", "OK G\r\n"),
            // Without one the client is asked for it, and then any DATA line
            // finishes the handshake.
            ("Lines", "AUTH EXTERNAL", "WantData", "DATA\r\n"),
            ("WantData", "DATA 30", "Lines", "OK G\r\n"),
            // A DATA line nobody asked for is not part of any handshake.
            ("Lines", "DATA 30", "Lines", "ERROR Unknown command\r\n"),
            ("Lines", "AUTH ANONYMOUS", "Lines", "OK G\r\n"),
            // libdbus writes the command and the mechanism in capitals; the
            // specification asks that of neither.
            ("Lines", "auth external 30", "Lines", "OK G\r\n"),
            ("Lines", "AUTH anonymous", "Lines", "OK G\r\n"),
            ("Lines", "begin", "Done", ""),
            // A mechanism this daemon does not have, and no mechanism at all,
            // are answered with the two it does.
            (
                "Lines",
                "AUTH KERBEROS_V4",
                "Lines",
                "REJECTED EXTERNAL ANONYMOUS\r\n",
            ),
            ("Lines", "AUTH", "Lines", "REJECTED EXTERNAL ANONYMOUS\r\n"),
            (
                "Lines",
                "CANCEL",
                "Lines",
                "REJECTED EXTERNAL ANONYMOUS\r\n",
            ),
            (
                "Lines",
                "ERROR not my fault",
                "Lines",
                "REJECTED EXTERNAL ANONYMOUS\r\n",
            ),
            // CANCEL from the middle of EXTERNAL puts the client back to
            // choosing a mechanism rather than through to messages.
            (
                "WantData",
                "CANCEL",
                "Lines",
                "REJECTED EXTERNAL ANONYMOUS\r\n",
            ),
            ("Lines", "BEGIN", "Done", ""),
            ("Lines", "NEGOTIATE_UNIX_FD", "Lines", "AGREE_UNIX_FD\r\n"),
            ("Lines", "WHAT", "Lines", "ERROR Unknown command\r\n"),
            ("Lines", "", "Lines", "ERROR Unknown command\r\n"),
        ];
        for (from, line, to, reply) in cases {
            let mut p = Peer::detached();
            p.auth = state(from);
            auth_line(&mut p, line, "G");
            assert_eq!(p.queued(), *reply, "answer to {line:?} in {from}");
            assert!(
                p.auth == state(to),
                "{line:?} in {from} should leave it in {to}"
            );
        }

        // `NEGOTIATE_UNIX_FD` is the only line that turns descriptor passing
        // on, and the daemon refuses to send any until it has seen it.
        let mut p = Peer::detached();
        p.auth = Auth::Lines;
        assert!(!p.fds_ok);
        auth_line(&mut p, "NEGOTIATE_UNIX_FD", "G");
        assert!(p.fds_ok, "the peer agreed to SCM_RIGHTS");
    }

    /// The handshake is lines, and a client that sends no newline is either
    /// attacking or broken; either way the daemon stops buffering for it long
    /// before a message-sized buffer. 16 KiB is far more than any auth line.
    #[test]
    fn an_auth_line_that_never_ends_is_refused() {
        let mut b = Bus::new("G".into(), "0".repeat(32));
        let mut p = Peer::detached();
        p.auth = Auth::Lines;
        p.inbuf = vec![b'A'; MAX_AUTH_LINE];
        // Still inside the ceiling: nothing to do yet, and no error.
        assert!(consume(1, &mut p, &mut b, "G").is_ok());
        p.inbuf = vec![b'A'; 16385];
        let e = consume(1, &mut p, &mut b, "G").unwrap_err();
        assert!(e.contains("auth line too long"), "{e}");
    }

    /// A message is handed the descriptors IT declares, not every one the
    /// peer happens to be holding: the peer may have pipelined several
    /// messages and the ones behind this message's own are not its.
    #[test]
    fn a_message_takes_only_the_descriptors_it_declared() {
        let mut b = Bus::new("G".into(), "0".repeat(32));
        let mut p = Peer::detached();
        p.auth = Auth::Done;
        // Three descriptors waiting, a message that declares one.
        for _ in 0..3 {
            p.recv_fds.push_back(unsafe { libc::dup(2) });
        }
        let mut m = Message::signal("/org/a", "org.example", "Unwatched");
        m.serial = 1;
        m.unix_fds = 1;
        p.inbuf = m.encode();

        b.add_connection(1, 101, 0);
        let mut hello = Message::method_call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "Hello",
        );
        hello.serial = 1;
        b.dispatch(1, hello);
        b.outbox.clear();

        consume(1, &mut p, &mut b, "G").unwrap();
        assert!(p.inbuf.is_empty(), "the whole message was consumed");
        assert_eq!(
            p.recv_fds.len(),
            2,
            "one descriptor went with the message and two stayed"
        );
    }

    /// A chunk goes out once and the daemon lets go of the descriptors that
    /// rode with it: they belong to the receiver now, and a daemon that keeps
    /// its copies is a session bus that runs out of descriptors.
    #[test]
    fn a_flush_writes_a_chunk_once_and_lets_go_of_its_descriptors() {
        let (mut p, other) = socket_pair();
        let spare = unsafe { libc::dup(2) };
        p.queue(b"hello".to_vec(), vec![spare]);
        assert!(flush(&mut p), "the peer is still there");
        assert!(p.out.is_empty(), "a written chunk is popped, not retried");
        assert_eq!(p.out_done, 0, "and the cursor is back to the start");
        assert!(
            !is_open(spare),
            "the daemon's copy of the descriptor is gone"
        );

        let mut buf = [0u8; 16];
        let n = unsafe { libc::read(other, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        assert_eq!(&buf[..n.max(0) as usize], b"hello");
        unsafe { libc::close(other) };
    }

    /// A chunk too big for the socket buffer goes out over several writes,
    /// and each one carries on from where the last stopped. A cursor that is
    /// set rather than advanced re-sends the bytes already gone, and the
    /// client decodes the overlap as a message that was never sent.
    #[test]
    fn a_partially_written_chunk_goes_on_from_where_it_stopped() {
        let (mut p, other) = socket_pair();
        // Bigger than any default socket buffer, so one write cannot finish.
        let big: Vec<u8> = (0..400_000u32).map(|i| i as u8).collect();
        p.queue(big.clone(), Vec::new());
        assert!(flush(&mut p));
        let first = p.out_done;
        assert!(
            first > 0 && first < big.len(),
            "the first write was partial: {first} of {}",
            big.len()
        );

        let mut got: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; 65536];
        for _ in 0..500 {
            loop {
                let n =
                    unsafe { libc::read(other, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if n <= 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n as usize]);
            }
            if p.out.is_empty() {
                break;
            }
            assert!(flush(&mut p));
        }
        assert!(p.out.is_empty(), "the chunk went out in full");
        assert_eq!(got.len(), big.len(), "no byte was sent twice");
        assert_eq!(got, big, "and they arrived in order");
        unsafe { libc::close(other) };
    }

    /// Everything a peer is still holding when it goes belongs to nobody, so
    /// `Drop` closes it: the descriptors it received and never handed on, and
    /// the ones queued for a write that will not happen now.
    #[test]
    fn a_peer_that_goes_closes_every_descriptor_it_was_holding() {
        let received = unsafe { libc::dup(2) };
        let queued = unsafe { libc::dup(2) };
        let own = {
            let (mut p, other) = socket_pair();
            unsafe { libc::close(other) };
            p.recv_fds.push_back(received);
            p.queue(b"never sent".to_vec(), vec![queued]);
            p.fd
        };
        assert!(!is_open(received), "a descriptor nobody claimed is closed");
        assert!(
            !is_open(queued),
            "so is one queued for a write that never ran"
        );
        assert!(!is_open(own), "and the socket itself");
    }

    /// `Ok(false)` is how the read path says the peer closed its end. Read it
    /// as "nothing to do" and the daemon keeps a dead connection in its poll
    /// set, which then reports readable for ever.
    #[test]
    fn a_closed_peer_is_reported_as_gone() {
        let (mut p, other) = socket_pair();
        unsafe { libc::close(other) };
        assert!(!read_into(&mut p).unwrap(), "the peer is gone");

        // Whereas a peer with nothing to say is still there.
        let (mut q, other2) = socket_pair();
        assert!(read_into(&mut q).unwrap(), "no data is not a goodbye");
        unsafe { libc::close(other2) };
    }

    /// A buffer at the message size limit is not over it, and the limit is
    /// what keeps a client from making the daemon hold an unbounded buffer.
    #[test]
    fn the_input_buffer_may_reach_the_message_limit_but_not_pass_it() {
        let (mut p, other) = socket_pair();
        p.inbuf = vec![0u8; MAX_MESSAGE_LEN];
        assert!(read_into(&mut p).is_ok(), "exactly at the limit is allowed");
        p.inbuf = vec![0u8; MAX_MESSAGE_LEN + 1];
        let e = read_into(&mut p).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
        unsafe { libc::close(other) };
    }

    /// A read that fills the buffer means there may be more behind it, so the
    /// daemon reads again rather than waiting for the next poll. Stopping on
    /// a full buffer leaves a whole message unread until something else
    /// happens on the socket -- which, for a client waiting on a reply to it,
    /// is never.
    #[test]
    fn a_read_that_fills_the_buffer_is_followed_by_another() {
        let (mut p, other) = socket_pair();
        // One chunk's worth and a little more, so the first read comes back
        // full: a stream socket hands over as much as it has.
        let payload = vec![b'x'; 8192 + 100];
        let n = unsafe {
            libc::send(
                other,
                payload.as_ptr() as *const libc::c_void,
                payload.len(),
                0,
            )
        };
        assert_eq!(n, payload.len() as isize, "the whole payload was buffered");
        assert!(read_into(&mut p).unwrap());
        assert_eq!(
            p.inbuf.len(),
            payload.len(),
            "it kept reading until the socket said no more"
        );
        unsafe { libc::close(other) };
    }

    /// Descriptors come out in the order they arrived, because `consume`
    /// hands the oldest to the oldest message. Reverse them and a message is
    /// given somebody else's descriptors -- which it then uses.
    #[test]
    fn descriptors_are_queued_in_the_order_they_arrived() {
        let (mut p, other) = socket_pair();
        let mut a = [0 as libc::c_int; 2];
        let mut b = [0 as libc::c_int; 2];
        assert_eq!(unsafe { libc::pipe(a.as_mut_ptr()) }, 0);
        assert_eq!(unsafe { libc::pipe(b.as_mut_ptr()) }, 0);

        // The two write ends, A first.
        assert!(send_each_fd(other, b"..", &[a[1], b[1]]));
        assert!(read_into(&mut p).unwrap());
        assert_eq!(p.recv_fds.len(), 2, "both descriptors arrived");

        // Write through the FIRST of them: it has to come out of pipe A.
        let first = p.recv_fds[0];
        let one = *b"1";
        assert_eq!(
            unsafe { libc::write(first, one.as_ptr() as *const libc::c_void, 1) },
            1
        );
        let mut got = [0u8; 1];
        set_nonblocking(a[0]);
        set_nonblocking(b[0]);
        assert_eq!(
            unsafe { libc::read(a[0], got.as_mut_ptr() as *mut libc::c_void, 1) },
            1,
            "the first descriptor received is the first one sent"
        );
        assert_eq!(got[0], b'1');

        for fd in [other, a[0], a[1], b[0], b[1]] {
            unsafe { libc::close(fd) };
        }
    }

    /// The descriptor queue's ceiling is a ceiling: at it the peer stays, one
    /// past it the peer goes. Nothing consumes these before a whole message
    /// decodes, so a client that attaches descriptors to bytes that never
    /// complete one has the daemon hold them for as long as it is connected.
    #[test]
    fn the_descriptor_queue_overflows_one_past_its_ceiling() {
        assert!(!fd_queue_overflowed(0));
        assert!(!fd_queue_overflowed(MAX_QUEUED_FDS));
        assert!(fd_queue_overflowed(MAX_QUEUED_FDS + 1));
    }

    /// Each message in the outbox gets ITS OWN parked descriptors. They are
    /// keyed by the message's index, and handing every message the first
    /// entry gives one peer another peer's descriptors and leaves the rest
    /// stranded in a map nobody drains.
    #[test]
    fn each_message_is_handed_the_descriptors_parked_for_it() {
        let (p1, o1) = socket_pair();
        let (p2, o2) = socket_pair();
        let mut peers = vec![(1u64, p1), (2u64, p2)];
        for (_, p) in peers.iter_mut() {
            p.fds_ok = true;
        }
        let first = unsafe { libc::dup(2) };
        let second = unsafe { libc::dup(2) };
        PENDING_FDS.with(|p| {
            let mut m = p.borrow_mut();
            m.insert(0, vec![first]);
            m.insert(1, vec![second]);
        });

        let mut b = Bus::new("G".into(), "0".repeat(32));
        let mut m = Message::signal("/org/a", "org.example", "Carries");
        m.serial = 1;
        m.unix_fds = 1;
        b.outbox.push((1, m.clone()));
        b.outbox.push((2, m));
        drain_outbox(&mut b, &mut peers);

        assert_eq!(peers[0].1.out[0].fds, vec![first], "message 0's own");
        assert_eq!(peers[1].1.out[0].fds, vec![second], "message 1's own");
        for fd in [o1, o2] {
            unsafe { libc::close(fd) };
        }
    }

    /// Descriptors are attached to the copies the dispatch produced, which
    /// means the ones that still DECLARE descriptors. The bus's own replies
    /// declare none, so a filter that takes everything hands an error message
    /// the descriptors of the call it is refusing.
    #[test]
    fn only_a_copy_that_declares_descriptors_is_given_any() {
        let mut b = Bus::new("G".into(), "0".repeat(32));
        b.add_connection(1, 101, 0);
        let mut hello = Message::method_call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "Hello",
        );
        hello.serial = 1;
        b.dispatch(1, hello);
        b.outbox.clear();
        PENDING_FDS.with(|p| p.borrow_mut().clear());

        // Something already queued for another peer, which also declares a
        // descriptor. Only the copies THIS dispatch produced are carriers:
        // count from the start of the outbox and the descriptors are parked
        // against a message that was handed over before this one arrived.
        let mut earlier = Message::signal("/org/a", "org.example", "Earlier");
        earlier.serial = 1;
        earlier.unix_fds = 1;
        b.outbox.push((9, earlier));

        // A call to a name nobody owns: the bus answers with an error, which
        // carries no descriptors of its own.
        let mut m = Message::method_call("org.example.Nobody", "/org/a", "org.example", "Do");
        m.serial = 2;
        m.unix_fds = 1;
        let spare = unsafe { libc::dup(2) };
        route(1, m, vec![spare], &mut b);

        assert_eq!(b.outbox.len(), 2, "the bus answered");
        assert!(b.outbox[1].1.error_name.is_some(), "with an error");
        let parked = PENDING_FDS.with(|p| p.borrow().len());
        assert_eq!(parked, 0, "and the error was given no descriptors");
        assert!(
            !is_open(spare),
            "a descriptor with nowhere to go is closed, not leaked"
        );
    }

    /// Every copy that carries descriptors gets its OWN: the first the
    /// originals and the rest duplicates. Hand the originals to more than one
    /// peer and the same descriptor number is closed twice -- the second
    /// close hitting whatever the daemon opened in between.
    #[test]
    fn every_carrier_is_given_its_own_descriptor() {
        let mut b = Bus::new("G".into(), "0".repeat(32));
        for id in [1u64, 2, 3, 4] {
            b.add_connection(id, 100 + id as u32, 0);
            let mut hello = Message::method_call(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "Hello",
            );
            hello.serial = 1;
            b.dispatch(id, hello);
        }
        // :1.2 owns the destination; :1.3 and :1.4 watch the calls.
        let mut req = Message::method_call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "RequestName",
        );
        req.serial = 2;
        req.set_body(&[
            crate::message::Arg::Str("org.example.Dest".into()),
            crate::message::Arg::U32(0),
        ]);
        b.dispatch(2, req);
        for id in [3u64, 4] {
            let mut add = Message::method_call(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "AddMatch",
            );
            add.serial = 3;
            add.set_body(&[crate::message::Arg::Str("type='method_call'".into())]);
            b.dispatch(id, add);
        }
        b.outbox.clear();
        PENDING_FDS.with(|p| p.borrow_mut().clear());

        let mut m = Message::method_call("org.example.Dest", "/org/a", "org.example", "Do");
        m.serial = 4;
        m.unix_fds = 1;
        let spare = unsafe { libc::dup(2) };
        route(1, m, vec![spare], &mut b);

        let parked: Vec<Vec<RawFd>> = PENDING_FDS.with(|p| p.borrow().values().cloned().collect());
        assert_eq!(parked.len(), 3, "three copies declare a descriptor");
        let mut all: Vec<RawFd> = parked.into_iter().flatten().collect();
        let before = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(
            all.len(),
            before,
            "no descriptor number was handed to two peers"
        );
        for fd in all {
            unsafe { libc::close(fd) };
        }
    }

    /// A session bus belongs to one uid and refusing the rest is its whole
    /// access control. A system bus is the opposite: it runs as root
    /// precisely so clients under other uids can reach it, and the same check
    /// there turns the socket it just created into a closed door --- which is
    /// what had pulseaudio logging a failure to reach the system bus on every
    /// boot.
    #[test]
    fn a_session_bus_keeps_to_its_own_uid_and_a_system_bus_does_not() {
        assert!(accepts_uid(false, 1000, 1000), "its own uid");
        assert!(!accepts_uid(false, 1001, 1000), "anybody else's");
        assert!(accepts_uid(true, 1001, 0), "a system bus takes every uid");
        assert!(accepts_uid(true, 0, 0));
    }

    /// And the modes follow the same split: a session bus shuts its directory
    /// and socket to everybody else, a system bus opens both.
    #[test]
    fn the_socket_modes_follow_which_bus_it_is() {
        assert_eq!(socket_modes(false), (0o700, 0o600), "session");
        assert_eq!(socket_modes(true), (0o755, 0o666), "system");
    }

    /// A socket file left over from a previous run is removed; one with a
    /// live bus behind it stops this one starting. Get it the wrong way round
    /// and either the daemon never starts, or it unlinks the socket the
    /// running bus is serving and the whole session loses its bus.
    #[test]
    fn a_leftover_socket_is_cleared_and_a_live_one_is_not() {
        let dir = std::env::temp_dir().join(format!("dbusd-stale-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let stale = dir.join("stale");
        let live = dir.join("live");

        // Nothing there at all: nothing to do.
        assert!(clear_stale_socket(&dir.join("absent").to_string_lossy()).is_ok());

        // A plain file nothing is listening on is a leftover, and goes.
        fs::write(&stale, b"not a socket").unwrap();
        assert!(clear_stale_socket(&stale.to_string_lossy()).is_ok());
        assert!(!stale.exists(), "the leftover was removed");

        // A live listener is refused, and still there afterwards.
        let listener = UnixListener::bind(&live).unwrap();
        let e = clear_stale_socket(&live.to_string_lossy()).unwrap_err();
        assert!(e.contains("already listening"), "{e}");
        assert!(live.exists(), "a live bus's socket is left alone");
        drop(listener);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A removal that fails has to be reported rather than swallowed. Letting
    /// it through means walking straight into `bind()`, whose EADDRINUSE names
    /// neither the path nor the reason -- and is the same answer a live bus
    /// gives, which is the one thing this function exists to tell apart. A
    /// read-only /run and a socket owned by another uid both land here; a
    /// directory is how a test reaches it without changing who it is.
    #[test]
    fn a_stale_socket_that_cannot_be_removed_says_so_instead_of_failing_at_bind() {
        let dir = std::env::temp_dir().join(format!("dbusd-unremovable-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let blocked = dir.join("bus");
        fs::create_dir_all(&blocked).unwrap();

        let e = clear_stale_socket(&blocked.to_string_lossy()).unwrap_err();
        assert!(e.contains("stale socket"), "{e}");
        assert!(
            e.contains(&blocked.to_string_lossy().to_string()),
            "the message has to name the path someone has to go and look at: {e}"
        );
        assert!(blocked.exists(), "and nothing was removed");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The machine id is 32 hex digits and nothing else, so a file holding
    /// anything else is not an id: the next path is tried and then the
    /// all-zeros fallback. Clients do validate it, and one that reads a
    /// 33-character id refuses the bus.
    #[test]
    fn a_machine_id_is_thirty_two_hex_digits_or_it_is_not_one() {
        for (text, want) in [
            ("0123456789abcdef0123456789abcdef", true),
            // The trailing newline systemd writes is not part of it.
            ("0123456789abcdef0123456789abcdef\n", true),
            ("  0123456789abcdef0123456789abcdef  \n", true),
            // Upper case is folded down: dbus asks for lower.
            ("0123456789ABCDEF0123456789ABCDEF", true),
            ("0123456789abcdef0123456789abcde", false),
            ("0123456789abcdef0123456789abcdeff", false),
            ("0123456789abcdef0123456789abcdeg", false),
            ("", false),
        ] {
            assert_eq!(
                machine_id_of(text).is_some(),
                want,
                "machine id from {text:?}"
            );
        }
        assert_eq!(
            machine_id_of("0123456789ABCDEF0123456789ABCDEF").as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
    }

    /// The paths are tried in order and the first one that holds a real id
    /// wins; a file that holds rubbish is skipped rather than believed, and
    /// with nothing to read the answer is the all-zeros id.
    #[test]
    fn the_machine_id_falls_through_the_paths_in_order() {
        let dir = std::env::temp_dir().join(format!("dbusd-mid-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let first = dir.join("first");
        let second = dir.join("second");
        let fp = first.to_string_lossy().into_owned();
        let sp = second.to_string_lossy().into_owned();

        assert_eq!(read_machine_id_from(&[&fp, &sp]), NO_MACHINE_ID);

        fs::write(&second, b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n").unwrap();
        assert_eq!(
            read_machine_id_from(&[&fp, &sp]),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "the second path is tried when the first is missing"
        );

        fs::write(&first, b"not an id at all").unwrap();
        assert_eq!(
            read_machine_id_from(&[&fp, &sp]),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "a file holding rubbish is skipped, not believed"
        );

        fs::write(&first, b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        assert_eq!(
            read_machine_id_from(&[&fp, &sp]),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "and the first path wins when it has one"
        );

        // Both of the real paths are the ones dbus itself looks at.
        assert_eq!(MACHINE_ID_PATHS[0], "/etc/machine-id");
        assert_eq!(MACHINE_ID_PATHS[1], "/var/lib/dbus/machine-id");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The GUID has to differ between two buses on one host, and between two
    /// hosts: clients tell one bus instance from another by it across a
    /// reconnect. Neither half is visible from a single GUID, which is why
    /// the clock and the pid are arguments here.
    #[test]
    fn the_guid_separates_two_buses_and_two_hosts() {
        const ID: &str = "0123456789abcdef0123456789abcdef";
        assert_eq!(guid_from(1, 1, ID).len(), 32);
        assert!(guid_from(1, 1, ID).bytes().all(|b| b.is_ascii_hexdigit()));

        // Two buses started at different times, and two at the same time by
        // different processes.
        assert_ne!(guid_from(1, 7, ID), guid_from(2, 7, ID));
        assert_ne!(guid_from(1, 7, ID), guid_from(1, 8, ID));
        // The pid goes into the HIGH half of the seed, where the clock's
        // nanoseconds do not reach. Fold it in at the bottom instead and a
        // bus started at nanosecond 0 by pid 1 gets the same GUID as one
        // started at nanosecond 1 by pid 0.
        assert_ne!(guid_from(0, 1, ID), guid_from(1, 0, ID));

        // Two hosts, same instant and pid: the machine id is folded in.
        let other = format!("ffffffff{}", &ID[8..]);
        assert_ne!(guid_from(5, 5, ID), guid_from(5, 5, &other));

        // And folded in with XOR, which can clear bits as well as set them.
        // OR could only set them, so two hosts whose ids differ in bits the
        // GUID already has would be handed the same one.
        let plain = guid_from(5, 5, "");
        let low = &plain[8..16];
        let self_cancelling = format!("{low}{}", "0".repeat(24));
        assert_eq!(self_cancelling.len(), 32);
        assert_eq!(
            &guid_from(5, 5, &self_cancelling)[8..16],
            "00000000",
            "a value exclusive-ored with itself is zero"
        );
    }
}
