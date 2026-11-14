//! `eclipse-netstack N ADDR/PREFIX[,GATEWAY] [driver=process|card=kernel]`
//! or `eclipse-netstack N dhcp [driver=process|card=kernel]`: a network
//! stack for virtio network card `N`, in this process, with the address
//! `ADDR` or the one a DHCP server leases it. What is not for the card's network goes to the
//! gateway, the one written or the lease's. When the machine's network
//! settings name the card, what they say is used and not the command line
//! (`netstack::governed`).
//!
//! The card must be one the kernel left to userspace
//! (`udriver=virtio-net@N` on its command line) and that no driver has.
//! This process is its driver: it claims the card, and its threads read
//! the card's rings and fill them themselves (`udrivers::net`), so a frame
//! goes from the card to the stack and back without a change of process.
//! When the card or this process fails, the process ends and whoever
//! started it starts it again; the kernel keeps the sockets meanwhile.
//!
//! With `driver=process` the driver is a process of its own, as it was
//! before the two were one: this starts it (`/bin/udrv-virtio-net N
//! stack=FD`), with a channel on which the driver hands the card over, and
//! starts it again when it dies. The address, the connections and what
//! smoltcp knows of the neighbours stay through that, which is what the
//! second process buys; every frame pays for it with a change of process
//! each way.
//!
//! With `card=kernel` the card is not a virtio one of this process's but
//! card `N` of the ones the kernel drives, the loopback not counted, which
//! the kernel lends (`udrivers::lent`): its driver stays in the kernel and
//! gives this process the frames. Until this stack has drivers of its own
//! for such cards, that is how it is the network of a machine that has
//! one. The loan ends with this process, and the card is the kernel's
//! stack's again.
//!
//! The kernel has no interface for this card. The stack answers for its
//! address by itself (ARP, ICMP echo), and serves the kernel the datagram
//! and stream sockets of the programs that asked for this stack
//! (`eclipse_netstack_serve`; `netstack::socks` has what the two say to
//! each other). On a kernel that has no such call it answers for its
//! address and nothing else.
//!
//! Three threads share the stack, under one lock. The card's sleeps on
//! the card's interrupt (or on the driver's FIFO), takes what the card
//! did, a turn at a time, and sends the kernel what arrived, all in one
//! message. The sockets' takes what the kernel says, every message that is
//! waiting, and gives the card the frames that made, all at once. The third sleeps until the stack has something
//! to do that no message brings about: asking again for a neighbour that
//! did not answer, sending again a segment nobody acknowledged.
//!
//! One message for all that arrived is what keeps this from costing a
//! change of process for each datagram or segment: whoever is woken runs
//! at once, so a wake for each is one at a time through four processes.
//! The card is given what there is for it as seldom: a receive slot to
//! give back waits for a frame to go with. And a driver that is a process
//! of its own is woken as seldom, for the same reason: there the
//! acknowledgement of what a program was just told waits a moment for the
//! program's answer (`HOLD`).
//!
//! The kernel is also told how much the card has carried, since it is the
//! kernel that shows the machine's interfaces and their counts: at once
//! after a quiet while and a few times a second while frames go
//! (`netstack::counts`), by whichever thread has just had a turn, and by
//! the third when no turn comes to say the last of it.
//!
//! The kernel is told the address once, for `getsockname`, when it is
//! given its sockets: at the start for an address that is written, with
//! the first lease for one that is asked for. A lease that comes back with
//! another address ends this process, for whoever started it to start it
//! again: the connections were the old address's.
//!
//! Exit status: 2 for a command line that is not one, 1 when there is no
//! card to take, when the card fails, and when a driver of its own cannot
//! be started or gives no card, 3 when the lease's address changed.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{exit, Child, Command};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::Thread;
use std::time::Duration;

use netstack::counts::{Counts, Teller};
use netstack::governed;
use netstack::port::Port;
use netstack::resolv::{self, Applied, Named};
use netstack::socks::lease::{Addressing, Lease};
use netstack::socks::six::Sixes;
use netstack::socks::tcp::OP_WRITE;
use netstack::socks::{parse_header, Arrived, Stack, MESSAGE_MAX, OP_SEND};
use smoltcp::iface::{InterfaceBuilder, NeighborCache, Routes};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpCidr, Ipv4Address, Ipv4Cidr};
use udrivers::channel::Channel;
use udrivers::claim;
use udrivers::fifo::Fifo;
use udrivers::lent::Lent;
use udrivers::net::{self, Feeder, Msg, NoCard, Offer, BUF_SIZE};
use udrivers::status;
use udrivers::virtio::Device;
use udrivers::zx::{self, nr, Handle, Status};

const NAME: &str = "eclipse-netstack";
const USAGE: &str =
    "usage: eclipse-netstack N ADDR/PREFIX[,GATEWAY]|dhcp [driver=process|card=kernel]";
/// The word that asks for the card's driver in a process of its own.
const APART: &str = "driver=process";
/// The word for a card the kernel drives and lends, in place of a virtio
/// card of this process's own.
const KERNELS: &str = "card=kernel";
/// The card's driver, when it is a process of its own, which this one
/// starts.
const DRIVER: &str = "/bin/udrv-virtio-net";
/// The messages of one turn: as many as the driver's FIFO holds, which is
/// more than a card has slots.
const TURN: usize = net::FIFO_DEPTH;
const _: () = assert!(net::SLOTS <= TURN);
/// The longest pause before the driver is started again, which is where
/// the pauses stop doubling for one that keeps dying.
const PAUSE_MAX: Duration = Duration::from_secs(8);
/// A driver that lasted this long was working: the next pause is the
/// shortest again.
const SETTLED: Duration = Duration::from_secs(10);

fn die(what: impl std::fmt::Display) -> ! {
    println!("{NAME}: {what}");
    exit(1);
}

