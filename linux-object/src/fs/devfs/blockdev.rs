use alloc::sync::Arc;
use core::any::Any;
use core::convert::TryFrom;
use rcore_fs::vfs::{make_rdev, FileType, FsError, INode, Metadata, PollStatus, Result, Timespec};
use rcore_fs_devfs::DevFS;
use zcore_drivers::{scheme::BlockScheme, scheme::Scheme, DeviceError};

/// Linux `BLKGETSIZE64` — total size in bytes (`linux/fs.h`).
const BLKGETSIZE64: u32 = 0x8008_1272;
/// Linux `BLKGETSIZE` — size in 512-byte sectors (`linux/fs.h`).
const BLKGETSIZE: u32 = 0x0000_1260;
/// Linux `BLKFLSBUF` — flush block device buffers (`_IO(0x12,97)`).
const BLKFLSBUF: u32 = 0x0000_1261;

/// Block device INode.
pub struct BlockDev {
    index: usize,
    block: Arc<dyn BlockScheme>,
    inode_id: usize,
    name: alloc::string::String,
}

impl BlockDev {
    pub fn new(index: usize, block: Arc<dyn BlockScheme>, name: alloc::string::String) -> Self {
        Self {
            index,
            block,
            inode_id: DevFS::new_inode_id(),
            name,
        }
    }

    pub fn block_scheme(&self) -> Arc<dyn BlockScheme> {
        self.block.clone()
    }
}

impl INode for BlockDev {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        const BS: usize = 512;
        #[repr(align(4096))]
        struct AlignedBuf([u8; 512]);
        let mut temp_buf = AlignedBuf([0u8; 512]);
        let mut done = 0usize;

        // Partial leading sector.
        let head_off = offset % BS;
        if head_off != 0 && done < buf.len() {
            let take = buf.len().min(BS - head_off);
            self.block
                .read_block(offset / BS, &mut temp_buf.0)
                .map_err(convert_error)?;
            buf[..take].copy_from_slice(&temp_buf.0[head_off..head_off + take]);
            done += take;
        }

        // Whole-sector middle: a single multi-sector transfer.
        let mid = ((buf.len() - done) / BS) * BS;
        if mid > 0 {
            self.block
                .read_block((offset + done) / BS, &mut buf[done..done + mid])
                .map_err(convert_error)?;
            done += mid;
        }

        // Partial trailing sector.
        if done < buf.len() {
            let take = buf.len() - done;
            self.block
                .read_block((offset + done) / BS, &mut temp_buf.0)
                .map_err(convert_error)?;
            buf[done..].copy_from_slice(&temp_buf.0[..take]);
            done += take;
        }

        Ok(done)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        const BS: usize = 512;
        #[repr(align(4096))]
        struct AlignedBuf([u8; 512]);
        let mut temp_buf = AlignedBuf([0u8; 512]);
        let mut done = 0usize;

        // Partial leading sector: read-modify-write.
        let head_off = offset % BS;
        if head_off != 0 && done < buf.len() {
            let take = buf.len().min(BS - head_off);
            let block_id = offset / BS;
            self.block
                .read_block(block_id, &mut temp_buf.0)
                .map_err(convert_error)?;
            temp_buf.0[head_off..head_off + take].copy_from_slice(&buf[..take]);
            self.block
                .write_block(block_id, &temp_buf.0)
                .map_err(convert_error)?;
            done += take;
        }

        // Whole-sector middle: a single multi-sector transfer.
        let mid = ((buf.len() - done) / BS) * BS;
        if mid > 0 {
            self.block
                .write_block((offset + done) / BS, &buf[done..done + mid])
                .map_err(convert_error)?;
            done += mid;
        }

        // Partial trailing sector: read-modify-write.
        if done < buf.len() {
            let take = buf.len() - done;
            let block_id = (offset + done) / BS;
            self.block
                .read_block(block_id, &mut temp_buf.0)
                .map_err(convert_error)?;
            temp_buf.0[..take].copy_from_slice(&buf[done..]);
            self.block
                .write_block(block_id, &temp_buf.0)
                .map_err(convert_error)?;
            done += take;
        }

