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
//! SEND    (kernel to stack)  16  [u8; 4] to    20  u16 port  22  u8 hop limit  23  u8 zero  24  the datagram
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
//! that it is dropped, as a full receive buffer drops. Each `SEND` says
//! how many hops its datagram may travel, which is the socket's `IP_TTL`
//! (`IP_MULTICAST_TTL` to a group): the socket's options are kept here
//! and the stack has none of them. A hop limit of 0 is the stack's own
//! choice, and what a kernel from before the byte had a meaning sends.
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
//! # ICMP
//!
//! `ping` asks for a socket that speaks ICMP, and it is served too
//! ([`ServedIcmpSocket`], in `icmp`): the raw one (`SOCK_RAW`), which
//! writes whole ICMP messages and reads whole IP packets, and the one that
//! only pings (`SOCK_DGRAM`):
//!
//! ```text
//! HEAR    (kernel to stack)
//! ICMP    (kernel to stack)  16  [u8; 4] to  20  u8 hop limit  21  three zero  24  the ICMP message
//! ```
//!
//! The stack keeps no ICMP socket apart from another: while one hears
//! (from its `HEAR` to its `CLOSE`) every IPv4 packet of ICMP the card
//! brings goes to the kernel, whole, as a record of a `DATA` for socket 0
//! and port 0, and the kernel gives each socket what is its own -- a raw
//! one all of them, as Linux does, and one that pings the answers to its
//! own echoes. An `ICMP` is a message to send for a socket that hears,
//! behind an IPv4 header the stack writes, with the hop limit the socket
//! has (`IP_TTL`). An echo to this machine is answered here, and never
//! reaches the stack.
//!
//! # What the card has carried
//!
//! The frames are the stack's and so is the count of them, which the
//! machine's interface has to show (`/proc/net/dev`, `ifconfig`):
//!
//! ```text
//! COUNTS  (stack to kernel)  16  eight u64: received bytes, frames, errors, dropped,
//!                                           then sent bytes, frames, errors, dropped
//! ```
//!
//! Each is how many since the process started, said every now and then
//! while they change and not for each frame. What is kept here
//! ([`StackLink::carried`]) is the sum of how much each grew, so the
//! interface's counts go on from where they were when a stack dies and
//! another comes, and a number that goes back adds nothing.
//!
//! # This machine
//!
//! What a program sends to 127.0.0.0/8, or to the stack's own address, is
//! for another program of this machine, and neither the card nor the
//! stack has anything to add to it. It does not go to the stack: the
//! kernel has both sockets, so a datagram goes from the one straight onto
//! the queue of the one that has the port, and a connection is two stream
//! sockets joined to each other -- what one writes is what the other has
//! to read, and what the other reads is the room the one gets back. The
//! same sockets, queues and counts as above, with the kernel saying to
//! each end what the stack would have said; no TCP, and no message. A
//! socket bound to an address of 127.0.0.0/8 is this machine's alone: the
//! stack is never told of it, so nothing from the card reaches it.
//!
//! # A stack that comes and goes
//!
//! The sockets are the machine's, not the stack process's
//! ([`StackLink`]). A program opens one before any stack is served, and
//! it works for what stays in the machine; what would leave it is
//! `ENETDOWN` until a stack comes. When the process dies -- it is a
//! process, it can be restarted -- a socket that listens goes on
//! listening and a datagram socket keeps its port, and the next process
//! to serve is told of them (`LISTEN`, `BIND`) as if they had just been
//! made. What ends with the process is what only it had, the TCP of the
//! connections it carried: those are over (`ENETDOWN`, once, then the
//! end), as when a machine in between forgets a connection. The numbers
//! it gave out are forgotten with it, so the next can give them again.
//!
//! # IPv6 sockets
//!
//! The stack has IPv4 and no IPv6, and a program does not ask which
//! before it opens its socket: plenty of them listen on an `AF_INET6`
//! socket at `::` and expect IPv4 to arrive there too. So an `AF_INET6`
//! socket is served like any other, and carries IPv4: `::` is every
//! address, `::ffff:a.b.c.d` is `a.b.c.d`, and `::1` is this machine,
//! which no version of IP has a part in. Any other IPv6 address is one
//! the stack cannot reach (`ENETUNREACH`) or does not have
//! (`EADDRNOTAVAIL`). Such a socket is told its addresses the same way,
//! IPv4 ones as `::ffff:a.b.c.d`.

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
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use kernel_hal::drivers::net::stack_card::Carried;
use kernel_hal::drivers::scheme::NetStats;
use kernel_hal::sync::Mutex;
use log::warn;
use smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address, Ipv6Address};
use zircon_object::impl_kobject;
use zircon_object::ipc::{Channel, MessagePacket};
use zircon_object::object::*;
use zircon_object::{ZxError, ZxResult};

mod icmp;
mod tcp;
pub use icmp::ServedIcmpSocket;
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
pub const OP_HEAR: u32 = 11;
pub const OP_ICMP: u32 = 12;
pub const OP_COUNTS: u32 = 13;

/// The socket of a `DATA` record that is an IPv4 packet of ICMP, for
/// whichever sockets hear: no socket has the number.
pub const HEARD: u64 = 0;

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
/// The numbers of a `COUNTS`.
pub const COUNTS: usize = 8;
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

/// `SEND`: `data` from socket `sock` for `addr:port`, to travel
/// `hop_limit` hops at most (0: as many as the stack gives a datagram by
/// itself, which is what a stack from before this byte had a meaning
/// reads there).
pub fn send_message(sock: u64, addr: [u8; 4], port: u16, hop_limit: u8, data: &[u8]) -> Vec<u8> {
    let mut m = header(OP_SEND, sock, 8 + data.len());
    m.extend_from_slice(&addr);
    m.extend_from_slice(&port.to_le_bytes());
    m.extend_from_slice(&[hop_limit, 0]);
    m.extend_from_slice(data);
    m
}

/// `HEAR`: socket `sock` hears ICMP from now on.
pub fn hear_message(sock: u64) -> Vec<u8> {
    header(OP_HEAR, sock, 0)
}

/// `ICMP`: `message` from socket `sock` to `addr`, to travel `hop_limit`
/// hops at most, which is not 0.
pub fn icmp_message(sock: u64, addr: [u8; 4], hop_limit: u8, message: &[u8]) -> Vec<u8> {
    let mut m = header(OP_ICMP, sock, 8 + message.len());
    m.extend_from_slice(&addr);
    m.extend_from_slice(&[hop_limit, 0, 0, 0]);
    m.extend_from_slice(message);
    m
}

/// `CLOSE`: socket `sock` is gone.
pub fn close_message(sock: u64) -> Vec<u8> {
    header(OP_CLOSE, sock, 0)
}

/// `COUNTS`: what the card has carried since the stack started, in the
/// order the message has them.
pub fn counts_message(counts: &[u64; COUNTS]) -> Vec<u8> {
    let mut m = header(OP_COUNTS, 0, COUNTS * 8);
    for count in counts {
        m.extend_from_slice(&count.to_le_bytes());
    }
    m
}

/// The numbers of a `COUNTS`, past its header: `None` when they are not
/// all there. What a later stack adds after them is not read.
pub fn parse_counts(numbers: &[u8]) -> Option<[u64; COUNTS]> {
    let mut counts = [0u64; COUNTS];
    for (i, count) in counts.iter_mut().enumerate() {
        *count = u64::from_le_bytes(numbers.get(i * 8..i * 8 + 8)?.try_into().ok()?);
    }
    Some(counts)
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
/// `SO_RCVTIMEO` and `SO_SNDTIMEO` as they were set: how long a socket that
/// blocks waits in a read or an accept (the first), in a write or a connect
/// (the second), before the call gives up. All zero waits without limit.
#[derive(Clone, Copy, Default)]
pub(super) struct Patience {
    pub(super) receive: [u8; 16],
    pub(super) send: [u8; 16],
}

impl Patience {
    /// `setsockopt`, when the option is one of the two.
    pub(super) fn set(&mut self, level: usize, opt: usize, data: &[u8]) -> Option<SysResult> {
        const SOL_SOCKET: usize = 1;
        const SO_RCVTIMEO: usize = 20;
        const SO_SNDTIMEO: usize = 21;
        if level != SOL_SOCKET || !matches!(opt, SO_RCVTIMEO | SO_SNDTIMEO) {
            return None;
        }
        Some(sockopt_timeval(data).map(|tv| {
            if opt == SO_RCVTIMEO {
                self.receive = tv;
            } else {
                self.send = tv;
            }
            0
        }))
    }
}

/// Where the waits of one call end. Reckoned at the first of them, so a
/// call that finds what it came for never reads the clock, and kept for
/// the rest: the time is the call's, not each wait's.
#[derive(Default)]
pub(super) struct Until(Option<Option<core::time::Duration>>);

impl Until {
    /// The end for a call that waits `tv` (a `struct timeval`).
    pub(super) fn of(&mut self, tv: &[u8; 16]) -> Option<core::time::Duration> {
        *self.0.get_or_insert_with(|| sockopt_deadline(tv))
    }
}

struct Shared {
    /// `READABLE` while the queue holds a datagram (or reading is over),
    /// `WRITABLE` always. Changed under `inner`.
    bus: Arc<Mutex<EventBus>>,
    inner: Mutex<Inner>,
    hears: Hears,
    /// An `AF_INET6` socket's: which table lists it.
    v6: bool,
}

/// What is queued on a socket.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Hears {
    /// The datagrams for its port.
    Datagrams,
    /// Every IPv4 packet of ICMP, whole: a raw socket.
    Icmp,
    /// The answers to the echoes it sent, which carry its `port` as
    /// their identifier, without the IPv4 header: a socket that pings.
    Echoes,
}

