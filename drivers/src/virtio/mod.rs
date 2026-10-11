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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every error `virtio-drivers` can return has to map to its own
    /// `DeviceError`, and the mapping is written out by hand: two arms that
    /// answered the same thing would turn a full buffer into a device that is
    /// not ready, and a caller that retries on `NotReady` would spin.
    ///
    /// `Error` is not exhaustive-matchable from here (it is another crate's),
    /// so the guard against a variant being added and silently falling into
    /// the wrong arm is that each of the six known ones is named here.
    #[test]
    fn every_virtio_error_maps_to_its_own_device_error() {
        let expected = [
            DeviceError::BufferTooSmall,
            DeviceError::NotReady,
            DeviceError::InvalidParam,
            DeviceError::DmaError,
            DeviceError::AlreadyExists,
            DeviceError::IoError,
        ];
        let pairs = [
            (Error::BufferTooSmall, DeviceError::BufferTooSmall),
            (Error::NotReady, DeviceError::NotReady),
            (Error::InvalidParam, DeviceError::InvalidParam),
            (Error::DmaError, DeviceError::DmaError),
            (Error::AlreadyUsed, DeviceError::AlreadyExists),
            (Error::IoError, DeviceError::IoError),
        ];
        for (from, want) in pairs {
            let shown = alloc::format!("{:?}", from);
            assert_eq!(DeviceError::from(from), want, "{} maps wrong", shown);
        }
        // And no two of them land on the same `DeviceError`: a collision is
        // what loses the distinction the mapping exists to keep.
        for (i, a) in expected.iter().enumerate() {
            for (j, b) in expected.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "two virtio errors map to the same device error");
                }
            }
        }
    }

    /// `AlreadyUsed` is the one arm whose names do not match, so it is the one
    /// a careless edit would "fix" into something else.
    #[test]
    fn an_already_used_descriptor_is_an_already_existing_resource() {
        assert_eq!(
            DeviceError::from(Error::AlreadyUsed),
            DeviceError::AlreadyExists
        );
    }

    /// The conversion is what `?` uses throughout the virtio drivers, so it
    /// has to work through `Result`, not only as a direct call.
    #[test]
    fn the_conversion_works_through_the_question_mark_operator() {
        fn inner() -> Result<(), Error> {
            Err(Error::DmaError)
        }
        fn outer() -> crate::DeviceResult {
            inner()?;
            Ok(())
        }
        assert_eq!(outer(), Err(DeviceError::DmaError));
    }
}
