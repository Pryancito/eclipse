//! Sockets served by a network stack that runs as a process.
//!
//! The kernel's own stack is smoltcp, in the kernel. `tools/eclipse-netstack`
//! is the same stack in a process, with a card of its own, and this is how a
//! program reaches it: the process registers one end of a channel
//! (`eclipse_netstack_serve`), and a datagram socket of a program that asked
//! for that stack (`eclipse_netstack_use`, see [`crate::process`]) is a
//! [`ServedUdpSocket`]. The descriptor, its flags, `poll` and `close` stay
//! here, as for any socket; what the socket sends and receives goes over the
//! channel.
//!
//! Every message is one way. Nothing here waits for an answer, so nothing a
//! program does on its socket can be held up by the stack beyond the stack
//! not delivering:
//!
//! ```text
//! all:    0  u32 zero   4  u32 op   8  u64 socket
//! BIND    (kernel to stack)  16  u16 port
//! SEND    (kernel to stack)  16  [u8; 4] to    20  u16 port  22  u16 zero  24  the datagram
//! CLOSE   (kernel to stack)
//! DATA    (stack to kernel)  16  one record after another, to the end:
//!             0  u64 socket  8  [u8; 4] from  12  u16 port  14  u16 length  16  the datagram
//! ```
//!
//! little-endian, the addresses as they are written. A `DATA` carries
//! every datagram a turn of the stack took off the card, for whichever
//! sockets (its header's socket is zero): one message wakes the kernel
//! once for all of them. The kernel numbers the
//! sockets and gives out the ports: `bind` answers at once, as the `Socket`
//! trait wants, and two sockets never get one port. A datagram that arrives
//! is queued on its socket here, up to [`RX_QUEUE_BYTES`], so `recvfrom` and
//! `poll` are answered from this side without a trip to the stack; past
//! that it is dropped, as a full receive buffer drops.
//!
//! What this trusts: the stack is a process the system started for the
//! purpose. It sees every datagram of the sockets it serves and can forge
//! what they receive, as a card's driver can; a message it sends that names
//! no socket, or is no message, is dropped.
//!
//! A stream socket ([`ServedTcpSocket`], in `tcp`) is the same bargain: the
//! connection is the stack's, a smoltcp socket there, and what the program
//! has read from and may write to is counted here, so `read`, `write` and
//! `poll` answer without a trip:
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
//! Bytes are counted both ways, so neither side holds more than it said
//! it would. The stack says how many bytes it takes (`CONNECTED`,
//! `ACCEPTED`, then `CREDIT` as the peer acknowledges them) and a `WRITE`
//! never carries more: a program that writes faster than the peer reads
//! finds its socket not writable. The kernel says how many bytes it
//! takes (`window`, then `WINDOW` as the program reads them) and the
//! stack sends no more `BYTES` than that: a program that does not read
//! closes the peer's window, as it should. A connection that arrives on
//! a listener is numbered by the stack, with [`ACCEPTED_BIT`] set, so its
//! numbers never meet the kernel's.
//!
//! IPv4 only, so far.

use crate::error::{LxError, LxResult};
use crate::fs::{FileLike, OpenFlags, PollStatus};
use crate::net::*;
use crate::sync::{Event, EventBus};
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use async_trait::async_trait;
use core::convert::TryInto;
use core::sync::atomic::{AtomicBool, Ordering};
use kernel_hal::sync::Mutex;
use log::warn;
use smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address};
use zircon_object::impl_kobject;
use zircon_object::ipc::{Channel, MessagePacket};
use zircon_object::object::*;
use zircon_object::{ZxError, ZxResult};

mod tcp;
pub use tcp::ServedTcpSocket;

/// The operations of a message.
pub const OP_BIND: u32 = 1;
pub const OP_SEND: u32 = 2;
pub const OP_CLOSE: u32 = 3;
pub const OP_DATA: u32 = 4;
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

/// Set in the number of a socket the stack numbered: a connection that
/// arrived on a listener.
pub const ACCEPTED_BIT: u64 = 1 << 63;
/// The bytes of an `ACCEPTED` record.
pub const ACCEPTED_LEN: usize = 16;
/// The bytes of a stream the kernel holds unread: what `CONNECT` and
/// `LISTEN` say as their window.
pub const STREAM_WINDOW: usize = 256 * 1024;
/// The most bytes of one `WRITE`: what fits a channel message.
pub const WRITE_MAX: usize = 65536 - HEADER_LEN;
/// The connections a listener asks the stack to have ready to take. The
/// `Socket` trait does not carry what `listen(2)` was told.
pub const LISTEN_BACKLOG: u16 = 8;

/// What every message starts with.
pub const HEADER_LEN: usize = 16;
/// Where the datagram starts in `SEND`.
pub const DATAGRAM_AT: usize = 24;
/// What each record of a `DATA` starts with.
pub const RECORD_LEN: usize = 16;
/// The longest datagram UDP carries over IPv4.
pub const MAX_DATAGRAM: usize = 65507;
// A datagram and its header fit one channel message (65536 bytes).
const _: () = assert!(DATAGRAM_AT + MAX_DATAGRAM <= 65536);

/// The bytes of datagrams a socket holds unread; one more is dropped.
pub const RX_QUEUE_BYTES: usize = 256 * 1024;
/// The messages the stack may have unread before a `send` is refused
/// (`ENOBUFS`): a program that sends faster than the stack reads would
/// otherwise grow the channel without end.
pub const SEND_BACKLOG: usize = 1024;

/// Where ephemeral ports come from (the IANA dynamic range).
const EPHEMERAL: core::ops::RangeInclusive<u16> = 49152..=65535;

fn header(op: u32, sock: u64, room: usize) -> Vec<u8> {
    let mut m = Vec::with_capacity(HEADER_LEN + room);
    m.extend_from_slice(&0u32.to_le_bytes());
    m.extend_from_slice(&op.to_le_bytes());
    m.extend_from_slice(&sock.to_le_bytes());
    m
}

/// `BIND`: socket `sock` has port `port`.
pub fn bind_message(sock: u64, port: u16) -> Vec<u8> {
    let mut m = header(OP_BIND, sock, 2);
    m.extend_from_slice(&port.to_le_bytes());
    m
}

/// `SEND`: `data` from socket `sock` for `addr:port`.
pub fn send_message(sock: u64, addr: [u8; 4], port: u16, data: &[u8]) -> Vec<u8> {
    let mut m = header(OP_SEND, sock, 8 + data.len());
    m.extend_from_slice(&addr);
    m.extend_from_slice(&port.to_le_bytes());
    m.extend_from_slice(&[0, 0]);
    m.extend_from_slice(data);
    m
}

/// `CLOSE`: socket `sock` is gone.
pub fn close_message(sock: u64) -> Vec<u8> {
    header(OP_CLOSE, sock, 0)
}

