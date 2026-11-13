//! `eclipse-netstack N ADDR/PREFIX[,GATEWAY]` or `eclipse-netstack N dhcp`:
//! a network stack for virtio network card `N`, in this process, with the
//! address `ADDR` or the one a DHCP server leases it. What is not for the
//! card's network goes to the gateway, the one written or the lease's.
//! When the machine's network settings name the card, what they say is
//! used and not the command line (`netstack::governed`).
//!
//! The card must be one the kernel left to userspace
//! (`udriver=virtio-net@N` on its command line) and that no driver has: this
//! starts the driver itself (`/bin/udrv-virtio-net N stack=FD`), with a
//! channel on which the driver hands the card over, and starts it again
//! when it dies. The address, and what smoltcp knows of the neighbours,
//! stay through that: the interface carries nothing for a moment, as the
//! kernel's does.
//!
//! The kernel has no interface for this card. The stack answers for its
//! address by itself (ARP, ICMP echo), and serves the kernel the datagram
//! and stream sockets of the programs that asked for this stack
//! (`eclipse_netstack_serve`; `netstack::socks` has what the two say to
//! each other). On a kernel that has no such call it answers for its
//! address and nothing else.
//!
//! Three threads share the stack, under one lock. The card's takes what
//! the driver says, a turn in one system call, and sends the kernel what
//! arrived, all in one message. The sockets' takes what the kernel says,
//! every message that is waiting, and tells the driver of the frames that
//! made, all in one write. The third sleeps until the stack has something
//! to do that no message brings about: asking again for a neighbour that
//! did not answer, sending again a segment nobody acknowledged.
//!
//! One message for all that arrived is what keeps this from costing a
//! change of process for each datagram or segment: whoever is woken runs
//! at once, so a wake for each is one at a time through four processes.
//! The driver is woken as seldom, for the same reason: a receive slot to
//! give back waits for a frame to go with, and the acknowledgement of
//! what a program was just told waits a moment for the program's answer
//! (`HOLD`).
//!
//! The kernel is told the address once, for `getsockname`, when it is
//! given its sockets: at the start for an address that is written, with
//! the first lease for one that is asked for. A lease that comes back with
//! another address ends this process, for whoever started it to start it
//! again: the connections were the old address's.
//!
//! Exit status: 2 for a command line that is not one, 1 when the driver
//! cannot be started or gives no card, 3 when the lease's address changed.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{exit, Child, Command};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use netstack::governed;
use netstack::port::Port;
use netstack::resolv::{self, Applied};
use netstack::socks::lease::{Addressing, Lease};
use netstack::socks::tcp::OP_WRITE;
use netstack::socks::{parse_header, Arrived, Stack, MESSAGE_MAX, OP_SEND};
use smoltcp::iface::{InterfaceBuilder, NeighborCache, Routes};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpCidr, Ipv4Address, Ipv4Cidr};
use udrivers::channel::Channel;
use udrivers::fifo::Fifo;
use udrivers::net::{Msg, Offer, BUF_SIZE};
use udrivers::status;
use udrivers::zx::{self, nr, Handle, Status};

const NAME: &str = "eclipse-netstack";
const USAGE: &str = "usage: eclipse-netstack N ADDR/PREFIX[,GATEWAY] | eclipse-netstack N dhcp";
/// The card's driver, which this process starts.
const DRIVER: &str = "/bin/udrv-virtio-net";
/// The messages of one turn: as many as the driver's FIFO holds.
const TURN: usize = udrivers::net::FIFO_DEPTH;
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

/// A card and the driver that serves it.
struct Link {
    driver: Child,
    fifo: Arc<Fifo<Msg>>,
    /// Held for the port over it, which the interface has: dropped, with
    /// the rest of the link, once the interface is on another.
    _window: Window,
    mac: [u8; 6],
}

