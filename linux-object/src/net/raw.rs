use crate::{
    error::{LxError, LxResult},
    fs::{FileLike, OpenFlags, PollEvents, PollStatus},
    net::*,
};
use alloc::sync::Arc;
use async_trait::async_trait;
use lock::Mutex;
use smoltcp::{
    socket::{RawPacketMetadata, RawSocket, RawSocketBuffer},
    wire::{IpProtocol, IpVersion, Ipv4Address, Ipv4Packet, Ipv6Address, Ipv6Packet, Ipv6Repr},
};

#[allow(unused_imports)]
use zircon_object::object::*;

pub struct RawSocketState {
    base: KObjectBase,
    inner: Arc<RawSocketInner>,
}

#[derive(Debug)]
struct RawSocketInner {
    handle: GlobalSocketHandle,
    header_included: Mutex<bool>,
    flags: Mutex<OpenFlags>,
    remote: Mutex<Option<Endpoint>>,
    ipv6: bool,
}

impl RawSocketState {
    /// missing documentation
    pub fn new(protocol: u8, ipv6: bool) -> LxResult<Self> {
        let rx_buffer = RawSocketBuffer::new(
            vec![RawPacketMetadata::EMPTY; RAW_METADATA_BUF],
            vec![0; RAW_RECVBUF],
        );
        let tx_buffer = RawSocketBuffer::new(
            vec![RawPacketMetadata::EMPTY; RAW_METADATA_BUF],
            vec![0; RAW_SENDBUF],
        );
        let socket = RawSocket::new(
            if ipv6 {
                IpVersion::Ipv6
            } else {
                IpVersion::Ipv4
            },
            IpProtocol::from(protocol),
            rx_buffer,
            tx_buffer,
        );
        let handle = super::register_smoltcp_socket(socket)?;

        Ok(RawSocketState {
            base: KObjectBase::new(),
            inner: Arc::new(RawSocketInner {
                handle,
                header_included: Mutex::new(false),
                flags: Mutex::new(OpenFlags::RDWR),
                remote: Mutex::new(None),
                ipv6,
            }),
        })
    }

    /// `raw_send_hdrinc`: with `IP_HDRINCL` the caller supplies the IPv4
    /// header, so what it hands over has to BE one.
    ///
    /// Nothing checked this. smoltcp parses the buffer when the interface
    /// dispatches it, and the first thing it does is `IpVersion::of_packet`,
    /// which indexes byte 0 with no length check: a zero-length `write(2)` on
    /// a raw socket with `IP_HDRINCL` set enqueued an empty packet and
    /// **panicked the kernel** with an index out of bounds -- and not in the
    /// caller, but in whichever thread next polled the interface, because the
    /// empty packet waits in the TX ring until then.
    ///
    /// The three rules are Linux's: `length < sizeof(struct iphdr)` and
    /// `iph->ihl * 4 > length` both answer EINVAL, and the version nibble has
    /// to read 4 -- smoltcp refuses to dispatch a packet whose version does not
    /// match the socket's and drops it in silence, so without that last one
    /// `write` reported a success that sent nothing.
    ///
    /// The header's `protocol` field is deliberately NOT checked, even though
    /// smoltcp drops a mismatch just as silently. Linux does not check it
    /// either: with `IP_HDRINCL` the protocol byte is the caller's to write,
    /// and refusing one that differs from the socket's would be a rule of our
    /// own invention. So that one case still reports a success that sends
    /// nothing, and fixing it belongs in smoltcp's dispatch, not here.
    ///
    /// On a non-empty buffer the length rule and the `ihl` rule coincide (a
    /// header length under 20 is refused either way), so the length rule earns
    /// its keep on exactly one input: the empty buffer, the one that panicked.
    fn check_included_header(data: &[u8]) -> LxResult<()> {
        if data.len() < 20 {
            return Err(LxError::EINVAL);
        }
        if data[0] >> 4 != 4 {
            return Err(LxError::EINVAL);
        }
        let ihl = ((data[0] & 0x0f) as usize) * 4;
        if ihl < 20 || ihl > data.len() {
            return Err(LxError::EINVAL);
        }
        Ok(())
    }
}

