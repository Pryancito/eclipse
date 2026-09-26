use crate::{ZxError, ZxResult};

/// Returns the BDF address without the bottom two bits masked off.
pub fn pci_bdf_raw_addr(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    ((bus as u32 & 0xff) << 16)         // bits 23-16 bus
        | ((dev as u32 & 0x1f) << 11)   // bits 15-11 device
        | ((func as u32 & 0x7) << 8)    // bits 10-8 func
        | (offset as u32 & 0xff) // bits 7-2 reg, with bottom 2 bits as well
}

cfg_if::cfg_if! {
if #[cfg(all(target_arch = "x86_64", target_os = "none"))] {
    use kernel_hal::x86_64::{Io, Pmio};
    use lock::Mutex;

    static PIO_LOCK: Mutex<()> = Mutex::new(());
    const PCI_CONFIG_ADDR: u16 = 0xcf8;
    const PCI_CONFIG_DATA: u16 = 0xcfc;
    const PCI_CONFIG_ENABLE: u32 = 1 << 31;

    pub fn pmio_config_read_addr(addr: u32, width: usize) -> ZxResult<u32> {
        let mut port_cfg = Pmio::<u32>::new(PCI_CONFIG_ADDR);
        let port_data = Pmio::<u32>::new(PCI_CONFIG_DATA);

        let _lock = PIO_LOCK.lock();
        let shift = ((addr & 0x3) << 3) as usize;
        if shift + width > 32 {
            return Err(ZxError::INVALID_ARGS);
        }
        port_cfg.write((addr & !0x3) | PCI_CONFIG_ENABLE);
        let tmp_val = u32::from_le(port_data.read());
        // Drop the IRQ-off window before returning; callers that poll config
        // space in a tight loop (NVIDIA bring-up bridge walks) must also
        // pump, but a yield here covers single-shot readers that nest under
        // other spinlocks.
        drop(_lock);
        lock::pump();
        Ok((tmp_val >> shift) & (((1u64 << width) - 1) as u32))
    }
    pub fn pmio_config_write_addr(addr: u32, val: u32, width: usize) -> ZxResult {
        let mut port_cfg = Pmio::<u32>::new(PCI_CONFIG_ADDR);
        let mut port_data = Pmio::<u32>::new(PCI_CONFIG_DATA);

        let _lock = PIO_LOCK.lock();
        let shift = ((addr & 0x3) << 3) as usize;
        if shift + width > 32 {
            return Err(ZxError::INVALID_ARGS);
        }
        port_cfg.write((addr & !0x3) | PCI_CONFIG_ENABLE);
        let width_mask = ((1u64 << width) - 1) as u32;
        let val = val & width_mask;
        let tmp_val = if width < 32 {
            (u32::from_le(port_data.read()) & !(width_mask << shift)) | (val << shift)
        } else {
            val
        };
        port_data.write(u32::to_le(tmp_val));
        drop(_lock);
        lock::pump();
        Ok(())
    }
} else {
    pub fn pmio_config_read_addr(_addr: u32, _width: usize) -> ZxResult<u32> {
        Err(ZxError::NOT_SUPPORTED)
    }
    pub fn pmio_config_write_addr(_addr: u32, _val: u32, _width: usize) -> ZxResult {
        Err(ZxError::NOT_SUPPORTED)
    }
}
} // cfg_if!

/// Bounds for the `width` and `offset` of a `zx_pci_cfg_pio_rw`.
///
/// Both come from userspace and `sys_pci_cfg_pio_rw` passes them through
/// untouched, so this is the only place they are looked at. The only checking
/// that used to happen was `shift + width > 32`, deep inside
/// [`pmio_config_read_addr`], which let through two shapes the bus cannot
/// express:
///
/// * **A width that is not 8, 16 or 32.** A width of 0 read and wrote nothing
///   and reported success. A width of 5 built a 5-bit mask, and for a write
///   that is a read-modify-write of a dword of configuration space with five
///   of its bits replaced -- an access no PCI transaction can make, on a
///   register the caller never named.
/// * **An access that is not naturally aligned.** A 16-bit access at offset 1
///   shifts by 8 and fits in the dword, so it passed; but `CONFIG_DATA` is one
///   dword register and the bus has no way to say "the two bytes starting at
///   byte 1". `check_config_access` already requires natural alignment for the
///   MMIO configuration syscalls; this is the same rule for the port-I/O one.
fn check_pio_access(offset: u8, width: usize) -> ZxResult {
    if !matches!(width, 8 | 16 | 32) {
        return Err(ZxError::INVALID_ARGS);
    }
    if !(offset as usize).is_multiple_of(width / 8) {
        return Err(ZxError::INVALID_ARGS);
    }
    Ok(())
}

pub fn pio_config_read(bus: u8, dev: u8, func: u8, offset: u8, width: usize) -> ZxResult<u32> {
    check_pio_access(offset, width)?;
    pmio_config_read_addr(pci_bdf_raw_addr(bus, dev, func, offset), width)
}

pub fn pio_config_write(
    bus: u8,
    dev: u8,
    func: u8,
    offset: u8,
    val: u32,
    width: usize,
) -> ZxResult {
    check_pio_access(offset, width)?;
    pmio_config_write_addr(pci_bdf_raw_addr(bus, dev, func, offset), val, width)
}

