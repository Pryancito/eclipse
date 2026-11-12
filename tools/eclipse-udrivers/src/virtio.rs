//! Bringing a virtio device up, over either transport the kernel hands out:
//! legacy virtio-mmio (aarch64 and riscv64 under QEMU `virt`) or modern
//! virtio-pci (x86_64 q35, MSI-X vector 0).

use core::ptr;

use crate::queue;
use crate::PAGE;

/// Device ids (virtio 1.1 §5).
pub const ID_NET: u32 = 1;
pub const ID_BLOCK: u32 = 2;

// Device status bits (virtio 1.1 §2.1).
const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;

// Legacy virtio-mmio registers (virtio 1.1 §4.2.4).
const MMIO_MAGIC: u32 = 0x000;
const MMIO_VERSION: u32 = 0x004;
const MMIO_DEVICE_ID: u32 = 0x008;
const MMIO_HOST_FEATURES: u32 = 0x010;
const MMIO_GUEST_FEATURES: u32 = 0x020;
const MMIO_GUEST_PAGE_SIZE: u32 = 0x028;
const MMIO_QUEUE_SEL: u32 = 0x030;
const MMIO_QUEUE_NUM_MAX: u32 = 0x034;
const MMIO_QUEUE_NUM: u32 = 0x038;
const MMIO_QUEUE_ALIGN: u32 = 0x03c;
const MMIO_QUEUE_PFN: u32 = 0x040;
const MMIO_QUEUE_NOTIFY: u32 = 0x050;
const MMIO_INTERRUPT_STATUS: u32 = 0x060;
const MMIO_INTERRUPT_ACK: u32 = 0x064;
const MMIO_STATUS: u32 = 0x070;
const MMIO_CONFIG: u32 = 0x100;
const MMIO_MAGIC_VALUE: u32 = 0x7472_6976; // "virt"

// Modern virtio-pci common configuration (virtio 1.1 §4.1.4.3).
const PCI_DEVICE_FEATURE_SELECT: u32 = 0x00;
const PCI_DEVICE_FEATURE: u32 = 0x04;
const PCI_DRIVER_FEATURE_SELECT: u32 = 0x08;
const PCI_DRIVER_FEATURE: u32 = 0x0c;
const PCI_CONFIG_MSIX_VECTOR: u32 = 0x10;
const PCI_DEVICE_STATUS: u32 = 0x14;
const PCI_QUEUE_SELECT: u32 = 0x16;
const PCI_QUEUE_SIZE: u32 = 0x18;
const PCI_QUEUE_MSIX_VECTOR: u32 = 0x1a;
const PCI_QUEUE_ENABLE: u32 = 0x1c;
const PCI_QUEUE_NOTIFY_OFF: u32 = 0x1e;
const PCI_QUEUE_DESC: u32 = 0x20;
const PCI_QUEUE_DRIVER: u32 = 0x28;
const PCI_QUEUE_DEVICE: u32 = 0x30;
/// VIRTIO_F_VERSION_1 is feature bit 32: bit 0 of feature word 1.
const F_VERSION_1_HI: u32 = 1;
const MSI_NO_VECTOR: u16 = 0xffff;

/// How the device is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Legacy virtio-mmio: everything in the one register window.
    Mmio,
    /// Modern virtio-pci: where in the window (a memory BAR) the common,
    /// notify and device structures are, and `notify_off_multiplier`.
    Pci {
        common: u32,
        notify: u32,
        device: u32,
        notify_multiplier: u32,
    },
}

/// What a driver asks of [`Device::bring_up`].
pub struct Setup<'a> {
    /// The device id the transport must report (checked on virtio-mmio; a
    /// PCI device was matched by the kernel before it was handed out).
    pub device_id: u32,
    /// Feature bits (of the low word) to take when the device offers them.
    pub want: u32,
    /// Feature bits the device must offer.
    pub need: u32,
    /// Descriptors per queue.
    pub queue_size: u16,
    /// The device address of each queue's two pages (see [`queue`]), and
    /// whether the queue raises the interrupt (MSI-X vector 0 on PCI, the
    /// one the kernel armed; virtio-mmio has a single line for all).
    pub queues: &'a [(u64, bool)],
}

/// What [`Device::bring_up`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Up {
    /// The feature bits (low word) agreed on.
    pub features: u32,
    /// The device refused MSI-X vector 0 for a queue that asked for it:
    /// there is no interrupt to wait on.
    pub irq_refused: bool,
}

