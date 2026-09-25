//! mount(2) and umount2(2)

use super::*;
use linux_object::fs::{mount_fs, umount_fs};
use linux_object::process::CAP_SYS_ADMIN;

/// Whether the caller may touch the mount table at all.
///
/// `fs/namespace.c` puts the question in one function and every mount-table
/// syscall -- `mount`, `umount`, `move_mount`, `pivot_root`, the lot -- opens
/// with it:
///
/// ```c
/// bool may_mount(void)
/// {
///         return ns_capable(current->nsproxy->mnt_ns->user_ns, CAP_SYS_ADMIN);
/// }
/// ```
///
/// Ahead of everything else, so an unprivileged caller cannot even learn from
/// the error code whether a path exists.
fn may_mount(syscall: &Syscall<'_>) -> linux_object::error::LxResult<()> {
    if syscall.linux_process().capable(CAP_SYS_ADMIN) {
        Ok(())
    } else {
        Err(LxError::EPERM)
    }
}

impl Syscall<'_> {
    /// Mount a filesystem.
    pub fn sys_mount(
        &self,
        source: UserInPtr<u8>,
        target: UserInPtr<u8>,
        fstype: UserInPtr<u8>,
        flags: usize,
        data: UserInPtr<u8>,
    ) -> SysResult {
        let source = source.as_c_str()?;
        let target = target.as_c_str()?;
        let fstype = fstype.as_c_str()?;
        let data = if data.is_null() { "" } else { data.as_c_str()? };
        info!(
            "mount: source={:?}, target={:?}, fstype={:?}, flags={:#x}",
            source, target, fstype, flags
        );
        may_mount(self)?;
        mount_fs(self.linux_process(), source, target, fstype, flags, data)?;
        linux_object::fs::dcache_invalidate();
        Ok(0)
    }

    /// Unmount a filesystem.
    pub fn sys_umount2(&self, target: UserInPtr<u8>, flags: usize) -> SysResult {
        let target = target.as_c_str()?;
        info!("umount2: target={:?}, flags={:#x}", target, flags);
        may_mount(self)?;
        umount_fs(target, flags)?;
        linux_object::fs::dcache_invalidate();
        Ok(0)
    }
}