/// missing in implementation
#[async_trait]
impl Socket for RawSocketState {
    async fn read(&self, data: &mut [u8]) -> (SysResult, Endpoint) {
        loop {
            drain_net_poll(4);
            if let Err(e) = crate::process::check_and_deliver_tty_interrupt() {
                return (Err(e), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
            }
            // Also honor non-TTY signals (SIGTERM/SIGALRM/...) so a blocked raw
            // recv returns EINTR, matching the icmp socket path.
            if let Err(e) = crate::process::check_signals() {
                return (Err(e), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
            }
            let remote = self.inner.remote.lock().as_ref().and_then(|ep| match ep {
                Endpoint::Ip(ip) => Some(ip.addr),
                _ => None,
            });
            if !self.inner.ipv6 {
                let proto = {
                    let net_sockets = get_sockets();
                    let mut sockets = net_sockets.lock();
                    let proto = sockets.get::<RawSocket>(self.inner.handle.0).ip_protocol();
                    proto
                };
                if proto == IpProtocol::Icmp {
                    if let Some((n, src)) = super::icmp_rx::pop_ipv4_raw_reply(remote, data) {
                        return (Ok(n), Endpoint::Ip(IpEndpoint::new(src, 0)));
                    }
                    // RxToken also enqueues the same frame on smoltcp RawSocket; drop it
                    // so BusyBox ping does not report (DUP!) with a second TTL.
                    {
                        let net_sockets = get_sockets();
                        let mut sockets = net_sockets.lock();
                        let mut socket = sockets.get::<RawSocket>(self.inner.handle.0);
                        if socket.can_recv() {
                            let _ = socket.recv_slice(data);
                        }
                    }
                    let non_block = self.inner.flags.lock().contains(OpenFlags::NON_BLOCK);
                    if non_block {
                        return (Err(LxError::EAGAIN), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
                    }
                    kernel_hal::thread::sleep_until(
                        kernel_hal::timer::timer_now() + core::time::Duration::from_millis(10),
                    )
                    .await;
                    continue;
                }
            }
            let net_sockets = get_sockets();
            let mut sockets = net_sockets.lock();
            let mut socket = sockets.get::<RawSocket>(self.inner.handle.0);
            if socket.can_recv() {
                if let Ok(size) = socket.recv_slice(data) {
                    drop(socket);
                    drop(sockets);
                    if self.inner.ipv6 {
                        if let Ok(packet) = Ipv6Packet::new_checked(&data[..size]) {
                            let src_addr = packet.src_addr();
                            let payload_len = size.saturating_sub(40);
                            data.copy_within(40..size, 0);
                            return (
                                Ok(payload_len),
                                Endpoint::Ip(IpEndpoint {
                                    addr: IpAddress::Ipv6(src_addr),
                                    port: 0,
                                }),
                            );
                        }
                    } else {
                        if let Ok(packet) = Ipv4Packet::new_checked(&data[..size]) {
                            return (
                                Ok(size),
                                Endpoint::Ip(IpEndpoint {
                                    addr: IpAddress::Ipv4(packet.src_addr()),
                                    port: 0,
                                }),
                            );
                        }
                    }
                    return (Err(LxError::EINVAL), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
                }
            }
            let non_block = self.inner.flags.lock().contains(OpenFlags::NON_BLOCK);
            drop(socket);
            drop(sockets);
            if non_block {
                return (Err(LxError::EAGAIN), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
            }
            kernel_hal::thread::sleep_until(
                kernel_hal::timer::timer_now() + core::time::Duration::from_millis(10),
            )
            .await;
        }
    }

    fn write(&self, data: &[u8], sendto_endpoint: Option<Endpoint>) -> SysResult {
        let endpoint = match sendto_endpoint {
            Some(ep) => Some(ep),
            None => self.inner.remote.lock().clone(),
        };
        let net_sockets = get_sockets();
        let mut sockets = net_sockets.lock();
        let mut socket = sockets.get::<RawSocket>(self.inner.handle.0);
        if *self.inner.header_included.lock() {
            Self::check_included_header(data)?;
            let result = match socket.send_slice(data) {
                Ok(()) => Ok(data.len()),
                Err(_) => Err(LxError::ENOBUFS),
            };
            drop(socket);
            drop(sockets);
            if result.is_ok() {
                flush_socket_egress();
            }
            return result;
        }
        let Endpoint::Ip(ip) = endpoint.ok_or(LxError::ENOTCONN)? else {
            return Err(LxError::EINVAL);
        };
        if !*self.inner.header_included.lock()
            && !self.inner.ipv6
            && socket.ip_protocol() == IpProtocol::Icmp
        {
            let IpAddress::Ipv4(dst) = ip.addr else {
                return Err(LxError::EINVAL);
            };
            if dst.is_loopback() || is_local_host_ipv4(dst) {
                super::icmp_rx::queue_echo_reply(IpAddress::Ipv4(dst), data.to_vec());
                return Ok(data.len());
            }
        }
        if self.inner.ipv6 {
            let IpAddress::Ipv6(dst) = ip.addr else {
                return Err(LxError::EINVAL);
            };
            let src = select_ipv6_for_dst(dst);
            if src.is_unspecified() {
                return Err(LxError::EINVAL);
            }

            // The IPv6 payload length is a 16-bit field; a larger payload would
            // wrap on `set_payload_len(len as u16)` and make `payload_mut()`
            // shorter than `data`, panicking the `copy_from_slice` below.
            if data.len() > u16::MAX as usize {
                return Err(LxError::EMSGSIZE);
            }
            let len = data.len();
            let mut buffer = vec![0u8; len + 40];
            let mut packet = Ipv6Packet::new_unchecked(&mut buffer);
            let ip_repr = Ipv6Repr {
                src_addr: src,
                dst_addr: dst,
                next_header: socket.ip_protocol(),
                payload_len: len,
                hop_limit: 64,
            };
            ip_repr.emit(&mut packet);
            packet.payload_mut().copy_from_slice(data);

            if socket.ip_protocol() == IpProtocol::Icmpv6 {
                let mut icmp_pkt = smoltcp::wire::Icmpv6Packet::new_unchecked(packet.payload_mut());
                icmp_pkt.fill_checksum(&IpAddress::Ipv6(src), &IpAddress::Ipv6(dst));
            }

            socket.send_slice(&buffer).map_err(|e| {
                warn!("raw socket send_slice failed: {:?}", e);
                LxError::ENOBUFS
            })?;

            drop(socket);
            drop(sockets);
            flush_socket_egress();
            Ok(len)
        } else {
            let IpAddress::Ipv4(mut v4_dst) = ip.addr else {
                return Err(LxError::EINVAL);
            };
            if v4_dst.is_unspecified() {
                v4_dst = Ipv4Address::new(127, 0, 0, 1);
            }
            if !v4_dst.is_unicast() && !v4_dst.is_broadcast() && !v4_dst.is_multicast() {
                warn!("raw socket: invalid destination address {:?}", v4_dst);
                return Err(LxError::EINVAL);
            }

            // The IPv4 total-length field is 16 bits (header + payload); a larger
            // payload would wrap on `set_total_len((20 + len) as u16)` and make
            // `payload_mut()` shorter than `data`, panicking `copy_from_slice`.
            if data.len() > (u16::MAX as usize - 20) {
                return Err(LxError::EMSGSIZE);
            }
            let len = data.len();
            let mut buffer = vec![0u8; len + 20];
            let mut packet = Ipv4Packet::new_unchecked(&mut buffer);
            packet.set_version(4);
            packet.set_header_len(20);
            packet.set_total_len((20 + len) as u16);
            packet.set_protocol(socket.ip_protocol());
            let src_addr = select_ipv4_for_dst(v4_dst);
            if src_addr.is_unspecified() {
                return Err(LxError::EINVAL);
            }
            packet.set_src_addr(src_addr);
            packet.set_dst_addr(v4_dst);
            packet.set_hop_limit(64);
            packet.payload_mut().copy_from_slice(data);
            packet.fill_checksum();

            socket.send_slice(&buffer).map_err(|e| {
                warn!("raw socket send_slice failed: {:?}", e);
                LxError::ENOBUFS
            })?;

            drop(socket);
            drop(sockets);
            flush_socket_egress();
            Ok(len)
        }
    }

    async fn connect(&self, endpoint: Endpoint) -> SysResult {
        let Endpoint::Ip(ip) = endpoint else {
            return Err(LxError::EINVAL);
        };
        let family_ok = matches!(
            (self.inner.ipv6, ip.addr),
            (true, IpAddress::Ipv6(_)) | (false, IpAddress::Ipv4(_))
        );
        if !family_ok {
            return Err(LxError::EINVAL);
        }
        *self.inner.remote.lock() = Some(Endpoint::Ip(ip));
        Ok(0)
    }

    fn bind(&self, _endpoint: Endpoint) -> SysResult {
        Ok(0)
    }

    fn setsockopt(&self, level: usize, opt: usize, data: &[u8]) -> SysResult {
        if let (IPPROTO_IP, IP_HDRINCL) = (level, opt) {
            // `IP_HDRINCL` is an AF_INET option; on an AF_INET6 socket Linux
            // answers ENOPROTOOPT. Accepting it here put the raw bytes of an
            // IPv4 header into a socket smoltcp opened for IPv6, which it then
            // dropped on dispatch while `write` reported success.
            if self.inner.ipv6 {
                return Err(LxError::ENOPROTOOPT);
            }
            if let Some(arg) = data.first() {
                *self.inner.header_included.lock() = *arg > 0;
                debug!("hdrincl set to {}", *self.inner.header_included.lock());
            }
        }
        Ok(0)
    }
    fn get_buffer_capacity(&self) -> Option<(usize, usize)> {
        let sockets = get_sockets();
        let mut s = sockets.lock();
        let socket = s.get::<RawSocket>(self.inner.handle.0);
        let (recv_ca, send_ca) = (
            socket.payload_recv_capacity(),
            socket.payload_send_capacity(),
        );
        Some((recv_ca, send_ca))
    }
    fn endpoint(&self) -> Option<Endpoint> {
        let addr = if self.inner.ipv6 {
            IpAddress::Ipv6(Ipv6Address::UNSPECIFIED)
        } else {
            IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)
        };
        Some(Endpoint::Ip(IpEndpoint { addr, port: 0 }))
    }
    fn remote_endpoint(&self) -> Option<Endpoint> {
        self.inner.remote.lock().clone()
    }
    fn socket_type(&self) -> Option<SocketType> {
        Some(SocketType::SOCK_RAW)
    }

    fn poll(&self, _events: PollEvents) -> (bool, bool, bool) {
        drain_net_poll(1);
        let s = get_sockets();
        let mut s = s.lock();
        let socket = s.get::<RawSocket>(self.inner.handle.0);
        let readable = socket.can_recv()
            || (!self.inner.ipv6
                && socket.ip_protocol() == IpProtocol::Icmp
                && super::icmp_rx::pending_for(false));
        (readable, socket.can_send(), false)
    }
}

zircon_object::impl_kobject!(RawSocketState);

#[async_trait]
impl FileLike for RawSocketState {
    /// A socket reports `S_IFSOCK` to `fstat(2)`, as on Linux.
    fn metadata(&self) -> LxResult<rcore_fs::vfs::Metadata> {
        Ok(crate::fs::anon_metadata(
            rcore_fs::vfs::FileType::Socket,
            self.id() as usize,
        ))
    }

    fn flags(&self) -> OpenFlags {
        *self.inner.flags.lock()
    }

    fn set_flags(&self, f: OpenFlags) -> LxResult {
        let mut flags = self.inner.flags.lock();
        flags.set(OpenFlags::APPEND, f.contains(OpenFlags::APPEND));
        flags.set(OpenFlags::NON_BLOCK, f.contains(OpenFlags::NON_BLOCK));
        flags.set(OpenFlags::CLOEXEC, f.contains(OpenFlags::CLOEXEC));
        Ok(())
    }

    async fn read(&self, buf: &mut [u8]) -> LxResult<usize> {
        Socket::read(self, buf).await.0
    }

    async fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> LxResult<usize> {
        // Sockets do not support positioned reads.
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
        let (read, write, error) = Socket::poll(self, events);
        Ok(PollStatus {
            read,
            write,
            error,
            hangup: false,
        })
    }

    fn ioctl(&self, request: usize, arg1: usize, arg2: usize, arg3: usize) -> LxResult<usize> {
        handle_net_ioctl(request, arg1, arg2, arg3, self.inner.ipv6)
    }

    fn as_socket(&self) -> LxResult<&dyn Socket> {
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    //! `SOCK_RAW` against `raw.c`, on the host: the socket lives in the real
    //! global smoltcp set and a loopback interface built here is what
    //! dispatches what `write` queues -- which is where the header the caller
    //! supplied is parsed, and where an unchecked one used to panic.

    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::vec;
    use alloc::vec::Vec;
    use smoltcp::iface::{Interface, InterfaceBuilder, Routes};
    use smoltcp::phy::{Loopback, Medium};
    use smoltcp::time::Instant;
    use smoltcp::wire::IpCidr;

    use crate::net::NET_TEST_LOCK as LOCK;

    const IPPROTO_ICMP: u8 = 1;

    fn sock(ipv6: bool) -> RawSocketState {
        let s = RawSocketState::new(IPPROTO_ICMP, ipv6).unwrap();
        FileLike::set_flags(&s, OpenFlags::NON_BLOCK).unwrap();
        s
    }

    fn hdrincl(s: &RawSocketState, on: bool) -> SysResult {
        Socket::setsockopt(s, IPPROTO_IP, IP_HDRINCL, &[u8::from(on)])
    }

    fn loopback() -> Interface<'static, Loopback> {
        InterfaceBuilder::new(Loopback::new(Medium::Ip))
            .ip_addrs([IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8)])
            .routes(Routes::new(BTreeMap::new()))
            .finalize()
    }

    /// Dispatch whatever the socket queued. This is the step that parses the
    /// caller's header, on whichever thread happens to poll next.
    fn deliver(iface: &mut Interface<'static, Loopback>) {
        let sets = get_sockets();
        let mut sets = sets.lock();
        for ms in 0..4 {
            let _ = iface.poll(&mut sets, Instant::from_millis(ms));
        }
    }

    /// A well-formed 20-byte IPv4 header plus `payload`.
    fn ipv4(protocol: u8, payload: &[u8]) -> Vec<u8> {
        let total = 20 + payload.len();
        let mut buf = vec![0u8; total];
        buf[0] = 0x45;
        buf[2..4].copy_from_slice(&(total as u16).to_be_bytes());
        buf[8] = 64;
        buf[9] = protocol;
        buf[12..16].copy_from_slice(&[127, 0, 0, 1]);
        buf[16..20].copy_from_slice(&[127, 0, 0, 1]);
        buf[20..].copy_from_slice(payload);
        buf
    }

    #[test]
    fn a_zero_length_hdrincl_write_is_refused_instead_of_panicking_the_kernel() {
        let _g = LOCK.lock();
        let s = sock(false);
        assert_eq!(hdrincl(&s, true), Ok(0));
        // `write(fd, "", 0)`. This enqueued an empty packet, and the panic came
        // later, inside whatever thread next polled the interface:
        // `IpVersion::of_packet` indexes byte 0 with no length check.
        assert_eq!(Socket::write(&s, &[], None), Err(LxError::EINVAL));
        // Anything shorter than an IPv4 header is the same answer
        // (`length < sizeof(struct iphdr)`).
        for n in 1..20 {
            let mut short = ipv4(IPPROTO_ICMP, &[]);
            short.truncate(n);
            assert_eq!(
                Socket::write(&s, &short, None),
                Err(LxError::EINVAL),
                "len {}",
                n
            );
        }
        // The ring is empty, so the poll that used to panic has nothing to do.
        let mut iface = loopback();
        deliver(&mut iface);

        // And the well-formed header still goes through.
        let good = ipv4(IPPROTO_ICMP, &[8, 0, 0, 0, 0, 1, 0, 1]);
        assert_eq!(Socket::write(&s, &good, None), Ok(good.len()));
        deliver(&mut iface);
    }

    #[test]
    fn a_hdrincl_header_that_smoltcp_would_drop_in_silence_is_refused_up_front() {
        let _g = LOCK.lock();
        let s = sock(false);
        assert_eq!(hdrincl(&s, true), Ok(0));
        let good = ipv4(IPPROTO_ICMP, &[8, 0, 0, 0, 0, 1, 0, 1]);

        // A version nibble that is not 4: smoltcp drops it without a word, so
        // `write` used to report a success that sent nothing.
        for version in [0u8, 5, 6, 0xf] {
            let mut bad = good.clone();
            bad[0] = (version << 4) | 0x05;
            assert_eq!(
                Socket::write(&s, &bad, None),
                Err(LxError::EINVAL),
                "version {}",
                version
            );
        }
        // An IHL below the 20-byte minimum, and one claiming more header than
        // the buffer holds (`iph->ihl * 4 > length`).
        for ihl in [0u8, 1, 4, 0x0f] {
            let mut bad = good.clone();
            bad[0] = 0x40 | ihl;
            assert_eq!(
                Socket::write(&s, &bad, None),
                Err(LxError::EINVAL),
                "ihl {}",
                ihl
            );
        }
        let mut iface = loopback();
        deliver(&mut iface);
    }

    #[test]
    fn hdrincl_is_an_ipv4_option_and_an_ipv6_socket_says_so() {
        let _g = LOCK.lock();
        let six = sock(true);
        // Linux answers ENOPROTOOPT: `IP_HDRINCL` is AF_INET only. Accepting it
        // put an IPv4 header into a socket smoltcp opened for IPv6, which then
        // dropped it on dispatch while `write` reported success.
        assert_eq!(hdrincl(&six, true), Err(LxError::ENOPROTOOPT));
        assert_eq!(hdrincl(&six, false), Err(LxError::ENOPROTOOPT));
        // Every other option stays lenient, as on the rest of the sockets.
        assert_eq!(Socket::setsockopt(&six, 1, 2, &1u32.to_ne_bytes()), Ok(0));
        assert_eq!(Socket::setsockopt(&six, IPPROTO_IP, 2, &[64]), Ok(0));

        // On IPv4 it is honoured, and switching it back off is honoured too.
        let four = sock(false);
        assert_eq!(hdrincl(&four, true), Ok(0));
        assert_eq!(Socket::write(&four, &[], None), Err(LxError::EINVAL));
        assert_eq!(hdrincl(&four, false), Ok(0));
        // With the kernel building the header, an empty payload is a legal
        // 20-byte datagram, so the length rule must not leak across.
        assert_eq!(Socket::write(&four, &[], Some(lo())), Ok(0));
        let mut iface = loopback();
        deliver(&mut iface);
    }

    fn lo() -> Endpoint {
        Endpoint::Ip(IpEndpoint::new(IpAddress::v4(127, 0, 0, 1), 0))
    }

    #[test]
    fn a_write_with_no_destination_at_all_is_enotconn() {
        let _g = LOCK.lock();
        let s = sock(false);
        // Neither `connect` nor a `sendto` address: `raw_sendmsg` has nowhere
        // to send it.
        assert_eq!(
            Socket::write(&s, &[8, 0, 0, 0], None),
            Err(LxError::ENOTCONN)
        );
        // `connect` records it, and then a bare `write` uses it.
        assert_eq!(async_std::task::block_on(Socket::connect(&s, lo())), Ok(0));
        assert!(
            matches!(Socket::remote_endpoint(&s), Some(Endpoint::Ip(ep)) if ep == IpEndpoint::new(IpAddress::v4(127, 0, 0, 1), 0))
        );
        assert_eq!(Socket::write(&s, &[8, 0, 0, 0, 0, 1, 0, 1], None), Ok(8));
        let mut iface = loopback();
        deliver(&mut iface);
    }

    #[test]
    fn connect_and_write_refuse_an_address_of_the_other_family() {
        let _g = LOCK.lock();
        let six = IpAddress::Ipv6(Ipv6Address::LOOPBACK);
        let four = sock(false);
        assert_eq!(
            async_std::task::block_on(Socket::connect(
                &four,
                Endpoint::Ip(IpEndpoint::new(six, 0))
            )),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            Socket::write(&four, &[8, 0], Some(Endpoint::Ip(IpEndpoint::new(six, 0)))),
            Err(LxError::EINVAL)
        );

        let sixsock = sock(true);
        assert_eq!(
            async_std::task::block_on(Socket::connect(&sixsock, lo())),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            Socket::write(&sixsock, &[128, 0], Some(lo())),
            Err(LxError::EINVAL)
        );
        // And a non-IP endpoint is refused on both.
        let unix = Endpoint::LinkLevel(crate::net::LinkLevelEndpoint::new(0));
        assert_eq!(
            async_std::task::block_on(Socket::connect(&four, unix)),
            Err(LxError::EINVAL)
        );
    }

    #[test]
    fn a_payload_too_big_for_the_length_field_is_emsgsize_not_a_panic() {
        let _g = LOCK.lock();
        // Both emitters write the length into a 16-bit field and then
        // `copy_from_slice` into the payload the field describes, so a payload
        // that wraps it made the copy panic.
        let four = sock(false);
        // Not a local address: a local one takes the self-ping shortcut before
        // the header is ever built.
        let off_link = Endpoint::Ip(IpEndpoint::new(IpAddress::v4(10, 0, 0, 1), 0));
        let big = vec![0u8; u16::MAX as usize - 19];
        assert_eq!(
            Socket::write(&four, &big, Some(off_link.clone())),
            Err(LxError::EMSGSIZE)
        );
        // One byte under the field is a size question no more.
        let edge = vec![0u8; u16::MAX as usize - 20];
        assert_ne!(
            Socket::write(&four, &edge, Some(off_link.clone())),
            Err(LxError::EMSGSIZE)
        );

        // The IPv6 emitter has the same guard, one field wider. It sits behind
        // source selection, so on a host with no IPv6 address of its own the
        // answer is EINVAL -- what matters here is that neither is a panic.
        let six = sock(true);
        let dst = Endpoint::Ip(IpEndpoint::new(IpAddress::Ipv6(Ipv6Address::LOOPBACK), 0));
        let big6 = vec![0u8; u16::MAX as usize + 1];
        assert!(matches!(
            Socket::write(&six, &big6, Some(dst)),
            Err(LxError::EMSGSIZE) | Err(LxError::EINVAL)
        ));
    }

    #[test]
    fn a_raw_socket_reports_its_type_and_reads_nothing_at_an_offset() {
        let _g = LOCK.lock();
        let s = sock(false);
        assert_eq!(Socket::socket_type(&s), Some(SocketType::SOCK_RAW));
        assert_eq!(
            FileLike::metadata(&s).map(|m| m.type_),
            Ok(rcore_fs::vfs::FileType::Socket)
        );
        // Sockets have no file position.
        let mut buf = [0u8; 4];
        assert_eq!(
            async_std::task::block_on(FileLike::read_at(&s, 0, &mut buf)),
            Err(LxError::ESPIPE)
        );
        // A fresh socket is writable and not in error, and has buffer room.
        // The ICMP receive queue is shared, so empty it first.
        while super::super::icmp_rx::pop_for(false, None).is_some() {}
        let (read, write, error) = Socket::poll(&s, PollEvents::all());
        assert!(!read && write && !error);
        let (rx, tx) = Socket::get_buffer_capacity(&s).expect("capacities");
        assert!(rx > 0 && tx > 0, "{} {}", rx, tx);
        // `set_flags` keeps the three flags it is allowed to and nothing else.
        FileLike::set_flags(&s, OpenFlags::NON_BLOCK | OpenFlags::CREATE).unwrap();
        let f = FileLike::flags(&s);
        assert!(f.contains(OpenFlags::NON_BLOCK));
        assert!(!f.contains(OpenFlags::CREATE));
    }

    #[test]
    fn a_nonblocking_read_with_nothing_queued_is_eagain() {
        let _g = LOCK.lock();
        let s = sock(false);
        let mut buf = [0u8; 64];
        // Drain anything a neighbouring test left in the shared ICMP queue.
        while super::super::icmp_rx::pop_for(false, None).is_some() {}
        assert_eq!(
            async_std::task::block_on(Socket::read(&s, &mut buf)).0,
            Err(LxError::EAGAIN)
        );
        assert_eq!(
            async_std::task::block_on(FileLike::read(&s, &mut buf)),
            Err(LxError::EAGAIN)
        );
    }
}
