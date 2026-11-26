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
/// An Intel e1000e network card (QEMU's 82574, or a PCH part such as an
/// I219): see [`crate::e1000e`].
pub const KIND_E1000E: usize = 3;
/// An NVMe controller (`udriver=nvme@N`); its transport is a plain PCI
/// function and the window is BAR 0.
pub const KIND_NVME: usize = 4;
/// An AHCI controller (`udriver=ahci@N`); its transport is a plain PCI
/// function and the window is BAR 5, the ABAR.
pub const KIND_AHCI: usize = 5;
/// A virtio input device (`udriver=virtio-input@N`): see [`crate::input`].
pub const KIND_VIRTIO_INPUT: usize = 6;
/// The keyboard and the aux port of the PC's i8042 (`udriver=i8042@0`):
/// see [`crate::ps2`]. Their window is I/O ports, reached with
/// [`port_in`] and [`port_out`].
pub const KIND_I8042: usize = 7;
pub const KIND_I8042_AUX: usize = 8;
/// An Intel HD Audio controller (`udriver=hda@N`): see [`crate::hda`].
pub const KIND_HDA: usize = 9;
/// An xHCI USB controller (`udriver=xhci@N`): see [`crate::xhci`].
pub const KIND_XHCI: usize = 10;
/// Any PCI function, named by its identity (`udriver=pci-VVVV-DDDD@N`):
/// see [`claim_pci`]. Its transport is a plain PCI function and the window
/// its first memory BAR.
pub const KIND_PCI: usize = 11;
/// An NVIDIA GPU (`udriver=nvidia@N`): its window is BAR 0; the rest of
/// it is reached through [`PciFn`].
pub const KIND_NVIDIA: usize = 12;

/// The rest of a PCI function this process holds, beyond the window of its
/// claim (`ECLIPSE_DEVICE_PCI_FN`): its other memory BARs, and its
/// configuration space, which the kernel reads and writes for it.
#[derive(Debug, Clone, Copy)]
pub struct PciFn {
    kind: usize,
    index: usize,
}

const PCI_FN_BAR: usize = 0;
const PCI_FN_CONFIG_READ: usize = 1;
const PCI_FN_CONFIG_WRITE: usize = 2;
const PCI_FN_PLACE: usize = 3;
const PCI_FN_BAR_LEN: usize = 4;

impl PciFn {
    /// Device `index` of claim `kind` ([`KIND_NVIDIA`]).
    pub fn of(kind: usize, index: usize) -> PciFn {
        PciFn { kind, index }
    }

    /// The `index`th function of identity `vendor:device` ([`claim_pci`]).
    pub fn of_pci(vendor: u16, device: u16, index: usize) -> PciFn {
        let id = (vendor as usize) << 16 | device as usize;
        PciFn {
            kind: KIND_PCI | id << 32,
            index,
        }
    }

    fn call(&self, op: usize, a: usize, b: usize, c: usize) -> Result<usize, Status> {
        // SAFETY: no memory of this process is named.
        unsafe {
            zx::call(
                nr::ECLIPSE_DEVICE_PCI_FN,
                [self.kind, self.index, op, a, b, c],
            )
        }
    }

    /// A VMO of memory BAR `bar`, write-combining when `combine` (an
    /// aperture of memory rather than registers), uncached otherwise.
    pub fn bar(&self, bar: usize, combine: bool) -> Result<Handle, Status> {
        self.call(PCI_FN_BAR, bar, combine as usize, 0)
            .map(|h| h as Handle)
    }

    /// `width` bytes (1, 2 or 4) of the configuration space at `offset`.
    pub fn config_read(&self, offset: usize, width: usize) -> Result<u32, Status> {
        self.call(PCI_FN_CONFIG_READ, offset, width, 0)
            .map(|v| v as u32)
    }

