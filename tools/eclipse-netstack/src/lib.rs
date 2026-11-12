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
//! What there is so far: the card, an address, what smoltcp answers by
//! itself -- ARP for that address and ICMP echo -- and UDP sockets. A
//! program's socket stays the kernel's object, and the kernel asks this
//! process, over a channel, to send its datagrams and is told of the ones
//! that arrive (`eclipse_netstack_serve`). Which programs: the ones started
//! by `eclipse-netstack-use`; every other socket is the kernel's stack's.
//! No TCP, no routes beyond the card's network, and no fragments.
//!
//! [`port`] is the stack's side of a served card and [`socks`] the sockets
//! over it: all of this that can be tested without a card or a kernel.
//! `scripts/udp_ping.py` measures it against the kernel's stack.

pub mod port;
pub mod socks;
