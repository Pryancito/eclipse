// udpsocket

use crate::error::{LxError, LxResult};
use crate::fs::{FileLike, OpenFlags, PollStatus};
use crate::net::*;
use alloc::{boxed::Box, sync::Arc, vec};
use async_trait::async_trait;
use kernel_hal::sync::Mutex;
use smoltcp::socket::{SocketSet, UdpPacketMetadata, UdpSocket, UdpSocketBuffer};
use smoltcp::wire::{IpAddress, Ipv4Address, Ipv6Address};
// use smoltcp::wire::{IpCidr, Ipv4Address, Ipv4Cidr};

// third part
#[allow(unused_imports)]
use zircon_object::impl_kobject;
#[allow(unused_imports)]
use zircon_object::object::*;

pub struct UdpSocketState {
    /// Kernel object base
    base: KObjectBase,
    /// UdpSocket Inner
    inner: Arc<Mutex<UdpInner>>,
}

/// UDP socket inner
#[derive(Debug)]
pub struct UdpInner {
    /// A wrapper for `SocketHandle`
    handle: GlobalSocketHandle,
    /// remember remote endpoint for connect fn
    remote_endpoint: Option<IpEndpoint>,
    /// flags on the socket
    flags: OpenFlags,
    /// ipv6 domain socket flag
    ipv6: bool,
    /// Last recvmsg flags (`MSG_TRUNC`, …); cleared by [`Socket::take_msg_flags`].
    last_msg_flags: i32,
    /// `shutdown(SHUT_RD)`: an empty queue reads as EOF from now on.
    read_closed: bool,
    /// `shutdown(SHUT_WR)`: recorded for `poll` (`SHUTDOWN_MASK` is a hangup);
    /// UDP sends are not refused by it on Linux either.
    write_closed: bool,
}

/// `datagram_poll` from the three facts a smoltcp socket exposes.
///
/// A socket with no port yet (`!is_open`) is a fresh UDP socket before its
/// first `bind`/`connect`/`sendto`: nothing to read, a send would bind it and
/// go, and nothing is wrong with it. It used to report `POLLERR` and nothing
/// else, so a client that polls for `POLLOUT` before its first send got an
/// error on a socket it had just created.
fn poll_state(
    is_open: bool,
    can_recv: bool,
    can_send: bool,
    read_closed: bool,
) -> (bool, bool, bool) {
    if !is_open {
        return (read_closed, true, false);
    }
    (can_recv || read_closed, can_send, false)
}

/// The errno for a datagram smoltcp would not queue. All four used to be
/// `EIO`, which no `sendto(2)` documents and which resolvers treat as a dead
/// socket: a destination port of 0 is `EINVAL` (`udp_sendmsg`), a datagram
/// larger than the socket buffer can ever hold is `EMSGSIZE`, and a full
/// queue is `EAGAIN`, which the syscall layer turns into a wait for a
/// blocking socket.
fn send_error(e: smoltcp::Error) -> LxError {
    match e {
        smoltcp::Error::Exhausted => LxError::EAGAIN,
        smoltcp::Error::Truncated => LxError::EMSGSIZE,
        smoltcp::Error::Unaddressable => LxError::EINVAL,
        _ => LxError::EIO,
    }
}

/// `shutdown(2)`'s `how` as (read side, write side); anything else is `EINVAL`.
fn shutdown_sides(howto: usize) -> LxResult<(bool, bool)> {
    match howto {
        0 => Ok((true, false)),
        1 => Ok((false, true)),
        2 => Ok((true, true)),
        _ => Err(LxError::EINVAL),
    }
}

/// `udp_lib_lport_inuse` without `SO_REUSEADDR`: `want` collides with an
/// open UDP socket on the same port when either side is bound to the
/// wildcard address or both are bound to the same one. smoltcp itself lets
/// any number of sockets bind one port and delivers each datagram to the
/// first that matches, so a second daemon on a busy port used to bind fine
/// and never hear anything.
fn udp_port_taken(set: &SocketSet<'_>, want: IpEndpoint) -> bool {
    set.iter().any(|socket| match socket {
        smoltcp::socket::Socket::Udp(udp) if udp.is_open() => {
            let bound = udp.endpoint();
            bound.port == want.port
                && (bound.addr.is_unspecified()
                    || want.addr.is_unspecified()
                    || bound.addr == want.addr)
        }
        _ => false,
    })
}

/// An ephemeral port no open UDP socket holds on `addr`, or `None` when the
/// whole dynamic range is taken. `get_ephemeral_port` alone hands out the
/// next number whatever is bound to it, so an autobind could land on a port
/// a daemon had bound explicitly and the replies went to the daemon.
fn free_ephemeral_port(set: &SocketSet<'_>, addr: IpAddress) -> Option<u16> {
    const RANGE: usize = 65535 - 49152;
    (0..RANGE)
        .map(|_| get_ephemeral_port())
        .find(|&port| !udp_port_taken(set, IpEndpoint::new(addr, port)))
}

