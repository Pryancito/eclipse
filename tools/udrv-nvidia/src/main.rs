//! `udrv-nvidia`: the kernel's NVIDIA driver (`zcore-drivers`' display
//! module and `nvidia-rm-sys` with NVIDIA's C), linked into a process.
//! What the driver needs from the system it gets through the symbols in
//! `platform` and `ports`; the GPU itself comes from the kernel
//! (`udriver=nvidia@N` leaves it to userspace), claimed in `gpu`.
//!
//! - `udrv-nvidia [N]`: claim every NVIDIA GPU left to userspace (only
//!   GPU `N` with `N`), bring the driver up on each and serve them: a
//!   card per GPU in `/dev/dri`, and `/dev/nvidia*` for all of them, from
//!   one RM. Until the kernel's driver goes, it is the one a boot uses,
//!   and this is run by hand.
//! - `udrv-nvidia check VVVV DDDD`: check this process's side of it on
//!   any PCI function (`udriver=pci-VVVV-DDDD@0`), QEMU's standard VGA on
//!   a machine with no NVIDIA GPU: the probe through the ports, the BARs
//!   through the mapper, what the boot knows of the display, DMA memory
//!   through the claim. The driver itself is not run.
//! - `udrv-nvidia irq-check VVVV DDDD OFF=VAL...`: the interrupt path of
//!   the driver's GSP boot, on any PCI function that raises one when its
//!   first BAR is written (QEMU's e1000e: `d0=4 c8=4`).
//! - `udrv-nvidia nvctl-check VVVV DDDD`: `/dev/nvidiactl` served with an
//!   RM that only checks the way there (`nvidia.nvidiactl` on the kernel's
//!   cmdline, and a PCI function claimed as for `check`, for its BAR and
//!   DMA memory): an escape's parameters reached through its pointer, the
//!   GPU node, mappings of its memory and its BAR, pages of the caller's
//!   lent to it and given back, an OS event posted from its nonstall
//!   interrupt waking a `poll`, a UVM semaphore pool lent to it while it
//!   has memory of it (`nvidia.uvm` too), the close.
mod drm;
mod gem;
mod gpu;
mod irq;
mod mapper;
mod nvctl;
mod platform;
mod ports;
mod syncobj;

use std::process::exit;

use zcore_drivers::builder::IoMapper;
use zcore_drivers::bus::pci_drivers::PciDriver;
use zcore_drivers::display::boot_display::{self, BootDisplay};
use zcore_drivers::display::driver_flags::{self, DriverFlags};

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
        Some("nvctl-check") => {
            let hex = |i: usize| {
                args.get(i)
                    .and_then(|s| u16::from_str_radix(s, 16).ok())
                    .unwrap_or_else(|| fail("usage: udrv-nvidia nvctl-check VVVV DDDD".into()))
            };
            nvctl_check(hex(1), hex(2))
        }
        n => run(n.and_then(|n| n.parse().ok())),
    }
}