/// The window of a card, mapped here until dropped.
struct Window {
    ptr: *mut u8,
    len: usize,
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: the mapping `zx::map` made, which nothing uses any more:
        // the port over it is dropped first.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

/// The card, when this process is its driver: what any thread needs to
/// give it frames to send and its receive slots back.
struct Own {
    device: Device,
    /// The offering halves of the card's rings. Taken before the stack's
    /// lock and never with it held: whoever has it gives the card what
    /// the stack has for it now, so two threads' frames reach the card in
    /// the order the stack made them.
    feeder: Mutex<Feeder>,
    /// The card's interrupt, or `None` where the card has to be looked at.
    interrupt: Option<Handle>,
}

impl Own {
    /// Give the card what the stack has for it: the receive slots it has
    /// read and the frames it wants sent. With `when_due`, only if that is
    /// worth a trip to the card ([`Port::due`]).
    fn give(&self, host: &Host, msgs: &mut [Msg; TURN], when_due: bool) {
        let mut feeder = locked(&self.feeder);
        let n = {
            let shared = locked(&host.shared);
            let port = shared.stack.device();
            if when_due && !port.due() {
                0
            } else {
                port.outgoing(msgs)
            }
        };
        if n == 0 {
            return;
        }
        match feeder.give(&msgs[..n]) {
            Ok((rx, tx)) => {
                if rx {
                    self.device.notify(net::RXQ);
                }
                if tx {
                    self.device.notify(net::TXQ);
                }
            }
            // The port makes them and checks them: a bug here, and nothing
            // to carry on with.
            Err(m) => die(format_args!(
                "a message for the card that means nothing: {m:?}"
            )),
        }
    }
}

/// The card, when it is one the kernel drives and has lent to this
/// process: what any thread needs to give it frames to send and its
/// receive slots back.
struct Borrowed {
    lent: Lent,
    /// Taken before the stack's lock and never with it held, as
    /// [`Own::feeder`] is and for the same reason: two threads' frames
    /// reach the card in the order the stack made them.
    giving: Mutex<()>,
}

impl Borrowed {
    /// Give the kernel what the stack has for the card: the receive slots
    /// it has read and the frames it wants sent, which the card has
    /// before this returns unless it has no room. With `when_due`, only if
    /// that is worth the call ([`Port::due`]). The transmit slots the card
    /// is through with come back with the same call.
    ///
    /// Whether the stack is owed another turn: it had run out of transmit
    /// slots, so what it had to send may not all be made yet, and some
    /// came back. Nothing else would bring that turn: the kernel wakes the
    /// card's thread for a slot only when a frame had to wait for the
    /// card.
    fn give(&self, host: &Host, msgs: &mut [Msg; TURN], when_due: bool) -> bool {
        let _giving = locked(&self.giving);
        let (n, short) = {
            let shared = locked(&host.shared);
            let port = shared.stack.device();
            let short = !port.can_send();
            if when_due && !port.due() {
                (0, short)
            } else {
                (port.outgoing(msgs), short)
            }
        };
        if n == 0 {
            return false;
        }
        let back = match self.lent.turn(msgs, n, false, false) {
            Ok(back) => back,
            Err(e) => die(format_args!(
                "the kernel's card is gone (net_lent_turn: {e})"
            )),
        };
        if back == 0 {
            return false;
        }
        let shared = locked(&host.shared);
        for m in &msgs[..back] {
            if !shared.stack.device().take(m) {
                println!("{NAME}: a message from the kernel's card that means nothing: {m:?}");
            }
        }
        short
    }
}

/// How the stack's threads reach the card.
#[derive(Clone)]
enum Wire {
    /// Through its driver, a process of its own, down the driver's FIFO.
    Driver(Arc<Fifo<Msg>>),
    /// It is this process's.
    Own(Arc<Own>),
    /// It is the kernel's, lent.
    Kernel(Arc<Borrowed>),
}

/// What the card's thread has of the card besides the [`Wire`].
enum Taker {
    /// The driver's process, and its window, held for the port over it,
    /// which the interface has: dropped, with the rest of the link, once
    /// the interface is on another.
    Driver { driver: Child, _window: Window },
    /// The taking halves of the card's rings.
    Own(net::Card),
    /// Nothing: the kernel has the card, and the wire is the loan.
    Kernel,
}

/// A card as the stack has it: its own, or a driver's that serves it.
struct Link {
    wire: Wire,
    taker: Taker,
    mac: [u8; 6],
}

/// Take card `index` for this process to drive: the link to it, and the
/// port for the stack, which is over the card's own slots.
fn take(index: usize) -> Result<(Link, Port), String> {
    let driven = net::drive(index, false).map_err(|e| match e {
        NoCard::NoGateway => "the gateway is off (zircon.gateway=0 on the command line?)".into(),
        NoCard::Other(e) => e,
    })?;
    if driven.irq_refused {
        println!("{NAME}: the device refused MSI-X vector 0; polling");
    }
    // SAFETY: the slots of the DMA memory, which is mapped for as long as
    // the process lasts; the card and this port are all that read or write
    // them, a slot at a time as the rings say whose it is.
    let port = unsafe { Port::new(driven.dma.ptr.add(net::DMA_WIN), net::RX_BUFS, net::TX_BUFS) }
        .ok_or("a card with more slots than a port takes")?;
    Ok((
        Link {
            wire: Wire::Own(Arc::new(Own {
                device: driven.device,
                feeder: Mutex::new(driven.feeder),
                interrupt: driven.interrupt,
            })),
            taker: Taker::Own(driven.card),
            mac: driven.mac,
        },
        port,
    ))
}

/// Borrow the kernel's card `index`: the link to it, and the port for the
/// stack, over a window of this process's that the kernel copies the
/// frames into and out of.
fn borrow(index: usize) -> Result<(Link, Port), String> {
    let lent = Lent::borrow(index, net::RX_BUFS, net::TX_BUFS).map_err(|e| match e {
        NoCard::NoGateway => "the gateway is off (zircon.gateway=0 on the command line?)".into(),
        NoCard::Other(e) => e,
    })?;
    // SAFETY: the loan's window, which lasts as long as the loan does;
    // the kernel, during this process's own calls, and this port are all
    // that read or write it, a slot at a time as the messages say whose it
    // is. The loan is in the link, which outlives the interface the port
    // goes into: the process ends before either is dropped.
    let port = unsafe { Port::new(lent.window(), lent.rx_slots, lent.tx_slots) }
        .ok_or("a window with more slots than a port takes")?;
    let mac = lent.mac;
    Ok((
        Link {
            wire: Wire::Kernel(Arc::new(Borrowed {
                lent,
                giving: Mutex::new(()),
            })),
            taker: Taker::Kernel,
            mac,
        },
        port,
    ))
}

/// Start the driver of card `index`, a process of its own, and take the
/// card it offers: the link to it, and the port for the stack.
fn start(index: usize) -> Result<(Link, Port), String> {
    let (ours, theirs) = Channel::create().map_err(|e| match e {
        Status::NO_GATEWAY => "the gateway is off (zircon.gateway=0 on the command line?)".into(),
        e => format!("channel_create: {e}"),
    })?;
    let fd = theirs.into_fd().map_err(|e| format!("handle_to_fd: {e}"))?;
    let spawned = Command::new(DRIVER)
        .arg(index.to_string())
        .arg(format!("stack={fd}"))
        .spawn();
    // The driver has its copy, or there is no driver: either way this one
    // goes, so that a driver that dies closes the channel.
    // SAFETY: a descriptor this function made.
    unsafe { libc::close(fd) };
    let mut driver = spawned.map_err(|e| format!("{DRIVER}: {e}"))?;

    let mut bytes = [0u8; Offer::LEN];
    let mut handles: [Handle; Offer::HANDLES] = [0; Offer::HANDLES];
    let taken = ours
        .read(&mut bytes, &mut handles)
        .map_err(|e| format!("the driver went without offering a card: {e}"))
        .and_then(|(nbytes, nhandles)| {
            if nhandles != Offer::HANDLES {
                return Err(format!("an offer with {nhandles} handles"));
            }
            let (fifo, window) = (Fifo::from_handle(handles[0]), handles[1]);
            let offer = Offer::from_bytes(&bytes[..nbytes]).ok_or("an offer of the wrong size")?;
            let (rx, tx) = (offer.rx_slots as usize, offer.tx_slots as usize);
            let len = rx.saturating_add(tx).saturating_mul(BUF_SIZE);
            // The counts are checked before anything is mapped by them.
            // SAFETY: nothing is read or written through a null window.
            if unsafe { Port::new(core::ptr::null_mut(), rx, tx) }.is_none() {
                zx::close(window);
                return Err(format!("an offer of {rx} and {tx} slots"));
            }
            let window = Window {
                ptr: zx::map(window, len)?,
                len,
            };
            // SAFETY: `len` bytes just mapped, which only this port uses,
            // and the port is dropped before the window is.
            let port = unsafe { Port::new(window.ptr, rx, tx) }.expect("checked above");
            Ok((fifo, window, port, offer.mac_bytes()))
        });
    match taken {
        Ok((fifo, window, port, mac)) => Ok((
            Link {
                wire: Wire::Driver(Arc::new(fifo)),
                taker: Taker::Driver {
                    driver,
                    _window: window,
                },
                mac,
            },
            port,
        )),
        Err(why) => {
            let _ = driver.kill();
            let _ = driver.wait();
            Err(why)
        }
    }
}

fn now() -> Instant {
    Instant::from_micros((udrivers::now_ns() / 1000) as i64)
}

/// What the threads share.
struct Shared {
    stack: Stack<Port>,
    /// The way to the card now, for the threads that are not the card's
    /// to give it a frame to send.
    wire: Wire,
    /// Whether the programs answer what arrives for them: one has sent
    /// something since the card's thread last waited for an answer to
    /// send its frames with (see [`HOLD`]).
    answering: bool,
    /// Whether the card's IPv6 addresses changed and the kernel has not
    /// been told: it is, with the first turn it has sockets here.
    six_owed: bool,
}

/// What every thread has: the stack, and the kernel once it has sockets
/// here.
struct Host {
    index: usize,
    /// Whether the card is one the kernel drives and lends (`card=kernel`).
    kernels: bool,
    mac: [u8; 6],
    /// What the machine's network settings said for this card when the
    /// stack started, as they said it ([`governed`]).
    governed: Option<String>,
    shared: Mutex<Shared>,
    /// The kernel's sockets, from when it was told this stack's address.
    kernel: OnceLock<Arc<Kernel>>,
    /// The card the kernel was told, or would have been had it wanted a
    /// stack: `getsockname` answers with its address, and the machine's
    /// interfaces show it, for as long as this process lasts.
    told: Mutex<Option<Card>>,
    /// The name servers of the lease and of the routers, as last heard:
    /// held while they are written down, and never with the stack's
    /// lock.
    named: Mutex<Named>,
    /// The driver the card has now and how many it has had, when the
    /// driver is a process of its own (zero when it is this one), for
    /// [`publish`].
    driver_pid: AtomicU32,
    driver_starts: AtomicU32,
    /// What the kernel was told the card has carried, and when. Taken
    /// with the stack's lock held or with none, never the other way.
    teller: Mutex<Teller>,
    /// The thread that keeps time, once it runs: woken to tell the
    /// kernel what a turn carried too soon after the last telling.
    clock: OnceLock<Thread>,
}

/// A lock, whatever became of the thread that had it last: the process
/// ends on a panic, so nobody is left to see a poisoned one.
fn locked<T>(lock: &Mutex<T>) -> MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|e| e.into_inner())
}

