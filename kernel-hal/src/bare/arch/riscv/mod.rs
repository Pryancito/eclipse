mod drivers;
mod trap;

pub mod config;
pub mod cpu;
pub mod interrupt;
pub mod mem;
pub mod sbi;
pub mod timer;
pub mod vm;

use crate::{mem::phys_to_virt, utils::init_once::InitOnce, PhysAddr};
use alloc::{string::String, vec::Vec};
use core::ops::Range;
use core::sync::atomic::{AtomicBool, Ordering};
use zcore_drivers::utils::devicetree::Devicetree;

static CMDLINE: InitOnce<String> = InitOnce::new_with_default(String::new());
static INITRD_REGION: InitOnce<Option<Range<PhysAddr>>> = InitOnce::new_with_default(None);
static MEMORY_REGIONS: InitOnce<Vec<Range<PhysAddr>>> = InitOnce::new_with_default(Vec::new());

/// Set once the primary hart's device-tree walk has registered every device.
///
/// Unlike x86_64 -- where the BSP starts the APs itself, long after
/// `primary_init()` -- every RISC-V hart is released by SBI at once, and
/// `zCore/src/platform/riscv/entry.rs` calls `boot_secondary_harts()` BEFORE
/// `primary_main()`. So a secondary reached `secondary_init()` while the
/// primary was still walking the device tree, found no `riscv-intc-cpuN`
/// device yet, and panicked in `drivers::intc_init()` with "IRQ device
/// 'riscv-intc' not initialized!". That is why the RISC-V jobs failed
/// non-deterministically: `Linux Libc Test Baremetal (riscv64)` aborted at
/// test 0 with every secondary down, while `Linux Other Test Baremetal
/// (riscv64)` passed 24 cases and panicked on 6.
static DRIVERS_READY: AtomicBool = AtomicBool::new(false);

pub const fn timer_interrupt_vector() -> usize {
    trap::SUPERVISOR_TIMER_INT_VEC
}

pub fn cmdline() -> String {
    CMDLINE.clone()
}

pub fn init_ram_disk() -> Option<&'static mut [u8]> {
    INITRD_REGION.as_ref().map(|range| unsafe {
        core::slice::from_raw_parts_mut(phys_to_virt(range.start) as *mut u8, range.len())
    })
}

pub fn primary_init_early() {
    let dt = Devicetree::from(phys_to_virt(crate::KCONFIG.dtb_paddr)).unwrap();
    if let Some(cmdline) = dt.bootargs() {
        info!("Load kernel cmdline from DTB: {:?}", cmdline);
        CMDLINE.init_once_by(cmdline.into());
    }
    // A device tree that reports no timebase, or reports zero, leaves the
    // default standing: the clock read divides by this number.
    if let Some(time_freq) = dt.timebase_frequency().filter(|hz| *hz != 0) {
        info!("Load timebase frequency from DTB: {} Hz", time_freq);
        super::cpu::TIMEBASE_HZ.init_once_by(time_freq as u64);
    }
    if let Some(initrd_region) = dt.initrd_region() {
        info!("Load initrd regions from DTB: {:#x?}", initrd_region);
        INITRD_REGION.init_once_by(Some(initrd_region));
    }
    if let Ok(regions) = dt.memory_regions() {
        info!("Load memory regions from DTB: {:#x?}", regions);
        MEMORY_REGIONS.init_once_by(regions);
    }
}

/// Switch this CPU onto the kernel's own page table.
///
/// Called from `bare::boot::primary_init`, before the scheduler exists. It
/// used to sit at the top of [`primary_init`] below, which is much later:
/// everything in between — `stack_guard::init` and the first `Executor::new`
/// among it — ran on the page table `entry.rs` built to get out of physical
/// addressing, and any page table edit made there was thrown away by this very
/// call. That is what left every coroutine stack without its guard band:
/// `install` punched its 4 KiB holes into the boot table, saw them, and then
/// the kernel switched to a table that had never heard of them.
pub fn activate_kernel_page_table() {
    vm::init();
}

pub fn primary_init() {
    drivers::init().unwrap();
    // Release the secondaries spinning in `secondary_init()`: the per-hart
    // `riscv-intc-cpuN` devices they look up exist only now.
    DRIVERS_READY.store(true, Ordering::Release);
}

pub fn timer_init() {
    timer::init();
}

pub fn secondary_init() {
    vm::init();
    // Wait for the primary hart's device-tree walk (see `DRIVERS_READY`).
    // Without this the lookups below race it and `.expect()` takes the machine
    // down. Plain spin: this runs once per hart at boot, with no scheduler yet.
    while !DRIVERS_READY.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    info!("cpu {} drivers init ...", crate::cpu::cpu_id());
    drivers::intc_init().unwrap();
    let plic = crate::drivers::all_irq()
        .find("riscv-plic")
        .expect("IRQ device 'riscv-plic' not initialized!");
    info!(
        "cpu {} enable plic: {:?}",
        crate::cpu::cpu_id(),
        plic.name()
    );
    plic.init_hart();
    // Join the online set, exactly as x86's and aarch64's `ap_signal_online`
    // do at the end of their own `secondary_init`. Without it `CPU_ONLINE`
    // stayed at "BSP only" forever on riscv however many harts came up, and
    // three things read that mask:
    //
    //  * `wake_kick_wanted`, which every reschedule IPI goes through. A hart
    //    that is not in the mask "is running no task, so there is nothing on
    //    it to wake" — so the kick was dropped and the task that had just
    //    been made runnable waited for the hart's next 250 Hz tick, up to
    //    4 ms. That is the cost #1398 removed by giving riscv a
    //    `send_wake_ipi` body at all, taken right back at the gate in front
    //    of it;
    //  * `sched_getaffinity`, which hands the mask to userspace, so `nproc`
    //    answered 1 and every thread pool sized itself for one hart;
    //  * the `/proc` busy% and perf denominators, which divide by the online
    //    count.
    //
    // This is the end of `secondary_init`, the same point in bring-up the
    // other two use: the hart has its dense logical id (`register_logical_id`
    // ran in `percpu`), its page table and its PLIC context. Being online is
    // deliberately NOT being an IPI target — that is `IPI_READY`, published
    // later from the executor's own entry path, once this hart runs with
    // interrupts on and can actually ack a shootdown.
    crate::common::ipi::mark_cpu_online(crate::cpu::cpu_id() as usize);
}
