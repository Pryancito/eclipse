//! Packaging of [`virtio-drivers` library](https://github.com/rcore-os/virtio-drivers).

mod blk;
mod console;
mod gpu;
mod input;

pub use blk::VirtIoBlk;
pub use console::VirtIoConsole;
pub use gpu::VirtIoGpu;
pub use input::VirtIoInput;
pub mod virtio_pci;
pub use virtio_drivers::VirtIOHeader;

use crate::DeviceError;
use core::convert::From;
use virtio_drivers::Error;

impl From<Error> for DeviceError {
    fn from(err: Error) -> Self {
        match err {
            Error::BufferTooSmall => Self::BufferTooSmall,
            Error::NotReady => Self::NotReady,
            Error::InvalidParam => Self::InvalidParam,
            Error::DmaError => Self::DmaError,
            Error::AlreadyUsed => Self::AlreadyExists,
            Error::IoError => Self::IoError,
        }
    }
}

/// The four symbols `virtio-drivers` reaches the kernel through.
///
/// They live in `kernel-hal`, which this crate does not depend on, so a host
/// test binary with the `virtio` feature on had no way to link -- which is
/// part of why the CI line for this crate did not turn the feature on, and why
/// these 696 lines were never compiled as a test target by anything.
///
/// Nothing here builds a virtqueue: a driver talking to a device is tested
/// where the driver is, over the device model in `virtio-drivers`. These exist
/// so the test binary links, and say so loudly if a test ever does reach them.
#[cfg(test)]
mod hal_for_tests {
    #[no_mangle]
    extern "C" fn virtio_dma_alloc(_pages: usize) -> usize {
        unimplemented!(
            "a test in zcore-drivers asked for DMA; the device model lives in virtio-drivers"
        )
    }

    #[no_mangle]
    extern "C" fn virtio_dma_dealloc(_paddr: usize, _pages: usize) -> i32 {
        unimplemented!("a test in zcore-drivers freed DMA")
    }

    #[no_mangle]
    extern "C" fn virtio_phys_to_virt(paddr: usize) -> usize {
        paddr
    }

    #[no_mangle]
    extern "C" fn virtio_virt_to_phys(vaddr: usize) -> usize {
        vaddr
    }
}
