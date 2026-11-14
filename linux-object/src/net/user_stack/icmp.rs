//! Sockets that speak ICMP, whose stack is a process: what `ping` opens.
//! What the kernel and the stack say to each other is in the module above.
//!
//! There are two, as in Linux. The raw one (`SOCK_RAW`) writes ICMP
//! messages as the program made them and reads every IPv4 packet of ICMP
//! the machine gets, header and all. The one that pings (`SOCK_DGRAM`)
//! writes echoes only, which leave with the socket's identifier in place
//! of the program's, and reads the answers to them, without the header.
//!
//! The same two of `AF_INET6` speak ICMPv6, and what is different about
//! them is in [`super::icmp6`].
//!
//! Not here: `IP_HDRINCL` (the header is the stack's to write), a raw
//! socket bound to one address hearing only what is for it, and what
//! Linux queues for a socket that pings when an echo comes back as an
//! error.

use super::*;
use alloc::borrow::Cow;

const ECHO_REPLY: u8 = 0;
const ECHO_REQUEST: u8 = 8;
/// What an echo starts with: type, code, checksum, identifier, sequence.
const ECHO_HEADER: usize = 8;
/// The IPv4 header a message travels behind.
const IP_HEADER: usize = 20;
/// The hop limit of what this machine says to itself.
const HOPS_HERE: u8 = 64;

/// The ICMP message of an IPv4 packet, when it carries one.
pub(super) fn message_of(packet: &[u8]) -> Option<&[u8]> {
    let first = *packet.first()?;
    let header = usize::from(first & 0x0f) * 4;
    if first >> 4 != 4 || header < IP_HEADER || packet.len() < header || packet[9] != 1 {
        return None;
    }
    Some(&packet[header..])
}

/// The identifier of `message` when it is the answer to an echo.
pub(super) fn echo_reply_ident(message: &[u8]) -> Option<u16> {
    (message.len() >= ECHO_HEADER && message[0] == ECHO_REPLY && message[1] == 0)
        .then(|| u16::from_be_bytes([message[4], message[5]]))
}

/// ICMP's destination unreachable and time exceeded: the messages about
/// something that was sent and not delivered.
const UNREACHABLE: u8 = 3;
const TIME_EXCEEDED: u8 = 11;

/// What a program is told when an ICMP message of this type and code
/// says that something its socket sent was not delivered, and whether
/// that is an error that does not mend by itself: Linux's table
/// (`icmp_err_convert`) and what `__udp4_lib_err` adds to it. `None` for
/// a message that is not about that.
///
/// A connect under way ends with the error, mending or not
/// (`tcp_v4_err`). A datagram socket's program hears only of the ones
/// that do not mend, and only on a connected socket.
///
/// Two codes nobody sends any more have on Linux an error this kernel
/// has no name for, and say the nearest: the destination host is unknown
/// (7, `EHOSTDOWN`) and the source is isolated (8, `ENONET`).
pub(super) fn undelivered_error(kind: u8, code: u8) -> Option<(LxError, bool)> {
    use LxError::*;
    match (kind, code) {
        (TIME_EXCEEDED, _) => Some((EHOSTUNREACH, false)),
        (UNREACHABLE, code) => Some(match code {
            // The network, the host: a router's word, which may change.
            0 | 11 => (ENETUNREACH, false),
            1 | 12 => (EHOSTUNREACH, false),
            // The protocol, the port: the host's own word.
            2 => (ENOPROTOOPT, true),
            3 => (ECONNREFUSED, true),
            // Too long for the way, and not to be cut.
            4 => (EMSGSIZE, true),
            5 => (EOPNOTSUPP, false),
            6 | 8 | 9 => (ENETUNREACH, true),
            7 | 10 | 13 | 14 | 15 => (EHOSTUNREACH, true),
            _ => (EHOSTUNREACH, false),
        }),
        _ => None,
    }
}

/// The error of a connect that a `FAILED` says ended because of an ICMP
/// message, whose type and code are its `bytes`: `None` when it carries
/// none, or none that means anything.
pub(super) fn connect_error(bytes: &[u8]) -> Option<LxError> {
    let &[kind, code] = bytes else {
        return None;
    };
    undelivered_error(kind, code).map(|(error, _)| error)
}