/// The link to the kernel: the channel the sockets' messages travel on.
struct Kernel {
    channel: Channel,
    /// Held to send what arrived, and taken before the stack's lock is
    /// let go: the datagrams reach the kernel in the order they left the
    /// stack, and no thread waits for the stack while another is in the
    /// middle of telling the kernel.
    sending: Mutex<()>,
}

/// Carry the frames through the stack and tell the kernel what arrived
/// for its sockets: whether there were bytes for a program in it. Takes
/// the lock, which it lets go before the kernel is told: that write wakes
/// whoever reads the socket, who runs at once and has something to send
/// before this thread is back.
fn turn<'a>(host: &'a Arc<Host>, mut shared: MutexGuard<'a, Shared>, data: &mut Vec<u8>) -> bool {
    let at = now();
    shared.stack.poll(at);
    if let Some(lease) = shared.stack.lease_news() {
        // Not under the lock: the kernel may have to be told.
        drop(shared);
        settle(host, lease);
        shared = locked(&host.shared);
    }
    if shared.stack.six_news() {
        shared.six_owed = true;
        let sixes = shared.stack.sixes();
        drop(shared);
        say_sixes(host, &sixes);
        shared = locked(&host.shared);
    }
    if shared.stack.names6_news() {
        let routers = shared.stack.names6();
        // Not under the lock: a file is written.
        drop(shared);
        name_servers(host, |named| named.routers = routers);
        shared = locked(&host.shared);
    }
    // Without the kernel there are no sockets for anything to arrive on.
    let Some(kernel) = host.kernel.get() else {
        return false;
    };
    if core::mem::take(&mut shared.six_owed) {
        // A message of its own, as rare as an address changes. A kernel
        // that is gone took the machine with it.
        let _ = kernel.channel.write(&shared.stack.sixes().message(), &[]);
    }
    tell_counts(host, &shared.stack.device().counts(), at);
    let mut bytes = false;
    loop {
        let arrived = shared.stack.arrived(data);
        bytes |= shared.stack.brought_bytes();
        if arrived == Arrived::Nothing {
            return bytes;
        }
        let sending = locked(&kernel.sending);
        if arrived == Arrived::All {
            drop(shared);
            // A kernel that is gone took the machine with it.
            let _ = kernel.channel.write(data, &[]);
            return bytes;
        }
        // More than a message holds, which a card's turn rarely brings:
        // this one goes under the lock, and the next is asked for.
        let _ = kernel.channel.write(data, &[]);
        drop(sending);
    }
}

