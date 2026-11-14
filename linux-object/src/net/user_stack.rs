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
//!     FAILED     number: why (refused, timed out, reset, unreachable, no room,
//!                unreachable as IPv6)
//!                bytes: none, or u8 type, u8 code of the ICMP message that
//!                said the peer is unreachable (of the ICMPv6 message, for
//!                the last)
//!     ACCEPTED   socket: the listener.  number: the bytes the stack takes.
//!                bytes: u64 the new socket, [u8; 4] from, u16 port, u16 zero
//!     BYTES      bytes: what arrived
//!     EOF        the peer will send no more
//!     CREDIT     number: more bytes the stack takes
//!     UNDELIVERED  socket: a datagram socket's.
//!                bytes: [u8; 4] to, u16 port, u8 type, u8 code
//!     UNDELIVERED6 socket: a datagram socket's.
//!                bytes: [u8; 16] to, u16 port, u8 type, u8 code (ICMPv6's)
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
//! # What was not delivered
//!
//! A router that has no way on, or a host with nobody at the port, says
//! so in an ICMP message about what it could not deliver. The stack
//! reads those, heard or not, and says whose it was: `UNDELIVERED` for a
//! datagram, by the port it left from, and `FAILED` with the message's
//! type and code for a connect under way, which ends there. What the
//! program is told is decided here, and it is what Linux tells it
//! ([`icmp::undelivered_error`]):
//!
//! - a connect fails with the error the message means: `EHOSTUNREACH`
//!   for a host, `ENETUNREACH` for a network, `ECONNREFUSED` for a port.
//! - a datagram socket hears of it only when it is connected to where
//!   the datagram went, and only of what does not mend by itself (nobody
//!   at the port: `ECONNREFUSED`). Its next read or write fails with the
//!   error, once; `SO_ERROR` takes it too, and `poll` says there is
//!   one (`POLLERR`, with nothing to read). A socket
//!   that is not connected is told nothing, as on Linux without
//!   `IP_RECVERR`, which is not kept here.
//!
//! A datagram for a port of this machine that no socket has is refused
//! the same way, by the kernel itself.
//!
//! What travelled as IPv6 has ICMPv6 say it, with its own types and
//! codes, and the stack passes those on as they are: `UNDELIVERED6` for
//! a datagram and `FAILED` with "unreachable as IPv6" for a connect. The
//! errors are Linux's for them ([`icmp6::undelivered_error`]): nobody
//! answers at the address is `EHOSTUNREACH`, which is also what the
//! stack says to itself of a neighbour that does not answer; nobody at
//! the port, `ECONNREFUSED`; forbidden on the way, `EACCES`.
//!
//! # Groups
//!
//! A program that asks to hear a group (`IP_ADD_MEMBERSHIP`) makes the
//! machine a member of it, and the stack is told of the machine, not of
//! the socket (`groups`):
//!
//! ```text
//! JOIN    (kernel to stack)  16  [u8; 4] group
//! LEAVE   (kernel to stack)  16  [u8; 4] group
//! ```
//!
//! What arrives for the group arrives for the socket that has the port,
//! and what a socket here sends to it reaches the member in this machine
//! as well as the card.
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
//! # The card's IPv6 addresses
//!
//! The stack makes them, from its card and from what the link's router
//! says, and they come and go while it runs. The machine's interface
//! shows them (`ifconfig`, `ip -6`, `/proc/net/if_inet6`), so the stack
//! says all of them each time one changes:
//!
//! ```text
//! SIX     (stack to kernel)  16  [u8; 16] the router, or zeroes  32  one address after another, to the end:
//!             0  [u8; 16] address  16  u8 the bits of its network's prefix  17  seven zero
//! ```
//!
//! What is kept here ([`StackLink::sixes`]) is the last of them, and
//! nothing once the stack is gone.
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
//! A program does not ask what the stack speaks before it opens its
//! socket: plenty of them listen on an `AF_INET6` socket at `::` and
//! expect IPv4 to arrive there too. So an `AF_INET6` socket is served
//! like any other, and carries IPv4: `::` is every address,
//! `::ffff:a.b.c.d` is `a.b.c.d`, and `::1` is this machine, which no
//! version of IP has a part in. Such a socket is told its IPv4 addresses
//! the same way, as `::ffff:a.b.c.d`.
//!
//! A datagram socket of that family carries IPv6 as well. What it sends
//! to any other IPv6 address travels as IPv6, and what arrives as IPv6
//! for its port is its own, with the longer address:
//!
//! ```text
//! SEND6   (kernel to stack)  16  [u8; 16] to   32  u16 port  34  u8 hop limit  35  u8 zero  36  the datagram
//! DATA6   (stack to kernel)  16  one record after another, to the end:
//!             0  u64 socket  8  [u8; 16] from  24  u16 port  26  u16 length  28  the datagram
//! ```
//!
//! The port is one for both: an `AF_INET` socket that has it is not
//! given what arrives as IPv6. Where the card has no way to
//! ([`Sixes::reaches`]) is `ENETUNREACH`, and an address of the card's
//! own is this machine, like `::1`. How many hops such a datagram may
//! travel is the socket's `IPV6_UNICAST_HOPS` (`IPV6_MULTICAST_HOPS` to
//! a group), as `IP_TTL` is for what travels as IPv4: `traceroute -6`
//! sends one that may travel one, then two.
//!
//! What a socket of that family is asked about its options of IPv6's
//! level it answers itself ([`Socket::getsockopt_own`]): the hops, and
//! that it is not for IPv6 only.
//!
//! So does a stream socket of that family. It connects to any other
//! IPv6 address with `CONNECT6`, and is told with `CONNECTED` which of
//! the card's addresses it connected from, which the stack chooses. It
//! listens with `LISTEN6`, for what arrives as either; an `AF_INET`
//! socket listens with `LISTEN`, and who knocks at its port as IPv6 is
//! refused. A connection that arrives as IPv6 is an `ACCEPTED6`:
//!
//! ```text
//! CONNECT6 (kernel to stack)  16  [u8; 16] to  32  u16 port  34  u16 local port  36  u32 window
//! LISTEN6  (kernel to stack)  as LISTEN
//!     CONNECTED  bytes: none, or [u8; 16] the address it connected from
//!     ACCEPTED6  as ACCEPTED.  bytes: u64 the new socket, [u8; 16] from,
//!                u16 port, u16 zero, [u8; 16] the address it arrived at
//! ```
//!
//! And so do the sockets that speak ICMPv6, what `ping -6` opens
//! ([`icmp6`]):
//!
//! ```text
//! ICMP6   (kernel to stack)  16  [u8; 16] to  32  u8 hop limit  33  three zero  36  the ICMPv6 message
//! HEAR6   (kernel to stack)
//! ```
//!
//! As `ICMP` and `HEAR`, with two differences. The stack chooses the
//! address the message is from, and ICMPv6's sum covers it: so the sum
//! is the stack's to write, and the kernel leaves its place alone. And
//! what such a socket hears comes as records of a `DATA6` for socket 0,
//! each a whole IPv6 packet of ICMPv6 whose sum the stack saw is right.

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
use kernel_hal::drivers::net::stack_card::{Carried, Sixes};
use kernel_hal::drivers::scheme::NetStats;
use kernel_hal::sync::Mutex;
use log::warn;
use smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address, Ipv6Address, Ipv6Cidr};
use zircon_object::impl_kobject;
use zircon_object::ipc::{Channel, MessagePacket};
use zircon_object::object::*;
use zircon_object::{ZxError, ZxResult};

