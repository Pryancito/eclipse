use crate::{HalError, HalResult};

hal_fn_impl! {
    impl mod crate::hal_fn::interrupt {
        fn wait_for_interrupt() {}
        fn intr_on() {}
        fn intr_off() {}
        fn intr_get() -> bool {
            false
        }
        /// Publish `reason` into the target's IPI queue, exactly as the three
        /// bare architectures do.
        ///
        /// This used to log and return `Ok(())`, which is the drift
        /// `publish_ipi_entry` was written to end: a send that reports success
        /// without delivering the payload and without noting an overflow. The
        /// initiator then waits on an acknowledgement for a request the target
        /// was never handed. Nothing delivers the interrupt itself here (there
        /// is no second CPU to interrupt -- guest threads are host threads),
        /// so the queue is drained by whoever calls the ack path.
        fn send_ipi(cpuid: usize, reason: usize) -> HalResult {
            trace!("ipi [{}] => [{}]: {:x}", super::cpu::cpu_id(), cpuid, reason);
            if !crate::common::ipi::publish_ipi_entry(cpuid, reason) {
                return Err(HalError);
            }
            Ok(())
        }
        fn ipi_reason() -> Vec<usize> {
            Vec::new()
        }
    }
}
