//! `udrv-nvidia`: the kernel's NVIDIA driver (`zcore-drivers`' display
//! module and `nvidia-rm-sys` with NVIDIA's C), linked into a process.
//! What the driver needs from the system it gets through the symbols in
//! `platform` and `ports`; the GPU itself comes from the kernel
//! (`udriver=nvidia@N` leaves it to userspace), claimed in `gpu`.
//!
//! - `udrv-nvidia [N]`: claim NVIDIA GPU `N` (0), bring the driver up on
//!   it and keep it. Serving `/dev/dri` and `/dev/nvidia*` from it comes
//!   in a later step; until then the kernel's driver is the one a boot
//!   uses, and this is run by hand.
//! - `udrv-nvidia check VVVV DDDD`: check this process's side of it on
//!   any PCI function (`udriver=pci-VVVV-DDDD@0`), QEMU's standard VGA on
//!   a machine with no NVIDIA GPU: the probe through the ports, the BARs
//!   through the mapper, DMA memory through the claim. The driver itself
//!   is not run.
mod gpu;
mod mapper;
mod platform;
mod ports;

use std::process::exit;

use zcore_drivers::builder::IoMapper;
use zcore_drivers::bus::pci_drivers::PciDriver;

const NAME: &str = "udrv-nvidia";

fn fail(what: String) -> ! {
    eprintln!("{NAME}: {what}");
    exit(1)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("check") => {
            let hex = |i: usize| {
                args.get(i)
                    .and_then(|s| u16::from_str_radix(s, 16).ok())
                    .unwrap_or_else(|| fail("usage: udrv-nvidia check VVVV DDDD".into()))
            };
            check(hex(1), hex(2))
        }
        n => run(n.and_then(|n| n.parse().ok()).unwrap_or(0)),
    }
}

fn run(index: usize) {
    let gpu = gpu::claim(gpu::Which::Nvidia(index)).unwrap_or_else(|e| fail(e));
    gpu.install_dma().unwrap_or_else(|e| fail(e));
    let mapper: std::sync::Arc<dyn IoMapper> = gpu.mapper();
    let drv = zcore_drivers::display::NvidiaGpuDriverPci;
    println!(
        "{NAME}: {:04x}:{:04x} at {:02x}:{:02x}.{}",
        gpu.dev.id.vendor_id, gpu.dev.id.device_id, gpu.loc.bus, gpu.loc.device, gpu.loc.function
    );
    match drv.init(&gpu.dev, &Some(mapper), None) {
        Ok(_device) => println!("{NAME}: {} up", drv.name()),
        Err(e) => fail(format!("{}: {e:?}", drv.name())),
    }
    // Nothing serves it yet: hold the GPU, as its driver.
    loop {
        std::thread::park();
    }
}

fn check(vendor: u16, device: u16) {
    let ok = |what: &str, ok: bool| {
        println!("{NAME}: {what}: {}", if ok { "ok" } else { "FAILED" });
        if !ok {
            exit(1);
        }
    };
    let gpu = gpu::claim(gpu::Which::Pci { vendor, device }).unwrap_or_else(|e| fail(e));
    let dev = &gpu.dev;
    ok(
        "the probe through the ports finds the function",
        dev.id.vendor_id == vendor && dev.id.device_id == device,
    );
    let config = |offset| gpu.pci.config_read(offset, 2).ok();
    ok(
        "...with the command register the kernel reads",
        config(4).map(|c| c as u16) == Some(dev.command.bits()),
    );
    for bar in &gpu.bars {
        let probed = match dev.bars[bar.index] {
            Some(pci::BAR::Memory(addr, len, _, _)) => Some((addr as usize, len as usize)),
            _ => None,
        };
        println!(
            "{NAME}: bar {}: {:#x} {:#x} (probe {probed:x?})",
            bar.index, bar.phys, bar.len
        );
        // The probe's length comes out of the `pci` crate 2 short, in the
        // kernel as here.
        ok(
            "the probe sizes a BAR as the kernel did, without touching it",
            probed
                .is_some_and(|(addr, len)| addr == bar.phys && len.next_power_of_two() == bar.len),
        );
    }
    let mapper = gpu.mapper();
    let first = gpu
        .bars
        .first()
        .copied()
        .unwrap_or_else(|| fail("no BAR".into()));
    let at = mapper.query_or_map(first.phys + 0x1000, 0x1000);
    extern "C" {
        fn drivers_phys_to_virt(paddr: usize) -> usize;
    }
    // SAFETY: a lookup among the ranges registered.
    let found = unsafe { drivers_phys_to_virt(first.phys + 0x1000) };
    ok(
        "a BAR is mapped and found again by phys_to_virt",
        at.is_some() && at == Some(found),
    );
    // SAFETY: the BAR's second page, mapped above.
    let back = unsafe {
        let p = (found as *mut u32).add(3);
        p.write_volatile(0x5eed_cafe);
        p.read_volatile()
    };
    ok("its memory is reached", back == 0x5eed_cafe);
    gpu.install_dma().unwrap_or_else(|e| fail(e));
    extern "C" {
        fn drivers_dma_alloc(pages: usize) -> usize;
    }
    // SAFETY: the driver's own allocation call.
    let dma = unsafe { drivers_dma_alloc(2) };
    // SAFETY: as above.
    let virt = unsafe { drivers_phys_to_virt(dma) };
    ok(
        "DMA memory has a device address and a mapping",
        dma != 0 && virt != 0,
    );
    // SAFETY: two pages just allocated, this process's.
    unsafe { (virt as *mut u64).add(1023).write_volatile(7) };
    println!("{NAME}: check: ok");
}
