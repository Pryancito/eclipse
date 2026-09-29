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
    use lock::Mutex;
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

    /// Byte `i` of a test disk. Period 251 rather than 256 so that two
    /// offsets a whole number of sectors apart never hold the same byte:
    /// with a 256-long pattern a read from the wrong sector is invisible.
    fn pat(i: usize) -> u8 {
        (i % 251) as u8
    }

    /// A disk that accepts writes and remembers every transfer.
    ///
    /// What `read_at` and `write_at` decide is not only which bytes move but
    /// *how many* transfers they make and at which block, and that is
    /// invisible in the bytes alone: a half-filled sector has to be read
    /// before it is written back, and the whole-sector middle has to go out
    /// in one call rather than one per sector.
    struct RecordingDisk {
        name: String,
        data: Mutex<Vec<u8>>,
        reads: Mutex<Vec<(usize, usize)>>,
        writes: Mutex<Vec<(usize, usize)>>,
        flushes: Mutex<usize>,
    }

    impl RecordingDisk {
        fn new(blocks: usize) -> Arc<Self> {
            Arc::new(Self {
                name: String::from("rec"),
                data: Mutex::new((0..blocks * 512).map(pat).collect()),
                reads: Mutex::new(Vec::new()),
                writes: Mutex::new(Vec::new()),
                flushes: Mutex::new(0),
            })
        }

        fn reads(&self) -> Vec<(usize, usize)> {
            self.reads.lock().clone()
        }

        fn writes(&self) -> Vec<(usize, usize)> {
            self.writes.lock().clone()
        }

        fn flushes(&self) -> usize {
            *self.flushes.lock()
        }

        fn byte(&self, i: usize) -> u8 {
            self.data.lock()[i]
        }
    }

    impl Scheme for RecordingDisk {
        fn name(&self) -> &str {
            &self.name
        }
    }

    impl BlockScheme for RecordingDisk {
        fn read_block(&self, block_id: usize, buf: &mut [u8]) -> DeviceResult {
            let data = self.data.lock();
            let off = block_id * 512;
            if buf.is_empty() || !buf.len().is_multiple_of(512) || off + buf.len() > data.len() {
                return Err(DeviceError::InvalidParam);
            }
            buf.copy_from_slice(&data[off..off + buf.len()]);
            self.reads.lock().push((block_id, buf.len()));
            Ok(())
        }

        fn write_block(&self, block_id: usize, buf: &[u8]) -> DeviceResult {
            let mut data = self.data.lock();
            let off = block_id * 512;
            if buf.is_empty() || !buf.len().is_multiple_of(512) || off + buf.len() > data.len() {
                return Err(DeviceError::InvalidParam);
            }
            data[off..off + buf.len()].copy_from_slice(buf);
            self.writes.lock().push((block_id, buf.len()));
            Ok(())
        }

        fn flush(&self) -> DeviceResult {
            *self.flushes.lock() += 1;
            Ok(())
        }

        fn block_count(&self) -> usize {
            self.data.lock().len() / 512
        }

        fn logical_block_size(&self) -> usize {
            512
        }
    }

    /// A disk that reports `lbs` as its logical block size and nothing else of
    /// interest, for the sanity check that reads it.
    fn disk_with_lbs(lbs: usize) -> Arc<dyn BlockScheme> {
        FakeDisk {
            name: String::from("fake"),
            data: vec![0u8; 4096],
            lbs,
        }
        .into_scheme()
    }

    // ── the device node: reads and writes in whole sectors ──────────────

    #[test]
    fn a_read_that_straddles_sectors_is_a_head_one_middle_and_a_tail() {
        // Three transfers, and the middle is a single multi-sector one rather
        // than a call per sector: an unaligned 4 KiB read is the every-time
        // case, and a per-sector loop is eight times the round trips.
        let disk = RecordingDisk::new(8);
        let dev = BlockDev::new(0, disk.clone(), String::from("rec"));
        let mut buf = [0u8; 512 + 512 + 100];
        // 100 bytes into sector 1: 412 of head, 512 of middle, 200 of tail.
        assert_eq!(dev.read_at(612, &mut buf).unwrap(), buf.len());
        assert_eq!(
            disk.reads(),
            vec![(1, 512), (2, 512), (3, 512)],
            "the half-filled ends one sector each, the middle in one go"
        );
        for (k, &b) in buf.iter().enumerate() {
            assert_eq!(b, pat(612 + k), "byte {k} of the read");
        }
    }

    #[test]
    fn a_read_of_whole_sectors_makes_no_trip_for_a_tail_that_is_not_there() {
        let disk = RecordingDisk::new(8);
        let dev = BlockDev::new(0, disk.clone(), String::from("rec"));
        let mut buf = [0u8; 1024];
        assert_eq!(dev.read_at(1024, &mut buf).unwrap(), 1024);
        assert_eq!(
            disk.reads(),
            vec![(2, 1024)],
            "nothing to fix up at either end"
        );
        assert_eq!(buf[0], pat(1024));
    }

    #[test]
    fn a_write_that_half_fills_a_sector_reads_it_back_before_writing_it() {
        // The bytes of the first and last sectors the caller did not name
        // have to survive, which is what makes those two a read-modify-write
        // and the middle a plain write.
        let disk = RecordingDisk::new(8);
        let dev = BlockDev::new(0, disk.clone(), String::from("rec"));
        let src: Vec<u8> = (0..512 + 512 + 100)
            .map(|i| 0x80 | (i % 101) as u8)
            .collect();
        assert_eq!(dev.write_at(612, &src).unwrap(), src.len());
        assert_eq!(
            disk.reads(),
            vec![(1, 512), (3, 512)],
            "only the half-filled ends"
        );
        assert_eq!(disk.writes(), vec![(1, 512), (2, 512), (3, 512)]);
        assert_eq!(
            disk.byte(611),
            pat(611),
            "the byte before the write is still the disk's"
        );
        for (k, &b) in src.iter().enumerate() {
            assert_eq!(disk.byte(612 + k), b, "byte {k} of the write");
        }
        assert_eq!(
            disk.byte(612 + src.len()),
            pat(612 + src.len()),
            "and the one after it"
        );
    }

    #[test]
    fn a_write_with_a_single_byte_past_the_last_whole_sector_still_writes_it() {
        let disk = RecordingDisk::new(8);
        let dev = BlockDev::new(0, disk.clone(), String::from("rec"));
        assert_eq!(dev.write_at(512, &[0x5Au8; 513]).unwrap(), 513);
        assert_eq!(
            disk.byte(1024),
            0x5A,
            "the byte that is a whole sector short"
        );
        assert_eq!(disk.byte(1025), pat(1025));
    }

    // ── the device node: what it tells userspace about itself ──────────

    #[test]
    fn the_size_ioctls_answer_in_the_units_their_names_promise() {
        // `BLKGETSIZE64` is bytes and `BLKGETSIZE` is 512-byte sectors. A
        // partitioner that reads the wrong one lays the table out at the
        // wrong end of the disk.
        let dev = BlockDev::new(0, RecordingDisk::new(8), String::from("rec"));
        let mut bytes = 0u64;
        assert_eq!(
            dev.io_control(0x8008_1272, &mut bytes as *mut u64 as usize)
                .unwrap(),
            0
        );
        assert_eq!(bytes, 8 * 512);
        let mut sectors = 0usize;
        assert_eq!(
            dev.io_control(0x0000_1260, &mut sectors as *mut usize as usize)
                .unwrap(),
            0
        );
        assert_eq!(sectors, 8);
    }

    #[test]
    fn flushing_the_buffers_reaches_the_driver_and_then_says_it_worked() {
        // This used to answer ENOSYS, which an installer reads as "your write
        // may not have landed".
        let disk = RecordingDisk::new(8);
        let dev = BlockDev::new(0, disk.clone(), String::from("rec"));
        assert_eq!(dev.io_control(0x0000_1261, 0).unwrap(), 0);
        assert_eq!(
            disk.flushes(),
            1,
            "the driver's own flush is what makes it true"
        );
    }

    #[test]
    fn an_ioctl_this_device_does_not_know_is_refused() {
        let dev = BlockDev::new(0, RecordingDisk::new(8), String::from("rec"));
        assert_eq!(dev.io_control(0x0000_1299, 0), Err(FsError::NotSupported));
        // The number one below the re-read-partition-table ioctl is not it.
        assert_eq!(dev.io_control(0x0000_125e, 0), Err(FsError::NotSupported));
    }

    #[test]
    fn the_metadata_is_a_block_device_measured_in_512_byte_sectors() {
        let dev = BlockDev::new(5, RecordingDisk::new(8), String::from("rec"));
        let m = dev.metadata().unwrap();
        assert_eq!(m.type_, FileType::BlockDevice);
        assert_eq!(m.blocks, 8);
        assert_eq!(m.blk_size, 512);
        assert_eq!(m.size, 8 * 512, "the size a stat(2) reports is bytes");
        assert_eq!(
            m.rdev,
            make_rdev(3, 5),
            "major 3 is the disk major and the minor is the device's index"
        );
    }

    #[test]
    fn every_driver_error_becomes_the_filesystem_error_that_means_the_same() {
        // `NotReady` is the one that matters: a disk that is merely busy has
        // to read as busy, not as a hardware failure the caller gives up on.
        for (dev, fs) in [
            (DeviceError::NotSupported, FsError::NotSupported),
            (DeviceError::NotReady, FsError::Busy),
            (DeviceError::InvalidParam, FsError::InvalidParam),
            (DeviceError::BufferTooSmall, FsError::DeviceError),
            (DeviceError::DmaError, FsError::DeviceError),
            (DeviceError::IoError, FsError::DeviceError),
            (DeviceError::AlreadyExists, FsError::DeviceError),
            (DeviceError::NoResources, FsError::DeviceError),
        ] {
            assert_eq!(convert_error(dev), fs, "{dev:?}");
        }
    }

    // ── a partition as a block device of its own ───────────────────────

    #[test]
    fn a_partition_addresses_the_parent_from_its_own_first_block() {
        let disk = RecordingDisk::new(64);
        let part = PartitionBlock::new(disk.clone(), String::from("p1"), 8, 4);
        let mut buf = [0u8; 512];
        part.read_block(0, &mut buf).unwrap();
        assert_eq!(
            disk.reads(),
            vec![(8, 512)],
            "block 0 of the partition is block 8 of the disk"
        );
        assert_eq!(buf[0], pat(8 * 512));
        part.write_block(3, &[0x77u8; 512]).unwrap();
        assert_eq!(disk.writes(), vec![(11, 512)]);
        assert_eq!(disk.byte(11 * 512), 0x77);
        assert_eq!(part.block_count(), 4, "its own size, not the whole disk's");
        // A partition is addressed in its parent's blocks, because anything
        // laid out inside it is.
        let big = PartitionBlock::new(disk_with_lbs(4096), String::from("p2"), 0, 8);
        assert_eq!(big.logical_block_size(), 4096);
    }

    #[test]
    fn a_partition_refuses_the_transfer_that_would_run_past_its_end() {
        // The last sector is inside and the one after it is not: a partition
        // that let a read run over its end would hand out the next one's
        // data.
        let disk = RecordingDisk::new(64);
        let part = PartitionBlock::new(disk, String::from("p1"), 8, 4);
        let mut one = [0u8; 512];
        assert!(
            part.read_block(3, &mut one).is_ok(),
            "the last sector is inside"
        );
        assert_eq!(part.read_block(4, &mut one), Err(DeviceError::InvalidParam));
        assert_eq!(
            part.write_block(4, &[0u8; 512]),
            Err(DeviceError::InvalidParam)
        );
        let mut four = [0u8; 2048];
        assert!(
            part.read_block(0, &mut four).is_ok(),
            "four sectors is exactly the partition"
        );
        assert_eq!(
            part.read_block(1, &mut four),
            Err(DeviceError::InvalidParam)
        );
    }

    #[test]
    fn a_logical_block_size_that_is_not_a_sane_power_of_two_falls_back_to_512() {
        // Everything a partition table says is in device blocks, so a garbage
        // value here reads the table at the wrong place or turns a header
        // read into a multi-megabyte transfer.
        for lbs in [0, 1, 256, 1536, 3072, 8192] {
            assert_eq!(device_block_size(&disk_with_lbs(lbs)), 512, "lbs {lbs}");
        }
        for lbs in [512, 1024, 2048, 4096] {
            assert_eq!(device_block_size(&disk_with_lbs(lbs)), lbs, "lbs {lbs}");
        }
    }

    // ── the sliding window the table is read through ───────────────────

    #[test]
    fn the_window_starts_on_the_sector_below_the_range_and_fills_itself() {
        let disk = RecordingDisk::new(16);
        let d: Arc<dyn BlockScheme> = disk.clone();
        let mut w = Window::new();
        let got = w.get(&d, 600, 24).unwrap();
        assert_eq!(
            got[0],
            pat(600),
            "the byte asked for, not the one the window starts on"
        );
        assert_eq!(got[23], pat(623));
        assert_eq!(
            disk.reads(),
            vec![(1, MAX_LBS)],
            "the 512-boundary at or below 600"
        );
    }

    #[test]
    fn the_window_answers_from_itself_while_the_range_is_inside_it() {
        // The GPT entry array is addressed in bytes and its entries need not
        // line up with sectors, so a walk of it asks for one arbitrary range
        // after another. Re-reading for each would be a transfer per entry.
        let disk = RecordingDisk::new(16);
        let d: Arc<dyn BlockScheme> = disk.clone();
        let mut w = Window::new();
        assert_eq!(w.get(&d, 600, 24).unwrap()[0], pat(600));
        assert_eq!(disk.reads().len(), 1);
        assert_eq!(
            w.get(&d, 512, 8).unwrap()[0],
            pat(512),
            "the window's own first byte"
        );
        assert_eq!(w.get(&d, 4000, 48).unwrap()[0], pat(4000));
        let win = MAX_LBS as u64;
        assert_eq!(
            w.get(&d, 512 + win - 8, 8).unwrap()[0],
            pat(512 + MAX_LBS - 8),
            "a range that ends exactly where the window does"
        );
        assert_eq!(disk.reads().len(), 1, "all of that out of one read");
        // And one byte further is a new window.
        assert_eq!(w.get(&d, 512 + win, 8).unwrap()[0], pat(512 + MAX_LBS));
        assert_eq!(disk.reads().len(), 2);
    }

    #[test]
    fn a_window_range_of_no_length_or_of_more_than_one_window_is_refused() {
        let disk = RecordingDisk::new(16);
        let d: Arc<dyn BlockScheme> = disk.clone();
        let mut w = Window::new();
        assert!(w.get(&d, 0, 0).is_none(), "no length is not a range");
        assert!(
            w.get(&d, 0, MAX_LBS + 1).is_none(),
            "more than the window holds"
        );
        assert!(
            w.get(&d, 0, MAX_LBS).is_some(),
            "exactly the window is allowed"
        );
    }

    #[test]
    fn a_window_range_that_runs_off_the_end_of_the_device_is_refused() {
        // The backup GPT header sits on the very last block, so the window is
        // clamped to what the device has -- and a range that needs more than
        // the clamp leaves has to be refused, not served from stale bytes.
        let disk = RecordingDisk::new(9);
        let d: Arc<dyn BlockScheme> = disk.clone();
        let mut w = Window::new();
        assert!(
            w.get(&d, 4600, 8).is_some(),
            "the last eight bytes of the device"
        );
        let mut w2 = Window::new();
        assert!(
            w2.get(&d, 4604, 8).is_none(),
            "four bytes past the end of it"
        );
    }

    #[test]
    fn a_partition_that_ends_on_the_last_sector_fits_and_one_past_it_does_not() {
        // 4Kn: one device block is eight API sectors.
        assert_eq!(to_api_sectors(0, 1, 4096, 8), Some((0, 8)));
        assert_eq!(to_api_sectors(1, 1, 4096, 16), Some((8, 8)));
        assert_eq!(
            to_api_sectors(1, 1, 4096, 15),
            None,
            "one sector short of fitting"
        );
        assert_eq!(
            to_api_sectors(0, 8, 512, 8),
            Some((0, 8)),
            "exactly the whole device"
        );
        assert_eq!(to_api_sectors(0, 9, 512, 8), None);
        assert_eq!(
            to_api_sectors(0, 0, 512, 8),
            None,
            "an empty partition is not one"
        );
        assert_eq!(
            to_api_sectors(u64::MAX, 2, 4096, 8),
            None,
            "the multiply overflows"
        );
    }

    // ── the MBR, its containers and its chains ─────────────────────────

    #[test]
    fn a_protective_mbr_is_recognised_in_any_slot_and_by_its_type_byte() {
        // The 0xEE entry is meant to be the first one, but nothing on disk
        // enforces that; and its type lives at byte 4 of the entry, not at
        // byte 0, which is the boot flag. Either mistake hands out one
        // partition covering the whole GPT disk.
        for slot in 0..4 {
            let mut disk = FakeDisk::new(512, 4096);
            put_mbr_entry(&mut disk.data, slot, 0xEE, 1, 4095);
            disk.data[510] = 0x55;
            disk.data[511] = 0xAA;
            assert!(
                scan_partitions(&disk.into_scheme()).is_empty(),
                "protective entry in slot {}",
                slot
            );
        }
    }

    #[test]
    fn an_empty_mbr_slot_is_not_a_partition_even_with_numbers_left_in_it() {
        // Type zero means unused. A table rewritten in place leaves the old
        // start and length sitting behind it.
        let mut disk = FakeDisk::new(512, 4096);
        put_mbr_entry(&mut disk.data, 0, 0x83, 2048, 1024);
        put_mbr_entry(&mut disk.data, 1, 0x00, 1024, 512);
        disk.data[510] = 0x55;
        disk.data[511] = 0xAA;
        assert_eq!(scan_partitions(&disk.into_scheme()), vec![(2048, 1024)]);
    }

    #[test]
    fn an_mbr_partition_that_starts_on_the_sector_after_the_table_is_kept() {
        // LBA 0 is the table itself, so a partition claiming it is garbage.
        // LBA 1 is the first sector a real one can have, and superfloppy and
        // embedded images do start there.
        let mut disk = FakeDisk::new(512, 4096);
        put_mbr_entry(&mut disk.data, 0, 0x83, 1, 2047);
        put_mbr_entry(&mut disk.data, 1, 0x83, 0, 512);
        disk.data[510] = 0x55;
        disk.data[511] = 0xAA;
        assert_eq!(
            scan_partitions(&disk.into_scheme()),
            vec![(1, 2047)],
            "the one at LBA 1 is real and the one at LBA 0 is not"
        );
    }

    #[test]
    fn a_linux_extended_container_is_walked_and_not_handed_out() {
        // 0x85 is the Linux extended type. Treating it as a filesystem hands
        // out the container, which overlaps every logical partition in it.
        let mut disk = FakeDisk::new(512, 8192);
        put_mbr_entry(&mut disk.data, 0, 0x85, 4096, 4096);
        disk.data[510] = 0x55;
        disk.data[511] = 0xAA;
        let ebr = 4096 * 512;
        put_mbr_entry(&mut disk.data[ebr..ebr + 512], 0, 0x83, 64, 1000);
        disk.data[ebr + 510] = 0x55;
        disk.data[ebr + 511] = 0xAA;
        assert_eq!(scan_partitions(&disk.into_scheme()), vec![(4160, 1000)]);
    }

    #[test]
    fn an_extended_entry_that_starts_on_lba_zero_is_not_a_chain() {
        // LBA 0 is the partition table, not an EBR. Walking it reads the
        // table's own first entry as a logical partition and hands out the
        // primary a second time.
        let mut disk = FakeDisk::new(512, 4096);
        put_mbr_entry(&mut disk.data, 0, 0x83, 2048, 1024);
        put_mbr_entry(&mut disk.data, 1, 0x05, 0, 4096);
        disk.data[510] = 0x55;
        disk.data[511] = 0xAA;
        assert_eq!(scan_partitions(&disk.into_scheme()), vec![(2048, 1024)]);
    }

    #[test]
    fn a_logical_partition_has_to_fit_inside_its_container() {
        // The container ends at its own start plus its length. One that ends
        // exactly there is inside; one that ends a block later is a table
        // that lies, and handing it out overlaps whatever comes next.
        let scan = |count: u32| {
            let mut disk = FakeDisk::new(512, 8192);
            put_mbr_entry(&mut disk.data, 0, 0x05, 64, 16);
            disk.data[510] = 0x55;
            disk.data[511] = 0xAA;
            let ebr = 64 * 512;
            put_mbr_entry(&mut disk.data[ebr..ebr + 512], 0, 0x83, 1, count);
            disk.data[ebr + 510] = 0x55;
            disk.data[ebr + 511] = 0xAA;
            scan_partitions(&disk.into_scheme())
        };
        assert_eq!(
            scan(15),
            vec![(65, 15)],
            "ends exactly at the container's end"
        );
        assert!(scan(16).is_empty(), "one block past it");
    }

    #[test]
    fn the_next_ebr_is_addressed_from_the_container_not_from_the_one_before() {
        // An EBR's second entry is relative to the *container*; its first is
        // relative to the EBR itself. Following the link from the current EBR
        // walks off down the disk and loses the rest of the chain.
        let mut disk = FakeDisk::new(512, 8192);
        put_mbr_entry(&mut disk.data, 0, 0x05, 1024, 3072);
        disk.data[510] = 0x55;
        disk.data[511] = 0xAA;
        for (i, &(rel_next, size)) in [(Some(512u32), 100u32), (Some(1024), 200), (None, 300)]
            .iter()
            .enumerate()
        {
            let at = (1024 + i * 512) * 512;
            put_mbr_entry(&mut disk.data[at..at + 512], 0, 0x83, 1, size);
            if let Some(r) = rel_next {
                put_mbr_entry(&mut disk.data[at..at + 512], 1, 0x05, r, 512);
            }
            disk.data[at + 510] = 0x55;
            disk.data[at + 511] = 0xAA;
        }
        assert_eq!(
            scan_partitions(&disk.into_scheme()),
            vec![(1025, 100), (1537, 200), (2049, 300)]
        );
    }

    #[test]
    fn a_chain_that_links_out_of_its_container_stops_at_the_boundary() {
        // The container's last block is `ext_start + ext_count - 1`, so a link
        // landing *on* `ext_end` is already outside it -- that sector belongs
        // to whatever partition comes next. Following it reads a stranger's
        // table, and its own second entry points back inside the container, so
        // the scan comes out with a partition it only reached by leaving.
        let mut disk = FakeDisk::new(512, 8192);
        put_mbr_entry(&mut disk.data, 0, 0x05, 1024, 512); // 1024..1536
        disk.data[510] = 0x55;
        disk.data[511] = 0xAA;
        let mut ebr = |lba: usize, count: u32, next_rel: u32| {
            let at = lba * 512;
            put_mbr_entry(&mut disk.data[at..at + 512], 0, 0x83, 1, count);
            if next_rel != 0 {
                put_mbr_entry(&mut disk.data[at..at + 512], 1, 0x05, next_rel, 512);
            }
            disk.data[at + 510] = 0x55;
            disk.data[at + 511] = 0xAA;
        };
        // The chain proper, then the stranger on the boundary, then the EBR it
        // would send us back to -- whose logical partition does fit inside.
        ebr(1024, 100, 512);
        ebr(1536, 10, 256);
        ebr(1280, 50, 0);
        assert_eq!(scan_partitions(&disk.into_scheme()), vec![(1025, 100)]);
    }

    #[test]
    fn an_extended_chain_on_a_4kn_disk_is_addressed_in_device_blocks() {
        // Every LBA in a partition table is a *device* block. Reading an EBR
        // in 512-byte units lands an eighth of the way in, where there is no
        // signature at all.
        let mut disk = FakeDisk::new(4096, 512);
        put_mbr_entry(&mut disk.data, 0, 0x05, 64, 16);
        disk.data[510] = 0x55;
        disk.data[511] = 0xAA;
        let ebr = 64 * 4096;
        put_mbr_entry(&mut disk.data[ebr..ebr + 512], 0, 0x83, 1, 8);
        disk.data[ebr + 510] = 0x55;
        disk.data[ebr + 511] = 0xAA;
        // 65 device blocks in and eight blocks long, in 512-byte API sectors.
        assert_eq!(scan_partitions(&disk.into_scheme()), vec![(65 * 8, 8 * 8)]);
    }

    // ── the GPT header and its entry array ─────────────────────────────

    #[test]
    fn the_backup_gpt_of_a_4kn_disk_is_on_its_last_device_block() {
        // The backup header's place is the last *device* block. Counting the
        // disk in 512-byte sectors puts it eight times too far out, where
        // there is nothing to read -- and a disk whose primary header is
        // damaged then comes up with no partitions at all.
        let mut disk = FakeDisk::new(4096, 64);
        put_protective_mbr(&mut disk.data);
        put_gpt(&mut disk.data, 4096, 63, 2, 128, &[(8, 15)]);
        assert_eq!(scan_partitions(&disk.into_scheme()), vec![(64, 64)]);
    }

    #[test]
    fn a_gpt_header_that_is_not_quite_right_is_not_a_gpt_header() {
        // Each of these is the difference between reading a table and walking
        // the array at the wrong stride or the wrong place. The MBR entry
        // underneath is what says the scan fell through rather than simply
        // finding nothing.
        let build = |fix: &dyn Fn(&mut Vec<u8>)| {
            let mut disk = FakeDisk::new(512, 4096);
            put_gpt(&mut disk.data, 512, 1, 2, 128, &[(2048, 3071)]);
            put_mbr_entry(&mut disk.data, 0, 0x83, 100, 200);
            disk.data[510] = 0x55;
            disk.data[511] = 0xAA;
            fix(&mut disk.data);
            scan_partitions(&disk.into_scheme())
        };
        assert_eq!(build(&|_| {}), vec![(2048, 1024)], "as written it is a GPT");
        assert_eq!(
            build(&|d| d[512 + 7] = b'X'),
            vec![(100, 200)],
            "the whole eight-byte signature has to match"
        );
        assert_eq!(
            build(&|d| d[512 + 72..512 + 80].copy_from_slice(&1u64.to_le_bytes())),
            vec![(100, 200)],
            "an entry array that overlaps the header itself"
        );
        assert_eq!(
            build(&|d| d[512 + 80..512 + 84].copy_from_slice(&0u32.to_le_bytes())),
            vec![(100, 200)],
            "a header that declares no entries at all"
        );
        assert_eq!(
            build(&|d| d[512 + 84..512 + 88].copy_from_slice(&192u32.to_le_bytes())),
            vec![(100, 200)],
            "an entry size that is not a multiple of 128"
        );
    }

    #[test]
    fn only_the_entries_the_header_declares_are_read() {
        // The array is as long as the header says it is. Reading further
        // finds whatever a previous, longer table left behind: partitions
        // that are not in this one.
        let mut disk = FakeDisk::new(512, 4096);
        put_protective_mbr(&mut disk.data);
        put_gpt(
            &mut disk.data,
            512,
            1,
            2,
            128,
            &[(2048, 2559), (2560, 3071), (3072, 3583)],
        );
        disk.data[512 + 80..512 + 84].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(
            scan_partitions(&disk.into_scheme()),
            vec![(2048, 512), (2560, 512)],
            "the third entry is written but not declared"
        );
    }

    #[test]
    fn a_gpt_entry_is_read_by_its_whole_type_guid_and_its_whole_range() {
        let mut disk = FakeDisk::new(512, 4096);
        put_protective_mbr(&mut disk.data);
        put_gpt(
            &mut disk.data,
            512,
            1,
            2,
            128,
            &[(2048, 2048), (2560, 2600), (3000, 3100)],
        );
        let e = 2 * 512;
        // A type GUID whose first eight bytes are zero is still a type GUID.
        disk.data[e..e + 8].copy_from_slice(&[0u8; 8]);
        // An entry that starts on LBA 0 is not a partition, whatever it says
        // its last block is.
        disk.data[e + 128 + 32..e + 128 + 40].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(
            scan_partitions(&disk.into_scheme()),
            vec![(2048, 1), (3000, 101)],
            "a one-block partition is a partition; an LBA-0 entry is not"
        );
    }

    #[test]
    fn a_disk_with_no_table_at_all_yields_nothing() {
        let disk = FakeDisk::new(512, 4096);
        assert!(scan_partitions(&disk.into_scheme()).is_empty());
    }
}