        Ok(done)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: true,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        let blocks = self.block.block_count();
        let size = blocks.saturating_mul(512);
        Ok(Metadata {
            dev: 1,
            inode: self.inode_id,
            size,
            blk_size: 512,
            blocks,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::BlockDevice,
            mode: 0o660, // owner & group read/write
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: make_rdev(3, self.index),
        })
    }

    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        let sectors = self.block.block_count() as u64;
        match cmd {
            BLKGETSIZE64 => {
                if data == 0 {
                    return Err(FsError::InvalidParam);
                }
                let size = sectors.saturating_mul(512);
                let mut out_ptr = kernel_hal::user::UserOutPtr::<u64>::from(data);
                out_ptr.write(size).map_err(|_| FsError::InvalidParam)?;
                Ok(0)
            }
            BLKGETSIZE => {
                if data == 0 {
                    return Err(FsError::InvalidParam);
                }
                let legacy = sectors as usize;
                let mut out_ptr = kernel_hal::user::UserOutPtr::<usize>::from(data);
                out_ptr.write(legacy).map_err(|_| FsError::InvalidParam)?;
                Ok(0)
            }
            0x0000_125f => {
                // BLKRRPART
                crate::fs::rescan_partitions(&self.name, &self.block, self.index)
                    .map_err(|_| FsError::DeviceError)?;
                Ok(0)
            }
            BLKFLSBUF => {
                // Eclipse escribe directamente al dispositivo (sin caché de
                // bloques), así que basta con delegar el flush del driver y
                // devolver éxito en lugar de ENOSYS. Herramientas como el
                // instalador lo invocan para asegurar la persistencia.
                let _ = self.block.flush();
                Ok(0)
            }
            _ => Err(FsError::NotSupported),
        }
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn sync_all(&self) -> Result<()> {
        Ok(())
    }

    fn sync_data(&self) -> Result<()> {
        Ok(())
    }
}

fn convert_error(e: DeviceError) -> FsError {
    match e {
        DeviceError::NotSupported => FsError::NotSupported,
        DeviceError::NotReady => FsError::Busy,
        DeviceError::InvalidParam => FsError::InvalidParam,
        DeviceError::BufferTooSmall
        | DeviceError::DmaError
        | DeviceError::IoError
        | DeviceError::AlreadyExists
        | DeviceError::NoResources => FsError::DeviceError,
    }
}

/// A wrapper block device that represents a partition on a physical block device.
pub struct PartitionBlock {
    parent: Arc<dyn BlockScheme>,
    name: alloc::string::String,
    start_block: usize,
    block_count: usize,
}

impl PartitionBlock {
    pub fn new(
        parent: Arc<dyn BlockScheme>,
        name: alloc::string::String,
        start_block: usize,
        block_count: usize,
    ) -> Self {
        Self {
            parent,
            name,
            start_block,
            block_count,
        }
    }
}

impl Scheme for PartitionBlock {
    fn name(&self) -> &str {
        &self.name
    }
    fn handle_irq(&self, irq_num: usize) {
        self.parent.handle_irq(irq_num);
    }
}

impl BlockScheme for PartitionBlock {
    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> zcore_drivers::DeviceResult {
        let nsectors = buf.len() / 512;
        if block_id + nsectors > self.block_count {
            return Err(zcore_drivers::DeviceError::InvalidParam);
        }
        self.parent.read_block(self.start_block + block_id, buf)
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) -> zcore_drivers::DeviceResult {
        let nsectors = buf.len() / 512;
        if block_id + nsectors > self.block_count {
            return Err(zcore_drivers::DeviceError::InvalidParam);
        }
        self.parent.write_block(self.start_block + block_id, buf)
    }

    fn flush(&self) -> zcore_drivers::DeviceResult {
        self.parent.flush()
    }

    fn block_count(&self) -> usize {
        self.block_count
    }

    // A partition sits on the parent disk and is addressed in the parent's
    // blocks, so anything inside it (a filesystem superblock, a nested table)
    // is laid out in those same units.
    fn logical_block_size(&self) -> usize {
        self.parent.logical_block_size()
    }
}

/// The unit `block_id`, `block_count` and every value this module returns are
/// expressed in: the `BlockScheme` API sector, always 512 bytes.
const API_SECTOR: usize = 512;

/// Largest device logical block we honour. 4096 covers every 4Kn disk on the
/// market; the cap keeps a garbage IDENTIFY / Identify-Namespace value from
/// turning a header read into a multi-megabyte transfer.
const MAX_LBS: usize = 4096;

