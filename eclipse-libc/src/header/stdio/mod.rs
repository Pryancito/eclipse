//! stdio.h - Standard I/O
use crate::types::*;
use eclipse_syscall::call::{write as sys_write, read as sys_read, close as sys_close};
use eclipse_syscall::flag::*;
use eclipse_syscall::number::SYS_OPEN;
use core::ptr;

const BUFSIZ: usize = 8192;
const MODE_READ: c_int = 1;
const MODE_WRITE: c_int = 2;
const MODE_APPEND: c_int = 4;

#[repr(C)]
pub struct FILE {
    fd: c_int,
    flags: c_int,
    buffer: *mut u8,
    buf_pos: usize,
    buf_size: usize,
    buf_capacity: usize,
}

static mut STDIN_STRUCT: FILE = FILE {
    fd: 0, flags: MODE_READ,
    buffer: ptr::null_mut(), buf_pos: 0, buf_size: 0, buf_capacity: 0,
};

static mut STDOUT_STRUCT: FILE = FILE {
    fd: 1, flags: MODE_WRITE,
    buffer: ptr::null_mut(), buf_pos: 0, buf_size: 0, buf_capacity: 0,
};

static mut STDERR_STRUCT: FILE = FILE {
    fd: 2, flags: MODE_WRITE,
    buffer: ptr::null_mut(), buf_pos: 0, buf_size: 0, buf_capacity: 0,
};

#[no_mangle]
pub static mut stdin: *mut FILE = unsafe { &mut STDIN_STRUCT as *mut FILE };

#[no_mangle]
pub static mut stdout: *mut FILE = unsafe { &mut STDOUT_STRUCT as *mut FILE };

#[no_mangle]
pub static mut stderr: *mut FILE = unsafe { &mut STDERR_STRUCT as *mut FILE };

unsafe fn parse_mode(mode: *const c_char) -> (c_int, usize) {
    let p = mode;
    let file_flags;
    let open_flags;
    
    match *p as u8 {
        b'r' => { file_flags = MODE_READ; open_flags = O_RDONLY; }
        b'w' => { file_flags = MODE_WRITE; open_flags = O_WRONLY | O_CREAT | O_TRUNC; }
        b'a' => { file_flags = MODE_WRITE | MODE_APPEND; open_flags = O_WRONLY | O_CREAT | O_APPEND; }
        _ => return (0, 0),
    }
    
    (file_flags, open_flags)
}

#[no_mangle]
pub unsafe extern "C" fn fopen(pathname: *const c_char, mode: *const c_char) -> *mut FILE {
    let (flags, open_flags) = parse_mode(mode);
    if flags == 0 {
        return ptr::null_mut();
    }
    
    use crate::c_str::strlen;
    let path_len = strlen(pathname);
    let path_slice = core::slice::from_raw_parts(pathname as *const u8, path_len);
    
    let fd = eclipse_syscall::syscall3(
        SYS_OPEN, path_slice.as_ptr() as usize, path_slice.len(), open_flags
    );
    
    if fd == usize::MAX {
        return ptr::null_mut();
    }
    
    let file = crate::header::stdlib::malloc(core::mem::size_of::<FILE>()) as *mut FILE;
    if file.is_null() {
        let _ = sys_close(fd);
        return ptr::null_mut();
    }
    
    let buffer = crate::header::stdlib::malloc(BUFSIZ) as *mut u8;
    if buffer.is_null() {
        crate::header::stdlib::free(file as *mut c_void);
        let _ = sys_close(fd);
        return ptr::null_mut();
    }
    
    (*file).fd = fd as c_int;
    (*file).flags = flags;
    (*file).buffer = buffer;
    (*file).buf_pos = 0;
    (*file).buf_size = 0;
    (*file).buf_capacity = BUFSIZ;
    
    file
}