struct Inner {
    queue: VecDeque<(IpEndpoint, Vec<u8>)>,
    queued_bytes: usize,
    /// The port, once bound.
    port: Option<u16>,
    /// The address of 127.0.0.0/8 it was bound to, when it was: a socket
    /// of this machine alone, which the stack is not told of.
    here: Option<Ipv4Address>,
    /// The peer of a connected socket.
    remote: Option<IpEndpoint>,
    flags: OpenFlags,
    /// `MSG_TRUNC` of the last read, for `recvmsg`.
    last_msg_flags: i32,
    /// `shutdown(SHUT_RD)`: an empty queue is the end.
    read_closed: bool,
    patience: Patience,
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

    /// A socket with nothing queued, that can be written to: what cannot
    /// be sent is refused, not waited for.
    fn new(hears: Hears, v6: bool) -> Arc<Shared> {
        let mut bus = EventBus::default();
        bus.set(Event::WRITABLE);
        Arc::new(Shared {
            bus: Arc::new(Mutex::new(bus)),
            inner: Mutex::new(Inner {
                queue: VecDeque::new(),
                queued_bytes: 0,
                port: None,
                here: None,
                remote: None,
                flags: OpenFlags::RDWR,
                last_msg_flags: 0,
                read_closed: false,
                patience: Patience::default(),
            }),
            hears,
            v6,
        })
    }

    /// Queue what is this socket's of an IPv4 packet of ICMP from `from`.
    fn deliver_heard(&self, from: Ipv4Address, packet: &[u8]) {
        let from = IpEndpoint::new(IpAddress::Ipv4(from), 0);
        match self.hears {
            Hears::Datagrams => {}
            Hears::Icmp => {
                self.deliver(from, packet);
            }
            Hears::Echoes => {
                let Some(message) = icmp::message_of(packet) else {
                    return;
                };
                let ident = self.inner.lock().port;
                if icmp::echo_reply_ident(message).is_some_and(|i| Some(i) == ident) {
                    self.deliver(from, message);
                }
            }
        }
    }

    /// The next datagram, taken off the queue or only looked at, as a
    /// socket of that family says who sent it.
    async fn receive(&self, v6: bool, data: &mut [u8], take: bool) -> (SysResult, Endpoint) {
        let nobody = || Endpoint::Ip(IpEndpoint::UNSPECIFIED);
        let mut until = Until::default();
        loop {
            let until = {
                let mut guard = self.inner.lock();
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
                    if inner.queue.is_empty() && !inner.read_closed {
                        self.bus.lock().clear(Event::READABLE);
                    }
                    return (Ok(n), Endpoint::Ip(shown(v6, from)));
                }
                if inner.read_closed {
                    return (Ok(0), nobody());
                }
                self.bus.lock().clear(Event::READABLE);
                if inner.flags.contains(OpenFlags::NON_BLOCK) {
                    return (Err(LxError::EAGAIN), nobody());
                }
                until.of(&inner.patience.receive)
            };
            // Woken by a datagram, or ended by a signal or by `SO_RCVTIMEO`
            // (`EAGAIN`).
            let bus = self.bus.clone();
            if let Err(e) = crate::sync::wait_for_event_until(bus, Event::READABLE, until).await {
                return (Err(e), nobody());
            }
        }
    }

    /// The wait of a read that blocks, outside `read` (a `splice` from
    /// the socket): it ends with `SO_RCVTIMEO` as the read's own does.
    async fn wait_readable(&self, until: &mut Until) -> LxResult<()> {
        let until = until.of(&self.inner.lock().patience.receive);
        let bus = self.bus.clone();
        crate::sync::wait_for_event_until(bus, Event::READABLE, until).await?;
        Ok(())
    }

    /// Queue a datagram another program of this machine sent to `to`. A
    /// socket bound to one address hears what is for that address.
    fn deliver_here(&self, to: Ipv4Address, from: IpEndpoint, data: &[u8]) {
        let here = self.inner.lock().here;
        if here.is_none_or(|here| here == to) {
            self.deliver(from, data);
        }
    }

    /// `shutdown(SHUT_RD)`: whoever waits to read is told.
    fn end_reading(&self) {
        let mut inner = self.inner.lock();
        inner.read_closed = true;
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
    /// Which datagram socket has each port: where a datagram from another
    /// program of this machine goes.
    bound: BTreeMap<u16, u64>,
    /// The sockets that hear ICMP.
    hearing: BTreeSet<u64>,
    /// The identifiers of the sockets that ping: their ports.
    idents: Ports,
    /// Which stream socket listens on each port: where a connection from
    /// another program of this machine goes.
    listening: BTreeMap<u16, u64>,
    /// The connections between two programs of this machine: each end's
    /// number to the other's, and to 0 once the other end has closed.
    joined: BTreeMap<u64, u64>,
}

/// A served socket as the machine's tables of sockets list it
/// (`/proc/net/tcp`, `/proc/net/udp`): what the kernel knows of it without
/// asking the stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Listed {
    /// Its address and port: no address when it has every one.
    pub local: IpEndpoint,
    /// Its peer, when it has one.
    pub remote: Option<IpEndpoint>,
    /// Its state, as Linux numbers them (`include/net/tcp_states.h`).
    pub state: u8,
    /// The bytes that arrived for it and no program has read.
    pub unread: usize,
    /// An `AF_INET6` socket: one of `/proc/net/tcp6` or `/proc/net/udp6`,
    /// where its addresses, which are IPv4's, are written as that family
    /// writes them (`::ffff:a.b.c.d`, and `::` for every address).
    pub v6: bool,
}

/// The states of a [`Listed`].
pub const TCP_ESTABLISHED: u8 = 1;
pub const TCP_SYN_SENT: u8 = 2;
pub const TCP_FIN_WAIT2: u8 = 5;
pub const TCP_CLOSE: u8 = 7;
pub const TCP_CLOSE_WAIT: u8 = 8;
pub const TCP_LAST_ACK: u8 = 9;
pub const TCP_LISTEN: u8 = 10;

/// The stack process that serves now.
struct Served {
    channel: Arc<Channel>,
    /// Which it is of the stacks there have been: 1 the first.
    turn: u64,
}

/// The served sockets of this machine, and the stack process that carries
/// what they say beyond it, while there is one.
///
/// The sockets are the machine's and not the process's: they are here
/// before the first stack is served, and when a stack dies they wait for
/// the next, which is told of the ones that are bound or listen. What
/// dies with a stack is what only it had: the connections it carried.
pub struct StackLink {
    served: Mutex<Option<Served>>,
    /// Whether a stack serves now: `served`, without its lock.
    up: AtomicBool,
    /// How many stacks there have been.
    turns: AtomicU64,
    /// The longest datagram the stack carries: what fits one frame of its
    /// card, for a stack that does not fragment.
    max_datagram: AtomicUsize,
    /// The stack's address, which is the local address of every
    /// connection it carries, as the bytes of a big-endian number: 0
    /// while there is no stack, or it did not say.
    address: AtomicU32,
    table: Mutex<Table>,
    /// Held while a socket takes a port or starts to listen and the stack
    /// is told, and while a new stack is told of all that have: so each
    /// is told once. Taken before any other lock here.
    telling: Mutex<()>,
    /// What the stacks' card has carried, all of them: see
    /// [`carried`](Self::carried).
    carried: Arc<Carried>,
    /// The last `COUNTS` of the stack that serves now: zero when it has
    /// said none, and for the next.
    counted: Mutex<[u64; COUNTS]>,
}

/// A stack process's turn at serving: what the task that reads its
/// channel holds.
pub struct Serving {
    link: Arc<StackLink>,
    channel: Arc<Channel>,
    turn: u64,
}

impl Serving {
    /// The sockets it serves.
    pub fn link(&self) -> &Arc<StackLink> {
        &self.link
    }

    /// The channel, for whoever sleeps on it until there is something to
    /// [`pump`](Self::pump).
    pub fn channel(&self) -> &Arc<Channel> {
        &self.channel
    }

    /// Whether the process is gone.
    pub fn is_over(&self) -> bool {
        !self.link.serves(self.turn)
    }

    /// Read what the stack has sent: each datagram and event to its
    /// socket.
    pub fn pump(&self) {
        self.link.pump_on(&self.channel, self.turn);
    }
}

/// The served sockets of this machine.
static MACHINE: Mutex<Option<Arc<StackLink>>> = Mutex::new(None);

/// The served sockets of this machine: where the socket of a program
/// that asked for the stack that runs as a process is made, whether one
/// runs yet or not.
pub fn machine() -> Arc<StackLink> {
    MACHINE.lock().get_or_insert_with(StackLink::new).clone()
}

/// A process serves a stack on `channel`, the kernel's end, that carries
/// datagrams of up to `max_datagram` bytes: a longer one is `EMSGSIZE` to
/// who sends it. `address` is the stack's own, which a connected socket
/// answers `getsockname` with. `ALREADY_BOUND` while another process
/// serves one.
pub fn serve(channel: Arc<Channel>, max_datagram: usize, address: [u8; 4]) -> ZxResult<Serving> {
    machine().attach(channel, max_datagram, address)
}

/// Whether every program's sockets are the served stack's, asked for or
/// not: see [`use_for_all`].
static FOR_ALL: AtomicBool = AtomicBool::new(false);

/// From now on every program is one that asked (`unetuse=all` on the
/// kernel command line): the machine's network is the stack that runs as
/// a process, and the kernel's own stack is nobody's.
pub fn use_for_all(on: bool) {
    FOR_ALL.store(on, Ordering::Relaxed);
}

/// Whether [`use_for_all`] is on.
pub fn is_for_all() -> bool {
    FOR_ALL.load(Ordering::Relaxed)
}

