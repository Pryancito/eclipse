//! `udrv-ahci [N [REPORT_FD]]`: the driver of AHCI controller `N` (default
//! 0) that the kernel left to userspace (`udriver=ahci@N` on its command
//! line), and of every SATA disk on it.
//!
//! Each disk goes to the kernel -- a new `/dev/sdX` -- and a thread of this
//! process serves it until the process is killed, as `udrv-virtio-blk`
//! does a virtio disk: same messages, same window, same kernel end
//! (`loader/src/linux/block_serve.rs`), the port its number among the
//! controller's disks. It is what a supervisor runs (`eclipse-init`, the
//! `uahci` service). When it dies the kernel resets the controller and
//! keeps the disks: their nodes stay, with whatever is mounted on them, and
//! requests wait up to ten seconds for the next driver.
//!
//! With `REPORT_FD`, a byte is written to that descriptor once every disk
//! is the kernel's, for a parent that waits for it.
//!
//! Exit status: 2 when the Zircon gateway is off, 1 on any other error and
//! when the kernel's end of a disk's FIFO goes away.

use std::process::exit;
use std::sync::Arc;

use udrivers::ahci::{self, Port, Regs, Wake, Waker};
use udrivers::blk::{self, Backend, Msg};
use udrivers::claim::{self, Dma};
use udrivers::fifo::Fifo;
use udrivers::status;
use udrivers::zx::{self, nr, Status};

const NAME: &str = "udrv-ahci";

fn die(what: impl std::fmt::Display) -> ! {
    println!("{NAME}: {what}");
    exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let index: usize = match args.get(1).map(|a| a.parse()) {
        None => 0,
        Some(Ok(index)) => index,
        Some(Err(_)) => die("usage: udrv-ahci [N [REPORT_FD]]"),
    };
    let report: Option<i32> = args.get(2).and_then(|a| a.parse().ok());

    // The driver before this one may have died a moment ago: three seconds
    // of patience.
    let claim = match claim::claim(claim::KIND_AHCI, index, 300) {
        Ok(claim) => claim,
        Err(Status::NO_GATEWAY) => {
            println!("{NAME}: the gateway is off (zircon.gateway=0 on the command line?)");
            exit(2);
        }
        Err(e) => die(format_args!(
            "claim (boot with udriver=ahci@N and a controller there): {e}"
        )),
    };
    if !claim.pci {
        die("the claim is not of a PCI function");
    }
    let base = claim.map_regs().unwrap_or_else(|e| die(e));
    // SAFETY: the claimed ABAR, mapped for good.
    let regs = unsafe { Regs::new(base) };
    if !regs.enable() {
        // Every DMA address here may be above 4 GiB.
        die("the controller addresses 32 bits only");
    }
    let ports = regs.disks();
    if ports.is_empty() {
        die("no SATA disk on the controller");
    }
    let wakes: Vec<(usize, Arc<Wake>)> = ports
        .iter()
        .map(|&p| (p, Arc::new(Wake::default())))
        .collect();
    // One disk sleeps on the interrupt itself; several share a thread
    // that wakes the one it is for.
    let direct = ports.len() == 1;
    if let (Some(irq), false) = (claim.interrupt, direct) {
        let wakes = wakes.clone();
        std::thread::spawn(move || {
            let status = ahci::interrupts(regs, irq, wakes);
            die(format_args!("interrupt wait: {status}"));
        });
    }
    if claim.interrupt.is_some() {
        regs.interrupts_on();
    }

    let mut serving = Vec::new();
    for (p, wake) in wakes {
        let dma = Dma::new(
            claim.bti,
            ahci::DMA_SIZE,
            Some((ahci::DMA_DATA, blk::DATA_MAX)),
        )
        .unwrap_or_else(|e| die(e));
        let window = dma.window.expect("asked for above");
        let wake = match claim.interrupt {
            None => Waker::Poll,
            Some(irq) if direct => Waker::Direct(irq),
            Some(_) => Waker::Shared(wake),
        };
        // SAFETY: `dma` is DMA_SIZE zeroed bytes the controller sees at
        // `daddr`, mapped for good, this port's alone.
        let port = match unsafe { Port::new(regs, p, dma.ptr, dma.daddr, wake) } {
            Ok(port) => port,
            Err(e) => {
                println!("{NAME}: {e}; port {p} left out");
                continue;
            }
        };
        let capacity = port.capacity();
        let (fifo, kernels) = Fifo::<Msg>::create(blk::FIFO_DEPTH)
            .unwrap_or_else(|e| die(format_args!("fifo_create: {e}")));
        // SAFETY: no pointers: two handles, which go to the kernel, and two
        // numbers.
        if let Err(e) = unsafe {
            zx::call(
                nr::ECLIPSE_BLOCK_SERVE,
                [
                    kernels as usize,
                    window as usize,
                    capacity as usize,
                    p,
                    0,
                    0,
                ],
            )
        } {
            die(format_args!("block_serve (port {p}): {e}"));
        }
        let place = format!("ahci@{index}.{p}");
        status::publish(
            &format!("driver.{place}"),
            &[
                ("program", NAME),
                ("device", &place),
                ("serves", "kernel"),
                ("sectors", &capacity.to_string()),
            ],
        );
        serving.push(std::thread::spawn(move || {
            let mut port = port;
            match blk::serve(&mut port, &fifo) {
                Status::PEER_CLOSED => die(format_args!(
                    "port {p}: the kernel's end of the FIFO went away"
                )),
                status => die(format_args!("port {p}: stopped serving: {status}")),
            }
        }));
    }
    if serving.is_empty() {
        die("no disk could be served");
    }
    if let Some(fd) = report {
        // SAFETY: one byte of a static string to a descriptor the parent
        // passed for this.
        unsafe {
            if libc::write(fd, b"u".as_ptr().cast(), 1) != 1 {
                die("write (report)");
            }
            libc::close(fd);
        }
    }
    for t in serving {
        let _ = t.join();
    }
    exit(1);
}
