//! `udrv-virtio-net [N [REPORT_FD | stack=FD]]`: the driver of virtio
//! network card `N` (default 0) that the kernel left to userspace
//! (`udriver=virtio-net@N` on its command line).
//!
//! The card goes to the kernel -- an interface, `ethN` -- and this process
//! carries its frames until it is killed. It is what a supervisor runs
//! (`eclipse-init`, the `unet` service). When it dies the kernel resets the
//! card and keeps the interface for ten seconds, with its addresses, routes
//! and connections: the next driver of the card with the same address gets
//! the same interface.
//!
//! With `REPORT_FD`, the card's address (six bytes) is written to that
//! descriptor once the card is the kernel's, for a parent that waits for
//! it.
//!
//! With `stack=FD`, the card does not go to the kernel at all. `FD` holds
//! one end of a channel, which the process that started this one made
//! (`tools/eclipse-netstack`), and what the kernel would have been given
//! goes down it instead: an [`Offer`] with the far end of the FIFO and the
//! window. That process is then the only one that sees the card's frames,
//! and they pass between the two without the kernel touching them.
//!
//! Exit status: 2 when the Zircon gateway is off, 1 on any other error and
//! when the other end of the FIFO goes away.

use std::process::exit;

use udrivers::channel::Channel;
use udrivers::claim::{self, Dma};
use udrivers::fifo::Fifo;
use udrivers::net::{self, Card, Feeder, Msg, Offer};
use udrivers::queue::Queue;
use udrivers::virtio::{self, Device, Setup};
use udrivers::zx::{self, nr, Status};

const NAME: &str = "udrv-virtio-net";
const USAGE: &str = "usage: udrv-virtio-net [N [REPORT_FD | stack=FD]]";

/// Who the card is served to.
enum Taker {
    /// The kernel, and the descriptor to report the address on, if any.
    Kernel(Option<i32>),
    /// The process at the other end of this channel.
    Stack(Channel),
}

fn die(what: impl std::fmt::Display) -> ! {
    println!("{NAME}: {what}");
    exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let index: usize = match args.get(1).map(|a| a.parse()) {
        None => 0,
        Some(Ok(index)) => index,
        Some(Err(_)) => die(USAGE),
    };
    // The channel is taken out of its descriptor before anything else: a
    // driver started for a stack must never fall back to the kernel.
    let taker = match args.get(2).map(|a| a.strip_prefix("stack=").ok_or(a)) {
        None => Taker::Kernel(None),
        Some(Err(report)) => Taker::Kernel(report.parse().ok()),
        Some(Ok(fd)) => match fd.parse().map(Channel::from_fd) {
            Ok(Ok(channel)) => Taker::Stack(channel),
            Ok(Err(e)) => die(format_args!(
                "stack={fd}: no channel in that descriptor: {e}"
            )),
            Err(_) => die(USAGE),
        },
    };

    // The driver before this one may have died a moment ago: three seconds
    // of patience.
    let claim = match claim::claim(claim::KIND_VIRTIO_NET, index, 300) {
        Ok(claim) => claim,
        Err(Status::NO_GATEWAY) => {
            println!("{NAME}: the gateway is off (zircon.gateway=0 on the command line?)");
            exit(2);
        }
        Err(e) => die(format_args!(
            "claim (boot with udriver=virtio-net@N and a card there): {e}"
        )),
    };
    let Some(transport) = claim.transport else {
        die("unknown transport");
    };
    let regs = claim.map_regs().unwrap_or_else(|e| die(e));
    // The slots are also the kernel's window: it copies a frame out of a
    // receive slot and into a transmit slot, and the card reads and writes
    // them where they are.
    let dma = Dma::new(
        claim.bti,
        net::DMA_SIZE,
        Some((net::DMA_WIN, net::WIN_SIZE)),
    )
    .unwrap_or_else(|e| die(e));
    let window = dma.window.expect("asked for above");

    // SAFETY: `regs` is the claimed device's window, mapped for good.
    let mut device = unsafe { Device::new(regs, transport) };
    // Both queues raise the interrupt: a frame the card has sent gives its
    // slot back to the kernel.
    let irq = claim.interrupt.is_some();
    let up = device
        .bring_up(&Setup {
            device_id: virtio::ID_NET,
            want: 0,
            need: net::F_MAC,
            queue_size: net::QUEUE_SIZE,
            queues: &[
                (dma.daddr + net::dma_queue(net::RXQ) as u64, irq),
                (dma.daddr + net::dma_queue(net::TXQ) as u64, irq),
            ],
        })
        .unwrap_or_else(|e| die(e));
    let mut mac = [0u8; 6];
    for (i, byte) in mac.iter_mut().enumerate() {
        *byte = device.config8(i as u32);
    }
    device.driver_ok();
    let interrupt = if up.irq_refused {
        println!("{NAME}: the device refused MSI-X vector 0; polling");
        None
    } else {
        claim.interrupt
    };

    // SAFETY: each queue's two pages of the DMA memory, zeroed, mapped for
    // good and nobody else's.
    let (rx_offers, rx_answers) =
        unsafe { Queue::new(dma.ptr.add(net::dma_queue(net::RXQ)), net::QUEUE_SIZE) }.split();
    let (tx_offers, tx_answers) =
        unsafe { Queue::new(dma.ptr.add(net::dma_queue(net::TXQ)), net::QUEUE_SIZE) }.split();
    let header = net::header_len(transport);
    // SAFETY: `dma` is DMA_SIZE bytes the device sees at `daddr`, mapped for
    // good; the halves are of the queues in it.
    let mut feeder = unsafe { Feeder::new(rx_offers, tx_offers, dma.ptr, dma.daddr, header) };
    let mut card = Card::new(rx_answers, tx_answers, header);
    feeder.fill(&device);

    let (fifo, kernels) = Fifo::<Msg>::create(net::FIFO_DEPTH)
        .unwrap_or_else(|e| die(format_args!("fifo_create: {e}")));
    match taker {
        Taker::Kernel(report) => {
            // SAFETY: no pointers: two handles, which go to the kernel, the
            // address and a count.
            if let Err(e) = unsafe {
                zx::call(
                    nr::ECLIPSE_NET_SERVE,
                    [
                        kernels as usize,
                        window as usize,
                        net::mac_word(mac) as usize,
                        net::RX_BUFS,
                        0,
                        0,
                    ],
                )
            } {
                die(format_args!("net_serve: {e}"));
            }
            if let Some(fd) = report {
                // SAFETY: the six bytes of `mac` to a descriptor the parent
                // passed for this.
                unsafe {
                    if libc::write(fd, mac.as_ptr().cast(), mac.len()) != mac.len() as isize {
                        die("write (report)");
                    }
                    libc::close(fd);
                }
            }
        }
        Taker::Stack(channel) => {
            // The channel is closed once the offer is in it: the FIFO is
            // what tells each side that the other has gone.
            if let Err(e) = channel.write(&Offer::of(mac).to_bytes(), &[kernels, window]) {
                die(format_args!("the stack did not take the card: {e}"));
            }
        }
    }

    // Two threads until one of them has a reason to stop, which ends the
    // process: there is nothing to return to. The card's first, then the
    // feeder's, and this one asleep: see `udrivers::net` for what that
    // order is worth.
    let (device, fifo) = (&device, &fifo);
    std::thread::scope(|s| {
        s.spawn(move || die(card.carry(device, interrupt, fifo)));
        s.spawn(move || die(feeder.feed(device, fifo)));
        loop {
            std::thread::park();
        }
    })
}
