//! `eclipse-netstack N ADDR/PREFIX`: a network stack for virtio network
//! card `N`, in this process, with the address `ADDR`.
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
//!
//! Exit status: 2 for a command line that is not one, 1 when the driver
//! cannot be started or gives no card.

use std::collections::BTreeMap;
use std::process::{exit, Child, Command};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use netstack::port::Port;
use netstack::socks::{Arrived, Stack, MESSAGE_MAX};
use smoltcp::iface::{InterfaceBuilder, NeighborCache};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpCidr, Ipv4Address, Ipv4Cidr};
use udrivers::channel::Channel;
use udrivers::fifo::Fifo;
use udrivers::net::{Msg, Offer, BUF_SIZE};
use udrivers::status;
use udrivers::zx::{self, nr, Handle, Status};

const NAME: &str = "eclipse-netstack";
const USAGE: &str = "usage: eclipse-netstack N ADDR/PREFIX";
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
}

type Lock = Arc<Mutex<Shared>>;

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
/// for its sockets. Takes the lock, which it lets go before the kernel is
/// told: that write wakes whoever reads the socket, who runs at once and
/// has something to send before this thread is back.
fn turn(mut shared: MutexGuard<'_, Shared>, kernel: Option<&Kernel>, data: &mut Vec<u8>) {
    shared.stack.poll(now());
    // Without the kernel there are no sockets for anything to arrive on.
    let Some(kernel) = kernel else { return };
    loop {
        let arrived = shared.stack.arrived(data);
        if arrived == Arrived::Nothing {
            return;
        }
        let sending = locked(&kernel.sending);
        if arrived == Arrived::All {
            drop(shared);
            // A kernel that is gone took the machine with it.
            let _ = kernel.channel.write(data, &[]);
            return;
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
fn tell_driver(shared: &Lock, msgs: &mut [Msg; TURN]) {
    let (n, fifo) = {
        let shared = locked(shared);
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
        locked(shared).stack.device().put_back(&msgs[went..n]);
    }
}

/// Carry the card's frames through the stack until its driver is gone. Why
/// it stopped.
///
/// One system call a turn (`eclipse_fifo_reply_wait`): what the last turn
/// left to say -- the receive slots that were read, the frames to send --
/// goes out, and the call comes back with what the card did meanwhile.
/// One more when a datagram arrived for a socket, to tell the kernel.
fn serve(shared: &Lock, kernel: Option<&Kernel>, fifo: &Fifo<Msg>) -> Status {
    let mut msgs = [Msg::default(); TURN];
    let mut data = Vec::with_capacity(MESSAGE_MAX);
    loop {
        let replies = locked(shared).stack.device().outgoing(&mut msgs);
        let got = match fifo.reply_wait(&mut msgs, replies) {
            Ok(got) => got,
            Err(e) => return e,
        };
        let shared = locked(shared);
        for m in &msgs[..got] {
            if !shared.stack.device().take(m) {
                println!("{NAME}: a message from the driver that means nothing: {m:?}");
            }
        }
        // Every frame that has room for its answer, in one go.
        turn(shared, kernel, &mut data);
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
fn serve_sockets(shared: Lock, kernel: Arc<Kernel>) {
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
        let mut sends = false;
        for done in 1.. {
            match locked(&shared).stack.request(&message[..len]) {
                Ok(send) => sends |= send,
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
            turn(locked(&shared), Some(&kernel), &mut data);
            tell_driver(&shared, &mut msgs);
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
fn keep_time(shared: Lock, kernel: Option<Arc<Kernel>>) {
    let mut msgs = [Msg::default(); TURN];
    let mut data = Vec::with_capacity(MESSAGE_MAX);
    loop {
        let idle = locked(&shared).stack.idle_for(now());
        let nap = idle.map_or(TICK_MAX, |d| Duration::from_micros(d.total_micros()));
        std::thread::sleep(nap.clamp(TICK_MIN, TICK_MAX));
        {
            let guard = locked(&shared);
            // Another thread's turn may have done it meanwhile, or put it
            // off: an acknowledgement that arrived, say.
            if guard.stack.idle_for(now()) != Some(smoltcp::time::Duration::ZERO) {
                continue;
            }
            turn(guard, kernel.as_deref(), &mut data);
        }
        tell_driver(&shared, &mut msgs);
    }
}

/// Become the stack the kernel's served sockets use: the channel it will
/// speak on, or why there is none.
fn register(max_datagram: usize, address: Ipv4Address) -> Result<Arc<Kernel>, String> {
    let (ours, theirs) = Channel::create().map_err(|e| format!("channel_create: {e}"))?;
    let theirs = theirs.into_handle();
    // SAFETY: no pointers. The handle is the kernel's when the call works.
    let served = unsafe {
        zx::call(
            nr::ECLIPSE_NETSTACK_SERVE,
            [
                theirs as usize,
                max_datagram,
                u32::from_be_bytes(address.0) as usize,
                0,
                0,
                0,
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let parsed = match &args[1..] {
        [index, addr] => index
            .parse::<usize>()
            .ok()
            .zip(addr.parse::<Ipv4Cidr>().ok()),
        _ => None,
    };
    let Some((index, addr)) = parsed else {
        println!("{USAGE}");
        exit(2);
    };

    let (mut link, port) = start(index).unwrap_or_else(|e| die(e));
    let mac = link.mac;
    let iface = InterfaceBuilder::new(port)
        .ethernet_addr(EthernetAddress(mac))
        .neighbor_cache(NeighborCache::new(BTreeMap::new()))
        .ip_addrs(vec![IpCidr::Ipv4(addr)])
        .finalize();
    let stack = Stack::new(iface);
    let kernel: Option<Arc<Kernel>> = match register(stack.max_datagram(), addr.address()) {
        Ok(kernel) => Some(kernel),
        Err(why) => {
            println!("{NAME}: no sockets, only the address: {why}");
            None
        }
    };
    let shared: Lock = Arc::new(Mutex::new(Shared {
        stack,
        fifo: link.fifo.clone(),
    }));
    println!(
        "{NAME}: card {index} ({}) is {addr}{}",
        EthernetAddress(mac),
        if kernel.is_some() {
            ", with sockets"
        } else {
            ""
        }
    );
    if let Some(kernel) = &kernel {
        let (shared, kernel) = (shared.clone(), kernel.clone());
        std::thread::spawn(move || serve_sockets(shared, kernel));
    }
    {
        let (shared, kernel) = (shared.clone(), kernel.clone());
        std::thread::spawn(move || keep_time(shared, kernel));
    }

    let mut pause = Duration::from_millis(100);
    let mut driver_starts = 1;
    loop {
        publish(index, addr, &link, driver_starts);
        let since = std::time::Instant::now();
        let why = serve(&shared, kernel.as_deref(), &link.fifo);
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
            let mut shared = locked(&shared);
            *shared.stack.device_mut() = port;
            shared.fifo = next.fifo.clone();
        }
        link = next;
        driver_starts += 1;
    }
}

/// Say what this stack is (see `udrivers::status`): the card, its
/// address, the driver it runs now and how many it has started, which
/// eclipse-init never sees. lunarmon's SERVIDORES list reads it.
fn publish(index: usize, addr: Ipv4Cidr, link: &Link, driver_starts: u32) {
    let device = format!("virtio-net@{index}");
    status::publish(
        &format!("stack.{device}"),
        &[
            ("program", NAME),
            ("device", &device),
            ("mac", &status::mac_text(link.mac)),
            ("addr", &addr.to_string()),
            ("driver_pid", &link.driver.id().to_string()),
            ("driver_starts", &driver_starts.to_string()),
        ],
    );
}
