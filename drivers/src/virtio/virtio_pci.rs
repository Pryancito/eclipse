use crate::builder::IoMapper;
use crate::bus::pci_drivers::PciDriver;
use crate::bus::resolve_window;
use crate::{Device, DeviceError, DeviceResult};
use alloc::sync::Arc;
use pci::{PCIDevice, BAR};

/// How many BARs a PCI function has, which is also every value the `bar` field
/// of a virtio capability may hold (spec 1.1 §4.1.4.1).
///
/// The number comes out of the **device's** config space and used to index
/// [`PCIDevice::bars`] --- six entries long --- with no check at all, so a
/// device that answered 6 or more panicked the kernel in the middle of PCI
/// enumeration.
const BAR_COUNT: u8 = 6;

/// How far the capability chain is followed before giving up.
///
/// The MSI walk in [`crate::bus::pci`] already carries three guards --- this
/// step limit, a stuck-pointer check and the floor below --- because the chain
/// is device-supplied and a cycle in it never comes back. This walk reads the
/// same chain a second time and had none of them: one `next` pointer aimed
/// backwards hung the boot before a single driver ran.
const MAX_CAP_TRAVERSAL: usize = 64;

/// The smallest pointer that can start a capability: below this is the
/// standard PCI header.
const MIN_CAP_PTR: u16 = 0x40;

/// The last offset a 16-byte vendor capability can start at and still fit in
/// the 256-byte config space; past it, the `offset` and `length` dwords would
/// come from whatever the access method wraps around to.
const MAX_CAP_PTR: u16 = 0xff - 15;

/// Length of the virtio common configuration structure (spec 1.1 §4.1.4.3).
pub const COMMON_CFG_LEN: u32 = 56;

/// A queue notification is one 16-bit write.
pub const NOTIFY_CFG_MIN_LEN: u32 = 2;

/// Device-specific configuration: at least one byte is read.
pub const DEVICE_CFG_MIN_LEN: u32 = 1;

const CAP_ID_VENDOR: u8 = 0x09;
const CFG_TYPE_COMMON: u8 = 1;
const CFG_TYPE_NOTIFY: u8 = 2;
const CFG_TYPE_DEVICE: u8 = 4;

/// The window a virtio PCI capability points at, exactly as the device
/// describes it. None of the three numbers has been checked yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapWindow {
    /// Index of the BAR the window lives in.
    pub bar: u8,
    /// Byte offset of the window inside that BAR.
    pub offset: u32,
    /// Length of the window, as the device reports it.
    pub length: u32,
}

/// The virtio structures a modern (PCI) device advertises through its
/// capability chain.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VirtioCaps {
    /// Common configuration (`VIRTIO_PCI_CAP_COMMON_CFG`).
    pub common: Option<CapWindow>,
    /// Notification area (`VIRTIO_PCI_CAP_NOTIFY_CFG`).
    pub notify: Option<CapWindow>,
    /// Device-specific configuration (`VIRTIO_PCI_CAP_DEVICE_CFG`).
    pub device: Option<CapWindow>,
}

/// Walk a device's PCI capability chain and collect the virtio structures.
///
/// `read8` and `read32` read that device's config space at a byte offset.
/// The walk is bounded three ways, see [`MAX_CAP_TRAVERSAL`].
pub fn scan_virtio_caps<R8, R32>(read8: R8, read32: R32) -> VirtioCaps
where
    R8: Fn(u16) -> u8,
    R32: Fn(u16) -> u32,
{
    let mut caps = VirtioCaps::default();
    let mut cap_ptr = read8(0x34) as u16;
    let mut prev_cap_ptr = 0u16;
    let mut steps = 0usize;

    while cap_ptr > 0 {
        if steps >= MAX_CAP_TRAVERSAL {
            warn!("[virtio-pci] capability chain longer than {} entries or cyclic, aborting traversal", MAX_CAP_TRAVERSAL);
            break;
        }
        if cap_ptr == prev_cap_ptr {
            warn!(
                "[virtio-pci] capability chain stuck at {:#x}, aborting traversal",
                cap_ptr
            );
            break;
        }
        if !(MIN_CAP_PTR..=MAX_CAP_PTR).contains(&cap_ptr) {
            warn!(
                "[virtio-pci] capability pointer {:#x} is outside config space, aborting traversal",
                cap_ptr
            );
            break;
        }
        steps += 1;

        if read8(cap_ptr) == CAP_ID_VENDOR {
            let cfg_type = read8(cap_ptr + 3);
            let window = CapWindow {
                bar: read8(cap_ptr + 4),
                offset: read32(cap_ptr + 8),
                length: read32(cap_ptr + 12),
            };
            match cfg_type {
                CFG_TYPE_COMMON => caps.common = Some(window),
                CFG_TYPE_NOTIFY => caps.notify = Some(window),
                CFG_TYPE_DEVICE => caps.device = Some(window),
                _ => {}
            }
            warn!(
                "VirtIO Cap: type={}, bar={}, offset={:#x}, len={}",
                cfg_type, window.bar, window.offset, window.length
            );
        }

        prev_cap_ptr = cap_ptr;
        cap_ptr = read8(cap_ptr + 1) as u16;
    }
    caps
}

