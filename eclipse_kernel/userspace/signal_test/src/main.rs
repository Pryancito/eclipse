#![no_std]
#![no_main]

use eclipse_libc::{println, getpid, sigaction, kill, exit, yield_cpu, SIGUSR1};

static mut SIGNAL_RECEIVED: bool = false;

extern "C" fn handle_sigusr1(signum: u32) {
    // We can't use println safely here if it uses locks that the main thread might hold,
    // but in this simple test it should be fine.
    // However, the kernel-side signal delivery print should show up too.
    unsafe {
        SIGNAL_RECEIVED = true;
    }
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
        unsafe {
            if SIGNAL_RECEIVED {
                break;
            }
        }
    }

    unsafe {
        if SIGNAL_RECEIVED {
            println!("SUCCESS: Signal handler was executed!");
            exit(0);
        } else {
            println!("FAILURE: Signal handler was NOT executed.");
            exit(1);
        }
    }
}
