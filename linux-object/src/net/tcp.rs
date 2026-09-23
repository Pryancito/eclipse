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
use smoltcp::socket::{SocketSet, TcpSocket, TcpSocketBuffer, TcpState};
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
    /// Pending `SO_ERROR` errno (cleared by getsockopt); 0 = none.
    pending_error: i32,
    /// Nonblocking connect() returned EINPROGRESS; cleared on success/failure.
    connect_in_progress: bool,
    /// True once the socket reached Established (for RST → ECONNRESET).
    was_connected: bool,
    /// The port this socket registered in `BIND_TABLE` (bind, or the
    /// autobind of `listen`); released on drop. Accepted children share the
    /// listener's port without a registration of their own.
    bound: Option<IpEndpoint>,
    /// `SO_REUSEADDR`.
    reuse_addr: bool,
    /// `shutdown(SHUT_RD)`: reads drain what is queued and then report EOF.
    read_closed: bool,
}

impl Drop for TcpInner {
    fn drop(&mut self) {
        // A listening socket reserves its port in LISTEN_TABLE. `shutdown()`
        // clears it, but a plain `close()`/process-exit just drops this object;
        // without this Drop the port would leak and stay permanently EADDRINUSE
        // (the classic "restart the server after a crash" failure). TcpInner is
        // the shared Arc payload, so this runs only once the last reference
        // (including any dup()'d fds) is gone. `unlisten` is idempotent, so it is
        // harmless if `shutdown()` already cleared the entry.
        if self.is_listening {
            if let Some(ep) = self.local_endpoint {
                crate::net::LISTEN_TABLE.unlisten(ep.port);
            }
        }
        if let Some(ep) = self.bound {
            crate::net::BIND_TABLE.release(ep);
        }
    }
}

/// One socket that holds a port, as `inet_csk_bind_conflict` sees it.
#[derive(Clone, Copy, Debug)]
struct PortHolder {
    endpoint: IpEndpoint,
    listening: bool,
    reuse_addr: bool,
}

/// `inet_csk_bind_conflict` without `SO_REUSEPORT`: `want` collides with a
/// holder of the same port on an overlapping address, unless both ends set
/// `SO_REUSEADDR` and the holder is not listening. That last clause is what
/// lets a server restart while its old connections linger.
fn bind_conflict<I: IntoIterator<Item = PortHolder>>(
    holders: I,
    want: IpEndpoint,
    reuse_addr: bool,
) -> bool {
    holders.into_iter().any(|holder| {
        endpoints_collide(holder.endpoint, want)
            && !(reuse_addr && holder.reuse_addr && !holder.listening)
    })
}

/// Every TCP port holder: the sockets bound but not yet open (only
/// `BIND_TABLE` knows them) and the sockets open in smoltcp (listening,
/// connecting, connected, or in TIME-WAIT; their `SO_REUSEADDR` is not
/// recorded in the set, so they are taken to have it, which only ever lets
/// a bind through that Linux would refuse, never the reverse). smoltcp
/// itself lets any number of sockets listen on or connect from one port and
/// hands each segment to the first that accepts it. A CLOSED socket is
/// skipped for clarity only: smoltcp clears its endpoints on the next
/// dispatch, and a listener closed by `shutdown` is still in `BIND_TABLE`.
fn tcp_port_taken(set: &SocketSet<'_>, want: IpEndpoint, reuse_addr: bool) -> bool {
    let bound = crate::net::BIND_TABLE
        .snapshot()
        .into_iter()
        .map(|b| PortHolder {
            endpoint: b.endpoint,
            listening: false,
            reuse_addr: b.reuse_addr,
        });
    let open = set.iter().filter_map(|socket| match socket {
        smoltcp::socket::Socket::Tcp(tcp) if tcp.state() != TcpState::Closed => Some(PortHolder {
            endpoint: tcp.local_endpoint(),
            listening: tcp.is_listening(),
            reuse_addr: true,
        }),
        _ => None,
    });
    bind_conflict(bound.chain(open), want, reuse_addr)
}

/// An ephemeral port no TCP socket holds on `addr`, or `None` when the
/// dynamic range is exhausted. `get_ephemeral_port` alone is a counter that
/// does not look at what is bound: `bind(0)` and `connect` landed on a port
/// a daemon listened on, and the listener answered the SYN-ACK with a RST.
/// The search never leans on `SO_REUSEADDR` (`inet_csk_find_open_port`
/// with `relax` off): a port shared that way is a poor pick.
fn free_ephemeral_port(set: &SocketSet<'_>, addr: IpAddress) -> Option<u16> {
    const RANGE: usize = 65535 - 49152;
    (0..RANGE)
        .map(|_| get_ephemeral_port())
        .find(|&port| !tcp_port_taken(set, IpEndpoint::new(addr, port), false))
}

/// smoltcp refuses to connect to port 0; on Linux the SYN goes out and the
/// answer is a RST, so the caller sees `ECONNREFUSED`, not `ENOBUFS`.
fn connect_error(e: smoltcp::Error) -> LxError {
    match e {
        smoltcp::Error::Unaddressable => LxError::ECONNREFUSED,
        smoltcp::Error::Illegal => LxError::EISCONN,
        _ => LxError::ENOBUFS,
    }
}

/// A `setsockopt` integer: four native-endian bytes, or a lone byte.
fn sockopt_int(data: &[u8]) -> LxResult<u32> {
    if data.len() >= 4 {
        Ok(u32::from_ne_bytes([data[0], data[1], data[2], data[3]]))
    } else if let Some(&b) = data.first() {
        Ok(b as u32)
    } else {
        Err(LxError::EINVAL)
    }
}

/// Build a TCP socket with delayed ACK disabled.
///
/// smoltcp's default is a 10 ms delayed ACK. Combined with a 1-segment window
/// (slow start after loss, or the previous e1000e `max_burst_size` clamp that
/// wrapped the advertised window to ~28 KiB) that is exactly 1 MSS / 10 ms =
/// 1.2 Mbps. Immediate ACKs let the peer's congestion window open at line rate.
fn new_tcp_socket(
    rx: TcpSocketBuffer<'static>,
    tx: TcpSocketBuffer<'static>,
) -> TcpSocket<'static> {
    let mut socket = TcpSocket::new(rx, tx);
    socket.set_ack_delay(None);
    socket
}

/// Fallback park (ms) for a blocking socket wait loop, adaptive on how many
/// consecutive polls have come back with no data.
///
/// TCP is self-clocked: the peer sends the next window only after it sees our
/// ACK, and we emit that ACK from `drain_net_urgent()` at the top of each loop
/// iteration. A *fixed* 5 ms park therefore stretches the effective RTT to ≥5 ms
/// (worse under KVM timer jitter, where the "5 ms" timer routinely fires far
/// later), and at a 1-segment window (slow start after a slow-start-burst drop)
/// that is exactly the residual "1 MSS / RTT ≈ 1.2 Mbps" cap — and the stretched
/// RTT also stops the peer's congestion window from ever recovering.
///
/// So park *tight* (1 ms) while a transfer is active: during a real download
/// `recv_slice` returns data within an iteration or two, so `consecutive_empty`
/// stays low and the ACK clock runs fast, which both raises throughput and lets
/// cwnd climb. Only once a connection has sat genuinely idle (many empty polls
/// in a row — an idle `irssi`, a kept-alive HTTP socket) does it relax back to
/// 5 ms, preserving the fix that stopped an idle recv from pegging a core.
#[inline]
fn adaptive_park_ms(consecutive_empty: u32) -> u64 {
    match consecutive_empty {
        0..=7 => 1,
        8..=31 => 2,
        _ => 5,
    }
}