/// Tell the kernel that the card has carried `counts`, at `at`, when it
/// has not been told and was last told long enough ago
/// (`netstack::counts`). A message of its own, a few times a second at
/// most: it wakes the kernel's task and no program.
fn tell_counts(host: &Host, counts: &Counts, at: Instant) {
    let Some(kernel) = host.kernel.get() else {
        return;
    };
    let told = {
        let mut teller = locked(&host.teller);
        teller.tell(counts, at).ok_or_else(|| teller.owes(counts))
    };
    match told {
        // A kernel that is gone took the machine with it.
        Ok(message) => {
            let _ = kernel.channel.write(&message, &[]);
        }
        // Too soon: the thread that keeps time may be asleep for longer
        // than this can wait, and does not know.
        Err(true) => {
            if let Some(clock) = host.clock.get() {
                clock.unpark();
            }
        }
        Err(false) => {}
    }
}

/// How long until the kernel is to be told what the card has carried,
/// when it has a kernel to tell and something to tell it.
fn counts_due_in(host: &Host, counts: &Counts) -> Option<Duration> {
    host.kernel.get()?;
    let due = locked(&host.teller).due_in(counts, now())?;
    Some(Duration::from_micros(due.total_micros()))
}

/// Tell the card, from a thread that is not the card's, what this turn
/// of the stack made: the frames to send, mostly. The card's thread sends
/// its own with its next turn.
fn tell_driver(host: &Arc<Host>, msgs: &mut [Msg; TURN], data: &mut Vec<u8>) {
    let wire = locked(&host.shared).wire.clone();
    let fifo = match wire {
        Wire::Driver(fifo) => fifo,
        Wire::Own(own) => return own.give(host, msgs, false),
        Wire::Kernel(card) => {
            while card.give(host, msgs, false) {
                turn(host, locked(&host.shared), data);
            }
            return;
        }
    };
    let n = locked(&host.shared).stack.device().outgoing(msgs);
    if n == 0 {
        return;
    }
    let went = match fifo.write(&msgs[..n]) {
        Ok(went) => went,
        // No room: the driver has a FIFO full of turns to read.
        Err(Status::SHOULD_WAIT) => 0,
        // The driver is gone, and the card's thread is finding out. The
        // next driver starts with a port that owes nothing.
        Err(_) => return,
    };
    if went < n {
        // What did not fit goes with the next turn, the card's or this
        // thread's: the driver answers what it has, and that wakes the
        // card's thread.
        locked(&host.shared).stack.device().put_back(&msgs[went..n]);
    }
}

/// How long the card's thread keeps the frames of a turn that brought a
/// program bytes, for the program's answer to go to the driver with them,
/// when the driver is a process of its own: in nanoseconds.
///
/// What such a turn has to send is an acknowledgement, and the program is
/// awake with the bytes before this thread is back. Sent now, the
/// acknowledgement wakes the driver for itself, and the answer wakes it
/// again; kept, the two are one wake and the driver puts them on the wire
/// together. The price when no answer comes is an acknowledgement late,
/// by this much or until the driver has more to say, once: after that the
/// programs are taken not to be answering ([`Shared::answering`]) until
/// one sends again, so what a program only reads is acknowledged as it
/// arrives.
const HOLD: u64 = 2_000_000;

