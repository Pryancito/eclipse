//! Sistema de syscalls del microkernel Eclipse
//!
//! Interfaz entre userspace y kernel: despacho central, compatibilidad Linux x86-64
//! y extensiones nativas (≥500).

// --- BEGIN fs ---
//! Filesystem-related syscalls implementation
//!
//! Implementation of VFS operations, scheme interaction, and file descriptor management.

use crate::process::current_process_id;
use alloc::format;
use alloc::vec::Vec;
use crate::serial;
use core::sync::atomic::Ordering;
use spin::Mutex;

pub fn sys_read(fd: u64, buf_ptr: u64, len: u64) -> u64 {
    if !is_user_pointer(buf_ptr, len) { return linux_abi_error(14); }
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        let mut kbuf = Vec::with_capacity(len as usize);
        unsafe { kbuf.set_len(len as usize); }
        match crate::scheme::read(fd_entry.scheme_id, fd_entry.resource_id, &mut kbuf, fd_entry.offset) {
            Ok(n) => {
                crate::fd::fd_set_offset(pid, fd as usize, fd_entry.offset + n as u64);
                if n > 0 && !copy_to_user(buf_ptr, &kbuf[..n]) { return linux_abi_error(14); }
                n as u64
            }
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_write(fd: u64, buf_ptr: u64, len: u64) -> u64 {
    if !is_user_pointer(buf_ptr, len) { return linux_abi_error(14); }
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        let mut kbuf = Vec::with_capacity(len as usize);
        unsafe { kbuf.set_len(len as usize); }
        if len > 0 && !copy_from_user(buf_ptr, &mut kbuf) { return linux_abi_error(14); }
        match crate::scheme::write(fd_entry.scheme_id, fd_entry.resource_id, &kbuf, fd_entry.offset) {
            Ok(n) => {
                crate::fd::fd_set_offset(pid, fd as usize, fd_entry.offset + n as u64);
                n as u64
            }
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_open(path_ptr: u64, flags: u64, mode: u64) -> u64 {
    let len = strlen_user_unique(path_ptr, 1023);
    if len == 0 { return linux_abi_error(2); }
    let mut path_buf = [0u8; 1024];
    if !copy_from_user(path_ptr, &mut path_buf[..len]) { return linux_abi_error(14); }
    let path = core::str::from_utf8(&path_buf[..len]).unwrap_or("");
    
    let pid = current_process_id().unwrap_or(0);
    let open_path_storage;
    let open_path: &str = if path.contains(':') {
        // Already a scheme path.
        path
    } else if path.starts_with('/') {
        // POSIX absolute paths → schemes.
        open_path_storage = user_path_to_scheme_path(path);
        open_path_storage.as_str()
    } else {
        // Relative path: best-effort treat as file: (cwd resolution is handled elsewhere in the kernel).
        open_path_storage = format!("file:{}", path);
        open_path_storage.as_str()
    };

    // Diagnóstico: labwc/Fontconfig suele colgar si open() se bloquea en el VFS.
    // Este log nos da el path real que está intentando abrir (solo para los PIDs del compositor).
    if pid == 9 || pid == 10 || pid == 11 {
        crate::serial::serial_printf(format_args!(
            "[open-diag] pid={} path='{}' open_path='{}' flags={:#x} mode={:#x}\n",
            pid, path, open_path, flags, mode
        ));
    }

    match crate::scheme::open(open_path, flags as usize, mode as u32) {
        Ok((scheme_id, resource_id)) => {
            if pid == 9 || pid == 10 || pid == 11 {
                crate::serial::serial_printf(format_args!(
                    "[open-diag] pid={} OK scheme_id={} resource_id={}\n",
                    pid, scheme_id, resource_id
                ));
            }
            crate::fd::fd_create(pid, scheme_id, resource_id).unwrap_or(0) as u64
        }
        Err(e) => {
            if pid == 9 || pid == 10 || pid == 11 {
                crate::serial::serial_printf(format_args!(
                    "[open-diag] pid={} ERR {}\n",
                    pid, e
                ));
            }
            (-(e as isize)) as u64
        }
    }
}

pub fn sys_openat(_dfd: u64, path_ptr: u64, flags: u64, mode: u64) -> u64 {
    // Basic openat stub, should handle relative paths using dfd
    sys_open(path_ptr, flags, mode)
}

pub fn sys_close(fd: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if crate::fd::fd_close(pid, fd as usize) { 0 } else { linux_abi_error(9) }
}

/// Rutas absolutas de usuario (`/…`) → schemes internos.
/// - `/dev/dri/*`  → `drm:*`
/// - `/dev/input/*`→ `input:*`
/// - `/dev/shm/*`  → `shm:*`
/// - `/dev/*`      → `dev:*`
/// - resto         → `file:/…`
fn user_path_to_scheme_path(path: &str) -> alloc::string::String {
    if path == "/dev/dri" || path.starts_with("/dev/dri/") {
        let rel = path.trim_start_matches("/dev/dri").trim_start_matches('/');
        return format!("drm:{}", rel);
    }
    if path == "/dev/input" || path.starts_with("/dev/input/") {
        let rel = path.trim_start_matches("/dev/input").trim_start_matches('/');
        return format!("input:{}", rel);
    }
    if path.starts_with("/dev/shm/") {
        let rel = path.trim_start_matches("/dev/shm/").trim_start_matches('/');
        return format!("shm:{}", rel);
    }
    if path == "/dev" || path.starts_with("/dev/") {
        let rel = path.trim_start_matches("/dev").trim_start_matches('/');
        return format!("dev:{}", rel);
    }
    if path.starts_with('/') {
        return format!("file:{}", path);
    }
    alloc::string::String::from(path)
}

pub fn sys_lseek(fd: u64, offset: i64, whence: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        match crate::scheme::lseek(fd_entry.scheme_id, fd_entry.resource_id, offset as isize, whence as usize, fd_entry.offset) {
            Ok(n) => {
                crate::fd::fd_set_offset(pid, fd as usize, n as u64);
                n as u64
            }
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_ioctl(fd: u64, request: u64, arg: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        match crate::scheme::ioctl(fd_entry.scheme_id, fd_entry.resource_id, request as usize, arg as usize) {
            Ok(n) => n as u64,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_ftruncate(fd: u64, len: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        match crate::scheme::ftruncate(fd_entry.scheme_id, fd_entry.resource_id, len as usize) {
            Ok(_) => 0,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_pread64(fd: u64, buf_ptr: u64, len: u64, offset: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        if !is_user_pointer(buf_ptr, len) { return linux_abi_error(14); }
        let mut kbuf = Vec::with_capacity(len as usize);
        unsafe { kbuf.set_len(len as usize); }
        match crate::scheme::pread(fd_entry.scheme_id, fd_entry.resource_id, &mut kbuf, offset) {
            Ok(n) => n as u64,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_pwrite64(fd: u64, buf_ptr: u64, len: u64, offset: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        if !is_user_pointer(buf_ptr, len) { return linux_abi_error(14); }
        let mut kbuf = Vec::with_capacity(len as usize);
        unsafe { kbuf.set_len(len as usize); }
        if len > 0 && !copy_from_user(buf_ptr, &mut kbuf) { return linux_abi_error(14); }
        match crate::scheme::pwrite(fd_entry.scheme_id, fd_entry.resource_id, &kbuf, offset) {
            Ok(n) => n as u64,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}


pub fn sys_writev(fd: u64, iov_ptr: u64, count: u64) -> u64 {
    let mut total = 0;
    let pid = current_process_id().unwrap_or(0);
    for i in 0..count {
        let iov_addr = iov_ptr + i * 16;
        if !is_user_pointer(iov_addr, 16) { return linux_abi_error(14); }
        let mut raw = [0u8; 16];
        if !copy_from_user(iov_addr, &mut raw) { return linux_abi_error(14); }
        let base = u64::from_le_bytes(raw[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(raw[8..16].try_into().unwrap());
        
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            if len > 0 && !is_user_pointer(base, len) { return linux_abi_error(14); }
            let mut kbuf = Vec::with_capacity(len as usize);
            unsafe { kbuf.set_len(len as usize); }
            if len > 0 && !copy_from_user(base, &mut kbuf) { return linux_abi_error(14); }
            match crate::scheme::write(fd_entry.scheme_id, fd_entry.resource_id, &kbuf, fd_entry.offset) {
                Ok(n) => {
                    crate::fd::fd_set_offset(pid, fd as usize, fd_entry.offset + n as u64);
                    total += n as u64;
                }
                Err(e) => return (-(e as isize)) as u64,
            }
        } else {
            return linux_abi_error(9);
        }
    }
    total
}

pub fn sys_readv(fd: u64, iov_ptr: u64, count: u64) -> u64 {
    let mut total = 0;
    let pid = current_process_id().unwrap_or(0);
    for i in 0..count {
        let iov_addr = iov_ptr + i * 16;
        if !is_user_pointer(iov_addr, 16) { return linux_abi_error(14); }
        let mut raw = [0u8; 16];
        if !copy_from_user(iov_addr, &mut raw) { return linux_abi_error(14); }
        let base = u64::from_le_bytes(raw[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(raw[8..16].try_into().unwrap());
        
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            if len > 0 && !is_user_pointer(base, len) { return linux_abi_error(14); }
            let mut kbuf = Vec::with_capacity(len as usize);
            unsafe { kbuf.set_len(len as usize); }
            match crate::scheme::read(fd_entry.scheme_id, fd_entry.resource_id, &mut kbuf, fd_entry.offset) {
                Ok(n) => {
                    crate::fd::fd_set_offset(pid, fd as usize, fd_entry.offset + n as u64);
                    if n > 0 && !copy_to_user(base, &kbuf[..n]) { return linux_abi_error(14); }
                    total += n as u64;
                }
                Err(e) => return (-(e as isize)) as u64,
            }
        } else {
            return linux_abi_error(9);
        }
    }
    total
}

pub fn sys_preadv(fd: u64, iov_ptr: u64, count: u64, offset: u64) -> u64 {
    let mut total = 0;
    let mut current_offset = offset;
    let pid = current_process_id().unwrap_or(0);
    for i in 0..count {
        let iov_addr = iov_ptr + i * 16;
        if !is_user_pointer(iov_addr, 16) { return linux_abi_error(14); }
        let mut raw = [0u8; 16];
        if !copy_from_user(iov_addr, &mut raw) { return linux_abi_error(14); }
        let base = u64::from_le_bytes(raw[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(raw[8..16].try_into().unwrap());
        
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            if len > 0 && !is_user_pointer(base, len) { return linux_abi_error(14); }
            let mut kbuf = Vec::with_capacity(len as usize);
            unsafe { kbuf.set_len(len as usize); }
            match crate::scheme::pread(fd_entry.scheme_id, fd_entry.resource_id, &mut kbuf, current_offset) {
                Ok(n) => {
                    total += n as u64;
                    current_offset += n as u64;
                    if n > 0 && !copy_to_user(base, &kbuf[..n]) { return linux_abi_error(14); }
                }
                Err(e) => return (-(e as isize)) as u64,
            }
        } else {
            return linux_abi_error(9);
        }
    }
    total
}

pub fn sys_pwritev(fd: u64, iov_ptr: u64, count: u64, offset: u64) -> u64 {
    let mut total = 0;
    let mut current_offset = offset;
    let pid = current_process_id().unwrap_or(0);
    for i in 0..count {
        let iov_addr = iov_ptr + i * 16;
        if !is_user_pointer(iov_addr, 16) { return linux_abi_error(14); }
        let mut raw = [0u8; 16];
        if !copy_from_user(iov_addr, &mut raw) { return linux_abi_error(14); }
        let base = u64::from_le_bytes(raw[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(raw[8..16].try_into().unwrap());
        
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            if len > 0 && !is_user_pointer(base, len) { return linux_abi_error(14); }
            let mut kbuf = Vec::with_capacity(len as usize);
            unsafe { kbuf.set_len(len as usize); }
            if len > 0 && !copy_from_user(base, &mut kbuf) { return linux_abi_error(14); }
            match crate::scheme::pwrite(fd_entry.scheme_id, fd_entry.resource_id, &kbuf, current_offset) {
                Ok(n) => {
                    total += n as u64;
                    current_offset += n as u64;
                }
                Err(e) => return (-(e as isize)) as u64,
            }
        } else {
            return linux_abi_error(9);
        }
    }
    total
}



pub fn sys_faccessat(_dfd: u64, path_ptr: u64, _mode: u64, _flags: u64) -> u64 {
    // Basic stub: check if path is valid
    let len = strlen_user_unique(path_ptr, 1023);
    if len == 0 { return linux_abi_error(2); }
    0
}

pub fn sys_pipe(pipefd_ptr: u64) -> u64 {
    sys_pipe2(pipefd_ptr, 0)
}

pub fn sys_pipe2(pipefd_ptr: u64, flags: u64) -> u64 {
    if !is_user_pointer(pipefd_ptr, 8) { return linux_abi_error(14); }
    let pid = current_process_id().unwrap_or(0);
    let scheme_id = match crate::scheme::get_scheme_id("pipe") {
        Some(id) => id,
        None => return linux_abi_error(38),
    };
    
    let (rh, wh) = crate::pipe::PIPE_SCHEME.new_pipe();
    
    // Set O_NONBLOCK if requested (0x800) or O_CLOEXEC (0x80000)
    if (flags & 0x800) != 0 {
        crate::pipe::PIPE_SCHEME.set_nonblock(rh, true);
        crate::pipe::PIPE_SCHEME.set_nonblock(wh, true);
    }

    let fd1 = match crate::fd::fd_create(pid, scheme_id, rh) {
        Some(fd) => fd,
        None => {
            use crate::scheme::Scheme;
            let _ = crate::pipe::PIPE_SCHEME.close(rh);
            let _ = crate::pipe::PIPE_SCHEME.close(wh);
            return linux_abi_error(24);
        }
    };
    let fd2 = match crate::fd::fd_create(pid, scheme_id, wh) {
        Some(fd) => fd,
        None => {
            use crate::scheme::Scheme;
            let _ = crate::fd::fd_close(pid, fd1);
            let _ = crate::pipe::PIPE_SCHEME.close(wh);
            return linux_abi_error(24);
        }
    };

    let b1 = (fd1 as i32).to_le_bytes();
    let b2 = (fd2 as i32).to_le_bytes();
    if !copy_to_user(pipefd_ptr, &b1) { return linux_abi_error(14); }
    if !copy_to_user(pipefd_ptr + 4, &b2) { return linux_abi_error(14); }
    0
}

pub fn sys_mkdir(path_ptr: u64, mode: u64) -> u64 {
    let len = strlen_user_unique(path_ptr, 1023);
    if len == 0 { return linux_abi_error(2); }
    let mut path_buf = [0u8; 1024];
    if !copy_from_user(path_ptr, &mut path_buf[..len]) { return linux_abi_error(14); }
    let path = core::str::from_utf8(&path_buf[..len]).unwrap_or("");

    match crate::scheme::mkdir(path, mode as u32) {
        Ok(_) => 0,
        Err(e) => (-(e as isize)) as u64,
    }
}

pub fn sys_rmdir(path_ptr: u64) -> u64 {
    let len = strlen_user_unique(path_ptr, 1023);
    if len == 0 { return linux_abi_error(2); }
    let mut path_buf = [0u8; 1024];
    if !copy_from_user(path_ptr, &mut path_buf[..len]) { return linux_abi_error(14); }
    let path = core::str::from_utf8(&path_buf[..len]).unwrap_or("");

    match crate::scheme::rmdir(path) {
        Ok(_) => 0,
        Err(e) => (-(e as isize)) as u64,
    }
}

pub fn sys_unlink(path_ptr: u64) -> u64 {
    let len = strlen_user_unique(path_ptr, 1023);
    if len == 0 { return linux_abi_error(2); }
    let mut path_buf = [0u8; 1024];
    if !copy_from_user(path_ptr, &mut path_buf[..len]) { return linux_abi_error(14); }
    let path = core::str::from_utf8(&path_buf[..len]).unwrap_or("");

    match crate::scheme::unlink(path) {
        Ok(_) => 0,
        Err(e) => (-(e as isize)) as u64,
    }
}

pub fn sys_rename(old_ptr: u64, new_ptr: u64) -> u64 {
    let old_len = strlen_user_unique(old_ptr, 1023);
    let new_len = strlen_user_unique(new_ptr, 1023);
    if old_len == 0 || new_len == 0 { return linux_abi_error(2); }
    
    let mut old_buf = [0u8; 1024];
    let mut new_buf = [0u8; 1024];
    if !copy_from_user(old_ptr, &mut old_buf[..old_len]) { return linux_abi_error(14); }
    if !copy_from_user(new_ptr, &mut new_buf[..new_len]) { return linux_abi_error(14); }
    
    let old_path = core::str::from_utf8(&old_buf[..old_len]).unwrap_or("");
    let new_path = core::str::from_utf8(&new_buf[..new_len]).unwrap_or("");

    match crate::scheme::rename(old_path, new_path) {
        Ok(_) => 0,
        Err(e) => (-(e as isize)) as u64,
    }
}

pub fn sys_readdir(fd: u64, buf_ptr: u64, len: u64) -> u64 {
    sys_getdents64(fd, buf_ptr, len)
}

pub fn sys_mount(src_ptr: u64, _target_ptr: u64) -> u64 {
    let src_len = strlen_user_unique(src_ptr, 1023);
    if src_len == 0 { return linux_abi_error(2); }
    let mut src_buf = [0u8; 1024];
    if !copy_from_user(src_ptr, &mut src_buf[..src_len]) { return linux_abi_error(14); }
    let src_path = core::str::from_utf8(&src_buf[..src_len]).unwrap_or("");
    
    // We only support mounting to the global root for now.
    match crate::filesystem::Filesystem::mount(src_path) {
        Ok(_) => 0,
        Err(_) => u64::MAX,
    }
}

pub fn sys_fmap(fd: u64, offset: u64, len: u64) -> u64 {
    if let Some(pid) = current_process_id() {
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            match crate::scheme::fmap(fd_entry.scheme_id, fd_entry.resource_id, offset as usize, len as usize) {
                Ok(addr) => {
                    // Convertir dirección kernel a física si aplica (evita crash 0xffff8000...)
                    let phys_addr: u64 = if (addr as u64) >= crate::memory::PHYS_MEM_OFFSET {
                        (addr as u64) - crate::memory::PHYS_MEM_OFFSET
                    } else {
                        addr as u64
                    };
                    let page_table = crate::process::get_process_page_table(current_process_id());
                    let vaddr = crate::memory::map_shared_memory_for_process(
                        page_table,
                        phys_addr,
                        len as u64
                    );
                    return vaddr;
                }
                Err(e) => {
                    serial::serial_printf(format_args!("SYS_FMAP: scheme::fmap failed with error {}\n", e));
                    return u64::MAX;
                }
            }
        }
    }
    u64::MAX
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct LegacyStat {
    pub st_dev:     u64,
    pub st_ino:     u64,
    pub st_mode:    u32,
    pub st_nlink:   u32,
    pub st_uid:     u32,
    pub st_gid:     u32,
    pub st_rdev:    u64,
    pub st_size:    u64,
    pub st_atime:   i64,
    pub st_mtime:   i64,
    pub st_ctime:   i64,
}

fn scheme_stat_to_legacy_stat(s: &crate::scheme::Stat) -> LegacyStat {
    LegacyStat {
        st_dev:     s.dev,
        st_ino:     s.ino,
        st_mode:    s.mode,
        st_nlink:   s.nlink,
        st_uid:     s.uid,
        st_gid:     s.gid,
        st_rdev:    s.rdev,
        st_size:    s.size,
        st_atime:   s.atime,
        st_mtime:   s.mtime,
        st_ctime:   s.ctime,
    }
}

pub fn sys_stat(path_ptr: u64, stat_ptr: u64) -> u64 {
    let len = strlen_user_unique(path_ptr, 1023);
    if len == 0 { return linux_abi_error(2); }
    let mut path_buf = [0u8; 1024];
    if !copy_from_user(path_ptr, &mut path_buf[..len]) { return linux_abi_error(14); }
    let path = core::str::from_utf8(&path_buf[..len]).unwrap_or("");
    
    if !is_user_pointer(stat_ptr, core::mem::size_of::<LegacyStat>() as u64) {
        return linux_abi_error(14);
    }

    let mut s = crate::scheme::Stat::default();
    match crate::scheme::stat(path, &mut s) {
        Ok(_) => {
            let legacy_s = scheme_stat_to_legacy_stat(&s);
            let out = unsafe {
                core::slice::from_raw_parts(&legacy_s as *const LegacyStat as *const u8, core::mem::size_of::<LegacyStat>())
            };
            if !copy_to_user(stat_ptr, out) { return linux_abi_error(14); }
            0
        }
        Err(e) => (-(e as isize)) as u64,
    }
}

pub fn sys_fstat(fd: u64, stat_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        if !is_user_pointer(stat_ptr, core::mem::size_of::<LegacyStat>() as u64) {
            return linux_abi_error(14);
        }
        let mut s = crate::scheme::Stat::default();
        match crate::scheme::fstat(fd_entry.scheme_id, fd_entry.resource_id, &mut s) {
            Ok(_) => {
                let legacy_s = scheme_stat_to_legacy_stat(&s);
                let out = unsafe {
                    core::slice::from_raw_parts(&legacy_s as *const LegacyStat as *const u8, core::mem::size_of::<LegacyStat>())
                };
                if !copy_to_user(stat_ptr, out) { return linux_abi_error(14); }
                0
            }
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_fstatat(_dfd: u64, path_ptr: u64, stat_ptr: u64, _flags: u64) -> u64 {
    sys_stat(path_ptr, stat_ptr)
}

pub fn sys_sync() -> u64 {
    0
}

pub fn sys_truncate(path_ptr: u64, len: u64) -> u64 {
    let path_len = strlen_user_unique(path_ptr, 1023);
    if path_len == 0 { return linux_abi_error(2); }
    let mut path_buf = [0u8; 1024];
    if !copy_from_user(path_ptr, &mut path_buf[..path_len]) { return linux_abi_error(14); }
    let path = core::str::from_utf8(&path_buf[..path_len]).unwrap_or("");

    match crate::scheme::open(path, 0x02, 0) { // O_RDWR
        Ok((sid, rid)) => {
            let res = match crate::scheme::ftruncate(sid, rid, len as usize) {
                Ok(_) => 0,
                Err(e) => (-(e as isize)) as u64,
            };
            let _ = crate::scheme::close(sid, rid);
            res
        }
        Err(e) => (-(e as isize)) as u64,
    }
}

pub fn sys_creat(path_ptr: u64, mode: u64) -> u64 {
    sys_open(path_ptr, 0x40 | 0x01 | 0x08, mode) // O_CREAT|O_WRONLY|O_TRUNC
}

pub fn sys_link(old_ptr: u64, new_ptr: u64) -> u64 {
    // For now, no hardlinks across schemes
    linux_abi_error(38)
}

pub fn sys_mkdirat(dfd: u64, path_ptr: u64, mode: u64) -> u64 {
    // Simple implementation: ignore dfd if path is absolute or AT_FDCWD
    sys_mkdir(path_ptr, mode)
}

pub fn sys_symlink(_old: u64, _new: u64) -> u64 {
    linux_abi_error(38)
}

pub fn sys_renameat2(old_dfd: u64, old_ptr: u64, new_dfd: u64, new_ptr: u64, _flags: u64) -> u64 {
    sys_rename(old_ptr, new_ptr)
}

pub fn sys_readlink(path_ptr: u64, buf_ptr: u64, len: u64) -> u64 {
    let path_len = strlen_user_unique(path_ptr, 1023);
    if path_len == 0 { return linux_abi_error(2); }
    let mut path_buf = [0u8; 1024];
    if !copy_from_user(path_ptr, &mut path_buf[..path_len]) { return linux_abi_error(14); }
    let path = core::str::from_utf8(&path_buf[..path_len]).unwrap_or("");

    match crate::scheme::readlink(path, len as usize) {
        Ok(target) => {
            let target_bytes = target.as_bytes();
            let n = target_bytes.len().min(len as usize);
            if !copy_to_user(buf_ptr, &target_bytes[..n]) {
                return linux_abi_error(14);
            }
            n as u64
        }
        Err(e) => (-(e as isize)) as u64,
    }
}

pub fn sys_chmod(_path: u64, _mode: u64) -> u64 { 0 }
pub fn sys_fchmod(_fd: u64, _mode: u64) -> u64 { 0 }
pub fn sys_chown(_path: u64, _owner: u64, _group: u64) -> u64 { 0 }
pub fn sys_fchown(_fd: u64, _owner: u64, _group: u64) -> u64 { 0 }
pub fn sys_lchown(_path: u64, _owner: u64, _group: u64) -> u64 { 0 }

pub fn sys_umask(mask: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let mut old_mask = 0;
    let _ = crate::process::modify_process(pid, |p| {
        let mut proc = p.proc.lock();
        old_mask = proc.umask;
        proc.umask = (mask & 0o777) as u32;
    });
    old_mask as u64
}

pub fn sys_getdents64(fd: u64, buf_ptr: u64, len: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        match crate::scheme::getdents(fd_entry.scheme_id, fd_entry.resource_id) {
            Ok(entries) => {
                let mut pos = 0;
                for name in entries {
                    let name_bytes = name.as_bytes();
                    let reclen = (8 + 8 + 2 + 1 + name_bytes.len() + 1 + 7) & !7;
                    if pos + reclen > len as usize { break; }
                    
                    let mut entry_buf = [0u8; 256];
                    unsafe {
                        core::ptr::write_unaligned(entry_buf.as_mut_ptr() as *mut u64, 1);
                        core::ptr::write_unaligned(entry_buf.as_mut_ptr().add(8) as *mut u64, 0);
                        core::ptr::write_unaligned(entry_buf.as_mut_ptr().add(16) as *mut u16, reclen as u16);
                        core::ptr::write_unaligned(entry_buf.as_mut_ptr().add(18) as *mut u8, 8);
                    }
                    entry_buf[19..19+name_bytes.len()].copy_from_slice(name_bytes);
                    entry_buf[19+name_bytes.len()] = 0;
                    
                    if !copy_to_user(buf_ptr + pos as u64, &entry_buf[..reclen]) {
                        return linux_abi_error(14);
                    }
                    pos += reclen;
                }
                pos as u64
            }
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_getcwd(buf_ptr: u64, len: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(p) = crate::process::get_process(pid) {
        let proc = p.proc.lock();
        let cw_len = proc.cwd_len;
        if len < (cw_len + 1) as u64 { return linux_abi_error(34); } // ERANGE
        if !copy_to_user(buf_ptr, &proc.cwd[..cw_len]) { return linux_abi_error(14); }
        if !copy_to_user(buf_ptr + cw_len as u64, &[0]) { return linux_abi_error(14); }
        buf_ptr
    } else {
        u64::MAX
    }
}

pub fn sys_chdir(path_ptr: u64) -> u64 {
    let len = strlen_user_unique(path_ptr, 127);
    if len == 0 { return linux_abi_error(2); }
    let mut path_buf = [0u8; 128];
    if !copy_from_user(path_ptr, &mut path_buf[..len]) { return linux_abi_error(14); }
    
    let pid = current_process_id().unwrap_or(0);
    let _ = crate::process::modify_process(pid, |p| {
        let mut proc = p.proc.lock();
        let actual_len = len.min(127);
        proc.cwd[..actual_len].copy_from_slice(&path_buf[..actual_len]);
        proc.cwd[actual_len] = 0;
        proc.cwd_len = actual_len;
    });
    0
}

pub fn sys_fchdir(_fd: u64) -> u64 {
    linux_abi_error(38)
}

pub fn sys_fcntl(fd: u64, cmd: u64, _arg: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        match cmd {
            0 => { // F_DUPFD
                crate::fd::fd_create(pid, fd_entry.scheme_id, fd_entry.resource_id).unwrap_or(0) as u64
            }
            1 => 0, // F_GETFD
            2 => 0, // F_SETFD
            3 => 0, // F_GETFL
            4 => 0, // F_SETFL
            _ => 0,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_flock(fd: u64, operation: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        match crate::scheme::flock(fd_entry.scheme_id, fd_entry.resource_id, operation as usize) {
            Ok(_) => 0,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_fsync(fd: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        match crate::scheme::fsync(fd_entry.scheme_id, fd_entry.resource_id) {
            Ok(_) => 0,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_fdatasync(fd: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        match crate::scheme::fdatasync(fd_entry.scheme_id, fd_entry.resource_id) {
            Ok(_) => 0,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_memfd_create(name_ptr: u64, _flags: u64) -> u64 {
    let mut name_buf = [0u8; 32];
    let mut name = "memfd";
    if name_ptr != 0 {
        if copy_from_user(name_ptr, &mut name_buf) {
             if let Ok(n) = core::str::from_utf8(&name_buf) {
                 name = n.trim_matches(char::from(0));
             }
        }
    }

    let pid = current_process_id().unwrap_or(0);
    let unique_name = format!("memfd:{}:{}", pid, name);
    let path = format!("shm:{}", unique_name);
    
    // O_CREAT (0x40) | O_RDWR (0x02)
    match crate::scheme::open(&path, 0x40 | 0x02, 0o666) {
        Ok((sid, rid)) => {
            // Unlink immediately so it's anonymous
            let _ = crate::scheme::unlink(&path);
            crate::fd::fd_create(pid, sid, rid).unwrap_or(usize::MAX) as u64
        }
        Err(e) => (-(e as isize)) as u64,
    }
}

pub fn sys_dup(fd: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        crate::fd::fd_create(pid, fd_entry.scheme_id, fd_entry.resource_id).unwrap_or(0) as u64
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_dup2(old_fd: u64, new_fd: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, old_fd as usize) {
        crate::fd::fd_replace(pid, new_fd as usize, fd_entry.scheme_id, fd_entry.resource_id, fd_entry.flags);
        new_fd
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_dup3(old_fd: u64, new_fd: u64, _flags: u64) -> u64 {
    sys_dup2(old_fd, new_fd)
}

// --- END fs ---
// --- BEGIN process ---
// Process-related syscalls implementation
//
// Implementation of process lifecycle, scheduling affinity, and threading.

use crate::process::{ProcessId, exit_process};
use crate::scheduler::yield_cpu;

pub fn sys_getpid() -> u64 {
    if let Some(pid) = current_process_id() {
        if let Some(p) = crate::process::get_process(pid) {
            return p.tgid as u64;
        }
    }
    0
}

pub fn sys_gettid() -> u64 {
    current_process_id().unwrap_or(0) as u64
}

pub fn sys_getppid() -> u64 {
    if let Some(pid) = current_process_id() {
        if let Some(process) = crate::process::get_process(pid) {
            return process.proc.lock().parent_pid.unwrap_or(0) as u64;
        }
    }
    0
}

pub fn sys_fork(context: &crate::interrupts::SyscallContext) -> u64 {
    // After fixing syscall_entry to populate context.rip/rflags from the IRET frame,
    // we can trust context.rip/rflags here for both `syscall` and `int 0x80` paths.
    let user_rip = context.rip;
    let user_rflags = context.rflags;
    crate::serial::serial_printf(format_args!(
        "[sys_fork] rip={:#x} rsp={:#x} rflags={:#x} rcx={:#x} r11={:#x}\n",
        context.rip, context.rsp, context.rflags, context.rcx, context.r11
    ));
    let ctx = crate::process::Context {
        rsp: context.rsp, rip: user_rip, rflags: user_rflags,
        rbp: context.rbp, rax: 0, rbx: context.rbx, rcx: context.rcx,
        rdx: context.rdx, rsi: context.rsi, rdi: context.rdi,
        r8: context.r8, r9: context.r9, r10: context.r10, r11: context.r11,
        r12: context.r12, r13: context.r13, r14: context.r14, r15: context.r15,
        fs_base: context.fs_base, gs_base: context.gs_base,
    };
    match crate::process::fork_process(&ctx) {
        Some(child_pid) => {
            crate::scheduler::enqueue_process(child_pid);
            child_pid as u64
        }
        None => linux_abi_error(11), // EAGAIN
    }
}

pub fn sys_exit(exit_code: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(process) = crate::process::get_process(pid) {
        process.proc.lock().exit_code = exit_code as i32;
    }
    exit_process();
    yield_cpu();
    0
}

pub fn sys_wait_pid(status_ptr: u64, wait_pid: u64, flags: u64) -> u64 {
    sys_wait_impl(status_ptr, wait_pid, flags)
}

pub fn sys_waitid(idtype: u64, id: u64, infop: u64, options: u64, _rusage: u64) -> u64 {
    let wait_pid = if idtype == 1 { id } else { 0 };
    let res = sys_wait_impl(0, wait_pid, options);
    if (res as i64) < 0 { return res; }
    if infop != 0 && is_user_pointer(infop, 128) {
        unsafe {
            let ptr = infop as *mut i32;
            ptr.write_unaligned(17); // SIGCHLD
            ptr.add(1).write_unaligned(0);
            ptr.add(2).write_unaligned(1); // CLD_EXITED
            ((infop + 16) as *mut i32).write_unaligned(res as i32);
        }
    }
    0
}

fn sys_wait_impl(status_ptr: u64, wait_pid: u64, flags: u64) -> u64 {
    let wnohang = (flags & 1) != 0;
    let wuntraced = (flags & 2) != 0;
    let current_pid = match current_process_id() { Some(pid) => pid, None => return u64::MAX };

    loop {
        let mut found_eligible_child = false;
        let mut terminated_child = None;

        {
            let mut table = crate::process::PROCESS_TABLE.lock();
            for slot in table.iter_mut() {
                if let Some(p) = slot {
                    let mut proc = p.proc.lock();
                    if proc.parent_pid == Some(current_pid) {
                        if wait_pid != 0 && p.id != wait_pid as u32 { continue; }
                        found_eligible_child = true;
                        if p.state == crate::process::ProcessState::Terminated {
                            terminated_child = Some((p.id, proc.exit_code));
                            break;
                        }
                        if wuntraced && p.state == crate::process::ProcessState::Stopped && !proc.notified_stopped {
                            terminated_child = Some((p.id, (0x7F | ((proc.exit_signal as u64) << 8)) as i32));
                            proc.notified_stopped = true;
                            break;
                        }
                    }
                }
            }
        }

        if let Some((child_pid, status)) = terminated_child {
            if status_ptr != 0 && is_user_pointer(status_ptr, 4) {
                let b = (status as u32).to_le_bytes();
                if !copy_to_user(status_ptr, &b) {
                    return linux_abi_error(14); // EFAULT
                }
            }
            if (status & 0x7F) == 0 { crate::process::remove_process(child_pid); }
            crate::process::unregister_child_waiter(current_pid);
            return child_pid as u64;
        }

        if !found_eligible_child { return linux_abi_error(10); } // ECHILD
        if wnohang { return 0; }

        if let Some(mut proc) = crate::process::get_process(current_pid) {
            proc.state = crate::process::ProcessState::WaitingForChild;
            crate::process::update_process(current_pid, proc);
            crate::process::register_child_waiter(current_pid);
        }
        yield_cpu();
    }
}

pub fn sys_exec(elf_ptr: u64, elf_size: u64) -> u64 {
    if elf_ptr == 0 || elf_size == 0 || elf_size > 128 * 1024 * 1024 { return u64::MAX; }
    if !is_user_pointer(elf_ptr, elf_size) { return u64::MAX; }
    
    let src = unsafe { core::slice::from_raw_parts(elf_ptr as *const u8, elf_size as usize) };
    let mut elf_data = Vec::with_capacity(elf_size as usize);
    elf_data.extend_from_slice(src);

    let current_pid = current_process_id().expect("exec without process");
    let _ = crate::process::vfork_detach_mm_for_exec_if_needed(current_pid);
    
    match crate::elf_loader::replace_process_image(current_pid, &elf_data) {
        Ok(res) => {
            if let Some(mut process) = crate::process::get_process(current_pid) {
                {
                    let proc = process.proc.lock();
                    let mut r = proc.resources.lock();
                    r.vmas.clear();
                    r.brk_current = res.max_vaddr;
                }
                
                {
                    let mut proc = process.proc.lock();
                    proc.mem_frames = (0x100000 / 4096) + res.segment_frames;
                    proc.dynamic_linker_aux = res.dynamic_linker;
                }
                
                process.fs_base = if res.dynamic_linker.is_some() { 0 } else { res.tls_base };
                crate::process::update_process(current_pid, process);
            }
            crate::process::clear_pending_process_args(current_pid);
            
            const STACK_BASE: u64 = 0x2000_0000;
            const STACK_SIZE: usize = 0x10_0000;
            let cr3 = crate::memory::get_cr3();
            let _ = crate::elf_loader::setup_user_stack(cr3, STACK_BASE, STACK_SIZE);
            crate::process::register_post_exec_vm_as(current_pid, &res, STACK_BASE, STACK_SIZE as u64);
            crate::fd::fd_ensure_stdio(current_pid);
            
            unsafe {
                let stack_top = STACK_BASE + STACK_SIZE as u64;
                let tls = if res.dynamic_linker.is_some() { 0 } else { res.tls_base };
                if res.dynamic_linker.is_some() {
                    crate::elf_loader::jump_to_userspace_dynamic_linker(
                        res.entry_point,
                        stack_top,
                        res.phdr_va,
                        res.phnum,
                        res.phentsize,
                        tls,
                    );
                } else {
                    crate::elf_loader::jump_to_userspace(
                        res.entry_point,
                        stack_top,
                        res.phdr_va,
                        res.phnum,
                        res.phentsize,
                        tls,
                    );
                }
            }
        }
        Err(_) => u64::MAX,
    }
}

pub fn sys_spawn(elf_ptr: u64, elf_size: u64, name_ptr: u64) -> u64 {
    if !is_user_pointer(elf_ptr, elf_size) { return u64::MAX; }
    let mut name_buf = [0u8; 16];
    if name_ptr != 0 {
        let len = strlen_user_unique(name_ptr, 15);
        let _ = copy_from_user(name_ptr, &mut name_buf[..len]);
    }
    let name = core::str::from_utf8(&name_buf).unwrap_or("unknown").trim_matches('\0');
    let elf_data = unsafe { core::slice::from_raw_parts(elf_ptr as *const u8, elf_size as usize) };
    
    match crate::process::spawn_process(elf_data, name) {
        Ok(pid) => {
            let parent = current_process_id().unwrap_or(1);
            if let Some(process) = crate::process::get_process(pid) {
                process.proc.lock().parent_pid = Some(parent);
            }
            pid as u64
        }
        Err(_) => u64::MAX,
    }
}

pub fn sys_spawn_service(service_id: u64, name_ptr: u64, name_len: u64) -> u64 {
    use eclipse_program_codes::spawn_service as svc;
    let path = match service_id {
        x if x == svc::LOG as u64 => svc::PATH_LOG,
        x if x == svc::DEVFS as u64 => svc::PATH_DEVFS,
        x if x == svc::FILESYSTEM as u64 => svc::PATH_FILESYSTEM,
        x if x == svc::INPUT as u64 => svc::PATH_INPUT,
        x if x == svc::DISPLAY as u64 => svc::PATH_DISPLAY,
        x if x == svc::AUDIO as u64 => svc::PATH_AUDIO,
        x if x == svc::NETWORK as u64 => svc::PATH_NETWORK,
        x if x == svc::GUI as u64 => svc::PATH_GUI,
        x if x == svc::SEATD as u64 => svc::PATH_SEATD,
        _ => return u64::MAX,
    };

    let elf_data = match crate::filesystem::read_file_alloc(path) {
        Ok(buf) => buf,
        Err(_) => return u64::MAX,
    };

    let mut name_buf = [0u8; 16];
    if name_ptr != 0 {
        let copy_len = (name_len as usize).min(15);
        let _ = copy_from_user(name_ptr, &mut name_buf[..copy_len]);
    }
    let name_str = core::str::from_utf8(&name_buf).unwrap_or("");
    let name = if name_str.trim_matches('\0').is_empty() {
        eclipse_program_codes::spawn_service_short_name(service_id as u32)
    } else {
        name_str.trim_matches('\0')
    };

    match crate::process::spawn_process(&elf_data, name) {
        Ok(pid) => {
            let parent = current_process_id().unwrap_or(1);
            if let Some(process) = crate::process::get_process(pid) {
                process.proc.lock().parent_pid = Some(parent);
            }
            crate::scheduler::enqueue_process(pid);
            pid as u64
        }
        Err(_) => u64::MAX,
    }
}

pub fn sys_spawn_with_stdio(elf_ptr: u64, elf_size: u64, name_ptr: u64, fd_in: u64, fd_out: u64, fd_err: u64) -> u64 {
    if !is_user_pointer(elf_ptr, elf_size) { return u64::MAX; }
    let mut elf_data = Vec::with_capacity(elf_size as usize);
    elf_data.extend_from_slice(unsafe { core::slice::from_raw_parts(elf_ptr as *const u8, elf_size as usize) });
    
    let mut name_buf = [0u8; 16];
    if name_ptr != 0 {
        let len = strlen_user_unique(name_ptr, 15);
        let _ = copy_from_user(name_ptr, &mut name_buf[..len]);
    }
    let name = core::str::from_utf8(&name_buf).unwrap_or("unknown").trim_matches('\0');

    match crate::process::spawn_process(&elf_data, name) {
        Ok(pid) => setup_stdio(pid, fd_in, fd_out, fd_err),
        Err(_) => u64::MAX,
    }
}

fn setup_stdio(pid: crate::process::ProcessId, fd_in: u64, fd_out: u64, fd_err: u64) -> u64 {
    let parent = current_process_id().unwrap_or(1);
    if let Some(process) = crate::process::get_process(pid) {
        process.proc.lock().parent_pid = Some(parent);
    }
    
    if let Some(child_idx) = crate::fd::pid_to_fd_idx(pid as u32) {
        let mut tables = crate::fd::FD_TABLES.lock();
        for (i, &fd) in [fd_in, fd_out, fd_err].iter().enumerate() {
            if let Some(p_fd) = crate::fd::fd_get(parent, fd as usize) {
                tables[child_idx].fds[i] = p_fd;
                if p_fd.in_use { let _ = crate::scheme::dup(p_fd.scheme_id, p_fd.resource_id); }
            }
        }
    }
    pid as u64
}

pub fn sys_spawn_with_stdio_args(pid: u64, args_ptr: u64, args_len: u64, _a: u64, _b: u64, _c: u64, _ctx: &mut crate::interrupts::SyscallContext) -> u64 {
    if !is_user_pointer(args_ptr, args_len) || args_len > 4096 { return u64::MAX; }
    let args = unsafe { core::slice::from_raw_parts(args_ptr as *const u8, args_len as usize) }.to_vec();
    crate::process::set_pending_process_args(pid as u32, args);
    crate::scheduler::enqueue_process(pid as u32);
    0
}

pub fn sys_get_process_args(buf_ptr: u64, buf_size: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, buf_size as usize) };
    crate::process::copy_pending_process_args(pid, buf) as u64
}

pub fn sys_spawn_with_stdio_path(path_ptr: u64, name_ptr: u64, fd_in: u64, fd_out: u64, fd_err: u64, _a: u64) -> u64 {
    let len = strlen_user_unique(path_ptr, 1023);
    if len == 0 { return u64::MAX; }
    let mut path_buf = [0u8; 1024];
    let _ = copy_from_user(path_ptr, &mut path_buf[..len]);
    let path = core::str::from_utf8(&path_buf[..len]).unwrap_or("");
    
    if let Some(pid) = crate::elf_loader::load_elf_path(path) {
        if name_ptr != 0 {
            let n_len = strlen_user_unique(name_ptr, 15);
            let mut n_buf = [0u8; 16];
            let _ = copy_from_user(name_ptr, &mut n_buf[..n_len]);
            if let Some(process) = crate::process::get_process(pid) {
                process.proc.lock().name[..16].copy_from_slice(&n_buf);
            }
        }
        return setup_stdio(pid, fd_in, fd_out, fd_err);
    }
    u64::MAX
}

pub fn sys_get_process_list(buf_ptr: u64, max_count: u64) -> u64 {
    use ProcessInfo;
    
    if buf_ptr == 0 || max_count == 0 { return 0; }
    if !is_user_pointer(buf_ptr, max_count * core::mem::size_of::<ProcessInfo>() as u64) {
        return u64::MAX;
    }

    let mut count = 0;
    let table = crate::process::PROCESS_TABLE.lock();
    for slot in table.iter() {
        if let Some(p) = slot {
            if count >= max_count as usize { break; }
            
            let mut name = [0u8; 32];
            let proc = p.proc.lock();
            name[..16].copy_from_slice(&proc.name);
            
            let info = ProcessInfo {
                pid: p.id,
                ppid: proc.parent_pid.unwrap_or(0),
                state: p.state as u32,
                cpu_usage: 0,
                mem_usage_kb: proc.mem_frames * 4,
                name,
                thread_count: 1,
                priority: 5,
            };
            
            unsafe {
                core::ptr::write_unaligned((buf_ptr as *mut ProcessInfo).add(count), info);
            }
            count += 1;
        }
    }
    count as u64
}

pub fn sys_set_process_name(name_ptr: u64, len: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let mut buf = [0u8; 16];
    let l = (len as usize).min(15);
    let _ = copy_from_user(name_ptr, &mut buf[..l]);
    if let Some(process) = crate::process::get_process(pid) {
        process.proc.lock().name = buf;
    }
    0
}

pub fn sys_sched_setaffinity(pid: u64, cpu_id: u64) -> u64 {
    let target = if pid == 0 { current_process_id().unwrap_or(0) } else { pid as u32 };
    let affinity = if cpu_id == u64::MAX { None } else { Some(cpu_id as u32) };
    let _ = crate::process::modify_process(target, |p| p.cpu_affinity = affinity);
    0
}

pub fn sys_sched_getaffinity(pid: u64, cpusetsize: u64, mask_ptr: u64) -> u64 {
    if cpusetsize < 8 || !is_user_pointer(mask_ptr, 8) { return linux_abi_error(22); }
    let mask: u64 = (1 << crate::cpu::get_active_cpu_count()) - 1;
    unsafe { *(mask_ptr as *mut u64) = mask; }
    0
}

pub fn sys_set_tid_address(tidptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let _ = crate::process::modify_process(pid, |p| p.clear_child_tid = tidptr);
    pid as u64
}

pub fn sys_clone(flags: u64, stack: u64, parent_tid_ptr: u64, child_tid_ptr: u64, tls: u64, ctx: &crate::interrupts::SyscallContext) -> u64 {
    crate::serial::serial_printf(format_args!(
        "[sys_clone] flags={:#x} ctx.rip={:#x} ctx.rsp={:#x} ctx.rcx={:#x} ctx.r11={:#x} stack={:#x}\n",
        flags, ctx.rip, ctx.rsp, ctx.rcx, ctx.r11, stack
    ));
    let mut child_ctx = crate::process::Context {
        rsp: if stack != 0 { stack } else { ctx.rsp },
        rip: ctx.rip,
        rflags: ctx.rflags,
        rbp: ctx.rbp,
        rax: 0,
        rbx: ctx.rbx,
        rcx: ctx.rcx,
        rdx: ctx.rdx,
        rsi: ctx.rsi,
        rdi: ctx.rdi,
        r8: ctx.r8,
        r9: ctx.r9,
        r10: ctx.r10,
        r11: ctx.r11,
        r12: ctx.r12,
        r13: ctx.r13,
        r14: ctx.r14,
        r15: ctx.r15,
        fs_base: if (flags & 0x80000) != 0 { tls } else { ctx.fs_base }, // CLONE_SETTLS
        gs_base: ctx.gs_base,
    };
    
    // Linux encodes exit signal in the low 8 bits of flags.
    // wlroots/musl usually pass SIGCHLD (17) or 0 for threads.
    let exit_signal = flags & 0xFF;
    let flags = flags & !0xFF;

    const CLONE_VM: u64 = 0x0000_0100;
    const CLONE_FS: u64 = 0x0000_0200;
    const CLONE_FILES: u64 = 0x0000_0400;
    const CLONE_SIGHAND: u64 = 0x0000_0800;
    const CLONE_VFORK: u64 = 0x0000_4000;
    const CLONE_THREAD: u64 = 0x0001_0000;
    const CLONE_SYSVSEM: u64 = 0x0004_0000;
    const CLONE_SETTLS: u64 = 0x0008_0000;
    const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
    const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
    const CLONE_DETACHED: u64 = 0x0040_0000;
    const CLONE_CHILD_SETTID: u64 = 0x0100_0000;
    const CLONE_IO: u64 = 0x8000_0000;

    // Allowed flags for thread-style clone (pthreads).
    const THREAD_STYLE_ALLOWED: u64 = CLONE_VM
        | CLONE_THREAD
        | CLONE_FS
        | CLONE_FILES
        | CLONE_SIGHAND
        | CLONE_VFORK
        | CLONE_SYSVSEM
        | CLONE_SETTLS
        | CLONE_PARENT_SETTID
        | CLONE_CHILD_CLEARTID
        | CLONE_DETACHED
        | CLONE_CHILD_SETTID
        | CLONE_IO;

    // Allowed flags for fork-style clone (no CLONE_THREAD): accept the same set minus CLONE_THREAD.
    const FORK_STYLE_ALLOWED: u64 = THREAD_STYLE_ALLOWED & !CLONE_THREAD;

    // Thread-style clone: share Proc/resources with parent.
    if (flags & CLONE_THREAD) != 0 {
        // Fail fast on unexpected ABI/flags combinations.
        if flags & !THREAD_STYLE_ALLOWED != 0 {
            crate::serial::serial_printf(format_args!(
                "[sys_clone] thread-style EINVAL: unsupported flags extra={:#x}\n",
                flags & !THREAD_STYLE_ALLOWED
            ));
            return linux_abi_error(22); // EINVAL
        }

        let current_pid = match current_process_id() {
            Some(p) => p,
            None => return u64::MAX,
        };
        let clear_child_tid = if (flags & CLONE_CHILD_CLEARTID) != 0 { child_tid_ptr } else { 0 };
        let set_child_tid = if (flags & CLONE_CHILD_SETTID) != 0 { child_tid_ptr } else { 0 };
        match crate::process::clone_thread_process(current_pid, &child_ctx, clear_child_tid, set_child_tid) {
            Some(child_tid) => {
                // Parent tid writeback: store child's TID in user memory.
                if (flags & CLONE_PARENT_SETTID) != 0 && parent_tid_ptr != 0 && is_user_pointer(parent_tid_ptr, 4) {
                    // Touch first to fault in the page if needed, then write.
                    unsafe {
                        let _ = core::ptr::read_volatile(parent_tid_ptr as *const u32);
                        core::ptr::write_volatile(parent_tid_ptr as *mut u32, child_tid);
                    }
                }
                crate::scheduler::enqueue_process(child_tid);
                return child_tid as u64;
            }
            None => return linux_abi_error(11),
        }
    }

    // Fork-style clone (no CLONE_THREAD): behave like fork(2) or vfork(2) depending on flags.
    //
    // Linux requires CLONE_VM with CLONE_VFORK.
    if (flags & CLONE_VFORK) != 0 && (flags & CLONE_VM) == 0 {
        crate::serial::serial_printf(format_args!(
            "[sys_clone] fork-style EINVAL: CLONE_VFORK without CLONE_VM exit_sig={}\n",
            exit_signal
        ));
        return linux_abi_error(22); // EINVAL
    }
    if flags & !FORK_STYLE_ALLOWED != 0 {
        crate::serial::serial_printf(format_args!(
            "[sys_clone] fork-style EINVAL: unsupported flags extra={:#x}\n",
            flags & !FORK_STYLE_ALLOWED
        ));
        return linux_abi_error(22); // EINVAL
    }

    let vfork_block_parent = (flags & CLONE_VFORK) != 0 && (flags & CLONE_VM) != 0;
    if vfork_block_parent {
        crate::serial::serial_printf(format_args!(
            "[sys_clone] vfork-style: parent will block until child exec/exit (exit_sig={})\n",
            exit_signal
        ));
    }

    let child_pid_opt = if vfork_block_parent {
        crate::process::vfork_process_shared_vm(&child_ctx)
    } else {
        crate::process::fork_process(&child_ctx)
    };

    match child_pid_opt {
        Some(child_pid) => {
            if (flags & CLONE_PARENT_SETTID) != 0 && parent_tid_ptr != 0 && is_user_pointer(parent_tid_ptr, 4) {
                unsafe {
                    let _ = core::ptr::read_volatile(parent_tid_ptr as *const u32);
                    core::ptr::write_volatile(parent_tid_ptr as *mut u32, child_pid);
                }
            }
            if (flags & CLONE_CHILD_CLEARTID) != 0 {
                let _ = crate::process::modify_process(child_pid, |p| p.clear_child_tid = child_tid_ptr);
            }
            if (flags & CLONE_CHILD_SETTID) != 0 {
                let _ = crate::process::modify_process(child_pid, |p| p.set_child_tid = child_tid_ptr);
            }
            crate::scheduler::enqueue_process(child_pid);

            if vfork_block_parent {
                // Mark the parent as waiting for this child and spin-yield until released.
                // The child releases the parent when it successfully execs or exits.
                let Some(ppid) = current_process_id() else {
                    return child_pid as u64;
                };
                let _ = crate::process::modify_process(ppid, |p| {
                    p.proc.lock().vfork_waiting_for_child = Some(child_pid);
                });
                loop {
                    let released = crate::process::get_process(ppid)
                        .map(|p| p.proc.lock().vfork_waiting_for_child != Some(child_pid))
                        .unwrap_or(true);
                    if released {
                        break;
                    }
                    crate::scheduler::yield_cpu();
                }
            }

            child_pid as u64
        }
        None => linux_abi_error(11),
    }
}

pub fn sys_execve(path_ptr: u64, argv_ptr: u64, envp_ptr: u64) -> u64 {
    // 1) Leer path (NUL-terminado).
    const MAX_PATH: usize = 1024;
    let len = strlen_user_unique(path_ptr, MAX_PATH - 1);
    let len_u64 = len as u64;
    if len == 0 || !is_user_pointer(path_ptr, len_u64 + 1) {
        return linux_abi_error(14); // EFAULT
    }
    let mut path_buf = [0u8; MAX_PATH];
    if !copy_from_user(path_ptr, &mut path_buf[..len]) {
        return linux_abi_error(14);
    }
    let path = core::str::from_utf8(&path_buf[..len]).unwrap_or("");

    // Acepta "scheme:path" o rutas POSIX "/...".
    let open_path_storage;
    let open_path: &str = if path.contains(':') {
        path
    } else if path.starts_with('/') {
        open_path_storage = user_path_to_scheme_path(path);
        open_path_storage.as_str()
    } else {
        open_path_storage = alloc::format!("file:{}", path);
        open_path_storage.as_str()
    };

    crate::serial::serial_printf(format_args!(
        "[EXECVE] enter path='{}' open_path='{}' argv_ptr={:#x} envp_ptr={:#x}\n",
        path, open_path, argv_ptr, envp_ptr
    ));

    // 2) Leer argv/envp (punteros a C-strings).
    // Límite anti-OOM (musl/wlroots pueden pasar buffers muy grandes en algunos paths).
    const MAX_EXECVE_ARG_ENV_BYTES: usize = 4 * 1024 * 1024;
    let mut total_bytes: usize = 0;

    let mut argv_strings: alloc::vec::Vec<alloc::vec::Vec<u8>> = alloc::vec::Vec::new();
    if argv_ptr != 0 {
        let mut off = argv_ptr;
        for _ in 0..256usize {
            if !is_user_pointer(off, 8) { break; }
            let mut raw = [0u8; 8];
            if !copy_from_user(off, &mut raw) { break; }
            let arg_ptr = u64::from_le_bytes(raw);
            if arg_ptr == 0 { break; }

            let alen = strlen_user_unique(arg_ptr, 4096);
            let alen_u64 = alen as u64;
            let mut s = if alen == 0 || !is_user_pointer(arg_ptr, alen_u64 + 1) {
                alloc::vec![0u8]
            } else {
                let mut tmp = alloc::vec![0u8; alen as usize];
                if !copy_from_user(arg_ptr, &mut tmp) { tmp.clear(); }
                tmp.push(0);
                tmp
            };
            total_bytes = total_bytes.saturating_add(s.len());
            if total_bytes > MAX_EXECVE_ARG_ENV_BYTES {
                return linux_abi_error(7); // E2BIG
            }
            argv_strings.push(core::mem::take(&mut s));
            off = off.wrapping_add(8);
        }
    }
    if argv_strings.is_empty() {
        // argv[0] = basename del ejecutable.
        let base = path.rsplit('/').next().unwrap_or(path);
        let mut s = base.as_bytes().to_vec();
        s.push(0);
        total_bytes = total_bytes.saturating_add(s.len());
        if total_bytes > MAX_EXECVE_ARG_ENV_BYTES {
            return linux_abi_error(7);
        }
        argv_strings.push(s);
    }

    let mut envp_strings: alloc::vec::Vec<alloc::vec::Vec<u8>> = alloc::vec::Vec::new();
    if envp_ptr != 0 {
        let mut off = envp_ptr;
        for _ in 0..1024usize {
            if !is_user_pointer(off, 8) { break; }
            let mut raw = [0u8; 8];
            if !copy_from_user(off, &mut raw) { break; }
            let env_ptr = u64::from_le_bytes(raw);
            if env_ptr == 0 { break; }

            let elen = strlen_user_unique(env_ptr, 65536);
            let elen_u64 = elen as u64;
            if elen == 0 || !is_user_pointer(env_ptr, elen_u64 + 1) {
                off = off.wrapping_add(8);
                continue;
            }
            let mut tmp = alloc::vec![0u8; elen as usize];
            if !copy_from_user(env_ptr, &mut tmp) {
                off = off.wrapping_add(8);
                continue;
            }
            tmp.push(0);
            total_bytes = total_bytes.saturating_add(tmp.len());
            if total_bytes > MAX_EXECVE_ARG_ENV_BYTES {
                return linux_abi_error(7); // E2BIG
            }
            envp_strings.push(tmp);
            off = off.wrapping_add(8);
        }
    }
    if envp_strings.is_empty() {
        // Env mínimo para musl/bash.
        let minimal_bytes: usize = crate::elf_loader::MINIMAL_ENVP.iter().map(|e| e.len()).sum();
        if total_bytes.saturating_add(minimal_bytes) > MAX_EXECVE_ARG_ENV_BYTES {
            return linux_abi_error(7);
        }
        for e in crate::elf_loader::MINIMAL_ENVP {
            envp_strings.push(e.to_vec());
        }
    }

    // 3) Reemplazar imagen SIN copiar todo el ELF al heap del kernel.
    // Usamos el loader por inode para evitar OOM con binarios grandes (p.ej. Mesa/labwc).
    let current_pid = current_process_id().expect("exec without process");
    let _ = crate::process::vfork_detach_mm_for_exec_if_needed(current_pid);
    let fs_path_storage;
    let fs_path: &str = if open_path.starts_with("file:") {
        // Normal: file:/usr/bin/...
        fs_path_storage = alloc::string::String::from(&open_path[5..]);
        fs_path_storage.as_str()
    } else {
        // Best-effort: if the caller passed a raw POSIX path, feed it directly.
        // Non-file schemes are not supported by replace_process_image_path yet.
        path
    };
    crate::serial::serial_printf(format_args!(
        "[EXECVE] pid={} loading via replace_process_image_path('{}') argc={} envc={}\n",
        current_pid, fs_path, argv_strings.len(), envp_strings.len()
    ));
    let res = match crate::elf_loader::replace_process_image_path(current_pid, fs_path) {
        Ok(r) => r,
        Err(_) => {
            crate::serial::serial_printf(format_args!(
                "[EXECVE] replace_process_image_path failed path='{}' open_path='{}'\n",
                path, open_path
            ));
            return linux_abi_error(8); // ENOEXEC
        }
    };

    // Actualizar metadatos del proceso.
    if let Some(mut process) = crate::process::get_process(current_pid) {
        {
            let proc = process.proc.lock();
            let mut r = proc.resources.lock();
            r.vmas.clear();
            r.brk_current = res.max_vaddr;
        }
        {
            let mut proc = process.proc.lock();
            proc.mem_frames = (0x100000 / 4096) + res.segment_frames;
            proc.dynamic_linker_aux = res.dynamic_linker;
            // Diagnóstico: activar strace para labwc (y heredar en threads).
            if fs_path.ends_with("/labwc") || fs_path.contains("/labwc") {
                proc.syscall_trace = true;
            }
        }
        process.fs_base = if res.dynamic_linker.is_some() { 0 } else { res.tls_base };
        crate::process::update_process(current_pid, process);
    }
    crate::process::clear_pending_process_args(current_pid);

    const STACK_BASE: u64 = 0x2000_0000;
    const STACK_SIZE: usize = 0x10_0000;
    let cr3 = crate::memory::get_cr3();
    if crate::elf_loader::setup_user_stack(cr3, STACK_BASE, STACK_SIZE).is_err() {
        return linux_abi_error(12); // ENOMEM
    }
    crate::process::register_post_exec_vm_as(current_pid, &res, STACK_BASE, STACK_SIZE as u64);
    crate::fd::fd_ensure_stdio(current_pid);

    // vfork: liberar al padre cuando el exec está listo para saltar.
    crate::process::vfork_wake_parent_waiting_for_child(current_pid);

    let stack_top = STACK_BASE + STACK_SIZE as u64;
    let tls_base = if res.dynamic_linker.is_some() { 0 } else { res.tls_base };
    crate::serial::serial_printf(format_args!(
        "[EXECVE] jumping entry={:#x} stack_top={:#x} argc={} envc={} tls={:#x}\n",
        res.entry_point, stack_top, argv_strings.len(), envp_strings.len(), tls_base
    ));
    unsafe {
        crate::elf_loader::jump_to_userspace_with_argv_envp(
            &res,
            stack_top,
            &argv_strings,
            &envp_strings,
            tls_base,
        );
    }
}

pub fn sys_wait4_linux(pid: u64, status: u64, options: u64, _rusage: u64) -> u64 {
    sys_wait_impl(status, pid, options)
}

pub fn sys_ptrace(request: u64, pid: u64, addr: u64, data: u64) -> u64 {
    linux_abi_error(38)
}

pub fn sys_prctl(option: u64, arg2: u64, _a: u64, _b: u64, _c: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    match option {
        15 => { // PR_SET_NAME
            let len = strlen_user_unique(arg2, 15);
            let mut name = [0u8; 16];
            if copy_from_user(arg2, &mut name[..len]) {
                if let Some(process) = crate::process::get_process(pid) {
                    process.proc.lock().name = name;
                }
            }
            0
        }
        _ => 0,
    }
}

pub fn sys_arch_prctl(code: u64, addr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    match code {
        0x1002 => { // SET_FS
            if let Some(mut process) = crate::process::get_process(pid) {
                process.fs_base = addr;
                crate::process::update_process(pid, process);
            }
            set_fs_base(addr);
            0
        }
        _ => linux_abi_error(22),
    }
}

pub fn sys_sched_set_deadline(pid: u32, runtime: u64, deadline: u64, period: u64) -> u64 {
    let target = if pid == 0 { current_process_id().unwrap_or(0) } else { pid };
    let _ = crate::process::modify_process(target, |p| {
        p.rt_params = Some(crate::process::RTParams {
            runtime,
            deadline,
            period,
            next_deadline: crate::interrupts::ticks() + deadline,
        });
    });
    0
}

pub fn sys_getuid() -> u64 {
    let pid = current_process_id().unwrap_or(0);
    crate::process::get_uid(pid) as u64
}

pub fn sys_getgid() -> u64 {
    let pid = current_process_id().unwrap_or(0);
    crate::process::get_gid(pid) as u64
}

pub fn sys_setuid(uid: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(process) = crate::process::get_process(pid) {
        let mut proc = process.proc.lock();
        proc.uid = uid as u32;
        proc.euid = uid as u32;
        proc.suid = uid as u32;
    }
    0
}

pub fn sys_setgid(gid: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(process) = crate::process::get_process(pid) {
        let mut proc = process.proc.lock();
        proc.gid = gid as u32;
        proc.egid = gid as u32;
        proc.sgid = gid as u32;
    }
    0
}

pub fn sys_geteuid() -> u64 {
    let pid = current_process_id().unwrap_or(0);
    crate::process::get_euid(pid) as u64
}

pub fn sys_getegid() -> u64 {
    let pid = current_process_id().unwrap_or(0);
    crate::process::get_egid(pid) as u64
}

pub fn sys_setpgid(pid: u64, pgid: u64) -> u64 {
    let target = if pid == 0 { current_process_id().unwrap_or(0) } else { pid as u32 };
    let new_pgid = if pgid == 0 { target } else { pgid as u32 };
    if let Some(process) = crate::process::get_process(target) {
        process.proc.lock().pgid = new_pgid;
    }
    0
}

pub fn sys_getpgrp() -> u64 {
    let pid = current_process_id().unwrap_or(0);
    crate::process::get_pgid(pid) as u64
}

pub fn sys_setsid() -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(process) = crate::process::get_process(pid) {
        let mut proc = process.proc.lock();
        proc.sid = pid;
        proc.pgid = pid;
    }
    pid as u64
}

pub fn sys_setreuid(ruid: u64, euid: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(process) = crate::process::get_process(pid) {
        let mut proc = process.proc.lock();
        if ruid != u64::MAX { proc.uid = ruid as u32; }
        if euid != u64::MAX { proc.euid = euid as u32; }
    }
    0
}

pub fn sys_setregid(rgid: u64, egid: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(process) = crate::process::get_process(pid) {
        let mut proc = process.proc.lock();
        if rgid != u64::MAX { proc.gid = rgid as u32; }
        if egid != u64::MAX { proc.egid = egid as u32; }
    }
    0
}

pub fn sys_setresuid(ruid: u64, euid: u64, suid: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(process) = crate::process::get_process(pid) {
        let mut proc = process.proc.lock();
        if ruid != u64::MAX { proc.uid = ruid as u32; }
        if euid != u64::MAX { proc.euid = euid as u32; }
        if suid != u64::MAX { proc.suid = suid as u32; }
    }
    0
}

pub fn sys_setresgid(rgid: u64, egid: u64, sgid: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(process) = crate::process::get_process(pid) {
        let mut proc = process.proc.lock();
        if rgid != u64::MAX { proc.gid = rgid as u32; }
        if egid != u64::MAX { proc.egid = egid as u32; }
        if sgid != u64::MAX { proc.sgid = sgid as u32; }
    }
    0
}

pub fn sys_getresuid(ruid_ptr: u64, euid_ptr: u64, suid_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(process) = crate::process::get_process(pid) {
        let proc = process.proc.lock();
        if ruid_ptr != 0 && is_user_pointer(ruid_ptr, 4) {
            let b = proc.uid.to_le_bytes();
            if !copy_to_user(ruid_ptr, &b) { return linux_abi_error(14); }
        }
        if euid_ptr != 0 && is_user_pointer(euid_ptr, 4) {
            let b = proc.euid.to_le_bytes();
            if !copy_to_user(euid_ptr, &b) { return linux_abi_error(14); }
        }
        if suid_ptr != 0 && is_user_pointer(suid_ptr, 4) {
            let b = proc.suid.to_le_bytes();
            if !copy_to_user(suid_ptr, &b) { return linux_abi_error(14); }
        }
        0
    } else { linux_abi_error(3) }
}

pub fn sys_getresgid(rgid_ptr: u64, egid_ptr: u64, sgid_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(process) = crate::process::get_process(pid) {
        let proc = process.proc.lock();
        if rgid_ptr != 0 && is_user_pointer(rgid_ptr, 4) {
            let b = proc.gid.to_le_bytes();
            if !copy_to_user(rgid_ptr, &b) { return linux_abi_error(14); }
        }
        if egid_ptr != 0 && is_user_pointer(egid_ptr, 4) {
            let b = proc.egid.to_le_bytes();
            if !copy_to_user(egid_ptr, &b) { return linux_abi_error(14); }
        }
        if sgid_ptr != 0 && is_user_pointer(sgid_ptr, 4) {
            let b = proc.sgid.to_le_bytes();
            if !copy_to_user(sgid_ptr, &b) { return linux_abi_error(14); }
        }
        0
    } else { linux_abi_error(3) }
}

pub fn sys_getpgid(pid: u64) -> u64 {
    let target = if pid == 0 { current_process_id().unwrap_or(0) } else { pid as u32 };
    crate::process::get_pgid(target) as u64
}

pub fn sys_get_last_exec_error(_buf: u64, _len: u64) -> u64 { 0 }

pub fn sys_thread_create(entry: u64, stack: u64, arg: u64, ctx: &crate::interrupts::SyscallContext) -> u64 {
    // Thread creation logic (simplified)
    sys_fork(ctx)
}

pub fn sys_strace(pid: u64, enable: u64) -> u64 {
    let target = if pid == 0 { current_process_id().unwrap_or(0) } else { pid as u32 };
    if let Some(process) = crate::process::get_process(target) {
        process.proc.lock().syscall_trace = enable != 0;
    }
    0
}
// --- END process ---
// --- BEGIN memory ---
// Memory-related syscalls implementation
//
// Implementation of mmap, munmap, mprotect and brk with robust VMA management.

use crate::process::{self};

pub mod linux_mmap_abi {
    pub const PROT_READ: u64 = 1;
    pub const PROT_WRITE: u64 = 2;
    pub const PROT_EXEC: u64 = 4;
    pub const PROT_MASK: u64 = 7;
    pub const MAP_FIXED: u64 = 0x10;
    pub const MAP_SHARED: u64 = 0x01;
    pub const MAP_PRIVATE: u64 = 0x02;
    pub const MAP_ANONYMOUS: u64 = 0x20;
    pub const MAP_POPULATE: u64 = 0x08000;
    pub const MAP_HUGETLB: u64 = 0x40000;
    pub const MAP_HUGE_2MB: u64 = 21 << 26;
    pub const USER_ARENA_LO: u64 = 0x6000_0000;
    pub const USER_ARENA_HI: u64 = 0x0000_7000_0000_0000;
    pub const USER_EXEC_STACK_LO: u64 = 0x2000_0000;
    pub const USER_EXEC_STACK_HI: u64 = USER_EXEC_STACK_LO + 0x10_0000;
    pub const ANON_SLACK_BYTES: u64 = 0x8000;
}

pub fn vma_find_gap(vmas: &[crate::process::VMARegion], len: u64) -> Option<u64> {
    let mut v = linux_mmap_abi::USER_ARENA_LO;
    loop {
        if v + len > linux_mmap_abi::USER_ARENA_HI { return None; }
        let mut overlap = false;
        for vma in vmas.iter() {
            if v < vma.end && v + len > vma.start {
                v = vma.end;
                overlap = true;
                break;
            }
        }
        if !overlap { return Some(v); }
    }
}

pub fn vma_remove_range(vmas: &mut Vec<crate::process::VMARegion>, lo: u64, hi: u64) {
    if hi <= lo { return; }
    let old = core::mem::take(vmas);
    for vma in old {
        if vma.end <= lo || vma.start >= hi {
            vmas.push(vma);
        } else {
            if vma.start < lo {
                let mut vma_lo = vma.clone();
                vma_lo.end = lo;
                vmas.push(vma_lo);
            }
            if vma.end > hi {
                let mut vma_hi = vma.clone();
                vma_hi.offset += hi - vma.start;
                vma_hi.start = hi;
                vmas.push(vma_hi);
            }
        }
    }
    vma_merge_adjacent(vmas);
}

pub fn vma_mprotect_range(vmas: &mut Vec<crate::process::VMARegion>, lo: u64, hi: u64, prot: u64) {
    if hi <= lo { return; }
    let old = core::mem::take(vmas);
    for vma in old {
        if vma.end <= lo || vma.start >= hi {
            vmas.push(vma);
        } else {
            if vma.start < lo {
                let mut vma_lo = vma.clone();
                vma_lo.end = lo;
                vmas.push(vma_lo);
            }

            let mid_start = vma.start.max(lo);
            let mid_end = vma.end.min(hi);
            let mut new_vma = vma.clone();
            new_vma.offset += mid_start - vma.start;
            new_vma.start = mid_start;
            new_vma.end = mid_end;
            new_vma.flags = prot;
            vmas.push(new_vma);

            if vma.end > hi {
                let mut vma_hi = vma.clone();
                vma_hi.offset += hi - vma.start;
                vma_hi.start = hi;
                vmas.push(vma_hi);
            }
        }
    }
    vma_merge_adjacent(vmas);
}

pub fn vma_merge_adjacent(vmas: &mut Vec<crate::process::VMARegion>) {
    if vmas.len() < 2 { return; }
    vmas.sort_by_key(|v| v.start);
    let old = core::mem::take(vmas);
    let mut iter = old.into_iter();
    if let Some(mut current) = iter.next() {
        for next in iter {
            if current.can_merge(&next) {
                current.end = next.end;
            } else {
                vmas.push(current);
                current = next;
            }
        }
        vmas.push(current);
    }
}

pub fn mprotect_expand_anon_slack(vmas: &[crate::process::VMARegion], mut lo: u64, mut hi: u64, prot: u64) -> (u64, u64) {
    if (prot & linux_mmap_abi::PROT_EXEC) == 0 || hi <= lo {
        return (lo, hi);
    }
    let mut changed = true;
    while changed {
        changed = false;
    for vma in vmas.iter() {
        let is_anon = matches!(vma.object.lock().obj_type, crate::vm_object::VMObjectType::Anonymous);
        if !is_anon { continue; }
        if lo < vma.end && hi > vma.start {
            let na = lo.min(vma.start);
            let ne = hi.max(vma.end);
            if na != lo || ne != hi {
                lo = na;
                hi = ne;
                changed = true;
            }
        }
    }
    }
    (lo, hi)
}

pub fn mmap_pte_linux_prot(base_prot: u64, anon_slack: u64, map_end: u64, page_vaddr: u64) -> u64 {
    if anon_slack == 0 { return base_prot; }
    let slack_lo = map_end.saturating_sub(anon_slack);
    if page_vaddr >= slack_lo {
        base_prot | linux_mmap_abi::PROT_EXEC
    } else {
        base_prot
    }
}

pub fn sys_mmap(addr: u64, len: u64, prot: u64, flags: u64, fd: u64, offset: u64) -> u64 {
    if len == 0 { return linux_abi_error(22); } // EINVAL
    let pid = current_process_id().unwrap_or(0);
    
    let is_anon = (flags & linux_mmap_abi::MAP_ANONYMOUS) != 0;
    let is_fixed = (flags & linux_mmap_abi::MAP_FIXED) != 0;
    
    if is_anon {
        let aligned_len = (len + 0xFFF) & !0xFFF;
        let slack = if (prot & linux_mmap_abi::PROT_EXEC) != 0 { linux_mmap_abi::ANON_SLACK_BYTES } else { 0 };
        let total_len = aligned_len + slack;

        let target_vaddr = if is_fixed {
            if addr == 0 || (addr & 0xFFF) != 0 { return linux_abi_error(22); }
            addr
        } else {
            if let Some(proc) = crate::process::get_process(pid) {
                let p_proc = proc.proc.lock();
                let r = p_proc.resources.lock();
                if let Some(v) = vma_find_gap(&r.vmas, total_len) {
                    v
                } else { return linux_abi_error(12); } // ENOMEM
            } else { return linux_abi_error(3); }
        };

        if let Some(mut proc) = crate::process::get_process(pid) {
            {
                let mut p_proc = proc.proc.lock();
                let mut r = p_proc.resources.lock();
                if is_fixed { vma_remove_range(&mut r.vmas, target_vaddr, target_vaddr + total_len); }
                
                let obj = crate::vm_object::VMObject::new_anonymous(total_len);
                r.vmas.push(crate::process::VMARegion {
                    start: target_vaddr,
                    end: target_vaddr + total_len,
                    flags: prot,
                    object: obj,
                    offset: 0,
                    is_huge: false,
                    is_shared: (flags & linux_mmap_abi::MAP_SHARED) != 0,
                });
                vma_merge_adjacent(&mut r.vmas);
                drop(r);
                p_proc.mem_frames += (total_len + 4095) / 4096;
            }
            crate::process::update_process(pid, proc);
            return target_vaddr;
        }
    } else {
        // File backed mmap (simplified for now)
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            match crate::scheme::fmap(fd_entry.scheme_id, fd_entry.resource_id, offset as usize, len as usize) {
                Ok(phys) => {
                    // Handle WC flag (bit 63)
                    let wc_requested = (phys as u64) & (1 << 63) != 0;
                    let phys_addr_raw = (phys as u64) & !(1 << 63);

                    let phys_addr = if phys_addr_raw >= crate::memory::PHYS_MEM_OFFSET {
                        phys_addr_raw - crate::memory::PHYS_MEM_OFFSET
                    } else { phys_addr_raw };
                    
                    let aligned_len = (len + 0xFFF) & !0xFFF;
                    let target_vaddr = if is_fixed {
                        if addr == 0 || (addr & 0xFFF) != 0 { return linux_abi_error(22); }
                        addr
                    } else {
                        if let Some(proc) = crate::process::get_process(pid) {
                            let p_proc = proc.proc.lock();
                            let r = p_proc.resources.lock();
                            if let Some(v) = vma_find_gap(&r.vmas, aligned_len) {
                                v
                            } else { return linux_abi_error(12); }
                        } else { return linux_abi_error(3); }
                    };

                    if let Some(mut proc) = crate::process::get_process(pid) {
                        {
                            let p_proc = proc.proc.lock();
                            let mut r = p_proc.resources.lock();
                            if is_fixed { vma_remove_range(&mut r.vmas, target_vaddr, target_vaddr + aligned_len); }
                            
                            // Map flags: Present=1, Writable=2, User=4 -> 7
                            let mut pt_flags = (prot | 7) & 7;
                            if wc_requested {
                                pt_flags |= 0x08; // PWT (Write-Through) -> Maps to PAT Index 1 (WC)
                            }
                            
                            crate::memory::map_user_range(r.page_table_phys, target_vaddr, phys_addr, aligned_len, pt_flags);
                            let obj = crate::vm_object::VMObject::new_physical(phys_addr, aligned_len);
                            r.vmas.push(crate::process::VMARegion {
                                start: target_vaddr,
                                end: target_vaddr + aligned_len,
                                flags: prot,
                                object: obj,
                                offset: 0,
                                is_huge: false,
                                is_shared: (flags & linux_mmap_abi::MAP_SHARED) != 0,
                            });
                            vma_merge_adjacent(&mut r.vmas);
                        }
                        crate::process::update_process(pid, proc);
                        return target_vaddr;
                    }
                }
                Err(e) => {
                    // Fallback: algunos esquemas (p.ej. `file:`) no implementan fmap todavía.
                    // Para soportar binarios dinámicos (musl mapea .so), hacemos un mmap "software":
                    // reservamos un VMA anónimo y copiamos el contenido del fichero a frames mapeados.
                    if e == crate::scheme::error::ENOSYS {
                        crate::serial::serial_printf(format_args!(
                            "[sys_mmap] fmap ENOSYS -> fallback copy fd={} off={} len={} prot={:#x} flags={:#x}\n",
                            fd, offset, len, prot, flags
                        ));
                        let aligned_len = (len + 0xFFF) & !0xFFF;
                        let target_vaddr = if is_fixed {
                            if addr == 0 || (addr & 0xFFF) != 0 { return linux_abi_error(22); }
                            addr
                        } else {
                            if let Some(proc) = crate::process::get_process(pid) {
                                let p_proc = proc.proc.lock();
                                let r = p_proc.resources.lock();
                                if let Some(v) = vma_find_gap(&r.vmas, aligned_len) {
                                    v
                                } else { return linux_abi_error(12); }
                            } else { return linux_abi_error(3); }
                        };

                        if let Some(mut proc) = crate::process::get_process(pid) {
                            let obj = crate::vm_object::VMObject::new_anonymous(aligned_len);
                            {
                                let p_proc = proc.proc.lock();
                                let mut r = p_proc.resources.lock();
                                if is_fixed { vma_remove_range(&mut r.vmas, target_vaddr, target_vaddr + aligned_len); }

                                // Registrar el VMA (para futuras comprobaciones de permisos).
                                r.vmas.push(crate::process::VMARegion {
                                    start: target_vaddr,
                                    end: target_vaddr + aligned_len,
                                    flags: prot,
                                    object: obj.clone(),
                                    offset: 0,
                                    is_huge: false,
                                    is_shared: (flags & linux_mmap_abi::MAP_SHARED) != 0,
                                });
                                vma_merge_adjacent(&mut r.vmas);

                                // Mapear y copiar páginas ahora (eager), porque el page fault handler
                                // para VMObjectType::File todavía no está implementado.
                                let leaf = crate::memory::linux_prot_to_leaf_pte_bits(prot);
                                let mut tmp = [0u8; 4096];
                                let mut off_in_file = offset;
                                let mut page_off = 0u64;
                                while page_off < aligned_len {
                                    let Some(phys) = crate::memory::alloc_phys_frame_for_anon_mmap() else {
                                        return linux_abi_error(12); // ENOMEM
                                    };
                                    let fv = crate::memory::PHYS_MEM_OFFSET + phys;
                                    unsafe { core::ptr::write_bytes(fv as *mut u8, 0, 4096); }

                                    // Leer hasta 4KB desde el esquema al buffer temporal.
                                    let want = core::cmp::min(4096u64, len.saturating_sub(page_off)) as usize;
                                    if want > 0 {
                                        // Limpiar el tmp para no filtrar datos viejos.
                                        tmp.fill(0);
                                        match crate::scheme::read(fd_entry.scheme_id, fd_entry.resource_id, &mut tmp[..want], off_in_file) {
                                            Ok(n) => {
                                                unsafe {
                                                    core::ptr::copy_nonoverlapping(tmp.as_ptr(), fv as *mut u8, n);
                                                }
                                            }
                                            Err(er) => {
                                                crate::serial::serial_printf(format_args!(
                                                    "[sys_mmap] fallback read failed: scheme_err={} fd={} off_in_file={} want={}\n",
                                                    er, fd, off_in_file, want
                                                ));
                                                return linux_abi_error(5); // EIO
                                            }
                                        }
                                        off_in_file = off_in_file.saturating_add(want as u64);
                                    }

                                    // Registrar la página en el objeto y mapearla en userspace.
                                    {
                                        let mut o = obj.lock();
                                        let idx = page_off / 4096;
                                        o.pages.insert(idx, phys);
                                    }
                                    crate::memory::map_user_page_4kb(r.page_table_phys, target_vaddr + page_off, phys, leaf);
                                    page_off += 4096;
                                }
                            }
                            crate::process::update_process(pid, proc);
                            return target_vaddr;
                        }
                        return linux_abi_error(3);
                    }
                    return (-(e as isize)) as u64;
                }
            }
        }
    }
    linux_abi_error(9) // EBADF
}


pub fn sys_munmap(addr: u64, len: u64) -> u64 {
    if addr == 0 || (addr & 0xFFF) != 0 || len == 0 { return linux_abi_error(22); }
    let pid = current_process_id().unwrap_or(0);
    if let Some(proc) = crate::process::get_process(pid) {
        let p_proc = proc.proc.lock();
        let mut r = p_proc.resources.lock();
        vma_remove_range(&mut r.vmas, addr, addr + len);
        // Unmap the pages from the page table and free the backing physical frames.
        crate::memory::unmap_user_range(r.page_table_phys, addr, len);
        return 0;
    }
    linux_abi_error(3)
}

pub fn sys_mprotect(addr: u64, len: u64, prot: u64) -> u64 {
    if addr == 0 || (addr & 0xFFF) != 0 || len == 0 { return linux_abi_error(22); }
    let pid = current_process_id().unwrap_or(0);
    if let Some(proc) = crate::process::get_process(pid) {
        let p_proc = proc.proc.lock();
        let mut r = p_proc.resources.lock();
        let (lo, hi) = mprotect_expand_anon_slack(&r.vmas, addr, addr + len, prot);
        vma_mprotect_range(&mut r.vmas, lo, hi, prot);
        // Aplicar también a las PTE ya presentes. Sin esto, el loader dinámico (musl)
        // puede mapear segmentos como RO y luego usar mprotect() para hacerlos RW
        // durante relocations; si las PTE no cambian, obtenemos #PF de protección.
        let _ = crate::memory::mprotect_user_range(r.page_table_phys, lo, hi - lo, prot);
        return 0;
    }
    linux_abi_error(3)
}

pub fn sys_brk(new_brk: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(mut proc) = crate::process::get_process(pid) {
        let (old_brk, current_brk) = {
                let mut p_proc = proc.proc.lock();
                let mut r = p_proc.resources.lock();
                let old_brk = r.brk_current;
                if new_brk == 0 || new_brk < old_brk { return old_brk; }
                
                let aligned_new = (new_brk + 0xFFF) & !0xFFF;
                if aligned_new > old_brk {
                    // Check if it fits
                    let mut overlap = false;
                    for vma in r.vmas.iter() {
                        if old_brk < vma.end && aligned_new > vma.start {
                            overlap = true;
                            break;
                        }
                    }
                    if overlap { return old_brk; }
                    
                    // Expand
                    let obj = crate::vm_object::VMObject::new_anonymous(aligned_new - old_brk);
                    r.vmas.push(crate::process::VMARegion {
                        start: old_brk,
                        end: aligned_new,
                        flags: 3, // RW
                        object: obj,
                        offset: 0,
                        is_huge: false,
                        is_shared: true, // Internal kernel maps are usually shared
                    });
                    vma_merge_adjacent(&mut r.vmas);
                    r.brk_current = aligned_new;
                    let current = r.brk_current;
                    drop(r);
                    p_proc.mem_frames += ((aligned_new - old_brk) / 4096) as u64;
                    (old_brk, current)
                } else {
                    let current = r.brk_current;
                    (old_brk, current)
                }
        };

        crate::process::update_process(pid, proc);
        return current_brk;
    }
    0
}

pub fn sys_mremap(addr: u64, old_len: u64, new_len: u64, flags: u64, new_addr: u64) -> u64 {
    // Basic stub
    linux_abi_error(38)
}

pub fn sys_madvise(addr: u64, len: u64, advice: u64) -> u64 {
    // Linux advice is often a hint; we can return success for most common cases
    0
}

pub fn sys_map_framebuffer(addr_ptr: u64, size_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some((phys, w, h, pitch, size, _)) = crate::boot::get_fb_info() {
        // Map the physical framebuffer into user space
        let target_vaddr = 0x4000_0000; // Fixed virtual address for FB or find gap
        let aligned_size = (size + 4095) & !4095;
        
        if let Some(mut proc) = crate::process::get_process(pid) {
            {
                let p_proc = proc.proc.lock();
                let mut r = p_proc.resources.lock();
                crate::memory::map_user_range(r.page_table_phys, target_vaddr, phys, aligned_size as u64, 3 << 0); // RW
                let obj = crate::vm_object::VMObject::new_physical(phys, aligned_size as u64);
                r.vmas.push(crate::process::VMARegion {
                    start: target_vaddr,
                    end: target_vaddr + aligned_size as u64,
                    flags: 3, // RW
                    object: obj,
                    offset: 0,
                    is_huge: false,
                    is_shared: true, // Framebuffer is shared
                });
                vma_merge_adjacent(&mut r.vmas);
            }
            crate::process::update_process(pid, proc);
            
            if addr_ptr != 0 && is_user_pointer(addr_ptr, 8) {
                unsafe { *(addr_ptr as *mut u64) = target_vaddr; }
            }
            if size_ptr != 0 && is_user_pointer(size_ptr, 8) {
                unsafe { *(size_ptr as *mut u64) = size as u64; }
            }
            return 0;
        }
    }
    linux_abi_error(5) // EIO
}

// --- END memory ---
// --- BEGIN ipc ---
// Ordering imported earlier in this module
use crate::ipc::{MessageType, receive_message};
// Mutex imported earlier in this module

/// Estadísticas de syscalls IPC
pub struct SyscallStats {
    pub send_calls: u64,
    pub receive_calls: u64,
}

pub static SYSCALL_STATS: Mutex<SyscallStats> = Mutex::new(SyscallStats {
    send_calls: 0,
    receive_calls: 0,
});

/// sys_send - Enviar mensaje IPC
/// arg4 = data_len (bytes to copy from data_ptr; max 512)
pub fn sys_send(server_id: u64, msg_type: u64, data_ptr: u64, data_len: u64) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.send_calls += 1;
    drop(stats);
    
    // Rechazar data_ptr en página nula (evita crash 0x11)
    if data_len > 0 && data_ptr != 0 && data_ptr < 0x1000 {
        return u64::MAX;
    }
    if let Some(client_id) = current_process_id() {
        let message_type = match msg_type {
            1 => MessageType::System,
            255 => MessageType::Signal, // Special signal type for P2P
            2 => MessageType::Memory,
            4 => MessageType::FileSystem,
            8 => MessageType::Network,
            0x10 => MessageType::Graphics,
            0x20 => MessageType::Audio,
            0x40 => MessageType::Input,
            _ => MessageType::User,
        };
        
        const MAX_MSG: usize = 512;
        let len = core::cmp::min(data_len as usize, MAX_MSG);
        let mut data = [0u8; 512];
        if len > 0 && data_ptr != 0 {
            if !copy_from_user(data_ptr, &mut data[..len]) {
                return u64::MAX;
            }
        }
        
        if len > 512 {
            crate::serial::serial_printf(format_args!("[IPC-SEND] ERROR: len {} exceeds max\n", len));
            return u64::MAX;
        }

        crate::serial::serial_printf(format_args!(
            "[IPC-SEND] from={} to={} type={:?} data_len={}\n",
            client_id, server_id, message_type, len
        ));

        if crate::ipc::send_message(client_id, server_id as u32, message_type, &data[..len]) {
            return 0; // Success
        }
    }
    
    u64::MAX // Error
}

/// sys_receive - Recibir mensaje IPC
pub fn sys_receive(buffer_ptr: u64, size: u64, sender_pid_ptr: u64) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.receive_calls += 1;
    drop(stats);
    
    // Rechazar punteros en página nula (evita crash 0x11 por punteros corruptos)
    if (buffer_ptr != 0 && buffer_ptr < 0x1000) || (sender_pid_ptr != 0 && sender_pid_ptr < 0x1000) {
        return u64::MAX;
    }
    if size > 4096 {
        return u64::MAX;
    }
    if buffer_ptr != 0 && !is_user_pointer(buffer_ptr, size) {
        return u64::MAX;
    }
    if sender_pid_ptr != 0 && !is_user_pointer(sender_pid_ptr, 8) {
        return u64::MAX;
    }
    
    if let Some(client_id) = current_process_id() {
        if let Some(msg) = receive_message(client_id) {
            RECV_OK.fetch_add(1, Ordering::Relaxed);
            crate::serial::serial_printf(format_args!(
                "[IPC-RECV] pid={} got msg data_size={} from={} type={:?}\n",
                client_id, msg.data_size, msg.from, msg.msg_type
            ));
            // Calcular cuántos bytes copiar al buffer del usuario
            let data_size = msg.data_size as usize;
            if data_size > 512 {
                crate::serial::serial_printf(format_args!("[IPC-RECV] ERROR: data_size {} corrupted\n", data_size));
                return u64::MAX;
            }
            let data_len = data_size.min(msg.data.len());
            let copy_len = core::cmp::min(size as usize, data_len);

            if copy_len > 0 && buffer_ptr != 0 {
                // Pre-check if copy_len is huge (e.g. -38)
                if copy_len > 1024 * 1024 { // 1MB threshold for insanity
                    crate::serial::serial_printf(format_args!("[IPC-RECV] ERROR: insane copy_len {}\n", copy_len));
                    return u64::MAX;
                }
                if !copy_to_user(buffer_ptr, &msg.data[..copy_len]) {
                    return u64::MAX;
                }
            }

            // Escribir el PID del remitente si se solicitó
            if sender_pid_ptr != 0 {
                let b = (msg.from as u64).to_le_bytes();
                if !copy_to_user(sender_pid_ptr, &b[..4]) {
                    return u64::MAX;
                }
            }
            return copy_len as u64;
        }
        RECV_EMPTY.fetch_add(1, Ordering::Relaxed);
        // Diagnóstico (solo una vez por segundo aproximadamente, usando RECV_EMPTY como throttle).
        if client_id == 11 {
            let empty = RECV_EMPTY.load(Ordering::Relaxed);
            if empty % 50000 == 1 {
                crate::serial::serial_printf(format_args!(
                    "[RECV-EMPTY] glxgears pid=11 mailbox empty (current_pid_from_gs={})\n",
                    client_id
                ));
            }
        }
    }
    0 // No hay mensajes
}

pub fn sys_receive_fast(ctx: &mut crate::interrupts::SyscallContext) -> u64 {
    let pid = match current_process_id() {
        Some(p) => p,
        None => return 0,
    };

    // Camino rápido al estilo del código antiguo: solo mensajes pequeños (≤24 B),
    // entregados directamente en registros.
    if let Some((data_size, from, data)) = crate::ipc::pop_small_message_24(pid) {
        RECV_OK.fetch_add(1, Ordering::Relaxed);
        let mut w = [0u64; 3];
        for i in 0..3 {
            let off = i * 8;
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&data[off..off + 8]);
            w[i] = u64::from_le_bytes(buf);
        }
        ctx.rdi = w[0];
        ctx.rsi = w[1];
        ctx.rdx = w[2];
        ctx.rcx = from as u64;
        return data_size as u64;
    }

    RECV_EMPTY.fetch_add(1, Ordering::Relaxed);
    0
}
// --- END ipc ---
// --- BEGIN misc ---
// Miscellaneous syscalls implementation
//
// Time, system info, and Eclipse-specific management syscalls.

// process module is imported earlier in this module
use alloc::string::String;

static HOSTNAME: Mutex<Option<String>> = Mutex::new(None);

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Timeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

#[repr(C)]
pub struct Utsname {
    pub sysname: [u8; 65],
    pub nodename: [u8; 65],
    pub release: [u8; 65],
    pub version: [u8; 65],
    pub machine: [u8; 65],
    pub domainname: [u8; 65],
}

#[repr(C)]
pub struct SysInfo {
    pub uptime: i64,
    pub loads: [u64; 3],
    pub totalram: u64,
    pub freeram: u64,
    pub sharedram: u64,
    pub bufferram: u64,
    pub totalswap: u64,
    pub freeswap: u64,
    pub procs: u16,
    pub totalhigh: u64,
    pub freehigh: u64,
    pub mem_unit: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Rlimit {
    pub rlim_cur: u64,
    pub rlim_max: u64,
}

#[repr(C)]
pub struct Rusage {
    pub ru_utime: Timeval,
    pub ru_stime: Timeval,
    pub ru_maxrss: i64,
    pub ru_ixrss: i64,
    pub ru_idrss: i64,
    pub ru_isrss: i64,
    pub ru_minflt: i64,
    pub ru_majflt: i64,
    pub ru_nswap: i64,
    pub ru_inblock: i64,
    pub ru_oublock: i64,
    pub ru_msgsnd: i64,
    pub ru_msgrcv: i64,
    pub ru_nsignals: i64,
    pub ru_nvcsw: i64,
    pub ru_nivcsw: i64,
}

pub fn sys_yield() -> u64 {
    crate::scheduler::yield_cpu();
    0
}

pub fn sys_nanosleep(req_ptr: u64, rem_ptr: u64) -> u64 {
    if !is_user_pointer(req_ptr, 16) { return linux_abi_error(14); }
    
    let mut ts = core::mem::MaybeUninit::<Timespec>::uninit();
    let ts_bytes = unsafe {
        core::slice::from_raw_parts_mut(ts.as_mut_ptr() as *mut u8, core::mem::size_of::<Timespec>())
    };
    if !copy_from_user(req_ptr, ts_bytes) {
        return linux_abi_error(14);
    }
    let ts = unsafe { ts.assume_init() };
    let ms = ts.tv_sec.saturating_mul(1000).saturating_add(ts.tv_nsec / 1_000_000);
    
    crate::scheduler::sleep(ms as u64);
    
    if rem_ptr != 0 && is_user_pointer(rem_ptr, 16) {
        let zero = Timespec { tv_sec: 0, tv_nsec: 0 };
        let out = unsafe {
            core::slice::from_raw_parts(&zero as *const Timespec as *const u8, core::mem::size_of::<Timespec>())
        };
        if !copy_to_user(rem_ptr, out) {
            return linux_abi_error(14);
        }
    }
    0
}

pub fn sys_gettimeofday(tv_ptr: u64, _tz_ptr: u64) -> u64 {
    if tv_ptr == 0 { return 0; }
    if !is_user_pointer(tv_ptr, core::mem::size_of::<Timeval>() as u64) {
        return linux_abi_error(14);
    }
    // `WALL_TIME_OFFSET` = (Unix time in s) − (uptime in s); ticks ≈ ms de uptime.
    let ticks = crate::interrupts::ticks();
    let wall_off_sec = WALL_TIME_OFFSET.load(Ordering::Relaxed);
    let sec = (wall_off_sec + ticks / 1000) as i64;
    let usec = ((ticks % 1000) * 1000) as i64;
    let tv = Timeval { tv_sec: sec, tv_usec: usec };
    let out = unsafe {
        core::slice::from_raw_parts(&tv as *const Timeval as *const u8, core::mem::size_of::<Timeval>())
    };
    if !copy_to_user(tv_ptr, out) { return linux_abi_error(14); }
    0
}

pub fn sys_clock_gettime(clk_id: u64, tp_ptr: u64) -> u64 {
    if !is_user_pointer(tp_ptr, core::mem::size_of::<Timespec>() as u64) {
        return linux_abi_error(14);
    }
    
    let uptime_ms = crate::interrupts::ticks();
    let (sec, nsec) = match clk_id {
        0 => { // CLOCK_REALTIME
            let off = WALL_TIME_OFFSET.load(Ordering::Relaxed);
            let s = off + uptime_ms / 1000;
            (s as i64, ((uptime_ms % 1000) * 1_000_000) as i64)
        }
        1 | 4 => { // CLOCK_MONOTONIC / CLOCK_BOOTTIME
            ((uptime_ms / 1000) as i64, ((uptime_ms % 1000) * 1_000_000) as i64)
        }
        _ => { // Otros relojes: monotónico
            ((uptime_ms / 1000) as i64, ((uptime_ms % 1000) * 1_000_000) as i64)
        }
    };
    
    let ts = Timespec { tv_sec: sec, tv_nsec: nsec };
    let out = unsafe {
        core::slice::from_raw_parts(&ts as *const Timespec as *const u8, core::mem::size_of::<Timespec>())
    };
    if !copy_to_user(tp_ptr, out) { return linux_abi_error(14); }
    0
}

pub fn sys_getrlimit(resource: u64, rlim_ptr: u64) -> u64 {
    if !is_user_pointer(rlim_ptr, 16) { return linux_abi_error(14); }
    let limit = match resource {
        7 => Rlimit { rlim_cur: 1024, rlim_max: 4096 }, // RLIMIT_NOFILE
        _ => Rlimit { rlim_cur: u64::MAX, rlim_max: u64::MAX },
    };
    let out = unsafe {
        core::slice::from_raw_parts(&limit as *const Rlimit as *const u8, core::mem::size_of::<Rlimit>())
    };
    if !copy_to_user(rlim_ptr, out) { return linux_abi_error(14); }
    0
}

pub fn sys_getrusage(_who: u64, usage_ptr: u64) -> u64 {
    if !is_user_pointer(usage_ptr, core::mem::size_of::<Rusage>() as u64) {
        return linux_abi_error(14);
    }
    let usage = unsafe { core::mem::zeroed::<Rusage>() };
    let out = unsafe {
        core::slice::from_raw_parts(&usage as *const Rusage as *const u8, core::mem::size_of::<Rusage>())
    };
    if !copy_to_user(usage_ptr, out) { return linux_abi_error(14); }
    0
}

pub fn sys_sysinfo(info_ptr: u64) -> u64 {
    if !is_user_pointer(info_ptr, core::mem::size_of::<SysInfo>() as u64) {
        return linux_abi_error(14);
    }
    
    let (total_frames, used_frames) = crate::memory::get_memory_stats();
    let sched_stats = crate::scheduler::get_stats();
    
    let info = SysInfo {
        uptime: (sched_stats.total_ticks / 1000) as i64,
        loads: [0, 0, 0],
        totalram: total_frames * 4096,
        freeram: total_frames.saturating_sub(used_frames) * 4096,
        sharedram: 0,
        bufferram: 0,
        totalswap: 0,
        freeswap: 0,
        procs: crate::process::process_count() as u16,
        totalhigh: 0,
        freehigh: 0,
        mem_unit: 1,
    };
    
    let out = unsafe {
        core::slice::from_raw_parts(&info as *const SysInfo as *const u8, core::mem::size_of::<SysInfo>())
    };
    if !copy_to_user(info_ptr, out) { return linux_abi_error(14); }
    0
}

pub fn sys_uname(buf_ptr: u64) -> u64 {
    if !is_user_pointer(buf_ptr, 390) { return linux_abi_error(14); }
    
    let mut uts = Utsname {
        sysname: [0; 65],
        nodename: [0; 65],
        release: [0; 65],
        version: [0; 65],
        machine: [0; 65],
        domainname: [0; 65],
    };
    
    fill_uts_buf(&mut uts.sysname, "Eclipse");
    {
        let h = HOSTNAME.lock();
        fill_uts_buf(&mut uts.nodename, h.as_deref().unwrap_or("eclipse"));
    }
    fill_uts_buf(&mut uts.release, "3.0.0-eclipse");
    fill_uts_buf(&mut uts.version, "#1 SMP Eclipse Microkernel");
    fill_uts_buf(&mut uts.machine, "x86_64");
    
    let out = unsafe {
        core::slice::from_raw_parts(&uts as *const Utsname as *const u8, core::mem::size_of::<Utsname>())
    };
    if !copy_to_user(buf_ptr, out) { return linux_abi_error(14); }
    0
}

fn fill_uts_buf(buf: &mut [u8; 65], s: &str) {
    let bytes = s.as_bytes();
    let n = bytes.len().min(64);
    buf[..n].copy_from_slice(&bytes[..n]);
}

pub fn sys_sethostname(name_ptr: u64, len: u64) -> u64 {
    if len > 64 { return linux_abi_error(22); }
    let mut buf = alloc::vec![0u8; len as usize];
    if !copy_from_user(name_ptr, &mut buf) {
        return linux_abi_error(14);
    }
    if let Ok(name) = core::str::from_utf8(&buf) {
        *HOSTNAME.lock() = Some(String::from(name));
        0
    } else {
        linux_abi_error(22)
    }
}

pub fn sys_getrandom(buf_ptr: u64, len: u64, _flags: u64) -> u64 {
    if !is_user_pointer(buf_ptr, len) { return linux_abi_error(14); }
    let mut i = 0;
    while i < len {
        let rnd = crate::cpu::get_random_u64();
        let bytes = rnd.to_ne_bytes();
        let to_copy = core::cmp::min(bytes.len() as u64, len - i);
        if !copy_to_user(buf_ptr + i, &bytes[..to_copy as usize]) {
            return linux_abi_error(14);
        }
        i += to_copy;
    }
    len
}

pub fn sys_membarrier(_cmd: u64, _flags: u64, _cpu_id: u64) -> u64 {
    unsafe { core::arch::asm!("mfence", options(nostack, preserves_flags)); }
    0
}

pub fn sys_get_service_binary(_service_id: u64, _buf_ptr: u64, _buf_size: u64) -> u64 {
    linux_abi_error(38) // ENOSYS
}

pub fn sys_get_logs(buf_ptr: u64, len: u64) -> u64 {
    if !is_user_pointer(buf_ptr, len) { return u64::MAX; }
    let count = crate::serial::copy_logs_to_user(buf_ptr, len);
    count as u64
}

pub fn sys_stop_progress() -> u64 {
    crate::progress::stop_logging();
    0
}

pub fn sys_register_log_hud(pid: u64) -> u64 {
    crate::progress::set_log_hud_pid(pid as u32);
    0
}

pub fn sys_get_storage_device_count() -> u64 {
    crate::storage::device_count() as u64
}

pub fn sys_get_system_stats(stats_ptr: u64) -> u64 {
    use SystemStats;

    if stats_ptr == 0 || !is_user_pointer(stats_ptr, core::mem::size_of::<SystemStats>() as u64) {
        return u64::MAX;
    }

    let sched_stats = crate::scheduler::get_stats();
    crate::nvidia::update_all_gpu_vitals();
    let vitals = crate::ai_core::get_vitals();
    let (total_frames, used_frames) = crate::memory::get_memory_stats();

    let stats = SystemStats {
        uptime_ms: sched_stats.total_ticks,
        idle_ms: sched_stats.idle_ticks,
        total_memory_kb: total_frames * 4,
        free_memory_kb: total_frames.saturating_sub(used_frames) * 4,
        cpu_load: vitals.cpu_load,
        cpu_temp: vitals.cpu_temp,
        gpu_load: vitals.gpu_load,
        gpu_temp: vitals.gpu_temp,
        gpu_vram_total_kb: vitals.gpu_vram_total_bytes / 1024,
        gpu_vram_used_kb: vitals.gpu_vram_used_bytes / 1024,
        anomaly_count: vitals.anomaly_count,
        heap_fragmentation: vitals.heap_fragmentation,
        wall_time_offset: WALL_TIME_OFFSET.load(Ordering::Relaxed),
    };

    let out = unsafe {
        core::slice::from_raw_parts(&stats as *const SystemStats as *const u8, core::mem::size_of::<SystemStats>())
    };
    if !copy_to_user(stats_ptr, out) { return linux_abi_error(14); }
    0
}

/// Fija el reloj de pared: `time` = Unix time en segundos; se guarda el offset frente al uptime.
pub fn sys_set_time(secs: u64) -> u64 {
    let uptime_ms = crate::scheduler::get_stats().total_ticks;
    let offset = secs.saturating_sub(uptime_ms / 1000);
    WALL_TIME_OFFSET.store(offset, Ordering::Relaxed);
    0
}

pub fn sys_read_key() -> u64 {
    crate::interrupts::read_key() as u64
}

pub fn sys_read_mouse_packet() -> u64 {
    crate::interrupts::read_mouse_packet() as u64
}

pub fn sys_register_device(_name_ptr: u64, _name_len: u64, _type_id: u64) -> u64 {
    0
}

pub fn sys_prlimit64(_pid: u64, resource: u64, new_limit_ptr: u64, old_limit_ptr: u64) -> u64 {
    let _ = new_limit_ptr;
    if old_limit_ptr != 0 {
        if !is_user_pointer(old_limit_ptr, 16) { return linux_abi_error(14); }
        let limit = match resource {
            7 => Rlimit { rlim_cur: 1024, rlim_max: 4096 },
            _ => Rlimit { rlim_cur: u64::MAX, rlim_max: u64::MAX },
        };
        let out = unsafe {
            core::slice::from_raw_parts(&limit as *const Rlimit as *const u8, core::mem::size_of::<Rlimit>())
        };
        if !copy_to_user(old_limit_ptr, out) { return linux_abi_error(14); }
    }
    0
}

pub fn sys_pci_enum_devices(buf_ptr: u64, max_count: u64, _a: u64) -> u64 {
    if buf_ptr == 0 {
        return crate::pci::get_device_count() as u64;
    }
    if !is_user_pointer(buf_ptr, max_count * core::mem::size_of::<crate::pci::PciDevice>() as u64) {
        return u64::MAX;
    }
    crate::pci::enum_devices_to_user(buf_ptr, max_count as usize) as u64
}

pub fn sys_pci_read_config(address: u64, offset: u64, size: u64) -> u64 {
    let bus = ((address >> 16) & 0xFF) as u8;
    let slot = ((address >> 8) & 0xFF) as u8;
    let func = (address & 0xFF) as u8;
    unsafe {
        match size {
            1 => crate::pci::pci_config_read_u8(bus, slot, func, offset as u8) as u64,
            2 => crate::pci::pci_config_read_u16(bus, slot, func, offset as u8) as u64,
            4 => crate::pci::pci_config_read_u32(bus, slot, func, offset as u8) as u64,
            _ => 0,
        }
    }
}

pub fn sys_pci_write_config(address: u64, offset: u64, size: u64, value: u64) -> u64 {
    let bus = ((address >> 16) & 0xFF) as u8;
    let slot = ((address >> 8) & 0xFF) as u8;
    let func = (address & 0xFF) as u8;
    unsafe {
        match size {
            1 => crate::pci::pci_config_write_u8(bus, slot, func, offset as u8, value as u8),
            2 => crate::pci::pci_config_write_u16(bus, slot, func, offset as u8, value as u16),
            4 => crate::pci::pci_config_write_u32(bus, slot, func, offset as u8, value as u32),
            _ => (),
        }
    }
    0
}
// --- END misc ---
// --- BEGIN graphics ---
// Graphics and DRM-related syscalls implementation
//
// Support for VirtIO-GPU, Virgl 3D, and standard display buffer management.

// Imports for this section are shared at module scope.

pub fn sys_get_framebuffer_info(user_buffer: u64) -> u64 {
    use crate::servers::FramebufferInfo;

    if user_buffer == 0 {
        return u64::MAX;
    }
    if !is_user_pointer(user_buffer, core::mem::size_of::<FramebufferInfo>() as u64) {
        return u64::MAX;
    }

    let (fb_phys, width, height, pitch, fb_size) = {
        let k = &crate::boot::get_boot_info().framebuffer;
        if crate::boot::gop_framebuffer_valid() {
            let addr = if k.base_address >= crate::memory::PHYS_MEM_OFFSET {
                k.base_address - crate::memory::PHYS_MEM_OFFSET
            } else {
                k.base_address
            };
            let pitch = k.pixels_per_scan_line * 4;
            let size = (pitch as u64).saturating_mul(k.height as u64);
            (addr, k.width, k.height, pitch, size)
        } else if let Some((phys, w, h, p, size)) = crate::virtio::get_primary_virtio_display() {
            (phys, w, h, p, size as u64)
        } else if let Some((phys, _bar1, w, h, pitch)) = crate::nvidia::get_nvidia_fb_info() {
            let size = (pitch as u64).saturating_mul(h as u64);
            (phys, w, h, pitch, size)
        } else {
            return u64::MAX;
        }
    };

    // Estilo “syscalls antiguas”: devolver una VA mapeada en el proceso (no un físico).
    let current_pid = crate::process::current_process_id();
    let page_table_phys = crate::process::get_process_page_table(current_pid);
    if page_table_phys == 0 {
        return u64::MAX;
    }
    let fb_vaddr = crate::memory::map_framebuffer_for_process(page_table_phys, fb_phys, fb_size);
    if fb_vaddr == 0 {
        return u64::MAX;
    }

    let syscall_fb = FramebufferInfo {
        address: fb_vaddr,
        width,
        height,
        pitch,
        bpp: 32,
        red_mask_size: 8,
        red_mask_shift: 16,
        green_mask_size: 8,
        green_mask_shift: 8,
        blue_mask_size: 8,
        blue_mask_shift: 0,
    };
    let out = unsafe {
        core::slice::from_raw_parts(&syscall_fb as *const FramebufferInfo as *const u8, core::mem::size_of::<FramebufferInfo>())
    };
    if !copy_to_user(user_buffer, out) { return u64::MAX; }
    0
}

pub fn sys_get_gpu_display_info(user_buffer: u64) -> u64 {
    if user_buffer == 0 {
        return u64::MAX;
    }
    if !is_user_pointer(user_buffer, 8) {
        return u64::MAX;
    }
    let Some((width, height)) = crate::virtio::get_gpu_display_info() else {
        return u64::MAX;
    };
    if !copy_to_user(user_buffer, &width.to_le_bytes()) { return u64::MAX; }
    if !copy_to_user(user_buffer + 4, &height.to_le_bytes()) { return u64::MAX; }
    0
}

pub fn sys_set_cursor_position(arg1: u64, arg2: u64) -> u64 {
    let x = arg1 as i32;
    let y = arg2 as i32;
    
    // Try via unified DRM subsystem first (handles VirtIO hardware cursor and future drivers)
    // Flags 0x02 = DRM_CURSOR_MOVE
    if !crate::drm::set_cursor(0, x, y, 0, 0x02) {
        // Fall back to legacy / software cursor if DRM failed or no driver supports it
        crate::sw_cursor::update(x as u32, y as u32);
    }
    0
}

pub fn sys_gpu_alloc_display_buffer(width: u64, height: u64, out_ptr: u64) -> u64 {
    let (width, height) = (width as u32, height as u32);
    if width == 0 || height == 0 || out_ptr == 0 {
        return u64::MAX;
    }
    if !is_user_pointer(out_ptr, 24) {
        return u64::MAX;
    }
    let Some((phys_addr, resource_id, pitch, size)) = crate::virtio::gpu_alloc_display_buffer(width, height) else {
        return u64::MAX;
    };
    let current_pid = crate::process::current_process_id();
    let page_table_phys = crate::process::get_process_page_table(current_pid);
    if page_table_phys == 0 {
        return u64::MAX;
    }
    let vaddr = crate::memory::map_framebuffer_for_process(page_table_phys, phys_addr, size as u64);
    if vaddr == 0 {
        return u64::MAX;
    }
    if !copy_to_user(out_ptr, &vaddr.to_le_bytes()) { return u64::MAX; }
    if !copy_to_user(out_ptr + 8, &resource_id.to_le_bytes()) { return u64::MAX; }
    if !copy_to_user(out_ptr + 12, &pitch.to_le_bytes()) { return u64::MAX; }
    if !copy_to_user(out_ptr + 16, &(size as u64).to_le_bytes()) { return u64::MAX; }
    0
}

pub fn sys_gpu_present(resource_id: u64, x: u64, y: u64, w: u64, h: u64) -> u64 {
    if crate::virtio::gpu_present(
        resource_id as u32,
        x as u32,
        y as u32,
        w as u32,
        h as u32,
    ) {
        0
    } else {
        u64::MAX
    }
}

pub fn sys_gpu_command(cmd: u64, arg1: u64, arg2: u64) -> u64 {
    0
}

pub fn sys_gpu_get_backend() -> u64 {
    // 1: VirtIO GPU
    // 2: NVIDIA
    // 3: EFI GOP (Fallback)
    if crate::virtio::get_primary_virtio_display().is_some() {
        1
    } else if crate::nvidia::get_nvidia_fb_info().is_some() {
        2
    } else {
        3
    }
}

pub fn sys_drm_page_flip(fd: u64, crtc_id: u64, fb_id: u64, flags: u64, user_data: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        match crate::scheme::ioctl(fd_entry.scheme_id, fd_entry.resource_id, 0x100 /* DRM_IOCTL_PAGE_FLIP */, fb_id as usize) {
            Ok(_) => 0,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_drm_get_caps(fd: u64, cap_ptr: u64) -> u64 {
    // Route to scheme ioctl
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        match crate::scheme::ioctl(fd_entry.scheme_id, fd_entry.resource_id, 0xC010640C /* DRM_IOCTL_GET_CAP */, cap_ptr as usize) {
            Ok(_) => 0,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_drm_alloc_buffer(fd: u64, width: u64, height: u64, bpp: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        #[repr(C)]
        struct DrmModeCreateDumb {
            height: u32, width: u32, bpp: u32, flags: u32,
            handle: u32, pitch: u32, size: u64,
        }
        let mut arg = DrmModeCreateDumb {
            height: height as u32,
            width: width as u32,
            bpp: bpp as u32,
            flags: 0, handle: 0, pitch: 0, size: 0,
        };
        match crate::scheme::ioctl(fd_entry.scheme_id, fd_entry.resource_id, 0xC02064B2 /* DRM_IOCTL_MODE_CREATE_DUMB */, &mut arg as *mut _ as usize) {
            Ok(_) => arg.handle as u64,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_drm_create_fb(fd: u64, handle: u64, width: u64, height: u64, pitch: u64, bpp: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        #[repr(C)]
        struct DrmModeFbCmd {
            fb_id: u32, width: u32, height: u32, pitch: u32,
            bpp: u32, depth: u32, handle: u32,
        }
        let mut cmd = DrmModeFbCmd {
            fb_id: 0,
            width: width as u32,
            height: height as u32,
            pitch: pitch as u32,
            bpp: bpp as u32,
            depth: 24, // Assuming 24-bit depth
            handle: handle as u32,
        };
        match crate::scheme::ioctl(fd_entry.scheme_id, fd_entry.resource_id, 0xC01C64AE /* DRM_IOCTL_MODE_ADDFB */, &mut cmd as *mut _ as usize) {
            Ok(_) => cmd.fb_id as u64,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_drm_map_handle(fd: u64, handle: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        #[repr(C)]
        struct DrmModeMapDumb {
            handle: u32, pad: u32, offset: u64,
        }
        let mut map = DrmModeMapDumb { handle: handle as u32, pad: 0, offset: 0 };
        match crate::scheme::ioctl(fd_entry.scheme_id, fd_entry.resource_id, 0xC01064B3 /* DRM_IOCTL_MODE_MAP_DUMB */, &mut map as *mut _ as usize) {
            Ok(_) => map.offset,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

// VirtIO-GPU / Virgl 3D Syscalls

pub fn sys_virgl_ctx_create(_ctx_id: u64, name_ptr: u64, name_len: u64) -> u64 {
    let mut name_buf = [0u8; 64];
    let l = (name_len as usize).min(63);
    if name_ptr != 0 { let _ = copy_from_user(name_ptr, &mut name_buf[..l]); }
    
    match crate::virtio::virgl_ctx_create(&name_buf[..l]) {
        Some(id) => id as u64,
        None => u64::MAX,
    }
}

pub fn sys_virgl_ctx_destroy(ctx_id: u64) -> u64 {
    if crate::virtio::virgl_ctx_destroy(ctx_id as u32) { 0 } else { u64::MAX }
}

pub fn sys_virgl_ctx_attach_resource(ctx_id: u64, res_id: u64) -> u64 {
    if crate::virtio::virgl_ctx_attach_resource(ctx_id as u32, res_id as u32) { 0 } else { u64::MAX }
}

pub fn sys_virgl_ctx_detach_resource(ctx_id: u64, res_id: u64) -> u64 {
    if crate::virtio::virgl_ctx_detach_resource(ctx_id as u32, res_id as u32) { 0 } else { u64::MAX }
}

pub fn sys_virgl_alloc_backing(res_id: u64, size: u64) -> u64 {
    if let Some((phys, _)) = crate::virtio::virgl_alloc_backing(size as usize) {
        phys
    } else {
        0
    }
}

pub fn sys_virgl_resource_attach_backing(res_id: u64, phys_addr: u64, size: u64) -> u64 {
    if crate::virtio::virgl_resource_attach_backing(res_id as u32, phys_addr, size as usize) { 0 } else { u64::MAX }
}

pub fn sys_virgl_submit_3d(ctx_id: u64, cmd_ptr: u64, cmd_len: u64) -> u64 {
    if !is_user_pointer(cmd_ptr, cmd_len) { return linux_abi_error(14); }
    
    // We need to copy the command buffer to kernel space because the driver
    // expects a slice and might use it for DMA.
    let mut cmd_buf = alloc::vec![0u8; cmd_len as usize];
    let _ = copy_from_user(cmd_ptr, &mut cmd_buf);
    
    if crate::virtio::virgl_submit_3d(ctx_id as u32, &cmd_buf) {
        0
    } else {
        u64::MAX
    }
}
// --- END graphics ---
// --- BEGIN network ---
// Network-related syscalls implementation

use alloc::string::ToString;
// current_process_id is imported at module scope; helpers are in this module.

#[repr(C)]
pub struct Msghdr {
    pub msg_name: u64,
    pub msg_namelen: u32,
    pub msg_iov: u64,
    pub msg_iovlen: u64,
    pub msg_control: u64,
    pub msg_controllen: u64,
    pub msg_flags: i32,
}

#[repr(C)]
pub struct Iovec {
    pub iov_base: u64,
    pub iov_len: u64,
}

#[repr(C)]
pub struct Cmsghdr {
    pub cmsg_len: u64,
    pub cmsg_level: i32,
    pub cmsg_type: i32,
}

pub fn sys_socket(domain: u64, type_: u64, protocol: u64) -> u64 {
    let path = format!("socket:{}/{}/{}", domain, type_, protocol);
    match crate::scheme::open(&path, 0, 0) {
        Ok((sid, rid)) => {
            let pid = current_process_id().unwrap_or(0);
            crate::fd::fd_create(pid, sid, rid).unwrap_or(0) as u64
        }
        Err(e) => linux_abi_error(e as i32),
    }
}

pub fn sys_connect(fd: u64, addr_ptr: u64, addr_len: u64) -> u64 {
    if let Some(pid) = current_process_id() {
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            let mut buf = Vec::with_capacity(addr_len as usize);
            unsafe { buf.set_len(addr_len as usize); }
            if !copy_from_user(addr_ptr, &mut buf) {
                return linux_abi_error(14);
            }
            
            // Extract path for UNIX sockets (skip family)
            let path = if addr_len > 2 {
                let s = String::from_utf8_lossy(&buf[2..]);
                s.trim_matches('\0').to_string()
            } else {
                String::new()
            };

            let socket_scheme = match crate::servers::get_socket_scheme() {
                Some(s) => s,
                None => return linux_abi_error(38),
            };

            match socket_scheme.connect(fd_entry.resource_id, &path) {
                Ok(_) => 0,
                Err(e) => linux_abi_error(e as i32),
            }
        } else { linux_abi_error(9) }
    } else { linux_abi_error(9) }
}

pub fn sys_accept(fd: u64, _addr_ptr: u64, _addr_len_ptr: u64) -> u64 {
    if let Some(pid) = current_process_id() {
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            let socket_scheme = match crate::servers::get_socket_scheme() {
                Some(s) => s,
                None => return linux_abi_error(38),
            };

            match socket_scheme.accept(fd_entry.resource_id) {
                Ok(new_rid) => {
                    crate::fd::fd_create(pid, fd_entry.scheme_id, new_rid).unwrap_or(0) as u64
                }
                Err(e) => linux_abi_error(e as i32),
            }
        } else { linux_abi_error(9) }
    } else { linux_abi_error(9) }
}

pub fn sys_sendto(fd: u64, buf_ptr: u64, len: u64, _flags: u64, _dest_ptr: u64, _dest_len: u64) -> u64 {
    fs::sys_write(fd, buf_ptr, len)
}

pub fn sys_recvfrom(fd: u64, buf_ptr: u64, len: u64, _flags: u64, _src_ptr: u64, _src_len_ptr: u64) -> u64 {
    fs::sys_read(fd, buf_ptr, len)
}

pub fn sys_sendmsg(fd: u64, msg_ptr: u64, _flags: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let fd_entry = match crate::fd::fd_get(pid, fd as usize) {
        Some(e) => e,
        None => return linux_abi_error(9),
    };

    if !is_user_pointer(msg_ptr, core::mem::size_of::<Msghdr>() as u64) {
        return linux_abi_error(14);
    }
    let mut msg = core::mem::MaybeUninit::<Msghdr>::uninit();
    let msg_bytes = unsafe {
        core::slice::from_raw_parts_mut(msg.as_mut_ptr() as *mut u8, core::mem::size_of::<Msghdr>())
    };
    if !copy_from_user(msg_ptr, msg_bytes) { return linux_abi_error(14); }
    let msg = unsafe { msg.assume_init() };

    // Accumulate data from iovecs
    let mut data = Vec::new();
    for i in 0..msg.msg_iovlen {
        let iov_ptr = msg.msg_iov + i * core::mem::size_of::<Iovec>() as u64;
        if !is_user_pointer(iov_ptr, core::mem::size_of::<Iovec>() as u64) { break; }
        let mut iov = core::mem::MaybeUninit::<Iovec>::uninit();
        let iov_bytes = unsafe {
            core::slice::from_raw_parts_mut(iov.as_mut_ptr() as *mut u8, core::mem::size_of::<Iovec>())
        };
        if !copy_from_user(iov_ptr, iov_bytes) { return linux_abi_error(14); }
        let iov = unsafe { iov.assume_init() };
        if iov.iov_len > 0 {
            let mut buf = Vec::with_capacity(iov.iov_len as usize);
            unsafe { buf.set_len(iov.iov_len as usize); }
            if copy_from_user(iov.iov_base, &mut buf) {
                data.extend_from_slice(&buf);
            }
        }
    }

    let socket_scheme = match crate::servers::get_socket_scheme() {
        Some(s) => s,
        None => return linux_abi_error(38),
    };

    // Handle control messages (SCM_RIGHTS)
    if msg.msg_control != 0 && msg.msg_controllen >= 16 {
        let mut fds_to_send = Vec::new();
        // Simplified: just read the first cmsghdr
        if !is_user_pointer(msg.msg_control, core::mem::size_of::<Cmsghdr>() as u64) {
            return linux_abi_error(14);
        }
        let mut cmsg = core::mem::MaybeUninit::<Cmsghdr>::uninit();
        let cmsg_bytes = unsafe {
            core::slice::from_raw_parts_mut(cmsg.as_mut_ptr() as *mut u8, core::mem::size_of::<Cmsghdr>())
        };
        if !copy_from_user(msg.msg_control, cmsg_bytes) { return linux_abi_error(14); }
        let cmsg = unsafe { cmsg.assume_init() };
        if cmsg.cmsg_level == 1 && cmsg.cmsg_type == 1 { // SOL_SOCKET, SCM_RIGHTS
            let fd_count = (cmsg.cmsg_len - 16) / 4;
            for i in 0..fd_count {
                let ufd_addr = msg.msg_control + 16 + i * 4;
                if !is_user_pointer(ufd_addr, 4) { return linux_abi_error(14); }
                let mut b = [0u8; 4];
                if !copy_from_user(ufd_addr, &mut b) { return linux_abi_error(14); }
                let ufd = i32::from_le_bytes(b);
                if let Some(entry) = crate::fd::fd_get(pid, ufd as usize) {
                    fds_to_send.push((entry.scheme_id, entry.resource_id));
                }
            }
        }
        if !fds_to_send.is_empty() {
            socket_scheme.socket_enqueue_fds(fd_entry.resource_id, fds_to_send);
        }
    }

    match socket_scheme.socket_write_raw(fd_entry.resource_id, &data) {
        Ok(n) => n as u64,
        Err(e) => linux_abi_error(e as i32),
    }
}

pub fn sys_recvmsg(fd: u64, msg_ptr: u64, _flags: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let fd_entry = match crate::fd::fd_get(pid, fd as usize) {
        Some(e) => e,
        None => return linux_abi_error(9),
    };

    if !is_user_pointer(msg_ptr, core::mem::size_of::<Msghdr>() as u64) {
        return linux_abi_error(14);
    }
    let mut msg = core::mem::MaybeUninit::<Msghdr>::uninit();
    let msg_bytes = unsafe {
        core::slice::from_raw_parts_mut(msg.as_mut_ptr() as *mut u8, core::mem::size_of::<Msghdr>())
    };
    if !copy_from_user(msg_ptr, msg_bytes) { return linux_abi_error(14); }
    let mut msg = unsafe { msg.assume_init() };

    // Calculate total buffer space in iovecs
    let mut total_space = 0;
    let mut iovecs = Vec::new();
    for i in 0..msg.msg_iovlen {
        let iov_ptr = msg.msg_iov + i * core::mem::size_of::<Iovec>() as u64;
        if !is_user_pointer(iov_ptr, core::mem::size_of::<Iovec>() as u64) { break; }
        let mut iov = core::mem::MaybeUninit::<Iovec>::uninit();
        let iov_bytes = unsafe {
            core::slice::from_raw_parts_mut(iov.as_mut_ptr() as *mut u8, core::mem::size_of::<Iovec>())
        };
        if !copy_from_user(iov_ptr, iov_bytes) { return linux_abi_error(14); }
        let iov = unsafe { iov.assume_init() };
        total_space += iov.iov_len;
        iovecs.push(iov);
    }

    let mut data = Vec::with_capacity(total_space as usize);
    unsafe { data.set_len(total_space as usize); }

    let socket_scheme = match crate::servers::get_socket_scheme() {
        Some(s) => s,
        None => return linux_abi_error(38),
    };

    let n = match socket_scheme.socket_read_raw(fd_entry.resource_id, &mut data) {
        Ok(n) => n,
        Err(e) => return linux_abi_error(e as i32),
    };

    // Distribute data back to iovecs
    let mut remaining = n;
    let mut offset = 0;
    for iov in iovecs {
        if remaining == 0 { break; }
        let to_copy = core::cmp::min(remaining as u64, iov.iov_len) as usize;
        if copy_to_user(iov.iov_base, &data[offset..offset+to_copy]) {
            remaining -= to_copy;
            offset += to_copy;
        }
    }

    // Handle delivered FDs
    if msg.msg_control != 0 && msg.msg_controllen >= 16 {
        if let Some(fds) = socket_scheme.socket_dequeue_fds(fd_entry.resource_id) {
            let cmsg = Cmsghdr {
                cmsg_len: (16 + fds.len() * 4) as u64,
                cmsg_level: 1, // SOL_SOCKET
                cmsg_type: 1,  // SCM_RIGHTS
            };
            let out = unsafe {
                core::slice::from_raw_parts(&cmsg as *const Cmsghdr as *const u8, core::mem::size_of::<Cmsghdr>())
            };
            if !copy_to_user(msg.msg_control, out) { return linux_abi_error(14); }
            for (i, (sid, rid)) in fds.iter().enumerate() {
                if let Some(ufd) = crate::fd::fd_create(pid, *sid, *rid) {
                    let addr = msg.msg_control + 16 + i as u64 * 4;
                    let b = (ufd as i32).to_le_bytes();
                    if !copy_to_user(addr, &b) { return linux_abi_error(14); }
                }
            }
        }
    }

    n as u64
}

pub fn sys_shutdown(fd: u64, how: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        let socket_scheme = match crate::servers::get_socket_scheme() {
            Some(s) => s,
            None => return linux_abi_error(38),
        };
        match socket_scheme.shutdown(fd_entry.resource_id, how as i32) {
            Ok(_) => 0,
            Err(e) => linux_abi_error(e as i32),
        }
    } else { linux_abi_error(9) }
}

pub fn sys_bind(fd: u64, addr_ptr: u64, addr_len: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        let mut buf = Vec::with_capacity(addr_len as usize);
        unsafe { buf.set_len(addr_len as usize); }
        if !copy_from_user(addr_ptr, &mut buf) {
            return linux_abi_error(14);
        }
        
        let path = if addr_len > 2 {
            String::from_utf8_lossy(&buf[2..]).trim_matches('\0').to_string()
        } else {
            String::new()
        };

        let socket_scheme = match crate::servers::get_socket_scheme() {
            Some(s) => s,
            None => return linux_abi_error(38),
        };

        match socket_scheme.bind(fd_entry.resource_id, path) {
            Ok(_) => 0,
            Err(e) => linux_abi_error(e as i32),
        }
    } else { linux_abi_error(9) }
}

pub fn sys_listen(fd: u64, _backlog: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        let socket_scheme = match crate::servers::get_socket_scheme() {
            Some(s) => s,
            None => return linux_abi_error(38),
        };
        match socket_scheme.listen(fd_entry.resource_id) {
            Ok(_) => 0,
            Err(e) => linux_abi_error(e as i32),
        }
    } else { linux_abi_error(9) }
}

pub fn sys_getsockname(fd: u64, addr_ptr: u64, addr_len_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        if !is_user_pointer(addr_len_ptr, 4) { return linux_abi_error(14); }
        let mut b = [0u8; 4];
        if !copy_from_user(addr_len_ptr, &mut b) { return linux_abi_error(14); }
        let addr_len = u32::from_le_bytes(b);
        
        let mut buf = Vec::with_capacity(addr_len as usize);
        unsafe { buf.set_len(addr_len as usize); }
        
        let socket_scheme = match crate::servers::get_socket_scheme() {
            Some(s) => s,
            None => return linux_abi_error(38),
        };

        match socket_scheme.getsockname(fd_entry.resource_id, &mut buf) {
            Ok(n) => {
                // Prepend family (1 for AF_UNIX)
                if addr_len >= 2 {
                    buf[0] = 1; buf[1] = 0;
                }
                if !copy_to_user(addr_ptr, &buf) { return linux_abi_error(14); }
                let out = ((n + 2) as u32).to_le_bytes();
                if !copy_to_user(addr_len_ptr, &out) { return linux_abi_error(14); }
                0
            }
            Err(e) => linux_abi_error(e as i32),
        }
    } else { linux_abi_error(9) }
}

pub fn sys_getpeername(fd: u64, addr_ptr: u64, addr_len_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        if !is_user_pointer(addr_len_ptr, 4) { return linux_abi_error(14); }
        let mut b = [0u8; 4];
        if !copy_from_user(addr_len_ptr, &mut b) { return linux_abi_error(14); }
        let addr_len = u32::from_le_bytes(b);
        
        let mut buf = Vec::with_capacity(addr_len as usize);
        unsafe { buf.set_len(addr_len as usize); }
        
        let socket_scheme = match crate::servers::get_socket_scheme() {
            Some(s) => s,
            None => return linux_abi_error(38),
        };

        match socket_scheme.getpeername(fd_entry.resource_id, &mut buf) {
            Ok(n) => {
                if addr_len >= 2 {
                    buf[0] = 1; buf[1] = 0;
                }
                if !copy_to_user(addr_ptr, &buf) { return linux_abi_error(14); }
                let out = ((n + 2) as u32).to_le_bytes();
                if !copy_to_user(addr_len_ptr, &out) { return linux_abi_error(14); }
                0
            }
            Err(e) => linux_abi_error(e as i32),
        }
    } else { linux_abi_error(9) }
}

pub fn sys_socketpair(domain: u64, type_: u64, protocol: u64, sv_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let socket_scheme = match crate::servers::get_socket_scheme() {
        Some(s) => s,
        None => return linux_abi_error(38),
    };
    
    match socket_scheme.socketpair(domain as u32, type_ as u32, protocol as u32) {
        Ok((s1, s2)) => {
            let scheme_id = match crate::scheme::get_scheme_id("socket") {
                Some(id) => id,
                None => return linux_abi_error(38),
            };
            
            let fd1 = match crate::fd::fd_create(pid, scheme_id, s1) {
                Some(fd) => fd,
                None => {
                    use crate::scheme::Scheme;
                    let _ = socket_scheme.close(s1);
                    let _ = socket_scheme.close(s2);
                    return linux_abi_error(24);
                }
            };
            let fd2 = match crate::fd::fd_create(pid, scheme_id, s2) {
                Some(fd) => fd,
                None => {
                    use crate::scheme::Scheme;
                    let _ = crate::fd::fd_close(pid, fd1);
                    let _ = socket_scheme.close(s2);
                    return linux_abi_error(24);
                }
            };
            
            if !is_user_pointer(sv_ptr, 8) { return linux_abi_error(14); }
            let b1 = (fd1 as i32).to_le_bytes();
            let b2 = (fd2 as i32).to_le_bytes();
            if !copy_to_user(sv_ptr, &b1) { return linux_abi_error(14); }
            if !copy_to_user(sv_ptr + 4, &b2) { return linux_abi_error(14); }
            0
        }
        Err(e) => linux_abi_error(e as i32),
    }
}

pub fn sys_setsockopt(fd: u64, level: u64, optname: u64, optval: u64, optlen: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        let mut buf = Vec::with_capacity(optlen as usize);
        unsafe { buf.set_len(optlen as usize); }
        let _ = copy_from_user(optval, &mut buf);

        let socket_scheme = match crate::servers::get_socket_scheme() {
            Some(s) => s,
            None => return linux_abi_error(38),
        };
        match socket_scheme.setsockopt(fd_entry.resource_id, level as i32, optname as i32, &buf) {
            Ok(_) => 0,
            Err(e) => linux_abi_error(e as i32),
        }
    } else { linux_abi_error(9) }
}

pub fn sys_getsockopt(fd: u64, level: u64, optname: u64, optval: u64, optlen_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        if !is_user_pointer(optlen_ptr, 4) { return linux_abi_error(14); }
        let mut b = [0u8; 4];
        if !copy_from_user(optlen_ptr, &mut b) { return linux_abi_error(14); }
        let optlen = u32::from_le_bytes(b);
        
        let mut buf = Vec::with_capacity(optlen as usize);
        unsafe { buf.set_len(optlen as usize); }

        let socket_scheme = match crate::servers::get_socket_scheme() {
            Some(s) => s,
            None => return linux_abi_error(38),
        };
        match socket_scheme.getsockopt(fd_entry.resource_id, level as i32, optname as i32, &mut buf) {
            Ok(n) => {
                if !copy_to_user(optval, &buf) { return linux_abi_error(14); }
                let out = (n as u32).to_le_bytes();
                if !copy_to_user(optlen_ptr, &out) { return linux_abi_error(14); }
                0
            }
            Err(e) => linux_abi_error(e as i32),
        }
    } else { linux_abi_error(9) }
}
// --- END network ---
// --- BEGIN multiplex ---
// I/O Multiplexing syscalls implementation
//
// Implementation of poll, select, and epoll.

// format! and Vec are imported at module scope
// process module is imported earlier; current_process_id and helpers are in this module.

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FdSet {
    pub fds_bits: [u64; 16], // 1024 bits
}

#[repr(C)]
pub struct PollFd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}

pub fn sys_poll(fds_ptr: u64, nfds: u64, timeout: u64) -> u64 {
    if nfds == 0 {
        if timeout == 0 { return 0; }
        // sleep for timeout ms
        crate::scheduler::sleep(timeout);
        return 0;
    }

    let bytes = match (nfds as usize).checked_mul(core::mem::size_of::<PollFd>()) {
        Some(b) => b as u64,
        None => return linux_abi_error(14),
    };
    if !is_user_pointer(fds_ptr, bytes) {
        return linux_abi_error(14);
    }

    let pid = current_process_id().unwrap_or(0);
    let start_tick = crate::interrupts::ticks();

    loop {
        // Copiar (nfds * PollFd) desde userspace, operar en kernel, y copiar de vuelta.
        let mut fds: Vec<PollFd> = Vec::with_capacity(nfds as usize);
        unsafe { fds.set_len(nfds as usize); }
        let fds_bytes = unsafe {
            core::slice::from_raw_parts_mut(fds.as_mut_ptr() as *mut u8, bytes as usize)
        };
        if !copy_from_user(fds_ptr, fds_bytes) {
            return linux_abi_error(14);
        }

        let mut count = 0;
        for pfd in fds.iter_mut() {
            pfd.revents = 0;
            if pfd.fd < 0 { continue; }

            if let Some(fd_entry) = crate::fd::fd_get(pid, pfd.fd as usize) {
                match crate::scheme::poll(fd_entry.scheme_id, fd_entry.resource_id, pfd.events as usize) {
                    Ok(res) => {
                        pfd.revents = res as i16;
                        if res != 0 { count += 1; }
                    }
                    Err(_) => {
                        pfd.revents = 0x020; // POLLNVAL
                        count += 1;
                    }
                }
            } else {
                pfd.revents = 0x020; // POLLNVAL
                count += 1;
            }
        }

        // Copiar resultados de vuelta a userspace.
        let out_bytes = unsafe {
            core::slice::from_raw_parts(fds.as_ptr() as *const u8, bytes as usize)
        };
        if !copy_to_user(fds_ptr, out_bytes) {
            return linux_abi_error(14);
        }
        
        if count > 0 { return count as u64; }
        if timeout == 0 { return 0; }
        if timeout != u64::MAX && (crate::interrupts::ticks() - start_tick) >= timeout {
            return 0;
        }
        
        crate::scheduler::yield_cpu();
    }
}

pub fn sys_select(nfds: u64, read_ptr: u64, write_ptr: u64, except_ptr: u64, timeout_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    
    let timeout_ms = if timeout_ptr != 0 {
        if !is_user_pointer(timeout_ptr, core::mem::size_of::<Timeval>() as u64) {
            return linux_abi_error(14);
        }
        let mut tv = core::mem::MaybeUninit::<Timeval>::uninit();
        let tv_bytes = unsafe {
            core::slice::from_raw_parts_mut(tv.as_mut_ptr() as *mut u8, core::mem::size_of::<Timeval>())
        };
        if !copy_from_user(timeout_ptr, tv_bytes) {
            return linux_abi_error(14);
        }
        let tv = unsafe { tv.assume_init() };
        Some(tv.tv_sec as u64 * 1000 + (tv.tv_usec as u64 / 1000))
    } else {
        None
    };
    
    let start_tick = crate::interrupts::ticks();
    
    // Copy sets to kernel
    let mut rset = if read_ptr != 0 {
        if !is_user_pointer(read_ptr, 128) { return linux_abi_error(14); }
        let mut s = core::mem::MaybeUninit::<FdSet>::uninit();
        let s_bytes = unsafe {
            core::slice::from_raw_parts_mut(s.as_mut_ptr() as *mut u8, core::mem::size_of::<FdSet>())
        };
        if !copy_from_user(read_ptr, s_bytes) { return linux_abi_error(14); }
        Some(unsafe { s.assume_init() })
    } else { None };
    
    let mut wset = if write_ptr != 0 {
        if !is_user_pointer(write_ptr, 128) { return linux_abi_error(14); }
        let mut s = core::mem::MaybeUninit::<FdSet>::uninit();
        let s_bytes = unsafe {
            core::slice::from_raw_parts_mut(s.as_mut_ptr() as *mut u8, core::mem::size_of::<FdSet>())
        };
        if !copy_from_user(write_ptr, s_bytes) { return linux_abi_error(14); }
        Some(unsafe { s.assume_init() })
    } else { None };
    
    let mut eset = if except_ptr != 0 {
        if !is_user_pointer(except_ptr, 128) { return linux_abi_error(14); }
        let mut s = core::mem::MaybeUninit::<FdSet>::uninit();
        let s_bytes = unsafe {
            core::slice::from_raw_parts_mut(s.as_mut_ptr() as *mut u8, core::mem::size_of::<FdSet>())
        };
        if !copy_from_user(except_ptr, s_bytes) { return linux_abi_error(14); }
        Some(unsafe { s.assume_init() })
    } else { None };

    loop {
        let mut ready_count = 0;
        let mut out_rset = FdSet::default();
        let mut out_wset = FdSet::default();
        let mut out_eset = FdSet::default();

        for fd in 0..nfds as usize {
            if fd >= 1024 { break; }
            let word = fd / 64;
            let bit = 1u64 << (fd % 64);
            
            let mut events = 0;
            if rset.as_ref().map_or(false, |s| (s.fds_bits[word] & bit) != 0) { events |= crate::scheme::event::POLLIN; }
            if wset.as_ref().map_or(false, |s| (s.fds_bits[word] & bit) != 0) { events |= crate::scheme::event::POLLOUT; }
            if eset.as_ref().map_or(false, |s| (s.fds_bits[word] & bit) != 0) { events |= crate::scheme::event::POLLERR; }
            
            if events == 0 { continue; }
            
            if let Some(fd_entry) = crate::fd::fd_get(pid, fd) {
                match crate::scheme::poll(fd_entry.scheme_id, fd_entry.resource_id, events) {
                    Ok(revents) if revents != 0 => {
                        if (revents & crate::scheme::event::POLLIN) != 0 { out_rset.fds_bits[word] |= bit; }
                        if (revents & crate::scheme::event::POLLOUT) != 0 { out_wset.fds_bits[word] |= bit; }
                        if (revents & (crate::scheme::event::POLLERR | crate::scheme::event::POLLHUP)) != 0 { out_eset.fds_bits[word] |= bit; }
                        ready_count += 1;
                    }
                    _ => {}
                }
            }
        }
        
        if ready_count > 0 {
            if rset.is_some() {
                let out = unsafe {
                    core::slice::from_raw_parts(&out_rset as *const FdSet as *const u8, core::mem::size_of::<FdSet>())
                };
                if !copy_to_user(read_ptr, out) { return linux_abi_error(14); }
            }
            if wset.is_some() {
                let out = unsafe {
                    core::slice::from_raw_parts(&out_wset as *const FdSet as *const u8, core::mem::size_of::<FdSet>())
                };
                if !copy_to_user(write_ptr, out) { return linux_abi_error(14); }
            }
            if eset.is_some() {
                let out = unsafe {
                    core::slice::from_raw_parts(&out_eset as *const FdSet as *const u8, core::mem::size_of::<FdSet>())
                };
                if !copy_to_user(except_ptr, out) { return linux_abi_error(14); }
            }
            return ready_count as u64;
        }
        
        if let Some(ms) = timeout_ms {
            if ms == 0 { return 0; }
            if (crate::interrupts::ticks() - start_tick) >= ms { return 0; }
        }
        
        crate::scheduler::yield_cpu();
    }
}

pub fn sys_ppoll(fds_ptr: u64, nfds: u64, timeout_ptr: u64, _sigmask_ptr: u64, _sigsetsize: u64) -> u64 {
    // Simplified ppoll (ignoring sigmask for now)
    let timeout = if timeout_ptr != 0 {
        if !is_user_pointer(timeout_ptr, core::mem::size_of::<Timespec>() as u64) {
            return linux_abi_error(14);
        }
        let mut ts = core::mem::MaybeUninit::<Timespec>::uninit();
        let ts_bytes = unsafe {
            core::slice::from_raw_parts_mut(ts.as_mut_ptr() as *mut u8, core::mem::size_of::<Timespec>())
        };
        if !copy_from_user(timeout_ptr, ts_bytes) { return linux_abi_error(14); }
        let ts = unsafe { ts.assume_init() };
        ts.tv_sec as u64 * 1000 + (ts.tv_nsec as u64 / 1_000_000)
    } else {
        u64::MAX
    };
    sys_poll(fds_ptr, nfds, timeout)
}

pub fn sys_pselect6(nfds: u64, read_ptr: u64, write_ptr: u64, except_ptr: u64, timeout_ptr: u64, _sigmask_ptr: u64) -> u64 {
    // Simplified pselect6 (ignoring sigmask for now)
    let timeout_ms = if timeout_ptr != 0 {
        if !is_user_pointer(timeout_ptr, core::mem::size_of::<Timespec>() as u64) {
            return linux_abi_error(14);
        }
        let mut ts = core::mem::MaybeUninit::<Timespec>::uninit();
        let ts_bytes = unsafe {
            core::slice::from_raw_parts_mut(ts.as_mut_ptr() as *mut u8, core::mem::size_of::<Timespec>())
        };
        if !copy_from_user(timeout_ptr, ts_bytes) { return linux_abi_error(14); }
        let ts = unsafe { ts.assume_init() };
        Some(ts.tv_sec as u64 * 1000 + (ts.tv_nsec as u64 / 1_000_000))
    } else {
        None
    };
    
    // We need to convert Timespec to Timeval for sys_select if we want to reuse it,
    // or just implement it here.
    
    // For simplicity, I'll just reuse sys_select logic but with Timespec.
    // (Actual implementation would be a common helper).
    
    // Stub for now:
    sys_select(nfds, read_ptr, write_ptr, except_ptr, 0) // TODO: implement properly
}

pub fn sys_epoll_create1(_flags: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    match crate::scheme::open("epoll:", 0, 0) {
        Ok((scheme_id, resource_id)) => {
            match crate::fd::fd_create(pid, scheme_id, resource_id) {
                Some(fd) => fd as u64,
                None => {
                    let _ = crate::scheme::close(scheme_id, resource_id);
                    linux_abi_error(24) // EMFILE
                }
            }
        }
        Err(e) => linux_abi_error(e as i32),
    }
}

pub fn sys_epoll_ctl(epfd: u64, op: u64, fd: u64, event_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let epfd_entry = match crate::fd::fd_get(pid, epfd as usize) {
        Some(e) => e,
        None => return linux_abi_error(9), // EBADF
    };
    
    let event = if event_ptr != 0 {
        if !is_user_pointer(event_ptr, core::mem::size_of::<crate::epoll::EpollEvent>() as u64) {
            return linux_abi_error(14);
        }
        let mut ev = core::mem::MaybeUninit::<crate::epoll::EpollEvent>::uninit();
        let ev_bytes = unsafe {
            core::slice::from_raw_parts_mut(ev.as_mut_ptr() as *mut u8, core::mem::size_of::<crate::epoll::EpollEvent>())
        };
        if !copy_from_user(event_ptr, ev_bytes) { return linux_abi_error(14); }
        Some(unsafe { ev.assume_init() })
    } else {
        None
    };
    
    let epoll_scheme = crate::epoll::get_epoll_scheme();
    match epoll_scheme.ctl(epfd_entry.resource_id, op as usize, fd as usize, event) {
        Ok(_) => 0,
        Err(e) => linux_abi_error(e as i32),
    }
}

pub fn sys_epoll_wait(epfd: u64, events_ptr: u64, maxevents: u64, timeout: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let epfd_entry = match crate::fd::fd_get(pid, epfd as usize) {
        Some(e) => e,
        None => return linux_abi_error(9), // EBADF
    };
    
    let epoll_scheme = crate::epoll::get_epoll_scheme();
    let watched_fds = match epoll_scheme.get_instance_watched_fds(epfd_entry.resource_id) {
        Some(w) => w,
        None => return linux_abi_error(22), // EINVAL
    };
    
    let start_tick = crate::interrupts::ticks();
    // In Linux, timeout is in ms. -1 means infinite.
    let timeout_ms = if timeout == u64::MAX { None } else { Some(timeout) };

    loop {
        let mut count = 0;
        for (fd, ev_cfg) in &watched_fds {
            if count >= maxevents { break; }
            
            if let Some(fd_entry) = crate::fd::fd_get(pid, *fd) {
                match crate::scheme::poll(fd_entry.scheme_id, fd_entry.resource_id, ev_cfg.events as usize) {
                    Ok(revents) if revents != 0 => {
                        let out_ev = crate::epoll::EpollEvent {
                            events: revents as u32,
                            data: ev_cfg.data,
                        };
                        unsafe {
                            let ptr = (events_ptr as *mut crate::epoll::EpollEvent).add(count as usize);
                            core::ptr::write_unaligned(ptr, out_ev);
                        }
                        count += 1;
                    }
                    _ => {}
                }
            }
        }
        
        if count > 0 { return count as u64; }
        if let Some(ms) = timeout_ms {
            if ms == 0 { return 0; }
            if (crate::interrupts::ticks() - start_tick) >= ms { return 0; }
        }
        
        crate::scheduler::yield_cpu();
    }
}

pub fn sys_timerfd_create(clockid: u64, flags: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let path = format!("{}/{}", clockid, flags);
    match crate::scheme::open(&format!("timerfd:{}", path), 0, 0) {
        Ok((scheme_id, resource_id)) => {
            match crate::fd::fd_create(pid, scheme_id, resource_id) {
                Some(fd) => fd as u64,
                None => {
                    let _ = crate::scheme::close(scheme_id, resource_id);
                    linux_abi_error(24) // EMFILE
                }
            }
        }
        Err(e) => linux_abi_error(e as i32),
    }
}

pub fn sys_timerfd_settime(fd: u64, flags: u64, new_ptr: u64, old_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let fd_entry = match crate::fd::fd_get(pid, fd as usize) {
        Some(e) => e,
        None => return linux_abi_error(9),
    };
    
    if !is_user_pointer(new_ptr, core::mem::size_of::<crate::timerfd::Itimerspec>() as u64) {
        return linux_abi_error(14);
    }
    let new_val = unsafe { core::ptr::read_unaligned(new_ptr as *const crate::timerfd::Itimerspec) };
    
    let timerfd_scheme = crate::timerfd::get_timerfd_scheme();
    match timerfd_scheme.settime(fd_entry.resource_id, flags as i32, &new_val, old_ptr) {
        Ok(_) => 0,
        Err(e) => linux_abi_error(e as i32),
    }
}

pub fn sys_timerfd_gettime(fd: u64, cur_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let fd_entry = match crate::fd::fd_get(pid, fd as usize) {
        Some(e) => e,
        None => return linux_abi_error(9),
    };
    
    let timerfd_scheme = crate::timerfd::get_timerfd_scheme();
    match timerfd_scheme.gettime(fd_entry.resource_id, cur_ptr) {
        Ok(_) => 0,
        Err(e) => linux_abi_error(e as i32),
    }
}

pub fn sys_eventfd2(initval: u64, flags: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let path = format!("{}/{}", initval, flags);
    match crate::scheme::open(&format!("eventfd:{}", path), 0, 0) {
        Ok((scheme_id, resource_id)) => {
            match crate::fd::fd_create(pid, scheme_id, resource_id) {
                Some(fd) => fd as u64,
                None => {
                    let _ = crate::scheme::close(scheme_id, resource_id);
                    linux_abi_error(24) // EMFILE
                }
            }
        }
        Err(e) => linux_abi_error(e as i32),
    }
}

pub fn sys_inotify_init1(_flags: u64) -> u64 {
    linux_abi_error(38) // ENOSYS
}

pub fn sys_inotify_add_watch(_fd: u64, _path_ptr: u64, _mask: u64) -> u64 {
    linux_abi_error(38) // ENOSYS
}

pub fn sys_pause() -> u64 {
    let pid = current_process_id().unwrap_or(0);
    loop {
        if crate::process::get_pending_signals(pid) != 0 {
            return linux_abi_error(4); // EINTR
        }
        crate::scheduler::yield_cpu();
    }
}
// --- END multiplex ---
// --- BEGIN signals ---
// Signal handling syscalls and infrastructure
//
// Implementation of Linux-compatible signal delivery and management.

// exit_process/current_process_id/yield_cpu and helpers are in this module scope

/// Build and push an `rt_sigframe` onto the user stack, then redirect `ctx`
/// (the syscall return context) to the signal handler.
pub fn push_rt_signal_frame(
    ctx: &mut crate::interrupts::SyscallContext,
    pid: crate::process::ProcessId,
    sig: u8,
    action: &crate::process::SignalAction,
    old_mask: u64,
    fault_addr: u64,
    trap_num: u64,
) -> bool {
    use crate::process::{SA_ONSTACK, SA_NODEFER, SA_RESETHAND, SS_DISABLE};
    use RtSigframe;
    use UContext;
    use SigContext;
    use StackT;
    use SigInfo;
    
    // Require a valid restorer (musl always sets SA_RESTORER + __restore_rt).
    if action.restorer == 0 {
        return false;
    }
    let (alt_sp, alt_sz, alt_flags) = crate::process::get_process_altstack(pid);
    let mut using_altstack = false;
    let user_rsp = if (action.flags & SA_ONSTACK) != 0 {
        if (alt_flags & crate::process::SS_DISABLE) == 0 {
            if (alt_flags & crate::process::SS_ONSTACK) == 0 {
                using_altstack = true;
                alt_sp + alt_sz
            } else {
                ctx.rsp
            }
        } else {
            ctx.rsp
        }
    } else {
        ctx.rsp
    };

    // Allocate frame: reserve frame, align to 16 B.
    const FRAME_SZ: u64 = core::mem::size_of::<RtSigframe>() as u64;
    let frame_addr = (user_rsp.wrapping_sub(FRAME_SZ)) & !15u64;

    // Build the frame.
    let mut frame = RtSigframe {
        pretcode: action.restorer,
        uc: UContext {
            uc_flags:    0,
            uc_link:     0,
            uc_stack:    StackT { 
                ss_sp:    alt_sp, 
                ss_flags: alt_flags, 
                _pad:     0, 
                ss_size:  alt_sz 
            },
            uc_mcontext: SigContext {
                r8:      ctx.r8,
                r9:      ctx.r9,
                r10:     ctx.r10,
                r11:     ctx.r11,
                r12:     ctx.r12,
                r13:     ctx.r13,
                r14:     ctx.r14,
                r15:     ctx.r15,
                rdi:     ctx.rdi,
                rsi:     ctx.rsi,
                rbp:     ctx.rbp,
                rbx:     ctx.rbx,
                rdx:     ctx.rdx,
                rax:     ctx.rax,
                rcx:     ctx.rcx,
                rsp:     ctx.rsp,
                rip:     ctx.rip,
                eflags:  ctx.rflags,
                cs:      ctx.cs as u16,
                gs:      0,
                fs:      0,
                ss:      ctx.ss as u16,
                err:     0,
                trapno:  trap_num,
                oldmask: old_mask,
                cr2:     fault_addr,
                fpstate: 0,
                _reserved1: [0u64; 8],
            },
            uc_sigmask: old_mask,
        },
        info: SigInfo {
            si_signo: sig as i32,
            si_errno: 0,
            si_code:  0, // Set later if needed
            _rest:    [0u8; 116],
        },
        _pad: 0,
        fpstate: [0; 512],
    };

    // Save FPU state to the frame.
    unsafe {
        core::arch::asm!("fxsave [{}]", in(reg) &mut frame.fpstate[0]);
    }

    // Point uc_mcontext.fpstate to the fpstate buffer ON THE USER STACK.
    frame.uc.uc_mcontext.fpstate = frame_addr + 448;

    // Write the frame to user memory safely.
    let frame_bytes = unsafe {
        core::slice::from_raw_parts(&frame as *const RtSigframe as *const u8, FRAME_SZ as usize)
    };
    
    if !copy_to_user(frame_addr, frame_bytes) {
        crate::serial::serial_printf(format_args!("[SIG] Failed to push frame to {:#018x}\n", frame_addr));
        return false;
    }

    // Block this signal during the handler (and any additional signals from sa_mask),
    // unless SA_NODEFER is set.
    let _ = crate::process::modify_process(pid, |p| {
        if (action.flags & SA_NODEFER) == 0 {
            p.signal_mask |= 1u64 << sig;
        }
        p.signal_mask |= action.mask;
        
        // SIGKILL and SIGSTOP are unblockable.
        p.signal_mask &= !((1u64 << 8) | (1u64 << 18));

        // Set SS_ONSTACK if we moved to the altstack.
        if using_altstack {
            p.sigaltstack.ss_flags |= crate::process::SS_ONSTACK;
        }
    });

    // SA_RESETHAND: reset handler to SIG_DFL after first delivery.
    if (action.flags & SA_RESETHAND) != 0 {
        let _ = crate::process::modify_process(pid, |p| {
            let mut proc = p.proc.lock();
            if (sig as usize) < proc.signal_actions.len() {
                proc.signal_actions[sig as usize].handler = 0;
            }
        });
    }

    // Set up context for handler.
    ctx.rip = action.handler;
    ctx.rsp = frame_addr;
    ctx.rdi = sig as u64;
    ctx.rsi = frame_addr + 312; // Offset of 'info'
    ctx.rdx = frame_addr + 8;   // Offset of 'uc'
    ctx.rax = 0;
    
    // Clear RFLAGS.TF to prevent single-stepping into handler.
    ctx.rflags &= !0x100;
    
    true
}

/// Deliver pending signals that have userspace handlers by pushing signal frames.
pub fn deliver_pending_signals_for_current(ctx: &mut crate::interrupts::SyscallContext) {
    let Some(pid) = current_process_id() else { return };
    if crate::process::get_process(pid)
        .map_or(true, |p| p.state == crate::process::ProcessState::Terminated)
    {
        return;
    }

    loop {
        let (sig, action, old_mask) = {
            let p = match crate::process::get_process(pid) {
                Some(p) => p,
                None => break,
            };
            let old_mask = p.signal_mask;
            let Some((sig, action)) = crate::process::pop_lowest_pending_signal(pid) else {
                break;
            };
            (sig, action, old_mask)
        };

        if action.handler == 1 {
            // SIG_IGN — discard.
            continue;
        }

        if action.handler != 0 {
            // Userspace handler: try to push a signal frame.
            if push_rt_signal_frame(ctx, pid, sig, &action, old_mask, 0, 0) {
                // Successfully set up; handler will run on iretq.
                // Deliver one signal at a time per syscall return.
                break;
            }
            // Frame build failed (bad stack): fall through to fatal handling.
        }

        // SIG_DFL or frame-build failure.
        let is_fatal = sig == 9 // SIGKILL is always fatal
            || !crate::process::signal_default_is_ignore(sig);

        if !is_fatal {
            continue;
        }

        if let Some(mut proc) = crate::process::get_process(pid) {
            proc.proc.lock().exit_code = (128 + sig as u64) as i32;
            crate::process::update_process(pid, proc);
        }
        exit_process();
        yield_cpu();
        return;
    }
}

/// Deliver a signal to a userspace process directly from the exception handler.
pub fn deliver_signal_from_exception(
    exc:        &mut crate::interrupts::ExceptionContext,
    pid:        crate::process::ProcessId,
    signum:     u8,
    si_code:    i32,
    fault_addr: u64,
) -> bool {
    use crate::process::{SA_ONSTACK, SA_NODEFER, SA_RESETHAND, SS_DISABLE};
    use RtSigframe;
    use UContext;
    use SigContext;
    use StackT;
    use SigInfo;

    let (action, old_mask, user_rsp, alt_sp, alt_sz, alt_flags, using_altstack) = {
        let p = match crate::process::get_process(pid) {
            Some(p) => p,
            None    => return false,
        };
        let action = p.proc.lock().signal_actions[signum as usize];
        if action.handler == 0 || action.handler == 1 || action.restorer == 0 {
            return false;
        }
        let old_mask = p.signal_mask;
        let ss = p.sigaltstack;
        let rsp = exc.rsp;
        let mut using_altstack = false;
        let rsp = if (action.flags & SA_ONSTACK) != 0
            && (ss.ss_flags & SS_DISABLE) == 0
        {
            if (ss.ss_flags & crate::process::SS_ONSTACK) == 0 {
                using_altstack = true;
                ss.ss_sp.wrapping_add(ss.ss_size)
            } else {
                rsp
            }
        } else {
            rsp
        };
        (action, old_mask, rsp, ss.ss_sp, ss.ss_size, ss.ss_flags, using_altstack)
    };

    const FRAME_SZ: u64 = core::mem::size_of::<RtSigframe>() as u64;
    let frame_addr = (user_rsp.wrapping_sub(FRAME_SZ)) & !15u64;

    let mut frame = RtSigframe {
        pretcode: action.restorer,
        uc: UContext {
            uc_flags:    0,
            uc_link:     0,
            uc_stack:    StackT { 
                ss_sp:    alt_sp, 
                ss_flags: alt_flags, 
                _pad:     0, 
                ss_size:  alt_sz 
            },
            uc_mcontext: SigContext {
                r8:      exc.r8,
                r9:      exc.r9,
                r10:     exc.r10,
                r11:     exc.r11,
                r12:     exc.r12,
                r13:     exc.r13,
                r14:     exc.r14,
                r15:     exc.r15,
                rdi:     exc.rdi,
                rsi:     exc.rsi,
                rbp:     exc.rbp,
                rbx:     exc.rbx,
                rdx:     exc.rdx,
                rax:     exc.rax,
                rcx:     exc.rcx,
                rsp:     exc.rsp,
                rip:     exc.rip,
                eflags:  exc.rflags,
                cs:      exc.cs as u16,
                gs:      0,
                fs:      0,
                ss:      exc.ss as u16,
                err:     exc.error_code,
                trapno:  exc.num,
                oldmask: old_mask,
                cr2:     fault_addr,
                fpstate: 0,
                _reserved1: [0u64; 8],
            },
            uc_sigmask: old_mask,
        },
        info: SigInfo {
            si_signo: signum as i32,
            si_errno: 0,
            si_code:  si_code,
            _rest:    {
                let mut r = [0u8; 116];
                let addr_bytes = fault_addr.to_ne_bytes();
                for i in 0..8 {
                    r[4 + i] = addr_bytes[i];
                }
                r
            },
        },
        _pad: 0,
        fpstate: [0; 512],
    };

    // Save FPU state to the frame.
    unsafe {
        core::arch::asm!("fxsave [{}]", in(reg) &mut frame.fpstate[0]);
    }

    // Point uc_mcontext.fpstate to the fpstate buffer ON THE USER STACK.
    frame.uc.uc_mcontext.fpstate = frame_addr + 448;

    // Write the frame to user memory safely.
    let frame_bytes = unsafe {
        core::slice::from_raw_parts(&frame as *const RtSigframe as *const u8, FRAME_SZ as usize)
    };
    
    if !copy_to_user(frame_addr, frame_bytes) {
        return false;
    }

    // Block this signal during the handler (and any additional signals from sa_mask),
    // unless SA_NODEFER is set.
    let _ = crate::process::modify_process(pid, |p| {
        if (action.flags & SA_NODEFER) == 0 {
            p.signal_mask |= 1u64 << signum;
        }
        p.signal_mask |= action.mask;
        
        // Set SS_ONSTACK if we moved to the altstack.
        if using_altstack {
            p.sigaltstack.ss_flags |= crate::process::SS_ONSTACK;
        }
    });

    // SA_RESETHAND: reset handler to SIG_DFL after first delivery.
    if (action.flags & SA_RESETHAND) != 0 {
        let _ = crate::process::modify_process(pid, |p| {
            p.proc.lock().signal_actions[signum as usize].handler = 0;
        });
    }

    // Redirect the iretq to the signal handler.
    exc.rsp = frame_addr;
    exc.rip = action.handler;
    exc.rdi = signum as u64;
    exc.rsi = frame_addr + 312; // Offset of 'info'
    exc.rdx = frame_addr + 8;   // Offset of 'uc'
    exc.rax = 0;
    
    // Clear RFLAGS.TF to prevent single-stepping into handler.
    exc.rflags &= !0x100;
    exc.rflags |= 0x200; // Ensure IF=1 on return

    true
}

pub fn sys_rt_sigaction(sig: u64, act_ptr: u64, old_ptr: u64, sigsetsize: u64) -> u64 {
    if sigsetsize != 8 { return linux_abi_error(22); }
    let pid = current_process_id().unwrap_or(0);
    
    if old_ptr != 0 {
        if !is_user_pointer(old_ptr, core::mem::size_of::<crate::process::SignalAction>() as u64) {
            return linux_abi_error(14);
        }
        if let Some(p) = crate::process::get_process(pid) {
            let proc = p.proc.lock();
            if (sig as usize) < proc.signal_actions.len() {
                let old = proc.signal_actions[sig as usize];
                let out = unsafe {
                    core::slice::from_raw_parts(&old as *const crate::process::SignalAction as *const u8, core::mem::size_of::<crate::process::SignalAction>())
                };
                if !copy_to_user(old_ptr, out) { return linux_abi_error(14); }
            }
        }
    }
    
    if act_ptr != 0 {
        if !is_user_pointer(act_ptr, core::mem::size_of::<crate::process::SignalAction>() as u64) {
            return linux_abi_error(14);
        }
        let mut act = core::mem::MaybeUninit::<crate::process::SignalAction>::uninit();
        let act_bytes = unsafe {
            core::slice::from_raw_parts_mut(act.as_mut_ptr() as *mut u8, core::mem::size_of::<crate::process::SignalAction>())
        };
        if !copy_from_user(act_ptr, act_bytes) { return linux_abi_error(14); }
        let act = unsafe { act.assume_init() };
        let _ = crate::process::modify_process(pid, |p| {
            let mut proc = p.proc.lock();
            if (sig as usize) < proc.signal_actions.len() {
                proc.signal_actions[sig as usize] = act;
            }
        });
    }
    0
}

pub fn sys_rt_sigprocmask(how: u64, set_ptr: u64, old_ptr: u64, sigsetsize: u64) -> u64 {
    if sigsetsize != 8 { return linux_abi_error(22); }
    let pid = current_process_id().unwrap_or(0);
    
    if old_ptr != 0 {
        if !is_user_pointer(old_ptr, 8) { return linux_abi_error(14); }
        if let Some(p) = crate::process::get_process(pid) {
            if !copy_to_user(old_ptr, &p.signal_mask.to_le_bytes()) { return linux_abi_error(14); }
        }
    }
    
    if set_ptr != 0 {
        if !is_user_pointer(set_ptr, 8) { return linux_abi_error(14); }
        let mut b = [0u8; 8];
        if !copy_from_user(set_ptr, &mut b) { return linux_abi_error(14); }
        let set = u64::from_le_bytes(b);
        let _ = crate::process::modify_process(pid, |p| {
            match how {
                0 => p.signal_mask |= set, // SIG_BLOCK
                1 => p.signal_mask &= !set, // SIG_UNBLOCK
                2 => p.signal_mask = set,   // SIG_SETMASK
                _ => {}
            }
            // SIGKILL and SIGSTOP are unblockable.
            p.signal_mask &= !((1u64 << 8) | (1u64 << 18));
        });
    }
    0
}

pub fn sys_rt_sigreturn(ctx: &mut crate::interrupts::SyscallContext) -> u64 {
    use RtSigframe;
    let pid = current_process_id().unwrap_or(0);
    let frame_ptr = ctx.rsp; // RIP is at frame_ptr + pretcode(8) + uc(304)... no, rsp points to frame
    
    if !is_user_pointer(frame_ptr, core::mem::size_of::<RtSigframe>() as u64) {
        exit_process();
        return 0;
    }
    
    let mut frame = unsafe { core::mem::MaybeUninit::<RtSigframe>::uninit().assume_init() };
    if !copy_from_user(frame_ptr, unsafe { core::slice::from_raw_parts_mut(&mut frame as *mut _ as *mut u8, core::mem::size_of::<RtSigframe>()) }) {
        exit_process();
        return 0;
    }
    
    // Restore registers from uc_mcontext
    let m = &frame.uc.uc_mcontext;
    ctx.r8 = m.r8; ctx.r9 = m.r9; ctx.r10 = m.r10; ctx.r11 = m.r11;
    ctx.r12 = m.r12; ctx.r13 = m.r13; ctx.r14 = m.r14; ctx.r15 = m.r15;
    ctx.rdi = m.rdi; ctx.rsi = m.rsi; ctx.rbp = m.rbp; ctx.rbx = m.rbx;
    ctx.rdx = m.rdx; ctx.rax = m.rax; ctx.rcx = m.rcx; ctx.rsp = m.rsp;
    ctx.rip = m.rip; ctx.rflags = m.eflags;
    
    // Restore signal mask
    let _ = crate::process::modify_process(pid, |p| {
        p.signal_mask = frame.uc.uc_sigmask;
        // SIGKILL and SIGSTOP are unblockable.
        p.signal_mask &= !((1u64 << 8) | (1u64 << 18));
        
        // Clear SS_ONSTACK if we were using it.
        if (frame.uc.uc_stack.ss_flags & crate::process::SS_ONSTACK) != 0 {
            p.sigaltstack.ss_flags &= !crate::process::SS_ONSTACK;
        }
    });

    // Restore FPU state
    unsafe {
        core::arch::asm!("fxrstor [{}]", in(reg) &frame.fpstate[0]);
    }
    
    ctx.rax
}

pub fn sys_kill(pid: u64, sig: u64) -> u64 {
    if pid == 0 || pid == 1 {
        return linux_abi_error(1); // EPERM
    }

    let target_pid = pid as crate::process::ProcessId;

    if sig == 0 {
        return if crate::process::get_process(target_pid).is_some() {
            0
        } else {
            linux_abi_error(3) // ESRCH
        };
    }

    if sig == 9 {
        let parent_pid = match crate::process::terminate_other_process_by_signal(target_pid, 9) {
            None => return linux_abi_error(3),
            Some(pp) => pp,
        };

        if let Some(ppid) = parent_pid {
            crate::process::wake_parent_from_wait(ppid);
        }
        return 0;
    }

    crate::process::set_pending_signal(target_pid, sig as u8);
    0
}

pub fn sys_tkill(tid: u64, sig: u64) -> u64 {
    // In our model TID == PID for now (single thread per process)
    sys_kill(tid, sig)
}

pub fn sys_rt_sigpending(set_ptr: u64, sigsetsize: u64) -> u64 {
    if sigsetsize != 8 { return linux_abi_error(22); }
    if !is_user_pointer(set_ptr, 8) { return linux_abi_error(14); }
    let pid = current_process_id().unwrap_or(0);
    if let Some(p) = crate::process::get_process(pid) {
        if !copy_to_user(set_ptr, &p.pending_signals.to_le_bytes()) { return linux_abi_error(14); }
        return 0;
    }
    linux_abi_error(3)
}

pub fn sys_sigaltstack(ss_ptr: u64, old_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    
    if old_ptr != 0 {
        if !is_user_pointer(old_ptr, core::mem::size_of::<crate::process::Sigaltstack>() as u64) {
            return linux_abi_error(14);
        }
        if let Some(p) = crate::process::get_process(pid) {
            let out = unsafe {
                core::slice::from_raw_parts(&p.sigaltstack as *const crate::process::Sigaltstack as *const u8, core::mem::size_of::<crate::process::Sigaltstack>())
            };
            if !copy_to_user(old_ptr, out) { return linux_abi_error(14); }
        }
    }
    
    if ss_ptr != 0 {
        if !is_user_pointer(ss_ptr, core::mem::size_of::<crate::process::Sigaltstack>() as u64) {
            return linux_abi_error(14);
        }
        let mut ss = core::mem::MaybeUninit::<crate::process::Sigaltstack>::uninit();
        let ss_bytes = unsafe {
            core::slice::from_raw_parts_mut(ss.as_mut_ptr() as *mut u8, core::mem::size_of::<crate::process::Sigaltstack>())
        };
        if !copy_from_user(ss_ptr, ss_bytes) { return linux_abi_error(14); }
        let ss = unsafe { ss.assume_init() };
        
        // Cannot change altstack if currently using it
        if let Some(p) = crate::process::get_process(pid) {
            if (p.sigaltstack.ss_flags & crate::process::SS_ONSTACK) != 0 {
                return linux_abi_error(16); // EBUSY
            }
        }
        
        let _ = crate::process::modify_process(pid, |p| {
            p.sigaltstack = ss;
        });
    }
    0
}

pub fn sys_signalfd4(fd: u64, mask_ptr: u64, sigsetsize: u64, flags: u64) -> u64 {
    // Basic stub for now, would return a file descriptor that can be read to get signals
    if sigsetsize != 8 { return linux_abi_error(22); }
    linux_abi_error(38) // ENOSYS
}
// --- END signals ---
// --- BEGIN futex ---
// Futex (fast userspace mutex) — cola de espera en el kernel para `FUTEX_WAIT` / `FUTEX_WAKE`.
// Basado en el comportamiento Linux x86-64 usado por musl/pthreads.

// Vec is imported at module scope
// Mutex imported earlier in this module

use crate::process::ProcessState;
use crate::scheduler::{add_sleep, enqueue_process};

// copy_from_user/is_user_pointer/linux_abi_error are in this module

#[inline]
fn read_user_u32_volatile(addr: u64) -> Option<u32> {
    if !is_user_pointer(addr, 4) {
        return None;
    }
    // Recuperación ante #PF (misma filosofía que copy_from_user).
    if unsafe { !crate::interrupts::set_recovery_point() } {
        let v = unsafe { core::ptr::read_volatile(addr as *const u32) };
        unsafe { crate::interrupts::clear_recovery_point() };
        Some(v)
    } else {
        unsafe { crate::interrupts::clear_recovery_point() };
        None
    }
}

#[inline]
fn write_user_u32_volatile(addr: u64, v: u32) -> bool {
    if !is_user_pointer(addr, 4) {
        return false;
    }
    if unsafe { !crate::interrupts::set_recovery_point() } {
        unsafe { core::ptr::write_volatile(addr as *mut u32, v) };
        unsafe { crate::interrupts::clear_recovery_point() };
        true
    } else {
        unsafe { crate::interrupts::clear_recovery_point() };
        false
    }
}

struct FutexWaiter {
    addr: u64,
    pid: process::ProcessId,
    bitset: u32,
}

static FUTEX_WAITERS: Mutex<Vec<FutexWaiter>> = Mutex::new(Vec::new());

/// Despierta todos los procesos en cola para `addr` (p. ej. `set_tid_address` / `CLONE_CHILD_CLEARTID`).
pub fn futex_wake_all_atomic(addr: u64) {
    let mut woken = 0u32;
    let mut i = 0;
    let mut waiters = FUTEX_WAITERS.lock();
    while i < waiters.len() {
        if waiters[i].addr == addr {
            let wpid = waiters[i].pid;
            waiters.remove(i);
            enqueue_process(wpid);
            woken = woken.saturating_add(1);
        } else {
            i += 1;
        }
    }
}

/// `sys_futex` — op en bits bajos; `FUTEX_PRIVATE_FLAG` (128) y reloj se ignoran salvo wait bitset.
pub fn sys_futex(uaddr: u64, op: u64, val: u64, timeout_ptr: u64, uaddr2: u64, val3: u32) -> u64 {
    let pid = process::current_process_id().unwrap_or(0);
    let cmd = op & 0x7F;

    match cmd {
        0 | 9 => futex_wait(pid, uaddr, op, val, timeout_ptr, val3, cmd == 9),
        1 | 10 => futex_wake(uaddr, val, uaddr2, val3, cmd == 10),
        3 | 4 => futex_requeue(uaddr, val, timeout_ptr, uaddr2, val3, cmd == 4),
        5 => futex_wake_op(uaddr, val, timeout_ptr, uaddr2, val3),
        6..=8 | 11 | 12 => linux_abi_error(38), // PI / requeue-PI — ENOSYS
        _ => linux_abi_error(38),
    }
}

fn futex_wait(
    pid: process::ProcessId,
    uaddr: u64,
    _op: u64,
    val: u64,
    timeout_ptr: u64,
    val3: u32,
    is_bitset: bool,
) -> u64 {
    let bitset: u32 = if is_bitset { val3 } else { 0xFFFF_FFFF };

    if !is_user_pointer(uaddr, 4) { return linux_abi_error(14); }

    {
        let mut waiters = FUTEX_WAITERS.lock();
        waiters.retain(|w| w.pid != pid);
        waiters.push(FutexWaiter {
            addr: uaddr,
            pid,
            bitset,
        });
    }

    let Some(current) = read_user_u32_volatile(uaddr) else {
        let mut waiters = FUTEX_WAITERS.lock();
        waiters.retain(|w| w.pid != pid);
        return linux_abi_error(14);
    };
    if current != val as u32 {
        let mut waiters = FUTEX_WAITERS.lock();
        waiters.retain(|w| w.pid != pid);
        return linux_abi_error(11); // EAGAIN
    }

    let timeout_ms = if timeout_ptr != 0 && is_user_pointer(timeout_ptr, 16) {
        let mut b = [0u8; 16];
        if !copy_from_user(timeout_ptr, &mut b) {
            let mut waiters = FUTEX_WAITERS.lock();
            waiters.retain(|w| w.pid != pid);
            return linux_abi_error(14);
        }
        let sec = i64::from_le_bytes(b[0..8].try_into().unwrap());
        let nsec = i64::from_le_bytes(b[8..16].try_into().unwrap());
        if sec < 0 || nsec < 0 {
            let mut waiters = FUTEX_WAITERS.lock();
            waiters.retain(|w| w.pid != pid);
            return linux_abi_error(22);
        }
        Some((sec as u64).saturating_mul(1000).saturating_add((nsec as u64) / 1_000_000))
    } else {
        None
    };

    let start_ticks = crate::interrupts::ticks();

    let cas_ok = process::compare_and_set_process_state(
        pid,
        ProcessState::Running,
        ProcessState::Blocked,
    )
    .ok()
    .unwrap_or(false);

    if !cas_ok {
        let mut waiters = FUTEX_WAITERS.lock();
        waiters.retain(|w| w.pid != pid);
        return 0;
    }

    if let Some(ms) = timeout_ms {
        let wake = start_ticks.saturating_add(ms);
        add_sleep(pid, wake);
    }

    loop {
        if let Some(p) = process::get_process(pid) {
            if p.state != ProcessState::Blocked {
                let mut waiters = FUTEX_WAITERS.lock();
                waiters.retain(|w| w.pid != pid);
                return 0;
            }
        } else {
            let mut waiters = FUTEX_WAITERS.lock();
            waiters.retain(|w| w.pid != pid);
            return 0;
        }

        if let Some(ms) = timeout_ms {
            if crate::interrupts::ticks().saturating_sub(start_ticks) >= ms {
                let mut waiters = FUTEX_WAITERS.lock();
                waiters.retain(|w| w.pid != pid);
                let _ = process::compare_and_set_process_state(
                    pid,
                    ProcessState::Blocked,
                    ProcessState::Running,
                );
                return linux_abi_error(110); // ETIMEDOUT
            }
        }

        yield_cpu();
    }
}

fn futex_wake(uaddr: u64, max: u64, _uaddr2: u64, val3: u32, is_bitset: bool) -> u64 {
    let bitset: u32 = if is_bitset { val3 } else { 0xFFFF_FFFF };
    let mut woken: u64 = 0;
    let mut waiters = FUTEX_WAITERS.lock();
    let mut i = 0;
    while i < waiters.len() && woken < max {
        if waiters[i].addr == uaddr && (waiters[i].bitset & bitset) != 0 {
            let wpid = waiters[i].pid;
            waiters.remove(i);
            enqueue_process(wpid);
            woken += 1;
        } else {
            i += 1;
        }
    }
    woken
}

fn futex_requeue(uaddr: u64, wake_n: u64, max_requeue: u64, uaddr2: u64, val3: u32, is_cmp: bool) -> u64 {
    if is_cmp {
        if !is_user_pointer(uaddr, 4) {
            return linux_abi_error(14);
        }
        let Some(current) = read_user_u32_volatile(uaddr) else { return linux_abi_error(14); };
        if current != val3 {
            return linux_abi_error(11);
        }
    }
    let mut woken: u64 = 0;
    let mut requeued: u64 = 0;
    let mut waiters = FUTEX_WAITERS.lock();
    let mut i = 0;
    while i < waiters.len() {
        if waiters[i].addr == uaddr {
            if woken < wake_n {
                let wpid = waiters[i].pid;
                waiters.remove(i);
                enqueue_process(wpid);
                woken += 1;
            } else if requeued < max_requeue {
                waiters[i].addr = uaddr2;
                requeued += 1;
                i += 1;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    woken + requeued
}

fn futex_wake_op(uaddr: u64, wake1: u64, val2: u64, uaddr2: u64, val3: u32) -> u64 {
    // val2 = número a despertar en uaddr2 si la comparación con *uaddr2 (tras op) cumple
    let old_val2 = if is_user_pointer(uaddr2, 4) {
        let op_num = (val3 >> 28) & 0xF;
        let cmp = (val3 >> 24) & 0xF;
        let oparg = ((val3 >> 12) & 0xFFF) as u32;
        let cmparg = (val3 & 0xFFF) as u32;
        let effective_oparg = if op_num & 8 != 0 {
            1u32 << (oparg & 31)
        } else {
            oparg
        };
        let Some(old) = read_user_u32_volatile(uaddr2) else { return linux_abi_error(14); };
        let new_val = match op_num & 7 {
            0 => effective_oparg,
            1 => old.wrapping_add(effective_oparg),
            2 => old | effective_oparg,
            3 => old & !effective_oparg,
            4 => old ^ effective_oparg,
            _ => old,
        };
        if !write_user_u32_volatile(uaddr2, new_val) { return linux_abi_error(14); }
        let cmp_ok = match cmp {
            0 => old == cmparg,
            1 => old != cmparg,
            2 => old < cmparg,
            3 => old <= cmparg,
            4 => old > cmparg,
            5 => old >= cmparg,
            _ => false,
        };
        Some(cmp_ok)
    } else {
        None
    };
    let do_u2 = old_val2.unwrap_or(false);
    let mut woken: u64 = 0;
    {
        let mut waiters = FUTEX_WAITERS.lock();
        let mut i = 0;
        while i < waiters.len() && woken < wake1 {
            if waiters[i].addr == uaddr {
                let wpid = waiters[i].pid;
                waiters.remove(i);
                enqueue_process(wpid);
                woken += 1;
            } else {
                i += 1;
            }
        }
        if do_u2 {
            let mut w2: u64 = 0;
            i = 0;
            while i < waiters.len() && w2 < val2 {
                if waiters[i].addr == uaddr2 {
                    let wpid = waiters[i].pid;
                    waiters.remove(i);
                    enqueue_process(wpid);
                    w2 += 1;
                } else {
                    i += 1;
                }
            }
            woken += w2;
        }
    }
    woken
}
// --- END futex ---

use core::sync::atomic::{AtomicU32, AtomicU64};
use crate::interrupts::SyscallContext;

/// Debug/Tracing: Track the last syscall to aid in kernel debugging.
pub(crate) static LAST_SYSCALL_PID: AtomicU32 = AtomicU32::new(0);
pub(crate) static LAST_SYSCALL_NUM: AtomicU64 = AtomicU64::new(0);
pub(crate) static RECV_OK: AtomicU64 = AtomicU64::new(0);
pub(crate) static RECV_EMPTY: AtomicU64 = AtomicU64::new(0);

/// Inicialización del sistema de syscalls
pub fn init() {
    // Por ahora nada que inicializar, pero se deja el hook
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u64)]
pub enum SyscallNumber {
    Read = 0,
    Write = 1,
    Open = 2,
    Close = 3,
    Stat = 4,
    Fstat = 5,
    Lstat = 6,
    Poll = 7,
    Lseek = 8,
    Mmap = 9,
    Mprotect = 10,
    Munmap = 11,
    Brk = 12,
    RtSigaction = 13,
    RtSigprocmask = 14,
    RtSigreturn = 15,
    Ioctl = 16,
    Pread64 = 17,
    Pwrite64 = 18,
    Readv = 19,
    Writev = 20,
    Access = 21,
    Pipe = 22,
    Select = 23,
    Yield = 24,
    Mremap = 25,
    Msync = 26,
    Mincore = 27,
    Madvise = 28,
    Shmget = 29,
    Shmat = 30,
    Shmctl = 31,
    Dup = 32,
    Dup2 = 33,
    Pause = 34,
    Nanosleep = 35,
    Getitimer = 36,
    Alarm = 37,
    Setitimer = 38,
    Getpid = 39,
    Sendfile = 40,
    Socket = 41,
    Connect = 42,
    Accept = 43,
    Sendto = 44,
    Recvfrom = 45,
    Sendmsg = 46,
    Recvmsg = 47,
    Shutdown = 48,
    Bind = 49,
    Listen = 50,
    Getsockname = 51,
    Getpeername = 52,
    Socketpair = 53,
    Setsockopt = 54,
    Getsockopt = 55,
    Clone = 56,
    Fork = 57,
    Vfork = 58,
    Execve = 59,
    Exit = 60,
    Wait4 = 61,
    Kill = 62,
    Uname = 63,
    Semget = 64,
    Semop = 65,
    Semctl = 66,
    Shmdt = 67,
    Msgget = 68,
    Msgsnd = 69,
    Msgrcv = 70,
    Msgctl = 71,
    Fcntl = 72,
    Flock = 73,
    Fsync = 74,
    Fdatasync = 75,
    Truncate = 76,
    Ftruncate = 77,
    Getdents = 78,
    Getcwd = 79,
    Chdir = 80,
    Fchdir = 81,
    Rename = 82,
    Mkdir = 83,
    Rmdir = 84,
    Creat = 85,
    Link = 86,
    Unlink = 87,
    Symlink = 88,
    Readlink = 89,
    Chmod = 90,
    Fchmod = 91,
    Chown = 92,
    Fchown = 93,
    Lchown = 94,
    Umask = 95,
    Gettimeofday = 96,
    Getrlimit = 97,
    Getrusage = 98,
    Sysinfo = 99,
    Times = 100,
    Ptrace = 101,
    Getuid = 102,
    Syslog = 103,
    Getgid = 104,
    Setuid = 105,
    Setgid = 106,
    Geteuid = 107,
    Getegid = 108,
    Setpgid = 109,
    Getppid = 110,
    Getpgrp = 111,
    Setsid = 112,
    Setreuid = 113,
    Setregid = 114,
    Getgroups = 115,
    Setgroups = 116,
    Setresuid = 117,
    Getresuid = 118,
    Setresgid = 119,
    Getresgid = 120,
    Getpgid = 121,
    Setfsuid = 122,
    Setfsgid = 123,
    Getsid = 124,
    Capget = 125,
    Capset = 126,
    RtSigpending = 127,
    RtSigtimedwait = 128,
    RtSigqueueinfo = 129,
    RtSigsuspend = 130,
    Sigaltstack = 131,
    Utime = 132,
    Mknod = 133,
    Uselib = 134,
    Personality = 135,
    Ustat = 136,
    Statfs = 137,
    Fstatfs = 138,
    Sysfs = 139,
    Getpriority = 140,
    Setpriority = 141,
    SchedSetparam = 142,
    SchedGetparam = 143,
    SchedSetscheduler = 144,
    SchedGetscheduler = 145,
    SchedGetPriorityMax = 146,
    SchedGetPriorityMin = 147,
    SchedRrGetInterval = 148,
    Mlock = 149,
    Munlock = 150,
    Mlockall = 151,
    Munlockall = 152,
    Vhangup = 153,
    ModifyLdt = 154,
    PivotRoot = 155,
    Sysctl = 156,
    Prctl = 157,
    ArchPrctl = 158,
    Adjtimex = 159,
    Setrlimit = 160,
    Chroot = 161,
    Sync = 162,
    Acct = 163,
    Settimeofday = 164,
    MountLinux = 165,
    Umount2 = 166,
    Swapon = 167,
    Swapoff = 168,
    Reboot = 169,
    Sethostname = 170,
    Setdomainname = 171,
    Iopl = 172,
    Ioperm = 173,
    Gettid = 186,
    Readahead = 187,
    Setxattr = 188,
    Lsetxattr = 189,
    Fsetxattr = 190,
    Getxattr = 191,
    Lgetxattr = 192,
    Fgetxattr = 193,
    Listxattr = 194,
    Llistxattr = 195,
    Flistxattr = 196,
    Removexattr = 197,
    Lremovexattr = 198,
    Fremovexattr = 199,
    Tkill = 200,
    Time = 201,
    Futex = 202,
    SchedSetaffinity = 203,
    SchedGetaffinity = 204,
    IoSetup = 206,
    IoDestroy = 207,
    IoGetevents = 208,
    IoSubmit = 209,
    IoCancel = 210,
    LookupDcookie = 212,
    EpollCreate = 213,
    RemapFilePages = 216,
    Getdents64 = 217,
    SetTidAddress = 218,
    RestartSyscall = 219,
    Semtimedop = 220,
    Fadvise64 = 221,
    TimerCreate = 222,
    TimerSettime = 223,
    TimerGettime = 224,
    TimerGetoverrun = 225,
    TimerDelete = 226,
    ClockSettime = 227,
    ClockGettime = 228,
    ClockGetres = 229,
    ClockNanosleep = 230,
    ExitGroup = 231,
    EpollWait = 232,
    EpollCtl = 233,
    Tgkill = 234,
    Utimes = 235,
    Mbind = 237,
    SetMempolicy = 238,
    GetMempolicy = 239,
    MqOpen = 240,
    MqUnlink = 241,
    MqTimedsend = 242,
    MqTimedreceive = 243,
    MqNotify = 244,
    MqGetsetattr = 245,
    KexecLoad = 246,
    Waitid = 247,
    AddKey = 248,
    RequestKey = 249,
    Keyctl = 250,
    IoprioSet = 251,
    IoprioGet = 252,
    InotifyInit = 253,
    InotifyAddWatch = 254,
    InotifyRmWatch = 255,
    MigratePages = 256,
    Openat = 257,
    Mkdirat = 258,
    Mknodat = 259,
    Fchownat = 260,
    Futimesat = 261,
    Newfstatat = 262,
    Unlinkat = 263,
    Renameat = 264,
    Linkat = 265,
    Symlinkat = 266,
    Readlinkat = 267,
    Fchmodat = 268,
    Faccessat = 269,
    Pselect6 = 270,
    Ppoll = 271,
    Unshare = 272,
    SetRobustList = 273,
    GetRobustList = 274,
    Splice = 275,
    Tee = 276,
    SyncFileRange = 277,
    Vmsplice = 278,
    MovePages = 279,
    Utimensat = 280,
    EpollPwait = 281,
    Signalfd4 = 282,
    TimerfdCreate = 283,
    Eventfd = 284,
    Fallocate = 285,
    TimerfdSettime = 286,
    TimerfdGettime = 287,
    Accept4 = 288,
    Eventfd2 = 290,
    EpollCreate1 = 291,
    Dup3 = 292,
    Pipe2 = 293,
    InotifyInit1 = 294,
    Preadv = 295,
    Pwritev = 296,
    RtTgsigqueueinfo = 297,
    PerfEventOpen = 298,
    Recvmmsg = 299,
    FanotifyInit = 300,
    FanotifyMark = 301,
    Prlimit64 = 302,
    NameToHandleAt = 303,
    OpenByHandleAt = 304,
    ClockAdjtime = 305,
    Syncfs = 306,
    Sendmmsg = 307,
    Setns = 308,
    Getcpu = 309,
    ProcessVmReadv = 310,
    ProcessVmWritev = 311,
    Kcmp = 312,
    FinitModule = 313,
    SchedSetattr = 314,
    SchedGetattr = 315,
    Renameat2 = 316,
    Seccomp = 317,
    Getrandom = 318,
    MemfdCreate = 319,
    KexecFileLoad = 320,
    Bpf = 321,
    Execveat = 322,
    Userfaultfd = 323,
    Membarrier = 324,
    Mlock2 = 325,
    CopyFileRange = 326,
    Preadv2 = 327,
    Pwritev2 = 328,
    PkeyMprotect = 329,
    PkeyAlloc = 330,
    PkeyFree = 331,
    Statx = 332,
    IoPgetevents = 333,
    Rseq = 334,
    PidfdSendSignal = 424,
    IoUringSetup = 425,
    IoUringEnter = 426,
    IoUringRegister = 427,
    OpenTree = 428,
    MoveMount = 429,
    Fsopen = 430,
    Fsconfig = 431,
    Fsmount = 432,
    Fspick = 433,
    PidfdOpen = 434,
    Clone3 = 435,
    CloseRange = 436,
    Openat2 = 437,
    PidfdGetfd = 438,
    Faccessat2 = 439,
    ProcessMadvise = 440,

    // Eclipse-specific syscalls (Range 500+)
    Send = 500,
    Receive = 501,
    GetServiceBinary = 502,
    GetFramebufferInfo = 503,
    MapFramebuffer = 504,
    PciEnumDevices = 505,
    PciReadConfig = 506,
    PciWriteConfig = 507,
    RegisterDevice = 508,
    Fmap = 509,
    Mount = 510,
    Spawn = 511,
    GetLastExecError = 512,
    ReadKey = 513,
    ReadMousePacket = 514,
    GetGpuDisplayInfo = 515,
    SetCursorPosition = 516,
    GpuAllocDisplayBuffer = 517,
    GpuPresent = 518,
    GetLogs = 519,
    GetStorageDeviceCount = 520,
    GetSystemStats = 521,
    GetProcessList = 522,
    SetProcessName = 523,
    SpawnService = 524,
    GpuCommand = 525,
    StopProgress = 526,
    GetGpuBackend = 527,
    DrmPageFlip = 528,
    DrmGetCaps = 529,
    DrmAllocBuffer = 530,
    DrmCreateFb = 531,
    DrmMapHandle = 532,
    SchedSetaffinityEclipse = 533,
    RegisterLogHud = 534,
    SetTime = 535,
    SpawnWithStdio = 536,
    ThreadCreate = 537,
    WaitPid = 538,
    Readdir = 539,
    SetChildArgs = 542,
    GetProcessArgs = 543,
    SpawnWithStdioPath = 544,
    Strace = 545,
    Exec = 546,

    VirglCtxCreate = 570,
    VirglCtxDestroy = 571,
    VirglCtxAttachResource = 572,
    VirglCtxDetachResource = 573,
    VirglAllocBacking = 574,
    VirglResourceAttachBacking = 575,
    VirglResourceSubmit3d = 576,

    ReceiveFast = 600,
}

// ----------------------------------------------------------------------------
// Internal namespace shims
// ----------------------------------------------------------------------------
// This file contains syscall implementations in a single module, but the
// dispatcher references them via namespaces like `fs::`, `memory::`, etc.
// Provide lightweight namespaces by re-exporting the parent module items.
pub mod fs { pub use super::*; }
pub mod memory { pub use super::*; }
pub mod ipc { pub use super::*; }
pub mod misc { pub use super::*; }
pub mod graphics { pub use super::*; }
pub mod network { pub use super::*; }
pub mod multiplex { pub use super::*; }
pub mod signals { pub use super::*; }
pub mod futex { pub use super::*; }
pub mod sc_process { pub use super::*; }


// Signal infrastructure lives in this module.

// Signal related types for ABI
#[repr(C)]
pub struct SigInfo {
    pub si_signo: i32,
    pub si_errno: i32,
    pub si_code:  i32,
    pub _rest:    [u8; 116],
}

#[repr(C)]
pub struct StackT {
    pub ss_sp:    u64,
    pub ss_flags: i32,
    pub _pad:     u32,
    pub ss_size:  u64,
}

#[repr(C)]
pub struct SigContext {
    pub r8: u64, pub r9: u64, pub r10: u64, pub r11: u64,
    pub r12: u64, pub r13: u64, pub r14: u64, pub r15: u64,
    pub rdi: u64, pub rsi: u64, pub rbp: u64, pub rbx: u64,
    pub rdx: u64, pub rax: u64, pub rcx: u64, pub rsp: u64,
    pub rip: u64, pub eflags: u64,
    pub cs: u16, pub gs: u16, pub fs: u16, pub ss: u16,
    pub err: u64, pub trapno: u64, pub oldmask: u64, pub cr2: u64,
    pub fpstate: u64,
    pub _reserved1: [u64; 8],
}

#[repr(C)]
pub struct UContext {
    pub uc_flags:    u64,
    pub uc_link:     u64,
    pub uc_stack:    StackT,
    pub uc_mcontext: SigContext,
    pub uc_sigmask:  u64,
}

#[repr(C)]
pub struct RtSigframe {
    pub pretcode: u64,
    pub uc:       UContext,
    pub info:     SigInfo,
    pub _pad:     u64,
    pub fpstate:  [u8; 512],
}

/// Statistics for sys_get_system_stats
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemStats {
    pub uptime_ms: u64,
    pub idle_ms: u64,
    pub total_memory_kb: u64,
    pub free_memory_kb: u64,
    pub cpu_load: [u32; 16],
    pub cpu_temp: [u32; 16],
    pub gpu_load: [u32; 4],
    pub gpu_temp: [u32; 4],
    pub gpu_vram_total_kb: u64,
    pub gpu_vram_used_kb: u64,
    pub anomaly_count: u32,
    pub heap_fragmentation: u32,
    pub wall_time_offset: u64,
}

/// Process info for sys_get_process_list
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub state: u32,
    pub cpu_usage: u32,
    pub mem_usage_kb: u64,
    pub name: [u8; 32],
    pub thread_count: u32,
    pub priority: u32,
}

/// Entrada principal de syscalls (desde el stub en `interrupts`).
pub extern "C" fn syscall_handler(
    num: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
    arg6: u64,
    context: &mut SyscallContext,
) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    LAST_SYSCALL_PID.store(pid, Ordering::Relaxed);
    LAST_SYSCALL_NUM.store(num, Ordering::Relaxed);

    if pid != 0 && !crate::ai_core::audit_syscall(pid, num) {
        return 0xFFFF_FFFF_FFFF_FFFF;
    }

    let (strace, p_name): (bool, String) = if let Some(p) = crate::process::get_process(pid) {
        let proc = p.proc.lock();
        let end = proc.name.iter().position(|&b| b == 0).unwrap_or(proc.name.len());
        let n = core::str::from_utf8(&proc.name[..end])
            .unwrap_or("?")
            .trim();
        let name = if n.is_empty() { String::from("unknown") } else { String::from(n) };
        (proc.syscall_trace, name)
    } else {
        (false, String::new())
    };

    let result = match num {
        // --- Filesystem (Linux) ---
        0   => fs::sys_read(arg1, arg2, arg3),
        1   => fs::sys_write(arg1, arg2, arg3),
        2   => fs::sys_open(arg1, arg2, arg3),
        3   => fs::sys_close(arg1),
        4   => fs::sys_stat(arg1, arg2),
        5   => fs::sys_fstat(arg1, arg2),
        6   => fs::sys_fstatat(0xFFFFFFFFFFFFFF9C, arg1, arg2, 0x100), // lstat
        7   => multiplex::sys_poll(arg1, arg2, arg3),
        8   => fs::sys_lseek(arg1, arg2 as i64, arg3),
        9   => memory::sys_mmap(arg1, arg2, arg3, arg4, arg5, arg6),
        10  => memory::sys_mprotect(arg1, arg2, arg3),
        11  => memory::sys_munmap(arg1, arg2),
        12  => memory::sys_brk(arg1),
        13  => signals::sys_rt_sigaction(arg1, arg2, arg3, arg4),
        14  => signals::sys_rt_sigprocmask(arg1, arg2, arg3, arg4),
        15  => signals::sys_rt_sigreturn(context),
        16  => fs::sys_ioctl(arg1, arg2, arg3),
        17  => fs::sys_pread64(arg1, arg2, arg3, arg4),
        18  => fs::sys_pwrite64(arg1, arg2, arg3, arg4),
        19  => fs::sys_readv(arg1, arg2, arg3),
        20  => fs::sys_writev(arg1, arg2, arg3),
        21  => fs::sys_faccessat(0xFFFFFFFFFFFFFF9C, arg1, arg2, 0), // access
        22  => fs::sys_pipe(arg1),
        23  => multiplex::sys_select(arg1, arg2, arg3, arg4, arg5),
        24  => misc::sys_yield(),
        25  => memory::sys_mremap(arg1, arg2, arg3, arg4, arg5),
        28  => memory::sys_madvise(arg1, arg2, arg3),
        32  => fs::sys_dup(arg1),
        33  => fs::sys_dup2(arg1, arg2),
        34  => multiplex::sys_pause(),
        35  => misc::sys_nanosleep(arg1, arg2),
        39  => sc_process::sys_getpid(),
        41  => network::sys_socket(arg1, arg2, arg3),
        42  => network::sys_connect(arg1, arg2, arg3),
        43  => network::sys_accept(arg1, arg2, arg3),
        44  => network::sys_sendto(arg1, arg2, arg3, arg4, arg5, arg6),
        45  => network::sys_recvfrom(arg1, arg2, arg3, arg4, arg5, arg6),
        46  => network::sys_sendmsg(arg1, arg2, arg3),
        47  => network::sys_recvmsg(arg1, arg2, arg3),
        48  => network::sys_shutdown(arg1, arg2),
        49  => network::sys_bind(arg1, arg2, arg3),
        50  => network::sys_listen(arg1, arg2),
        51  => network::sys_getsockname(arg1, arg2, arg3),
        52  => network::sys_getpeername(arg1, arg2, arg3),
        53  => network::sys_socketpair(arg1, arg2, arg3, arg4),
        54  => network::sys_setsockopt(arg1, arg2, arg3, arg4, arg5),
        55  => network::sys_getsockopt(arg1, arg2, arg3, arg4, arg5),
        56  => sc_process::sys_clone(arg1, arg2, arg3, arg4, arg5, context),
        57  => sc_process::sys_fork(context),
        58  => sc_process::sys_fork(context), // vfork
        59  => sc_process::sys_execve(arg1, arg2, arg3),
        60  => sc_process::sys_exit(arg1),
        61  => sc_process::sys_wait4_linux(arg1, arg2, arg3, arg4),
        62  => signals::sys_kill(arg1, arg2),
        63  => misc::sys_uname(arg1),
        72  => fs::sys_fcntl(arg1, arg2, arg3),
        73  => fs::sys_flock(arg1, arg2),
        74  => fs::sys_fsync(arg1),
        75  => fs::sys_fdatasync(arg1),
        76  => fs::sys_truncate(arg1, arg2),
        77  => fs::sys_ftruncate(arg1, arg2),
        78  => fs::sys_getdents64(arg1, arg2, arg3),
        79  => fs::sys_getcwd(arg1, arg2),
        80  => fs::sys_chdir(arg1),
        81  => fs::sys_fchdir(arg1),
        82  => fs::sys_rename(arg1, arg2),
        83  => fs::sys_mkdir(arg1, arg2),
        84  => fs::sys_rmdir(arg1),
        85  => fs::sys_creat(arg1, arg2),
        86  => fs::sys_link(arg1, arg2),
        87  => fs::sys_unlink(arg1),
        88  => fs::sys_symlink(arg1, arg2),
        89  => fs::sys_readlink(arg1, arg2, arg3),
        90  => fs::sys_chmod(arg1, arg2),
        91  => fs::sys_fchmod(arg1, arg2),
        92  => fs::sys_chown(arg1, arg2, arg3),
        93  => fs::sys_fchown(arg1, arg2, arg3),
        94  => fs::sys_lchown(arg1, arg2, arg3),
        95  => fs::sys_umask(arg1),
        96  => misc::sys_gettimeofday(arg1, arg2),
        97  => misc::sys_getrlimit(arg1, arg2),
        98  => misc::sys_getrusage(arg1, arg2),
        99  => misc::sys_sysinfo(arg1),
        100 => linux_abi_error(38), // times — no implementado
        101 => sc_process::sys_ptrace(arg1, arg2, arg3, arg4),
        102 => sc_process::sys_getuid(),
        103 => linux_abi_error(38), // syslog — no implementado
        104 => sc_process::sys_getgid(),
        105 => sc_process::sys_setuid(arg1),
        106 => sc_process::sys_setgid(arg1),
        107 => sc_process::sys_geteuid(),
        108 => sc_process::sys_getegid(),
        109 => sc_process::sys_setpgid(arg1, arg2),
        110 => sc_process::sys_getppid(),
        111 => sc_process::sys_getpgrp(),
        112 => sc_process::sys_setsid(),
        113 => sc_process::sys_setreuid(arg1, arg2),
        114 => sc_process::sys_setregid(arg1, arg2),
        117 => sc_process::sys_setresuid(arg1, arg2, arg3),
        118 => sc_process::sys_getresuid(arg1, arg2, arg3),
        119 => sc_process::sys_setresgid(arg1, arg2, arg3),
        120 => sc_process::sys_getresgid(arg1, arg2, arg3),
        121 => sc_process::sys_getpgid(arg1),
        127 => signals::sys_rt_sigpending(arg1, arg2),
        131 => signals::sys_sigaltstack(arg1, arg2),
        157 => sc_process::sys_prctl(arg1, arg2, arg3, arg4, arg5),
        158 => sc_process::sys_arch_prctl(arg1, arg2),
        162 => fs::sys_sync(),
        170 => misc::sys_sethostname(arg1, arg2),
        186 => sc_process::sys_gettid(),
        200 => signals::sys_tkill(arg1, arg2),
        202 => futex::sys_futex(arg1, arg2, arg3, arg4, arg5, arg6 as u32),
        204 => sc_process::sys_sched_getaffinity(arg1, arg2, arg3),
        217 => fs::sys_getdents64(arg1, arg2, arg3),
        218 => sc_process::sys_set_tid_address(arg1),
        228 => misc::sys_clock_gettime(arg1, arg2),
        230 => misc::sys_nanosleep(arg3, arg4),
        231 => sc_process::sys_exit(arg1),
        232 => multiplex::sys_epoll_wait(arg1, arg2, arg3, arg4),
        233 => multiplex::sys_epoll_ctl(arg1, arg2, arg3, arg4),
        247 => sc_process::sys_waitid(arg1, arg2, arg3, arg4, arg5),
        254 => multiplex::sys_inotify_add_watch(arg1, arg2, arg3),
        257 => fs::sys_openat(arg1, arg2, arg3, arg4),
        258 => fs::sys_mkdirat(arg1, arg2, arg3),
        262 => fs::sys_fstatat(arg1, arg2, arg3, arg4),
        269 => fs::sys_faccessat(arg1, arg2, arg3, arg4),
        270 => multiplex::sys_pselect6(arg1, arg2, arg3, arg4, arg5, arg6),
        271 => multiplex::sys_ppoll(arg1, arg2, arg3, arg4, arg5),
        282 => signals::sys_signalfd4(arg1, arg2, arg3, arg4),
        283 => multiplex::sys_timerfd_create(arg1, arg2),
        286 => multiplex::sys_timerfd_settime(arg1, arg2, arg3, arg4),
        287 => multiplex::sys_timerfd_gettime(arg1, arg2),
        290 => multiplex::sys_eventfd2(arg1, arg2),
        291 => multiplex::sys_epoll_create1(arg1),
        292 => fs::sys_dup3(arg1, arg2, arg3),
        293 => fs::sys_pipe2(arg1, arg2),
        294 => multiplex::sys_inotify_init1(arg1),
        295 => fs::sys_preadv(arg1, arg2, arg3, arg4),
        296 => fs::sys_pwritev(arg1, arg2, arg3, arg4),
        302 => misc::sys_prlimit64(arg1, arg2, arg3, arg4),
        314 => sc_process::sys_sched_set_deadline(arg1 as u32, arg2, arg3, arg4),
        316 => fs::sys_renameat2(arg1, arg2, arg3, arg4, arg5),
        318 => misc::sys_getrandom(arg1, arg2, arg3),
        319 => fs::sys_memfd_create(arg1, arg2),
        324 => misc::sys_membarrier(arg1, arg2, arg3),
        439 => fs::sys_faccessat(arg1, arg2, arg3, arg4),

        // --- Eclipse Extensions (500+) ---
        500 => ipc::sys_send(arg1, arg2, arg3, arg4),
        501 => ipc::sys_receive(arg1, arg2, arg3),
        502 => misc::sys_get_service_binary(arg1, arg2, arg3),
        503 => graphics::sys_get_framebuffer_info(arg1),
        504 => memory::sys_map_framebuffer(arg1, arg2),
        505 => misc::sys_pci_enum_devices(arg1, arg2, arg3),
        506 => misc::sys_pci_read_config(arg1, arg2, arg3),
        507 => misc::sys_pci_write_config(arg1, arg2, arg3, arg4),
        508 => misc::sys_register_device(arg1, arg2, arg3),
        509 => fs::sys_fmap(arg1, arg2, arg3),
        510 => fs::sys_mount(arg1, arg2),
        511 => sc_process::sys_spawn(arg1, arg2, arg3),
        512 => sc_process::sys_get_last_exec_error(arg1, arg2),
        513 => misc::sys_read_key(),
        514 => misc::sys_read_mouse_packet(),
        515 => graphics::sys_get_gpu_display_info(arg1),
        516 => graphics::sys_set_cursor_position(arg1, arg2),
        517 => graphics::sys_gpu_alloc_display_buffer(arg1, arg2, arg3),
        518 => graphics::sys_gpu_present(arg1, arg2, arg3, arg4, arg5),
        519 => misc::sys_get_logs(arg1, arg2),
        520 => misc::sys_get_storage_device_count(),
        521 => misc::sys_get_system_stats(arg1),
        522 => sc_process::sys_get_process_list(arg1, arg2),
        523 => sc_process::sys_set_process_name(arg1, arg2),
        524 => sc_process::sys_spawn_service(arg1, arg2, arg3),
        525 => graphics::sys_gpu_command(arg1, arg2, arg3),
        526 => misc::sys_stop_progress(),
        527 => graphics::sys_gpu_get_backend(),
        528 => graphics::sys_drm_page_flip(arg1, arg2, arg3, arg4, arg5),
        529 => graphics::sys_drm_get_caps(arg1, arg2),
        530 => graphics::sys_drm_alloc_buffer(arg1, arg2, arg3, arg4),
        531 => graphics::sys_drm_create_fb(arg1, arg2, arg3, arg4, arg5, arg6),
        532 => graphics::sys_drm_map_handle(arg1, arg2),
        533 => sc_process::sys_sched_setaffinity(arg1, arg2),
        534 => misc::sys_register_log_hud(arg1),
        535 => misc::sys_set_time(arg1),
        536 => sc_process::sys_spawn_with_stdio(arg1, arg2, arg3, arg4, arg5, arg6),
        537 => sc_process::sys_thread_create(arg1, arg2, arg3, context),
        538 => sc_process::sys_wait_pid(arg1, arg2, arg3),
        539 => fs::sys_readdir(arg1, arg2, arg3),
        542 => sc_process::sys_spawn_with_stdio_args(arg1, arg2, arg3, arg4, arg5, arg6, context),
        543 => sc_process::sys_get_process_args(arg1, arg2),
        544 => sc_process::sys_spawn_with_stdio_path(arg1, arg2, arg3, arg4, arg5, arg6),
        545 => sc_process::sys_strace(arg1, arg2),
        546 => sc_process::sys_exec(arg1, arg2),

        570 => graphics::sys_virgl_ctx_create(arg1, arg2, arg3),
        571 => graphics::sys_virgl_ctx_destroy(arg1),
        572 => graphics::sys_virgl_ctx_attach_resource(arg1, arg2),
        573 => graphics::sys_virgl_ctx_detach_resource(arg1, arg2),
        574 => graphics::sys_virgl_alloc_backing(arg1, arg2),
        575 => graphics::sys_virgl_resource_attach_backing(arg1, arg2, arg3),
        576 => graphics::sys_virgl_submit_3d(arg1, arg2, arg3),

        600 => ipc::sys_receive_fast(context),

        _ => {
            let cpu = crate::process::get_cpu_id();
            if num < 500 {
                crate::serial::serial_printf(format_args!(
                    "[SYSCALL] Unknown syscall: {} (Linux range) from pid {} on CPU {}\n",
                    num, pid, cpu
                ));
                linux_abi_error(38)
            } else {
                crate::serial::serial_printf(format_args!(
                    "[SYSCALL] Unknown syscall: {} (Eclipse range) from pid {} on CPU {}\n",
                    num, pid, cpu
                ));
                u64::MAX
            }
        }
    };

    context.rax = result;

    // No entregar señales en la vuelta de exit / exit_group (el proceso termina).
    if num != 60 && num != 231 {
        signals::deliver_pending_signals_for_current(context);
    }

    result
}

/// Convierte `errno` Linux (1..4095) a valor de retorno en RAX (`-errno` como unsigned).
#[inline]
pub fn linux_abi_error(errno: i32) -> u64 {
    if errno <= 0 || errno >= 4096 {
        u64::MAX
    } else {
        (errno.wrapping_neg()) as u64
    }
}

/// Duerme al proceso actual ~`ms` milisegundos (delegado en el scheduler).
pub fn process_sleep_ms(ms: u64) {
    crate::scheduler::sleep(ms);
}

/// Offset de tiempo de pared: `Unix_sec ≈ WALL_TIME_OFFSET + interrupts::ticks()/1000`.
pub static WALL_TIME_OFFSET: AtomicU64 = AtomicU64::new(0);

pub fn linux_makedev(major: u32, minor: u32) -> u64 {
    ((minor as u64 & 0xff) << 0) | ((major as u64 & 0xfff) << 8) | ((minor as u64 & !0xff) << 12) | ((major as u64 & !0xfff) << 32)
}

pub fn is_user_pointer(ptr: u64, len: u64) -> bool {
    if len == 0 { return true; }
    // Reject NULL and the first page to catch null pointer dereferences
    if ptr < 4096 { return false; }
    if ptr >= 0x0000_8000_0000_0000 { return false; }
    if ptr.checked_add(len).map_or(true, |end| end >= 0x0000_8000_0000_0000) { return false; }
    true
}

pub fn copy_to_user(user_ptr: u64, src: &[u8]) -> bool {
    if !is_user_pointer(user_ptr, src.len() as u64) { return false; }
    // Recuperación ante #PF: si userspace apunta a una página no mapeada,
    // devolvemos `false` en vez de matar al kernel.
    if unsafe { !crate::interrupts::set_recovery_point() } {
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), user_ptr as *mut u8, src.len());
        }
        unsafe { crate::interrupts::clear_recovery_point() };
        true
    } else {
        // set_recovery_point devolvió "estoy recuperando de un fault"
        unsafe { crate::interrupts::clear_recovery_point() };
        false
    }
}

pub fn copy_from_user(user_ptr: u64, dest: &mut [u8]) -> bool {
    if !is_user_pointer(user_ptr, dest.len() as u64) { return false; }
    // Recuperación ante #PF: evita panics al leer memoria de userspace inválida.
    if unsafe { !crate::interrupts::set_recovery_point() } {
        unsafe {
            core::ptr::copy_nonoverlapping(user_ptr as *const u8, dest.as_mut_ptr(), dest.len());
        }
        unsafe { crate::interrupts::clear_recovery_point() };
        true
    } else {
        unsafe { crate::interrupts::clear_recovery_point() };
        false
    }
}

pub fn strlen_user_unique(user_ptr: u64, max_len: usize) -> usize {
    for i in 0..max_len {
        if !is_user_pointer(user_ptr + i as u64, 1) { return i; }
        let c = unsafe { *( (user_ptr + i as u64) as *const u8 ) };
        if c == 0 { return i; }
    }
    max_len
}

pub fn set_fs_base(addr: u64) {
    unsafe {
        crate::cpu::wrmsr(0xC0000100, addr); // FS_BASE
    }
}
