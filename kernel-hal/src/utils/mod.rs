#[cfg(not(feature = "libos"))]
use core::cell::UnsafeCell;

pub(crate) mod page_table;

#[cfg(test)]
pub(crate) mod test_frames;

pub(crate) mod init_once;

pub mod deferred_job;
pub mod lazy_init;
pub mod mpsc_queue;

#[cfg(not(feature = "libos"))]
pub struct PerCpuCell<T>(pub UnsafeCell<T>);

#[cfg(not(feature = "libos"))]
// #Safety: Only the corresponding cpu will access it.
unsafe impl<T> Sync for PerCpuCell<T> {}

#[cfg(not(feature = "libos"))]
impl<T> PerCpuCell<T> {
    pub const fn new(t: T) -> Self {
        Self(UnsafeCell::new(t))
    }

    pub fn get(&self) -> &T {
        unsafe { &*self.0.get() }
    }

    #[allow(clippy::mut_from_ref)]
    pub fn get_mut(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
}
