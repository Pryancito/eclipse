#![no_std]
#![no_main]

use core::sync::atomic::{AtomicBool, Ordering};
use eclipse_libc::{println, getpid, sigaction, kill, exit, yield_cpu, SIGUSR1};

static SIGNAL_RECEIVED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigusr1(_signum: u32) {
    SIGNAL_RECEIVED.store(true, Ordering::Release);
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    println!("=== SIGNAL TEST START ===");
    let pid = getpid();
    println!("My PID is: {}", pid);

    println!("Registering handler for SIGUSR1 ({})", SIGUSR1);
    sigaction(SIGUSR1, handle_sigusr1);

    println!("Sending SIGUSR1 to myself...");
    kill(pid, SIGUSR1);

    println!("Waiting for signal...");
    // Give some time for the signal to be delivered.
    // Signal delivery happens on return from syscall (e.g. yield_cpu or println).
    for _ in 0..10 {
        yield_cpu();
        if SIGNAL_RECEIVED.load(Ordering::Acquire) {
            break;
        }
    }

    if SIGNAL_RECEIVED.load(Ordering::Acquire) {
        println!("SUCCESS: Signal handler was executed!");
        exit(0);
    } else {
        println!("FAILURE: Signal handler was NOT executed.");
        exit(1);
    }
}
