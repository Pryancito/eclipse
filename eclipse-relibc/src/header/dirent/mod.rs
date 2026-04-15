//! dirent.h - Directory operations
use crate::types::*;
use crate::internal_alloc::{malloc, free};

pub const DT_UNKNOWN: c_uchar = 0;
pub const DT_DIR:     c_uchar = 4;
pub const DT_REG:     c_uchar = 8;
pub const DT_LNK:     c_uchar = 10;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct dirent {
    pub d_ino: ino_t,
    pub d_off: off_t,
    pub d_reclen: c_ushort,
    pub d_type: c_uchar,
    pub d_name: [c_char; 256],
}

/// Internal DIR state: holds the listing buffer returned by SYS_READDIR.
pub struct DIR {
    /// Copy of the path so we can stat entries for d_type.
    path: [u8; 1024],
    path_len: usize,
    /// Heap-allocated newline-separated listing from SYS_READDIR.
    buf: *mut u8,
    buf_len: usize,
    /// Current byte offset into `buf`.
    pos: usize,
    /// Heap-allocated dirent returned by readdir().
    current: *mut dirent,
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn opendir(name: *const c_char) -> *mut DIR {
    if name.is_null() {
        *crate::header::errno::__errno_location() = 22; // EINVAL
        return core::ptr::null_mut();
    }
    let name_str = core::ffi::CStr::from_ptr(name).to_str().unwrap_or("");

    // Allocate a buffer for the listing (up to 64 KiB).
    const LIST_BUF_SIZE: usize = 65536;
    let buf = malloc(LIST_BUF_SIZE) as *mut u8;
    if buf.is_null() {
        *crate::header::errno::__errno_location() = 12; // ENOMEM
        return core::ptr::null_mut();
    }
    let bytes_written = match crate::eclipse_syscall::call::readdir(name_str, core::slice::from_raw_parts_mut(buf, LIST_BUF_SIZE)) {
        Ok(n) => n,
        Err(e) => {
            free(buf as *mut c_void);
            *crate::header::errno::__errno_location() = e.errno as c_int;
            return core::ptr::null_mut();
        }
    };

    let current = malloc(core::mem::size_of::<dirent>()) as *mut dirent;
    if current.is_null() {
        free(buf as *mut c_void);
        *crate::header::errno::__errno_location() = 12;
        return core::ptr::null_mut();
    }

    let dir = malloc(core::mem::size_of::<DIR>()) as *mut DIR;
    if dir.is_null() {
        free(buf as *mut c_void);
        free(current as *mut c_void);
        *crate::header::errno::__errno_location() = 12;
        return core::ptr::null_mut();
    }

    let dir_ref = &mut *dir;
    // Store path.
    let plen = name_str.len().min(1023);
    dir_ref.path[..plen].copy_from_slice(&name_str.as_bytes()[..plen]);
    dir_ref.path[plen] = 0;
    dir_ref.path_len = plen;
    dir_ref.buf = buf;
    dir_ref.buf_len = bytes_written;
    dir_ref.pos = 0;
    dir_ref.current = current;

    dir
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn readdir(dirp: *mut DIR) -> *mut dirent {
    if dirp.is_null() { return core::ptr::null_mut(); }
    let dir = &mut *dirp;

    // Skip past any leading newlines.
    while dir.pos < dir.buf_len && *dir.buf.add(dir.pos) == b'\n' {
        dir.pos += 1;
    }
    if dir.pos >= dir.buf_len { return core::ptr::null_mut(); }

    // Read until '\n' or end of buffer.
    let start = dir.pos;
    while dir.pos < dir.buf_len && *dir.buf.add(dir.pos) != b'\n' {
        dir.pos += 1;
    }
    let entry_len = dir.pos - start;
    if entry_len == 0 { return core::ptr::null_mut(); }
    // Skip the newline.
    if dir.pos < dir.buf_len { dir.pos += 1; }

    // Copy name into dirent.
    let ent = &mut *dir.current;
    core::ptr::write_bytes(ent as *mut dirent, 0, 1);
    let copy_len = entry_len.min(255);
    core::ptr::copy_nonoverlapping(
        dir.buf.add(start) as *const c_char,
        ent.d_name.as_mut_ptr(),
        copy_len,
    );
    ent.d_name[copy_len] = 0;
    ent.d_reclen = core::mem::size_of::<dirent>() as c_ushort;
    ent.d_ino = 1; // Placeholder inode
    ent.d_type = DT_UNKNOWN;

    // Try to stat the entry to determine d_type.
    let name_slice = core::slice::from_raw_parts(ent.d_name.as_ptr() as *const u8, copy_len);
    if let Ok(name_str) = core::str::from_utf8(name_slice) {
        // Build full path: dir_path + "/" + name
        let plen = dir.path_len;
        let full_len = plen + 1 + copy_len;
        if full_len < 1020 {
            let mut full_path = [0u8; 1024];
            full_path[..plen].copy_from_slice(&dir.path[..plen]);
            full_path[plen] = b'/';
            full_path[plen+1..plen+1+copy_len].copy_from_slice(name_str.as_bytes());
            full_path[full_len] = 0;
            if let Ok(full_str) = core::str::from_utf8(&full_path[..full_len]) {
                let mut st = crate::eclipse_syscall::call::Stat::default();
                if crate::eclipse_syscall::call::fstat_at(0, full_str, &mut st, 0).is_ok() {
                    // mode bits: 0o0040000 = dir, 0o0100000 = regular file
                    let fmt = st.mode & 0o170000;
                    ent.d_type = if fmt == 0o040000 { DT_DIR }
                                 else if fmt == 0o100000 { DT_REG }
                                 else if fmt == 0o120000 { DT_LNK }
                                 else { DT_UNKNOWN };
                    ent.d_ino = st.ino as ino_t;
                }
            }
        }
    }

    dir.current
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn closedir(dirp: *mut DIR) -> c_int {
    if dirp.is_null() { return -1; }
    let dir = &*dirp;
    if !dir.buf.is_null() { free(dir.buf as *mut c_void); }
    if !dir.current.is_null() { free(dir.current as *mut c_void); }
    free(dirp as *mut c_void);
    0
}

/// rewinddir — reset directory stream to the beginning.
#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn rewinddir(dirp: *mut DIR) {
    if !dirp.is_null() {
        (*dirp).pos = 0;
    }
}

/// dirfd — return fd associated with DIR (Eclipse has none; return -1).
#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn dirfd(_dirp: *mut DIR) -> c_int {
    -1
}