#[no_mangle]
pub unsafe extern "C" fn fclose(stream: *mut FILE) -> c_int {
    if stream.is_null() {
        return -1;
    }
    
    fflush(stream);
    
    let result = match sys_close((*stream).fd as usize) {
        Ok(_) => 0,
        Err(_) => -1,
    };
    
    if !(*stream).buffer.is_null() {
        crate::header::stdlib::free((*stream).buffer as *mut c_void);
    }
    crate::header::stdlib::free(stream as *mut c_void);
    
    result
}

#[no_mangle]
pub unsafe extern "C" fn fflush(stream: *mut FILE) -> c_int {
    if stream.is_null() {
        return 0;
    }
    
    if ((*stream).flags & MODE_WRITE) != 0 && (*stream).buf_pos > 0 {
        let buffer_slice = core::slice::from_raw_parts((*stream).buffer, (*stream).buf_pos);
        
        match sys_write((*stream).fd as usize, buffer_slice) {
            Ok(_) => {
                (*stream).buf_pos = 0;
                0
            }
            Err(_) => -1,
        }
    } else {
        0
    }
}

#[no_mangle]
pub unsafe extern "C" fn fwrite(
    ptr: *const c_void, size: size_t, nmemb: size_t, stream: *mut FILE,
) -> size_t {
    if stream.is_null() || ptr.is_null() {
        return 0;
    }
    
    let total_size = size * nmemb;
    let data = core::slice::from_raw_parts(ptr as *const u8, total_size);
    
    if (*stream).buffer.is_null() || total_size > (*stream).buf_capacity {
        return match sys_write((*stream).fd as usize, data) {
            Ok(n) => n / size,
            Err(_) => 0,
        };
    }
    
    let mut written = 0;
    while written < total_size {
        let remaining = total_size - written;
        let space = (*stream).buf_capacity - (*stream).buf_pos;
        let to_write = remaining.min(space);
        
        ptr::copy_nonoverlapping(
            data.as_ptr().add(written),
            (*stream).buffer.add((*stream).buf_pos),
            to_write
        );
        
        (*stream).buf_pos += to_write;
        written += to_write;
        
        if (*stream).buf_pos >= (*stream).buf_capacity {
            if fflush(stream) != 0 {
                return written / size;
            }
        }
    }
    
    nmemb
}

#[no_mangle]
pub unsafe extern "C" fn fread(
    ptr: *mut c_void, size: size_t, nmemb: size_t, stream: *mut FILE,
) -> size_t {
    if stream.is_null() || ptr.is_null() {
        return 0;
    }
    
    let total_size = size * nmemb;
    let data = core::slice::from_raw_parts_mut(ptr as *mut u8, total_size);
    
    match sys_read((*stream).fd as usize, data) {
        Ok(n) => n / size,
        Err(_) => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn putchar(c: c_int) -> c_int {
    fputc(c, stdout)
}

#[no_mangle]
pub unsafe extern "C" fn fputc(c: c_int, stream: *mut FILE) -> c_int {
    let ch = c as u8;
    if fwrite(&ch as *const u8 as *const c_void, 1, 1, stream) == 1 {
        c
    } else {
        -1
    }
}

#[no_mangle]
pub unsafe extern "C" fn puts(s: *const c_char) -> c_int {
    use crate::c_str::strlen;
    let len = strlen(s);
    let slice = core::slice::from_raw_parts(s as *const u8, len);
    
    if fwrite(slice.as_ptr() as *const c_void, 1, len, stdout) != len {
        return -1;
    }
    
    if fputc(b'\n' as c_int, stdout) == -1 {
        return -1;
    }
    
    fflush(stdout);
    0
}

#[no_mangle]
pub unsafe extern "C" fn fputs(s: *const c_char, stream: *mut FILE) -> c_int {
    use crate::c_str::strlen;
    let len = strlen(s);
    let slice = core::slice::from_raw_parts(s as *const u8, len);
    
    if fwrite(slice.as_ptr() as *const c_void, 1, len, stream) != len {
        -1
    } else {
        0
    }
}
