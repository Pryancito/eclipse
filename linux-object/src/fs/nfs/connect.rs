//! Opening the connection, **synchronously**, and the seam that lets the
//! handshake above it be tested without one.
//!
//! The transport of the previous batch takes a socket that is already
//! connected, because connecting is a wait of its own: its own timeout, its
//! own states and its own error mapping, and none of it belongs in `send`.
//! This is that wait, and it is the same shape as everything else in this
//! client — a loop that drives the stack by hand and comes back whatever the
//! clock says — because the caller is a `mount(2)` that cannot `await`.
//!
//! # Why not `TcpSocketState::connect`
//!
//! Because it is `async`, and its blocking half
//! (`wait_for_handshake`) parks on `NetRxOrTimeoutFuture`. That `.await` is
//! the only asynchronous thing in it: the loop around it already drains the
//! stack by hand. So this is that loop with the park replaced by
//! [`kernel_hal::thread::yield_now_sync`], exactly as the transport's own
//! park was arrived at — and it answers with the same three errnos, for the
//! same three reasons, because a mount that fails differently from a `connect`
//! against the same server would be its own bug report.
//!
//! # Why a trait
//!
//! [`Connector`] exists so the handshake in [`greeting`](super::greeting) can
//! be measured. A `GETPORT` that asks for the wrong program, a `MNT` sent to
//! the NFS port, a mount that leaves the mount daemon's connection open — all
//! of those are mistakes about *which port is dialled and what is sent down
//! it*, and a test can only see them if making a connection is something it
//! can answer for. The real one dials TCP; the one in the tests hands back a
//! scripted server and writes down which port was asked for.

use alloc::vec;

use smoltcp::socket::{TcpSocket, TcpSocketBuffer, TcpState};
use smoltcp::wire::{IpAddress, IpEndpoint};

use zcore_drivers::net::get_sockets;

use crate::error::{LxError, LxResult};
use crate::net::GlobalSocketHandle;

use super::client::RpcTransport;
use super::transport::{KernelDriver, KernelTransport, StackDriver};

/// How long to wait for a TCP handshake before giving up.
///
/// The same thirty seconds `TcpSocketState::connect` waits, so a mount against
/// an unreachable server fails when a `connect(2)` against it would.
pub const CONNECT_TIMEOUT_MS: u64 = 30_000;

/// The most consecutive passes the handshake wait will make **without the
/// clock advancing at all**.
///
/// The third copy of the same exit, for the third loop whose only other way
/// out is the clock. See
/// [`MAX_RECV_PASSES`](super::transport::MAX_RECV_PASSES): a stalled timer
/// here would hold the thread that drives the network stack, so this loop
/// returns whatever the clock says and the mount fails rather than the
/// machine's networking stopping.
pub const MAX_CONNECT_PASSES: u32 = 5_000;

/// Makes a connection to one port on the server.
///
/// The handshake dials up to three ports — portmap, `mountd`, `nfsd` — and
/// which one it dials when is the thing worth measuring, so it goes through
/// here rather than through a function the tests cannot answer for.
pub trait Connector {
    /// What a connection is, once made.
    type Transport: RpcTransport;

    /// Connect to `port` on the server this connector was built for, taking
    /// no longer than `timeout_ms`.
    ///
    /// The timeout is an argument and not a property of the connector
    /// because the caller is spending a budget: the greeting has one
    /// deadline for all three of its steps, and a connection that may take
    /// the connector's own thirty seconds whatever is left of that budget is
    /// how three steps inside their own timeouts add up to a `mount(2)` that
    /// hangs for minutes. A connector may still impose its own ceiling.
    fn connect(&mut self, port: u16, timeout_ms: u64) -> LxResult<Self::Transport>;
}

/// The real one: a kernel TCP socket to a fixed address.
///
/// The address, not the hostname: resolving is `async` and happens on the
/// mount path above, which is also where a hostname that does not resolve is
/// reported. By the time anything here runs there is an address.
pub struct TcpConnector {
    /// The server.
    addr: IpAddress,
    /// How long one handshake may take.
    timeout_ms: u64,
}

impl TcpConnector {
    /// A connector to `addr`, waiting [`CONNECT_TIMEOUT_MS`] per handshake.
    pub fn new(addr: IpAddress) -> Self {
        Self {
            addr,
            timeout_ms: CONNECT_TIMEOUT_MS,
        }
    }

    /// Shorten the handshake timeout.
    pub fn with_timeout(mut self, ms: u64) -> Self {
        self.timeout_ms = ms;
        self
    }
}

impl Connector for TcpConnector {
    type Transport = KernelTransport<KernelDriver>;

