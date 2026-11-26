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
//! A USB disk on a port is served to the kernel as a disk (an `sdX`), from
//! a thread of its own, as `udrv-nvme` serves a namespace: the kernel's
//! requests come on a FIFO, the data in a window the controller reads and
//! writes directly. When the disk is pulled out its FIFO goes, and the
//! kernel drops the disk once no driver has come back for it.
//!
//! Exit status: 2 when the Zircon gateway is off, 1 on any other error and
//! when the kernel lets go of a node.

use std::collections::BTreeMap;
use std::process::exit;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use udrivers::blk::{self, Backend, Io, Msg};
use udrivers::claim::{self, Dma};
use udrivers::evdev::{self, Evdev, EV_LED, EV_SYN, SYN_REPORT};
use udrivers::fifo::Fifo;
use udrivers::served::Node;
use udrivers::status;
use udrivers::usbhid;
use udrivers::xhci::{self, Role, Xhci};
use udrivers::zx::{self, nr, Handle, Status};

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
    /// The claim's BTI, for each disk's window.
    bti: Handle,
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
        Ok(new) => {
            for hid in new {
                describe(shared, x, hid, out);
            }
        }
        Err(e) => println!("{NAME}: port {port}: {e}"),
    }
}

/// What HID `hid`, just added, is to be served as.
fn describe(shared: &'static Shared, x: &Xhci, hid: usize, out: &mut Vec<Added>) {
    let Some(h) = x.hids[hid].as_ref() else {
        return;
    };
    let place = format!("xhci@{}-{}.{}", shared.index, h.path, h.iface);
    let phys = format!("usb-xhci{}-{}/input{}", shared.index, h.path, h.iface);
    let (info, keyboard) = match &h.role {
        Role::Keyboard(_) => (usbhid::keyboard_info(&h.name, &phys, h.id), true),
        Role::Pointer(p) => (p.info(&h.name, &phys, h.id), false),
    };
    out.push((hid, info, place, keyboard));
}

