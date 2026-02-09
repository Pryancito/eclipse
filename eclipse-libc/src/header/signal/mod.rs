//! signal.h - Signal handling
use crate::types::*;

pub type sig_atomic_t = c_int;

// Signal numbers
pub const SIGHUP: c_int = 1;
pub const SIGINT: c_int = 2;
pub const SIGQUIT: c_int = 3;
pub const SIGILL: c_int = 4;
pub const SIGTRAP: c_int = 5;
pub const SIGABRT: c_int = 6;
pub const SIGBUS: c_int = 7;
pub const SIGFPE: c_int = 8;
pub const SIGKILL: c_int = 9;
pub const SIGUSR1: c_int = 10;
pub const SIGSEGV: c_int = 11;
pub const SIGUSR2: c_int = 12;
pub const SIGPIPE: c_int = 13;
pub const SIGALRM: c_int = 14;
pub const SIGTERM: c_int = 15;
pub const SIGCHLD: c_int = 17;
pub const SIGCONT: c_int = 18;
pub const SIGSTOP: c_int = 19;

// Signal handler types
pub const SIG_DFL: usize = 0;
pub const SIG_IGN: usize = 1;
pub const SIG_ERR: usize = usize::MAX;

pub type sighandler_t = extern "C" fn(c_int);

// Simple signal handling (stubs for now)
static mut SIGNAL_HANDLERS: [usize; 32] = [SIG_DFL; 32];

#[no_mangle]
pub unsafe extern "C" fn signal(signum: c_int, handler: sighandler_t) -> sighandler_t {
    if signum < 0 || signum >= 32 {
        return core::mem::transmute(SIG_ERR);
    }
    
    let old_handler = SIGNAL_HANDLERS[signum as usize];
    SIGNAL_HANDLERS[signum as usize] = handler as usize;
    
    core::mem::transmute(old_handler)
}

#[no_mangle]
pub unsafe extern "C" fn raise(sig: c_int) -> c_int {
    if sig < 0 || sig >= 32 {
        return -1;
    }
    
    let handler = SIGNAL_HANDLERS[sig as usize];
    
    match handler {
        SIG_DFL => {
            // Default action - for now, just exit on most signals
            if sig == SIGABRT || sig == SIGSEGV || sig == SIGILL {
                use eclipse_syscall::call::exit;
                exit(128 + sig);
            }
            0
        }
        SIG_IGN => 0,  // Ignore
        _ => {
            // Call custom handler
            let handler_fn: sighandler_t = core::mem::transmute(handler);
            handler_fn(sig);
            0
        }
    }
}

#[repr(C)]
pub struct sigaction {
    pub sa_handler: sighandler_t,
    pub sa_mask: sigset_t,
    pub sa_flags: c_int,
}

pub type sigset_t = c_ulong;

#[no_mangle]
pub unsafe extern "C" fn sigaction(
    signum: c_int,
    act: *const sigaction,
    oldact: *mut sigaction,
) -> c_int {
    // Stub implementation
    -1
}

#[no_mangle]
pub unsafe extern "C" fn sigemptyset(set: *mut sigset_t) -> c_int {
    if set.is_null() {
        return -1;
    }
    *set = 0;
    0
}

#[no_mangle]
pub unsafe extern "C" fn sigfillset(set: *mut sigset_t) -> c_int {
    if set.is_null() {
        return -1;
    }
    *set = !0;
    0
}

#[no_mangle]
pub unsafe extern "C" fn sigaddset(set: *mut sigset_t, signum: c_int) -> c_int {
    if set.is_null() || signum < 0 || signum >= 64 {
        return -1;
    }
    *set |= 1 << signum;
    0
}

#[no_mangle]
pub unsafe extern "C" fn sigdelset(set: *mut sigset_t, signum: c_int) -> c_int {
    if set.is_null() || signum < 0 || signum >= 64 {
        return -1;
    }
    *set &= !(1 << signum);
    0
}
