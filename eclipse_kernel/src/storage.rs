use alloc::vec::Vec;
use alloc::sync::Arc;
use spin::Mutex;

/// Trait representing a block device with 4096-byte blocks.
pub trait BlockDevice: Send + Sync {
    fn read(&self, block: u64, buffer: &mut [u8]) -> Result<(), &'static str>;
    fn write(&self, block: u64, buffer: &[u8]) -> Result<(), &'static str>;
    fn capacity(&self) -> u64; // Capacity in 4096-byte blocks
    fn name(&self) -> &'static str; // Driver name (e.g., "VirtIO", "AHCI", "NVMe")
}

static DEVICES: Mutex<Vec<Arc<dyn BlockDevice>>> = Mutex::new(Vec::new());

/// Register a new block device in the global registry.
pub fn register_device(device: Arc<dyn BlockDevice>) {
    let mut devices = DEVICES.lock();
    crate::serial::serial_print("[STORAGE] Registered ");
    crate::serial::serial_print(device.name());
    crate::serial::serial_print(" device as disk:");
    crate::serial::serial_print_dec(devices.len() as u64);
    crate::serial::serial_print("\n");
    devices.push(device);
}

/// Get a device by index.
pub fn get_device(index: usize) -> Option<Arc<dyn BlockDevice>> {
    DEVICES.lock().get(index).cloned()
}

/// Get the number of registered block devices.
pub fn device_count() -> usize {
    DEVICES.lock().len()
}

/// Print all registered block devices to the serial console.
pub fn list_devices() {
    let devices = DEVICES.lock();
    if devices.is_empty() {
        crate::serial::serial_print("[STORAGE]   (none)\n");
        return;
    }
    for (i, dev) in devices.iter().enumerate() {
        let cap_blocks = dev.capacity();          // 4 KiB blocks
        let cap_mib   = cap_blocks * 4 / 1024;   // MiB
        let cap_gib   = cap_mib / 1024;           // GiB
        crate::serial::serial_print("disk:");
        crate::serial::serial_print_dec(i as u64);
        crate::serial::serial_print(" driver=");
        crate::serial::serial_print(dev.name());
        crate::serial::serial_print(" capacity=");
        if cap_gib > 0 {
            crate::serial::serial_print_dec(cap_gib);
            crate::serial::serial_print("G");
        } else {
            crate::serial::serial_print_dec(cap_mib);
            crate::serial::serial_print("M");
        }
    }
    crate::serial::serial_print("\n");
}

// ── DiskScheme ────────────────────────────────────────────────────────────────
// Exposes disk:0, disk:1, … as seekable byte streams via the kernel scheme
// registry.  Works for any block device registered in this module (VirtIO,
// AHCI, NVMe, ATA, …).  Belongs here — not in any specific driver.

use crate::scheme::{Scheme, Stat, error as scheme_error};

struct OpenDisk {
    disk_idx: usize,
    offset: u64,            // byte offset *within this view* (0 = start of partition or disk)
    partition_offset: u64,  // byte offset of the partition start on the raw disk (0 = whole disk)
    partition_size: u64,    // byte length of the partition (u64::MAX = whole disk, no bound)
}

static OPEN_DISKS: Mutex<Vec<Option<OpenDisk>>> = Mutex::new(Vec::new());

pub struct DiskScheme;

