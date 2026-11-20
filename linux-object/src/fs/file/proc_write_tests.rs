//! A `/proc` setting is stored with its file's lock let go: the store of
//! `/proc/switch_root` reads the installed root's superblock from a disk
//! served from userspace, and a wait for that disk with a kernel lock held
//! can hang the CPU the disk's driver needs (seen on real hardware).

use super::*;
use crate::fs::procfs::ProcSysWritableINode;
use lock::Mutex;

/// The file the store below looks at, and what it saw.
static OPEN: Mutex<Option<Arc<File>>> = Mutex::new(None);
static SEEN: Mutex<Option<(String, bool)>> = Mutex::new(None);

fn store(value: &str) -> rcore_fs::vfs::Result<()> {
    let file = OPEN.lock().clone().unwrap();
    let free = file.inner.try_write().is_some();
    *SEEN.lock() = Some((String::from(value), free));
    Ok(())
}

#[test]
fn a_proc_setting_is_stored_with_its_file_unlocked() {
    let inode: Arc<dyn INode> = Arc::new(ProcSysWritableINode {
        inode: 1,
        generate: String::new,
        store,
    });
    let file = File::new(inode, OpenFlags::WRONLY, String::from("/proc/test"));
    *OPEN.lock() = Some(file.clone());

    assert_eq!(file.write(b"/dev/nvme0n1p2\n").unwrap(), 15);
    assert_eq!(
        SEEN.lock().take(),
        Some((String::from("/dev/nvme0n1p2"), true))
    );
    // The offset still moves, as for any other write.
    assert_eq!(file.inner.read().offset, 15);
    *OPEN.lock() = None;
}

#[test]
fn a_proc_setting_open_for_reading_refuses_a_write() {
    let inode: Arc<dyn INode> = Arc::new(ProcSysWritableINode {
        inode: 2,
        generate: String::new,
        store: |_| panic!("stored through a read-only file"),
    });
    let file = File::new(inode, OpenFlags::RDONLY, String::from("/proc/test"));
    assert_eq!(file.write(b"x").unwrap_err(), LxError::EBADF);
}
