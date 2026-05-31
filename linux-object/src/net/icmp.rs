use crate::{
    error::{LxError, LxResult},
    fs::{FileLike, OpenFlags, PollEvents, PollStatus},
    net::*,
};
use alloc::sync::Arc;
use async_trait::async_trait;
use lock::Mutex;
use smoltcp::wire::{IpAddress, Ipv4Address};

#[allow(unused_imports)]
use zircon_object::object::*;

pub struct IcmpSocketState {
    base: KObjectBase,
    inner: Arc<IcmpSocketInner>,
}

#[derive(Debug)]
struct IcmpSocketInner {
    flags: Mutex<OpenFlags>,
    remote: Mutex<Option<Endpoint>>,
    echo_seq: Mutex<u16>,
    echo_id: u16,
    ipv6: bool,
}

impl IcmpSocketState {
    /// missing documentation
    pub fn new(ipv6: bool) -> Self {
        IcmpSocketState {
            base: KObjectBase::new(),
            inner: Arc::new(IcmpSocketInner {
                flags: Mutex::new(OpenFlags::RDWR),
                remote: Mutex::new(None),
                echo_seq: Mutex::new(0),
                echo_id: (kernel_hal::timer::timer_now().as_micros() as u16).wrapping_add(1),
                ipv6,
            }),
        }
    }

    fn make_echo_request_payload(&self, data: &[u8]) -> alloc::vec::Vec<u8> {
        let expect_type = if self.inner.ipv6 { 128 } else { 8 };
        if data.len() >= 8 && data[0] == expect_type && data[1] == 0 {
            return data.to_vec();
        }
        let mut out = vec![0u8; 8 + data.len()];
        out[0] = expect_type;
        out[1] = 0;
        out[4..6].copy_from_slice(&self.inner.echo_id.to_be_bytes());
        let mut seq = self.inner.echo_seq.lock();
        out[6..8].copy_from_slice(&seq.to_be_bytes());
        *seq = seq.wrapping_add(1);
        out[8..].copy_from_slice(data);
        out
    }

    fn remote_ipv4(&self) -> Option<Ipv4Address> {
        let Endpoint::Ip(ip) = self.inner.remote.lock().clone()? else {
            return None;
        };
        match ip.addr {
            IpAddress::Ipv4(v4) => Some(v4),
            _ => None,
        }
    }

    fn remote_ip(&self) -> Option<IpAddress> {
        let Endpoint::Ip(ip) = self.inner.remote.lock().clone()? else {
            return None;
        };
        Some(ip.addr)
    }

    fn kick_rx(&self) {
        // If we have a known remote, drain only that interface's RX ring.
        // Otherwise drain all non-loopback interfaces so we don't miss the
        // ICMP reply when ping uses sendto() without a prior connect().
        if self.inner.ipv6 {
            if let Some(Endpoint::Ip(ip)) = self.inner.remote.lock().clone() {
                if let IpAddress::Ipv6(dst) = ip.addr {
                    if dst.is_loopback() {
                        crate::net::poll_ifaces();
                        return;
                    }
                    if let Ok(dev) = netdev_for_ipv6(dst) {
                        netdev_drain_rx(dev.as_ref());
                        return;
                    }
                }
            }
        } else {
            if let Some(dst) = self.remote_ipv4() {
                if dst.is_loopback() {
                    crate::net::poll_ifaces();
                    return;
                }
                if let Ok(dev) = netdev_for_ipv4(dst) {
                    netdev_drain_rx(dev.as_ref());
                    return;
                }
            }
        }
        // Fallback: drain all non-loopback devices.
        for dev in kernel_hal::net::get_net_device().iter() {
            if dev.get_ifname() != "loopback" {
                netdev_drain_rx(dev.as_ref());
            } else {
                crate::net::poll_ifaces();
            }
        }
    }
}

