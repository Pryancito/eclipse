//! signal.h - Signals
use crate::types::*;

pub type OsSigHandlerPtr = unsafe extern "C" fn(c_int);

// Signal numbers (Linux x86-64 compatible)
pub const SIGHUP:    c_int = 1;
pub const SIGINT:    c_int = 2;
pub const SIGQUIT:   c_int = 3;
pub const SIGILL:    c_int = 4;
pub const SIGTRAP:   c_int = 5;
pub const SIGABRT:   c_int = 6;
pub const SIGFPE:    c_int = 8;
pub const SIGKILL:   c_int = 9;
pub const SIGSEGV:   c_int = 11;
pub const SIGPIPE:   c_int = 13;
pub const SIGALRM:   c_int = 14;
pub const SIGTERM:   c_int = 15;
pub const SIGUSR1:   c_int = 10;
pub const SIGUSR2:   c_int = 12;
pub const SIGCHLD:   c_int = 17;
pub const SIGCONT:   c_int = 18;
pub const SIGSTOP:   c_int = 19;
pub const SIGTSTP:   c_int = 20;
pub const SIGTTIN:   c_int = 21;
pub const SIGTTOU:   c_int = 22;
pub const SIGURG:    c_int = 23;
pub const SIGXCPU:   c_int = 24;
pub const SIGXFSZ:   c_int = 25;
pub const SIGVTALRM: c_int = 26;
pub const SIGPROF:   c_int = 27;
pub const SIGWINCH:  c_int = 28;
pub const NSIG:      c_int = 32;

// Signal action flags
pub const SA_NOCLDSTOP: c_int = 1;
pub const SA_NOCLDWAIT: c_int = 2;
pub const SA_SIGINFO:   c_int = 4;
pub const SA_ONSTACK:   c_int = 0x08000000;
pub const SA_RESTART:   c_int = 0x10000000;
pub const SA_NODEFER:   c_int = 0x40000000;
pub const SA_RESETHAND: c_int = -2147483648i32; // 0x80000000

// SIG_DFL and SIG_IGN as raw function pointer values.
// In C these are (void(*)(int))0 and (void(*)(int))1.
pub const SIG_DFL: usize = 0;
pub const SIG_IGN: usize = 1;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct sigaction {
    pub sa_handler: Option<OsSigHandlerPtr>,
    pub sa_mask: sigset_t,
    pub sa_flags: c_int,
    pub sa_restorer: Option<unsafe extern "C" fn()>,
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn signal(signum: c_int, handler: Option<OsSigHandlerPtr>) -> Option<OsSigHandlerPtr> {
    use crate::eclipse_syscall::call::sigaction as sys_sigaction;
    let mut act: sigaction = core::mem::zeroed();
    act.sa_handler = handler;
    act.sa_flags = SA_RESTART;
    let mut oldact: sigaction = core::mem::zeroed();
    match sys_sigaction(signum as usize, &act as *const sigaction as usize, &mut oldact as *mut sigaction as usize) {
        Ok(_) => oldact.sa_handler,
        Err(_) => None,
    }
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn kill(pid: pid_t, sig: c_int) -> c_int {
    use crate::eclipse_syscall::call::kill;
    match kill(pid as usize, sig as usize) {
        Ok(_) => 0,
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn sigaction(signum: c_int, act: *const sigaction, oldact: *mut sigaction) -> c_int {
    use crate::eclipse_syscall::call::sigaction;
    match sigaction(signum as usize, act as usize, oldact as usize) {
        Ok(_) => 0,
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn sigemptyset(set: *mut sigset_t) -> c_int {
    if !set.is_null() {
        (*set).sig[0] = 0;
    }
    0
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn sigaddset(set: *mut sigset_t, signum: c_int) -> c_int {
    if !set.is_null() && signum > 0 && signum <= 64 {
        (*set).sig[0] |= 1 << (signum - 1);
    }
    0
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn sigprocmask(how: c_int, set: *const sigset_t, oldset: *mut sigset_t) -> c_int {
    use crate::eclipse_syscall::call::sigprocmask;
    let set_ptr = if set.is_null() { 0 } else { set as usize };
    let oldset_ptr = if oldset.is_null() { 0 } else { oldset as usize };
    
    match sigprocmask(how as usize, set_ptr, oldset_ptr) {
        Ok(_) => 0,
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn sigdelset(set: *mut sigset_t, signum: c_int) -> c_int {
    if !set.is_null() && signum > 0 && signum <= 64 {
        (*set).sig[0] &= !(1u64 << (signum - 1));
    }
    0
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn sigismember(set: *const sigset_t, signum: c_int) -> c_int {
    if set.is_null() || signum <= 0 || signum > 64 { return 0; }
    if ((*set).sig[0] & (1u64 << (signum - 1))) != 0 { 1 } else { 0 }
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn sigfillset(set: *mut sigset_t) -> c_int {
    if !set.is_null() {
        (*set).sig[0] = u64::MAX;
    }
    0
}

/// sigpending — get set of pending signals.
#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn sigpending(set: *mut sigset_t) -> c_int {
    if !set.is_null() { (*set).sig[0] = 0; }
    0
}

/// sigsuspend — wait for signal.
#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn sigsuspend(_mask: *const sigset_t) -> c_int {
    // Block forever (pause) — the kernel will wake us on signal delivery.
    eclipse_syscall::syscall0(eclipse_syscall::SYS_PAUSE);
    *crate::header::errno::__errno_location() = 4; // EINTR
    -1
}

/// alarm — set an alarm signal after `seconds` seconds.
#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn alarm(_seconds: crate::types::c_uint) -> crate::types::c_uint {
    0 // Stub: alarm not yet implemented
}

/// raise — send signal to the current process.
#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn raise(sig: c_int) -> c_int {
    use crate::eclipse_syscall::call::kill;
    let pid = crate::eclipse_syscall::call::getpid();
    match kill(pid, sig as usize) {
        Ok(_) => 0,
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}
