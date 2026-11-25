//! In-kernel self-tests of the memory paths a host cannot run: `eclipse.ktest`.
//!
//! The host suites run the page-table walker, the entry formats and the VMO
//! tree on a model of memory. What they cannot run is the machine reading
//! those entries: that a mapping the walker wrote is the one the MMU
//! translates through, that an unmap really leaves the TLB, that the dirty
//! bit the stack poison trusts is set by the hardware. Those are what this
//! checks, on whichever of the three architectures the kernel booted on, with
//! the real MMU and the real TLB.
//!
//! With `eclipse.ktest` on the command line the kernel runs these right after
//! `primary_init` -- before any process, on the boot CPU -- prints one line
//! per case and a summary to the serial port, and powers the machine off.
//! `scripts/qemu-ktest.sh` boots it under QEMU and reads the lines:
//!
//! ```text
//! ktest: PASS kernel_mapping_reads_the_vmo_frames
//! ktest: done pass=5 fail=0 skip=1
//! ```
//!
//! Every case only ever touches memory it mapped itself, through the kernel
//! address space (`KERNEL_ASPACE`), and never takes a fault on purpose: a
//! kernel fault with no thread behind it halts the machine, which is a
//! failed run, not a failed case.

use alloc::sync::Arc;
use core::fmt::Write;

use zircon_object::vm::{MMUFlags, VmObject, KERNEL_ASPACE, PAGE_SIZE};

/// Whether `eclipse.ktest` is on the command line.
pub fn wanted(cmdline: &str) -> bool {
    kernel_hal::cmdline::flag(cmdline, "eclipse.ktest")
}

