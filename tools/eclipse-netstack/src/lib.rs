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
//! itself -- ARP for that address and ICMP echo -- and UDP and TCP
//! sockets. A program's socket stays the kernel's object, and the kernel
//! asks this process, over a channel, to send its datagrams and make its
//! connections, and is told of what arrives (`eclipse_netstack_serve`).
//! Which programs: the ones started by `eclipse-netstack-use`, and every
//! other socket is the kernel's stack's; or all of them, on a boot with
//! `unetuse=all`. The address is written on the command
//! line or leased from a DHCP server, and one gateway carries what is not
//! for the card's network ([`socks::lease`]); the machine's network
//! settings say which instead when they name the card ([`governed`]); the
//! name servers of a lease go to `/etc/resolv.conf` ([`resolv`]). IPv4 only, and no
//! fragments: a program's `AF_INET6` socket is served too, and carries
//! IPv4 (`::` is every address, `::ffff:a.b.c.d` is `a.b.c.d`).
//!
//! What one program of the machine says to another, at 127.0.0.0/8 or at
//! the stack's own address, never comes here: the kernel has both sockets
//! and joins them, with no stack between (`linux-object/src/net/user_stack.rs`).
//! And the sockets outlive this process: started again, it is told of
//! the ones that listen or have a port, and only the connections it was
//! carrying are lost.
//!
//! [`port`] is the stack's side of a served card and [`socks`] the sockets
//! over it: all of this that can be tested without a card or a kernel.
//! `scripts/udp_ping.py` and `scripts/tcp_bench.py` measure it against the
//! kernel's stack from the host, and `eclipse-tcp-bench` from a program of
//! the guest, which is how what never reaches the card is timed.

pub mod governed;
pub mod port;
pub mod resolv;
pub mod socks;
