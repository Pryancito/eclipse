pub mod config;
pub mod cpu;
pub mod drivers;
pub mod interrupt;
pub mod mem;
pub mod smp;
pub mod timer;
pub mod trap;
pub mod vm;

use crate::KCONFIG;
use crate::{mem::phys_to_virt, utils::init_once::InitOnce, PhysAddr};
use alloc::string::{String, ToString};
use core::ops::Range;

hal_fn_impl_default!(crate::hal_fn::console);

static INITRD_REGION: InitOnce<Option<Range<PhysAddr>>> = InitOnce::new_with_default(None);
static CMDLINE: InitOnce<String> = InitOnce::new_with_default(String::new());

pub fn cmdline() -> String {
    CMDLINE.clone()
}

pub fn init_ram_disk() -> Option<&'static mut [u8]> {
    INITRD_REGION.as_ref().map(|range| unsafe {
        core::slice::from_raw_parts_mut(phys_to_virt(range.start) as *mut u8, range.len())
    })
}

/// Per-CPU control registers that have to be set on every core, not once.
///
/// The secondaries get the same two out of the SMP trampoline, which copies
/// this core's values; see `smp::TransRegs` for what happens to a core that
/// misses them.
fn init_percpu_control_regs() {
    use cortex_a::{asm::barrier, registers::CPACR_EL1};
    use tock_registers::interfaces::{Readable, Writeable};

    // User contexts preserve the architectural FP/SIMD state, so permit both
    // EL0 and EL1 to execute those instructions instead of trapping on first use.
    CPACR_EL1.set(CPACR_EL1.get() | (0b11 << 20));
    let mut cntkctl: usize;
    unsafe {
        core::arch::asm!("mrs {0}, cntkctl_el1", out(reg) cntkctl);
        // Allow EL0 to read both physical and virtual architectural counters.
        core::arch::asm!("msr cntkctl_el1, {0}", in(reg) (cntkctl | 0b11));
    }
    unsafe { barrier::isb(barrier::SY) };
}

pub fn primary_init_early() {
    init_percpu_control_regs();
    CMDLINE.init_once_by(KCONFIG.cmdline.to_string());
    drivers::init_early();
}

/// Switch this CPU onto the kernel's own page table. See the riscv64 twin of
/// this function for why it is not the first line of [`primary_init`].
pub fn activate_kernel_page_table() {
    vm::init();
}

pub fn primary_init() {
    drivers::init();
    // Bring up secondary cores now that the kernel page table and GIC are ready.
    smp::start_secondary_cores();
}

pub fn secondary_init() {
    // CPACR_EL1/CNTKCTL_EL1 are set by the SMP trampoline, before any compiled
    // Rust runs on this core -- see `smp::TransRegs`.
    //
    // First, put this core's TTBR0_EL1 back where the boot core's is. The
    // trampoline loads it with the identity table `build_identity_ttbr0` made
    // so the PC stays valid while the MMU comes on, and after the `br` to the
    // high-half entry nothing needs it again -- but nothing dropped it either,
    // so every secondary kept the trampoline's two physical pages mapped RWX
    // at their own addresses, for as long as the machine ran or until the core
    // first entered userspace. The boot core does not: `vm::init` clears
    // TTBR0_EL1 the moment it activates the kernel table, and riscv's
    // secondaries reach the same place by calling `vm::init` themselves from
    // their own `secondary_init`. This is the third answer to that question.
    //
    // It also publishes this core's active address space, which until now was
    // the "unknown" that makes a core a target of every TLB shootdown in the
    // system regardless of whose address space it is flushing.
    crate::vm::activate_kernel_paging();
    //
    // Bring up this core's half of the GIC. This used to be two writes done
    // by hand here -- GICC_CTLR and GICC_PMR, the CPU interface -- and it
    // stopped there, with the comment "GIC SGI are always-on". They are not.
    //
    // Interrupt ids 0..32 are private to a core and their distributor
    // registers are **banked per CPU interface**: `GICD_ISENABLER0` is one
    // address and one mapping, but the copy an access reaches is the copy
    // belonging to whichever core issued it. `init_early` enabled the timer
    // PPI (30) and the shootdown SGI (0) while running on the boot core, so
    // it enabled them for the boot core, and every core that came up
    // afterwards had its own bank at reset: nothing enabled.
    //
    // A core in that state is not dead, which is why this survived. It takes
    // the SGI's effect on nothing and the PPI's on nothing:
    //
    //  * no timer PPI is no 250 Hz scheduler tick, so the core runs whatever
    //    task it is handed until that task yields, and never preempts;
    //  * no IPI SGI is no TLB-shootdown acknowledgement, and the initiator's
    //    wait for it has no timeout. It is also no reschedule kick, so work
    //    placed on this core waits for a tick that does not come either.
    //
    // `init_hart` is the same call riscv makes here for its PLIC context, for
    // the same reason, and it now also does the two CPU-interface writes, so
    // there is one description of what a core needs instead of two.
    crate::drivers::primary_irq().init_hart();
    // Re-enable interrupts on this AP.
    interrupt::intr_on();
    smp::ap_signal_online();
}

pub const fn timer_interrupt_vector() -> usize {
    30
}

pub fn timer_init() {
    timer::init();
}
