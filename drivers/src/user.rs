//! Devices the kernel leaves to a userspace driver.
//!
//! `udriver=virtio-blk@N` on the command line keeps the kernel's hands off
//! the `N`th virtio block device it finds (counting from 0, in probe order):
//! it is not initialised, not registered as a block device, and its window
//! and interrupt line are recorded here instead. `udriver=virtio-net@N` does
//! the same for a virtio network card, which the kernel has no driver for at
//! all: without the reservation it is simply not looked at. Several devices
//! are named with commas: `udriver=virtio-blk@1,virtio-net@0`. A process can then claim
//! it, once, and drive it from userspace (`eclipse_device_claim` in the
//! loader's Zircon gateway).
//!
//! The count is per kind and runs across every bus the kernel probes, so
//! `virtio-blk@1` is "the second virtio disk" whatever the transport. The
//! default is no reservation at all: the kernel takes every device, as it
//! always has.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use spin::{Mutex, Once};

/// A virtio block device.
pub const VIRTIO_BLK: &str = "virtio-blk";
/// A virtio network card.
pub const VIRTIO_NET: &str = "virtio-net";
/// The kinds that can be reserved.
const KINDS: [&str; 2] = [VIRTIO_BLK, VIRTIO_NET];

/// How the device is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Legacy virtio over MMIO (spec 1.1 §4.2): the whole register window.
    VirtioMmio,
    /// Modern virtio over PCI (spec 1.1 §4.1): the window is the memory
    /// BAR holding all four structures, at these offsets in it.
    VirtioPci {
        /// `VIRTIO_PCI_CAP_COMMON_CFG`.
        common: u32,
        /// `VIRTIO_PCI_CAP_NOTIFY_CFG`.
        notify: u32,
        /// `VIRTIO_PCI_CAP_ISR_CFG`.
        isr: u32,
        /// `VIRTIO_PCI_CAP_DEVICE_CFG`.
        device: u32,
        /// `notify_off_multiplier`.
        notify_multiplier: u32,
    },
}

/// A device reserved for userspace.
#[derive(Debug)]
pub struct UserDevice {
    /// The kind, as the command line names it.
    pub kind: &'static str,
    /// Its place among the devices of its kind (the `N` of `kind@N`).
    pub index: usize,
    /// How its registers are laid out.
    pub transport: Transport,
    /// Physical address of its register window.
    pub paddr: usize,
    /// Size of that window, in bytes.
    pub size: usize,
    /// Its interrupt, as the number its controller's `IrqScheme` takes,
    /// when known: the line on a GIC or PLIC, the vector of an MSI on x86.
    pub irq: Option<usize>,
    claimed: AtomicBool,
}

impl UserDevice {
    /// Take the device. Only the first caller gets `true`: the device has
    /// one driver, and nothing hands it back (the registers are not reset,
    /// so a second driver would start from whatever state the first one
    /// left).
    pub fn claim(&self) -> bool {
        !self.claimed.swap(true, Ordering::AcqRel)
    }

    /// Whether a driver has taken it.
    pub fn is_claimed(&self) -> bool {
        self.claimed.load(Ordering::Acquire)
    }
}

/// What the command line asked for: each a `(kind, index)`.
static WANTED: Once<Vec<(&'static str, usize)>> = Once::new();
/// Devices of each of [`KINDS`] seen so far, reserved or not.
static SEEN: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];
static RESERVED: Mutex<Vec<Arc<UserDevice>>> = Mutex::new(Vec::new());

/// Parse one item of a `udriver=` value: `virtio-blk@N` or `virtio-net@N`.
/// Anything else reserves nothing (and says so). Not `kind:N`: the command
/// line is split on `:`.
pub fn parse_spec(spec: &str) -> Option<(&'static str, usize)> {
    let (kind, index) = spec.split_once('@')?;
    let kind = KINDS.iter().find(|k| **k == kind)?;
    let index = index.parse().ok()?;
    Some((kind, index))
}

/// Parse a whole `udriver=` value: items separated by commas. The ones
/// that name nothing reservable are left out.
pub fn parse_specs(spec: &str) -> Vec<(&'static str, usize)> {
    spec.split(',').filter_map(parse_spec).collect()
}

/// Set the reservation from the command line's `udriver=` value, before
/// the buses are probed. Only the first call counts.
pub fn set_reservation(spec: Option<&str>) {
    WANTED.call_once(|| {
        let mut wanted = Vec::new();
        for item in spec.unwrap_or("").split(',').filter(|s| !s.is_empty()) {
            match parse_spec(item) {
                Some((kind, index)) => {
                    info!("udriver: reserving {kind} #{index} for userspace");
                    wanted.push((kind, index));
                }
                None => warn!("udriver={item}: not a device this kernel can reserve"),
            }
        }
        wanted
    });
}

/// A bus found a virtio block device: count it, and answer whether it is
/// the one to leave alone. When it is, it is recorded and the caller must
/// not touch it -- not even to reset it.
pub fn reserve_virtio_blk(
    transport: Transport,
    paddr: usize,
    size: usize,
    irq: Option<usize>,
) -> bool {
    reserve_if(VIRTIO_BLK, true, transport, paddr, size, || irq)
}

/// A bus found a virtio network card: count it, and answer whether it is
/// one to hand to userspace. The kernel drives none of them either way.
pub fn reserve_virtio_net(
    transport: Transport,
    paddr: usize,
    size: usize,
    irq: Option<usize>,
) -> bool {
    reserve_if(VIRTIO_NET, true, transport, paddr, size, || irq)
}

