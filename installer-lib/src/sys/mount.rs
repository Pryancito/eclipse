//! Wrappers nativos para mount/umount/sync

use std::ffi::{CString, CStr};
use std::io;
use std::path::Path;

/// Flags para mount (de sys/mount.h)
pub const MS_RDONLY: u64 = 1;
pub const MS_NOSUID: u64 = 2;
pub const MS_NODEV: u64 = 4;
pub const MS_NOEXEC: u64 = 8;
pub const MS_SYNCHRONOUS: u64 = 16;
pub const MS_REMOUNT: u64 = 32;

/// Flags para umount2
pub const MNT_FORCE: i32 = 1;
pub const MNT_DETACH: i32 = 2;
pub const MNT_EXPIRE: i32 = 4;
pub const UMOUNT_NOFOLLOW: i32 = 8;

extern "C" {
    fn mount(
        source: *const libc::c_char,
        target: *const libc::c_char,
        filesystemtype: *const libc::c_char,
        mountflags: libc::c_ulong,
        data: *const libc::c_void,
    ) -> libc::c_int;
    
    fn umount2(
        target: *const libc::c_char,
        flags: libc::c_int,
    ) -> libc::c_int;
    
    fn sync();
}

/// Montar un filesystem
pub fn mount_fs<P: AsRef<Path>>(
    source: P,
    target: P,
    fstype: &str,
    flags: u64,
) -> io::Result<()> {
    let source_cstr = CString::new(source.as_ref().to_str().unwrap())?;
    let target_cstr = CString::new(target.as_ref().to_str().unwrap())?;
    let fstype_cstr = CString::new(fstype)?;
    
    let result = unsafe {
        mount(
            source_cstr.as_ptr(),
            target_cstr.as_ptr(),
            fstype_cstr.as_ptr(),
            flags as libc::c_ulong,
            std::ptr::null(),
        )
    };
    
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Desmontar un filesystem
pub fn umount_fs<P: AsRef<Path>>(target: P, flags: i32) -> io::Result<()> {
    let target_cstr = CString::new(target.as_ref().to_str().unwrap())?;
    
    let result = unsafe {
        umount2(target_cstr.as_ptr(), flags)
    };
    
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Sincronizar filesystems (escribir buffers al disco)
pub fn sync_fs() {
    unsafe {
        sync();
    }
}

/// Montar FAT32
pub fn mount_fat32<P: AsRef<Path>>(source: P, target: P) -> io::Result<()> {
    mount_fs(source, target, "vfat", 0)
}

/// Montar EclipseFS usando FUSE
pub fn mount_eclipsefs<P: AsRef<Path>>(source: P, target: P) -> io::Result<()> {
    // Para EclipseFS usaremos nuestro propio FUSE driver
    mount_fs(source, target, "fuse", 0)
}

/// Desmontar de forma segura (con sync)
pub fn safe_umount<P: AsRef<Path>>(target: P) -> io::Result<()> {
    sync_fs();
    umount_fs(target, 0)
}

/// Desmontar forzado
pub fn force_umount<P: AsRef<Path>>(target: P) -> io::Result<()> {
    umount_fs(target, MNT_FORCE | MNT_DETACH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    
    #[test]
    fn test_mount_wrapper_compiles() {
        // Solo verificar que compila
        let _ = mount_fs("/dev/sda1", "/mnt/test", "ext4", 0);
    }
}