/// The Internet checksum of `bytes`: 0 for what carries its own and has
/// it right.
pub(super) fn checksum(bytes: &[u8]) -> u16 {
    let mut sum = 0u32;
    let (pairs, odd) = bytes.as_chunks::<2>();
    for pair in pairs {
        sum += u32::from(u16::from_be_bytes(*pair));
    }
    if let [last] = odd {
        sum += u32::from(*last) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Write `message`'s checksum into it.
fn seal(message: &mut [u8]) {
    message[2..4].copy_from_slice(&[0, 0]);
    let sum = checksum(message);
    message[2..4].copy_from_slice(&sum.to_be_bytes());
}

/// The IPv4 packet that carries `message` from `from` to `to`.
fn packet(from: Ipv4Address, to: Ipv4Address, hop_limit: u8, message: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(IP_HEADER + message.len());
    packet.extend_from_slice(&[0x45, 0]);
    packet.extend_from_slice(&((IP_HEADER + message.len()) as u16).to_be_bytes());
    // No identification, never a fragment.
    packet.extend_from_slice(&[0, 0, 0x40, 0]);
    packet.extend_from_slice(&[hop_limit, 1, 0, 0]);
    packet.extend_from_slice(&from.0);
    packet.extend_from_slice(&to.0);
    let sum = checksum(&packet);
    packet[10..12].copy_from_slice(&sum.to_be_bytes());
    packet.extend_from_slice(message);
    packet
}

/// The IPv4 address `addr` names.
fn v4(addr: IpAddress) -> LxResult<Ipv4Address> {
    match addr {
        IpAddress::Ipv4(addr) => Ok(addr),
        IpAddress::Unspecified => Ok(Ipv4Address::UNSPECIFIED),
        _ => Err(LxError::EAFNOSUPPORT),
    }
}

/// Where a program means when it says `addr`: nowhere is this machine
/// (`ip_route_output`).
fn destination(addr: IpAddress) -> LxResult<Ipv4Address> {
    let addr = v4(addr)?;
    Ok(if addr.is_unspecified() {
        Ipv4Address::new(127, 0, 0, 1)
    } else {
        addr
    })
}

impl StackLink {
    /// An ICMP message a program of this machine sent to this machine at
    /// `to`. Whoever hears ICMP hears it, and an echo is answered, as the
    /// stack of a machine answers one: neither leaves it.
    fn icmp_here(&self, to: Ipv4Address, message: &[u8]) {
        let from = self.here_for(to);
        self.heard(from, &packet(from, to, HOPS_HERE, message));
        let echo = message.len() >= ECHO_HEADER && message[0] == ECHO_REQUEST && message[1] == 0;
        // One with a wrong checksum is dropped, as one off a card is.
        if echo && checksum(message) == 0 {
            let mut answer = message.to_vec();
            answer[0] = ECHO_REPLY;
            seal(&mut answer);
            self.heard(to, &packet(to, from, HOPS_HERE, &answer));
        }
    }
}

/// A socket that speaks ICMP, or ICMPv6, whose stack is a process.
pub struct ServedIcmpSocket {
    base: KObjectBase,
    pub(super) link: Arc<StackLink>,
    pub(super) sock: u64,
    /// `shared.v6` for a socket of ICMPv6.
    pub(super) shared: Arc<Shared>,
    /// The options a program sets and asks for again. `IP_TTL` is the
    /// one that changes what leaves.
    opts: Mutex<StoredInetOpts>,
    /// And the ones of a socket of ICMPv6.
    pub(super) six: Mutex<icmp6::Opts>,
}

impl ServedIcmpSocket {
    /// A socket of `link` that speaks ICMP: the raw one when `raw`, else
    /// the one that pings.
    pub fn new(link: Arc<StackLink>, raw: bool) -> LxResult<Self> {
        Self::open(link, raw, false)
    }

    /// The same, that speaks ICMPv6.
    pub fn new6(link: Arc<StackLink>, raw: bool) -> LxResult<Self> {
        Self::open(link, raw, true)
    }

    fn open(link: Arc<StackLink>, raw: bool, six: bool) -> LxResult<Self> {
        let shared = Shared::new(if raw { Hears::Icmp } else { Hears::Echoes }, six);
        let sock = {
            let mut table = link.table.lock();
            table.next_sock += 1;
            let sock = table.next_sock;
            table.socks.insert(sock, Arc::downgrade(&shared));
            sock
        };
        link.hear(sock, six);
        Ok(ServedIcmpSocket {
            base: KObjectBase::new(),
            link,
            sock,
            shared,
            opts: Mutex::new(StoredInetOpts::default()),
            six: Mutex::new(icmp6::Opts::default()),
        })
    }

    pub(super) fn is_raw(&self) -> bool {
        self.shared.hears == Hears::Icmp
    }

    fn is_six(&self) -> bool {
        self.shared.v6
    }

    /// The identifier of a socket that pings, giving it any when it has
    /// none: what a send or a connect on an unbound socket does.
    pub(super) fn autobind(&self) -> LxResult<u16> {
        if let Some(ident) = self.shared.inner.lock().port {
            return Ok(ident);
        }
        let ident = self.link.ident(0)?;
        self.shared.inner.lock().port = Some(ident);
        Ok(ident)
    }

    /// What leaves when a socket that pings writes `data`: an echo, with
    /// the socket's identifier and the checksum that goes with it
    /// (`ping_common_sendmsg`: anything else is `EINVAL`).
    fn echo(&self, data: &[u8]) -> LxResult<Vec<u8>> {
        if data.len() < ECHO_HEADER || data[0] != ECHO_REQUEST || data[1] != 0 {
            return Err(LxError::EINVAL);
        }
        let ident = self.autobind()?;
        let mut message = data.to_vec();
        message[4..6].copy_from_slice(&ident.to_be_bytes());
        seal(&mut message);
        Ok(message)
    }
}

impl Drop for ServedIcmpSocket {
    fn drop(&mut self) {
        let ident = match self.shared.hears {
            Hears::Echoes => self.shared.inner.lock().port,
            _ => None,
        };
        self.link.deaf(self.sock, ident);
    }
}

#[async_trait]
impl Socket for ServedIcmpSocket {
    async fn read(&self, data: &mut [u8]) -> (SysResult, Endpoint) {
        self.shared.receive(data, true).await
    }

    async fn peek(&self, data: &mut [u8]) -> (SysResult, Endpoint) {
        self.shared.receive(data, false).await
    }

    fn write(&self, data: &[u8], sendto_endpoint: Option<Endpoint>) -> SysResult {
        if self.is_six() {
            return self.write6(data, sendto_endpoint);
        }
        let to = match sendto_endpoint {
            Some(Endpoint::Ip(to)) => to,
            Some(_) => return Err(LxError::EAFNOSUPPORT),
            None => self.shared.inner.lock().remote.ok_or(LxError::ENOTCONN)?,
        };
        let to = destination(to.addr)?;
        // The length of an IPv4 packet is 16 bits, header and all.
        if data.len() > usize::from(u16::MAX) - IP_HEADER {
            return Err(LxError::EMSGSIZE);
        }
        let message = if self.is_raw() {
            Cow::Borrowed(data)
        } else {
            Cow::Owned(self.echo(data)?)
        };
        if self.link.is_here(to) {
            self.link.icmp_here(to, &message);
            return Ok(data.len());
        }
        let (broadcast, ttl) = {
            let opts = self.opts.lock();
            // To a group, as far as a group's are to go (`ip_select_ttl`).
            let ttl = if to.is_multicast() {
                opts.mcast_ttl
            } else {
                opts.ip_ttl
            };
            (opts.broadcast, ttl)
        };
        // `ip_route_output`: everybody is not written to by accident.
        if to.is_broadcast() && !broadcast {
            return Err(LxError::EACCES);
        }
        // Nothing leaves the machine while no stack serves.
        if self.link.is_down() {
            return Err(LxError::ENETDOWN);
        }
        // A UDP datagram's header is as long as an echo's, and the stack
        // does not fragment either.
        if message.len() > self.link.max_datagram.load(Ordering::Acquire) + ECHO_HEADER {
            return Err(LxError::EMSGSIZE);
        }
        let hop_limit = ttl.clamp(1, 255) as u8;
        let said = icmp_message(self.sock, to.0, hop_limit, &message);
        self.link.tell(said, false)?;
        Ok(data.len())
    }

    fn poll(&self, _events: PollEvents) -> (bool, bool, bool) {
        let inner = self.shared.inner.lock();
        (!inner.queue.is_empty() || inner.read_closed, true, false)
    }

    async fn connect(&self, endpoint: Endpoint) -> SysResult {
        if self.is_six() {
            return self.connect6(endpoint);
        }
        let Endpoint::Ip(peer) = endpoint else {
            return Err(LxError::EAFNOSUPPORT);
        };
        let peer = destination(peer.addr)?;
        if !self.is_raw() {
            self.autobind()?;
        }
        // Who answers has no port to answer from.
        self.shared.inner.lock().remote = Some(IpEndpoint::new(IpAddress::Ipv4(peer), 0));
        Ok(0)
    }

    fn bind(&self, endpoint: Endpoint) -> SysResult {
        if self.is_six() {
            return self.bind6(endpoint);
        }
        let Endpoint::Ip(local) = endpoint else {
            return Err(LxError::EINVAL);
        };
        let addr = v4(local.addr)?;
        if !addr.is_unspecified() && !self.link.is_here(addr) {
            return Err(LxError::EADDRNOTAVAIL);
        }
        // A raw socket has no port, and a `sockaddr`'s is not looked at.
        if self.is_raw() {
            return Ok(0);
        }
        // `ping_bind`: a socket that has an identifier is `EINVAL`.
        if self.shared.inner.lock().port.is_some() {
            return Err(LxError::EINVAL);
        }
        let ident = self.link.ident(local.port)?;
        self.shared.inner.lock().port = Some(ident);
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
        let ident = self.shared.inner.lock().port.unwrap_or(0);
        let local = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED), ident);
        Some(Endpoint::Ip(shown(Takes::of(self.is_six(), false), local)))
    }

    fn remote_endpoint(&self) -> Option<Endpoint> {
        self.shared.inner.lock().remote.map(Endpoint::Ip)
    }

    fn is_inet(&self) -> bool {
        true
    }

    fn is_raw_ipv4(&self) -> bool {
        self.is_raw() && !self.is_six()
    }

    fn so_domain(&self) -> Option<u32> {
        Some(if self.is_six() {
            Domain::AF_INET6
        } else {
            Domain::AF_INET
        } as u32)
    }

    fn so_protocol(&self) -> Option<u32> {
        // IPPROTO_ICMPV6, IPPROTO_ICMP
        Some(if self.is_six() { 58 } else { 1 })
    }

    fn socket_type(&self) -> Option<SocketType> {
        Some(if self.is_raw() {
            SocketType::SOCK_RAW
        } else {
            SocketType::SOCK_DGRAM
        })
    }

    fn take_msg_flags(&self) -> i32 {
        core::mem::take(&mut self.shared.inner.lock().last_msg_flags)
    }

    fn getsockopt_own(&self, level: usize, opt: usize) -> Option<Vec<u8>> {
        if self.is_six() {
            self.getsockopt6(level, opt)
        } else {
            None
        }
    }

    fn recv_cmsgs(&self) -> Vec<(i32, i32, i32)> {
        if self.is_six() {
            self.recv_cmsgs6()
        } else {
            Vec::new()
        }
    }

    fn setsockopt(&self, level: usize, opt: usize, data: &[u8]) -> SysResult {
        const IPPROTO_IP: usize = 0;
        const IP_HDRINCL: usize = 3;
        if self.is_six() {
            if let Some(done) = self.setsockopt6(level, opt, data) {
                return done;
            }
        }
        if level == IPPROTO_IP && opt == IP_HDRINCL {
            // Not an option of a socket that pings. A raw one may say it
            // leaves the header to who sends for it, which is how it is;
            // that it writes its own is what the stack does not take.
            if !self.is_raw() {
                return Err(LxError::ENOPROTOOPT);
            }
            check_setsockopt_len(level, opt, data)?;
            return if data.iter().take(4).any(|b| *b != 0) {
                Err(LxError::EOPNOTSUPP)
            } else {
                Ok(0)
            };
        }
        // The two timeouts are kept where the waits look.
        if let Some(kept) = self.shared.inner.lock().patience.set(level, opt, data) {
            return kept;
        }
        if let Some(kept) = self.opts.lock().try_setsockopt(level, opt, data) {
            return kept;
        }
        check_setsockopt_len(level, opt, data)?;
        Ok(0)
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

impl_kobject!(ServedIcmpSocket);

#[async_trait]
impl FileLike for ServedIcmpSocket {
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
        handle_net_ioctl(request, arg1, arg2, arg3, false)
    }

    /// `FIONREAD`: the size of what is read next, 0 when there is nothing.
    fn readable_bytes(&self) -> Option<usize> {
        let inner = self.shared.inner.lock();
        Some(inner.queue.front().map_or(0, |(_, d, _)| d.len()))
    }

    fn as_socket(&self) -> LxResult<&dyn Socket> {
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        any, at, ip, stack, stack_of, Stack, HOST, MAX_FOR_TESTS, STACK_ADDR,
    };
    use super::*;
    use crate::net::NET_TEST_LOCK as LOCK;

    const HERE: [u8; 4] = [127, 0, 0, 1];

    fn socket(link: &Arc<StackLink>, raw: bool) -> ServedIcmpSocket {
        let s = ServedIcmpSocket::new(link.clone(), raw).unwrap();
        FileLike::set_flags(&s, OpenFlags::NON_BLOCK).unwrap();
        s
    }

    /// A destination, as `sendto` gives it: no port.
    fn to(addr: [u8; 4]) -> Option<Endpoint> {
        Some(Endpoint::Ip(at(addr, 0)))
    }

    fn recv(s: &ServedIcmpSocket, buf: &mut [u8]) -> (SysResult, IpEndpoint) {
        let (n, from) = async_std::task::block_on(Socket::read(s, buf));
        (n, ip(from))
    }

    /// What `recv` reads next, and who sent it.
    fn next(s: &ServedIcmpSocket) -> Option<(Vec<u8>, IpEndpoint)> {
        let mut buf = [0u8; 2048];
        match recv(s, &mut buf) {
            (Ok(n), from) => Some((buf[..n].to_vec(), from)),
            (Err(LxError::EAGAIN), _) => None,
            (Err(e), _) => panic!("{:?}", e),
        }
    }

    /// An ICMP message of this type with this identifier, sequence 1,
    /// and its checksum right.
    fn icmp(kind: u8, ident: u16, data: &[u8]) -> Vec<u8> {
        let mut m = vec![kind, 0, 0, 0];
        m.extend_from_slice(&ident.to_be_bytes());
        m.extend_from_slice(&[0, 1]);
        m.extend_from_slice(data);
        seal(&mut m);
        m
    }

    /// The stack heard `message` from HOST.
    fn hears(stack: &Stack, message: &[u8]) {
        let from = Ipv4Address(HOST);
        let packet = packet(from, Ipv4Address(STACK_ADDR), 57, message);
        stack.arrives(HEARD, HOST, 0, &packet);
    }

    #[test]
    fn what_was_not_delivered_is_the_error_linux_says() {
        use LxError::*;
        // `icmp_err_convert`, code by code, and whether it mends.
        let table = [
            (ENETUNREACH, false),
            (EHOSTUNREACH, false),
            (ENOPROTOOPT, true),
            (ECONNREFUSED, true),
            (EMSGSIZE, true),
            (EOPNOTSUPP, false),
            (ENETUNREACH, true),
            (EHOSTUNREACH, true),
            (ENETUNREACH, true),
            (ENETUNREACH, true),
            (EHOSTUNREACH, true),
            (ENETUNREACH, false),
            (EHOSTUNREACH, false),
            (EHOSTUNREACH, true),
            (EHOSTUNREACH, true),
            (EHOSTUNREACH, true),
        ];
        for (code, said) in table.iter().enumerate() {
            assert_eq!(
                undelivered_error(3, code as u8),
                Some(*said),
                "code {}",
                code
            );
        }
        assert_eq!(undelivered_error(3, 16), Some((EHOSTUNREACH, false)));
        // Out of hops, in transit or in pieces.
        assert_eq!(undelivered_error(11, 0), Some((EHOSTUNREACH, false)));
        assert_eq!(undelivered_error(11, 1), Some((EHOSTUNREACH, false)));
        // An echo, its answer, a redirect: about nothing that was sent.
        for kind in [0, 5, 8, 12] {
            assert_eq!(undelivered_error(kind, 0), None, "type {}", kind);
        }
        assert_eq!(connect_error(&[3, 1]), Some(EHOSTUNREACH));
        assert_eq!(connect_error(&[3, 3]), Some(ECONNREFUSED));
        assert_eq!(connect_error(&[]), None);
        assert_eq!(connect_error(&[3, 1, 0]), None);
        assert_eq!(connect_error(&[8, 0]), None);
    }

    #[test]
    fn the_checksum_is_the_internets() {
        // RFC 1071's own example, and an odd length.
        let words = [0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
        assert_eq!(checksum(&words), !0xddf2);
        assert_eq!(checksum(&[0x12, 0x34, 0x56]), !(0x1234 + 0x5600));
        let m = icmp(ECHO_REQUEST, 7, b"odd");
        assert_eq!(checksum(&m), 0);
        let p = packet(Ipv4Address(HOST), Ipv4Address(STACK_ADDR), 64, &m);
        assert_eq!(checksum(&p[..IP_HEADER]), 0);
        assert_eq!(p.len(), IP_HEADER + m.len());
        assert_eq!(message_of(&p), Some(&m[..]));
        // What is no IPv4 packet of ICMP carries no message.
        assert_eq!(message_of(&[]), None);
        assert_eq!(message_of(&p[..IP_HEADER - 1]), None);
        let mut udp = p.clone();
        udp[9] = 17;
        assert_eq!(message_of(&udp), None);
        let mut six = p;
        six[0] = 0x65;
        assert_eq!(message_of(&six), None);
    }

    #[test]
    fn a_raw_socket_sends_its_messages_as_they_are_and_reads_whole_packets() {
        let _g = LOCK.lock();
        let stack = stack();
        let s = socket(&stack.link, true);
        assert_eq!(Socket::socket_type(&s), Some(SocketType::SOCK_RAW));
        assert_eq!(Socket::so_protocol(&s), Some(1));
        // The stack is told the socket hears before it says anything.
        assert_eq!(stack.told(), Some(hear_message(s.sock)));
        assert_eq!(Socket::write(&s, b"x", None), Err(LxError::ENOTCONN));
        // The message is the program's: its identifier, its checksum,
        // whatever its type.
        let ping = icmp(ECHO_REQUEST, 0x4242, b"anybody?");
        assert_eq!(Socket::write(&s, &ping, to(HOST)), Ok(ping.len()));
        assert_eq!(stack.told(), Some(icmp_message(s.sock, HOST, 64, &ping)));
        let odd = [13u8, 0, 0xde, 0xad];
        assert_eq!(Socket::write(&s, &odd, to(HOST)), Ok(4));
        assert_eq!(stack.told(), Some(icmp_message(s.sock, HOST, 64, &odd)));
        // `IP_TTL` is how far it goes.
        assert_eq!(Socket::setsockopt(&s, 0, 2, &3u32.to_ne_bytes()), Ok(0));
        assert_eq!(Socket::ip_ttl(&s), 3);
        assert_eq!(Socket::write(&s, &ping, to(HOST)), Ok(ping.len()));
        assert_eq!(stack.told(), Some(icmp_message(s.sock, HOST, 3, &ping)));
        // And `IP_MULTICAST_TTL` to a group: the next machine, unless
        // the program says further.
        let everybody = [224, 0, 0, 1];
        assert_eq!(Socket::write(&s, &ping, to(everybody)), Ok(ping.len()));
        let said = icmp_message(s.sock, everybody, 1, &ping);
        assert_eq!(stack.told(), Some(said));
        assert_eq!(Socket::setsockopt(&s, 0, 33, &[9]), Ok(0));
        assert_eq!(Socket::write(&s, &ping, to(everybody)), Ok(ping.len()));
        let said = icmp_message(s.sock, everybody, 9, &ping);
        assert_eq!(stack.told(), Some(said));
        // One frame's worth and no more, and nobody is everybody by
        // accident.
        let long = vec![0u8; MAX_FOR_TESTS + ECHO_HEADER + 1];
        assert_eq!(Socket::write(&s, &long, to(HOST)), Err(LxError::EMSGSIZE));
        assert_eq!(
            Socket::write(&s, &long[..long.len() - 1], to(HOST)),
            Ok(long.len() - 1)
        );
        assert!(stack.told().is_some());
        assert_eq!(Socket::write(&s, &ping, to([255; 4])), Err(LxError::EACCES));
        assert_eq!(stack.told(), None);

        // What the stack hears is read whole, header first, whoever it
        // is for and whatever it is.
        assert_eq!(Socket::poll(&s, PollEvents::IN), (false, true, false));
        let pong = icmp(ECHO_REPLY, 0x4242, b"anybody?");
        hears(&stack, &pong);
        hears(&stack, &icmp(ECHO_REPLY, 0x9999, b"not yours"));
        assert_eq!(Socket::poll(&s, PollEvents::IN), (true, true, false));
        assert_eq!(FileLike::readable_bytes(&s), Some(IP_HEADER + pong.len()));
        let (packet, from) = next(&s).unwrap();
        assert_eq!(from, at(HOST, 0));
        assert_eq!(packet[8], 57);
        assert_eq!(message_of(&packet), Some(&pong[..]));
        let (packet, _) = next(&s).unwrap();
        assert_eq!(echo_reply_ident(message_of(&packet).unwrap()), Some(0x9999));
        assert_eq!(next(&s), None);

        // The header is the stack's to write.
        assert_eq!(Socket::setsockopt(&s, 0, 3, &0u32.to_ne_bytes()), Ok(0));
        assert_eq!(
            Socket::setsockopt(&s, 0, 3, &1u32.to_ne_bytes()),
            Err(LxError::EOPNOTSUPP)
        );
        assert!(!Socket::ip_hdrincl(&s));

        let closed = s.sock;
        drop(s);
        assert_eq!(stack.told(), Some(close_message(closed)));
        // And what is heard after that is nobody's.
        hears(&stack, &pong);
    }

    #[test]
    fn a_socket_that_pings_sends_echoes_as_its_own_and_reads_their_answers() {
        let _g = LOCK.lock();
        let stack = stack();
        let s = socket(&stack.link, false);
        let other = socket(&stack.link, false);
        assert_eq!(Socket::socket_type(&s), Some(SocketType::SOCK_DGRAM));
        assert_eq!(stack.told(), Some(hear_message(s.sock)));
        assert_eq!(stack.told(), Some(hear_message(other.sock)));
        // An echo and nothing else (`ping_common_sendmsg`).
        assert_eq!(
            Socket::write(&s, &[8, 0, 0], to(HOST)),
            Err(LxError::EINVAL)
        );
        let reply = icmp(ECHO_REPLY, 1, b"");
        assert_eq!(Socket::write(&s, &reply, to(HOST)), Err(LxError::EINVAL));
        assert_eq!(stack.told(), None);
        assert_eq!(ip(Socket::endpoint(&s).unwrap()).port, 0);

        // It leaves with the socket's identifier, which is its port, and
        // the checksum that goes with that.
        let ping = icmp(ECHO_REQUEST, 0x4242, b"anybody?");
        assert_eq!(Socket::write(&s, &ping, to(HOST)), Ok(ping.len()));
        let ident = ip(Socket::endpoint(&s).unwrap()).port;
        assert_ne!(ident, 0);
        let sent = icmp(ECHO_REQUEST, ident, b"anybody?");
        assert_eq!(stack.told(), Some(icmp_message(s.sock, HOST, 64, &sent)));
        // Two sockets never ping as one.
        assert_eq!(Socket::write(&other, &ping, to(HOST)), Ok(ping.len()));
        let theirs = ip(Socket::endpoint(&other).unwrap()).port;
        assert_ne!(theirs, ident);
        assert!(stack.told().is_some());

        // The answer to its echo is its own, without the header; the
        // answer to another's is not, nor is anything that is no answer.
        hears(&stack, &icmp(ECHO_REPLY, theirs, b"for the other"));
        hears(
            &stack,
            &icmp(ECHO_REQUEST, ident, b"an echo, not an answer"),
        );
        hears(&stack, &icmp(3, ident, b"an error"));
        let pong = icmp(ECHO_REPLY, ident, b"anybody?");
        hears(&stack, &pong);
        assert_eq!(next(&s), Some((pong, at(HOST, 0))));
        assert_eq!(next(&s), None);
        assert_eq!(next(&other).map(|(m, _)| m.len()), Some(8 + 13));

        // Bound, it pings as what it was bound to, once.
        let bound = socket(&stack.link, false);
        assert_eq!(Socket::bind(&bound, any(theirs)), Err(LxError::EADDRINUSE));
        assert_eq!(Socket::bind(&bound, any(777)), Ok(0));
        assert_eq!(Socket::bind(&bound, any(778)), Err(LxError::EINVAL));
        assert_eq!(
            Socket::bind(&socket(&stack.link, false), Endpoint::Ip(at(HOST, 0))),
            Err(LxError::EADDRNOTAVAIL)
        );
        while stack.told().is_some() {}
        assert_eq!(Socket::write(&bound, &ping, to(HOST)), Ok(ping.len()));
        let sent = icmp(ECHO_REQUEST, 777, b"anybody?");
        assert_eq!(
            stack.told(),
            Some(icmp_message(bound.sock, HOST, 64, &sent))
        );
        // And its identifier is free when it is gone.
        drop(bound);
        assert_eq!(Socket::bind(&socket(&stack.link, false), any(777)), Ok(0));
        // Not an option of this kind of socket.
        assert_eq!(
            Socket::setsockopt(&s, 0, 3, &0u32.to_ne_bytes()),
            Err(LxError::ENOPROTOOPT)
        );
    }

    #[test]
    fn a_connected_socket_pings_its_peer_and_hears_nobody_else() {
        let _g = LOCK.lock();
        let stack = stack();
        let s = socket(&stack.link, false);
        let connect = |to| async_std::task::block_on(Socket::connect(&s, Endpoint::Ip(to)));
        assert_eq!(connect(at(HOST, 0)), Ok(0));
        assert_eq!(Socket::remote_endpoint(&s).map(ip), Some(at(HOST, 0)));
        let ident = ip(Socket::endpoint(&s).unwrap()).port;
        assert_ne!(ident, 0);
        while stack.told().is_some() {}
        let ping = icmp(ECHO_REQUEST, 0, b"hi");
        assert_eq!(FileLike::write(&s, &ping), Ok(ping.len()));
        let sent = icmp(ECHO_REQUEST, ident, b"hi");
        assert_eq!(stack.told(), Some(icmp_message(s.sock, HOST, 64, &sent)));
        let pong = icmp(ECHO_REPLY, ident, b"hi");
        let stranger = [10, 0, 9, 77];
        let lie = packet(Ipv4Address(stranger), Ipv4Address(STACK_ADDR), 64, &pong);
        stack.arrives(HEARD, stranger, 0, &lie);
        hears(&stack, &pong);
        assert_eq!(next(&s), Some((pong, at(HOST, 0))));
        assert_eq!(next(&s), None);
    }

    #[test]
    fn an_echo_to_this_machine_is_answered_here_with_or_without_a_stack() {
        let _g = LOCK.lock();
        let link = StackLink::new();
        let raw = socket(&link, true);
        let pings = socket(&link, false);
        let ping = icmp(ECHO_REQUEST, 0x4242, b"me?");
        // No stack: what would leave the machine has nowhere to go.
        assert_eq!(Socket::write(&raw, &ping, to(HOST)), Err(LxError::ENETDOWN));
        // What stays in it is heard and answered. A raw socket hears the
        // echo and its answer, each a whole packet.
        assert_eq!(Socket::write(&raw, &ping, to(HERE)), Ok(ping.len()));
        let (asked, from) = next(&raw).unwrap();
        assert_eq!(from, at(HERE, 0));
        assert_eq!(checksum(&asked[..IP_HEADER]), 0);
        assert_eq!(message_of(&asked), Some(&ping[..]));
        let (answered, from) = next(&raw).unwrap();
        assert_eq!(from, at(HERE, 0));
        assert_eq!(
            (&answered[12..16], &answered[16..20]),
            (&HERE[..], &HERE[..])
        );
        let pong = icmp(ECHO_REPLY, 0x4242, b"me?");
        assert_eq!(message_of(&answered), Some(&pong[..]));
        assert_eq!(next(&raw), None);
        // The socket that pings heard neither: they were not its own.
        assert_eq!(next(&pings), None);
        // Nowhere is here too, and the socket that pings reads its answer.
        assert_eq!(Socket::write(&pings, &ping, to([0; 4])), Ok(ping.len()));
        let ident = ip(Socket::endpoint(&pings).unwrap()).port;
        let pong = icmp(ECHO_REPLY, ident, b"me?");
        assert_eq!(next(&pings), Some((pong, at(HERE, 0))));
        assert_eq!(next(&raw).map(|(p, _)| p[IP_HEADER]), Some(ECHO_REQUEST));
        assert_eq!(next(&raw).map(|(p, _)| p[IP_HEADER]), Some(ECHO_REPLY));
        // An echo with a wrong checksum is heard and not answered, and
        // what is no echo has no answer.
        let mut wrong = ping.clone();
        wrong[2] ^= 0xff;
        assert_eq!(Socket::write(&raw, &wrong, to(HERE)), Ok(wrong.len()));
        assert_eq!(Socket::write(&raw, &[13, 0, 0, 0], to(HERE)), Ok(4));
        assert_eq!(
            next(&raw).map(|(p, _)| p.len()),
            Some(IP_HEADER + wrong.len())
        );
        assert_eq!(next(&raw).map(|(p, _)| p.len()), Some(IP_HEADER + 4));
        assert_eq!(next(&raw), None);

        // The stack that comes is told of both, and its own address is
        // this machine from then on.
        let stack = stack_of(&link);
        assert_eq!(stack.told(), Some(hear_message(raw.sock)));
        assert_eq!(stack.told(), Some(hear_message(pings.sock)));
        assert_eq!(stack.told(), None);
        assert_eq!(Socket::write(&pings, &ping, to(STACK_ADDR)), Ok(ping.len()));
        assert_eq!(stack.told(), None);
        let pong = icmp(ECHO_REPLY, ident, b"me?");
        assert_eq!(next(&pings), Some((pong, at(STACK_ADDR, 0))));
        assert_eq!(Socket::write(&pings, &ping, to(HOST)), Ok(ping.len()));
        assert!(stack.told().is_some());
    }

    #[test]
    fn a_read_waits_as_long_as_so_rcvtimeo_says() {
        let _g = LOCK.lock();
        let stack = stack();
        let s = ServedIcmpSocket::new(stack.link.clone(), true).unwrap();
        let mut tv = [0u8; 16];
        tv[8..].copy_from_slice(&30_000i64.to_ne_bytes());
        assert_eq!(Socket::setsockopt(&s, 1, 20, &tv), Ok(0));
        assert_eq!(Socket::so_rcvtimeo(&s), tv);
        let before = kernel_hal::timer::timer_now();
        let mut buf = [0u8; 64];
        assert_eq!(recv(&s, &mut buf).0, Err(LxError::EAGAIN));
        let took = kernel_hal::timer::timer_now() - before;
        assert!(took >= core::time::Duration::from_millis(30), "{:?}", took);
    }
}
