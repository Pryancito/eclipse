//! The network stack of Eclipse OS as a process.
//!
//! The kernel's stack is smoltcp, fed by a card's driver: one of the
//! kernel's own, or a process that serves the card to it
//! (`tools/eclipse-udrivers`, `eclipse_net_serve`). Here the same smoltcp
//! runs in a process that is the card's driver as well: it claims the
//! card, and a frame goes from the card's rings to the stack and back
//! inside this process. The kernel hands it the card's interrupts, and
//! that is all: it has no interface for this card, and none of its frames
//! pass through it. With `driver=process` the driver is a process of its
//! own, as it was before: it is started with a channel, sends what it
//! would have given the kernel down it -- the far end of its FIFO and the
//! window its frames sit in -- and a frame crosses between the two
//! through memory they share, the kernel carrying the FIFO's messages.
//!
//! What there is so far: the card, an address, what smoltcp answers by
//! itself -- ARP for that address and ICMP echo -- and UDP and TCP
//! sockets. A program's socket stays the kernel's object, and the kernel
//! asks this process, over a channel, to send its datagrams and make its
//! connections, and is told of what arrives (`eclipse_netstack_serve`).
//! Which programs: all of them, on a machine whose only card is this
//! stack's, which is what a boot that leaves it the card gets
//! (`udriver=virtio-net@0`); with `unetuse=asked` on the kernel command
//! line, or where the kernel drives a card of its own, the ones started by
//! `eclipse-netstack-use`, and every other socket is the kernel's
//! stack's. The address is written on the command
//! line or leased from a DHCP server, and one gateway carries what is not
//! for the card's network ([`socks::lease`]); the machine's network
//! settings say which instead when they name the card ([`governed`]); the
//! name servers of a lease go to `/etc/resolv.conf` ([`resolv`]); how much
//! the card has carried is told to the kernel, which shows it
//! ([`counts`]). IPv4 only, and no
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

pub mod counts;
pub mod governed;
pub mod port;
pub mod resolv;
pub mod socks;