/// The IPv4 address a program names with `addr` on a socket of its
/// family: see "IPv6 sockets" above. `elsewhere` is the answer for an
/// IPv6 address that names nothing a stack without IPv6 has.
fn plain(v6: bool, addr: IpAddress, elsewhere: LxError) -> LxResult<IpAddress> {
    match (v6, addr) {
        (_, IpAddress::Unspecified) | (false, IpAddress::Ipv4(_)) => Ok(addr),
        (true, IpAddress::Ipv6(addr)) => {
            if addr.is_unspecified() {
                Ok(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED))
            } else if addr.is_loopback() {
                Ok(IpAddress::v4(127, 0, 0, 1))
            } else {
                addr.as_ipv4().map(IpAddress::Ipv4).ok_or(elsewhere)
            }
        }
        _ => Err(LxError::EAFNOSUPPORT),
    }
}

/// `endpoint` as a socket of that family says it: the other way of
/// [`plain`].
fn shown(v6: bool, endpoint: IpEndpoint) -> IpEndpoint {
    if !v6 {
        return endpoint;
    }
    let addr = match endpoint.addr {
        IpAddress::Ipv4(addr) if !addr.is_unspecified() => Ipv6Address::from(addr),
        IpAddress::Ipv6(addr) => addr,
        _ => Ipv6Address::UNSPECIFIED,
    };
    IpEndpoint::new(IpAddress::Ipv6(addr), endpoint.port)
}

impl StackLink {
    /// Sockets that no stack serves yet.
    pub fn new() -> Arc<StackLink> {
        Arc::new(StackLink {
            served: Mutex::new(None),
            up: AtomicBool::new(false),
            turns: AtomicU64::new(0),
            max_datagram: AtomicUsize::new(0),
            address: AtomicU32::new(0),
            table: Mutex::new(Table::default()),
            telling: Mutex::new(()),
            carried: Arc::new(Carried::default()),
            counted: Mutex::new([0; COUNTS]),
        })
    }

    /// The datagram sockets as `/proc/net/udp` and `/proc/net/udp6` list
    /// them between the two, in the order of their numbers: the ones that have a port, as Linux lists the
    /// ones in its tables. One with a peer is established, as there, and
    /// the others are closed, which is how a datagram socket that is
    /// open and has no peer is listed. A socket that speaks ICMP is not
    /// UDP's and is not here.
    pub fn listed_datagrams(&self) -> Vec<Listed> {
        let socks: Vec<Arc<Shared>> = {
            let table = self.table.lock();
            table.socks.values().filter_map(Weak::upgrade).collect()
        };
        socks
            .iter()
            .filter(|shared| shared.hears == Hears::Datagrams)
            .filter_map(|shared| {
                let inner = shared.inner.lock();
                let addr = inner.here.unwrap_or(Ipv4Address::UNSPECIFIED);
                Some(Listed {
                    local: IpEndpoint::new(IpAddress::Ipv4(addr), inner.port?),
                    remote: inner.remote,
                    state: if inner.remote.is_some() {
                        TCP_ESTABLISHED
                    } else {
                        TCP_CLOSE
                    },
                    unread: inner.queued_bytes,
                    v6: shared.v6,
                })
            })
            .collect()
    }

    /// What the card of the stacks that have served these sockets has
    /// carried, as each said it (`COUNTS`): for the interface that shows
    /// that card. It is the machine's, so it goes on through a stack
    /// that dies and the one that comes after.
    pub fn carried(&self) -> Arc<Carried> {
        self.carried.clone()
    }

    /// [`serve`], on these sockets. The stack is told of the sockets that
    /// are there already and are its business: the datagram sockets that
    /// have a port and the stream sockets that listen, less the ones
    /// that are this machine's alone, and the ones that hear ICMP.
    pub fn attach(
        self: &Arc<Self>,
        channel: Arc<Channel>,
        max_datagram: usize,
        address: [u8; 4],
    ) -> ZxResult<Serving> {
        if !(1..=MAX_DATAGRAM).contains(&max_datagram) {
            return Err(ZxError::INVALID_ARGS);
        }
        // The one before may have died a moment ago, with nobody there
        // yet to say so: ask its channel.
        self.pump();
        let _telling = self.telling.lock();
        let turn = {
            let mut served = self.served.lock();
            if served.is_some() {
                return Err(ZxError::ALREADY_BOUND);
            }
            let turn = self.turns.fetch_add(1, Ordering::AcqRel) + 1;
            self.max_datagram.store(max_datagram, Ordering::Release);
            self.address
                .store(u32::from_be_bytes(address), Ordering::Release);
            *served = Some(Served {
                channel: channel.clone(),
                turn,
            });
            self.up.store(true, Ordering::Release);
            turn
        };
        let (bound, listening, hearing) = {
            let table = self.table.lock();
            let bound: Vec<(u64, u16, Arc<Shared>)> = table
                .bound
                .iter()
                .filter_map(|(port, sock)| {
                    let shared = table.socks.get(sock).and_then(Weak::upgrade)?;
                    Some((*sock, *port, shared))
                })
                .collect();
            let listening: Vec<(u64, u16, Arc<tcp::Stream>)> = table
                .listening
                .iter()
                .filter_map(|(port, sock)| {
                    let stream = table.streams.get(sock).and_then(Weak::upgrade)?;
                    Some((*sock, *port, stream))
                })
                .collect();
            let hearing: Vec<u64> = table.hearing.iter().copied().collect();
            (bound, listening, hearing)
        };
        for sock in hearing {
            let _ = self.tell(hear_message(sock), true);
        }
        for (sock, port, shared) in bound {
            if shared.inner.lock().here.is_none() {
                let _ = self.tell(bind_message(sock, port), true);
            }
        }
        for (sock, port, stream) in listening {
            if stream.listens_for_the_stack() {
                let listen = listen_message(sock, port, LISTEN_BACKLOG, STREAM_WINDOW as u32);
                let _ = self.tell(listen, true);
            }
        }
        Ok(Serving {
            link: self.clone(),
            channel,
            turn,
        })
    }

    /// Whether no stack serves now: what leaves the machine has nowhere
    /// to go (`ENETDOWN`). What stays in it does not notice.
    pub fn is_down(&self) -> bool {
        !self.up.load(Ordering::Acquire)
    }

    /// Whether the stack of that turn still serves.
    fn serves(&self, turn: u64) -> bool {
        !self.is_down() && self.turns.load(Ordering::Acquire) == turn
    }

    /// The stack that serves now: its channel and its turn.
    fn current(&self) -> Option<(Arc<Channel>, u64)> {
        if self.is_down() {
            return None;
        }
        let served = self.served.lock();
        served.as_ref().map(|s| (s.channel.clone(), s.turn))
    }

    /// The stack's address: unspecified while there is none, or it did
    /// not say.
    fn address(&self) -> Ipv4Address {
        Ipv4Address(self.address.load(Ordering::Acquire).to_be_bytes())
    }

    /// Read what the stack that serves now has sent.
    pub fn pump(&self) {
        if let Some((channel, turn)) = self.current() {
            self.pump_on(&channel, turn);
        }
    }

    /// Read what the stack of that turn has sent: each datagram onto its
    /// socket's queue, each event to its socket.
    fn pump_on(&self, channel: &Arc<Channel>, turn: u64) {
        while self.serves(turn) {
            match channel.read() {
                Ok(msg) => self.take(&msg.data),
                Err(ZxError::SHOULD_WAIT) => return,
                Err(_) => self.gone(turn),
            }
        }
    }

    fn take(&self, m: &[u8]) {
        match parse_header(m) {
            Some((OP_DATA, _)) => self.take_datagrams(&m[HEADER_LEN..]),
            Some((OP_EVENTS, _)) => self.take_events(&m[HEADER_LEN..]),
            Some((OP_COUNTS, _)) => self.take_counts(&m[HEADER_LEN..]),
            _ => warn!(
                "user stack: a message that means nothing ({} bytes)",
                m.len()
            ),
        }
    }