enum Outcome {
    Pass,
    Fail(alloc::string::String),
    Skip(&'static str),
}

type Case = (&'static str, fn() -> Outcome);

const CASES: &[Case] = &[
    (
        "kernel_mapping_reads_the_vmo_frames",
        kernel_mapping_reads_the_vmo_frames,
    ),
    (
        "a_write_through_the_mapping_lands_in_the_frame",
        a_write_through_the_mapping_lands_in_the_frame,
    ),
    (
        "a_remap_at_the_same_address_is_seen_at_once",
        a_remap_at_the_same_address_is_seen_at_once,
    ),
    (
        "a_snapshot_mapping_keeps_reading_the_old_page",
        a_snapshot_mapping_keeps_reading_the_old_page,
    ),
    (
        "the_hardware_sets_the_dirty_bit_the_stack_poison_trusts",
        the_hardware_sets_the_dirty_bit_the_stack_poison_trusts,
    ),
    (
        "a_vmap_stack_is_backed_page_by_page",
        a_vmap_stack_is_backed_page_by_page,
    ),
];

const ARCH: &str = if cfg!(target_arch = "x86_64") {
    "x86_64"
} else if cfg!(target_arch = "aarch64") {
    "aarch64"
} else {
    "riscv64"
};

fn line(args: core::fmt::Arguments) {
    kernel_hal::console::serial_write_fmt_spin(format_args!("ktest: {}\r\n", args));
}

/// Run every case, print the summary and power off. Never returns.
pub fn run_and_power_off() -> ! {
    line(format_args!("start {} cases on {}", CASES.len(), ARCH));
    let (mut pass, mut fail, mut skip) = (0, 0, 0);
    for (name, case) in CASES {
        match case() {
            Outcome::Pass => {
                pass += 1;
                line(format_args!("PASS {}", name));
            }
            Outcome::Fail(why) => {
                fail += 1;
                line(format_args!("FAIL {}: {}", name, why));
            }
            Outcome::Skip(why) => {
                skip += 1;
                line(format_args!("SKIP {}: {}", name, why));
            }
        }
    }
    line(format_args!(
        "done pass={} fail={} skip={}",
        pass, fail, skip
    ));
    kernel_hal::cpu::power_off()
}

macro_rules! check {
    ($cond:expr, $($arg:tt)*) => {
        if !$cond {
            let mut s = alloc::string::String::new();
            let _ = write!(s, $($arg)*);
            return Outcome::Fail(s);
        }
    };
}

/// A VMO of `pages` whose page `i` is filled with byte `seed + i`.
fn filled(pages: usize, seed: u8) -> Arc<VmObject> {
    let vmo = VmObject::new_paged(pages);
    let mut buf = alloc::vec![0u8; PAGE_SIZE];
    for i in 0..pages {
        buf.fill(seed.wrapping_add(i as u8));
        vmo.write(i * PAGE_SIZE, &buf).expect("vmo write");
    }
    vmo
}

fn map(vmo: &Arc<VmObject>, at: Option<usize>, flags: MMUFlags) -> Option<usize> {
    let len = vmo.len();
    let off = at.map(|va| va - KERNEL_ASPACE.addr());
    KERNEL_ASPACE.map(off, vmo.clone(), 0, len, flags).ok()
}

fn read_byte(va: usize) -> u8 {
    // SAFETY: every caller reads inside a mapping it made and still holds.
    unsafe { core::ptr::read_volatile(va as *const u8) }
}

fn write_byte(va: usize, v: u8) {
    // SAFETY: as `read_byte`, on a mapping made writable.
    unsafe { core::ptr::write_volatile(va as *mut u8, v) }
}

const RW: MMUFlags = MMUFlags::READ.union(MMUFlags::WRITE);

/// The entries the walker wrote are the ones the MMU translates through:
/// a wrong frame address or a wrong permission encoding in this
/// architecture's entry format reads the wrong bytes, or halts.
fn kernel_mapping_reads_the_vmo_frames() -> Outcome {
    let vmo = filled(8, 0x40);
    let Some(va) = map(&vmo, None, MMUFlags::READ) else {
        return Outcome::Fail("map refused".into());
    };
    let mut bad = None;
    for i in 0..8 {
        for at in [0, 1, PAGE_SIZE / 2, PAGE_SIZE - 1] {
            let got = read_byte(va + i * PAGE_SIZE + at);
            if got != 0x40 + i as u8 && bad.is_none() {
                bad = Some((i, at, got));
            }
        }
    }
    let _ = KERNEL_ASPACE.unmap(va, vmo.len());
    check!(bad.is_none(), "page/offset/byte {:x?}", bad);
    Outcome::Pass
}

fn a_write_through_the_mapping_lands_in_the_frame() -> Outcome {
    let vmo = filled(4, 0);
    let Some(va) = map(&vmo, None, RW) else {
        return Outcome::Fail("map refused".into());
    };
    for i in 0..4 {
        write_byte(va + i * PAGE_SIZE + 7, 0xd0 + i as u8);
    }
    let _ = KERNEL_ASPACE.unmap(va, vmo.len());
    for i in 0..4 {
        let mut b = [0u8; 1];
        vmo.read(i * PAGE_SIZE + 7, &mut b).unwrap();
        check!(
            b[0] == 0xd0 + i as u8,
            "page {} reads {:#x} through the VMO",
            i,
            b[0]
        );
    }
    Outcome::Pass
}

/// Read a page through a mapping (so this CPU's TLB holds it), unmap it,
/// map a different VMO at the very same address and read again: an unmap
/// that left the translation behind reads the first VMO's bytes.
fn a_remap_at_the_same_address_is_seen_at_once() -> Outcome {
    let a = filled(2, 0xa0);
    let b = filled(2, 0xb0);
    let Some(va) = map(&a, None, MMUFlags::READ) else {
        return Outcome::Fail("map refused".into());
    };
    let first = read_byte(va) as usize + read_byte(va + PAGE_SIZE) as usize;
    check!(first == 0xa0 + 0xa1, "first mapping reads {:#x}", first);
    if KERNEL_ASPACE.unmap(va, a.len()).is_err() {
        return Outcome::Fail("unmap refused".into());
    }
    let Some(again) = map(&b, Some(va), MMUFlags::READ) else {
        return Outcome::Fail("remap at the same address refused".into());
    };
    check!(again == va, "remap landed at {:#x}, not {:#x}", again, va);
    let (p0, p1) = (read_byte(va), read_byte(va + PAGE_SIZE));
    let _ = KERNEL_ASPACE.unmap(va, b.len());
    check!(
        (p0, p1) == (0xb0, 0xb1),
        "stale translation: read {:#x} {:#x} after the remap",
        p0,
        p1
    );
    Outcome::Pass
}

/// A snapshot child mapped read-only, and the parent written through the
/// VMO: the copy goes to the parent and the child's mapping keeps the
/// frame it had. The parent is not mapped, since a mapping the copy takes a
/// page away from would fault here with no thread to resolve it.
fn a_snapshot_mapping_keeps_reading_the_old_page() -> Outcome {
    let parent = filled(3, 0x10);
    let Ok(child) = parent.create_child(false, 0, 3 * PAGE_SIZE) else {
        return Outcome::Fail("create_child refused".into());
    };
    let Some(va) = map(&child, None, MMUFlags::READ) else {
        return Outcome::Fail("map refused".into());
    };
    let before = read_byte(va + PAGE_SIZE);
    parent.write(PAGE_SIZE, &[0x99]).unwrap();
    let after = read_byte(va + PAGE_SIZE);
    let mut p = [0u8; 1];
    parent.read(PAGE_SIZE, &mut p).unwrap();
    let _ = KERNEL_ASPACE.unmap(va, child.len());
    check!(before == 0x11, "child read {:#x} before the write", before);
    check!(
        after == 0x11,
        "child read {:#x} after the parent's write",
        after
    );
    check!(p[0] == 0x99, "parent reads {:#x}", p[0]);
    Outcome::Pass
}

/// The scheduler poisons only the part of a recycled stack the dirty bits
/// say was written (`executor::poison_bound`); that is safe only if the
/// MMU really sets the bit on a write. x86_64 must answer, and answer at
/// least the page written; riscv64 and aarch64 must say they cannot.
fn the_hardware_sets_the_dirty_bit_the_stack_poison_trusts() -> Outcome {
    let vmo = filled(4, 0);
    let Some(va) = map(&vmo, None, RW) else {
        return Outcome::Fail("map refused".into());
    };
    let len = vmo.len();
    let mut pt = kernel_hal::vm::PageTable::from_current();
    // Nothing has been through `va` yet, so no TLB holds these entries and
    // the cleared bits need no flush before the write below.
    pt.clear_dirty_range(va, len);
    let clean = pt.dirty_depth_from_top(va, len);
    // The top page only: a stack grows down from there.
    write_byte(va + len - 8, 1);
    let dirty = pt.dirty_depth_from_top(va, len);
    let _ = KERNEL_ASPACE.unmap(va, len);
    if cfg!(target_arch = "x86_64") {
        check!(clean == Some(0), "a cleared range answers {:?}", clean);
        check!(
            matches!(dirty, Some(d) if d >= PAGE_SIZE),
            "a write to the top page answers {:?}",
            dirty
        );
        Outcome::Pass
    } else {
        check!(
            dirty.is_none(),
            "no dirty bit on this architecture, yet it answers {:?}",
            dirty
        );
        Outcome::Pass
    }
}

/// A coroutine stack from the window of scattered frames: every page is
/// mapped to the frame the walker says, and a write through the stack's
/// address reaches that frame through the physmap.
#[cfg(target_arch = "x86_64")]
fn a_vmap_stack_is_backed_page_by_page() -> Outcome {
    const SIZE: usize = 64 * PAGE_SIZE;
    let Some(base) = kernel_hal::vmap_stack::alloc(SIZE) else {
        return Outcome::Skip("window off (see dmesg vmap_stack)");
    };
    use kernel_hal::vm::GenericPageTable;
    let pt = kernel_hal::vm::PageTable::from_current();
    let mut bad = None;
    for i in 0..SIZE / PAGE_SIZE {
        let va = base + i * PAGE_SIZE;
        write_byte(va + 3, 0x5a ^ i as u8);
        match pt.query(va) {
            Ok((pa, _, _)) => {
                let via_phys = kernel_hal::mem::phys_to_virt(pa) + 3;
                if read_byte(via_phys) != 0x5a ^ i as u8 && bad.is_none() {
                    bad = Some(i);
                }
            }
            Err(_) => {
                if bad.is_none() {
                    bad = Some(i);
                }
            }
        }
    }
    let freed = kernel_hal::vmap_stack::free(base, SIZE);
    check!(
        bad.is_none(),
        "page {:?} not backed by the frame its entry names",
        bad
    );
    check!(freed, "free refused its own block");
    Outcome::Pass
}

#[cfg(not(target_arch = "x86_64"))]
fn a_vmap_stack_is_backed_page_by_page() -> Outcome {
    Outcome::Skip("the stack window is x86_64 only")
}