/// A virtio device's registers.
pub struct Device {
    regs: *mut u8,
    transport: Transport,
    /// Where each queue is notified, once the device is up.
    notify: Vec<*mut u8>,
}

impl Device {
    /// The device whose register window starts at `regs`.
    ///
    /// # Safety
    /// `regs` is that window, mapped for as long as the `Device` lives, and
    /// `transport` is what it holds.
    pub unsafe fn new(regs: *mut u8, transport: Transport) -> Device {
        Device {
            regs,
            transport,
            notify: Vec::new(),
        }
    }

    pub fn transport(&self) -> Transport {
        self.transport
    }

    fn rd32(&self, off: u32) -> u32 {
        // SAFETY: a register of the window (`new`).
        unsafe { ptr::read_volatile(self.regs.add(off as usize) as *const u32) }
    }
    fn rd16(&self, off: u32) -> u16 {
        // SAFETY: as `rd32`.
        unsafe { ptr::read_volatile(self.regs.add(off as usize) as *const u16) }
    }
    fn rd8(&self, off: u32) -> u8 {
        // SAFETY: as `rd32`.
        unsafe { ptr::read_volatile(self.regs.add(off as usize)) }
    }
    fn wr32(&self, off: u32, v: u32) {
        // SAFETY: as `rd32`.
        unsafe { ptr::write_volatile(self.regs.add(off as usize) as *mut u32, v) }
    }
    fn wr16(&self, off: u32, v: u16) {
        // SAFETY: as `rd32`.
        unsafe { ptr::write_volatile(self.regs.add(off as usize) as *mut u16, v) }
    }
    fn wr8(&self, off: u32, v: u8) {
        // SAFETY: as `rd32`.
        unsafe { ptr::write_volatile(self.regs.add(off as usize), v) }
    }

    /// Reset the device, agree on features and give it its queues. The
    /// device is not running yet: read what the driver needs of its
    /// configuration, then [`Self::driver_ok`].
    pub fn bring_up(&mut self, setup: &Setup) -> Result<Up, String> {
        match self.transport {
            Transport::Mmio => self.bring_up_mmio(setup),
            Transport::Pci {
                common,
                notify,
                notify_multiplier,
                ..
            } => self.bring_up_pci(setup, common, notify, notify_multiplier),
        }
    }

    fn bring_up_mmio(&mut self, setup: &Setup) -> Result<Up, String> {
        if self.rd32(MMIO_MAGIC) != MMIO_MAGIC_VALUE || self.rd32(MMIO_VERSION) != 1 {
            return Err(format!(
                "not a legacy virtio-mmio device, version {}",
                self.rd32(MMIO_VERSION)
            ));
        }
        if self.rd32(MMIO_DEVICE_ID) != setup.device_id {
            return Err(format!(
                "not the device expected: virtio id {}",
                self.rd32(MMIO_DEVICE_ID)
            ));
        }
        self.wr32(MMIO_STATUS, 0);
        self.wr32(MMIO_STATUS, (STATUS_ACKNOWLEDGE | STATUS_DRIVER) as u32);
        let offered = self.rd32(MMIO_HOST_FEATURES);
        let features = agree(offered, setup)?;
        self.wr32(MMIO_GUEST_FEATURES, features);
        self.wr32(MMIO_GUEST_PAGE_SIZE, PAGE as u32);
        self.notify.clear();
        for (q, &(at, _)) in setup.queues.iter().enumerate() {
            self.wr32(MMIO_QUEUE_SEL, q as u32);
            let max = self.rd32(MMIO_QUEUE_NUM_MAX);
            if max < setup.queue_size as u32 {
                return Err(format!("queue too small: {max}"));
            }
            self.wr32(MMIO_QUEUE_NUM, setup.queue_size as u32);
            self.wr32(MMIO_QUEUE_ALIGN, PAGE as u32);
            self.wr32(MMIO_QUEUE_PFN, (at / PAGE as u64) as u32);
            // SAFETY: a register of the window.
            self.notify
                .push(unsafe { self.regs.add(MMIO_QUEUE_NOTIFY as usize) });
        }
        Ok(Up {
            features,
            irq_refused: false,
        })
    }

