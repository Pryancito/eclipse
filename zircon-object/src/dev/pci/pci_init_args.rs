//! `sys_pci_init` args.
//!
//! reference: zircon/system/public/zircon/syscalls/pci.h

use super::constants::*;
use crate::{ZxError, ZxResult};
use kernel_hal::{
    drivers::prelude::{IrqPolarity, IrqTriggerMode},
    interrupt,
};

#[repr(transparent)]
#[derive(Clone, Copy, Debug)]
pub struct PciIrqSwizzleLut(
    [[[u32; PCI_MAX_LEGACY_IRQ_PINS]; PCI_MAX_FUNCTIONS_PER_DEVICE]; PCI_MAX_DEVICES_PER_BUS],
);

#[repr(C)]
#[derive(Debug)]
pub struct PciInitArgsIrqs {
    pub global_irq: u32,
    pub level_triggered: bool,
    pub active_high: bool,
    pub padding1: [u8; 2],
}

#[repr(C)]
#[derive(Debug)]
pub struct PciInitArgsHeader {
    pub dev_pin_to_global_irq: PciIrqSwizzleLut,
    pub num_irqs: u32,
    pub irqs: [PciInitArgsIrqs; PCI_MAX_IRQS],
    pub addr_window_count: u32,
}

#[repr(C)]
#[derive(Debug)]
pub struct PciInitArgsAddrWindows {
    pub base: u64,
    pub size: usize,
    pub bus_start: u8,
    pub bus_end: u8,
    pub cfg_space_type: u8,
    pub has_ecam: bool,
    pub padding1: [u8; 4],
}

pub const PCI_INIT_ARG_MAX_ECAM_WINDOWS: usize = 2;
pub const PCI_INIT_ARG_MAX_SIZE: usize = core::mem::size_of::<PciInitArgsAddrWindows>()
    * PCI_INIT_ARG_MAX_ECAM_WINDOWS
    + core::mem::size_of::<PciInitArgsHeader>();