    /// Write the low `width` bytes of `value` at `offset`.
    /// [`Status::ACCESS_DENIED`] for a register only the kernel sets: the
    /// identity, the BARs, the ROM.
    pub fn config_write(&self, offset: usize, width: usize, value: u32) -> Result<(), Status> {
        self.call(PCI_FN_CONFIG_WRITE, offset, width, value as usize)
            .map(|_| ())
    }

    /// Where the function is: `(bus, device, function)`.
    pub fn place(&self) -> Result<(u8, u8, u8), Status> {
        self.call(PCI_FN_PLACE, 0, 0, 0)
            .map(|v| ((v >> 8) as u8, (v >> 3 & 0x1f) as u8, (v & 7) as u8))
    }

    /// The length of memory BAR `bar`, 0 when it is not one (an I/O BAR,
    /// the upper half of a 64-bit one, none): what sizing it would say,
    /// which only the kernel may do.
    pub fn bar_len(&self, bar: usize) -> Result<usize, Status> {
        self.call(PCI_FN_BAR_LEN, bar, 0, 0)
    }
}

const TRANSPORT_VIRTIO_MMIO: u32 = 1;
const TRANSPORT_VIRTIO_PCI: u32 = 2;
const TRANSPORT_PCI: u32 = 3;
const TRANSPORT_PORTS: u32 = 4;

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
    /// not know, and for a PCI function that is not virtio.
    pub transport: Option<Transport>,
    /// Whether it is a PCI function that is not virtio, whose window is its
    /// first memory BAR: registers as its datasheet lays them out.
    pub pci: bool,
    /// Whether its window is I/O ports, from port `offset`, `size` of them:
    /// there are no registers to map.
    pub ports: bool,
    /// The interrupt line, for the record (`u32::MAX` when unknown).
    pub irq: u32,
    /// Where the register window starts inside the VMO, and its size.
    pub offset: u64,
    pub size: u64,
    /// The device's interrupt object, `None` when there is none to wait on.
    pub interrupt: Option<Handle>,
    /// A plain PCI function's `vendor << 16 | device`, when the kernel says
    /// (it does for an e1000e); 0 otherwise, and from a kernel that did
    /// not yet.
    pub pci_id: u32,
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
            pci: low(w[1]) == TRANSPORT_PCI,
            ports: low(w[1]) == TRANSPORT_PORTS,
            irq: high(w[1]),
            offset: w[2],
            size: w[3],
            interrupt: match low(w[7]) {
                0 => None,
                handle => Some(handle),
            },
            pci_id: if low(w[1]) == TRANSPORT_PCI {
                low(w[4])
            } else {
                0
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
    claim_with(kind, index, 0, patience)
}

/// Claim the `index`th PCI function whose vendor is `vendor` and device
/// `device` (`udriver=pci-VVVV-DDDD@N`), as [`claim`] does. Such a device
/// is not reset by the kernel when its driver dies: it stays out of use
/// until the next boot.
pub fn claim_pci(vendor: u16, device: u16, index: usize, patience: u32) -> Result<Claim, Status> {
    claim_with(
        KIND_PCI,
        index,
        (vendor as usize) << 16 | device as usize,
        patience,
    )
}