/// Claim every NVIDIA GPU left to userspace (`udriver=nvidia@0,nvidia@1`,
/// ...), or only GPU `only`, bring the driver up on each and serve them:
/// one RM for all of them, as the kernel's driver is, so a buffer of one
/// is any GPU's (P2P, UVM across GPUs), and a card per GPU.
fn run(only: Option<usize>) {
    let gpus = match only {
        Some(index) => vec![gpu::claim(gpu::Which::Nvidia(index)).unwrap_or_else(|e| fail(e))],
        None => gpu::claim_all(gpu::Which::Nvidia).unwrap_or_else(|e| fail(e)),
    };
    // Every GPU is claimed before any memory is made: a buffer reaches
    // the GPUs whose initiators it is pinned in, which are those there are
    // when it is made.
    gpu::install_dma(&gpus).unwrap_or_else(|e| fail(e));
    // The boot line's `nvidia.*` switches, as the kernel's driver was
    // told them, before any GPU comes up (the RM's registry among them).
    match driver_flags() {
        Ok(flags) => {
            println!(
                "{NAME}: nvidia.* switches {:#x}, registry {:?}",
                flags.bits, flags.registry
            );
            flags.install();
        }
        Err(e) => println!("{NAME}: the boot line's nvidia.* switches are unknown ({e}); defaults"),
    }
    // What the kernel's driver is told at boot, before any GPU comes up:
    // which of them scans out the console decides how it is brought up.
    match boot_display() {
        Ok(display) => {
            println!("{NAME}: {}", describe(&display));
            display.install();
        }
        Err(e) => {
            println!("{NAME}: the boot's display is unknown ({e}); no GPU drives the console")
        }
    }
    let drv = zcore_drivers::display::NvidiaGpuDriverPci;
    let mut cards = Vec::new();
    for gpu in &gpus {
        let mapper: std::sync::Arc<dyn IoMapper> = gpu.mapper();
        println!(
            "{NAME}: {:04x}:{:04x} at {:02x}:{:02x}.{}",
            gpu.dev.id.vendor_id,
            gpu.dev.id.device_id,
            gpu.loc.bus,
            gpu.loc.device,
            gpu.loc.function
        );
        let irq = gpu_irq(gpu);
        match drv.init(&gpu.dev, &Some(mapper), irq) {
            Ok(zcore_drivers::Device::DrmDisplay(card, _)) => cards.push(card),
            Ok(_) => fail(format!("{}: not a DRM card", drv.name())),
            Err(e) => fail(format!("{}: {e:?}", drv.name())),
        }
    }
    // What the kernel does for its own GPUs at boot, before anything else
    // touches the RM: the GSP firmware to every GPU, and the RM up on each
    // that does not scan out the console (the console one stays cold unless
    // `nvidia.console_gpu`). Before the RM says what it has: a GPU whose RM
    // never came up has a node no client can open.
    gsp_firmware(&cards);
    if cmdline_has("nvidia.noautoboot") {
        println!("{NAME}: nvidia.noautoboot: no GPU's RM is brought up");
    } else if !bringup_guard::enter() {
        eprintln!(
            "{NAME}: the last bring-up of the RM killed this process; \
             serving the cards without it this time"
        );
    } else {
        for card in &cards {
            let line = card.auto_bringup_compute();
            if !line.is_empty() {
                println!("{NAME}: {line}");
            }
        }
        bringup_guard::leave();
    }
    println!(
        "{NAME}: {} up on {} GPU(s), serving them",
        drv.name(),
        cards.len()
    );
    // `/dev/nvidiactl`, on its own thread: its escapes wait in the RM, not
    // behind the cards' calls. One for every GPU: the RM's.
    std::thread::spawn(|| {
        let rm = zcore_drivers::display::nvctl::remote::ThisRm;
        let hello = zcore_drivers::display::nvctl::remote::hello();
        match nvctl::serve(&rm, &hello) {
            Ok(why) => eprintln!("{NAME}: the kernel let go of the RM: {why}"),
            Err((udrivers::zx::Status::NOT_SUPPORTED, _)) => {
                println!("{NAME}: no /dev/nvidiactl (nvidia.nvidiactl is off)")
            }
            Err((_, e)) => eprintln!("{NAME}: {e}"),
        }
    });
    // A card per GPU, each on its own thread, served in the order the
    // kernel found them, so the first GPU's is the first card.
    let mut served = Vec::new();
    for card in cards {
        served.push(std::thread::spawn(move || match drm::serve(card) {
            Ok(why) => fail(format!("the kernel let go of a card: {why}")),
            Err(e) => fail(e),
        }));
    }
    for card in served {
        let _ = card.join();
    }
}

/// The GSP-RM firmware, where the image has it and the kernel reads it
/// from for its own GPUs.
const GSP_FIRMWARE: &str = "/lib/firmware/nvidia/gsp/gsp.bin";

/// Hand every GPU the GSP firmware, or why there is none: without it no
/// RM comes up.
fn gsp_firmware(cards: &[std::sync::Arc<dyn zcore_drivers::scheme::DrmScheme>]) {
    match std::fs::read(GSP_FIRMWARE) {
        Ok(bytes) if !bytes.is_empty() => {
            println!("{NAME}: GSP-RM firmware, {} bytes", bytes.len());
            let status = format!("OK: {} bytes delivered by {NAME}", bytes.len());
            for card in cards {
                card.set_gsp_firmware(bytes.clone());
                card.set_gsp_firmware_status(status.clone());
            }
        }
        Ok(_) => gsp_firmware_missing(cards, format!("{GSP_FIRMWARE} is 0 bytes")),
        Err(e) => gsp_firmware_missing(cards, format!("{GSP_FIRMWARE}: {e}")),
    }
}

fn gsp_firmware_missing(
    cards: &[std::sync::Arc<dyn zcore_drivers::scheme::DrmScheme>],
    why: String,
) {
    println!("{NAME}: no GSP-RM firmware ({why}): no GPU's RM comes up");
    for card in cards {
        card.set_gsp_firmware_status(why.clone());
    }
}

