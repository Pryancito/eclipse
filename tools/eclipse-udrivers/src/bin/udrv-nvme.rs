//! `udrv-nvme [N [REPORT_FD]]`: the driver of NVMe controller `N` (default
//! 0), which the kernel leaves to userspace: it has no NVMe driver.
//!
//! The first namespace goes to the kernel -- a new `/dev/nvmeXn1` -- and
//! this process serves it until it is killed, as `udrv-virtio-blk` does a
//! virtio disk: same messages, same window, same kernel end
//! (`loader/src/linux/block_serve.rs`). It is what a supervisor runs
//! (`eclipse-init`, the `unvme` service). When it dies the kernel resets
//! the controller and keeps the disk: the node stays, with whatever is
//! mounted on it, and requests wait up to ten seconds for the next driver.
//!
//! With `REPORT_FD`, a byte is written to that descriptor once the disk is
//! the kernel's, for a parent that waits for it.
//!
//! Exit status: 2 when the Zircon gateway is off, 1 on any other error and
//! when the kernel's end of the FIFO goes away.

use std::process::exit;

use udrivers::blk::{self, Msg};
use udrivers::claim::{self, Dma};
use udrivers::fifo::Fifo;
use udrivers::nvme::{self, Nvme};
use udrivers::status;
use udrivers::zx::{self, nr, Status};

const NAME: &str = "udrv-nvme";

fn die(what: impl std::fmt::Display) -> ! {
    println!("{NAME}: {what}");
    exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let index: usize = match args.get(1).map(|a| a.parse()) {
        None => 0,
        Some(Ok(index)) => index,
        Some(Err(_)) => die("usage: udrv-nvme [N [REPORT_FD]]"),
    };
    let report: Option<i32> = args.get(2).and_then(|a| a.parse().ok());

    // The driver before this one may have died a moment ago: three seconds
    // of patience.
    let claim = match claim::claim(claim::KIND_NVME, index, 300) {
        Ok(claim) => claim,
        Err(Status::NO_GATEWAY) => {
            println!("{NAME}: the gateway is off (zircon.gateway=0 on the command line?)");
            exit(2);
        }
        Err(e) => die(format_args!(
            "claim (boot with udriver=nvme@N and a controller there): {e}"
        )),
    };
    if !claim.pci {
        die("the claim is not of a PCI function");
    }
    let regs = claim.map_regs().unwrap_or_else(|e| die(e));
    // The data pages are also the kernel's window: what it reads and
    // writes there, the controller does, with no copy on either side.
    let dma = Dma::new(
        claim.bti,
        nvme::DMA_SIZE,
        Some((nvme::DMA_DATA, blk::DATA_MAX)),
    )
    .unwrap_or_else(|e| die(e));
    let window = dma.window.expect("asked for above");

    // SAFETY: `regs` is the claimed BAR, mapped for good; `dma` is
    // DMA_SIZE zeroed bytes the controller sees at `daddr`, mapped for
    // good, touched only by the controller and, in the window, the kernel.
    let mut disk =
        unsafe { Nvme::new(regs, dma.ptr, dma.daddr, claim.interrupt) }.unwrap_or_else(|e| die(e));
    let capacity = disk_capacity(&disk);
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
    let place = format!("nvme@{index}");
    status::publish(
        &format!("driver.{place}"),
        &[
            ("program", NAME),
            ("device", &place),
            ("serves", "kernel"),
            ("sectors", &capacity.to_string()),
            ("max-transfer", &disk.max_xfer().to_string()),
        ],
    );
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

    match blk::serve(&mut disk, &fifo) {
        Status::PEER_CLOSED => println!("{NAME}: the kernel's end of the FIFO went away"),
        status => println!("{NAME}: stopped serving: {status}"),
    }
    exit(1);
}

fn disk_capacity(disk: &Nvme) -> u64 {
    use udrivers::blk::Backend;
    disk.capacity()
}