/// [`reserve_virtio_blk`] for a disk that may not be fit to hand over
/// (`usable` false: a PCI disk without a modern transport). It is counted
/// all the same, so the index of every later disk stays put, and the
/// kernel keeps it. `irq` is asked only for the disk that is handed over,
/// so a bus can set up the interrupt of that one disk and no other.
pub fn reserve_virtio_blk_if(
    usable: bool,
    transport: Transport,
    paddr: usize,
    size: usize,
    irq: impl FnOnce() -> Option<usize>,
) -> bool {
    reserve_if(VIRTIO_BLK, usable, transport, paddr, size, irq)
}

/// The one counter-and-record behind every `reserve_*`: `kind` is one of
/// [`KINDS`].
pub fn reserve_if(
    kind: &'static str,
    usable: bool,
    transport: Transport,
    paddr: usize,
    size: usize,
    irq: impl FnOnce() -> Option<usize>,
) -> bool {
    let Some(slot) = KINDS.iter().position(|k| *k == kind) else {
        return false;
    };
    let index = SEEN[slot].fetch_add(1, Ordering::AcqRel);
    let wanted = WANTED.get().is_some_and(|w| w.contains(&(kind, index)));
    if !wanted {
        return false;
    }
    if !usable {
        warn!("udriver: {kind} #{index} has no transport a userspace driver can use; the kernel keeps it");
        return false;
    }
    let irq = irq();
    info!("udriver: {kind} #{index} at {paddr:#x} (+{size:#x}, irq {irq:?}) left to userspace");
    RESERVED.lock().push(Arc::new(UserDevice {
        kind,
        index,
        transport,
        paddr,
        size,
        irq,
        claimed: AtomicBool::new(false),
    }));
    true
}

/// The reserved device of `kind` with this index, if there is one.
pub fn find(kind: &str, index: usize) -> Option<Arc<UserDevice>> {
    RESERVED
        .lock()
        .iter()
        .find(|d| d.kind == kind && d.index == index)
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spec_names_a_kind_and_an_index() {
        assert_eq!(parse_spec("virtio-blk@1"), Some((VIRTIO_BLK, 1)));
        assert_eq!(parse_spec("virtio-blk@0"), Some((VIRTIO_BLK, 0)));
        assert_eq!(parse_spec("virtio-blk"), None);
        assert_eq!(parse_spec("virtio-blk@x"), None);
        assert_eq!(parse_spec("virtio-net@0"), Some((VIRTIO_NET, 0)));
        assert_eq!(parse_spec("virtio-gpu@0"), None);
        assert_eq!(parse_spec(""), None);
    }

    #[test]
    fn several_devices_are_named_with_commas() {
        assert_eq!(
            parse_specs("virtio-blk@1,virtio-net@0"),
            [(VIRTIO_BLK, 1), (VIRTIO_NET, 0)]
        );
        assert_eq!(parse_specs("virtio-blk@1"), [(VIRTIO_BLK, 1)]);
        assert_eq!(parse_specs("nonsense,virtio-net@2"), [(VIRTIO_NET, 2)]);
        assert!(parse_specs("").is_empty());
    }

    #[test]
    fn a_device_is_claimed_once() {
        let dev = UserDevice {
            kind: VIRTIO_BLK,
            index: 0,
            transport: Transport::VirtioMmio,
            paddr: 0x1000,
            size: 0x200,
            irq: None,
            claimed: AtomicBool::new(false),
        };
        assert!(!dev.is_claimed());
        assert!(dev.claim());
        assert!(!dev.claim());
        assert!(dev.is_claimed());
    }

    // The global state is set once per process, so one test walks it.
    #[test]
    fn only_the_wanted_device_is_reserved() {
        set_reservation(Some("virtio-blk@1,virtio-net@0"));
        let base = SEEN[0].load(Ordering::Acquire) + SEEN[1].load(Ordering::Acquire);
        // Other tests may not count devices, so the index is absolute only
        // when this test starts from zero.
        if base != 0 {
            return;
        }
        assert!(!reserve_virtio_blk(
            Transport::VirtioMmio,
            0x1000,
            0x200,
            Some(1)
        ));
        assert!(reserve_virtio_blk(
            Transport::VirtioMmio,
            0x2000,
            0x200,
            Some(2)
        ));
        assert!(!reserve_virtio_blk(
            Transport::VirtioMmio,
            0x3000,
            0x200,
            Some(3)
        ));
        let dev = find(VIRTIO_BLK, 1).unwrap();
        assert_eq!((dev.paddr, dev.size, dev.irq), (0x2000, 0x200, Some(2)));
        assert!(find(VIRTIO_BLK, 0).is_none());
        assert!(find(VIRTIO_BLK, 2).is_none());
        // Each kind counts its own devices: the first card is #0 whatever
        // number of disks came before it.
        assert!(reserve_virtio_net(
            Transport::VirtioMmio,
            0x4000,
            0x200,
            Some(4)
        ));
        assert!(!reserve_virtio_net(
            Transport::VirtioMmio,
            0x5000,
            0x200,
            Some(5)
        ));
        let card = find(VIRTIO_NET, 0).unwrap();
        assert_eq!((card.paddr, card.irq), (0x4000, Some(4)));
        assert!(find(VIRTIO_NET, 1).is_none());
    }
}
