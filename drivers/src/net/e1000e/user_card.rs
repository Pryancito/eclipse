//! `udriver=e1000e@N`: the card is a userspace driver's
//! (`tools/eclipse-udrivers`, see [`crate::user`]), and the kernel does not
//! so much as reset it until that driver dies.

use alloc::sync::Arc;

use pci::PCIDevice;

use crate::builder::IoMapper;

/// The register BAR of an 82574 (datasheet §9.1.1.4: 128 KiB of memory),
/// and of the PCH parts, whose flash window sits at 0xE000 in it.
const E1000E_BAR0_LEN: usize = 128 * 1024;

/// Whether the userspace e1000e driver can drive this part when the boot
/// names it (`udriver=e1000e@N`): every part this kernel's driver takes,
/// the 82574/82583 and the PCH ones (82579, I217, I218, I219), which it
/// brings up as this driver does (ULP, LANPHYPC, K1, the PHY's LPLU bits,
/// the NVM in the flash window).
fn user_driver_takes(device_id: u16) -> bool {
    super::e1000e_device_matched(device_id) || userspace_only(device_id)
}

/// The parts only the userspace driver drives, which this kernel's e1000e
/// does not: the copper 82571, 82572 and 82573 (Linux `82571.c`), and the
/// MACs of the ICH8, ICH9 and ICH10 chipsets and of the first PCH, with
/// an 82577 or 82578 PHY (`ich8lan.c`). They are
/// matched here so that they are counted and left to it; with no driver
/// in userspace for them (a second card, `unetcard=kernel`) nobody drives
/// them, as before.
pub(super) fn userspace_only(device_id: u16) -> bool {
    matches!(
        device_id,
        0x105e | 0x10a4 | 0x10bc | 0x10d5 | 0x107d | 0x10b9 | 0x108b | 0x108c | 0x109a
            // ICH8.
            | 0x1049 | 0x104a | 0x104b | 0x104c | 0x104d | 0x10c4 | 0x10c5
            // ICH9.
            | 0x10bd | 0x294c | 0x10bf | 0x10f5 | 0x10cb | 0x10c0 | 0x10c3 | 0x10c2 | 0x10e5
            // ICH10.
            | 0x10cc | 0x10cd | 0x10ce | 0x10de | 0x10df | 0x1525
            // The first PCH: 82577 and 82578.
            | 0x10ea | 0x10eb | 0x10ef | 0x10f0
            // 80003ES2LAN copper.
            | 0x1096 | 0x10ba
    )
}

/// Whether it goes to userspace with no word on the command line: every
/// part its driver there takes. The PCH parts and the 82583 have only been
/// run against a pretend card (QEMU has none of them), so a boot that
/// finds that driver wanting goes back with `unetcard=kernel`: the kernel
/// drives the card and lends it to the stack process, as before.
fn user_driver_unasked(device_id: u16) -> bool {
    user_driver_takes(device_id)
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
        && (!user_driver_unasked(dev.id.device_id) || virtio_net_on_the_bus())
    {
        crate::user::keep_first_e1000e();
    }
    crate::user::reserve_pci_if(
        crate::user::E1000E,
        user_driver_takes(dev.id.device_id),
        (dev.id.vendor_id as u32) << 16 | dev.id.device_id as u32,
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
    fn every_part_the_userspace_driver_takes_goes_to_it_unasked() {
        for id in [0x10d3, 0x10f6, 0x150c, 0x15b8, 0x153a, 0x1502] {
            assert!(user_driver_unasked(id), "{:#x}", id);
        }
        assert!(!user_driver_unasked(0x1533));
    }

    #[test]
    fn asked_for_every_part_the_kernel_driver_takes_goes() {
        for id in [
            0x10d3, 0x10f6, 0x150c, 0x1502, 0x153a, 0x15a1, 0x1570, 0x15b8, 0x0d4f, 0x550b,
        ] {
            assert!(user_driver_takes(id), "{:#x}", id);
        }
        // The copper 82571, 82572, 82573 and 80003ES2LAN and the ICHs:
        // the userspace driver's only.
        for id in [
            0x105e, 0x10b9, 0x108c, 0x1049, 0x10f5, 0x1525, 0x10ea, 0x10f0, 0x1096, 0x10ba,
        ] {
            assert!(user_driver_takes(id) && userspace_only(id), "{:#x}", id);
        }
        assert!(!userspace_only(0x15b8));
        // The 80003ES2LAN's SerDes ports: no PHY to bring up.
        assert!(!user_driver_takes(0x1098) && !user_driver_takes(0x10bb));
        // An I210 (igb): no driver of ours.
        assert!(!user_driver_takes(0x1533));
    }
}