/// A message's operation and socket, when it has both.
pub fn parse_header(m: &[u8]) -> Option<(u32, u64)> {
    if m.len() < HEADER_LEN {
        return None;
    }
    let op = u32::from_le_bytes(m[4..8].try_into().ok()?);
    let sock = u64::from_le_bytes(m[8..16].try_into().ok()?);
    Some((op, sock))
}

/// The address, port and datagram of a `SEND` message.
pub fn parse_send(m: &[u8]) -> Option<([u8; 4], u16, &[u8])> {
    if m.len() < DATAGRAM_AT || m.len() - DATAGRAM_AT > MAX_DATAGRAM {
        return None;
    }
    let addr: [u8; 4] = m[16..20].try_into().ok()?;
    let port = u16::from_le_bytes(m[20..22].try_into().ok()?);
    Some((addr, port, &m[DATAGRAM_AT..]))
}

/// A `DATA` with no records yet.
pub fn data_message() -> Vec<u8> {
    header(OP_DATA, 0, 0)
}

/// Add a record to the `DATA` message `m`: `data`, from `addr:port`, for
/// socket `sock`.
pub fn push_record(m: &mut Vec<u8>, sock: u64, addr: [u8; 4], port: u16, data: &[u8]) {
    debug_assert!(data.len() <= MAX_DATAGRAM);
    m.extend_from_slice(&sock.to_le_bytes());
    m.extend_from_slice(&addr);
    m.extend_from_slice(&port.to_le_bytes());
    m.extend_from_slice(&(data.len() as u16).to_le_bytes());
    m.extend_from_slice(data);
}

/// One record of a `DATA`: its socket, who sent the datagram, and it.
pub type Record<'a> = (u64, [u8; 4], u16, &'a [u8]);

/// The first record of `records`, the rest of a `DATA` message, and what
/// follows it. `None` when what is there is not a whole record.
pub fn parse_record(records: &[u8]) -> Option<(Record<'_>, &[u8])> {
    if records.len() < RECORD_LEN {
        return None;
    }
    let sock = u64::from_le_bytes(records[..8].try_into().ok()?);
    let addr: [u8; 4] = records[8..12].try_into().ok()?;
    let port = u16::from_le_bytes(records[12..14].try_into().ok()?);
    let len = u16::from_le_bytes(records[14..16].try_into().ok()?) as usize;
    let rest = &records[RECORD_LEN..];
    if len > rest.len() {
        return None;
    }
    Some(((sock, addr, port, &rest[..len]), &rest[len..]))
}

/// `CONNECT`: socket `sock`, from port `local`, to `addr:port`; the kernel
/// takes `window` bytes of what arrives.
pub fn connect_message(sock: u64, addr: [u8; 4], port: u16, local: u16, window: u32) -> Vec<u8> {
    let mut m = header(OP_CONNECT, sock, 12);
    m.extend_from_slice(&addr);
    m.extend_from_slice(&port.to_le_bytes());
    m.extend_from_slice(&local.to_le_bytes());
    m.extend_from_slice(&window.to_le_bytes());
    m
}

/// `LISTEN`: socket `sock` takes connections to `port`, `backlog` of them
/// ready at a time, and `window` bytes of what arrives on each.
pub fn listen_message(sock: u64, port: u16, backlog: u16, window: u32) -> Vec<u8> {
    let mut m = header(OP_LISTEN, sock, 8);
    m.extend_from_slice(&port.to_le_bytes());
    m.extend_from_slice(&backlog.to_le_bytes());
    m.extend_from_slice(&window.to_le_bytes());
    m
}

/// `WRITE`: `data` for the peer of socket `sock`.
pub fn write_message(sock: u64, data: &[u8]) -> Vec<u8> {
    debug_assert!(data.len() <= WRITE_MAX);
    let mut m = header(OP_WRITE, sock, data.len());
    m.extend_from_slice(data);
    m
}

/// `SHUTDOWN`: socket `sock` will write no more.
pub fn shutdown_message(sock: u64) -> Vec<u8> {
    header(OP_SHUTDOWN, sock, 0)
}

/// `WINDOW`: the kernel takes `bytes` more for socket `sock`.
pub fn window_message(sock: u64, bytes: u32) -> Vec<u8> {
    let mut m = header(OP_WINDOW, sock, 4);
    m.extend_from_slice(&bytes.to_le_bytes());
    m
}

/// An `EVENTS` with no records yet.
pub fn events_message() -> Vec<u8> {
    header(OP_EVENTS, 0, 0)
}

/// Add a record to the `EVENTS` message `m`.
pub fn push_event(m: &mut Vec<u8>, sock: u64, what: u16, number: u32, bytes: &[u8]) {
    debug_assert!(bytes.len() <= u16::MAX as usize);
    m.extend_from_slice(&sock.to_le_bytes());
    m.extend_from_slice(&what.to_le_bytes());
    m.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    m.extend_from_slice(&number.to_le_bytes());
    m.extend_from_slice(bytes);
}

/// The bytes of an `ACCEPTED`: the new socket `sock`, connected from
/// `addr:port`.
pub fn accepted_bytes(sock: u64, addr: [u8; 4], port: u16) -> [u8; ACCEPTED_LEN] {
    let mut b = [0u8; ACCEPTED_LEN];
    b[..8].copy_from_slice(&sock.to_le_bytes());
    b[8..12].copy_from_slice(&addr);
    b[12..14].copy_from_slice(&port.to_le_bytes());
    b
}

/// One record of an `EVENTS`: its socket, what it says, its number and
/// its bytes.
pub type EventRecord<'a> = (u64, u16, u32, &'a [u8]);

/// The first record of `records`, the rest of an `EVENTS` message, and
/// what follows it. `None` when what is there is not a whole record.
pub fn parse_event(records: &[u8]) -> Option<(EventRecord<'_>, &[u8])> {
    if records.len() < RECORD_LEN {
        return None;
    }
    let sock = u64::from_le_bytes(records[..8].try_into().ok()?);
    let what = u16::from_le_bytes(records[8..10].try_into().ok()?);
    let len = u16::from_le_bytes(records[10..12].try_into().ok()?) as usize;
    let number = u32::from_le_bytes(records[12..16].try_into().ok()?);
    let rest = &records[RECORD_LEN..];
    if len > rest.len() {
        return None;
    }
    Some(((sock, what, number, &rest[..len]), &rest[len..]))
}

/// What a socket shares with the stack's link: where datagrams are queued
/// and what says so.
struct Shared {
    /// `READABLE` while the queue holds a datagram (or reading is over),
    /// `WRITABLE` always. Changed under `inner`.
    bus: Arc<Mutex<EventBus>>,
    inner: Mutex<Inner>,
}

