use alloc::{boxed::Box, sync::Arc};
use kernel_hal::sync::Mutex;

use super::InterruptTrait;
use crate::dev::pci::{constants::PCIE_IRQRET_MASK, IPciNode};
use crate::{ZxError, ZxResult};

pub struct PciInterrupt {
    device: Arc<dyn IPciNode>,
    irq_id: usize,
    maskable: bool,
    inner: Mutex<PciInterruptInner>,
}

#[derive(Default)]
struct PciInterruptInner {
    register: bool,
}

impl PciInterrupt {
    /// Wrap one of `device`'s IRQ vectors.
    ///
    /// `vector` is not checked here, and cannot usefully be: how many vectors
    /// the device has is decided by `zx_pci_set_irq_mode`, which userspace may
    /// call again at any time. [`Self::register_handler`] is where the number
    /// meets the table that answers for it, and that is where a vector the
    /// device does not have comes back as `INVALID_ARGS` -- it used to be an
    /// `assert!`, so `zx_pci_map_interrupt` with any vector number below ten
    /// panicked the kernel.
    pub fn new(device: Arc<dyn IPciNode>, vector: u32, maskable: bool) -> Box<Self> {
        Box::new(PciInterrupt {
            device,
            irq_id: vector as _,
            maskable,
            inner: Default::default(),
        })
    }
}

impl InterruptTrait for PciInterrupt {
    fn mask(&self) {
        let inner = self.inner.lock();
        if self.maskable && inner.register {
            // `InterruptTrait` has nowhere to report this, and the caller is
            // often a `Drop`. The device having dropped the vector out from
            // under us is the expected way for it to fail, and it means the
            // vector is already as masked as it will ever be.
            if let Err(err) = self.device.disable_irq(self.irq_id) {
                warn!(
                    "pci: no se pudo enmascarar el vector {}: {:?}",
                    self.irq_id, err
                );
            }
        }
    }

    fn unmask(&self) {
        let inner = self.inner.lock();
        if self.maskable && inner.register {
            if let Err(err) = self.device.enable_irq(self.irq_id) {
                warn!(
                    "pci: no se pudo desenmascarar el vector {}: {:?}",
                    self.irq_id, err
                );
            }
        }
    }

    fn register_handler(&self, handle: Box<dyn Fn() + Send + Sync>) -> ZxResult {
        let mut inner = self.inner.lock();
        if inner.register {
            return Err(ZxError::ALREADY_BOUND);
        }
        self.device.register_irq_handle(
            self.irq_id,
            Box::new(move || {
                handle();
                PCIE_IRQRET_MASK
            }),
        )?;
        inner.register = true;
        Ok(())
    }

    fn unregister_handler(&self) -> ZxResult {
        let mut inner = self.inner.lock();
        if !inner.register {
            return Ok(());
        }
        self.device.unregister_irq_handle(self.irq_id);
        inner.register = false;
        Ok(())
    }
}
