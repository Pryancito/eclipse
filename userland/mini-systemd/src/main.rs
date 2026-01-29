#![no_std]
#![no_main]

use core::panic::PanicInfo;

/// Syscall numbers for x86_64 Linux ABI
const SYS_WRITE: u64 = 1;
const SYS_EXIT: u64 = 60;

/// Write to file descriptor using syscall
#[inline(never)]
fn sys_write(fd: u64, buf: *const u8, count: u64) -> i64 {
    let ret: i64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") SYS_WRITE => ret,
            in("rdi") fd,
            in("rsi") buf,
            in("rdx") count,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

/// Exit process using syscall
#[inline(never)]
fn sys_exit(code: u64) -> ! {
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") SYS_EXIT,
            in("rdi") code,
            options(noreturn)
        );
    }
}

/// Entry point for the systemd process
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Write startup message to stdout (fd 1)
    let msg = b"Eclipse-systemd: Init process started (PID 1)\n";
    sys_write(1, msg.as_ptr(), msg.len() as u64);
    
    let msg2 = b"Eclipse-systemd: Minimal init running\n";
    sys_write(1, msg2.as_ptr(), msg2.len() as u64);
    
    let msg3 = b"Eclipse-systemd: Exiting successfully\n";
    sys_write(1, msg3.as_ptr(), msg3.len() as u64);
    
    // Exit with success code
    sys_exit(0);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    let msg = b"Eclipse-systemd: PANIC!\n";
    sys_write(2, msg.as_ptr(), msg.len() as u64);
    sys_exit(1);
}
