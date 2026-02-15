//! stdio.h - Standard I/O
use crate::types::*;
use eclipse_syscall::call::{write as sys_write, read as sys_read, close as sys_close};
use eclipse_syscall::flag::{O_RDONLY, O_WRONLY, O_CREAT, O_TRUNC, O_APPEND};
use eclipse_syscall::number::{SYS_OPEN, SYS_LSEEK};
use core::ptr;

pub const EOF: c_int = -1;

pub const BUFSIZ: usize = 1024;
const MODE_READ: c_int = 1;
const MODE_WRITE: c_int = 2;
const MODE_APPEND: c_int = 4;

#[cfg(not(any(target_os = "none", target_os = "linux", eclipse_target)))]
pub type FILE = c_void;

#[cfg(any(target_os = "none", target_os = "linux", eclipse_target))]
#[repr(C)]
pub struct FILE {
    fd: c_int,
    flags: c_int,
    buffer: *mut u8,
    buf_pos: usize,
    buf_size: usize,
    buf_capacity: usize,
}

#[cfg(any(target_os = "none", target_os = "linux", eclipse_target))]
static mut STDIN_STRUCT: FILE = FILE {
    fd: 0, flags: MODE_READ,
    buffer: ptr::null_mut(), buf_pos: 0, buf_size: 0, buf_capacity: 0,
};

#[cfg(any(target_os = "none", target_os = "linux", eclipse_target))]
static mut STDOUT_STRUCT: FILE = FILE {
    fd: 1, flags: MODE_WRITE,
    buffer: ptr::null_mut(), buf_pos: 0, buf_size: 0, buf_capacity: 0,
};

#[cfg(any(target_os = "none", target_os = "linux", eclipse_target))]
static mut STDERR_STRUCT: FILE = FILE {
    fd: 2, flags: MODE_WRITE,
    buffer: ptr::null_mut(), buf_pos: 0, buf_size: 0, buf_capacity: 0,
};

#[no_mangle]
pub static mut stdin: *mut FILE = &raw mut STDIN_STRUCT as *mut FILE;

#[cfg(any(target_os = "none", target_os = "linux", eclipse_target))]
#[no_mangle]
pub static mut stdout: *mut FILE = &raw mut STDOUT_STRUCT as *mut FILE;

#[cfg(any(target_os = "none", target_os = "linux", eclipse_target))]
#[no_mangle]
pub static mut stderr: *mut FILE = &raw mut STDERR_STRUCT as *mut FILE;

#[cfg(not(any(target_os = "none", target_os = "linux", eclipse_target)))]
extern "C" {
    pub static mut stdin: *mut FILE;
    pub static mut stdout: *mut FILE;
    pub static mut stderr: *mut FILE;
}

#[cfg(not(any(target_os = "none", target_os = "linux", eclipse_target)))]
mod host {
    use super::*;
    
    extern "C" {
        pub fn fopen(pathname: *const c_char, mode: *const c_char) -> *mut FILE;
        pub fn fclose(stream: *mut FILE) -> c_int;
        pub fn fflush(stream: *mut FILE) -> c_int;
        pub fn fwrite(ptr: *const c_void, size: size_t, nmemb: size_t, stream: *mut FILE) -> size_t;
        pub fn fread(ptr: *mut c_void, size: size_t, nmemb: size_t, stream: *mut FILE) -> size_t;
        pub fn putchar(c: c_int) -> c_int;
        pub fn fputc(c: c_int, stream: *mut FILE) -> c_int;
        pub fn puts(s: *const c_char) -> c_int;
        pub fn fputs(s: *const c_char, stream: *mut FILE) -> c_int;
        pub fn fseek(stream: *mut FILE, offset: c_long, whence: c_int) -> c_int;
        pub fn ftell(stream: *mut FILE) -> c_long;
        pub fn rewind(stream: *mut FILE);
        pub fn feof(stream: *mut FILE) -> c_int;
        pub fn ferror(stream: *mut FILE) -> c_int;
        pub fn clearerr(stream: *mut FILE);
        pub fn remove(pathname: *const c_char) -> c_int;
        pub fn rename(oldpath: *const c_char, newpath: *const c_char) -> c_int;
    }
}

#[cfg(not(any(target_os = "none", target_os = "linux", eclipse_target)))]
pub use self::host::*;

