use crate::context::TrapReason;
use trapframe::TrapFrame;

pub(super) const X86_INT_LOCAL_APIC_BASE: usize = 0xf0;
pub(super) const _X86_INT_APIC_SPURIOUS: usize = X86_INT_LOCAL_APIC_BASE;
pub(super) const X86_INT_APIC_TIMER: usize = X86_INT_LOCAL_APIC_BASE + 0x1;
pub(super) const _X86_INT_APIC_ERROR: usize = X86_INT_LOCAL_APIC_BASE + 0x2;

// ISA IRQ numbers
pub(super) const _X86_ISA_IRQ_PIT: usize = 0;
pub(super) const X86_ISA_IRQ_KEYBOARD: usize = 1;
pub(super) const _X86_ISA_IRQ_PIC2: usize = 2;
pub(super) const X86_ISA_IRQ_COM2: usize = 3;
pub(super) const X86_ISA_IRQ_COM1: usize = 4;
pub(super) const _X86_ISA_IRQ_CMOSRTC: usize = 8;
pub(super) const X86_ISA_IRQ_MOUSE: usize = 12;
pub(super) const _X86_ISA_IRQ_IDE: usize = 14;

fn breakpoint() {
    panic!("\nEXCEPTION: Breakpoint");
}

#[cfg(target_arch = "x86_64")]
fn kernel_trap_name(trap_num: usize) -> &'static str {
    match trap_num {
        0x0 => "#DE divide error",
        0x1 => "#DB debug",
        0x3 => "#BP breakpoint",
        0x4 => "#OF overflow",
        0x5 => "#BR bound range",
        0x6 => "#UD invalid opcode",
        0x7 => "#NM device not available (FPU/SSE?)",
        0x8 => "#DF double fault",
        0x9 => "#MC coprocessor segment overrun",
        0xa => "#TS invalid TSS",
        0xb => "#NP segment not present",
        0xc => "#SS stack fault",
        0xd => "#GP general protection",
        0xe => "#PF page fault",
        0x10 => "#MF x87 FP exception",
        0x11 => "#AC alignment check",
        0x13 => "#XF SIMD FP exception",
        0x100 => "syscall",
        _ => "unknown",
    }
}

pub(super) fn super_timer() {
    crate::timer::timer_tick();
}

#[no_mangle]
pub extern "C" fn trap_handler(tf: &mut TrapFrame) {
    trace!(
        "Interrupt: {:#x} @ CPU{}",
        tf.trap_num,
        super::cpu::cpu_id()
    );

    match TrapReason::from(tf.trap_num, tf.error_code) {
        TrapReason::HardwareBreakpoint | TrapReason::SoftwareBreakpoint => breakpoint(),
        TrapReason::PageFault(vaddr, flags) => crate::KHANDLER.handle_page_fault(vaddr, flags),
        TrapReason::Interrupt(vector) => {
            crate::interrupt::handle_irq(vector);
            // Timer preemption is handled in the thread trap path (e.g.
            // `loader/src/linux.rs` calls `yield_now` on TIMER). Never call
            // `executor::handle_timeout()` here: it context-switches from IRQ
            // context and abandons the trap frame → triple fault (QEMU/VBox).
        }
        other => {
            let trap_num = match &other {
                TrapReason::GernelFault(n) => *n,
                TrapReason::Syscall => 0x100,
                TrapReason::UndefinedInstruction => 0x6,
                TrapReason::UnalignedAccess => 0x11,
                _ => tf.trap_num,
            };
            crate::klog_err!(
                "kernel trap on CPU{}: {:?} ({}) rip={:#x} cs={:#x} rflags={:#x} err={:#x}\n{:#x?}",
                super::cpu::cpu_id(),
                other,
                kernel_trap_name(trap_num),
                tf.rip,
                tf.cs,
                tf.rflags,
                tf.error_code,
                tf,
            );
            panic!("Unhandled kernel trap {}", kernel_trap_name(trap_num));
        }
    }
}
