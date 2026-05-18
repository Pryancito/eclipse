use crate::{
    error::{LxError, LxResult},
    fs::{FileLike, OpenFlags, PollEvents, PollStatus},
    net::*,
};
use alloc::sync::Arc;
use async_trait::async_trait;
use lock::Mutex;
use smoltcp::{
    socket::{IcmpEndpoint, IcmpPacketMetadata, IcmpSocket, IcmpSocketBuffer},
    wire::IpAddress,
};

#[allow(unused_imports)]
use zircon_object::object::*;

pub struct IcmpSocketState {
    base: KObjectBase,
    inner: Arc<IcmpSocketInner>,
}

#[derive(Debug)]
struct IcmpSocketInner {
    handle: GlobalSocketHandle,
    flags: Mutex<OpenFlags>,
    remote: Mutex<Option<Endpoint>>,
}

impl IcmpSocketState {
    /// missing documentation
    pub fn new() -> Self {
        let rx_buffer = IcmpSocketBuffer::new(
            vec![IcmpPacketMetadata::EMPTY; ICMP_METADATA_BUF],
            vec![0; ICMP_RECVBUF],
        );
        let tx_buffer = IcmpSocketBuffer::new(
            vec![IcmpPacketMetadata::EMPTY; ICMP_METADATA_BUF],
            vec![0; ICMP_SENDBUF],
        );
        let socket = IcmpSocket::new(rx_buffer, tx_buffer);
        let handle = GlobalSocketHandle(get_sockets().lock().add(socket));

        IcmpSocketState {
            base: KObjectBase::new(),
            inner: Arc::new(IcmpSocketInner {
                handle,
                flags: Mutex::new(OpenFlags::RDWR),
                remote: Mutex::new(None),
            }),
        }
    }

    fn ensure_bound(&self, data: &[u8]) -> LxResult {
        let sockets = get_sockets();
        let mut set = sockets.lock();
        let mut socket = set.get::<IcmpSocket>(self.inner.handle.0);
        if socket.is_open() {
            return Ok(());
        }
        if data.len() >= 6 {
            let ident = u16::from_be_bytes([data[4], data[5]]);
            socket
                .bind(IcmpEndpoint::Ident(ident))
                .map_err(|_| LxError::EINVAL)?;
            return Ok(());
        }
        socket
            .bind(IcmpEndpoint::Ident(0))
            .map_err(|_| LxError::EINVAL)
    }
}

#[async_trait]
impl Socket for IcmpSocketState {
    async fn read(&self, data: &mut [u8]) -> (SysResult, Endpoint) {
        loop {
            drain_net_poll(4);
            if let Err(e) = crate::process::check_and_deliver_tty_interrupt() {
                return (Err(e), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
            }
            let net_sockets = get_sockets();
            let mut sockets = net_sockets.lock();
            let mut socket = sockets.get::<IcmpSocket>(self.inner.handle.0);
            if socket.can_recv() {
                if let Ok((size, addr)) = socket.recv_slice(data) {
                    let endpoint = match addr {
                        IpAddress::Ipv4(v4) => IpEndpoint::new(v4.into(), 0),
                        _ => IpEndpoint::UNSPECIFIED,
                    };
                    drop(socket);
                    drop(sockets);
                    return (Ok(size), Endpoint::Ip(endpoint));
                }
            }
            let non_block = self.inner.flags.lock().contains(OpenFlags::NON_BLOCK);
            drop(socket);
            drop(sockets);
            if non_block {
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

        self.ensure_bound(data)?;

        let net_sockets = get_sockets();
        let mut sockets = net_sockets.lock();
        let mut socket = sockets.get::<IcmpSocket>(self.inner.handle.0);
        socket
            .send_slice(data, IpAddress::Ipv4(dst))
            .map_err(|_| LxError::ENOBUFS)?;
        drop(socket);
        drop(sockets);
        drain_net_poll(32);
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

    fn bind(&self, endpoint: Endpoint) -> SysResult {
        let Endpoint::Ip(ip) = endpoint else {
            return Err(LxError::EINVAL);
        };
        let ident = ip.port;
        let sockets = get_sockets();
        let mut set = sockets.lock();
        let mut socket = set.get::<IcmpSocket>(self.inner.handle.0);
        socket
            .bind(IcmpEndpoint::Ident(ident))
            .map_err(|_| LxError::EINVAL)?;
        Ok(0)
    }

    fn setsockopt(&self, _level: usize, _opt: usize, _data: &[u8]) -> SysResult {
        Ok(0)
    }

    fn get_buffer_capacity(&self) -> Option<(usize, usize)> {
        let sockets = get_sockets();
        let mut s = sockets.lock();
        let socket = s.get::<IcmpSocket>(self.inner.handle.0);
        Some((
            socket.payload_recv_capacity(),
            socket.payload_send_capacity(),
        ))
    }

    fn socket_type(&self) -> Option<SocketType> {
        Some(SocketType::SOCK_DGRAM)
    }

    fn poll(&self, _events: PollEvents) -> (bool, bool, bool) {
        drain_net_poll(1);
        let s = get_sockets();
        let mut s = s.lock();
        let socket = s.get::<IcmpSocket>(self.inner.handle.0);
        (socket.can_recv(), socket.can_send(), false)
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
        unimplemented!()
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
