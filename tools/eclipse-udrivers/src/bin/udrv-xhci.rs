//! `udrv-xhci [N]`: the driver of xHCI USB controller `N` (default 0),
//! which the kernel left to userspace (`udriver=xhci@N` on its command
//! line), and of the keyboards and pointers on its root ports: each one
//! served as a `/dev/input/eventK` (`udrivers::evdev`), the first that no
//! other input device has.
//!
//! The controller's interrupt wakes a thread that takes every event the
//! controller has (`udrivers::xhci`), turns each report into evdev events
//! (`udrivers::usbhid`) and gives them to the device's node. A device
//! plugged in later gets its nodes then; one pulled out leaves its nodes,
//! quiet. LEDs written to a keyboard's node go to the keyboard, from a
//! thread of their own. When this process goes the nodes go with it; the
//! kernel stops and resets the controller, and the next driver starts it
//! again.
//!
//! Exit status: 2 when the Zircon gateway is off, 1 on any other error and
//! when the kernel lets go of a node.

use std::collections::BTreeMap;
use std::process::exit;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use udrivers::claim::{self, Dma};
use udrivers::evdev::{self, Evdev, EV_LED, EV_SYN, SYN_REPORT};
use udrivers::served::Node;
use udrivers::status;
use udrivers::usbhid;
use udrivers::xhci::{self, Role, Xhci};
use udrivers::zx::Status;

const NAME: &str = "udrv-xhci";

fn die(what: impl std::fmt::Display) -> ! {
    println!("{NAME}: {what}");
    exit(1);
}

/// A HID just added, to serve: its index, what its node says, where it
/// is, and whether it is a keyboard.
type Added = (usize, evdev::Info, String, bool);
/// The events of one report of a HID, by its index.
type Frame = (usize, Vec<(u16, u16, i32)>);

/// One HID interface, served.
struct Served {
    node: Node,
    name: String,
    dev: Mutex<Evdev>,
}

/// What every thread shares.
struct Shared {
    index: usize,
    xhci: Mutex<Xhci>,
    /// The node of each HID, by its index in the controller's list.
    served: Mutex<BTreeMap<usize, &'static Served>>,
    /// LED reports to send: (HID, report).
    leds: Mutex<Vec<(usize, u8)>>,
    led_wanted: Condvar,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Serve HID `hid` of the controller, which has just been added.
fn serve(shared: &'static Shared, hid: usize, info: evdev::Info, place: &str, keyboard: bool) {
    let (node, name) = match evdev::serve_free() {
        Ok(n) => n,
        Err(e) => {
            println!("{NAME}: {place}: {e}");
            return;
        }
    };
    status::publish(
        &format!("driver.{place}"),
        &[
            ("program", NAME),
            ("device", place),
            ("serves", &format!("/dev/{name}")),
            ("name", &info.name),
        ],
    );
    println!("{NAME}: {place} \"{}\" is /dev/{name}", info.name);
    let served: &'static Served = Box::leak(Box::new(Served {
        node,
        name,
        dev: Mutex::new(Evdev::new(info, evdev::now)),
    }));
    if keyboard {
        // LEDs the keyboard's readers light, as the keyboard has them.
        let mut lit = [false; 3];
        lock(&served.dev).send_with(Box::new(move |out| {
            let before = lit;
            for &(kind, code, value) in out {
                if kind == EV_LED && (code as usize) < lit.len() {
                    lit[code as usize] = value != 0;
                }
            }
            if lit != before {
                lock(&shared.leds).push((hid, usbhid::led_report(&lit)));
                shared.led_wanted.notify_one();
            }
        }));
    }
    lock(&shared.served).insert(hid, served);
    std::thread::spawn(move || {
        let why = served.node.run(&served.dev);
        die(format_args!(
            "the kernel let go of /dev/{}: {why}",
            served.name
        ));
    });
}

/// Drive the device on `port`, and serve what it has.
fn attach(shared: &'static Shared, x: &mut Xhci, port: u8, out: &mut Vec<Added>) {
    match x.attach(port) {
        Ok(added) => {
            for hid in added {
                let Some(h) = x.hids[hid].as_ref() else {
                    continue;
                };
                let place = format!("xhci@{}-{}.{}", shared.index, h.port, h.iface);
                let phys = format!("usb-xhci{}-{}/input{}", shared.index, h.port, h.iface);
                let (info, keyboard) = match &h.role {
                    Role::Keyboard(_) => (usbhid::keyboard_info(&h.name, &phys, h.id), true),
                    Role::Pointer(p) => (p.info(&h.name, &phys, h.id), false),
                };
                out.push((hid, info, place, keyboard));
            }
        }
        Err(e) => println!("{NAME}: port {port}: {e}"),
    }
}

