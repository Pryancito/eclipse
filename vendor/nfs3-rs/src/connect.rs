//! The seam between the greeting and whatever makes connections: a TCP
//! socket of the kernel's network, one of a process's, or a scripted
//! server in a test.
//!
//! # Why a trait
//!
//! [`Connector`] exists so the handshake in [`greeting`](super::greeting) can
//! be measured. A `GETPORT` that asks for the wrong program, a `MNT` sent to
//! the NFS port, a mount that leaves the mount daemon's connection open — all
//! of those are mistakes about *which port is dialled and what is sent down
//! it*, and a test can only see them if making a connection is something it
//! can answer for. The real ones dial TCP; the one in the tests hands back a
//! scripted server and writes down which port was asked for.
//!
//! It is also the greeting's clock. The kernel reads its timer and a
//! process reads `CLOCK_MONOTONIC`; a test moves its own, so a step that
//! takes too long is a number it sets and not a wait it sits through.

use crate::client::RpcTransport;
use crate::errno::LxResult;

/// How long to wait for a TCP handshake before giving up.
///
/// The thirty seconds a `connect(2)` waits, so a mount against an
/// unreachable server fails when a `connect(2)` against it would.
pub const CONNECT_TIMEOUT_MS: u64 = 30_000;

/// Makes a connection to one port on the server, and tells the time.
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

    /// Nanoseconds on a clock that does not go back, from any start.
    fn now_ns(&self) -> u64;

    /// [`now_ns`](Self::now_ns) in milliseconds: what a
    /// [`Budget`](crate::greeting::Budget) is read against.
    fn now_ms(&self) -> u64 {
        self.now_ns() / 1_000_000
    }

    /// A starting transaction id for a connection that lives for one or
    /// two calls.
    ///
    /// Derived from the clock for the reason [`RpcClient::new`] gives: a
    /// reply from a previous connection to the same server, arriving on a
    /// reused local port, must not carry an id this client is waiting for.
    ///
    /// [`RpcClient::new`]: crate::client::RpcClient::new
    fn first_xid(&self) -> u32 {
        self.now_ns() as u32
    }
}
