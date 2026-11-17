//! Raw sockets of IPv4 whose stack is a process, of any protocol but
//! ICMP: what a program opens with `socket(AF_INET, SOCK_RAW, protocol)`
//! to speak a protocol of its own, or to write whole packets. The ones
//! of ICMP are `ping`'s ([`super::icmp`]). What the kernel and the stack
//! say to each other is in the module above.
//!
//! Such a socket writes what goes behind an IPv4 header the kernel
//! writes for it, or the header too (`IP_HDRINCL`, and always for
//! `IPPROTO_RAW`), and reads every IPv4 packet of its protocol the
//! machine gets, header and all. The stack goes on doing with those
//! packets what it did: a raw socket of UDP sees the datagrams, and the
//! socket that has their port still gets them.
//!
//! A packet a program made leaves as Linux would send it (`raw_send_hdrinc`):
//! its length is the one written, one from nowhere is from this machine,
//! and its sum is done again. The stack's header is twenty bytes and
//! never a fragment, so options and the identification do not travel.
//!
//! The same socket of `AF_INET6` is in [`super::raw6`]. Not here: a
//! socket bound to one address hearing only what is for it, and a packet for this machine
//! reaching anything but the raw sockets of its protocol.

use super::*;
use core::sync::atomic::AtomicBool;

/// ICMP, whose raw sockets are [`super::icmp`]'s.
pub(super) const ICMP: u8 = 1;
/// `IPPROTO_RAW`: a socket that only sends, and whose program writes
/// the header.
const IPPROTO_RAW: u8 = 255;
/// The IPv4 header a program's bytes travel behind.
const IP_HEADER: usize = 20;
/// What a datagram's headers take of a frame: the stack says the
/// longest datagram it carries, and a packet is that and both.
const DATAGRAM_HEADERS: usize = IP_HEADER + 8;

/// The protocol `packet` carries, when it is an IPv4 packet.
pub(super) fn protocol_of(packet: &[u8]) -> Option<u8> {
    (packet.len() >= IP_HEADER && packet[0] >> 4 == 4).then(|| packet[9])
}

/// Write the sum of the IPv4 header `packet` starts with.
fn seal(packet: &mut [u8]) {
    let header = usize::from(packet[0] & 0x0f) * 4;
    packet[10..12].copy_from_slice(&[0, 0]);
    let sum = icmp::checksum(&packet[..header]);
    packet[10..12].copy_from_slice(&sum.to_be_bytes());
}

/// The IPv4 packet that carries `data`, of `protocol`, from `from` to
/// `to`.
fn packet(from: Ipv4Address, to: Ipv4Address, protocol: u8, hop_limit: u8, data: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(IP_HEADER + data.len());
    packet.extend_from_slice(&[0x45, 0]);
    packet.extend_from_slice(&((IP_HEADER + data.len()) as u16).to_be_bytes());
    // No identification, never a fragment.
    packet.extend_from_slice(&[0, 0, 0x40, 0]);
    packet.extend_from_slice(&[hop_limit, protocol, 0, 0]);
    packet.extend_from_slice(&from.0);
    packet.extend_from_slice(&to.0);
    seal(&mut packet);
    packet.extend_from_slice(data);
    packet
}

/// The packet a program made, header and all, as it leaves
/// (`raw_send_hdrinc`): what is no IPv4 header is `EINVAL`, and so is a
/// packet with no hops to travel, which no stack carries.
fn included(data: &[u8]) -> LxResult<Vec<u8>> {
    if data.len() < IP_HEADER || data[0] >> 4 != 4 {
        return Err(LxError::EINVAL);
    }
    let header = usize::from(data[0] & 0x0f) * 4;
    if header < IP_HEADER || header > data.len() || data[8] == 0 {
        return Err(LxError::EINVAL);
    }
    let mut packet = data.to_vec();
    packet[2..4].copy_from_slice(&(data.len() as u16).to_be_bytes());
    Ok(packet)
}

/// A raw socket of IPv4, of a protocol that is not ICMP, whose stack is
/// a process.
pub struct ServedRawSocket {
    base: KObjectBase,
    pub(super) link: Arc<StackLink>,
    pub(super) sock: u64,
    /// `shared.v6` for a socket of `AF_INET6`.
    pub(super) shared: Arc<Shared>,
    pub(super) protocol: u8,
    /// `IP_HDRINCL`: the program writes the header.
    header_included: AtomicBool,
    /// The options a program sets and asks for again. `IP_TTL` is the
    /// one that changes what leaves.
    opts: Mutex<StoredInetOpts>,
    /// And the ones of a socket of `AF_INET6`.
    pub(super) six: Mutex<raw6::Opts>,
}