/// Take what the card did (`msgs`) and carry the frames through the
/// stack, telling the kernel what arrived for its sockets: whether what
/// that left to send is to be kept a moment ([`HOLD`]).
fn carried(host: &Arc<Host>, msgs: &[Msg], data: &mut Vec<u8>) -> bool {
    let guard = locked(&host.shared);
    for m in msgs {
        if !guard.stack.device().take(m) {
            println!("{NAME}: a message from the card that means nothing: {m:?}");
        }
    }
    // Every frame that has room for its answer, in one go.
    if !turn(host, guard, data) {
        return false;
    }
    let mut shared = locked(&host.shared);
    let hold = shared.answering && shared.stack.device().has_frames();
    // Until a program sends again.
    shared.answering &= !hold;
    hold
}

/// Carry the frames of a card a driver serves through the stack until the
/// driver is gone. Why it stopped.
///
/// One system call a turn (`eclipse_fifo_reply_wait`): what the last turn
/// left to say -- the receive slots that were read, the frames to send --
/// goes out, and the call comes back with what the card did meanwhile.
/// One more when something arrived for a socket, to tell the kernel, and
/// then one to wait for the answer ([`HOLD`]).
///
/// The driver is told only of what is worth waking it for
/// ([`Port::due`]): a receive slot to give back waits for a frame to go
/// with.
fn serve(host: &Arc<Host>, fifo: &Fifo<Msg>) -> Status {
    let mut msgs = [Msg::default(); TURN];
    let mut data = Vec::with_capacity(MESSAGE_MAX);
    loop {
        let replies = {
            let shared = locked(&host.shared);
            let port = shared.stack.device();
            if port.due() {
                port.outgoing(&mut msgs)
            } else {
                0
            }
        };
        let got = match fifo.reply_wait(&mut msgs, replies) {
            Ok(got) => got,
            Err(e) => return e,
        };
        if carried(host, &msgs[..got], &mut data) {
            // Time up, or the driver has more to say: either way what is
            // left to send goes with the next turn's call.
            if let Err(e) = fifo.wait_readable_for(HOLD) {
                return e;
            }
        }
    }
}

/// How long between two looks at a card that has no interrupt.
const LOOK: Duration = Duration::from_micros(200);

/// Carry the frames of this process's own card through the stack, for as
/// long as the card works. Why it stopped.
///
/// Asleep on the card's interrupt between bursts. A burst is taken out of
/// the rings a turn at a time until they are empty; after each turn the
/// card gets back the receive slots that were read and the frames to
/// send, when they are worth the trip ([`Port::due`]), so it is never
/// left without slots in the middle of a burst. One system call to wait,
/// and one more when something arrived for a socket, to tell the kernel.
///
/// Nothing is kept for a program's answer, as it is for a driver that is
/// another process ([`HOLD`]): what a turn leaves to send wakes nobody
/// here, and waiting for the answer would be waiting blind, since the
/// card's interrupt stays masked from one wait for it to the next. Timed
/// on riscv64 (QEMU counting instructions, one CPU), a thousand echoes of
/// 56 bytes with a host: 4.0 ms each when the frames were kept, 0.55 to
/// 0.7 ms sent at once, and 1.0 ms with the driver in its own process.
fn serve_card(host: &Arc<Host>, own: &Own, card: &mut net::Card) -> String {
    let mut got = [Msg::default(); net::SLOTS];
    let mut msgs = [Msg::default(); TURN];
    let mut data = Vec::with_capacity(MESSAGE_MAX);
    loop {
        if let Some(interrupt) = own.interrupt {
            if let Err(e) = claim::interrupt_wait(interrupt) {
                return format!("interrupt_wait: {e}");
            }
            own.device.ack();
        }
        let mut any = false;
        loop {
            let n = match card.collect(&mut got) {
                Ok(0) => break,
                Ok(n) => n,
                Err(why) => return why,
            };
            any = true;
            carried(host, &got[..n], &mut data);
            own.give(host, &mut msgs, true);
        }
        if own.interrupt.is_none() && !any {
            own.device.ack();
            std::thread::sleep(LOOK);
        }
    }
}

/// Carry the frames of a card the kernel lends through the stack, for as
/// long as the loan lasts. Why it stopped.
///
/// Asleep in the kernel between bursts (`eclipse_net_lent_turn`), which
/// comes back with the frames that arrived, each in a receive slot of this
/// process's window, and with the transmit slots of frames that had to
/// wait for room on the card. After a turn the kernel gets back the
/// receive slots that were read and the frames to send, when they are
/// worth the call ([`Port::due`]). Nothing is kept for a program's answer
/// ([`HOLD`]), as with a card of this process's own ([`serve_card`]): a
/// frame given to the kernel is on the card when the call returns, and
/// wakes nobody.
fn serve_lent(host: &Arc<Host>, card: &Borrowed) -> String {
    let mut got = [Msg::default(); TURN];
    let mut msgs = [Msg::default(); TURN];
    let mut data = Vec::with_capacity(MESSAGE_MAX);
    loop {
        let n = match card.lent.turn(&mut got, 0, true, true) {
            Ok(n) => n,
            Err(e) => return format!("net_lent_turn: {e}"),
        };
        carried(host, &got[..n], &mut data);
        while card.give(host, &mut msgs, true) {
            turn(host, locked(&host.shared), &mut data);
        }
    }
}

/// The most messages of the kernel's done before the driver is told of
/// the frames they made: fewer than a socket holds on their way to the
/// card, so a program that sends in a burst loses none to that.
const BURST: usize = 16;