    fn bring_up_pci(
        &mut self,
        setup: &Setup,
        c: u32,
        notify: u32,
        multiplier: u32,
    ) -> Result<Up, String> {
        self.wr8(c + PCI_DEVICE_STATUS, 0);
        while self.rd8(c + PCI_DEVICE_STATUS) != 0 {
            core::hint::spin_loop();
        }
        self.wr8(c + PCI_DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
        self.wr32(c + PCI_DEVICE_FEATURE_SELECT, 1);
        if self.rd32(c + PCI_DEVICE_FEATURE) & F_VERSION_1_HI == 0 {
            return Err("device without VIRTIO_F_VERSION_1".into());
        }
        self.wr32(c + PCI_DEVICE_FEATURE_SELECT, 0);
        let offered = self.rd32(c + PCI_DEVICE_FEATURE);
        let features = agree(offered, setup)?;
        self.wr32(c + PCI_DRIVER_FEATURE_SELECT, 0);
        self.wr32(c + PCI_DRIVER_FEATURE, features);
        self.wr32(c + PCI_DRIVER_FEATURE_SELECT, 1);
        self.wr32(c + PCI_DRIVER_FEATURE, F_VERSION_1_HI);
        self.wr8(
            c + PCI_DEVICE_STATUS,
            STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK,
        );
        if self.rd8(c + PCI_DEVICE_STATUS) & STATUS_FEATURES_OK == 0 {
            return Err(format!(
                "device refused the features: status {}",
                self.rd8(c + PCI_DEVICE_STATUS)
            ));
        }
        // No interrupt for configuration changes: nothing here reads them.
        self.wr16(c + PCI_CONFIG_MSIX_VECTOR, MSI_NO_VECTOR);
        let mut irq_refused = false;
        self.notify.clear();
        for (q, &(at, irq)) in setup.queues.iter().enumerate() {
            self.wr16(c + PCI_QUEUE_SELECT, q as u16);
            let max = self.rd16(c + PCI_QUEUE_SIZE);
            if max < setup.queue_size {
                return Err(format!("queue too small: {max}"));
            }
            self.wr16(c + PCI_QUEUE_SIZE, setup.queue_size);
            // MSI-X vector 0 is the one the kernel armed, and the one the
            // claim's interrupt object listens on. The device answers
            // NO_VECTOR when it cannot take it.
            self.wr16(
                c + PCI_QUEUE_MSIX_VECTOR,
                if irq { 0 } else { MSI_NO_VECTOR },
            );
            if irq && self.rd16(c + PCI_QUEUE_MSIX_VECTOR) != 0 {
                irq_refused = true;
            }
            let desc = at;
            let driver = at + queue::avail_offset(setup.queue_size) as u64;
            let device = at + queue::USED_OFFSET as u64;
            for (reg, addr) in [
                (PCI_QUEUE_DESC, desc),
                (PCI_QUEUE_DRIVER, driver),
                (PCI_QUEUE_DEVICE, device),
            ] {
                self.wr32(c + reg, addr as u32);
                self.wr32(c + reg + 4, (addr >> 32) as u32);
            }
            let notify_off = self.rd16(c + PCI_QUEUE_NOTIFY_OFF);
            self.wr16(c + PCI_QUEUE_ENABLE, 1);
            // SAFETY: the queue's notify register, inside the window.
            self.notify.push(unsafe {
                self.regs
                    .add(notify as usize + notify_off as usize * multiplier as usize)
            });
        }
        Ok(Up {
            features,
            irq_refused,
        })
    }

    /// Byte `off` of the device's own configuration.
    pub fn config8(&self, off: u32) -> u8 {
        self.rd8(self.config_base() + off)
    }

    /// The 32-bit word at `off` of the device's own configuration.
    pub fn config32(&self, off: u32) -> u32 {
        self.rd32(self.config_base() + off)
    }

    fn config_base(&self) -> u32 {
        match self.transport {
            Transport::Mmio => MMIO_CONFIG,
            Transport::Pci { device, .. } => device,
        }
    }

    /// The driver is ready: the device starts.
    pub fn driver_ok(&mut self) {
        match self.transport {
            Transport::Mmio => self.wr32(
                MMIO_STATUS,
                (STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK) as u32,
            ),
            Transport::Pci { common, .. } => self.wr8(
                common + PCI_DEVICE_STATUS,
                STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
            ),
        }
    }

    /// Tell the device there is something new in queue `q`.
    pub fn notify(&self, q: usize) {
        let reg = self.notify[q];
        // virtio-mmio registers take 32-bit accesses only (QEMU drops a
        // 16-bit store to QueueNotify); the PCI notify register is 16 bits
        // wide.
        // SAFETY: the register `bring_up` found for this queue.
        unsafe {
            match self.transport {
                Transport::Mmio => ptr::write_volatile(reg as *mut u32, q as u32),
                Transport::Pci { .. } => ptr::write_volatile(reg as *mut u16, q as u16),
            }
        }
    }

    /// Quiet the device's interrupt. virtio-mmio holds its line up until the
    /// status is acked, and the kernel unmasks the line on the next wait; an
    /// MSI-X message needs nothing.
    pub fn ack(&self) {
        if self.transport == Transport::Mmio {
            let isr = self.rd32(MMIO_INTERRUPT_STATUS);
            if isr != 0 {
                self.wr32(MMIO_INTERRUPT_ACK, isr);
            }
        }
    }
}

/// The feature bits to take of those `offered`.
fn agree(offered: u32, setup: &Setup) -> Result<u32, String> {
    if offered & setup.need != setup.need {
        return Err(format!(
            "the device lacks a feature the driver needs: offered {offered:#x}, needed {:#x}",
            setup.need
        ));
    }
    Ok(offered & (setup.want | setup.need))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::tests::Pages;

    fn setup(queues: &[(u64, bool)]) -> Setup<'_> {
        Setup {
            device_id: ID_BLOCK,
            want: 1 << 9,
            need: 0,
            queue_size: 64,
            queues,
        }
    }

