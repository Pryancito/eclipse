//! The stream sockets of the stack: the connections the kernel asks for a
//! program, and the ones that arrive for a program that listens.
//!
//! A connection is a smoltcp socket here and a count on each side of what
//! the other still takes, so that neither holds more than it said it
//! would (`linux-object/src/net/user_stack.rs` says it at length):
//!
//! ```text
//! CONNECT  (kernel to stack)  16  [u8; 4] to  20  u16 port  22  u16 local port  24  u32 window
//! LISTEN   (kernel to stack)  16  u16 port  18  u16 backlog  20  u32 window
//! WRITE    (kernel to stack)  16  the bytes
//! SHUTDOWN (kernel to stack)
//! WINDOW   (kernel to stack)  16  u32 bytes
//! EVENTS   (stack to kernel)  16  one record after another, to the end:
//!              0  u64 socket  8  u16 what  10  u16 length  12  u32 number  16  `length` bytes
//!     CONNECTED  number: the bytes the stack takes
//!     FAILED     number: why (refused, timed out, reset, unreachable, no room)
//!     ACCEPTED   socket: the listener.  number: the bytes the stack takes.
//!                bytes: u64 the new socket, [u8; 4] from, u16 port, u16 zero
//!     BYTES      bytes: what arrived
//!     EOF        the peer will send no more
//!     CREDIT     number: more bytes the stack takes
//! ```
//!
//! An `EVENTS` carries what a turn of the stack made of every connection,
//! as a `DATA` carries its datagrams: the kernel is woken once.

use std::collections::BTreeMap;

use smoltcp::phy::Device;
use smoltcp::socket::{SocketHandle, TcpSocket, TcpSocketBuffer, TcpState};
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address};

use super::{Arrived, Refused, Stack, HEADER_LEN, MESSAGE_MAX, RECORD_LEN};

pub const OP_CONNECT: u32 = 5;
pub const OP_LISTEN: u32 = 6;
pub const OP_WRITE: u32 = 7;
pub const OP_SHUTDOWN: u32 = 8;
pub const OP_WINDOW: u32 = 9;
pub const OP_EVENTS: u32 = 10;

/// What a record of an `EVENTS` says.
pub const EV_CONNECTED: u16 = 1;
pub const EV_FAILED: u16 = 2;
pub const EV_ACCEPTED: u16 = 3;
pub const EV_BYTES: u16 = 4;
pub const EV_EOF: u16 = 5;
pub const EV_CREDIT: u16 = 6;

/// Why a connection `FAILED`.
pub const WHY_REFUSED: u32 = 1;
pub const WHY_TIMED_OUT: u32 = 2;
pub const WHY_RESET: u32 = 3;
pub const WHY_UNREACHABLE: u32 = 4;
pub const WHY_NO_ROOM: u32 = 5;

/// Set in the number of a connection that arrived on a listener: the
/// stack numbers those, and the kernel every other socket.
pub const ACCEPTED_BIT: u64 = 1 << 63;
/// The bytes of an `ACCEPTED` record.
pub const ACCEPTED_LEN: usize = 16;

/// What a connection holds that the peer sent and the kernel has no room
/// for yet. With the kernel's own window, what the peer can send to a
/// program that does not read.
const RX_BYTES: usize = 256 * 1024;
/// What a connection holds that the program wrote and the peer has not
/// acknowledged: the credit the kernel starts with.
const TX_BYTES: usize = 256 * 1024;
/// How long a connect is tried for, as the kernel's stack tries.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(30_000);
/// A connect that ended this long before its time was refused; later, it
/// timed out.
const CONNECT_SLACK: Duration = Duration::from_millis(1_000);
/// How long a connection the program closed is given to say goodbye,
/// before it is reset.
const LINGER: Duration = Duration::from_millis(30_000);
/// The most connections a listener has ready to be made at once, whatever
/// backlog it asks for: each is a socket with its buffers.
const BACKLOG_MAX: usize = 16;
/// The most stream sockets of every kind.
const STREAMS_MAX: usize = 1024;
/// Handshakes one listener has in the middle at most: past this, who
/// knocks is refused until one of them finishes or is given up on.
const HALF_MADE_MAX: usize = 64;

/// A connection.
pub(super) struct Conn {
    handle: SocketHandle,
    /// The bytes the kernel still takes.
    window: usize,
    /// The bytes the kernel may still send, as the kernel counts them.
    credit: usize,
    /// A connect under way, and since when (the first turn that saw it).
    connecting: Option<Option<Instant>>,
    /// `EOF` was said.
    eof: bool,
    /// The state the turn before left it in: a connection that is closed
    /// and was not on its way there was reset.
    seen: TcpState,
}

/// A socket that takes connections.
pub(super) struct Listener {
    port: u16,
    backlog: usize,
    /// The window of each connection it makes.
    window: usize,
    /// The sockets that listen, or are in the middle of a handshake.
    ready: Vec<SocketHandle>,
}

/// A connection the program closed, saying goodbye on its own.
pub(super) struct Closing {
    handle: SocketHandle,
    /// When it is reset; set by the first turn that sees it.
    until: Option<Instant>,
    /// It was reset: the next turn forgets it.
    reset: bool,
}

/// The stream sockets of a stack.
#[derive(Default)]
pub(super) struct Streams {
    conns: BTreeMap<u64, Conn>,
    listeners: BTreeMap<u64, Listener>,
    closing: Vec<Closing>,
    /// What a message from the kernel left to say: a connect that could
    /// not be started.
    owed: Vec<(u64, u16, u32)>,
    /// The number of the last connection that arrived.
    accepted: u64,
}

impl Streams {
    pub(super) fn is_empty(&self) -> bool {
        self.conns.is_empty()
            && self.listeners.is_empty()
            && self.closing.is_empty()
            && self.owed.is_empty()
    }

    pub(super) fn knows(&self, sock: u64) -> bool {
        self.conns.contains_key(&sock) || self.listeners.contains_key(&sock)
    }