impl TcpSocketState {
    /// missing documentation
    pub fn new(ipv6: bool) -> LxResult<Self> {
        let rx_buffer = TcpSocketBuffer::new(vec![0; TCP_RECVBUF]);
        let tx_buffer = TcpSocketBuffer::new(vec![0; TCP_SENDBUF]);
        let socket = new_tcp_socket(rx_buffer, tx_buffer);
        let handle = super::register_smoltcp_socket(socket)?;

        Ok(TcpSocketState {
            base: KObjectBase::new(),
            inner: Arc::new(Mutex::new(TcpInner {
                handle,
                local_endpoint: None,
                is_listening: false,
                flags: OpenFlags::RDWR,
                ipv6,
                pending_error: 0,
                connect_in_progress: false,
                was_connected: false,
                bound: None,
                reuse_addr: false,
                read_closed: false,
            })),
        })
    }

    fn endpoint_matches_family(ipv6: bool, ep: &IpEndpoint) -> bool {
        matches!(
            (ipv6, ep.addr),
            (true, IpAddress::Ipv6(_)) | (false, IpAddress::Ipv4(_))
        )
    }

    fn loopback_addr(ipv6: bool) -> IpAddress {
        if ipv6 {
            IpAddress::Ipv6(Ipv6Address::LOOPBACK)
        } else {
            IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1))
        }
    }

    /// `inet_csk_get_port(sk, 0)`: take a free ephemeral port for a socket
    /// that has not bound. Caller holds `inner` and the socket set.
    fn autobind(inner: &mut TcpInner, set: &SocketSet<'_>) -> LxResult<IpEndpoint> {
        let port = free_ephemeral_port(set, IpAddress::Unspecified).ok_or(LxError::EADDRINUSE)?;
        let ep = IpEndpoint::new(IpAddress::Unspecified, port);
        crate::net::BIND_TABLE.insert(ep, inner.reuse_addr);
        inner.local_endpoint = Some(ep);
        inner.bound = Some(ep);
        Ok(ep)
    }
}

