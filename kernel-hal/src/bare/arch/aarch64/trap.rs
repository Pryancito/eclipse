use crate::context::TrapReason;
use crate::{Kind, Source, KCONFIG};
use cortex_a::registers::FAR_EL1;
use tock_registers::interfaces::Readable;
use trapframe::TrapFrame;
use zcore_drivers::irq::gic_400::get_irq_num;

#[no_mangle]
pub extern "C" fn trap_handler(tf: &mut TrapFrame) {
    let source = Source::from_num(tf.trap_num & 0xffff);
    let kind = Kind::from_num((tf.trap_num >> 16) & 0xffff);
    trace!("Exception from {:?}", source);
    match kind {
        Some(Kind::Synchronous) => {
            sync_handler(tf);
        }
        Some(Kind::Irq) => {
            use crate::hal_fn::mem::phys_to_virt;
            crate::interrupt::handle_irq(get_irq_num(
                phys_to_virt(KCONFIG.gic_base + 0x1_0000),
                phys_to_virt(KCONFIG.gic_base),
            ));
        }
        // A vector-table entry that names no kind at all used to panic inside
        // `Kind::from`, before this got to say what it was looking at.
        other => {
            panic!(
                "Unsupported exception type: {:?}, TrapFrame: {:?}",
                other, tf
            );
        }
    }
    trace!("Exception end");
}

fn breakpoint(elr: &mut usize) {
    info!("Exception::Breakpoint: A breakpoint set @0x{:x} ", elr);
    *elr += 4;
}

/// `spsr_el1.M[3:0]`: the exception level the trap was taken from. `0b0000`
/// is EL0t, i.e. userspace; anything else is the kernel's own EL1.
const SPSR_MODE_MASK: usize = 0b1111;

/// The x86 ring-0 and ring-3 code selectors, which is the vocabulary
/// `kstats::last_fault_from_kernel` speaks. See the call in `sync_handler`.
const KERNEL_CS_EQUIVALENT: u64 = 0x08;
const USER_CS_EQUIVALENT: u64 = 0x23;

fn sync_handler(tf: &mut TrapFrame) {
    match TrapReason::from(tf.trap_num) {
        TrapReason::PageFault(vaddr, flags) => {
            // [diag] Stash the faulting register file, as the x86_64 entry
            // does and this one never did. See the same block in the riscv
            // entry: `note_fault_regs` had no caller outside x86_64, so a
            // kernel #PF on this architecture reported `rip=0x0` and `no
            // register file was recorded for this fault`.
            let _fault_scope = crate::kstats::note_fault_regs(
                tf.elr as u64,
                tf.general.x29 as u64,
                tf.sp as u64,
                // `FAULT_CS` is read only through `last_fault_from_kernel`,
                // which asks "nonzero, and low two bits clear". aarch64 has no
                // CS; the equivalent fact is the mode field of `spsr_el1`.
                // Encode it in the values that predicate was written for.
                if tf.spsr & SPSR_MODE_MASK == 0 {
                    USER_CS_EQUIVALENT
                } else {
                    KERNEL_CS_EQUIVALENT
                },
            );
            // In `kstats::GPR_NAMES` order for this architecture.
            crate::kstats::note_fault_gprs(&[
                tf.general.x0 as u64,
                tf.general.x1 as u64,
                tf.general.x2 as u64,
                tf.general.x3 as u64,
                tf.general.x4 as u64,
                tf.general.x5 as u64,
                tf.general.x6 as u64,
                tf.general.x7 as u64,
                tf.general.x8 as u64,
                tf.general.x9 as u64,
                tf.general.x10 as u64,
                tf.general.x19 as u64,
                tf.general.x20 as u64,
                tf.general.x29 as u64,
                tf.general.x30 as u64,
                tf.sp as u64,
            ]);
            // No `note_fault_rflags`: `last_fault_rflags` reads zero, which the
            // reporter treats as "nothing recorded".
            crate::KHANDLER.handle_page_fault(vaddr, flags)
        }
        TrapReason::SoftwareBreakpoint => breakpoint(&mut tf.elr),
        other => error!(
            "Unsupported trap in kernel: {:?}, FAR_EL1: {:#x?}",
            other,
            FAR_EL1.get()
        ),
    }
}