    /// Every smoltcp socket there is here.
    fn sockets(&self) -> usize {
        self.conns.len()
            + self.closing.len()
            + self
                .listeners
                .values()
                .map(|l| l.ready.len())
                .sum::<usize>()
    }
}

/// Make `m` an `EVENTS` with no records yet.
pub fn start_events(m: &mut Vec<u8>) {
    m.clear();
    m.extend_from_slice(&0u32.to_le_bytes());
    m.extend_from_slice(&OP_EVENTS.to_le_bytes());
    m.extend_from_slice(&0u64.to_le_bytes());
}

/// Add a record to the `EVENTS` message `m`.
pub fn push_event(m: &mut Vec<u8>, sock: u64, what: u16, number: u32, bytes: &[u8]) {
    m.extend_from_slice(&sock.to_le_bytes());
    m.extend_from_slice(&what.to_le_bytes());
    m.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    m.extend_from_slice(&number.to_le_bytes());
    m.extend_from_slice(bytes);
}

/// Whether a record with `bytes` bytes still fits `m`.
fn fits(m: &[u8], bytes: usize) -> bool {
    m.len() + RECORD_LEN + bytes <= MESSAGE_MAX
}

fn socket() -> TcpSocket<'static> {
    let mut socket = TcpSocket::new(
        TcpSocketBuffer::new(vec![0u8; RX_BYTES]),
        TcpSocketBuffer::new(vec![0u8; TX_BYTES]),
    );
    // As the kernel's stack: an acknowledgement held back is a peer whose
    // window opens late.
    socket.set_ack_delay(None);
    socket
}

/// A socket that takes a connection to `port`. A handshake that starts
/// on it and is not finished is given up on in the time a connect is.
fn listening(port: u16) -> Option<TcpSocket<'static>> {
    let mut socket = socket();
    socket.listen(port).ok()?;
    socket.set_timeout(Some(CONNECT_TIMEOUT));
    Some(socket)
}

fn u16_at(m: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([m[at], m[at + 1]])
}

fn u32_at(m: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([m[at], m[at + 1], m[at + 2], m[at + 3]])
}

impl<D: for<'d> Device<'d>> Stack<D> {
    /// `CONNECT`. A connect that cannot be started is answered `FAILED`
    /// by the next turn, which is why this asks for one.
    pub(super) fn connect(&mut self, sock: u64, m: &[u8]) -> Result<bool, Refused> {
        if m.len() != HEADER_LEN + 12 {
            return Err(Refused::Malformed);
        }
        if self.knows(sock) {
            return Err(Refused::Bind);
        }
        let addr = Ipv4Address::from_bytes(&m[16..20]);
        let (port, local) = (u16_at(m, 20), u16_at(m, 22));
        let window = u32_at(m, 24) as usize;
        if port == 0 || local == 0 {
            return Err(Refused::Malformed);
        }
        // Nowhere to send it (no gateway for what is off the card's
        // network), and nobody to connect to at everybody's address.
        if addr.is_broadcast() || !self.reaches(addr) {
            self.streams.owed.push((sock, EV_FAILED, WHY_UNREACHABLE));
            return Ok(true);
        }
        if self.streams.sockets() >= STREAMS_MAX {
            self.streams.owed.push((sock, EV_FAILED, WHY_NO_ROOM));
            return Ok(true);
        }
        let mut socket = socket();
        let to = IpEndpoint::new(IpAddress::Ipv4(addr), port);
        if socket.connect(to, local).is_err() {
            return Err(Refused::Malformed);
        }
        // After the connect, which forgets the one before.
        socket.set_timeout(Some(CONNECT_TIMEOUT));
        let seen = socket.state();
        let handle = self.set.add(socket);
        self.streams.conns.insert(
            sock,
            Conn {
                handle,
                window,
                credit: 0,
                connecting: Some(None),
                eof: false,
                seen,
            },
        );
        Ok(true)
    }

    /// `LISTEN`.
    pub(super) fn listen(&mut self, sock: u64, m: &[u8]) -> Result<bool, Refused> {
        if m.len() != HEADER_LEN + 8 {
            return Err(Refused::Malformed);
        }
        let port = u16_at(m, 16);
        if port == 0 || self.knows(sock) {
            return Err(Refused::Bind);
        }
        let listener = Listener {
            port,
            backlog: (u16_at(m, 18) as usize).clamp(1, BACKLOG_MAX),
            window: u32_at(m, 20) as usize,
            ready: Vec::new(),
        };
        self.streams.listeners.insert(sock, listener);
        self.replenish();
        Ok(false)
    }

    /// `WRITE`: the bytes go on the connection's way out. The kernel
    /// counted them against what it was told there is room for.
    pub(super) fn write(&mut self, sock: u64, m: &[u8]) -> Result<bool, Refused> {
        let bytes = &m[HEADER_LEN..];
        let conn = self.streams.conns.get_mut(&sock).ok_or(Refused::NoSocket)?;
        if bytes.len() > conn.credit {
            return Err(Refused::Full);
        }
        conn.credit -= bytes.len();
        // A connection that was reset a moment ago takes nothing, and the
        // kernel is about to hear of it.
        let sent = self.set.get::<TcpSocket>(conn.handle).send_slice(bytes);
        Ok(matches!(sent, Ok(n) if n > 0))
    }

    /// `SHUTDOWN`: no more will be written, which the peer is told once
    /// what was written is out.
    pub(super) fn shutdown(&mut self, sock: u64) -> Result<bool, Refused> {
        let conn = self.streams.conns.get_mut(&sock).ok_or(Refused::NoSocket)?;
        let mut socket = self.set.get::<TcpSocket>(conn.handle);
        socket.close();
        conn.seen = socket.state();
        Ok(true)
    }