/// Do what the kernel asks for its sockets, until the kernel's end of the
/// channel is gone (which it never is: it outlives this process).
///
/// One system call for the first message (`eclipse_channel_reply_wait`,
/// with nothing to reply), one for each that is already waiting behind it
/// and one that finds no more, and then one to tell the driver of all the
/// frames they made.
fn serve_sockets(host: Arc<Host>, kernel: Arc<Kernel>) {
    let mut message = vec![0u8; MESSAGE_MAX];
    let mut msgs = [Msg::default(); TURN];
    let mut data = Vec::with_capacity(MESSAGE_MAX);
    let debug = std::env::var_os("NETSTACK_DEBUG").is_some();
    loop {
        let mut len = match kernel.channel.wait_read(&mut message) {
            Ok(len) => len,
            Err(e) => {
                println!("{NAME}: the kernel's channel is gone ({e}): no more sockets");
                return;
            }
        };
        let (mut sends, mut answers) = (false, false);
        for done in 1.. {
            match locked(&host.shared).stack.request(&message[..len]) {
                Ok(send) => {
                    sends |= send;
                    // A program with something to say, which is what the
                    // card's thread waits for ([`HOLD`]).
                    answers |=
                        matches!(parse_header(&message[..len]), Some((OP_WRITE | OP_SEND, _)));
                }
                // A datagram that cannot go is dropped, as on any network;
                // anything else the kernel does not send.
                Err(why) if debug => {
                    println!("{NAME}: refused ({why:?}) a message of {len} bytes")
                }
                Err(_) => {}
            }
            if done == BURST {
                break;
            }
            match kernel.channel.try_read(&mut message) {
                Ok(Some(next)) => len = next,
                // Nothing more, or no kernel: the wait above says which.
                _ => break,
            }
        }
        if sends {
            let mut guard = locked(&host.shared);
            guard.answering |= answers;
            turn(&host, guard, &mut data);
            tell_driver(&host, &mut msgs, &mut data);
        }
    }
}

/// The longest the clock's thread sleeps: what was not pending when it
/// went to sleep is at most this late.
const TICK_MAX: Duration = Duration::from_secs(1);
/// The shortest: a stack that always has something due does not spin.
const TICK_MIN: Duration = Duration::from_millis(1);

/// When it is due, do what the stack has pending that no frame and no
/// message will make it do, and tell the kernel what the card has carried
/// since it was last told: the turn that carried it may have been the
/// last for a while. It sleeps until then, or until a turn that carried
/// something wakes it to count the time again ([`tell_counts`]).
fn keep_time(host: Arc<Host>) {
    let _ = host.clock.set(std::thread::current());
    let mut msgs = [Msg::default(); TURN];
    let mut data = Vec::with_capacity(MESSAGE_MAX);
    loop {
        let (idle, counts) = {
            let shared = locked(&host.shared);
            (shared.stack.idle_for(now()), shared.stack.device().counts())
        };
        let nap = idle.map_or(TICK_MAX, |d| Duration::from_micros(d.total_micros()));
        let nap = counts_due_in(&host, &counts).map_or(nap, |due| nap.min(due));
        std::thread::park_timeout(nap.clamp(TICK_MIN, TICK_MAX));
        {
            let guard = locked(&host.shared);
            tell_counts(&host, &guard.stack.device().counts(), now());
            // Another thread's turn may have done it meanwhile, or put it
            // off: an acknowledgement that arrived, say.
            if guard.stack.idle_for(now()) != Some(smoltcp::time::Duration::ZERO) {
                continue;
            }
            turn(&host, guard, &mut data);
        }
        tell_driver(&host, &mut msgs, &mut data);
    }
}

/// Become the stack the kernel's served sockets use: the channel it will
/// speak on, or why there is none.
fn register(max_datagram: usize, card: Card) -> Result<Arc<Kernel>, String> {
    let (ours, theirs) = Channel::create().map_err(|e| format!("channel_create: {e}"))?;
    let theirs = theirs.into_handle();
    // SAFETY: no pointers. The handle is the kernel's when the call works.
    let served = unsafe {
        zx::call(
            nr::ECLIPSE_NETSTACK_SERVE,
            [
                theirs as usize,
                max_datagram,
                u32::from_be_bytes(card.address.address().0) as usize,
                card.address.prefix_len() as usize,
                card.gateway.map_or(0, |g| u32::from_be_bytes(g.0)) as usize,
                card.mac_number() as usize,
            ],
        )
    };
    match served {
        Ok(_) => Ok(Arc::new(Kernel {
            channel: ours,
            sending: Mutex::new(()),
        })),
        Err(e) => {
            zx::close(theirs);
            Err(match e {
                Status::ALREADY_BOUND => "another process serves the kernel's sockets".into(),
                e => format!("eclipse_netstack_serve: {e}"),
            })
        }
    }
}

/// What the kernel is told of the card when it is given its sockets: the
/// address the sockets have, and with the network, the gateway and the MAC
/// what a kernel where every program uses this stack shows as the
/// machine's interface (`ifconfig`, `ip`).
#[derive(Clone, Copy, PartialEq, Eq)]
struct Card {
    mac: [u8; 6],
    address: Ipv4Cidr,
    gateway: Option<Ipv4Address>,
}

impl Card {
    /// The MAC as a number, first byte highest: how it fits one argument.
    fn mac_number(&self) -> u64 {
        self.mac.iter().fold(0, |n, &byte| n << 8 | u64::from(byte))
    }
}

/// Give the kernel its sockets on this stack, whose card is `card`: once,
/// since the kernel is told the card with them. A second address or
/// gateway is the end of this process.
fn serve_kernel(host: &Arc<Host>, address: Ipv4Cidr, gateway: Option<Ipv4Address>) {
    let card = Card {
        mac: host.mac,
        address,
        gateway,
    };
    let mut told = locked(&host.told);
    match *told {
        None => {}
        Some(was) if was == card => return,
        Some(was) => {
            println!(
                "{NAME}: the address is {address}{} now and the kernel was told {}{}: starting over",
                through(gateway),
                was.address,
                through(was.gateway)
            );
            exit(3);
        }
    }
    *told = Some(card);
    let max_datagram = locked(&host.shared).stack.max_datagram();
    match register(max_datagram, card) {
        Ok(kernel) => {
            let _ = host.kernel.set(kernel.clone());
            let host = host.clone();
            std::thread::spawn(move || serve_sockets(host, kernel));
        }
        Err(why) => println!("{NAME}: no sockets, only the address: {why}"),
    }
}