struct Inner {
    queue: VecDeque<(IpEndpoint, Vec<u8>)>,
    queued_bytes: usize,
    /// The port, once bound.
    port: Option<u16>,
    /// The peer of a connected socket.
    remote: Option<IpEndpoint>,
    flags: OpenFlags,
    /// `MSG_TRUNC` of the last read, for `recvmsg`.
    last_msg_flags: i32,
    /// `shutdown(SHUT_RD)`: an empty queue is the end.
    read_closed: bool,
}

impl Shared {
    /// Queue a datagram that arrived. `false` when there was no room.
    fn deliver(&self, from: IpEndpoint, data: &[u8]) -> bool {
        let mut inner = self.inner.lock();
        if inner.read_closed || inner.queued_bytes + data.len() > RX_QUEUE_BYTES {
            return false;
        }
        inner.queued_bytes += data.len();
        inner.queue.push_back((from, data.to_vec()));
        self.bus.lock().set(Event::READABLE);
        true
    }

    /// `shutdown(SHUT_RD)`: whoever waits to read is told.
    fn end_reading(&self) {
        let mut inner = self.inner.lock();
        inner.read_closed = true;
        self.bus.lock().set(Event::READABLE);
    }

    /// The stack is gone: whoever waits to read is woken to find out. Under
    /// `inner`, as every change of the bus, so a reader that looked a
    /// moment before and is about to sleep does not miss it.
    fn wake(&self) {
        let _inner = self.inner.lock();
        self.bus.lock().set(Event::READABLE);
    }
}

/// The ports of one protocol: which are taken, and where the search for
/// a free one starts.
struct Ports {
    taken: BTreeSet<u16>,
    next: u16,
}

impl Default for Ports {
    fn default() -> Self {
        Ports {
            taken: BTreeSet::new(),
            next: *EPHEMERAL.start(),
        }
    }
}

impl Ports {
    /// Take the port `wanted`, or any when it is 0. `EADDRINUSE` when
    /// another socket has it or none is left.
    fn take(&mut self, wanted: u16) -> LxResult<u16> {
        if wanted != 0 {
            if !self.taken.insert(wanted) {
                return Err(LxError::EADDRINUSE);
            }
            return Ok(wanted);
        }
        let span = EPHEMERAL.len() as u32;
        let first = self.next;
        let port = (0..span)
            .map(|i| {
                let at = (first - EPHEMERAL.start()) as u32 + i;
                EPHEMERAL.start() + (at % span) as u16
            })
            .find(|p| !self.taken.contains(p))
            .ok_or(LxError::EADDRINUSE)?;
        self.taken.insert(port);
        self.next = if port == *EPHEMERAL.end() {
            *EPHEMERAL.start()
        } else {
            port + 1
        };
        Ok(port)
    }
}

#[derive(Default)]
struct Table {
    socks: BTreeMap<u64, Weak<Shared>>,
    /// The stream sockets, the kernel's and the ones the stack numbered.
    streams: BTreeMap<u64, Weak<tcp::Stream>>,
    ports: Ports,
    /// TCP's ports, which are not UDP's.
    stream_ports: Ports,
    next_sock: u64,
}

/// The kernel's end of a stack that runs as a process.
pub struct StackLink {
    channel: Arc<Channel>,
    /// The longest datagram the stack carries: what fits one frame of its
    /// card, for a stack that does not fragment.
    max_datagram: usize,
    /// The stack's address, which is the local address of every
    /// connection: unspecified when the stack did not say.
    address: Ipv4Address,
    table: Mutex<Table>,
    dead: AtomicBool,
}

/// The stack that serves the sockets of the programs that ask for one.
static STACK: Mutex<Option<Arc<StackLink>>> = Mutex::new(None);

/// A process serves a stack on `channel`, the kernel's end, that carries
/// datagrams of up to `max_datagram` bytes: a longer one is `EMSGSIZE` to
/// who sends it. `address` is the stack's own, which a connected socket
/// answers `getsockname` with. `ALREADY_BOUND` while another process
/// serves one.
pub fn serve(
    channel: Arc<Channel>,
    max_datagram: usize,
    address: [u8; 4],
) -> ZxResult<Arc<StackLink>> {
    if !(1..=MAX_DATAGRAM).contains(&max_datagram) {
        return Err(ZxError::INVALID_ARGS);
    }
    let mut stack = STACK.lock();
    if let Some(old) = stack.as_ref() {
        // The one before may have died a moment ago, with nobody there
        // yet to say so: ask its channel.
        old.pump();
        if !old.is_dead() {
            return Err(ZxError::ALREADY_BOUND);
        }
    }
    let link = Arc::new(StackLink {
        channel,
        max_datagram,
        address: Ipv4Address(address),
        table: Mutex::new(Table::default()),
        dead: AtomicBool::new(false),
    });
    *stack = Some(link.clone());
    Ok(link)
}

/// The stack being served, if there is one.
pub fn current() -> Option<Arc<StackLink>> {
    STACK.lock().clone().filter(|link| !link.is_dead())
}

impl StackLink {
    /// Whether the process is gone.
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    /// The channel, for whoever sleeps on it until there is something to
    /// [`pump`](Self::pump).
    pub fn channel(&self) -> &Arc<Channel> {
        &self.channel
    }

    /// Read what the stack has sent: each datagram onto its socket's queue.
    pub fn pump(&self) {
        while !self.is_dead() {
            match self.channel.read() {
                Ok(msg) => self.take(&msg.data),
                Err(ZxError::SHOULD_WAIT) => return,
                Err(_) => self.died(),
            }
        }
    }

    fn take(&self, m: &[u8]) {
        match parse_header(m) {
            Some((OP_DATA, _)) => self.take_datagrams(&m[HEADER_LEN..]),
            Some((OP_EVENTS, _)) => self.take_events(&m[HEADER_LEN..]),
            _ => warn!(
                "user stack: a message that means nothing ({} bytes)",
                m.len()
            ),
        }
    }

    fn take_events(&self, mut records: &[u8]) {
        while !records.is_empty() {
            let Some(((sock, what, number, bytes), rest)) = parse_event(records) else {
                warn!(
                    "user stack: {} bytes that are no record of an event",
                    records.len()
                );
                return;
            };
            records = rest;
            self.event(sock, what, number, bytes);
        }
    }