    /// `WINDOW`: the kernel takes more. A turn, when there is something
    /// waiting to be taken.
    pub(super) fn window(&mut self, sock: u64, m: &[u8]) -> Result<bool, Refused> {
        if m.len() != HEADER_LEN + 4 {
            return Err(Refused::Malformed);
        }
        let conn = self.streams.conns.get_mut(&sock).ok_or(Refused::NoSocket)?;
        conn.window = conn.window.saturating_add(u32_at(m, 16) as usize);
        Ok(self.set.get::<TcpSocket>(conn.handle).recv_queue() > 0)
    }

    /// `CLOSE` of a stream socket: `None` when `sock` is not one.
    pub(super) fn close_stream(&mut self, sock: u64) -> Option<bool> {
        if let Some(listener) = self.streams.listeners.remove(&sock) {
            // Whoever was in the middle of connecting, or had just
            // connected and was not told of yet, is reset.
            let mut resets = false;
            for handle in listener.ready {
                let mut socket = self.set.get::<TcpSocket>(handle);
                if socket.state() == TcpState::Listen {
                    drop(socket);
                    self.set.remove(handle);
                } else {
                    socket.abort();
                    self.streams.closing.push(Closing {
                        handle,
                        until: None,
                        reset: false,
                    });
                    resets = true;
                }
            }
            return Some(resets);
        }
        let conn = self.streams.conns.remove(&sock)?;
        let mut socket = self.set.get::<TcpSocket>(conn.handle);
        if socket.state() == TcpState::Closed && !socket.remote_endpoint().is_specified() {
            // Over already, with nothing left to say.
            drop(socket);
            self.set.remove(conn.handle);
            return Some(false);
        }
        // `tcp_close`: a program that closes with bytes it did not read
        // resets the connection; otherwise it ends as connections end,
        // while the program is somewhere else.
        if socket.recv_queue() > 0 {
            socket.abort();
        } else {
            socket.close();
        }
        self.streams.closing.push(Closing {
            handle: conn.handle,
            until: None,
            reset: false,
        });
        Some(true)
    }

    /// Give every listener the sockets it wants ready.
    fn replenish(&mut self) {
        let mut sockets = self.streams.sockets();
        for listener in self.streams.listeners.values_mut() {
            let mut listening = listener
                .ready
                .iter()
                .filter(|&&handle| self.set.get::<TcpSocket>(handle).state() == TcpState::Listen)
                .count();
            let most = listener.backlog + HALF_MADE_MAX;
            while listening < listener.backlog
                && listener.ready.len() < most
                && sockets < STREAMS_MAX
            {
                let Some(socket) = self::listening(listener.port) else {
                    break;
                };
                listener.ready.push(self.set.add(socket));
                listening += 1;
                sockets += 1;
            }
        }
    }

    /// Make `m` the `EVENTS` of what the last turn made of the stream
    /// sockets: all of it, or as much as fits one message.
    pub(super) fn events(&mut self, m: &mut Vec<u8>) -> Arrived {
        start_events(m);
        let now = self.now;
        let mut drained = false;
        let more = self.fill(m, now, &mut drained);
        self.brought |= drained;
        if !more {
            self.reap(now);
            self.replenish();
            // Bytes that left for the kernel may have opened the window
            // enough to tell the peer, and whoever waits for that waits
            // for this stack's next turn: it goes now.
            if drained && self.iface.poll_delay(&self.set, now) == Some(Duration::ZERO) {
                let _ = self.iface.poll(&mut self.set, now);
            }
        }
        if more {
            Arrived::More
        } else if m.len() > HEADER_LEN {
            Arrived::All
        } else {
            Arrived::Nothing
        }
    }

