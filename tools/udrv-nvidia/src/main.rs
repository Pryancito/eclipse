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
//! - `udrv-nvidia irq-check VVVV DDDD OFF=VAL...`: the interrupt path of
//!   the driver's GSP boot, on any PCI function that raises one when its
//!   first BAR is written (QEMU's e1000e: `d0=4 c8=4`).
mod drm;
mod gem;
mod gpu;
mod irq;
mod mapper;
mod platform;
mod ports;
mod syncobj;

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
        Some("irq-check") => irq_check(&args[1..]),
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
    let irq = gpu_irq(&gpu);
    let card = match drv.init(&gpu.dev, &Some(mapper), irq) {
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

/// The GPU's MSI, for the driver's GSP boot of the GPU that scans out the
/// console: the claim's interrupt, behind the scheme `zcore_drivers::net`
/// hands the driver, numbered as the kernel's scan numbers it (`irq + 32`).
/// The line the driver is given, `None` when the claim has none.
fn gpu_irq(gpu: &gpu::Gpu) -> Option<usize> {
    match (gpu.claim.interrupt, gpu.claim.irq) {
        (Some(interrupt), irq) if irq != u32::MAX => {
            irq::install(irq::ClaimIrq::new(
                irq as usize + 32,
                Box::new(move || udrivers::claim::interrupt_wait(interrupt)),
            ));
            Some(irq as usize)
        }
        _ => {
            println!("{NAME}: the claim has no interrupt; the GSP boot runs without MSIs");
            None
        }
    }
}

/// `udrv-nvidia irq-check VVVV DDDD OFF=VAL...`: the interrupt path the
/// driver's GSP boot uses, on any PCI function that can be made to raise
/// one by writing its first BAR (an e1000e: `d0=4 c8=4`, a link-status
/// cause unmasked, then set). The handler goes in through
/// `msi_register_and_unmask` as the driver's does, and has to run.
fn irq_check(args: &[String]) {
    fn usage<T>() -> T {
        fail("usage: udrv-nvidia irq-check VVVV DDDD OFF=VAL...".into())
    }
    let hex16 = |s: Option<&String>| {
        s.and_then(|s| u16::from_str_radix(s, 16).ok())
            .unwrap_or_else(usage)
    };
    let (vendor, device) = (hex16(args.first()), hex16(args.get(1)));
    let writes: Vec<(usize, u32)> = args[2..]
        .iter()
        .map(|w| {
            w.split_once('=')
                .and_then(|(o, v)| {
                    Some((
                        usize::from_str_radix(o, 16).ok()?,
                        u32::from_str_radix(v, 16).ok()?,
                    ))
                })
                .unwrap_or_else(usage)
        })
        .collect();
    let gpu = gpu::claim(gpu::Which::Pci { vendor, device }).unwrap_or_else(|e| fail(e));
    let irq = gpu_irq(&gpu).unwrap_or_else(|| fail("the claim has no interrupt".into()));
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = count.clone();
    let handler: zcore_drivers::scheme::IrqHandler = std::sync::Arc::new(move || {
        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    let vector = irq + 32;
    let registered = zcore_drivers::net::msi_register_and_unmask(vector, handler);
    println!("{NAME}: vector {vector} registered: {registered}");
    let bar = gpu
        .bars
        .first()
        .copied()
        .unwrap_or_else(|| fail("no BAR".into()));
    let at = gpu
        .mapper()
        .query_or_map(bar.phys, bar.len)
        .unwrap_or_else(|| fail("BAR not mapped".into()));
    let start = std::time::Instant::now();
    for (offset, value) in writes {
        // SAFETY: inside the first BAR, mapped above.
        unsafe { ((at + offset) as *mut u32).write_volatile(value) };
    }
    while count.load(std::sync::atomic::Ordering::SeqCst) == 0
        && start.elapsed() < std::time::Duration::from_secs(2)
    {
        std::thread::yield_now();
    }
    let n = count.load(std::sync::atomic::Ordering::SeqCst);
    println!(
        "{NAME}: handler ran {n} time(s), first after {} us",
        start.elapsed().as_micros()
    );
    zcore_drivers::net::msi_mask_and_unregister(vector);
    if !registered || n == 0 {
        fail("irq-check FAILED".into());
    }
    println!("{NAME}: irq-check ok");
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

/// `CREATE_DUMB` of a 64x64 buffer on `file`, then `DESTROY_DUMB` of it.
fn dumb_buffer(file: &std::fs::File) -> bool {
    use std::os::fd::AsRawFd;
    const CREATE_DUMB: u64 = 0xc020_64b2;
    const DESTROY_DUMB: u64 = 0xc004_64b4;
    // `struct drm_mode_create_dumb`: height, width, bpp, flags, then the
    // handle, pitch and size the kernel answers.
    let mut c = [64u32, 64, 32, 0, 0, 0, 0, 0];
    // SAFETY: `c` is the 32 bytes of the struct.
    if unsafe { libc::ioctl(file.as_raw_fd(), CREATE_DUMB as _, c.as_mut_ptr()) } != 0 {
        return false;
    }
    let mut handle = c[4];
    // SAFETY: `handle` is the struct, a u32.
    unsafe { libc::ioctl(file.as_raw_fd(), DESTROY_DUMB as _, &mut handle) == 0 }
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
    let file = open_rw(&node).unwrap_or_else(|| fail("the node does not open".into()));
    // The driver's own ioctl, with an array its struct points at.
    let pushes = [[3u64, 0], [4, 0], [5, 0]];
    let mut exec = drm::check::Exec {
        push_count: pushes.len() as u32,
        push_ptr: pushes.as_ptr() as u64,
        ..Default::default()
    };
    let asked = exec;
    let r = {
        use std::os::fd::AsRawFd;
        // SAFETY: `exec` and the pushes it points at outlive the call.
        unsafe { libc::ioctl(file.as_raw_fd(), drm::check::EXEC as _, &mut exec) }
    };
    ok(
        "EXEC reaches the card with its pushes and comes back",
        r == 0
            && exec
                == drm::check::Exec {
                    channel: 12,
                    ..asked
                },
    );
    // An EXEC that waits: the kernel asks the card which waits are the
    // CPU's, and sleeps on them before the call (with the nouveau uAPI on).
    let uapi =
        std::fs::read_to_string("/proc/cmdline").is_ok_and(|c| c.contains("nvidia.nouveau_uapi"));
    match syncobj_signaled(&file) {
        Some(handle) => {
            let waits = [[0u32, handle, 0, 0]];
            let mut e = drm::check::Exec {
                wait_count: 1,
                wait_ptr: waits.as_ptr() as u64,
                ..Default::default()
            };
            let r = {
                use std::os::fd::AsRawFd;
                // SAFETY: `e` and the waits it points at outlive the call.
                unsafe { libc::ioctl(file.as_raw_fd(), drm::check::EXEC as _, &mut e) }
            };
            let asked = card.waits_asked.load(std::sync::atomic::Ordering::Relaxed);
            if uapi {
                ok(
                    "the kernel asks the card for an EXEC's CPU waits, and waits",
                    r == 0 && asked == 1,
                );
            } else {
                println!(
                    "{NAME}: an EXEC's CPU waits: not asked without nvidia.nouveau_uapi ({asked})"
                );
            }
        }
        None => println!("{NAME}: no syncobj from the node: the EXEC waits are not tried"),
    }
    // A dumb buffer the kernel makes is told to the card, and its end too.
    let dumb = dumb_buffer(&file);
    ok(
        "a dumb buffer is imported by the card, and freed",
        dumb && card.imported.load(std::sync::atomic::Ordering::Relaxed) == 1
            && card.freed.load(std::sync::atomic::Ordering::Relaxed) == 1,
    );
    // What a call to the card costs, there and back, beside an ioctl the
    // kernel answers alone (`DRM_IOCTL_VERSION`, of the cached name).
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
    time("EXEC with 3 pushes", &|| {
        use std::os::fd::AsRawFd;
        let mut e = asked;
        // SAFETY: as above.
        unsafe { libc::ioctl(file.as_raw_fd(), drm::check::EXEC as _, &mut e) };
    });
    println!("{NAME}: drm-check: ok");
}

/// A syncobj of `file`'s, made signaled (`DRM_IOCTL_SYNCOBJ_CREATE`).
fn syncobj_signaled(file: &std::fs::File) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut create = [0u32, 1]; // handle, DRM_SYNCOBJ_CREATE_SIGNALED
                                // SAFETY: `create` is the ioctl's struct, alive for the call.
    let r = unsafe { libc::ioctl(file.as_raw_fd(), 0xc008_64bf_u32 as _, create.as_mut_ptr()) };
    (r == 0 && create[0] != 0).then_some(create[0])
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
    // The GEM objects the driver makes are in the kernel's table, and only
    // in memory that is its own.
    use zcore_drivers::scheme::gem_mmap::{self, DecRef, HandleSlice};
    let slice = gem_mmap::alloc_handle_slice();
    ok(
        "the kernel gives the driver a slice of GEM handles",
        slice != HandleSlice::exhausted(),
    );
    let (h, pid, size) = (slice.base(), 4242, 2 * 4096);
    gem_mmap::register(h, dma as u64, size, pid);
    ok(
        "a GEM object in its DMA memory is in the kernel's table",
        gem_mmap::lookup(h) == Some((dma as u64, size)) && gem_mmap::holds(h, pid),
    );
    gem_mmap::register(h + 1, dma as u64 + size, 4096, pid);
    ok(
        "...and one in memory that is not its own is refused",
        gem_mmap::lookup(h + 1).is_none(),
    );
    gem_mmap::register_device_memory(h + 2, first.phys as u64, 4096, pid);
    ok(
        "a GEM object in its BAR is device memory",
        gem_mmap::is_device_memory(h + 2),
    );
    ok(
        "its last reference closed, it leaves the table",
        gem_mmap::dec_ref(h, pid) == DecRef::Freed && gem_mmap::lookup(h).is_none(),
    );
    syncobj_check(dma as u64, size, ok);
    println!("{NAME}: check: ok");
}

/// The fences the driver attaches are in the kernel's table, and only in
/// memory that is its own (`size` bytes of DMA memory at `dma`). There is
/// no client here to make a syncobj, so the handle is one the table does
/// not have: the kernel takes the fence's memory and answers that.
fn syncobj_check(dma: u64, size: u64, ok: impl Fn(&str, bool)) {
    use zcore_drivers::scheme::syncobj::{self, wire, WaitOutcome};
    extern "C" {
        fn drivers_syncobj(
            op: u32,
            inp: *const u64,
            in_len: usize,
            out: *mut u64,
            out_len: usize,
        ) -> i64;
        fn drivers_timer_now_as_micros() -> u64;
    }
    let attach = |phys: u64| {
        let inp = [0, 1, phys, 0, 5, 3, 1];
        // SAFETY: seven words in, none out.
        unsafe {
            drivers_syncobj(
                wire::ATTACH,
                inp.as_ptr(),
                inp.len(),
                core::ptr::null_mut(),
                0,
            )
        }
    };
    ok(
        "a fence in its DMA memory is taken by the kernel's table",
        attach(dma + 64) == 0,
    );
    ok(
        "...and one in memory that is not its own is refused",
        attach(dma + size) == -(libc::EPERM as i64),
    );
    ok(
        "the kernel's table answers the driver's calls",
        syncobj::pending_hw_fences(0, 1).is_empty()
            && !syncobj::timeline_signal(0, 1)
            && syncobj::describe(&[0], None).contains("/-1"),
    );
    // SAFETY: the process's clock.
    let soon = unsafe { drivers_timer_now_as_micros() } + 1_000;
    let r = syncobj::wait(&[0], None, true, soon);
    let t = std::time::Instant::now();
    for _ in 0..100 {
        syncobj::wait(&[0], None, true, soon);
    }
    println!(
        "{NAME}: a wait on a handle the table lacks: {:.1} us",
        t.elapsed().as_secs_f64() * 1e6 / 100.0
    );
    ok("a wait on it is invalid", matches!(r, WaitOutcome::Invalid));
    let t = std::time::Instant::now();
    const N: u32 = 200;
    for _ in 0..N {
        syncobj::poll_pending();
    }
    println!(
        "{NAME}: poll_pending through the kernel: {:.1} us",
        t.elapsed().as_secs_f64() * 1e6 / N as f64
    );
}
