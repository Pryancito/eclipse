use crate::context::TrapReason;
use riscv::register::scause;
use trapframe::TrapFrame;
pub(super) const SUPERVISOR_TIMER_INT_VEC: usize = 5; // scause::Interrupt::SupervisorTimer

/// `sstatus.SPP`: the privilege level the trap was taken from. Set means
/// supervisor (the kernel faulted on itself), clear means user.
const SSTATUS_SPP: usize = 1 << 8;

/// The x86 ring-0 and ring-3 code selectors, which is the vocabulary
/// `kstats::last_fault_from_kernel` speaks. See the call in `trap_handler`.
const KERNEL_CS_EQUIVALENT: u64 = 0x08;
const USER_CS_EQUIVALENT: u64 = 0x23;

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
        TrapReason::PageFault(vaddr, flags) => {
            // [diag] Stash the faulting register file, exactly as the x86_64
            // entry does. `handle_page_fault` receives only vaddr+flags, and
            // every diagnostic the kernel #PF reporter prints -- the faulting
            // pc, the call chain, "which register held the bad address", even
            // "was this fault taken in the kernel" -- reads back from here.
            //
            // Nothing recorded it on this architecture. `note_fault_regs` had
            // no caller outside x86_64, so a riscv64 kernel fault reported
            // `rip=0x0` and `no register file was recorded for this fault`,
            // and that is the whole reason the `null-range #PF` this job takes
            // inside the heap allocator has never been readable: the report
            // names the victim address and nothing that could name the
            // culprit.
            let _fault_scope = crate::kstats::note_fault_regs(
                tf.sepc as u64,
                tf.general.s0 as u64,
                tf.general.sp as u64,
                // `FAULT_CS` is read only through `last_fault_from_kernel`,
                // which asks "nonzero, and low two bits clear". riscv has no
                // CS; the equivalent fact is `sstatus.SPP`, the privilege the
                // trap came *from*. Encode it in the values that predicate was
                // written for, so one reader keeps working on both: x86's
                // ring-0 selector when SPP says supervisor, its ring-3 one
                // when SPP says user. A plain `sstatus` would have answered
                // "kernel" for a user fault, because its low bits are clear.
                if tf.sstatus & SSTATUS_SPP != 0 {
                    KERNEL_CS_EQUIVALENT
                } else {
                    USER_CS_EQUIVALENT
                },
            );
            // The rest of the register file, in `kstats::GPR_NAMES` order for
            // this architecture: the argument registers carry the pointers the
            // faulting call had in hand, which is what turns "a read of 0x1"
            // into "this object's field was null".
            // `s2..s4` rather than `gp`/`t1`/`t2`: `gp` is a constant, and
            // the free-list walk this file was instrumented for keeps its
            // cursor in `s2` (`ld s2, 0(s2)`), which the first table did not
            // record -- the one register the report needed was the one it
            // did not have.
            crate::kstats::note_fault_gprs(&[
                tf.general.ra as u64,
                tf.general.sp as u64,
                tf.general.s0 as u64,
                tf.general.s1 as u64,
                tf.general.s2 as u64,
                tf.general.s3 as u64,
                tf.general.s4 as u64,
                tf.general.a0 as u64,
                tf.general.a1 as u64,
                tf.general.a2 as u64,
                tf.general.a3 as u64,
                tf.general.a4 as u64,
                tf.general.a5 as u64,
                tf.general.a6 as u64,
                tf.general.a7 as u64,
                tf.general.t0 as u64,
            ]);
            // No `note_fault_rflags`: riscv has no flags register, and
            // `last_fault_rflags` reads zero, which the reporter already
            // treats as "nothing recorded" rather than printing a bogus line.
            crate::KHANDLER.handle_page_fault(vaddr, flags)
        }
        TrapReason::Interrupt(vector) => {
            crate::interrupt::handle_irq(vector);
            // Timer preemption: see thread trap path — do not context-switch here.
            let _ = vector;
        }
        other => panic!("Undefined trap: {:x?} {:#x?}", other, tf),
    }
}
