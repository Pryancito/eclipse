//! Syscall wrappers para Eclipse OS
use core::arch::asm;

pub const SYS_EXIT: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_READ: u64 = 2;
pub const SYS_SEND: u64 = 3;
pub const SYS_RECEIVE: u64 = 4;
pub const SYS_YIELD: u64 = 5;
pub const SYS_GETPID: u64 = 6;
pub const SYS_FORK: u64 = 7;
pub const SYS_EXEC: u64 = 8;
pub const SYS_WAIT: u64 = 9;
pub const SYS_GET_SERVICE_BINARY: u64 = 10;
pub const SYS_OPEN: u64 = 11;
pub const SYS_CLOSE: u64 = 12;
pub const SYS_GETPPID: u64 = 13;

// File open flags
pub const O_RDONLY: i32 = 0x0000;
pub const O_WRONLY: i32 = 0x0001;
pub const O_RDWR: i32 = 0x0002;
pub const O_CREAT: i32 = 0x0040;
pub const O_TRUNC: i32 = 0x0200;
pub const O_APPEND: i32 = 0x0400;

#[inline(always)]
unsafe fn syscall0(n: u64) -> u64 {
    let ret: u64;
    asm!("int 0x80", in("rax") n, lateout("rax") ret, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall1(n: u64, arg1: u64) -> u64 {
    let ret: u64;
    asm!("int 0x80", in("rax") n, in("rdi") arg1, lateout("rax") ret, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall2(n: u64, arg1: u64, arg2: u64) -> u64 {
    let ret: u64;
    asm!("int 0x80", in("rax") n, in("rdi") arg1, in("rsi") arg2, lateout("rax") ret, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall3(n: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    let ret: u64;
    asm!("int 0x80", in("rax") n, in("rdi") arg1, in("rsi") arg2, in("rdx") arg3, lateout("rax") ret, options(nostack));
    ret
}

pub fn exit(code: i32) -> ! {
    unsafe { syscall1(SYS_EXIT, code as u64); }
    loop {}
}

pub fn write(fd: u32, buf: &[u8]) -> isize {
    unsafe { syscall3(SYS_WRITE, fd as u64, buf.as_ptr() as u64, buf.len() as u64) as isize }
}

pub fn read(fd: u32, buf: &mut [u8]) -> isize {
    unsafe { syscall3(SYS_READ, fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64) as isize }
}

pub fn yield_cpu() {
    unsafe { syscall0(SYS_YIELD); }
}

pub fn getpid() -> u32 {
    unsafe { syscall0(SYS_GETPID) as u32 }
}

pub fn getppid() -> u32 {
    unsafe { syscall0(SYS_GETPPID) as u32 }
}

pub fn fork() -> i32 {
    let pid = unsafe { syscall0(SYS_FORK) as i32 };
    // DEBUG: Print what fork() returned
    unsafe {
        let msg = if pid == 0 {
            "[LIBC] fork() returned 0 (child)\n"
        } else if pid > 0 {
            "[LIBC] fork() returned positive (parent)\n"
        } else {
            "[LIBC] fork() returned negative (error)\n"
        };
        syscall3(SYS_WRITE, 1, msg.as_ptr() as u64, msg.len() as u64);
        
        // Also print the actual PID value
        let prefix = "[LIBC] fork() return value: ";
        syscall3(SYS_WRITE, 1, prefix.as_ptr() as u64, prefix.len() as u64);
        
        // Convert PID to string and print (simple approach)
        let mut buf = [0u8; 20];
        let mut n = if pid < 0 { -pid } else { pid };
        let mut i = 0;
        if n == 0 {
            buf[0] = b'0';
            i = 1;
        } else {
            while n > 0 {
                buf[i] = b'0' + (n % 10) as u8;
                n /= 10;
                i += 1;
            }
        }
        // Reverse
        for j in 0..i/2 {
            buf.swap(j, i-1-j);
        }
        if pid < 0 {
            syscall3(SYS_WRITE, 1, b"-".as_ptr() as u64, 1);
        }
        syscall3(SYS_WRITE, 1, buf.as_ptr() as u64, i as u64);
        syscall3(SYS_WRITE, 1, b"\n".as_ptr() as u64, 1);
    }
    pid
}

pub fn exec(elf_buffer: &[u8]) -> i32 {
    unsafe { syscall2(SYS_EXEC, elf_buffer.as_ptr() as u64, elf_buffer.len() as u64) as i32 }
}

pub fn wait(status: Option<&mut i32>) -> i32 {
    let status_ptr = match status {
        Some(s) => s as *mut i32 as u64,
        None => 0,
    };
    unsafe { syscall1(SYS_WAIT, status_ptr) as i32 }
}

/// Get service binary by ID
/// Returns (pointer, size) or (0, 0) on error
pub fn get_service_binary(service_id: u32) -> (*const u8, usize) {
    let mut ptr: u64 = 0;
    let mut size: u64 = 0;
    
    let result = unsafe {
        syscall3(
            SYS_GET_SERVICE_BINARY,
            service_id as u64,
            &mut ptr as *mut u64 as u64,
            &mut size as *mut u64 as u64
        )
    };
    
    if result == 0 {
        (ptr as *const u8, size as usize)
    } else {
        (core::ptr::null(), 0)
    }
}

/// Open a file
/// Returns file descriptor on success, -1 on error
pub fn open(path: &str, flags: i32, _mode: i32) -> i32 {
    unsafe {
        syscall3(
            SYS_OPEN,
            path.as_ptr() as u64,
            path.len() as u64,
            flags as u64
        ) as i32
    }
}

/// Close a file descriptor
/// Returns 0 on success, -1 on error
pub fn close(fd: i32) -> i32 {
    unsafe {
        syscall1(SYS_CLOSE, fd as u64) as i32
    }
}
/// Send a message to a server
/// Returns 0 on success, -1 on error
pub fn send(server_id: u32, msg_type: u32, data: &[u8]) -> i32 {
    unsafe {
        syscall3(
            SYS_SEND,
            server_id as u64,
            msg_type as u64,
            data.as_ptr() as u64
        ) as i32
    }
}

/// Receive a message
/// Returns (length, sender_pid) or (0, 0) if no message
pub fn receive(buffer: &mut [u8]) -> (usize, u32) {
    let mut sender_pid: u64 = 0;
    
    let result = unsafe {
        syscall3(
            SYS_RECEIVE,
            buffer.as_mut_ptr() as u64,
            buffer.len() as u64,
            &mut sender_pid as *mut u64 as u64
        )
    };
    
    if result > 0 {
        (result as usize, sender_pid as u32)
    } else {
        (0, 0)
    }
}