/// `, with sockets` once the kernel has them here.
fn with_sockets(host: &Host) -> &'static str {
    if host.kernel.get().is_some() {
        ", with sockets"
    } else {
        ""
    }
}

/// `, gateway ADDR` when there is one.
fn through(gateway: Option<Ipv4Address>) -> String {
    gateway
        .map(|gateway| format!(", gateway {gateway}"))
        .unwrap_or_default()
}

/// The lease changed: the stack has made the interface what it says, and
/// the rest is told here. A first lease is when the kernel gets its
/// sockets.
fn settle(host: &Arc<Host>, lease: Option<Lease>) {
    let index = host.index;
    match lease {
        Some(lease) => {
            serve_kernel(host, lease.address, lease.gateway);
            println!(
                "{NAME}: card {index} is {} by its lease{}{}",
                lease.address,
                through(lease.gateway),
                with_sockets(host)
            );
            name_servers(host, |named| named.lease = lease.dns);
        }
        None => println!("{NAME}: card {index} lost its lease: no address until it has another"),
    }
    publish(host);
}

/// Say what the card has of IPv6 now, which is what its router made of
/// it.
fn say_sixes(host: &Host, sixes: &Sixes) {
    let index = host.index;
    let addresses: Vec<String> = sixes.addresses.iter().map(|a| a.to_string()).collect();
    let router = sixes
        .router
        .map(|router| format!(", router {router}"))
        .unwrap_or_default();
    match addresses.as_slice() {
        [] => println!("{NAME}: card {index} has no IPv6 address"),
        addresses => println!(
            "{NAME}: card {index} is {}{router}",
            addresses.join(" and ")
        ),
    }
    publish(host);
}