impl Scheme for DiskScheme {
    fn open(&self, path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        // Accept three formats:
        //   "N"        → disk:N        (raw disk, legacy offset 25856 in kernel)
        //   "NpM"      → disk:NpM      (partition M of disk N, GPT-based)
        //   "N@OFFSET" → disk:N@OFFSET (raw disk N starting at block OFFSET)
        let path = path.trim_start_matches('/');

        let (disk_idx, part_info): (usize, Option<(u64, u64)>) =
            if let Some(p_pos) = path.find('p') {
                let n = path[..p_pos].parse::<usize>().map_err(|_| scheme_error::EINVAL)?;
                let m = path[p_pos+1..].parse::<usize>().map_err(|_| scheme_error::EINVAL)?;
                let range = gpt_partition_range(n, m).ok_or(scheme_error::ENOENT)?;
                (n, Some(range))
            } else if let Some(at_pos) = path.find('@') {
                let n = path[..at_pos].parse::<usize>().map_err(|_| scheme_error::EINVAL)?;
                let off_blocks = path[at_pos+1..].parse::<u64>().map_err(|_| scheme_error::EINVAL)?;
                (n, Some((off_blocks * 4096, u64::MAX)))
            } else {
                (path.parse::<usize>().map_err(|_| scheme_error::EINVAL)?, None)
            };

        if get_device(disk_idx).is_none() {
            crate::serial::serial_print("[DISK-SCHEME] open failed: disk:");
            crate::serial::serial_print_dec(disk_idx as u64);
            crate::serial::serial_print(" not found (total=");
            crate::serial::serial_print_dec(device_count() as u64);
            crate::serial::serial_print(")\n");
            return Err(scheme_error::ENOENT);
        }

        let (partition_offset, partition_size) = part_info.unwrap_or((0, u64::MAX));

        let od = OpenDisk { disk_idx, offset: 0, partition_offset, partition_size };
        let mut open_disks = OPEN_DISKS.lock();
        for (i, slot) in open_disks.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(od);
                return Ok(i);
            }
        }
        let id = open_disks.len();
        open_disks.push(Some(od));
        Ok(id)
    }

    fn read(&self, id: usize, buffer: &mut [u8]) -> Result<usize, usize> {
        let mut open_disks = OPEN_DISKS.lock();
        let disk = open_disks.get_mut(id).and_then(|s| s.as_mut()).ok_or(scheme_error::EBADF)?;

        // Clamp to partition bounds
        let avail_in_partition = if disk.partition_size == u64::MAX {
            buffer.len() as u64
        } else {
            disk.partition_size.saturating_sub(disk.offset)
        };
        if avail_in_partition == 0 { return Ok(0); }
        let read_len = core::cmp::min(buffer.len() as u64, avail_in_partition) as usize;

        let abs_offset = disk.partition_offset + disk.offset;
        let block_num = abs_offset / 4096;
        let offset_in_block = (abs_offset % 4096) as usize;

        let mut temp = alloc::vec![0u8; 4096];
        let ok = crate::bcache::read_block(disk.disk_idx, block_num, &mut temp).is_ok();

        if !ok {
            crate::serial::serial_print("[DISK-SCHEME] read failed: disk:");
            crate::serial::serial_print_dec(disk.disk_idx as u64);
            crate::serial::serial_print(" block=");
            crate::serial::serial_print_dec(block_num);
            crate::serial::serial_print("\n");
            return Err(scheme_error::EIO);
        }

        let available = 4096 - offset_in_block;
        let to_copy = core::cmp::min(read_len, available);
        buffer[..to_copy].copy_from_slice(&temp[offset_in_block..offset_in_block + to_copy]);
        disk.offset += to_copy as u64;
        Ok(to_copy)
    }

    fn write(&self, id: usize, buffer: &[u8]) -> Result<usize, usize> {
        if buffer.is_empty() { return Ok(0); }

        let mut open_disks = OPEN_DISKS.lock();
        let disk = open_disks.get_mut(id).and_then(|s| s.as_mut()).ok_or(scheme_error::EBADF)?;

        // Clamp to partition bounds
        let avail_in_partition = if disk.partition_size == u64::MAX {
            buffer.len() as u64
        } else {
            disk.partition_size.saturating_sub(disk.offset)
        };
        if avail_in_partition == 0 { return Ok(0); }
        let write_len = core::cmp::min(buffer.len() as u64, avail_in_partition) as usize;

        let abs_offset = disk.partition_offset + disk.offset;
        let block_num = abs_offset / 4096;
        let offset_in_block = (abs_offset % 4096) as usize;
        let to_copy = core::cmp::min(write_len, 4096 - offset_in_block);

        // Read-modify-write for partial blocks
        if offset_in_block != 0 || to_copy != 4096 {
            let mut temp = alloc::vec![0u8; 4096];
            if crate::bcache::read_block(disk.disk_idx, block_num, &mut temp).is_err() {
                return Err(scheme_error::EIO);
            }
            temp[offset_in_block..offset_in_block + to_copy].copy_from_slice(&buffer[..to_copy]);
            if crate::bcache::write_block(disk.disk_idx, block_num, &temp).is_err() {
                return Err(scheme_error::EIO);
            }
        } else {
            if crate::bcache::write_block(disk.disk_idx, block_num, buffer).is_err() {
                return Err(scheme_error::EIO);
            }
        }

        disk.offset += to_copy as u64;
        Ok(to_copy)
    }

    fn lseek(&self, id: usize, offset: isize, whence: usize) -> Result<usize, usize> {
        let mut open_disks = OPEN_DISKS.lock();
        let disk = open_disks.get_mut(id).and_then(|s| s.as_mut()).ok_or(scheme_error::EBADF)?;

        let view_size = if disk.partition_size == u64::MAX {
            get_device(disk.disk_idx).map(|d| d.capacity() * 4096).unwrap_or(0)
        } else {
            disk.partition_size
        };

        let new_offset: u64 = match whence {
            0 => offset as u64,                                    // SEEK_SET
            1 => (disk.offset as isize + offset) as u64,          // SEEK_CUR
            2 => (view_size as isize + offset) as u64,            // SEEK_END
            _ => return Err(scheme_error::EINVAL),
        };
        disk.offset = new_offset;
        Ok(new_offset as usize)
    }

    fn close(&self, id: usize) -> Result<usize, usize> {
        let mut open_disks = OPEN_DISKS.lock();
        if let Some(slot) = open_disks.get_mut(id) {
            *slot = None;
            Ok(0)
        } else {
            Err(scheme_error::EBADF)
        }
    }

    fn fstat(&self, id: usize, stat: &mut Stat) -> Result<usize, usize> {
        let open_disks = OPEN_DISKS.lock();
        let disk = open_disks.get(id).and_then(|s| s.as_ref()).ok_or(scheme_error::EBADF)?;
        let cap_bytes = if disk.partition_size == u64::MAX {
            get_device(disk.disk_idx).map(|d| d.capacity() * 4096).unwrap_or(0)
        } else {
            disk.partition_size
        };
        stat.size = cap_bytes;
        stat.blksize = 4096;
        stat.blocks = cap_bytes / 512;
        Ok(0)
    }
}

