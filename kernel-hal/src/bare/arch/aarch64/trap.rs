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

fn sync_handler(tf: &mut TrapFrame) {
    match TrapReason::from(tf.trap_num) {
        TrapReason::PageFault(vaddr, flags) => crate::KHANDLER.handle_page_fault(vaddr, flags),
        TrapReason::SoftwareBreakpoint => breakpoint(&mut tf.elr),
        other => error!(
            "Unsupported trap in kernel: {:?}, FAR_EL1: {:#x?}",
            other,
            FAR_EL1.get()
        ),
    }
}