// Moved to mod.rs as public constants

// Moved to mod.rs as public structures

// Helpers moved to mod.rs

impl UdpSocketState {
    /// missing documentation
    pub fn new(ipv6: bool) -> LxResult<Self> {
        info!("udp new");
        let rx_buffer = UdpSocketBuffer::new(
            vec![UdpPacketMetadata::EMPTY; UDP_METADATA_BUF],
            vec![0; UDP_RECVBUF],
        );
        let tx_buffer = UdpSocketBuffer::new(
            vec![UdpPacketMetadata::EMPTY; UDP_METADATA_BUF],
            vec![0; UDP_SENDBUF],
        );
        let socket = UdpSocket::new(rx_buffer, tx_buffer);
        let handle = super::register_smoltcp_socket(socket)?;

        Ok(UdpSocketState {
            base: KObjectBase::new(),
            inner: Arc::new(Mutex::new(UdpInner {
                handle,
                remote_endpoint: None,
                flags: OpenFlags::RDWR,
                ipv6,
                last_msg_flags: 0,
                read_closed: false,
                write_closed: false,
            })),
        })
    }

    /// Give an unbound smoltcp socket a free ephemeral port on the family's
    /// wildcard address (`inet_autobind`); `EAGAIN` when none is free, as
    /// `udp_lib_get_port` reports it. A socket that already has a port is
    /// left alone.
    fn autobind(
        set: &mut SocketSet<'_>,
        handle: smoltcp::socket::SocketHandle,
        ipv6: bool,
    ) -> LxResult {
        if set.get::<UdpSocket>(handle).is_open() {
            return Ok(());
        }
        let addr = Self::family_addr(ipv6);
        let port = free_ephemeral_port(set, addr).ok_or(LxError::EAGAIN)?;
        set.get::<UdpSocket>(handle)
            .bind(IpEndpoint::new(addr, port))
            .map_err(|e| {
                warn!("udp autobind failed: {:?}", e);
                LxError::EINVAL
            })
    }

    fn family_addr(ipv6: bool) -> IpAddress {
        if ipv6 {
            IpAddress::Ipv6(Ipv6Address::UNSPECIFIED)
        } else {
            IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)
        }
    }

    /// `datagram_poll`: both sides shut is `POLLHUP`.
    fn hangup(&self) -> bool {
        let inner = self.inner.lock();
        inner.read_closed && inner.write_closed
    }

    fn endpoint_matches_family(ipv6: bool, ep: &IpEndpoint) -> bool {
        matches!(
            (ipv6, ep.addr),
            (true, IpAddress::Ipv6(_)) | (false, IpAddress::Ipv4(_))
        )
    }
}