/// Start the driver of card `index` and take the card it offers: the link
/// to it, and the port for the stack.
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
                driver,
                fifo: Arc::new(fifo),
                _window: window,
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
    /// The FIFO of the driver the card has now, for the threads that are
    /// not the card's to tell it of a frame to send.
    fifo: Arc<Fifo<Msg>>,
    /// Whether the programs answer what arrives for them: one has sent
    /// something since the card's thread last waited for an answer to
    /// send its frames with (see [`HOLD`]).
    answering: bool,
}

/// What every thread has: the stack, and the kernel once it has sockets
/// here.
struct Host {
    index: usize,
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
    /// The driver the card has now and how many it has had, for
    /// [`publish`].
    driver_pid: AtomicU32,
    driver_starts: AtomicU32,
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
    shared.stack.poll(now());
    if let Some(lease) = shared.stack.lease_news() {
        // Not under the lock: the kernel may have to be told.
        drop(shared);
        settle(host, lease);
        shared = locked(&host.shared);
    }
    // Without the kernel there are no sockets for anything to arrive on.
    let Some(kernel) = host.kernel.get() else {
        return false;
    };
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

/// Tell the driver, from a thread that is not the card's, what this turn
/// of the stack made: the frames to send, mostly. The card's thread sends
/// its own with its next turn.
fn tell_driver(host: &Host, msgs: &mut [Msg; TURN]) {
    let (n, fifo) = {
        let shared = locked(&host.shared);
        (shared.stack.device().outgoing(msgs), shared.fifo.clone())
    };
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
/// program bytes, for the program's answer to go to the driver with them:
/// in nanoseconds.
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

/// Carry the card's frames through the stack until its driver is gone. Why
/// it stopped.
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
        let guard = locked(&host.shared);
        for m in &msgs[..got] {
            if !guard.stack.device().take(m) {
                println!("{NAME}: a message from the driver that means nothing: {m:?}");
            }
        }
        // Every frame that has room for its answer, in one go.
        if !turn(host, guard, &mut data) {
            continue;
        }
        let hold = {
            let mut shared = locked(&host.shared);
            let hold = shared.answering && shared.stack.device().has_frames();
            // Until a program sends again.
            shared.answering &= !hold;
            hold
        };
        if hold {
            // Time up, or the driver has more to say: either way what is
            // left to send goes with the next turn's call.
            if let Err(e) = fifo.wait_readable_for(HOLD) {
                return e;
            }
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
            tell_driver(&host, &mut msgs);
        }
    }
}

/// The longest the clock's thread sleeps: what was not pending when it
/// went to sleep is at most this late.
const TICK_MAX: Duration = Duration::from_secs(1);
/// The shortest: a stack that always has something due does not spin.
const TICK_MIN: Duration = Duration::from_millis(1);

/// When it is due, do what the stack has pending that no frame and no
/// message will make it do.
fn keep_time(host: Arc<Host>) {
    let mut msgs = [Msg::default(); TURN];
    let mut data = Vec::with_capacity(MESSAGE_MAX);
    loop {
        let idle = locked(&host.shared).stack.idle_for(now());
        let nap = idle.map_or(TICK_MAX, |d| Duration::from_micros(d.total_micros()));
        std::thread::sleep(nap.clamp(TICK_MIN, TICK_MAX));
        {
            let guard = locked(&host.shared);
            // Another thread's turn may have done it meanwhile, or put it
            // off: an acknowledgement that arrived, say.
            if guard.stack.idle_for(now()) != Some(smoltcp::time::Duration::ZERO) {
                continue;
            }
            turn(&host, guard, &mut data);
        }
        tell_driver(&host, &mut msgs);
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
            name_servers(host, &lease);
        }
        None => println!("{NAME}: card {index} lost its lease: no address until it has another"),
    }
    publish(host);
}