    /// The card has carried this much since its stack started: what is
    /// more than that stack said last is added to what the machine's
    /// interface has carried.
    fn take_counts(&self, numbers: &[u8]) {
        let Some(now) = parse_counts(numbers) else {
            warn!(
                "user stack: {} bytes that are no counts of a card",
                numbers.len()
            );
            return;
        };
        let more = {
            let mut counted = self.counted.lock();
            let mut more = [0u64; COUNTS];
            for ((more, now), was) in more.iter_mut().zip(now).zip(counted.iter()) {
                // A count that goes back is a stack that lost count:
                // nothing is taken away for it.
                *more = now.saturating_sub(*was);
            }
            *counted = now;
            more
        };
        let [rx_bytes, rx_packets, rx_errors, rx_dropped, tx_bytes, tx_packets, tx_errors, tx_dropped] =
            more;
        self.carried.add(&NetStats {
            rx_bytes,
            rx_packets,
            tx_bytes,
            tx_packets,
            rx_errors,
            rx_dropped,
            tx_errors,
            tx_dropped,
        });
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
            if sock == HEARD {
                self.heard(Ipv4Address(addr), data);
                continue;
            }
            // A datagram for a socket that closed meanwhile is nobody's.
            let shared = self.table.lock().socks.get(&sock).and_then(Weak::upgrade);
            if let Some(shared) = shared {
                let from = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address(addr)), port);
                shared.deliver(from, data);
            }
        }
    }

    /// The process of that turn is gone, and the next stack can be
    /// served. The sockets stay: a listener goes on listening and a
    /// datagram socket keeps its port, for the programs of this machine
    /// now and for the next stack when it comes. What goes is what only
    /// the process had: a connection it carried is over (`ENETDOWN`),
    /// for who was asleep on it too.
    fn gone(&self, turn: u64) {
        {
            let mut served = self.served.lock();
            if served.as_ref().map(|s| s.turn) != Some(turn) {
                return;
            }
            *served = None;
            self.up.store(false, Ordering::Release);
            self.address.store(0, Ordering::Release);
            // The next one counts from nothing.
            *self.counted.lock() = [0; COUNTS];
        }
        warn!("user stack: the process is gone");
        let streams: Vec<Arc<tcp::Stream>> = {
            let mut table = self.table.lock();
            let streams = table.streams.values().filter_map(Weak::upgrade).collect();
            // The numbers the process gave out are the next one's to give.
            table.streams.retain(|sock, _| sock & ACCEPTED_BIT == 0);
            streams
        };
        for stream in streams {
            stream.lose();
        }
    }

    /// A message to the stack. `ENETDOWN` when there is none, `ENOBUFS`
    /// when it has [`SEND_BACKLOG`] unread and `urgent` is not set.
    fn tell(&self, data: Vec<u8>, urgent: bool) -> LxResult<()> {
        let Some((channel, turn)) = self.current() else {
            return Err(LxError::ENETDOWN);
        };
        if !urgent && channel.peer_pending() >= SEND_BACKLOG {
            return Err(LxError::ENOBUFS);
        }
        let msg = MessagePacket {
            data,
            handles: Vec::new(),
        };
        channel.write(msg).map_err(|_| {
            self.gone(turn);
            LxError::ENETDOWN
        })
    }

    /// Give a socket the port `wanted`, or any when it is 0, and tell the
    /// stack, unless the socket is this machine's alone (`told`).
    /// `EADDRINUSE` when another has it or none is left. With no stack
    /// to tell, the port is the socket's all the same: the next stack is
    /// told when it comes.
    fn bind(&self, sock: u64, wanted: u16, told: bool) -> LxResult<u16> {
        let _telling = self.telling.lock();
        let port = {
            let mut table = self.table.lock();
            let port = table.ports.take(wanted)?;
            table.bound.insert(port, sock);
            port
        };
        if told {
            let _ = self.tell(bind_message(sock, port), true);
        }
        Ok(port)
    }

    /// A socket is gone: its number and its port are free, and the stack
    /// is told, if it was ever told of the socket (`told`).
    fn close(&self, sock: u64, port: Option<u16>, told: bool) {
        {
            let mut table = self.table.lock();
            table.socks.remove(&sock);
            if let Some(port) = port {
                table.ports.taken.remove(&port);
                table.bound.remove(&port);
            }
        }
        if told {
            let _ = self.tell(close_message(sock), true);
        }
    }

    /// A socket that speaks ICMP is there: the stack is told to hear for
    /// it, and the next stack when it comes.
    fn hear(&self, sock: u64) {
        let _telling = self.telling.lock();
        self.table.lock().hearing.insert(sock);
        let _ = self.tell(hear_message(sock), true);
    }

    /// A socket that speaks ICMP is gone: its number is free, and its
    /// identifier if it had one, and the stack is told.
    fn deaf(&self, sock: u64, ident: Option<u16>) {
        let _telling = self.telling.lock();
        {
            let mut table = self.table.lock();
            table.socks.remove(&sock);
            table.hearing.remove(&sock);
            if let Some(ident) = ident {
                table.idents.taken.remove(&ident);
            }
        }
        let _ = self.tell(close_message(sock), true);
    }

    /// Give a socket that pings the identifier `wanted`, or any when it
    /// is 0. `EADDRINUSE` when another has it or none is left.
    fn ident(&self, wanted: u16) -> LxResult<u16> {
        self.table.lock().idents.take(wanted)
    }

    /// An IPv4 packet of ICMP from `from`, off the card or made here:
    /// to every socket that hears, which takes what is its own.
    fn heard(&self, from: Ipv4Address, packet: &[u8]) {
        let hearing: Vec<Arc<Shared>> = {
            let table = self.table.lock();
            let socks = table.hearing.iter();
            socks
                .filter_map(|sock| table.socks.get(sock).and_then(Weak::upgrade))
                .collect()
        };
        for shared in hearing {
            shared.deliver_heard(from, packet);
        }
    }

    /// Whether `addr` is this machine: what is sent there is for another
    /// of its programs, and never leaves it.
    fn is_here(&self, addr: Ipv4Address) -> bool {
        addr.is_loopback() || (!self.address().is_unspecified() && addr == self.address())
    }

    /// The address this machine speaks from to who it reaches at `to`,
    /// which is one of its own: 127.0.0.1 within 127.0.0.0/8, as Linux
    /// picks it, and the stack's address to the stack's address.
    fn here_for(&self, to: Ipv4Address) -> Ipv4Address {
        if to.is_loopback() {
            Ipv4Address::new(127, 0, 0, 1)
        } else {
            to
        }
    }

    /// A datagram from one program of this machine to another: onto the
    /// queue of the socket that has the port, if one has. One that nobody
    /// has it for is lost, as a datagram is.
    fn send_here(&self, from: IpEndpoint, to: Ipv4Address, port: u16, data: &[u8]) {
        let shared = {
            let table = self.table.lock();
            let sock = table.bound.get(&port);
            sock.and_then(|sock| table.socks.get(sock))
                .and_then(Weak::upgrade)
        };
        if let Some(shared) = shared {
            shared.deliver_here(to, from, data);
        }
    }
}

/// A datagram socket whose stack is a process.
pub struct ServedUdpSocket {
    base: KObjectBase,
    link: Arc<StackLink>,
    sock: u64,
    /// An `AF_INET6` socket: see "IPv6 sockets" at the top.
    v6: bool,
    shared: Arc<Shared>,
    /// What `setsockopt` was told, for `getsockopt` to say again; the
    /// hop limits go with each datagram.
    opts: Mutex<StoredInetOpts>,
}

impl ServedUdpSocket {
    /// A socket of `link`, `AF_INET6` when `v6`.
    pub fn new(link: Arc<StackLink>, v6: bool) -> LxResult<Self> {
        let shared = Shared::new(Hears::Datagrams, v6);
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
            v6,
            shared,
            opts: Mutex::new(StoredInetOpts::default()),
        })
    }

    /// How many hops a datagram for `to` may travel: `IP_TTL`, or
    /// `IP_MULTICAST_TTL` when `to` is a group's address. Never 0, which
    /// on the wire to the stack means the stack's own.
    fn hop_limit(&self, to: Ipv4Address) -> u8 {
        let opts = self.opts.lock();
        let limit = if to.is_multicast() {
            opts.mcast_ttl
        } else {
            opts.ip_ttl
        };
        limit.clamp(1, 255) as u8
    }

    /// The socket's port, binding it to any when it has none: what a send
    /// or a connect on an unbound socket does.
    fn autobind(&self) -> LxResult<u16> {
        if let Some(port) = self.shared.inner.lock().port {
            return Ok(port);
        }
        let port = self.link.bind(self.sock, 0, true)?;
        self.shared.inner.lock().port = Some(port);
        Ok(port)
    }
}

/// A question the kernel itself asks for a program whose sockets are
/// served, and waits the answer of: its resolver's, to a name server.
/// The program's own sockets go by the stack that runs as a process, so
/// what the kernel asks on its behalf goes the same way, from a datagram
/// socket of this machine like any other.
pub struct Asked {
    socket: ServedUdpSocket,
    /// When the wait for the answer ends.
    until: core::time::Duration,
}

/// Send `question` to `to` by the stack of `link`, to wait `patience` for
/// what comes back. What a `sendto` would say when it cannot go:
/// `ENETDOWN` with no stack, `EAFNOSUPPORT` for an IPv6 address.
pub async fn ask(
    link: &Arc<StackLink>,
    to: IpEndpoint,
    question: &[u8],
    patience: core::time::Duration,
) -> LxResult<Asked> {
    let socket = ServedUdpSocket::new(link.clone(), false)?;
    // Connected: what anybody else sends to its port is not an answer.
    socket.connect(Endpoint::Ip(to)).await?;
    Socket::write(&socket, question, None)?;
    Ok(Asked {
        socket,
        until: kernel_hal::timer::timer_now() + patience,
    })
}

impl Asked {
    /// The next datagram from who was asked. `ETIMEDOUT` when the
    /// patience ran out first, `EINTR` when a signal ended the wait.
    pub async fn answer(&self) -> LxResult<Vec<u8>> {
        let now = kernel_hal::timer::timer_now();
        let left = self.until.checked_sub(now).filter(|left| !left.is_zero());
        let left = left.ok_or(LxError::ETIMEDOUT)?;
        // The read gives up as `SO_RCVTIMEO` has it: all zero would be
        // never.
        let micros = left.as_micros().max(1) as u64;
        let mut tv = [0u8; 16];
        tv[..8].copy_from_slice(&((micros / 1_000_000) as i64).to_ne_bytes());
        tv[8..].copy_from_slice(&((micros % 1_000_000) as i64).to_ne_bytes());
        self.socket.shared.inner.lock().patience.receive = tv;
        let mut buf = vec![0u8; MAX_DATAGRAM];
        match self.socket.shared.receive(false, &mut buf, true).await.0 {
            Ok(n) => {
                buf.truncate(n);
                Ok(buf)
            }
            Err(LxError::EAGAIN) => Err(LxError::ETIMEDOUT),
            Err(e) => Err(e),
        }
    }
}

impl Drop for ServedUdpSocket {
    fn drop(&mut self) {
        let (port, here) = {
            let inner = self.shared.inner.lock();
            (inner.port, inner.here)
        };
        self.link.close(self.sock, port, here.is_none());
    }
}

#[async_trait]
impl Socket for ServedUdpSocket {
    async fn read(&self, data: &mut [u8]) -> (SysResult, Endpoint) {
        self.shared.receive(self.v6, data, true).await
    }