impl ServedRawSocket {
    /// A raw socket of `link` that speaks `protocol`.
    pub fn new(link: Arc<StackLink>, protocol: u8) -> LxResult<Self> {
        debug_assert_ne!(protocol, ICMP);
        Self::open(link, protocol, false)
    }

    /// The same of `AF_INET6`, where the protocol is not ICMPv6.
    pub fn new6(link: Arc<StackLink>, protocol: u8) -> LxResult<Self> {
        debug_assert_ne!(protocol, raw6::ICMPV6);
        Self::open(link, protocol, true)
    }

    fn open(link: Arc<StackLink>, protocol: u8, six: bool) -> LxResult<Self> {
        let shared = Shared::new(Hears::Packets(protocol), six);
        let sock = {
            let mut table = link.table.lock();
            table.next_sock += 1;
            let sock = table.next_sock;
            table.socks.insert(sock, Arc::downgrade(&shared));
            sock
        };
        link.hear(sock, shared.hears, six);
        Ok(ServedRawSocket {
            base: KObjectBase::new(),
            link,
            sock,
            shared,
            protocol,
            header_included: AtomicBool::new(protocol == IPPROTO_RAW),
            opts: Mutex::new(StoredInetOpts::default()),
            six: Mutex::new(raw6::Opts::default()),
        })
    }

    pub(super) fn is_six(&self) -> bool {
        self.shared.v6
    }
}

impl Drop for ServedRawSocket {
    fn drop(&mut self) {
        self.link.deaf(self.sock, None);
    }
}

