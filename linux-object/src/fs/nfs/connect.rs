//! Opening the connection, **synchronously**, and the seam that lets the
//! handshake above it be tested without one.
//!
//! The connection is a stream socket of the network stack that runs as a
//! process ([`served`](super::served)): the network every program of this
//! machine uses is the one a mount uses.
//!
//! What a connector is, and the greeting's clock it keeps, is the `nfs3`
//! crate's ([`Connector`]); this is the kernel's.

use zcore_drivers::wire::IpAddress;

use super::{errno_of, Errno};

use super::served::ServedTransport;

pub use nfs3::connect::{Connector, CONNECT_TIMEOUT_MS};

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

    fn connect(&mut self, port: u16, timeout_ms: u64) -> Result<Self::Transport, Errno> {
        // The caller's budget and this connector's ceiling, whichever is
        // shorter: the ceiling is what a connector built with
        // `with_timeout` promised, and the budget is what is left of the
        // caller's deadline.
        let ms = timeout_ms.min(self.timeout_ms);
        let link = crate::net::user_stack::machine();
        ServedTransport::connect(link, self.addr, port, ms).map_err(errno_of)
    }

    fn now_ns(&self) -> u64 {
        kernel_hal::timer::timer_now().as_nanos() as u64
    }
}