/// Write the name servers of a lease where every program's resolver
/// looks, as the script of a `udhcpc` does for a card of the kernel's.
/// While the card is the machine's: the kernel has been told it by now,
/// and shows it at once if it is.
fn name_servers(host: &Host, lease: &Lease) {
    if !resolv::is_the_machines(Path::new(resolv::SYS_NET), host.mac) {
        return;
    }
    let conf = resolv::RESOLV_CONF;
    let named = || {
        let dns: Vec<String> = lease.dns.iter().map(|d| d.to_string()).collect();
        dns.join(", ")
    };
    match resolv::apply(&lease.dns, Path::new(conf), Path::new(resolv::FIXED)) {
        Ok(Applied::Written) => println!("{NAME}: {conf} names {}, by the lease", named()),
        Ok(Applied::Fixed) => println!(
            "{NAME}: the lease names {}, and {conf} keeps the servers fixed by hand",
            named()
        ),
        Ok(Applied::Unchanged | Applied::Nothing) => {}
        Err(e) => eprintln!("{NAME}: {conf}: {e}"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let parsed = match &args[1..] {
        [index, how] => index.parse::<usize>().ok().zip(Addressing::parse(how)),
        _ => None,
    };
    let Some((index, how)) = parsed else {
        println!("{USAGE}");
        exit(2);
    };

    let (mut link, port) = start(index).unwrap_or_else(|e| die(e));
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
        .finalize();
    let mut stack = Stack::new(iface);
    match how {
        Addressing::Written(_, gateway) => {
            stack.set_gateway(gateway);
        }
        Addressing::Leased => stack.ask_for_lease(),
    }
    let host = Arc::new(Host {
        index,
        mac,
        governed,
        shared: Mutex::new(Shared {
            stack,
            fifo: link.fifo.clone(),
            answering: false,
        }),
        kernel: OnceLock::new(),
        told: Mutex::new(None),
        driver_pid: AtomicU32::new(link.driver.id()),
        driver_starts: AtomicU32::new(1),
    });
    let card = format!("card {index} ({})", EthernetAddress(mac));
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
        let why = serve(&host, &link.fifo);
        let status = link.driver.wait();
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
                    let _ = other.driver.kill();
                    let _ = other.driver.wait();
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
        // other thread is in the middle of a frame.
        {
            let mut shared = locked(&host.shared);
            *shared.stack.device_mut() = port;
            shared.fifo = next.fifo.clone();
        }
        link = next;
        host.driver_pid.store(link.driver.id(), Ordering::Relaxed);
        host.driver_starts.fetch_add(1, Ordering::Relaxed);
    }
}

/// Say what this stack is (see `udrivers::status`): the card, its address
/// and where that came from, what the machine's network settings said for
/// it, the gateway and the lease's name servers, the
/// driver it runs now and how many it has started, which eclipse-init
/// never sees. lunarmon's SERVIDORES list reads it.
fn publish(host: &Host) {
    let (address, gateway, dns, leases) = {
        let shared = locked(&host.shared);
        let stack = &shared.stack;
        let dns = stack.lease().map(|lease| lease.dns.clone());
        (
            stack.address(),
            stack.gateway(),
            dns.unwrap_or_default(),
            stack.leases(),
        )
    };
    let device = format!("virtio-net@{}", host.index);
    let from = match (leases, &host.governed) {
        (true, _) => "dhcp",
        (false, Some(_)) => "network settings",
        (false, None) => "command line",
    };
    let or_none = |text: Option<String>| text.unwrap_or_else(|| "none".into());
    let dns: Vec<String> = dns.iter().map(|server| server.to_string()).collect();
    status::publish(
        &format!("stack.{device}"),
        &[
            ("program", NAME),
            ("device", &device),
            ("mac", &status::mac_text(host.mac)),
            ("addr", &or_none(address.map(|a| a.to_string()))),
            ("addr_from", from),
            ("governed", host.governed.as_deref().unwrap_or("none")),
            ("gateway", &or_none(gateway.map(|g| g.to_string()))),
            ("dns", &or_none((!dns.is_empty()).then(|| dns.join(",")))),
            (
                "driver_pid",
                &host.driver_pid.load(Ordering::Relaxed).to_string(),
            ),
            (
                "driver_starts",
                &host.driver_starts.load(Ordering::Relaxed).to_string(),
            ),
        ],
    );
}
