//! The connection through the stack process: a served TCP socket, driven
//! **synchronously**.
//!
//! [`KernelTransport`](super::transport::KernelTransport) drives the kernel's
//! own smoltcp by hand. On a machine whose network is a stack process
//! ([`is_for_all`](crate::net::user_stack::is_for_all)) the kernel's stack has
//! no card under it, so a mount through it reached nobody: the NFS client
//! was the one thing left that could not use the network every program
//! uses. This is the same four methods over a
//! [`ServedTcpSocket`], the socket a program's `socket(2)` gets there.
//!
//! # What waiting is here
//!
//! The caller is a `mount(2)`, or a `read(2)` of a file on the mount, and
//! cannot `await`. With the kernel's stack the wait drives the card itself.
//! Here the bytes come from another process, so the wait has to let that
//! process run and then read what it sent: [`StackLink::pump`] reads the
//! stack's messages without the link's task, and between two looks the CPU
//! is given away ([`between_looks`]) as a request to a served filesystem
//! gives it away. From the first turn given away, the stack's channel
//! becoming readable has this CPU take the wait up again
//! ([`KickOn`]), so the reply is not left for the scheduler's cadence.
//!
//! The socket never waits by itself (`O_NONBLOCK`), which is what lets its
//! `async` calls be asked once and answered on the spot.

use alloc::sync::Arc;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use kernel_hal::thread::SyncWait;
use smoltcp::wire::{IpAddress, IpEndpoint};
use zircon_object::object::{KickOn, Signal};

use crate::error::{LxError, LxResult};
use crate::fs::served::between_looks;
use crate::fs::{FileLike, OpenFlags};
use crate::net::user_stack::{ServedTcpSocket, StackLink};
use crate::net::{Endpoint, Socket};

use super::client::{RpcTransport, TransportError};
use super::transport::{park_ms, MAX_PARK_PASSES, SEND_TIMEOUT_MS};

/// The answer of a call that does not wait. A socket with `O_NONBLOCK`
/// answers each of its calls at the first poll; one that did not is told
/// to try again, which is what it would have said.
fn at_once<T>(call: impl Future<Output = LxResult<T>>) -> LxResult<T> {
    let mut call = core::pin::pin!(call);
    match Pin::new(&mut call).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(answer) => answer,
        Poll::Pending => Err(LxError::EAGAIN),
    }
}

fn now_ms() -> u64 {
    kernel_hal::timer::timer_now().as_millis() as u64
}

/// Passes of a wait in which the clock did not move: the exit of a loop
/// whose only other exit is the clock, as in the kernel transport.
struct Stalls {
    seen: u64,
    passes: u32,
}

impl Stalls {
    fn new() -> Self {
        Stalls {
            seen: now_ms(),
            passes: 0,
        }
    }

    /// One more pass. True when there have been too many with the clock
    /// still.
    fn too_many(&mut self) -> bool {
        let now = now_ms();
        if now > self.seen {
            self.seen = now;
            self.passes = 0;
            return false;
        }
        self.passes += 1;
        self.passes >= MAX_PARK_PASSES
    }
}

/// A connection to an NFS server through the stack process.
pub struct ServedTransport {
    socket: ServedTcpSocket,
    link: Arc<StackLink>,
    /// The stack's channel has this CPU take a wait up again: made at the
    /// first turn given away, and kept for the connection.
    kick: Option<KickOn>,
    /// Reads in a row that found nothing: how long the next wait may be.
    empty: u32,
    /// Out of step with the server: nothing more goes through it.
    poisoned: bool,
}

impl ServedTransport {
    /// Connect to `addr:port` through `link`, taking no longer than
    /// `timeout_ms`. Answers what `connect(2)` answers a program.
    pub(crate) fn connect(
        link: Arc<StackLink>,
        addr: IpAddress,
        port: u16,
        timeout_ms: u64,
    ) -> LxResult<Self> {
        let socket = ServedTcpSocket::new(link.clone(), false)?;
        socket.set_flags(OpenFlags::NON_BLOCK)?;
        let mut this = ServedTransport {
            socket,
            link,
            kick: None,
            empty: 0,
            poisoned: false,
        };
        let to = Endpoint::Ip(IpEndpoint::new(addr, port));
        match at_once(Socket::connect(&this.socket, to)) {
            Ok(_) => return Ok(this),
            Err(LxError::EINPROGRESS) | Err(LxError::EAGAIN) => {}
            Err(e) => return Err(e),
        }
        let deadline = now_ms().saturating_add(timeout_ms);
        let mut stalls = Stalls::new();
        let mut away = SyncWait::new();
        loop {
            this.link.pump();
            match this.socket.connect_outcome() {
                Some(Ok(())) => return Ok(this),
                Some(Err(e)) => return Err(e),
                None => {}
            }
            // Dropping the socket is what gives the connection up: the
            // stack is told to close what it was still making.
            if now_ms() >= deadline || stalls.too_many() {
                return Err(LxError::ETIMEDOUT);
            }
            this.look_away(&mut away);
        }
    }