impl PciInitArgsHeader {
    pub fn configure_interrupt(&mut self) -> ZxResult {
        for i in 0..self.num_irqs as usize {
            let irq = &mut self.irqs[i];
            let global_irq = irq.global_irq;
            if !interrupt::is_valid_irq(global_irq as usize) {
                irq.global_irq = PCI_NO_IRQ_MAPPING;
                self.dev_pin_to_global_irq.remove_irq(global_irq);
            } else {
                let tm = if irq.level_triggered {
                    IrqTriggerMode::Level
                } else {
                    IrqTriggerMode::Edge
                };
                let pol = if irq.active_high {
                    IrqPolarity::ActiveHigh
                } else {
                    IrqPolarity::ActiveLow
                };
                interrupt::configure_irq(global_irq as usize, tm, pol)
                    .map_err(|_| ZxError::INVALID_ARGS)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
impl PciIrqSwizzleLut {
    /// A table of zeros, for the tests of the bus driver: every pin of every
    /// function mapped to global IRQ 0. The tuple field is private, and
    /// `PCIeBusDriver::add_root` takes one of these by value, so without this
    /// the bus driver's own module cannot build a root to add.
    pub(super) fn zeroed() -> Self {
        Self(
            [[[0; PCI_MAX_LEGACY_IRQ_PINS]; PCI_MAX_FUNCTIONS_PER_DEVICE]; PCI_MAX_DEVICES_PER_BUS],
        )
    }
}

impl PciIrqSwizzleLut {
    pub(super) fn swizzle(&self, dev_id: usize, func_id: usize, pin: usize) -> ZxResult<usize> {
        if dev_id >= PCI_MAX_DEVICES_PER_BUS
            || func_id >= PCI_MAX_FUNCTIONS_PER_DEVICE
            || pin >= PCI_MAX_LEGACY_IRQ_PINS
        {
            return Err(ZxError::INVALID_ARGS);
        }
        let irq = self.0[dev_id][func_id][pin];
        if irq == PCI_NO_IRQ_MAPPING {
            Err(ZxError::NOT_FOUND)
        } else {
            Ok(irq as usize)
        }
    }

    fn remove_irq(&mut self, irq: u32) {
        for dev in self.0.iter_mut() {
            for func in dev.iter_mut() {
                for pin in func.iter_mut() {
                    if *pin == irq {
                        *pin = PCI_NO_IRQ_MAPPING;
                    }
                }
            }
        }
    }
}
#[cfg(test)]
mod swizzle_tests {
    use super::*;

    /// Index of the slot `(dev, func, pin)` names, counted across the whole
    /// table. Used as that slot's global IRQ below, so a swizzle that reads a
    /// neighbour's slot cannot answer right by accident.
    fn slot(dev: usize, func: usize, pin: usize) -> u32 {
        ((dev * PCI_MAX_FUNCTIONS_PER_DEVICE + func) * PCI_MAX_LEGACY_IRQ_PINS + pin) as u32
    }

    /// A table whose every pin is mapped to the IRQ `slot` gives it.
    fn numbered() -> PciIrqSwizzleLut {
        let mut lut = PciIrqSwizzleLut::zeroed();
        for dev in 0..PCI_MAX_DEVICES_PER_BUS {
            for func in 0..PCI_MAX_FUNCTIONS_PER_DEVICE {
                for pin in 0..PCI_MAX_LEGACY_IRQ_PINS {
                    lut.0[dev][func][pin] = slot(dev, func, pin);
                }
            }
        }
        lut
    }

    /// Every pin of every function of every device reads its own slot. This
    /// is how the bus driver turns a legacy INTx pin into the global IRQ line
    /// the platform wired it to, so reading a neighbour's slot points a
    /// device's interrupt handler at another device's line.
    #[test]
    fn each_pin_of_each_function_reads_its_own_slot() {
        let lut = numbered();
        for dev in 0..PCI_MAX_DEVICES_PER_BUS {
            for func in 0..PCI_MAX_FUNCTIONS_PER_DEVICE {
                for pin in 0..PCI_MAX_LEGACY_IRQ_PINS {
                    assert_eq!(
                        lut.swizzle(dev, func, pin),
                        Ok(slot(dev, func, pin) as usize),
                        "{:02x}.{}, pin {}",
                        dev,
                        func,
                        pin
                    );
                }
            }
        }
    }

    /// The three bounds are the three dimensions of the table, and the last
    /// index of each one is inside it. The table is indexed raw right after
    /// this check, so an off-by-one here is a kernel read past the end of the
    /// array with a device number userspace chose.
    #[test]
    fn a_coordinate_outside_the_table_is_refused_and_the_last_one_is_not() {
        let lut = numbered();
        let last_dev = PCI_MAX_DEVICES_PER_BUS - 1;
        let last_func = PCI_MAX_FUNCTIONS_PER_DEVICE - 1;
        let last_pin = PCI_MAX_LEGACY_IRQ_PINS - 1;
        assert_eq!(
            lut.swizzle(last_dev, last_func, last_pin),
            Ok(slot(last_dev, last_func, last_pin) as usize)
        );
        for bad in [
            (PCI_MAX_DEVICES_PER_BUS, last_func, last_pin),
            (last_dev, PCI_MAX_FUNCTIONS_PER_DEVICE, last_pin),
            (last_dev, last_func, PCI_MAX_LEGACY_IRQ_PINS),
            (usize::MAX, 0, 0),
            (0, usize::MAX, 0),
            (0, 0, usize::MAX),
        ] {
            assert_eq!(
                lut.swizzle(bad.0, bad.1, bad.2),
                Err(ZxError::INVALID_ARGS),
                "{:?}",
                bad
            );
        }
    }

    /// A pin the platform mapped nowhere answers NOT_FOUND, told apart from a
    /// coordinate that names no pin at all. The two cannot share an answer:
    /// the bus driver falls back to a different interrupt for the first and
    /// has a bug for the second.
    #[test]
    fn an_unmapped_pin_is_not_found_and_not_the_sentinel_as_a_number() {
        let mut lut = numbered();
        lut.0[3][2][1] = PCI_NO_IRQ_MAPPING;
        assert_eq!(lut.swizzle(3, 2, 1), Err(ZxError::NOT_FOUND));
        // Its neighbours are untouched, and IRQ zero is a line like any
        // other, not a way of saying "no line".
        assert_eq!(lut.swizzle(3, 2, 0), Ok(slot(3, 2, 0) as usize));
        assert_eq!(lut.swizzle(0, 0, 0), Ok(0));
    }

    /// Dropping an IRQ clears every slot that names it, leaves the rest
    /// alone, and clears them to the sentinel rather than to zero. Zero is a
    /// valid global IRQ, so clearing to it would leave every pin that lost
    /// its line reading as mapped to the first one.
    #[test]
    fn removing_an_irq_clears_every_slot_that_names_it_to_the_sentinel() {
        let mut lut = PciIrqSwizzleLut::zeroed();
        lut.0[0][0][0] = 9;
        lut.0[PCI_MAX_DEVICES_PER_BUS - 1][PCI_MAX_FUNCTIONS_PER_DEVICE - 1]
            [PCI_MAX_LEGACY_IRQ_PINS - 1] = 9;
        lut.0[1][1][1] = 10;

        lut.remove_irq(9);

        assert_eq!(lut.swizzle(0, 0, 0), Err(ZxError::NOT_FOUND));
        assert_eq!(
            lut.swizzle(
                PCI_MAX_DEVICES_PER_BUS - 1,
                PCI_MAX_FUNCTIONS_PER_DEVICE - 1,
                PCI_MAX_LEGACY_IRQ_PINS - 1
            ),
            Err(ZxError::NOT_FOUND),
            "the last slot of the table was not swept"
        );
        assert_eq!(
            lut.swizzle(1, 1, 1),
            Ok(10),
            "another IRQ was taken with it"
        );
        assert_eq!(lut.swizzle(2, 2, 2), Ok(0), "the zeros are IRQ zero");

        // And removing an IRQ nothing maps to leaves the table as it was.
        lut.remove_irq(11);
        assert_eq!(lut.swizzle(1, 1, 1), Ok(10));
        assert_eq!(lut.swizzle(2, 2, 2), Ok(0));
    }
}

#[cfg(test)]
mod abi_tests {
    use super::*;

    /// `zx_pci_init_arg_t`. `sys_pci_init` reads this straight out of a
    /// userspace buffer and bounds that read with `PCI_INIT_ARG_MAX_SIZE`,
    /// which is derived from these sizes rather than written down twice -- so
    /// a field that grows moves the limit with it, and these are the numbers
    /// that say what userspace has to send.
    #[test]
    fn the_init_args_are_the_sizes_the_syscall_reads_from_userspace() {
        assert_eq!(core::mem::size_of::<PciInitArgsIrqs>(), 8);
        assert_eq!(core::mem::align_of::<PciInitArgsIrqs>(), 4);
        assert_eq!(
            core::mem::size_of::<PciIrqSwizzleLut>(),
            PCI_MAX_DEVICES_PER_BUS * PCI_MAX_FUNCTIONS_PER_DEVICE * PCI_MAX_LEGACY_IRQ_PINS * 4
        );
        assert_eq!(core::mem::size_of::<PciInitArgsAddrWindows>(), 24);
        assert_eq!(core::mem::align_of::<PciInitArgsAddrWindows>(), 8);

        let header = core::mem::size_of::<PciInitArgsHeader>();
        assert_eq!(
            header,
            core::mem::size_of::<PciIrqSwizzleLut>() + 4 + PCI_MAX_IRQS * 8 + 4
        );
        assert_eq!(PCI_INIT_ARG_MAX_ECAM_WINDOWS, 2);
        assert_eq!(
            PCI_INIT_ARG_MAX_SIZE,
            header + PCI_INIT_ARG_MAX_ECAM_WINDOWS * 24
        );
    }

    /// Where each field sits, which is what makes the two reads
    /// `sys_pci_init` does line up: it reads the header, then the address
    /// windows from `init_buf + size_of::<PciInitArgsHeader>()`. Two fields
    /// that swapped places would keep the size and the padding and still
    /// compile.
    #[test]
    fn each_field_of_the_init_args_is_where_the_abi_puts_it() {
        assert_eq!(
            core::mem::offset_of!(PciInitArgsHeader, dev_pin_to_global_irq),
            0
        );
        let lut = core::mem::size_of::<PciIrqSwizzleLut>();
        assert_eq!(core::mem::offset_of!(PciInitArgsHeader, num_irqs), lut);
        assert_eq!(core::mem::offset_of!(PciInitArgsHeader, irqs), lut + 4);
        assert_eq!(
            core::mem::offset_of!(PciInitArgsHeader, addr_window_count),
            lut + 4 + PCI_MAX_IRQS * 8,
            "the window count comes after the IRQ array, not before it"
        );

        assert_eq!(core::mem::offset_of!(PciInitArgsIrqs, global_irq), 0);
        assert_eq!(core::mem::offset_of!(PciInitArgsIrqs, level_triggered), 4);
        assert_eq!(core::mem::offset_of!(PciInitArgsIrqs, active_high), 5);
        assert_eq!(core::mem::offset_of!(PciInitArgsIrqs, padding1), 6);

        assert_eq!(core::mem::offset_of!(PciInitArgsAddrWindows, base), 0);
        assert_eq!(core::mem::offset_of!(PciInitArgsAddrWindows, size), 8);
        assert_eq!(core::mem::offset_of!(PciInitArgsAddrWindows, bus_start), 16);
        assert_eq!(core::mem::offset_of!(PciInitArgsAddrWindows, bus_end), 17);
        assert_eq!(
            core::mem::offset_of!(PciInitArgsAddrWindows, cfg_space_type),
            18
        );
        assert_eq!(core::mem::offset_of!(PciInitArgsAddrWindows, has_ecam), 19);
        assert_eq!(core::mem::offset_of!(PciInitArgsAddrWindows, padding1), 20);
    }
}

#[cfg(test)]
mod configure_tests {
    use super::*;

    /// A header that declares no IRQs asks the platform for nothing, and that
    /// is the only path through `configure_interrupt` a host test can take:
    /// every other one reaches `is_valid_irq` and `configure_irq`, which the
    /// hosted HAL does not implement, so the loop body panics here whatever
    /// the table says.
    ///
    /// It is still the path worth pinning. `num_irqs` is a field userspace
    /// fills in and it bounds a loop over a fixed array, so a loop that runs
    /// one time too many reads an entry nobody wrote and hands it to the
    /// platform as an interrupt to configure.
    #[test]
    fn a_header_that_declares_no_irqs_asks_the_platform_for_nothing() {
        let mut args = PciInitArgsHeader {
            dev_pin_to_global_irq: PciIrqSwizzleLut::zeroed(),
            num_irqs: 0,
            irqs: core::array::from_fn(|_| PciInitArgsIrqs {
                global_irq: PCI_NO_IRQ_MAPPING,
                level_triggered: false,
                active_high: false,
                padding1: [0; 2],
            }),
            addr_window_count: 1,
        };
        assert_eq!(args.configure_interrupt(), Ok(()));
    }
}
