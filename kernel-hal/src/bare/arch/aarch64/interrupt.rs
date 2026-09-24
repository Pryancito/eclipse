//! Interrupts management.
use crate::HalResult;
use alloc::vec::Vec;
use cortex_a::asm::wfi;

hal_fn_impl! {
    impl mod crate::hal_fn::interrupt {
        fn wait_for_interrupt() {
            intr_on();
            wfi();
            intr_off();
        }

        fn handle_irq(vector: usize) {
            // TODO: timer and other devices with GIC interrupt controller
            // Cached primary-IRQ reference avoids the RwLock + Arc clone on
            // each interrupt that `all_irq().first_unwrap()` would do.
            crate::drivers::primary_irq().handle_irq(vector);
            if vector == 30 {
                debug!("Timer");
            }
        }

        fn intr_off() {
            unsafe {
                core::arch::asm!("msr daifset, #2");
            }
        }

        fn intr_on() {
            unsafe {
                core::arch::asm!("msr daifclr, #2");
            }
        }

        fn intr_get() -> bool {
            use cortex_a::registers::DAIF;
            use tock_registers::interfaces::Readable;
            !DAIF.is_set(DAIF::I)
        }

        fn send_ipi(cpuid: usize, reason: usize) -> HalResult {
            trace!("ipi [{}] => [{}]: {:x}", super::cpu::cpu_id(), cpuid, reason);
            // Resolve the target BEFORE publishing, and resolve it through the
            // affinity map rather than using the logical id as the target bit.
            //
            // A GICv2 SGI target list names CPU *interfaces* — on the
            // single-cluster systems GICv2 exists on, interface `n` is the core
            // with `Aff0 == n`. The dense logical id is not that number: ids go
            // out in the order cores reach `register_logical_id`, and
            // `start_secondary_cores` fires every `CPU_ON` before waiting for
            // any of them, so arrival order is whatever order the firmware
            // schedules them in. `1 << cpuid` therefore aimed the SGI at
            // whichever core happened to hold that Aff0, and a shootdown that
            // reaches the wrong core is one the initiator waits for forever:
            // the core it asked never flushes, and the core it woke
            // acknowledges nothing on its behalf. `logical_to_affinity` has
            // existed for this since the map was written, with no caller.
            //
            // `None` also covers the two cases the old `cpuid >= 8` check
            // could not see: a logical id no core was ever given (it used to
            // resolve to affinity 0, the boot CPU), and a core outside the boot
            // cluster, whose Aff0 collides with a core in it.
            let Some(affinity) = super::cpu::logical_to_affinity(cpuid) else {
                warn!("send_ipi: logical cpu {} names no core — dropped", cpuid);
                return Err(crate::HalError);
            };
            let Some(target) = crate::common::cpu_topology::gicv2_sgi_target(affinity) else {
                warn!(
                    "send_ipi: cpu {} (affinity {:#x}) is beyond the GICv2 SGI target list",
                    cpuid, affinity
                );
                return Err(crate::HalError);
            };
            // Push reason into per-CPU IPI queue, noting an overflow if it
            // will not fit — shared with the other architectures so the two
            // halves of that contract cannot drift apart again.
            if !crate::common::ipi::publish_ipi_entry(cpuid, reason) {
                warn!("send_ipi: logical cpu {} has no IPI queue — dropped", cpuid);
                return Err(crate::HalError);
            }
            // Send GIC SGI #0 to the target CPU (GICv2 GICD_SGIR)
            // GICD_SGIR: [25:24]=TargetListFilter=0b00 (use list), [23:16]=CPUTargetList, [3:0]=SGIINTID
            let gic_base = crate::hal_fn::mem::phys_to_virt(crate::KCONFIG.gic_base);
            const GICD_SGIR: usize = 0x0F00;
            let val: u32 = target << 16; // SGI 0
            unsafe {
                core::ptr::write_volatile((gic_base + GICD_SGIR) as *mut u32, val);
            }
            Ok(())
        }

        /// The scheduler's reschedule kick: the same SGI `send_ipi` delivers,
        /// with **no queue entry** — `handle_ipi` drains and acknowledges, and
        /// an empty drain asks for no flush.
        ///
        /// Like riscv, this was the empty default in `hal_fn.rs`, so the whole
        /// wake-preemption path above it (`request_resched`, the coalesced
        /// IPI, the sleeping mask) ended in a function with no body and every
        /// cross-CPU wake waited for the target's next tick.
        fn send_wake_ipi(cpuid: usize) {
            if !crate::common::ipi::wake_kick_wanted(cpuid) {
                return;
            }
            // Resolved through the affinity map, not `1 << cpuid`: see
            // `send_ipi` above for what the dense logical id is not.
            let Some(affinity) = super::cpu::logical_to_affinity(cpuid) else {
                return;
            };
            let Some(target) = crate::common::cpu_topology::gicv2_sgi_target(affinity) else {
                return;
            };
            let gic_base = crate::hal_fn::mem::phys_to_virt(crate::KCONFIG.gic_base);
            const GICD_SGIR: usize = 0x0F00;
            unsafe {
                core::ptr::write_volatile((gic_base + GICD_SGIR) as *mut u32, target << 16);
            }
        }

        fn ipi_reason() -> Vec<usize> {
            crate::common::ipi::ipi_reason()
        }
    }
}