    /// The records of [`events`](Self::events). Whether there are more
    /// than fit.
    fn fill(&mut self, m: &mut Vec<u8>, now: Instant, drained: &mut bool) -> bool {
        while let Some(&(sock, what, number)) = self.streams.owed.first() {
            if !fits(m, 0) {
                return true;
            }
            push_event(m, sock, what, number, &[]);
            self.streams.owed.remove(0);
        }

        // The connections that were made on a listener. Their numbers are
        // after every number of the kernel's, so what arrived on one
        // follows it in this same message.
        for (&sock, listener) in self.streams.listeners.iter_mut() {
            let mut i = 0;
            while i < listener.ready.len() {
                let handle = listener.ready[i];
                let mut socket = self.set.get::<TcpSocket>(handle);
                let state = socket.state();
                match state {
                    TcpState::Listen | TcpState::SynReceived => {}
                    // A socket whose handshake was given up on listens
                    // again.
                    TcpState::Closed | TcpState::TimeWait => {
                        if socket.listen(listener.port).is_ok() {
                            socket.set_timeout(Some(CONNECT_TIMEOUT));
                        }
                    }
                    // Made. Also one whose peer closed it at once: the
                    // program accepts it and reads the end.
                    _ => {
                        if !fits(m, ACCEPTED_LEN) {
                            return true;
                        }
                        let from = socket.remote_endpoint();
                        let IpAddress::Ipv4(addr) = from.addr else {
                            socket.abort();
                            i += 1;
                            continue;
                        };
                        // From here it lasts as long as the peer answers.
                        socket.set_timeout(None);
                        self.streams.accepted += 1;
                        let new = ACCEPTED_BIT | self.streams.accepted;
                        let mut bytes = [0u8; ACCEPTED_LEN];
                        bytes[..8].copy_from_slice(&new.to_le_bytes());
                        bytes[8..12].copy_from_slice(&addr.0);
                        bytes[12..14].copy_from_slice(&from.port.to_le_bytes());
                        push_event(m, sock, EV_ACCEPTED, TX_BYTES as u32, &bytes);
                        listener.ready.swap_remove(i);
                        self.streams.conns.insert(
                            new,
                            Conn {
                                handle,
                                window: listener.window,
                                credit: TX_BYTES,
                                connecting: None,
                                eof: false,
                                seen: state,
                            },
                        );
                        continue;
                    }
                }
                i += 1;
            }
        }

        let mut gone = Vec::new();
        let mut more = false;
        for (&sock, conn) in self.streams.conns.iter_mut() {
            let mut socket = self.set.get::<TcpSocket>(conn.handle);
            let state = socket.state();
            if let Some(since) = conn.connecting.as_mut() {
                let since = *since.get_or_insert(now);
                match state {
                    TcpState::SynSent => continue,
                    TcpState::Closed => {
                        if !fits(m, 0) {
                            more = true;
                            break;
                        }
                        let why = if now + CONNECT_SLACK >= since + CONNECT_TIMEOUT {
                            WHY_TIMED_OUT
                        } else {
                            WHY_REFUSED
                        };
                        push_event(m, sock, EV_FAILED, why, &[]);
                        gone.push(sock);
                        continue;
                    }
                    _ => {
                        if !fits(m, 0) {
                            more = true;
                            break;
                        }
                        // Connected: from here it lasts as long as the
                        // peer answers, as a connection does.
                        socket.set_timeout(None);
                        conn.connecting = None;
                        conn.credit = TX_BYTES;
                        push_event(m, sock, EV_CONNECTED, TX_BYTES as u32, &[]);
                    }
                }
            }

            // Closed, and it was not closing: reset, by the peer.
            if state == TcpState::Closed
                && !matches!(
                    conn.seen,
                    TcpState::LastAck | TcpState::TimeWait | TcpState::Closed
                )
            {
                if !fits(m, 0) {
                    more = true;
                    break;
                }
                push_event(m, sock, EV_FAILED, WHY_RESET, &[]);
                gone.push(sock);
                continue;
            }
            conn.seen = state;

            // What arrived, as far as the kernel has room.
            while conn.window > 0 && socket.can_recv() {
                if !fits(m, 1) {
                    more = true;
                    break;
                }
                let room = (MESSAGE_MAX - m.len() - RECORD_LEN)
                    .min(conn.window)
                    .min(u16::MAX as usize);
                let took = socket.recv(|bytes| {
                    let n = bytes.len().min(room);
                    push_event(m, sock, EV_BYTES, 0, &bytes[..n]);
                    (n, n)
                });
                match took {
                    Ok(n) if n > 0 => {
                        conn.window -= n;
                        *drained = true;
                    }
                    _ => break,
                }
            }
            if more {
                break;
            }

            // The end, once everything before it went.
            let ended = matches!(
                state,
                TcpState::CloseWait
                    | TcpState::LastAck
                    | TcpState::Closing
                    | TcpState::TimeWait
                    | TcpState::Closed
            );
            if ended && !conn.eof && socket.recv_queue() == 0 {
                if !fits(m, 0) {
                    more = true;
                    break;
                }
                push_event(m, sock, EV_EOF, 0, &[]);
                conn.eof = true;
            }

            // What the peer acknowledged is room again; said a quarter of
            // the buffer at a time, not for every acknowledgement.
            if socket.may_send() {
                let free = TX_BYTES - socket.send_queue();
                let grant = free.saturating_sub(conn.credit);
                if grant >= TX_BYTES / 4 {
                    if !fits(m, 0) {
                        more = true;
                        break;
                    }
                    push_event(m, sock, EV_CREDIT, grant as u32, &[]);
                    conn.credit += grant;
                }
            }
        }
        for sock in gone {
            if let Some(conn) = self.streams.conns.remove(&sock) {
                self.set.remove(conn.handle);
            }
        }
        more
    }

    /// Forget the closed connections that have said goodbye, and reset
    /// the ones that took too long over it.
    fn reap(&mut self, now: Instant) {
        let set = &mut self.set;
        self.streams.closing.retain_mut(|closing| {
            let mut socket = set.get::<TcpSocket>(closing.handle);
            // TIME-WAIT has nothing left to say that is worth a socket.
            let over = closing.reset
                || socket.state() == TcpState::TimeWait
                || (socket.state() == TcpState::Closed && !socket.remote_endpoint().is_specified());
            if over {
                drop(socket);
                set.remove(closing.handle);
                return false;
            }
            let until = *closing.until.get_or_insert(now + LINGER);
            if now >= until {
                // One reset, which the next turn sends if it can.
                socket.abort();
                closing.reset = true;
            }
            true
        });
    }
}

#[cfg(test)]
mod tests {
    //! The stack against THEIRS, the machine at the other end of the
    //! bench's wire, which says its segments by hand.

    use super::*;
    use crate::port::testing::*;
    use crate::port::Port;
    use smoltcp::wire::TcpControl;
    use udrivers::net::OP_TX;

    /// An event as the kernel is told of it.
    type Told = (u64, u16, u32, Vec<u8>);

    /// A stack, the wire of its card, and the time.
    struct Wire {
        stack: Stack<Port>,
        window: Window,
        ms: i64,
    }

