use std::prelude::v1::*;
use core::sync::atomic::{AtomicBool, Ordering};

static SIGNAL_RECEIVED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigusr1(_signum: u32) {
    SIGNAL_RECEIVED.store(true, Ordering::Release);
}

fn main() {
    println!("=== SIGNAL TEST START ===");
    let pid = unsafe { std::libc::getpid() };
    println!("My PID is: {}", pid);

    println!("Registering handler for SIGUSR1 ({})", std::libc::SIGUSR1);
    unsafe { std::libc::sigaction(std::libc::SIGUSR1, handle_sigusr1); }

    println!("Sending SIGUSR1 to myself...");
    unsafe { std::libc::kill(pid, std::libc::SIGUSR1); }

    println!("Waiting for signal...");
    // Give some time for the signal to be delivered.
    // Signal delivery happens on return from syscall (e.g. yield_cpu or sleep_ms).
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(1));
        if SIGNAL_RECEIVED.load(Ordering::Acquire) {
            break;
        }
    }

    if SIGNAL_RECEIVED.load(Ordering::Acquire) {
        println!("SUCCESS: Signal handler was executed!");
        unsafe { std::libc::exit(0); }
    } else {
        println!("FAILURE: Signal handler was NOT executed.");
        unsafe { std::libc::exit(1); }
    }
}
