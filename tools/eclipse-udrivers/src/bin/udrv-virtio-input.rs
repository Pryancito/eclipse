//! `udrv-virtio-input [N]`: the driver of virtio input device `N` (default
//! 0) that the kernel left to userspace (`udriver=virtio-input@N` on its
//! command line), served as `/dev/input/eventK`: the first `K` that no
//! other input device has.
//!
//! What programs read of the node, and what its ioctls answer, is evdev's
//! (`udrivers::evdev`): libinput, a compositor or `evtest` take it as they
//! take a node of the kernel's. When this process goes the node goes with
//! it, and whoever had it open is told the device is gone; the kernel
//! resets the device, and the next driver serves it again.
//!
//! Exit status: 2 when the Zircon gateway is off, 1 on any other error and
//! when the kernel lets go of the node.

use std::process::exit;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use udrivers::claim;
use udrivers::evdev::{self, Evdev};
use udrivers::input::{self, NoDevice};
use udrivers::served::Node;
use udrivers::status;

const NAME: &str = "udrv-virtio-input";
/// The major of input devices, and the minor of `event0`.
const INPUT_MAJOR: u32 = 13;
const EVENT_MINOR_BASE: u32 = 64;
/// How many `eventK` are tried.
const EVENT_NODES: u32 = 32;

fn die(what: impl std::fmt::Display) -> ! {
    println!("{NAME}: {what}");
    exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let index: usize = match args.get(1).map(|a| a.parse()) {
        None => 0,
        Some(Ok(index)) => index,
        Some(Err(_)) => die("usage: udrv-virtio-input [N]"),
    };
    let input::Driven {
        device,
        mut events,
        status: statusq,
        info,
        interrupt,
        dma: _dma,
    } = match input::drive(index) {
        Ok(driven) => driven,
        Err(NoDevice::NoGateway) => {
            println!("{NAME}: the gateway is off (zircon.gateway=0 on the command line?)");
            exit(2);
        }
        Err(NoDevice::Other(e)) => die(e),
    };
    if interrupt.is_none() {
        println!("{NAME}: no interrupt from the device; polling");
    }

    // The first eventK nobody has: the kernel's own input devices take
    // theirs first, and other drivers like this one theirs.
    let mut last = String::new();
    let served = (0..EVENT_NODES).find_map(|k| {
        let name = format!("input/event{k}");
        match Node::serve(&name, INPUT_MAJOR, EVENT_MINOR_BASE + k, 0o660) {
            Ok(node) => Some((node, name)),
            Err(e) => {
                last = e;
                None
            }
        }
    });
    let Some((node, name)) = served else {
        die(format_args!("no /dev/input/event* to serve: {last}"))
    };

    let place = format!("virtio-input@{index}");
    status::publish(
        &format!("driver.{place}"),
        &[
            ("program", NAME),
            ("device", &place),
            ("serves", &format!("/dev/{name}")),
            ("name", &info.name),
        ],
    );
    println!("{NAME}: {place} \"{}\" is /dev/{name}", info.name);

    let device = Arc::new(device);
    let statusq = Arc::new(Mutex::new(statusq));
    let mut dev = Evdev::new(info, evdev::now);
    {
        let (device, statusq) = (device.clone(), statusq.clone());
        dev.send_with(Box::new(move |out| {
            statusq
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .send(&device, out)
        }));
    }
    let dev = Mutex::new(dev);

    let (node, dev, device) = (&node, &dev, &*device);
    std::thread::scope(|s| {
        // The device's events, as they come.
        s.spawn(move || loop {
            match interrupt {
                Some(irq) => {
                    if let Err(e) = claim::interrupt_wait(irq) {
                        die(format_args!("interrupt_wait: {e}"));
                    }
                }
                None => std::thread::sleep(Duration::from_millis(5)),
            }
            device.ack();
            let mut d = dev.lock().unwrap_or_else(|p| p.into_inner());
            events.drain(device, |kind, code, value| {
                d.feed(kind, code, value);
            });
            evdev::tell(node, &mut d);
        });
        die(format_args!(
            "the kernel let go of /dev/{name}: {}",
            node.run(dev)
        ));
    })
}