#[cfg(test)]
mod pmio_tests {
    use super::*;

    #[test]
    fn a_bdf_address_puts_each_field_where_config_address_expects_it() {
        // The legacy 0xCF8 layout: bus 23-16, device 15-11, function 10-8,
        // register 7-2, and the bottom two bits kept so the accessors can work
        // out the shift inside the dword.
        assert_eq!(pci_bdf_raw_addr(0, 0, 0, 0), 0);
        assert_eq!(pci_bdf_raw_addr(1, 0, 0, 0), 0x0001_0000);
        assert_eq!(pci_bdf_raw_addr(0xFF, 0, 0, 0), 0x00FF_0000);
        assert_eq!(pci_bdf_raw_addr(0, 1, 0, 0), 0x0000_0800);
        assert_eq!(pci_bdf_raw_addr(0, 0x1F, 0, 0), 0x0000_F800);
        assert_eq!(pci_bdf_raw_addr(0, 0, 1, 0), 0x0000_0100);
        assert_eq!(pci_bdf_raw_addr(0, 0, 7, 0), 0x0000_0700);
        assert_eq!(pci_bdf_raw_addr(0, 0, 0, 0xFF), 0x0000_00FF);
        // All of them at once, with no field bleeding into another.
        assert_eq!(pci_bdf_raw_addr(0x12, 0x03, 0x05, 0x34), 0x0012_1D34);
    }

    #[test]
    fn a_device_or_function_number_that_cannot_exist_cannot_reach_another_one() {
        // Five bits of device and three of function: the numbers above them are
        // masked off rather than shifted into the neighbouring field, which
        // would otherwise turn device 32 into bus 1.
        assert_eq!(
            pci_bdf_raw_addr(0, 0x20, 0, 0),
            pci_bdf_raw_addr(0, 0, 0, 0)
        );
        assert_eq!(
            pci_bdf_raw_addr(0, 0xFF, 0, 0),
            pci_bdf_raw_addr(0, 0x1F, 0, 0)
        );
        assert_eq!(
            pci_bdf_raw_addr(0, 0, 0x08, 0),
            pci_bdf_raw_addr(0, 0, 0, 0)
        );
        assert_eq!(
            pci_bdf_raw_addr(0, 0, 0xFF, 0),
            pci_bdf_raw_addr(0, 0, 0x7, 0)
        );
    }

    #[test]
    fn a_width_the_bus_cannot_express_is_refused() {
        // `width` comes straight from `zx_pci_cfg_pio_rw` with nothing looking
        // at it. A width of 0 used to read and write nothing and report
        // success; a width of 5 built a five-bit mask, and for a write that is
        // a read-modify-write of a dword no PCI transaction can make.
        for width in [0usize, 1, 2, 4, 5, 7, 9, 24, 31, 33, 64, usize::MAX] {
            assert_eq!(
                pio_config_read(0, 0, 0, 0, width).err(),
                Some(ZxError::INVALID_ARGS),
                "read of {} bits",
                width
            );
            assert_eq!(
                pio_config_write(0, 0, 0, 0, 0, width).err(),
                Some(ZxError::INVALID_ARGS),
                "write of {} bits",
                width
            );
        }
    }

    #[test]
    fn an_access_that_is_not_naturally_aligned_is_refused() {
        // `CONFIG_DATA` is one dword register; the bus has no way to say "the
        // two bytes starting at byte 1". The only check there used to be was
        // `shift + width > 32`, which these all pass.
        for (offset, width) in [
            (1u8, 16usize),
            (3, 16),
            (1, 32),
            (2, 32),
            (0x35, 16),
            (0x3D, 32),
        ] {
            assert_eq!(
                pio_config_read(0, 0, 0, offset, width).err(),
                Some(ZxError::INVALID_ARGS),
                "{} bits at {:#x}",
                width,
                offset
            );
            assert_eq!(
                pio_config_write(0, 0, 0, offset, 0, width).err(),
                Some(ZxError::INVALID_ARGS),
                "{} bits at {:#x}",
                width,
                offset
            );
        }
    }

    #[test]
    fn an_access_the_bus_can_express_gets_past_the_bounds_check() {
        // Naturally aligned and a real width: rejected only because a test
        // binary is never `target_os = "none"`, so there is no port I/O here at
        // all. What matters is that the answer is not `INVALID_ARGS`.
        for (offset, width) in [
            (0u8, 8usize),
            (1, 8),
            (0xFF, 8),
            (0, 16),
            (2, 16),
            (0xFE, 16),
            (0, 32),
            (4, 32),
            (0xFC, 32),
        ] {
            assert_eq!(
                pio_config_read(0, 0, 0, offset, width).err(),
                Some(ZxError::NOT_SUPPORTED),
                "{} bits at {:#x}",
                width,
                offset
            );
            assert_eq!(
                pio_config_write(0, 0, 0, offset, 0, width).err(),
                Some(ZxError::NOT_SUPPORTED),
                "{} bits at {:#x}",
                width,
                offset
            );
        }
    }
}
