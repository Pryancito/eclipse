//! The network stack of Eclipse OS as a process.
//!
//! The kernel's stack is smoltcp, fed by a card's driver: one of the
//! kernel's own, or a process that serves the card to it
//! (`tools/eclipse-udrivers`, `eclipse_net_serve`). Here the same smoltcp
//! runs in a process, and the driver serves the card to it instead: the
//! driver is started with a channel, sends what it would have given the
//! kernel down it -- the far end of its FIFO and the window its frames sit
//! in -- and from then on a frame goes from the card to the stack and back
//! through memory the two processes share. The kernel carries the FIFO's
//! messages and wakes whoever waits on them, and that is all: it has no
//! interface for this card, and none of its frames pass through it.
//!
//! What there is so far is the bottom of a stack: the card, an address,
//! and what smoltcp answers by itself -- ARP for that address and ICMP
//! echo. No sockets yet, so no program can use this interface; it exists
//! to be measured against the kernel's (`scripts/udp_ping.py`).
//!
//! [`port`] is the stack's side of a served card, which is all of this
//! that can be tested without one.

pub mod port;
pub mod socks;
