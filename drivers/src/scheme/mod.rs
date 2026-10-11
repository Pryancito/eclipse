//! The [`Scheme`] describe some functions must be implemented for different type of devices,
//! there are many [`Scheme`] traits in this mod.
//!
//! If you need to develop a new device, just implement the corresponding trait.
//!
//! The [`Scheme`] trait is suitable for any architecture.

pub(super) mod audio;
pub(super) mod block;
pub(super) mod display;
pub mod drm;
pub mod gem_mmap;
pub(super) mod input;
pub(super) mod irq;
pub(super) mod net;
pub mod prewait;
pub mod syncobj;
pub(super) mod uart;

#[macro_use]
pub(super) mod event;
pub(super) use impl_event_scheme;

use alloc::sync::Arc;

pub use audio::AudioScheme;
pub use block::BlockScheme;
pub use display::{set_scanout_mirror_x, DisplayScheme};
pub use drm::{
    ComputeLaunchResult, DrmScheme, COMPUTE_OP_BENCH, COMPUTE_OP_INFO, COMPUTE_OP_SAXPY,
};
pub use event::EventScheme;
pub use input::{AbsInfo, InputScheme};
pub use irq::{IrqHandler, IrqScheme};
pub use net::{NetScheme, NetStats, RouteInfo};
pub use uart::UartScheme;

/// Common of all device drivers.
///
/// Every device must says its name and handles interrupts.
pub trait Scheme: SchemeUpcast + Send + Sync {
    /// Returns name of the driver.
    fn name(&self) -> &str;

    /// Handles an interrupt.
    fn handle_irq(&self, _irq_num: usize) {}
}

/// Used to convert a concrete type pointer to a general [`Scheme`] pointer.
pub trait SchemeUpcast {
    /// Performs the conversion.
    fn upcast<'a>(self: Arc<Self>) -> Arc<dyn Scheme + 'a>
    where
        Self: 'a;
}

impl<T: Scheme + Sized> SchemeUpcast for T {
    fn upcast<'a>(self: Arc<Self>) -> Arc<dyn Scheme + 'a>
    where
        Self: 'a,
    {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// The smallest thing that is a device: a name, and nothing else.
    struct Plain;

    impl Scheme for Plain {
        fn name(&self) -> &str {
            "plain"
        }
    }

    /// A device that does handle interrupts, to tell the default apart from
    /// an override.
    struct Handling {
        seen: AtomicUsize,
    }

    impl Scheme for Handling {
        fn name(&self) -> &str {
            "handling"
        }
        fn handle_irq(&self, irq_num: usize) {
            self.seen.store(irq_num, Ordering::Relaxed);
        }
    }

    /// Most devices are polled or have no interrupt wired, so the default
    /// `handle_irq` must do nothing -- quietly. A spurious interrupt on a
    /// shared line reaches every device on it, and the ones that have nothing
    /// to do with it must simply return.
    #[test]
    fn a_device_without_interrupts_ignores_one_quietly() {
        Plain.handle_irq(0);
        Plain.handle_irq(usize::MAX);
    }

    /// And a device that does implement it gets the number it was given.
    #[test]
    fn a_device_that_handles_interrupts_is_given_the_number() {
        let dev = Handling {
            seen: AtomicUsize::new(usize::MAX),
        };
        dev.handle_irq(11);
        assert_eq!(dev.seen.load(Ordering::Relaxed), 11);
    }

    /// `upcast` is how a concrete driver is put into a `Device`, and the
    /// whole point is that it does not build a second object: the resulting
    /// `dyn Scheme` must be the SAME allocation, so a driver's state is not
    /// silently cloned behind its back.
    #[test]
    fn upcasting_keeps_the_same_allocation() {
        let dev = Arc::new(Plain);
        let before = Arc::strong_count(&dev);
        let erased: Arc<dyn Scheme> = dev.clone().upcast();
        assert_eq!(erased.name(), "plain");
        assert_eq!(
            Arc::strong_count(&dev),
            before + 1,
            "upcast did not share the allocation"
        );
        drop(erased);
        assert_eq!(Arc::strong_count(&dev), before);
    }

    /// And the erased pointer still dispatches to the driver's own override,
    /// not to the trait default.
    #[test]
    fn an_upcast_device_still_dispatches_to_its_own_override() {
        let dev = Arc::new(Handling {
            seen: AtomicUsize::new(usize::MAX),
        });
        let erased: Arc<dyn Scheme> = dev.clone().upcast();
        erased.handle_irq(5);
        assert_eq!(dev.seen.load(Ordering::Relaxed), 5);
    }
}
