//! `udrv-pci-check VVVV DDDD`: claim the first PCI function of that
//! identity (`udriver=pci-VVVV-DDDD@0` on the boot line) and check what
//! `ECLIPSE_DEVICE_PCI_FN` reaches of it: its configuration space, read
//! and written by the kernel for it and only where a driver may write, and
//! its memory BARs beyond the claim's window. QEMU's standard VGA
//! (`1234 1111`) is the function it is run on: BAR 0 is its framebuffer
//! and BAR 2 its registers, where the Bochs display interface answers its
//! version at 0x500. Where the function is and how long its BARs are
//! is the kernel's to say too.
use std::process::exit;
use udrivers::claim::{claim_pci, PciFn};
use udrivers::zx::{self, Status};

const NAME: &str = "udrv-pci-check";

fn check(what: &str, ok: bool) {
    println!("{NAME}: {what}: {}", if ok { "ok" } else { "FAILED" });
    if !ok {
        exit(1);
    }
}

fn hex(s: Option<&String>) -> u16 {
    s.and_then(|s| u16::from_str_radix(s, 16).ok())
        .unwrap_or_else(|| {
            eprintln!("usage: {NAME} VVVV DDDD");
            exit(2)
        })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (vendor, device) = (hex(args.get(1)), hex(args.get(2)));
    let other = PciFn::of_pci(vendor, device, 0);
    check(
        "nothing of a function this process does not hold",
        other.config_read(0, 2) == Err(Status::ACCESS_DENIED),
    );
    let claim = claim_pci(vendor, device, 0, 1).unwrap_or_else(|e| {
        eprintln!("{NAME}: claim {vendor:04x}:{device:04x}: {e}");
        exit(1)
    });
    let f = PciFn::of_pci(vendor, device, 0);
    check(
        "the configuration space says which function it is",
        f.config_read(0, 2) == Ok(vendor as u32) && f.config_read(2, 2) == Ok(device as u32),
    );
    let bar0 = f.config_read(0x10, 4).unwrap_or(0);
    check("a dword is read whole", bar0 & !0xf != 0);
    let command = f.config_read(4, 2).unwrap_or(0);
    check("memory decoding is on", command & 2 != 0);
    check(
        "the command register is a driver's to write",
        f.config_write(4, 2, command).is_ok() && f.config_read(4, 2) == Ok(command),
    );
    check(
        "a BAR is the kernel's",
        f.config_write(0x10, 4, 0) == Err(Status::ACCESS_DENIED)
            && f.config_read(0x10, 4) == Ok(bar0),
    );
    check(
        "...and so is the identity",
        f.config_write(0, 2, 0xffff) == Err(Status::ACCESS_DENIED),
    );
    check(
        "an access outside the space is refused",
        f.config_read(0x100, 4) == Err(Status::OUT_OF_RANGE)
            && f.config_read(0x41, 2) == Err(Status::OUT_OF_RANGE),
    );
    // BAR 0, write-combining, as a framebuffer is: what is written there
    // reads back.
    let fb = f.bar(0, true).unwrap_or_else(|e| {
        eprintln!("{NAME}: bar 0: {e}");
        exit(1)
    });
    let p = zx::map(fb, 4096).unwrap_or_else(|e| {
        eprintln!("{NAME}: map bar 0: {e}");
        exit(1)
    }) as *mut u32;
    // SAFETY: the first page of the framebuffer, mapped above.
    let back = unsafe {
        p.add(7).write_volatile(0x00c0_ffee);
        p.add(7).read_volatile()
    };
    check("a BAR's memory is reached", back == 0x00c0_ffee);
    // BAR 2: the Bochs display interface's registers, beyond the claim's
    // window (BAR 0).
    let regs = f
        .bar(2, false)
        .and_then(|h| zx::map(h, 4096).map_err(|_| Status::IO));
    let id = regs.map(|r| {
        // SAFETY: the register page of BAR 2, mapped above: the interface's
        // ID register is the first index, at 0x500.
        unsafe { (r.add(0x500) as *const u16).read_volatile() }
    });
    check(
        "another BAR's registers answer",
        matches!(id, Ok(v) if (0xb0c0..=0xb0cf).contains(&v)),
    );
    check(
        "a BAR the function lacks",
        f.bar(1, false) == Err(Status::OUT_OF_RANGE),
    );
    // Where it is and how long its BARs are, which a driver cannot size
    // itself: QEMU's standard VGA has 16 MiB of video memory and a page of
    // registers, and no BAR 1.
    let place = f.place();
    println!("{NAME}: place {place:x?}");
    check("it says where it is", place.is_ok());
    check(
        "its BARs are as long as the kernel found them",
        f.bar_len(0) == Ok(16 << 20) && f.bar_len(2) == Ok(4096) && f.bar_len(1) == Ok(0),
    );
    let _ = claim;
    println!("{NAME}: pci-fn: ok");
}
