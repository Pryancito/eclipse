pub use crate::arch::ContextData;
use core::cell::UnsafeCell;

#[derive(Debug, Default)]
pub struct Context {
    context: UnsafeCell<usize>,
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;

    #[test]
    fn switch_save_cell_can_be_written_through_its_raw_pointer() {
        let context = Context::default();
        unsafe { *(context.get_context() as *mut usize) = 0x1234 };
        assert_eq!(context.get_sp(), 0x1234);
    }

    #[test]
    fn parked_context_is_returned_as_a_snapshot() {
        let mut frame = ContextData::new(1, 0, 2);
        let mut context = Context::default();
        context.set_context(&mut frame as *mut ContextData as usize);
        let snapshot = context.get_context_data();
        frame.rip = 3;
        assert_eq!(snapshot.rip, 1);
        assert_eq!((frame.rip, context.get_pc()), (3, 3));
        assert_eq!(context.get_pgbr(), 2);
    }
}

impl Context {
    pub fn set_context(&mut self, addr: usize) {
        *self.context.get_mut() = addr;
    }

    /// Snapshot an initialized, parked frame; it must not be switched concurrently.
    pub fn get_context_data(&self) -> ContextData {
        unsafe {
            let context = *self.context.get() as *const ContextData;
            core::ptr::read(context)
        }
    }

    #[cfg(target_arch = "x86_64")]
    pub fn get_context(&self) -> usize {
        self.context.get() as usize
    }

    #[cfg(target_arch = "x86_64")]
    pub fn get_sp(&self) -> usize {
        unsafe { *self.context.get() }
    }

    #[cfg(target_arch = "x86_64")]
    pub fn get_pc(&self) -> usize {
        let context_data = self.get_context_data();
        context_data.rip
    }

    #[cfg(target_arch = "x86_64")]
    pub fn get_pgbr(&self) -> usize {
        let context_data = self.get_context_data();
        context_data.cr3
    }

    #[cfg(target_arch = "riscv64")]
    pub fn get_context(&self) -> usize {
        unsafe { *self.context.get() }
    }

    #[cfg(target_arch = "riscv64")]
    pub fn get_sp(&self) -> usize {
        let context_data = self.get_context_data();
        context_data.sp
    }

    #[cfg(target_arch = "riscv64")]
    pub fn get_pc(&self) -> usize {
        let context_data = self.get_context_data();
        context_data.ra
    }

    #[cfg(target_arch = "riscv64")]
    pub fn get_pgbr(&self) -> usize {
        let context_data = self.get_context_data();
        context_data.satp
    }

    #[cfg(target_arch = "aarch64")]
    pub fn get_context(&self) -> usize {
        unsafe { *self.context.get() }
    }

    #[cfg(target_arch = "aarch64")]
    pub fn get_sp(&self) -> usize {
        self.get_context_data().sp
    }

    #[cfg(target_arch = "aarch64")]
    pub fn get_pc(&self) -> usize {
        let context_data = self.get_context_data();
        context_data.lr
    }

    #[cfg(target_arch = "aarch64")]
    pub fn get_pgbr(&self) -> usize {
        let context_data = self.get_context_data();
        context_data.ttbr0
    }
}