#[async_trait]
impl Socket for ServedRawSocket {
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
        // The length of an IPv4 packet is 16 bits, header and all.
        if data.len() > usize::from(u16::MAX) - IP_HEADER {
            return Err(LxError::EMSGSIZE);
        }
        let to = match sendto_endpoint {
            Some(Endpoint::Ip(to)) => to,
            Some(_) => return Err(LxError::EAFNOSUPPORT),
            None => {
                let remote = self.shared.inner.lock().remote;
                remote.ok_or(LxError::EDESTADDRREQ)?
            }
        };
        let mut packet = if self.header_included.load(Ordering::Relaxed) {
            included(data)?
        } else {
            let to = icmp::destination(to.addr)?;
            let opts = self.opts.lock();
            // To a group, as far as a group's are to go (`ip_select_ttl`).
            let ttl = if to.is_multicast() {
                opts.mcast_ttl
            } else {
                opts.ip_ttl
            };
            let nowhere = Ipv4Address::UNSPECIFIED;
            packet(nowhere, to, self.protocol, ttl.clamp(1, 255) as u8, data)
        };
        // Where the header says, which is where it goes.
        let to = Ipv4Address::from_bytes(&packet[16..20]);
        let here = self.link.is_here(to);
        if here && packet[12..16] == [0; 4] {
            // From nowhere is from this machine: to itself from the
            // address it was called by. To anybody else it is from the
            // card it leaves by, which is the stack's to say: it may have
            // several, and writes the address there.
            let from = self.link.here_for(to);
            packet[12..16].copy_from_slice(&from.0);
        }
        seal(&mut packet);
        if here {
            // Neither a frame nor the stack's: `lo` counts it, and the
            // raw sockets of its protocol hear it.
            crate::net::count_on_lo(packet.len() - IP_HEADER, IP_HEADER);
            let from = Ipv4Address::from_bytes(&packet[12..16]);
            self.link.heard(from, &packet);
            return Ok(data.len());
        }
        // `ip_route_output`: everybody is not written to by accident.
        if to.is_broadcast() && !self.opts.lock().broadcast {
            return Err(LxError::EACCES);
        }
        // Nothing leaves the machine while no stack serves.
        if self.link.is_down() {
            return Err(self.link.down());
        }
        // The stack does not fragment.
        if packet.len() > self.link.max_datagram.load(Ordering::Acquire) + DATAGRAM_HEADERS {
            return Err(LxError::EMSGSIZE);
        }
        self.link.tell(packet_message(self.sock, &packet), false)?;
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
        let peer = icmp::destination(peer.addr)?;
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
        // A raw socket has no port, and a `sockaddr`'s is not looked at.
        let addr = icmp::v4(local.addr)?;
        if !addr.is_unspecified() && !self.link.is_here(addr) {
            return Err(LxError::EADDRNOTAVAIL);
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
        let local = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED), 0);
        Some(Endpoint::Ip(shown(Takes::of(self.is_six(), false), local)))
    }

    fn remote_endpoint(&self) -> Option<Endpoint> {
        self.shared.inner.lock().remote.map(Endpoint::Ip)
    }

    fn is_inet(&self) -> bool {
        true
    }

    fn is_raw_ipv4(&self) -> bool {
        !self.is_six()
    }

    fn ip_hdrincl(&self) -> bool {
        self.header_included.load(Ordering::Relaxed)
    }

    fn so_domain(&self) -> Option<u32> {
        Some(if self.is_six() {
            Domain::AF_INET6
        } else {
            Domain::AF_INET
        } as u32)
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
            let hops = self.shared.inner.lock().last_hops;
            self.six.lock().hops.cmsgs(hops)
        } else {
            Vec::new()
        }
    }

    fn so_protocol(&self) -> Option<u32> {
        Some(u32::from(self.protocol))
    }

    fn socket_type(&self) -> Option<SocketType> {
        Some(SocketType::SOCK_RAW)
    }

    fn take_msg_flags(&self) -> i32 {
        core::mem::take(&mut self.shared.inner.lock().last_msg_flags)
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
            check_setsockopt_len(level, opt, data)?;
            let on = data.iter().take(4).any(|b| *b != 0);
            self.header_included.store(on, Ordering::Relaxed);
            return Ok(0);
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

impl_kobject!(ServedRawSocket);

#[async_trait]
impl FileLike for ServedRawSocket {
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
    use super::super::tests::{at, ip, stack, HOST, MAX_FOR_TESTS, STACK_ADDR};
    use super::*;
    use crate::net::NET_TEST_LOCK as LOCK;

    const UDP: u8 = 17;
    const HERE: [u8; 4] = [127, 0, 0, 1];

    fn socket(link: &Arc<StackLink>, protocol: u8) -> ServedRawSocket {
        let s = ServedRawSocket::new(link.clone(), protocol).unwrap();
        FileLike::set_flags(&s, OpenFlags::NON_BLOCK).unwrap();
        s
    }

    /// A destination, as `sendto` gives it: no port.
    fn to(addr: [u8; 4]) -> Option<Endpoint> {
        Some(Endpoint::Ip(at(addr, 0)))
    }

    /// What `recv` reads next, and who sent it.
    fn next(s: &ServedRawSocket) -> Option<(Vec<u8>, IpEndpoint)> {
        let mut buf = [0u8; 2048];
        match async_std::task::block_on(Socket::read(s, &mut buf)) {
            (Ok(n), from) => Some((buf[..n].to_vec(), ip(from))),
            (Err(LxError::EAGAIN), _) => None,
            (Err(e), _) => panic!("{:?}", e),
        }
    }

    fn hdrincl(s: &ServedRawSocket, on: bool) -> SysResult {
        s.setsockopt(0, 3, &u32::from(on).to_ne_bytes())
    }

    #[test]
    fn the_messages_are_laid_out_as_the_stack_reads_them() {
        // The same bytes `tools/eclipse-netstack/src/socks/raw.rs` reads,
        // written out: neither side can move a field alone.
        assert_eq!(
            raw_message(7, 17),
            [0, 0, 0, 0, 29, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 17, 0, 0, 0]
        );
        assert_eq!(
            packet_message(7, &[0x45, 0]),
            [0, 0, 0, 0, 30, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0x45, 0]
        );
    }

    #[test]
    fn what_a_raw_socket_writes_leaves_behind_a_header_of_the_kernels() {
        let _lock = LOCK.lock();
        let stack = stack();
        let s = socket(&stack.link, UDP);
        assert_eq!(stack.told(), Some(raw_message(s.sock, UDP)));
        // Nowhere to send it yet.
        assert_eq!(
            Socket::write(&s, b"12345678", None),
            Err(LxError::EDESTADDRREQ)
        );
        assert_eq!(s.setsockopt(0, 2, &9u32.to_ne_bytes()), Ok(0));
        assert_eq!(Socket::write(&s, b"12345678", to(HOST)), Ok(8));
        let said = stack.told().unwrap();
        assert_eq!(parse_header(&said), Some((OP_PACKET, s.sock)));
        let sent = &said[HEADER_LEN..];
        assert_eq!(sent.len(), IP_HEADER + 8);
        assert_eq!(sent[..2], [0x45, 0]);
        assert_eq!(sent[2..4], 28u16.to_be_bytes());
        // Nine hops, of UDP, to the host, from nowhere: the stack
        // writes the address of the card it leaves by.
        assert_eq!(sent[8..10], [9, UDP]);
        assert_eq!(icmp::checksum(&sent[..IP_HEADER]), 0);
        assert_eq!((&sent[12..16], &sent[16..20]), (&[0u8; 4][..], &HOST[..]));
        assert_eq!(&sent[IP_HEADER..], b"12345678");
        // Connected, a write needs no address; the longest is what one
        // frame carries, and the stack does not cut a packet.
        assert_eq!(
            async_std::task::block_on(s.connect(Endpoint::Ip(at(HOST, 0)))),
            Ok(0)
        );
        let room = MAX_FOR_TESTS + 8;
        assert_eq!(Socket::write(&s, &vec![0; room], None), Ok(room));
        assert!(stack.told().is_some());
        assert_eq!(
            Socket::write(&s, &vec![0; room + 1], None),
            Err(LxError::EMSGSIZE)
        );
        assert_eq!(stack.told(), None);
        drop(s);
        assert!(matches!(
            stack.told().map(|m| parse_header(&m)),
            Some(Some((OP_CLOSE, _)))
        ));
    }

    #[test]
    fn a_header_the_program_wrote_leaves_as_linux_sends_it() {
        let _lock = LOCK.lock();
        let stack = stack();
        // `IPPROTO_RAW` writes its header without being asked to.
        let s = socket(&stack.link, IPPROTO_RAW);
        assert!(s.ip_hdrincl());
        let _ = stack.told();
        // From nowhere, a length that is wrong, no sum: a packet of UDP.
        let mut made = vec![0x45, 0, 0, 0, 0x12, 0x34, 0, 0, 7, UDP, 0, 0];
        made.extend_from_slice(&[0; 4]);
        made.extend_from_slice(&HOST);
        made.extend_from_slice(b"12345678");
        // Where `sendto` says is not looked at: the header says.
        assert_eq!(Socket::write(&s, &made, to([192, 0, 2, 1])), Ok(made.len()));
        let said = stack.told().unwrap();
        assert_eq!(parse_header(&said), Some((OP_PACKET, s.sock)));
        let sent = &said[HEADER_LEN..];
        assert_eq!(sent[2..4], 28u16.to_be_bytes());
        assert_eq!(sent[4..6], [0x12, 0x34]);
        assert_eq!(sent[8..10], [7, UDP]);
        assert_eq!(icmp::checksum(&sent[..IP_HEADER]), 0);
        assert_eq!((&sent[12..16], &sent[16..20]), (&[0u8; 4][..], &HOST[..]));
        assert_eq!(&sent[IP_HEADER..], b"12345678");
        // What is no header is refused, and nothing is said.
        assert_eq!(Socket::write(&s, &[], to(HOST)), Err(LxError::EINVAL));
        assert_eq!(
            Socket::write(&s, &made[..19], to(HOST)),
            Err(LxError::EINVAL)
        );
        let mut six = made.clone();
        six[0] = 0x65;
        assert_eq!(Socket::write(&s, &six, to(HOST)), Err(LxError::EINVAL));
        let mut long = made.clone();
        long[0] = 0x4f;
        assert_eq!(Socket::write(&s, &long, to(HOST)), Err(LxError::EINVAL));
        let mut spent = made.clone();
        spent[8] = 0;
        assert_eq!(Socket::write(&s, &spent, to(HOST)), Err(LxError::EINVAL));
        assert_eq!(stack.told(), None);
        // A socket of a protocol writes the header when it says so, and
        // stops when it says so.
        let udp = socket(&stack.link, UDP);
        let _ = stack.told();
        assert!(!udp.ip_hdrincl());
        assert_eq!(hdrincl(&udp, true), Ok(0));
        assert!(udp.ip_hdrincl());
        assert_eq!(
            Socket::write(&udp, b"short", to(HOST)),
            Err(LxError::EINVAL)
        );
        assert_eq!(Socket::write(&udp, &made, to(HOST)), Ok(made.len()));
        assert_eq!(stack.told().unwrap().len(), HEADER_LEN + made.len());
        assert_eq!(hdrincl(&udp, false), Ok(0));
        assert_eq!(Socket::write(&udp, b"short", to(HOST)), Ok(5));
    }

    #[test]
    fn a_raw_socket_hears_the_packets_of_its_protocol_and_no_other() {
        let _lock = LOCK.lock();
        let stack = stack();
        let udp = socket(&stack.link, UDP);
        let other = socket(&stack.link, 89);
        let mute = socket(&stack.link, IPPROTO_RAW);
        let ping = ServedIcmpSocket::new(stack.link.clone(), true).unwrap();
        FileLike::set_flags(&ping, OpenFlags::NON_BLOCK).unwrap();
        let (host, us) = (Ipv4Address(HOST), Ipv4Address(STACK_ADDR));
        let datagram = packet(host, us, UDP, 57, b"12345678");
        stack.arrives(HEARD, HOST, 0, &datagram);
        assert_eq!(next(&udp), Some((datagram.clone(), at(HOST, 0))));
        assert_eq!(next(&other), None);
        assert_eq!(next(&mute), None);
        // Nor does the raw socket of ICMP, which heard everything that
        // came this way while only ICMP did.
        let mut buf = [0u8; 64];
        let (n, _) = async_std::task::block_on(Socket::read(&ping, &mut buf));
        assert_eq!(n, Err(LxError::EAGAIN));
        // And what is heard of ICMP is not a raw socket's of UDP.
        stack.arrives(
            HEARD,
            HOST,
            0,
            &packet(host, us, ICMP, 57, &[0, 0, 0xff, 0xff]),
        );
        assert_eq!(next(&udp), None);
        let (n, _) = async_std::task::block_on(Socket::read(&ping, &mut buf));
        assert_eq!(n, Ok(IP_HEADER + 4));
        // A connected socket hears its peer and nobody else.
        assert_eq!(
            async_std::task::block_on(udp.connect(Endpoint::Ip(at(HOST, 0)))),
            Ok(0)
        );
        let stranger = [10, 0, 9, 77];
        stack.arrives(
            HEARD,
            stranger,
            0,
            &packet(Ipv4Address(stranger), us, UDP, 57, b"x"),
        );
        stack.arrives(HEARD, HOST, 0, &datagram);
        assert_eq!(next(&udp), Some((datagram, at(HOST, 0))));
        assert_eq!(next(&udp), None);
    }

    #[test]
    fn a_packet_for_this_machine_is_heard_here_and_never_leaves() {
        let _lock = LOCK.lock();
        let stack = stack();
        let s = socket(&stack.link, UDP);
        let listener = socket(&stack.link, UDP);
        let _ = (stack.told(), stack.told());
        for (to_addr, from_addr) in [(HERE, HERE), (STACK_ADDR, STACK_ADDR)] {
            assert_eq!(Socket::write(&s, b"12345678", to(to_addr)), Ok(8));
            assert_eq!(stack.told(), None);
            let (heard, from) = next(&listener).unwrap();
            assert_eq!(from, at(from_addr, 0));
            assert_eq!(heard[9], UDP);
            assert_eq!(icmp::checksum(&heard[..IP_HEADER]), 0);
            assert_eq!(
                (&heard[12..16], &heard[16..20]),
                (&from_addr[..], &to_addr[..])
            );
            assert_eq!(&heard[IP_HEADER..], b"12345678");
            // Who sent it speaks the protocol too.
            assert!(next(&s).is_some());
        }
        // It binds to this machine's addresses and no other.
        assert_eq!(s.bind(Endpoint::Ip(at(STACK_ADDR, 0))), Ok(0));
        assert_eq!(s.bind(Endpoint::Ip(at([0; 4], 0))), Ok(0));
        assert_eq!(
            s.bind(Endpoint::Ip(at(HOST, 0))),
            Err(LxError::EADDRNOTAVAIL)
        );
    }

    #[test]
    fn the_next_stack_is_told_of_the_raw_sockets_there_are() {
        let _lock = LOCK.lock();
        let first = stack();
        let s = socket(&first.link, UDP);
        let link = first.link.clone();
        drop(first);
        let next = super::super::tests::stack_of(&link);
        let mut said = Vec::new();
        while let Some(m) = next.told() {
            said.push(m);
        }
        assert!(said.contains(&raw_message(s.sock, UDP)));
    }
}