/// Whether the boot line has `flag`, as the kernel reads its flags: a word
/// of its own, words split by blanks and `:`.
fn cmdline_has(flag: &str) -> bool {
    std::fs::read_to_string("/proc/cmdline")
        .map(|line| has_flag(&line, flag))
        .unwrap_or(false)
}

fn has_flag(cmdline: &str, flag: &str) -> bool {
    cmdline
        .split([':', ' ', '\t', '\n'])
        .any(|word| word == flag)
}

/// What the boot knows of the display, as the kernel's driver was told.
fn boot_display() -> Result<BootDisplay, udrivers::zx::Status> {
    let mut bytes = [0u8; boot_display::LEN];
    udrivers::claim::boot_display(&mut bytes)?;
    Ok(BootDisplay::decode(&bytes))
}

/// The boot line's `nvidia.*` switches, as the kernel's driver was told.
fn driver_flags() -> Result<DriverFlags, udrivers::zx::Status> {
    let mut bytes = vec![0u8; driver_flags::MAX];
    let n = udrivers::claim::driver_flags(&mut bytes)?;
    DriverFlags::decode(&bytes[..n]).ok_or(udrivers::zx::Status::IO)
}

fn describe(display: &BootDisplay) -> String {
    let fb = match display.fb {
        Some((phys, w, h, pitch)) => {
            format!("boot framebuffer {w}x{h} at {phys:#x} (pitch {pitch})")
        }
        None => "no boot framebuffer".into(),
    };
    let edid = match display.edid {
        Some((_, len)) => format!("an EDID of {len} bytes"),
        None => "no EDID".into(),
    };
    let asked = match display.requested {
        Some((w, h)) => format!("{w}x{h} asked for"),
        None => "no resolution asked for".into(),
    };
    format!("{fb}, {edid}, {asked}")
}