/// Take what the controller has, and give it to the nodes. The lock of the
/// controller is let go before a node's is taken, since writing LEDs takes
/// them the other way round. Whether events wait still, set aside while a
/// device was enumerated.
///
/// Several threads come here (the interrupt's, the LEDs', each disk's
/// after its turn). [`IN_ORDER`] is held from the taking to the giving, so
/// that reports reach a node in the order the controller gave them: without
/// it a key's release could overtake its press, and the key stayed down.
/// Nothing that holds another lock takes it.
fn service(shared: &'static Shared) -> bool {
    static IN_ORDER: Mutex<()> = Mutex::new(());
    let _in_order = lock(&IN_ORDER);
    let mut frames: Vec<Frame> = Vec::new();
    let mut added = Vec::new();
    let pending;
    let mut gone_any = false;
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
        for (hub, bitmap) in found.hubs {
            let changes = x.hub_changed(hub, &bitmap);
            // A disk behind it may have gone: its thread looks.
            gone_any = true;
            for hid in changes.added {
                describe(shared, &x, hid, &mut added);
            }
            let mut served = lock(&shared.served);
            for hid in changes.gone {
                if let Some(s) = served.remove(&hid) {
                    println!(
                        "{NAME}: /dev/{} was pulled out of its hub: it is quiet",
                        s.name
                    );
                }
            }
        }
        for port in found.ports {
            if x.connected(port) && !x.attached(port) {
                attach(shared, &mut x, port, &mut added);
            } else if !x.connected(port) && x.attached(port) {
                let gone = x.detach(port);
                gone_any = true;
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
    if gone_any {
        kick_disks();
    }
    for (hid, info, place, keyboard) in added {
        serve(shared, hid, info, &place, keyboard);
    }
    serve_new_disks(shared);
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

/// A disk on the controller, as the kernel's requests reach it.
struct UsbDisk {
    shared: &'static Shared,
    disk: usize,
    sectors: u64,
    /// Where the controller sees the window.
    daddr: u64,
}

impl Backend for UsbDisk {
    fn capacity(&self) -> u64 {
        self.sectors
    }

    fn has_flush(&self) -> bool {
        true
    }

    fn run(&mut self, ios: &[Io], done: &mut [bool]) -> Result<(), Status> {
        let mut gone = false;
        let pending;
        {
            let mut x = lock(&self.shared.xhci);
            x.quiet(true);
            let mut i = 0;
            while i < ios.len() && !gone {
                let io = &ios[i];
                // Requests that follow one another on the disk and in the
                // window go as one command.
                let mut n = 1;
                let mut len = io.len();
                if !io.is_flush() {
                    while let Some(next) = ios.get(i + n) {
                        let follows = next.is_read() == io.is_read()
                            && !next.is_flush()
                            && next.sector() == io.sector() + (len / blk::SECTOR) as u64
                            && next.off() == io.off() + len
                            && len + next.len() <= blk::DATA_MAX;
                        if !follows {
                            break;
                        }
                        len += next.len();
                        n += 1;
                    }
                }
                let (sectors, at) = if io.is_flush() {
                    (0, 0)
                } else {
                    ((len / blk::SECTOR) as u32, self.daddr + io.off() as u64)
                };
                let write = !io.is_read() && !io.is_flush();
                let ok = match x.disk_io(self.disk, write, io.sector(), sectors, at) {
                    Ok(ok) => ok,
                    Err(e) => {
                        println!("{NAME}: disk {}: {e}", self.disk);
                        gone = x.disks.get(self.disk).is_none_or(|d| d.is_none());
                        false
                    }
                };
                done[i..i + n].fill(ok);
                i += n;
            }
            x.quiet(false);
            pending = x.pending();
        }
        // What came while the disk had the controller waits in its stash.
        if pending {
            while service(self.shared) {}
        }
        if gone {
            return Err(Status::PEER_CLOSED);
        }
        Ok(())
    }
}

/// Serve disk `k` of the controller to the kernel, from a thread of its
/// own, until it goes.
fn serve_disk(
    shared: &'static Shared,
    k: usize,
    name: String,
    sectors: u64,
    port: &str,
    unit: usize,
    iface: u8,
) {
    let dma = match Dma::new(shared.bti, blk::DATA_MAX, Some((0, blk::DATA_MAX))) {
        Ok(d) => d,
        Err(e) => {
            println!("{NAME}: disk on port {port}: {e}");
            return;
        }
    };
    let window = dma.window.expect("asked for above");
    let (fifo, kernels) = match Fifo::<Msg>::create(blk::FIFO_DEPTH) {
        Ok(f) => f,
        Err(e) => {
            println!("{NAME}: disk on port {port}: fifo_create: {e}");
            return;
        }
    };
    // SAFETY: no pointers: two handles, which go to the kernel, a count
    // and which of the controller's disks it is (its root port and route,
    // which a driver started again finds it on).
    if let Err(e) = unsafe {
        zx::call(
            nr::ECLIPSE_BLOCK_SERVE,
            [
                kernels as usize,
                window as usize,
                sectors as usize,
                unit,
                0,
                0,
            ],
        )
    } {
        println!("{NAME}: disk on port {port}: block_serve: {e}");
        return;
    }
    let place = format!("xhci@{}-{port}.{iface}", shared.index);
    status::publish(
        &format!("driver.{place}"),
        &[
            ("program", NAME),
            ("device", &place),
            ("serves", "kernel"),
            ("sectors", &sectors.to_string()),
            ("name", &name),
        ],
    );
    println!("{NAME}: {place} \"{name}\" is a disk of {sectors} sectors");
    // The window is mapped for good (`Dma` unmaps nothing): the kernel may
    // still use it after the disk goes.
    let daddr = dma.daddr;
    std::thread::spawn(move || {
        let mut disk = UsbDisk {
            shared,
            disk: k,
            sectors,
            daddr,
        };
        let why = serve_until_gone(&mut disk, &fifo);
        println!("{NAME}: {place}: stopped serving: {why}");
        // The FIFO goes with this thread: the kernel waits for a driver of
        // this disk again, and drops it when none comes.
        drop(fifo);
    });
}

/// The threads serving disks, to wake with [`KICK`] when a disk may have
/// gone.
/// (As numbers: musl's `pthread_t` is a pointer, which a static cannot hold.)
static DISK_THREADS: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// The signal that wakes a disk's thread from its wait on the kernel.
const KICK: libc::c_int = libc::SIGUSR1;

extern "C" fn kicked(_: libc::c_int) {}

/// Have [`KICK`] cut a wait short, without killing anyone.
fn take_kicks() {
    // SAFETY: a handler that does nothing, without SA_RESTART so that the
    // wait it lands in ends.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = kicked as extern "C" fn(libc::c_int) as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(KICK, &sa, std::ptr::null_mut());
    }
}

/// How many times [`kick_disks`] has been called: a kick that came while
/// the thread was not waiting is seen by this.
static KICKS: AtomicUsize = AtomicUsize::new(0);

/// Wake every disk's thread to look whether its disk is still there.
fn kick_disks() {
    KICKS.fetch_add(1, Ordering::SeqCst);
    for &t in lock(&DISK_THREADS).iter() {
        // SAFETY: a thread on the list has not ended: it takes itself off
        // under this lock before it does.
        unsafe { libc::pthread_kill(t as libc::pthread_t, KICK) };
    }
}

/// Answer the kernel's requests for `disk` until its end of the FIFO goes,
/// the disk stops answering, or it is pulled out. Each turn is one call:
/// the replies go out and the next requests come in. A disk pulled out
/// kicks the thread awake, and its FIFO goes then.
fn serve_until_gone(disk: &mut UsbDisk, fifo: &Fifo<Msg>) -> Status {
    // SAFETY: this thread's own id.
    let me = unsafe { libc::pthread_self() } as usize;
    lock(&DISK_THREADS).push(me);
    let why = serve_turns(disk, fifo);
    lock(&DISK_THREADS).retain(|&t| t != me);
    why
}

fn serve_turns(disk: &mut UsbDisk, fifo: &Fifo<Msg>) -> Status {
    let mut msgs = [Msg::default(); blk::FIFO_DEPTH];
    let mut got = 0;
    let mut kicks = KICKS.load(Ordering::SeqCst);
    let gone = |disk: &UsbDisk| {
        let x = lock(&disk.shared.xhci);
        x.disks.get(disk.disk).is_none_or(|d| d.is_none())
    };
    loop {
        let now = KICKS.load(Ordering::SeqCst);
        if now != kicks {
            kicks = now;
            if gone(disk) {
                let _ = fifo.write(&msgs[..got]);
                return Status::PEER_CLOSED;
            }
        }
        got = match fifo.reply_wait_once(&mut msgs, got) {
            Ok(n) => n,
            // The replies went before the kick; the next turn looks.
            Err(Status::INTERNAL_INTR_RETRY) => 0,
            Err(e) => return e,
        };
        if got == 0 {
            continue;
        }
        let (capacity, has_flush) = (disk.capacity(), disk.has_flush());
        let mut failed = None;
        blk::answer(&mut msgs[..got], capacity, has_flush, |ios, done| {
            if failed.is_none() {
                if let Err(status) = disk.run(ios, done) {
                    failed = Some(status);
                }
            }
        });
        if let Some(status) = failed {
            let _ = fifo.write(&msgs[..got]);
            return status;
        }
    }
}

/// Serve every disk the controller added since the last look.
fn serve_new_disks(shared: &'static Shared) {
    let fresh: Vec<_> = {
        let mut x = lock(&shared.xhci);
        let ks = x.new_disks();
        ks.into_iter()
            .filter_map(|k| {
                x.disks[k].as_ref().map(|d| {
                    (
                        k,
                        d.name.clone(),
                        d.sectors,
                        d.path.clone(),
                        d.unit,
                        d.iface,
                    )
                })
            })
            .collect()
    };
    for (k, name, sectors, path, unit, iface) in fresh {
        serve_disk(shared, k, name, sectors, &path, unit, iface);
    }
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
    take_kicks();
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
        bti: claim.bti,
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
    serve_new_disks(shared);
    if lock(&shared.served).is_empty() && lock(&shared.xhci).disks.is_empty() {
        println!("{NAME}: no keyboard, pointer or disk on the controller yet");
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