    fn connect(&mut self, port: u16, timeout_ms: u64) -> LxResult<Self::Transport> {
        let mut driver = KernelDriver;
        // The caller's budget and this connector's ceiling, whichever is
        // shorter: the ceiling is what a connector built with
        // `with_timeout` promised, and the budget is what is left of the
        // caller's deadline.
        let ms = timeout_ms.min(self.timeout_ms);
        let handle = connect_sync(self.addr, port, ms, &mut driver)?;
        Ok(KernelTransport::new(handle))
    }
}

/// Open a TCP connection and wait for the handshake to finish.
///
/// Answers like `connect(2)` does, and for the same reasons:
///
/// - `ECONNREFUSED` when the peer answered with a reset, which moves the
///   socket from `SynSent` straight to `Closed`. Reported the moment it is
///   seen rather than by spinning to the deadline, because "nothing is
///   listening there" and "the server is unreachable" are different problems
///   and the person reading the mount's error has to tell them apart.
/// - `ETIMEDOUT` when the handshake simply never completes. The socket is
///   **aborted** before returning, as `tcp.rs` aborts it, so the next attempt
///   starts a new handshake instead of inheriting this one's.
/// - `EADDRNOTAVAIL` when there is no free local port, and `ENOMEM` when the
///   socket set is full, which is what registering a socket answers.
pub(crate) fn connect_sync<D: StackDriver>(
    addr: IpAddress,
    port: u16,
    timeout_ms: u64,
    driver: &mut D,
) -> LxResult<GlobalSocketHandle> {
    let socket = TcpSocket::new(
        TcpSocketBuffer::new(vec![0u8; crate::net::TCP_RECVBUF]),
        TcpSocketBuffer::new(vec![0u8; crate::net::TCP_SENDBUF]),
    );
    let handle = crate::net::register_smoltcp_socket(socket)?;

    // The local port is picked and used under one hold of the socket set, so
    // no other socket can take it in between -- the same reason `tcp.rs`
    // gives for doing it that way, and the same race if it is not.
    {
        let sockets = get_sockets();
        let mut sets = sockets.lock();
        let local = crate::net::tcp::free_ephemeral_port(&sets, IpAddress::Unspecified)
            .ok_or(LxError::EADDRNOTAVAIL)?;
        sets.get::<TcpSocket>(handle.inner())
            .connect((addr, port), IpEndpoint::new(IpAddress::Unspecified, local))
            .map_err(crate::net::tcp::connect_error)?;
    }

    wait_for_handshake(&handle, timeout_ms, driver)?;
    Ok(handle)
}

/// The waiting half: drive the stack until the socket is connected, refused,
/// or out of time.
fn wait_for_handshake<D: StackDriver>(
    handle: &GlobalSocketHandle,
    timeout_ms: u64,
    driver: &mut D,
) -> LxResult<()> {
    let deadline = now_ms().saturating_add(timeout_ms);
    let mut stalls = 0u32;
    let mut seen = now_ms();
    loop {
        driver.drive();

        let sockets = get_sockets();
        let state = {
            let mut sets = sockets.lock();
            let socket = sets.get::<TcpSocket>(handle.inner());
            socket.state()
        };
        match state {
            TcpState::Established => return Ok(()),
            // A reset in answer to the SYN: nothing is listening on that
            // port. `SynSent -> Closed` is how smoltcp reports it, and
            // waiting out the deadline would turn "refused" into "timed out".
            TcpState::Closed | TcpState::TimeWait => {
                abort(handle, driver);
                return Err(LxError::ECONNREFUSED);
            }
            // Still handshaking, or in a state this wait does not interpret.
            _ => {}
        }

        let before = now_ms();
        if before >= deadline {
            // Given up on rather than left behind: a socket still in
            // `SynSent` would keep retransmitting its SYN, and the next
            // attempt would be a second handshake to the same place.
            abort(handle, driver);
            return Err(LxError::ETIMEDOUT);
        }
        kernel_hal::thread::yield_now_sync();
        let after = now_ms();
        let newest = if before > after { before } else { after };
        if newest > seen {
            seen = newest;
            stalls = 0;
        } else {
            stalls += 1;
            if stalls >= MAX_CONNECT_PASSES {
                abort(handle, driver);
                return Err(LxError::ETIMEDOUT);
            }
        }
    }
}

/// Reset the socket and push the reset out.
fn abort<D: StackDriver>(handle: &GlobalSocketHandle, driver: &mut D) {
    {
        let sockets = get_sockets();
        let mut sets = sockets.lock();
        sets.get::<TcpSocket>(handle.inner()).abort();
    }
    driver.drive();
}

fn now_ms() -> u64 {
    kernel_hal::timer::timer_now().as_millis() as u64
}

#[cfg(test)]
#[path = "connect_tests.rs"]
mod connect_tests;
