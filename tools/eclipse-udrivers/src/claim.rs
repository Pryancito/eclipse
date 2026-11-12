//! Taking a device from the kernel, and the memory to drive it with.
//!
//! `eclipse_device_claim(kind, index, *claim)` gives a process a device the
//! kernel left alone: its registers as a physical VMO, a BTI to get DMA
//! memory through, and its interrupt as an interrupt object. One driver per
//! device at a time; when the process dies the kernel resets the device and
//! it can be claimed again.

use std::time::Duration;

use crate::virtio::Transport;
use crate::zx::{self, nr, Handle, Status};
use crate::PAGE;

/// Claim kinds.
pub const KIND_VIRTIO_BLK: usize = 1;
pub const KIND_VIRTIO_NET: usize = 2;

const TRANSPORT_VIRTIO_MMIO: u32 = 1;
const TRANSPORT_VIRTIO_PCI: u32 = 2;

const BTI_PERM_READ: usize = 1 << 0;
const BTI_PERM_WRITE: usize = 1 << 1;
const BTI_CONTIGUOUS: usize = 1 << 4;
const VMO_CHILD_SLICE: usize = 1 << 3;

/// What the kernel hands out with a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Claim {
    /// The registers: a physical VMO covering the pages that hold them.
    pub regs: Handle,
    /// For DMA memory and its device addresses.
    pub bti: Handle,
    /// How the device is reached, `None` for a transport this crate does
    /// not know.
    pub transport: Option<Transport>,
    /// The interrupt line, for the record (`u32::MAX` when unknown).
    pub irq: u32,
    /// Where the register window starts inside the VMO, and its size.
    pub offset: u64,
    pub size: u64,
    /// The device's interrupt object, `None` when there is none to wait on.
    pub interrupt: Option<Handle>,
}

impl Claim {
    /// The claim in the eight words the kernel writes (see
    /// `ECLIPSE_DEVICE_CLAIM` in `loader/src/linux.rs`): a pair is its low
    /// half, then its high half.
    pub fn from_words(w: &[u64; 8]) -> Claim {
        let low = |v: u64| v as u32;
        let high = |v: u64| (v >> 32) as u32;
        let transport = match low(w[1]) {
            TRANSPORT_VIRTIO_MMIO => Some(Transport::Mmio),
            TRANSPORT_VIRTIO_PCI => Some(Transport::Pci {
                common: low(w[4]),
                notify: high(w[4]),
                device: high(w[5]),
                notify_multiplier: low(w[6]),
            }),
            _ => None,
        };
        Claim {
            regs: low(w[0]),
            bti: high(w[0]),
            transport,
            irq: high(w[1]),
            offset: w[2],
            size: w[3],
            interrupt: match low(w[7]) {
                0 => None,
                handle => Some(handle),
            },
        }
    }

    /// How many bytes of the registers' VMO to map to reach the whole
    /// window: whole pages, from the start of the VMO.
    pub fn span(&self) -> usize {
        (self.offset + self.size).div_ceil(PAGE as u64) as usize * PAGE
    }

    /// Map the register window. The pointer is to its first register.
    pub fn map_regs(&self) -> Result<*mut u8, String> {
        let base = zx::map(self.regs, self.span())?;
        // SAFETY: `offset` is inside the mapping of `span` bytes.
        Ok(unsafe { base.add(self.offset as usize) })
    }
}

/// Claim device `index` of `kind`. A claim refused because the device has a
/// driver is tried again every 10 ms, `patience` times in all: the driver
/// before this one may have died a moment ago, and the kernel resets the
/// device before it gives it out.
pub fn claim(kind: usize, index: usize, patience: u32) -> Result<Claim, Status> {
    let mut words = [0u64; 8];
    let mut tries = patience.max(1);
    loop {
        // SAFETY: the kernel writes eight words where `words` has room for them.
        let r = unsafe {
            zx::call(
                nr::ECLIPSE_DEVICE_CLAIM,
                [kind, index, words.as_mut_ptr() as usize, 0, 0, 0],
            )
        };
        tries -= 1;
        match r {
            Ok(_) => return Ok(Claim::from_words(&words)),
            Err(Status::ALREADY_BOUND) if tries > 0 => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(e),
        }
    }
}

/// DMA memory: contiguous, pinned, and mapped here.
pub struct Dma {
    /// Where this process sees it.
    pub ptr: *mut u8,
    pub len: usize,
    /// Where the device sees it.
    pub daddr: u64,
    /// The window asked for in [`Dma::new`]: a VMO over that part of the
    /// memory and no other, to hand to someone else.
    pub window: Option<Handle>,
}

