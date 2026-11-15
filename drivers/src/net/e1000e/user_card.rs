//! `udriver=e1000e@N`: the card is a userspace driver's
//! (`tools/eclipse-udrivers`, see [`crate::user`]), and the kernel does not
//! so much as reset it until that driver dies.

use alloc::sync::Arc;

use pci::PCIDevice;

use crate::builder::IoMapper;

/// The register BAR of an 82574 (datasheet §9.1.1.4: 128 KiB of memory).
const E1000E_BAR0_LEN: usize = 128 * 1024;

/// Whether the userspace e1000e driver can drive this part: the 82574L and
/// 82574LA, which is what QEMU's `e1000e` is. The PCH parts (I217 to I219)
/// need the PHY and flash work only the kernel's driver does.
fn user_driver_takes(device_id: u16) -> bool {
    matches!(device_id, 0x10d3 | 0x10f6)
}

/// Whether the machine has a virtio network card on PCI: the card the
/// stack process takes before any other.
fn virtio_net_on_the_bus() -> bool {
    use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
    // SAFETY: the configuration space the bus's own scan reads.
    unsafe { pci::scan_bus(&PortOpsImpl, PCI_ACCESS) }
        .any(|dev| dev.id.vendor_id == 0x1af4 && matches!(dev.id.device_id, 0x1000 | 0x1041))
}

/// Count this card among the e1000e's and answer whether the boot leaves
/// it to userspace, in which case it is recorded and the caller must not
/// touch it. Counted whether it can be handed over or not, so `@N` names
/// the same card either way.
pub(super) fn left(
    dev: &PCIDevice,
    mapper: &Option<Arc<dyn IoMapper>>,
    irq: Option<usize>,
    bar0: usize,
) -> bool {
    // Unasked, the card goes to userspace only if its driver there takes
    // it and no virtio card is the stack process's already: otherwise the
    // kernel drives it, as it always has, and says nothing of it.
    if crate::user::first_e1000e_unasked()
        && (!user_driver_takes(dev.id.device_id) || virtio_net_on_the_bus())
    {
        crate::user::keep_first_e1000e();
    }
    crate::user::reserve_if(
        crate::user::E1000E,
        user_driver_takes(dev.id.device_id),
        crate::user::Transport::Pci,
        bar0,
        E1000E_BAR0_LEN,
        || crate::user::Wiring {
            // The MSI that `bus::pci` armed; x86 MSI numbers sit 32 below
            // their vectors, which is what the APIC takes.
            irq: irq.map(|irq| {
                if cfg!(target_arch = "x86_64") {
                    irq + 32
                } else {
                    irq
                }
            }),
            // The kernel's own view of the registers, for the one thing it
            // does to the card: reset it when its driver dies.
            vaddr: mapper
                .as_ref()
                .and_then(|m| m.query_or_map(bar0, E1000E_BAR0_LEN)),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_82574_goes_to_the_userspace_driver() {
        assert!(user_driver_takes(0x10d3));
        assert!(user_driver_takes(0x10f6));
        // An I219-V: the kernel keeps it.
        assert!(!user_driver_takes(0x15b8));
    }
}
