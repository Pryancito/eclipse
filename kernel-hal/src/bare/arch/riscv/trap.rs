use crate::context::TrapReason;
use riscv::register::scause;
use trapframe::TrapFrame;
pub(super) const SUPERVISOR_TIMER_INT_VEC: usize = 5; // scause::Interrupt::SupervisorTimer

fn breakpoint(sepc: &mut usize) {
    info!("Exception::Breakpoint: A breakpoint set @0x{:x} ", sepc);

    //sepc为触发中断指令ebreak的地址
    //防止无限循环中断，让sret返回时跳转到sepc的下一条指令地址
    *sepc += 2
}

pub(super) fn super_timer() {
    super::timer::timer_set_next();
    crate::timer::timer_tick();
    //发生外界中断时，epc的指令还没有执行，故无需修改epc到下一条
}

/// The supervisor software interrupt: this architecture's TLB-shootdown IPI.
///
/// It used to call `ipi_reason()` and log what came back. That call consumed
/// the queue, so the handler *ate* every shootdown request this CPU was sent
/// and then flushed nothing and acknowledged nothing -- leaving the stale
/// mapping in place and the initiator spinning on a watermark that, with the
/// queue now reading empty, no later drain could ever advance. The initiator's
/// wait has no timeout (correctness over latency) and the NMI escalation is
/// x86-only, so on riscv every cross-CPU shootdown was a hang.
///
/// Drain and acknowledge instead, the same call x86_64 wires to its 0xf3
/// vector. Allocation-free, which an interrupt handler on the shootdown path
/// has to be: it is reached with the caller's lock held.
pub(super) fn super_soft() {
    #[allow(deprecated)]
    sbi_rt::legacy::clear_ipi();
    crate::common::ipi::tlb_shootdown_ack();
}

#[no_mangle]
pub extern "C" fn trap_handler(tf: &mut TrapFrame) {
    let scause = scause::read();
    trace!("kernel trap happened: {:?}", TrapReason::from(scause));
    trace!(
        "sepc = 0x{:x} pgtoken = 0x{:x}",
        tf.sepc,
        crate::vm::current_vmtoken()
    );
    match TrapReason::from(scause) {
        TrapReason::SoftwareBreakpoint => breakpoint(&mut tf.sepc),
        TrapReason::PageFault(vaddr, flags) => crate::KHANDLER.handle_page_fault(vaddr, flags),
        TrapReason::Interrupt(vector) => {
            crate::interrupt::handle_irq(vector);
            // Timer preemption: see thread trap path — do not context-switch here.
            let _ = vector;
        }
        other => panic!("Undefined trap: {:x?} {:#x?}", other, tf),
    }
}
