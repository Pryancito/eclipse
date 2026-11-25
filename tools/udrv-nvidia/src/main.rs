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
mod drm;
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
        Some("drm-check") => drm_check(),
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
    let card = match drv.init(&gpu.dev, &Some(mapper), None) {
        Ok(zcore_drivers::Device::DrmDisplay(card, _)) => card,
        Ok(_) => fail(format!("{}: not a DRM card", drv.name())),
        Err(e) => fail(format!("{}: {e:?}", drv.name())),
    };
    println!("{NAME}: {} up, serving it", drv.name());
    match drm::serve(card) {
        Ok(why) => fail(format!("the kernel let go of the card: {why}")),
        Err(e) => fail(e),
    }
}

/// `DRM_IOCTL_MODE_GETRESOURCES`, `_IOWR('d', 0xA0, struct drm_mode_card_res)`.
const GETRESOURCES: u64 = 0xc040_64a0;

#[repr(C)]
#[derive(Default)]
struct CardRes {
    fbs: u64,
    crtcs: u64,
    connectors: u64,
    encoders: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

/// The CRTCs and connectors the node at `path` lists.
fn resources(path: &str) -> Option<(Vec<u32>, Vec<u32>)> {
    resources_of(&open_rw(path)?)
}

fn open_rw(path: &str) -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .ok()
}

/// `DRM_IOCTL_VERSION` with no buffers: the lengths only.
fn version_of(file: &std::fs::File) {
    use std::os::fd::AsRawFd;
    const VERSION: u64 = 0xc040_6400;
    let mut v = [0u64; 8];
    // SAFETY: `v` is the 64 bytes of `struct drm_version`, with every
    // buffer pointer null and every length 0.
    unsafe { libc::ioctl(file.as_raw_fd(), VERSION as _, v.as_mut_ptr()) };
}

/// What `GETRESOURCES` on `file` lists.
fn resources_of(file: &std::fs::File) -> Option<(Vec<u32>, Vec<u32>)> {
    use std::os::fd::AsRawFd;
    let mut crtcs = vec![0u32; 16];
    let mut connectors = vec![0u32; 16];
    let mut res = CardRes {
        crtcs: crtcs.as_mut_ptr() as u64,
        connectors: connectors.as_mut_ptr() as u64,
        count_crtcs: 16,
        count_connectors: 16,
        ..CardRes::default()
    };
    // SAFETY: `res` and the two arrays it points to outlive the call.
    if unsafe { libc::ioctl(file.as_raw_fd(), GETRESOURCES as _, &mut res) } != 0 {
        return None;
    }
    crtcs.truncate(res.count_crtcs.min(16) as usize);
    connectors.truncate(res.count_connectors.min(16) as usize);
    Some((crtcs, connectors))
}

/// `udrv-nvidia drm-check`: serve a card with nothing behind it
/// (`drm::check::Card`) and check, through its node, that the kernel's
/// DRM core asks it: the card the driver of a GPU is served as, without
/// the GPU.
fn drm_check() {
    use drm::check::{Card, CONNECTOR, CRTC};
    let ok = |what: &str, ok: bool| {
        println!("{NAME}: {what}: {}", if ok { "ok" } else { "FAILED" });
        if !ok {
            exit(1);
        }
    };
    let card = std::sync::Arc::new(Card::default());
    let served = card.clone();
    std::thread::spawn(move || match drm::serve(served) {
        Ok(why) => fail(format!("the kernel let go of the card: {why}")),
        Err(e) => fail(e),
    });
    let mut node = None;
    for _ in 0..200 {
        node = (0..8)
            .map(|n| format!("/dev/dri/card{n}"))
            .find(|path| resources(path).is_some_and(|(crtcs, _)| crtcs.contains(&CRTC)));
        if node.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    println!("{NAME}: node {node:?}");
    let node = node.unwrap_or_else(|| fail("no node lists the card's CRTC".into()));
    ok("the card is served as a node of /dev/dri", true);
    ok(
        "its node lists its own CRTC and connector",
        resources(&node) == Some((vec![CRTC], vec![CONNECTOR])),
    );
    // What a call to the card costs, there and back, beside an ioctl the
    // kernel answers alone (`DRM_IOCTL_VERSION`, of the cached name).
    let file = open_rw(&node).unwrap_or_else(|| fail("the node does not open".into()));
    let n = 2000;
    let time = |what: &str, f: &dyn Fn()| {
        let answered = || drm::ANSWERED.load(std::sync::atomic::Ordering::Relaxed);
        let (t, calls) = (std::time::Instant::now(), answered());
        for _ in 0..n {
            f();
        }
        let us = t.elapsed().as_secs_f64() * 1e6 / n as f64;
        let calls = (answered() - calls) as f64 / n as f64;
        println!("{NAME}: {what}: {us:.1} us, {calls:.1} calls to the card");
    };
    time("GETRESOURCES through the served card", &|| {
        let _ = resources_of(&file);
    });
    time("VERSION, the kernel alone", &|| version_of(&file));
    println!("{NAME}: drm-check: ok");
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