    fn rd32(pages: &Pages, off: u32) -> u32 {
        let b = &pages.bytes()[off as usize..off as usize + 4];
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }

    /// A window that reads like a legacy virtio-mmio block device at rest.
    /// Memory is no device (nothing reacts to a write), but what the driver
    /// leaves in the registers is what a device would have been told.
    fn mmio_window(features: u32) -> Pages {
        let mut pages = Pages::new(1);
        let b = pages.bytes_mut();
        b[0..4].copy_from_slice(&MMIO_MAGIC_VALUE.to_le_bytes());
        b[4..8].copy_from_slice(&1u32.to_le_bytes());
        b[8..12].copy_from_slice(&ID_BLOCK.to_le_bytes());
        b[0x10..0x14].copy_from_slice(&features.to_le_bytes());
        b[0x34..0x38].copy_from_slice(&1024u32.to_le_bytes());
        pages
    }

    #[test]
    fn only_the_features_asked_for_and_offered_are_taken() {
        let s = setup(&[]);
        assert_eq!(agree(0xffff_ffff, &s), Ok(1 << 9));
        assert_eq!(agree(0, &s), Ok(0));
        let needy = Setup {
            need: 1 << 5,
            ..setup(&[])
        };
        assert_eq!(agree(1 << 5 | 1 << 9 | 1, &needy), Ok(1 << 5 | 1 << 9));
        assert!(agree(1 << 9, &needy).is_err());
    }

    #[test]
    fn a_legacy_mmio_device_is_told_its_queue_by_page_number() {
        let mut window = mmio_window(1 << 9 | 1 << 2);
        let mut dev = unsafe { Device::new(window.ptr(), Transport::Mmio) };
        let up = dev.bring_up(&setup(&[(0x8_7000, true)])).unwrap();
        assert_eq!(up.features, 1 << 9);
        assert!(!up.irq_refused);
        assert_eq!(rd32(&window, MMIO_GUEST_FEATURES), 1 << 9);
        assert_eq!(rd32(&window, MMIO_GUEST_PAGE_SIZE), 4096);
        assert_eq!(rd32(&window, MMIO_QUEUE_NUM), 64);
        assert_eq!(rd32(&window, MMIO_QUEUE_ALIGN), 4096);
        assert_eq!(rd32(&window, MMIO_QUEUE_PFN), 0x87);
        // Not running until the driver says so.
        assert_eq!(rd32(&window, MMIO_STATUS), 3);
        dev.driver_ok();
        assert_eq!(rd32(&window, MMIO_STATUS), 7);
        dev.notify(0);
        assert_eq!(rd32(&window, MMIO_QUEUE_NOTIFY), 0);
    }

    #[test]
    fn a_window_that_is_not_the_device_expected_is_left_alone() {
        let mut window = mmio_window(0);
        window.bytes_mut()[8] = ID_NET as u8;
        let mut dev = unsafe { Device::new(window.ptr(), Transport::Mmio) };
        assert!(dev.bring_up(&setup(&[(0x1000, true)])).is_err());
        assert_eq!(rd32(&window, MMIO_STATUS), 0, "nothing was written to it");
        let mut blank = Pages::new(1);
        let mut dev = unsafe { Device::new(blank.ptr(), Transport::Mmio) };
        assert!(dev.bring_up(&setup(&[(0x1000, true)])).is_err());
    }