/// The GPU's MSI, for the driver's GSP boot of the GPU that scans out the
/// console: the claim's interrupt, behind the scheme `zcore_drivers::net`
/// hands the driver, numbered as the kernel's scan numbers it (`irq + 32`).
/// The line the driver is given, `None` when the claim has none.
fn gpu_irq(gpu: &gpu::Gpu) -> Option<usize> {
    match (gpu.claim.interrupt, gpu.claim.irq) {
        (Some(interrupt), irq) if irq != u32::MAX => {
            let installed = irq::install(irq::ClaimIrq::new(
                irq as usize + 32,
                Box::new(move || udrivers::claim::interrupt_wait(interrupt)),
            ));
            if !installed {
                println!("{NAME}: irq {irq} is another GPU's already; this one's GSP boot runs without MSIs");
                return None;
            }
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
    let gpu = gpu::claim(gpu::Which::Pci {
        vendor,
        device,
        index: 0,
    })
    .unwrap_or_else(|e| fail(e));
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

/// `udrv-nvidia nvctl-check`: see the top of this file.
fn nvctl_check(vendor: u16, device: u16) {
    use std::os::fd::AsRawFd;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    let ok = |what: &str, ok: bool| {
        println!("{NAME}: {what}: {}", if ok { "ok" } else { "FAILED" });
        if !ok {
            exit(1);
        }
    };
    static RM: std::sync::OnceLock<nvctl::check::CheckRm> = std::sync::OnceLock::new();
    let rm = RM.get_or_init(Default::default);
    let gpu = gpu::claim(gpu::Which::Pci {
        vendor,
        device,
        index: 0,
    })
    .unwrap_or_else(|e| fail(e));
    gpu::install_dma(std::slice::from_ref(&gpu)).unwrap_or_else(|e| fail(e));
    let mapper = gpu.mapper();
    let bar = gpu
        .bars
        .first()
        .copied()
        .unwrap_or_else(|| fail("no BAR".into()));
    extern "C" {
        fn drivers_dma_alloc(pages: usize) -> usize;
        fn drivers_dma_dealloc(paddr: usize, pages: usize) -> i32;
        fn drivers_phys_to_virt(paddr: usize) -> usize;
    }
    // SAFETY: the driver's own allocation and lookup calls.
    let system = unsafe { drivers_dma_alloc(1) };
    let system_virt = unsafe { drivers_phys_to_virt(system) };
    // SAFETY: the page just allocated, this process's.
    unsafe { (system_virt as *mut u64).write_volatile(0x5eed_0000_0000_0001) };
    rm.system.store(system as u64, Ordering::Relaxed);
    rm.bar.store(bar.phys as u64, Ordering::Relaxed);
    std::thread::spawn(move || match nvctl::serve(rm, &nvctl::check::hello()) {
        Ok(why) => eprintln!("{NAME}: the kernel let go of the RM: {why}"),
        Err((_, e)) => fail(e),
    });
    let start = Instant::now();
    while std::fs::metadata("/dev/nvidia0").is_err() && start.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(5));
    }
    ok(
        "the served RM's GPU has its node",
        std::fs::metadata("/dev/nvidia0").is_ok(),
    );
    let ctl = open_rw("/dev/nvidiactl").unwrap_or_else(|| fail("open /dev/nvidiactl".into()));
    // `_IOWR('F', NV_ESC_RM_CONTROL, NVOS54_PARAMETERS)`.
    const CONTROL: u64 = 0xc020_462a;
    let mut params = [10u32, 20, 30, 40];
    let mut control = [0u8; nvctl::check::NVOS54_LEN];
    control[16..24].copy_from_slice(&(params.as_mut_ptr() as u64).to_le_bytes());
    control[24..28].copy_from_slice(&16u32.to_le_bytes());
    let escape = |control: &mut [u8]| {
        // SAFETY: `control` is the 32 bytes of NVOS54_PARAMETERS, whose
        // pointer is to `params`, alive for the call.
        unsafe { libc::ioctl(ctl.as_raw_fd(), CONTROL as _, control.as_mut_ptr()) }
    };
    let ret = escape(&mut control);
    ok(
        "an RM_CONTROL's parameters are read and written where it points",
        ret == 0 && params == [11, 21, 31, 41] && control[28..32] == [0; 4],
    );
    const N: u32 = 100;
    let t = Instant::now();
    for _ in 0..N {
        escape(&mut control);
    }
    println!(
        "{NAME}: RM_CONTROL through the served RM: {} us",
        t.elapsed().as_micros() / N as u128
    );
    ok("...each of them", params == [111, 121, 131, 141]);
    // A pointer the caller does not have: the RM is told so.
    control[16..24].copy_from_slice(&8u64.to_le_bytes());
    ok("a pointer to nothing is refused", escape(&mut control) != 0);
    ok("the GPU node opens", open_rw("/dev/nvidia0").is_some());

    // RM_MAP_MEMORY, then mmap of the open the mapping is recorded on.
    const MAP_MEMORY: u64 = 0xc038_464e;
    let map = |h_memory: u32, on: &std::fs::File| {
        let mut p = [0u8; 56];
        p[8..12].copy_from_slice(&h_memory.to_le_bytes());
        p[48..52].copy_from_slice(&on.as_raw_fd().to_le_bytes());
        // SAFETY: `p` is the 56 bytes of NVOS33_PARAMETERS.
        unsafe { libc::ioctl(ctl.as_raw_fd(), MAP_MEMORY as _, p.as_mut_ptr()) }
    };
    let mmap = |on: &std::fs::File| {
        // SAFETY: a new shared mapping of one page of `on`.
        let at = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                on.as_raw_fd(),
                0,
            )
        };
        (at != libc::MAP_FAILED).then_some(at as *mut u64)
    };
    let on_ctl = open_rw("/dev/nvidiactl").unwrap_or_else(|| fail("open /dev/nvidiactl".into()));
    ok(
        "the RM's system memory is granted",
        map(nvctl::check::grant::SYSTEM, &on_ctl) == 0,
    );
    let seen = mmap(&on_ctl).unwrap_or_else(|| fail("mmap of its system memory".into()));
    // SAFETY: the page mapped above.
    let word = unsafe { seen.read_volatile() };
    ok(
        "...and mapped, the same memory",
        word == 0x5eed_0000_0000_0001,
    );
    // The driver frees it: the mapping keeps its pages.
    // SAFETY: the page it allocated, freed once.
    unsafe { drivers_dma_dealloc(system, 1) };
    unsafe { seen.add(1).write_volatile(42) };
    ok(
        "a mapping outlives the driver's free of its memory",
        unsafe {
            seen.read_volatile() == 0x5eed_0000_0000_0001 && seen.add(1).read_volatile() == 42
        },
    );
    let on_gpu = open_rw("/dev/nvidia0").unwrap_or_else(|| fail("open /dev/nvidia0".into()));
    ok(
        "an aperture in its BAR is granted",
        map(nvctl::check::grant::APERTURE, &on_gpu) == 0,
    );
    let window = mmap(&on_gpu).unwrap_or_else(|| fail("mmap of its aperture".into()));
    let driver_side = mapper
        .query_or_map(bar.phys, 4096)
        .unwrap_or_else(|| fail("BAR not mapped".into())) as *mut u64;
    // SAFETY: the same page of the BAR, mapped twice.
    unsafe { window.add(3).write_volatile(0xa9e7_7e50) };
    ok(
        "...and mapped, the BAR itself",
        unsafe { driver_side.add(3).read_volatile() } == 0xa9e7_7e50,
    );
    let refused = open_rw("/dev/nvidiactl").unwrap_or_else(|| fail("open /dev/nvidiactl".into()));
    ok(
        "memory the RM's process never pinned is refused",
        map(nvctl::check::grant::NOT_OURS, &refused) != 0 && mmap(&refused).is_none(),
    );
    ok(
        "...and the RM told so",
        rm.unrecorded.load(Ordering::Relaxed) != 0,
    );
    lent_pages_check(rm, &map, &ok);
    os_event_check(rm, &ok);
    uvm_check(rm, &ctl, &ok);
    drop(ctl);
    let start = Instant::now();
    while rm.released.load(Ordering::Relaxed) == 0 && start.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(5));
    }
    ok(
        "a close reaches the RM",
        rm.released.load(Ordering::Relaxed) != 0,
    );
    println!(
        "{NAME}: nvctl-check ok ({} escapes)",
        rm.escapes.load(Ordering::Relaxed)
    );
}

