//! The NVMe and AHCI controllers, which the kernel has no driver for: each
//! one is left to its driver in userspace (`tools/eclipse-udrivers`,
//! `udrv-nvme` and `udrv-ahci`), as a virtio network card is.
//!
//! Neither of these drives anything. They find the controller's register
//! window and interrupt, record them in [`crate::user`], and decline the
//! device, so the bus has nothing to register. What the kernel still does
//! to a controller is reset it when the process driving it dies, through the
//! window recorded here.
//!
//! The kernel's own NVMe and AHCI drivers lived in `nvme/` and `ata/`. They
//! went when every boot had a driver in userspace to take the controller:
//! `zircon.gateway=0`, the boot with no gateway for that driver, now has no
//! NVMe or SATA disk at all.

use alloc::sync::Arc;

use pci::{PCIDevice, BAR};

use crate::builder::IoMapper;
use crate::bus::pci_drivers::PciDriver;
use crate::{Device, DeviceError, DeviceResult};

/// The interrupt a userspace driver waits on: the MSI that `bus::pci`
/// armed. x86 MSI numbers sit 32 below their vectors, and the vector is
/// what the APIC's `register_handler` takes.
fn vector(irq: Option<usize>) -> Option<usize> {
    irq.map(|irq| {
        if cfg!(target_arch = "x86_64") {
            irq + 32
        } else {
            irq
        }
    })
}

/// An NVMe controller (class 01, subclass 08).
pub struct NvmeForUser;

impl PciDriver for NvmeForUser {
    fn name(&self) -> &str {
        "nvme (udriver)"
    }

    fn matched(&self, _vendor_id: u16, _device_id: u16) -> bool {
        false
    }

    fn matched_dev(&self, dev: &PCIDevice) -> bool {
        dev.id.class == 0x01 && dev.id.subclass == 0x08
    }

    fn init(
        &self,
        dev: &PCIDevice,
        mapper: &Option<Arc<dyn IoMapper>>,
        irq: Option<usize>,
    ) -> DeviceResult<Device> {
        // The registers are BAR 0. A controller without it is counted all
        // the same, so `nvme@N` names the same controller either way.
        let (addr, len) = match dev.bars[0] {
            Some(BAR::Memory(addr, len, _, _)) => (addr as usize, len as usize),
            _ => (0, 0),
        };
        crate::user::reserve_if(
            crate::user::NVME,
            addr != 0,
            crate::user::Transport::Pci,
            addr,
            len,
            || crate::user::Wiring {
                irq: vector(irq),
                // The kernel's own view of the registers, for the one thing
                // it does to the controller: reset it when its driver dies.
                vaddr: mapper.as_ref().and_then(|m| m.query_or_map(addr, len)),
            },
        );
        Err(DeviceError::NotSupported)
    }
}

/// An AHCI (SATA) controller, with every disk on its ports: class 01,
/// prog-if 01, and the SATA, IDE or RAID subclass.
pub struct AhciForUser;

impl PciDriver for AhciForUser {
    fn name(&self) -> &str {
        "ahci (udriver)"
    }

    fn matched(&self, _vendor_id: u16, _device_id: u16) -> bool {
        false
    }

    fn matched_dev(&self, dev: &PCIDevice) -> bool {
        dev.id.class == 0x01
            && dev.id.prog_if == 0x01
            && (dev.id.subclass == 0x06 || dev.id.subclass == 0x01 || dev.id.subclass == 0x04)
    }

    fn init(
        &self,
        dev: &PCIDevice,
        mapper: &Option<Arc<dyn IoMapper>>,
        irq: Option<usize>,
    ) -> DeviceResult<Device> {
        let (addr, len) = abar(dev);
        // At least the 32 KiB the driver reaches: the generic registers and
        // all 32 ports.
        let len = len.max(4096 * 8);
        if addr != 0 {
            // The driver needs the controller's memory decoding and its bus
            // mastering, and a firmware may have left either off.
            #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
            unsafe {
                let ops = &crate::bus::pci::PortOpsImpl;
                let am = crate::bus::pci::PCI_ACCESS;
                let command = am.read16(ops, dev.loc, 0x04);
                crate::bus::pci::cfgwrite::cfg_write16(ops, am, dev.loc, 0x04, command | 0x0007);
            }
        }
        crate::user::reserve_if(
            crate::user::AHCI,
            addr != 0,
            crate::user::Transport::Pci,
            addr,
            len,
            || crate::user::Wiring {
                irq: vector(irq),
                vaddr: Some(crate::bus::resolve_window(mapper, addr, len, 0)),
            },
        );
        Err(DeviceError::NotSupported)
    }
}

/// Where the AHCI registers (the ABAR) are: BAR 5. Some firmware (VirtualBox)
/// leaves the decoded `bars[5]` empty though the register holds the address,
/// and on x86 it is read straight from config space then.
fn abar(dev: &PCIDevice) -> (usize, usize) {
    if let Some(BAR::Memory(addr, len, _, _)) = dev.bars[5] {
        return (addr as usize, len as usize);
    }
    #[cfg(target_arch = "x86_64")]
    {
        use crate::bus::pci::{read_bar_addr, PortOpsImpl, BAR5_REG, PCI_ACCESS};
        let addr = unsafe { read_bar_addr(&PortOpsImpl, PCI_ACCESS, dev.loc, BAR5_REG) as usize };
        (addr, 0)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        (0, 0)
    }
}
