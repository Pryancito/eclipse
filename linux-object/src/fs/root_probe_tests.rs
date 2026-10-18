//! Which filesystem `open_block_root` pivots onto. EclipseFS is the
//! installed default and btrfs the alternative, and both must still boot.

use super::eclipsefs_mount::testdisk::{formatted, RamDisk};
use super::*;

fn block_inode(disk: Arc<RamDisk>) -> Arc<dyn INode> {
    Arc::new(devfs::BlockDev::new(0, disk, String::from("sda2")))
}

struct BtrfsFormatter(Arc<RamDisk>);

impl btrfs::BlockDevice for BtrfsFormatter {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> btrfs::Result<()> {
        let d = self.0.bytes.lock();
        let o = offset as usize;
        buf.copy_from_slice(&d[o..o + buf.len()]);
        Ok(())
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> btrfs::Result<()> {
        let mut d = self.0.bytes.lock();
        let o = offset as usize;
        d[o..o + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn sync(&self) -> btrfs::Result<()> {
        Ok(())
    }
    fn size(&self) -> u64 {
        self.0.bytes.lock().len() as u64
    }
}

fn btrfs_disk(bytes: usize) -> Arc<RamDisk> {
    let disk = super::eclipsefs_mount::testdisk::blank(bytes);
    let opts = btrfs::mkfs::MkfsOptions {
        label: String::from("ROOT"),
        fsid: [1; 16],
        chunk_uuid: [2; 16],
        dev_uuid: [3; 16],
        subvol_uuid: [4; 16],
        now: (1_700_000_000, 0),
    };
    btrfs::mkfs::format(&BtrfsFormatter(disk.clone()), &opts).unwrap();
    disk
}

#[test]
fn an_eclipsefs_partition_is_pivoted_onto_as_eclipsefs() {
    let disk = formatted(16 * 1024 * 1024);
    let (fs, fstype) = open_block_root(block_inode(disk)).expect("EclipseFS root");
    assert_eq!(fstype, "eclipsefs");
    assert!(fs.root_inode().list().is_ok());
    assert!(is_installed_block_root(fstype));
}

#[test]
fn a_btrfs_partition_still_boots() {
    let disk = btrfs_disk(32 * 1024 * 1024);
    let (_, fstype) = open_block_root(block_inode(disk)).expect("btrfs root");
    assert_eq!(fstype, "btrfs");
    assert!(is_installed_block_root(fstype));
}

/// A partition that held btrfs and was rewritten with EclipseFS can keep
/// the old btrfs superblock at 64 KiB; the root is the EclipseFS.
#[test]
fn eclipsefs_wins_over_a_stale_btrfs_superblock() {
    let old = btrfs_disk(32 * 1024 * 1024);
    let stale: Vec<u8> = old.bytes.lock()[0x10000..0x11000].to_vec();
    let disk = formatted(32 * 1024 * 1024);
    disk.bytes.lock()[0x10000..0x11000].copy_from_slice(&stale);
    let dev: Arc<dyn zcore_drivers::scheme::BlockScheme> = disk.clone();
    assert!(btrfs_mount::probe_btrfs_superblock(&dev));
    assert_eq!(open_block_root(block_inode(disk)).unwrap().1, "eclipsefs");
}

#[test]
fn only_installed_filesystems_count_as_an_installed_root() {
    assert!(!is_installed_block_root("rootfs"));
    assert!(!is_installed_block_root("sfs"));
}

/// Reads of a file on an installed root wait for their turn in a queue
/// (`file::disk_read_gate`), the files of one filesystem in the same one; a
/// file in RAM reads at once.
#[test]
fn files_of_an_installed_root_read_in_turn_and_files_in_ram_do_not() {
    for (disk, want) in [
        (formatted(16 * 1024 * 1024), "eclipsefs"),
        (btrfs_disk(32 * 1024 * 1024), "btrfs"),
    ] {
        let (fs, fstype) = open_block_root(block_inode(disk)).expect(want);
        assert_eq!(fstype, want);
        let root = fs.root_inode();
        let a = root.create("a", FileType::File, 0o644).unwrap();
        let b = root.create("b", FileType::File, 0o644).unwrap();
        let gate = file::disk_read_gate(&a).expect(want);
        assert!(core::ptr::eq(gate, file::disk_read_gate(&b).unwrap()));
    }
    let ram = RamFS::new().root_inode();
    let f = ram.create("a", FileType::File, 0o644).unwrap();
    assert!(file::disk_read_gate(&f).is_none());
}
