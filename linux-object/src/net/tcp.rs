// Tcpsocket

// crate
use crate::error::{LxError, LxResult};
use crate::fs::{FileLike, OpenFlags, PollStatus};
use crate::net::*;
use alloc::sync::Arc;
use kernel_hal::thread;
use lock::Mutex;

// alloc
use alloc::boxed::Box;
use alloc::vec;

// smoltcp
use smoltcp::socket::{TcpSocket, TcpSocketBuffer, TcpState};
use smoltcp::wire::{IpAddress, Ipv4Address, Ipv6Address};

// async
use async_trait::async_trait;

// third part
#[allow(unused_imports)]
use zircon_object::object::*;

/// TCP socket structure
pub struct TcpSocketState {
    /// Kernel object base
    base: KObjectBase,
    /// TcpSocket Inner
    inner: Arc<Mutex<TcpInner>>,
}

/// TCP socket inner
#[derive(Debug)]
pub struct TcpInner {
    /// missing documentation
    handle: GlobalSocketHandle,
    /// missing documentation
    local_endpoint: Option<IpEndpoint>, // save local endpoint for bind()
    /// missing documentation
    is_listening: bool,
    /// flags on the socket
    flags: OpenFlags,
    /// ipv6 domain socket flag
    ipv6: bool,
}

impl Default for TcpSocketState {
    fn default() -> Self {
        TcpSocketState::new(false)
    }
}

impl TcpSocketState {
    /// missing documentation
    pub fn new(ipv6: bool) -> Self {
        let rx_buffer = TcpSocketBuffer::new(vec![0; TCP_RECVBUF]);
        let tx_buffer = TcpSocketBuffer::new(vec![0; TCP_SENDBUF]);
        let socket = TcpSocket::new(rx_buffer, tx_buffer);
        let handle = GlobalSocketHandle(get_sockets().lock().add(socket));

        TcpSocketState {
            base: KObjectBase::new(),
            inner: Arc::new(Mutex::new(TcpInner {
                handle,
                local_endpoint: None,
                is_listening: false,
                flags: OpenFlags::RDWR,
                ipv6,
            })),
        }
    }
}