    /// Give the CPU away once, to be taken back when the stack has sent
    /// something. The first time only arranges that: what the stack sent
    /// before it was arranged raised nothing, so the caller looks again.
    fn look_away(&mut self, away: &mut SyncWait) {
        if self.kick.is_none() {
            if let Some(channel) = self.link.channel() {
                self.kick = Some(KickOn::new(
                    &*channel,
                    Signal::READABLE | Signal::PEER_CLOSED,
                ));
                return;
            }
        }
        between_looks(away);
    }

    /// Whether a read would find something now: bytes, the end, or an
    /// error.
    fn has_news(&self) -> bool {
        match FileLike::poll(&self.socket, crate::fs::PollEvents::IN) {
            Ok(status) => status.read || status.error || status.hangup,
            Err(_) => true,
        }
    }

    fn poison(&mut self) {
        self.poisoned = true;
    }
}

/// What a failed write means to the client.
fn write_error(e: LxError) -> TransportError {
    match e {
        LxError::EPIPE | LxError::ENOTCONN | LxError::ENETDOWN => TransportError::Closed,
        LxError::ECONNRESET => TransportError::Reset,
        _ => TransportError::Other,
    }
}

/// What a failed read means to the client.
fn read_error(e: LxError) -> TransportError {
    match e {
        LxError::ECONNRESET => TransportError::Reset,
        LxError::EPIPE | LxError::ENOTCONN | LxError::ENETDOWN => TransportError::Closed,
        _ => TransportError::Other,
    }
}

impl RpcTransport for ServedTransport {
    fn send(&mut self, data: &[u8]) -> Result<(), TransportError> {
        if self.poisoned {
            return Err(TransportError::NotConnected);
        }
        let deadline = now_ms().saturating_add(SEND_TIMEOUT_MS);
        let mut stalls = Stalls::new();
        let mut away = None;
        let mut sent = 0;
        while sent < data.len() {
            let failed = match Socket::write(&self.socket, &data[sent..], None) {
                Ok(n) if n > 0 => {
                    sent += n;
                    continue;
                }
                // No room: the stack gives more as the server takes bytes.
                Ok(_) | Err(LxError::EAGAIN) => None,
                Err(e) => Some(write_error(e)),
            };
            let failed = failed.or_else(|| {
                (now_ms() >= deadline || stalls.too_many()).then_some(TransportError::Other)
            });
            if let Some(e) = failed {
                // Part of a record went out and the rest never will: the
                // server waits for bytes this client thinks it sent.
                if sent > 0 {
                    self.give_up();
                }
                return Err(e);
            }
            self.link.pump();
            self.look_away(away.get_or_insert_with(SyncWait::new));
        }
        Ok(())
    }

    fn recv(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        if self.poisoned {
            return Err(TransportError::NotConnected);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let answer = at_once(async { Socket::read(&self.socket, buf).await.0 });
        match answer {
            // The server closed: "nothing yet" is `EAGAIN`, below.
            Ok(0) => Err(TransportError::Closed),
            Ok(n) => {
                self.empty = 0;
                Ok(n)
            }
            Err(LxError::EAGAIN) => {
                self.empty = self.empty.saturating_add(1);
                Ok(0)
            }
            Err(e) => Err(read_error(e)),
        }
    }

    fn now_ms(&self) -> u64 {
        now_ms()
    }

    fn poll(&mut self) {
        self.link.pump();
        if self.poisoned || self.has_news() {
            return;
        }
        // As long as this many empty reads ask for, and no longer than
        // the reply takes: the caller's loop has the deadline.
        let until = now_ms() + park_ms(self.empty);
        let mut stalls = Stalls::new();
        let mut away = SyncWait::new();
        loop {
            self.look_away(&mut away);
            self.link.pump();
            if self.has_news() || now_ms() >= until || stalls.too_many() {
                return;
            }
        }
    }

    fn give_up(&mut self) {
        self.poison();
        // Both ways shut: the stack closes its end, and the server sees
        // the connection end instead of half a record that never grows.
        const SHUT_RDWR: usize = 2;
        let _ = Socket::shutdown(&self.socket, SHUT_RDWR);
    }
}

#[cfg(test)]
#[path = "served_tests.rs"]
mod tests;
