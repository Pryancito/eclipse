//! NVMe (NVM Express) Driver
//!
//! Uses the nvme-oxide crate for PCIe NVMe disk access.
//! Supports multiple NVMe controllers and all namespaces on each controller.
//! Each namespace is registered as an independent block device (disk:N).

use alloc::sync::Arc;
use nvme_oxide::{Dma, NVMeDev};
use crate::{memory, serial, pci};

/// Eclipse OS implementation of the nvme-oxide Dma trait.
pub struct EclipseNvmeDma;

impl Dma for EclipseNvmeDma {
    /// Allocates physically contiguous memory. Returns virtual address.
    unsafe fn alloc(&self, size: usize, align: usize) -> Option<usize> {
        memory::alloc_dma_buffer(size, align)
            .map(|(ptr, _)| ptr as usize)
    }

    unsafe fn free(&self, _addr: usize, _size: usize, _align: usize) {
        // Eclipse's alloc_dma_buffer has no free - memory leaks on drop
    }

    /// Maps MMIO region. Returns virtual address.
    unsafe fn map_mmio(&self, phys: usize, size: usize) -> Option<usize> {
        let virt = memory::map_mmio_range(phys as u64, size);
        if virt == 0 { None } else { Some(virt as usize) }
    }

    unsafe fn unmap_mmio(&self, _virt: usize, _size: usize) {
        // Eclipse HHDM mapping persists
    }

    fn virt_to_phys(&self, va: usize) -> usize {
        memory::virt_to_phys(va as u64) as usize
    }

    fn page_size(&self) -> usize { 4096 }
}

// ── Eclipse OS block size ────────────────────────────────────────────────────

/// Block size used by the Eclipse OS storage layer (4 KiB).
const ECLIPSE_BLOCK_SIZE: usize = 4096;

// ── NvmeDisk — one NVMe namespace exposed as a block device ─────────────────

struct NvmeDisk {
    /// The NVMe namespace Arc — keeps both namespace and device alive.
    ns: Arc<nvme_oxide::Ns<EclipseNvmeDma>>,
    /// NVMe logical block size in bytes (typically 512 or 4096).
    blk_sz: usize,
    /// Drive capacity expressed in 4096-byte Eclipse blocks.
    total_blocks: u64,
    /// Keeps the NVMeDev (and its DMA allocations) alive for the lifetime
    /// of this disk handle, even if no other Arc to the device exists.
    _dev: Arc<NVMeDev<EclipseNvmeDma>>,
}

// NvmeDisk holds raw pointers inside nvme-oxide types.  Eclipse OS runs a
// cooperative, single-threaded kernel scheduler, so cross-thread aliasing
// cannot occur in practice.
unsafe impl Send for NvmeDisk {}
unsafe impl Sync for NvmeDisk {}

impl crate::storage::BlockDevice for NvmeDisk {
    fn read(&self, block: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        if buffer.len() != ECLIPSE_BLOCK_SIZE {
            return Err("Buffer must be 4096 bytes");
        }
        if self.blk_sz == 0 {
            return Err("NVMe: zero block size");
        }
        if self.blk_sz >= ECLIPSE_BLOCK_SIZE {
            // e.g. blk_sz == 4096: one NVMe block = one Eclipse block.
            self.ns.read(block, buffer).map_err(|_| "NVMe read failed")
        } else {
            // e.g. blk_sz == 512: read multiple NVMe sectors per Eclipse block.
            let spb = (ECLIPSE_BLOCK_SIZE / self.blk_sz) as u64; // sectors per block
            let lba_base = block * spb;
            let mut off = 0usize;
            for i in 0..spb {
                self.ns.read(lba_base + i, &mut buffer[off..off + self.blk_sz])
                    .map_err(|_| "NVMe read failed")?;
                off += self.blk_sz;
            }
            Ok(())
        }
    }

    fn write(&self, block: u64, buffer: &[u8]) -> Result<(), &'static str> {
        if buffer.len() != ECLIPSE_BLOCK_SIZE {
            return Err("Buffer must be 4096 bytes");
        }
        if self.blk_sz == 0 {
            return Err("NVMe: zero block size");
        }
        