    #[test]
    fn a_queue_smaller_than_the_driver_needs_is_refused() {
        let mut window = mmio_window(0);
        window.bytes_mut()[0x34..0x38].copy_from_slice(&32u32.to_le_bytes());
        let mut dev = unsafe { Device::new(window.ptr(), Transport::Mmio) };
        assert!(dev.bring_up(&setup(&[(0x1000, true)])).is_err());
    }

    #[test]
    fn the_mmio_interrupt_is_acked_with_what_the_status_shows() {
        let mut window = mmio_window(0);
        window.bytes_mut()[MMIO_INTERRUPT_STATUS as usize] = 1;
        let dev = unsafe { Device::new(window.ptr(), Transport::Mmio) };
        dev.ack();
        assert_eq!(rd32(&window, MMIO_INTERRUPT_ACK), 1);
    }

    #[test]
    fn a_modern_pci_device_is_told_where_each_ring_is() {
        let mut window = Pages::new(1);
        let b = window.bytes_mut();
        // Offers VERSION_1 (read with select 1) and the flush bit, queues
        // of up to 256, and notifies queue 0 at offset 3 of the notify area.
        b[0x04..0x08].copy_from_slice(&(1u32 | 1 << 9).to_le_bytes());
        b[0x18..0x1a].copy_from_slice(&256u16.to_le_bytes());
        b[0x1e..0x20].copy_from_slice(&3u16.to_le_bytes());
        b[0x40c..0x40e].copy_from_slice(&0xffffu16.to_le_bytes());
        let pci = Transport::Pci {
            common: 0,
            notify: 0x400,
            device: 0x800,
            notify_multiplier: 4,
        };
        let mut dev = unsafe { Device::new(window.ptr(), pci) };
        let up = dev.bring_up(&setup(&[(0x1_2345_6000, true)])).unwrap();
        assert_eq!(up.features, 1 << 9);
        assert!(!up.irq_refused);
        let rd16 = |w: &Pages, off: usize| u16::from_le_bytes([w.bytes()[off], w.bytes()[off + 1]]);
        assert_eq!(rd16(&window, 0x18), 64);
        assert_eq!(rd16(&window, 0x1a), 0, "the queue raises MSI-X vector 0");
        assert_eq!(
            rd16(&window, 0x10),
            0xffff,
            "configuration changes raise none"
        );
        assert_eq!(rd16(&window, 0x1c), 1);
        assert_eq!(rd32(&window, 0x20), 0x2345_6000);
        assert_eq!(rd32(&window, 0x24), 1);
        assert_eq!(rd32(&window, 0x28), 0x2345_6000 + 1024);
        assert_eq!(rd32(&window, 0x30), 0x2345_6000 + 4096);
        assert_eq!(rd32(&window, 0x34), 1);
        assert_eq!(window.bytes()[0x14], 11, "features agreed, not yet running");
        dev.driver_ok();
        assert_eq!(window.bytes()[0x14], 15);
        dev.notify(0);
        assert_eq!(rd16(&window, 0x40c), 0, "notified where notify_off says");
    }

    #[test]
    fn a_pci_queue_that_wants_no_interrupt_is_given_no_vector() {
        let mut window = Pages::new(1);
        let b = window.bytes_mut();
        b[0x04..0x08].copy_from_slice(&1u32.to_le_bytes());
        b[0x18..0x1a].copy_from_slice(&64u16.to_le_bytes());
        let pci = Transport::Pci {
            common: 0,
            notify: 0x400,
            device: 0x800,
            notify_multiplier: 4,
        };
        let mut dev = unsafe { Device::new(window.ptr(), pci) };
        dev.bring_up(&setup(&[(0x6000, false)])).unwrap();
        assert_eq!(&window.bytes()[0x1a..0x1c], &[0xff, 0xff]);
    }

    #[test]
    fn the_configuration_is_read_where_each_transport_keeps_it() {
        let mut window = Pages::new(1);
        window.bytes_mut()[0x100..0x104].copy_from_slice(&0x1234u32.to_le_bytes());
        window.bytes_mut()[0x300..0x304].copy_from_slice(&0x5678u32.to_le_bytes());
        let dev = unsafe { Device::new(window.ptr(), Transport::Mmio) };
        assert_eq!(dev.config32(0), 0x1234);
        let pci = Transport::Pci {
            common: 0,
            notify: 0x200,
            device: 0x300,
            notify_multiplier: 4,
        };
        let dev = unsafe { Device::new(window.ptr(), pci) };
        assert_eq!(dev.config32(0), 0x5678);
        assert_eq!(dev.config8(1), 0x56);
    }
}
