//! NVMe (NVM Express) Driver
//!
//! Uses the nvme-oxide crate for PCIe NVMe disk access.
//! Integrates with bcache for block I/O.

use spin::Mutex;
use nvme_oxide::{Dma, NVMeDev};
use alloc::sync::Arc;
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

    fn page_size(&self) -> usize {
        4096
    }
}

static NVME_DEV: Mutex<Option<Arc<NVMeDev<EclipseNvmeDma>>>> = Mutex::new(None);
static NVME_NS: Mutex<Option<Arc<nvme_oxide::Ns<EclipseNvmeDma>>>> = Mutex::new(None);
static NVME_BLK_SZ: Mutex<usize> = Mutex::new(4096);

/// Initialize NVMe driver if a controller is present.
pub fn init() {
    let nvme_dev = pci::find_nvme_controller();
    let Some(dev) = nvme_dev else {
        return;
    };

    let bar0_phys = unsafe { pci::get_bar(&dev, 0) };
    let bar0_phys = (bar0_phys & !0xF) as usize;
    if bar0_phys == 0 {
        serial::serial_print("[NVMe] No valid BAR0\n");
        return;
    }

    unsafe { pci::enable_device(&dev, true); }

    let dma = EclipseNvmeDma;
    match NVMeDev::new(bar0_phys, dma) {
        Ok(arc_dev) => {
            serial::serial_print("[NVMe] Controller initialized\n");

            let ns = arc_dev.ns(1).or_else(|| arc_dev.ns_list().first().cloned());
            if let Some(ns) = ns {
                let blk_sz = ns.blk_sz();
                let blk_cnt = ns.blk_cnt();
                serial::serial_print("[NVMe] NS1: blk_sz=");
                serial::serial_print_dec(blk_sz as u64);
                serial::serial_print(" blk_cnt=");
                serial::serial_print_dec(blk_cnt);
                serial::serial_print("\n");

                *NVME_DEV.lock() = Some(arc_dev);
                *NVME_NS.lock() = Some(ns);
                *NVME_BLK_SZ.lock() = blk_sz;

                crate::storage::register_device(Arc::new(NvmeDisk));
            } else {
                serial::serial_print("[NVMe] No namespace found\n");
            }
        }
        Err(_e) => {
            serial::serial_print("[NVMe] Init failed\n");
        }
    }
}

struct NvmeDisk;

impl crate::storage::BlockDevice for NvmeDisk {
    fn read(&self, block: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        read_block(block, buffer)
    }

    fn write(&self, block: u64, buffer: &[u8]) -> Result<(), &'static str> {
        write_block(block, buffer)
    }

    fn capacity(&self) -> u64 {
        let ns = NVME_NS.lock();
        if let Some(ns) = ns.as_ref() {
            let blk_sz = *NVME_BLK_SZ.lock();
            let sectors_per_block = 4096 / blk_sz as u64;
            ns.blk_cnt() / sectors_per_block
        } else {
            0
        }
    }

    fn name(&self) -> &'static str {
        "NVMe"
    }
}

fn read_ns(lba: u64, buf: &mut [u8]) -> Result<(), &'static str> {
    let ns = NVME_NS.lock();
    let Some(ns) = ns.as_ref() else { return Err("No NVMe namespace"); };
    ns.read(lba, buf).map_err(|_| "NVMe read failed")
}

fn write_ns(lba: u64, buf: &[u8]) -> Result<(), &'static str> {
    let ns = NVME_NS.lock();
    let Some(ns) = ns.as_ref() else { return Err("No NVMe namespace"); };
    ns.write(lba, buf).map_err(|_| "NVMe write failed")
}

/// Read one 4096-byte block. Bcache uses 4096; NVMe may use 512 or 4096.
pub fn read_block(block_num: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
    if buffer.len() != 4096 {
        return Err("Buffer must be 4096 bytes");
    }
    let blk_sz = *NVME_BLK_SZ.lock();
    if blk_sz == 4096 {
        read_ns(block_num, buffer)
    } else {
        let lba_base = block_num * 8;
        let mut off = 0;
        for i in 0..8 {
            let sector = &mut buffer[off..off + 512];
            read_ns(lba_base + i, sector)?;
            off += 512;
        }
        Ok(())
    }
}

/// Write one 4096-byte block.
pub fn write_block(block_num: u64, buffer: &[u8]) -> Result<(), &'static str> {
    if buffer.len() != 4096 {
        return Err("Buffer must be 4096 bytes");
    }
    let blk_sz = *NVME_BLK_SZ.lock();
    if blk_sz == 4096 {
        write_ns(block_num, buffer)
    } else {
        let lba_base = block_num * 8;
        let mut off = 0;
        for i in 0..8 {
            write_ns(lba_base + i, &buffer[off..off + 512])?;
            off += 512;
        }
        Ok(())
    }
}

/// Check if NVMe driver is available.
pub fn is_available() -> bool {
    NVME_NS.lock().is_some()
}
