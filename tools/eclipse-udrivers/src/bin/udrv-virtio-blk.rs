//! `udrv-virtio-blk [N [REPORT_FD]]`: the driver of virtio disk `N` (default
//! 0) that the kernel left to userspace (`udriver=virtio-blk@N` on its
//! command line).
//!
//! The disk goes to the kernel -- a new `/dev/vdX` -- and this process
//! serves it until it is killed, checking and writing nothing of its own.
//! It is what a supervisor runs (`eclipse-init`, the `ublk` service). When
//! it dies the kernel resets the device and keeps the disk: the node stays,
//! with whatever is mounted on it, and requests wait up to ten seconds for
//! the next driver, which gets the same disk; what was out with the dead
//! one is asked again.
//!
//! With `REPORT_FD`, a byte is written to that descriptor once the disk is
//! the kernel's, for a parent that waits for it.
//!
//! Exit status: 2 when the Zircon gateway is off, 1 on any other error and
//! when the kernel's end of the FIFO goes away.

use std::process::exit;

use udrivers::blk::{self, Disk, Msg};
use udrivers::claim::{self, Dma};
use udrivers::fifo::Fifo;
use udrivers::virtio::{self, Device, Setup};
use udrivers::zx::{self, nr, Status};

const NAME: &str = "udrv-virtio-blk";

fn die(what: impl std::fmt::Display) -> ! {
    println!("{NAME}: {what}");
    exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let index: usize = match args.get(1).map(|a| a.parse()) {
        None => 0,
        Some(Ok(index)) => index,
        Some(Err(_)) => die("usage: udrv-virtio-blk [N [REPORT_FD]]"),
    };
    let report: Option<i32> = args.get(2).and_then(|a| a.parse().ok());

    // The driver before this one may have died a moment ago: three seconds
    // of patience.
    let claim = match claim::claim(claim::KIND_VIRTIO_BLK, index, 300) {
        Ok(claim) => claim,
        Err(Status::NO_GATEWAY) => {
            println!("{NAME}: the gateway is off (zircon.gateway=0 on the command line?)");
            exit(2);
        }
        Err(e) => die(format_args!(
            "claim (boot with udriver=virtio-blk@N and a disk there): {e}"
        )),
    };
    let Some(transport) = claim.transport else {
        die("unknown transport");
    };
    let regs = claim.map_regs().unwrap_or_else(|e| die(e));
    // The data pages are also the kernel's window: what it reads and
    // writes there, the device does, with no copy on either side.
    let dma = Dma::new(
        claim.bti,
        blk::DMA_SIZE,
        Some((blk::DMA_DATA, blk::DATA_MAX)),
    )
    .unwrap_or_else(|e| die(e));
    let window = dma.window.expect("asked for above");

    // SAFETY: `regs` is the claimed device's window, mapped for good.
    let mut device = unsafe { Device::new(regs, transport) };
    let up = device
        .bring_up(&Setup {
            device_id: virtio::ID_BLOCK,
            want: blk::F_FLUSH,
            need: 0,
            queue_size: blk::QUEUE_SIZE,
            queues: &[(dma.daddr + blk::DMA_QUEUE as u64, claim.interrupt.is_some())],
        })
        .unwrap_or_else(|e| die(e));
    let capacity = device.config32(0) as u64 | (device.config32(4) as u64) << 32;
    device.driver_ok();
    let interrupt = if up.irq_refused {
        println!("{NAME}: the device refused MSI-X vector 0; polling");
        None
    } else {
        claim.interrupt
    };
    if capacity < 64 {
        die(format_args!("disk too small, sectors: {capacity}"));
    }

    let (fifo, kernels) = Fifo::<Msg>::create(blk::FIFO_DEPTH)
        .unwrap_or_else(|e| die(format_args!("fifo_create: {e}")));
    // SAFETY: no pointers: two handles, which go to the kernel, and a count.
    if let Err(e) = unsafe {
        zx::call(
            nr::ECLIPSE_BLOCK_SERVE,
            [
                kernels as usize,
                window as usize,
                capacity as usize,
                0,
                0,
                0,
            ],
        )
    } {
        die(format_args!("block_serve: {e}"));
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

    // SAFETY: `dma` is DMA_SIZE bytes the device sees at `daddr`, mapped for
    // good; only the device, and the kernel through the window, touch it.
    let mut disk = unsafe {
        Disk::new(
            device,
            dma.ptr,
            dma.daddr,
            interrupt,
            capacity,
            up.features & blk::F_FLUSH != 0,
        )
    };
    match blk::serve(&mut disk, &fifo) {
        Status::PEER_CLOSED => println!("{NAME}: the kernel's end of the FIFO went away"),
        status => println!("{NAME}: stopped serving: {status}"),
    }
    exit(1);
}