/// missing in implementation
#[async_trait]
impl Socket for UdpSocketState {
    /// read to buffer
    async fn read(&self, data: &mut [u8]) -> (SysResult, Endpoint) {
        info!("udp read");
        let (handle, non_block, remote, read_closed) = {
            let inner = self.inner.lock();
            (
                inner.handle.0,
                inner.flags.contains(OpenFlags::NON_BLOCK),
                inner.remote_endpoint,
                inner.read_closed,
            )
        };
        loop {
            let sets = get_sockets();
            let mut sets = sets.lock();
            let mut socket = sets.get::<UdpSocket>(handle);
            // Use recv() so we know the full datagram length for MSG_TRUNC.
            let copied_len = match socket.recv() {
                Ok((buffer, endpoint)) => {
                    let full_len = buffer.len();
                    let n = data.len().min(full_len);
                    data[..n].copy_from_slice(&buffer[..n]);
                    Ok((n, endpoint, full_len > n))
                }
                Err(e) => Err(e),
            };
            drop(socket);
            drop(sets);

            match copied_len {
                Ok((size, endpoint, truncated)) => {
                    // Connected UDP: only deliver datagrams from the peer.
                    if let Some(peer) = remote {
                        if endpoint != peer {
                            continue;
                        }
                    }
                    if truncated {
                        // MSG_TRUNC = 0x20 — set for recvmsg to report truncation.
                        self.inner.lock().last_msg_flags |= 0x20;
                    } else {
                        self.inner.lock().last_msg_flags = 0;
                    }
                    return (Ok(size), Endpoint::Ip(endpoint));
                }
                Err(smoltcp::Error::Exhausted) => {
                    // What was queued before `shutdown(SHUT_RD)` came out
                    // above; an empty queue after it is EOF, as
                    // `__skb_wait_for_more_packets` answers `RCV_SHUTDOWN`.
                    if read_closed {
                        return (Ok(0), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
                    }
                    drain_net_poll(4);
                    // The receive buffer is empty. Try again later...
                    if non_block {
                        debug!("NON_BLOCK: Try again later...");
                        return (Err(LxError::EAGAIN), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
                    } else {
                        trace!("udp Exhausted. try again")
                    }
                }
                Err(err) => {
                    error!("udp socket recv_slice error: {:?}", err);
                    return (
                        Err(LxError::ENOTCONN),
                        Endpoint::Ip(IpEndpoint::UNSPECIFIED),
                    );
                }
            }
            if let Err(e) = crate::process::check_and_deliver_tty_interrupt() {
                return (Err(e), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
            }
            // Also honor non-TTY signals (SIGTERM/SIGALRM/...) so a blocked
            // recvfrom returns EINTR, matching the icmp socket path.
            if let Err(e) = crate::process::check_signals() {
                return (Err(e), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
            }
            kernel_hal::deferred_job::drain_deferred_jobs();
            // Park on the RX IRQ waker (5 ms fallback) instead of busy-spinning
            // — an idle udhcpc blocked on recvfrom otherwise pegs a core.
            kernel_hal::net::NetRxOrTimeoutFuture::new(5).await;
        }
    }
    async fn peek(&self, data: &mut [u8]) -> (SysResult, Endpoint) {
        let (handle, non_block, remote, read_closed) = {
            let inner = self.inner.lock();
            (
                inner.handle.0,
                inner.flags.contains(OpenFlags::NON_BLOCK),
                inner.remote_endpoint,
                inner.read_closed,
            )
        };
        loop {
            let sets = get_sockets();
            let mut sets = sets.lock();
            let mut socket = sets.get::<UdpSocket>(handle);
            let copied_len: Result<(usize, IpEndpoint, bool), _> = match socket.peek() {
                Ok((buffer, endpoint)) => {
                    let full_len = buffer.len();
                    let endpoint = *endpoint;
                    let n = data.len().min(full_len);
                    data[..n].copy_from_slice(&buffer[..n]);
                    Ok((n, endpoint, full_len > n))
                }
                Err(e) => Err(e),
            };
            // If connected and the front datagram is from a different peer,
            // discard it (Linux filters at recv) then keep looking.
            if let (Ok((_, endpoint, _)), Some(peer)) = (&copied_len, remote) {
                if *endpoint != peer {
                    let _ = socket.recv();
                    drop(socket);
                    drop(sets);
                    continue;
                }
            }
            drop(socket);
            drop(sets);

            match copied_len {
                Ok((size, endpoint, truncated)) => {
                    if truncated {
                        self.inner.lock().last_msg_flags |= 0x20;
                    } else {
                        self.inner.lock().last_msg_flags = 0;
                    }
                    return (Ok(size), Endpoint::Ip(endpoint));
                }
                Err(smoltcp::Error::Exhausted) => {
                    if read_closed {
                        return (Ok(0), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
                    }
                    drain_net_poll(4);
                    if non_block {
                        return (Err(LxError::EAGAIN), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
                    }
                }
                Err(err) => {
                    error!("udp socket peek_slice error: {:?}", err);
                    return (
                        Err(LxError::ENOTCONN),
                        Endpoint::Ip(IpEndpoint::UNSPECIFIED),
                    );
                }
            }
            if let Err(e) = crate::process::check_and_deliver_tty_interrupt() {
                return (Err(e), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
            }
            // Also honor non-TTY signals (SIGTERM/SIGALRM/...) so a blocked
            // recvfrom returns EINTR, matching the icmp socket path.
            if let Err(e) = crate::process::check_signals() {
                return (Err(e), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
            }
            kernel_hal::deferred_job::drain_deferred_jobs();
            // Park on the RX IRQ waker (5 ms fallback) instead of busy-spinning
            // — an idle udhcpc blocked on recvfrom otherwise pegs a core.
            kernel_hal::net::NetRxOrTimeoutFuture::new(5).await;
        }
    }
    /// write from buffer
    fn write(&self, data: &[u8], sendto_endpoint: Option<Endpoint>) -> SysResult {
        info!("udp write");
        let inner = self.inner.lock();
        let remote_endpoint = {
            if let Some(Endpoint::Ip(ref endpoint)) = sendto_endpoint {
                endpoint
            } else if let Some(ref endpoint) = inner.remote_endpoint {
                endpoint
            } else {
                return Err(LxError::ENOTCONN);
            }
        };
        if !Self::endpoint_matches_family(inner.ipv6, remote_endpoint) {
            return Err(LxError::EINVAL);
        }

        let sets = get_sockets();
        let mut sets = sets.lock();
        Self::autobind(&mut sets, inner.handle.0, inner.ipv6)?;
        let sent = sets
            .get::<UdpSocket>(inner.handle.0)
            .send_slice(data, *remote_endpoint);
        drop(sets);
        drop(inner);
        flush_socket_egress();

        match sent {
            Ok(()) => Ok(data.len()),
            Err(err) => {
                debug!("udp send_slice failed: {:?}", err);
                Err(send_error(err))
            }
        }
    }
    /// connect
    async fn connect(&self, endpoint: Endpoint) -> SysResult {
        if let Endpoint::Ip(ip) = endpoint {
            let is_ipv6 = self.inner.lock().ipv6;
            if !Self::endpoint_matches_family(is_ipv6, &ip) {
                return Err(LxError::EINVAL);
            }
            let handle = self.inner.lock().handle.0;
            let sockets = get_sockets();
            let mut set = sockets.lock();
            Self::autobind(&mut set, handle, is_ipv6)?;
            drop(set);

            self.inner.lock().remote_endpoint = Some(ip);
            Ok(0)
        } else {
            Err(LxError::EINVAL)
        }
    }
    /// wait for some event on a file descriptor
    fn poll(&self, events: PollEvents) -> (bool, bool, bool) {
        //poll_ifaces();

        let inner = self.inner.lock();
        let (recv_state, send_state) = {
            let sets = get_sockets();
            let mut sets = sets.lock();
            let socket = sets.get::<UdpSocket>(inner.handle.0);
            (socket.can_recv(), socket.can_send())
        };
        if (events.contains(PollEvents::IN) && !recv_state)
            || (events.contains(PollEvents::OUT) && !send_state)
        {
            crate::net::drain_net_tick();
        }

        let sets = get_sockets();
        let mut sets = sets.lock();
        let socket = sets.get::<UdpSocket>(inner.handle.0);
        let state = poll_state(
            socket.is_open(),
            socket.can_recv(),
            socket.can_send(),
            inner.read_closed,
        );
        debug!("udp poll: {:?}", state);
        state
    }

    fn bind(&self, endpoint: Endpoint) -> SysResult {
        info!("udp bind");
        #[allow(irrefutable_let_patterns)]
        if let Endpoint::Ip(mut ip) = endpoint {
            // Copy out of `inner` before locking SOCKETS (inner->SOCKETS is
            // the order everywhere else; the old `set.get(self.inner.lock()..)`
            // nested them the other way round).
            let (handle, is_ipv6) = {
                let inner = self.inner.lock();
                (inner.handle.0, inner.ipv6)
            };
            if !Self::endpoint_matches_family(is_ipv6, &ip) {
                return Err(LxError::EINVAL);
            }
            let sockets = get_sockets();
            let mut set = sockets.lock();
            // `inet_bind`: a socket that already has a port is `EINVAL`, not
            // "address in use".
            if set.get::<UdpSocket>(handle).is_open() {
                return Err(LxError::EINVAL);
            }
            if ip.port == 0 {
                ip.port = free_ephemeral_port(&set, ip.addr).ok_or(LxError::EADDRINUSE)?;
            } else if udp_port_taken(&set, ip) {
                return Err(LxError::EADDRINUSE);
            }
            let bound = set.get::<UdpSocket>(handle).bind(ip);
            drop(set);
            match bound {
                Ok(()) => {
                    crate::net::drain_net_urgent();
                    Ok(0)
                }
                Err(e) => {
                    warn!("udp bind failed: {:?}", e);
                    Err(LxError::EINVAL)
                }
            }
        } else {
            Err(LxError::EINVAL)
        }
    }
    /// `inet_listen`: only a stream socket listens.
    fn listen(&self) -> SysResult {
        Err(LxError::EOPNOTSUPP)
    }
    /// `inet_shutdown` on a datagram socket: the sides are recorded whatever
    /// the state, and the call reports `ENOTCONN` when there is no peer
    /// (`TCP_CLOSE`), `0` otherwise. Used to be `EINVAL` for everything.
    fn shutdown(&self, howto: usize) -> SysResult {
        let (rd, wr) = shutdown_sides(howto)?;
        let mut inner = self.inner.lock();
        inner.read_closed |= rd;
        inner.write_closed |= wr;
        if inner.remote_endpoint.is_none() {
            return Err(LxError::ENOTCONN);
        }
        Ok(0)
    }
    /// `inet_accept`: a datagram socket has no accept.
    async fn accept(&self) -> LxResult<(Arc<dyn FileLike>, Endpoint)> {
        Err(LxError::EOPNOTSUPP)
    }
    fn endpoint(&self) -> Option<Endpoint> {
        // Copy handle/ipv6 out of `inner` and drop the guard before locking the
        // global socket set. The hot paths (poll/write) take inner->SOCKETS;
        // locking SOCKETS->inner here is an ABBA inversion that deadlocks two
        // threads sharing one fd (spinlocks).
        let (handle, ipv6) = {
            let inner = self.inner.lock();
            (inner.handle.0, inner.ipv6)
        };
        let net_sockets = get_sockets();
        let mut sockets = net_sockets.lock();
        let socket = sockets.get::<UdpSocket>(handle);
        let ep = socket.endpoint();
        let addr = if ep.addr.is_unspecified() {
            if ipv6 {
                IpAddress::Ipv6(Ipv6Address::UNSPECIFIED)
            } else {
                IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)
            }
        } else {
            ep.addr
        };
        Some(Endpoint::Ip(IpEndpoint::new(addr, ep.port)))
    }
    fn remote_endpoint(&self) -> Option<Endpoint> {
        let inner = self.inner.lock();
        inner.remote_endpoint.map(|ep| {
            let addr = if ep.addr.is_unspecified() {
                if inner.ipv6 {
                    IpAddress::Ipv6(Ipv6Address::UNSPECIFIED)
                } else {
                    IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)
                }
            } else {
                ep.addr
            };
            Endpoint::Ip(IpEndpoint::new(addr, ep.port))
        })
    }
    fn setsockopt(&self, _level: usize, _opt: usize, _data: &[u8]) -> SysResult {
        // Accept harmlessly, like TCP's fallthrough and the `Socket` default.
        // debug, not warn: this is a no-op success, and a client that sets an
        // option on every send (Firefox's resolver does) turned it into a
        // steady stream of kernel warnings -- expensive on a serial console
        // and misleading, since nothing is failing.
        debug!("udp setsockopt: accepted as a no-op");
        Ok(0)
    }

    fn ioctl(&self, request: usize, arg1: usize, arg2: usize, arg3: usize) -> SysResult {
        // trace, not warn: this fires on every SIOCGIFFLAGS/ifconfig poll and
        // floods the console during a download, burying the [tcp read] STALL
        // diagnostic that actually matters.
        trace!("UdpSocket: ioctl request={:#x}, arg1={:#x}", request, arg1);
        let ipv6 = self.inner.lock().ipv6;
        handle_net_ioctl(request, arg1, arg2, arg3, ipv6)
    }

    fn get_buffer_capacity(&self) -> Option<(usize, usize)> {
        // Read the handle and drop the `inner` guard before locking SOCKETS to
        // preserve the inner->SOCKETS order (avoids the ABBA deadlock).
        let handle = self.inner.lock().handle.0;
        let sockets = get_sockets();
        let mut set = sockets.lock();
        let socket = set.get::<UdpSocket>(handle);
        let (recv_ca, send_ca) = (
            socket.payload_recv_capacity(),
            socket.payload_send_capacity(),
        );
        Some((recv_ca, send_ca))
    }

    fn socket_type(&self) -> Option<SocketType> {
        Some(SocketType::SOCK_DGRAM)
    }

    fn take_msg_flags(&self) -> i32 {
        core::mem::replace(&mut self.inner.lock().last_msg_flags, 0)
    }
}

impl_kobject!(UdpSocketState);

#[async_trait]
impl FileLike for UdpSocketState {
    /// A socket reports `S_IFSOCK` to `fstat(2)`, as on Linux.
    fn metadata(&self) -> LxResult<rcore_fs::vfs::Metadata> {
        Ok(crate::fs::anon_metadata(
            rcore_fs::vfs::FileType::Socket,
            self.id() as usize,
        ))
    }

    fn flags(&self) -> OpenFlags {
        self.inner.lock().flags
    }

    fn set_flags(&self, f: OpenFlags) -> LxResult {
        let flags = &mut self.inner.lock().flags;

        // See fcntl, only O_APPEND, O_ASYNC, O_DIRECT, O_NOATIME, O_NONBLOCK
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
            hangup: self.hangup(),
        })
    }

    async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus> {
        let (mut read, mut write, mut error) = Socket::poll(self, events);
        let ready = (events.contains(PollEvents::IN) && read)
            || (events.contains(PollEvents::OUT) && write)
            || error;
        if !ready {
            kernel_hal::net::NetRxOrTimeoutFuture::new(5).await;
            (read, write, error) = Socket::poll(self, events);
        }
        Ok(PollStatus {
            read,
            write,
            error,
            hangup: self.hangup(),
        })
    }

    fn ioctl(&self, request: usize, arg1: usize, arg2: usize, arg3: usize) -> LxResult<usize> {
        Socket::ioctl(self, request, arg1, arg2, arg3)
    }

    fn dup(&self) -> Arc<dyn FileLike> {
        Arc::new(Self {
            base: KObjectBase::new(),
            inner: self.inner.clone(),
        })
    }

    fn as_socket(&self) -> LxResult<&dyn Socket> {
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    //! The datagram socket against `udp.c`/`af_inet.c`, on the host: the
    //! socket states live in the real global smoltcp set, and a loopback
    //! interface built here carries datagrams between two of them. Every
    //! test takes `LOCK`: the set is shared, and `Interface::poll` on it
    //! moves every socket's packets.

    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::vec::Vec;
    use lock::Mutex as TestMutex;
    use smoltcp::iface::{Interface, InterfaceBuilder, Routes};
    use smoltcp::phy::{Loopback, Medium};
    use smoltcp::time::Instant;
    use smoltcp::wire::IpCidr;

    static LOCK: TestMutex<()> = TestMutex::new(());

    fn v4(port: u16) -> Endpoint {
        Endpoint::Ip(IpEndpoint::new(
            IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
            port,
        ))
    }

    fn lo(port: u16) -> Endpoint {
        Endpoint::Ip(IpEndpoint::new(IpAddress::v4(127, 0, 0, 1), port))
    }

    fn sock() -> UdpSocketState {
        let s = UdpSocketState::new(false).unwrap();
        FileLike::set_flags(&s, OpenFlags::NON_BLOCK).unwrap();
        s
    }

    fn loopback() -> Interface<'static, Loopback> {
        InterfaceBuilder::new(Loopback::new(Medium::Ip))
            .ip_addrs([IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8)])
            .routes(Routes::new(BTreeMap::new()))
            .finalize()
    }

    /// Move whatever is queued through the loopback.
    fn deliver(iface: &mut Interface<'static, Loopback>) {
        let sets = get_sockets();
        let mut sets = sets.lock();
        for ms in 0..8 {
            let _ = iface.poll(&mut sets, Instant::from_millis(ms));
        }
    }

    fn recv(s: &UdpSocketState, buf: &mut [u8]) -> (SysResult, Endpoint) {
        async_std::task::block_on(Socket::read(s, buf))
    }

    fn peek(s: &UdpSocketState, buf: &mut [u8]) -> (SysResult, Endpoint) {
        async_std::task::block_on(Socket::peek(s, buf))
    }

    #[test]
    fn a_fresh_socket_polls_writable_and_not_in_error() {
        let _g = LOCK.lock();
        assert_eq!(poll_state(false, false, false, false), (false, true, false));
        assert_eq!(poll_state(true, false, true, false), (false, true, false));
        assert_eq!(poll_state(true, true, true, false), (true, true, false));
        // shutdown(SHUT_RD): readable (EOF) whether or not there is a port.
        assert_eq!(poll_state(false, false, false, true), (true, true, false));
        assert_eq!(poll_state(true, false, false, true), (true, false, false));
        let s = sock();
        assert_eq!(
            Socket::poll(&s, PollEvents::all()),
            (false, true, false),
            "an unbound UDP socket used to poll as POLLERR"
        );
    }

    #[test]
    fn a_port_another_socket_holds_is_in_use_and_binding_twice_is_einval() {
        let _g = LOCK.lock();
        let a = sock();
        let b = sock();
        assert_eq!(Socket::bind(&a, v4(40010)), Ok(0));
        assert_eq!(
            Socket::bind(&b, v4(40010)),
            Err(LxError::EADDRINUSE),
            "smoltcp lets two sockets share a port; Linux does not"
        );
        // The wildcard collides with a specific address and vice versa...
        assert_eq!(Socket::bind(&b, lo(40010)), Err(LxError::EADDRINUSE));
        let c = sock();
        assert_eq!(Socket::bind(&c, lo(40011)), Ok(0));
        let d = sock();
        assert_eq!(Socket::bind(&d, v4(40011)), Err(LxError::EADDRINUSE));
        // ...but two different specific addresses coexist, as on Linux.
        assert_eq!(
            Socket::bind(
                &d,
                Endpoint::Ip(IpEndpoint::new(IpAddress::v4(127, 0, 0, 2), 40011))
            ),
            Ok(0)
        );
        // `inet_bind` on a socket that already has a port: EINVAL, whether
        // the new port is free or its own.
        assert_eq!(Socket::bind(&a, v4(40012)), Err(LxError::EINVAL));
        assert_eq!(Socket::bind(&a, v4(40010)), Err(LxError::EINVAL));
        // Dropping the holder frees the port.
        drop(a);
        assert_eq!(Socket::bind(&b, v4(40010)), Ok(0));
    }

    /// Walk the ephemeral allocator round its range until its next answer
    /// will be `port`.
    fn rewind_allocator_to(port: u16) {
        let before = if port == 49152 { 65534 } else { port - 1 };
        for _ in 0..(2 * 16384) {
            if get_ephemeral_port() == before {
                return;
            }
        }
        panic!("allocator never came round to {}", before);
    }

    fn port_of(s: &UdpSocketState) -> u16 {
        match Socket::endpoint(s) {
            Some(Endpoint::Ip(ep)) => ep.port,
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn an_ephemeral_port_skips_what_is_bound() {
        let _g = LOCK.lock();
        // Pin the next 40 ephemeral numbers by binding them explicitly, wind
        // the allocator back so its next answer is the first of them, and
        // ask for one: it must not be any of those.
        let mut held = Vec::new();
        let mut taken = Vec::new();
        for _ in 0..40 {
            let port = get_ephemeral_port();
            let s = sock();
            if Socket::bind(&s, v4(port)).is_ok() {
                taken.push(port);
                held.push(s);
            }
        }
        assert!(taken.len() >= 30, "could not pin the range: {:?}", taken);
        let first = taken[0];
        rewind_allocator_to(first);
        let sets = get_sockets();
        let sets = sets.lock();
        let picked = free_ephemeral_port(&sets, IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)).unwrap();
        assert!(!taken.contains(&picked), "picked {} which is bound", picked);
        assert!(!udp_port_taken(
            &sets,
            IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED), picked)
        ));
        drop(sets);
        // An implicit bind (first sendto) lands on a free one too...
        rewind_allocator_to(first);
        let s = sock();
        assert_eq!(Socket::write(&s, b"x", Some(lo(40013))), Ok(1));
        let mine = port_of(&s);
        assert!(!taken.contains(&mine), "autobind took bound port {}", mine);
        assert!(mine >= 49152);
        // ...and so does an explicit bind to port 0.
        rewind_allocator_to(first);
        let z = sock();
        assert_eq!(Socket::bind(&z, v4(0)), Ok(0));
        let zp = port_of(&z);
        assert!(!taken.contains(&zp), "bind(0) took bound port {}", zp);
    }

    #[test]
    fn a_send_that_cannot_be_queued_names_its_reason() {
        let _g = LOCK.lock();
        assert_eq!(send_error(smoltcp::Error::Exhausted), LxError::EAGAIN);
        assert_eq!(send_error(smoltcp::Error::Truncated), LxError::EMSGSIZE);
        assert_eq!(send_error(smoltcp::Error::Unaddressable), LxError::EINVAL);
        assert_eq!(send_error(smoltcp::Error::Illegal), LxError::EIO);
        let s = sock();
        // Port 0: `udp_sendmsg` says EINVAL; this used to be EIO.
        assert_eq!(Socket::write(&s, b"x", Some(lo(0))), Err(LxError::EINVAL));
        // Bigger than the socket buffer can ever hold: EMSGSIZE, and the
        // socket is fine afterwards.
        let big = alloc::vec![0u8; UDP_SENDBUF + 1];
        assert_eq!(
            Socket::write(&s, &big, Some(lo(40014))),
            Err(LxError::EMSGSIZE)
        );
        assert_eq!(Socket::write(&s, b"still fine", Some(lo(40014))), Ok(10));
        // No peer and no address: ENOTCONN, before any of that.
        assert_eq!(Socket::write(&s, b"x", None), Err(LxError::ENOTCONN));
        // A full transmit ring (nothing drains it here) is EAGAIN, which the
        // syscall layer turns into a wait on a blocking socket.
        let chunk = alloc::vec![1u8; 16 * 1024];
        let mut last = Ok(0);
        for _ in 0..(UDP_METADATA_BUF + 8) {
            last = Socket::write(&s, &chunk, Some(lo(40014)));
            if last.is_err() {
                break;
            }
        }
        assert_eq!(last, Err(LxError::EAGAIN));
        // Known gap: smoltcp's `can_send` only counts packets, not payload
        // bytes, so POLLOUT stays on with a payload ring this full and a
        // blocking sender re-tries on the 5 ms poll tick instead of
        // sleeping until room appears.
        assert!(Socket::poll(&s, PollEvents::OUT).1);
    }

    #[test]
    fn a_datagram_crosses_the_loopback_and_a_short_read_reports_msg_trunc() {
        let _g = LOCK.lock();
        let mut iface = loopback();
        let a = sock();
        let b = sock();
        assert_eq!(Socket::bind(&b, v4(40020)), Ok(0));
        assert_eq!(Socket::write(&a, b"hola mundo", Some(lo(40020))), Ok(10));
        deliver(&mut iface);
        assert!(Socket::poll(&b, PollEvents::IN).0);
        let mut buf = [0u8; 4];
        // peek does not consume, and reports truncation like recv.
        let (n, from) = peek(&b, &mut buf);
        assert_eq!(n, Ok(4));
        assert_eq!(&buf, b"hola");
        assert_eq!(Socket::take_msg_flags(&b), 0x20, "MSG_TRUNC");
        let a_port = match Socket::endpoint(&a) {
            Some(Endpoint::Ip(ep)) => ep.port,
            other => panic!("{:?}", other),
        };
        assert!(
            matches!(from, Endpoint::Ip(ep) if ep == IpEndpoint::new(IpAddress::v4(127, 0, 0, 1), a_port))
        );
        let mut whole = [0u8; 64];
        let (n, _) = recv(&b, &mut whole);
        assert_eq!(n, Ok(10));
        assert_eq!(&whole[..10], b"hola mundo");
        assert_eq!(
            Socket::take_msg_flags(&b),
            0,
            "a whole datagram is not truncated"
        );
        assert_eq!(recv(&b, &mut whole).0, Err(LxError::EAGAIN));
        // recv reports the truncation too, and the rest of the datagram is
        // gone with it (a datagram is not a stream).
        assert_eq!(Socket::write(&a, b"segundo", Some(lo(40020))), Ok(7));
        deliver(&mut iface);
        assert_eq!(recv(&b, &mut buf).0, Ok(4));
        assert_eq!(&buf, b"segu");
        assert_eq!(Socket::take_msg_flags(&b), 0x20, "MSG_TRUNC on recv");
        assert_eq!(recv(&b, &mut whole).0, Err(LxError::EAGAIN));
    }

    #[test]
    fn a_connected_socket_only_hears_its_peer() {
        let _g = LOCK.lock();
        let mut iface = loopback();
        let peer = sock();
        let stranger = sock();
        let c = sock();
        assert_eq!(Socket::bind(&peer, v4(40030)), Ok(0));
        assert_eq!(Socket::bind(&stranger, v4(40031)), Ok(0));
        assert_eq!(Socket::bind(&c, v4(40032)), Ok(0));
        assert_eq!(
            async_std::task::block_on(Socket::connect(&c, lo(40030))),
            Ok(0)
        );
        assert_eq!(Socket::write(&stranger, b"psst", Some(lo(40032))), Ok(4));
        assert_eq!(Socket::write(&peer, b"hi", Some(lo(40032))), Ok(2));
        deliver(&mut iface);
        let mut buf = [0u8; 8];
        let (n, from) = recv(&c, &mut buf);
        assert_eq!(n, Ok(2), "the stranger's datagram is dropped");
        assert_eq!(&buf[..2], b"hi");
        assert!(
            matches!(from, Endpoint::Ip(ep) if ep == IpEndpoint::new(IpAddress::v4(127, 0, 0, 1), 40030))
        );
        assert_eq!(recv(&c, &mut buf).0, Err(LxError::EAGAIN));
        // A connected socket writes without an address, to its peer.
        assert_eq!(Socket::write(&c, b"back", None), Ok(4));
        deliver(&mut iface);
        assert_eq!(recv(&peer, &mut buf).0, Ok(4));
    }

    #[test]
    fn shutdown_is_validated_reports_notconn_and_reads_as_eof_after_the_queue() {
        let _g = LOCK.lock();
        assert_eq!(shutdown_sides(0), Ok((true, false)));
        assert_eq!(shutdown_sides(1), Ok((false, true)));
        assert_eq!(shutdown_sides(2), Ok((true, true)));
        assert_eq!(shutdown_sides(3), Err(LxError::EINVAL));
        let mut iface = loopback();
        let a = sock();
        let b = sock();
        assert_eq!(Socket::bind(&b, v4(40040)), Ok(0));
        assert_eq!(Socket::write(&a, b"queued", Some(lo(40040))), Ok(6));
        deliver(&mut iface);
        // Unconnected: ENOTCONN, but the side is shut all the same.
        assert_eq!(Socket::shutdown(&b, 3), Err(LxError::EINVAL));
        assert_eq!(Socket::shutdown(&b, 0), Err(LxError::ENOTCONN));
        let mut buf = [0u8; 16];
        assert_eq!(
            recv(&b, &mut buf).0,
            Ok(6),
            "what was queued still comes out"
        );
        assert_eq!(recv(&b, &mut buf).0, Ok(0), "then EOF, not EAGAIN");
        assert_eq!(peek(&b, &mut buf).0, Ok(0));
        assert!(
            Socket::poll(&b, PollEvents::IN).0,
            "RCV_SHUTDOWN is readable"
        );
        assert!(!FileLike::poll(&b, PollEvents::IN).unwrap().hangup);
        assert_eq!(Socket::shutdown(&b, 1), Err(LxError::ENOTCONN));
        assert!(
            FileLike::poll(&b, PollEvents::IN).unwrap().hangup,
            "both sides shut"
        );
        // Connected: 0.
        let c = sock();
        assert_eq!(
            async_std::task::block_on(Socket::connect(&c, lo(40040))),
            Ok(0)
        );
        assert_eq!(Socket::shutdown(&c, 2), Ok(0));
    }

    #[test]
    fn listen_and_accept_are_not_supported_on_a_datagram_socket() {
        let _g = LOCK.lock();
        let s = sock();
        assert_eq!(Socket::listen(&s), Err(LxError::EOPNOTSUPP));
        assert!(matches!(
            async_std::task::block_on(Socket::accept(&s)),
            Err(LxError::EOPNOTSUPP)
        ));
    }
}