    fn take_datagrams(&self, mut records: &[u8]) {
        while !records.is_empty() {
            let Some(((sock, addr, port, data), rest)) = parse_record(records) else {
                warn!(
                    "user stack: {} bytes that are no record of a datagram",
                    records.len()
                );
                return;
            };
            records = rest;
            // A datagram for a socket that closed meanwhile is nobody's.
            let shared = self.table.lock().socks.get(&sock).and_then(Weak::upgrade);
            if let Some(shared) = shared {
                let from = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address(addr)), port);
                shared.deliver(from, data);
            }
        }
    }

    /// The process is gone: its sockets answer `ENETDOWN` from now on,
    /// also to who was asleep on one, and the next stack can be served. A
    /// socket does not move to the next stack: the program opens another.
    fn died(&self) {
        if self.dead.swap(true, Ordering::AcqRel) {
            return;
        }
        warn!("user stack: the process is gone");
        let (socks, streams): (Vec<Weak<Shared>>, Vec<Weak<tcp::Stream>>) = {
            let table = self.table.lock();
            (
                table.socks.values().cloned().collect(),
                table.streams.values().cloned().collect(),
            )
        };
        for shared in socks.iter().filter_map(Weak::upgrade) {
            shared.wake();
        }
        for stream in streams.iter().filter_map(Weak::upgrade) {
            stream.wake();
        }
    }

    /// A message to the stack. `ENETDOWN` when it is gone, `ENOBUFS` when
    /// it has [`SEND_BACKLOG`] unread and `urgent` is not set.
    fn tell(&self, data: Vec<u8>, urgent: bool) -> LxResult<()> {
        if self.is_dead() {
            return Err(LxError::ENETDOWN);
        }
        if !urgent && self.channel.peer_pending() >= SEND_BACKLOG {
            return Err(LxError::ENOBUFS);
        }
        let msg = MessagePacket {
            data,
            handles: Vec::new(),
        };
        self.channel.write(msg).map_err(|_| {
            self.died();
            LxError::ENETDOWN
        })
    }

    /// Give a socket the port `wanted`, or any when it is 0, and tell the
    /// stack. `EADDRINUSE` when another has it or none is left.
    fn bind(&self, sock: u64, wanted: u16) -> LxResult<u16> {
        let port = self.table.lock().ports.take(wanted)?;
        if let Err(e) = self.tell(bind_message(sock, port), true) {
            self.table.lock().ports.taken.remove(&port);
            return Err(e);
        }
        Ok(port)
    }

    /// A socket is gone: its number and its port are free, and the stack
    /// is told.
    fn close(&self, sock: u64, port: Option<u16>) {
        {
            let mut table = self.table.lock();
            table.socks.remove(&sock);
            if let Some(port) = port {
                table.ports.taken.remove(&port);
            }
        }
        let _ = self.tell(close_message(sock), true);
    }
}

/// A datagram socket whose stack is a process.
pub struct ServedUdpSocket {
    base: KObjectBase,
    link: Arc<StackLink>,
    sock: u64,
    shared: Arc<Shared>,
}

impl ServedUdpSocket {
    /// A socket of the stack behind `link`. `ENETDOWN` when it is gone.
    pub fn new(link: Arc<StackLink>) -> LxResult<Self> {
        if link.is_dead() {
            return Err(LxError::ENETDOWN);
        }
        let mut bus = EventBus::default();
        // A datagram socket can always be written to: what cannot be sent
        // is refused, not waited for.
        bus.set(Event::WRITABLE);
        let shared = Arc::new(Shared {
            bus: Arc::new(Mutex::new(bus)),
            inner: Mutex::new(Inner {
                queue: VecDeque::new(),
                queued_bytes: 0,
                port: None,
                remote: None,
                flags: OpenFlags::RDWR,
                last_msg_flags: 0,
                read_closed: false,
            }),
        });
        let sock = {
            let mut table = link.table.lock();
            table.next_sock += 1;
            let sock = table.next_sock;
            table.socks.insert(sock, Arc::downgrade(&shared));
            sock
        };
        Ok(ServedUdpSocket {
            base: KObjectBase::new(),
            link,
            sock,
            shared,
        })
    }

    /// The socket's port, binding it to any when it has none: what a send
    /// or a connect on an unbound socket does.
    fn autobind(&self) -> LxResult<u16> {
        if let Some(port) = self.shared.inner.lock().port {
            return Ok(port);
        }
        let port = self.link.bind(self.sock, 0)?;
        self.shared.inner.lock().port = Some(port);
        Ok(port)
    }

    /// The next datagram, taken off the queue or only looked at.
    async fn receive(&self, data: &mut [u8], take: bool) -> (SysResult, Endpoint) {
        let nobody = || Endpoint::Ip(IpEndpoint::UNSPECIFIED);
        loop {
            {
                let mut guard = self.shared.inner.lock();
                let inner = &mut *guard;
                let remote = inner.remote;
                // A connected socket hears its peer and nobody else.
                while matches!((inner.queue.front(), remote), (Some((from, _)), Some(peer)) if *from != peer)
                {
                    if let Some((_, dropped)) = inner.queue.pop_front() {
                        inner.queued_bytes -= dropped.len();
                    }
                }
                if let Some((from, datagram)) = inner.queue.front() {
                    let (from, len) = (*from, datagram.len());
                    let n = data.len().min(len);
                    data[..n].copy_from_slice(&datagram[..n]);
                    // MSG_TRUNC = 0x20: the datagram was longer than the buffer.
                    inner.last_msg_flags = if len > n { 0x20 } else { 0 };
                    if take {
                        inner.queue.pop_front();
                        inner.queued_bytes -= len;
                    }
                    if inner.queue.is_empty() && !inner.read_closed && !self.link.is_dead() {
                        self.shared.bus.lock().clear(Event::READABLE);
                    }
                    return (Ok(n), Endpoint::Ip(from));
                }
                if inner.read_closed {
                    return (Ok(0), nobody());
                }
                if self.link.is_dead() {
                    return (Err(LxError::ENETDOWN), nobody());
                }
                self.shared.bus.lock().clear(Event::READABLE);
                if inner.flags.contains(OpenFlags::NON_BLOCK) {
                    return (Err(LxError::EAGAIN), nobody());
                }
            }
            // Woken by a datagram, or ended by a signal.
            if let Err(e) =
                crate::sync::wait_for_event(self.shared.bus.clone(), Event::READABLE).await
            {
                return (Err(e), nobody());
            }
        }
    }
}

impl Drop for ServedUdpSocket {
    fn drop(&mut self) {
        let port = self.shared.inner.lock().port;
        self.link.close(self.sock, port);
    }
}

#[async_trait]
impl Socket for ServedUdpSocket {
    async fn read(&self, data: &mut [u8]) -> (SysResult, Endpoint) {
        self.receive(data, true).await
    }

    async fn peek(&self, data: &mut [u8]) -> (SysResult, Endpoint) {
        self.receive(data, false).await
    }

    fn write(&self, data: &[u8], sendto_endpoint: Option<Endpoint>) -> SysResult {
        let to = match sendto_endpoint {
            Some(Endpoint::Ip(to)) => to,
            Some(_) => return Err(LxError::EAFNOSUPPORT),
            None => self.shared.inner.lock().remote.ok_or(LxError::ENOTCONN)?,
        };
        let IpAddress::Ipv4(addr) = to.addr else {
            return Err(LxError::EAFNOSUPPORT);
        };
        if data.len() > self.link.max_datagram {
            return Err(LxError::EMSGSIZE);
        }
        self.autobind()?;
        self.link
            .tell(send_message(self.sock, addr.0, to.port, data), false)?;
        Ok(data.len())
    }

