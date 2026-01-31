//! Syscall wrappers para Eclipse OS
use core::arch::asm;

pub const SYS_EXIT: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_READ: u64 = 2;
pub const SYS_SEND: u64 = 3;
pub const SYS_RECEIVE: u64 = 4;
pub const SYS_YIELD: u64 = 5;
pub const SYS_GETPID: u64 = 6;

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
