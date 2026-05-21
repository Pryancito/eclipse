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
}

impl IcmpSocketState {
    /// missing documentation
    pub fn new() -> Self {
        IcmpSocketState {
            base: KObjectBase::new(),
            inner: Arc::new(IcmpSocketInner {
                flags: Mutex::new(OpenFlags::RDWR),
                remote: Mutex::new(None),
            }),
        }
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

    fn kick_rx(&self) {
        let Some(dst) = self.remote_ipv4() else {
            return;
        };
        if let Ok(dev) = netdev_for_ipv4(dst) {
            netdev_drain_rx(dev.as_ref());
        }
    }
}

#[async_trait]
impl Socket for IcmpSocketState {
    async fn read(&self, data: &mut [u8]) -> (SysResult, Endpoint) {
        loop {
            if let Some((pkt, src)) = icmp_rx::pop() {
                let n = pkt.len().min(data.len());
                data[..n].copy_from_slice(&pkt[..n]);
                return (
                    Ok(n),
                    Endpoint::Ip(IpEndpoint::new(src.into(), 0)),
                );
            }

            self.kick_rx();

            if let Err(e) = crate::process::check_and_deliver_tty_interrupt() {
                return (Err(e), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
            }
            if let Err(e) = crate::process::check_signals() {
                return (Err(e), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
            }

            if self.inner.flags.lock().contains(OpenFlags::NON_BLOCK) {
                return (
                    Err(LxError::EAGAIN),
                    Endpoint::Ip(IpEndpoint::UNSPECIFIED),
                );
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

        let ip_len = 20 + data.len();
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
            pkt.payload_mut().copy_from_slice(data);
            pkt.fill_checksum();
        }
        send_ip_ethernet(&ip)?;
        self.kick_rx();
        Ok(data.len())
    }

    async fn connect(&self, endpoint: Endpoint) -> SysResult {
        if matches!(endpoint, Endpoint::Ip(_)) {
            *self.inner.remote.lock() = Some(endpoint);
            Ok(0)
        } else {
            Err(LxError::EINVAL)
        }
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
        Ok(PollStatus {
            read,
            write,
            error,
        })
    }

    async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus> {
        let (read, write, error) = Socket::poll(self, events);
        Ok(PollStatus {
            read,
            write,
            error,
        })
    }

    fn ioctl(&self, request: usize, arg1: usize, arg2: usize, arg3: usize) -> LxResult<usize> {
        handle_net_ioctl(request, arg1, arg2, arg3)
    }

    fn as_socket(&self) -> LxResult<&dyn Socket> {
        Ok(self)
    }
}