#[async_trait]
impl Socket for IcmpSocketState {
    async fn read(&self, data: &mut [u8]) -> (SysResult, Endpoint) {
        loop {
            let remote = self.remote_ip();
            if let Some((pkt, src)) = icmp_rx::pop_for(self.inner.ipv6, remote) {
                let n = pkt.len().min(data.len());
                data[..n].copy_from_slice(&pkt[..n]);
                return (Ok(n), Endpoint::Ip(IpEndpoint::new(src.into(), 0)));
            }

            // Drain deferred jobs first: the NIC IRQ handler pushes a deferred
            // poll() job that routes incoming frames through smoltcp and then
            // calls push_packet -> deliver_from_frame. Without draining here,
            // the ICMP echo reply sits in the deferred queue forever.
            kernel_hal::deferred_job::drain_deferred_jobs();

            // Direct RX drain bypasses smoltcp and calls push_packet/deliver_from_frame
            // immediately — covers the case where the reply arrived before the
            // deferred poll job was scheduled (tight timing on QEMU).
            self.kick_rx();

            if let Err(e) = crate::process::check_and_deliver_tty_interrupt() {
                return (Err(e), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
            }
            if let Err(e) = crate::process::check_signals() {
                return (Err(e), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
            }

            if self.inner.flags.lock().contains(OpenFlags::NON_BLOCK) {
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
        let Endpoint::Ip(ip) = endpoint.ok_or(LxError::ENOTCONN)? else {
            return Err(LxError::EINVAL);
        };
        if self.inner.ipv6 {
            let IpAddress::Ipv6(dst) = ip.addr else {
                return Err(LxError::EINVAL);
            };
            let src = select_ipv6_for_dst(dst);
            if src.is_unspecified() {
                return Err(LxError::EINVAL);
            }

            let icmp_payload = self.make_echo_request_payload(data);
            let ip_len = 40 + icmp_payload.len();
            let mut ip = vec![0u8; ip_len];
            {
                let mut pkt = smoltcp::wire::Ipv6Packet::new_unchecked(&mut ip);
                pkt.set_version(6);
                pkt.set_payload_len(icmp_payload.len() as u16);
                pkt.set_next_header(smoltcp::wire::IpProtocol::Icmpv6);
                pkt.set_src_addr(src);
                pkt.set_dst_addr(dst);
                pkt.set_hop_limit(64);
                pkt.payload_mut().copy_from_slice(&icmp_payload);

                let mut icmp_pkt = smoltcp::wire::Icmpv6Packet::new_unchecked(pkt.payload_mut());
                icmp_pkt.fill_checksum(&IpAddress::Ipv6(src), &IpAddress::Ipv6(dst));
                info!(
                    "[icmp write] Filled ICMPv6 checksum: 0x{:04x}, src: {}, dst: {}, bytes: {:?}",
                    icmp_pkt.checksum(),
                    src,
                    dst,
                    &ip[40..]
                );
            }
            if dst.is_loopback() {
                if let Ok(dev) = crate::net::iface_by_name("loopback") {
                    dev.send(&ip).map_err(|_| LxError::EIO)?;
                    crate::net::poll_ifaces();
                    self.kick_rx();
                    return Ok(data.len());
                }
            }
            send_ip6_ethernet(&ip)?;
            self.kick_rx();
            Ok(data.len())
        } else {
            let IpAddress::Ipv4(dst) = ip.addr else {
                return Err(LxError::EINVAL);
            };
            if !dst.is_unicast() {
                return Err(LxError::EINVAL);
            }

            let src = select_ipv4_for_dst(dst);
            if src.is_unspecified() {
                return Err(LxError::EINVAL);
            }

            let icmp_payload = self.make_echo_request_payload(data);
            let ip_len = 20 + icmp_payload.len();
            let mut ip = vec![0u8; ip_len];
            {
                let mut pkt = smoltcp::wire::Ipv4Packet::new_unchecked(&mut ip);
                pkt.set_version(4);
                pkt.set_header_len(20);
                pkt.set_total_len(ip_len as u16);
                pkt.set_protocol(smoltcp::wire::IpProtocol::Icmp);
                pkt.set_src_addr(src);
                pkt.set_dst_addr(dst);
                pkt.set_hop_limit(64);
                pkt.payload_mut().copy_from_slice(&icmp_payload);
                let mut icmp_pkt = smoltcp::wire::Icmpv4Packet::new_unchecked(pkt.payload_mut());
                icmp_pkt.fill_checksum();
                pkt.fill_checksum();
            }
            if dst.is_loopback() {
                if let Ok(dev) = crate::net::iface_by_name("loopback") {
                    dev.send(&ip).map_err(|_| LxError::EIO)?;
                    crate::net::poll_ifaces();
                    self.kick_rx();
                    return Ok(data.len());
                }
            }
            send_ip_ethernet(&ip)?;
            self.kick_rx();
            Ok(data.len())
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

    fn setsockopt(&self, _level: usize, _opt: usize, _data: &[u8]) -> SysResult {
        Ok(0)
    }

    fn get_buffer_capacity(&self) -> Option<(usize, usize)> {
        Some((ICMP_RECVBUF, ICMP_SENDBUF))
    }

    fn socket_type(&self) -> Option<SocketType> {
        Some(SocketType::SOCK_DGRAM)
    }

    fn poll(&self, _events: PollEvents) -> (bool, bool, bool) {
        kernel_hal::deferred_job::drain_deferred_jobs();
        self.kick_rx();
        let readable = icmp_rx::pending();
        (readable, true, false)
    }
}

zircon_object::impl_kobject!(IcmpSocketState);

#[async_trait]
impl FileLike for IcmpSocketState {
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

    fn dup(&self) -> Arc<dyn FileLike> {
        Arc::new(Self {
            base: KObjectBase::new(),
            inner: self.inner.clone(),
        })
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
        Ok(PollStatus { read, write, error })
    }

    async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus> {
        kernel_hal::deferred_job::drain_deferred_jobs();
        let (read, write, error) = Socket::poll(self, events);
        Ok(PollStatus { read, write, error })
    }

    fn ioctl(&self, request: usize, arg1: usize, arg2: usize, arg3: usize) -> LxResult<usize> {
        handle_net_ioctl(request, arg1, arg2, arg3, self.inner.ipv6)
    }

    fn as_socket(&self) -> LxResult<&dyn Socket> {
        Ok(self)
    }
}
