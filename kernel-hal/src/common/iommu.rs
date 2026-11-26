//! What the IOMMU lets a device see: the seam between the kernel objects that
//! grant a device memory (a bus transaction initiator and its pins) and the
//! hardware that enforces it (VT-d, AMD-Vi, an SMMU).
//!
//! Without an IOMMU a device reaches every byte of RAM, and a driver in a
//! process that programs its device with any address it likes reaches every
//! byte too: taking the driver out of the kernel isolates its code but not
//! its DMA. With one, each device a process drives gets a [`DmaDomain`] of
//! its own, and only what that process pinned for it is in there.
//!
//! A domain is a *sparse identity*: a page is mapped at the device address
//! equal to its physical address. So the addresses `zx_bti_pin` answers are
//! the same with an IOMMU and without one, no driver changes, and the kernel
//! checks that take a physical address from a driver (its GEM objects, its
//! fence zones) hold either way. What changes is that everything else is
//! absent from the device's view.
//!
//! A hardware backend (VT-d, AMD-Vi on x86_64) calls [`install`] once it
//! has found and programmed its units, on every boot whose firmware
//! describes one; with `iommu=off` on the command line it never does. With
//! no unit installed [`domain_for`] answers `None` and every device is in
//! passthrough, as on a machine without an IOMMU.

use alloc::sync::Arc;
use lock::Mutex;

use crate::{HalResult, MMUFlags, PhysAddr};

/// A PCI function by its place: what an IOMMU tells requesters apart by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PciRequester {
    pub segment: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl PciRequester {
    /// The 16-bit requester id the function puts on its transactions,
    /// `bus << 8 | device << 3 | function`: the index of its context entry
    /// (VT-d) or its device table entry (AMD-Vi).
    pub fn rid(&self) -> u16 {
        (self.bus as u16) << 8 | ((self.device as u16) & 0x1f) << 3 | (self.function as u16) & 0x7
    }
}

/// One device's view of memory. Dropping the last reference puts the device
/// back in passthrough: the domain's backend does that in its own `Drop`,
/// after the device has been reset.
pub trait DmaDomain: Send + Sync {
    /// Let the device reach `len` bytes (whole pages) at `paddr`, at device
    /// address `paddr`, for the accesses in `flags` (`READ`, `WRITE`). Only
    /// once this answers `Ok` may the device be told the address.
    fn map(&self, paddr: PhysAddr, len: usize, flags: MMUFlags) -> HalResult;

    /// Take `len` bytes at `paddr` out of the device's view. When this
    /// returns, the device can no longer reach them (its translation caches
    /// have been invalidated), so the frames may go back to the pool.
    fn unmap(&self, paddr: PhysAddr, len: usize);

    /// Let the device raise only the interrupts whose vectors are in
    /// `vectors` (sent to the CPU the kernel routes device interrupts to),
    /// and drop every other message it writes to the interrupt window: a
    /// driver that programs its device with any MSI it likes can no longer
    /// fire any vector on any CPU. `false` when this unit cannot limit
    /// them per device, which leaves them as they were.
    fn limit_interrupts(&self, vectors: &[u8]) -> bool {
        let _ = vectors;
        false
    }
}

/// The IOMMU hardware the kernel found and took over.
pub trait IommuUnit: Send + Sync {
    /// Its name in the log: `vt-d`, `amd-vi`, ...
    fn name(&self) -> &'static str;

    /// A fresh, empty domain for `requester`, which from now on reaches only
    /// what is mapped in it (plus the regions the firmware reserves for it).
    /// `None` when this unit does not cover that function, or cannot give it
    /// a domain: the device then stays in passthrough.
    fn domain_for(&self, requester: PciRequester) -> Option<Arc<dyn DmaDomain>>;

    /// Write to the log the DMA faults recorded since the last call (a
    /// device reaching for memory it was not given), and clear them. Called
    /// from a slow kernel task: faults raise no interrupt here.
    fn poll_faults(&self) {}
}

static UNIT: Mutex<Option<Arc<dyn IommuUnit>>> = Mutex::new(None);

/// Make `unit` the one that hands out domains. Called once, by the backend
/// that found it, before any device is claimed.
pub fn install(unit: Arc<dyn IommuUnit>) {
    crate::klog_info!("iommu: {} translating claimed devices", unit.name());
    *UNIT.lock() = Some(unit);
}

/// The unit in use, if any.
pub fn unit() -> Option<Arc<dyn IommuUnit>> {
    UNIT.lock().clone()
}

/// A domain of its own for `requester`, when an IOMMU is in use and covers
/// it; `None` leaves it in passthrough.
pub fn domain_for(requester: PciRequester) -> Option<Arc<dyn DmaDomain>> {
    let unit = unit()?;
    unit.domain_for(requester)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The requester id is how both VT-d and AMD-Vi index their tables, so
    /// a bit out of place hands a device another device's domain.
    #[test]
    fn the_requester_id_packs_bus_device_and_function() {
        let r = |bus, device, function| PciRequester {
            segment: 0,
            bus,
            device,
            function,
        };
        assert_eq!(r(0, 0, 0).rid(), 0);
        assert_eq!(r(0, 0x1f, 2).rid(), 0x00fa);
        assert_eq!(r(0x3a, 0x1d, 7).rid(), 0x3aef);
        assert_eq!(r(0xff, 0x1f, 7).rid(), 0xffff);
        assert_eq!(r(1, 0, 0).rid(), 0x0100);
    }
}