/// Read the GPT partition table of disk `disk_idx` and return the byte offset
/// and byte length of the 1-based partition number `part_num`.
/// Returns None if the disk has no GPT, or the partition does not exist.
fn gpt_partition_range(disk_idx: usize, part_num: usize) -> Option<(u64, u64)> {
    if part_num == 0 { return None; }

    // GPT header is at LBA 1 (byte offset 512). Our block size is 4096 bytes,
    // so the GPT header sits inside block 0 starting at byte 512.
    let mut block0 = alloc::vec![0u8; 4096];
    crate::storage::get_device(disk_idx)?.read(0, &mut block0).ok()?;

    // Check GPT signature at byte 512.
    const GPT_SIG: &[u8] = b"EFI PART";
    if &block0[512..520] != GPT_SIG { return None; }

    let h = &block0[512..];
    let part_entry_lba = u64::from_le_bytes([h[72],h[73],h[74],h[75],h[76],h[77],h[78],h[79]]);
    let num_entries    = u32::from_le_bytes([h[80],h[81],h[82],h[83]]) as usize;
    let entry_size     = u32::from_le_bytes([h[84],h[85],h[86],h[87]]) as usize;

    if entry_size < 128 || num_entries == 0 || part_num > num_entries { return None; }

    // Locate the partition entry array.
    let part_table_block       = part_entry_lba / 8;  // 512-byte LBA → 4096-byte block
    let part_table_byte_offset = ((part_entry_lba % 8) * 512) as usize;

    let total_bytes   = num_entries * entry_size;
    let blocks_needed = ((part_table_byte_offset + total_bytes + 4095) / 4096).min(8);

    let mut part_buf = alloc::vec![0u8; blocks_needed * 4096];
    for b in 0..blocks_needed {
        let slice = &mut part_buf[b * 4096..(b + 1) * 4096];
        if crate::storage::get_device(disk_idx)?.read(part_table_block + b as u64, slice).is_err() {
            return None;
        }
    }

    // GPT partition numbers are 1-based in user-facing notation.
    // We iterate entries in order, skipping unused ones, and count only valid entries.
    let mut valid_count = 0usize;
    for i in 0..num_entries {
        let entry_start = part_table_byte_offset + i * entry_size;
        if entry_start + 128 > part_buf.len() { break; }
        let e = &part_buf[entry_start..];

        // Type GUID at offset 0 — all zeros means unused.
        if e[0..16].iter().all(|&b| b == 0) { continue; }

        valid_count += 1;
        if valid_count == part_num {
            // StartingLBA at +32, EndingLBA at +40 (both 512-byte LBAs).
            let start_lba = u64::from_le_bytes([e[32],e[33],e[34],e[35],e[36],e[37],e[38],e[39]]);
            let end_lba   = u64::from_le_bytes([e[40],e[41],e[42],e[43],e[44],e[45],e[46],e[47]]);
            let start_byte = start_lba * 512;
            let size_bytes = (end_lba - start_lba + 1) * 512;
            crate::serial::serial_printf(format_args!("[STORAGE] GPT disk:{}p{} -> start_blk={} size_mib={}\n", 
                disk_idx, part_num, start_byte / 4096, size_bytes / 1024 / 1024));
            return Some((start_byte, size_bytes));
        }
    }

    None
}

/// Register the disk: scheme in the kernel scheme registry.
/// Call this from main.rs after ALL storage drivers have been initialised
/// (VirtIO, NVMe, AHCI, ATA) so every device is already in the registry.
pub fn register_disk_scheme() {
    crate::serial::serial_print("[STORAGE] Registering disk: scheme\n");
    crate::scheme::register_scheme("disk", Arc::new(DiskScheme));
}
