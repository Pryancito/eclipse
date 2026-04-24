//! Modular Syscall Interface for Eclipse OS
//! Distributes syscall handling across specialized modules.

use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

pub mod fs;
pub mod process;
pub mod memory;
pub mod ipc;
pub mod misc;
pub mod graphics;
pub mod network;

pub use process::{deliver_signal_from_exception, futex_wake_all_atomic};

// Stats y monitoreo
pub struct SyscallStats {
    pub total_calls: AtomicU64,
    pub read_calls: AtomicU64,
    pub write_calls: AtomicU64,
    pub exit_calls: AtomicU64,
    pub fork_calls: AtomicU64,
    pub exec_calls: AtomicU64,
    pub send_calls: AtomicU64,
    pub receive_calls: AtomicU64,
    pub yield_calls: AtomicU64,
}

pub static SYSCALL_STATS: SyscallStats = SyscallStats {
    total_calls: AtomicU64::new(0),
    read_calls: AtomicU64::new(0),
    write_calls: AtomicU64::new(0),
    exit_calls: AtomicU64::new(0),
    fork_calls: AtomicU64::new(0),
    exec_calls: AtomicU64::new(0),
    send_calls: AtomicU64::new(0),
    receive_calls: AtomicU64::new(0),
    yield_calls: AtomicU64::new(0),
};

#[derive(Default, Clone, Copy)]
#[repr(C)]
pub struct LinuxStat {
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub __pad0: u32,
    pub rdev: u64,
    pub size: i64,
    pub blksize: i64,
    pub blocks: i64,
    pub atime: i64,
    pub atime_nsec: i64,
    pub mtime: i64,
    pub mtime_nsec: i64,
    pub ctime: i64,
    pub ctime_nsec: i64,
    pub __unused: [i64; 3],
}

pub static LAST_SYSCALL_NUM: AtomicU64 = AtomicU64::new(0);
pub static LAST_SYSCALL_PID: AtomicU64 = AtomicU64::new(0);
pub static LAST_EXEC_ERR: Mutex<[u8; 80]> = Mutex::new([0u8; 80]);
pub static WALL_TIME_OFFSET: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    crate::serial::serial_print("Modular syscall system initialized\n");
}

pub fn current_process_id() -> Option<u32> {
    crate::process::current_process_id()
}

pub fn yield_cpu() {
    crate::scheduler::yield_cpu();
}

pub fn process_sleep_ms(ms: u64) -> u64 {
    crate::scheduler::sleep(ms);
    0
}

pub fn linux_abi_error(errno: i32) -> u64 {
    -(errno as i64) as u64
}

pub fn linux_makedev(major: u32, minor: u32) -> u64 {
    (((major as u64) & 0xfffff000) << 32) |
    (((major as u64) & 0x00000fff) << 8) |
    (((minor as u64) & 0xffffff00) << 12) |
    ((minor as u64) & 0x000000ff)
}

pub fn copy_to_user(user_ptr: u64, src: &[u8]) -> bool {
    if user_ptr == 0 || user_ptr >= 0xFFFF800000000000 { return false; }
    unsafe {
        let dest = core::slice::from_raw_parts_mut(user_ptr as *mut u8, src.len());
        dest.copy_from_slice(src);
    }
    true
}

pub fn copy_from_user(user_ptr: u64, dest: &mut [u8]) -> bool {
    if user_ptr == 0 || user_ptr >= 0xFFFF800000000000 { return false; }
    unsafe {
        let src = core::slice::from_raw_parts(user_ptr as *const u8, dest.len());
        dest.copy_from_slice(src);
    }
    true
}

pub fn is_user_pointer(ptr: u64, len: u64) -> bool {
    ptr != 0 && ptr < 0xFFFF800000000000 && (ptr + len) < 0xFFFF800000000000
}

pub fn strlen_user_unique(ptr: u64, max: usize) -> u64 {
    if ptr == 0 { return 0; }
    let mut len = 0;
    while len < max {
        let mut b = 0u8;
        if !copy_from_user(ptr + len as u64, core::slice::from_mut(&mut b)) { break; }
        if b == 0 { break; }
        len += 1;
    }
    len as u64
}

pub fn user_path_to_scheme_path(user_path: &str) -> alloc::string::String {
    if user_path.starts_with('/') {
        alloc::format!("file:{}", user_path)
    } else {
        alloc::string::String::from(user_path)
    }
}

pub extern "C" fn syscall_handler(syscall_num: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64, arg6: u64, context: &mut crate::interrupts::SyscallContext) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    LAST_SYSCALL_PID.store(pid as u64, Ordering::Relaxed);
    LAST_SYSCALL_NUM.store(syscall_num, Ordering::Relaxed);
    handle_syscall(syscall_num, arg1, arg2, arg3, arg4, arg5, arg6, context)
}