/// Upper bound on the number of GPT entries we will walk. The header field is
/// device-supplied and 32 bits wide; the spec's own minimum is 128.
const MAX_GPT_ENTRIES: u32 = 512;

/// Upper bound on the length of an MBR extended-partition (EBR) chain. The
/// chain is a linked list living on the disk, so a corrupt or malicious link
/// can point backwards; this bounds the walk even if the visited check is
/// somehow evaded.
const MAX_EBR_LINKS: usize = 64;

#[repr(align(4096))]
struct AlignedBuf([u8; MAX_LBS]);

/// The device logical block size, sanity-checked. Anything that is not a power
/// of two in `512..=4096` means the driver reported nonsense, and assuming 512
/// is both the old behaviour and the safe one.
fn device_block_size(block: &Arc<dyn BlockScheme>) -> usize {
    let lbs = block.logical_block_size();
    if lbs.is_power_of_two() && (API_SECTOR..=MAX_LBS).contains(&lbs) {
        lbs
    } else {
        warn!(
            "[part] driver reported a logical block size of {} bytes; assuming {}",
            lbs, API_SECTOR
        );
        API_SECTOR
    }
}

/// A 512-aligned sliding window over the device.
///
/// The GPT partition array is addressed in bytes, not sectors: its first entry
/// sits at `PartitionEntryLBA * lbs` and each subsequent one `SizeOfPartitionEntry`
/// further along, a value the header is free to set to anything that is a
/// multiple of 128. So entries do not line up with sectors, and the reader has
/// to be able to hand out an arbitrary byte range. Keeping the last window
/// means the usual 128-byte-entry array costs one read per 4096 bytes rather
/// than one per entry.
struct Window {
    buf: AlignedBuf,
    /// Byte offset of `buf[0]` on the device, always a multiple of 512.
    start: u64,
    /// Valid bytes in `buf`; zero until the first read.
    len: usize,
}

impl Window {
    fn new() -> Self {
        Self {
            buf: AlignedBuf([0u8; MAX_LBS]),
            start: 0,
            len: 0,
        }
    }