    fn poll(&self, _events: PollEvents) -> (bool, bool, bool) {
        let inner = self.shared.inner.lock();
        let dead = self.link.is_dead();
        (
            !inner.queue.is_empty() || inner.read_closed || dead,
            true,
            dead,
        )
    }

    async fn connect(&self, endpoint: Endpoint) -> SysResult {
        let Endpoint::Ip(peer) = endpoint else {
            return Err(LxError::EAFNOSUPPORT);
        };
        if !matches!(peer.addr, IpAddress::Ipv4(_)) {
            return Err(LxError::EAFNOSUPPORT);
        }
        self.autobind()?;
        self.shared.inner.lock().remote = Some(peer);
        Ok(0)
    }

    fn bind(&self, endpoint: Endpoint) -> SysResult {
        let Endpoint::Ip(local) = endpoint else {
            return Err(LxError::EINVAL);
        };
        if !matches!(local.addr, IpAddress::Ipv4(_) | IpAddress::Unspecified) {
            return Err(LxError::EAFNOSUPPORT);
        }
        // `inet_bind`: a socket that has a port is `EINVAL`.
        if self.shared.inner.lock().port.is_some() {
            return Err(LxError::EINVAL);
        }
        let port = self.link.bind(self.sock, local.port)?;
        self.shared.inner.lock().port = Some(port);
        Ok(0)
    }

    fn shutdown(&self, howto: usize) -> SysResult {
        let (rd, _wr) = shutdown_sides(howto)?;
        if rd {
            self.shared.end_reading();
        }
        if self.shared.inner.lock().remote.is_none() {
            return Err(LxError::ENOTCONN);
        }
        Ok(0)
    }

    fn endpoint(&self) -> Option<Endpoint> {
        let port = self.shared.inner.lock().port.unwrap_or(0);
        Some(Endpoint::Ip(IpEndpoint::new(
            IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
            port,
        )))
    }

    fn remote_endpoint(&self) -> Option<Endpoint> {
        self.shared.inner.lock().remote.map(Endpoint::Ip)
    }

    fn is_inet(&self) -> bool {
        true
    }

    fn so_domain(&self) -> Option<u32> {
        Some(Domain::AF_INET as u32)
    }

    fn so_protocol(&self) -> Option<u32> {
        Some(17) // IPPROTO_UDP
    }

    fn socket_type(&self) -> Option<SocketType> {
        Some(SocketType::SOCK_DGRAM)
    }

    fn take_msg_flags(&self) -> i32 {
        core::mem::take(&mut self.shared.inner.lock().last_msg_flags)
    }
}

impl_kobject!(ServedUdpSocket);

#[async_trait]
impl FileLike for ServedUdpSocket {
    fn metadata(&self) -> LxResult<rcore_fs::vfs::Metadata> {
        Ok(crate::fs::anon_metadata(
            rcore_fs::vfs::FileType::Socket,
            self.id() as usize,
        ))
    }

    fn flags(&self) -> OpenFlags {
        self.shared.inner.lock().flags
    }

    fn set_flags(&self, f: OpenFlags) -> LxResult {
        self.shared.inner.lock().flags.take_settable(f);
        Ok(())
    }

    async fn read(&self, buf: &mut [u8]) -> LxResult<usize> {
        Socket::read(self, buf).await.0
    }

    async fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> LxResult<usize> {
        Err(LxError::ESPIPE)
    }

    fn write(&self, buf: &[u8]) -> LxResult<usize> {
        Socket::write(self, buf, None)
    }

    fn poll(&self, events: PollEvents) -> LxResult<PollStatus> {
        let (read, write, error) = Socket::poll(self, events);
        Ok(PollStatus {
            read,
            write,
            error,
            hangup: false,
        })
    }

    async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus> {
        loop {
            let status = FileLike::poll(self, events)?;
            let (want_read, want_write) = (events.wants_read(), events.wants_write());
            if (want_read && status.read) || want_write || !want_read {
                return Ok(status);
            }
            crate::sync::wait_for_event(self.shared.bus.clone(), Event::READABLE).await?;
        }
    }

    fn subscribe_readiness(
        &self,
        events: PollEvents,
        waker: &core::task::Waker,
    ) -> Option<crate::sync::ReadinessSub> {
        let mask = crate::fs::poll_events_to_bus_mask(events);
        Some(crate::sync::subscribe_readiness_on(
            &self.shared.bus,
            mask,
            waker,
        ))
    }

    fn readiness_seq(&self, events: PollEvents) -> Option<u64> {
        let mask = crate::fs::poll_events_to_bus_mask(events);
        Some(self.shared.bus.lock().seq_for(mask))
    }

    fn subscribe_edge(
        &self,
        events: PollEvents,
        waker: &core::task::Waker,
        seen: u64,
    ) -> Option<crate::sync::ReadinessSub> {
        let mask = crate::fs::poll_events_to_bus_mask(events);
        Some(crate::sync::subscribe_edge_on(
            &self.shared.bus,
            mask,
            waker,
            seen,
        ))
    }

    /// `FIONREAD`: the size of the next datagram, 0 when there is none.
    fn readable_bytes(&self) -> Option<usize> {
        let inner = self.shared.inner.lock();
        Some(inner.queue.front().map_or(0, |(_, d)| d.len()))
    }