/// `nvctl-check`'s OS descriptors: an `RM_ALLOC_MEMORY` of the caller's
/// own pages lends them to the RM, which the process's devices are granted
/// (so the kernel takes a mapping of them as the process's), until
/// `RM_FREE` gives them back; refused, they come back at once.
fn lent_pages_check(
    rm: &nvctl::check::CheckRm,
    map: &dyn Fn(u32, &std::fs::File) -> i32,
    ok: &dyn Fn(&str, bool),
) {
    use std::os::fd::AsRawFd;
    use std::sync::atomic::Ordering;
    use udrivers::served::Node;
    const PAGES: usize = 3;
    // SAFETY: a new private mapping of this process's.
    let buf = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            PAGES * 4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if buf == libc::MAP_FAILED {
        fail("mmap of the client's buffer".into());
    }
    let gpu = open_rw("/dev/nvidia0").unwrap_or_else(|| fail("open /dev/nvidia0".into()));
    // `_IOWR('F', NV_ESC_RM_ALLOC_MEMORY, NVOS02_PARAMETERS)` and
    // `_IOWR('F', NV_ESC_RM_FREE, NVOS00_PARAMETERS)`.
    const ALLOC_MEMORY: u64 = 0xc038_4627;
    const FREE: u64 = 0xc010_4629;
    let alloc = |h: u32| {
        let mut p = [0u8; 56];
        p[8..12].copy_from_slice(&h.to_le_bytes());
        // NV01_MEMORY_SYSTEM_OS_DESCRIPTOR, its address and its limit.
        p[12..16].copy_from_slice(&0x71u32.to_le_bytes());
        p[24..32].copy_from_slice(&(buf as u64).to_le_bytes());
        p[32..40].copy_from_slice(&((PAGES * 4096 - 1) as u64).to_le_bytes());
        // SAFETY: `p` is the 56 bytes of NVOS02_PARAMETERS.
        let ret = unsafe { libc::ioctl(gpu.as_raw_fd(), ALLOC_MEMORY as _, p.as_mut_ptr()) };
        (ret, u32::from_le_bytes(p[40..44].try_into().unwrap()))
    };
    let free = |h: u32| {
        let mut p = [0u8; 16];
        p[8..12].copy_from_slice(&h.to_le_bytes());
        // SAFETY: `p` is the 16 bytes of NVOS00_PARAMETERS.
        let ret = unsafe { libc::ioctl(gpu.as_raw_fd(), FREE as _, p.as_mut_ptr()) };
        (ret, u32::from_le_bytes(p[12..16].try_into().unwrap()))
    };
    let on = open_rw("/dev/nvidiactl").unwrap_or_else(|| fail("open /dev/nvidiactl".into()));
    let before = nvctl::GIVEN_BACK.load(Ordering::Relaxed);
    ok(
        "an OS descriptor's pages are lent to the RM",
        alloc(0x700) == (0, 0),
    );
    let first = rm.lent.load(Ordering::Relaxed);
    ok(
        "...all of them, by their physical addresses",
        rm.lent_pages.load(Ordering::Relaxed) == PAGES && first != 0 && first.is_multiple_of(4096),
    );
    ok(
        "...kept while the RM holds them",
        nvctl::GIVEN_BACK.load(Ordering::Relaxed) == before,
    );
    ok(
        "...granted to its devices (a mapping of them is its process's)",
        map(nvctl::check::grant::LENT, &on) == 0,
    );
    ok("...and freed", free(0x700) == (0, 0));
    let given = nvctl::GIVEN_BACK.load(Ordering::Relaxed);
    ok("...given back to the kernel", given != before);
    ok(
        "...and no longer its devices'",
        map(nvctl::check::grant::LENT, &on) != 0,
    );
    ok(
        "a loan goes back once",
        Node::give_back_nvctl(given).is_err(),
    );
    let (ret, status) = alloc(nvctl::check::REFUSED);
    ok(
        "pages the RM does not take come back at once",
        ret == 0 && status != 0 && nvctl::GIVEN_BACK.load(Ordering::Relaxed) > given,
    );
    ok(
        "...and the kernel took every one",
        nvctl::GIVE_BACK_REFUSED.load(Ordering::Relaxed) == 0,
    );
    // SAFETY: the mapping made above, no longer used.
    unsafe { libc::munmap(buf, PAGES * 4096) };
}