        // DIAGNOSTIC: Log writes to verify if seatd/labwc are triggering disk I/O
        // crate::serial::serial_printf(format_args!("[NVMe] write block {}\n", block));

        if self.blk_sz >= ECLIPSE_BLOCK_SIZE {
            self.ns.write(block, buffer).map_err(|_| "NVMe write failed")
        } else {
            let spb = (ECLIPSE_BLOCK_SIZE / self.blk_sz) as u64;
            let lba_base = block * spb;
            let mut off = 0usize;
            for i in 0..spb {
                self.ns.write(lba_base + i, &buffer[off..off + self.blk_sz])
                    .map_err(|_| "NVMe write failed")?;
                off += self.blk_sz;
            }
            Ok(())
        }
    }

    fn capacity(&self) -> u64 { self.total_blocks }

    fn name(&self) -> &'static str { "NVMe" }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Initialise NVMe drivers for every detected controller.
/// Each namespace on each controller is registered as an independent block device.
pub fn init() {
    let controllers = pci::find_all_nvme_controllers();
    if controllers.is_empty() {
        return;
    }
    for (idx, dev) in controllers.iter().enumerate() {
        serial::serial_print("[NVMe] Initialising controller ");
        serial::serial_print_dec(idx as u64);
        serial::serial_print("\n");
        init_controller(dev);
    }
}

fn init_controller(dev: &pci::PciDevice) {
    // NVMe BAR0 is the 64-bit MMIO Controller Memory Buffer (CMB).
    let bar0_raw = unsafe { pci::get_bar(dev, 0) };
    let bar0_phys = (bar0_raw & !0xF) as usize;
    if bar0_phys == 0 {
        serial::serial_print("[NVMe] No valid BAR0 — skipping controller\n");
        return;
    }

    unsafe { pci::enable_device(dev, true); }

    match NVMeDev::new(bar0_phys, EclipseNvmeDma) {
        Ok(arc_dev) => {
            // ns_list() returns a borrowed slice; clone into an owned Vec so we
            // can append the NS-1 fallback when the list is empty.
            let mut namespaces: alloc::vec::Vec<Arc<nvme_oxide::Ns<EclipseNvmeDma>>> =
                arc_dev.ns_list().to_vec();
            if namespaces.is_empty() {
                if let Some(ns1) = arc_dev.ns(1) {
                    namespaces.push(ns1);
                } else {
                    serial::serial_print("[NVMe] No namespaces found\n");
                    return;
                }
            }

            for ns in namespaces {
                let blk_sz  = ns.blk_sz();
                let blk_cnt = ns.blk_cnt();

                if blk_sz == 0 {
                    serial::serial_print("[NVMe] Namespace with blk_sz=0, skipping\n");
                    continue;
                }

                // Convert raw NVMe block count to 4096-byte Eclipse blocks.
                let total_blocks = if blk_sz >= ECLIPSE_BLOCK_SIZE {
                    blk_cnt * (blk_sz / ECLIPSE_BLOCK_SIZE) as u64
                } else {
                    blk_cnt / (ECLIPSE_BLOCK_SIZE / blk_sz) as u64
                };

                serial::serial_print("[NVMe] NS blk_sz=");
                serial::serial_print_dec(blk_sz as u64);
                serial::serial_print(" blk_cnt=");
                serial::serial_print_dec(blk_cnt);
                serial::serial_print(" -> ");
                serial::serial_print_dec(total_blocks);
                serial::serial_print(" 4KiB blocks\n");

                crate::storage::register_device(Arc::new(NvmeDisk {
                    ns,
                    blk_sz,
                    total_blocks,
                    _dev: arc_dev.clone(),
                }));
            }
        }
        Err(_) => {
            serial::serial_print("[NVMe] Controller init failed\n");
        }
    }
}

/// Returns `true` if at least one NVMe block device has been registered.
pub fn is_available() -> bool {
    for i in 0..crate::storage::device_count() {
        if let Some(dev) = crate::storage::get_device(i) {
            if dev.name() == "NVMe" {
                return true;
            }
        }
    }
    false
}