mod groups;
mod icmp;
mod icmp6;
mod tcp;
pub use groups::{join_message, leave_message, Membership, MEMBERSHIPS_MAX, OP_JOIN, OP_LEAVE};
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
pub const OP_SIX: u32 = 16;
pub const OP_SEND6: u32 = 17;
pub const OP_DATA6: u32 = 18;
pub const OP_CONNECT6: u32 = 19;
pub const OP_LISTEN6: u32 = 20;
pub const OP_ICMP6: u32 = 21;
pub const OP_HEAR6: u32 = 22;

/// The socket of a `DATA` record that is an IPv4 packet of ICMP, or of a
/// `DATA6` record that is an IPv6 packet of ICMPv6, for whichever sockets
/// hear: no socket has the number.
pub const HEARD: u64 = 0;

/// What a record of an `EVENTS` says.
pub const EV_CONNECTED: u16 = 1;
pub const EV_FAILED: u16 = 2;
pub const EV_ACCEPTED: u16 = 3;
pub const EV_BYTES: u16 = 4;
pub const EV_EOF: u16 = 5;
pub const EV_CREDIT: u16 = 6;
pub const EV_UNDELIVERED: u16 = 7;
pub const EV_ACCEPTED6: u16 = 8;
pub const EV_UNDELIVERED6: u16 = 9;

/// Why a connection `FAILED`.
pub const WHY_REFUSED: u32 = 1;
pub const WHY_TIMED_OUT: u32 = 2;
pub const WHY_RESET: u32 = 3;
pub const WHY_UNREACHABLE: u32 = 4;
pub const WHY_NO_ROOM: u32 = 5;
pub const WHY_UNREACHABLE6: u32 = 6;

/// Set in the number of a socket the stack numbered: a connection that
/// arrived on a listener.
pub const ACCEPTED_BIT: u64 = 1 << 63;
/// The bytes of an `ACCEPTED` record.
pub const ACCEPTED_LEN: usize = 16;
/// The bytes of an `ACCEPTED6` record.
pub const ACCEPTED6_LEN: usize = 44;
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
/// The bytes of an `UNDELIVERED` record.
pub const UNDELIVERED_LEN: usize = 8;
/// The bytes of an `UNDELIVERED6` record.
pub const UNDELIVERED6_LEN: usize = 20;
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

