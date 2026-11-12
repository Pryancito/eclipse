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
//! The kernel has no interface for this card. Until this stack has
//! sockets, all it does is answer for its address: ARP, and ICMP echo.
//!
//! Exit status: 2 for a command line that is not one, 1 when the driver
//! cannot be started or gives no card.

use std::collections::BTreeMap;
use std::process::{exit, Child, Command};
use std::time::Duration;

use netstack::port::Port;
use smoltcp::iface::{Interface, InterfaceBuilder, NeighborCache};
use smoltcp::socket::SocketSet;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpCidr, Ipv4Cidr};
use udrivers::channel::Channel;
use udrivers::fifo::Fifo;
use udrivers::net::{Msg, Offer, BUF_SIZE};
use udrivers::zx::{self, Handle, Status};

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
    fifo: Fifo<Msg>,
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
                fifo,
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

/// Carry the card's frames through the stack until its driver is gone. Why
/// it stopped.
///
/// One system call a turn (`eclipse_fifo_reply_wait`): what the last turn
/// left to say -- the receive slots that were read, the frames to send --
/// goes out, and the call comes back with what the card did meanwhile.
fn serve(
    iface: &mut Interface<'static, Port>,
    sockets: &mut SocketSet,
    fifo: &Fifo<Msg>,
) -> Status {
    let mut msgs = [Msg::default(); TURN];
    loop {
        let replies = iface.device().outgoing(&mut msgs);
        let got = match fifo.reply_wait(&mut msgs, replies) {
            Ok(got) => got,
            Err(e) => return e,
        };
        for m in &msgs[..got] {
            if !iface.device().take(m) {
                println!("{NAME}: a message from the driver that means nothing: {m:?}");
            }
        }
        // Every frame that has room for its answer, in one go. What poll
        // has to say about a frame it did not understand is not news.
        let _ = iface.poll(sockets, now());
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
    let mut iface = InterfaceBuilder::new(port)
        .ethernet_addr(EthernetAddress(mac))
        .neighbor_cache(NeighborCache::new(BTreeMap::new()))
        .ip_addrs(vec![IpCidr::Ipv4(addr)])
        .finalize();
    let mut sockets = SocketSet::new(vec![]);
    println!("{NAME}: card {index} ({}) is {addr}", EthernetAddress(mac));

    let mut pause = Duration::from_millis(100);
    loop {
        let since = std::time::Instant::now();
        let why = serve(&mut iface, &mut sockets, &link.fifo);
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
        // nothing is left that points into that.
        *iface.device_mut() = port;
        link = next;
    }
}