pub fn handle_syscall(syscall_num: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64, arg6: u64, context: &mut crate::interrupts::SyscallContext) -> u64 {
    SYSCALL_STATS.total_calls.fetch_add(1, Ordering::Relaxed);
    let pid = current_process_id().unwrap_or(0);
    
    let proc_ctx = crate::process::Context {
        r15: context.r15, r14: context.r14, r13: context.r13, r12: context.r12,
        r11: context.r11, r10: context.r10, r9: context.r9, r8: context.r8,
        rbp: context.rbp, rdi: context.rdi, rsi: context.rsi, rdx: context.rdx,
        rcx: context.rcx, rbx: context.rbx, rax: context.rax, rip: context.rip,
        rflags: context.rflags, rsp: context.rsp,
        fs_base: context.fs_base,
        gs_base: context.gs_base,
    };

    match syscall_num {
        // --- Linux Standard Syscalls ---
        0 => fs::sys_read(arg1, arg2, arg3),
        1 => fs::sys_write(arg1, arg2, arg3),
        2 => fs::sys_open(arg1, arg2, arg3),
        3 => fs::sys_close(arg1),
        4 => fs::sys_stat(arg1, arg2),
        5 => fs::sys_fstat(arg1, arg2),
        8 => fs::sys_lseek(arg1, arg2 as i64, arg3),
        9 => memory::sys_mmap(arg1, arg2, arg3, arg4, arg5, arg6),
        10 => memory::sys_mprotect(arg1, arg2, arg3),
        11 => memory::sys_munmap(arg1, arg2),
        12 => memory::sys_brk(arg1),
        13 => process::sys_rt_sigaction(arg1, arg2, arg3, arg4),
        14 => process::sys_rt_sigprocmask(arg1, arg2, arg3, arg4),
        15 => process::sys_rt_sigreturn(context),
        16 => fs::sys_ioctl(arg1, arg2, arg3),
        22 => ipc::sys_pipe(arg1),
        24 => ipc::sys_yield(),
        32 => fs::sys_dup(arg1),
        33 => fs::sys_dup2(arg1, arg2),
        34 => ipc::sys_pause(),
        35 => misc::sys_nanosleep(arg1, arg2),
        39 => process::sys_getpid(),
        41 => ipc::sys_socket(arg1, arg2, arg3),
        42 => ipc::sys_connect(arg1, arg2, arg3),
        43 => ipc::sys_accept(arg1, arg2, arg3),
        44 => ipc::sys_sendto(arg1, arg2, arg3, arg4, arg5, arg6),
        45 => ipc::sys_recvfrom(arg1, arg2, arg3, arg4, arg5, arg6),
        46 => ipc::sys_sendmsg(arg1, arg2, arg3),
        47 => ipc::sys_recvmsg(arg1, arg2, arg3),
        48 => ipc::sys_shutdown(arg1, arg2),
        49 => ipc::sys_bind(arg1, arg2, arg3),
        50 => ipc::sys_listen(arg1, arg2),
        51 => ipc::sys_getsockname(arg1, arg2, arg3),
        52 => ipc::sys_getpeername(arg1, arg2, arg3),
        53 => ipc::sys_socketpair(arg1, arg2, arg3, arg4),
        54 => ipc::sys_setsockopt(arg1, arg2, arg3, arg4, arg5),
        55 => ipc::sys_getsockopt(arg1, arg2, arg3, arg4, arg5),
        56 => process::sys_clone(arg1, arg2, arg3, &proc_ctx),
        57 => process::sys_fork(&proc_ctx),
        59 => process::sys_execve(arg1, arg2, arg3),
        60 => process::sys_exit(arg1),
        61 => process::sys_wait4_linux(arg1, arg2, arg3, arg4),
        62 => process::sys_kill(arg1, arg2),
        63 => misc::sys_uname(arg1),
        72 => fs::sys_fcntl(arg1, arg2, arg3),
        77 => fs::sys_ftruncate(arg1, arg2),
        79 => fs::sys_getcwd(arg1, arg2),
        80 => fs::sys_chdir(arg1),
        83 => fs::sys_mkdir(arg1, arg2),
        84 => fs::sys_rmdir(arg1),
        87 => fs::sys_unlink(arg1),
        96 => misc::sys_gettimeofday(arg1, arg2),
        102 => process::sys_getuid(),
        104 => process::sys_getgid(),
        107 => process::sys_geteuid(),
        108 => process::sys_getegid(),
        109 => process::sys_setpgid(arg1, arg2),
        110 => process::sys_getppid(),
        112 => process::sys_setsid(),
        131 => process::sys_sigaltstack(arg1, arg2),
        158 => process::sys_arch_prctl(arg1, arg2, context),
        186 => process::sys_gettid(),
        202 => process::sys_futex(arg1, arg2, arg3, arg4, arg5, arg6 as u32),
        218 => process::sys_set_tid_address(arg1),
        293 => ipc::sys_pipe2(arg1, arg2),
        441 => ipc::sys_eventfd2(arg1, arg2),
        
        // --- Eclipse OS Specific Syscalls ---
        500 => misc::sys_get_ticks(),
        501 => ipc::sys_send(arg1, arg2, arg3, arg4),
        502 => ipc::sys_receive(arg1, arg2, arg3),
        503 => ipc::sys_receive_fast(arg1, arg2, arg3),
        510 => graphics::sys_map_fb(arg1),
        511 => graphics::sys_get_fb_info(arg1),
        512 => graphics::sys_drm_ioctl(arg1, arg2, arg3),
        513 => graphics::sys_drm_get_caps(arg1, arg2),
        514 => graphics::sys_drm_alloc_buffer(arg1, arg2),
        515 => graphics::sys_drm_create_fb(arg1, arg2),
        516 => graphics::sys_drm_map_handle(arg1, arg2),
        517 => graphics::sys_drm_get_magic(arg1, arg2),
        518 => graphics::sys_drm_auth_magic(arg1, arg2),
        519 => graphics::sys_drm_set_master(arg1),
        520 => graphics::sys_drm_drop_master(arg1),
        542 => process::sys_spawn(arg1, arg2),
        
        _ => {
            if syscall_num != 1 { // skip frequent writes
                 crate::serial::serial_printf(format_args!("[SYSCALL] Unknown: {} from PID {}\n", syscall_num, pid));
            }
            linux_abi_error(38) // ENOSYS
        }
    }
}
