//! Opening the connection, **synchronously**, and the seam that lets the
//! handshake above it be tested without one.
//!
//! The connection is a stream socket of the network stack that runs as a
//! process ([`served`](super::served)): the network every program of this
//! machine uses is the one a mount uses.
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

use zcore_drivers::wire::IpAddress;

use crate::error::LxResult;

use super::client::RpcTransport;
use super::served::ServedTransport;

/// How long to wait for a TCP handshake before giving up.
///
/// The thirty seconds a `connect(2)` waits, so a mount against an
/// unreachable server fails when a `connect(2)` against it would.
pub const CONNECT_TIMEOUT_MS: u64 = 30_000;

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

/// The real one: a served TCP socket to a fixed address.
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
    type Transport = ServedTransport;

    fn connect(&mut self, port: u16, timeout_ms: u64) -> LxResult<Self::Transport> {
        // The caller's budget and this connector's ceiling, whichever is
        // shorter: the ceiling is what a connector built with
        // `with_timeout` promised, and the budget is what is left of the
        // caller's deadline.
        let ms = timeout_ms.min(self.timeout_ms);
        let link = crate::net::user_stack::machine();
        ServedTransport::connect(link, self.addr, port, ms)
    }
}