    async fn peek(&self, data: &mut [u8]) -> (SysResult, Endpoint) {
        self.shared.receive(self.v6, data, false).await
    }

    fn write(&self, data: &[u8], sendto_endpoint: Option<Endpoint>) -> SysResult {
        let to = match sendto_endpoint {
            Some(Endpoint::Ip(mut to)) => {
                to.addr = plain(self.v6, to.addr, LxError::ENETUNREACH)?;
                to
            }
            Some(_) => return Err(LxError::EAFNOSUPPORT),
            None => self.shared.inner.lock().remote.ok_or(LxError::ENOTCONN)?,
        };
        let IpAddress::Ipv4(addr) = to.addr else {
            return Err(LxError::EAFNOSUPPORT);
        };
        let here = self.shared.inner.lock().here;
        if self.link.is_here(addr) {
            // No card, so no frame to fit: the longest datagram there is.
            if data.len() > MAX_DATAGRAM {
                return Err(LxError::EMSGSIZE);
            }
            let port = self.autobind()?;
            let from = here.unwrap_or_else(|| self.link.here_for(addr));
            let from = IpEndpoint::new(IpAddress::Ipv4(from), port);
            self.link.send_here(from, addr, to.port, data);
            return Ok(data.len());
        }
        // `ip_route_output`: nothing leaves the machine from an address
        // that is only the machine's.
        if here.is_some() {
            return Err(LxError::EINVAL);
        }
        // Nothing leaves the machine while no stack serves.
        if self.link.is_down() {
            return Err(LxError::ENETDOWN);
        }
        if data.len() > self.link.max_datagram.load(Ordering::Acquire) {
            return Err(LxError::EMSGSIZE);
        }
        self.autobind()?;
        let said = send_message(self.sock, addr.0, to.port, self.hop_limit(addr), data);
        self.link.tell(said, false)?;
        Ok(data.len())
    }

    fn poll(&self, _events: PollEvents) -> (bool, bool, bool) {
        let inner = self.shared.inner.lock();
        (!inner.queue.is_empty() || inner.read_closed, true, false)
    }

    async fn connect(&self, endpoint: Endpoint) -> SysResult {
        let Endpoint::Ip(mut peer) = endpoint else {
            return Err(LxError::EAFNOSUPPORT);
        };
        peer.addr = plain(self.v6, peer.addr, LxError::ENETUNREACH)?;
        if !matches!(peer.addr, IpAddress::Ipv4(_)) {
            return Err(LxError::EAFNOSUPPORT);
        }
        self.autobind()?;
        self.shared.inner.lock().remote = Some(peer);
        Ok(0)
    }

    fn bind(&self, endpoint: Endpoint) -> SysResult {
        let Endpoint::Ip(mut local) = endpoint else {
            return Err(LxError::EINVAL);
        };
        local.addr = plain(self.v6, local.addr, LxError::EADDRNOTAVAIL)?;
        // `inet_bind`: a socket that has a port is `EINVAL`.
        if self.shared.inner.lock().port.is_some() {
            return Err(LxError::EINVAL);
        }
        // At an address of 127.0.0.0/8 it is this machine's alone: the
        // stack, which would bring it what the card hears, is not told.
        let here = match local.addr {
            IpAddress::Ipv4(addr) if addr.is_loopback() => Some(addr),
            _ => None,
        };
        // Said before the port is taken: a stack that starts to serve
        // just then is told of the sockets that are not this machine's
        // alone.
        self.shared.inner.lock().here = here;
        let bound = self.link.bind(self.sock, local.port, here.is_none());
        let mut inner = self.shared.inner.lock();
        match bound {
            Ok(port) => inner.port = Some(port),
            Err(e) => {
                inner.here = None;
                return Err(e);
            }
        }
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
        let inner = self.shared.inner.lock();
        let local = IpEndpoint::new(
            IpAddress::Ipv4(inner.here.unwrap_or(Ipv4Address::UNSPECIFIED)),
            inner.port.unwrap_or(0),
        );
        Some(Endpoint::Ip(shown(self.v6, local)))
    }

    fn remote_endpoint(&self) -> Option<Endpoint> {
        let remote = self.shared.inner.lock().remote?;
        Some(Endpoint::Ip(shown(self.v6, remote)))
    }

    fn is_inet(&self) -> bool {
        true
    }