    fn as_socket(&self) -> LxResult<&dyn Socket> {
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    //! The socket against a stack that is the test: it holds the other end
    //! of the channel, reads what the kernel tells it and writes what
    //! arrived. Every test takes `LOCK`: one stack is served at a time.

    use super::*;
    use crate::net::NET_TEST_LOCK as LOCK;

    /// The process's end of the channel.
    pub(super) struct Stack {
        pub(super) link: Arc<StackLink>,
        pub(super) end: Arc<Channel>,
    }

    pub(super) fn stack() -> Stack {
        let (kernel, end) = Channel::create();
        let link = serve(kernel, MAX_FOR_TESTS, STACK_ADDR).expect("the stack before is gone");
        Stack { link, end }
    }

    impl Stack {
        fn socket(&self) -> ServedUdpSocket {
            let s = ServedUdpSocket::new(self.link.clone()).unwrap();
            FileLike::set_flags(&s, OpenFlags::NON_BLOCK).unwrap();
            s
        }

        /// The next message the kernel sent, if it sent one.
        pub(super) fn told(&self) -> Option<Vec<u8>> {
            self.end.read().ok().map(|m| m.data)
        }

        /// A datagram arrived for socket `sock`.
        fn arrives(&self, sock: u64, from: [u8; 4], port: u16, data: &[u8]) {
            let mut m = data_message();
            push_record(&mut m, sock, from, port, data);
            self.say(m);
        }

        pub(super) fn say(&self, data: Vec<u8>) {
            let msg = MessagePacket {
                data,
                handles: Vec::new(),
            };
            self.end.write(msg).unwrap();
            self.link.pump();
        }
    }

    pub(super) fn any(port: u16) -> Endpoint {
        Endpoint::Ip(IpEndpoint::new(
            IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
            port,
        ))
    }

    pub(super) fn at(addr: [u8; 4], port: u16) -> IpEndpoint {
        IpEndpoint::new(IpAddress::Ipv4(Ipv4Address(addr)), port)
    }

    /// A destination, as `sendto` gives it.
    fn to(addr: [u8; 4], port: u16) -> Option<Endpoint> {
        Some(Endpoint::Ip(at(addr, port)))
    }

    pub(super) fn ip(endpoint: Endpoint) -> IpEndpoint {
        match endpoint {
            Endpoint::Ip(ip) => ip,
            _ => panic!("not an IP endpoint"),
        }
    }

    /// The socket's port, 0 when it has none.
    fn port(s: &ServedUdpSocket) -> u16 {
        ip(Socket::endpoint(s).unwrap()).port
    }

    fn recv(s: &ServedUdpSocket, buf: &mut [u8]) -> (SysResult, IpEndpoint) {
        let (n, from) = async_std::task::block_on(Socket::read(s, buf));
        (n, ip(from))
    }

    fn peek(s: &ServedUdpSocket, buf: &mut [u8]) -> (SysResult, IpEndpoint) {
        let (n, from) = async_std::task::block_on(Socket::peek(s, buf));
        (n, ip(from))
    }

    fn readable(s: &ServedUdpSocket) -> bool {
        Socket::poll(s, PollEvents::IN).0
    }

    pub(super) const HOST: [u8; 4] = [10, 0, 9, 1];
    /// The address the stack of the tests says it has.
    pub(super) const STACK_ADDR: [u8; 4] = [10, 0, 9, 15];
    /// What a stack over Ethernet carries: a frame's 1500 bytes less the
    /// IP and UDP headers.
    const MAX_FOR_TESTS: usize = 1472;

    #[test]
    fn a_message_is_its_header_and_what_the_operation_adds() {
        let bind = bind_message(7, 5000);
        assert_eq!(bind.len(), HEADER_LEN + 2);
        assert_eq!(&bind[..4], &[0; 4], "not a reply to any call");
        assert_eq!(parse_header(&bind), Some((OP_BIND, 7)));
        assert_eq!(&bind[16..], &5000u16.to_le_bytes());

        let send = send_message(u64::MAX, [10, 0, 9, 1], 53, b"hola");
        assert_eq!(parse_header(&send), Some((OP_SEND, u64::MAX)));
        assert_eq!(parse_send(&send), Some(([10, 0, 9, 1], 53, &b"hola"[..])));
        // An empty datagram is one.
        let empty = send_message(1, [1, 2, 3, 4], 9, b"");
        assert_eq!(empty.len(), DATAGRAM_AT);
        assert_eq!(parse_send(&empty), Some(([1, 2, 3, 4], 9, &b""[..])));

        // A DATA is its records, one after another.
        let mut data = data_message();
        assert_eq!(parse_header(&data), Some((OP_DATA, 0)));
        assert_eq!(parse_record(&data[HEADER_LEN..]), None);
        push_record(&mut data, 7, [10, 0, 9, 1], 53, b"uno");
        push_record(&mut data, 8, [10, 0, 9, 2], 54, b"");
        let (first, rest) = parse_record(&data[HEADER_LEN..]).unwrap();
        assert_eq!(first, (7, [10, 0, 9, 1], 53, &b"uno"[..]));
        let (second, rest) = parse_record(rest).unwrap();
        assert_eq!(second, (8, [10, 0, 9, 2], 54, &b""[..]));
        assert!(rest.is_empty());
        // A record cut short, in its head or in its datagram, is none.
        assert_eq!(parse_record(&data[HEADER_LEN..HEADER_LEN + 15]), None);
        assert_eq!(parse_record(&data[HEADER_LEN..HEADER_LEN + 18]), None);

        assert_eq!(parse_header(&close_message(3)), Some((OP_CLOSE, 3)));
        assert_eq!(close_message(3).len(), HEADER_LEN);

        // The same bytes `tools/eclipse-netstack/src/socks.rs` reads and
        // builds, written out: neither side can move a field alone.
        let mut data = data_message();
        push_record(
            &mut data,
            0x0807_0605_0403_0201,
            [10, 0, 9, 1],
            0x1234,
            b"hi",
        );
        assert_eq!(
            data,
            [
                0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 0, 9,
                1, 0x34, 0x12, 2, 0, b'h', b'i'
            ]
        );
        assert_eq!(
            send_message(7, [10, 0, 9, 1], 0x1234, b"hi"),
            [
                0, 0, 0, 0, 2, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 10, 0, 9, 1, 0x34, 0x12, 0, 0,
                b'h', b'i'
            ]
        );
        assert_eq!(
            bind_message(7, 5000),
            [0, 0, 0, 0, 1, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0x88, 0x13]
        );
        assert_eq!((OP_SEND, OP_CLOSE), (2, 3));

        // Too short to be a message, or to be a datagram.
        assert_eq!(parse_header(&send[..HEADER_LEN - 1]), None);
        assert_eq!(parse_send(&send[..DATAGRAM_AT - 1]), None);
        // Longer than UDP carries.
        let long = alloc::vec![0u8; DATAGRAM_AT + MAX_DATAGRAM + 1];
        assert_eq!(parse_send(&long), None);
    }

    #[test]
    fn bind_gives_the_port_asked_for_once_and_tells_the_stack() {
        let _g = LOCK.lock();
        let stack = stack();
        let a = stack.socket();
        let b = stack.socket();
        assert_ne!(a.sock, b.sock);
        assert_eq!(port(&a), 0);

        assert_eq!(Socket::bind(&a, any(7000)), Ok(0));
        assert_eq!(stack.told(), Some(bind_message(a.sock, 7000)));
        assert_eq!(port(&a), 7000);
        // A socket has one port, and a port one socket.
        assert_eq!(Socket::bind(&a, any(7001)), Err(LxError::EINVAL));
        assert_eq!(Socket::bind(&b, any(7000)), Err(LxError::EADDRINUSE));
        assert_eq!(stack.told(), None, "a bind refused is not told");

        // Closed, its port is free and the stack knows.
        let closed = a.sock;
        drop(a);
        assert_eq!(stack.told(), Some(close_message(closed)));
        assert_eq!(Socket::bind(&b, any(7000)), Ok(0));
        assert_eq!(stack.told(), Some(bind_message(b.sock, 7000)));
    }

    #[test]
    fn a_send_from_a_socket_without_a_port_gets_it_one_first() {
        let _g = LOCK.lock();
        let stack = stack();
        let a = stack.socket();
        let b = stack.socket();
        assert_eq!(Socket::write(&a, b"uno", to(HOST, 9)), Ok(3));
        let port_a = port(&a);
        assert!(EPHEMERAL.contains(&port_a));
        assert_eq!(stack.told(), Some(bind_message(a.sock, port_a)));
        assert_eq!(stack.told(), Some(send_message(a.sock, HOST, 9, b"uno")));
        // The second send has the port already; another socket gets another.
        assert_eq!(Socket::write(&a, b"dos", to(HOST, 9)), Ok(3));
        assert_eq!(stack.told(), Some(send_message(a.sock, HOST, 9, b"dos")));
        assert_eq!(Socket::write(&b, b"", to(HOST, 9)), Ok(0));
        let bind_b = stack.told().unwrap();
        assert_eq!(parse_header(&bind_b), Some((OP_BIND, b.sock)));
        assert_ne!(&bind_b[16..], &port_a.to_le_bytes());

        // No destination and no peer; a peer that is not IPv4; too long.
        assert_eq!(Socket::write(&a, b"x", None), Err(LxError::ENOTCONN));
        let v6 = Endpoint::Ip(IpEndpoint::new(
            IpAddress::Ipv6(smoltcp::wire::Ipv6Address::LOOPBACK),
            9,
        ));
        assert_eq!(
            Socket::write(&a, b"x", Some(v6)),
            Err(LxError::EAFNOSUPPORT)
        );
        // The longest the stack carries goes; one byte more does not.
        let long = alloc::vec![0u8; MAX_FOR_TESTS + 1];
        assert_eq!(
            Socket::write(&a, &long[..MAX_FOR_TESTS], to(HOST, 9)),
            Ok(MAX_FOR_TESTS)
        );
        assert_eq!(
            Socket::write(&a, &long, to(HOST, 9)),
            Err(LxError::EMSGSIZE)
        );
    }

    #[test]
    fn ephemeral_ports_skip_the_ones_taken_and_wrap() {
        let _g = LOCK.lock();
        let stack = stack();
        let first = *EPHEMERAL.start();
        let taken = stack.socket();
        assert_eq!(Socket::bind(&taken, any(first)), Ok(0));
        let a = stack.socket();
        assert_eq!(Socket::bind(&a, any(0)), Ok(0));
        assert_eq!(port(&a), first + 1);
        // From the last of the range the next is the first one free.
        stack.link.table.lock().ports.next = *EPHEMERAL.end();
        let b = stack.socket();
        let c = stack.socket();
        assert_eq!(Socket::bind(&b, any(0)), Ok(0));
        assert_eq!(Socket::bind(&c, any(0)), Ok(0));
        assert_eq!(port(&b), *EPHEMERAL.end());
        assert_eq!(port(&c), first + 2);
    }

    #[test]
    fn a_datagram_that_arrives_is_read_from_its_socket_with_who_sent_it() {
        let _g = LOCK.lock();
        let stack = stack();
        let a = stack.socket();
        let b = stack.socket();
        let mut buf = [0u8; 16];
        assert!(!readable(&a));
        assert_eq!(recv(&a, &mut buf).0, Err(LxError::EAGAIN));
        assert_eq!(FileLike::readable_bytes(&a), Some(0));

        // Two datagrams of one turn of the stack, in one message.
        let mut turn = data_message();
        push_record(&mut turn, a.sock, HOST, 4000, b"primero");
        push_record(&mut turn, a.sock, [10, 0, 9, 2], 4001, b"segundo, y largo");
        stack.say(turn);
        assert!(readable(&a));
        assert!(!readable(&b), "it was not for this one");
        assert_eq!(FileLike::readable_bytes(&a), Some(7));

        // Looked at, it is still there.
        assert_eq!(peek(&a, &mut buf), (Ok(7), at(HOST, 4000)));
        assert_eq!(recv(&a, &mut buf), (Ok(7), at(HOST, 4000)));
        assert_eq!(&buf[..7], b"primero");
        assert_eq!(Socket::take_msg_flags(&a), 0);

        // A buffer too short takes what fits, says so, and the rest is gone.
        let mut short = [0u8; 7];
        assert_eq!(recv(&a, &mut short), (Ok(7), at([10, 0, 9, 2], 4001)));
        assert_eq!(&short, b"segundo");
        assert_eq!(Socket::take_msg_flags(&a), 0x20);
        assert_eq!(Socket::take_msg_flags(&a), 0);
        assert!(!readable(&a));
        assert_eq!(recv(&a, &mut buf).0, Err(LxError::EAGAIN));

        // An empty datagram is read as one.
        stack.arrives(a.sock, HOST, 4000, b"");
        assert!(readable(&a));
        assert_eq!(recv(&a, &mut buf), (Ok(0), at(HOST, 4000)));
        assert_eq!(recv(&a, &mut buf).0, Err(LxError::EAGAIN));
    }

    #[test]
    fn what_the_stack_says_that_is_no_datagram_for_a_socket_is_dropped() {
        let _g = LOCK.lock();
        let stack = stack();
        let a = stack.socket();
        let gone = stack.socket();
        let gone_sock = gone.sock;
        drop(gone);
        stack.arrives(gone_sock, HOST, 1, b"late");
        stack.arrives(9999, HOST, 1, b"nobody's");
        stack.say(alloc::vec![1, 2, 3]);
        stack.say(bind_message(a.sock, 80));
        stack.say(send_message(a.sock, HOST, 1, b"not DATA"));
        // A DATA with nothing in it, and one whose second record is cut
        // short: the first is delivered all the same.
        stack.say(data_message());
        assert!(!readable(&a));
        let mut cut = data_message();
        push_record(&mut cut, a.sock, HOST, 1, b"whole");
        push_record(&mut cut, a.sock, HOST, 1, b"cut short");
        cut.truncate(cut.len() - 1);
        stack.say(cut);
        let mut buf = [0u8; 16];
        assert_eq!(recv(&a, &mut buf).0, Ok(5));
        assert!(!readable(&a));
        assert!(!stack.link.is_dead(), "nonsense does not end the stack");
        stack.arrives(a.sock, HOST, 1, b"real");
        assert!(readable(&a));
    }

    #[test]
    fn a_socket_holds_so_many_bytes_unread_and_drops_the_next_datagram() {
        let _g = LOCK.lock();
        let stack = stack();
        let a = stack.socket();
        let big = alloc::vec![7u8; 60_000];
        for _ in 0..RX_QUEUE_BYTES / big.len() {
            stack.arrives(a.sock, HOST, 1, &big);
        }
        stack.arrives(a.sock, HOST, 2, &big);
        let mut buf = alloc::vec![0u8; 65536];
        for _ in 0..RX_QUEUE_BYTES / big.len() {
            assert_eq!(recv(&a, &mut buf), (Ok(big.len()), at(HOST, 1)));
        }
        assert_eq!(
            recv(&a, &mut buf).0,
            Err(LxError::EAGAIN),
            "the one past the limit"
        );
        // With room again, the next is taken.
        stack.arrives(a.sock, HOST, 3, b"after");
        assert_eq!(recv(&a, &mut buf), (Ok(5), at(HOST, 3)));
    }

    #[test]
    fn a_connected_socket_sends_to_its_peer_and_hears_nobody_else() {
        let _g = LOCK.lock();
        let stack = stack();
        let a = stack.socket();
        assert!(Socket::remote_endpoint(&a).is_none());
        assert_eq!(
            async_std::task::block_on(Socket::connect(&a, Endpoint::Ip(at(HOST, 7)))),
            Ok(0)
        );
        assert_eq!(Socket::remote_endpoint(&a).map(ip), Some(at(HOST, 7)));
        assert_eq!(
            parse_header(&stack.told().unwrap()),
            Some((OP_BIND, a.sock))
        );
        assert_eq!(Socket::write(&a, b"ping", None), Ok(4));
        assert_eq!(stack.told(), Some(send_message(a.sock, HOST, 7, b"ping")));

        stack.arrives(a.sock, [10, 0, 9, 9], 7, b"a stranger");
        stack.arrives(a.sock, HOST, 8, b"another port");
        stack.arrives(a.sock, HOST, 7, b"pong");
        let mut buf = [0u8; 16];
        assert_eq!(recv(&a, &mut buf), (Ok(4), at(HOST, 7)));
        assert_eq!(&buf[..4], b"pong");
        assert_eq!(recv(&a, &mut buf).0, Err(LxError::EAGAIN));
    }

    #[test]
    fn shutdown_of_the_reading_side_reads_the_queue_and_then_the_end() {
        let _g = LOCK.lock();
        let stack = stack();
        let a = stack.socket();
        stack.arrives(a.sock, HOST, 1, b"queued");
        assert_eq!(Socket::shutdown(&a, 3), Err(LxError::EINVAL));
        // Not connected: ENOTCONN, and the side is shut all the same.
        assert_eq!(Socket::shutdown(&a, 0), Err(LxError::ENOTCONN));
        stack.arrives(a.sock, HOST, 1, b"after");
        let mut buf = [0u8; 16];
        assert_eq!(recv(&a, &mut buf).0, Ok(6));
        assert_eq!(recv(&a, &mut buf).0, Ok(0));
        assert!(readable(&a));
    }

    #[test]
    fn a_send_is_refused_while_the_stack_has_too_many_unread() {
        let _g = LOCK.lock();
        let stack = stack();
        let a = stack.socket();
        assert_eq!(Socket::bind(&a, any(7000)), Ok(0));
        // The bind is one of them.
        for _ in 1..SEND_BACKLOG {
            assert_eq!(Socket::write(&a, b"x", to(HOST, 9)), Ok(1));
        }
        assert_eq!(Socket::write(&a, b"x", to(HOST, 9)), Err(LxError::ENOBUFS));
        // A close is told all the same: the stack must not keep the socket.
        let b = stack.socket();
        drop(b);
        assert_eq!(stack.link.channel.peer_pending(), SEND_BACKLOG + 1);
        // Once the stack has read enough, a send goes through again.
        assert!(stack.told().is_some());
        assert_eq!(Socket::write(&a, b"x", to(HOST, 9)), Err(LxError::ENOBUFS));
        assert!(stack.told().is_some());
        assert_eq!(Socket::write(&a, b"x", to(HOST, 9)), Ok(1));
        let mut last = None;
        while let Some(m) = stack.told() {
            last = Some(m);
        }
        assert_eq!(last, Some(send_message(a.sock, HOST, 9, b"x")));
    }

    #[test]
    fn when_the_stack_dies_its_sockets_say_the_network_is_down() {
        let _g = LOCK.lock();
        let stack = stack();
        let a = stack.socket();
        stack.arrives(a.sock, HOST, 1, b"last");
        // One stack at a time.
        let (other, _other_end) = Channel::create();
        assert_eq!(
            serve(other, MAX_FOR_TESTS, STACK_ADDR).err(),
            Some(ZxError::ALREADY_BOUND)
        );
        // And one that carries nothing, or more than UDP does, is none.
        for bad in [0, MAX_DATAGRAM + 1] {
            let (other, _other_end) = Channel::create();
            assert_eq!(
                serve(other, bad, STACK_ADDR).err(),
                Some(ZxError::INVALID_ARGS)
            );
        }
        assert!(current().is_some());

        let Stack { link, end } = stack;
        drop(end);
        link.pump();
        assert!(link.is_dead());
        assert!(current().is_none());
        assert_eq!(
            ServedUdpSocket::new(link.clone()).err(),
            Some(LxError::ENETDOWN)
        );

        // What had arrived is read; then every operation says so, and
        // `poll` reports the socket readable and in error, so who waits on
        // it comes to read that.
        let mut buf = [0u8; 8];
        assert_eq!(recv(&a, &mut buf).0, Ok(4));
        assert_eq!(recv(&a, &mut buf).0, Err(LxError::ENETDOWN));
        assert_eq!(Socket::poll(&a, PollEvents::IN), (true, true, true));
        assert_eq!(Socket::write(&a, b"x", to(HOST, 9)), Err(LxError::ENETDOWN));
        assert_eq!(Socket::bind(&a, any(7000)), Err(LxError::ENETDOWN));
        // The port a refused bind took is not kept.
        assert!(link.table.lock().ports.taken.is_empty());

        // The next stack is served, and the old socket is not its.
        let next = self::stack();
        let b = next.socket();
        assert_eq!(Socket::bind(&b, any(7000)), Ok(0));
        assert_eq!(recv(&a, &mut buf).0, Err(LxError::ENETDOWN));
        drop(a);
        assert_eq!(next.told(), Some(bind_message(b.sock, 7000)));
        assert_eq!(next.told(), None, "the old socket's close is not told");
    }

    #[test]
    fn a_stack_found_dead_by_a_write_is_dead() {
        let _g = LOCK.lock();
        let Stack { link, end } = stack();
        let a = ServedUdpSocket::new(link.clone()).unwrap();
        drop(end);
        assert_eq!(Socket::write(&a, b"x", to(HOST, 9)), Err(LxError::ENETDOWN));
        assert!(link.is_dead());
        // And `serve` finds it out by itself when nobody pumped.
        let Stack { link, end } = stack();
        drop(end);
        assert!(!link.is_dead());
        let _next = stack();
        assert!(link.is_dead());
    }
}