/// Take what the controller has, and give it to the nodes. The lock of the
/// controller is let go before a node's is taken, since writing LEDs takes
/// them the other way round. Whether events wait still, set aside while a
/// device was enumerated.
fn service(shared: &'static Shared) -> bool {
    let mut frames: Vec<Frame> = Vec::new();
    let mut added = Vec::new();
    let pending;
    {
        let mut x = lock(&shared.xhci);
        let found = x.service();
        for (hid, r) in found.reports {
            let Some(h) = x.hids[hid].as_mut() else {
                continue;
            };
            let events = match &mut h.role {
                Role::Keyboard(k) => k.report(&r),
                Role::Pointer(p) => p.report(&r),
            };
            frames.push((hid, events));
        }
        for port in found.ports {
            if x.connected(port) && !x.attached(port) {
                attach(shared, &mut x, port, &mut added);
            } else if !x.connected(port) && x.attached(port) {
                let gone = x.detach(port);
                let mut served = lock(&shared.served);
                for hid in gone {
                    if let Some(s) = served.remove(&hid) {
                        println!(
                            "{NAME}: port {port} was pulled out: /dev/{} is quiet",
                            s.name
                        );
                    }
                }
            }
        }
        pending = x.pending();
    }
    for (hid, info, place, keyboard) in added {
        serve(shared, hid, info, &place, keyboard);
    }
    let served = lock(&shared.served).clone();
    for (hid, events) in frames {
        let Some(s) = served.get(&hid) else {
            continue;
        };
        let mut d = lock(&s.dev);
        for (kind, code, value) in events {
            d.feed(kind, code, value);
        }
        d.feed(EV_SYN, SYN_REPORT, 0);
        evdev::tell(&s.node, &mut d);
    }
    pending
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let index: usize = match args.get(1).map(|a| a.parse()) {
        None => 0,
        Some(Ok(index)) => index,
        Some(Err(_)) => die("usage: udrv-xhci [N]"),
    };
    let claim = match claim::claim(claim::KIND_XHCI, index, 300) {
        Ok(c) => c,
        Err(Status::NO_GATEWAY) => {
            println!("{NAME}: the gateway is off (zircon.gateway=0 on the command line?)");
            exit(2);
        }
        Err(e) => die(format_args!(
            "claim (boot with udriver=xhci@N and a controller there): {e}"
        )),
    };
    if !claim.pci {
        die("the claim is not of a PCI function");
    }
    let regs = claim.map_regs().unwrap_or_else(|e| die(e));
    let dma = Dma::new(claim.bti, xhci::DMA_SIZE, None).unwrap_or_else(|e| die(e));
    if claim.interrupt.is_none() {
        println!("{NAME}: no interrupt; polling");
    }
    // SAFETY: `regs` is the claimed BAR, mapped for good; `dma` is
    // DMA_SIZE zeroed bytes the controller sees at `daddr`, mapped for
    // good, touched by no one else.
    let x = unsafe { Xhci::new(regs, &dma, claim.interrupt.is_some()) }.unwrap_or_else(|e| die(e));
    let shared: &'static Shared = Box::leak(Box::new(Shared {
        index,
        xhci: Mutex::new(x),
        served: Mutex::new(BTreeMap::new()),
        leds: Mutex::new(Vec::new()),
        led_wanted: Condvar::new(),
    }));
    // A device is seen on its port a moment after the port has power.
    std::thread::sleep(Duration::from_millis(50));
    let mut added = Vec::new();
    {
        let mut x = lock(&shared.xhci);
        for port in 1..=x.ports() {
            if x.connected(port) {
                attach(shared, &mut x, port, &mut added);
            }
        }
    }
    for (hid, info, place, keyboard) in added {
        serve(shared, hid, info, &place, keyboard);
    }
    if lock(&shared.served).is_empty() {
        println!("{NAME}: no keyboard or pointer on the controller yet");
    }

    std::thread::spawn(move || loop {
        let want = {
            let mut leds = lock(&shared.leds);
            while leds.is_empty() {
                leds = shared
                    .led_wanted
                    .wait(leds)
                    .unwrap_or_else(|p| p.into_inner());
            }
            std::mem::take(&mut *leds)
        };
        {
            let mut x = lock(&shared.xhci);
            for (hid, report) in want {
                if let Err(e) = x.set_leds(hid, report) {
                    println!("{NAME}: LEDs: {e}");
                }
            }
        }
        // What came while the LEDs went out waits in the controller's
        // stash, and no interrupt will say so again.
        while service(shared) {}
    });

    loop {
        while service(shared) {}
        match claim.interrupt {
            Some(irq) => {
                if let Err(e) = claim::interrupt_wait(irq) {
                    die(format_args!("interrupt_wait: {e}"));
                }
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}