/// `nvctl-check`'s OS events: an open registered with `ALLOC_OS_EVENT` is
/// polled, which has the kernel ask the RM to look at its nonstall
/// interrupts; the RM posts what it finds, and the poll wakes with it on
/// the queue (`RM_GET_EVENT_DATA`).
fn os_event_check(rm: &nvctl::check::CheckRm, ok: &dyn Fn(&str, bool)) {
    use std::os::fd::AsRawFd;
    use std::sync::atomic::Ordering;
    use std::time::Instant;
    // `_IOWR('F', NV_ESC_ALLOC_OS_EVENT, 16)`, `..._FREE_OS_EVENT` and
    // `NV_ESC_RM_GET_EVENT_DATA`.
    const ALLOC_OS_EVENT: u64 = 0xc010_46ce;
    const FREE_OS_EVENT: u64 = 0xc010_46cf;
    const GET_EVENT_DATA: u64 = 0xc010_4652;
    const CLIENT: u32 = 0xc1d0_0001;
    let ev = open_rw("/dev/nvidia0").unwrap_or_else(|| fail("open /dev/nvidia0".into()));
    let fd = ev.as_raw_fd();
    let ioctl = |nr: u64, p: &mut [u8; 16]| {
        // SAFETY: `p` is the 16 bytes of each of the three payloads.
        unsafe { libc::ioctl(fd, nr as _, p.as_mut_ptr()) }
    };
    let status = |p: &[u8; 16]| u32::from_le_bytes(p[12..16].try_into().unwrap());
    let mut p = [0u8; 16];
    p[0..4].copy_from_slice(&CLIENT.to_le_bytes());
    p[8..12].copy_from_slice(&(fd as u32).to_le_bytes());
    ok(
        "an OS event is registered with the RM",
        ioctl(ALLOC_OS_EVENT, &mut p) == 0 && status(&p) == 0,
    );
    let posted = nvctl::POSTED.load(Ordering::Relaxed);
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let t = Instant::now();
    // SAFETY: one pollfd, this process's.
    let ready = unsafe { libc::poll(&mut poll, 1, 5000) };
    let took = t.elapsed();
    ok(
        "a poll on it wakes with what the RM's nonstall interrupt posted",
        ready == 1 && poll.revents & libc::POLLIN != 0,
    );
    println!(
        "{NAME}: OS event through the served RM: {} us from poll to wake, {} looks",
        took.as_micros(),
        rm.looks.load(Ordering::Relaxed)
    );
    ok(
        "...posted to the kernel by the RM's process",
        nvctl::POSTED.load(Ordering::Relaxed) == posted + 1
            && nvctl::POST_REFUSED.load(Ordering::Relaxed) == 0,
    );
    let mut event = [0u8; 16];
    let mut get = [0u8; 16];
    get[0..8].copy_from_slice(&(event.as_mut_ptr() as u64).to_le_bytes());
    ok(
        "...and read back as the RM said it",
        ioctl(GET_EVENT_DATA, &mut get) == 0 && status(&get) == 0 && {
            let (h, index, info32, info16) = nvctl::check::EVENT;
            let word = |at: usize| u32::from_le_bytes(event[at..at + 4].try_into().unwrap());
            word(0) == h
                && word(4) == index
                && word(8) == info32
                && u16::from_le_bytes([event[12], event[13]]) == info16
        },
    );
    ok(
        "...once",
        ioctl(GET_EVENT_DATA, &mut get) == 0 && status(&get) != 0,
    );
    let mut free = [0u8; 16];
    free[0..4].copy_from_slice(&CLIENT.to_le_bytes());
    free[8..12].copy_from_slice(&(fd as u32).to_le_bytes());
    ok(
        "...and freed in the RM",
        ioctl(FREE_OS_EVENT, &mut free) == 0 && status(&free) == 0,
    );
    ok(
        "...once",
        ioctl(FREE_OS_EVENT, &mut free) == 0 && status(&free) != 0,
    );
}