    /// Bytes `off..off + len` of the device, or `None` if the read failed or
    /// the range is bigger than one window.
    fn get(&mut self, block: &Arc<dyn BlockScheme>, off: u64, len: usize) -> Option<&[u8]> {
        if len == 0 || len > MAX_LBS {
            return None;
        }
        let end = off.checked_add(len as u64)?;
        let cached = self.len != 0 && off >= self.start && end <= self.start + self.len as u64;
        if !cached {
            // Start the window on the 512-boundary at or below `off` and fill
            // the whole buffer, so a sequential walk of the entry array keeps
            // hitting the cache, then clamp it to what the device actually
            // has: the backup GPT header sits on the very last block, and a
            // read that ran past the end would be rejected outright.
            let start = off & !(API_SECTOR as u64 - 1);
            let first = start / API_SECTOR as u64;
            let need = (off - start) as usize + len;
            let avail = ((block.block_count() as u64).saturating_sub(first) as usize)
                .saturating_mul(API_SECTOR);
            let want = MAX_LBS.min(avail);
            if want < need {
                return None;
            }
            block
                .read_block(usize::try_from(first).ok()?, &mut self.buf.0[..want])
                .ok()?;
            self.start = start;
            self.len = want;
        }
        let rel = (off - self.start) as usize;
        Some(&self.buf.0[rel..rel + len])
    }
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// Turn a device-LBA range into the (start, count) pair in 512-byte API
/// sectors that the rest of the kernel speaks, rejecting anything that does
/// not fit inside the device.
fn to_api_sectors(
    start_lba: u64,
    count_lba: u64,
    lbs: usize,
    total_api: usize,
) -> Option<(usize, usize)> {
    let per = (lbs / API_SECTOR) as u64;
    let start = start_lba.checked_mul(per)?;
    let count = count_lba.checked_mul(per)?;
    if count == 0 {
        return None;
    }
    let end = start.checked_add(count)?;
    if end > total_api as u64 {
        return None;
    }
    Some((usize::try_from(start).ok()?, usize::try_from(count).ok()?))
}

/// Scans a block device for partition tables (MBR/GPT) and returns a vector
/// of partitions as (start_sector, size_sectors) pairs, in 512-byte sectors.
///
/// Everything a partition table says is in *device* logical blocks, which are
/// 4096 bytes on a 4Kn disk. Reading the GPT header from byte 512 on such a
/// disk finds nothing, which is why a perfectly good 4K-formatted NVMe SSD
/// used to come up with zero partitions.
pub fn scan_partitions(block: &Arc<dyn BlockScheme>) -> alloc::vec::Vec<(usize, usize)> {
    let mut partitions = alloc::vec::Vec::new();
    let lbs = device_block_size(block);
    let total_api = block.block_count();
    if total_api == 0 {
        return partitions;
    }
    let mut win = Window::new();

    // The protective MBR always lives in the first 512 bytes, whatever the
    // device block size is.
    let mut mbr = [0u8; API_SECTOR];
    match win.get(block, 0, API_SECTOR) {
        Some(b) => mbr.copy_from_slice(b),
        None => return partitions,
    }
    let has_mbr_signature = u16::from_le_bytes([mbr[510], mbr[511]]) == 0xAA55;

    // Try GPT first: its header is at device LBA 1, and a protective MBR entry
    // of type 0xEE only hints at it. If the primary header is unreadable or
    // corrupt, the spec puts a backup on the last block of the device.
    let primary = lbs as u64;
    let total_lba = (total_api / (lbs / API_SECTOR)) as u64;
    let backup = total_lba.checked_sub(1).map(|last| last * lbs as u64);
    for header_off in core::iter::once(primary).chain(backup) {
        if let Some(found) = parse_gpt(block, &mut win, header_off, lbs, total_api) {
            if header_off != primary {
                warn!(
                    "[part] primary GPT header unusable; using the backup at LBA {}",
                    total_lba - 1
                );
            }
            return found;
        }
    }

    if !has_mbr_signature {
        return partitions;
    }
    // A protective MBR with no usable GPT is not an MBR disk: walking its
    // 0xEE entry would hand out a partition covering the whole device. The
    // entry is meant to be the first one but nothing on disk enforces that.
    if (0..4).any(|i| mbr[446 + i * 16 + 4] == 0xEE) {
        warn!("[part] protective MBR found but no valid GPT header");
        return partitions;
    }

    for i in 0..4 {
        let e = &mbr[446 + i * 16..446 + i * 16 + 16];
        let part_type = e[4];
        if part_type == 0 {
            continue;
        }
        let start_lba = le_u32(&e[8..12]) as u64;
        let count_lba = le_u32(&e[12..16]) as u64;
        if is_extended(part_type) {
            // The logical partitions live in a linked list of EBRs inside this
            // container; the container itself is not a usable partition.
            walk_ebr_chain(
                block,
                &mut win,
                start_lba,
                count_lba,
                lbs,
                total_api,
                &mut partitions,
            );
            continue;
        }
        if start_lba == 0 {
            continue;
        }
        if let Some(p) = to_api_sectors(start_lba, count_lba, lbs, total_api) {
            partitions.push(p);
        } else {
            warn!("[part] MBR entry {i} is out of bounds (lba {start_lba}, {count_lba} blocks)");
        }
    }
    partitions
}

/// The MBR partition types that mean "this is a container of logical
/// partitions", not a filesystem.
fn is_extended(part_type: u8) -> bool {
    matches!(part_type, 0x05 | 0x0F | 0x85)
}

/// Follow the EBR linked list of an extended partition, appending each logical
/// partition it names.
fn walk_ebr_chain(
    block: &Arc<dyn BlockScheme>,
    win: &mut Window,
    ext_start: u64,
    ext_count: u64,
    lbs: usize,
    total_api: usize,
    out: &mut alloc::vec::Vec<(usize, usize)>,
) {
    if ext_start == 0 {
        return;
    }
    let ext_end = ext_start.saturating_add(ext_count);
    let mut current = ext_start;
    let mut seen = alloc::vec::Vec::new();
    for _ in 0..MAX_EBR_LINKS {
        if seen.contains(&current) {
            warn!("[part] EBR chain loops back to LBA {current}; stopping");
            return;
        }
        seen.push(current);

        let ebr = match win.get(block, current * lbs as u64, API_SECTOR) {
            Some(b) => b,
            None => return,
        };
        if u16::from_le_bytes([ebr[510], ebr[511]]) != 0xAA55 {
            return;
        }
        // First entry: the logical partition, addressed relative to this EBR.
        let first = &ebr[446..462];
        if first[4] != 0 {
            let rel = le_u32(&first[8..12]) as u64;
            let count = le_u32(&first[12..16]) as u64;
            let start = current.saturating_add(rel);
            if rel != 0 && count != 0 && start.saturating_add(count) <= ext_end {
                match to_api_sectors(start, count, lbs, total_api) {
                    Some(p) => out.push(p),
                    None => warn!("[part] logical partition at LBA {start} is out of bounds"),
                }
            }
        }
        // Second entry: the next EBR, addressed relative to the *container*.
        let next = &ebr[462..478];
        if !is_extended(next[4]) {
            return;
        }
        let rel = le_u32(&next[8..12]) as u64;
        if rel == 0 {
            return;
        }
        current = ext_start.saturating_add(rel);
        if current >= ext_end {
            return;
        }
    }
    warn!("[part] EBR chain longer than {MAX_EBR_LINKS} links; stopping");
}

/// Parse a GPT header at `header_off` bytes and its partition array. Returns
/// `None` when there is no usable header there, so the caller can fall back to
/// the backup copy.
fn parse_gpt(
    block: &Arc<dyn BlockScheme>,
    win: &mut Window,
    header_off: u64,
    lbs: usize,
    total_api: usize,
) -> Option<alloc::vec::Vec<(usize, usize)>> {
    // The header is 92 bytes; 512 is the smallest read we can do anyway.
    let header = win.get(block, header_off, API_SECTOR)?;
    if &header[0..8] != b"EFI PART" {
        return None;
    }
    // Note: the header and array CRC32s (offsets 16 and 88) are not verified —
    // there is no CRC implementation in this crate. Every field read below is
    // range-checked instead, which is what actually keeps a corrupt table from
    // producing a partition that overlaps the rest of the disk.
    let entry_lba = le_u64(&header[72..80]);
    let num_entries = le_u32(&header[80..84]);
    let entry_size = le_u32(&header[84..88]) as usize;

    if entry_lba < 2 || num_entries == 0 || !(128..=MAX_LBS).contains(&entry_size) {
        warn!(
            "[part] GPT header at byte {header_off} is implausible: entry_lba={entry_lba}, \
             entries={num_entries}, entry_size={entry_size}"
        );
        return None;
    }
    // Entries are defined as a multiple of 128 bytes; anything else means we
    // would be walking the array at the wrong stride.
    if !entry_size.is_multiple_of(128) {
        warn!("[part] GPT entry size {entry_size} is not a multiple of 128");
        return None;
    }
    let entries = num_entries.min(MAX_GPT_ENTRIES);
    if entries != num_entries {
        warn!("[part] GPT declares {num_entries} entries; reading the first {entries}");
    }

    let array_base = entry_lba.checked_mul(lbs as u64)?;
    let mut partitions = alloc::vec::Vec::new();
    for i in 0..entries {
        let off = array_base.checked_add(i as u64 * entry_size as u64)?;
        // Only the first 48 bytes of an entry matter here: type GUID, unique
        // GUID, first LBA, last LBA.
        let e = match win.get(block, off, 48) {
            Some(e) => e,
            None => break,
        };
        // An unused entry is defined by a zero *type* GUID. The old test —
        // "any non-zero byte anywhere in the entry" — also fired on a stale
        // name or GUID left behind by a previous table.
        if e[0..16].iter().all(|&b| b == 0) {
            continue;
        }
        let first_lba = le_u64(&e[32..40]);
        let last_lba = le_u64(&e[40..48]);
        if last_lba < first_lba || first_lba == 0 {
            warn!("[part] GPT entry {i} has a bad LBA range {first_lba}..={last_lba}");
            continue;
        }
        match to_api_sectors(first_lba, last_lba - first_lba + 1, lbs, total_api) {
            Some(p) => partitions.push(p),
            None => warn!("[part] GPT entry {i} runs past the end of the device"),
        }
    }
    Some(partitions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use zcore_drivers::{DeviceError, DeviceResult};

    /// A disk made of a `Vec<u8>`, addressed the way a driver addresses one:
    /// `block_id` in 512-byte units, whatever the device's own block size is.
    struct FakeDisk {
        name: String,
        data: Vec<u8>,
        lbs: usize,
    }

    impl FakeDisk {
        fn new(lbs: usize, blocks: usize) -> Self {
            Self {
                name: String::from("fake"),
                data: vec![0u8; lbs * blocks],
                lbs,
            }
        }

        fn into_scheme(self) -> Arc<dyn BlockScheme> {
            Arc::new(self)
        }
    }

    impl Scheme for FakeDisk {
        fn name(&self) -> &str {
            &self.name
        }
    }

    impl BlockScheme for FakeDisk {
        fn read_block(&self, block_id: usize, buf: &mut [u8]) -> DeviceResult {
            if buf.is_empty() || !buf.len().is_multiple_of(512) {
                return Err(DeviceError::InvalidParam);
            }
            let off = block_id * 512;
            if off + buf.len() > self.data.len() {
                return Err(DeviceError::InvalidParam);
            }
            buf.copy_from_slice(&self.data[off..off + buf.len()]);
            Ok(())
        }

        fn write_block(&self, _block_id: usize, _buf: &[u8]) -> DeviceResult {
            Err(DeviceError::NotSupported)
        }

        fn flush(&self) -> DeviceResult {
            Ok(())
        }

        fn block_count(&self) -> usize {
            self.data.len() / 512
        }

        fn logical_block_size(&self) -> usize {
            self.lbs
        }
    }

    /// An arbitrary but non-zero type GUID (the EFI System Partition's).
    const ESP_GUID: [u8; 16] = [
        0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9,
        0x3b,
    ];

    fn put_protective_mbr(d: &mut [u8]) {
        d[446] = 0x00;
        d[450] = 0xEE;
        d[454..458].copy_from_slice(&1u32.to_le_bytes());
        d[458..462].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        d[510] = 0x55;
        d[511] = 0xAA;
    }

    /// Write a GPT header at `header_lba` plus its entry array, in device blocks.
    fn put_gpt(
        d: &mut [u8],
        lbs: usize,
        header_lba: u64,
        entry_lba: u64,
        entry_size: usize,
        parts: &[(u64, u64)],
    ) {
        let h = header_lba as usize * lbs;
        d[h..h + 8].copy_from_slice(b"EFI PART");
        d[h + 72..h + 80].copy_from_slice(&entry_lba.to_le_bytes());
        d[h + 80..h + 84].copy_from_slice(&(parts.len() as u32).to_le_bytes());
        d[h + 84..h + 88].copy_from_slice(&(entry_size as u32).to_le_bytes());
        for (i, &(first, last)) in parts.iter().enumerate() {
            let e = entry_lba as usize * lbs + i * entry_size;
            d[e..e + 16].copy_from_slice(&ESP_GUID);
            d[e + 16..e + 32].copy_from_slice(&[0x11u8; 16]);
            d[e + 32..e + 40].copy_from_slice(&first.to_le_bytes());
            d[e + 40..e + 48].copy_from_slice(&last.to_le_bytes());
        }
    }

    fn put_mbr_entry(d: &mut [u8], slot: usize, ptype: u8, start: u32, count: u32) {
        let o = 446 + slot * 16;
        d[o + 4] = ptype;
        d[o + 8..o + 12].copy_from_slice(&start.to_le_bytes());
        d[o + 12..o + 16].copy_from_slice(&count.to_le_bytes());
    }

    /// A GPT disk with three partitions, read through a 512-byte-block driver.
    #[test]
    fn gpt_on_a_512_byte_disk() {
        let mut disk = FakeDisk::new(512, 4096);
        put_protective_mbr(&mut disk.data);
        put_gpt(
            &mut disk.data,
            512,
            1,
            2,
            128,
            &[(2048, 3071), (3072, 3583), (3584, 4000)],
        );
        let parts = scan_partitions(&disk.into_scheme());
        assert_eq!(parts, vec![(2048, 1024), (3072, 512), (3584, 417)]);
    }

    /// The same table on a 4Kn disk. Every LBA in it means 4096 bytes, so the
    /// header is at byte 4096 and each partition covers eight times as many
    /// 512-byte API sectors. Before `logical_block_size()` existed this
    /// returned an empty vector: the scanner looked for "EFI PART" at byte 512
    /// and found zeroes.
    #[test]
    fn gpt_on_a_4kn_disk() {
        let mut disk = FakeDisk::new(4096, 4096);
        put_protective_mbr(&mut disk.data);
        put_gpt(
            &mut disk.data,
            4096,
            1,
            2,
            128,
            &[(2048, 3071), (3072, 3583), (3584, 4000)],
        );
        let parts = scan_partitions(&disk.into_scheme());
        assert_eq!(parts, vec![(16384, 8192), (24576, 4096), (28672, 3336)]);
    }

    /// The old parser read sectors 2..=5 only, so it stopped at 16 partitions.
    #[test]
    fn gpt_with_more_than_sixteen_partitions() {
        let mut disk = FakeDisk::new(512, 8192);
        put_protective_mbr(&mut disk.data);
        let entries: Vec<(u64, u64)> = (0..40)
            .map(|i| (2048 + i * 64, 2048 + i * 64 + 63))
            .collect();
        put_gpt(&mut disk.data, 512, 1, 2, 128, &entries);
        let parts = scan_partitions(&disk.into_scheme());
        assert_eq!(parts.len(), 40);
        assert_eq!(parts[0], (2048, 64));
        assert_eq!(parts[39], (2048 + 39 * 64, 64));
    }

    /// `PartitionEntryLBA` and `SizeOfPartitionEntry` are header fields, not
    /// constants; the old parser hardcoded LBA 2 and 128 bytes.
    #[test]
    fn gpt_with_a_relocated_array_and_wide_entries() {
        let mut disk = FakeDisk::new(512, 4096);
        put_protective_mbr(&mut disk.data);
        put_gpt(
            &mut disk.data,
            512,
            1,
            34,
            256,
            &[(2048, 3071), (3072, 3583)],
        );
        let parts = scan_partitions(&disk.into_scheme());
        assert_eq!(parts, vec![(2048, 1024), (3072, 512)]);
    }

    /// An entry is unused when its *type* GUID is zero. Leftover bytes
    /// elsewhere in the entry used to be enough to make one up.
    #[test]
    fn gpt_entry_with_a_zero_type_guid_is_unused() {
        let mut disk = FakeDisk::new(512, 4096);
        put_protective_mbr(&mut disk.data);
        put_gpt(
            &mut disk.data,
            512,
            1,
            2,
            128,
            &[(2048, 3071), (3072, 3583)],
        );
        // Wipe the second entry's type GUID but leave its stale name behind.
        let e = 2 * 512 + 128;
        disk.data[e..e + 16].fill(0);
        disk.data[e + 56..e + 72].fill(0x41);
        let parts = scan_partitions(&disk.into_scheme());
        assert_eq!(parts, vec![(2048, 1024)]);
    }

    /// A partition that claims to end past the last sector is dropped, not
    /// handed to the filesystem layer.
    #[test]
    fn gpt_entry_past_the_end_of_the_device_is_dropped() {
        let mut disk = FakeDisk::new(512, 4096);
        put_protective_mbr(&mut disk.data);
        put_gpt(
            &mut disk.data,
            512,
            1,
            2,
            128,
            &[(2048, 3071), (3072, 99999)],
        );
        let parts = scan_partitions(&disk.into_scheme());
        assert_eq!(parts, vec![(2048, 1024)]);
    }

    /// When the primary header is unreadable the spec's backup copy, on the
    /// last block of the device, is what mounts the disk.
    #[test]
    fn gpt_falls_back_to_the_backup_header() {
        let mut disk = FakeDisk::new(512, 4096);
        put_protective_mbr(&mut disk.data);
        put_gpt(&mut disk.data, 512, 4095, 2, 128, &[(2048, 3071)]);
        let parts = scan_partitions(&disk.into_scheme());
        assert_eq!(parts, vec![(2048, 1024)]);
    }

    /// A protective MBR whose GPT is gone must yield nothing: its single 0xEE
    /// entry spans the whole disk and is not a filesystem.
    #[test]
    fn protective_mbr_without_a_gpt_yields_nothing() {
        let mut disk = FakeDisk::new(512, 4096);
        put_protective_mbr(&mut disk.data);
        let parts = scan_partitions(&disk.into_scheme());
        assert!(parts.is_empty(), "{:?}", parts);
    }

    #[test]
    fn plain_mbr_primaries() {
        let mut disk = FakeDisk::new(512, 4096);
        put_mbr_entry(&mut disk.data, 0, 0x83, 2048, 1024);
        put_mbr_entry(&mut disk.data, 1, 0x0C, 3072, 512);
        disk.data[510] = 0x55;
        disk.data[511] = 0xAA;
        let parts = scan_partitions(&disk.into_scheme());
        assert_eq!(parts, vec![(2048, 1024), (3072, 512)]);
    }

    /// An extended partition is a container: the old parser reported it as a
    /// partition of its own and never found the logical ones inside it.
    #[test]
    fn mbr_extended_partition_chain() {
        let mut disk = FakeDisk::new(512, 8192);
        put_mbr_entry(&mut disk.data, 0, 0x83, 2048, 1024);
        put_mbr_entry(&mut disk.data, 1, 0x05, 4096, 4096);
        disk.data[510] = 0x55;
        disk.data[511] = 0xAA;

        // First EBR at LBA 4096: its logical partition starts 64 blocks later,
        // and the chain continues 2048 blocks past the container's start.
        let ebr0 = 4096 * 512;
        put_mbr_entry(&mut disk.data[ebr0..ebr0 + 512], 0, 0x83, 64, 1000);
        put_mbr_entry(&mut disk.data[ebr0..ebr0 + 512], 1, 0x05, 2048, 2048);
        disk.data[ebr0 + 510] = 0x55;
        disk.data[ebr0 + 511] = 0xAA;

        // Second EBR at LBA 6144, holding the last logical partition.
        let ebr1 = 6144 * 512;
        put_mbr_entry(&mut disk.data[ebr1..ebr1 + 512], 0, 0x83, 64, 1000);
        disk.data[ebr1 + 510] = 0x55;
        disk.data[ebr1 + 511] = 0xAA;

        let parts = scan_partitions(&disk.into_scheme());
        assert_eq!(parts, vec![(2048, 1024), (4160, 1000), (6208, 1000)]);
    }

    /// An EBR chain that links back to a block it already visited must not
    /// spin forever.
    #[test]
    fn mbr_extended_chain_loop_terminates() {
        let mut disk = FakeDisk::new(512, 8192);
        put_mbr_entry(&mut disk.data, 0, 0x05, 4096, 4096);
        disk.data[510] = 0x55;
        disk.data[511] = 0xAA;

        let ebr0 = 4096 * 512;
        put_mbr_entry(&mut disk.data[ebr0..ebr0 + 512], 0, 0x83, 64, 1000);
        put_mbr_entry(&mut disk.data[ebr0..ebr0 + 512], 1, 0x05, 2048, 2048);
        disk.data[ebr0 + 510] = 0x55;
        disk.data[ebr0 + 511] = 0xAA;

        // The second EBR points at itself.
        let ebr1 = 6144 * 512;
        put_mbr_entry(&mut disk.data[ebr1..ebr1 + 512], 0, 0x83, 64, 500);
        put_mbr_entry(&mut disk.data[ebr1..ebr1 + 512], 1, 0x05, 2048, 2048);
        disk.data[ebr1 + 510] = 0x55;
        disk.data[ebr1 + 511] = 0xAA;

        let parts = scan_partitions(&disk.into_scheme());
        assert_eq!(parts, vec![(4160, 1000), (6208, 500)]);
    }

    /// A driver reporting a nonsensical block size must not be believed.
    #[test]
    fn implausible_block_size_falls_back_to_512() {
        let mut disk = FakeDisk::new(512, 4096);
        disk.lbs = 777;
        put_protective_mbr(&mut disk.data);
        put_gpt(&mut disk.data, 512, 1, 2, 128, &[(2048, 3071)]);
        let parts = scan_partitions(&disk.into_scheme());
        assert_eq!(parts, vec![(2048, 1024)]);
    }

    #[test]
    fn a_disk_with_no_table_at_all_yields_nothing() {
        let disk = FakeDisk::new(512, 4096);
        assert!(scan_partitions(&disk.into_scheme()).is_empty());
    }
}
