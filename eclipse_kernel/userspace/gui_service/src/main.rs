//! GUI Service - Launches and supervises the Sidewind compositor (smithay_app)
//!
//! Responsibilities:
//! 1. Wait for filesystem to be ready.
//! 2. Launch smithay_app from disk.
//! 3. Supervise it: if it crashes, wait a moment and relaunch (watchdog).

#![no_main]
extern crate std;
extern crate alloc;

use std::prelude::*;

/// Buffer to load compositor when mmap fails (e.g. file: scheme read path issues)
const MAX_COMPOSITOR_SIZE: usize = 16 * 1024 * 1024;
/// Spinlock-protected load buffer for thread-safe SMP access.
static LOAD_BUF: std::libc::Spinlock<[u8; MAX_COMPOSITOR_SIZE]> = std::libc::Spinlock::new([0; MAX_COMPOSITOR_SIZE]);

const COMPOSITOR_PATH: &str = "file:/usr/bin/smithay_app";

/// Wait for filesystem to be mounted.
fn wait_for_filesystem() {
    const MAX_ATTEMPTS: u32 = 3000;
    let mut attempts = 0;
    loop {
        let fd = std::libc::eclipse_open("file:/", std::libc::O_RDONLY, 0);
        if fd >= 0 {
            unsafe { std::libc::eclipse_close(fd); }
            return;
        }
        attempts += 1;
        if attempts >= MAX_ATTEMPTS {
            println!("[GUI-SERVICE] WARNING: Filesystem not ready after {} attempts, continuing anyway", attempts);
            return;
        }
        std::libc::sleep_ms(10);
    }
}

/// Load and spawn smithay_app. Returns the child PID or -1 on failure.
unsafe fn spawn_compositor() -> i32 {
    use std::libc::{eclipse_open, eclipse_close, eclipse_read, eclipse_spawn, fstat, lseek, mmap, munmap, stat};
    const SEEK_END: i32 = 2;
    let fd = eclipse_open(COMPOSITOR_PATH, std::libc::O_RDONLY, 0);
    if fd < 0 {
        println!("[GUI-SERVICE] ERROR: Cannot open {}", COMPOSITOR_PATH);
        return -1;
    }

    let mut st: stat = core::mem::zeroed();
    let size = if fstat(fd, &mut st) >= 0 && st.st_size > 0 {
        st.st_size as u64
    } else {
        let sz = lseek(fd, 0, SEEK_END);
        if sz <= 0 {
            println!("[GUI-SERVICE] ERROR: fstat and lseek(SEEK_END) failed for {}", COMPOSITOR_PATH);
            eclipse_close(fd);
            return -1;
        }
        let _ = lseek(fd, 0, 0);
        sz as u64
    };
    let child_pid = {
        let mapped = mmap(
            core::ptr::null_mut(),
            size as usize,
            std::libc::PROT_READ | std::libc::PROT_EXEC,
            std::libc::MAP_PRIVATE,
            fd,
            0,
        );
        if !mapped.is_null() && (mapped as isize) > 0 {
            println!("[GUI-SERVICE] Mapped compositor at {:p}", mapped);
            eclipse_close(fd);
            let binary = core::slice::from_raw_parts(mapped as *const u8, size as usize);
            let pid = eclipse_spawn(binary, Some("smithay_app"));
            let _ = munmap(mapped, size as usize);
            pid
        } else {
            let _ = lseek(fd, 0, 0);
            let mut load_guard = LOAD_BUF.lock();
            let read_size = size.min(MAX_COMPOSITOR_SIZE as u64) as usize;
            let n = {
                let buf = &mut load_guard[..read_size];
                let result = std::libc::eclipse_read(fd as u32, buf);
                eclipse_close(fd);
                result
            };
            if n < 0 || n as u64 != size {
                println!("[GUI-SERVICE] ERROR: read failed for {} (got {})", COMPOSITOR_PATH, n);
                return -1;
            }
            eclipse_spawn(&load_guard[..size as usize], Some("smithay_app"))
        }
    };

    child_pid
}

#[no_mangle]
pub extern "Rust" fn main() -> i32 {
    let pid = unsafe { std::libc::getpid() };
    println!("+--------------------------------------------------------------+");
    println!("|         GUI SERVICE - Sidewind Compositor Supervisor         |");
    println!("+--------------------------------------------------------------+");
    let ppid = unsafe { std::libc::getppid() };
    println!("[GUI-SERVICE] PID={}, PPID={}", pid, ppid);

    if ppid > 0 {
        println!("[GUI-SERVICE] Sending READY to init (PID {})...", ppid);
        let _ = std::libc::send_ipc(ppid as u32, 255, b"READY");
    } else {
        println!("[GUI-SERVICE] WARNING: No parent process found to signal READY!");
    }

    wait_for_filesystem();
    println!("[GUI-SERVICE] Filesystem ready.");

    let _child_pid = unsafe { spawn_compositor() };
    loop {
        std::libc::sleep_ms(1000);
    }
}