#[async_trait]
impl Socket for TcpSocketState {
    /// read to buffer
    async fn read(&self, data: &mut [u8]) -> (SysResult, Endpoint) {
        let (handle, flags) = {
            let inner = self.inner.lock();
            (inner.handle.0, inner.flags)
        };
        debug!(
            "tcp read handle={} req_len={} nonblock={}",
            handle,
            data.len(),
            flags.contains(OpenFlags::NON_BLOCK)
        );
        let deadline =
            kernel_hal::timer::timer_now() + core::time::Duration::from_secs(120);
        loop {
            // Drive the NIC FIRST so any deferred RX is in the socket before
            // recv_slice is called.
            kernel_hal::deferred_job::drain_deferred_jobs();
            poll_ifaces();

            let sets = get_sockets();
            let mut sets = sets.lock();
            let mut socket = sets.get::<TcpSocket>(handle);

            let state = socket.state();

            // Detect closed/reset connection BEFORE calling recv_slice.
            // When the peer sends RST or FIN, smoltcp transitions the socket
            // out of Established, but recv_slice may still return Exhausted
            // (empty RX buffer) instead of Finished, causing an infinite loop.
            let peer_closed = matches!(
                state,
                TcpState::Closed
                    | TcpState::CloseWait
                    | TcpState::TimeWait
                    | TcpState::FinWait2
            );

            let mut copied_len = socket.recv_slice(data);
            if let Ok(0) = copied_len {
                if !data.is_empty() {
                    copied_len = Err(smoltcp::Error::Exhausted);
                }
            }
            trace!("[tcp read] state={:?} result={:?}", state, copied_len);
            drop(socket);
            drop(sets);

            // If the peer has closed but recv_slice returned Exhausted (empty
            // buffer), treat it as EOF so callers don't loop forever.
            if peer_closed {
                if let Err(smoltcp::Error::Exhausted) = copied_len {
                    return (Ok(0), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
                }
            }

            match copied_len {
                Err(smoltcp::Error::Exhausted) => {
                    if flags.contains(OpenFlags::NON_BLOCK) {
                        return (Err(LxError::EAGAIN), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
                    }
                    // Hard timeout: avoid blocking forever if the peer goes silent.
                    if kernel_hal::timer::timer_now() >= deadline {
                        warn!("[tcp read] deadline exceeded, returning EOF");
                        return (Ok(0), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
                    }
                }
                Ok(size) => {
                    // We just freed RX buffer space via recv_slice. Drive the
                    // NIC again so smoltcp emits the window-update/ACK now,
                    // instead of on the next read() call. Without this, a peer
                    // sending more than one receive-window (TLS handshakes and
                    // large downloads exceed the 64 KiB window) can stall
                    // waiting for the window to reopen.
                    poll_ifaces();
                    let endpoint = get_sockets()
                        .lock()
                        .get::<TcpSocket>(handle)
                        .remote_endpoint();
                    return (Ok(size), Endpoint::Ip(endpoint));
                }
                Err(smoltcp::Error::Finished) => {
                    return (Ok(0), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
                }
                Err(err) => {
                    error!("Tcp socket read error: {:?}", err);
                    return (
                        Err(LxError::ENOTCONN),
                        Endpoint::Ip(IpEndpoint::UNSPECIFIED),
                    );
                }
            }
            if let Err(e) = crate::process::check_and_deliver_tty_interrupt() {
                return (Err(e), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
            }
            // Re-queue immediately instead of timer-sleeping. The timer-backed
            // wake (sleep_until / NetRxOrTimeoutFuture) does NOT resume a
            // blocking socket read in this executor — the task is parked and
            // never re-polled, which froze every TLS handshake (the client
            // sends ClientHello, then blocks reading ServerHello forever).
            // yield_now keeps the task runnable so the loop keeps driving
            // poll_ifaces and picks up RX data as soon as it lands.
            thread::yield_now().await;
        }
    }
    async fn peek(&self, data: &mut [u8]) -> (SysResult, Endpoint) {
        let (handle, flags) = {
            let inner = self.inner.lock();
            (inner.handle.0, inner.flags)
        };
        loop {
            kernel_hal::deferred_job::drain_deferred_jobs();
            poll_ifaces();

            let sets = get_sockets();
            let mut sets = sets.lock();
            let mut socket = sets.get::<TcpSocket>(handle);
            let state = socket.state();
            let mut copied_len = socket.peek_slice(data);
            if let Ok(0) = copied_len {
                if !data.is_empty() {
                    copied_len = Err(smoltcp::Error::Exhausted);
                }
            }
            log::warn!(
                "[tcp peek debug] data.len()={}, state={:?}, result={:?}",
                data.len(),
                state,
                copied_len
            );
            drop(socket);
            drop(sets);
            match copied_len {
                Err(smoltcp::Error::Exhausted) => {
                    if flags.contains(OpenFlags::NON_BLOCK) {
                        return (Err(LxError::EAGAIN), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
                    }
                }
                Ok(size) => {
                    let endpoint = get_sockets()
                        .lock()
                        .get::<TcpSocket>(handle)
                        .remote_endpoint();
                    return (Ok(size), Endpoint::Ip(endpoint));
                }
                Err(smoltcp::Error::Finished) => {
                    return (Ok(0), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
                }
                Err(err) => {
                    error!("Tcp socket peek error: {:?}", err);
                    return (
                        Err(LxError::ENOTCONN),
                        Endpoint::Ip(IpEndpoint::UNSPECIFIED),
                    );
                }
            }
            if let Err(e) = crate::process::check_and_deliver_tty_interrupt() {
                return (Err(e), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
            }
            // See read(): timer-backed wakes don't resume a blocking socket op
            // in this executor, so keep the task runnable via yield_now.
            thread::yield_now().await;
        }
    }
    /// write from buffer
    fn write(&self, data: &[u8], _sendto_endpoint: Option<Endpoint>) -> SysResult {
        let (handle, flags) = {
            let inner = self.inner.lock();
            (inner.handle.0, inner.flags)
        };
        if data.is_empty() {
            return Ok(0);
        }
        // Retry until at least one byte is queued. A full TX buffer returns
        // Ok(0); for a blocking socket we must keep draining ACKs (poll_ifaces)
        // and try again instead of returning a 0-length write, which makes
        // libc/busybox spin or treat the write as failed.
        let deadline =
            kernel_hal::timer::timer_now() + core::time::Duration::from_secs(30);
        loop {
            let copied_len = {
                let sets = get_sockets();
                let mut sets = sets.lock();
                let mut socket = sets.get::<TcpSocket>(handle);
                socket.send_slice(data)
            };
            poll_ifaces();

            match copied_len {
                Ok(0) => {
                    if flags.contains(OpenFlags::NON_BLOCK) {
                        return Err(LxError::EAGAIN);
                    }
                    if kernel_hal::timer::timer_now() >= deadline {
                        warn!("[tcp write] TX buffer full, deadline exceeded");
                        return Err(LxError::ENOBUFS);
                    }
                    // Synchronous trait: drain ACKs so the peer's window frees
                    // up TX buffer space before retrying.
                    kernel_hal::deferred_job::drain_deferred_jobs();
                    poll_ifaces();
                }
                Ok(size) => {
                    return Ok(size);
                }
                Err(err) => {
                    error!("Tcp socket write error: {:?}", err);
                    return Err(LxError::ENOBUFS);
                }
            }
        }
    }
    /// connect
    async fn connect(&self, endpoint: Endpoint) -> SysResult {
        let inner = self.inner.lock();
        #[allow(warnings)]
        if let Endpoint::Ip(ip) = endpoint {
            get_sockets()
                .lock()
                .get::<TcpSocket>(inner.handle.0)
                .connect(ip, get_ephemeral_port())
                .map_err(|_| LxError::ENOBUFS)?;

            // Use a 30-second wall-clock deadline. Each iteration sleeps 5ms,
            // so we have up to 6000 tries before giving up — plenty of time for
            // a real internet round-trip through QEMU slirp.
            let deadline = kernel_hal::timer::timer_now() + core::time::Duration::from_secs(30);
            loop {
                // Transmit pending packets (SYN, ACK, etc.)
                poll_ifaces();
                kernel_hal::deferred_job::drain_deferred_jobs();

                match get_sockets()
                    .lock()
                    .get::<TcpSocket>(inner.handle.0)
                    .state()
                {
                    TcpState::SynSent | TcpState::SynReceived => {
                        // still connecting — keep waiting
                    }
                    TcpState::Established => {
                        return Ok(0);
                    }
                    // Terminal failure states: RST received or connection refused
                    TcpState::Closed | TcpState::TimeWait => {
                        // Only give up if a meaningful amount of time has passed
                        // (the socket starts in Closed before SYN is sent, so we
                        // must not fail immediately on the very first iteration).
                        if kernel_hal::timer::timer_now() >= deadline {
                            warn!("connect: timed out after 30s");
                            return Err(LxError::ETIMEDOUT);
                        }
                        // Short initial delay then continue; the SYN may not have
                        // been transmitted yet.
                    }
                    _ => {
                        warn!("connect: unexpected state, retrying");
                    }
                }

                thread::sleep_until(kernel_hal::timer::timer_now() + core::time::Duration::from_millis(5)).await;

                if kernel_hal::timer::timer_now() >= deadline {
                    warn!("connect: timed out after 30s");
                    return Err(LxError::ETIMEDOUT);
                }
            }
        } else {
            error!("connect: bad endpoint");
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
            let socket = sets.get::<TcpSocket>(inner.handle.0);
            debug!(
                "tcp is_listening: {:?}, now tcp state: {:?}",
                inner.is_listening,
                socket.state()
            );

            (socket.can_recv(), socket.can_send())
        };
        if (events.contains(PollEvents::IN) && !recv_state)
            || (events.contains(PollEvents::OUT) && !send_state)
        {
            poll_ifaces();
        }

        let (mut read, mut write, mut error) = (false, false, false);

        let sets = get_sockets();
        let mut sets = sets.lock();
        let socket = sets.get::<TcpSocket>(inner.handle.0);

        if inner.is_listening {
            if let Some(ep) = inner.local_endpoint {
                if let Ok(true) = crate::net::LISTEN_TABLE.can_accept(ep.port) {
                    read = true;
                }
            }
        } else if !socket.is_open() {
            error = true;
            read = true;
            write = true;
        } else {
            if socket.can_recv() {
                read = true; // POLLIN
            } else {
                match socket.state() {
                    TcpState::CloseWait | TcpState::Closing | TcpState::LastAck | TcpState::TimeWait => {
                        read = true;
                    }
                    _ => {}
                }
            }
            if socket.can_send() {
                write = true; // POLLOUT
            }
        }
        debug!("tcp poll: {:?}", (read, write, error));
        (read, write, error)
    }

    fn bind(&self, endpoint: Endpoint) -> SysResult {
        let mut inner = self.inner.lock();
        if let Endpoint::Ip(mut ip) = endpoint {
            if ip.port == 0 {
                ip.port = get_ephemeral_port();
            }
            inner.local_endpoint = Some(ip);
            inner.is_listening = false;
            Ok(0)
        } else {
            Err(LxError::EINVAL)
        }
    }

    fn listen(&self) -> SysResult {
        let mut inner = self.inner.lock();
        if inner.is_listening {
            info!("It's already listening");
            return Ok(0);
        }

        let local_endpoint = inner.local_endpoint.ok_or(LxError::EINVAL)?;
        info!("socket listening on {:?}", local_endpoint);

        crate::net::LISTEN_TABLE.listen(local_endpoint)?;
        inner.is_listening = true;
        Ok(0)
    }

    fn shutdown(&self, howto: usize) -> SysResult {
        let mut inner = self.inner.lock();
        if inner.is_listening {
            if let Some(ep) = inner.local_endpoint {
                crate::net::LISTEN_TABLE.unlisten(ep.port);
            }
            inner.is_listening = false;
        }
        let sets = get_sockets();
        let mut sets = sets.lock();
        let mut socket = sets.get::<TcpSocket>(inner.handle.0);
        if howto == 1 || howto == 2 {
            socket.close();
        }
        Ok(0)
    }

    async fn accept(&self) -> LxResult<(Arc<dyn FileLike>, Endpoint)> {
        let inner = self.inner.lock();
        let endpoint = inner.local_endpoint.ok_or(LxError::EINVAL)?;
        let non_block = inner.flags.contains(OpenFlags::NON_BLOCK);
        let is_ipv6 = inner.ipv6;
        drop(inner);
        
        loop {
            if let Ok((handle, (local, remote))) = crate::net::LISTEN_TABLE.accept(endpoint.port) {
                let new_handle = GlobalSocketHandle(handle);
                let new_socket = Arc::new(TcpSocketState {
                    base: KObjectBase::new(),
                    inner: Arc::new(Mutex::new(TcpInner {
                        handle: new_handle,
                        local_endpoint: Some(local),
                        is_listening: false,
                        flags: OpenFlags::RDWR,
                        ipv6: is_ipv6,
                    })),
                });
                return Ok((
                    new_socket as Arc<dyn FileLike>,
                    Endpoint::Ip(remote),
                ));
            } else {
                if non_block {
                    return Err(LxError::EAGAIN);
                }
                poll_ifaces();
                kernel_hal::deferred_job::drain_deferred_jobs();
                thread::sleep_until(kernel_hal::timer::timer_now() + core::time::Duration::from_millis(5)).await;
            }
        }
    }

    fn endpoint(&self) -> Option<Endpoint> {
        let inner = self.inner.lock();
        let ep = inner.local_endpoint.unwrap_or_else(|| {
            let sets = get_sockets();
            let mut sets = sets.lock();
            let socket = sets.get::<TcpSocket>(inner.handle.0);
            socket.local_endpoint()
        });
        let addr = if ep.addr.is_unspecified() {
            if inner.ipv6 {
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
        let sets = get_sockets();
        let mut sets = sets.lock();
        let inner = self.inner.lock();
        let socket = sets.get::<TcpSocket>(inner.handle.0);
        if socket.is_open() {
            let ep = socket.remote_endpoint();
            let addr = if ep.addr.is_unspecified() {
                if inner.ipv6 {
                    IpAddress::Ipv6(Ipv6Address::UNSPECIFIED)
                } else {
                    IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)
                }
            } else {
                ep.addr
            };
            Some(Endpoint::Ip(IpEndpoint::new(addr, ep.port)))
        } else {
            None
        }
    }

    fn get_buffer_capacity(&self) -> Option<(usize, usize)> {
        let sockets = get_sockets();
        let mut set = sockets.lock();
        let socket = set.get::<TcpSocket>(self.inner.lock().handle.0);
        let (recv_ca, send_ca) = (socket.recv_capacity(), socket.send_capacity());
        Some((recv_ca, send_ca))
    }

    fn socket_type(&self) -> Option<SocketType> {
        Some(SocketType::SOCK_STREAM)
    }
}

impl_kobject!(TcpSocketState);

#[async_trait]
impl FileLike for TcpSocketState {
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
        Ok(PollStatus { read, write, error })
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
        Ok(PollStatus { read, write, error })
    }

    fn ioctl(&self, request: usize, arg1: usize, arg2: usize, arg3: usize) -> LxResult<usize> {
        handle_net_ioctl(request, arg1, arg2, arg3)
    }

    fn as_socket(&self) -> LxResult<&dyn Socket> {
        Ok(self)
    }
}