/// Place `cap` inside its BAR, returning `(physical base, BAR length, offset)`.
///
/// Returns `None` --- rather than panicking, or handing back a pointer outside
/// the mapping --- when the device names a BAR that does not exist or is not
/// memory, or a window that does not hold `min_len` bytes inside that BAR.
pub fn cap_window(
    bars: &[Option<BAR>; BAR_COUNT as usize],
    cap: CapWindow,
    min_len: u32,
) -> Option<(usize, usize, usize)> {
    if cap.bar >= BAR_COUNT {
        warn!(
            "[virtio-pci] capability names BAR {}, and a function has {}",
            cap.bar, BAR_COUNT
        );
        return None;
    }
    let (addr, bar_len) = match bars[cap.bar as usize] {
        Some(BAR::Memory(addr, len, _, _)) => (addr as usize, len as usize),
        Some(BAR::IO(..)) => {
            warn!(
                "[virtio-pci] capability names BAR {}, which is an I/O BAR",
                cap.bar
            );
            return None;
        }
        None => {
            warn!(
                "[virtio-pci] capability names BAR {}, which the device does not implement",
                cap.bar
            );
            return None;
        }
    };
    if cap.length < min_len {
        warn!(
            "[virtio-pci] capability window in BAR {} is {} bytes, short of the {} the driver reads",
            cap.bar, cap.length, min_len
        );
        return None;
    }
    let end = (cap.offset as usize).checked_add(cap.length as usize)?;
    if end > bar_len {
        warn!(
            "[virtio-pci] capability window {:#x}..{:#x} runs off the end of BAR {} ({:#x} bytes)",
            cap.offset, end, cap.bar, bar_len
        );
        return None;
    }
    Some((addr, bar_len, cap.offset as usize))
}

pub struct VirtIoPciDriver;

impl PciDriver for VirtIoPciDriver {
    fn name(&self) -> &str {
        "virtio-pci"
    }

    fn matched(&self, vendor_id: u16, _device_id: u16) -> bool {
        vendor_id == 0x1af4
    }