    fn message(op: u32, sock: u64, rest: &[u8]) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(&0u32.to_le_bytes());
        m.extend_from_slice(&op.to_le_bytes());
        m.extend_from_slice(&sock.to_le_bytes());
        m.extend_from_slice(rest);
        m
    }

    fn connect(sock: u64, to: Ipv4Address, port: u16, local: u16, window: u32) -> Vec<u8> {
        let mut rest = to.0.to_vec();
        rest.extend_from_slice(&port.to_le_bytes());
        rest.extend_from_slice(&local.to_le_bytes());
        rest.extend_from_slice(&window.to_le_bytes());
        message(OP_CONNECT, sock, &rest)
    }

    fn listen(sock: u64, port: u16, backlog: u16, window: u32) -> Vec<u8> {
        let mut rest = port.to_le_bytes().to_vec();
        rest.extend_from_slice(&backlog.to_le_bytes());
        rest.extend_from_slice(&window.to_le_bytes());
        message(OP_LISTEN, sock, &rest)
    }

    fn write(sock: u64, bytes: &[u8]) -> Vec<u8> {
        message(OP_WRITE, sock, bytes)
    }

    fn window(sock: u64, bytes: u32) -> Vec<u8> {
        message(OP_WINDOW, sock, &bytes.to_le_bytes())
    }

    fn close(sock: u64) -> Vec<u8> {
        message(super::super::OP_CLOSE, sock, &[])
    }

    fn told(sock: u64, what: u16, number: u32, bytes: &[u8]) -> Told {
        (sock, what, number, bytes.to_vec())
    }

    impl Wire {
        /// A stack over a bench's card, which THEIRS has met.
        fn new() -> Wire {
            let mut bench = Bench::new();
            bench.meet();
            let Bench { window, iface, .. } = bench;
            Wire {
                stack: Stack::new(iface),
                window,
                ms: 1,
            }
        }

        /// A turn of the stack: the segments it sent, which the card is
        /// done with, and what it tells the kernel.
        fn turn(&mut self) -> (Vec<Segment>, Vec<Told>) {
            let (mut sent, mut events) = (Vec::new(), Vec::new());
            loop {
                self.stack.poll(Instant::from_millis(self.ms));
                let mut m = Vec::new();
                loop {
                    let arrived = self.stack.arrived(&mut m);
                    if arrived == Arrived::Nothing {
                        break;
                    }
                    assert_eq!(super::super::parse_header(&m), Some((OP_EVENTS, 0)));
                    assert!(m.len() <= MESSAGE_MAX);
                    let mut rest = &m[HEADER_LEN..];
                    while !rest.is_empty() {
                        let sock = u64::from_le_bytes(rest[..8].try_into().unwrap());
                        let what = u16_at(rest, 8);
                        let len = u16_at(rest, 10) as usize;
                        let number = u32_at(rest, 12);
                        events.push((sock, what, number, rest[16..16 + len].to_vec()));
                        rest = &rest[16 + len..];
                    }
                    if arrived == Arrived::All {
                        break;
                    }
                }
                // The card sends what there is and gives the slots back,
                // which may let the stack send more.
                let mut any = false;
                for msg in turn(self.stack.device()) {
                    if msg.op == OP_TX {
                        sent.push(tcp_of(&self.window.sent(&msg)));
                        self.window.done(self.stack.device(), &msg);
                        any = true;
                    }
                }
                if !any {
                    return (sent, events);
                }
            }
        }

        /// THEIRS says `segment`, and the turn that makes.
        fn hears(&mut self, segment: &Segment) -> (Vec<Segment>, Vec<Told>) {
            self.window
                .deliver(self.stack.device(), 0, &tcp_frame(segment));
            self.turn()
        }

        /// The kernel asks `m`, which is done, and the turn that makes.
        fn asked(&mut self, m: &[u8]) -> (Vec<Segment>, Vec<Told>) {
            self.stack.request(m).unwrap();
            self.turn()
        }
    }

    /// THEIRS's side of one connection: its port, ours, and where each
    /// side's sequence numbers are.
    struct Peer {
        port: u16,
        ours: u16,
        seq: u32,
        ack: u32,
    }

    impl Peer {
        fn segment(&mut self, control: TcpControl, data: &[u8]) -> Segment {
            let segment = Segment {
                from: self.port,
                to: self.ours,
                control,
                seq: self.seq,
                ack: Some(self.ack),
                data: data.to_vec(),
            };
            self.seq = segment.end();
            segment
        }

        /// THEIRS has heard `segment` of ours.
        fn heard(&mut self, segment: &Segment) {
            assert_eq!((segment.from, segment.to), (self.ours, self.port));
            assert_eq!(segment.seq, self.ack, "in order");
            self.ack = segment.end();
        }
    }

    /// Socket `sock` of the kernel connected from port 50000 to THEIRS's
    /// port 80, with a window of `window` bytes.
    fn connected(wire: &mut Wire, sock: u64, window: u32) -> Peer {
        let (sent, events) = wire.asked(&connect(sock, THEIR_IP, 80, 50000, window));
        assert_eq!(events, []);
        assert_eq!(sent.len(), 1);
        let syn = &sent[0];
        assert_eq!(
            (syn.from, syn.to, syn.control, syn.ack),
            (50000, 80, TcpControl::Syn, None)
        );
        let mut peer = Peer {
            port: 80,
            ours: 50000,
            seq: 7000,
            ack: syn.end(),
        };
        let mut answer = peer.segment(TcpControl::Syn, &[]);
        answer.ack = Some(peer.ack);
        let (sent, events) = wire.hears(&answer);
        assert_eq!(events, [told(sock, EV_CONNECTED, TX_BYTES as u32, &[])]);
        assert_eq!(sent.len(), 1);
        assert_eq!(
            (sent[0].control, sent[0].ack),
            (TcpControl::None, Some(peer.seq))
        );
        peer
    }

    /// A connection from THEIRS's port `from` to a listener on 8080:
    /// THEIRS's side of it, and what the kernel is told as it is made.
    fn arrives(wire: &mut Wire, from: u16, first: &[u8]) -> (Peer, Vec<Told>) {
        let mut peer = Peer {
            port: from,
            ours: 8080,
            seq: 9000,
            ack: 0,
        };
        let mut syn = peer.segment(TcpControl::Syn, &[]);
        syn.ack = None;
        let (sent, events) = wire.hears(&syn);
        assert_eq!(events, [], "not a connection until the handshake is done");
        assert_eq!(sent.len(), 1);
        assert_eq!(
            (sent[0].control, sent[0].ack),
            (TcpControl::Syn, Some(peer.seq))
        );
        peer.ack = sent[0].end();
        let (_, events) = wire.hears(&peer.segment(TcpControl::None, first));
        (peer, events)
    }

    #[test]
    fn the_stream_messages_are_these_bytes() {
        // The same bytes `linux-object/src/net/user_stack/tcp.rs` writes
        // and reads: a field moved on one side fails on the other.
        assert_eq!(
            connect(7, Ipv4Address([10, 0, 9, 1]), 80, 49152, 0x0004_0000),
            [
                0, 0, 0, 0, 5, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, // header
                10, 0, 9, 1, 80, 0, 0, 0xc0, 0, 0, 4, 0,
            ]
        );
        assert_eq!(
            listen(7, 8080, 8, 0x0004_0000),
            [0, 0, 0, 0, 6, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0x90, 0x1f, 8, 0, 0, 0, 4, 0]
        );
        assert_eq!(
            write(7, b"hola"),
            [0, 0, 0, 0, 7, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, b'h', b'o', b'l', b'a']
        );
        assert_eq!(
            message(OP_SHUTDOWN, 7, &[]),
            [0, 0, 0, 0, 8, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            window(7, 0x0002_0000),
            [0, 0, 0, 0, 9, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0]
        );

        // And the stack reads them so: a connect from that port to that
        // one, a listener there, whose first connection is numbered 1.
        let mut wire = Wire::new();
        let (sent, _) = wire.asked(&connect(7, THEIR_IP, 80, 49152, 0x0004_0000));
        assert_eq!((sent[0].from, sent[0].to), (49152, 80));
        wire.stack
            .request(&listen(9, 8080, 8, 0x0004_0000))
            .unwrap();
        let (_, events) = arrives(&mut wire, 40000, b"hola");
        let mut m = Vec::new();
        start_events(&mut m);
        assert_eq!(m, [0, 0, 0, 0, 10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        for (sock, what, number, bytes) in &events {
            push_event(&mut m, *sock, *what, *number, bytes);
        }
        assert_eq!(
            m[HEADER_LEN..],
            [
                9, 0, 0, 0, 0, 0, 0, 0, 3, 0, 16, 0, 0, 0, 4, 0, // ACCEPTED
                1, 0, 0, 0, 0, 0, 0, 0x80, 10, 0, 9, 1, 0x40, 0x9c, 0, 0, //
                1, 0, 0, 0, 0, 0, 0, 0x80, 4, 0, 4, 0, 0, 0, 0, 0, // BYTES
                b'h', b'o', b'l', b'a',
            ]
        );
    }

    #[test]
    fn a_connect_is_a_handshake_and_then_bytes_both_ways() {
        let mut wire = Wire::new();
        let mut peer = connected(&mut wire, 5, 1000);
        // What the program writes leaves by the card.
        let (sent, events) = wire.asked(&write(5, b"GET / HTTP/1.0\r\n\r\n"));
        assert_eq!(events, []);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].data, b"GET / HTTP/1.0\r\n\r\n");
        peer.heard(&sent[0]);
        // What the peer says reaches the kernel, once: bytes for the
        // program, which the news of its socket were not.
        assert!(!wire.stack.brought_bytes());
        let (sent, events) = wire.hears(&peer.segment(TcpControl::Psh, b"HTTP/1.0 200 OK\r\n"));
        assert_eq!(events, [told(5, EV_BYTES, 0, b"HTTP/1.0 200 OK\r\n")]);
        assert!(wire.stack.brought_bytes());
        assert!(!wire.stack.brought_bytes(), "asked once");
        assert_eq!(sent.len(), 1, "acknowledged at once");
        assert_eq!(sent[0].ack, Some(peer.seq));
        assert_eq!(wire.turn(), (vec![], vec![]));
        // A connect or a listen for a socket that is one is not done.
        assert_eq!(
            wire.stack.request(&connect(5, THEIR_IP, 81, 50001, 1000)),
            Err(Refused::Bind)
        );
        assert_eq!(
            wire.stack.request(&listen(5, 8080, 1, 1000)),
            Err(Refused::Bind)
        );
    }

    #[test]
    fn a_connect_nobody_takes_fails_and_says_why() {
        let mut wire = Wire::new();
        // Refused: the peer resets it.
        let (sent, _) = wire.asked(&connect(5, THEIR_IP, 81, 50000, 1000));
        let reset = Segment {
            from: 81,
            to: 50000,
            control: TcpControl::Rst,
            seq: 0,
            ack: Some(sent[0].end()),
            data: vec![],
        };
        let (sent, events) = wire.hears(&reset);
        assert_eq!(sent, []);
        assert_eq!(events, [told(5, EV_FAILED, WHY_REFUSED, &[])]);
        // The socket is gone: said once, and its number is free.
        assert_eq!(wire.turn(), (vec![], vec![]));
        assert_eq!(wire.stack.request(&write(5, b"x")), Err(Refused::NoSocket));
        assert_eq!(wire.stack.request(&close(5)), Ok(false));

        // A network the card is not on: no segment at all.
        let far = Ipv4Address([192, 168, 1, 1]);
        let (sent, events) = wire.asked(&connect(5, far, 80, 50000, 1000));
        assert_eq!(sent, []);
        assert_eq!(events, [told(5, EV_FAILED, WHY_UNREACHABLE, &[])]);

        // Nobody answers: asked again for a while, then given up on.
        let (sent, _) = wire.asked(&connect(5, THEIR_IP, 82, 50000, 1000));
        assert_eq!(sent.len(), 1);
        wire.ms += 20_000;
        let (sent, events) = wire.turn();
        assert!(sent.iter().all(|s| s.control == TcpControl::Syn));
        assert_eq!(events, []);
        wire.ms += 11_000;
        let (_, events) = wire.turn();
        assert_eq!(events, [told(5, EV_FAILED, WHY_TIMED_OUT, &[])]);
        assert!(wire.stack.streams.is_empty());
        assert_eq!(wire.stack.idle_for(Instant::from_millis(wire.ms)), None);

        // What is no connect is not done.
        for bad in [
            connect(5, THEIR_IP, 0, 50000, 1000),
            connect(5, THEIR_IP, 80, 0, 1000),
            message(OP_CONNECT, 5, &[0; 11]),
            message(OP_LISTEN, 5, &[0; 7]),
            message(OP_WINDOW, 5, &[0; 3]),
            message(OP_SHUTDOWN, 5, &[0]),
        ] {
            assert_eq!(wire.stack.request(&bad), Err(Refused::Malformed));
        }
        assert_eq!(
            wire.stack.request(&listen(5, 0, 1, 1000)),
            Err(Refused::Bind)
        );
        assert_eq!(wire.stack.request(&window(5, 1)), Err(Refused::NoSocket));
        assert_eq!(
            wire.stack.request(&message(OP_SHUTDOWN, 5, &[])),
            Err(Refused::NoSocket)
        );
    }

    #[test]
    fn the_kernel_is_sent_no_more_than_it_takes() {
        let mut wire = Wire::new();
        let mut peer = connected(&mut wire, 5, 10);
        let (_, events) = wire.hears(&peer.segment(TcpControl::Psh, b"0123456789abcdef"));
        assert_eq!(events, [told(5, EV_BYTES, 0, b"0123456789")]);
        // The rest waits here, however many turns pass.
        assert_eq!(wire.turn().1, []);
        assert_eq!(wire.stack.request(&window(5, 4)), Ok(true));
        assert_eq!(wire.turn().1, [told(5, EV_BYTES, 0, b"abcd")]);
        assert_eq!(wire.stack.request(&window(5, 100)), Ok(true));
        assert_eq!(wire.turn().1, [told(5, EV_BYTES, 0, b"ef")]);
        // Room with nothing waiting for it asks for no turn.
        assert_eq!(wire.stack.request(&window(5, 100)), Ok(false));
        // The end is said after the last byte, not before.
        wire.hears(&peer.segment(TcpControl::Psh, b"fin"));
        let (_, events) = wire.hears(&peer.segment(TcpControl::Fin, &[]));
        assert_eq!(events, [told(5, EV_EOF, 0, &[])]);
        assert_eq!(wire.turn().1, [], "said once");

        // With no room at all, the end waits behind the bytes.
        let mut wire = Wire::new();
        let mut peer = connected(&mut wire, 5, 0);
        let (_, events) = wire.hears(&peer.segment(TcpControl::Fin, b"ultimo"));
        assert_eq!(events, []);
        wire.stack.request(&window(5, 6)).unwrap();
        assert_eq!(
            wire.turn().1,
            [told(5, EV_BYTES, 0, b"ultimo"), told(5, EV_EOF, 0, &[])]
        );
    }

    #[test]
    fn the_kernel_writes_what_it_was_told_there_is_room_for() {
        let mut wire = Wire::new();
        let mut peer = connected(&mut wire, 5, 1000);
        // More than the credit is the kernel not counting.
        let piece = vec![0x5au8; 65520];
        let pieces = TX_BYTES / piece.len();
        let mut written = 0;
        let mut sent = Vec::new();
        for _ in 0..pieces {
            let (more, events) = wire.asked(&write(5, &piece));
            assert_eq!(events, []);
            sent.extend(more);
            written += piece.len();
        }
        let last = vec![0x5au8; TX_BYTES - written];
        sent.extend(wire.asked(&write(5, &last)).0);
        assert_eq!(wire.stack.request(&write(5, b"x")), Err(Refused::Full));

        // The peer takes what it has window for and acknowledges it, again
        // and again. A little room is not worth a message; once a quarter
        // of the buffer is room again the kernel is told of all of it.
        let mut acked = 0;
        loop {
            for segment in &sent {
                peer.heard(segment);
                acked += segment.data.len();
            }
            assert!(!sent.is_empty() && acked < TX_BYTES);
            let (more, events) = wire.hears(&peer.segment(TcpControl::None, &[]));
            sent = more;
            if acked >= TX_BYTES / 4 {
                assert_eq!(events, [told(5, EV_CREDIT, acked as u32, &[])]);
                break;
            }
            assert_eq!(events, []);
        }
        // That much and no more.
        assert_eq!(wire.stack.request(&write(5, &vec![1u8; acked])), Ok(true));
        assert_eq!(wire.stack.request(&write(5, b"x")), Err(Refused::Full));
    }

    #[test]
    fn a_reset_is_told_and_the_connection_forgotten() {
        let mut wire = Wire::new();
        let mut peer = connected(&mut wire, 5, 1000);
        let (sent, events) = wire.hears(&peer.segment(TcpControl::Rst, &[]));
        assert_eq!(sent, []);
        assert_eq!(events, [told(5, EV_FAILED, WHY_RESET, &[])]);
        assert_eq!(wire.turn(), (vec![], vec![]));
        assert_eq!(wire.stack.request(&write(5, b"x")), Err(Refused::NoSocket));
        assert!(wire.stack.streams.is_empty());
    }

    #[test]
    fn each_side_closes_its_half_and_a_close_says_goodbye_by_itself() {
        let mut wire = Wire::new();
        let mut peer = connected(&mut wire, 5, 1000);
        // The program is done writing: a FIN, and the peer still talks.
        let (sent, events) = wire.asked(&message(OP_SHUTDOWN, 5, &[]));
        assert_eq!(events, []);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].control, TcpControl::Fin);
        peer.heard(&sent[0]);
        let (_, events) = wire.hears(&peer.segment(TcpControl::Psh, b"aun"));
        assert_eq!(events, [told(5, EV_BYTES, 0, b"aun")]);
        let (sent, events) = wire.hears(&peer.segment(TcpControl::Fin, &[]));
        assert_eq!(events, [told(5, EV_EOF, 0, &[])]);
        assert_eq!(sent.last().unwrap().ack, Some(peer.seq));
        // Over, both ways, and that is no reset: nothing more is said,
        // now or when smoltcp is done waiting.
        wire.ms += 11_000;
        assert_eq!(wire.turn(), (vec![], vec![]));
        assert_eq!(wire.stack.request(&close(5)), Ok(false));
        assert!(wire.stack.streams.is_empty());

        // A program that closes: the FIN goes without it, and the socket
        // is forgotten when the peer has answered.
        let mut peer = connected(&mut wire, 6, 1000);
        assert_eq!(wire.stack.request(&close(6)), Ok(true));
        assert_eq!(wire.stack.request(&write(6, b"x")), Err(Refused::NoSocket));
        let (sent, events) = wire.turn();
        assert_eq!(events, []);
        assert_eq!(sent[0].control, TcpControl::Fin);
        peer.heard(&sent[0]);
        let (sent, events) = wire.hears(&peer.segment(TcpControl::Fin, &[]));
        assert_eq!(events, [], "nobody to tell");
        assert_eq!(sent.last().unwrap().ack, Some(peer.seq));
        assert!(wire.stack.streams.is_empty());

        // One that closes with bytes it never read resets the peer.
        let mut peer = connected(&mut wire, 7, 0);
        wire.hears(&peer.segment(TcpControl::Psh, b"nadie lo lee"));
        assert_eq!(wire.stack.request(&close(7)), Ok(true));
        let (sent, _) = wire.turn();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].control, TcpControl::Rst);
        wire.turn();
        assert!(wire.stack.streams.is_empty());

        // A peer that never answers the goodbye is reset in the end.
        let _peer = connected(&mut wire, 8, 1000);
        assert_eq!(wire.stack.request(&close(8)), Ok(true));
        wire.turn();
        wire.ms += 31_000;
        let (sent, _) = wire.turn();
        assert!(sent.iter().any(|s| s.control == TcpControl::Rst));
        wire.turn();
        assert!(wire.stack.streams.is_empty());
    }

    #[test]
    fn a_listener_makes_connections_and_numbers_them() {
        let mut wire = Wire::new();
        assert_eq!(wire.stack.request(&listen(7, 8080, 2, 1000)), Ok(false));
        // The connection and what came with it, in one message, the
        // connection first.
        let (mut one, events) = arrives(&mut wire, 40001, b"GET /");
        let first = ACCEPTED_BIT | 1;
        let mut accepted = first.to_le_bytes().to_vec();
        accepted.extend_from_slice(&[10, 0, 9, 1, 0x41, 0x9c, 0, 0]);
        assert_eq!(
            events,
            [
                told(7, EV_ACCEPTED, TX_BYTES as u32, &accepted),
                told(first, EV_BYTES, 0, b"GET /"),
            ]
        );
        // It is a connection like any other.
        let (sent, _) = wire.asked(&write(first, b"200"));
        assert_eq!(sent[0].data, b"200");
        one.heard(&sent[0]);

        // The listener goes on listening: two more, both made.
        let (_two, events) = arrives(&mut wire, 40002, b"");
        assert_eq!(events.len(), 1);
        assert_eq!((events[0].0, events[0].1), (7, EV_ACCEPTED));
        assert_eq!(events[0].3[..8], (ACCEPTED_BIT | 2).to_le_bytes());
        let (_three, events) = arrives(&mut wire, 40003, b"");
        assert_eq!(events[0].3[..8], (ACCEPTED_BIT | 3).to_le_bytes());

        // A handshake nobody finishes is given up on, and its socket
        // listens again: a listener is not used up by who only knocks.
        // And only so many may knock at once.
        let mut taken = 0;
        for from in 41000..41000 + 2 * HALF_MADE_MAX as u16 {
            let syn = Segment {
                from,
                to: 8080,
                control: TcpControl::Syn,
                seq: 1,
                ack: None,
                data: vec![],
            };
            let (sent, _) = wire.hears(&syn);
            match sent[0].control {
                TcpControl::Syn => taken += 1,
                other => assert_eq!(other, TcpControl::Rst, "refused"),
            }
        }
        assert!(taken > BACKLOG_MAX, "more than its backlog: {taken}");
        assert!(taken <= HALF_MADE_MAX + BACKLOG_MAX, "{taken}");
        let before = wire.stack.streams.sockets();
        wire.ms += 31_000;
        wire.turn();
        wire.turn();
        assert!(wire.stack.streams.sockets() <= before);
        let listening = wire.stack.streams.listeners[&7]
            .ready
            .iter()
            .filter(|&&h| wire.stack.set.get::<TcpSocket>(h).state() == TcpState::Listen)
            .count();
        assert!(listening >= 2, "its backlog, ready again");

        // Closed, it takes no more: the next is reset. The ones it made
        // go on.
        assert_eq!(wire.stack.request(&close(7)), Ok(false));
        let syn = Segment {
            from: 40004,
            to: 8080,
            control: TcpControl::Syn,
            seq: 1,
            ack: None,
            data: vec![],
        };
        let (sent, events) = wire.hears(&syn);
        assert_eq!(events, []);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].control, TcpControl::Rst);
        let (_, events) = wire.hears(&one.segment(TcpControl::Psh, b"mas"));
        assert_eq!(events, [told(first, EV_BYTES, 0, b"mas")]);
    }

    #[test]
    fn what_a_turn_brings_that_does_not_fit_one_message_is_asked_for_again() {
        let mut wire = Wire::new();
        let mut peer = connected(&mut wire, 5, 200_000);
        // 100 kB arrive while the kernel is not asked.
        let segment = vec![0x33u8; 1000];
        for _ in 0..100 {
            wire.window.deliver(
                wire.stack.device(),
                0,
                &tcp_frame(&peer.segment(TcpControl::None, &segment)),
            );
            wire.stack.poll(Instant::from_millis(wire.ms));
            for msg in turn(wire.stack.device()) {
                wire.window.done(wire.stack.device(), &msg);
            }
        }
        let mut m = Vec::new();
        assert_eq!(wire.stack.arrived(&mut m), Arrived::More);
        let first = m.len();
        assert_eq!(wire.stack.arrived(&mut m), Arrived::All);
        // A record that fills the first message, and the rest in one of
        // the second.
        assert_eq!(first, MESSAGE_MAX);
        assert_eq!(first + m.len(), 100_000 + 2 * HEADER_LEN + 2 * RECORD_LEN);
        assert_eq!(wire.stack.arrived(&mut m), Arrived::Nothing);
    }
}