/// `nvctl-check`'s UVM: a VA space with the served RM's GPU and a GPU VA
/// space on it, then a semaphore pool, whose pages are lent to the RM
/// for the memory it makes of them and given back when the pool is freed.
fn uvm_check(rm: &nvctl::check::CheckRm, ctl: &std::fs::File, ok: &dyn Fn(&str, bool)) {
    use std::os::fd::AsRawFd;
    use std::sync::atomic::Ordering;
    let Some(uvm) = open_rw("/dev/nvidia-uvm") else {
        println!("{NAME}: no /dev/nvidia-uvm (nvidia.uvm is off): UVM not checked");
        return;
    };
    let call = |cmd: u64, p: &mut [u8], status_at: usize| {
        // SAFETY: `p` is the whole of the command's params.
        let ret = unsafe { libc::ioctl(uvm.as_raw_fd(), cmd as _, p.as_mut_ptr()) };
        ret == 0 && p[status_at..status_at + 4] == [0; 4]
    };
    let uuid = [0x5au8; 16];
    ok("UVM initializes", call(0x3000_0001, &mut [0u8; 16], 8));
    let mut gpu = [0u8; 40];
    gpu[..16].copy_from_slice(&uuid);
    ok("...with the served RM's GPU", call(37, &mut gpu, 36));
    let mut vas = [0u8; 32];
    vas[..16].copy_from_slice(&uuid);
    vas[16..20].copy_from_slice(&ctl.as_raw_fd().to_le_bytes());
    vas[20..24].copy_from_slice(&0xc1d0_0002u32.to_le_bytes());
    vas[24..28].copy_from_slice(&0x5c00_0007u32.to_le_bytes());
    ok("...and a GPU VA space on it", call(25, &mut vas, 28));
    let given = nvctl::GIVEN_BACK.load(Ordering::Relaxed);
    const BASE: u64 = 0x7e00_0000_0000;
    const LEN: u64 = 2 * 4096;
    let mut pool = vec![0u8; 9248];
    pool[0..8].copy_from_slice(&BASE.to_le_bytes());
    pool[8..16].copy_from_slice(&LEN.to_le_bytes());
    ok("a semaphore pool is made", call(68, &mut pool, 9240));
    ok(
        "...its pages lent to the RM, held while it has memory of them",
        rm.pool_size.load(Ordering::Relaxed) == LEN
            && rm.pools.lock().unwrap().len() == 1
            && nvctl::GIVEN_BACK.load(Ordering::Relaxed) == given,
    );
    ok(
        "...and mapped on the GPU",
        rm.mapped.load(Ordering::Relaxed) == 1,
    );
    let mut free = [0u8; 24];
    free[0..8].copy_from_slice(&BASE.to_le_bytes());
    free[8..16].copy_from_slice(&LEN.to_le_bytes());
    ok("...and freed", call(34, &mut free, 16));
    ok(
        "...its pages given back with the memory",
        rm.pools.lock().unwrap().is_empty()
            && nvctl::GIVEN_BACK.load(Ordering::Relaxed) != given
            && nvctl::GIVE_BACK_REFUSED.load(Ordering::Relaxed) == 0,
    );
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
    // Every function of that identity the kernel left to userspace, as
    // `run` claims every GPU: two behind the ports and the DMA memory of
    // both, with `udriver=pci-VVVV-DDDD@0,pci-VVVV-DDDD@1`.
    let gpus = gpu::claim_all(|index| gpu::Which::Pci {
        vendor,
        device,
        index,
    })
    .unwrap_or_else(|e| fail(e));
    println!("{NAME}: {} function(s) claimed", gpus.len());
    extern "C" {
        fn drivers_phys_to_virt(paddr: usize) -> usize;
    }
    for gpu in &gpus {
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
                probed.is_some_and(|(addr, len)| {
                    addr == bar.phys && len.next_power_of_two() == bar.len
                }),
            );
        }
        let mapper = gpu.mapper();
        let first = gpu
            .bars
            .first()
            .copied()
            .unwrap_or_else(|| fail("no BAR".into()));
        let at = mapper.query_or_map(first.phys + 0x1000, 0x1000);
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
    }
    let gpu = &gpus[0];
    let first = gpu.bars[0];
    // The firmware's console is on QEMU's first VGA: its framebuffer is
    // in that function's BAR 0, where the driver looks for it.
    let display = boot_display().unwrap_or_else(|e| fail(format!("boot display: {e}")));
    println!("{NAME}: {}", describe(&display));
    ok(
        "the boot's framebuffer is told, in the first function's BAR 0",
        display.fb.is_some_and(|(phys, ..)| {
            (first.phys..first.phys + first.len).contains(&(phys as usize))
        }),
    );
    display.install();
    ok(
        "...and the driver is told it as the kernel's is",
        zcore_drivers::display::boot_fb_info() == display.fb,
    );
    // The boot line's switches: `nvidia.nouveau_uapi` on it (or not) is
    // told, and the driver here takes it.
    let flags = driver_flags().unwrap_or_else(|e| fail(format!("driver flags: {e}")));
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let asked = cmdline
        .split([':', ' ', '\t', '\n'])
        .any(|t| t == "nvidia.nouveau_uapi");
    println!(
        "{NAME}: nvidia.* switches {:#x} (nvidia.nouveau_uapi {}on the boot line)",
        flags.bits,
        if asked { "" } else { "not " }
    );
    ok(
        "the boot line's nvidia.nouveau_uapi is told as it is",
        (flags.bits & driver_flags::NOUVEAU_UAPI != 0) == asked,
    );
    flags.install();
    ok(
        "...and the driver here takes it",
        zcore_drivers::display::nouveau_uapi_enabled() == asked,
    );
    gpu::install_dma(&gpus).unwrap_or_else(|e| fail(e));
    extern "C" {
        fn drivers_dma_alloc(pages: usize) -> usize;
    }
    // SAFETY: the driver's own allocation call.
    let dma = unsafe { drivers_dma_alloc(2) };
    // SAFETY: as above.
    let virt = unsafe { drivers_phys_to_virt(dma) };
    ok(
        "DMA memory has one device address for every function, and a mapping",
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

#[cfg(test)]
mod tests {
    use super::has_flag;

    #[test]
    fn a_flag_is_a_word_of_the_boot_line() {
        let line = "LOG=warn nvidia.noautoboot:nvidia.cuda udriver=nvidia@0\n";
        assert!(has_flag(line, "nvidia.noautoboot"));
        assert!(has_flag(line, "nvidia.cuda"));
        assert!(!has_flag(line, "nvidia.noauto"));
        assert!(!has_flag("nvidia.noautobootx", "nvidia.noautoboot"));
    }
}

/// A bring-up of the RM that kills the process (a fault in the C) must
/// not kill the desktop with it: the service starts the process again,
/// and that run serves the cards without trying the RM a second time.
/// A mark in `/tmp` says a bring-up is under way; the run that finds it
/// left behind takes it away and skips the bring-up, so the one after
/// tries again.
mod bringup_guard {
    const MARK: &str = "/tmp/udrv-nvidia.bringup";

    /// Whether to bring the RM up now.
    pub fn enter() -> bool {
        enter_at(std::path::Path::new(MARK))
    }

    /// The bring-up came back.
    pub fn leave() {
        let _ = std::fs::remove_file(MARK);
    }

    pub(super) fn enter_at(mark: &std::path::Path) -> bool {
        if std::fs::remove_file(mark).is_ok() {
            return false;
        }
        // Without a mark there is no guard, but the bring-up still runs.
        let _ = std::fs::write(mark, b"");
        true
    }

    #[cfg(test)]
    mod tests {
        use super::enter_at;

        #[test]
        fn a_bring_up_that_never_came_back_is_skipped_once() {
            let mark =
                std::env::temp_dir().join(format!("udrv-nvidia-guard-{}", std::process::id()));
            let _ = std::fs::remove_file(&mark);
            assert!(enter_at(&mark), "first run brings the RM up");
            // It died: the mark is still there.
            assert!(!enter_at(&mark), "the next run skips it");
            assert!(enter_at(&mark), "and the one after tries again");
            let _ = std::fs::remove_file(&mark);
        }
    }
}
