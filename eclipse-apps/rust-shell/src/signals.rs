use core::sync::atomic::{AtomicUsize, Ordering};

pub const SIGINT: i32 = 2;
pub const SIGKILL: i32 = 9;
pub const SIGTERM: i32 = 15;
pub const SIGCHLD: i32 = 17;
pub const SIGCONT: i32 = 18;
pub const SIGSTOP: i32 = 19;
pub const SIGTSTP: i32 = 20;
pub const SIGWINCH: i32 = 28;

pub static FG_PID: AtomicUsize = AtomicUsize::new(0);

#[cfg(target_vendor = "eclipse")]
use eclipse_syscall;

#[repr(C)]
pub struct SigAction {
    pub sa_handler: Option<extern "C" fn(i32)>,
    pub sa_mask: u64,
    pub sa_flags: i32,
    pub sa_restorer: Option<extern "C" fn()>,
}

pub extern "C" fn shell_signal_handler(sig: i32) {
    if sig == SIGWINCH {
        // Trigger a redraw or update env vars.
        // For now, we'll let the next prompt update it.
        return;
    }
    let pid = FG_PID.load(Ordering::SeqCst);
    if pid != 0 {
        #[cfg(target_vendor = "eclipse")]
        unsafe {
            let _ = eclipse_syscall::call::kill(pid, sig as usize);
        }
    }
}

pub fn setup_signals() {
    #[cfg(target_vendor = "eclipse")]
    {
        let sa = SigAction {
            sa_handler: Some(shell_signal_handler),
            sa_mask: 0,
            sa_flags: 0,
            sa_restorer: None,
        };
        
        let _ = eclipse_syscall::call::sigaction(SIGINT as usize, &sa as *const _ as usize, 0);
        let _ = eclipse_syscall::call::sigaction(SIGTSTP as usize, &sa as *const _ as usize, 0);
        let _ = eclipse_syscall::call::sigaction(SIGWINCH as usize, &sa as *const _ as usize, 0);
    }
}

pub fn set_fg_pid(pid: usize) {
    FG_PID.store(pid, Ordering::SeqCst);
    #[cfg(target_vendor = "eclipse")]
    unsafe {
        // Fallback to our own PID if pid is 0 (regain control)
        let target = if pid == 0 { 
            eclipse_syscall::call::getpid() as u32 
        } else { 
            pid as u32 
        };
        // TIOCSPGRP = 5
        let _ = eclipse_syscall::call::ioctl(0, 0x5, &target as *const _ as usize);
    }
}

pub fn get_fg_pid() -> usize {
    FG_PID.load(Ordering::SeqCst)
}