#[cfg(any(target_os = "none", target_os = "linux", eclipse_target))]
mod target {
    use super::*;
    use crate::c_str::strlen;
    use crate::header::string::{strcpy, strncpy};

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
    pub unsafe extern "C" fn fdopen(fd: c_int, mode: *const c_char) -> *mut FILE {
        let (flags, _open_flags) = parse_mode(mode);
        if flags == 0 {
            return ptr::null_mut();
        }

        let file = crate::header::stdlib::malloc(core::mem::size_of::<FILE>()) as *mut FILE;
        if file.is_null() {
            return ptr::null_mut();
        }

        let buffer = crate::header::stdlib::malloc(BUFSIZ) as *mut u8;
        if buffer.is_null() {
            crate::header::stdlib::free(file as *mut c_void);
            return ptr::null_mut();
        }

        (*file).fd = fd;
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
    pub unsafe extern "C" fn fileno(stream: *mut FILE) -> c_int {
        if stream.is_null() {
            return -1;
        }
        (*stream).fd
    }

    #[no_mangle]
    pub unsafe extern "C" fn ungetc(c: c_int, stream: *mut FILE) -> c_int {
        // Simple stub: usually returns c on success, or EOF.
        // We'll just return c (pretend success) or EOF if stream is null.
        if stream.is_null() {
            return EOF;
        }
        // Ideally enforce pushback buffer logic, but for stub just ignore
        c
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
        let len = strlen(s);
        let slice = core::slice::from_raw_parts(s as *const u8, len);
        
        if fwrite(slice.as_ptr() as *const c_void, 1, len, stream) != len {
            -1
        } else {
            0
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn fseek(stream: *mut FILE, offset: c_long, whence: c_int) -> c_int {
        if stream.is_null() {
            return -1;
        }
        
        // Flush write buffer if needed
        if (*stream).flags & MODE_WRITE != 0 {
            fflush(stream);
        }
        
        // Call lseek syscall
        let result = eclipse_syscall::syscall3(
            SYS_LSEEK,
            (*stream).fd as usize,
            offset as usize,
            whence as usize
        );
        
        if result as isize == -1 {
            return -1;
        }
        
        // Clear read buffer
        (*stream).buf_pos = 0;
        (*stream).buf_size = 0;
        
        0
    }

    #[no_mangle]
    pub unsafe extern "C" fn ftell(stream: *mut FILE) -> c_long {
        if stream.is_null() {
            return -1;
        }
        
        // Get current position
        let result = eclipse_syscall::syscall3(
            SYS_LSEEK,
            (*stream).fd as usize,
            0,
            SEEK_CUR as usize
        );
        
        if result as isize == -1 {
            return -1;
        }
        
        result as c_long
    }

    #[no_mangle]
    pub unsafe extern "C" fn rewind(stream: *mut FILE) {
        fseek(stream, 0, SEEK_SET as c_int);
    }

    #[no_mangle]
    pub unsafe extern "C" fn ferror(stream: *mut FILE) -> c_int {
        if stream.is_null() {
            return 1;
        }
        0
    }

    #[no_mangle]
    pub unsafe extern "C" fn feof(stream: *mut FILE) -> c_int {
        if stream.is_null() {
            return 1;
        }
        // Simple implementation
        0
    }

    #[no_mangle]
    pub unsafe extern "C" fn clearerr(stream: *mut FILE) {
        if !stream.is_null() {
            // Would clear error and EOF flags
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn remove(_pathname: *const c_char) -> c_int {
        -1  // TODO: Implement SYS_UNLINK
    }

    #[no_mangle]
    pub unsafe extern "C" fn rename(_oldpath: *const c_char, _newpath: *const c_char) -> c_int {
        -1  // TODO: Implement SYS_RENAME
    }

    #[no_mangle]
    pub unsafe extern "C" fn getc(stream: *mut FILE) -> c_int {
        fgetc(stream)
    }

    #[no_mangle]
    pub unsafe extern "C" fn fgetc(stream: *mut FILE) -> c_int {
        let mut c: u8 = 0;
        if fread(&mut c as *mut u8 as *mut c_void, 1, 1, stream) == 1 {
            c as c_int
        } else {
            EOF
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn fgets(s: *mut c_char, size: c_int, stream: *mut FILE) -> *mut c_char {
        if size <= 0 || s.is_null() || stream.is_null() {
            return core::ptr::null_mut();
        }

        let mut i = 0;
        while i < size - 1 {
            let c = fgetc(stream);
            if c == EOF {
                if i == 0 {
                    return core::ptr::null_mut();
                }
                break;
            }
            *s.add(i as usize) = c as c_char;
            i += 1;
            if c == b'\n' as c_int {
                break;
            }
        }
        *s.add(i as usize) = 0;
        s
    }

    fn int_to_str(i: i64, buf: &mut [u8], base: u8) -> &[u8] {
        if i == 0 {
            buf[0] = b'0';
            return &buf[0..1];
        }
        let negative = i < 0;
        let mut u = if negative { -i as u64 } else { i as u64 };
        let mut pos = buf.len();
        while u > 0 {
            pos -= 1;
            let digit = (u % base as u64) as u8;
            buf[pos] = if digit < 10 { b'0' + digit } else { b'a' + digit - 10 };
            u /= base as u64;
        }
        if negative {
            pos -= 1;
            buf[pos] = b'-';
        }
        &buf[pos..]
    }

    fn uint_to_str(mut i: u64, buf: &mut [u8], base: u8) -> &[u8] {
        if i == 0 {
            buf[0] = b'0';
            return &buf[0..1];
        }
        let mut pos = buf.len();
        while i > 0 {
            pos -= 1;
            let digit = (i % base as u64) as u8;
            buf[pos] = if digit < 10 { b'0' + digit } else { b'a' + digit - 10 };
            i /= base as u64;
        }
        &buf[pos..]
    }

    trait Writer {
        fn write(&mut self, data: &[u8]);
    }

    struct BufferWriter {
        ptr: *mut u8,
        capacity: usize,
        pos: usize,
        count: usize,
    }

    impl Writer for BufferWriter {
        fn write(&mut self, data: &[u8]) {
            for &b in data {
                if self.pos + 1 < self.capacity {
                    unsafe { *self.ptr.add(self.pos) = b; }
                    self.pos += 1;
                }
                self.count += 1;
            }
        }
    }

    struct CountingWriter {
        count: usize,
    }

    impl Writer for CountingWriter {
        fn write(&mut self, data: &[u8]) {
            self.count += data.len();
        }
    }

    struct FprintfWriter {
        stream: *mut FILE,
        count: usize,
    }

    impl Writer for FprintfWriter {
        fn write(&mut self, data: &[u8]) {
            unsafe {
                fwrite(data.as_ptr() as *const c_void, 1, data.len(), self.stream);
            }
            self.count += data.len();
        }
    }

    unsafe fn vformat<W: Writer>(writer: &mut W, format: *const c_char, mut ap: core::ffi::VaList) {
        let mut p = format;
        while *p != 0 {
            if *p == b'%' as c_char {
                p = p.add(1);
                
                // Skip length modifiers for now
                if *p == b'l' as c_char || *p == b'z' as c_char {
                    p = p.add(1);
                    if *p == b'l' as c_char { // Handle ll
                        p = p.add(1);
                    }
                }

                match *p as u8 {
                    b'%' => { writer.write(b"%"); }
                    b's' => {
                        let s = ap.arg::<*const c_char>();
                        if s.is_null() {
                            writer.write(b"(null)");
                        } else {
                            let len = strlen(s);
                            let slice = core::slice::from_raw_parts(s as *const u8, len);
                            writer.write(slice);
                        }
                    }
                    b'd' | b'i' => {
                        let d = ap.arg::<c_int>();
                        let mut buf = [0u8; 32];
                        let res = int_to_str(d as i64, &mut buf, 10);
                        writer.write(res);
                    }
                    b'u' => {
                        let u = ap.arg::<c_uint>();
                        let mut buf = [0u8; 32];
                        let res = uint_to_str(u as u64, &mut buf, 10);
                        writer.write(res);
                    }
                    b'x' | b'X' => { // Handle both %x and %X
                        let u = ap.arg::<c_uint>();
                        let mut buf = [0u8; 32];
                        let res = uint_to_str(u as u64, &mut buf, 16);
                        writer.write(res);
                    }
                    b'p' => {
                        let ptr = ap.arg::<*const c_void>();
                        let mut buf = [0u8; 32];
                        writer.write(b"0x");
                        let res = uint_to_str(ptr as u64, &mut buf, 16);
                        writer.write(res);
                    }
                    b'c' => {
                        let c = ap.arg::<c_int>();
                        writer.write(&[c as u8]);
                    }
                    _ => {
                        // If we don't recognize it, just print it as is (with a marker for debugging)
                        writer.write(b"%");
                        writer.write(&[*p as u8]);
                    }
                }
            } else {
                writer.write(&[*p as u8]);
            }
            p = p.add(1);
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn printf(format: *const c_char, args: ...) -> c_int {
        fputs(b"DEBUG_PRINTF\n\0".as_ptr() as *const c_char, stdout);
        vfprintf(stdout, format, args)
    }

    #[no_mangle]
    pub unsafe extern "C" fn fprintf(stream: *mut FILE, format: *const c_char, args: ...) -> c_int {
        vfprintf(stream, format, args)
    }

    #[no_mangle]
    pub unsafe extern "C" fn sprintf(s: *mut c_char, format: *const c_char, args: ...) -> c_int {
        vsprintf(s, format, args)
    }

    #[no_mangle]
    pub unsafe extern "C" fn snprintf(s: *mut c_char, n: size_t, format: *const c_char, args: ...) -> c_int {
        vsnprintf(s, n, format, args)
    }

    #[no_mangle]
    pub unsafe extern "C" fn vfprintf(stream: *mut FILE, format: *const c_char, ap: core::ffi::VaList) -> c_int {
        let mut writer = FprintfWriter { stream, count: 0 };
        vformat(&mut writer, format, ap);
        writer.count as c_int
    }

    #[no_mangle]
    pub unsafe extern "C" fn vsprintf(s: *mut c_char, format: *const c_char, ap: core::ffi::VaList) -> c_int {
        vsnprintf(s, usize::MAX, format, ap)
    }

    #[no_mangle]
    pub unsafe extern "C" fn vsnprintf(s: *mut c_char, n: size_t, format: *const c_char, ap: core::ffi::VaList) -> c_int {
        let mut writer = BufferWriter { ptr: s as *mut u8, capacity: n, pos: 0, count: 0 };
        vformat(&mut writer, format, ap);
        if n > 0 {
            *s.add(writer.pos) = 0;
        }
        writer.count as c_int
    }

    #[no_mangle]
    pub unsafe extern "C" fn vasprintf(
        strp: *mut *mut c_char, 
        format: *const c_char, 
        ap: core::ffi::VaList
    ) -> c_int {
        if strp.is_null() { return -1; }

        let mut v = ::alloc::vec::Vec::new();
        {
            struct VecWriter<'a>(&'a mut ::alloc::vec::Vec<u8>);
            impl<'a> Writer for VecWriter<'a> {
                fn write(&mut self, data: &[u8]) {
                    self.0.extend_from_slice(data);
                }
            }
            let mut vw = VecWriter(&mut v);
            // Asegúrate de que vformat sea compatible con la ABI de C para variadics
            vformat(&mut vw, format, ap);
        }

        let len = v.len();
        // CRITICAL: Usa el malloc que Xfbdev espera (el de tu Libc exportada)
        let buf = crate::internal_alloc::malloc(len + 1) as *mut c_char;
        
        if buf.is_null() { 
            *strp = core::ptr::null_mut();
            return -1; 
        }

        ptr::copy_nonoverlapping(v.as_ptr(), buf as *mut u8, len);
        *buf.add(len) = 0; // Null terminator
        
        *strp = buf;
        len as c_int
    }

    #[no_mangle]
    pub unsafe extern "C" fn asprintf(strp: *mut *mut c_char, format: *const c_char, args: ...) -> c_int {
        vasprintf(strp, format, args)
    }

    #[no_mangle]
    pub unsafe extern "C" fn fscanf(_stream: *mut FILE, _format: *const c_char, _: ...) -> c_int {
        0
    }

    #[no_mangle]
    pub unsafe extern "C" fn scanf(_format: *const c_char, _: ...) -> c_int {
        0
    }

    #[no_mangle]
    pub unsafe extern "C" fn sscanf(_str: *const c_char, _format: *const c_char, _: ...) -> c_int {
        0
    }
}

#[cfg(any(target_os = "none", target_os = "linux", eclipse_target))]
pub use self::target::*;

#[no_mangle]
pub unsafe extern "C" fn setlinebuf(_stream: *mut FILE) {
    // Stub
}