/// `SEND6`: `data` from socket `sock` to `addr:port` as IPv6, to travel
/// `hop_limit` hops at most (0: as many as the stack gives one).
pub fn send6_message(sock: u64, addr: [u8; 16], port: u16, hop_limit: u8, data: &[u8]) -> Vec<u8> {
    let mut m = header(OP_SEND6, sock, 20 + data.len());
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

/// `HEAR6`: socket `sock` hears ICMPv6 from now on.
pub fn hear6_message(sock: u64) -> Vec<u8> {
    header(OP_HEAR6, sock, 0)
}

/// `ICMP6`: `message` from socket `sock` to `addr`, to travel `hop_limit`
/// hops at most, which is not 0. Its sum is the stack's to write.
pub fn icmp6_message(sock: u64, addr: [u8; 16], hop_limit: u8, message: &[u8]) -> Vec<u8> {
    let mut m = header(OP_ICMP6, sock, 20 + message.len());
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

/// What each address of a `SIX` takes, and the most of them that are
/// taken: more than a card has.
const SIX_ADDRESS_LEN: usize = 24;
const SIX_ADDRESSES_MAX: usize = 16;

/// `SIX`: the card has `addresses`, and `router` is its link's.
pub fn six_message(router: Option<Ipv6Address>, addresses: &[Ipv6Cidr]) -> Vec<u8> {
    let mut m = header(OP_SIX, 0, 16 + addresses.len() * SIX_ADDRESS_LEN);
    m.extend_from_slice(&router.unwrap_or(Ipv6Address::UNSPECIFIED).0);
    for address in addresses {
        m.extend_from_slice(&address.address().0);
        m.extend_from_slice(&[address.prefix_len(), 0, 0, 0, 0, 0, 0, 0]);
    }
    m
}

/// What a `SIX` says, past its header: `None` when it is not whole, or
/// names what no card has -- an address that is a group's or none, a
/// prefix of more bits than an address.
pub fn parse_six(said: &[u8]) -> Option<(Option<Ipv6Address>, Vec<Ipv6Cidr>)> {
    let router = Ipv6Address::from_bytes(said.get(..16)?);
    let (records, rest) = said[16..].as_chunks::<SIX_ADDRESS_LEN>();
    if !rest.is_empty() || records.len() > SIX_ADDRESSES_MAX {
        return None;
    }
    let mut addresses = Vec::new();
    for record in records {
        let address = Ipv6Address::from_bytes(&record[..16]);
        let prefix = record[16];
        if !address.is_unicast() || prefix > 128 {
            return None;
        }
        addresses.push(Ipv6Cidr::new(address, prefix));
    }
    let router = (!router.is_unspecified()).then_some(router);
    if router.is_some_and(|router| !router.is_unicast()) {
        return None;
    }
    Some((router, addresses))
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

/// What each record of a `DATA6` starts with.
pub const RECORD6_LEN: usize = 28;

/// One record of a `DATA6`: the socket, who sent the datagram, and it.
pub type Record6<'a> = (u64, [u8; 16], u16, &'a [u8]);

/// The first record of `records`, the rest of a `DATA6` message, and
/// what follows it. `None` when what is there is not a whole record.
pub fn parse_record6(records: &[u8]) -> Option<(Record6<'_>, &[u8])> {
    if records.len() < RECORD6_LEN {
        return None;
    }
    let sock = u64::from_le_bytes(records[..8].try_into().ok()?);
    let addr: [u8; 16] = records[8..24].try_into().ok()?;
    let port = u16::from_le_bytes(records[24..26].try_into().ok()?);
    let len = u16::from_le_bytes(records[26..28].try_into().ok()?) as usize;
    let rest = &records[RECORD6_LEN..];
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

/// `CONNECT6`: as [`connect_message`], to an IPv6 address.
pub fn connect6_message(sock: u64, addr: [u8; 16], port: u16, local: u16, window: u32) -> Vec<u8> {
    let mut m = header(OP_CONNECT6, sock, 24);
    m.extend_from_slice(&addr);
    m.extend_from_slice(&port.to_le_bytes());
    m.extend_from_slice(&local.to_le_bytes());
    m.extend_from_slice(&window.to_le_bytes());
    m
}

/// `LISTEN`: socket `sock` takes connections to `port`, `backlog` of them
/// ready at a time, and `window` bytes of what arrives on each.
pub fn listen_message(sock: u64, port: u16, backlog: u16, window: u32) -> Vec<u8> {
    listen_of(OP_LISTEN, sock, port, backlog, window)
}

/// `LISTEN6`: as [`listen_message`], for the connections that arrive as
/// IPv6 as well.
pub fn listen6_message(sock: u64, port: u16, backlog: u16, window: u32) -> Vec<u8> {
    listen_of(OP_LISTEN6, sock, port, backlog, window)
}

fn listen_of(op: u32, sock: u64, port: u16, backlog: u16, window: u32) -> Vec<u8> {
    let mut m = header(op, sock, 8);
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

/// The bytes of an `ACCEPTED6`: the new socket `sock`, connected from
/// `addr:port` to the card's address `at`.
pub fn accepted6_bytes(sock: u64, addr: [u8; 16], port: u16, at: [u8; 16]) -> [u8; ACCEPTED6_LEN] {
    let mut b = [0u8; ACCEPTED6_LEN];
    b[..8].copy_from_slice(&sock.to_le_bytes());
    b[8..24].copy_from_slice(&addr);
    b[24..26].copy_from_slice(&port.to_le_bytes());
    b[28..44].copy_from_slice(&at);
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

/// The bytes of an `UNDELIVERED`: the datagram that went to `addr:port`
/// was the subject of an ICMP message of this type and code.
pub fn undelivered_bytes(addr: [u8; 4], port: u16, kind: u8, code: u8) -> [u8; UNDELIVERED_LEN] {
    let mut bytes = [0u8; UNDELIVERED_LEN];
    bytes[..4].copy_from_slice(&addr);
    bytes[4..6].copy_from_slice(&port.to_le_bytes());
    bytes[6] = kind;
    bytes[7] = code;
    bytes
}

/// Where the datagram of an `UNDELIVERED` went, and the type and code of
/// the message about it.
pub fn parse_undelivered(bytes: &[u8]) -> Option<(IpEndpoint, u8, u8)> {
    let bytes: &[u8; UNDELIVERED_LEN] = bytes.try_into().ok()?;
    let addr = Ipv4Address([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let port = u16::from_le_bytes([bytes[4], bytes[5]]);
    Some((
        IpEndpoint::new(IpAddress::Ipv4(addr), port),
        bytes[6],
        bytes[7],
    ))
}

/// The bytes of an `UNDELIVERED6`: the datagram that went to `addr:port`
/// as IPv6 was the subject of an ICMPv6 message of this type and code.
pub fn undelivered6_bytes(addr: [u8; 16], port: u16, kind: u8, code: u8) -> [u8; UNDELIVERED6_LEN] {
    let mut bytes = [0u8; UNDELIVERED6_LEN];
    bytes[..16].copy_from_slice(&addr);
    bytes[16..18].copy_from_slice(&port.to_le_bytes());
    bytes[18] = kind;
    bytes[19] = code;
    bytes
}

/// Where the datagram of an `UNDELIVERED6` went, and the type and code
/// of the message about it.
pub fn parse_undelivered6(bytes: &[u8]) -> Option<(IpEndpoint, u8, u8)> {
    let bytes: &[u8; UNDELIVERED6_LEN] = bytes.try_into().ok()?;
    let mut addr = [0u8; 16];
    addr.copy_from_slice(&bytes[..16]);
    let port = u16::from_le_bytes([bytes[16], bytes[17]]);
    Some((
        IpEndpoint::new(IpAddress::Ipv6(Ipv6Address(addr)), port),
        bytes[18],
        bytes[19],
    ))
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
    /// `WRITABLE` always, `ERROR` while an error waits to be told.
    /// Changed under `inner`.
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
    /// Every IPv4 packet of ICMP, whole: a raw socket. One of
    /// `AF_INET6` (`v6`): every ICMPv6 message its filter lets by,
    /// without the IPv6 header.
    Icmp,
    /// The answers to the echoes it sent, which carry its `port` as
    /// their identifier, without the IP header: a socket that pings.
    Echoes,
}

struct Inner {
    /// What arrived: who sent it, the bytes, and the hops it had left
    /// when that is known (ICMPv6), else 0.
    queue: VecDeque<(IpEndpoint, Vec<u8>, u8)>,
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
    /// The hops of the last read, for `recvmsg`.
    last_hops: u8,
    /// The types of ICMPv6 a raw socket does not want (`ICMP6_FILTER`):
    /// a bit each, set for one that is not, as the program wrote them
    /// (words of 32 bits, the lowest byte first on every machine this
    /// kernel runs on).
    blocked: [u8; 32],
    /// `shutdown(SHUT_RD)`: an empty queue is the end.
    read_closed: bool,
    patience: Patience,
    /// What was said of a datagram this socket sent its peer and was not
    /// delivered: what the next read or write answers, once, and
    /// `SO_ERROR`.
    error: Option<LxError>,
    /// The groups it asked to be a member of (`IP_ADD_MEMBERSHIP`).
    groups: Vec<Ipv4Address>,
}

impl Shared {
    /// Queue a datagram that arrived. `false` when there was no room.
    fn deliver(&self, from: IpEndpoint, data: &[u8]) -> bool {
        self.deliver_with(from, data, 0)
    }

    /// [`deliver`](Self::deliver), of what had `hops` left.
    fn deliver_with(&self, from: IpEndpoint, data: &[u8], hops: u8) -> bool {
        let mut inner = self.inner.lock();
        if inner.read_closed || inner.queued_bytes + data.len() > RX_QUEUE_BYTES {
            return false;
        }
        inner.queued_bytes += data.len();
        inner.queue.push_back((from, data.to_vec(), hops));
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
                last_hops: 0,
                blocked: [0; 32],
                read_closed: false,
                patience: Patience::default(),
                error: None,
                groups: Vec::new(),
            }),
            hears,
            v6,
        })
    }

    /// A datagram this socket sent to `to` was not delivered, and will
    /// not be the next time either: `error`. The program hears of it when
    /// the socket is connected to `to` (`__udp4_lib_err`: the socket is
    /// found by both ends of the datagram, and one that is not connected
    /// is told nothing without `IP_RECVERR`). Whoever waits to read is
    /// woken to be told.
    fn undelivered(&self, to: IpEndpoint, error: LxError) {
        let mut inner = self.inner.lock();
        if self.hears != Hears::Datagrams || inner.remote != Some(to) {
            return;
        }
        inner.error = Some(error);
        self.bus.lock().set(Event::ERROR);
    }

    /// The error that waits to be told, which is told once
    /// (`sock_error`).
    fn take_error(&self) -> Option<LxError> {
        Self::told(&self.bus, &mut self.inner.lock())
    }

    /// [`take_error`](Self::take_error), for who holds the socket's lock.
    fn told(bus: &Mutex<EventBus>, inner: &mut Inner) -> Option<LxError> {
        let error = inner.error.take()?;
        bus.lock().clear(Event::ERROR);
        Some(error)
    }

    /// Queue what is this socket's of an IPv4 packet of ICMP from `from`.
    fn deliver_heard(&self, from: Ipv4Address, packet: &[u8]) {
        if self.v6 {
            return;
        }
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

    /// Queue `message`, ICMPv6 from `from` that had `hops` left, when it
    /// is this socket's: any that a raw socket's filter lets by
    /// (`icmpv6_filter`), and for a socket that pings the answer to an
    /// echo of its own.
    fn deliver_heard6(&self, from: Ipv6Address, hops: u8, message: &[u8]) {
        let Some(&kind) = message.first() else {
            return;
        };
        if !self.v6 {
            return;
        }
        let from = IpEndpoint::new(IpAddress::Ipv6(from), 0);
        let wanted = {
            let inner = self.inner.lock();
            match self.hears {
                Hears::Datagrams => false,
                Hears::Icmp => inner.blocked[usize::from(kind >> 3)] & (1 << (kind & 7)) == 0,
                Hears::Echoes => {
                    icmp6::echo_reply_ident(message).is_some_and(|i| Some(i) == inner.port)
                }
            }
        };
        if wanted {
            self.deliver_with(from, message, hops);
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
                // Before what is queued, as Linux reads (`__skb_recv_udp`).
                if let Some(error) = Self::told(&self.bus, inner) {
                    return (Err(error), nobody());
                }
                let remote = inner.remote;
                // A connected socket hears its peer and nobody else.
                while matches!((inner.queue.front(), remote), (Some((from, _, _)), Some(peer)) if *from != peer)
                {
                    if let Some((_, dropped, _)) = inner.queue.pop_front() {
                        inner.queued_bytes -= dropped.len();
                    }
                }
                if let Some((from, datagram, hops)) = inner.queue.front() {
                    let (from, len, hops) = (*from, datagram.len(), *hops);
                    let n = data.len().min(len);
                    data[..n].copy_from_slice(&datagram[..n]);
                    inner.last_hops = hops;
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
            // Woken by a datagram or by an error to be told, or ended by
            // a signal or by `SO_RCVTIMEO` (`EAGAIN`).
            let bus = self.bus.clone();
            let news = Event::READABLE | Event::ERROR;
            if let Err(e) = crate::sync::wait_for_event_until(bus, news, until).await {
                return (Err(e), nobody());
            }
        }
    }

    /// The wait of a read that blocks, outside `read` (a `splice` from
    /// the socket): it ends with `SO_RCVTIMEO` as the read's own does.
    async fn wait_readable(&self, until: &mut Until) -> LxResult<()> {
        let until = until.of(&self.inner.lock().patience.receive);
        let bus = self.bus.clone();
        let news = Event::READABLE | Event::ERROR;
        crate::sync::wait_for_event_until(bus, news, until).await?;
        Ok(())
    }

    /// Queue a datagram another program of this machine sent to `to`. A
    /// socket bound to one address hears what is for that address:
    /// `false` when this one is bound to another. A datagram there was no
    /// room for is lost, and was this socket's all the same.
    fn deliver_here(&self, to: Ipv4Address, from: IpEndpoint, data: &[u8]) -> bool {
        let here = self.inner.lock().here;
        if !here.is_none_or(|here| here == to) {
            return false;
        }
        self.deliver(from, data);
        true
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
    /// The groups the machine is a member of, and how many sockets asked
    /// for each.
    groups: BTreeMap<Ipv4Address, usize>,
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
    /// The IPv6 addresses of the card of the stack that serves now: see
    /// [`sixes`](Self::sixes).
    sixes: Arc<Sixes>,
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

/// As [`plain`], for a socket whose datagrams also travel as IPv6: an
/// IPv6 address that is none of those stays what it is.
fn either(v6: bool, addr: IpAddress, elsewhere: LxError) -> LxResult<IpAddress> {
    match addr {
        IpAddress::Ipv6(six)
            if v6 && !six.is_unspecified() && !six.is_loopback() && six.as_ipv4().is_none() =>
        {
            Ok(addr)
        }
        _ => plain(v6, addr, elsewhere),
    }
}

/// The header IPv6 has more than IPv4's: what the longest datagram of a
/// frame loses when it travels as IPv6.
const IPV6_MORE: usize = 20;

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
            sixes: Arc::new(Sixes::default()),
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

    /// What the card of the stack that serves now has of IPv6, as it
    /// said last (`SIX`): for the interface that shows that card.
    /// Nothing while no stack serves.
    pub fn sixes(&self) -> Arc<Sixes> {
        self.sixes.clone()
    }

    /// [`serve`], on these sockets. The stack is told of the sockets that
    /// are there already and are its business: the datagram sockets that
    /// have a port and the stream sockets that listen, less the ones
    /// that are this machine's alone, and the ones that hear ICMP. And
    /// of the groups the machine is a member of.
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
            let hearing: Vec<(u64, bool)> = table
                .hearing
                .iter()
                .filter_map(|sock| {
                    let shared = table.socks.get(sock).and_then(Weak::upgrade)?;
                    Some((*sock, shared.v6))
                })
                .collect();
            (bound, listening, hearing)
        };
        for (sock, six) in hearing {
            let said = if six {
                hear6_message(sock)
            } else {
                hear_message(sock)
            };
            let _ = self.tell(said, true);
        }
        for (sock, port, shared) in bound {
            if shared.inner.lock().here.is_none() {
                let _ = self.tell(bind_message(sock, port), true);
            }
        }
        for (sock, port, stream) in listening {
            if stream.listens_for_the_stack() {
                let _ = self.tell(stream.listen_message(sock, port), true);
            }
        }
        for group in self.groups() {
            let _ = self.tell(join_message(group.0), true);
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
            Some((OP_DATA6, _)) => self.take_datagrams6(&m[HEADER_LEN..]),
            Some((OP_EVENTS, _)) => self.take_events(&m[HEADER_LEN..]),
            Some((OP_COUNTS, _)) => self.take_counts(&m[HEADER_LEN..]),
            Some((OP_SIX, _)) => match parse_six(&m[HEADER_LEN..]) {
                Some((router, addresses)) => self.sixes.say(addresses, router),
                None => warn!(
                    "user stack: {} bytes that are no addresses of a card",
                    m.len() - HEADER_LEN
                ),
            },
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
            match what {
                EV_UNDELIVERED | EV_UNDELIVERED6 => self.undelivered(sock, what, bytes),
                _ => self.event(sock, what, number, bytes),
            }
        }
    }

    /// `UNDELIVERED`, or `UNDELIVERED6` as `what` says: an ICMP message,
    /// or one of ICMPv6, said that a datagram of socket `sock` was not
    /// delivered. Its program is told of an error that
    /// does not mend by itself, if the socket is one that hears of them
    /// ([`Shared::undelivered`]); the rest (a router that has no way just
    /// now, a datagram that ran out of hops) is the news of whoever asked
    /// for ICMP, and a datagram socket's program sends again or gives up
    /// by itself.
    fn undelivered(&self, sock: u64, what: u16, bytes: &[u8]) {
        let said = if what == EV_UNDELIVERED6 {
            parse_undelivered6(bytes)
                .map(|(to, kind, code)| (to, icmp6::undelivered_error(kind, code)))
        } else {
            parse_undelivered(bytes)
                .map(|(to, kind, code)| (to, icmp::undelivered_error(kind, code)))
        };
        let Some((to, error)) = said else {
            warn!("user stack: an UNDELIVERED of {} bytes", bytes.len());
            return;
        };
        let Some((error, true)) = error else {
            return;
        };
        let shared = self.table.lock().socks.get(&sock).and_then(Weak::upgrade);
        if let Some(shared) = shared {
            shared.undelivered(to, error);
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

    /// The datagrams of a `DATA6`, each onto the queue of its socket:
    /// one of `AF_INET6`, since no other is given what travelled as
    /// IPv6.
    fn take_datagrams6(&self, mut records: &[u8]) {
        while !records.is_empty() {
            let Some(((sock, addr, port, data), rest)) = parse_record6(records) else {
                warn!(
                    "user stack: {} bytes that are no record of a datagram",
                    records.len()
                );
                return;
            };
            records = rest;
            if sock == HEARD {
                if let Some((hops, message)) = icmp6::message_of(data) {
                    self.heard6(Ipv6Address(addr), hops, message);
                }
                continue;
            }
            let shared = self.table.lock().socks.get(&sock).and_then(Weak::upgrade);
            if let Some(shared) = shared.filter(|shared| shared.v6) {
                let from = IpEndpoint::new(IpAddress::Ipv6(Ipv6Address(addr)), port);
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
            // And says its own addresses.
            self.sixes.forget();
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

    /// A socket that speaks ICMP is there, or ICMPv6 when `six`: the
    /// stack is told to hear for it, and the next stack when it comes.
    fn hear(&self, sock: u64, six: bool) {
        let _telling = self.telling.lock();
        self.table.lock().hearing.insert(sock);
        let said = if six {
            hear6_message(sock)
        } else {
            hear_message(sock)
        };
        let _ = self.tell(said, true);
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

    /// The sockets that hear ICMP, of either kind.
    fn hearing(&self) -> Vec<Arc<Shared>> {
        let table = self.table.lock();
        let socks = table.hearing.iter();
        socks
            .filter_map(|sock| table.socks.get(sock).and_then(Weak::upgrade))
            .collect()
    }

    /// An IPv4 packet of ICMP from `from`, off the card or made here:
    /// to every socket that hears, which takes what is its own.
    fn heard(&self, from: Ipv4Address, packet: &[u8]) {
        for shared in self.hearing() {
            shared.deliver_heard(from, packet);
        }
    }

    /// An ICMPv6 message from `from` that had `hops` left, off the card
    /// or made here: to every socket that hears, which takes what is its
    /// own.
    fn heard6(&self, from: Ipv6Address, hops: u8, message: &[u8]) {
        for shared in self.hearing() {
            shared.deliver_heard6(from, hops, message);
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

    /// A datagram a program of this machine sent to an IPv6 address of
    /// the card's: onto the queue of the `AF_INET6` socket that has the
    /// port, unless it is bound to another address of this machine.
    /// `false` when there is none.
    fn send_here6(&self, from: IpEndpoint, port: u16, data: &[u8]) -> bool {
        let shared = {
            let table = self.table.lock();
            let sock = table.bound.get(&port);
            sock.and_then(|sock| table.socks.get(sock))
                .and_then(Weak::upgrade)
        };
        shared.is_some_and(|shared| {
            let anywhere = shared.inner.lock().here.is_none();
            shared.v6 && anywhere && {
                shared.deliver(from, data);
                true
            }
        })
    }

    /// A datagram from one program of this machine to another: onto the
    /// queue of the socket that has the port, if one has. `false` when
    /// nobody has it at that address: the datagram is lost, and who sent
    /// it is refused as a host with nobody at the port refuses.
    fn send_here(&self, from: IpEndpoint, to: Ipv4Address, port: u16, data: &[u8]) -> bool {
        let shared = {
            let table = self.table.lock();
            let sock = table.bound.get(&port);
            sock.and_then(|sock| table.socks.get(sock))
                .and_then(Weak::upgrade)
        };
        shared.is_some_and(|shared| shared.deliver_here(to, from, data))
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
    /// The same, of the options that are IPv6's: how many hops what
    /// travels as IPv6 may (`IPV6_UNICAST_HOPS`, `IPV6_MULTICAST_HOPS`).
    six: Mutex<icmp6::Opts>,
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
            six: Mutex::new(icmp6::Opts::default()),
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

    /// Send `data` to `addr:port`, an IPv6 address that is no IPv4 one
    /// in disguise: by the stack as IPv6, or to the socket of this
    /// machine that has the port when the address is the card's own.
    fn write6(&self, addr: Ipv6Address, port: u16, data: &[u8]) -> SysResult {
        let here = {
            let mut inner = self.shared.inner.lock();
            if let Some(error) = Shared::told(&self.shared.bus, &mut inner) {
                return Err(error);
            }
            inner.here
        };
        let sixes = &self.link.sixes;
        if sixes.has(&addr) {
            if data.len() > MAX_DATAGRAM {
                return Err(LxError::EMSGSIZE);
            }
            let from = IpEndpoint::new(IpAddress::Ipv6(addr), self.autobind()?);
            if !self.link.send_here6(from, port, data) {
                let to = IpEndpoint::new(IpAddress::Ipv6(addr), port);
                self.shared.undelivered(to, LxError::ECONNREFUSED);
            }
            return Ok(data.len());
        }
        // Nothing leaves the machine from an address that is only the
        // machine's, nor while no stack serves, nor for where the card
        // has no way to.
        if here.is_some() {
            return Err(LxError::EINVAL);
        }
        if self.link.is_down() {
            return Err(LxError::ENETDOWN);
        }
        if !sixes.reaches(&addr) {
            return Err(LxError::ENETUNREACH);
        }
        if data.len() + IPV6_MORE > self.link.max_datagram.load(Ordering::Acquire) {
            return Err(LxError::EMSGSIZE);
        }
        self.autobind()?;
        // As many hops as the socket was told (`IPV6_UNICAST_HOPS`,
        // `IPV6_MULTICAST_HOPS`). When it was not, a group's is not to
        // leave the link, and the rest travel as far as the stack lets a
        // datagram.
        let said = self.six.lock().hops_said(&addr);
        let hop_limit = said.unwrap_or(u8::from(addr.is_multicast()));
        let said = send6_message(self.sock, addr.0, port, hop_limit, data);
        self.link.tell(said, false)?;
        Ok(data.len())
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
        self.leave_every_group();
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
                to.addr = either(self.v6, to.addr, LxError::ENETUNREACH)?;
                to
            }
            Some(_) => return Err(LxError::EAFNOSUPPORT),
            None => self.shared.inner.lock().remote.ok_or(LxError::ENOTCONN)?,
        };
        let addr = match to.addr {
            IpAddress::Ipv4(addr) => addr,
            IpAddress::Ipv6(addr) => return self.write6(addr, to.port, data),
            _ => return Err(LxError::EAFNOSUPPORT),
        };
        let here = {
            let mut inner = self.shared.inner.lock();
            // What was said of a datagram before this one is told first,
            // and this one is not sent (`sock_error`).
            if let Some(error) = Shared::told(&self.shared.bus, &mut inner) {
                return Err(error);
            }
            inner.here
        };
        if self.link.is_here(addr) {
            // No card, so no frame to fit: the longest datagram there is.
            if data.len() > MAX_DATAGRAM {
                return Err(LxError::EMSGSIZE);
            }
            let port = self.autobind()?;
            let from = here.unwrap_or_else(|| self.link.here_for(addr));
            let from = IpEndpoint::new(IpAddress::Ipv4(from), port);
            if !self.link.send_here(from, addr, to.port, data) {
                // Nobody at the port, said as the ICMP message that
                // would have come back says it.
                self.shared.undelivered(to, LxError::ECONNREFUSED);
            }
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
        let port = self.autobind()?;
        let said = send_message(self.sock, addr.0, to.port, self.hop_limit(addr), data);
        self.link.tell(said, false)?;
        if addr.is_multicast() {
            self.loop_back(port, to, data);
        }
        Ok(data.len())
    }

    fn poll(&self, _events: PollEvents) -> (bool, bool, bool) {
        let inner = self.shared.inner.lock();
        // An error is not something to read: `udp_poll` says it apart.
        (
            !inner.queue.is_empty() || inner.read_closed,
            true,
            inner.error.is_some(),
        )
    }

    fn take_so_error(&self) -> i32 {
        self.shared.take_error().map_or(0, |e| e as isize as i32)
    }

    async fn connect(&self, endpoint: Endpoint) -> SysResult {
        let Endpoint::Ip(mut peer) = endpoint else {
            return Err(LxError::EAFNOSUPPORT);
        };
        peer.addr = either(self.v6, peer.addr, LxError::ENETUNREACH)?;
        match peer.addr {
            IpAddress::Ipv4(_) => {}
            // `ip6_datagram_connect`: no way there is said now.
            IpAddress::Ipv6(addr) => {
                let sixes = &self.link.sixes;
                if !sixes.has(&addr) && !sixes.reaches(&addr) {
                    return Err(LxError::ENETUNREACH);
                }
            }
            _ => return Err(LxError::EAFNOSUPPORT),
        }
        self.autobind()?;
        self.shared.inner.lock().remote = Some(peer);
        Ok(0)
    }

    fn bind(&self, endpoint: Endpoint) -> SysResult {
        let Endpoint::Ip(mut local) = endpoint else {
            return Err(LxError::EINVAL);
        };
        local.addr = either(self.v6, local.addr, LxError::EADDRNOTAVAIL)?;
        // An IPv6 address has to be one the card has; the socket then
        // hears at its port, like one bound to `::`.
        if let IpAddress::Ipv6(addr) = local.addr {
            if !self.link.sixes.has(&addr) {
                return Err(LxError::EADDRNOTAVAIL);
            }
        }
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
        if let Some(done) = self.membership(level, opt, data) {
            return done;
        }
        if level == icmp6::IPPROTO_IPV6 {
            // IPv6's are no options of an `AF_INET` socket
            // (`ip_setsockopt`).
            if !self.v6 {
                return Err(LxError::ENOPROTOOPT);
            }
            if let Some(kept) = self.six.lock().set_hops(opt, data) {
                return kept.map(|()| 0);
            }
        }
        if let Some(kept) = self.opts.lock().try_setsockopt(level, opt, data) {
            return kept;
        }
        setsockopt_unstored(self, level, opt, data)
    }

    fn getsockopt_own(&self, level: usize, opt: usize) -> Option<Vec<u8>> {
        if !self.v6 || level != icmp6::IPPROTO_IPV6 {
            return None;
        }
        // Whether what arrives says its hops is not kept here: nothing
        // says it of a datagram.
        let said = self
            .six
            .lock()
            .get(opt)
            .filter(|_| !icmp6::tells_hops(opt))?;
        Some(said.to_ne_bytes().to_vec())
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
            if (want_read && status.read) || want_write || !want_read || status.error {
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
        Some(inner.queue.front().map_or(0, |(_, d, _)| d.len()))
    }

    fn as_socket(&self) -> LxResult<&dyn Socket> {
        Ok(self)
    }
}

#[cfg(test)]
mod tests;