    fn so_domain(&self) -> Option<u32> {
        Some(if self.v6 {
            Domain::AF_INET6 as u32
        } else {
            Domain::AF_INET as u32
        })
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

    fn setsockopt(&self, level: usize, opt: usize, data: &[u8]) -> SysResult {
        // The two timeouts are kept where the waits look.
        if let Some(kept) = self.shared.inner.lock().patience.set(level, opt, data) {
            return kept;
        }
        if let Some(kept) = self.opts.lock().try_setsockopt(level, opt, data) {
            return kept;
        }
        setsockopt_unstored(self, level, opt, data)
    }

    fn so_reuseaddr(&self) -> bool {
        self.opts.lock().reuse_addr
    }

    fn so_broadcast(&self) -> bool {
        self.opts.lock().broadcast
    }

    fn so_keepalive(&self) -> bool {
        self.opts.lock().keepalive
    }

    fn so_reuseport(&self) -> bool {
        self.opts.lock().reuse_port
    }

    fn so_linger(&self) -> (bool, i32) {
        let opts = self.opts.lock();
        (opts.linger_on, opts.linger_sec)
    }

    fn so_rcvtimeo(&self) -> [u8; 16] {
        self.shared.inner.lock().patience.receive
    }

    fn so_sndtimeo(&self) -> [u8; 16] {
        self.shared.inner.lock().patience.send
    }

    fn ip_tos(&self) -> u32 {
        self.opts.lock().ip_tos
    }

    fn ip_ttl(&self) -> u32 {
        self.opts.lock().ip_ttl
    }

    fn ip_multicast_ttl(&self) -> u32 {
        self.opts.lock().mcast_ttl
    }

    fn ip_multicast_loop(&self) -> bool {
        self.opts.lock().mcast_loop
    }

    fn ip_multicast_if(&self) -> u32 {
        self.opts.lock().mcast_if
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

    /// The wait of a read that blocks (a `splice` from the socket): it
    /// ends with `SO_RCVTIMEO` as the read's own does.
    async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus> {
        let mut until = Until::default();
        loop {
            let status = FileLike::poll(self, events)?;
            let (want_read, want_write) = (events.wants_read(), events.wants_write());
            if (want_read && status.read) || want_write || !want_read {
                return Ok(status);
            }
            self.shared.wait_readable(&mut until).await?;
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

    /// What `ifconfig` and `ip` ask of any socket: the interfaces are
    /// still the kernel's to list, whichever stack the socket is of.
    fn ioctl(&self, request: usize, arg1: usize, arg2: usize, arg3: usize) -> LxResult<usize> {
        super::handle_net_ioctl(request, arg1, arg2, arg3, false)
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
    //! arrived. Each test's stack serves sockets of its own.

    extern crate std;

    use super::*;
    use crate::net::NET_TEST_LOCK as LOCK;

    /// The process's end of the channel.
    pub(super) struct Stack {
        pub(super) link: Arc<StackLink>,
        pub(super) end: Arc<Channel>,
    }

    /// A stack that serves sockets of its own: no test sees another's.
    pub(super) fn stack() -> Stack {
        stack_of(&StackLink::new())
    }

    /// A stack that serves the sockets of `link`, which has none now.
    pub(super) fn stack_of(link: &Arc<StackLink>) -> Stack {
        let (kernel, end) = Channel::create();
        link.attach(kernel, MAX_FOR_TESTS, STACK_ADDR)
            .expect("the stack before is gone");
        Stack {
            link: link.clone(),
            end,
        }
    }

    impl Stack {
        fn socket(&self) -> ServedUdpSocket {
            let s = ServedUdpSocket::new(self.link.clone(), false).unwrap();
            FileLike::set_flags(&s, OpenFlags::NON_BLOCK).unwrap();
            s
        }

        /// The next message the kernel sent, if it sent one.
        pub(super) fn told(&self) -> Option<Vec<u8>> {
            self.end.read().ok().map(|m| m.data)
        }

        /// A datagram arrived for socket `sock`.
        pub(super) fn arrives(&self, sock: u64, from: [u8; 4], port: u16, data: &[u8]) {
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
    pub(super) const MAX_FOR_TESTS: usize = 1472;

    #[test]
    fn a_message_is_its_header_and_what_the_operation_adds() {
        let bind = bind_message(7, 5000);
        assert_eq!(bind.len(), HEADER_LEN + 2);
        assert_eq!(&bind[..4], &[0; 4], "not a reply to any call");
        assert_eq!(parse_header(&bind), Some((OP_BIND, 7)));
        assert_eq!(&bind[16..], &5000u16.to_le_bytes());

        let send = send_message(u64::MAX, [10, 0, 9, 1], 53, 64, b"hola");
        assert_eq!(parse_header(&send), Some((OP_SEND, u64::MAX)));
        assert_eq!(parse_send(&send), Some(([10, 0, 9, 1], 53, &b"hola"[..])));
        // The hop limit and a zero between the port and the datagram.
        assert_eq!(&send[20..24], &[53, 0, 64, 0]);
        // An empty datagram is one.
        let empty = send_message(1, [1, 2, 3, 4], 9, 64, b"");
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

        // What `ping`'s sockets say, written out as the stack reads it.
        assert_eq!(
            hear_message(7),
            [0, 0, 0, 0, 11, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            icmp_message(7, [10, 0, 9, 1], 64, &[8, 0]),
            [0, 0, 0, 0, 12, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 10, 0, 9, 1, 64, 0, 0, 0, 8, 0]
        );
        assert_eq!(HEARD, 0);

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
            send_message(7, [10, 0, 9, 1], 0x1234, 64, b"hi"),
            [
                0, 0, 0, 0, 2, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 10, 0, 9, 1, 0x34, 0x12, 64, 0,
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
    fn a_datagram_for_this_machine_goes_from_socket_to_socket() {
        let _g = LOCK.lock();
        let stack = stack();
        let here = [127, 0, 0, 1];
        let server = stack.socket();
        assert_eq!(Socket::bind(&server, any(5353)), Ok(0));
        let client = stack.socket();
        while stack.told().is_some() {}

        // The sender gets a port, which the stack is told as of any bound
        // socket, and that is all the stack hears of it.
        assert_eq!(Socket::write(&client, b"hola", to(here, 5353)), Ok(4));
        let sent_from = port(&client);
        assert_eq!(stack.told(), Some(bind_message(client.sock, sent_from)));
        assert_eq!(stack.told(), None);
        let mut buf = [0u8; 16];
        assert!(readable(&server));
        assert_eq!(recv(&server, &mut buf), (Ok(4), at(here, sent_from)));
        assert_eq!(&buf[..4], b"hola");
        // And the answer, to a socket connected to who it asked: it hears
        // it, since it comes from there.
        async_std::task::block_on(Socket::connect(&client, to(here, 5353).unwrap())).unwrap();
        assert_eq!(
            Socket::write(&server, b"que tal", to(here, sent_from)),
            Ok(7)
        );
        assert_eq!(recv(&client, &mut buf), (Ok(7), at(here, 5353)));

        // The stack's own address is this machine too, and is where the
        // datagram then comes from.
        assert_eq!(Socket::write(&client, b"yo", to(STACK_ADDR, 5353)), Ok(2));
        assert_eq!(recv(&server, &mut buf), (Ok(2), at(STACK_ADDR, sent_from)));
        // No frame of a card to fit: longer than the stack carries goes.
        let long = alloc::vec![9u8; MAX_FOR_TESTS + 1000];
        assert_eq!(
            Socket::write(&client, &long, to(here, 5353)),
            Ok(long.len())
        );
        let mut big = alloc::vec![0u8; 4096];
        assert_eq!(recv(&server, &mut big).0, Ok(long.len()));
        // A port nobody has: lost, as a datagram is.
        assert_eq!(Socket::write(&client, b"nadie", to(here, 9)), Ok(5));
        assert_eq!(stack.told(), None);
    }

    #[test]
    fn a_boot_that_asks_for_every_program_has_asked_for_each() {
        let _g = LOCK.lock();
        let proc = crate::process::LinuxProcess::new(rcore_fs_ramfs::RamFS::new(), 0);
        assert!(!proc.uses_user_netstack());
        // `unetuse=all`: every program is one that asked.
        use_for_all(true);
        assert!(is_for_all());
        assert!(proc.uses_user_netstack());
        use_for_all(false);
        assert!(!proc.uses_user_netstack());
        // And one that asks still has what it asked for.
        proc.use_user_netstack();
        assert!(proc.uses_user_netstack());
    }

    /// `addr` as an `AF_INET6` socket is given it and gives it.
    pub(super) fn at6(addr: &str, port: u16) -> IpEndpoint {
        let addr: core::net::Ipv6Addr = addr.parse().unwrap();
        IpEndpoint::new(IpAddress::Ipv6(Ipv6Address(addr.octets())), port)
    }

    #[test]
    fn an_ipv6_datagram_socket_carries_ipv4() {
        let _g = LOCK.lock();
        let stack = stack();
        let here = [127, 0, 0, 1];
        // `::` is every address, and the port is the stack's to hear on.
        let server = ServedUdpSocket::new(stack.link.clone(), true).unwrap();
        FileLike::set_flags(&server, OpenFlags::NON_BLOCK).unwrap();
        assert_eq!(Socket::bind(&server, Endpoint::Ip(at6("::", 5353))), Ok(0));
        assert_eq!(stack.told(), Some(bind_message(server.sock, 5353)));
        assert_eq!(ip(Socket::endpoint(&server).unwrap()), at6("::", 5353));
        // It is the IPv6 table's, with the address it has, which is none.
        let listed = stack.link.listed_datagrams();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].v6);
        assert_eq!(listed[0].local, at([0, 0, 0, 0], 5353));

        // What an IPv4 socket sends it comes from an IPv4 address, said
        // the way such a socket says one; and it answers there.
        let client = stack.socket();
        assert_eq!(Socket::write(&client, b"hola", to(here, 5353)), Ok(4));
        let sent_from = port(&client);
        let mut buf = [0u8; 16];
        let from = at6("::ffff:127.0.0.1", sent_from);
        assert_eq!(recv(&server, &mut buf), (Ok(4), from));
        assert_eq!(
            Socket::write(&server, b"que tal", Some(Endpoint::Ip(from))),
            Ok(7)
        );
        assert_eq!(recv(&client, &mut buf), (Ok(7), at(here, 5353)));
        // `::1` is this machine as well.
        assert_eq!(
            Socket::write(&server, b"yo", Some(Endpoint::Ip(at6("::1", sent_from)))),
            Ok(2)
        );
        assert_eq!(recv(&client, &mut buf), (Ok(2), at(here, 5353)));
        while stack.told().is_some() {}

        // An IPv4 address elsewhere goes to the stack as what it is, and
        // what comes from there is said as it was written.
        let there = Endpoint::Ip(at6("::ffff:10.0.9.1", 53));
        assert_eq!(Socket::write(&server, b"q", Some(there.clone())), Ok(1));
        assert_eq!(
            stack.told(),
            Some(send_message(server.sock, HOST, 53, 64, b"q"))
        );
        stack.arrives(server.sock, HOST, 53, b"r");
        stack.link.pump();
        assert_eq!(recv(&server, &mut buf), (Ok(1), at6("::ffff:10.0.9.1", 53)));
        async_std::task::block_on(Socket::connect(&server, there)).unwrap();
        assert_eq!(
            ip(Socket::remote_endpoint(&server).unwrap()),
            at6("::ffff:10.0.9.1", 53)
        );

        // The stack has no IPv6: an address that is only that is nowhere
        // it reaches and none it has. And an IPv4 address is not one an
        // `AF_INET6` socket is given.
        let other = ServedUdpSocket::new(stack.link.clone(), true).unwrap();
        let v6 = Endpoint::Ip(at6("2001:db8::1", 53));
        assert_eq!(
            Socket::write(&other, b"q", Some(v6.clone())),
            Err(LxError::ENETUNREACH)
        );
        assert_eq!(
            async_std::task::block_on(Socket::connect(&other, v6.clone())),
            Err(LxError::ENETUNREACH)
        );
        assert_eq!(Socket::bind(&other, v6), Err(LxError::EADDRNOTAVAIL));
        assert_eq!(
            Socket::bind(&other, Endpoint::Ip(at(here, 5354))),
            Err(LxError::EAFNOSUPPORT)
        );
        // Bound to `::1` it is this machine's alone, like one bound to
        // 127.0.0.1.
        while stack.told().is_some() {}
        assert_eq!(Socket::bind(&other, Endpoint::Ip(at6("::1", 5354))), Ok(0));
        assert_eq!(stack.told(), None);
        assert_eq!(
            ip(Socket::endpoint(&other).unwrap()),
            at6("::ffff:127.0.0.1", 5354)
        );
    }

    #[test]
    fn a_datagram_socket_bound_to_this_machine_is_not_the_stacks() {
        let _g = LOCK.lock();
        let stack = stack();
        let here = [127, 0, 0, 1];
        let server = stack.socket();
        assert_eq!(
            Socket::bind(&server, Endpoint::Ip(at([127, 0, 0, 53], 53))),
            Ok(0)
        );
        // The stack would bring it what the card hears for the port.
        assert_eq!(stack.told(), None);
        assert_eq!(
            ip(Socket::endpoint(&server).unwrap()),
            at([127, 0, 0, 53], 53)
        );

        let client = stack.socket();
        assert_eq!(Socket::write(&client, b"a", to([127, 0, 0, 53], 53)), Ok(1));
        // For another address of this machine's it is not there.
        assert_eq!(Socket::write(&client, b"b", to(here, 53)), Ok(1));
        assert_eq!(Socket::write(&client, b"c", to(STACK_ADDR, 53)), Ok(1));
        let mut buf = [0u8; 16];
        let (n, from) = recv(&server, &mut buf);
        assert_eq!((n, &buf[..1]), (Ok(1), &b"a"[..]));
        assert_eq!(recv(&server, &mut buf).0, Err(LxError::EAGAIN));
        // It answers from the address it has, and to this machine only.
        assert_eq!(
            Socket::write(&server, b"r", Some(Endpoint::Ip(from))),
            Ok(1)
        );
        assert_eq!(recv(&client, &mut buf), (Ok(1), at([127, 0, 0, 53], 53)));
        assert_eq!(
            Socket::write(&server, b"x", to(HOST, 53)),
            Err(LxError::EINVAL)
        );

        // The stack heard of the client's port and nothing else, and does
        // not hear of the server's close.
        while let Some(m) = stack.told() {
            assert_eq!(m, bind_message(client.sock, port(&client)));
        }
        drop(server);
        assert_eq!(stack.told(), None);
        let rebound = stack.socket();
        assert_eq!(Socket::bind(&rebound, any(53)), Ok(0));
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
        assert_eq!(
            stack.told(),
            Some(send_message(a.sock, HOST, 9, 64, b"uno"))
        );
        // The second send has the port already; another socket gets another.
        assert_eq!(Socket::write(&a, b"dos", to(HOST, 9)), Ok(3));
        assert_eq!(
            stack.told(),
            Some(send_message(a.sock, HOST, 9, 64, b"dos"))
        );
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
        stack.say(send_message(a.sock, HOST, 1, 64, b"not DATA"));
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
        assert!(!stack.link.is_down(), "nonsense does not end the stack");
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
        assert_eq!(
            stack.told(),
            Some(send_message(a.sock, HOST, 7, 64, b"ping"))
        );

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
        let (channel, _) = stack.link.current().unwrap();
        assert_eq!(channel.peer_pending(), SEND_BACKLOG + 1);
        // Once the stack has read enough, a send goes through again.
        assert!(stack.told().is_some());
        assert_eq!(Socket::write(&a, b"x", to(HOST, 9)), Err(LxError::ENOBUFS));
        assert!(stack.told().is_some());
        assert_eq!(Socket::write(&a, b"x", to(HOST, 9)), Ok(1));
        let mut last = None;
        while let Some(m) = stack.told() {
            last = Some(m);
        }
        assert_eq!(last, Some(send_message(a.sock, HOST, 9, 64, b"x")));
    }

    #[test]
    fn when_the_stack_dies_its_sockets_wait_for_the_next() {
        let _g = LOCK.lock();
        let stack = stack();
        let here = [127, 0, 0, 1];
        let a = stack.socket();
        assert_eq!(Socket::bind(&a, any(7000)), Ok(0));
        let local = stack.socket();
        assert_eq!(Socket::bind(&local, Endpoint::Ip(at(here, 7001))), Ok(0));
        stack.arrives(a.sock, HOST, 1, b"last");
        // One stack at a time.
        let (other, _other_end) = Channel::create();
        assert_eq!(
            stack.link.attach(other, MAX_FOR_TESTS, STACK_ADDR).err(),
            Some(ZxError::ALREADY_BOUND)
        );
        // And one that carries nothing, or more than UDP does, is none.
        for bad in [0, MAX_DATAGRAM + 1] {
            let (other, _other_end) = Channel::create();
            assert_eq!(
                stack.link.attach(other, bad, STACK_ADDR).err(),
                Some(ZxError::INVALID_ARGS)
            );
        }
        assert!(!stack.link.is_down());

        let Stack { link, end } = stack;
        drop(end);
        link.pump();
        assert!(link.is_down());

        // What had arrived is read, and then there is nothing to read:
        // the socket is not in error, it waits. Nothing leaves the
        // machine, and that is said to who tries.
        let mut buf = [0u8; 8];
        assert_eq!(recv(&a, &mut buf).0, Ok(4));
        assert_eq!(recv(&a, &mut buf).0, Err(LxError::EAGAIN));
        assert_eq!(Socket::poll(&a, PollEvents::IN), (false, true, false));
        assert_eq!(Socket::write(&a, b"x", to(HOST, 9)), Err(LxError::ENETDOWN));
        // What stays in the machine does not notice.
        assert_eq!(Socket::write(&a, b"aqui", to(here, 7001)), Ok(4));
        assert_eq!(recv(&local, &mut buf), (Ok(4), at(here, 7000)));
        // And a socket is made, and takes a port, with no stack at all.
        let b = ServedUdpSocket::new(link.clone(), false).unwrap();
        FileLike::set_flags(&b, OpenFlags::NON_BLOCK).unwrap();
        assert_eq!(Socket::bind(&b, any(7002)), Ok(0));
        assert_eq!(
            Socket::bind(&stack_socket(&link), any(7002)),
            Err(LxError::EADDRINUSE)
        );

        // The next stack is told of the sockets that have a port, less
        // the one that is this machine's alone, and they are its: what
        // it hears for them arrives, and it is told when one closes.
        let next = stack_of(&link);
        assert_eq!(next.told(), Some(bind_message(a.sock, 7000)));
        assert_eq!(next.told(), Some(bind_message(b.sock, 7002)));
        assert_eq!(next.told(), None);
        next.arrives(a.sock, HOST, 1, b"again");
        link.pump();
        assert_eq!(recv(&a, &mut buf), (Ok(5), at(HOST, 1)));
        assert_eq!(Socket::write(&b, b"x", to(HOST, 9)), Ok(1));
        assert_eq!(next.told(), Some(send_message(b.sock, HOST, 9, 64, b"x")));
        let closed = a.sock;
        drop(a);
        assert_eq!(next.told(), Some(close_message(closed)));
    }

    #[test]
    fn a_datagram_read_waits_as_long_as_so_rcvtimeo_says() {
        let _lock = LOCK.lock();
        let stack = stack();
        let s = stack.socket();
        assert_eq!(Socket::bind(&s, any(5300)), Ok(0));
        FileLike::set_flags(&s, OpenFlags::empty()).unwrap();

        let mut tv = [0u8; 16];
        tv[8..].copy_from_slice(&30_000i64.to_ne_bytes());
        assert_eq!(Socket::so_rcvtimeo(&s), [0u8; 16]);
        assert_eq!(Socket::setsockopt(&s, 1, 20, &tv), Ok(0));
        assert_eq!(Socket::so_rcvtimeo(&s), tv);
        // SO_SNDTIMEO is kept too, and waits for nothing: a datagram that
        // cannot be sent is refused.
        assert_eq!(Socket::setsockopt(&s, 1, 21, &tv), Ok(0));
        assert_eq!(Socket::so_sndtimeo(&s), tv);
        let mut wrong = tv;
        wrong[8..].copy_from_slice(&(-1i64).to_ne_bytes());
        assert_eq!(Socket::setsockopt(&s, 1, 20, &wrong), Err(LxError::EINVAL));
        // What it keeps nowhere is answered as before.
        assert_eq!(Socket::setsockopt(&s, 1, 6, &1u32.to_ne_bytes()), Ok(0));
        assert_eq!(
            Socket::setsockopt(&s, 6, 1, &1u32.to_ne_bytes()),
            Err(LxError::ENOPROTOOPT)
        );

        // Nothing comes: the read gives up when the time is over and not
        // before, and the socket is as it was.
        let mut buf = [0u8; 16];
        let before = kernel_hal::timer::timer_now();
        assert_eq!(recv(&s, &mut buf).0, Err(LxError::EAGAIN));
        let took = kernel_hal::timer::timer_now() - before;
        assert!(took >= core::time::Duration::from_millis(30), "{:?}", took);
        stack.arrives(s.sock, HOST, 53, b"tarde");
        assert_eq!(recv(&s, &mut buf), (Ok(5), at(HOST, 53)));
    }

    #[test]
    fn a_question_of_the_kernels_goes_by_the_stack_and_waits_for_its_answer() {
        let _g = LOCK.lock();
        let stack = stack();
        let patience = core::time::Duration::from_secs(5);
        let waiter = {
            let link = stack.link.clone();
            std::thread::spawn(move || {
                async_std::task::block_on(async {
                    let asked = ask(&link, at(HOST, 53), b"who?", patience).await?;
                    let first = asked.answer().await?;
                    let second = asked.answer().await?;
                    Ok::<_, LxError>((first, second))
                })
            })
        };
        // It leaves as any program's datagram does: a socket with a port
        // of its own, then the datagram.
        let sock = loop {
            match stack.told() {
                Some(m) if parse_header(&m).map(|h| h.0) == Some(OP_SEND) => {
                    assert_eq!(parse_send(&m), Some((HOST, 53, &b"who?"[..])));
                    break parse_header(&m).unwrap().1;
                }
                _ => std::thread::yield_now(),
            }
        };
        // What somebody else sends to that port is no answer.
        stack.arrives(sock, [10, 0, 9, 77], 53, b"not who was asked");
        stack.arrives(sock, HOST, 53, b"one");
        stack.arrives(sock, HOST, 53, b"two");
        assert_eq!(
            waiter.join().unwrap(),
            Ok((b"one".to_vec(), b"two".to_vec()))
        );
        // And the socket goes with the question.
        assert_eq!(stack.told(), Some(close_message(sock)));

        // No answer is `ETIMEDOUT` when the patience runs out, and from
        // then on.
        let short = core::time::Duration::from_millis(30);
        let asked =
            async_std::task::block_on(ask(&stack.link, at(HOST, 53), b"again", short)).unwrap();
        let before = kernel_hal::timer::timer_now();
        assert_eq!(
            async_std::task::block_on(asked.answer()),
            Err(LxError::ETIMEDOUT)
        );
        assert!(kernel_hal::timer::timer_now() - before >= short);
        assert_eq!(
            async_std::task::block_on(asked.answer()),
            Err(LxError::ETIMEDOUT)
        );

        // What cannot be sent is said when it is asked: a server the
        // stack has no way to (IPv6), and no stack at all.
        let six = IpEndpoint::new(IpAddress::v6(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1), 53);
        let refused = async_std::task::block_on(ask(&stack.link, six, b"x", short)).err();
        assert_eq!(refused, Some(LxError::EAFNOSUPPORT));
        let nobody = StackLink::new();
        let refused = async_std::task::block_on(ask(&nobody, at(HOST, 53), b"x", short)).err();
        assert_eq!(refused, Some(LxError::ENETDOWN));
    }

    #[test]
    fn a_datagram_travels_as_many_hops_as_its_socket_was_told() {
        let _g = LOCK.lock();
        let stack = stack();
        let s = stack_socket(&stack.link);
        const IPPROTO_IP: usize = 0;
        const IP_TTL: usize = 2;
        const IP_MULTICAST_TTL: usize = 33;
        let group = [224, 0, 0, 251];
        let sent = |whom: [u8; 4], data: &[u8]| {
            assert_eq!(Socket::write(&s, data, to(whom, 9)), Ok(data.len()));
            loop {
                let m = stack.told().unwrap();
                if parse_header(&m).map(|h| h.0) == Some(OP_SEND) {
                    break m;
                }
            }
        };
        // As Linux has them before anybody says: 64, and 1 to a group.
        assert_eq!(Socket::ip_ttl(&s), 64);
        assert_eq!(sent(HOST, b"a"), send_message(s.sock, HOST, 9, 64, b"a"));
        assert_eq!(sent(group, b"b"), send_message(s.sock, group, 9, 1, b"b"));

        // What `traceroute` does: one hop, then two.
        assert_eq!(
            Socket::setsockopt(&s, IPPROTO_IP, IP_TTL, &1u32.to_ne_bytes()),
            Ok(0)
        );
        assert_eq!(Socket::ip_ttl(&s), 1);
        assert_eq!(sent(HOST, b"c"), send_message(s.sock, HOST, 9, 1, b"c"));
        assert_eq!(
            Socket::setsockopt(&s, IPPROTO_IP, IP_TTL, &2u32.to_ne_bytes()),
            Ok(0)
        );
        assert_eq!(sent(HOST, b"d"), send_message(s.sock, HOST, 9, 2, b"d"));
        // One byte is an option too, and 255 the most.
        assert_eq!(Socket::setsockopt(&s, IPPROTO_IP, IP_TTL, &[255]), Ok(0));
        assert_eq!(sent(HOST, b"e"), send_message(s.sock, HOST, 9, 255, b"e"));

        // A group's datagrams have their own, and the others keep theirs.
        let ttl = 5u32.to_ne_bytes();
        assert_eq!(
            Socket::setsockopt(&s, IPPROTO_IP, IP_MULTICAST_TTL, &ttl),
            Ok(0)
        );
        assert_eq!(Socket::ip_multicast_ttl(&s), 5);
        assert_eq!(sent(group, b"f"), send_message(s.sock, group, 9, 5, b"f"));
        assert_eq!(sent(HOST, b"g"), send_message(s.sock, HOST, 9, 255, b"g"));

        // The other options a program sets are the ones it reads back.
        const SOL_SOCKET: usize = 1;
        const SO_BROADCAST: usize = 6;
        assert!(!Socket::so_broadcast(&s));
        assert_eq!(
            Socket::setsockopt(&s, SOL_SOCKET, SO_BROADCAST, &1u32.to_ne_bytes()),
            Ok(0)
        );
        assert!(Socket::so_broadcast(&s));
    }

    /// A socket of `link` that does not wait.
    #[test]
    fn the_datagram_sockets_that_have_a_port_are_listed() {
        let _g = LOCK.lock();
        let stack = stack();
        // No port, not in the table; nor a socket that pings, which has
        // an identifier and no port of UDP's.
        let fresh = stack.socket();
        let pings = ServedIcmpSocket::new(stack.link.clone(), false).unwrap();
        assert_eq!(stack.link.listed_datagrams(), []);

        let a = stack.socket();
        assert_eq!(Socket::bind(&a, any(7000)), Ok(0));
        let local = stack.socket();
        assert_eq!(
            Socket::bind(&local, Endpoint::Ip(at([127, 0, 0, 1], 7001))),
            Ok(0)
        );
        let row = |local, remote: Option<IpEndpoint>, unread| Listed {
            local,
            remote,
            state: if remote.is_some() {
                TCP_ESTABLISHED
            } else {
                TCP_CLOSE
            },
            unread,
            v6: false,
        };
        let here = row(at([127, 0, 0, 1], 7001), None, 0);
        assert_eq!(
            stack.link.listed_datagrams(),
            [row(at([0, 0, 0, 0], 7000), None, 0), here]
        );
        // What arrived and nobody read is counted, and a peer is shown.
        stack.arrives(a.sock, HOST, 53, b"hello");
        stack.arrives(a.sock, HOST, 53, b"you");
        assert_eq!(
            stack.link.listed_datagrams(),
            [row(at([0, 0, 0, 0], 7000), None, 8), here]
        );
        let mut buf = [0u8; 8];
        assert_eq!(recv(&a, &mut buf).0, Ok(5));
        let connected = async_std::task::block_on(Socket::connect(&a, Endpoint::Ip(at(HOST, 53))));
        assert_eq!(connected, Ok(0));
        assert_eq!(
            stack.link.listed_datagrams(),
            [row(at([0, 0, 0, 0], 7000), Some(at(HOST, 53)), 3), here]
        );
        // Closed, it is gone from the table; and the table outlives the
        // stack, as the sockets do.
        drop(a);
        assert_eq!(stack.link.listed_datagrams(), [here]);
        let Stack { link, end } = stack;
        drop(end);
        link.pump();
        assert_eq!(link.listed_datagrams(), [here]);
        drop((fresh, pings, local));
        assert_eq!(link.listed_datagrams(), []);
    }

    #[test]
    fn what_the_card_has_carried_is_what_its_stacks_said_one_after_another() {
        let _g = LOCK.lock();
        let stack = stack();
        let carried = stack.link.carried();
        let shown = |carried: &Carried| {
            let c = carried.so_far();
            (c.rx_bytes, c.rx_packets, c.tx_bytes, c.tx_packets)
        };
        assert_eq!(shown(&carried), (0, 0, 0, 0));

        // The message, as the stack writes it.
        let said = counts_message(&[600, 4, 1, 2, 420, 3, 5, 6]);
        assert_eq!(said.len(), HEADER_LEN + COUNTS * 8);
        assert_eq!(parse_header(&said), Some((OP_COUNTS, 0)));
        assert_eq!(&said[16..24], &600u64.to_le_bytes());
        assert_eq!(&said[48..56], &420u64.to_le_bytes());
        stack.say(said);
        let c = carried.so_far();
        assert_eq!((c.rx_bytes, c.rx_packets), (600, 4));
        assert_eq!((c.rx_errors, c.rx_dropped), (1, 2));
        assert_eq!((c.tx_bytes, c.tx_packets), (420, 3));
        assert_eq!((c.tx_errors, c.tx_dropped), (5, 6));

        // Said again, nothing more; said since the start, so what is new
        // is the difference; and a number that goes back adds nothing.
        stack.say(counts_message(&[600, 4, 1, 2, 420, 3, 5, 6]));
        assert_eq!(shown(&carried), (600, 4, 420, 3));
        stack.say(counts_message(&[700, 5, 1, 2, 420, 3, 5, 6]));
        assert_eq!(shown(&carried), (700, 5, 420, 3));
        stack.say(counts_message(&[100, 1, 0, 0, 500, 4, 5, 6]));
        assert_eq!(shown(&carried), (700, 5, 500, 4));
        // What a later stack adds after the eight is not read, and fewer
        // than eight are no counts.
        let mut longer = counts_message(&[160, 2, 0, 0, 500, 4, 5, 6]);
        longer.extend_from_slice(&[0xff; 8]);
        stack.say(longer);
        assert_eq!(shown(&carried), (760, 6, 500, 4));
        let mut short = counts_message(&[9000, 9000, 0, 0, 9000, 9000, 0, 0]);
        short.truncate(HEADER_LEN + COUNTS * 8 - 1);
        stack.say(short);
        assert_eq!(shown(&carried), (760, 6, 500, 4));

        // The stack dies. The interface has carried what it had, and the
        // next stack counts from nothing: what it says is all new.
        let Stack { link, end } = stack;
        drop(end);
        link.pump();
        assert!(link.is_down());
        assert_eq!(shown(&carried), (760, 6, 500, 4));
        let next = stack_of(&link);
        next.say(counts_message(&[40, 1, 0, 0, 50, 2, 0, 0]));
        assert_eq!(shown(&link.carried()), (800, 7, 550, 6));
        assert!(Arc::ptr_eq(&carried, &link.carried()));
    }

    fn stack_socket(link: &Arc<StackLink>) -> ServedUdpSocket {
        let s = ServedUdpSocket::new(link.clone(), false).unwrap();
        FileLike::set_flags(&s, OpenFlags::NON_BLOCK).unwrap();
        s
    }

    #[test]
    fn a_stack_found_dead_by_a_write_is_gone() {
        let _g = LOCK.lock();
        let Stack { link, end } = stack();
        let a = stack_socket(&link);
        drop(end);
        assert_eq!(Socket::write(&a, b"x", to(HOST, 9)), Err(LxError::ENETDOWN));
        assert!(link.is_down());
        // And the next one finds it out by itself when nobody pumped.
        let Stack { link, end } = stack();
        drop(end);
        assert!(!link.is_down());
        let _next = stack_of(&link);
        assert!(!link.is_down());
    }

    #[test]
    fn the_machine_has_sockets_before_it_has_a_stack() {
        let _g = LOCK.lock();
        let link = StackLink::new();
        assert!(link.is_down());
        let here = [127, 0, 0, 1];
        // Two programs talk with nobody serving anything.
        let server = stack_socket(&link);
        assert_eq!(Socket::bind(&server, any(5353)), Ok(0));
        let client = stack_socket(&link);
        assert_eq!(Socket::write(&client, b"hola", to(here, 5353)), Ok(4));
        let mut buf = [0u8; 8];
        assert_eq!(recv(&server, &mut buf).0, Ok(4));
        // Outside there is no way yet.
        assert_eq!(
            Socket::write(&client, b"x", to(HOST, 9)),
            Err(LxError::ENETDOWN)
        );
        // The stack comes and is told of both, each with its port.
        let stack = stack_of(&link);
        assert_eq!(stack.told(), Some(bind_message(server.sock, 5353)));
        assert_eq!(stack.told(), Some(bind_message(client.sock, port(&client))));
        assert_eq!(Socket::write(&client, b"x", to(HOST, 9)), Ok(1));
        assert_eq!(
            stack.told(),
            Some(send_message(client.sock, HOST, 9, 64, b"x"))
        );
    }
}