impl Dma {
    /// `len` zeroed bytes (whole pages) of DMA memory through `bti`. It is
    /// the driver's for the life of the process: the kernel unpins it when
    /// the process dies, after resetting the device.
    ///
    /// `window` is `(offset, len)`, whole pages, of a part to cut out as a
    /// VMO of its own. It is asked for here because it has to be cut before
    /// the memory is mapped: mapping moves the VMO's handle away.
    pub fn new(bti: Handle, len: usize, window: Option<(usize, usize)>) -> Result<Dma, String> {
        assert!(len.is_multiple_of(PAGE));
        let mut vmo: Handle = 0;
        // SAFETY: the kernel writes a handle into `vmo`.
        unsafe {
            zx::call(
                nr::VMO_CREATE_CONTIGUOUS,
                [bti as usize, len, 0, &mut vmo as *mut Handle as usize, 0, 0],
            )
        }
        .map_err(|e| format!("vmo_create_contiguous: {e}"))?;
        let mut daddr: u64 = 0;
        let mut pmt: Handle = 0;
        // SAFETY: the kernel writes one address into `daddr` (a contiguous
        // pin has one) and a handle into `pmt`.
        unsafe {
            zx::call8(
                nr::BTI_PIN,
                [
                    bti as usize,
                    BTI_PERM_READ | BTI_PERM_WRITE | BTI_CONTIGUOUS,
                    vmo as usize,
                    0,
                    len,
                    &mut daddr as *mut u64 as usize,
                    1,
                    &mut pmt as *mut Handle as usize,
                ],
            )
        }
        .map_err(|e| format!("bti_pin: {e}"))?;
        let window = match window {
            Some((offset, part)) => {
                assert!(offset + part <= len);
                Some(slice(vmo, offset, part)?)
            }
            None => None,
        };
        let ptr = zx::map(vmo, len)?;
        // SAFETY: `len` bytes just mapped, this process's alone so far.
        unsafe { ptr.write_bytes(0, len) };
        Ok(Dma {
            ptr,
            len,
            daddr,
            window,
        })
    }
}

/// A VMO over `len` bytes of `vmo` from `offset`.
fn slice(vmo: Handle, offset: usize, len: usize) -> Result<Handle, String> {
    assert!(offset.is_multiple_of(PAGE) && len.is_multiple_of(PAGE));
    let mut child: Handle = 0;
    // SAFETY: the kernel writes a handle into `child`.
    unsafe {
        zx::call(
            nr::VMO_CREATE_CHILD,
            [
                vmo as usize,
                VMO_CHILD_SLICE,
                offset,
                len,
                &mut child as *mut Handle as usize,
                0,
            ],
        )
    }
    .map_err(|e| format!("vmo_create_child (slice): {e}"))?;
    Ok(child)
}

/// Sleep until the device's interrupt fires.
pub fn interrupt_wait(interrupt: Handle) -> Result<(), Status> {
    let mut timestamp: i64 = 0;
    // SAFETY: the kernel writes the interrupt's timestamp into `timestamp`.
    unsafe {
        zx::call(
            nr::INTERRUPT_WAIT,
            [
                interrupt as usize,
                &mut timestamp as *mut i64 as usize,
                0,
                0,
                0,
                0,
            ],
        )
    }
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_virtio_mmio_claim_is_read_out_of_its_eight_words() {
        let c = Claim::from_words(&[
            0x0000_0007_0000_0005, // regs 5, bti 7
            0x0000_0031_0000_0001, // mmio, line 0x31
            0xe00,
            0x200,
            0,
            0,
            0,
            9,
        ]);
        assert_eq!(c.regs, 5);
        assert_eq!(c.bti, 7);
        assert_eq!(c.transport, Some(Transport::Mmio));
        assert_eq!(c.irq, 0x31);
        assert_eq!((c.offset, c.size), (0xe00, 0x200));
        assert_eq!(c.interrupt, Some(9));
        // QEMU `virt` packs eight transports into a page: the window ends
        // where its page does.
        assert_eq!(c.span(), 4096);
    }

    #[test]
    fn a_virtio_pci_claim_carries_where_its_structures_are() {
        let c = Claim::from_words(&[
            0x0000_0004_0000_0003,
            0xffff_ffff_0000_0002, // pci, line unknown
            0,
            0x4000,
            0x0000_3000_0000_0000, // common 0, notify 0x3000
            0x0000_2000_0000_1000, // isr 0x1000, device 0x2000
            4,
            0, // no interrupt object
        ]);
        assert_eq!(
            c.transport,
            Some(Transport::Pci {
                common: 0,
                notify: 0x3000,
                device: 0x2000,
                notify_multiplier: 4
            })
        );
        assert_eq!(c.irq, u32::MAX);
        assert_eq!(c.interrupt, None);
        assert_eq!(c.span(), 0x4000);
    }

    #[test]
    fn a_transport_nobody_here_knows_is_not_guessed_at() {
        let c = Claim::from_words(&[0, 3, 0, 0x1000, 0, 0, 0, 0]);
        assert_eq!(c.transport, None);
    }

    #[test]
    fn a_window_that_ends_inside_a_page_maps_that_whole_page() {
        let c = Claim::from_words(&[0, 1, 0x1e00, 0x400, 0, 0, 0, 0]);
        assert_eq!(c.span(), 0x3000);
    }
}