/// Write the name servers there are where every program's resolver
/// looks, once `heard` has said what changed of them: the lease's, as
/// the script of a `udhcpc` does for a card of the kernel's, and after
/// them the ones the link's routers name. While the card is the
/// machine's: the kernel has been told it by the time there is a lease,
/// and shows it at once if it is. What a router named before that is
/// kept, and written with the lease's.
fn name_servers(host: &Host, heard: impl FnOnce(&mut Named)) {
    let mut named = locked(&host.named);
    heard(&mut named);
    let Some(card) = resolv::card_name(Path::new(resolv::SYS_NET), host.mac) else {
        return;
    };
    let conf = resolv::RESOLV_CONF;
    let whose = match (named.lease.is_empty(), named.routers.is_empty()) {
        (false, true) => "the lease",
        (true, _) => "the router",
        (false, false) => "the lease and the router",
    };
    let servers = || named.written(&card).join(", ");
    match resolv::apply(&named, &card, Path::new(conf), Path::new(resolv::FIXED)) {
        Ok(Applied::Written) => println!("{NAME}: {conf} names {}, by {whose}", servers()),
        Ok(Applied::Fixed) => println!(
            "{NAME}: {whose} names {}, and {conf} keeps the servers fixed by hand",
            servers()
        ),
        Ok(Applied::Unchanged | Applied::Nothing) => {}
        Err(e) => eprintln!("{NAME}: {conf}: {e}"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // The card and its address, then at most one word on whose the card
    // is: a driver of its own is a virtio card's, and the kernel's card
    // has the kernel for its driver.
    let (words, apart, kernels) = match &args[1..] {
        [words @ .., last] if last == APART => (words, true, false),
        [words @ .., last] if last == KERNELS => (words, false, true),
        words => (words, false, false),
    };
    let parsed = match words {
        [index, how] => index.parse::<usize>().ok().zip(Addressing::parse(how)),
        _ => None,
    };
    let Some((index, how)) = parsed else {
        println!("{USAGE}");
        exit(2);
    };

    let taken = if kernels {
        borrow(index)
    } else if apart {
        start(index)
    } else {
        take(index)
    };
    let (mut link, port) = taken.unwrap_or_else(|e| die(e));
    let mac = link.mac;
    // The machine's settings for this card, when they say any, and not
    // the command line.
    let governed = governed::word(Path::new(governed::DIR), mac);
    let how = match governed.as_deref().map(Addressing::parse) {
        None => how,
        Some(Some(theirs)) => theirs,
        Some(None) => {
            println!(
                "{NAME}: {} says {:?}, which is no address: the command line's stands",
                governed::path(Path::new(governed::DIR), mac).display(),
                governed.as_deref().unwrap_or_default()
            );
            how
        }
    };
    // No address is what an interface that asks for one starts with.
    let address = match how {
        Addressing::Written(address, _) => address,
        Addressing::Leased => Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0),
    };
    let iface = InterfaceBuilder::new(port)
        .ethernet_addr(EthernetAddress(mac))
        .neighbor_cache(NeighborCache::new(BTreeMap::new()))
        .ip_addrs(vec![IpCidr::Ipv4(address)])
        .routes(Routes::new(BTreeMap::new()))
        .ipv4_multicast_groups(BTreeMap::new())
        .ipv6_multicast_groups(BTreeMap::new())
        .finalize();
    let mut stack = Stack::new(iface);
    match how {
        Addressing::Written(_, gateway) => {
            stack.set_gateway(gateway);
        }
        Addressing::Leased => stack.ask_for_lease(),
    }
    stack.speak_six();
    let host = Arc::new(Host {
        index,
        kernels,
        mac,
        governed,
        shared: Mutex::new(Shared {
            stack,
            wire: link.wire.clone(),
            answering: false,
            six_owed: false,
        }),
        kernel: OnceLock::new(),
        told: Mutex::new(None),
        named: Mutex::new(Named::default()),
        driver_pid: AtomicU32::new(match &link.taker {
            Taker::Driver { driver, .. } => driver.id(),
            Taker::Own(_) | Taker::Kernel => 0,
        }),
        driver_starts: AtomicU32::new(apart as u32),
        teller: Mutex::new(Teller::default()),
        clock: OnceLock::new(),
    });
    let card = format!(
        "{}card {index} ({})",
        if kernels { "the kernel's " } else { "" },
        EthernetAddress(mac)
    );
    match how {
        Addressing::Written(address, gateway) => {
            serve_kernel(&host, address, gateway);
            println!(
                "{NAME}: {card} is {address}{}{}",
                through(gateway),
                with_sockets(&host)
            );
        }
        // The clock's thread finds the first question due at once.
        Addressing::Leased => println!("{NAME}: {card} asks for its address"),
    }
    {
        let host = host.clone();
        std::thread::spawn(move || keep_time(host));
    }

    let mut pause = Duration::from_millis(100);
    loop {
        publish(&host);
        let since = std::time::Instant::now();
        let (fifo, driver) = match (&link.wire, &mut link.taker) {
            (Wire::Driver(fifo), Taker::Driver { driver, .. }) => (fifo, driver),
            // The card is this process's, and so is its failing: whoever
            // started the stack starts it again, with the card reset.
            (Wire::Own(own), Taker::Own(card)) => {
                let why = serve_card(&host, own, card);
                die(format_args!("card {index} is gone ({why})"));
            }
            // The kernel's, and the same goes: the loan ends with this
            // process and the next one borrows the card again.
            (Wire::Kernel(card), Taker::Kernel) => {
                let why = serve_lent(&host, card);
                die(format_args!("the kernel's card {index} is gone ({why})"));
            }
            _ => unreachable!("a link is made whole, of one kind"),
        };
        let why = serve(&host, fifo);
        let status = driver.wait();
        println!(
            "{NAME}: the driver is gone ({why}{}); starting another",
            status.map(|s| format!(", {s}")).unwrap_or_default()
        );
        if since.elapsed() >= SETTLED {
            pause = Duration::from_millis(100);
        }
        // The next driver, however long it takes: the address is still
        // this stack's, and nothing else can drive the card.
        let (next, port) = loop {
            std::thread::sleep(pause);
            pause = (pause * 2).min(PAUSE_MAX);
            match start(index) {
                Ok(next) if next.0.mac == mac => break next,
                Ok((mut other, _)) => {
                    if let Taker::Driver { driver, .. } = &mut other.taker {
                        let _ = driver.kill();
                        let _ = driver.wait();
                    }
                    die(format_args!(
                        "card {index} came back as {}: not the card this stack had",
                        EthernetAddress(other.mac)
                    ));
                }
                Err(e) => println!("{NAME}: {e}"),
            }
        };
        // The new port first, then the old link goes, its window with it:
        // nothing is left that points into that. Under the lock, so no
        // other thread is in the middle of a frame. What the card had
        // carried it has carried still.
        {
            let mut shared = locked(&host.shared);
            port.count_from(shared.stack.device().counts());
            *shared.stack.device_mut() = port;
            shared.wire = next.wire.clone();
        }
        link = next;
        if let Taker::Driver { driver, .. } = &link.taker {
            host.driver_pid.store(driver.id(), Ordering::Relaxed);
        }
        host.driver_starts.fetch_add(1, Ordering::Relaxed);
    }
}

/// Say what this stack is (see `udrivers::status`): the card, its address
/// and where that came from, what the machine's network settings said for
/// it, the gateway and the lease's name servers, and whose the card's
/// driver is: this process (`driver=own`), or a process of its own
/// (`driver=process`), with the one it runs now and how many it has
/// started, which eclipse-init never sees. lunarmon's SERVIDORES list
/// reads it.
fn publish(host: &Host) {
    let (address, gateway, dns, leases, sixes) = {
        let shared = locked(&host.shared);
        let stack = &shared.stack;
        let dns = stack.lease().map(|lease| lease.dns.clone());
        (
            stack.address(),
            stack.gateway(),
            dns.unwrap_or_default(),
            stack.leases(),
            stack.sixes(),
        )
    };
    let addr6: Vec<String> = sixes.addresses.iter().map(|a| a.to_string()).collect();
    let device = if host.kernels {
        format!("kernel-net@{}", host.index)
    } else {
        format!("virtio-net@{}", host.index)
    };
    let from = match (leases, &host.governed) {
        (true, _) => "dhcp",
        (false, Some(_)) => "network settings",
        (false, None) => "command line",
    };
    let or_none = |text: Option<String>| text.unwrap_or_else(|| "none".into());
    let dns: Vec<String> = dns.iter().map(|server| server.to_string()).collect();
    let starts = host.driver_starts.load(Ordering::Relaxed);
    let mut fields: Vec<(&str, String)> = vec![
        ("program", NAME.into()),
        ("device", device.clone()),
        ("mac", status::mac_text(host.mac)),
        ("addr", or_none(address.map(|a| a.to_string()))),
        ("addr_from", from.into()),
        (
            "governed",
            host.governed.clone().unwrap_or_else(|| "none".into()),
        ),
        ("gateway", or_none(gateway.map(|g| g.to_string()))),
        ("dns", or_none((!dns.is_empty()).then(|| dns.join(",")))),
        (
            "addr6",
            or_none((!addr6.is_empty()).then(|| addr6.join(","))),
        ),
        ("gateway6", or_none(sixes.router.map(|r| r.to_string()))),
    ];
    // A driver that is this process, or the kernel, is no other process
    // to show.
    if host.kernels {
        fields.push(("driver", "kernel".into()));
    } else if starts == 0 {
        fields.push(("driver", "own".into()));
    } else {
        fields.push(("driver", "process".into()));
        fields.push((
            "driver_pid",
            host.driver_pid.load(Ordering::Relaxed).to_string(),
        ));
        fields.push(("driver_starts", starts.to_string()));
    }
    let fields: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
    status::publish(&format!("stack.{device}"), &fields);
}
