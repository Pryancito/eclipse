//! `udrv-ps2`: the driver of the PC's i8042 that the kernel left to
//! userspace (`udriver=i8042@0` on its command line): its keyboard and its
//! mouse, served as two `/dev/input/eventK` (`udrivers::evdev`), the first
//! two that no other input device has.
//!
//! The kernel does each `in` and `out` for this process
//! (`udrivers::claim::port_in`), and its two interrupts, the keyboard's
//! and the mouse's, wake a thread each; either one takes every byte that
//! waits, whichever port it is from. LEDs written to the keyboard's node
//! go to the keyboard. When this process goes the nodes go with it; the
//! kernel turns both ports off, and the next driver starts them again.
//!
//! Exit status: 2 when the Zircon gateway is off, 1 on any other error and
//! when the kernel lets go of a node.

use std::process::exit;
use std::sync::Mutex;
use std::time::Duration;

use udrivers::claim::{self, Claim};
use udrivers::evdev::{self, Evdev, EV_KEY, EV_LED, EV_SYN, SYN_REPORT};
use udrivers::ps2::{self, Keyboard, Mouse, Ports};
use udrivers::served::Node;
use udrivers::status;
use udrivers::zx::{Handle, Status};

const NAME: &str = "udrv-ps2";

fn die(what: impl std::fmt::Display) -> ! {
    println!("{NAME}: {what}");
    exit(1);
}

fn claim_port(kind: usize, what: &str) -> Claim {
    match claim::claim(kind, 0, 300) {
        Ok(c) => c,
        Err(Status::NO_GATEWAY) => {
            println!("{NAME}: the gateway is off (zircon.gateway=0 on the command line?)");
            exit(2);
        }
        Err(e) => die(format_args!(
            "claim of the {what} (boot with udriver=i8042@0): {e}"
        )),
    }
}

/// What reads the controller, and what its bytes are halfway through.
struct Controller {
    io: Ports,
    keyboard: Keyboard,
    mouse: Mouse,
}

/// One input device of the two, served.
struct Served {
    node: Node,
    name: String,
    dev: Mutex<Evdev>,
}

fn serve(info: evdev::Info, place: &str) -> Served {
    let (node, name) = evdev::serve_free().unwrap_or_else(|e| die(e));
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
    Served {
        node,
        name,
        dev: Mutex::new(Evdev::new(info, evdev::now)),
    }
}

/// Take what the controller has, and give it to the two devices: the lock
/// of the controller is let go before a device's is taken, since writing
/// LEDs takes them the other way round.
///
/// Two threads come here, one per interrupt, and either takes the bytes of
/// both devices. [`IN_ORDER`] is held from the taking to the giving, so
/// that what one thread took first is given first: without it a key's
/// release, taken second, could reach the device before its press, and the
/// key stayed down for good. Nothing that holds another lock takes it.
fn service(ctl: &Mutex<Controller>, keyboard: &Served, mouse: &Served) {
    static IN_ORDER: Mutex<()> = Mutex::new(());
    let _in_order = IN_ORDER.lock().unwrap_or_else(|p| p.into_inner());
    let mut keys = Vec::new();
    let mut packets = Vec::new();
    {
        let mut c = ctl.lock().unwrap_or_else(|p| p.into_inner());
        let mut bytes = Vec::new();
        ps2::take(&mut c.io, &mut bytes);
        for (aux, b) in bytes {
            if aux {
                packets.extend(c.mouse.byte(b));
            } else {
                keys.extend(c.keyboard.byte(b));
            }
        }
    }
    if !keys.is_empty() {
        let mut d = keyboard.dev.lock().unwrap_or_else(|p| p.into_inner());
        for (code, down) in keys {
            // A key held is sent again by the keyboard: that is a repeat.
            let value = match (down, d.is_down(code)) {
                (false, _) => 0,
                (true, false) => 1,
                (true, true) => 2,
            };
            d.feed(EV_KEY, code, value);
            d.feed(EV_SYN, SYN_REPORT, 0);
        }
        evdev::tell(&keyboard.node, &mut d);
    }
    if !packets.is_empty() {
        let mut d = mouse.dev.lock().unwrap_or_else(|p| p.into_inner());
        for p in packets {
            for (kind, code, value) in ps2::packet_events(&p) {
                d.feed(kind, code, value);
            }
            d.feed(EV_SYN, SYN_REPORT, 0);
        }
        evdev::tell(&mouse.node, &mut d);
    }
}

fn main() {
    let kbd = claim_port(claim::KIND_I8042, "keyboard");
    let aux = claim_port(claim::KIND_I8042_AUX, "aux port");

    let mut io = Ports;
    let proto = ps2::init(&mut io);
    // It lives as long as the process, LEDs written to the node included.
    let ctl: &'static Mutex<Controller> = Box::leak(Box::new(Mutex::new(Controller {
        io,
        keyboard: Keyboard::default(),
        mouse: Mouse::new(proto),
    })));

    let keyboard = serve(ps2::keyboard_info(), "i8042@0");
    let mouse = serve(ps2::mouse_info(proto), "i8042-aux@0");
    {
        // LEDs the keyboard's readers light, as the keyboard has them.
        let mut lit = [false; 3];
        keyboard
            .dev
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .send_with(Box::new(move |out| {
                let before = lit;
                for &(kind, code, value) in out {
                    if kind == EV_LED && (code as usize) < lit.len() {
                        lit[code as usize] = value != 0;
                    }
                }
                if lit != before {
                    let mut c = ctl.lock().unwrap_or_else(|p| p.into_inner());
                    let [num, caps, scroll] = lit;
                    ps2::set_leds(&mut c.io, ps2::led_mask(num, caps, scroll));
                }
            }));
    }

    let wake = |irq: Option<Handle>| {
        let (keyboard, mouse) = (&keyboard, &mouse);
        move || loop {
            service(ctl, keyboard, mouse);
            match irq {
                Some(irq) => {
                    if let Err(e) = claim::interrupt_wait(irq) {
                        die(format_args!("interrupt_wait: {e}"));
                    }
                }
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    };
    if kbd.interrupt.is_none() || aux.interrupt.is_none() {
        println!("{NAME}: an interrupt is missing; polling");
    }
    std::thread::scope(|s| {
        s.spawn(wake(kbd.interrupt));
        s.spawn(wake(aux.interrupt));
        s.spawn(|| {
            let why = mouse.node.run(&mouse.dev);
            die(format_args!(
                "the kernel let go of /dev/{}: {why}",
                mouse.name
            ));
        });
        let why = keyboard.node.run(&keyboard.dev);
        die(format_args!(
            "the kernel let go of /dev/{}: {why}",
            keyboard.name
        ))
    })
}
