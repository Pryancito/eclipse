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
//! DATA    (stack to kernel)  16  [u8; 4] from  20  u16 port  22  u16 zero  24  the datagram
//! ```
//!
//! little-endian, the addresses as they are written. The kernel numbers the
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
//! IPv4 datagram sockets only, so far.

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

/// The operations of a message.
pub const OP_BIND: u32 = 1;
pub const OP_SEND: u32 = 2;
pub const OP_CLOSE: u32 = 3;
pub const OP_DATA: u32 = 4;

/// What every message starts with.
pub const HEADER_LEN: usize = 16;
/// Where the datagram starts in `SEND` and `DATA`.
pub const DATAGRAM_AT: usize = 24;
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

/// `SEND` or `DATA` (`op`): `data` for, or from, `addr:port`.
pub fn datagram_message(op: u32, sock: u64, addr: [u8; 4], port: u16, data: &[u8]) -> Vec<u8> {
    let mut m = header(op, sock, 8 + data.len());
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

/// The address, port and datagram of a `SEND` or `DATA` message.
pub fn parse_datagram(m: &[u8]) -> Option<([u8; 4], u16, &[u8])> {
    if m.len() < DATAGRAM_AT || m.len() - DATAGRAM_AT > MAX_DATAGRAM {
        return None;
    }
    let addr: [u8; 4] = m[16..20].try_into().ok()?;
    let port = u16::from_le_bytes(m[20..22].try_into().ok()?);
    Some((addr, port, &m[DATAGRAM_AT..]))
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

#[derive(Default)]
struct Table {
    socks: BTreeMap<u64, Weak<Shared>>,
    ports: BTreeSet<u16>,
    next_sock: u64,
    next_port: u16,
}

/// The kernel's end of a stack that runs as a process.
pub struct StackLink {
    channel: Arc<Channel>,
    /// The longest datagram the stack carries: what fits one frame of its
    /// card, for a stack that does not fragment.
    max_datagram: usize,
    table: Mutex<Table>,
    dead: AtomicBool,
}

/// The stack that serves the sockets of the programs that ask for one.
static STACK: Mutex<Option<Arc<StackLink>>> = Mutex::new(None);

/// A process serves a stack on `channel`, the kernel's end, that carries
/// datagrams of up to `max_datagram` bytes: a longer one is `EMSGSIZE` to
/// who sends it. `ALREADY_BOUND` while another process serves one.
pub fn serve(channel: Arc<Channel>, max_datagram: usize) -> ZxResult<Arc<StackLink>> {
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
        table: Mutex::new(Table {
            next_port: *EPHEMERAL.start(),
            ..Table::default()
        }),
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
        let Some((OP_DATA, sock)) = parse_header(m) else {
            warn!(
                "user stack: a message that means nothing ({} bytes)",
                m.len()
            );
            return;
        };
        let Some((addr, port, data)) = parse_datagram(m) else {
            warn!("user stack: a datagram that is not one ({} bytes)", m.len());
            return;
        };
        // A datagram for a socket that closed meanwhile is nobody's.
        let shared = self.table.lock().socks.get(&sock).and_then(Weak::upgrade);
        if let Some(shared) = shared {
            let from = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address(addr)), port);
            shared.deliver(from, data);
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
        let socks: Vec<Weak<Shared>> = self.table.lock().socks.values().cloned().collect();
        for shared in socks.iter().filter_map(Weak::upgrade) {
            shared.wake();
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
        let port = {
            let mut table = self.table.lock();
            let port = if wanted != 0 {
                if table.ports.contains(&wanted) {
                    return Err(LxError::EADDRINUSE);
                }
                wanted
            } else {
                let span = EPHEMERAL.len() as u32;
                let first = table.next_port;
                (0..span)
                    .map(|i| {
                        let at = (first - EPHEMERAL.start()) as u32 + i;
                        EPHEMERAL.start() + (at % span) as u16
                    })
                    .find(|p| !table.ports.contains(p))
                    .ok_or(LxError::EADDRINUSE)?
            };
            table.ports.insert(port);
            if wanted == 0 {
                table.next_port = if port == *EPHEMERAL.end() {
                    *EPHEMERAL.start()
                } else {
                    port + 1
                };
            }
            port
        };
        if let Err(e) = self.tell(bind_message(sock, port), true) {
            self.table.lock().ports.remove(&port);
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
                table.ports.remove(&port);
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
        self.link.tell(
            datagram_message(OP_SEND, self.sock, addr.0, to.port, data),
            false,
        )?;
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
    struct Stack {
        link: Arc<StackLink>,
        end: Arc<Channel>,
    }

    fn stack() -> Stack {
        let (kernel, end) = Channel::create();
        let link = serve(kernel, MAX_FOR_TESTS).expect("the stack before is gone");
        Stack { link, end }
    }

    impl Stack {
        fn socket(&self) -> ServedUdpSocket {
            let s = ServedUdpSocket::new(self.link.clone()).unwrap();
            FileLike::set_flags(&s, OpenFlags::NON_BLOCK).unwrap();
            s
        }

        /// The next message the kernel sent, if it sent one.
        fn told(&self) -> Option<Vec<u8>> {
            self.end.read().ok().map(|m| m.data)
        }

        /// A datagram arrived for socket `sock`.
        fn arrives(&self, sock: u64, from: [u8; 4], port: u16, data: &[u8]) {
            self.say(datagram_message(OP_DATA, sock, from, port, data));
        }

        fn say(&self, data: Vec<u8>) {
            let msg = MessagePacket {
                data,
                handles: Vec::new(),
            };
            self.end.write(msg).unwrap();
            self.link.pump();
        }
    }

    fn any(port: u16) -> Endpoint {
        Endpoint::Ip(IpEndpoint::new(
            IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
            port,
        ))
    }

    fn at(addr: [u8; 4], port: u16) -> IpEndpoint {
        IpEndpoint::new(IpAddress::Ipv4(Ipv4Address(addr)), port)
    }

    /// A destination, as `sendto` gives it.
    fn to(addr: [u8; 4], port: u16) -> Option<Endpoint> {
        Some(Endpoint::Ip(at(addr, port)))
    }

    fn ip(endpoint: Endpoint) -> IpEndpoint {
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

    const HOST: [u8; 4] = [10, 0, 9, 1];
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

        let send = datagram_message(OP_SEND, u64::MAX, [10, 0, 9, 1], 53, b"hola");
        assert_eq!(parse_header(&send), Some((OP_SEND, u64::MAX)));
        assert_eq!(
            parse_datagram(&send),
            Some(([10, 0, 9, 1], 53, &b"hola"[..]))
        );
        // An empty datagram is one.
        let empty = datagram_message(OP_DATA, 1, [1, 2, 3, 4], 9, b"");
        assert_eq!(empty.len(), DATAGRAM_AT);
        assert_eq!(parse_datagram(&empty), Some(([1, 2, 3, 4], 9, &b""[..])));

        assert_eq!(parse_header(&close_message(3)), Some((OP_CLOSE, 3)));
        assert_eq!(close_message(3).len(), HEADER_LEN);

        // The same bytes `tools/eclipse-netstack/src/socks.rs` reads and
        // builds, written out: neither side can move a field alone.
        assert_eq!(
            datagram_message(OP_DATA, 0x0807_0605_0403_0201, [10, 0, 9, 1], 0x1234, b"hi"),
            [
                0, 0, 0, 0, 4, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 0, 9, 1, 0x34, 0x12, 0, 0,
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
        assert_eq!(parse_datagram(&send[..DATAGRAM_AT - 1]), None);
        // Longer than UDP carries.
        let long = alloc::vec![0u8; DATAGRAM_AT + MAX_DATAGRAM + 1];
        assert_eq!(parse_datagram(&long), None);
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
            Some(datagram_message(OP_SEND, a.sock, HOST, 9, b"uno"))
        );
        // The second send has the port already; another socket gets another.
        assert_eq!(Socket::write(&a, b"dos", to(HOST, 9)), Ok(3));
        assert_eq!(
            stack.told(),
            Some(datagram_message(OP_SEND, a.sock, HOST, 9, b"dos"))
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
        stack.link.table.lock().next_port = *EPHEMERAL.end();
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

        stack.arrives(a.sock, HOST, 4000, b"primero");
        stack.arrives(a.sock, [10, 0, 9, 2], 4001, b"segundo, y largo");
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
        stack.say(datagram_message(OP_SEND, a.sock, HOST, 1, b"not DATA"));
        stack.say(header(OP_DATA, a.sock, 0));
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
        assert_eq!(
            stack.told(),
            Some(datagram_message(OP_SEND, a.sock, HOST, 7, b"ping"))
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
        assert_eq!(last, Some(datagram_message(OP_SEND, a.sock, HOST, 9, b"x")));
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
            serve(other, MAX_FOR_TESTS).err(),
            Some(ZxError::ALREADY_BOUND)
        );
        // And one that carries nothing, or more than UDP does, is none.
        for bad in [0, MAX_DATAGRAM + 1] {
            let (other, _other_end) = Channel::create();
            assert_eq!(serve(other, bad).err(), Some(ZxError::INVALID_ARGS));
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
        assert!(link.table.lock().ports.is_empty());

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