/// [`claim`] with argument 3 of the call, the identity for [`KIND_PCI`].
fn claim_with(kind: usize, index: usize, id: usize, patience: u32) -> Result<Claim, Status> {
    let mut words = [0u64; 8];
    let mut tries = patience.max(1);
    loop {
        // SAFETY: the kernel writes eight words where `words` has room for them.
        let r = unsafe {
            zx::call(
                nr::ECLIPSE_DEVICE_CLAIM,
                [kind, index, words.as_mut_ptr() as usize, id, 0, 0],
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

/// What the boot knows of the display (`ECLIPSE_BOOT_DISPLAY`), into
/// `out`, as `zcore_drivers::display::boot_display` lays it out: for the
/// driver of a GPU this process holds, which the kernel's is told at boot.
/// `ACCESS_DENIED` to a process that holds no GPU.
pub fn boot_display(out: &mut [u8]) -> Result<(), Status> {
    // SAFETY: the kernel writes at most `out.len()` bytes into `out`.
    unsafe {
        zx::call(
            nr::ECLIPSE_BOOT_DISPLAY,
            [out.as_mut_ptr() as usize, out.len(), 0, 0, 0, 0],
        )
    }
    .map(|_| ())
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
    /// The pin that gives the device its address, for [`Dma::release`].
    pub pmt: Handle,
    /// The pins that give the other devices of [`Dma::new_on`] the same
    /// address, for [`Dma::release_on`].
    pub also: Vec<Handle>,
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
        Dma::new_on(&[bti], len, window)
    }

    /// The same, for every device of `btis`, the first one's: one process
    /// driving several (a GPU per initiator) whose memory any of them may
    /// reach. Each pin is a grant of the device's IOMMU, and each must
    /// give the same device address, the one the first gives (the kernel
    /// maps device memory at its physical address), or the memory is
    /// refused.
    pub fn new_on(
        btis: &[Handle],
        len: usize,
        window: Option<(usize, usize)>,
    ) -> Result<Dma, String> {
        assert!(len.is_multiple_of(PAGE));
        let (&bti, others) = btis.split_first().expect("at least one device");
        let mut vmo: Handle = 0;
        // SAFETY: the kernel writes a handle into `vmo`.
        unsafe {
            zx::call(
                nr::VMO_CREATE_CONTIGUOUS,
                [bti as usize, len, 0, &mut vmo as *mut Handle as usize, 0, 0],
            )
        }
        .map_err(|e| format!("vmo_create_contiguous: {e}"))?;
        let pin = |bti: Handle| -> Result<(u64, Handle), String> {
            let mut daddr: u64 = 0;
            let mut pmt: Handle = 0;
            // SAFETY: the kernel writes one address into `daddr` (a
            // contiguous pin has one) and a handle into `pmt`.
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
            Ok((daddr, pmt))
        };
        let unpin = |pmt: Handle| {
            // SAFETY: no pointers; `pmt` is consumed.
            let _ = unsafe { zx::call(nr::PMT_UNPIN, [pmt as usize, 0, 0, 0, 0, 0]) };
        };
        let (daddr, pmt) = pin(bti).inspect_err(|_| zx::close(vmo))?;
        let mut also = Vec::with_capacity(others.len());
        for &other in others {
            let refused = match pin(other) {
                Ok((there, pmt)) if there == daddr => {
                    also.push(pmt);
                    continue;
                }
                Ok((there, pmt)) => {
                    unpin(pmt);
                    format!("another device sees it at {there:#x}, not {daddr:#x}")
                }
                Err(e) => e,
            };
            also.into_iter().chain([pmt]).for_each(unpin);
            zx::close(vmo);
            return Err(refused);
        }
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
            pmt,
            also,
        })
    }

    /// Give the memory back: the device loses it (`zx_pmt_unpin` takes the
    /// IOMMU grant first), then the mapping, which holds the VMO's last
    /// reference here, goes. A driver that allocates and frees buffers for
    /// as long as it runs (a GPU's GEM objects) needs this, or every buffer
    /// it ever made stays pinned. The kernel keeps any frame a client still
    /// maps by its physical address until that mapping goes.
    ///
    /// Only for memory made without a window: a window is a second VMO
    /// someone else may hold, and handing the pages back under it is not
    /// this call's to decide.
    pub fn release(ptr: *mut u8, len: usize, pmt: Handle) -> Result<(), Status> {
        Dma::release_on(ptr, len, pmt, Vec::new())
    }

    /// The same for memory of [`Dma::new_on`]: every device loses it
    /// before it goes. One pin refused leaves the memory mapped here (a
    /// device may still reach it), its error returned.
    pub fn release_on(
        ptr: *mut u8,
        len: usize,
        pmt: Handle,
        also: Vec<Handle>,
    ) -> Result<(), Status> {
        let mut failed = Ok(());
        for other in also {
            // SAFETY: no pointers; `other` is consumed.
            if let Err(e) = unsafe { zx::call(nr::PMT_UNPIN, [other as usize, 0, 0, 0, 0, 0]) } {
                failed = Err(e);
            }
        }
        failed?;
        // SAFETY: no pointers; `pmt` is consumed.
        unsafe { zx::call(nr::PMT_UNPIN, [pmt as usize, 0, 0, 0, 0, 0]) }?;
        // SAFETY: `ptr..ptr+len` is the mapping `new` made, which the caller
        // no longer uses.
        unsafe { libc::munmap(ptr as *mut libc::c_void, len) };
        Ok(())
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

/// Read `width` bytes (1, 2 or 4) at port `offset` of the window of
/// device `index` of claim `kind`, which this process holds: the kernel
/// does the `in` (`ECLIPSE_DEVICE_PORT`).
pub fn port_in(kind: usize, index: usize, offset: usize, width: usize) -> Result<u32, Status> {
    // SAFETY: no pointers.
    unsafe { zx::call(nr::ECLIPSE_DEVICE_PORT, [kind, index, offset, width, 0, 0]) }
        .map(|v| v as u32)
}

/// Write `width` bytes of `value` at port `offset`, as [`port_in`] reads.
pub fn port_out(
    kind: usize,
    index: usize,
    offset: usize,
    width: usize,
    value: u32,
) -> Result<(), Status> {
    // SAFETY: no pointers.
    unsafe {
        zx::call(
            nr::ECLIPSE_DEVICE_PORT,
            [kind, index, offset, width, value as usize, 1],
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
        let c = Claim::from_words(&[0, 9, 0, 0x1000, 0, 0, 0, 0]);
        assert_eq!(c.transport, None);
        assert!(!c.pci && !c.ports);
    }

    #[test]
    fn a_pci_function_that_is_not_virtio_has_its_bar_and_no_virtio_transport() {
        let c = Claim::from_words(&[
            0x0000_0004_0000_0003,
            0x0000_0029_0000_0003, // plain PCI, vector 0x29
            0,
            0x2_0000,
            0,
            0,
            0,
            6,
        ]);
        assert!(c.pci);
        assert_eq!(c.transport, None);
        assert_eq!((c.irq, c.interrupt), (0x29, Some(6)));
        assert_eq!(c.span(), 0x2_0000);
        assert_eq!(c.pci_id, 0, "a kernel that does not say");
        let virtio = Claim::from_words(&[0, 1, 0, 0x200, 0, 0, 0, 0]);
        assert!(!virtio.pci);
    }

    #[test]
    fn a_pci_function_says_which_part_it_is_in_word_four() {
        let c = Claim::from_words(&[0, 3, 0, 0x2_0000, 0x8086_15b8, 0, 0, 0]);
        assert_eq!(c.pci_id, 0x8086_15b8);
        // A virtio function's word four is its structures, not an identity.
        let v = Claim::from_words(&[0, 2, 0, 0x4000, 0x8086_15b8, 0, 0, 0]);
        assert_eq!(v.pci_id, 0);
    }

    #[test]
    fn a_device_on_ports_has_its_first_port_and_no_registers() {
        let c = Claim::from_words(&[
            0x0000_0004_0000_0000, // no regs, bti 4
            0x0000_0001_0000_0004, // ports, line 1
            0x60,
            5,
            0,
            0,
            0,
            8,
        ]);
        assert!(c.ports && !c.pci);
        assert_eq!(c.transport, None);
        assert_eq!((c.regs, c.offset, c.size), (0, 0x60, 5));
        assert_eq!((c.irq, c.interrupt), (1, Some(8)));
    }

    #[test]
    fn a_window_that_ends_inside_a_page_maps_that_whole_page() {
        let c = Claim::from_words(&[0, 1, 0x1e00, 0x400, 0, 0, 0, 0]);
        assert_eq!(c.span(), 0x3000);
    }
}
