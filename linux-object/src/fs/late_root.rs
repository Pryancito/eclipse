//! The installed root that comes after init.
//!
//! The kernel picks `/` in [`super::create_root_fs`], before any process
//! runs. An installed system's root is a partition of its disk (`ROOT=` on
//! the command line, `/dev/nvme0n1p2`), and every disk controller is driven
//! by a process now (NVMe, AHCI, virtio-blk, xHCI). That process is started
//! by init, from the boot medium (the initramfs on the ESP): when the kernel
//! looks, the disk is not there yet, `/` stays the boot medium, and the
//! machine runs the installer's small image with nothing of the installed
//! system mounted but the ESP.
//!
//! So that root is remembered here as **awaited** ([`awaited`]): `ROOT=`
//! names a device that is not there, of a family a userspace driver was
//! left to serve. Init (`eclipse-init`) reads it in `/proc/switch_root`,
//! starts the disk drivers, waits for the device and writes its name back
//! ([`switch`]). The kernel then does what it would have done at boot: opens
//! the EclipseFS or btrfs on it, mounts on it the same /dev, /tmp and /run
//! the boot medium wore (what the drivers left in /run is still there), and
//! makes it the root of every process whose root was the boot medium's. Init
//! then executes the installed system's own init, which starts the rest.
//!
//! Like Linux's `switch_root` from an initramfs, except that the boot medium
//! stays in memory under the processes started from it (the drivers keep
//! running on their own image).

use alloc::{
    string::{String, ToString},
    sync::Arc,
};

use lock::Mutex;
use rcore_fs::vfs::INode;
use rcore_fs_devfs::DevFS;
use rcore_fs_mountfs::{MNode, MountFS};
use rcore_fs_ramfs::RamFS;

use crate::error::{LxError, LxResult};
use crate::process::ProcessExt;

/// The pseudo filesystems the boot medium wears, which a later root wears
/// too.
pub(super) struct Pseudo {
    pub devfs: Arc<DevFS>,
    pub tmp: Arc<RamFS>,
    pub run: Arc<RamFS>,
}

/// The boot medium kept as `/`, while it still is.
struct Boot {
    root: Arc<MNode>,
    pseudo: Pseudo,
    /// The device `ROOT=` names, when it is awaited.
    awaited: Option<String>,
}

static BOOT: Mutex<Option<Boot>> = Mutex::new(None);

/// The disk families a root can be on, and the drivers (`udriver=` kinds)
/// that serve each: the node names the kernel gives their disks.
fn families(name: &str) -> &'static [&'static str] {
    use kernel_hal::drivers::user::{AHCI, NVME, VIRTIO_BLK, XHCI};
    const NVMES: &[&str] = &[NVME];
    const VIRTIOS: &[&str] = &[VIRTIO_BLK];
    // A SATA disk, or a USB one behind xHCI.
    const SCSI: &[&str] = &[AHCI, XHCI];
    if name.starts_with("nvme") {
        NVMES
    } else if name.starts_with("vd") {
        VIRTIOS
    } else if name.starts_with("sd") {
        SCSI
    } else {
        &[]
    }
}

/// Whether the device `dev` (`/dev/nvme0n1p2`) is one a driver in
/// `left` (`/proc/udrivers`' lines, `nvme@0`) would bring: a disk of its
/// family. Without such a driver nothing will bring it, and init must not
/// wait for it (`make qemu` says `ROOT=/dev/vda` and has no disk there).
pub(super) fn served_later(dev: &str, left: &[String]) -> bool {
    let name = dev.rsplit('/').next().unwrap_or(dev);
    let kinds = families(name);
    left.iter().any(|device| {
        let kind = device.split('@').next().unwrap_or(device);
        kinds.contains(&kind)
    })
}

/// Remember the boot medium kept as `/`, and whether `ROOT=` names a
/// device a driver process will bring.
pub(super) fn note_boot_root(root: &Arc<MNode>, pseudo: Pseudo) {
    let cmdline = kernel_hal::boot::cmdline();
    let keep = cmdline
        .split(':')
        .any(|o| o.trim().eq_ignore_ascii_case("ROOTKEEP=1"));
    let awaited = super::parse_root_cmdline(&cmdline)
        .filter(|_| !keep)
        .filter(|dev| device_node(dev).is_none())
        .filter(|dev| served_later(dev, &kernel_hal::drivers::user::reserved()))
        .map(String::from);
    if let Some(dev) = &awaited {
        warn!(
            "[boot] create_root_fs: ROOT={} is not here yet; its driver is a process, init asks for it in /proc/switch_root",
            dev
        );
    }
    *BOOT.lock() = Some(Boot {
        root: root.clone(),
        pseudo,
        awaited,
    });
}

/// The device the root is awaited on, if it is (`/proc/switch_root`).
pub fn awaited() -> Option<String> {
    BOOT.lock().as_ref().and_then(|b| b.awaited.clone())
}

/// The `/dev` node named by `dev`, if it is there.
fn device_node(dev: &str) -> Option<Arc<dyn INode>> {
    let name = dev.trim().rsplit('/').next()?;
    let devfs = super::DEVFS_ROOT.lock().clone()?;
    devfs.find(name).ok()
}

