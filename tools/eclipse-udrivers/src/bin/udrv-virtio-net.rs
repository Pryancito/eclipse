//! `udrv-virtio-net N stack=FD`: the driver of virtio network card `N`
//! that the kernel left to userspace (`udriver=virtio-net@N` on its command
//! line), for the network stack that runs as a process.
//!
//! `FD` holds one end of a channel, which the process that started this
//! one made (`tools/eclipse-netstack`, with `unetdriver=process`), and the
//! card is offered down it: an [`Offer`] with the far end of the FIFO and
//! the window. That process is then the only one that sees the card's
//! frames, and they pass between the two without the kernel touching
//! them. The kernel has no stack to give a card to.
//!
//! Exit status: 2 when the Zircon gateway is off, 1 on any other error and
//! when the other end of the FIFO goes away.

use std::process::exit;

use udrivers::channel::Channel;
use udrivers::fifo::Fifo;
use udrivers::net::{self, Msg, NoCard, Offer};
use udrivers::status;

const NAME: &str = "udrv-virtio-net";
const USAGE: &str = "usage: udrv-virtio-net N stack=FD";

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
    // The channel is taken out of its descriptor before anything else.
    let channel = match args.get(2).and_then(|a| a.strip_prefix("stack=")) {
        None => die(USAGE),
        Some(fd) => match fd.parse().map(Channel::from_fd) {
            Ok(Ok(channel)) => channel,
            Ok(Err(e)) => die(format_args!(
                "stack={fd}: no channel in that descriptor: {e}"
            )),
            Err(_) => die(USAGE),
        },
    };

    // The slots are also the stack's window: it copies a frame out of a
    // receive slot and into a transmit slot, and the card reads and writes
    // them where they are.
    let net::Driven {
        device,
        mut feeder,
        mut card,
        interrupt,
        irq_refused,
        mac,
        dma,
    } = match net::drive(index, true) {
        Ok(driven) => driven,
        Err(NoCard::NoGateway) => {
            println!("{NAME}: the gateway is off (zircon.gateway=0 on the command line?)");
            exit(2);
        }
        Err(NoCard::Other(e)) => die(e),
    };
    let window = dma.window.expect("asked for above");
    if irq_refused {
        println!("{NAME}: the device refused MSI-X vector 0; polling");
    }

    let (fifo, kernels) = Fifo::<Msg>::create(net::FIFO_DEPTH)
        .unwrap_or_else(|e| die(format_args!("fifo_create: {e}")));
    // The channel is closed once the offer is in it: the FIFO is what
    // tells each side that the other has gone.
    if let Err(e) = channel.write(&Offer::of(mac).to_bytes(), &[kernels, window]) {
        die(format_args!("the stack did not take the card: {e}"));
    }

    let place = format!("virtio-net@{index}");
    status::publish(
        &format!("driver.{place}"),
        &[
            ("program", NAME),
            ("device", &place),
            ("serves", "stack"),
            ("mac", &status::mac_text(mac)),
        ],
    );

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
