//! GUI Service - Launches and supervises the Sidewind compositor (smithay_app)
//!
//! Responsibilities:
//! 1. Wait for filesystem to be ready.
//! 2. Launch smithay_app from disk.
//! 3. Supervise it: if it crashes, wait a moment and relaunch (watchdog).

#![no_std]
#![no_main]

use eclipse_libc::{
    println, getpid, getppid, yield_cpu, send, open, close, read, O_RDONLY,
    mmap, munmap, PROT_READ, PROT_EXEC, MAP_PRIVATE, fstat, lseek, Stat, wait, spawn,
    SEEK_END,
};

/// Buffer to load compositor when mmap fails (e.g. file: scheme read path issues)
const MAX_COMPOSITOR_SIZE: usize = 512 * 1024;
static mut LOAD_BUF: [u8; MAX_COMPOSITOR_SIZE] = [0; MAX_COMPOSITOR_SIZE];

const COMPOSITOR_PATH: &str = "file:/usr/bin/smithay_app";
/// Yield iterations between restart attempts (~1s of busy-wait)
const RESTART_DELAY_YIELDS: u32 = 500_000;
/// Maximum restart attempts (0 = unlimited)
const MAX_RESTARTS: u32 = 0;

/// Wait for filesystem to be mounted.
fn wait_for_filesystem() {
    const MAX_ATTEMPTS: u32 = 200;
    let mut attempts = 0;
    loop {
        let fd = open("file:/", O_RDONLY, 0);
        if fd >= 0 {
            close(fd);
            return;
        }
        attempts += 1;
        if attempts >= MAX_ATTEMPTS {
            println!("[GUI-SERVICE] WARNING: Filesystem not ready after {} attempts, continuing anyway", attempts);
            return;
        }
        for _ in 0..50 { yield_cpu(); }
    }
}

/// Load and spawn smithay_app. Returns the child PID or -1 on failure.
unsafe fn spawn_compositor() -> i32 {
    let fd = open(COMPOSITOR_PATH, O_RDONLY, 0);
    if fd < 0 {
        println!("[GUI-SERVICE] ERROR: Cannot open {}", COMPOSITOR_PATH);
        return -1;
    }

    let mut st: Stat = core::mem::zeroed();
    let size = if fstat(fd, &mut st) >= 0 && st.size > 0 {
        st.size as u64
    } else {
        // Fallback: fstat may return size=0 (EclipseFS fstat bug). Use lseek(SEEK_END).
        let sz = lseek(fd, 0, SEEK_END);
        if sz <= 0 {
            println!("[GUI-SERVICE] ERROR: fstat and lseek(SEEK_END) failed for {}", COMPOSITOR_PATH);
            close(fd);
            return -1;
        }
        let _ = lseek(fd, 0, 0); // Reset to start for mmap
        sz as u64
    };
    let child_pid = {
        let mapped = mmap(0, size, PROT_READ | PROT_EXEC, MAP_PRIVATE, fd, 0);
        if mapped != u64::MAX && mapped != 0 {
            close(fd);
            let binary = core::slice::from_raw_parts(mapped as *const u8, size as usize);
            let pid = spawn(binary);
            let _ = munmap(mapped, size);
            pid
        } else {
            // mmap failed (e.g. file scheme). Fallback: load via read().
            let _ = lseek(fd, 0, 0); // SEEK_SET to start
            let buf = unsafe { &mut LOAD_BUF[..size.min(MAX_COMPOSITOR_SIZE as u64) as usize] };
            let n = read(fd as u32, buf);
            close(fd);
            if n < 0 || n as u64 != size {
                println!("[GUI-SERVICE] ERROR: read failed for {} (got {})", COMPOSITOR_PATH, n);
                return -1;
            }
            spawn(unsafe { &LOAD_BUF[..size as usize] })
        }
    };

    child_pid
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let pid = getpid();

    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║         GUI SERVICE - Sidewind Compositor Supervisor         ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    let ppid = getppid();
    println!("[GUI-SERVICE] PID={}, PPID={}", pid, ppid);

    // Notify parent (systemd/init) that we are ready
    if ppid > 0 {
        println!("[GUI-SERVICE] Sending READY to init (PID {})...", ppid);
        let _ = send(ppid, 255, b"READY");
    } else {
        println!("[GUI-SERVICE] WARNING: No parent process found to signal READY!");
    }

    // Wait for filesystem before loading compositor from disk
    wait_for_filesystem();
    println!("[GUI-SERVICE] Filesystem ready.");

    // Supervisor watchdog loop
    let mut restarts: u32 = 0;
    loop {
        println!("[GUI-SERVICE] Launching {} (attempt #{})...", COMPOSITOR_PATH, restarts + 1);

        let child_pid = unsafe { spawn_compositor() };

        if child_pid <= 0 {
            println!("[GUI-SERVICE] Failed to spawn compositor. Retrying in a moment...");
        } else {
            println!("[GUI-SERVICE] Compositor running as PID {}", child_pid);
            // Wait for the compositor to exit
            let mut status: i32 = 0;
            let exited = wait(Some(&mut status));
            println!("[GUI-SERVICE] Compositor (PID {}) exited (status={}).", child_pid, exited);
        }

        restarts += 1;
        if MAX_RESTARTS > 0 && restarts >= MAX_RESTARTS {
            println!("[GUI-SERVICE] Max restarts ({}) reached. Giving up.", MAX_RESTARTS);
            loop { yield_cpu(); }
        }

        // Brief delay before restarting
        println!("[GUI-SERVICE] Restarting compositor in ~1s...");
        for _ in 0..RESTART_DELAY_YIELDS { yield_cpu(); }
    }
}