    fn init(
        &self,
        dev: &PCIDevice,
        mapper: &Option<Arc<dyn IoMapper>>,
        _irq: Option<usize>,
    ) -> DeviceResult<Device> {
        let device_id = dev.id.device_id;

        warn!("VirtIO device {:x} found!", device_id);

        #[cfg(feature = "virtio")]
        {
            use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
            let ops = &PortOpsImpl;
            let am = PCI_ACCESS;

            let caps = scan_virtio_caps(
                |off| unsafe { am.read8(ops, dev.loc, off) },
                |off| unsafe { am.read32(ops, dev.loc, off) },
            );

            let resolve = |pa: usize, len: usize, off: usize| -> usize {
                resolve_window(mapper, pa, len, off)
            };

            let window = |cap: Option<CapWindow>, min_len: u32| -> usize {
                cap.and_then(|c| cap_window(&dev.bars, c, min_len))
                    .map(|(pa, len, off)| resolve(pa, len, off))
                    .unwrap_or(0)
            };

            if let Some((addr, bar_len, offset)) = caps
                .common
                .and_then(|c| cap_window(&dev.bars, c, COMMON_CFG_LEN))
            {
                let common_vaddr = resolve(addr, bar_len, offset);
                let device_vaddr = window(caps.device, DEVICE_CFG_MIN_LEN);
                let notify_vaddr = window(caps.notify, NOTIFY_CFG_MIN_LEN);

                let (fb_vaddr, fb_size) =
                    if let Some(BAR::Memory(fb_addr, fb_len, _, _)) = dev.bars[0] {
                        (
                            resolve(fb_addr as usize, fb_len as usize, 0),
                            fb_len as usize,
                        )
                    } else {
                        (0, 0)
                    };

                if device_id == 0x1050 {
                    match crate::virtio::VirtIoGpu::new_modern(
                        common_vaddr,
                        device_vaddr,
                        notify_vaddr,
                        fb_vaddr,
                        fb_size,
                    ) {
                        Ok(gpu) => {
                            warn!("VirtIO Modern GPU initialized successfully!");
                            return Ok(Device::Drm(Arc::new(gpu)));
                        }
                        Err(e) => warn!("VirtIO Modern GPU init failed: {:?}", e),
                    }
                }
            }

            // Fallback to legacy if no modern caps found or failed
            if let Some(BAR::Memory(addr, len, _, _)) = dev.bars[0] {
                let header_len = core::mem::size_of::<crate::virtio::VirtIOHeader>();
                if (len as usize) < header_len {
                    warn!(
                        "[virtio-pci] BAR0 is {:#x} bytes, short of the {:#x} a legacy header needs",
                        len, header_len
                    );
                    return Err(DeviceError::NotSupported);
                }
                let vaddr = resolve(addr as usize, len as usize, 0);
                let header = unsafe { &mut *(vaddr as *mut crate::virtio::VirtIOHeader) };

                match device_id {
                    0x1050 => {
                        if let Ok(gpu) = crate::virtio::VirtIoGpu::new(header) {
                            return Ok(Device::Drm(Arc::new(gpu)));
                        }
                    }
                    0x1001 | 0x1042 => {
                        if let Ok(blk) = crate::virtio::VirtIoBlk::new(header) {
                            return Ok(Device::Block(Arc::new(blk)));
                        }
                    }
                    0x1003 | 0x1043 => {
                        if let Ok(console) = crate::virtio::VirtIoConsole::new(header) {
                            return Ok(Device::Uart(Arc::new(console)));
                        }
                    }
                    0x1012 | 0x1052 => {
                        if let Ok(input) = crate::virtio::VirtIoInput::new(header) {
                            return Ok(Device::Input(Arc::new(input)));
                        }
                    }
                    _ => {
                        warn!("VirtIO legacy device {:x} is not yet supported", device_id);
                    }
                }
            }

            Err(DeviceError::NotSupported)
        }
        #[cfg(not(feature = "virtio"))]
        {
            Err(DeviceError::NotSupported)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;
    use pci::{Prefetchable, Type};

    /// A device's 256-byte config space, plus a count of how many times it has
    /// been read.
    ///
    /// The count is the point: a walk that does not bound itself never comes
    /// back, and a test that never comes back is a suite that hangs without
    /// naming anything. The limit is far above any legal chain, so it only ever
    /// fires on a loop --- and indexing `bytes` fails a walk that reads outside
    /// the 256 bytes a PCI function has.
    struct ConfigSpace {
        /// 256 bytes of config space and sixteen more so a capability can be
        /// *written* at the very end of it. Reading those sixteen is the bug.
        bytes: [u8; 272],
        reads: Cell<usize>,
    }

    impl ConfigSpace {
        fn new(first_cap: u8) -> Self {
            let mut bytes = [0u8; 272];
            bytes[0x34] = first_cap;
            Self {
                bytes,
                reads: Cell::new(0),
            }
        }

        /// A 16-byte virtio vendor capability.
        fn cap(
            mut self,
            at: usize,
            next: u8,
            cfg_type: u8,
            bar: u8,
            offset: u32,
            length: u32,
        ) -> Self {
            self.bytes[at] = CAP_ID_VENDOR;
            self.bytes[at + 1] = next;
            self.bytes[at + 2] = 16;
            self.bytes[at + 3] = cfg_type;
            self.bytes[at + 4] = bar;
            self.bytes[at + 8..at + 12].copy_from_slice(&offset.to_le_bytes());
            self.bytes[at + 12..at + 16].copy_from_slice(&length.to_le_bytes());
            self
        }

        /// Any other capability: an id and a `next` pointer, which is all the
        /// walk looks at.
        fn other(mut self, at: usize, next: u8, id: u8) -> Self {
            self.bytes[at] = id;
            self.bytes[at + 1] = next;
            self
        }

        fn read8(&self, off: u16) -> u8 {
            let n = self.reads.get() + 1;
            self.reads.set(n);
            assert!(
                n < 4096,
                "the capability walk has read config space {} times: it is going round a loop",
                n
            );
            assert!(
                off < 0x100,
                "the walk read config space at {:#x}, and a PCI function has 0x100 bytes",
                off
            );
            self.bytes[off as usize]
        }

        fn read32(&self, off: u16) -> u32 {
            let o = off as usize;
            u32::from_le_bytes([
                self.bytes[o],
                self.bytes[o + 1],
                self.bytes[o + 2],
                self.bytes[o + 3],
            ])
        }

        fn scan(&self) -> VirtioCaps {
            scan_virtio_caps(|off| self.read8(off), |off| self.read32(off))
        }
    }

    /// What QEMU's `-vga virtio` advertises: the three structures in BAR 2, one
    /// page each, with an MSI-X capability in the middle of the chain.
    fn qemu_like() -> ConfigSpace {
        ConfigSpace::new(0x40)
            .cap(0x40, 0x50, CFG_TYPE_COMMON, 2, 0x0000, 0x1000)
            .other(0x50, 0x60, 0x11)
            .cap(0x60, 0x70, CFG_TYPE_NOTIFY, 2, 0x1000, 0x1000)
            .cap(0x70, 0x00, CFG_TYPE_DEVICE, 2, 0x2000, 0x1000)
    }

    fn memory_bars() -> [Option<BAR>; 6] {
        let mut bars = [None; 6];
        bars[0] = Some(BAR::Memory(
            0xfd00_0000,
            0x0100_0000,
            Prefetchable::No,
            Type::Bits32,
        ));
        bars[2] = Some(BAR::Memory(
            0xfe00_0000,
            0x4000,
            Prefetchable::No,
            Type::Bits64,
        ));
        bars[4] = Some(BAR::IO(0xc000, 0x20));
        bars
    }

    #[test]
    fn the_three_windows_a_modern_device_advertises() {
        let caps = qemu_like().scan();
        assert_eq!(
            caps.common,
            Some(CapWindow {
                bar: 2,
                offset: 0x0000,
                length: 0x1000
            })
        );
        assert_eq!(
            caps.notify,
            Some(CapWindow {
                bar: 2,
                offset: 0x1000,
                length: 0x1000
            })
        );
        assert_eq!(
            caps.device,
            Some(CapWindow {
                bar: 2,
                offset: 0x2000,
                length: 0x1000
            })
        );
    }

    #[test]
    fn a_cfg_type_the_driver_does_not_use_is_ignored() {
        // 3 is the ISR status structure: read by nobody here.
        let caps = ConfigSpace::new(0x40)
            .cap(0x40, 0x00, 3, 2, 0x3000, 0x1000)
            .scan();
        assert_eq!(caps, VirtioCaps::default());
    }

    #[test]
    fn a_device_with_no_capabilities_at_all() {
        assert_eq!(ConfigSpace::new(0).scan(), VirtioCaps::default());
    }

    #[test]
    fn a_capability_pointing_at_itself_stops_at_once() {
        let space = ConfigSpace::new(0x40).cap(0x40, 0x40, CFG_TYPE_COMMON, 2, 0, 0x1000);
        let caps = space.scan();
        // The window is still read: the chain is refused, not the device.
        assert!(caps.common.is_some());
        // And refused on the spot, not after running the step limit down: the
        // stuck-pointer check is what does that, and it is the reason this
        // costs a handful of reads instead of hundreds.
        assert!(
            space.reads.get() < 16,
            "a self-loop cost {} config-space reads",
            space.reads.get()
        );
    }

    #[test]
    fn a_ring_of_capabilities_stops_at_the_step_limit() {
        // Twelve capabilities, the last pointing back at the first: every step
        // moves to a different pointer, so only the step limit ends this.
        let mut space = ConfigSpace::new(0x40);
        for i in 0..11u8 {
            space = space.other(0x40 + i as usize * 4, 0x44 + i * 4, 0x05);
        }
        space = space.other(0x40 + 11 * 4, 0x40, 0x05);
        assert_eq!(space.scan(), VirtioCaps::default());
        assert!(
            space.reads.get() < 4 * MAX_CAP_TRAVERSAL + 8,
            "the walk took {} reads for a ring of twelve",
            space.reads.get()
        );
    }

    #[test]
    fn a_capability_pointer_inside_the_standard_header_is_refused() {
        // 0x20 is a BAR, not a capability. Reading it as one walks off into
        // whatever the header holds.
        let caps = ConfigSpace::new(0x20)
            .cap(0x20, 0x00, CFG_TYPE_COMMON, 2, 0, 0x1000)
            .scan();
        assert_eq!(caps, VirtioCaps::default());
    }

    #[test]
    fn a_capability_at_the_very_end_of_config_space_is_refused() {
        // A 16-byte capability starting at 0xf8 would read its length from
        // 0x104, which is not in config space at all: what comes back is
        // whatever the access method wraps around to.
        let caps = ConfigSpace::new(0xf8)
            .cap(0xf8, 0x00, CFG_TYPE_COMMON, 2, 0x1000, 0x1000)
            .scan();
        assert_eq!(caps, VirtioCaps::default());
    }

    #[test]
    fn the_last_capability_that_still_fits_is_read() {
        let caps = ConfigSpace::new(MAX_CAP_PTR as u8)
            .cap(
                MAX_CAP_PTR as usize,
                0x00,
                CFG_TYPE_COMMON,
                2,
                0x1000,
                0x1000,
            )
            .scan();
        assert_eq!(caps.common.map(|c| c.offset), Some(0x1000));
    }

    #[test]
    fn the_bar_index_the_device_invents_is_not_an_array_index() {
        let bars = memory_bars();
        for bar in [BAR_COUNT, 7, 0x80, 0xff] {
            let cap = CapWindow {
                bar,
                offset: 0,
                length: 0x1000,
            };
            assert_eq!(
                cap_window(&bars, cap, COMMON_CFG_LEN),
                None,
                "BAR {} was accepted",
                bar
            );
        }
    }

    #[test]
    fn an_io_bar_does_not_hold_a_capability() {
        let cap = CapWindow {
            bar: 4,
            offset: 0,
            length: 0x1000,
        };
        assert_eq!(cap_window(&memory_bars(), cap, COMMON_CFG_LEN), None);
    }

    #[test]
    fn a_bar_the_device_does_not_implement() {
        let cap = CapWindow {
            bar: 1,
            offset: 0,
            length: 0x1000,
        };
        assert_eq!(cap_window(&memory_bars(), cap, COMMON_CFG_LEN), None);
    }

    #[test]
    fn a_window_shorter_than_what_the_driver_reads() {
        let cap = CapWindow {
            bar: 2,
            offset: 0,
            length: COMMON_CFG_LEN - 1,
        };
        assert_eq!(cap_window(&memory_bars(), cap, COMMON_CFG_LEN), None);
        // ...and a device that reports no length at all.
        let cap = CapWindow {
            bar: 2,
            offset: 0,
            length: 0,
        };
        assert_eq!(cap_window(&memory_bars(), cap, COMMON_CFG_LEN), None);
    }

    #[test]
    fn a_window_that_runs_off_the_end_of_its_bar() {
        let bars = memory_bars();
        // BAR 2 is 0x4000 long: this one ends one byte past it.
        let cap = CapWindow {
            bar: 2,
            offset: 0x3001,
            length: 0x1000,
        };
        assert_eq!(cap_window(&bars, cap, NOTIFY_CFG_MIN_LEN), None);
        // And an offset that is not even inside the BAR.
        let cap = CapWindow {
            bar: 2,
            offset: u32::MAX,
            length: 0x1000,
        };
        assert_eq!(cap_window(&bars, cap, NOTIFY_CFG_MIN_LEN), None);
    }

    #[test]
    fn the_windows_a_modern_device_advertises_resolve_inside_their_bar() {
        let bars = memory_bars();
        let caps = qemu_like().scan();
        assert_eq!(
            cap_window(&bars, caps.common.unwrap(), COMMON_CFG_LEN),
            Some((0xfe00_0000, 0x4000, 0x0000))
        );
        assert_eq!(
            cap_window(&bars, caps.notify.unwrap(), NOTIFY_CFG_MIN_LEN),
            Some((0xfe00_0000, 0x4000, 0x1000))
        );
        assert_eq!(
            cap_window(&bars, caps.device.unwrap(), DEVICE_CFG_MIN_LEN),
            Some((0xfe00_0000, 0x4000, 0x2000))
        );
    }

    #[test]
    fn a_window_that_exactly_fills_the_end_of_its_bar_is_accepted() {
        let cap = CapWindow {
            bar: 2,
            offset: 0x3000,
            length: 0x1000,
        };
        assert_eq!(
            cap_window(&memory_bars(), cap, NOTIFY_CFG_MIN_LEN),
            Some((0xfe00_0000, 0x4000, 0x3000))
        );
        // And one exactly as long as the structure that reads it.
        let cap = CapWindow {
            bar: 2,
            offset: 0,
            length: COMMON_CFG_LEN,
        };
        assert_eq!(
            cap_window(&memory_bars(), cap, COMMON_CFG_LEN),
            Some((0xfe00_0000, 0x4000, 0))
        );
    }
}