#[async_trait]
impl Socket for TcpSocketState {
    /// read to buffer
    async fn read(&self, data: &mut [u8]) -> (SysResult, Endpoint) {
        let (handle, flags, read_closed) = {
            let inner = self.inner.lock();
            (inner.handle.0, inner.flags, inner.read_closed)
        };
        debug!(
            "tcp read handle={} req_len={} nonblock={}",
            handle,
            data.len(),
            flags.contains(OpenFlags::NON_BLOCK)
        );
        let deadline = kernel_hal::timer::timer_now() + core::time::Duration::from_secs(120);
        let mut empty_polls: u32 = 0;
        loop {
            // Drive the NIC FIRST so any deferred RX is in the socket before
            // recv_slice is called. Use the UNTHROTTLED drain here: this is a
            // blocking read actively waiting for data, so we must pull RX as
            // fast as it arrives. The throttled tick (every 4–32 ms) lets a
            // fast download overflow the e1000e RX ring (~384 KiB fills in a
            // few ms on a real link) before we drain it — packets drop and the
            // large transfer wedges, while small ones that fit the ring work.
            // The aggressive poll stops the instant recv_slice returns data.
            kernel_hal::deferred_job::drain_deferred_jobs();
            crate::net::drain_net_urgent();

            let sets = get_sockets();
            let mut sets = sets.lock();
            let mut socket = sets.get::<TcpSocket>(handle);

            let state = socket.state();

            let mut copied_len = socket.recv_slice(data);
            if let Ok(0) = copied_len {
                if !data.is_empty() {
                    copied_len = Err(smoltcp::Error::Exhausted);
                }
            }

            // Receive-half EOF must be decided by smoltcp's `may_recv()`, NOT by
            // the raw TCP state. `may_recv()` stays true while the peer can still
            // send — ESTABLISHED, and crucially FIN-WAIT-1/FIN-WAIT-2, where WE
            // closed our transmit half but the peer keeps streaming (exactly what
            // an HTTP client does: it `shutdown(SHUT_WR)`s after sending the
            // request, then reads the response body). It only goes false once the
            // peer's FIN has been received AND the receive buffer is drained
            // (CLOSE-WAIT/CLOSING/LAST-ACK/TIME-WAIT/CLOSED with no buffered data).
            //
            // The previous code treated FIN-WAIT-2 as "peer closed" and returned
            // EOF the moment `recv_slice` was momentarily Exhausted mid-transfer,
            // which truncated downloads intermittently (apk then RSA-verified a
            // short APKINDEX -> "BAD signature").
            // `tcp_recvmsg` after `shutdown(SHUT_RD)`: what is queued is still
            // handed out, then EOF instead of waiting.
            let recv_closed = !socket.may_recv() || read_closed;
            trace!(
                "[tcp read] state={:?} recv_closed={} result={:?}",
                state,
                recv_closed,
                copied_len
            );
            drop(socket);
            drop(sets);

            // Receive half closed and nothing left to read -> real EOF.
            if recv_closed {
                if let Err(smoltcp::Error::Exhausted) = copied_len {
                    // Log the terminal state at error! (survives LOG=error) so a
                    // truncated download (peer FIN/RST mid-transfer -> apk gets a
                    // short APKINDEX) is visible in dmesg: state names WHY the
                    // receive half closed (a clean CLOSE-WAIT after all data, vs
                    // a RST/abort). Fires at most once per connection close.
                    error!(
                        "[tcp read] EOF handle={} state={:?} may_recv=false (recv half closed)",
                        handle, state
                    );
                    return (Ok(0), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
                }
            }

            match copied_len {
                Err(smoltcp::Error::Exhausted) => {
                    if flags.contains(OpenFlags::NON_BLOCK) {
                        return (Err(LxError::EAGAIN), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
                    }
                    // Hard timeout: avoid blocking forever if the peer goes
                    // silent. Return ETIMEDOUT, NOT Ok(0): a length-0 read is
                    // an orderly-shutdown (EOF) signal, so faking it on a still-
                    // open connection makes the caller believe the peer closed
                    // cleanly and treat a truncated transfer as complete (this
                    // is exactly the "short APKINDEX -> BAD signature" class of
                    // corruption). An error lets the caller retry/fail correctly.
                    if kernel_hal::timer::timer_now() >= deadline {
                        let (may, rq, sq) = {
                            let s = get_sockets();
                            let mut s = s.lock();
                            let sk = s.get::<TcpSocket>(handle);
                            (sk.may_recv(), sk.recv_queue(), sk.send_queue())
                        };
                        error!(
                            "[tcp read] deadline exceeded -> ETIMEDOUT handle={} state={:?} may_recv={} recv_queue={} send_queue={} ({})",
                            handle, state, may, rq, sq,
                            if rq > 65536 {
                                "CONSUMER-BLOCKED (app not reading / downstream stall)"
                            } else if rq == 0 && may {
                                "PEER-SILENT (window open, no data: lost ACK/window-update)"
                            } else {
                                "OTHER"
                            }
                        );
                        return (
                            Err(LxError::ETIMEDOUT),
                            Endpoint::Ip(IpEndpoint::UNSPECIFIED),
                        );
                    }
                }
                Ok(size) => {
                    crate::net::drain_net_urgent();
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
                    // Illegal (and similar aborts): receive half torn down by
                    // RST/reset — ECONNRESET, not ENOTCONN.
                    error!("Tcp socket read error: {:?}", err);
                    return (
                        Err(LxError::ECONNRESET),
                        Endpoint::Ip(IpEndpoint::UNSPECIFIED),
                    );
                }
            }
            if let Err(e) = crate::process::check_and_deliver_tty_interrupt() {
                return (Err(e), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
            }
            // Also honor non-TTY signals (SIGTERM/SIGALRM/...) so a blocked
            // recv returns EINTR, matching the udp/raw/icmp socket paths.
            if let Err(e) = crate::process::check_signals() {
                return (Err(e), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
            }
            // Park until the NIC's RX IRQ wakes us (immediate on data) or a
            // short fallback timer fires, instead of busy-spinning with
            // yield_now — which pegged a core at 100% for any socket blocked in
            // recv (e.g. an idle irssi). The fallback still drives poll_ifaces
            // if a wake is ever missed, so a stalled wake can never freeze the
            // op the way a pure timer/IRQ park once did. The park is adaptive
            // (see `adaptive_park_ms`): 1 ms while a transfer is active so the
            // ACK self-clock isn't stretched (the residual ~1.2 Mbps cap),
            // relaxing to 5 ms only once the connection has sat idle.
            empty_polls = empty_polls.saturating_add(1);
            kernel_hal::net::NetRxOrTimeoutFuture::new(adaptive_park_ms(empty_polls)).await;
        }
    }
    async fn peek(&self, data: &mut [u8]) -> (SysResult, Endpoint) {
        let (handle, flags, read_closed) = {
            let inner = self.inner.lock();
            (inner.handle.0, inner.flags, inner.read_closed)
        };
        let mut empty_polls: u32 = 0;
        loop {
            kernel_hal::deferred_job::drain_deferred_jobs();
            crate::net::drain_net_tick();

            let sets = get_sockets();
            let mut sets = sets.lock();
            let mut socket = sets.get::<TcpSocket>(handle);
            let mut copied_len = socket.peek_slice(data);
            if let Ok(0) = copied_len {
                if !data.is_empty() {
                    copied_len = Err(smoltcp::Error::Exhausted);
                }
            }
            drop(socket);
            drop(sets);
            match copied_len {
                Err(smoltcp::Error::Exhausted) => {
                    if read_closed {
                        return (Ok(0), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
                    }
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
                        Err(LxError::ECONNRESET),
                        Endpoint::Ip(IpEndpoint::UNSPECIFIED),
                    );
                }
            }
            if let Err(e) = crate::process::check_and_deliver_tty_interrupt() {
                return (Err(e), Endpoint::Ip(IpEndpoint::UNSPECIFIED));
            }
            // Park on the RX IRQ waker with an adaptive fallback — see read().
            empty_polls = empty_polls.saturating_add(1);
            kernel_hal::net::NetRxOrTimeoutFuture::new(adaptive_park_ms(empty_polls)).await;
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
        let deadline = kernel_hal::timer::timer_now() + core::time::Duration::from_secs(30);
        loop {
            let copied_len = {
                let sets = get_sockets();
                let mut sets = sets.lock();
                let mut socket = sets.get::<TcpSocket>(handle);
                socket.send_slice(data)
            };
            crate::net::drain_net_tick();

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
                    crate::net::drain_net_urgent();
                }
                Ok(size) => {
                    flush_socket_egress();
                    return Ok(size);
                }
                Err(err) => {
                    // smoltcp returns `Illegal` once the socket can no longer
                    // send: it has left Established/CloseWait, i.e. the peer
                    // reset or closed the connection mid-stream. The correct
                    // errno for a write on a torn-down connection is EPIPE
                    // ("broken pipe") — not ENOBUFS, whose "No buffer space
                    // available" text made TLS libraries report a bogus
                    // "handshake failed: No buffer space available".
                    warn!(
                        "[tcp write] send failed: {:?} (connection no longer sendable)",
                        err
                    );
                    return Err(LxError::EPIPE);
                }
            }
        }
    }
    /// connect
    async fn connect(&self, endpoint: Endpoint) -> SysResult {
        let (handle, ipv6, non_block) = {
            let inner = self.inner.lock();
            (
                inner.handle.0,
                inner.ipv6,
                inner.flags.contains(OpenFlags::NON_BLOCK),
            )
        };
        let Endpoint::Ip(mut ip) = endpoint else {
            error!("connect: bad endpoint");
            return Err(LxError::EINVAL);
        };
        if !Self::endpoint_matches_family(ipv6, &ip) {
            return Err(LxError::EINVAL);
        }
        // `ip_route_connect`: the unspecified address means this host.
        if ip.addr.is_unspecified() {
            ip.addr = Self::loopback_addr(ipv6);
        }

        {
            let sockets = get_sockets();
            let mut sets = sockets.lock();
            let socket = sets.get::<TcpSocket>(handle);
            // A connect() while the handshake is still in progress must return
            // EALREADY (POSIX), not EISCONN. Apps re-issue connect() to poll a
            // non-blocking connect to completion; EISCONN makes them believe the
            // socket is connected and write() on SynSent -> EPIPE.
            if matches!(socket.state(), TcpState::SynSent | TcpState::SynReceived) {
                return Err(LxError::EALREADY);
            }
            if socket.is_active() {
                return Err(LxError::EISCONN);
            }
        }

        // Honor a prior bind(): use its local endpoint; if unbound, take an
        // ephemeral port nobody holds (`inet_hash_connect`), chosen and used
        // under one hold of the set so no other socket can slip in between.
        let bound = self.inner.lock().local_endpoint;
        let connected = {
            let sockets = get_sockets();
            let mut sets = sockets.lock();
            let local_endpoint = match bound {
                Some(ep) => ep,
                None => {
                    let port = free_ephemeral_port(&sets, IpAddress::Unspecified)
                        .ok_or(LxError::EADDRNOTAVAIL)?;
                    IpEndpoint::new(IpAddress::Unspecified, port)
                }
            };
            let connected = sets
                .get::<TcpSocket>(handle)
                .connect(ip, local_endpoint)
                .map_err(connect_error);
            connected
        };
        connected?;

        // Reflect the endpoint smoltcp actually bound (addr may stay
        // unspecified until the stack picks a source).
        {
            let actual = get_sockets()
                .lock()
                .get::<TcpSocket>(handle)
                .local_endpoint();
            self.inner.lock().local_endpoint = Some(actual);
        }

        prepare_ipv4_stack();
        drain_net_poll(8);

        let state = get_sockets().lock().get::<TcpSocket>(handle).state();
        if matches!(state, TcpState::Established) {
            flush_socket_egress();
            let mut inner = self.inner.lock();
            inner.connect_in_progress = false;
            inner.was_connected = true;
            inner.pending_error = 0;
            return Ok(0);
        }
        if non_block {
            if matches!(state, TcpState::SynSent | TcpState::SynReceived) {
                self.inner.lock().connect_in_progress = true;
                return Err(LxError::EINPROGRESS);
            }
            if matches!(state, TcpState::Closed | TcpState::TimeWait) {
                self.inner.lock().pending_error = LxError::ECONNREFUSED as isize as i32;
                return Err(LxError::ECONNREFUSED);
            }
        }

        let deadline = kernel_hal::timer::timer_now() + core::time::Duration::from_secs(30);
        let mut empty_polls: u32 = 0;
        loop {
            drain_net_poll(4);
            kernel_hal::deferred_job::drain_deferred_jobs();

            match get_sockets().lock().get::<TcpSocket>(handle).state() {
                TcpState::SynSent | TcpState::SynReceived => {}
                TcpState::Established => {
                    flush_socket_egress();
                    let mut inner = self.inner.lock();
                    inner.connect_in_progress = false;
                    inner.was_connected = true;
                    inner.pending_error = 0;
                    return Ok(0);
                }
                TcpState::Closed | TcpState::TimeWait => {
                    // The peer refused the connection (RST moves SynSent->Closed).
                    // Report it immediately with the correct errno instead of
                    // spinning until the 30s deadline and returning ETIMEDOUT.
                    // Mirrors the non-blocking path above. ETIMEDOUT is still
                    // returned below for the SynSent/SynReceived no-response case.
                    let mut inner = self.inner.lock();
                    inner.connect_in_progress = false;
                    inner.pending_error = LxError::ECONNREFUSED as isize as i32;
                    return Err(LxError::ECONNREFUSED);
                }
                other => {
                    warn!("connect: unexpected state {:?}, retrying", other);
                }
            }

            if kernel_hal::timer::timer_now() >= deadline {
                warn!("connect: timed out after 30s");
                let mut inner = self.inner.lock();
                inner.connect_in_progress = false;
                inner.pending_error = LxError::ETIMEDOUT as isize as i32;
                return Err(LxError::ETIMEDOUT);
            }

            // Park on the RX IRQ waker (adaptive fallback) while the handshake
            // completes, rather than busy-spinning — see read().
            empty_polls = empty_polls.saturating_add(1);
            kernel_hal::net::NetRxOrTimeoutFuture::new(adaptive_park_ms(empty_polls)).await;
        }
    }
    /// wait for some event on a file descriptor
    fn poll(&self, events: PollEvents) -> (bool, bool, bool) {
        //poll_ifaces();
        let mut inner = self.inner.lock();
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
            crate::net::drain_net_tick();
        }

        let (mut read, mut write, mut error) = (false, false, false);

        let sets = get_sockets();
        let mut sets = sets.lock();
        let socket = sets.get::<TcpSocket>(inner.handle.0);

        if inner.is_listening {
            // A pending connection may already be in CloseWait (peer closed right
            // after connecting); signal POLLIN so accept() runs instead of the
            // listener silently wedging. Mirrors the accept() state check.
            read = matches!(socket.state(), TcpState::Established | TcpState::CloseWait);
        } else if !socket.is_open() {
            error = true;
            read = true;
            write = true;
            // Record SO_ERROR for a failed nonblocking connect or an RST.
            if inner.connect_in_progress {
                if inner.pending_error == 0 {
                    inner.pending_error = LxError::ECONNREFUSED as isize as i32;
                }
                inner.connect_in_progress = false;
            } else if inner.was_connected && inner.pending_error == 0 {
                inner.pending_error = LxError::ECONNRESET as isize as i32;
                inner.was_connected = false;
            }
        } else {
            if matches!(socket.state(), TcpState::Established) {
                inner.connect_in_progress = false;
                inner.was_connected = true;
            }
            if socket.can_recv() || inner.read_closed {
                read = true; // POLLIN
            } else {
                match socket.state() {
                    TcpState::CloseWait
                    | TcpState::Closing
                    | TcpState::LastAck
                    | TcpState::TimeWait => {
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
        let Endpoint::Ip(mut ip) = endpoint else {
            return Err(LxError::EINVAL);
        };
        let mut inner = self.inner.lock();
        if !Self::endpoint_matches_family(inner.ipv6, &ip) {
            return Err(LxError::EINVAL);
        }
        // `inet_bind`: a socket that already has a local port (bound,
        // connected, or accepted) cannot be bound again.
        if inner.local_endpoint.is_some() {
            return Err(LxError::EINVAL);
        }
        let sockets = get_sockets();
        let set = sockets.lock();
        if ip.port == 0 {
            ip.port = free_ephemeral_port(&set, ip.addr).ok_or(LxError::EADDRINUSE)?;
        } else if tcp_port_taken(&set, ip, inner.reuse_addr) {
            return Err(LxError::EADDRINUSE);
        }
        crate::net::BIND_TABLE.insert(ip, inner.reuse_addr);
        drop(set);
        inner.local_endpoint = Some(ip);
        inner.bound = Some(ip);
        inner.is_listening = false;
        Ok(0)
    }

    fn listen(&self) -> SysResult {
        let mut inner = self.inner.lock();
        if inner.is_listening {
            info!("It's already listening");
            return Ok(0);
        }

        let local_endpoint = {
            let sockets = get_sockets();
            let mut set = sockets.lock();
            // `inet_listen`: only a closed socket can start listening.
            if set.get::<TcpSocket>(inner.handle.0).is_open() {
                return Err(LxError::EINVAL);
            }
            match inner.local_endpoint {
                Some(ep) => ep,
                None => Self::autobind(&mut inner, &set)?,
            }
        };
        info!("socket listening on {:?}", local_endpoint);

        if !crate::net::LISTEN_TABLE.can_listen(local_endpoint.port) {
            return Err(LxError::EADDRINUSE);
        }

        get_sockets()
            .lock()
            .get::<TcpSocket>(inner.handle.0)
            .listen(local_endpoint)
            .map_err(|_| LxError::ENOBUFS)?;

        crate::net::LISTEN_TABLE.listen(local_endpoint)?;
        inner.is_listening = true;
        Ok(0)
    }

    fn shutdown(&self, howto: usize) -> SysResult {
        let (shut_rd, shut_wr) = shutdown_sides(howto)?;
        let mut inner = self.inner.lock();
        let sets = get_sockets();
        let mut sets = sets.lock();
        let mut socket = sets.get::<TcpSocket>(inner.handle.0);
        if inner.is_listening {
            // `inet_shutdown` on TCP_LISTEN: a read side stops the listener
            // (`tcp_disconnect`), SHUT_WR alone leaves it listening.
            if shut_rd {
                if let Some(ep) = inner.local_endpoint {
                    crate::net::LISTEN_TABLE.unlisten(ep.port);
                }
                inner.is_listening = false;
                socket.close();
            }
            return Ok(0);
        }
        if shut_rd {
            inner.read_closed = true;
        }
        // `inet_shutdown` on TCP_CLOSE: the sides are recorded, the call fails.
        if !socket.is_open() {
            return Err(LxError::ENOTCONN);
        }
        if shut_wr {
            socket.close();
        }
        Ok(0)
    }

    async fn accept(&self) -> LxResult<(Arc<dyn FileLike>, Endpoint)> {
        let (endpoint, non_block, is_ipv6) = {
            let inner = self.inner.lock();
            (
                inner.local_endpoint.ok_or(LxError::EINVAL)?,
                inner.flags.contains(OpenFlags::NON_BLOCK),
                inner.ipv6,
            )
        };

        loop {
            crate::net::drain_net_tick();
            kernel_hal::deferred_job::drain_deferred_jobs();

            let established = {
                let handle = self.inner.lock().handle.0;
                let sockets = get_sockets();
                let mut sockets = sockets.lock();
                let socket = sockets.get::<TcpSocket>(handle);
                // Accept any post-handshake connection, not only `Established`.
                // A peer that connects and immediately closes (health checks,
                // port scans) drives the listen socket Established->CloseWait
                // within a single `poll_ifaces()` batch, so the transient
                // `Established` is often never observed. If we only matched
                // `Established`, the listener swap below would never run: the
                // socket stays stuck in CloseWait (with a remote endpoint set,
                // so smoltcp rejects new SYNs) and the port goes permanently
                // deaf. The child accepted in CloseWait correctly delivers any
                // buffered data and then EOF.
                matches!(socket.state(), TcpState::Established | TcpState::CloseWait)
            };

            if established {
                let listen_handle = self.inner.lock().handle.0;
                let (local, remote) = {
                    let sockets = get_sockets();
                    let mut sockets = sockets.lock();
                    let socket = sockets.get::<TcpSocket>(listen_handle);
                    (socket.local_endpoint(), socket.remote_endpoint())
                };

                let rx_buffer = TcpSocketBuffer::new(super::kernel_vec_zeroed(super::TCP_RECVBUF)?);
                let tx_buffer = TcpSocketBuffer::new(super::kernel_vec_zeroed(super::TCP_SENDBUF)?);
                let mut new_listen = new_tcp_socket(rx_buffer, tx_buffer);
                new_listen.listen(endpoint).map_err(|_| LxError::ENOBUFS)?;

                let new_listen_handle = {
                    let sockets = get_sockets();
                    let mut sockets = sockets.lock();
                    // Respect the global socket-count cap that bounds the fixed
                    // kernel heap (each socket pins SEND/RECV buffers). accept()
                    // added directly via SocketSet::add and so bypassed the cap
                    // that register_smoltcp_socket() enforces for every other
                    // socket creation.
                    if super::smoltcp_socket_count(&sockets) >= super::MAX_SMOLTCIP_SOCKETS {
                        return Err(LxError::ENOBUFS);
                    }
                    sockets.add(new_listen)
                };

                let child_handle = {
                    let mut inner = self.inner.lock();
                    core::mem::replace(&mut inner.handle, GlobalSocketHandle(new_listen_handle))
                };

                let new_socket = Arc::new(TcpSocketState {
                    base: KObjectBase::new(),
                    inner: Arc::new(Mutex::new(TcpInner {
                        handle: child_handle,
                        local_endpoint: Some(local),
                        is_listening: false,
                        flags: OpenFlags::RDWR,
                        ipv6: is_ipv6,
                        pending_error: 0,
                        connect_in_progress: false,
                        was_connected: true,
                        bound: None,
                        reuse_addr: false,
                        read_closed: false,
                    })),
                });
                return Ok((new_socket as Arc<dyn FileLike>, Endpoint::Ip(remote)));
            } else {
                if non_block {
                    return Err(LxError::EAGAIN);
                }
                thread::sleep_until(
                    kernel_hal::timer::timer_now() + core::time::Duration::from_millis(5),
                )
                .await;
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
        // Copy what we need out of `inner` and DROP its guard before locking the
        // global socket set. Every hot path (poll/endpoint/read/connect) takes
        // inner->SOCKETS; taking SOCKETS->inner here is a lock-order inversion
        // that deadlocks two threads sharing one fd (these are spinlocks).
        let (handle, ipv6) = {
            let inner = self.inner.lock();
            (inner.handle.0, inner.ipv6)
        };
        let sets = get_sockets();
        let mut sets = sets.lock();
        let socket = sets.get::<TcpSocket>(handle);
        if socket.is_open() {
            let ep = socket.remote_endpoint();
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
        } else {
            None
        }
    }

    fn get_buffer_capacity(&self) -> Option<(usize, usize)> {
        // Read the handle and drop the `inner` guard before locking SOCKETS to
        // preserve the inner->SOCKETS order (avoids the ABBA deadlock).
        let handle = self.inner.lock().handle.0;
        let sockets = get_sockets();
        let mut set = sockets.lock();
        let socket = set.get::<TcpSocket>(handle);
        let (recv_ca, send_ca) = (socket.recv_capacity(), socket.send_capacity());
        Some((recv_ca, send_ca))
    }

    fn socket_type(&self) -> Option<SocketType> {
        Some(SocketType::SOCK_STREAM)
    }

    fn setsockopt(&self, level: usize, opt: usize, data: &[u8]) -> SysResult {
        const SOL_SOCKET: usize = 1;
        const SO_REUSEADDR: usize = 2;
        const IPPROTO_TCP: usize = 6;
        const TCP_NODELAY: usize = 1;
        if level == SOL_SOCKET && opt == SO_REUSEADDR {
            // Read at bind time (`sk_reuse`); set it before `bind`, as servers do.
            self.inner.lock().reuse_addr = sockopt_int(data)? != 0;
            return Ok(0);
        }
        if level == IPPROTO_TCP && opt == TCP_NODELAY {
            let optval = sockopt_int(data)?;
            // TCP_NODELAY disables Nagle; smoltcp's API is the inverse flag.
            let handle = self.inner.lock().handle.0;
            get_sockets()
                .lock()
                .get::<TcpSocket>(handle)
                .set_nagle_enabled(optval == 0);
            return Ok(0);
        }
        // Other options: accept harmlessly (same lenient default as Socket).
        Ok(0)
    }

    fn take_so_error(&self) -> i32 {
        let mut inner = self.inner.lock();
        // Opportunistically latch connect/RST failures before clearing.
        if inner.pending_error == 0 {
            let handle = inner.handle.0;
            let sets = get_sockets();
            let mut sets = sets.lock();
            let socket = sets.get::<TcpSocket>(handle);
            if !socket.is_open() {
                if inner.connect_in_progress {
                    inner.pending_error = LxError::ECONNREFUSED as isize as i32;
                    inner.connect_in_progress = false;
                } else if inner.was_connected {
                    inner.pending_error = LxError::ECONNRESET as isize as i32;
                    inner.was_connected = false;
                }
            } else if matches!(socket.state(), TcpState::Established) {
                inner.connect_in_progress = false;
                inner.was_connected = true;
            }
        }
        core::mem::replace(&mut inner.pending_error, 0)
    }
}

impl_kobject!(TcpSocketState);

#[async_trait]
impl FileLike for TcpSocketState {
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
            hangup: false,
        })
    }

    fn ioctl(&self, request: usize, arg1: usize, arg2: usize, arg3: usize) -> LxResult<usize> {
        let ipv6 = self.inner.lock().ipv6;
        handle_net_ioctl(request, arg1, arg2, arg3, ipv6)
    }

    fn as_socket(&self) -> LxResult<&dyn Socket> {
        Ok(self)
    }
}
#[cfg(test)]
mod transfer_bench {
    //! Host bench: a large TCP transfer over a smoltcp loopback interface using
    //! the *real* socket buffer sizes (TCP_RECVBUF/TCP_SENDBUF), with a
    //! deliberately slow reader so the receive buffer fills and the connection
    //! must ride the window / zero-window update path — exactly what separates a
    //! large download from a small one. A stall detector fails the test if no
    //! progress is made for a long time (a window-update deadlock), and the
    //! payload is verified byte-for-byte (corruption).
    //!
    //! QEMU x86_64 runs with `-nic none`, so this is the only place we can
    //! reproduce a large transfer off real hardware. If it passes, large-
    //! transfer flow-control with our config is sound and the bug is in the
    //! driver/integration; if it stalls or corrupts, we found it here.

    use alloc::collections::BTreeMap;
    use alloc::vec;
    use smoltcp::iface::{InterfaceBuilder, Routes};
    use smoltcp::phy::{Loopback, Medium};
    use smoltcp::socket::{SocketSet, TcpSocket, TcpSocketBuffer};
    use smoltcp::time::Instant;
    use smoltcp::wire::{IpAddress, IpCidr};

    fn mk_sock() -> TcpSocket<'static> {
        TcpSocket::new(
            TcpSocketBuffer::new(vec![0u8; crate::net::TCP_RECVBUF]),
            TcpSocketBuffer::new(vec![0u8; crate::net::TCP_SENDBUF]),
        )
    }

    fn run_transfer(total: usize, reader_chunk: usize) {
        let device = Loopback::new(Medium::Ip);
        let mut iface = InterfaceBuilder::new(device)
            .ip_addrs([IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8)])
            .routes(Routes::new(BTreeMap::new()))
            .finalize();

        let mut sockets = SocketSet::new(vec![]);
        let sh = sockets.add(mk_sock());
        let ch = sockets.add(mk_sock());

        sockets.get::<TcpSocket>(sh).listen(1234).unwrap();
        sockets
            .get::<TcpSocket>(ch)
            .connect((IpAddress::v4(127, 0, 0, 1), 1234), 49152)
            .unwrap();

        let mut sent = 0usize;
        let mut recvd = 0usize;
        let mut rbuf = vec![0u8; reader_chunk];
        let mut clock = 0i64;
        let mut idle = 0u64;

        while recvd < total {
            clock += 1; // advance 1 ms per poll so smoltcp timers progress
            let _ = iface.poll(&mut sockets, Instant::from_millis(clock));

            // Sender: push as much as the send window allows.
            {
                let mut s = sockets.get::<TcpSocket>(sh);
                while sent < total && s.can_send() {
                    let remaining = total - sent;
                    let mut chunk = vec![0u8; remaining.min(32 * 1024)];
                    for (j, b) in chunk.iter_mut().enumerate() {
                        *b = (sent + j) as u8;
                    }
                    match s.send_slice(&chunk) {
                        Ok(n) if n > 0 => sent += n,
                        _ => break,
                    }
                }
            }

            // Receiver: read at most `reader_chunk` per poll — the throttle that
            // keeps the rx buffer near-full and forces window updates.
            let mut progressed = false;
            {
                let mut c = sockets.get::<TcpSocket>(ch);
                if c.can_recv() {
                    if let Ok(n) = c.recv_slice(&mut rbuf) {
                        for i in 0..n {
                            assert_eq!(
                                rbuf[i],
                                (recvd + i) as u8,
                                "data corruption at byte {}",
                                recvd + i
                            );
                        }
                        if n > 0 {
                            recvd += n;
                            progressed = true;
                        }
                    }
                }
            }

            idle = if progressed { 0 } else { idle + 1 };
            assert!(
                idle < 5_000_000,
                "TRANSFER STALLED: recvd={} of {} (sent={}) — window-update deadlock",
                recvd,
                total,
                sent
            );
        }
        assert_eq!(recvd, total, "did not receive the whole stream");
    }

    /// 16 MiB through 2 MiB buffers with a 16 KiB/poll reader: the buffer fills
    /// ~8 times over and the window cycles closed/open continuously.
    #[test]
    fn large_transfer_slow_reader() {
        run_transfer(16 * 1024 * 1024, 16 * 1024);
    }

    /// A faster reader (256 KiB/poll) — should breeze through; guards against a
    /// regression where even unthrottled large transfers stall.
    #[test]
    fn large_transfer_fast_reader() {
        run_transfer(16 * 1024 * 1024, 256 * 1024);
    }
}

#[cfg(test)]
mod port_tests {
    //! `bind`/`listen`/`connect`/`shutdown` against `af_inet.c` and
    //! `inet_connection_sock.c`, on the host: the sockets live in the real
    //! global smoltcp set and a loopback interface built here carries the
    //! handshake between two of them. Every test takes `NET_TEST_LOCK`
    //! (shared with the UDP tests) and uses its own ports, 41010-41090.

    use super::*;
    use alloc::collections::BTreeMap;
    use smoltcp::iface::{Interface, InterfaceBuilder, Routes};
    use smoltcp::phy::{Loopback, Medium};
    use smoltcp::time::Instant;
    use smoltcp::wire::IpCidr;

    use crate::net::NET_TEST_LOCK as LOCK;

    fn ep(addr: IpAddress, port: u16) -> IpEndpoint {
        IpEndpoint::new(addr, port)
    }

    fn any(port: u16) -> IpEndpoint {
        ep(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED), port)
    }

    fn v4(port: u16) -> Endpoint {
        Endpoint::Ip(any(port))
    }

    fn lo(port: u16) -> Endpoint {
        Endpoint::Ip(ep(IpAddress::v4(127, 0, 0, 1), port))
    }

    fn holder(endpoint: IpEndpoint, listening: bool, reuse_addr: bool) -> PortHolder {
        PortHolder {
            endpoint,
            listening,
            reuse_addr,
        }
    }

    fn sock() -> TcpSocketState {
        let s = TcpSocketState::new(false).unwrap();
        FileLike::set_flags(&s, OpenFlags::NON_BLOCK).unwrap();
        s
    }

    fn reuse(s: &TcpSocketState) {
        assert_eq!(Socket::setsockopt(s, 1, 2, &1u32.to_ne_bytes()), Ok(0));
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
        for ms in 0..16 {
            let _ = iface.poll(&mut sets, Instant::from_millis(ms));
        }
    }

    fn state_of(s: &TcpSocketState) -> TcpState {
        let handle = s.inner.lock().handle.0;
        get_sockets().lock().get::<TcpSocket>(handle).state()
    }

    fn port_of(s: &TcpSocketState) -> u16 {
        match Socket::endpoint(s) {
            Some(Endpoint::Ip(ep)) => ep.port,
            other => panic!("{:?}", other),
        }
    }

    fn connect(s: &TcpSocketState, to: Endpoint) -> SysResult {
        async_std::task::block_on(Socket::connect(s, to))
    }

    fn read(s: &dyn Socket, buf: &mut [u8]) -> SysResult {
        async_std::task::block_on(s.read(buf)).0
    }

    /// A listener on `port`, a client connected to it through `iface`, and
    /// the accepted child.
    fn connected_pair(
        iface: &mut Interface<'static, Loopback>,
        port: u16,
    ) -> (TcpSocketState, TcpSocketState, Arc<dyn FileLike>) {
        let server = sock();
        assert_eq!(Socket::bind(&server, v4(port)), Ok(0));
        assert_eq!(Socket::listen(&server), Ok(0));
        let client = sock();
        assert_eq!(connect(&client, lo(port)), Err(LxError::EINPROGRESS));
        deliver(iface);
        assert_eq!(state_of(&client), TcpState::Established);
        let (child, from) = async_std::task::block_on(Socket::accept(&server)).unwrap();
        assert!(matches!(from, Endpoint::Ip(ep) if ep.port == port_of(&client)));
        // accept() hands out a blocking child; a test must never park.
        child.set_flags(OpenFlags::NON_BLOCK).unwrap();
        (server, client, child)
    }

    #[test]
    fn the_bind_conflict_rule_is_linuxs_without_reuseport() {
        let l = IpAddress::v4(127, 0, 0, 1);
        let other = IpAddress::v4(10, 0, 0, 1);
        let idle = |e| holder(e, false, false);
        // Same port: the wildcard overlaps everything, a specific address
        // only itself.
        assert!(bind_conflict([idle(any(80))], any(80), false));
        assert!(bind_conflict([idle(any(80))], ep(l, 80), false));
        assert!(bind_conflict([idle(ep(l, 80))], any(80), false));
        assert!(bind_conflict([idle(ep(l, 80))], ep(l, 80), false));
        assert!(!bind_conflict([idle(ep(l, 80))], ep(other, 80), false));
        assert!(!bind_conflict([idle(any(80))], any(81), false));
        assert!(!bind_conflict([], any(80), true));
        // SO_REUSEADDR must be on both sides, and the holder must not listen.
        assert!(!bind_conflict(
            [holder(any(80), false, true)],
            any(80),
            true
        ));
        assert!(bind_conflict(
            [holder(any(80), false, true)],
            any(80),
            false
        ));
        assert!(bind_conflict(
            [holder(any(80), false, false)],
            any(80),
            true
        ));
        assert!(bind_conflict([holder(any(80), true, true)], any(80), true));
        // One conflicting holder among many is enough.
        assert!(bind_conflict(
            [idle(any(79)), holder(any(80), true, true), idle(any(81))],
            any(80),
            true
        ));
    }

    #[test]
    fn binding_twice_is_einval_and_a_port_another_socket_holds_is_in_use() {
        let _g = LOCK.lock();
        let a = sock();
        assert_eq!(Socket::bind(&a, v4(41010)), Ok(0));
        assert_eq!(
            Socket::bind(&a, v4(41011)),
            Err(LxError::EINVAL),
            "inet_bind: a bound socket cannot be rebound"
        );
        assert_eq!(port_of(&a), 41010);
        let b = sock();
        assert_eq!(
            Socket::bind(&b, v4(41010)),
            Err(LxError::EADDRINUSE),
            "a bound socket is invisible to smoltcp; only the bind table knows it"
        );
        assert_eq!(Socket::bind(&b, lo(41010)), Err(LxError::EADDRINUSE));
        assert_eq!(Socket::bind(&b, v4(0)), Ok(0));
        assert!(port_of(&b) >= 49152);
        // The port is given back when the socket goes away, not before.
        let c = sock();
        assert_eq!(Socket::bind(&c, v4(41010)), Err(LxError::EADDRINUSE));
        drop(a);
        assert_eq!(Socket::bind(&c, v4(41010)), Ok(0));
        // A different specific address is free.
        let d = sock();
        assert_eq!(Socket::bind(&d, lo(41012)), Ok(0));
        let e = sock();
        assert_eq!(
            Socket::bind(&e, Endpoint::Ip(ep(IpAddress::v4(10, 0, 0, 1), 41012))),
            Ok(0)
        );
    }

    #[test]
    fn so_reuseaddr_lets_idle_sockets_share_a_port_but_never_a_listener() {
        let _g = LOCK.lock();
        let a = sock();
        let b = sock();
        reuse(&a);
        reuse(&b);
        assert_eq!(Socket::bind(&a, v4(41020)), Ok(0));
        assert_eq!(Socket::bind(&b, v4(41020)), Ok(0));
        let c = sock();
        assert_eq!(
            Socket::bind(&c, v4(41020)),
            Err(LxError::EADDRINUSE),
            "SO_REUSEADDR has to be set on both sides"
        );
        assert_eq!(Socket::listen(&a), Ok(0));
        assert_eq!(
            Socket::listen(&b),
            Err(LxError::EADDRINUSE),
            "two listeners on one port need SO_REUSEPORT"
        );
        let d = sock();
        reuse(&d);
        assert_eq!(
            Socket::bind(&d, v4(41020)),
            Err(LxError::EADDRINUSE),
            "a listener blocks the port even for SO_REUSEADDR"
        );
        // Off again is off.
        let e = sock();
        reuse(&e);
        assert_eq!(Socket::setsockopt(&e, 1, 2, &0u32.to_ne_bytes()), Ok(0));
        assert_eq!(Socket::bind(&e, v4(41021)), Ok(0));
        let f = sock();
        reuse(&f);
        assert_eq!(Socket::bind(&f, v4(41021)), Err(LxError::EADDRINUSE));
        assert_eq!(Socket::setsockopt(&f, 1, 2, &[]), Err(LxError::EINVAL));
        // Each binder holds its own registration: dropping the listener
        // leaves the idle sharer's.
        drop(a);
        let strict = sock();
        assert_eq!(Socket::bind(&strict, v4(41020)), Err(LxError::EADDRINUSE));
        drop(b);
        assert_eq!(Socket::bind(&strict, v4(41020)), Ok(0));
    }

    #[test]
    fn an_ephemeral_port_skips_a_listener_for_bind_and_for_connect() {
        let _g = LOCK.lock();
        let next = get_ephemeral_port();
        crate::net::rewind_ephemeral_port_to(next);
        let server = sock();
        assert_eq!(Socket::bind(&server, v4(next)), Ok(0));
        assert_eq!(Socket::listen(&server), Ok(0));
        // bind(0) would have taken `next` and hidden the listener.
        crate::net::rewind_ephemeral_port_to(next);
        let a = sock();
        assert_eq!(Socket::bind(&a, v4(0)), Ok(0));
        assert_ne!(port_of(&a), next);
        assert_eq!(port_of(&a), if next == 65534 { 49152 } else { next + 1 });
        // connect() would have used `next` as its source port and the
        // listener would have answered the SYN-ACK.
        crate::net::rewind_ephemeral_port_to(next);
        let other = sock();
        assert_eq!(Socket::bind(&other, v4(41030)), Ok(0));
        assert_eq!(Socket::listen(&other), Ok(0));
        let c = sock();
        assert_eq!(connect(&c, lo(41030)), Err(LxError::EINPROGRESS));
        assert_ne!(port_of(&c), next);
        assert_ne!(port_of(&c), port_of(&a));
        // A bound-but-idle socket is skipped too, even one that would
        // share with SO_REUSEADDR (inet_csk_find_open_port, relax off).
        crate::net::rewind_ephemeral_port_to(port_of(&a));
        let d = sock();
        assert_eq!(connect(&d, lo(41030)), Err(LxError::EINPROGRESS));
        assert_ne!(port_of(&d), port_of(&a));
        let used = [next, port_of(&a), port_of(&c), port_of(&d)];
        let p = loop {
            let p = get_ephemeral_port();
            if !used.contains(&p) {
                break p;
            }
        };
        let shared = sock();
        reuse(&shared);
        assert_eq!(Socket::bind(&shared, v4(p)), Ok(0));
        crate::net::rewind_ephemeral_port_to(p);
        let e = sock();
        reuse(&e);
        assert_eq!(Socket::bind(&e, v4(0)), Ok(0));
        assert_ne!(port_of(&e), p);
    }

    #[test]
    fn listen_autobinds_and_an_open_socket_can_neither_listen_nor_bind() {
        let _g = LOCK.lock();
        let mut iface = loopback();
        let server = sock();
        assert_eq!(
            Socket::listen(&server),
            Ok(0),
            "inet_listen binds an unbound socket to an ephemeral port"
        );
        let port = port_of(&server);
        assert!(port >= 49152, "{}", port);
        assert_eq!(Socket::listen(&server), Ok(0), "listen is idempotent");
        let taken = sock();
        assert_eq!(Socket::bind(&taken, v4(port)), Err(LxError::EADDRINUSE));

        let client = sock();
        assert_eq!(connect(&client, lo(port)), Err(LxError::EINPROGRESS));
        deliver(&mut iface);
        let (child, _) = async_std::task::block_on(Socket::accept(&server)).unwrap();
        let child = child.as_socket().unwrap();
        assert_eq!(child.listen(), Err(LxError::EINVAL));
        assert_eq!(Socket::listen(&client), Err(LxError::EINVAL));
        assert_eq!(Socket::bind(&client, v4(41040)), Err(LxError::EINVAL));
        assert_eq!(child.bind(v4(41040)), Err(LxError::EINVAL));
        assert!(port_of(&client) >= 49152);
        // A stopped listener stays bound (the autobind registered its port
        // like a bind would), so even SO_REUSEADDR cannot take it...
        assert_eq!(Socket::shutdown(&server, 0), Ok(0));
        let reuser = sock();
        reuse(&reuser);
        assert_eq!(Socket::bind(&reuser, v4(port)), Err(LxError::EADDRINUSE));
        // ...until the listener is dropped; its child does not hold it.
        drop(server);
        let again = sock();
        reuse(&again);
        assert_eq!(
            Socket::bind(&again, v4(port)),
            Ok(0),
            "a restarted server binds over its live connections with SO_REUSEADDR"
        );
        let strict = sock();
        assert_eq!(
            Socket::bind(&strict, lo(port)),
            Err(LxError::EADDRINUSE),
            "without it the established child still holds the port"
        );
    }

    #[test]
    fn a_connect_to_port_zero_is_refused_and_the_unspecified_address_is_this_host() {
        let _g = LOCK.lock();
        let mut iface = loopback();
        assert_eq!(
            connect_error(smoltcp::Error::Unaddressable),
            LxError::ECONNREFUSED
        );
        assert_eq!(connect_error(smoltcp::Error::Illegal), LxError::EISCONN);
        assert_eq!(connect_error(smoltcp::Error::Exhausted), LxError::ENOBUFS);
        let c = sock();
        assert_eq!(
            connect(&c, lo(0)),
            Err(LxError::ECONNREFUSED),
            "Linux sends the SYN to port 0 and gets a RST"
        );
        let server = sock();
        assert_eq!(Socket::bind(&server, v4(41050)), Ok(0));
        assert_eq!(Socket::listen(&server), Ok(0));
        assert_eq!(
            connect(&c, v4(41050)),
            Err(LxError::EINPROGRESS),
            "0.0.0.0 routes to loopback (ip_route_connect)"
        );
        deliver(&mut iface);
        assert_eq!(state_of(&c), TcpState::Established);
        assert!(matches!(
            Socket::remote_endpoint(&c),
            Some(Endpoint::Ip(ep)) if ep == IpEndpoint::new(IpAddress::v4(127, 0, 0, 1), 41050)
        ));
        assert_eq!(connect(&c, lo(41050)), Err(LxError::EISCONN));
    }

    #[test]
    fn shutdown_is_validated_reports_notconn_and_shut_rd_reads_as_eof_after_the_queue() {
        let _g = LOCK.lock();
        let mut iface = loopback();
        let idle = sock();
        assert_eq!(Socket::shutdown(&idle, 3), Err(LxError::EINVAL));
        assert_eq!(
            Socket::shutdown(&idle, 0),
            Err(LxError::ENOTCONN),
            "inet_shutdown on TCP_CLOSE"
        );

        let (_server, client, child) = connected_pair(&mut iface, 41060);
        let child = child.as_socket().unwrap();
        assert_eq!(Socket::write(&client, b"hola", None), Ok(4));
        deliver(&mut iface);
        assert_eq!(child.poll(PollEvents::all()), (true, true, false));
        assert_eq!(child.shutdown(0), Ok(0));
        let mut buf = [0u8; 8];
        // What arrived before SHUT_RD is still handed out (`tcp_recvmsg`),
        // then EOF where a non-blocking read used to say EAGAIN.
        assert_eq!(read(child, &mut buf), Ok(4));
        assert_eq!(&buf[..4], b"hola");
        assert_eq!(read(child, &mut buf), Ok(0));
        assert_eq!(async_std::task::block_on(child.peek(&mut buf)).0, Ok(0));
        assert_eq!(
            child.poll(PollEvents::all()),
            (true, true, false),
            "EOF is readable"
        );
        // The write side is untouched: the client still hears the child.
        assert_eq!(child.write(b"adios", None), Ok(5));
        deliver(&mut iface);
        assert_eq!(read(&client, &mut buf), Ok(5));
        assert_eq!(&buf[..5], b"adios");
        // SHUT_WR sends the FIN and the client reads EOF; its own write
        // side stays open.
        assert_eq!(child.shutdown(1), Ok(0));
        deliver(&mut iface);
        assert_eq!(read(&client, &mut buf), Ok(0));
        assert_eq!(Socket::write(&client, b"x", None), Ok(1));
    }

    #[test]
    fn shut_wr_leaves_a_listener_listening_and_shut_rd_stops_it() {
        let _g = LOCK.lock();
        let mut iface = loopback();
        let server = sock();
        assert_eq!(Socket::bind(&server, v4(41070)), Ok(0));
        assert_eq!(Socket::listen(&server), Ok(0));
        assert_eq!(Socket::shutdown(&server, 1), Ok(0));
        assert!(!crate::net::LISTEN_TABLE.can_listen(41070));
        let first = sock();
        assert_eq!(connect(&first, lo(41070)), Err(LxError::EINPROGRESS));
        deliver(&mut iface);
        assert_eq!(state_of(&first), TcpState::Established);
        assert!(async_std::task::block_on(Socket::accept(&server)).is_ok());

        assert_eq!(Socket::shutdown(&server, 0), Ok(0));
        assert!(crate::net::LISTEN_TABLE.can_listen(41070));
        assert_eq!(state_of(&server), TcpState::Closed);
        let second = sock();
        assert_eq!(connect(&second, lo(41070)), Err(LxError::EINPROGRESS));
        deliver(&mut iface);
        assert_eq!(state_of(&second), TcpState::Closed, "nobody listens: RST");
        // The client the RST closed no longer holds its port (smoltcp
        // clears the endpoints of a CLOSED socket, as tcp_set_state does).
        crate::net::rewind_ephemeral_port_to(port_of(&second));
        let third = sock();
        assert_eq!(connect(&third, lo(41071)), Err(LxError::EINPROGRESS));
        assert_eq!(port_of(&third), port_of(&second));
        // Still bound, so it can listen again and nobody else can bind there.
        assert_eq!(Socket::bind(&sock(), v4(41070)), Err(LxError::EADDRINUSE));
        assert_eq!(Socket::listen(&server), Ok(0));
        assert_eq!(Socket::shutdown(&server, 2), Ok(0));
        assert!(crate::net::LISTEN_TABLE.can_listen(41070));
    }
}