/// Make the awaited device `dev` the root (see the module's text).
///
/// `EINVAL` for another device than the awaited one, or for one with no
/// EclipseFS or btrfs on it; `EBUSY` once the root is no longer the boot
/// medium, or when nothing is awaited; `ENOENT` while the device is not
/// there yet.
pub fn switch(dev: &str) -> LxResult<String> {
    // Taken out, and the lock let go: everything below reads the disk, and
    // a disk a process serves is waited for by giving the CPU away, which
    // must not be done holding a spin lock. Put back on a refusal.
    let b = {
        let mut boot = BOOT.lock();
        match boot.as_ref().and_then(|b| b.awaited.as_deref()) {
            None => return Err(LxError::EBUSY),
            Some(want) if dev.trim() != want => return Err(LxError::EINVAL),
            Some(_) => {}
        }
        boot.take().unwrap()
    };
    match switch_to(&b) {
        Ok(source) => Ok(source),
        Err(e) => {
            *BOOT.lock() = Some(b);
            Err(e)
        }
    }
}

fn switch_to(b: &Boot) -> LxResult<String> {
    let want = b.awaited.as_deref().unwrap_or_default();
    let inode = device_node(want).ok_or(LxError::ENOENT)?;
    let Some((fs, fstype)) = super::open_block_root(inode) else {
        warn!("[root] {} has no EclipseFS or btrfs that mounts", want);
        return Err(LxError::EINVAL);
    };
    warn!("[root] switching / to {} ({})", want, fstype);
    let mountfs = MountFS::new(fs);
    let root = super::dress_root(
        &mountfs,
        want,
        fstype,
        b.pseudo.devfs.clone(),
        b.pseudo.tmp.clone(),
        b.pseudo.run.clone(),
    );
    let new: Arc<dyn INode> = root;
    let mut moved = 0usize;
    for proc in crate::process::live_processes() {
        let linux = proc.linux();
        if !same_inode(&linux.root_inode(), &b.root) {
            continue;
        }
        linux.set_root_inode(new.clone());
        // The same path on the new root (`/` for init); one that is not
        // there holds no directory, like a removed one.
        let cwd = linux.current_working_directory();
        linux.change_directory(&alloc::format!("/{}", cwd.trim_start_matches('/')));
        moved += 1;
    }
    super::dcache_invalidate();
    warn!("[root] / is {} now ({} process(es) moved)", want, moved);
    // The kernel's own pass over the new root's fstab, as after a boot
    // that found the root at once.
    kernel_hal::thread::spawn(async {
        super::mount_fstab_deferred().await;
    });
    Ok(want.to_string())
}

/// Whether `a` is the inode `b`, by address (an `Arc<dyn INode>` made from
/// the same `Arc<MNode>`).
fn same_inode(a: &Arc<dyn INode>, b: &Arc<MNode>) -> bool {
    core::ptr::eq(Arc::as_ptr(a) as *const u8, Arc::as_ptr(b) as *const u8)
}

/// `/proc/switch_root`: the device the installed root is awaited on, one
/// line, while `/` is still the boot medium because that device's driver is
/// a process init has not started yet; empty otherwise. Init (PID 1) writes
/// the device back once it is there, and it becomes `/` ([`switch`]).
pub(crate) fn proc_content() -> String {
    awaited().map(|dev| format!("{dev}\n")).unwrap_or_default()
}

/// A write of `/proc/switch_root` by the process `pid`: only PID 1 may.
pub(crate) fn proc_store(dev: &str, pid: Option<u64>) -> rcore_fs::vfs::Result<()> {
    use rcore_fs::vfs::FsError;
    if pid != Some(1) {
        return Err(FsError::NotPermitted);
    }
    switch(dev).map(|_| ()).map_err(|e| match e {
        LxError::ENOENT => FsError::EntryNotFound,
        LxError::EBUSY => FsError::Busy,
        _ => FsError::InvalidParam,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn left(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn an_nvme_root_is_awaited_when_nvme_was_left_to_a_process() {
        assert!(served_later("/dev/nvme0n1p2", &left(&["nvme@0"])));
        assert!(served_later("/dev/nvme1n1p3", &left(&["ahci@0", "nvme@1"])));
        assert!(!served_later("/dev/nvme0n1p2", &left(&["ahci@0"])));
    }

    #[test]
    fn a_sata_or_usb_root_is_awaited_from_ahci_or_xhci() {
        assert!(served_later("/dev/sda2", &left(&["ahci@1"])));
        assert!(served_later("/dev/sdb2", &left(&["xhci@0"])));
        assert!(!served_later(
            "/dev/sda2",
            &left(&["nvme@0", "virtio-net@0"])
        ));
    }

    #[test]
    fn a_virtio_root_is_awaited_only_from_virtio_blk() {
        assert!(served_later("/dev/vda2", &left(&["virtio-blk@0"])));
        // `make qemu` on x86_64: ROOT=/dev/vda, the ESP on q35's AHCI.
        assert!(!served_later("/dev/vda", &left(&["ahci@0", "e1000e@0"])));
    }

    #[test]
    fn nothing_left_awaits_nothing() {
        assert!(!served_later("/dev/nvme0n1p2", &[]));
        assert!(!served_later(
            "/dev/mmcblk0p2",
            &left(&["nvme@0", "ahci@0"])
        ));
    }
}
