//! Sistema de syscalls del microkernel
//! 
//! Implementa la interfaz entre userspace y kernel

use alloc::format;
use alloc::string::String;

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use crate::process::{self, ProcessId, exit_process, current_process_id};
use crate::scheduler::yield_cpu;
use crate::ipc::{MessageType, send_message, receive_message, pop_small_message_24};
use crate::scheme::{Scheme, error as scheme_error, Stat};
use crate::serial;
use spin::Mutex;
use alloc::sync::Arc;
use eclipse_program_codes::spawn_service as svc;

/// Debug: último PID y número de syscall (para heartbeat cuando se congela input).
pub(crate) static LAST_SYSCALL_PID: AtomicU32 = AtomicU32::new(0);
pub(crate) static LAST_SYSCALL_NUM: AtomicU64 = AtomicU64::new(0);

/// Debug: receive/receive_fast que devolvieron mensaje vs vacío (reseteado cada heartbeat).
pub(crate) static RECV_OK: AtomicU64 = AtomicU64::new(0);
pub(crate) static RECV_EMPTY: AtomicU64 = AtomicU64::new(0);

/// Offset global para el reloj de tiempo real (Unix timestamp - uptime_ticks/1000)
pub(crate) static WALL_TIME_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Números de syscalls
#[repr(u64)]
#[derive(Debug, Clone, Copy)]
pub enum SyscallNumber {
    // --- Linux x86-64 standard syscalls ---
    Read = 0,
    Write = 1,
    Open = 2,
    Close = 3,
    Stat = 4,
    Fstat = 5,
    Lstat = 6,
    Poll = 7,
    Lseek = 8,
    Mmap = 9,
    Mprotect = 10,
    Munmap = 11,
    Brk = 12,
    SigAction = 13,
    Sigprocmask = 14,
    RtSigreturn = 15,
    Ioctl = 16,
    Pread64 = 17,
    Writev = 20,
    Access = 21,
    Pipe = 22,
    Yield = 24,
    Madvise = 28,
    Dup = 32,
    Dup2 = 33,
    Pause = 34,
    Nanosleep = 35,
    GetPid = 39,
    Socket = 41,
    Connect = 42,
    Accept = 43,
    Sendto = 44,
    Recvfrom = 45,
    Sendmsg = 46,
    Recvmsg = 47,
    Shutdown = 48,
    Bind = 49,
    Listen = 50,
    Getsockname = 51,
    Getpeername = 52,
    Socketpair = 53,
    Setsockopt = 54,
    Getsockopt = 55,
    Clone = 56,
    Fork = 57,
    Vfork = 58,
    Execve = 59,
    Exit = 60,
    Wait4 = 61,
    Kill = 62,
    Uname = 63,
    Fcntl = 72,
    Flock = 73,
    Fsync = 74,
    Fdatasync = 75,
    Truncate = 76,
    Ftruncate = 77,
    Getdents = 78,
    Getcwd = 79,
    Chdir = 80,
    Fchdir = 81,
    Rename = 82,
    Mkdir = 83,
    Rmdir = 84,
    Creat = 85,
    Link = 86,
    Unlink = 87,
    Symlink = 88,
    Readlink = 89,
    Chmod = 90,
    Fchmod = 91,
    Chown = 92,
    Fchown = 93,
    Lchown = 94,
    Umask = 95,
    Gettimeofday = 96,
    Getrlimit = 97,
    Getrusage = 98,
    Sysinfo = 99,
    Getuid = 102,
    Syslog = 103,
    Getgid = 104,
    Setuid = 105,
    Setgid = 106,
    Geteuid = 107,
    Getegid = 108,
    Setpgid = 109,
    Getppid = 110,
    Getpgrp = 111,
    Setsid = 112,
    Setreuid = 113,
    Setregid = 114,
    Getresuid = 118,
    Getresgid = 120,
    Getpgid = 121,
    Sigaltstack = 131,
    ArchPrctl = 158,
    SetHostName = 170,
    Gettid = 186,
    Tkill = 200,
    Futex = 202,
    Getdents64 = 217,
    SetTidAddress = 218,
    ClockGettime = 228,
    ClockNanosleep = 230,
    ExitGroup = 231,
    EpollWait = 232,
    EpollCtl = 233,
    GetLogin = 247,
    InotifyAddWatch = 254,
    Openat = 257,
    Fstatat = 262,
    Faccessat = 269,
    Pselect6 = 270,
    Signalfd4 = 282,
    TimerfdCreate = 283,
    TimerfdSettime = 286,
    Eventfd2 = 290,
    EpollCreate1 = 291,
    Dup3 = 292,
    Pipe2 = 293,
    InotifyInit1 = 294,
    Prlimit64 = 302,
    GetRandom = 318,
    Membarrier = 324,

    // --- Eclipse-specific (500+) ---
    Send = 500,
    Receive = 501,
    GetServiceBinary = 502,
    GetFramebufferInfo = 503,
    MapFramebuffer = 504,
    PciEnumDevices = 505,
    PciReadConfig = 506,
    PciWriteConfig = 507,
    RegisterDevice = 508,
    Fmap = 509,
    Mount = 510,
    Spawn = 511,
    GetLastExecError = 512,
    ReadKey = 513,
    ReadMousePacket = 514,
    GetGpuDisplayInfo = 515,
    SetCursorPosition = 516,
    GpuAllocDisplayBuffer = 517,
    GpuPresent = 518,
    GetLogs = 519,
    GetStorageDeviceCount = 520,
    GetSystemStats = 521,
    GetProcessList = 522,
    SetProcessName = 523,
    SpawnService = 524,
    GpuCommand = 525,
    StopProgress = 526,
    GetGpuBackend = 527,
    DrmPageFlip = 528,
    DrmGetCaps = 529,
    DrmAllocBuffer = 530,
    DrmCreateFb = 531,
    DrmMapHandle = 532,
    SchedSetAffinity = 533,
    RegisterLogHud = 534,
    SetTime = 535,
    SpawnWithStdio = 536,
    ThreadCreate = 537,
    WaitPid = 538,
    /// ELF cargado desde ruta VFS en el kernel (ver `sys_spawn_with_stdio_path`).
    SpawnWithStdioPath = 544,
    Strace = 545,
    Exec = 546,
    ReceiveFast = 600,
}



/// lseek whence values (POSIX standard)
pub const SEEK_SET: u64 = 0; // Absolute position
pub const SEEK_CUR: u64 = 1; // Relative to current position  
pub const SEEK_END: u64 = 2; // Relative to end of file

/// Constantes Linux para `mmap` / `mprotect` (x86_64, musl).
///
/// `ANON_SLACK_BYTES`: páginas extra en `mmap` anónimo para desbordes lógicos y trampolines;
/// ver `mmap_pte_linux_prot` y `mprotect_expand_anon_slack`.
mod linux_mmap_abi {
    pub const PROT_MASK: u64 = 7;
    pub const PROT_EXEC: u64 = 4;
    pub const MAP_FIXED: u64 = 0x10;
    pub const MAP_SHARED: u64 = 0x01;
    pub const MAP_ANONYMOUS: u64 = 0x20;
    /// Pre-populate page table entries. When set, physical frames are allocated
    /// eagerly (like Linux `MAP_POPULATE`). Without it, anonymous private mappings
    /// are lazy: frames are allocated on first access via the demand-page handler.
    pub const MAP_POPULATE: u64 = 0x08000;
    /// Donde `mmap_find_free` coloca `mmap(NULL, …)` anónimo.
    pub const USER_ARENA_LO: u64 = 0x6000_0000;
    pub const USER_ARENA_HI: u64 = 0x7000_0000;
    /// Pila fija tras `exec`/`execve` / `spawn` (1 MiB en 512 MiB virtuales).
    pub const USER_EXEC_STACK_LO: u64 = 0x2000_0000;
    pub const USER_EXEC_STACK_HI: u64 = USER_EXEC_STACK_LO + 0x10_0000;
    /// Páginas extra más allá del tamaño redondeado (trampolines / desbordes de musl).
    /// Incluye margen para fetch de instrucción de hasta 15 B en el último byte de página
    /// (p. ej. RIP=0x…3fff y CR2=0x…4000: hacía falta >4 páginas de colchón).
    pub const ANON_SLACK_BYTES: u64 = 0x8000;
}

/// Remove the range `[lo, hi)` from the VMA list, splitting VMAs that partially overlap it.
///
/// Unlike a simple `retain`, this preserves portions of VMAs that lie outside the removed range
/// so that kernel-managed slack pages (instruction-decode guards) are not orphaned when a
/// subsequent `MAP_FIXED` or `munmap` covers only part of a slack VMA.
fn vma_remove_range(vmas: &mut alloc::vec::Vec<crate::process::VMARegion>, lo: u64, hi: u64) {
    if hi <= lo {
        return;
    }
    let old: alloc::vec::Vec<crate::process::VMARegion> = core::mem::take(vmas);
    for vma in old {
        if vma.end <= lo || vma.start >= hi {
            vmas.push(vma);
        } else {
            if vma.start < lo {
                vmas.push(crate::process::VMARegion { end: lo, ..vma });
            }
            if vma.end > hi {
                vmas.push(crate::process::VMARegion { start: hi, ..vma });
            }
        }
    }
    vma_merge_adjacent(vmas);
}

fn vma_mprotect_range(vmas: &mut alloc::vec::Vec<crate::process::VMARegion>, lo: u64, hi: u64, prot: u64) {
    if hi <= lo {
        return;
    }
    let old: alloc::vec::Vec<crate::process::VMARegion> = core::mem::take(vmas);
    for vma in old {
        if vma.end <= lo || vma.start >= hi {
            vmas.push(vma);
        } else {
            if vma.start < lo {
                vmas.push(crate::process::VMARegion { end: lo, ..vma });
            }

            let mid_start = vma.start.max(lo);
            let mid_end = vma.end.min(hi);
            let mut new_vma = vma.clone();
            new_vma.start = mid_start;
            new_vma.end = mid_end;
            new_vma.flags = prot;
            vmas.push(new_vma);

            if vma.end > hi {
                vmas.push(crate::process::VMARegion { start: hi, ..vma });
            }
        }
    }
    vma_merge_adjacent(vmas);
}

/// Combina VMAs adyacentes con flags e identidad idénticos para evitar fragmentación.
fn vma_merge_adjacent(vmas: &mut alloc::vec::Vec<crate::process::VMARegion>) {
    if vmas.len() < 2 { return; }

    // 1. Ordenar por dirección de inicio para facilitar el merge lineal.
    vmas.sort_by_key(|v| v.start);

    // 2. Merge lineal en un nuevo vector.
    let old = core::mem::take(vmas);
    let mut iter = old.into_iter();
    
    if let Some(mut current) = iter.next() {
        for next in iter {
            if current.can_merge(&next) {
                // Son adyacentes e idénticas: extender el final de la actual.
                current.end = next.end;
            } else {
                // No son combinables: guardar la actual e iniciar nueva candidata.
                vmas.push(current);
                current = next;
            }
        }
        vmas.push(current);
    }
}

/// Si `PROT_EXEC` y el rango toca un VMA anónimo con colchón del kernel, amplía a todo el VMA.
fn mprotect_expand_anon_slack(
    vmas: &[crate::process::VMARegion],
    mut lo: u64,
    mut hi: u64,
    prot: u64,
) -> (u64, u64) {
    use linux_mmap_abi::PROT_EXEC;
    if (prot & PROT_EXEC) == 0 || hi <= lo {
        return (lo, hi);
    }
    let mut changed = true;
    while changed {
        changed = false;
        for vma in vmas.iter() {
            if vma.file_backed || vma.anon_kernel_slack == 0 {
                continue;
            }
            if lo < vma.end && hi > vma.start {
                let na = lo.min(vma.start);
                let ne = hi.max(vma.end);
                if na != lo || ne != hi {
                    lo = na;
                    hi = ne;
                    changed = true;
                }
            }
        }
    }
    (lo, hi)
}

/// `prot` Linux para una página de `mmap`: el colchón anónimo lleva siempre bit ejecutable.
fn mmap_pte_linux_prot(base_prot: u64, anon_slack: u64, map_end: u64, page_vaddr: u64) -> u64 {
    use linux_mmap_abi::PROT_EXEC;
    if anon_slack == 0 {
        return base_prot;
    }
    let slack_lo = map_end.saturating_sub(anon_slack);
    if page_vaddr >= slack_lo {
        base_prot | PROT_EXEC
    } else {
        base_prot
    }
}

/// Estadísticas de syscalls
pub struct SyscallStats {
    pub total_calls: u64,
    pub exit_calls: u64,
    pub write_calls: u64,
    pub read_calls: u64,
    pub send_calls: u64,
    pub receive_calls: u64,
    pub yield_calls: u64,
    pub fork_calls: u64,
    pub exec_calls: u64,
    pub wait_calls: u64,
    pub open_calls: u64,
    pub close_calls: u64,
    pub lseek_calls: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SystemStats {
    pub uptime_ticks: u64,
    pub idle_ticks: u64,
    pub total_mem_frames: u64,
    pub used_mem_frames: u64,
    pub cpu_count: u64,
    // AI-CORE Vitals
    pub cpu_temp: [u32; 16],
    pub gpu_load: [u32; 4],
    pub gpu_temp: [u32; 4],
    pub gpu_vram_total_bytes: u64,
    pub gpu_vram_used_bytes: u64,
    pub anomaly_count: u32,
    pub heap_fragmentation: u32,
    pub wall_time_offset: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ProcessInfo {
    pub pid: u32,
    pub state: u32, // ProcessState enum as u32
    pub name: [u8; 16],
    pub cpu_ticks: u64,
    pub mem_frames: u64,
}


static SYSCALL_STATS: Mutex<SyscallStats> = Mutex::new(SyscallStats {
    total_calls: 0,
    exit_calls: 0,
    write_calls: 0,
    read_calls: 0,
    send_calls: 0,
    receive_calls: 0,
    yield_calls: 0,
    fork_calls: 0,
    exec_calls: 0,
    wait_calls: 0,
    open_calls: 0,
    close_calls: 0,
    lseek_calls: 0,
});

/// Maximum path length for syscall path buffers
/// Linux uses PATH_MAX = 4096, we use 1024 as a reasonable compromise
const MAX_PATH_LENGTH: usize = 1024;

/// Linux ABI: failures return `-errno` in RAX (as unsigned). musl's `__syscall_ret` treats
/// `r > -4096UL` as error and sets `errno = (int)-r`. A bare `u64::MAX` becomes errno **1 (EPERM)**,
/// so missing files look like "Operation not permitted" instead of ENOENT.
#[inline]
fn linux_abi_error(errno: i32) -> u64 {
    if errno <= 0 || errno >= 4096 {
        u64::MAX
    } else {
        (-errno) as u64
    }
}

#[inline]
fn syscall_error_for_current_process(errno: i32) -> u64 {
    match crate::process::current_process_id().and_then(|pid| crate::process::get_process(pid)) {
        Some(p) if p.is_linux => linux_abi_error(errno),
        _ => u64::MAX,
    }
}

/// Handler principal de syscalls
pub extern "C" fn syscall_handler(
    syscall_num: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
    arg6: u64,
    context: &mut crate::interrupts::SyscallContext,
) -> u64 {
    let pid = crate::process::current_process_id().unwrap_or(0);
    LAST_SYSCALL_PID.store(pid as u32, Ordering::Relaxed);
    LAST_SYSCALL_NUM.store(syscall_num, Ordering::Relaxed);

    // Stage 1: AI Audit
    if pid != 0 {
        if !crate::ai_core::audit_syscall(pid as u32, syscall_num) {
            return 0xFFFF_FFFF_FFFF_FFFF;
        }
    }

    // Stage 2: Context creation
    let process_context = crate::process::Context {
        rsp: context.rsp,
        rip: context.rip,
        rflags: context.rflags,
        rbp: context.rbp,
        rax: context.rax,
        rbx: context.rbx,
        rcx: context.rcx,
        rdx: context.rdx,
        rsi: context.rsi,
        rdi: context.rdi,
        r8: context.r8,
        r9: context.r9,
        r10: context.r10,
        r11: context.r11,
        r12: context.r12,
        r13: context.r13,
        r14: context.r14,
        r15: context.r15,
    };
    
    // Stage 3: Stats
    let mut stats = SYSCALL_STATS.lock();
    stats.total_calls += 1;
    drop(stats);

    // Stage 4: Tracing
    let trace = crate::process::get_process(pid).map_or(false, |p| p.syscall_trace);
    if trace {
        let p_name = crate::process::get_process(pid).map(|p| {
            let mut n = alloc::string::String::new();
            for &b in p.name.iter() {
                if b == 0 { break; }
                n.push(b as char);
            }
            n
        }).unwrap_or_else(|| alloc::string::String::from("unknown"));
        
        serial::serial_printf(format_args!(
            "[strace] pid={} ({}) call {}({:#x}, {:#x}, {:#x}, {:#x}, {:#x}, {:#x})\n",
            pid, p_name, syscall_num, arg1, arg2, arg3, arg4, arg5, arg6
        ));
    }

    // Stage 5: Dispatch
    let ret = match syscall_num {
        // --- Linux Compatibility Syscalls (x86_64) ---
        0   => sys_read(arg1, arg2, arg3),
        1   => sys_write(arg1, arg2, arg3),
        2   => sys_open(arg1, arg2, arg3),
        3   => sys_close(arg1),
        4   => sys_stat(arg1, arg2),
        5   => sys_fstat(arg1, arg2),
        6   => sys_fstatat(0, arg1, arg2, 0x100), // lstat -> AT_SYMLINK_NOFOLLOW (0x100)
        7   => sys_poll(arg1, arg2, arg3),
        8   => sys_lseek(arg1, arg2 as i64, arg3 as usize),
        9   => sys_mmap(arg1, arg2, arg3, arg4, arg5, arg6),
        // pread64(fd, buf, count, pos): 4th arg is r10 — musl ld.so lo usa al leer ELF/.so
        17  => sys_pread64(arg1, arg2, arg3, arg4),
        10  => sys_mprotect(arg1, arg2, arg3),
        11  => sys_munmap(arg1, arg2),
        12  => sys_brk(arg1),
        13  => sys_sigaction(arg1, arg2, arg3),
        14  => sys_sigprocmask(arg1, arg2, arg3),
        15  => sys_rt_sigreturn(),
        28  => sys_madvise(arg1, arg2, arg3),
        16  => sys_ioctl(arg1, arg2, arg3),
        20  => sys_writev(arg1, arg2, arg3),
        21  => sys_faccessat(0, arg1, arg2, 0),
        22  => sys_pipe(arg1),
        24  => sys_yield(),
        32  => sys_dup(arg1),
        33  => sys_dup2(arg1, arg2),
        34  => sys_pause(),
        35  => sys_nanosleep(arg1),
        39  => sys_getpid(),
        41  => sys_socket(arg1, arg2, arg3),
        42  => sys_connect(arg1, arg2, arg3),
        43  => sys_accept(arg1, arg2, arg3),
        46  => sys_sendmsg(arg1, arg2, arg3),
        47  => sys_recvmsg(arg1, arg2, arg3),
        44  => sys_sendto(arg1, arg2, arg3, arg4, arg5, arg6),
        45  => sys_recvfrom(arg1, arg2, arg3, arg4, arg5, arg6),
        48  => sys_shutdown(arg1, arg2),
        49  => sys_bind(arg1, arg2, arg3),
        50  => sys_listen(arg1, arg2),
        51  => sys_getsockname(arg1, arg2, arg3),
        52  => sys_getpeername(arg1, arg2, arg3),
        54  => sys_setsockopt(arg1, arg2, arg3, arg4, arg5),
        55  => sys_getsockopt(arg1, arg2, arg3, arg4, arg5),
        56  => sys_clone(arg1, arg2, arg3, &process_context),
        57  => sys_fork(&process_context),
        58  => sys_fork(&process_context), // vfork: behave as fork
        59  => sys_execve(arg1, arg2, arg3),
        60  => sys_exit(arg1),
        61  => sys_wait4_linux(arg1, arg2, arg3),
        62  => sys_kill(arg1, arg2),
        63  => sys_uname(arg1),
        72  => sys_fcntl(arg1, arg2, arg3),
        73  => sys_flock(arg1, arg2),
        74  => sys_fsync(arg1),
        75  => sys_fdatasync(arg1),
        76  => sys_truncate(arg1, arg2),
        77  => sys_ftruncate(arg1, arg2),
        78  => sys_getdents64(arg1, arg2, arg3), // getdents (same format as getdents64 in Eclipse)
        79  => sys_getcwd(arg1, arg2),
        80  => sys_chdir(arg1),
        81  => sys_fchdir(arg1),
        82  => sys_rename(arg1, arg2),
        83  => sys_mkdir(arg1, arg2),
        84  => sys_rmdir(arg1),
        85  => sys_creat(arg1, arg2),
        86  => sys_link(arg1, arg2),
        87  => sys_unlink(arg1),
        88  => sys_symlink(arg1, arg2),
        89  => sys_readlink(arg1, arg2, arg3),
        90  => sys_chmod(arg1, arg2),
        91  => sys_fchmod(arg1, arg2),
        92  => sys_chown(arg1, arg2, arg3),
        93  => sys_fchown(arg1, arg2, arg3),
        94  => sys_lchown(arg1, arg2, arg3),
        95  => sys_umask(arg1),
        96  => sys_gettimeofday(arg1, arg2),
        97  => sys_getrlimit(arg1, arg2),
        98  => sys_getrusage(arg1, arg2),
        99  => sys_sysinfo(arg1),
        100 => linux_abi_error(38), // times — ENOSYS stub
        102 => sys_getuid(),
        103 => linux_abi_error(38), // syslog — ENOSYS stub
        104 => sys_getgid(),
        105 => sys_setuid(arg1),
        106 => sys_setgid(arg1),
        107 => sys_geteuid(),
        108 => sys_getegid(),
        109 => sys_setpgid(arg1, arg2),
        110 => sys_getppid(),
        111 => sys_getpgrp(),
        112 => sys_setsid(),
        113 => sys_setreuid(arg1, arg2),
        114 => sys_setregid(arg1, arg2),
        115 => sys_setresuid(arg1, arg2, arg3),
        117 => sys_setresgid(arg1, arg2, arg3),
        118 => sys_getresuid(arg1, arg2, arg3),
        120 => sys_getresgid(arg1, arg2, arg3),
        121 => sys_getpgid(arg1),
        131 => sys_sigaltstack(arg1, arg2),
        158 => sys_arch_prctl(arg1, arg2),
        // clock_nanosleep(clockid, flags, req, rem) — usado por std/musl para sleep.
        // En Eclipse tratamos esto como nanosleep(req) e ignoramos clockid/flags/rem.
        230 => sys_nanosleep(arg3),
        170 => sys_sethostname(arg1, arg2),
        186 => sys_gettid(),
        200 => sys_tkill(arg1, arg2),
        202 => sys_futex(arg1, arg2, arg3, arg4, arg5, arg6 as u32),
        218 => sys_set_tid_address(arg1),
        217 => sys_getdents64(arg1, arg2, arg3),
        228 => sys_clock_gettime(arg1, arg2),
        231 => sys_exit(arg1), // Linux exit_group
        247 => sys_getlogin(arg1, arg2),
        // openat(dirfd, path, flags, mode): mode en r10 — imprescindible para musl (cargar .so)
        257 => sys_openat(arg1, arg2, arg3, arg4),
        262 => sys_fstatat(arg1, arg2, arg3, arg4),
        269 => sys_faccessat(arg1, arg2, arg3, arg4),
        270 => sys_pselect6(arg1, arg2, arg3, arg4, arg5, arg6),
        292 => sys_dup3(arg1, arg2, arg3),
        293 => sys_pipe2(arg1, arg2),
        302 => sys_prlimit64(arg1, arg2, arg3, arg4),
        318 => sys_getrandom(arg1, arg2, arg3),
        319 => sys_memfd_create(arg1, arg2),
        324 => sys_membarrier(arg1, arg2, arg3),
        439 => sys_faccessat(arg1, arg2, arg3, arg4),

        // --- Eclipse Native Syscalls (500+) ---
        500 => sys_send(arg1, arg2, arg3, arg4),
        501 => sys_receive(arg1, arg2, arg3),
        502 => sys_get_service_binary(arg1, arg2, arg3),
        503 => sys_get_framebuffer_info(arg1),
        504 => sys_map_framebuffer(),
        505 => sys_pci_enum_devices(arg1, arg2, arg3),
        506 => sys_pci_read_config(arg1, arg2, arg3),
        507 => sys_pci_write_config(arg1, arg2, arg3),
        508 => sys_register_device(arg1, arg2, arg3),
        509 => sys_fmap(arg1, arg2, arg3),
        510 => sys_mount(arg1, arg2),
        511 => sys_spawn(arg1, arg2, arg3),
        512 => sys_get_last_exec_error(arg1, arg2),
        513 => sys_read_key(),
        514 => sys_read_mouse_packet(),
        515 => sys_get_gpu_display_info(arg1),
        516 => sys_set_cursor_position(arg1, arg2),
        517 => sys_gpu_alloc_display_buffer(arg1, arg2, arg3),
        518 => sys_gpu_present(arg1, arg2, arg3, arg4, arg5),
        250 => sys_gpu_present(arg1, arg2, arg3, arg4, arg5), // Legacy alias for compatibility
        519 => sys_get_logs(arg1, arg2),
        520 => sys_get_storage_device_count(),
        521 => sys_get_system_stats(arg1),
        522 => sys_get_process_list(arg1, arg2),
        523 => sys_set_process_name(arg1, arg2),
        524 => sys_spawn_service(arg1, arg2, arg3),
        525 => sys_gpu_command(arg1, arg2, arg3, arg4),
        526 => sys_stop_progress(),
        527 => sys_gpu_get_backend(),
        528 => sys_drm_page_flip(arg1),
        529 => sys_drm_get_caps(arg1),
        530 => sys_drm_alloc_buffer(arg1),
        531 => sys_drm_create_fb(arg1, arg2, arg3, arg4),
        532 => sys_drm_map_handle(arg1),
        533 => sys_sched_setaffinity(arg1, arg2),
        534 => sys_register_log_hud(arg1),
        535 => sys_set_time(arg1),
        536 => sys_spawn_with_stdio(arg1, arg2, arg3, arg4, arg5, arg6),
        537 => sys_thread_create(arg1, arg2, arg3, &process_context),
        538 => sys_wait_pid(arg1, arg2, arg3),
        539 => sys_readdir(arg1, arg2, arg3),
        540 => sys_unlink(arg1),
        541 => sys_mkdir(arg1, arg2),
        542 => sys_spawn_with_stdio_args(arg1, arg2, arg3, arg4, arg5, arg6, context),
        543 => sys_get_process_args(arg1, arg2),
        544 => sys_spawn_with_stdio_path(arg1, arg2, arg3, arg4, arg5, arg6),
        545 => sys_strace(arg1, arg2),
        // Eclipse-native exec: replace current process with a raw ELF buffer.
        // Syscall 59 is the Linux-compatible execve(path, argv, envp); this slot
        // keeps the original Eclipse API (elf_ptr: u64, elf_size: u64) alive.
        53  => sys_socketpair(arg1, arg2, arg3, arg4),
        232 => sys_epoll_wait(arg1, arg2, arg3, arg4),
        233 => sys_epoll_ctl(arg1, arg2, arg3, arg4),
        282 => sys_signalfd4(arg1, arg2, arg3, arg4),
        283 => sys_timerfd_create(arg1, arg2),
        286 => sys_timerfd_settime(arg1, arg2, arg3, arg4),
        287 => sys_timerfd_gettime(arg1, arg2),
        254 => sys_inotify_add_watch(arg1, arg2, arg3),
        294 => sys_inotify_init1(arg1),
        289 => sys_pipe2(arg1, arg2),
        290 => sys_eventfd2(arg1, arg2),
        291 => sys_epoll_create1(arg1),
        546 => sys_exec(arg1, arg2),
        600 => sys_receive_fast(context),
        _ => {
            serial::serial_printf(format_args!(
                "[SYSCALL] Unknown syscall: {}{} from process {} on CPU {}\n",
                syscall_num,
                if syscall_num < 500 { " (Linux Range)" } else { " (Eclipse Range)" },
                crate::process::current_process_id().unwrap_or(0),
                crate::process::get_cpu_id()
            ));
            if syscall_num < 500 {
                linux_abi_error(38) // ENOSYS
            } else {
                u64::MAX
            }
        }
    };
    
    if trace {
        serial::serial_printf(format_args!("[strace] pid={} returns {:#x}\n", pid, ret));
    }

    context.rax = ret;

    // Entregar señales pendientes antes de volver a userspace (no reentrar tras exit).
    if syscall_num != 60 && syscall_num != 231 {
        crate::process::deliver_pending_signals_for_current();
    }

    ret
}


/// sys_get_system_stats - Obtener estadísticas globales del sistema
fn sys_get_system_stats(stats_ptr: u64) -> u64 {
    if stats_ptr == 0 || !is_user_pointer(stats_ptr, core::mem::size_of::<SystemStats>() as u64) {
        return u64::MAX;
    }

    let sched_stats = crate::scheduler::get_stats();
    let (pool_total_mem, pool_used_mem) = crate::memory::get_memory_stats();

    // Report "RAM total" from UEFI conventional memory computed by the bootloader.
    // This fixes the dashboard label showing the pool capacity instead of real RAM.
    let boot_bi = crate::boot::get_boot_info();
    let total_mem = if boot_bi.conventional_mem_total_bytes > 0 {
        (boot_bi.conventional_mem_total_bytes / 4096).max(1)
    } else {
        pool_total_mem
    };
    let used_mem = pool_used_mem.min(total_mem);
    crate::nvidia::update_all_gpu_vitals();
    let vitals = crate::ai_core::get_vitals();

    let stats = SystemStats {
        uptime_ticks: sched_stats.total_ticks,
        idle_ticks: sched_stats.idle_ticks,
        total_mem_frames: total_mem,
        used_mem_frames: used_mem,
        cpu_count: crate::cpu::get_active_cpu_count() as u64,
        cpu_temp: vitals.cpu_temp,
        gpu_load: vitals.gpu_load,
        gpu_temp: vitals.gpu_temp,
        gpu_vram_total_bytes: vitals.gpu_vram_total_bytes,
        gpu_vram_used_bytes: vitals.gpu_vram_used_bytes,
        anomaly_count: vitals.anomaly_count,
        heap_fragmentation: vitals.heap_fragmentation,
        wall_time_offset: WALL_TIME_OFFSET.load(Ordering::Relaxed),
    };

    unsafe {
        core::ptr::write_unaligned(stats_ptr as *mut SystemStats, stats);
    }

    0
}

/// sys_set_time - Establecer el tiempo real del sistema (ajusta el offset)
fn sys_set_time(secs: u64) -> u64 {
    let uptime_ms = crate::scheduler::get_stats().total_ticks;
    let offset = secs.saturating_sub(uptime_ms / 1000);
    WALL_TIME_OFFSET.store(offset, Ordering::Relaxed);
    0
}

/// sys_get_process_list - Listar procesos y su estado (PID, nombre, etc)
fn sys_get_process_list(buf_ptr: u64, max_count: u64) -> u64 {
    if buf_ptr == 0 || max_count == 0 || max_count > 256 {
        return u64::MAX;
    }
    if !is_user_pointer(buf_ptr, max_count * core::mem::size_of::<ProcessInfo>() as u64) {
        return u64::MAX;
    }

    let mut count = 0;
    
    x86_64::instructions::interrupts::without_interrupts(|| {
        let table = crate::process::PROCESS_TABLE.lock();
        for slot in table.iter() {
            if let Some(p) = slot {
                if count >= max_count as usize { break; }
                
                let info = ProcessInfo {
                    pid: p.id,
                    state: p.state as u32,
                    name: p.name,
                    cpu_ticks: p.cpu_ticks,
                    mem_frames: p.mem_frames,
                };
                
                unsafe {
                    core::ptr::write_unaligned((buf_ptr as *mut ProcessInfo).add(count), info);
                }
                count += 1;
            }
        }
    });
    
    count as u64
}

/// sys_strace - Habilitar/deshabilitar rastreo de syscalls
fn sys_strace(pid: u64, enable: u64) -> u64 {
    let target_pid = if pid == 0 {
        crate::process::current_process_id().unwrap_or(0)
    } else {
        pid as crate::process::ProcessId
    };

    if let Some(mut p) = crate::process::get_process(target_pid) {
        p.syscall_trace = enable != 0;
        crate::process::update_process(target_pid, p);
        0
    } else {
        u64::MAX
    }
}

/// sys_kill - Terminar un proceso por su PID
/// sys_kill(pid, sig) — terminar o señalizar un proceso.
/// SIGKILL (9) y SIGTERM (15) terminan al proceso; otras señales van a la cola pendiente.
/// `sig == 0`: comprobación de existencia (POSIX), sin enviar señal.
fn sys_kill(pid: u64, sig: u64) -> u64 {
    if pid == 0 || pid == 1 {
        return u64::MAX; // No se puede matar al kernel ni al init
    }

    let target_pid = pid as crate::process::ProcessId;

    if sig == 0 {
        return if crate::process::get_process(target_pid).is_some() {
            0
        } else {
            u64::MAX
        };
    }

    // Señales no fatales: entregarlas como pendientes (se aplican al volver de syscall).
    if sig != 9 && sig != 15 {
        crate::process::set_pending_signal(target_pid, sig as u8);
        return 0;
    }

    serial::serial_printf(format_args!("[KILL] pid={} sig={}\n", target_pid, sig));

    let parent_pid = match crate::process::terminate_other_process_by_signal(target_pid, sig as u8) {
        None => return u64::MAX,
        Some(pp) => pp,
    };

    if let Some(ppid) = parent_pid {
        crate::process::wake_parent_from_wait(ppid);
    }

    0
}

/// sys_set_process_name - Cambiar el nombre del proceso actual
fn sys_set_process_name(name_ptr: u64, name_len: u64) -> u64 {
    if name_ptr == 0 || !is_user_pointer(name_ptr, 1) {
        return u64::MAX;
    }

    let mut name_buf = [0u8; 16];
    let len = core::cmp::min(name_len as usize, 15);
    for i in 0..len {
        if !is_user_pointer(name_ptr + i as u64, 1) { break; }
        let b = unsafe { *( (name_ptr + i as u64) as *const u8 ) };
        if b == 0 { break; }
        name_buf[i] = b;
    }

    if let Some(pid) = crate::process::current_process_id() {
        if let Some(mut p) = crate::process::get_process(pid) {
            p.name = name_buf;
            crate::process::update_process(pid, p);
            return 0;
        }
    }

    u64::MAX
}

/// sys_spawn_service - Spawn a system service process by embedded binary ID.
/// arg1: service_id (0 = log, 1 = devfs, 2 = filesystem, 3 = input, 4 = display,
///                   5 = audio, 6 = network, 7 = gui)
/// arg2: pointer to name string in user space (optional, 0 = derive from service_id)
/// arg3: name length
/// Returns: PID of new service process on success, u64::MAX on error.
///
/// This is the preferred way for init to start services. It avoids the fork+exec
/// overhead and directly creates a clean process from the embedded kernel binary.
use alloc::vec::Vec;

// Cache de binarios de servicios leídos desde el filesystem (/sbin/*).
static mut SERVICE_LOG_BIN: Option<Vec<u8>> = None;
static mut SERVICE_DEVFS_BIN: Option<Vec<u8>> = None;
static mut SERVICE_FS_BIN: Option<Vec<u8>> = None;
static mut SERVICE_INPUT_BIN: Option<Vec<u8>> = None;
static mut SERVICE_DISPLAY_BIN: Option<Vec<u8>> = None;
static mut SERVICE_AUDIO_BIN: Option<Vec<u8>> = None;
static mut SERVICE_NET_BIN: Option<Vec<u8>> = None;
static mut SERVICE_GUI_BIN: Option<Vec<u8>> = None;
static mut SERVICE_SEATD_BIN: Option<Vec<u8>> = None;

static SERVICE_BIN_LOCK: spin::Mutex<()> = spin::Mutex::new(());

fn get_service_slice(service_id: u64) -> Option<&'static [u8]> {
    // Acquire a global lock during the load check/filesystem read to prevent
    // race conditions on SMP systems where multiple CPUs try to load the
    // same service binary simultaneously.
    let _guard = SERVICE_BIN_LOCK.lock();

    unsafe {
        let (slot, path) = match service_id {
            x if x == svc::LOG as u64 => (&mut SERVICE_LOG_BIN, svc::PATH_LOG),
            x if x == svc::DEVFS as u64 => (&mut SERVICE_DEVFS_BIN, svc::PATH_DEVFS),
            x if x == svc::FILESYSTEM as u64 => (&mut SERVICE_FS_BIN, svc::PATH_FILESYSTEM),
            x if x == svc::INPUT as u64 => (&mut SERVICE_INPUT_BIN, svc::PATH_INPUT),
            x if x == svc::DISPLAY as u64 => (&mut SERVICE_DISPLAY_BIN, svc::PATH_DISPLAY),
            x if x == svc::AUDIO as u64 => (&mut SERVICE_AUDIO_BIN, svc::PATH_AUDIO),
            x if x == svc::NETWORK as u64 => (&mut SERVICE_NET_BIN, svc::PATH_NETWORK),
            x if x == svc::GUI as u64 => (&mut SERVICE_GUI_BIN, svc::PATH_GUI),
            x if x == svc::SEATD as u64 => (&mut SERVICE_SEATD_BIN, svc::PATH_SEATD),
            _ => return None,
        };

        // Si el cache está vacío (Vec len=0) tratamos el slot como inválido.
        // En hardware real puede ocurrir que un arranque anterior deje un slot en estado
        // inconsistente (por ejemplo por cargas parciales) y entonces `spawn_service`
        // falla sin intentar el fallback a disco.
        let cached_len = slot.as_ref().map(|v| v.len()).unwrap_or(0);

        if slot.is_none() || cached_len == 0 {
            match crate::filesystem::read_file_alloc(path) {
                Ok(buf) => {
                    *slot = Some(buf);
                }
                Err(e) => {
                    serial::serial_printf(format_args!(
                        "[SYSCALL] ERROR loading service {}: {}\n",
                        path,
                        e
                    ));
                    return None;
                }
            }
        }

        slot.as_ref().map(|v| v.as_slice())
    }
}

fn sys_spawn_service(service_id: u64, name_ptr: u64, name_len: u64) -> u64 {
    // Derive the on-disk path for this service ID.
    let path = match service_id {
        x if x == svc::LOG as u64       => svc::PATH_LOG,
        x if x == svc::DEVFS as u64     => svc::PATH_DEVFS,
        x if x == svc::FILESYSTEM as u64 => svc::PATH_FILESYSTEM,
        x if x == svc::INPUT as u64     => svc::PATH_INPUT,
        x if x == svc::DISPLAY as u64   => svc::PATH_DISPLAY,
        x if x == svc::AUDIO as u64     => svc::PATH_AUDIO,
        x if x == svc::NETWORK as u64   => svc::PATH_NETWORK,
        x if x == svc::GUI as u64       => svc::PATH_GUI,
        x if x == svc::SEATD as u64     => svc::PATH_SEATD,
        _ => {
            serial::serial_print("[SYSCALL] spawn_service: invalid service_id\n");
            return u64::MAX;
        }
    };

    // Load the binary transiently – no persistent kernel-heap cache.
    // The Vec is dropped at the end of this function, reclaiming the memory
    // immediately after the new process has been created.
    let elf_data: Vec<u8> = match crate::filesystem::read_file_alloc(path) {
        Ok(buf) => buf,
        Err(e) => {
            serial::serial_printf(format_args!(
                "[SYSCALL] spawn_service: failed to load {}: {}\n",
                path, e
            ));
            return u64::MAX;
        }
    };

    // Read the optional name from user space
    let mut name_buf = [0u8; 16];
    if name_ptr != 0 && name_len > 0 {
        let copy_len = (name_len as usize).min(15);
        if is_user_pointer(name_ptr, copy_len as u64) {
            unsafe {
                core::ptr::copy_nonoverlapping(name_ptr as *const u8, name_buf.as_mut_ptr(), copy_len);
            }
        }
    }
    let name_str = core::str::from_utf8(&name_buf).unwrap_or("");
    let name_trimmed = if name_str.trim_matches('\0').is_empty() {
        eclipse_program_codes::spawn_service_short_name(service_id as u32)
    } else {
        name_str.trim_matches('\0')
    };

    let result = match crate::process::spawn_process(&elf_data, name_trimmed) {
        Ok(pid) => {
            // Set parent_pid so init can wait() for the child
            if let Some(caller_pid) = current_process_id() {
                if let Some(mut child) = crate::process::get_process(pid) {
                    child.parent_pid = Some(caller_pid);
                    crate::process::update_process(pid, child);
                }
            }

            crate::scheduler::enqueue_process(pid);

            pid as u64
        }
        Err(e) => {
            serial::serial_printf(format_args!("[SYSCALL] spawn_service failed: {}\n", e));
            u64::MAX
        }
    };
    // elf_data is dropped here when the function returns, freeing the kernel-heap
    // allocation immediately rather than keeping it cached indefinitely.
    result
}

/// sys_get_logs - Obtener los últimos logs del kernel (para el HUD del compositor)
fn sys_get_logs(buf_ptr: u64, buf_len: u64) -> u64 {
    if buf_ptr == 0 || buf_len == 0 || buf_len > 4096 {
        return u64::MAX;
    }
    if !is_user_pointer(buf_ptr, buf_len) {
        return u64::MAX;
    }
    
    let mut tmp = [0u8; 1024];
    let n = crate::progress::get_logs(&mut tmp);
    let copy_len = core::cmp::min(n, buf_len as usize);
    
    unsafe {
        core::ptr::copy_nonoverlapping(tmp.as_ptr(), buf_ptr as *mut u8, copy_len);
    }
    
    copy_len as u64
}

/// sys_stop_progress - Desactivar logs y barra de progreso del kernel
fn sys_stop_progress() -> u64 {
    crate::progress::stop_logging();
    0
}

/// sys_register_log_hud - Registrar PID que recibirá líneas de log por IPC (p. ej. smithay_app).
/// pid=0 para desregistrar. Llamar cuando el compositor esté listo para mostrar el HUD.
fn sys_register_log_hud(pid: u64) -> u64 {
    crate::progress::set_log_hud_pid(pid as u32);
    0
}

/// sys_drm_page_flip - Perform an atomic page flip (KMS)
/// arg1: fb_id
/// Returns 0 on success, u64::MAX on failure
fn sys_drm_page_flip(fb_id: u64) -> u64 {
    if crate::drm::page_flip(fb_id as u32) {
        0
    } else {
        u64::MAX
    }
}

/// sys_drm_get_caps - Get DRM capabilities
fn sys_drm_get_caps(caps_ptr: u64) -> u64 {
    if caps_ptr == 0 || !is_user_pointer(caps_ptr, core::mem::size_of::<crate::drm::DrmCaps>() as u64) {
        return u64::MAX;
    }
    if let Some(caps) = crate::drm::get_caps() {
        unsafe {
             core::ptr::copy_nonoverlapping(&caps as *const _ as *const u8, caps_ptr as *mut u8, core::mem::size_of::<crate::drm::DrmCaps>());
        }
        0
    } else {
        u64::MAX
    }
}

/// sys_drm_alloc_buffer - Allocate a GEM buffer
fn sys_drm_alloc_buffer(size: u64) -> u64 {
    if size == 0 || size > crate::drm::MAX_GEM_BUFFER_SIZE as u64 {
        return u64::MAX;
    }
    if let Some(handle) = crate::drm::alloc_buffer(size as usize) {
        handle.id as u64
    } else {
        u64::MAX
    }
}

/// sys_drm_create_fb - Create a DRM framebuffer
fn sys_drm_create_fb(handle: u64, width: u64, height: u64, pitch: u64) -> u64 {
    if let Some(fb_id) = crate::drm::create_fb(handle as u32, width as u32, height as u32, pitch as u32) {
        fb_id as u64
    } else {
        u64::MAX
    }
}

/// sys_drm_map_fb - Map a DRM framebuffer into userspace
fn sys_drm_map_handle(handle_id: u64) -> u64 {
    let handle = match crate::drm::get_handle(handle_id as u32) {
        Some(h) => h,
        None => return u64::MAX,
    };
    
    let current_pid = match crate::process::current_process_id() {
        Some(pid) => pid,
        None => return u64::MAX,
    };
    
    if let Some(mut proc) = crate::process::get_process(current_pid) {
        let mut r = proc.resources.lock();
        
        let aligned_length = (handle.size + 0xFFF) & !0xFFF;
        
        // Find a free VMA spot (0x60000000 to 0x70000000).
        // Jump to the end of any overlapping VMA instead of scanning page-by-page.
        let span = aligned_length as u64;
        let mut candidate: u64 = 0x60000000;
        let target_addr = loop {
            if candidate >= 0x70000000 {
                return u64::MAX;
            }
            let next = r.vmas.iter()
                .filter(|vma| candidate < vma.end && candidate.saturating_add(span) > vma.start)
                .map(|vma| vma.end)
                .max();
            match next {
                None => break candidate,
                Some(end) => candidate = end,
            }
        };
        
        // Map the physical frames of the GEM handle directly to the found userspace address.
        // Use Write-Combining (WC) caching: PWT=1, PCD=0 selects PAT index 1 which is
        // configured as WC in memory::init_pat() (see eclipse_kernel/src/memory.rs).
        // WC is optimal for write-only framebuffer access patterns (avoids cache thrashing).
        use x86_64::structures::paging::PageTableFlags;
        let flags = (PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE | PageTableFlags::WRITE_THROUGH).bits();
        
        crate::memory::map_physical_range(
            r.page_table_phys,
            handle.phys_addr,
            aligned_length as u64,
            target_addr,
            flags
        );
        
        serial::serial_printf(format_args!("[SYSCALL] drm_map_handle: pid={} handle={} addr={:#x} length={}\n", current_pid, handle_id, target_addr, aligned_length));

        // Record the VMA
        r.vmas.push(crate::process::VMARegion {
            start: target_addr,
            end: target_addr + aligned_length as u64,
            flags: flags, 
            file_backed: false,
            anon_kernel_slack: 0,
        });
        
        proc.mem_frames += (aligned_length as u64 + 4095) / 4096;
        drop(r);
        crate::process::update_process(current_pid, proc);
        
        return target_addr;
    }
    
    u64::MAX
}

/// sys_sched_setaffinity - Fijar afinidad de CPU para un proceso
/// pid=0 significa el proceso actual. cpu_id=u32::MAX significa cualquier CPU (quitar afinidad).
fn sys_sched_setaffinity(pid: u64, cpu_id: u64) -> u64 {
    use crate::process::NO_CPU;
    use crate::scheduler::MAX_CPUS;

    let target_pid = if pid == 0 {
        match crate::process::current_process_id() {
            Some(p) => p,
            None => return u64::MAX,
        }
    } else {
        pid as u32
    };

    let affinity = if cpu_id == u64::from(NO_CPU) || cpu_id >= MAX_CPUS as u64 {
        None
    } else {
        Some(cpu_id as u32)
    };

    x86_64::instructions::interrupts::without_interrupts(|| {
        if let Some(slot) = crate::ipc::pid_to_slot_fast(target_pid) {
            let mut table = crate::process::PROCESS_TABLE.lock();
            if let Some(p) = table[slot].as_mut() {
                if p.id == target_pid {
                    p.cpu_affinity = affinity;
                    return 0u64;
                }
            }
        }
        u64::MAX
    })
}

/// Length of null-terminated string in user space (max max_len bytes).
fn strlen_user_unique(path_ptr: u64, max_len: usize) -> u64 {
    if path_ptr == 0 || max_len == 0 {
        return 0;
    }
    for i in 0..max_len {
        if !is_user_pointer(path_ptr.wrapping_add(i as u64), 1) {
            return i as u64;
        }
        let b = unsafe { *((path_ptr as usize + i) as *const u8) };
        if b == 0 {
            return i as u64;
        }
    }
    max_len as u64
}

/// Rutas absolutas de usuario (`/…`) → `file:…`; `/dev/…` → `dev:…`; `/sys/…` → `sys:…`.
fn user_path_to_scheme_path(path_str: &str) -> String {
    if path_str == "/dev/dri" || path_str.starts_with("/dev/dri/") {
        let rel = path_str.trim_start_matches("/dev/dri").trim_start_matches('/');
        format!("drm:{}", rel)
    } else if path_str == "/dev/input" || path_str.starts_with("/dev/input/") {
        let rel = path_str.trim_start_matches("/dev/input").trim_start_matches('/');
        format!("input:{}", rel)
    } else if path_str == "/dev" || path_str.starts_with("/dev/") {
        let rel = path_str.trim_start_matches("/dev").trim_start_matches('/');
        format!("dev:{}", rel)
    } else if path_str == "/sys" || path_str.starts_with("/sys/") {
        let rel = path_str.trim_start_matches("/sys").trim_start_matches('/');
        format!("sys:{}", rel)
    } else if path_str.starts_with('/') {
        format!("file:{}", path_str)
    } else {
        String::from(path_str)
    }
}

/// Translate Linux x86_64 syscall ABI to Eclipse numbers (for static glibc binaries like Xfbdev).
/// Returns (eclipse_syscall_num, arg1, arg2, arg3, arg4, arg5) or None if not a known Linux syscall.
// translate_linux_abi_unique removed as it is no longer necessary after syscall alignment.

/// Verify if a pointer range points to valid user memory
/// User memory range: 0x0000_0000_0000_0000 to 0x0000_7FFF_FFFF_FFFF
#[inline(never)]
pub fn is_user_pointer(ptr: u64, len: u64) -> bool {
    // Check for null pointer or pointers in the first page (usually unmapped)
    if ptr < 0x1000 {
        return false;
    }
    
    // Check for overflow
    let end = match ptr.checked_add(len) {
        Some(e) => e,
        None => return false,
    };
    
    // Check upper bound (Canonical lower half)
    if end > 0x0000_8000_0000_0000 {
        serial::serial_printf(format_args!(
            "[SYSCALL] is_user_pointer failed: {:#X} len {} end {:#X}\n",
            ptr, len, end
        ));
        return false;
    }
    
    // Additional alignment checks could be added here
    
    true
}

/// sys_exit - Terminar proceso actual
fn sys_exit(exit_code: u64) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.exit_calls += 1;
    drop(stats);
    
    let pid = current_process_id().unwrap_or(0);

    serial::serial_printf(format_args!("[EXIT] pid={} code={}\n", pid, exit_code));

    // Store the exit code in the PCB so sys_wait() can report it to the parent.
    if let Some(mut proc) = crate::process::get_process(pid) {
        proc.exit_code = exit_code;
        crate::process::update_process(pid, proc);
    }

    exit_process();
    yield_cpu();
    0
}

/// sys_write - Write to a file descriptor
/// 
/// STATUS: Partially implemented
/// - stdout/stderr (fd 1,2): ✅ Working - writes to serial
/// - Regular files (fd 3+): ⚠️ Tracked but not persisted to disk
/// 
/// sys_write - Write to a file descriptor (IMPLEMENTED)
/// 
/// STATUS: Fully implemented ✅
/// - stdout/stderr (fd 1,2): ✅ Working - writes to serial
/// - Regular files (fd 3+): ✅ Working - writes persisted to disk
/// 
/// Writes data to an open file descriptor. For stdout/stderr, output goes to
/// the serial console. For regular files, data is written to the filesystem
/// and persisted to disk.
/// 
/// Limitations:
/// - Cannot extend files beyond current size
/// - No block allocation for file growth
/// - Writes limited to existing file content length
/// 
/// TODO:
/// - Implement file extension (allocate new blocks)
/// - Implement block allocation for growing files
/// - Add inode metadata updates (mtime, size)
fn sys_write(fd: u64, buf_ptr: u64, len: u64) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.write_calls += 1;
    drop(stats);
    
    // Validate parameters
    // A zero-length write/read is valid: return 0 (Linux behaviour).
    if len == 0 {
        return 0;
    }
    if buf_ptr == 0 || len > 1024 * 1024 {
        return linux_abi_error(22); // EINVAL
    }

    if !is_user_pointer(buf_ptr, len) {
        return linux_abi_error(14); // EFAULT
    }
    
    // File descriptor routing
    // La salida estándar (fd 1 y 2) ya está inicializada hacia "log:" vía fd_init_stdio,
    // así que no necesitamos hardcodear llamadas serial_print que romperían los pipes/pty.

    if let Some(pid) = current_process_id() {
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            // SAFETY: Use a kernel-space bounce buffer to avoid direct driver access to user memory.
            let mut total = 0usize;
            while total < len as usize {
                let chunk_len = core::cmp::min((len as usize) - total, 4096);
                let mut bounce = [0u8; 4096];
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        (buf_ptr as *const u8).add(total),
                        bounce.as_mut_ptr(),
                        chunk_len,
                    );
                }
                
                let offset = fd_entry.offset;
                match crate::scheme::write(fd_entry.scheme_id, fd_entry.resource_id, &bounce[..chunk_len], offset + total as u64) {
                    Ok(n) => {
                        total += n;
                        crate::fd::fd_update_offset(pid, fd as usize, offset + total as u64);
                        if n < chunk_len { break; } // Short write
                    }
                    Err(e) => {
                        return if total > 0 { total as u64 } else { linux_abi_error(e as i32) };
                    }
                }
            }
            return total as u64;
        }
    }
    
    linux_abi_error(9) // EBADF
}

/// sys_read - Leer de un file descriptor (IMPLEMENTED)
fn sys_read(fd: u64, buf_ptr: u64, len: u64) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.read_calls += 1;
    drop(stats);
    
    // A zero-length read is valid: return 0 (Linux behaviour).
    if len == 0 {
        return 0;
    }
    if buf_ptr == 0 || len > 32 * 1024 * 1024 {
        return linux_abi_error(22); // EINVAL
    }
    
    if !is_user_pointer(buf_ptr, len) {
        return linux_abi_error(14); // EFAULT
    }
    
    if let Some(pid) = current_process_id() {
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            let offset = fd_entry.offset;
            let mut bounce = [0u8; 4096]; // 4 KB bounce buffer on stack (safer)
            let read_len = core::cmp::min(len as usize, bounce.len());

            match crate::scheme::read(fd_entry.scheme_id, fd_entry.resource_id, &mut bounce[..read_len], offset) {
                Ok(bytes_read) => {
                    if bytes_read > 0 {
                        unsafe {
                            core::ptr::copy_nonoverlapping(bounce.as_ptr(), buf_ptr as *mut u8, bytes_read);
                        }
                    }
                    crate::fd::fd_update_offset(pid, fd as usize, offset + bytes_read as u64);
                    
                    if pid == 10 || (bytes_read == 0 && pid >= 10 && pid <= 20) {
                         serial::serial_printf(format_args!("[SYSCALL] read(fd={}) returned {} bytes at offset {} (pid={}, scheme={}, resource={})\n", fd, bytes_read, offset, pid, fd_entry.scheme_id, fd_entry.resource_id));
                    }
                    return bytes_read as u64;
                },
                Err(e) => {
                    if e != crate::scheme::error::EAGAIN {
                        serial::serial_printf(format_args!("[SYSCALL] read() scheme error: {}\n", e));
                    }
                    return (-(e as isize)) as u64;
                }
            }
        } else {
            serial::serial_print("[SYSCALL] read() failed: FD not found\n");
        }
    }
    
    u64::MAX
}

/// pread64(fd, buf, count, offset) — lectura sin avanzar el offset del descriptor (ld-musl / libc).
fn sys_pread64(fd: u64, buf_ptr: u64, count: u64, offset: u64) -> u64 {
    if buf_ptr == 0 || count == 0 || count > 32 * 1024 * 1024 {
        return syscall_error_for_current_process(crate::scheme::error::EINVAL as i32);
    }
    if !is_user_pointer(buf_ptr, count) {
        return syscall_error_for_current_process(crate::scheme::error::EFAULT as i32);
    }
    let Some(pid) = current_process_id() else {
        return syscall_error_for_current_process(crate::scheme::error::ESRCH as i32);
    };
    let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) else {
        return syscall_error_for_current_process(crate::scheme::error::EBADF as i32);
    };
    let scheme_id = fd_entry.scheme_id;
    let resource_id = fd_entry.resource_id;

    let mut total = 0usize;
    while total < count as usize {
        let chunk_len = core::cmp::min((count as usize) - total, 4096);
        let mut bounce = [0u8; 4096];
        
        match crate::scheme::read(scheme_id, resource_id, &mut bounce[..chunk_len], offset + total as u64) {
            Ok(0) => break,
            Ok(n) => {
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        bounce.as_ptr(),
                        (buf_ptr as *mut u8).add(total),
                        n,
                    );
                }
                total += n;
                if n < chunk_len { break; } // Short read
            }
            Err(e) => {
                if e != crate::scheme::error::EAGAIN {
                    crate::serial::serial_printf(format_args!(
                        "[SYSCALL] pread64() scheme error: {}\n",
                        e
                    ));
                }
                return (-(e as isize)) as u64;
            }
        }
    }

    total as u64
}

/// sys_get_last_exec_error - Copy last exec() failure message to user buffer (for init/services)
fn sys_get_last_exec_error(out_ptr: u64, out_len: u64) -> u64 {
    if out_ptr == 0 || out_len == 0 || out_len > 256 {
        return u64::MAX;
    }
    if !is_user_pointer(out_ptr, out_len) {
        return u64::MAX;
    }
    let buf = LAST_EXEC_ERR.lock();
    let mut copy_len = 0;
    while copy_len < buf.len() && buf[copy_len] != 0 {
        copy_len += 1;
    }
    let copy_len = core::cmp::min(copy_len, out_len as usize);
    unsafe {
        core::ptr::copy_nonoverlapping(buf.as_ptr(), out_ptr as *mut u8, copy_len);
        if copy_len < out_len as usize {
            *((out_ptr as *mut u8).add(copy_len)) = 0;
        }
    }
    copy_len as u64
}

/// sys_read_key - Read one scancode from PS/2 keyboard buffer (non-blocking).
/// Returns 0 if buffer empty, otherwise the scancode (1-255).
fn sys_read_key() -> u64 {
    crate::interrupts::read_key() as u64
}

/// sys_read_mouse_packet - Read one PS/2 mouse packet (non-blocking).
/// Returns 0 if buffer empty; otherwise packed u32: buttons | (dx<<8) | (dy<<16), dx/dy sign-extended from i8.
fn sys_read_mouse_packet() -> u64 {
    let p = crate::interrupts::read_mouse_packet();
    if p == 0xFFFFFFFF {
        return u64::MAX; // Empty marker
    }
    p as u64
}

/// sys_ioctl - Device control (FBIOGET_VSCREENINFO, FBIOGET_FSCREENINFO, etc.)
fn sys_ioctl(fd: u64, request: u64, arg: u64) -> u64 {
    if arg >= 0xFFFF800000000000 { // Kernel Higher Half
        crate::serial::serial_print("[SYSCALL] sys_ioctl rejected kernel arg\n");
        return -(crate::scheme::error::EFAULT as isize) as u64;
    }

    if let Some(pid) = current_process_id() {
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            match crate::scheme::ioctl(
                fd_entry.scheme_id,
                fd_entry.resource_id,
                request as usize,
                arg as usize,
            ) {
                Ok(ret) => return ret as u64,
                Err(e) => {
                     if e != crate::scheme::error::EAGAIN && e != crate::scheme::error::ENOSYS {
                         serial::serial_printf(format_args!(
                             "[SYSCALL] sys_ioctl failed: {} for fd {} req {:#018X}\n",
                             e, fd, request
                         ));
                     }
                     return -(e as isize) as u64;
                }
            }
        }
    }
    -(crate::scheme::error::EBADF as isize) as u64
}

/// sys_ftruncate - Change the length of a file
fn sys_ftruncate(fd: u64, length: u64) -> u64 {
    if let Some(pid) = current_process_id() {
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            match crate::scheme::ftruncate(
                fd_entry.scheme_id,
                fd_entry.resource_id,
                length as usize,
            ) {
                Ok(ret) => return ret as u64,
                Err(e) => return -(e as isize) as u64,
            }
        }
    }
    -(crate::scheme::error::EBADF as isize) as u64
}

/// sys_send - Enviar mensaje IPC
/// arg4 = data_len (bytes to copy from data_ptr; max 256)
fn sys_send(server_id: u64, msg_type: u64, data_ptr: u64, data_len: u64) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.send_calls += 1;
    drop(stats);
    
    // Rechazar data_ptr en página nula (evita crash 0x11)
    if data_len > 0 && data_ptr != 0 && data_ptr < 0x1000 {
        return u64::MAX;
    }
    if let Some(client_id) = current_process_id() {
        let message_type = match msg_type {
            1 => MessageType::System,
            255 => MessageType::Signal, // Special signal type for P2P
            2 => MessageType::Memory,
            4 => MessageType::FileSystem,
            8 => MessageType::Network,
            0x10 => MessageType::Graphics,
            0x20 => MessageType::Audio,
            0x40 => MessageType::Input,
            _ => MessageType::User,
        };
        
        const MAX_MSG: usize = 512;
        let len = core::cmp::min(data_len as usize, MAX_MSG);
        let mut data = [0u8; 512];
        if len > 0 && data_ptr != 0 {
            if is_user_pointer(data_ptr, len as u64) {
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        data_ptr as *const u8,
                        data.as_mut_ptr(),
                        len,
                    );
                }
            }
        }
        
        if send_message(client_id, server_id as u32, message_type, &data[..len]) {
            return 0; // Success
        }
    }
    
    u64::MAX // Error
}

/// sys_receive - Recibir mensaje IPC
fn sys_receive(buffer_ptr: u64, size: u64, sender_pid_ptr: u64) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.receive_calls += 1;
    drop(stats);
    
    // Rechazar punteros en página nula (evita crash 0x11 por punteros corruptos)
    if buffer_ptr < 0x1000 || (sender_pid_ptr != 0 && sender_pid_ptr < 0x1000) {
        return u64::MAX;
    }
    if size == 0 || size > 4096 {
        return u64::MAX;
    }
    if !is_user_pointer(buffer_ptr, size) {
        return u64::MAX;
    }
    if sender_pid_ptr != 0 && !is_user_pointer(sender_pid_ptr, 8) {
        return u64::MAX;
    }
    
    if let Some(client_id) = current_process_id() {
        if let Some(msg) = receive_message(client_id) {
            RECV_OK.fetch_add(1, Ordering::Relaxed);
            // Diagnóstico: loguear mensajes recibidos por PID 11 (glxgears).
            if client_id == 11 {
                crate::serial::serial_printf(format_args!(
                    "[RECV-SLOW] glxgears pid=11 got msg data_size={} from={}\n",
                    msg.data_size, msg.from
                ));
            }
            // Calcular cuántos bytes copiar al buffer del usuario
            let data_len = (msg.data_size as usize).min(msg.data.len());
            let copy_len = core::cmp::min(size as usize, data_len);

            unsafe {
                // Copiar datos del mensaje (puede ser 0 bytes, lo cual es válido)
                if copy_len > 0 {
                    let user_buf = core::slice::from_raw_parts_mut(
                        buffer_ptr as *mut u8,
                        copy_len,
                    );
                    user_buf.copy_from_slice(&msg.data[..copy_len]);
                }

                // Escribir el PID del remitente si se solicitó
                if sender_pid_ptr != 0 {
                    if is_user_pointer(sender_pid_ptr, 4) {
                        unsafe {
                            core::ptr::copy_nonoverlapping(&msg.from, sender_pid_ptr as *mut u32, 1);
                        }
                    }
                }
            }
            return copy_len as u64;
        }
        RECV_EMPTY.fetch_add(1, Ordering::Relaxed);
        // Diagnóstico (solo una vez por segundo aproximadamente, usando RECV_EMPTY como throttle).
        if client_id == 11 {
            let empty = RECV_EMPTY.load(Ordering::Relaxed);
            if empty % 50000 == 1 {
                crate::serial::serial_printf(format_args!(
                    "[RECV-EMPTY] glxgears pid=11 mailbox empty (current_pid_from_gs={})\n",
                    client_id
                ));
            }
        }
    }
    0 // No hay mensajes
}

/// sys_receive_fast - Fast path IPC: entrega mensaje pequeño (≤24 bytes) directo en registros.
///
/// Retorna en RAX el data_size (0 = sin mensaje). Si hay mensaje:
///   RDI = data[0..8]  (primer u64 LE)
///   RSI = data[8..16] (segundo u64 LE)
///   RDX = data[16..24] (tercer u64 LE)
///   RCX = sender PID (msg.from)
///
/// Si el siguiente mensaje en el mailbox es > 24 bytes, retorna 0 (usa receive normal).
fn sys_receive_fast(context: &mut crate::interrupts::SyscallContext) -> u64 {
    if let Some(client_id) = current_process_id() {
        if let Some((data_size, from, data)) = pop_small_message_24(client_id) {
            RECV_OK.fetch_add(1, Ordering::Relaxed);
            // Empaquetar data[0..24] en 3 u64 LE (sin Message en stack → menos riesgo de corrupción/#UD)
            let mut w = [0u64; 3];
            for i in 0..3 {
                let off = i * 8;
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&data[off..off + 8]);
                w[i] = u64::from_le_bytes(buf);
            }
            context.rdi = w[0];
            context.rsi = w[1];
            context.rdx = w[2];
            context.rcx = from as u64;
            return data_size as u64;
        }
        RECV_EMPTY.fetch_add(1, Ordering::Relaxed);
    }
    0 // Sin mensaje pequeño disponible → usar receive() normal
}

/// sys_yield - Ceder CPU voluntariamente
fn sys_yield() -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.yield_calls += 1;
    drop(stats);
    
    yield_cpu();
    0
}

/// sys_pause - Suspend calling thread until a signal is received (Linux pause ABI)
fn sys_pause() -> u64 {
    // Basic implementation: yield and return 0. 
    // In a fuller implementation, this would block the thread until its signal queue is non-empty.
    yield_cpu();
    0
}

/// sys_getpid - Obtener PID del proceso actual
fn sys_getpid() -> u64 {
    if let Some(pid) = current_process_id() {
        if let Some(p) = crate::process::get_process(pid) {
            return p.tgid as u64;
        }
    }
    0
}

/// sys_gettid - Get unique thread identifier
fn sys_gettid() -> u64 {
    if let Some(pid) = current_process_id() {
        pid as u64
    } else {
        0
    }
}

/// sys_getppid - Obtener PID del proceso padre
fn sys_getppid() -> u64 {
    use crate::process::{current_process_id, get_process};
    
    if let Some(pid) = current_process_id() {
        if let Some(proc) = get_process(pid) {
            if let Some(ppid) = proc.parent_pid {
                return ppid as u64;
            }
        }
    }
    0
}

/// sys_fork - Create a new process (child)
/// Returns: Child PID in parent, 0 in child, -1 on error
fn sys_fork(context: &crate::process::Context) -> u64 {
    use crate::process;

    let mut stats = SYSCALL_STATS.lock();
    stats.fork_calls += 1;
    drop(stats);

    let linux_abi = process::current_process_id()
        .and_then(process::get_process)
        .map(|p| p.is_linux)
        .unwrap_or(false);

    // Create child process with modified context
    // The child needs to see RAX=0 (return value of fork)
    let mut child_context = *context;
    child_context.rax = 0;

    match process::fork_process(&child_context) {
        Some(child_pid) => {
            crate::scheduler::enqueue_process(child_pid);
            child_pid as u64
        }
        None => {
            serial::serial_print("[SYSCALL] fork() failed - could not create child\n");
            if linux_abi {
                linux_abi_error(11)
            } else {
                u64::MAX
            }
        }
    }
}

/// Release the service-binary cache entry whose data buffer contains `ptr`.
///
/// Called from `sys_exec` after copying out of a kernel-half pointer that was
/// previously handed to userspace by `sys_get_service_binary`.  Once the copy
/// is complete the cache is no longer needed and can be freed to reclaim heap.
fn release_service_binary_containing_ptr(ptr: u64) {
    let _guard = SERVICE_BIN_LOCK.lock();
    unsafe {
        // List every service-binary cache slot.  The compiler infers the array
        // length, so adding a new service only requires adding it here.
        let slots = [
            &raw mut SERVICE_LOG_BIN,
            &raw mut SERVICE_DEVFS_BIN,
            &raw mut SERVICE_FS_BIN,
            &raw mut SERVICE_INPUT_BIN,
            &raw mut SERVICE_DISPLAY_BIN,
            &raw mut SERVICE_AUDIO_BIN,
            &raw mut SERVICE_NET_BIN,
            &raw mut SERVICE_GUI_BIN,
            &raw mut SERVICE_SEATD_BIN,
        ];
        for slot_ptr in slots.iter() {
            let slot = &mut **slot_ptr;
            if let Some(ref v) = *slot {
                let start = v.as_ptr() as u64;
                let end   = start + v.len() as u64;
                if ptr >= start && ptr < end {
                    *slot = None; // drops the Vec<u8>, reclaiming heap
                    return;
                }
            }
        }
    }
}


/// The kernel image itself is linked at KERNEL_OFFSET (0xFFFF_8000_0000_0000),
/// so service binaries embedded in .rodata live at addresses starting there.
const KERNEL_HALF: u64 = 0xFFFF_8000_0000_0000;

/// Byte length of an ELF buffer after the kernel heap rounds the allocation up to a
/// multiple of `usize` (8 on x86_64), matching `linked_list_allocator` / `Layout` padding.
///
/// `sys_exec` uses `Vec::with_capacity(elf_size)`; the guard below uses the byte length
/// rounded **up** to `align_of::<usize>()` (8 on x86_64). Sizes from **128 MiB − 7** through
/// **128 MiB − 1** round up to **exactly** 128 MiB and are rejected, avoiding
/// `Layout { size: 134217728, align: 8 }` on a tight heap.
#[inline]
fn elf_byte_len_heap_padded(byte_len: u64) -> usize {
    let n = byte_len as usize;
    n.saturating_add(core::mem::size_of::<usize>() - 1) & !(core::mem::size_of::<usize>() - 1)
}

/// `true` iff a `Vec<u8>` holding `byte_len` bytes will not ask the global allocator for a
/// block of 128 MiB or more (after alignment padding).
#[inline]
fn elf_size_allowed_for_kernel_heap_copy(byte_len: u64) -> bool {
    byte_len > 0 && elf_byte_len_heap_padded(byte_len) < 128 * 1024 * 1024
}

/// Último mensaje de fallo de exec (para que userspace pueda mostrarlo sin serial)
const LAST_EXEC_ERR_LEN: usize = 80;
static LAST_EXEC_ERR: spin::Mutex<[u8; LAST_EXEC_ERR_LEN]> = spin::Mutex::new([0u8; LAST_EXEC_ERR_LEN]);

fn set_last_exec_error(msg: &[u8]) {
    let mut buf = LAST_EXEC_ERR.lock();
    let n = core::cmp::min(msg.len(), LAST_EXEC_ERR_LEN.saturating_sub(1));
    buf[..n].copy_from_slice(&msg[..n]);
    buf[n] = 0;
}

/// sys_exec - Replace current process with new program
/// arg1: pointer to ELF buffer
/// arg2: size of ELF buffer
/// Returns: 0 on success (doesn't return on success), -1 on error
fn sys_exec(elf_ptr: u64, elf_size: u64) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.exec_calls += 1;
    drop(stats);
    
    set_last_exec_error(b"exec: (no reason)"); // fallback if we return -1 without setting below

    if elf_ptr == 0 || !elf_size_allowed_for_kernel_heap_copy(elf_size) {
        set_last_exec_error(b"exec: invalid parameters");
        serial::serial_print("[SYSCALL] exec() invalid parameters\n");
        return u64::MAX;
    }
    
    // When the pointer is in kernel half (from get_service_binary), the current process
    // may not have that range mapped. Copy the ELF using kernel CR3 so we read valid data.
    let kernel_cr3 = crate::memory::get_kernel_cr3();
    
    if elf_ptr >= KERNEL_HALF && kernel_cr3 == 0 {
        serial::serial_print("[SYSCALL] exec: WARNING: kernel CR3 not set, copy may be invalid\n");
    }

    let elf_data: alloc::vec::Vec<u8> = if elf_ptr >= KERNEL_HALF && kernel_cr3 != 0 {
        let current_cr3 = crate::memory::get_cr3();
        unsafe {
            crate::memory::set_cr3(kernel_cr3);
        }
        let mut copy = alloc::vec::Vec::with_capacity(elf_size as usize);
        let src = unsafe { core::slice::from_raw_parts(elf_ptr as *const u8, elf_size as usize) };
        copy.extend_from_slice(src);
        unsafe {
            crate::memory::set_cr3(current_cr3);
        }
        // The service-binary cache entry that contained elf_ptr is no longer
        // needed once the data has been copied.  Release it to reclaim heap.
        release_service_binary_containing_ptr(elf_ptr);
        copy
    } else {
        // Validate the user-supplied pointer before touching it.  Without this check
        // a process could pass a canonical kernel-space address (>= 0xFFFF_8000_0000_0000)
        // that also happens to be below KERNEL_HALF (e.g. very high but not higher-half)
        // and have the kernel copy arbitrary memory into the ELF buffer.
        if !is_user_pointer(elf_ptr, elf_size) {
            set_last_exec_error(b"exec: invalid ELF buffer pointer");
            serial::serial_print("[SYSCALL] exec() security violation: non-user ELF pointer\n");
            return u64::MAX;
        }
        let src = unsafe { core::slice::from_raw_parts(elf_ptr as *const u8, elf_size as usize) };
        let mut copy = alloc::vec::Vec::with_capacity(elf_size as usize);
        copy.extend_from_slice(src);
        copy
    };

    // Comprobar que la copia tiene magic ELF (si no, el copy con kernel CR3 falló o no se usó)
    if elf_data.len() < 4 || elf_data[0] != 0x7f || elf_data[1] != b'E' || elf_data[2] != b'L' || elf_data[3] != b'F' {
        set_last_exec_error(b"exec: ELF copy invalid (bad magic)");
        serial::serial_print("[SYSCALL] exec() copy has bad ELF magic\n");
        return u64::MAX;
    }

    // Replace current process with ELF binary
    let current_pid = current_process_id().expect("exec called without current process");
    if let Err(msg) = crate::process::vfork_detach_mm_for_exec_if_needed(current_pid) {
        set_last_exec_error(msg.as_bytes());
        return u64::MAX;
    }
    match crate::elf_loader::replace_process_image(current_pid, elf_data.as_slice()) {
        Ok(res) => {
            serial::serial_printf(format_args!(
                "[SYSCALL] exec: replace_process_image success, entry={:#x} max_v={:#x} segments={}\n",
                res.entry_point, res.max_vaddr, res.segment_frames
            ));
            // Initialize heap (brk) for the new process
            if let Some(pid) = current_process_id() {
                if let Some(mut proc) = crate::process::get_process(pid) {
                    // Discard VMAs from the old image so that mmap's gap-search loop
                    // does not see phantom occupied ranges from the previous binary.
                    // Accumulating them across exec() calls also causes unbounded
                    // kernel-heap growth.
                    {
                        let mut r = proc.resources.lock();
                        r.vmas.clear();
                        r.brk_current = res.max_vaddr;
                    }
                    proc.mem_frames = (0x100000 / 4096) + res.segment_frames; // stack + segments
                    // Intérprete dinámico: dejar %fs en 0 hasta que ld-musl monte TLS (no usar tls_base del main).
                    proc.fs_base = if res.dynamic_linker.is_some() {
                        0
                    } else {
                        res.tls_base
                    };
                    proc.dynamic_linker_aux = res.dynamic_linker;
                    crate::process::update_process(pid, proc);
                }
            }

            // Misma PID, nueva imagen: no reutilizar argv del binario anterior (p. ej. sh → exec).
            crate::process::clear_pending_process_args(current_pid);

            // Explicitly drop the ELF copy before the non-returning jump so that
            // the heap memory is reclaimed.  jump_to_userspace* is declared `-> !`
            // and Rust will not run drop glue after a diverging call.
            drop(elf_data);

            // This doesn't return - we jump to the new process entry point.
            // Map a fresh 1 MB user stack for the exec'd binary.
            // A forked child only inherits the parent's 256 KB stack (up to 0x20040000),
            // but jump_to_userspace places the initial RSP near USER_STACK_TOP.
            const USER_STACK_BASE: u64 = 0x2000_0000;
            const USER_STACK_SIZE: usize = 0x10_0000; // 1 MB
            let cr3 = crate::memory::get_cr3();
            if let Err(e) = crate::elf_loader::setup_user_stack(cr3, USER_STACK_BASE, USER_STACK_SIZE) {
                set_last_exec_error(b"exec: failed to allocate user stack");
                serial::serial_print("[SYSCALL] exec() failed to allocate user stack: ");
                serial::serial_print(e);
                serial::serial_print("\n");
                return u64::MAX;
            }
            crate::process::register_post_exec_vm_as(
                current_pid,
                &res,
                USER_STACK_BASE,
                USER_STACK_SIZE as u64,
            );
            crate::fd::fd_ensure_stdio(current_pid);
            crate::serial::serial_printf(format_args!(
                "[EXEC] pid={} salto userspace entry={:#x} stack_top={:#x} phdr={:#x} dyn={}\n",
                current_pid,
                res.entry_point,
                USER_STACK_BASE + USER_STACK_SIZE as u64,
                res.phdr_va,
                res.dynamic_linker.is_some()
            ));
            unsafe {
                let stack_top: u64 = USER_STACK_BASE + USER_STACK_SIZE as u64;
                if res.dynamic_linker.is_some() {
                    crate::elf_loader::jump_to_userspace_dynamic_linker(
                        res.entry_point,
                        stack_top,
                        res.phdr_va,
                        res.phnum,
                        res.phentsize,
                    );
                } else {
                    crate::elf_loader::jump_to_userspace(
                        res.entry_point,
                        stack_top,
                        res.phdr_va,
                        res.phnum,
                        res.phentsize,
                    );
                }
            }
        }
        Err(msg) => {
            set_last_exec_error(msg.as_bytes());
            serial::serial_print("[SYSCALL] exec() failed: ");
            serial::serial_print(msg);
            serial::serial_print("\n");
            return u64::MAX;
        }
    }
}

/// sys_spawn - Create a new process from an ELF buffer
/// arg1: pointer to ELF buffer
/// arg2: size of ELF buffer
/// arg3: pointer to process name string (optional)
/// Returns: PID of new process on success, -1 on error
fn sys_spawn(elf_ptr: u64, elf_size: u64, name_ptr: u64) -> u64 {
    if elf_ptr == 0 || !elf_size_allowed_for_kernel_heap_copy(elf_size) {
        serial::serial_print("[SYSCALL] spawn() invalid parameters\n");
        return u64::MAX;
    }

    if !is_user_pointer(elf_ptr, elf_size) {
        serial::serial_print("[SYSCALL] spawn() security violation: invalid buffer pointer\n");
        return u64::MAX;
    }

    let mut name_buf = [0u8; 16];
    if name_ptr != 0 && is_user_pointer(name_ptr, 1) {
        // Read up to 15 bytes (+ null terminator)
        for i in 0..15 {
            if !is_user_pointer(name_ptr + i as u64, 1) { break; }
            let b = unsafe { *( (name_ptr + i as u64) as *const u8 ) };
            if b == 0 { break; }
            name_buf[i] = b;
        }
    }
    let name_str = core::str::from_utf8(&name_buf).unwrap_or("unknown");
    let name_trimmed = name_str.trim_matches(char::from(0));
    
    // Create slice from buffer
    let elf_data = unsafe {
        core::slice::from_raw_parts(elf_ptr as *const u8, elf_size as usize)
    };
    
    // Spawn the new process
    match crate::process::spawn_process(elf_data, name_trimmed) {
        Ok(pid) => {
            // Siempre fijar padre (init=1 si no hay proceso actual) para que wait() pueda cosechar.
            let parent_pid = crate::process::current_process_id().unwrap_or(1);
            if let Some(mut child) = crate::process::get_process(pid) {
                child.parent_pid = Some(parent_pid);
                crate::process::update_process(pid, child);
            }

            // We do NOT enqueue yet! The caller must call set_child_args (542)
            // if they want the process to start.
            pid as u64
        },
        Err(e) => {
            serial::serial_print("[SYSCALL] spawn() failed: ");
            serial::serial_print(e);
            serial::serial_print("\n");
            u64::MAX
        }
    }
}

/// sys_spawn_with_stdio - Create a new process and replace its stdin/stdout/stderr
/// arg1: pointer to ELF buffer
/// arg2: size of ELF buffer
/// arg3: pointer to process name string
/// arg4: fd to map to stdin (0)
/// arg5: fd to map to stdout (1)
/// arg6: fd to map to stderr (2)
/// sys_set_child_args (542): el padre llama esto justo después de spawn_with_stdio
/// para registrar el argv del hijo antes de que el scheduler lo ejecute.
fn sys_spawn_with_stdio_args(
    child_pid_arg: u64, args_ptr: u64, args_len: u64,
    _a4: u64, _a5: u64, _a6: u64,
    _ctx: &mut crate::interrupts::SyscallContext,
) -> u64 {
    if args_ptr == 0 || args_len == 0 || args_len > 4096 { return u64::MAX; }
    if !is_user_pointer(args_ptr, args_len) { return u64::MAX; }
    let args_data = unsafe {
        core::slice::from_raw_parts(args_ptr as *const u8, args_len as usize)
    }.to_vec();
    crate::process::set_pending_process_args(child_pid_arg as crate::process::ProcessId, args_data);

    // Now that arguments are registered, we can safely start the process.
    crate::scheduler::enqueue_process(child_pid_arg as crate::process::ProcessId);
    0
}

/// sys_get_process_args (543): el proceso llama esto al inicio para leer su argv.
/// Devuelve el número de bytes escritos en buf (formato: NUL-separados).
/// La entrada del kernel no se consume: varias lecturas devuelven los mismos datos
/// hasta que el proceso termina (`exit_process` libera la copia).
fn sys_get_process_args(buf_ptr: u64, buf_size: u64) -> u64 {
    if buf_ptr == 0 || buf_size == 0 { return 0; }
    if !is_user_pointer(buf_ptr, buf_size) { return 0; }
    let pid = match crate::process::current_process_id() {
        Some(p) => p,
        None => return 0,
    };
    let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, buf_size as usize) };
    crate::process::copy_pending_process_args(pid, buf) as u64
}

/// arg1: ruta absoluta NUL (`/bin/foo`), arg2: nombre proceso (opcional, hasta 16 bytes + NUL), arg3–5: fds stdio.
fn sys_spawn_with_stdio_path(
    path_ptr: u64,
    name_ptr: u64,
    fd_in: u64,
    fd_out: u64,
    fd_err: u64,
    _a6: u64,
) -> u64 {
    use alloc::vec::Vec;
    const MAX_PATH: usize = 1024;
    let path_len = strlen_user_unique(path_ptr, MAX_PATH);
    if path_ptr == 0 || path_len == 0 || path_len >= MAX_PATH as u64 {
        return u64::MAX;
    }
    if !is_user_pointer(path_ptr, path_len + 1) {
        return u64::MAX;
    }
    let path_str = unsafe {
        let slice = core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize);
        match core::str::from_utf8(slice) {
            Ok(s) if !s.is_empty() => s,
            _ => return u64::MAX,
        }
    };

    let pid = match crate::elf_loader::load_elf_path(path_str) {
        Some(p) => p,
        None => return u64::MAX,
    };

    let name_trimmed = if name_ptr != 0 {
        if !is_user_pointer(name_ptr, 16) {
            return u64::MAX;
        }
        let name_slice = unsafe { core::slice::from_raw_parts(name_ptr as *const u8, 16) };
        let len = name_slice.iter().position(|&b| b == 0).unwrap_or(16);
        core::str::from_utf8(&name_slice[..len]).unwrap_or("unknown")
    } else {
        path_str.rsplit('/').next().unwrap_or(path_str)
    };

    // Update process name if it was explicitly provided.
    if name_ptr != 0 {
        crate::process::modify_process(pid, |p| {
            let n = name_trimmed.len().min(16);
            p.name[..n].copy_from_slice(&name_trimmed.as_bytes()[..n]);
            if n < 16 { p.name[n] = 0; }
        }).unwrap();
    }

    setup_spawned_process_stdio(pid, fd_in, fd_out, fd_err)
}

fn setup_spawned_process_stdio(
    pid: crate::process::ProcessId,
    fd_in: u64,
    fd_out: u64,
    fd_err: u64,
) -> u64 {
    let parent_pid = crate::process::current_process_id().unwrap_or(1);
    crate::process::modify_process(pid, |p| {
        p.parent_pid = Some(parent_pid);
    })
    .unwrap();

    {
        let p_fd_in  = crate::fd::fd_get(parent_pid, fd_in  as usize);
        let p_fd_out = crate::fd::fd_get(parent_pid, fd_out as usize);
        let p_fd_err = crate::fd::fd_get(parent_pid, fd_err as usize);

        let mut old_to_close: alloc::vec::Vec<(usize, usize)> = alloc::vec::Vec::with_capacity(3);

        if let Some(child_fd_idx) = crate::fd::pid_to_fd_idx(pid as u32) {
            let mut tables = crate::fd::FD_TABLES.lock();
            if let Some(ref new_fd) = p_fd_in {
                let old = &tables[child_fd_idx].fds[0];
                if old.in_use { old_to_close.push((old.scheme_id, old.resource_id)); }
                tables[child_fd_idx].fds[0] = *new_fd;
            }
            if let Some(ref new_fd) = p_fd_out {
                let old = &tables[child_fd_idx].fds[1];
                if old.in_use { old_to_close.push((old.scheme_id, old.resource_id)); }
                tables[child_fd_idx].fds[1] = *new_fd;
            }
            if let Some(ref new_fd) = p_fd_err {
                let old = &tables[child_fd_idx].fds[2];
                if old.in_use { old_to_close.push((old.scheme_id, old.resource_id)); }
                tables[child_fd_idx].fds[2] = *new_fd;
            }
        }

        let mut closed: alloc::vec::Vec<(usize, usize)> = alloc::vec::Vec::with_capacity(3);
        for pair in old_to_close {
            if !closed.contains(&pair) {
                let _ = crate::scheme::close(pair.0, pair.1);
                closed.push(pair);
            }
        }

        for fd_opt in [p_fd_in, p_fd_out, p_fd_err] {
            if let Some(fd) = fd_opt {
                if fd.in_use {
                    let _ = crate::scheme::dup(fd.scheme_id, fd.resource_id);
                }
            }
        }
    }

    pid as u64
}

fn sys_spawn_with_stdio_from_elf(
    elf_data: alloc::vec::Vec<u8>,
    name_trimmed: &str,
    fd_in: u64,
    fd_out: u64,
    fd_err: u64,
) -> u64 {
    match crate::process::spawn_process(&elf_data, name_trimmed) {
        Ok(pid) => setup_spawned_process_stdio(pid, fd_in, fd_out, fd_err),
        Err(_) => u64::MAX,
    }
}

fn sys_spawn_with_stdio(elf_ptr: u64, elf_size: u64, name_ptr: u64, fd_in: u64, fd_out: u64, fd_err: u64) -> u64 {
    // Re-implement the base logic to avoid the race condition!
    // We cannot call sys_spawn directly because it enqueues the process,
    // making it runnable before we replace its FDs.

    use alloc::vec::Vec;
    if elf_ptr == 0 || !elf_size_allowed_for_kernel_heap_copy(elf_size) {
        return u64::MAX;
    }
    if !is_user_pointer(elf_ptr, elf_size) {
        return u64::MAX;
    }
    let elf_slice = unsafe { core::slice::from_raw_parts(elf_ptr as *const u8, elf_size as usize) };
    let mut elf_data = Vec::with_capacity(elf_size as usize);
    elf_data.extend_from_slice(elf_slice);

    let name_trimmed = if name_ptr != 0 {
        if !is_user_pointer(name_ptr, 16) {
            return u64::MAX;
        }
        let name_slice = unsafe { core::slice::from_raw_parts(name_ptr as *const u8, 16) };
        let len = name_slice.iter().position(|&b| b == 0).unwrap_or(16);
        core::str::from_utf8(&name_slice[..len]).unwrap_or("unknown")
    } else {
        "unknown"
    };

    sys_spawn_with_stdio_from_elf(elf_data, name_trimmed, fd_in, fd_out, fd_err)
}

/// sys_wait - Wait for child process to terminate
/// arg1: pointer to status variable (or 0 for non-blocking poll / WNOHANG semantics)
/// Returns: PID of terminated child, or -1 on error
fn sys_wait(status_ptr: u64) -> u64 {
    sys_wait_impl(status_ptr, 0, 0)
}

/// Esperar hijo concreto (`wait_pid == 0` → cualquier hijo).
/// flags: bit 0 = WNOHANG, bit 1 = WUNTRACED
fn sys_wait_pid(status_ptr: u64, wait_pid: u64, flags: u64) -> u64 {
    sys_wait_impl(status_ptr, wait_pid, flags)
}

fn sys_wait_impl(status_ptr: u64, wait_pid: u64, flags: u64) -> u64 {
    use crate::process;
    let wnohang = (flags & 1) != 0;
    let wuntraced = (flags & 2) != 0;

    let mut stats = SYSCALL_STATS.lock();
    stats.wait_calls += 1;
    drop(stats);

    let current_pid = match process::current_process_id() {
        Some(pid) => pid,
        None => {
            serial::serial_print("[SYSCALL] wait() failed - no current process\n");
            return u64::MAX;
        }
    };

    loop {
        if wait_pid != 0 {
            let wp = wait_pid as process::ProcessId;
            match process::get_process(wp) {
                Some(mut proc) if proc.parent_pid == Some(current_pid) => {
                        if proc.state == process::ProcessState::Terminated {
                            if status_ptr != 0 && is_user_pointer(status_ptr, 4) {
                                let wait_status = ((proc.exit_code as u32) & 0xFF) << 8;
                                unsafe {
                                    if status_ptr != 0 && is_user_pointer(status_ptr, 4) {
                                        unsafe {
                                            core::ptr::copy_nonoverlapping(&wait_status, status_ptr as *mut u32, 1);
                                        }
                                    }
                                }
                            }
                            process::unregister_child_waiter(current_pid);
                            // Reap zombie: free PROCESS_TABLE slot and PID map entry.
                            process::remove_process(wp);
                            return wp as u64;
                        }

                        if wuntraced && proc.state == process::ProcessState::Stopped {
                            if status_ptr != 0 && is_user_pointer(status_ptr, 4) {
                                let wait_status = 0x7F | ((proc.exit_signal as u32) << 8);
                                unsafe {
                                    if status_ptr != 0 && is_user_pointer(status_ptr, 4) {
                                        unsafe {
                                            core::ptr::copy_nonoverlapping(&wait_status, status_ptr as *mut u32, 1);
                                        }
                                    }
                                }
                            }
                            process::unregister_child_waiter(current_pid);
                            // NO cosechamos: el proceso sigue vivo aunque detenido
                            return wp as u64;
                        }
                }
                _ => {
                    process::unregister_child_waiter(current_pid);
                    return u64::MAX;
                }
            }
        } else {
            let mut has_children = false;
            let processes = process::list_processes();

            for (pid, state) in processes.iter() {
                if *pid == 0 {
                    continue;
                }

                if let Some(mut proc) = process::get_process(*pid) {
                    if proc.parent_pid == Some(current_pid) {
                        has_children = true;
                        if state == &process::ProcessState::Terminated {
                            if status_ptr != 0 && is_user_pointer(status_ptr, 4) {
                                let wait_status = ((proc.exit_code as u32) & 0xFF) << 8;
                                unsafe {
                                    if status_ptr != 0 && is_user_pointer(status_ptr, 4) {
                                        unsafe {
                                            core::ptr::copy_nonoverlapping(&wait_status, status_ptr as *mut u32, 1);
                                        }
                                    }
                                }
                            }

                            process::unregister_child_waiter(current_pid);
                            // Reap zombie: free PROCESS_TABLE slot and PID map entry.
                            process::remove_process(*pid);
                            return *pid as u64;
                        }

                        if wuntraced && state == &process::ProcessState::Stopped {
                            if status_ptr != 0 && is_user_pointer(status_ptr, 4) {
                                let wait_status = 0x7F | ((proc.exit_signal as u32) << 8);
                                unsafe {
                                    if status_ptr != 0 && is_user_pointer(status_ptr, 4) {
                                        unsafe {
                                            core::ptr::copy_nonoverlapping(&wait_status, status_ptr as *mut u32, 1);
                                        }
                                    }
                                }
                            }
                            process::unregister_child_waiter(current_pid);
                            // NO cosechamos
                            return *pid as u64;
                        }
                    }
                }
            }

            if !has_children {
                process::unregister_child_waiter(current_pid);
                return u64::MAX;
            }
        }

        // WNOHANG: si ningún hijo ha terminado aún, devolver 0 sin bloquear.
        if wnohang {
            return 0;
        }

        // Sin status_ptr no hay a dónde escribir el estado → salir.
        if status_ptr == 0 {
            return u64::MAX;
        }

        // Registrar ANTES de marcar como Blocked para evitar la race condition
        // "lost wakeup": si el hijo termina entre la comprobación inicial y el
        // registro, wake_parent_from_wait() nos encontrará en la lista de
        // espera y no perderemos la notificación.
        process::register_child_waiter(current_pid);

        // DOUBLE-CHECK: el hijo puede haber terminado entre la comprobación
        // inicial (arriba) y register_child_waiter.  Si ya está Terminated,
        // cancelamos el sueño y volvemos al inicio del loop (que lo recogerá).
        //
        // Nota: sólo hacemos el double-check para wait_pid != 0 (hijo concreto).
        // Para wait_pid == 0 (cualquier hijo) omitimos el double-check con
        // list_processes() para evitar una gran asignación en el stack del kernel.
        // En ese caso, la race es tolerable: el bucle se despertará igualmente
        // por el siguiente tick del temporizador.
        let child_done = if wait_pid != 0 {
            let wp = wait_pid as process::ProcessId;
            process::get_process(wp).map_or(false, |p| {
                p.state == process::ProcessState::Terminated
                    && p.parent_pid == Some(current_pid)
            })
        } else {
            false
        };

        if child_done {
            // El hijo terminó mientras nos registrábamos: cancelar el sueño
            // y dejar que el siguiente ciclo del loop recoja el resultado.
            process::unregister_child_waiter(current_pid);
            continue;
        }

        // TRIPLE-CHECK (SMP): entre el double-check y marcar Blocked el hijo puede
        // terminar y wake_parent_from_wait ejecutarse mientras el padre sigue Running.
        // Si además consumiéramos el waiter en wake, el padre quedaría Blocked sin wake.
        // Con wake que ya no borra el waiter, esto acorta la ventana residual.
        if wait_pid != 0 {
            let wp = wait_pid as process::ProcessId;
            if let Some(p) = process::get_process(wp) {
                if p.state == process::ProcessState::Terminated
                    && p.parent_pid == Some(current_pid)
                {
                    process::unregister_child_waiter(current_pid);
                    continue;
                }
            }
        }

        x86_64::instructions::interrupts::without_interrupts(|| {
            let slot = crate::ipc::pid_to_slot_fast(current_pid);
            let mut table = process::PROCESS_TABLE.lock();
            if let Some(slot_idx) = slot {
                if let Some(p) = table[slot_idx].as_mut() {
                    if p.id == current_pid {
                        p.state = process::ProcessState::Blocked;
                    }
                }
            }
        });

        crate::scheduler::yield_cpu();
    }
}

/// sys_get_service_binary - Get pointer and size of embedded service binary
/// Args: service_id (0-7, ver `eclipse_program_codes::spawn_service`), out_ptr, out_size
/// Returns: 0 on success, -1 on error
fn sys_get_service_binary(service_id: u64, out_ptr: u64, out_size: u64) -> u64 {
    // Validate pointers
    if out_ptr == 0 || out_size == 0 {
        return u64::MAX;
    }
    
    // Check user pointer validity
    if !is_user_pointer(out_ptr, 8) || !is_user_pointer(out_size, 8) {
         serial::serial_print("[SYSCALL] get_service_binary - invalid user pointers\n");
         return u64::MAX;
    }
    
    // Get service binary based on ID, cargándolo desde /sbin si es necesario.
    let slice = match get_service_slice(service_id) {
        Some(s) => s,
        None => {
            serial::serial_print("[SYSCALL] Invalid service ID or failed to load from disk\n");
            return u64::MAX;
        }
    };
    let bin_ptr = slice.as_ptr() as u64;
    let bin_size = slice.len() as u64;
    
    // Write pointer and size to user-provided addresses
    unsafe {
        core::ptr::copy_nonoverlapping(&bin_ptr, out_ptr as *mut u64, 1);
        core::ptr::copy_nonoverlapping(&bin_size, out_size as *mut u64, 1);
    }
    
    0 // Success
}

/// sys_register_device - Register a new device node (Syscall 27)
#[inline(never)]
fn sys_register_device(name_ptr: u64, name_len: u64, type_id: u64) -> u64 {
    if name_ptr == 0 || name_len == 0 || name_len > 256 {
        return u64::MAX;
    }
    
    if !is_user_pointer(name_ptr, name_len) {
        serial::serial_printf(format_args!(
            "[SYSCALL] register_device validation FAILED: ptr={:#018X}, len={}\n",
            name_ptr, name_len
        ));
        return u64::MAX;
    }
    
    let name = unsafe {
        let slice = core::slice::from_raw_parts(name_ptr as *const u8, name_len as usize);
        core::str::from_utf8(slice).unwrap_or("")
    };
    
    let device_type = match type_id {
        0 => crate::filesystem::DeviceType::Block,
        1 => crate::filesystem::DeviceType::Char,
        2 => crate::filesystem::DeviceType::Network,
        3 => crate::filesystem::DeviceType::Input,
        4 => crate::filesystem::DeviceType::Audio,
        5 => crate::filesystem::DeviceType::Display,
        6 => crate::filesystem::DeviceType::USB,
        _ => crate::filesystem::DeviceType::Unknown,
    };
    
    let driver_pid = if let Some(pid) = current_process_id() { pid as u64 } else { 0 };
    
    if crate::filesystem::register_device(name, device_type, driver_pid) {
        0
    } else {
        u64::MAX
    }
}

/// sys_open - Open a file or scheme resource
fn sys_open(path_ptr: u64, flags: u64, mode: u64) -> u64 {
    let path_len = strlen_user_unique(path_ptr, 4096);
    let mut stats = SYSCALL_STATS.lock();
    stats.open_calls += 1;
    drop(stats);
    
    // Validate parameters
    if path_ptr == 0 || path_len == 0 || path_len > 4096 {
        return syscall_error_for_current_process(crate::scheme::error::EINVAL as i32);
    }

    if !is_user_pointer(path_ptr, path_len) {
        return syscall_error_for_current_process(crate::scheme::error::EFAULT as i32);
    }
    
    // Extract path string
    let path = unsafe {
        let slice = core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize);
        core::str::from_utf8(slice).unwrap_or("")
    };
    

    // Route through scheme system via centralized translation utility
    let scheme_path = user_path_to_scheme_path(path);
    
    let (scheme_id, resource_id) = match crate::scheme::open(&scheme_path, flags as usize, mode as u32) {
        Ok(res) => res,
        Err(e) => {
            if e != crate::scheme::error::EAGAIN 
                && !(e == crate::scheme::error::ENOENT && path.starts_with("/tmp/"))
            {
                serial::serial_printf(format_args!("[SYSCALL] open('{}') -> '{}' failed: error {}\n", path, scheme_path, e));
            }
            return syscall_error_for_current_process(e as i32);
        }
    };

    if let Some(pid) = current_process_id() {
        match crate::fd::fd_open(pid, scheme_id, resource_id, flags as u32) {
            Some(fd) => {
                fd as u64
            }
            None => {
                // FD table is full (MAX_FD_PER_PROCESS entries in use).
                // Release the scheme resource so it isn't permanently leaked.
                let _ = crate::scheme::close(scheme_id, resource_id);
                syscall_error_for_current_process(24) // EMFILE
            }
        }
    } else {
        // No current process — release the scheme resource to avoid a leak.
        let _ = crate::scheme::close(scheme_id, resource_id);
        syscall_error_for_current_process(crate::scheme::error::ESRCH as i32)
    }
}

/// Linux `AT_FDCWD` (-100): rutas absolutas/relativas al cwd del proceso (aquí raíz del VFS).
const LINUX_AT_FDCWD: u64 = (-100_i64) as u64;

/// openat(dirfd, pathname, flags, mode) — requerido por musl para abrir bibliotecas compartidas.
fn sys_openat(dirfd: u64, path_ptr: u64, flags: u64, mode: u64) -> u64 {
    if dirfd != LINUX_AT_FDCWD {
        // Sin fd de directorio real: solo soportamos AT_FDCWD (caso habitual de ld-musl).
        return syscall_error_for_current_process(crate::scheme::error::EBADF as i32);
    }
    sys_open(path_ptr, flags, mode)
}

/// sys_close - Close a file descriptor
fn sys_close(fd: u64) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.close_calls += 1;
    drop(stats);
    
    if let Some(pid) = current_process_id() {
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            // Close in scheme
            let _ = crate::scheme::close(fd_entry.scheme_id, fd_entry.resource_id);
            
            // Close in FD table
            if crate::fd::fd_close(pid, fd as usize) {
                return 0;
            }
        }
    }
    u64::MAX
}

/// sys_lseek - Change file offset
fn sys_lseek(fd: u64, offset: i64, whence: usize) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.lseek_calls += 1;
    drop(stats);
    
    if let Some(pid) = current_process_id() {
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            serial::serial_printf(format_args!(
                "[SYSCALL] lseek(fd={}, offset={}, whence={})\n",
                fd, offset, whence
            ));

            match crate::scheme::lseek(fd_entry.scheme_id, fd_entry.resource_id, offset as isize, whence, fd_entry.offset) {
                Ok(new_offset) => {
                    crate::fd::fd_update_offset(pid, fd as usize, new_offset as u64);
                    return new_offset as u64;
                }
                Err(_) => return u64::MAX,
            }
        }
    }
    
    u64::MAX
}

/// sys_fmap - Map a resource into memory via its scheme
fn sys_fmap(fd: u64, offset: u64, len: u64) -> u64 {
    if let Some(pid) = current_process_id() {
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            match crate::scheme::fmap(fd_entry.scheme_id, fd_entry.resource_id, offset as usize, len as usize) {
                Ok(addr) => {
                    // Convertir dirección kernel a física si aplica (evita crash 0xffff8000...)
                    let phys_addr: u64 = if (addr as u64) >= crate::memory::PHYS_MEM_OFFSET {
                        (addr as u64) - crate::memory::PHYS_MEM_OFFSET
                    } else {
                        addr as u64
                    };
                    let page_table = crate::process::get_process_page_table(crate::process::current_process_id());
                    let vaddr = crate::memory::map_shared_memory_for_process(
                        page_table,
                        phys_addr,
                        len as u64
                    );
                    return vaddr;
                }
                Err(e) => {
                    serial::serial_printf(format_args!("SYS_FMAP: scheme::fmap failed with error {}\n", e));
                    return u64::MAX;
                }
            }
        }
    }
    
    u64::MAX
}

/// Obtener estadísticas de syscalls
pub fn get_stats() -> SyscallStats {
    let stats = SYSCALL_STATS.lock();
    SyscallStats {
        total_calls: stats.total_calls,
        exit_calls: stats.exit_calls,
        write_calls: stats.write_calls,
        read_calls: stats.read_calls,
        send_calls: stats.send_calls,
        receive_calls: stats.receive_calls,
        yield_calls: stats.yield_calls,
        fork_calls: stats.fork_calls,
        exec_calls: stats.exec_calls,
        wait_calls: stats.wait_calls,
        open_calls: stats.open_calls,
        close_calls: stats.close_calls,
        lseek_calls: stats.lseek_calls,
    }
}

/// sys_mount - Mount the root filesystem
fn sys_mount(path_ptr: u64, path_len: u64) -> u64 {
    // Default to disk:0 if no path provided (backward compatibility)
    let device_path = if path_ptr != 0 && path_len != 0 && path_len <= 1024 {
        if !is_user_pointer(path_ptr, path_len) {
            return u64::MAX;
        }
        unsafe {
            let slice = core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize);
            core::str::from_utf8(slice).unwrap_or("disk:0")
        }
    } else {
        "disk:0"
    };

    match crate::filesystem::mount_root(device_path) {
        Ok(_) => 0,
        // Robust check for already mounted: if it contains the phrase, treat as success.
        Err(e) if e.contains("already mounted") => 0,
        Err(e) => {
            serial::serial_printf(format_args!("[SYSCALL] mount({}) failed: {}\n", device_path, e));
            u64::MAX
        }
    }
}

/// sys_get_framebuffer_info - Get framebuffer information from bootloader
/// Accepts a pointer to userspace buffer and copies framebuffer info into it
/// Returns 0 on success, -1 on failure
/// sys_get_framebuffer_info - Get framebuffer information from bootloader
/// Accepts a pointer to userspace buffer and copies framebuffer info into it
/// Returns 0 on success, -1 on failure
fn sys_get_framebuffer_info(user_buffer: u64) -> u64 {
    use crate::servers::FramebufferInfo;

    if user_buffer == 0 {
        return u64::MAX;
    }
    if !is_user_pointer(user_buffer, core::mem::size_of::<FramebufferInfo>() as u64) {
        return u64::MAX;
    }

    let (fb_address, width, height, pitch) = {
        let k = &crate::boot::get_boot_info().framebuffer;
        if crate::boot::gop_framebuffer_valid() {
            let addr = if k.base_address >= crate::memory::PHYS_MEM_OFFSET {
                k.base_address - crate::memory::PHYS_MEM_OFFSET
            } else {
                k.base_address
            };
            let pitch = k.pixels_per_scan_line * 4;
            (addr, k.width, k.height, pitch)
        } else if let Some((phys, w, h, p, _size)) = crate::virtio::get_primary_virtio_display() {
            (phys, w, h, p)
        } else if let Some((phys, _bar1, w, h, pitch)) = crate::nvidia::get_nvidia_fb_info() {
            (phys, w, h, pitch)
        } else {
            return u64::MAX;
        }
    };

    unsafe {
        let syscall_fb = FramebufferInfo {
            address: fb_address,
            width,
            height,
            pitch,
            bpp: 32,
            red_mask_size: 8,
            red_mask_shift: 16,
            green_mask_size: 8,
            green_mask_shift: 8,
            blue_mask_size: 8,
            blue_mask_shift: 0,
        };
        core::ptr::write_unaligned(user_buffer as *mut FramebufferInfo, syscall_fb);
    }
    0
}

/// sys_get_gpu_display_info - Get display dimensions from VirtIO GPU (if present)
/// arg1: pointer to userspace buffer (8 bytes: width u32, height u32)
/// Returns 0 on success, u64::MAX if no VirtIO GPU or invalid buffer
fn sys_get_gpu_display_info(user_buffer: u64) -> u64 {
    if user_buffer == 0 {
        return u64::MAX;
    }
    if !is_user_pointer(user_buffer, 8) {
        return u64::MAX;
    }
    let Some((width, height)) = crate::virtio::get_gpu_display_info() else {
        return u64::MAX;
    };
    unsafe {
        core::ptr::write_unaligned(user_buffer as *mut u32, width);
        core::ptr::write_unaligned((user_buffer + 4) as *mut u32, height);
    }
    0
}

/// sys_set_cursor_position - Set cursor position.
/// On VirtIO GPU (QEMU): uses hardware cursor via MOVE_CURSOR command.
/// On real hardware (EFI GOP): renders a software cursor into the framebuffer.
/// arg1: x (u32), arg2: y (u32). Always returns 0.
fn sys_set_cursor_position(arg1: u64, arg2: u64) -> u64 {
    let x = arg1 as i32;
    let y = arg2 as i32;
    
    // Try via unified DRM subsystem first (handles VirtIO hardware cursor and future drivers)
    // Flags 0x02 = DRM_CURSOR_MOVE
    if !crate::drm::set_cursor(0, x, y, 0, 0x02) {
        // Fall back to legacy / software cursor if DRM failed or no driver supports it
        crate::sw_cursor::update(x as u32, y as u32);
    }
    0
}

/// sys_gpu_alloc_display_buffer - Allocate VirtIO GPU 2D buffer and map into process
/// arg1: width (u32), arg2: height (u32), arg3: output buffer ptr (24 bytes)
/// Output layout: vaddr u64, resource_id u32, pitch u32, size u64
/// Returns 0 on success, u64::MAX on failure
fn sys_gpu_alloc_display_buffer(width: u64, height: u64, out_ptr: u64) -> u64 {
    let (width, height) = (width as u32, height as u32);
    if width == 0 || height == 0 || out_ptr == 0 {
        return u64::MAX;
    }
    if !is_user_pointer(out_ptr, 24) {
        return u64::MAX;
    }
    let Some((phys_addr, resource_id, pitch, size)) = crate::virtio::gpu_alloc_display_buffer(width, height) else {
        return u64::MAX;
    };
    let current_pid = crate::process::current_process_id();
    let page_table_phys = crate::process::get_process_page_table(current_pid);
    if page_table_phys == 0 {
        return u64::MAX;
    }
    let vaddr = crate::memory::map_framebuffer_for_process(page_table_phys, phys_addr, size as u64);
    if vaddr == 0 {
        return u64::MAX;
    }
    unsafe {
        let buf = out_ptr as *mut u8;
        core::ptr::write_unaligned(buf as *mut u64, vaddr);
        core::ptr::write_unaligned(buf.add(8) as *mut u32, resource_id);
        core::ptr::write_unaligned(buf.add(12) as *mut u32, pitch);
        core::ptr::write_unaligned(buf.add(16) as *mut u64, size as u64);
    }
    0
}

/// sys_gpu_present - Present GPU buffer to screen (transfer + flush)
/// arg1: resource_id, arg2: x, arg3: y, arg4: w, arg5: h
/// Returns 0 on success, u64::MAX on failure
fn sys_gpu_present(resource_id: u64, x: u64, y: u64, w: u64, h: u64) -> u64 {
    if crate::virtio::gpu_present(
        resource_id as u32,
        x as u32,
        y as u32,
        w as u32,
        h as u32,
    ) {
        0
    } else {
        u64::MAX
    }
}

/// sys_virgl_ctx_create - Create a Virgl 3D context
/// arg1: pointer to debug name (optional, can be 0)
/// arg2: max length to copy (0-64). If 0 and ptr valid, treat as null-terminated up to 64
/// Returns ctx_id (1..16) on success, 0 on failure
fn sys_virgl_ctx_create(name_ptr: u64, max_len: u64) -> u64 {
    let mut buf = [0u8; 64];
    let name_slice: &[u8] = if name_ptr == 0 {
        &[]
    } else {
        let len = (max_len as usize).min(64);
        if len == 0 {
            // Null-terminated: copy up to 64 bytes until we hit \0
            let mut i = 0;
            while i < 64 {
                if !is_user_pointer(name_ptr.wrapping_add(i as u64), 1) {
                    break;
                }
                let b = unsafe { core::ptr::read(name_ptr.wrapping_add(i as u64) as *const u8) };
                buf[i] = b;
                if b == 0 {
                    break;
                }
                i += 1;
            }
            &buf[..i]
        } else {
            if !is_user_pointer(name_ptr, len as u64) {
                return 0;
            }
            unsafe {
                core::ptr::copy_nonoverlapping(name_ptr as *const u8, buf.as_mut_ptr(), len);
            }
            &buf[..len]
        }
    };
    crate::virtio::virgl_ctx_create(name_slice).map(u64::from).unwrap_or(0)
}

/// sys_virgl_ctx_destroy - Destroy a Virgl 3D context
/// arg1: ctx_id (1..16)
/// Returns 0 on success, u64::MAX on failure
fn sys_virgl_ctx_destroy(ctx_id: u64) -> u64 {
    if crate::virtio::virgl_ctx_destroy(ctx_id as u32) {
        0
    } else {
        u64::MAX
    }
}

/// sys_virgl_ctx_attach_resource - Attach resource to Virgl context
fn sys_virgl_ctx_attach_resource(ctx_id: u64, resource_id: u64) -> u64 {
    if crate::virtio::virgl_ctx_attach_resource(ctx_id as u32, resource_id as u32) {
        0
    } else {
        u64::MAX
    }
}

/// sys_virgl_ctx_detach_resource - Detach resource from Virgl context
fn sys_virgl_ctx_detach_resource(ctx_id: u64, resource_id: u64) -> u64 {
    if crate::virtio::virgl_ctx_detach_resource(ctx_id as u32, resource_id as u32) {
        0
    } else {
        u64::MAX
    }
}

/// sys_virgl_alloc_backing - Allocate backing memory for Virgl 3D resource
/// arg1: size in bytes
/// Returns vaddr (identity-mapped, vaddr == phys) on success, 0 on failure
fn sys_virgl_alloc_backing(size: u64) -> u64 {
    let size = size as usize;
    let Some((phys_addr, alloc_size)) = crate::virtio::virgl_alloc_backing(size) else {
        return 0;
    };
    let current_pid = crate::process::current_process_id();
    let page_table_phys = crate::process::get_process_page_table(current_pid);
    if page_table_phys == 0 {
        return 0;
    }
    let vaddr = crate::memory::map_framebuffer_for_process(page_table_phys, phys_addr, alloc_size as u64);
    if vaddr == 0 {
        0
    } else {
        vaddr
    }
}

/// sys_virgl_resource_attach_backing - Attach backing memory to Virgl resource
/// arg1: resource_id, arg2: vaddr (from virgl_alloc_backing, identity-mapped), arg3: size
/// Returns 0 on success, u64::MAX on failure
fn sys_virgl_resource_attach_backing(resource_id: u64, vaddr: u64, size: u64) -> u64 {
    if size == 0 {
        return u64::MAX;
    }
    if crate::virtio::virgl_resource_attach_backing(resource_id as u32, vaddr, size as usize) {
        0
    } else {
        u64::MAX
    }
}

/// sys_virgl_submit_3d - Submit Virgl 3D command buffer
/// arg1: ctx_id, arg2: pointer to command buffer, arg3: length
/// Returns 0 on success, u64::MAX on failure
fn sys_virgl_submit_3d(ctx_id: u64, cmd_ptr: u64, cmd_len: u64) -> u64 {
    const MAX_SUBMIT_SIZE: usize = 256 * 1024; // 256KB
    let len = cmd_len as usize;
    if len == 0 || len > MAX_SUBMIT_SIZE {
        return u64::MAX;
    }
    if cmd_ptr == 0 || !is_user_pointer(cmd_ptr, len as u64) {
        return u64::MAX;
    }
    let mut buf = alloc::vec![0u8; len];
    unsafe {
        core::ptr::copy_nonoverlapping(cmd_ptr as *const u8, buf.as_mut_ptr(), len);
    }
    if crate::virtio::virgl_submit_3d(ctx_id as u32, &buf) {
        0
    } else {
        u64::MAX
    }
}

/// sys_gpu_command - Generic GPU command dispatcher
/// arg1: kind (0=VirtIO, 1=NVIDIA)
/// arg2: command (backend-specific)
/// arg3: payload_ptr
/// arg4: payload_len
fn sys_gpu_command(kind: u64, command: u64, payload_ptr: u64, payload_len: u64) -> u64 {
    match kind {
        0 => {
            // Backend: VirtIO-GPU
            match command {
                0 => sys_virgl_submit_3d(0, payload_ptr, payload_len), // Default context 0
                _ => u64::MAX,
            }
        }
        1 => {
            // Backend: NVIDIA — command 0 = fill_rect (x, y, w, h, color), 20 bytes
            const MAX_PAYLOAD: usize = 64;
            let len = payload_len as usize;
            if len == 0 || len > MAX_PAYLOAD || payload_ptr == 0 || !is_user_pointer(payload_ptr, len as u64) {
                return u64::MAX;
            }
            let mut buf = [0u8; MAX_PAYLOAD];
            unsafe {
                core::ptr::copy_nonoverlapping(payload_ptr as *const u8, buf.as_mut_ptr(), len);
            }
            match command {
                0 => if crate::nvidia::fill_rect(&buf[..len]) { 0 } else { u64::MAX },
                1 => if crate::nvidia::blit_rect(&buf[..len]) { 0 } else { u64::MAX },
                2 => if crate::nvidia::blit_from_handle(&buf[..len]) { 0 } else { u64::MAX },
                _ => u64::MAX,
            }
        }
        _ => u64::MAX,
    }
}

fn sys_gpu_get_backend() -> u64 {
    if let Some(driver) = crate::drm::get_primary_driver() {
        if driver.name() == "nvidia" {
            return 1;
        } else if driver.name() == "virtio-gpu" {
            return 0;
        }
    }
    2 // Software fallback
}

/// sys_map_framebuffer - Map framebuffer physical memory into process virtual space
/// Returns the virtual address where framebuffer is mapped, or 0 on failure
fn sys_map_framebuffer() -> u64 {
    // Check whether the EFI GOP framebuffer from the bootloader is actually valid.
    // boot::get_framebuffer_info() always returns a non-null pointer to the static
    // BootInfo struct (even when it was never populated by the bootloader, in which
    // case base_address == 0xDEADBEEF and dimensions are 0).  We must inspect the
    // struct contents to decide whether to use EFI GOP or fall back to NVIDIA BAR1.
    let fb_info_ptr = crate::boot::get_framebuffer_info();
    let gop_valid = crate::boot::gop_framebuffer_valid();

    if !gop_valid {
        // Try NVIDIA BAR1 as fallback for real hardware without EFI GOP
        if let Some((_fb_phys, bar1_phys, width, height, pitch)) = crate::nvidia::get_nvidia_fb_info() {
            let single_frame_size = (pitch as u64) * (height as u64);
            let mut fb_size = single_frame_size * 2;
            fb_size = (fb_size + 0xFFF) & !0xFFF;
            fb_size = fb_size.saturating_add(0x400000);
            let current_pid = crate::process::current_process_id();
            let page_table_phys = crate::process::get_process_page_table(current_pid);
            if page_table_phys == 0 {
                serial::serial_print("MAP_FB: ERROR - Could not get process page table\n");
                return 0;
            }
            let vaddr = crate::memory::map_framebuffer_for_process(page_table_phys, bar1_phys, fb_size);
            return vaddr;
        }
        return 0;
    }
    
    let kernel_fb = unsafe { &*(fb_info_ptr as *const crate::boot::FramebufferInfo) };
    
    // NUNCA usar dirección kernel - convertir a física para mapeo
    let fb_phys = if kernel_fb.base_address >= crate::memory::PHYS_MEM_OFFSET {
        kernel_fb.base_address - crate::memory::PHYS_MEM_OFFSET
    } else {
        kernel_fb.base_address
    };
    
    // Calculate framebuffer size correctly
    // pixels_per_scan_line * height * 4 bytes per pixel (32bpp)
    // Map 2x size to support double buffering in display_service
    let single_frame_size = (kernel_fb.pixels_per_scan_line * kernel_fb.height * 4) as u64;
    let mut fb_size = single_frame_size * 2;
    
    // Align to 4KB (page size)
    fb_size = (fb_size + 0xFFF) & !0xFFF;
    // Add 4MB padding for stride/alignment quirks (evita Page Fault al escribir en regiones adyacentes)
    fb_size = fb_size.saturating_add(0x400000);
    
    // Get current process page table
    let current_pid = crate::process::current_process_id();
    let page_table_phys = crate::process::get_process_page_table(current_pid);
    
    if page_table_phys == 0 {
        serial::serial_print("MAP_FB: ERROR - Could not get process page table\n");
        return 0;
    }
    
    // Map framebuffer into process address space (identity mapping: vaddr = phys)
    let vaddr = crate::memory::map_framebuffer_for_process(page_table_phys, fb_phys, fb_size);
    vaddr
}

/// sys_pci_enum_devices - Enumerate PCI devices by class
/// Args:
///   arg1: class_code (0xFF = all devices, 0x04 = multimedia/audio)
///   arg2: buffer pointer (array of PciDeviceInfo structs)
///   arg3: buffer size (max number of devices)
/// Returns: number of devices found, or u64::MAX on error
fn sys_pci_enum_devices(class_code: u64, buffer_ptr: u64, max_devices: u64) -> u64 {
    // Validate parameters
    if buffer_ptr == 0 || max_devices == 0 || max_devices > 256 {
        serial::serial_print("[SYSCALL] pci_enum_devices - invalid parameters\n");
        return u64::MAX;
    }

    // Validate the userspace buffer pointer before writing to it
    let buf_byte_len = max_devices * 8 * core::mem::size_of::<u64>() as u64;
    if !is_user_pointer(buffer_ptr, buf_byte_len) {
        serial::serial_print("[SYSCALL] pci_enum_devices - invalid buffer pointer\n");
        return u64::MAX;
    }
    
    // Get devices from PCI subsystem
    let devices = if class_code == 0x04 {
        // Multimedia/Audio devices
        crate::pci::find_audio_devices()
    } else if class_code == 0x0C {
        // Serial Bus Controllers (USB, etc.)
        crate::pci::find_usb_controllers()
    } else if class_code == 0x02 {
        // Network Controllers (Ethernet, etc.)
        crate::pci::find_network_controllers()
    } else if class_code == 0x03 {
        // Display Controllers (NVIDIA, etc.)
        crate::pci::find_nvidia_gpus()
    } else if class_code == 0xFF {
        // All devices - not implemented for now
        serial::serial_print("[SYSCALL] pci_enum_devices - all devices not supported yet\n");
        return 0;
    } else {
        serial::serial_print("[SYSCALL] pci_enum_devices - unsupported class\n");
        return 0;
    };
    
    let count = core::cmp::min(devices.len(), max_devices as usize);
    
    // Copy device info to userspace buffer
    // Each device is represented as: bus, device, function, vendor_id, device_id, class, subclass, bar0
    unsafe {
        let user_buf = core::slice::from_raw_parts_mut(
            buffer_ptr as *mut u64,
            count * 8  // 8 u64 fields per device
        );
        
        for (i, dev) in devices.iter().take(count).enumerate() {
            let offset = i * 8;
            user_buf[offset + 0] = dev.bus as u64;
            user_buf[offset + 1] = dev.device as u64;
            user_buf[offset + 2] = dev.function as u64;
            user_buf[offset + 3] = dev.vendor_id as u64;
            user_buf[offset + 4] = dev.device_id as u64;
            user_buf[offset + 5] = dev.class_code as u64;
            user_buf[offset + 6] = dev.subclass as u64;
            user_buf[offset + 7] = dev.bar0 as u64;
        }
    }
    
    count as u64
}


/// sys_pci_read_config - Read PCI configuration space
/// Args:
///   arg1: device location (bus << 16 | device << 8 | function)
///   arg2: offset in config space
///   arg3: size (1, 2, or 4 bytes)
/// Returns: value read, or u64::MAX on error
fn sys_pci_read_config(device_location: u64, offset: u64, size: u64) -> u64 {
    let bus = ((device_location >> 16) & 0xFF) as u8;
    let device = ((device_location >> 8) & 0xFF) as u8;
    let function = (device_location & 0xFF) as u8;

    // Validate parameters before truncating offset to u8 to avoid silent wrap-around
    if device > 31 || function > 7 || offset > 252 {
        serial::serial_print("[SYSCALL] pci_read_config - invalid parameters\n");
        return u64::MAX;
    }
    let offset = offset as u8;
    
    unsafe {
        match size {
            1 => crate::pci::pci_config_read_u8(bus, device, function, offset) as u64,
            2 => crate::pci::pci_config_read_u16(bus, device, function, offset) as u64,
            4 => crate::pci::pci_config_read_u32(bus, device, function, offset) as u64,
            _ => {
                serial::serial_print("[SYSCALL] pci_read_config - invalid size\n");
                u64::MAX
            }
        }
    }
}

/// sys_pci_write_config - Write PCI configuration space
/// Args:
///   arg1: device location (bus << 16 | device << 8 | function)
///   arg2: offset in config space
///   arg3: value to write (size determined by offset alignment)
/// Returns: 0 on success, u64::MAX on error
fn sys_pci_write_config(device_location: u64, offset: u64, value: u64) -> u64 {
    let bus = ((device_location >> 16) & 0xFF) as u8;
    let device = ((device_location >> 8) & 0xFF) as u8;
    let function = (device_location & 0xFF) as u8;

    // Validate parameters before truncating offset to u8 to avoid silent wrap-around
    if device > 31 || function > 7 || offset > 252 {
        serial::serial_print("[SYSCALL] pci_write_config - invalid parameters\n");
        return u64::MAX;
    }
    let offset = offset as u8;
    
    // For now, only allow writing to command register (offset 0x04)
    // This is a security measure - we don't want userspace to mess with arbitrary PCI config
    if offset != 0x04 {
        serial::serial_print("[SYSCALL] pci_write_config - only command register writes allowed\n");
        return u64::MAX;
    }
    
    unsafe {
        crate::pci::pci_config_write_u16(bus, device, function, offset, value as u16);
    }
    
    0
}

/// sys_mmap - Map memory into process address space
/// 
/// Arguments:
///   addr: Suggested address (0 = kernel chooses)
///   length: Number of bytes to map
///   prot: Protection flags (PROT_READ | PROT_WRITE | PROT_EXEC)
///   flags: MAP_PRIVATE | MAP_ANONYMOUS | MAP_SHARED
///   fd: File descriptor (ignored for anonymous mappings)
/// 
/// Returns: Address of mapped region, or u64::MAX on error
fn sys_mmap(addr: u64, length: u64, prot: u64, flags: u64, fd: u64, offset: u64) -> u64 {
    use crate::process::{self, VMARegion};
    use crate::memory;
    use crate::serial;

    if length == 0 || length > 0x0000_7FFF_FFFF_FFFF {
        return syscall_error_for_current_process(crate::scheme::error::EINVAL as i32);
    }
    let aligned_length = (length + 0xFFF) & !0xFFF;

    let current_pid = match process::current_process_id() {
        Some(pid) => pid,
        None => return syscall_error_for_current_process(crate::scheme::error::ESRCH as i32),
    };

    // Resolve file descriptor before acquiring process resources.
    // A file-backed mapping populates each page with content from the open file.
    // MAP_ANONYMOUS: mapping is not backed by a file.
    use linux_mmap_abi::{
        ANON_SLACK_BYTES, MAP_ANONYMOUS, MAP_FIXED, MAP_POPULATE, MAP_SHARED, USER_ARENA_HI,
        USER_ARENA_LO,
    };
    const MMAP_MAX_FD: u64 = crate::fd::MAX_FDS_PER_PROCESS as u64;
    let fd_entry = if (flags & MAP_ANONYMOUS) == 0 && fd < MMAP_MAX_FD {
        crate::fd::fd_get(current_pid, fd as usize)
} else {
        None
    };

    if let Some(mut proc) = process::get_process(current_pid) {
        let mut r = proc.resources.lock();
        let page_table_phys = r.page_table_phys;

        let map_fixed_in_mmap_arena = (flags & MAP_FIXED) != 0
            && addr >= USER_ARENA_LO
            && addr
                .checked_add(aligned_length)
                .map_or(false, |end| end <= USER_ARENA_HI);
        use linux_mmap_abi::PROT_EXEC;
        let anon_slack: u64 = if (flags & MAP_ANONYMOUS) != 0 && fd_entry.is_none() {
            // Anonymous mappings always get slack (unless MAP_FIXED outside the arena).
            if (flags & MAP_FIXED) == 0 || map_fixed_in_mmap_arena {
                ANON_SLACK_BYTES
            } else {
                0
            }
        } else if fd_entry.is_some() && (prot & PROT_EXEC) != 0 && (flags & MAP_FIXED) == 0 {
            // Non-MAP_FIXED file-backed PROT_EXEC mappings (e.g. library text segments
            // chosen by the kernel) also need slack pages so that multi-byte instructions
            // at the very end of the last mapped page can be fetched without a page fault.
            // Pages beyond file_len are already zeroed by the frame-allocation path.
            ANON_SLACK_BYTES
        } else {
            0
        };

        /// Rango [candidate, candidate+span) libre respecto a `vmas`.
        fn mmap_find_free(r: &crate::process::ProcessResources, span: u64) -> Option<u64> {
            let mut candidate = linux_mmap_abi::USER_ARENA_LO;
            while candidate < linux_mmap_abi::USER_ARENA_HI {
                // Find the highest end of any VMA that overlaps [candidate, candidate+span).
                // Jumping directly to that end skips all pages inside large mappings in O(n)
                // instead of O(vma_size/page_size * n).
                let next = r.vmas.iter()
                    .filter(|vma| candidate < vma.end && candidate.saturating_add(span) > vma.start)
                    .map(|vma| vma.end)
                    .max();
                match next {
                    None => return Some(candidate),
                    Some(end) => candidate = end,
                }
            }
            None
        }

        // Intervalo de páginas a mapear: como Linux, redondeo inferior de addr y superior de addr+len.
        let (map_start, map_end) = if addr != 0 && (flags & MAP_FIXED) != 0 {
            if (addr & 0xFFF) != 0 {
                drop(r);
                drop(proc);
                return syscall_error_for_current_process(crate::scheme::error::EINVAL as i32);
            }
            // Reject kernel-space MAP_FIXED addresses: the subsequent
            // unmap_user_range call would corrupt shared HHDM page tables.
            if addr >= memory::USER_SPACE_END {
                drop(r);
                drop(proc);
                return syscall_error_for_current_process(crate::scheme::error::EINVAL as i32);
            }
            let unmap_span = aligned_length.saturating_add(anon_slack);
            if addr < linux_mmap_abi::USER_EXEC_STACK_HI
                && addr.saturating_add(unmap_span) > linux_mmap_abi::USER_EXEC_STACK_LO
            {
                drop(r);
                drop(proc);
                return syscall_error_for_current_process(crate::scheme::error::EINVAL as i32);
            }
            memory::unmap_user_range(page_table_phys, addr, unmap_span);
            let t0 = addr;
            let t1 = addr.saturating_add(aligned_length);
            let t1_ext = t1.saturating_add(anon_slack);
            // Split VMAs that partially overlap [t0, t1_ext) instead of removing them
            // entirely.  A file-backed MAP_FIXED text segment may start inside a kernel
            // slack VMA; dropping the whole VMA leaves the preceding slack page untracked
            // and mmap_find_free then reuses it, causing a subsequent unmap that removes
            // the instruction-decode guard page → page-fault #14 on multi-byte instructions
            // at the last byte of the last mapped text page.
            vma_remove_range(&mut r.vmas, t0, t1_ext);
            (addr, t1)
        } else if addr != 0 {
            let ms = addr & !0xFFF;
            let me = (addr.saturating_add(length).saturating_add(0xFFF)) & !0xFFF;
            if me <= ms {
                drop(r);
                drop(proc);
                return syscall_error_for_current_process(crate::scheme::error::EINVAL as i32);
            }
            let span = me - ms;
            let need = span.saturating_add(anon_slack);
            let overlaps_exec_stack = ms < linux_mmap_abi::USER_EXEC_STACK_HI
                && me.saturating_add(anon_slack) > linux_mmap_abi::USER_EXEC_STACK_LO;
            let hint_ok = !overlaps_exec_stack
                && !r
                    .vmas
                    .iter()
                    .any(|vma| ms < vma.end && me.saturating_add(anon_slack) > vma.start);
            if hint_ok {
                (ms, me)
            } else if let Some(c) = mmap_find_free(&r, need) {
                (c, c + span)
            } else {
                drop(r);
                drop(proc);
                return syscall_error_for_current_process(crate::scheme::error::ENOMEM as i32);
            }
        } else if let Some(c) = mmap_find_free(&r, aligned_length.saturating_add(anon_slack)) {
            (c, c + aligned_length)
        } else {
            drop(r);
            drop(proc);
            return syscall_error_for_current_process(crate::scheme::error::ENOMEM as i32);
        };

        let mut map_end = map_end;
        map_end = map_end.saturating_add(anon_slack);

        let map_total = map_end - map_start;
        let num_pages_mapped = map_total / 4096;

        // Map pages with real physical frames. For shared file-backed mappings, we attempt
        // to use fmap to get the direct physical address. For private mappings (anonymous 
        // or file-backed), we allocate new frames and copy data if needed.
        let mut current = map_start;
        let end = map_end;
        let file_len = length as usize;
        let mut file_offset: usize = 0;

        let is_shared = (flags & MAP_SHARED) != 0;

        // Try to use fmap for shared mappings
        let mut fmap_phys_base = None;
        let mut wc_write_through = false;

        if is_shared {
            if let Some(ref fde) = fd_entry {
                if let Ok(phys) = crate::scheme::fmap(
                    fde.scheme_id,
                    fde.resource_id,
                    offset as usize,
                    map_total as usize,
                ) {
                    // Detect Write-Combining (WC) signal in bit 63
                    if (phys >> 63) != 0 {
                        wc_write_through = true;
                    }
                    fmap_phys_base = Some((phys & !(1 << 63)) as u64);
                }
            }
        }

        use linux_mmap_abi::PROT_MASK;
        let linux_p = prot & PROT_MASK;

        // Lazy (demand-paging) path: anonymous private mappings without MAP_POPULATE
        // are NOT backed by physical frames at mmap() time.  Frames are allocated on
        // first access by the page-fault demand-page handler in memory.rs.
        // This matches Linux behaviour and prevents the kernel heap from being exhausted
        // by large PROT_NONE reservations (e.g. musl malloc's 1 GiB initial mmap).
        //
        // Mappings that require eager allocation:
        //   • file-backed (fd_entry.is_some())
        //   • shared device mappings (fmap_phys_base.is_some())
        //   • MAP_POPULATE flag explicitly requested
        let is_lazy = (flags & MAP_ANONYMOUS) != 0
            && fd_entry.is_none()
            && fmap_phys_base.is_none()
            && (flags & MAP_POPULATE) == 0;

        if !is_lazy {
        while current < end {
            let frame_phys = if let Some(base) = fmap_phys_base {
                // Use physical frame directly from fmap
                base + (current - map_start)
            } else if let Some(phys) = memory::alloc_phys_frame_for_anon_mmap() {
                // Zero the frame via the higher-half direct mapping.
                let frame_virt = memory::PHYS_MEM_OFFSET + phys;
                unsafe { core::ptr::write_bytes(frame_virt as *mut u8, 0, 4096); }

            // For private file-backed mappings, read the next 4 KB of file data into the private frame.
            if let Some(ref fde) = fd_entry {
                let bytes_mapped = (current - map_start) as usize;
                let current_offset = offset as usize + bytes_mapped;
                
                let remaining = file_len.saturating_sub(bytes_mapped);
                if remaining > 0 {
                    let to_read = remaining.min(4096);
                    let frame_slice = unsafe {
                        core::slice::from_raw_parts_mut(frame_virt as *mut u8, to_read)
                    };
                    match crate::scheme::read(fde.scheme_id, fde.resource_id, frame_slice, current_offset as u64) {
                        Ok(n) => { /* OK */ }
                        Err(e) => {
                            serial::serial_printf(format_args!(
                                "[SYSCALL] mmap: file read error at offset {}: {}\n",
                                current_offset, e
                            ));
                        }
                    }
                }
            }
                phys
            } else {
                serial::serial_print("[SYSCALL] mmap: physical frame pool exhausted\n");
                // Never map to physical address 0 as a fallback: that turns allocation
                // failures into NULL dereferences in userspace (hard to debug).
                return syscall_error_for_current_process(crate::scheme::error::ENOMEM as i32);
            };

            let page_prot = mmap_pte_linux_prot(linux_p, anon_slack, map_end, current);
            let mut pte_leaf = memory::linux_prot_to_leaf_pte_bits(page_prot);
            if wc_write_through {
                pte_leaf |= x86_64::structures::paging::PageTableFlags::WRITE_THROUGH.bits();
            }
            memory::map_user_page_4kb(page_table_phys, current, frame_phys, pte_leaf);
            current += 4096;
        }
        } // end !is_lazy

        // MAP_FIXED + PROT_EXEC with no kernel slack yet (`anon_slack == 0`): add zeroed
        // executable slack pages after the mapping so multi-byte instructions at the last
        // byte of the last page (e.g. RIP=0x…1fff, CR2=0x…2000) do not #PF.
        //
        // Applies to (1) file-backed PT_LOAD-style maps (ld.so) and (2) anonymous MAP_FIXED
        // **outside** [USER_ARENA_LO, USER_ARENA_HI), where we intentionally omit
        // ANON_SLACK_BYTES — musl/cargo still need a tail guard for decode past page end.
        //
        // Two bugs fixed here vs the original approach:
        //
        // 1. The old code skipped pages that were already present (e.g. an eagerly
        //    populated page at this address from MAP_POPULATE or a file-backed mapping).
        //    Skipping them leaves the page mapped with NX set — an instruction fetch
        //    through the boundary gives error 0x15 (present, NX violation), and if the
        //    page is later freed the error becomes 0x14 (not present).
        //    Fix: always remap any existing page with exec permission.
        //
        // 2. The old code did NOT add the slack region to the VMA list, so
        //    `mmap_find_free` could silently place a new anonymous allocation there.
        //    This happens when the data PT_LOAD segment is far from text (e.g. 2 MB
        //    alignment): the gap between the text VMA and the data VMA is large enough
        //    that `mmap_find_free(ANON_SLACK_BYTES + aligned_len)` returns an address
        //    inside the slack.  The resulting `unmap_user_range` frees the slack page,
        //    and a subsequent `munmap` of that allocation leaves it "not present"
        //    (error 0x14) at the moment the CPU fetches instruction bytes across the
        //    page boundary.  Fix: register the slack region as a VMA entry.
        if (flags & MAP_FIXED) != 0
            && (linux_p & PROT_EXEC) != 0
            && anon_slack == 0
        {
            let slack_end = map_end.saturating_add(ANON_SLACK_BYTES);
            let slack_prot = memory::linux_prot_to_leaf_pte_bits(linux_p | PROT_EXEC);
            let mut sv = map_end;
            while sv < slack_end {
                if let Some(existing_phys) = memory::get_user_page_phys(page_table_phys, sv) {
                    // Page already present (e.g. eagerly allocated by a previous MAP_POPULATE
                    // or file-backed mapping at this address).  Remap it with exec permission
                    // so that instruction decoding across the page boundary succeeds.
                    memory::map_user_page_4kb(page_table_phys, sv, existing_phys, slack_prot);
                } else if let Some(phys) = memory::alloc_phys_frame_for_anon_mmap() {
                    let fv = memory::PHYS_MEM_OFFSET + phys;
                    unsafe { core::ptr::write_bytes(fv as *mut u8, 0, 4096); }
                    memory::map_user_page_4kb(page_table_phys, sv, phys, slack_prot);
                }
                sv = sv.saturating_add(4096);
            }
            // Register the slack region as a VMA so that mmap_find_free does not
            // allocate into this window.  Without this, a large gap between the text
            // and data PT_LOAD segments (common with 2 MB-aligned shared libraries)
            // lets the allocator reuse the slack address, and a later munmap of that
            // allocation leaves the page absent (error 0x14 on instruction fetch).
            r.vmas.push(VMARegion {
                start:             map_end,
                end:               slack_end,
                flags:             prot,
                file_backed:       false,
                anon_kernel_slack: ANON_SLACK_BYTES,
            });
        }

        r.vmas.push(VMARegion {
            start: map_start,
            end: map_end,
            flags: prot,
            file_backed: fd_entry.is_some(),
            anon_kernel_slack: anon_slack,
        });

        proc.mem_frames += if is_lazy { 0 } else { num_pages_mapped };
        drop(r);
        process::update_process(current_pid, proc);
        return map_start;
    }
    syscall_error_for_current_process(crate::scheme::error::ESRCH as i32)
}

/// Linux `mprotect` — used heavily by musl for RELRO (RW → R after relocations).
fn sys_mprotect(addr: u64, len: u64, prot: u64) -> u64 {
    use crate::memory;
    use crate::process;
    let Some(pid) = process::current_process_id() else {
        return syscall_error_for_current_process(crate::scheme::error::ESRCH as i32);
    };
    let Some(proc) = process::get_process(pid) else {
        return syscall_error_for_current_process(crate::scheme::error::ESRCH as i32);
    };
    let (page_table_phys, eff_addr, eff_len) = {
        let mut r = proc.resources.lock();
        let pt = r.page_table_phys;
        if len == 0 {
            (pt, addr, 0u64)
        } else {
            let Some(req_end) = addr.checked_add(len) else {
                return syscall_error_for_current_process(crate::scheme::error::EINVAL as i32);
            };

            // Update VMA flags (splitting if needed)
            vma_mprotect_range(&mut r.vmas, addr, req_end, prot);

            let (eff_addr, eff_end) =
                mprotect_expand_anon_slack(&r.vmas, addr, req_end, prot);
            let el = eff_end.saturating_sub(eff_addr);
            (pt, eff_addr, el)
        }
    };
    if memory::mprotect_user_range(page_table_phys, eff_addr, eff_len, prot) {
        0
    } else {
        syscall_error_for_current_process(crate::scheme::error::EINVAL as i32)
    }
}

fn sys_munmap(addr: u64, length: u64) -> u64 {
    use crate::process;
    use crate::memory;
    if length == 0 { return u64::MAX; }
    // Reject kernel-space addresses: unmapping through a kernel virtual address
    // would walk the shared bootloader HHDM page tables and corrupt them.
    if addr >= memory::USER_SPACE_END {
        return syscall_error_for_current_process(crate::scheme::error::EINVAL as i32);
    }
    if let Some(pid) = process::current_process_id() {
        if let Some(mut proc) = process::get_process(pid) {
            let mut r = proc.resources.lock();
            let page_table_phys = r.page_table_phys;
            memory::unmap_user_range(page_table_phys, addr, length);
            // Split VMAs that partially overlap [addr, addr+length) instead of removing
            // them entirely.  Removing a whole VMA on partial overlap (e.g. munmap of a
            // sub-range of a kernel slack VMA) leaves the remaining pages untracked:
            // mmap_find_free then treats them as free, the next allocation calls
            // unmap_user_range on them, and subsequent instruction-decode across a page
            // boundary causes a page-not-present fault (#14).
            vma_remove_range(&mut r.vmas, addr, addr.saturating_add(length));
            proc.mem_frames = proc.mem_frames.saturating_sub((length + 4095) / 4096);
            drop(r);
            process::update_process(pid, proc);
            return 0;
        }
    }
    u64::MAX
}

fn sys_brk(addr: u64) -> u64 {
    use crate::process;
    use crate::memory;
    use crate::serial;
    if let Some(pid) = process::current_process_id() {
        if let Some(mut proc) = process::get_process(pid) {
            let mut r = proc.resources.lock();
            let current_brk = r.brk_current;
            if addr == 0 { return current_brk; }
            if current_brk == 0 { return u64::MAX; }
            
            let old_page_end = (current_brk + 4095) & !4095;
            let new_page_end = (addr + 4095) & !4095;
            
            if new_page_end > old_page_end {
                let mut curr = old_page_end;
                while curr < new_page_end {
                    match memory::alloc_phys_frame_for_anon_mmap() {
                        Some(frame_phys) => {
                            // Zero the new heap page before handing it to userspace.
                            let frame_virt = memory::PHYS_MEM_OFFSET + frame_phys;
                            unsafe { core::ptr::write_bytes(frame_virt as *mut u8, 0, 4096); }
                            memory::map_user_page_4kb(
                                r.page_table_phys,
                                curr,
                                frame_phys,
                                memory::linux_prot_to_leaf_pte_bits(3),
                            );
                            proc.mem_frames += 1;
                        }
                        None => {
                            serial::serial_print("[SYSCALL] brk: physical frame pool exhausted\n");
                            // Per Linux brk(2): on failure return the current (unchanged) break.
                            // Update brk_current to reflect the pages we did successfully map.
                            let mapped_brk = curr;
                            r.brk_current = mapped_brk;
                            drop(r);
                            process::update_process(pid, proc);
                            return mapped_brk;
                        }
                    }
                    curr += 4096;
                }
            } else if new_page_end < old_page_end {
                let mut curr = new_page_end;
                while curr < old_page_end {
                    memory::unmap_user_range(r.page_table_phys, curr, 4096);
                    curr += 4096;
                }
                proc.mem_frames = proc.mem_frames.saturating_sub((old_page_end - new_page_end) / 4096);
            }
            
            r.brk_current = addr;
            drop(r);
            process::update_process(pid, proc);
            return addr;
        }
    }
    // Return the current break on error, per POSIX convention
    if let Some(pid) = process::current_process_id() {
        if let Some(proc) = process::get_process(pid) {
            return proc.resources.lock().brk_current;
        }
    }
    u64::MAX
}

/// sys_clone - Create a new thread or process
/// 
/// Arguments:
///   flags: CLONE_* flags determining what is shared
///   stack: Stack pointer for new thread (0 = kernel allocates)
///   parent_tid: Where to store TID in parent (can be 0)
///   context: Current process register context (needed for fork-style clone)
/// 
/// Returns: TID of new thread/process, or u64::MAX on error
fn sys_clone(flags: u64, stack: u64, parent_tid_arg: u64, context: &crate::process::Context) -> u64 {
    use crate::process;

    let raw_flags = flags;
    let ppid = current_process_id().unwrap_or(0);
    serial::serial_printf(format_args!(
        "[CLONE] enter pid={} raw_flags={:#x} stack={:#x} rdx/ptid={:#x} r8_tls={:#x} r10_ctid={:#x}\n",
        ppid,
        raw_flags,
        stack,
        parent_tid_arg,
        context.r8,
        context.r10
    ));

    // Linux encodes exit signal in the low byte of flags.
    let exit_signal = flags & 0xFF;
    let flags = flags & !0xFF;

    // CLONE_VM (0x100) and CLONE_THREAD (0x10000)
    const CLONE_VM: u64 = 0x00000100;
    const CLONE_THREAD: u64 = 0x00010000;
    // Flags commonly used by pthreads/musl/glibc. For now we accept them and
    // either already behave as-if shared (single address space) or we ignore.
    const CLONE_FS: u64 = 0x00000200;
    const CLONE_FILES: u64 = 0x00000400;
    const CLONE_SIGHAND: u64 = 0x00000800;
    const CLONE_VFORK: u64 = 0x00004000;
    const CLONE_SYSVSEM: u64 = 0x00040000;
    const CLONE_SETTLS: u64 = 0x00080000;
    const CLONE_PARENT_SETTID: u64 = 0x00100000;
    const CLONE_CHILD_CLEARTID: u64 = 0x00200000;
    const CLONE_DETACHED: u64 = 0x00400000;
    const CLONE_CHILD_SETTID: u64 = 0x01000000;
    const CLONE_IO: u64 = 0x80000000;

    /// Flags permitidos en `clone` sin `CLONE_THREAD` (fork / vfork vía `clone`).
    /// Alineado con la máscara del camino thread, sin `CLONE_THREAD`.
    const FORK_STYLE_CLONE_ALLOWED: u64 = CLONE_VM
        | CLONE_FS
        | CLONE_FILES
        | CLONE_SIGHAND
        | CLONE_VFORK
        | CLONE_SYSVSEM
        | CLONE_SETTLS
        | CLONE_PARENT_SETTID
        | CLONE_CHILD_CLEARTID
        | CLONE_DETACHED
        | CLONE_CHILD_SETTID
        | CLONE_IO;

    let linux_clone_caller = process::get_process(ppid)
        .map(|p| p.is_linux)
        .unwrap_or(false);

    // Fork-style clone: CLONE_THREAD not set → fork(2) / vfork vía clone.
    // - Linux exige `CLONE_VM` con `CLONE_VFORK` (EINVAL si no).
    // - `CLONE_VFORK|CLONE_VM`: padre bloqueado en `clone` hasta `execve` exitoso o `exit`
    //   del hijo; el hijo comparte `ProcessResources` (misma tabla de páginas y FD slot)
    //   hasta que `execve` duplica VM+FD (`vfork_detach_mm_for_exec_if_needed`).
    if (flags & CLONE_THREAD) == 0 {
        if (flags & CLONE_VFORK) != 0 && (flags & CLONE_VM) == 0 {
            serial::serial_printf(format_args!(
                "[CLONE] fork-style EINVAL: CLONE_VFORK without CLONE_VM pid={}\n",
                ppid
            ));
            return if linux_clone_caller {
                linux_abi_error(22)
            } else {
                u64::MAX
            };
        }
        if flags & !FORK_STYLE_CLONE_ALLOWED != 0 {
            serial::serial_printf(format_args!(
                "[CLONE] fork-style EINVAL pid={} bad_flags={:#x}\n",
                ppid,
                flags & !FORK_STYLE_CLONE_ALLOWED
            ));
            return if linux_clone_caller {
                linux_abi_error(22)
            } else {
                u64::MAX
            };
        }

        let vfork_block_parent =
            (flags & CLONE_VFORK) != 0 && (flags & CLONE_VM) != 0;

        serial::serial_printf(format_args!(
            "[CLONE] fork-style pid={} flags={:#x} exit_sig={} vfork_shared_vm={}\n",
            ppid, flags, exit_signal, vfork_block_parent
        ));
        let mut child_context = *context;
        child_context.rax = 0; // child sees 0 as fork() return value
        let child_pid = if vfork_block_parent {
            match process::vfork_process_shared_vm(&child_context) {
                Some(c) => c,
                None => {
                    serial::serial_printf(format_args!(
                        "[CLONE] vfork shared-vm FAIL pid={}\n",
                        ppid
                    ));
                    return if linux_clone_caller {
                        linux_abi_error(11)
                    } else {
                        u64::MAX
                    };
                }
            }
        } else {
            match process::fork_process(&child_context) {
                Some(c) => c,
                None => {
                    serial::serial_printf(format_args!(
                        "[CLONE] fork FAIL pid={} flags={:#x} exit_signal={}\n",
                        ppid, flags, exit_signal
                    ));
                    return if linux_clone_caller {
                        linux_abi_error(11)
                    } else {
                        u64::MAX
                    };
                }
            }
        };

        if vfork_block_parent {
            if let Some(mut p) = process::get_process(ppid) {
                p.vfork_waiting_for_child = Some(child_pid);
                process::update_process(ppid, p);
            }
        }

        serial::serial_printf(format_args!(
            "[CLONE] fork ok parent={} child={}\n",
            ppid, child_pid
        ));
        crate::scheduler::enqueue_process(child_pid);

        if vfork_block_parent {
            loop {
                let released = process::get_process(ppid)
                    .map(|p| p.vfork_waiting_for_child != Some(child_pid))
                    .unwrap_or(true);
                if released {
                    break;
                }
                yield_cpu();
            }
        }

        return child_pid as u64;
    }

    // Reject any other unknown flags to surface unexpected ABI changes early.
    let allowed = CLONE_VM
        | CLONE_THREAD
        | CLONE_FS
        | CLONE_FILES
        | CLONE_SIGHAND
        | CLONE_VFORK
        | CLONE_SYSVSEM
        | CLONE_SETTLS
        | CLONE_PARENT_SETTID
        | CLONE_CHILD_CLEARTID
        | CLONE_DETACHED
        | CLONE_CHILD_SETTID
        | CLONE_IO;
    if flags & !allowed != 0 {
        serial::serial_printf(format_args!(
            "[CLONE] unsupported extra flags pid={} bad={:#x} (allowed mask ok)\n",
            ppid,
            flags & !allowed
        ));
        return u64::MAX;
    }

    serial::serial_printf(format_args!(
        "[CLONE] thread-style pid={} flags={:#x} stack={:#x} set_tls={} fs_base_child={:#x}\n",
        ppid,
        flags,
        stack,
        (flags & CLONE_SETTLS) != 0,
        if (flags & CLONE_SETTLS) != 0 {
            context.r8
        } else {
            0u64
        }
    ));

    if let Some(parent_pid) = process::current_process_id() {
        if let Some(parent) = process::get_process(parent_pid) {
            // Share the resources Arc
            let resources = Arc::clone(&parent.resources);
            
            // Create a new process entry for this thread
            let mut thread = process::Process::new(resources);
            
            // New PID
            let tid = process::next_pid();
            thread.id = tid;

            // TGID Propagation: CLONE_THREAD means child shares the same PID as parent.
            if (flags & CLONE_THREAD) != 0 {
                thread.tgid = parent.tgid;
            } else {
                thread.tgid = tid;
            }

            thread.state = process::ProcessState::Blocked;
            thread.priority = parent.priority;
            thread.time_slice = parent.time_slice;
            thread.parent_pid = Some(parent_pid);
            // CLONE_SETTLS: use r8 (newtls syscall arg) as the child's FS base.
            // Without this, all threads share the parent's TLS, causing __pthread_exit
            // to corrupt the parent's pthread descriptor and call exit(0).
            if (flags & CLONE_SETTLS) != 0 {
                thread.fs_base = context.r8;
            } else {
                thread.fs_base = parent.fs_base;
            }
            thread.is_linux = parent.is_linux;
            
            // Copy registers from the current syscall context
            thread.context = *context;
            
            // Set the child return value (RAX=0)
            thread.context.rax = 0;
            
            // Allocate a kernel stack for this thread
            let kstack_size = 32768; // 32KB
            let kstack = alloc::vec![0u8; kstack_size];
            let kstack_top = kstack.as_ptr() as u64 + kstack_size as u64;
            core::mem::forget(kstack);
            let kstack_top_aligned = kstack_top & !0xF;
            thread.kernel_stack_top = kstack_top_aligned;

            // Prepare the kernel stack for return to userspace (iretq frame)
            let kstack_ptr = unsafe {
                let mut p = kstack_top_aligned as *mut u64;
                p = p.offset(-1);
                *p = 0x23; // SS
                p = p.offset(-1);
                *p = if stack != 0 { stack } else { context.rsp }; // User RSP
                p = p.offset(-1);
                *p = context.rflags; // RFLAGS
                p = p.offset(-1);
                *p = 0x1b; // CS
                p = p.offset(-1);
                *p = context.rip; // User RIP
                p
            };

            // New threads start at fork_child_setup, which will use the iretq frame
            // on the kernel stack to jump to userspace.
            thread.context.rip = crate::interrupts::fork_child_setup as *const () as u64;
            thread.context.rsp = kstack_ptr as u64;

            let mut success = false;
            x86_64::instructions::interrupts::without_interrupts(|| {
                let mut table = process::PROCESS_TABLE.lock();
                for (slot_idx, slot) in table.iter_mut().enumerate() {
                    if slot.is_none() || matches!(slot, Some(ref p) if p.state == process::ProcessState::Terminated && p.current_cpu == process::NO_CPU) {
                        *slot = Some(thread);
                        crate::ipc::register_pid_slot(tid, slot_idx);
                        success = true;
                        break;
                    }
                }
            });

            if success {
                // 1. CLONE_PARENT_SETTID: write the new TID into *ptid (rdx).
                if (flags & CLONE_PARENT_SETTID) != 0 {
                    let ptid_addr = context.rdx;
                    if ptid_addr != 0 && is_user_pointer(ptid_addr, 4) {
                        // Force page presence by reading first, then write.
                        unsafe {
                            let val = core::ptr::read_volatile(ptid_addr as *const u32);
                            core::ptr::write_volatile(ptid_addr as *mut u32, tid);
                        }
                    }
                }
                
                // 2. CLONE_CHILD_SETTID: write the new TID into *ctid (r10).
                if (flags & CLONE_CHILD_SETTID) != 0 {
                    let ctid_addr = context.r10;
                    if ctid_addr != 0 && is_user_pointer(ctid_addr, 4) {
                        // Force page presence by reading first, then write.
                        unsafe {
                            let _val = core::ptr::read_volatile(ctid_addr as *const u32);
                            core::ptr::write_volatile(ctid_addr as *mut u32, tid);
                        }
                    }
                }

                // 3. CLONE_CHILD_CLEARTID: store ctid address in thread struct (r10).
                if (flags & CLONE_CHILD_CLEARTID) != 0 {
                    if let Some(mut t) = process::get_process(tid) {
                        t.clear_child_tid = context.r10;
                        process::update_process(tid, t);
                    }
                }

                let child_tgid = if (flags & CLONE_THREAD) != 0 {
                    parent.tgid
                } else {
                    tid
                };
                serial::serial_printf(format_args!(
                    "[CLONE] thread OK parent={} new_tid={} tgid={}\n",
                    parent_pid, tid, child_tgid
                ));
                crate::scheduler::enqueue_process(tid);
                return tid as u64;
            }
            serial::serial_printf(format_args!(
                "[CLONE] thread FAIL pid={} no PROCESS_TABLE slot (tid={})\n",
                parent_pid, tid
            ));
        } else {
            serial::serial_printf(format_args!(
                "[CLONE] thread FAIL pid={} get_process(None)\n",
                ppid
            ));
        }
    } else {
        serial::serial_printf(format_args!(
            "[CLONE] thread FAIL current_process_id None (entry logged ppid={})\n",
            ppid
        ));
    }

    serial::serial_printf(format_args!(
        "[CLONE] exit ERR ppid={} raw_flags={:#x}\n",
        ppid, raw_flags
    ));
    u64::MAX
}

/// sys_thread_create — nuevo hilo (comparte VM del padre), primera ejecución en `entry` con **rdi** = `arg`.
fn sys_thread_create(stack_top: u64, entry: u64, arg: u64, context: &process::Context) -> u64 {
    use crate::process;

    if stack_top < 0x1000 || entry < 0x1000 {
        return u64::MAX;
    }
    if !is_user_pointer(stack_top.saturating_sub(8), 8) {
        return u64::MAX;
    }
    if !is_user_pointer(entry, 1) {
        return u64::MAX;
    }
    if arg != 0 && !is_user_pointer(arg, 8) {
        return u64::MAX;
    }

    // RSP inicial debe ser múltiplo de 16: el ABI SysV amd64 y código SIMD (p. ej. tiny-skia
    // con `movaps` respecto a RSP) asumen alineación; un `stack_top` arbitrario provoca #GP en ring 3.
    let stack_top = stack_top & !0xF;
    if stack_top < 0x1000 {
        return u64::MAX;
    }

    if let Some(parent_pid) = process::current_process_id() {
        if let Some(parent) = process::get_process(parent_pid) {
            let resources = Arc::clone(&parent.resources);
            let mut thread = process::Process::new(resources);
            let tid = process::next_pid();
            thread.id = tid;
            thread.state = process::ProcessState::Blocked;
            thread.priority = parent.priority;
            thread.time_slice = parent.time_slice;
            thread.parent_pid = Some(parent_pid);
            thread.fs_base = parent.fs_base;
            thread.is_linux = parent.is_linux;

            // Initialize thread context from syscall context
            thread.context = *context;
            thread.context.rip = entry;
            thread.context.rdi = arg;
            thread.context.rax = 0;

            // Allocate a kernel stack for this thread
            let kstack_size = 32768u32;
            let kstack = alloc::vec![0u8; kstack_size as usize];
            let kstack_top = kstack.as_ptr() as u64 + u64::from(kstack_size);
            core::mem::forget(kstack);
            let kstack_top_aligned = kstack_top & !0xF;
            thread.kernel_stack_top = kstack_top_aligned;

            // Prepare the kernel stack for return to userspace (iretq frame)
            let kstack_ptr = unsafe {
                let mut p = kstack_top_aligned as *mut u64;
                p = p.offset(-1);
                *p = 0x23; // SS
                p = p.offset(-1);
                *p = stack_top; // User RSP
                p = p.offset(-1);
                *p = context.rflags; // RFLAGS
                p = p.offset(-1);
                *p = 0x1b; // CS
                p = p.offset(-1);
                *p = entry; // User RIP
                p
            };

            // New threads start at fork_child_setup, which will use the iretq frame
            // on the kernel stack to jump to userspace.
            thread.context.rip = crate::interrupts::fork_child_setup as *const () as u64;
            thread.context.rsp = kstack_ptr as u64;

            let mut success = false;
            x86_64::instructions::interrupts::without_interrupts(|| {
                let mut table = process::PROCESS_TABLE.lock();
                for (slot_idx, slot) in table.iter_mut().enumerate() {
                    if slot.is_none()
                        || matches!(
                            slot,
                            Some(ref p) if p.state == process::ProcessState::Terminated
                                && p.current_cpu == process::NO_CPU
                        )
                    {
                        *slot = Some(thread);
                        crate::ipc::register_pid_slot(tid, slot_idx);
                        success = true;
                        break;
                    }
                }
            });

            if success {
                crate::scheduler::enqueue_process(tid);
                return tid as u64;
            }
        }
    }

    u64::MAX
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

struct FutexWaiter {
    addr: u64,
    pid: crate::process::ProcessId,
    bitset: u32,
}

static FUTEX_WAITERS: Mutex<alloc::vec::Vec<FutexWaiter>> = Mutex::new(alloc::vec::Vec::new());

/// sys_futex — Linux-compatible fast userspace mutex (x86-64 ABI).
///
/// op bits:
///   [6:0]  = command  (FUTEX_CMD_MASK = 0x7f)
///   [7]    = FUTEX_PRIVATE_FLAG (128)
///   [8]    = FUTEX_CLOCK_REALTIME (256)
fn sys_futex(uaddr: u64, op: u64, val: u64, timeout_ptr: u64, uaddr2: u64, val3: u32) -> u64 {
    use crate::process;

    let pid = process::current_process_id().unwrap_or(0);
    let cmd = op & 0x7F; // strip FUTEX_PRIVATE_FLAG and FUTEX_CLOCK_REALTIME

    match cmd {
        0 | 9 => {
            // FUTEX_WAIT (0) / FUTEX_WAIT_BITSET (9)
            // Block until *uaddr != val or woken by FUTEX_WAKE.
            let bitset: u32 = if cmd == 9 { val3 } else { 0xFFFF_FFFF };

            if !is_user_pointer(uaddr, 4) {
                return linux_abi_error(14); // EFAULT
            }

            // Add ourselves to the waiter list first, then re-check the value.
            // This ordering is important: a concurrent FUTEX_WAKE that fires between
            // the value check and the push would be lost if we pushed after.
            {
                let mut waiters = FUTEX_WAITERS.lock();
                // Ensure we don't have a stale entry for this process before adding a new one.
                // A process can only wait on one address at a time.
                waiters.retain(|w| w.pid != pid);
                waiters.push(FutexWaiter { addr: uaddr, pid, bitset });
            }

            // Re-read the futex word under our own (already-added) entry.
            let current_val = unsafe { *(uaddr as *const u32) };
            if current_val != val as u32 {
                // Value already changed — remove our entry and return EAGAIN.
                let mut waiters = FUTEX_WAITERS.lock();
                waiters.retain(|w| w.pid != pid);
                return linux_abi_error(11); // EAGAIN
            }

            // Calculate timeout if provided
            let timeout_ms = if timeout_ptr != 0 && is_user_pointer(timeout_ptr, 16) {
                let ts = unsafe { &*(timeout_ptr as *const Timespec) };
                if ts.tv_sec < 0 || ts.tv_nsec < 0 {
                    return linux_abi_error(22); // EINVAL
                }
                Some((ts.tv_sec as u64 * 1000) + (ts.tv_nsec as u64 / 1_000_000))
            } else {
                None
            };
            let start_ticks = crate::interrupts::ticks();

            // Block the current process and yield.
            match process::compare_and_set_process_state(
                pid,
                process::ProcessState::Running,
                process::ProcessState::Blocked,
            ) {
                Ok(true) => {
                    if let Some(ms) = timeout_ms {
                        crate::scheduler::add_sleep(pid, start_ticks.saturating_add(ms));
                    }

                    loop {
                        // Check if we were woken up (state changed back to Running/Runnable)
                        if let Some(p) = process::get_process(pid) {
                            if p.state != process::ProcessState::Blocked {
                                break;
                            }
                        }

                        // Check for timeout (even if we are Blocked, if we are in the sleep queue, 
                        // wake_sleeping_processes will set us to Ready and we will run again)
                        if let Some(ms) = timeout_ms {
                            if crate::interrupts::ticks() - start_ticks >= ms {
                                let mut waiters = FUTEX_WAITERS.lock();
                                waiters.retain(|w| w.pid != pid);
                                let _ = process::compare_and_set_process_state(
                                    pid,
                                    process::ProcessState::Blocked,
                                    process::ProcessState::Running,
                                );
                                return linux_abi_error(110); // ETIMEDOUT
                            }
                        }

                        crate::scheduler::yield_cpu();
                    }

                    // Final cleanup of waiter entry
                    let mut waiters = FUTEX_WAITERS.lock();
                    waiters.retain(|w| w.pid != pid);
                    0
                }
                _ => {
                    // Was already woken (or state wasn't Running); remove entry.
                    let mut waiters = FUTEX_WAITERS.lock();
                    waiters.retain(|w| w.pid != pid);
                    0
                }
            }
        }
        1 | 10 => {
            // FUTEX_WAKE (1) / FUTEX_WAKE_BITSET (10)
            // Wake up to `val` threads waiting on uaddr whose bitset overlaps.
            let bitset: u32 = if cmd == 10 { val3 } else { 0xFFFF_FFFF };
            let mut woken: u64 = 0;
            let mut waiters = FUTEX_WAITERS.lock();
            let mut i = 0;
            while i < waiters.len() && woken < val {
                if waiters[i].addr == uaddr && (waiters[i].bitset & bitset) != 0 {
                    let waiter_pid = waiters[i].pid;
                    crate::scheduler::enqueue_process(waiter_pid);
                    waiters.remove(i);
                    woken += 1;
                } else {
                    i += 1;
                }
            }
            woken
        }
        3 | 4 => {
            // FUTEX_REQUEUE (3) / FUTEX_CMP_REQUEUE (4)
            // Wake `val` threads on uaddr; requeue up to `timeout_ptr` threads to uaddr2.
            // CMP_REQUEUE also checks that *uaddr == val3 first.
            if cmd == 4 {
                if !is_user_pointer(uaddr, 4) {
                    return linux_abi_error(14); // EFAULT
                }
                let current_val = unsafe { *(uaddr as *const u32) };
                if current_val != val3 {
                    return linux_abi_error(11); // EAGAIN
                }
            }

            let max_requeue = timeout_ptr; // Linux ABI: timeout field carries val2 here
            let mut woken: u64 = 0;
            let mut requeued: u64 = 0;
            let mut waiters = FUTEX_WAITERS.lock();
            let mut i = 0;
            while i < waiters.len() {
                if waiters[i].addr == uaddr {
                    if woken < val {
                        let waiter_pid = waiters[i].pid;
                        crate::scheduler::enqueue_process(waiter_pid);
                        waiters.remove(i);
                        woken += 1;
                    } else if requeued < max_requeue {
                        waiters[i].addr = uaddr2;
                        requeued += 1;
                        i += 1;
                    } else {
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
            woken + requeued
        }
        5 => {
            // FUTEX_WAKE_OP
            // val3 encodes: op[31:28] | cmp[27:24] | oparg[23:12] | cmparg[11:0]
            // 1. Apply atomic operation to *uaddr2; save the old value.
            // 2. Wake `val` waiters on uaddr.
            // 3. If comparison(old_val, cmparg) is true, also wake `val2` waiters on uaddr2.
            //    val2 is carried in timeout_ptr per the Linux ABI.
            let val2 = timeout_ptr; // number of waiters to maybe wake on uaddr2

            let old_val2: Option<(u32, bool)> = if is_user_pointer(uaddr2, 4) {
                let ptr = uaddr2 as *mut u32;
                let op_num  = (val3 >> 28) & 0xF;
                let cmp     = (val3 >> 24) & 0xF;
                let oparg   = ((val3 >> 12) & 0xFFF) as u32;
                let cmparg  = (val3 & 0xFFF) as u32;

                // Determine the actual oparg (may be a shift amount).
                let effective_oparg = if op_num & 8 != 0 { 1u32 << (oparg & 31) } else { oparg };

                let old = unsafe { core::ptr::read_volatile(ptr) };
                let new_val = match op_num & 7 {
                    0 => effective_oparg,                  // FUTEX_OP_SET
                    1 => old.wrapping_add(effective_oparg),// FUTEX_OP_ADD
                    2 => old | effective_oparg,            // FUTEX_OP_OR
                    3 => old & !effective_oparg,           // FUTEX_OP_ANDN
                    4 => old ^ effective_oparg,            // FUTEX_OP_XOR
                    _ => old,
                };
                unsafe { core::ptr::write_volatile(ptr, new_val) };

                // Evaluate comparison against the OLD value.
                let cmp_ok = match cmp {
                    0 => old == cmparg,  // FUTEX_OP_CMP_EQ
                    1 => old != cmparg,  // FUTEX_OP_CMP_NE
                    2 => old <  cmparg,  // FUTEX_OP_CMP_LT
                    3 => old <= cmparg,  // FUTEX_OP_CMP_LE
                    4 => old >  cmparg,  // FUTEX_OP_CMP_GT
                    5 => old >= cmparg,  // FUTEX_OP_CMP_GE
                    _ => false,
                };

                Some((old, cmp_ok))
            } else {
                None
            };

            let do_wake_uaddr2 = old_val2.map_or(false, |(_, cmp_ok)| cmp_ok);

            let mut woken: u64 = 0;
            let mut waiters = FUTEX_WAITERS.lock();

            // Wake up to `val` threads on uaddr.
            let mut i = 0;
            while i < waiters.len() && woken < val {
                if waiters[i].addr == uaddr {
                    let waiter_pid = waiters[i].pid;
                    crate::scheduler::enqueue_process(waiter_pid);
                    waiters.remove(i);
                    woken += 1;
                } else {
                    i += 1;
                }
            }

            // Conditionally wake up to `val2` threads on uaddr2.
            if do_wake_uaddr2 {
                let mut woken2: u64 = 0;
                let mut i = 0;
                while i < waiters.len() && woken2 < val2 {
                    if waiters[i].addr == uaddr2 {
                        let waiter_pid = waiters[i].pid;
                        crate::scheduler::enqueue_process(waiter_pid);
                        waiters.remove(i);
                        woken2 += 1;
                    } else {
                        i += 1;
                    }
                }
                woken += woken2;
            }
            woken
        }
        6 => {
            // FUTEX_LOCK_PI — priority-inheritance locking (best-effort stub).
            // Return ENOSYS so the caller falls back to a non-PI path.
            linux_abi_error(38) // ENOSYS
        }
        7 => {
            // FUTEX_UNLOCK_PI — priority-inheritance unlock (best-effort stub).
            linux_abi_error(38) // ENOSYS
        }
        8 => {
            // FUTEX_TRYLOCK_PI (best-effort stub).
            linux_abi_error(38) // ENOSYS
        }
        11 | 12 => {
            // FUTEX_WAIT_REQUEUE_PI / FUTEX_CMP_REQUEUE_PI (best-effort stubs).
            linux_abi_error(38) // ENOSYS
        }
        _ => linux_abi_error(38), // ENOSYS
    }
}

/// Wake all processes waiting on the specified futex address.
/// This is used internally by the kernel (e.g., during thread exit).
pub fn futex_wake_all_atomic(uaddr: u64) {
    let mut waiters = FUTEX_WAITERS.lock();
    let mut i = 0;
    while i < waiters.len() {
        if waiters[i].addr == uaddr {
            let waiter_pid = waiters[i].pid;
            // Wake the process: enqueue_process will handle state transition to Ready.
            crate::scheduler::enqueue_process(waiter_pid);
            waiters.remove(i);
        } else {
            i += 1;
        }
    }
}

/// Block the current process for `ms` timer ticks (1 ms per tick at 1000 Hz PIT).
/// Used by `nanosleep` and Linux `poll`.
pub(crate) fn process_sleep_ms(ms: u64) -> u64 {
    if ms == 0 {
        yield_cpu();
        return 0;
    }

    let current_tick = crate::interrupts::ticks();
    let wake_tick = current_tick.saturating_add(ms);

    if let Some(pid) = current_process_id() {
        x86_64::instructions::interrupts::without_interrupts(|| {
            let _ = crate::process::modify_process_state(pid, crate::process::ProcessState::Blocked);
            crate::scheduler::add_sleep(pid, wake_tick);
        });
    }

    yield_cpu();
    0
}

/// sys_nanosleep - Sleep for specified time
/// 
/// Arguments:
///   req: Pointer to timespec structure { tv_sec: i64, tv_nsec: i64 }
/// 
/// Returns: 0 on success, u64::MAX on error (EFAULT/EINVAL)
fn sys_nanosleep(req: u64) -> u64 {
    // Validate user pointer: timespec is 2 × i64 = 16 bytes
    if !is_user_pointer(req, 16) {
        return u64::MAX;
    }

    let (tv_sec, tv_nsec): (i64, i64) = unsafe {
        let ptr = req as *const i64;
        // Use read_unaligned: the user-supplied pointer may not be 8-byte aligned.
        // On x86_64 the hardware tolerates unaligned loads, but ptr.read() is
        // defined to require alignment (Rust/LLVM UB if misaligned).
        (ptr.read_unaligned(), ptr.add(1).read_unaligned())
    };

    // Reject negative or out-of-range values (EINVAL)
    if tv_sec < 0 || tv_nsec < 0 || tv_nsec >= 1_000_000_000 {
        return u64::MAX;
    }

    // Calculate sleep duration in milliseconds (PIT runs at 1000 Hz = 1 tick/ms)
    let ms = (tv_sec as u64)
        .saturating_mul(1000)
        .saturating_add(tv_nsec as u64 / 1_000_000);

    process_sleep_ms(ms)
}

/// Linux `tkill` / `tgkill`-style: señal a un hilo por TID (aquí mismo modelo de proceso que `kill`).
fn sys_tkill(tid: u64, sig: u64) -> u64 {
    let target = tid as ProcessId;
    if crate::process::get_process(target).is_none() {
        return syscall_error_for_current_process(3); // ESRCH
    }
    sys_kill(tid, sig)
}

/// Linux `poll(2)` — multiplexación mínima para musl/cargo (POLLIN/POLLOUT en fds válidos).
fn sys_poll(fds_ptr: u64, nfds: u64, timeout_raw: u64) -> u64 {
    const POLLIN:  i16 = 0x0001;
    const POLLPRI: i16 = 0x0002;
    const POLLOUT: i16 = 0x0004;
    const POLLNVAL: i16 = 0x0020;

    let timeout = timeout_raw as i32;
    let Some(pid) = current_process_id() else {
        return linux_abi_error(9);
    };

    // `poll(NULL, 0, timeout)` se usa como temporizador.
    if nfds == 0 {
        if timeout == 0 {
            return 0;
        }
        if timeout < 0 {
            process_sleep_ms(u64::saturating_sub(u64::MAX, 1) / 2);
            return 0;
        }
        process_sleep_ms(timeout as u32 as u64);
        return 0;
    }

    let Some(total) = nfds.checked_mul(8) else {
        return linux_abi_error(22);
    };
    if total > 1024 * 1024 {
        return linux_abi_error(22);
    }
    if fds_ptr == 0 || !is_user_pointer(fds_ptr, total) {
        return linux_abi_error(14);
    }

    // Sleep duration per retry iteration (ms).
    const POLL_SLEEP_MS: u64 = 1;

    // Maximum number of retry iterations before giving up:
    //   timeout < 0 → infinite (u64::MAX)
    //   timeout = 0 → single check, no sleep (handled by early return below)
    //   timeout > 0 → approximately timeout ms; +2 accounts for rounding and
    //                 ensures at least one retry even for very short timeouts
    let max_retries: u64 = if timeout < 0 {
        u64::MAX
    } else {
        (timeout as u64).saturating_div(POLL_SLEEP_MS).saturating_add(2)
    };

    let mut retries: u64 = 0;
    loop {
        let mut ready: u64 = 0;

        for i in 0..nfds {
            let base = fds_ptr + i * 8;
            let fd = unsafe { (base as *const i32).read_unaligned() };
            let events = unsafe { ((base + 4) as *const i16).read_unaligned() };
            let revents_ptr = (base + 6) as *mut i16;

            if fd < 0 {
                unsafe { revents_ptr.write_unaligned(0) };
                continue;
            }

            let fd_u = fd as usize;
            let rev: i16 = if let Some(fd_entry) = crate::fd::fd_get(pid, fd_u) {
                // Convert poll event bits to scheme event bits and query the scheme.
                let scheme_ev =
                    (if events & (POLLIN | POLLPRI) != 0 { crate::scheme::event::POLLIN }  else { 0 }) |
                    (if events & POLLOUT             != 0 { crate::scheme::event::POLLOUT } else { 0 });

                // Fall back to "always ready" when the scheme does not implement poll
                // (default returns Ok(events)), so non-socket fds behave as before.
                let ready_ev = crate::scheme::poll(
                    fd_entry.scheme_id,
                    fd_entry.resource_id,
                    scheme_ev,
                ).unwrap_or(scheme_ev);

                let mut r: i16 = 0;
                if (ready_ev & crate::scheme::event::POLLIN) != 0 {
                    if events & POLLIN  != 0 { r |= POLLIN; }
                    if events & POLLPRI != 0 { r |= POLLPRI; }
                }
                if (ready_ev & crate::scheme::event::POLLOUT) != 0 && events & POLLOUT != 0 {
                    r |= POLLOUT;
                }

                if r & (POLLIN | POLLPRI | POLLOUT) != 0 {
                    ready = ready.saturating_add(1);
                }
                r
            } else {
                // Invalid fd — always counts as ready (POLLNVAL)
                ready = ready.saturating_add(1);
                POLLNVAL
            };

            unsafe { revents_ptr.write_unaligned(rev) };
        }

        // Return as soon as at least one fd is ready, or when we've exhausted the timeout.
        if ready > 0 || timeout == 0 || retries >= max_retries {
            return ready;
        }

        // No fds ready yet — yield the CPU briefly and retry.
        process_sleep_ms(POLL_SLEEP_MS);
        retries += 1;
    }
}


fn sys_fstat(fd: u64, stat_ptr: u64) -> u64 {
    if stat_ptr == 0 { return u64::MAX; }
    if !is_user_pointer(stat_ptr, core::mem::size_of::<LinuxStat>() as u64) {
        return u64::MAX;
    }
    
    if let Some(pid) = current_process_id() {
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            let mut stat = crate::scheme::Stat::default();
            
            // Call scheme fstat
            match crate::scheme::fstat(fd_entry.scheme_id, fd_entry.resource_id, &mut stat) {
                Ok(_) => {
                    // Return Linux ABI stat struct, not the internal scheme::Stat layout.
                    let lstat = LinuxStat {
                        st_dev: stat.dev,
                        st_ino: stat.ino,
                        st_nlink: stat.nlink as u64,
                        st_mode: stat.mode,
                        st_uid: stat.uid,
                        st_gid: stat.gid,
                        __pad0: 0,
                        st_rdev: stat.rdev,
                        st_size: stat.size as i64,
                        st_blksize: stat.blksize as i64,
                        st_blocks: stat.blocks as i64,
                        st_atime: stat.atime as u64,
                        st_atime_nsec: 0,
                        st_mtime: stat.mtime as u64,
                        st_mtime_nsec: 0,
                        st_ctime: stat.ctime as u64,
                        st_ctime_nsec: 0,
                        __unused: [0; 3],
                    };
                    unsafe {
                        core::ptr::write_unaligned(stat_ptr as *mut LinuxStat, lstat);
                    }
                    return 0;
                }
                Err(_) => {
                }
            }
        }
    }
    u64::MAX
}

/// Inicializar sistema de syscalls
pub fn init() {
    serial::serial_print("Syscall system initialized\n");
}

/// sys_arch_prctl - Architecture-specific process control (Syscall 32)
/// TLS support: ARCH_SET_FS / ARCH_GET_FS for x86_64 thread pointer (%fs base).
fn sys_arch_prctl(code: u64, addr: u64) -> u64 {
    const ARCH_SET_GS: u64 = 0x1001;
    const ARCH_SET_FS: u64 = 0x1002;
    const ARCH_GET_FS: u64 = 0x1003;
    const ARCH_GET_GS: u64 = 0x1004;

    let pid = match crate::process::current_process_id() {
        Some(p) => p,
        None => return u64::MAX,
    };

    match code {
        ARCH_SET_FS => {
            // Validate address (canonical, user-space)
            if addr > 0x0000_7FFF_FFFF_F000 {
                return u64::MAX;
            }

            // Update process struct (used on context switch)
            if let Some(mut proc) = crate::process::get_process(pid) {
                proc.fs_base = addr;
                crate::process::update_process(pid, proc);
            }

            // Apply immediately to current CPU (MSR FS_BASE)
            unsafe {
                use core::arch::asm;
                let msr_fs_base = 0xC0000100u32;
                let low = addr as u32;
                let high = (addr >> 32) as u32;
                asm!("wrmsr", in("ecx") msr_fs_base, in("eax") low, in("edx") high, options(nomem, nostack, preserves_flags));
            }
            0
        }
        ARCH_GET_FS => {
            // Linux ABI: write current fs_base to *addr (user pointer)
            if addr == 0 || !is_user_pointer(addr, 8) {
                return u64::MAX;
            }
            let fs_base = crate::process::current_process_id()
                .and_then(|p| crate::process::get_process(p))
                .map(|proc| proc.fs_base)
                .unwrap_or(0);
            unsafe {
                (addr as *mut u64).write(fs_base);
            }
            0
        }
        ARCH_SET_GS | ARCH_GET_GS => {
            // GS not used for TLS on x86_64; stub returns -ENOSYS
            u64::MAX
        }
        _ => {
            serial::serial_printf(format_args!("[SYSCALL] arch_prctl: Unsupported code {:#018X}\n", code));
            u64::MAX
        }
    }
}

/// Linux `membarrier(2)` (NR 324). musl/glibc consultan capacidades y emiten barreras explícitas.
/// Eclipse no implementa el modelo NUMA/global de Linux; `QUERY` anuncia solo `GLOBAL` y el resto es no-op con éxito.
fn sys_membarrier(cmd: u64, _flags: u64, _cpu_id: u64) -> u64 {
    const MEMBARRIER_CMD_QUERY: u64 = 0;
    const MEMBARRIER_CMD_GLOBAL: u64 = 1 << 0;

    if cmd == MEMBARRIER_CMD_QUERY {
        return MEMBARRIER_CMD_GLOBAL;
    }
    0
}

/// sys_getrandom - Fill buffer with random bytes
fn sys_getrandom(buf_ptr: u64, len: u64, _flags: u64) -> u64 {
    if buf_ptr == 0 || len == 0 || len > 1024 * 1024 {
        return u64::MAX;
    }

    if !is_user_pointer(buf_ptr, len) {
        return u64::MAX;
    }

    let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, len as usize) };
    
    // Try to use RDRAND instruction if available, otherwise fall back to RDTSC-based PRNG
    if has_rdrand() {
        fill_random_rdrand(buf);
    } else {
        fill_random_rdtsc(buf);
    }

    len
}

/// sys_memfd_create - Create an anonymous RAM-backed file (Linux syscall 319).
///
/// `name_ptr` is an optional debugging label (ignored).
/// `flags` may include MFD_CLOEXEC (0x0001) and MFD_ALLOW_SEALING (0x0002).
///
/// Returns an fd referencing the new anonymous file, or -errno on error.
fn sys_memfd_create(_name_ptr: u64, flags: u64) -> u64 {
    const MFD_CLOEXEC: u64 = 0x0001;
    // MFD_ALLOW_SEALING (0x0002) is accepted but sealing is not enforced.

    let pid = match crate::process::current_process_id() {
        Some(p) => p,
        None => return linux_abi_error(crate::scheme::error::ESRCH as i32),
    };

    // Open a new slot in the memfd scheme.
    let (scheme_id, resource_id) = match crate::scheme::open("memfd:/", 0, 0) {
        Ok(pair) => pair,
        Err(e) => return linux_abi_error(e as i32),
    };

    let fd_flags: u32 = if (flags & MFD_CLOEXEC) != 0 { 0x80000 } else { 0 }; // O_CLOEXEC=0x80000
    match crate::fd::fd_open(pid, scheme_id, resource_id, fd_flags) {
        Some(fd) => fd as u64,
        None => {
            let _ = crate::scheme::close(scheme_id, resource_id);
            linux_abi_error(crate::scheme::error::ENOMEM as i32)
        }
    }
}


/// Check if RDRAND instruction is available via CPUID
fn has_rdrand() -> bool {
    unsafe {
        let ecx: u32;
        
        // CPUID leaf 1: Feature Information
        // We only care about ECX, so we save/restore EBX to avoid LLVM issues
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "pop rbx",
            inout("eax") 1u32 => _,
            out("ecx") ecx,
            out("edx") _,
            options(nomem, nostack, preserves_flags)
        );
        
        // RDRAND is bit 30 of ECX
        (ecx & (1 << 30)) != 0
    }
}

/// Fill buffer with random data using RDRAND instruction
fn fill_random_rdrand(buf: &mut [u8]) {
    let mut offset = 0;
    
    // Fill 8 bytes at a time using RDRAND
    while offset + 8 <= buf.len() {
        let mut val: u64 = 0;
        let mut success = false;
        
        // Try up to 10 times (RDRAND can fail if entropy is low)
        for _ in 0..10 {
            unsafe {
                let mut cf: u8;
                core::arch::asm!(
                    "rdrand {val}",
                    "setc {cf}",
                    val = out(reg) val,
                    cf = out(reg_byte) cf,
                    options(nomem, nostack)
                );
                if cf != 0 {
                    success = true;
                    break;
                }
            }
        }
        
        if success {
            buf[offset..offset+8].copy_from_slice(&val.to_le_bytes());
            offset += 8;
        } else {
            // RDRAND failed, fall back to RDTSC for remaining bytes
            fill_random_rdtsc(&mut buf[offset..]);
            return;
        }
    }
    
    // Fill remaining bytes (less than 8)
    if offset < buf.len() {
        let mut val: u64 = 0;
        unsafe {
            for _ in 0..10 {
                let mut cf: u8;
                core::arch::asm!(
                    "rdrand {val}",
                    "setc {cf}",
                    val = out(reg) val,
                    cf = out(reg_byte) cf,
                    options(nomem, nostack)
                );
                if cf != 0 {
                    let remaining = buf.len() - offset;
                    let bytes = val.to_le_bytes();
                    buf[offset..].copy_from_slice(&bytes[..remaining]);
                    return;
                }
            }
        }
        // If RDRAND failed, fall back to RDTSC
        fill_random_rdtsc(&mut buf[offset..]);
    }
}

/// Fill buffer with random data using RDTSC-based PRNG (fallback)
fn fill_random_rdtsc(buf: &mut [u8]) {
    let mut seed = unsafe { core::arch::x86_64::_rdtsc() };
    for i in 0..buf.len() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        buf[i] = (seed >> 32) as u8;
    }
}

// --- Socket Syscalls ---

/// sys_socket - Create an endpoint for communication
/// domain: AF_UNIX=1, AF_INET=2
/// type: SOCK_STREAM=1, SOCK_DGRAM=2
/// protocol: 0
fn sys_socket(domain: u64, type_: u64, protocol: u64) -> u64 {
    let path = alloc::format!("socket:{}/{}/{}", domain, type_, protocol);
    match crate::scheme::open(&path, 0, 0) {
        Ok((scheme_id, resource_id)) => {
            if let Some(pid) = current_process_id() {
                if let Some(fd) = crate::fd::fd_open(pid, scheme_id, resource_id, 0) {
                    return fd as u64;
                }
            }
        }
        Err(e) => {
            serial::serial_printf(format_args!(
                "[SYSCALL] socket(domain={}, type={}) -> failed with error {}\n",
                domain, type_, e
            ));
            return linux_abi_error(e as i32);
        }
    }
    linux_abi_error(12) // ENOMEM or other internal error
}

/// sys_bind - Bind a name to a socket
/// fd: socket file descriptor
/// addr: pointer to sockaddr structure
/// addrlen: size of sockaddr structure
fn sys_bind(fd: u64, addr: u64, addrlen: u64) -> u64 {
    // Validate arguments
    if addr == 0 || addrlen < 2 {
        return linux_abi_error(22); // EINVAL
    }
    
    if !is_user_pointer(addr, addrlen) {
        return linux_abi_error(14); // EFAULT
    }
    
    // Read address family (first 2 bytes)
    let family = unsafe { *(addr as *const u16) };
    
    if family == 1 { // AF_UNIX
        // Path starts at offset 2
        // Max path len in sockaddr_un is 108
        // We need to parse strict C string or length-bounded string
        
        let path_start = addr + 2;
        let mut path_len = strlen_user_unique(path_start, (addrlen - 2) as usize);
        
        // Handle Abstract Sockets (Linux): starts with \0
        let is_abstract = if path_len == 0 && addrlen > 2 {
            let first_byte = unsafe { *(path_start as *const u8) };
            first_byte == 0
        } else {
            false
        };

        if is_abstract {
            // For abstract sockets, the name IS the buffer (up to addrlen-2)
            // We'll represent them as "@path" or similar in our internal string
            // or just use a special prefix. Let's use "@" prefix.
            path_len = (addrlen - 2) as u64;
            if path_len > 107 { path_len = 107; }
        }

        // Copy path to kernel temporary buffer
        let mut path_buf = [0u8; 110];
        let mut final_path_str = String::new();

        if is_abstract {
            final_path_str.push('@');
            unsafe {
                // Skip the leading null byte for the actual name
                core::ptr::copy_nonoverlapping((path_start + 1) as *const u8, path_buf.as_mut_ptr(), (path_len - 1) as usize);
                if let Ok(s) = core::str::from_utf8(&path_buf[0..(path_len - 1) as usize]) {
                    final_path_str.push_str(s);
                }
            }
        } else {
            if path_len > 107 { return u64::MAX; }
            unsafe {
                core::ptr::copy_nonoverlapping(path_start as *const u8, path_buf.as_mut_ptr(), path_len as usize);
            }
            if let Ok(s) = core::str::from_utf8(&path_buf[0..path_len as usize]) {
                final_path_str.push_str(s);
            }
        }
        
        // Create the file node (only for non-abstract)
        if !is_abstract {
            let file_path = alloc::format!("file:{}", final_path_str);
            if let Ok((_scheme_id, _resource_id)) = crate::scheme::open(&file_path, 0x40 | 0x80, 0o777) {
                 // Successfully created file node. 
            } else {
                // Warning only: UNIX sockets are managed in-memory, the node is for convenience only.
                serial::serial_printf(format_args!("[SYSCALL] bind warning: could not create node for path {}\n", final_path_str));
            }
        }

        if let (Some(pid), Some(scheme)) = (current_process_id(), crate::servers::get_socket_scheme()) {
            if let Some(fd_info) = crate::fd::fd_get(pid, fd as usize) {
                scheme.bind(fd_info.resource_id, final_path_str).ok();
                return 0;
            }
        }
    } else if family == 2 { // AF_INET
        // sockaddr_in: offset 2 is port (2 bytes, big endian), offset 4 is IP (4 bytes)
        let port_ptr = addr + 2;
        let ip_ptr = addr + 4;
        let port = unsafe { u16::from_be(*(port_ptr as *const u16)) };
        let ip = unsafe { *(ip_ptr as *const [u8; 4]) };
        
        let path = alloc::format!("{}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], port);

        if let (Some(pid), Some(scheme)) = (current_process_id(), crate::servers::get_socket_scheme()) {
            if let Some(fd_info) = crate::fd::fd_get(pid, fd as usize) {
                if scheme.bind(fd_info.resource_id, path).is_ok() {
                    return 0;
                }
            }
        }
    }
    
    u64::MAX
}

/// sys_listen - Listen for connections on a socket
fn sys_listen(fd: u64, backlog: u64) -> u64 {
    if let (Some(pid), Some(scheme)) = (current_process_id(), crate::servers::get_socket_scheme()) {
        if let Some(fd_info) = crate::fd::fd_get(pid, fd as usize) {
            match scheme.listen(fd_info.resource_id) {
                Ok(_) => return 0,
                Err(e) => return linux_abi_error(e as i32),
            }
        }
        return linux_abi_error(9); // EBADF
    }
    linux_abi_error(38) // ENOSYS
}

/// sys_accept - Accept a connection on a socket
fn sys_accept(fd: u64, addr: u64, addrlen: u64) -> u64 {
    serial::serial_printf(format_args!("[SYSCALL] accept(fd={}, addr={:#x}, addrlen={})\n", fd, addr, addrlen));
    
    if let (Some(pid), Some(scheme)) = (current_process_id(), crate::servers::get_socket_scheme()) {
        if let Some(fd_info) = crate::fd::fd_get(pid, fd as usize) {
            match scheme.accept(fd_info.resource_id) {
                Ok(new_res_id) => {
                    // Create a new FD for the accepted connection
                    if let Some(new_fd) = crate::fd::fd_open(pid, fd_info.scheme_id, new_res_id, fd_info.flags) {
                        return new_fd as u64;
                    }
                    return linux_abi_error(12); // ENOMEM
                },
                Err(e) => return linux_abi_error(e as i32),
            }
        }
        return linux_abi_error(9); // EBADF
    }
    linux_abi_error(38) // ENOSYS
}

fn sys_connect(fd: u64, addr: u64, addrlen: u64) -> u64 {
    if addr == 0 || addrlen < 2 { return linux_abi_error(22); } // EINVAL
    if !is_user_pointer(addr, addrlen) { return linux_abi_error(14); } // EFAULT
    let family = unsafe { *(addr as *const u16) };
    
    if family == 1 { // AF_UNIX
        let path_start = addr + 2;
        let path_len = strlen_user_unique(path_start, (addrlen - 2) as usize);
        let mut path_buf = [0u8; 108];
        if path_len > 107 { return linux_abi_error(22); } // EINVAL
        unsafe {
            core::ptr::copy_nonoverlapping(path_start as *const u8, path_buf.as_mut_ptr(), path_len as usize);
        }
        path_buf[path_len as usize] = 0;
        let path_str = match core::str::from_utf8(&path_buf[0..path_len as usize]) {
            Ok(s) => s,
            Err(_) => return linux_abi_error(22),
        };

        serial::serial_printf(format_args!("[SYSCALL] connect(fd={}, path='{}')\n", fd, path_str));

        if let (Some(pid), Some(scheme)) = (current_process_id(), crate::servers::get_socket_scheme()) {
            if let Some(fd_info) = crate::fd::fd_get(pid, fd as usize) {
                match scheme.connect(fd_info.resource_id, path_str) {
                    Ok(_) => return 0,
                    Err(e) => return linux_abi_error(e as i32),
                }
            }
            return linux_abi_error(9); // EBADF
        }
        return linux_abi_error(38); // ENOSYS
    } else if family == 2 { // AF_INET
        let port_ptr = addr + 2;
        let ip_ptr = addr + 4;
        let port = unsafe { u16::from_be(*(port_ptr as *const u16)) };
        let ip = unsafe { *(ip_ptr as *const [u8; 4]) };
        
        let path = alloc::format!("{}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], port);

        if let (Some(pid), Some(scheme)) = (current_process_id(), crate::servers::get_socket_scheme()) {
            if let Some(fd_info) = crate::fd::fd_get(pid, fd as usize) {
                match scheme.connect(fd_info.resource_id, &path) {
                    Ok(_) => return 0,
                    Err(e) => return linux_abi_error(e as i32),
                }
            }
            return linux_abi_error(9); // EBADF
        }
        return linux_abi_error(38); // ENOSYS
    }

    linux_abi_error(97) // EAFNOSUPPORT
}

/// sys_setsockopt - Set options on a socket
fn sys_setsockopt(_fd: u64, _level: u64, _optname: u64, _optval: u64, _optlen: u64) -> u64 {
    // Stub: Always return success
    0
}

/// sys_getsockopt - Get options on a socket
fn sys_getsockopt(fd: u64, level: u64, optname: u64, optval: u64, optlen_ptr: u64) -> u64 {
    const SOL_SOCKET: u64 = 1;
    const SO_PEERCRED: u64 = 17;

    serial::serial_printf(format_args!("[SYSCALL] getsockopt(fd={}, level={}, optname={}, optval={:#x}, optlen_ptr={:#x})\n", 
        fd, level, optname, optval, optlen_ptr));

    if level == SOL_SOCKET && optname == SO_PEERCRED {
        if optval == 0 || optlen_ptr == 0 || !is_user_pointer(optlen_ptr, 4) {
            return -(scheme_error::EINVAL as i64) as u64;
        }

        let provided_len = unsafe { *(optlen_ptr as *const u32) };
        if provided_len < 12 {
            return -(scheme_error::EINVAL as i64) as u64;
        }

        let pid = process::current_process_id().unwrap_or(0);
        let uid = 0;
        let gid = 0;

        if is_user_pointer(optval, 12) {
            unsafe {
                let ucred_ptr = optval as *mut u32;
                ucred_ptr.write_volatile(pid);
                ucred_ptr.add(1).write_volatile(uid);
                ucred_ptr.add(2).write_volatile(gid);
                
                *(optlen_ptr as *mut u32) = 12;
            }
            return 0;
        }
    }

    // Stub for other options: Always return success for now
    0
}

// Constants shared by sys_sendmsg / sys_recvmsg.
/// Size of `struct cmsghdr` on x86_64 (cmsg_len:8 + cmsg_level:4 + cmsg_type:4).
const CMSG_HDR_SIZE: u64 = 16;
/// `SOL_SOCKET` option level.
const SOL_SOCKET_LEVEL: i32 = 1;
/// `SCM_RIGHTS` — pass file descriptors as ancillary data.
const SCM_RIGHTS_TYPE: i32 = 1;
/// Maximum number of file descriptors that may be passed in a single sendmsg/recvmsg.
/// Wayland only ever passes one fd at a time; eight is a generous upper bound.
const MAX_PASS_FDS: usize = 8;

/// sys_sendmsg - Send a message on a socket (Linux syscall 46).
///
/// Reads the `struct msghdr` layout expected by the x86_64 Linux ABI:
///   offset  0 : msg_name       (*mut u8,  8 bytes)
///   offset  8 : msg_namelen    (u32,      4 bytes + 4 padding)
///   offset 16 : msg_iov        (*mut IoVec, 8 bytes)
///   offset 24 : msg_iovlen     (usize,    8 bytes)
///   offset 32 : msg_control    (*mut u8,  8 bytes)
///   offset 40 : msg_controllen (usize,    8 bytes)
///   offset 48 : msg_flags      (i32,      4 bytes)
///
/// For AF_UNIX sockets only SCM_RIGHTS ancillary data (fd passing) is
/// handled; everything else is silently ignored.
fn sys_sendmsg(fd: u64, msg_ptr: u64, _flags: u64) -> u64 {
    if msg_ptr == 0 || !is_user_pointer(msg_ptr, 56) {
        return linux_abi_error(14); // EFAULT
    }

    // Read iov pointer + length (offsets 16, 24).
    let msg_iov_ptr     = unsafe { *((msg_ptr + 16) as *const u64) };
    let msg_iovlen      = unsafe { *((msg_ptr + 24) as *const u64) } as usize;
    let msg_control     = unsafe { *((msg_ptr + 32) as *const u64) };
    let msg_controllen  = unsafe { *((msg_ptr + 40) as *const u64) } as usize;

    if msg_iovlen > 0 {
        if !is_user_pointer(msg_iov_ptr, msg_iovlen as u64 * 16) { return linux_abi_error(1); }
    }
    
    let mut total_len = 0usize;
    for i in 0..msg_iovlen {
        let iov_entry = msg_iov_ptr + (i as u64) * 16;
        let iov_len = unsafe { *((iov_entry + 8) as *const u64) } as usize;
        total_len += iov_len;
    }

    // ── Gather payload from iovec array ────────────────────────────────────
    // Cap total payload to CONNECTION_BUFFER_CAP: the socket buffer is bounded
    // to that size, so copying more is wasteful and — more importantly — a
    // process can pass arbitrarily large iov_len values (e.g. a 128 MiB SHM
    // buffer used as a scatter-gather entry) which would cause a kernel-heap
    // OOM panic identical to the one fixed in sys_recvmsg.
    const SENDMSG_MAX_CAP: usize = crate::servers::CONNECTION_BUFFER_CAP;
    let max_iov = msg_iovlen.min(64);
    let mut total_send: usize = 0;
    for i in 0..max_iov {
        let iov_entry = msg_iov_ptr + (i as u64) * 16;
        if !is_user_pointer(iov_entry, 16) { break; }
        let iov_len = unsafe { *((iov_entry + 8) as *const u64) } as usize;
        total_send = total_send.saturating_add(iov_len);
    }
    let total_send = total_send.min(SENDMSG_MAX_CAP);

    let mut all_data: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(total_send);
    let mut remaining = total_send;
    for i in 0..max_iov {
        if remaining == 0 { break; }
        let iov_entry = msg_iov_ptr + (i as u64) * 16;
        if !is_user_pointer(iov_entry, 16) { break; }
        let iov_base = unsafe { *(iov_entry as *const u64) };
        let iov_len  = unsafe { *((iov_entry + 8) as *const u64) } as usize;
        if iov_base == 0 || iov_len == 0 { continue; }
        let chunk = iov_len.min(remaining);
        if !is_user_pointer(iov_base, chunk as u64) { continue; }
        let slice = unsafe { core::slice::from_raw_parts(iov_base as *const u8, chunk) };
        all_data.extend_from_slice(slice);
        remaining -= chunk;
    }

    // ── Extract file descriptors from SCM_RIGHTS control message ───────────
    // CmsgHdr (x86_64): cmsg_len(8) + cmsg_level(4) + cmsg_type(4) = 16 bytes.
    // FD data starts at offset 16 (cmsg_align(16) == 16 on 64-bit).
    let mut fd_pairs: alloc::vec::Vec<(usize, usize)> = alloc::vec::Vec::new();
    if msg_control != 0 && msg_controllen as u64 >= CMSG_HDR_SIZE
        && is_user_pointer(msg_control, msg_controllen as u64)
    {
        let cmsg_len    = unsafe { *(msg_control as *const u64) } as u64;
        let cmsg_level  = unsafe { *((msg_control + 8) as *const i32) };
        let cmsg_type   = unsafe { *((msg_control + 12) as *const i32) };
        if cmsg_level == SOL_SOCKET_LEVEL && cmsg_type == SCM_RIGHTS_TYPE && cmsg_len >= CMSG_HDR_SIZE {
            let data_len = (cmsg_len - CMSG_HDR_SIZE) as usize;
            let n_fds    = data_len / core::mem::size_of::<i32>();
            if let Some(sender_pid) = crate::process::current_process_id() {
                for i in 0..n_fds.min(MAX_PASS_FDS) {
                    let fd_offset = msg_control + CMSG_HDR_SIZE + (i as u64) * 4;
                    if !is_user_pointer(fd_offset, 4) { break; }
                    let raw_fd = unsafe { *(fd_offset as *const i32) };
                    if let Some(fi) = crate::fd::fd_get(sender_pid, raw_fd as usize) {
                        fd_pairs.push((fi.scheme_id, fi.resource_id));
                    }
                }
            }
        }
    }

    // ── Write payload to socket connection buffer ───────────────────────────
    if let Some(pid) = crate::process::current_process_id() {
        if let Some(fd_info) = crate::fd::fd_get(pid, fd as usize) {
            if let Some(scheme) = crate::servers::get_socket_scheme() {
                // Enqueue FDs before writing data so the peer can retrieve them
                // alongside the bytes it reads next.
                if !fd_pairs.is_empty() {
                    scheme.socket_enqueue_fds(fd_info.resource_id, fd_pairs);
                }
                match scheme.socket_write_raw(fd_info.resource_id, &all_data) {
                    Ok(n) => return n as u64,
                    Err(e) => return linux_abi_error(e as i32),
                }
            }
        }
    }

    linux_abi_error(9) // EBADF
}

/// sys_recvmsg - Receive a message from a socket (Linux syscall 47).
///
/// Mirrors the layout described in sys_sendmsg.  Scatters incoming bytes
/// across the iovec array and delivers any pending SCM_RIGHTS fd batches
/// into the control buffer.
fn sys_recvmsg(fd: u64, msg_ptr: u64, flags: u64) -> u64 {
    if msg_ptr == 0 || !is_user_pointer(msg_ptr, 56) {
        return linux_abi_error(14); // EFAULT
    }

    let msg_iovlen      = unsafe { *((msg_ptr + 24) as *const u64) } as usize;
    let msg_iov_ptr     = unsafe { *((msg_ptr + 16) as *const u64) };
    let msg_iovlen      = unsafe { *((msg_ptr + 24) as *const u64) } as usize;
    let msg_control     = unsafe { *((msg_ptr + 32) as *const u64) };
    let msg_controllen  = unsafe { *((msg_ptr + 40) as *const u64) } as usize;
    
    serial::serial_printf(format_args!("[SYSCALL] recvmsg(fd={}, iovlen={}, controllen={}, flags={})\n", 
        fd, msg_iovlen, msg_controllen, flags));

    // ── Determine total iov capacity ────────────────────────────────────────
    // Cap to CONNECTION_BUFFER_CAP: the socket buffer itself is bounded to that
    // size, so there is never more data to read.  Without this cap a process
    // can pass arbitrarily large iov_len values (e.g. wlroots passes 128 MiB
    // receive buffers) causing a kernel-heap OOM panic.
    const RECVMSG_MAX_CAP: usize = crate::servers::CONNECTION_BUFFER_CAP;
    let max_iov = msg_iovlen.min(64);
    let mut total_cap: usize = 0;
    for i in 0..max_iov {
        let iov_entry = msg_iov_ptr + (i as u64) * 16;
        if !is_user_pointer(iov_entry, 16) { break; }
        let iov_len = unsafe { *((iov_entry + 8) as *const u64) } as usize;
        total_cap = total_cap.saturating_add(iov_len);
    }
    let total_cap = total_cap.min(RECVMSG_MAX_CAP);
    if total_cap == 0 { return 0; }

    // ── Read from socket connection buffer ──────────────────────────────────
    const MSG_DONTWAIT: u64 = 0x40;
    let nonblock = (flags & MSG_DONTWAIT) != 0;

    let (n_read, fd_pairs_opt) = if let Some(pid) = current_process_id() {
        if let Some(fd_info) = crate::fd::fd_get(pid, fd as usize) {
            if let Some(scheme) = crate::servers::get_socket_scheme() {
                let mut tmp = alloc::vec![0u8; total_cap];
                // Blocking loop: retry with a yield until data arrives or an error occurs.
                let result = loop {
                    match scheme.socket_read_raw(fd_info.resource_id, &mut tmp) {
                        Ok(n) => {
                            // Scatter bytes into iovec entries.
                            let mut written = 0usize;
                            for i in 0..max_iov {
                                if written >= n { break; }
                                let iov_entry = msg_iov_ptr + (i as u64) * 16;
                                if !is_user_pointer(iov_entry, 16) { break; }
                                let iov_base = unsafe { *(iov_entry as *const u64) };
                                let iov_len  = unsafe { *((iov_entry + 8) as *const u64) } as usize;
                                if iov_base == 0 || iov_len == 0 { continue; }
                                if !is_user_pointer(iov_base, iov_len as u64) { continue; }
                                let chunk = (n - written).min(iov_len);
                                unsafe {
                                    core::ptr::copy_nonoverlapping(
                                        tmp[written..].as_ptr(),
                                        iov_base as *mut u8,
                                        chunk,
                                    );
                                }
                                written += chunk;
                            }
                            break Ok((written, scheme.socket_dequeue_fds(fd_info.resource_id)));
                        }
                        Err(e) if e == crate::scheme::error::EAGAIN && !nonblock => {
                            // No data yet on a blocking socket — yield and retry.
                            // Safety: `scheme` is an Arc (reference count), not a lock guard.
                            // The fd table holding the Arc persists across yields, so the
                            // SocketScheme object remains alive for the lifetime of this call.
                            crate::scheduler::yield_cpu();
                            // socket_read_raw re-acquires its internal mutex on each call,
                            // allowing other threads/processes to write to the socket.
                        }
                        Err(e) => break Err(e),
                    }
                };
                match result {
                    Ok(pair) => pair,
                    Err(e) => return linux_abi_error(e as i32),
                }
            } else { return linux_abi_error(38); }
        } else { return linux_abi_error(9); }
    } else { return linux_abi_error(1); };

    // ── Deliver queued file descriptors (SCM_RIGHTS) and credentials (SCM_CREDENTIALS) ──
    if msg_control != 0 && is_user_pointer(msg_control, msg_controllen as u64) {
        let mut cmsg_offset = 0;
        let mut total_written = 0usize;

        // 1. Deliver FDs (SCM_RIGHTS)
        if let Some(fds) = fd_pairs_opt {
            if !fds.is_empty() {
                let needed = CMSG_HDR_SIZE as usize + fds.len() * 4;
                if msg_controllen >= cmsg_offset + needed {
                    if let Some(receiver_pid) = crate::process::current_process_id() {
                        let mut new_fds: alloc::vec::Vec<i32> = alloc::vec::Vec::new();
                        for (s_id, r_id) in &fds {
                            let recv_r_id = crate::scheme::dup_independent(*s_id, *r_id).unwrap_or(*r_id);
                            if let Some(new_fd) = crate::fd::fd_open(receiver_pid, *s_id, recv_r_id, 0) {
                                new_fds.push(new_fd as i32);
                            }
                        }
                        
                        unsafe {
                            let h_ptr = (msg_control + cmsg_offset as u64) as *mut u64;
                            h_ptr.write_volatile(needed as u64); // cmsg_len
                            *((msg_control + cmsg_offset as u64 + 8) as *mut i32) = 1; // SOL_SOCKET
                            *((msg_control + cmsg_offset as u64 + 12) as *mut i32) = 1; // SCM_RIGHTS
                            let data_ptr = (msg_control + cmsg_offset as u64 + 16) as *mut i32;
                            for (i, &fd_val) in new_fds.iter().enumerate() {
                                data_ptr.add(i).write_volatile(fd_val);
                            }
                        }
                        cmsg_offset += (needed + 7) & !7; // Align to 8 bytes
                        total_written += needed;
                    }
                }
            }
        }

        // 2. Deliver Peer Credentials (SCM_CREDENTIALS)
        // Musl seatd (and wlr_backend) often sets SO_PASSCRED or expects these.
        const SCM_CREDENTIALS: i32 = 2;
        let cred_needed = CMSG_HDR_SIZE as usize + 12; // sizeof(struct ucred)
        if msg_controllen >= cmsg_offset + cred_needed {
            let pid = current_process_id().unwrap_or(0);
            unsafe {
                let cmsg_ptr = msg_control + cmsg_offset as u64;
                *(cmsg_ptr as *mut u64) = cred_needed as u64; // cmsg_len
                *((cmsg_ptr + 8) as *mut i32) = 1; // SOL_SOCKET
                *((cmsg_ptr + 12) as *mut i32) = SCM_CREDENTIALS;
                let ucred_ptr = (cmsg_ptr + 16) as *mut u32;
                ucred_ptr.write_volatile(pid);
                ucred_ptr.add(1).write_volatile(0); // UID
                ucred_ptr.add(2).write_volatile(0); // GID
            }
            cmsg_offset += (cred_needed + 7) & !7; // Align to 8 bytes
            total_written += cred_needed;
            serial::serial_printf(format_args!("[SYSCALL] recvmsg injected SCM_CREDENTIALS (pid={})\n", pid));
        }

        // Update msg_controllen back to userspace via the pointer
        unsafe {
            *((msg_ptr + 40) as *mut u64) = total_written as u64;
        }
    }

    n_read as u64
}

/// Linux-compatible stat structure for x86_64
#[repr(C)]
struct LinuxStat {
    st_dev: u64,
    st_ino: u64,
    st_nlink: u64,
    st_mode: u32,
    st_uid: u32,
    st_gid: u32,
    __pad0: u32,
    st_rdev: u64,
    st_size: i64,
    st_blksize: i64,
    st_blocks: i64,
    st_atime: u64,
    st_atime_nsec: u64,
    st_mtime: u64,
    st_mtime_nsec: u64,
    st_ctime: u64,
    st_ctime_nsec: u64,
    __unused: [i64; 3],
}

#[inline(always)]
pub(crate) fn linux_makedev(major: u32, minor: u32) -> u64 {
    // Linux dev_t encoding (glibc/musl).
    // Equivalent to gnu_dev_makedev().
    let major = major as u64;
    let minor = minor as u64;
    ((major & 0xFFFF_F000) << 32)
        | ((major & 0x0000_0FFF) << 8)
        | ((minor & 0xFFFF_FF00) << 12)
        | (minor & 0x0000_00FF)
}

fn sys_fstatat(dirfd: u64, path_ptr: u64, stat_ptr: u64, flags: u64) -> u64 {
    let _ = (dirfd, flags); // AT_FDCWD / AT_SYMLINK_NOFOLLOW: best-effort for now

    if path_ptr == 0 || stat_ptr == 0 {
        return linux_abi_error(22); // EINVAL
    }
    if !is_user_pointer(stat_ptr, core::mem::size_of::<LinuxStat>() as u64) {
        return linux_abi_error(14); // EFAULT
    }

    let path_len = strlen_user_unique(path_ptr, 4096);
    if path_len == 0 {
        return linux_abi_error(22); // EINVAL
    }

    let mut path_buf = [0u8; MAX_PATH_LENGTH];
    if path_len >= MAX_PATH_LENGTH as u64 {
        return linux_abi_error(36); // ENAMETOOLONG
    }

    unsafe {
        core::ptr::copy_nonoverlapping(path_ptr as *const u8, path_buf.as_mut_ptr(), path_len as usize);
    }
    path_buf[path_len as usize] = 0;

    let path_str = match core::str::from_utf8(&path_buf[0..path_len as usize]) {
        Ok(s) => s,
        Err(_) => return linux_abi_error(22),
    };

    let scheme_path = user_path_to_scheme_path(path_str);
    match crate::scheme::open(&scheme_path, 0, 0) {
        Ok((scheme_id, resource_id)) => {
            let mut stat = crate::scheme::Stat::default();
            let fstat_ok = crate::scheme::fstat(scheme_id, resource_id, &mut stat).is_ok();
            let _ = crate::scheme::close(scheme_id, resource_id);
            if !fstat_ok {
                return linux_abi_error(5); // EIO
            }
            let lstat = LinuxStat {
                st_dev: stat.dev,
                st_ino: stat.ino,
                st_nlink: stat.nlink as u64,
                st_mode: stat.mode,
                st_uid: stat.uid,
                st_gid: stat.gid,
                __pad0: 0,
                st_rdev: stat.dev,
                st_size: stat.size as i64,
                st_blksize: stat.blksize as i64,
                st_blocks: stat.blocks as i64,
                st_atime: stat.atime as u64,
                st_atime_nsec: 0,
                st_mtime: stat.mtime as u64,
                st_mtime_nsec: 0,
                st_ctime: stat.ctime as u64,
                st_ctime_nsec: 0,
                __unused: [0; 3],
            };
            unsafe {
                core::ptr::write_unaligned(stat_ptr as *mut LinuxStat, lstat);
            }
            0
        }
        Err(e) => linux_abi_error(e as i32),
    }
}

/// sys_stat - Get file status by path
fn sys_stat(path_ptr: u64, stat_ptr: u64) -> u64 {
    // Linux stat(path, stat_ptr) is fstatat(AT_FDCWD, path, stat_ptr, 0)
    // We treat all paths as relative to root or absolute for now.
    sys_fstatat(0, path_ptr, stat_ptr, 0)
}

/// sys_writev - Vectorized write
fn sys_writev(fd: u64, iov_ptr: u64, iov_cnt: u64) -> u64 {
    if iov_ptr == 0 || iov_cnt == 0 || iov_cnt > 1024 {
        return linux_abi_error(22); // EINVAL
    }
    
    // iovec struct size is 16 bytes on x86_64
    if !is_user_pointer(iov_ptr, iov_cnt * 16) {
        return linux_abi_error(14); // EFAULT
    }
    
    let mut total_written = 0;
    for i in 0..iov_cnt {
        // IMPORTANT: `is_user_pointer()` only validates the range, not that the page is mapped.
        // A malicious/buggy process can pass an unmapped iovec pointer and trigger a kernel #PF.
        // Use the kernel fault-recovery mechanism to turn that into a normal syscall failure.
        let (base, len): (u64, u64) = unsafe {
            if crate::interrupts::set_recovery_point() {
                crate::interrupts::clear_recovery_point();
                return if total_written > 0 { total_written } else { u64::MAX };
            }
            let ptr = (iov_ptr + i * 16) as *const u64;
            let base = ptr.read_unaligned();
            let len = ptr.add(1).read_unaligned();
            crate::interrupts::clear_recovery_point();
            (base, len)
        };
        
        if len == 0 { continue; }
        let ret = sys_write(fd, base, len);
        if ret == u64::MAX {
            return if total_written > 0 { total_written } else { u64::MAX };
        }
        total_written += ret;
    }
    
    total_written
}

/// struct utsname - used by sys_uname
#[repr(C)]
struct Utsname {
    sysname: [u8; 65],
    nodename: [u8; 65],
    release: [u8; 65],
    version: [u8; 65],
    machine: [u8; 65],
    domainname: [u8; 65],
}

/// sys_uname - Get system information
fn sys_uname(buf_ptr: u64) -> u64 {
    if !is_user_pointer(buf_ptr, core::mem::size_of::<Utsname>() as u64) {
        return u64::MAX;
    }
    
    let mut uts = unsafe { core::mem::zeroed::<Utsname>() };
    
    let fill = |buf: &mut [u8; 65], s: &str| {
        let bytes = s.as_bytes();
        let len = bytes.len().min(64);
        buf[..len].copy_from_slice(&bytes[..len]);
    };
    
    fill(&mut uts.sysname, "EclipseOS");
    fill(&mut uts.nodename, "eclipse");
    fill(&mut uts.release, "0.2.0");
    fill(&mut uts.version, "Eclipse Microkernel v0.2.0");
    fill(&mut uts.machine, "x86_64");
    fill(&mut uts.domainname, "(none)");
    
    unsafe {
        *(buf_ptr as *mut Utsname) = uts;
    }
    
    0
}

/// sys_getcwd - Get current working directory
fn sys_getcwd(buf_ptr: u64, size: u64) -> u64 {
    if buf_ptr == 0 || size < 2 {
        return u64::MAX;
    }
    
    if !is_user_pointer(buf_ptr, 2) {
        return u64::MAX;
    }
    
    let kpath = b"/";
    unsafe {
        core::ptr::copy_nonoverlapping(kpath.as_ptr(), buf_ptr as *mut u8, 1);
        *((buf_ptr as *mut u8).add(1)) = 0;
    }
    
    buf_ptr
}

/// sys_getresuid / sys_getresgid - Get identity (stubbed to 0)
fn sys_getuid() -> u64 { 0 }
fn sys_geteuid() -> u64 { 0 }
fn sys_getgid() -> u64 { 0 }
fn sys_getegid() -> u64 { 0 }

fn sys_getresuid(ruid_ptr: u64, euid_ptr: u64, suid_ptr: u64) -> u64 {
    for ptr in &[ruid_ptr, euid_ptr, suid_ptr] {
        if *ptr != 0 && is_user_pointer(*ptr, 4) {
            let zero: u32 = 0;
            unsafe { core::ptr::copy_nonoverlapping(&zero, *ptr as *mut u32, 1); }
        }
    }
    0
}

fn sys_getresgid(rgid_ptr: u64, egid_ptr: u64, sgid_ptr: u64) -> u64 {
    for ptr in &[rgid_ptr, egid_ptr, sgid_ptr] {
        if *ptr != 0 && is_user_pointer(*ptr, 4) {
            unsafe { *(*ptr as *mut u32) = 0; }
        }
    }
    0
}

/// sys_getlogin - Get user name
fn sys_getlogin(buf: u64, len: u64) -> u64 {
    if buf == 0 || len < 5 || !is_user_pointer(buf, 5) {
        return u64::MAX;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(b"root\0".as_ptr(), buf as *mut u8, 5);
    }
    0
}

/// sys_set_tid_address - Stub for thread-local storage setup
fn sys_set_tid_address(tid_ptr: u64) -> u64 {
    if let Some(pid) = current_process_id() {
        if let Some(mut p) = crate::process::get_process(pid) {
            p.clear_child_tid = tid_ptr;
            crate::process::update_process(pid, p);
        }
    }
    sys_gettid() // Return current TID
}

/// sys_clock_gettime - Get real or monotonic time
fn sys_clock_gettime(clk_id: u64, tp_ptr: u64) -> u64 {
    if !is_user_pointer(tp_ptr, 16) {
        return u64::MAX;
    }
    
    let uptime_ms = crate::scheduler::get_stats().total_ticks;
    let (sec, nsec) = match clk_id {
        0 => { // CLOCK_REALTIME
            let offset = WALL_TIME_OFFSET.load(core::sync::atomic::Ordering::Relaxed);
            (offset + (uptime_ms / 1000), (uptime_ms % 1000) * 1_000_000)
        }
        1 | 4 => { // CLOCK_MONOTONIC | CLOCK_BOOTTIME
            (uptime_ms / 1000, (uptime_ms % 1000) * 1_000_000)
        }
        _ => return u64::MAX,
    };
    
    unsafe {
        let ptr = tp_ptr as *mut u64;
        ptr.write_unaligned(sec);
        ptr.add(1).write_unaligned(nsec);
    }
    
    0
}

/// sys_mkdir - Create a directory
fn sys_mkdir(path_ptr: u64, mode: u64) -> u64 {
    let path_len = strlen_user_unique(path_ptr, 4096);
    if path_len == 0 { return u64::MAX; }
    
    // Copy path to buffer
    let mut path_buf = [0u8; MAX_PATH_LENGTH];
    if path_len >= MAX_PATH_LENGTH as u64 { return u64::MAX; } // Path too long
    
    unsafe {
        core::ptr::copy_nonoverlapping(path_ptr as *const u8, path_buf.as_mut_ptr(), path_len as usize);
    }
    
    let path_str = match core::str::from_utf8(&path_buf[0..path_len as usize]) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };

    let scheme_path = user_path_to_scheme_path(path_str);
    match crate::scheme::mkdir(&scheme_path, mode as u32) {
        Ok(_) => 0,
        Err(e) => linux_abi_error(e as i32),
    }
}

/// sys_unlink - Delete a name and possibly the file it refers to
fn sys_unlink(path_ptr: u64) -> u64 {
    let path_len = strlen_user_unique(path_ptr, 4096);
    if path_len == 0 { return u64::MAX; }
    
    let mut path_buf = [0u8; MAX_PATH_LENGTH];
    if path_len >= MAX_PATH_LENGTH as u64 { return u64::MAX; }
    
    unsafe {
        core::ptr::copy_nonoverlapping(path_ptr as *const u8, path_buf.as_mut_ptr(), path_len as usize);
    }
    
    let path_str = match core::str::from_utf8(&path_buf[0..path_len as usize]) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };

    let scheme_path = user_path_to_scheme_path(path_str);
    match crate::scheme::unlink(&scheme_path) {
        Ok(_) => 0,
        Err(e) => linux_abi_error(e as i32),
    }
}

/// sys_rename - Renombra ruta (solo soportado en esquemas que implementen `rename`, p.ej. tmp en file:).
fn sys_rename(old_ptr: u64, new_ptr: u64) -> u64 {
    let old_len = strlen_user_unique(old_ptr, MAX_PATH_LENGTH);
    let new_len = strlen_user_unique(new_ptr, MAX_PATH_LENGTH);
    if old_len == 0 || new_len == 0 {
        return u64::MAX;
    }
    if old_len >= MAX_PATH_LENGTH as u64 || new_len >= MAX_PATH_LENGTH as u64 {
        return u64::MAX;
    }

    let mut old_buf = [0u8; MAX_PATH_LENGTH];
    let mut new_buf = [0u8; MAX_PATH_LENGTH];
    unsafe {
        core::ptr::copy_nonoverlapping(old_ptr as *const u8, old_buf.as_mut_ptr(), old_len as usize);
        core::ptr::copy_nonoverlapping(new_ptr as *const u8, new_buf.as_mut_ptr(), new_len as usize);
    }
    let old_str = match core::str::from_utf8(&old_buf[0..old_len as usize]) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };
    let new_str = match core::str::from_utf8(&new_buf[0..new_len as usize]) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };

    let old_scheme = user_path_to_scheme_path(old_str);
    let new_scheme = user_path_to_scheme_path(new_str);
    match crate::scheme::rename(&old_scheme, &new_scheme) {
        Ok(_) => 0,
        Err(_) => u64::MAX,
    }
}

/// sys_get_storage_device_count - Get the number of registered block devices
fn sys_get_storage_device_count() -> u64 {
    crate::storage::device_count() as u64
}

// ---------------------------------------------------------------------------
// sys_readdir - Listar entradas de un directorio
// ---------------------------------------------------------------------------
/// sys_readdir(path_ptr, buf_ptr, buf_size) — Eclipse syscall 539.
///
/// Lee los nombres de los hijos del directorio en `path` y los escribe en `buf`
/// separados por '\n'.  Devuelve el número de bytes escritos, o u64::MAX si el
/// directorio no existe o no se puede leer.
fn sys_readdir(path_ptr: u64, buf_ptr: u64, buf_size: u64) -> u64 {
    if path_ptr == 0 || buf_ptr == 0 || buf_size == 0 {
        return u64::MAX;
    }
    if !is_user_pointer(path_ptr, 1) || !is_user_pointer(buf_ptr, buf_size) {
        return u64::MAX;
    }

    // Leer la ruta
    let mut path_buf = [0u8; 1024];
    let path_len = unsafe {
        let mut l = 0usize;
        while l < 1023 {
            let b = *((path_ptr + l as u64) as *const u8);
            if b == 0 { break; }
            path_buf[l] = b;
            l += 1;
        }
        l
    };
    let path = match core::str::from_utf8(&path_buf[..path_len]) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };

    let scheme_path = user_path_to_scheme_path(path);
    
    // Attempt to open and read from scheme
    if let Ok((scheme_id, resource_id)) = crate::scheme::open(&scheme_path, 0, 0) {
        let mut bounce = [0u8; 4096];
        let read_len = core::cmp::min(buf_size as usize, bounce.len());
        let res = match crate::scheme::read(scheme_id, resource_id, &mut bounce[..read_len], 0) {
            Ok(n) => {
                unsafe {
                    core::ptr::copy_nonoverlapping(bounce.as_ptr(), buf_ptr as *mut u8, n);
                }
                n as u64
            }
            Err(_) => u64::MAX,
        };
        let _ = crate::scheme::close(scheme_id, resource_id);
        if res != u64::MAX {
            return res;
        }
    }

    // Fallback to legacy FS listing if scheme open failed or returned error
    // (This handles the case where FS doesn't use the scheme system yet for readdir)
    let clean = if path.starts_with('/') { &path[1..] } else { path };
    let fs_path = if clean.is_empty() { "" } else { clean };

    match crate::filesystem::list_dir_children(fs_path) {
        Ok(names) => {
            let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, buf_size as usize) };
            let mut written = 0usize;
            for name in &names {
                let bytes = name.as_bytes();
                if written + bytes.len() + 1 >= buf.len() { break; }
                buf[written..written + bytes.len()].copy_from_slice(bytes);
                written += bytes.len();
                buf[written] = b'\n';
                written += 1;
            }
            written as u64
        }
        Err(_) => u64::MAX,
    }
}

// ---------------------------------------------------------------------------
// sys_pipe - Crear un par de descriptores de fichero anónimos (POSIX pipe)
// ---------------------------------------------------------------------------
/// sys_pipe(pipefd_ptr) — Linux syscall 22.
/// Crea una pipe anónima y escribe los dos FDs en [pipefd[0], pipefd[1]]:
///   pipefd[0] → extremo de lectura
///   pipefd[1] → extremo de escritura
/// Devuelve 0 en éxito, u64::MAX en error.
fn sys_pipe(pipefd_ptr: u64) -> u64 {
    if pipefd_ptr == 0 || !is_user_pointer(pipefd_ptr, 8) {
        return u64::MAX;
    }

    let pid = match current_process_id() {
        Some(p) => p,
        None    => return u64::MAX,
    };

    // Crear el canal en el singleton global y obtener los resource_ids
    let (read_handle, write_handle) = crate::pipe::PIPE_SCHEME.new_pipe();

    // Averiguar el scheme_id del scheme "pipe" en el registro
    let scheme_id = match crate::scheme::get_scheme_id("pipe") {
        Some(id) => id,
        None     => return u64::MAX,
    };

    // Asignar FDs en la tabla del proceso actual
    let read_fd = match crate::fd::fd_open(pid, scheme_id, read_handle, 0) {
        Some(fd) => fd,
        None     => return u64::MAX,
    };
    let write_fd = match crate::fd::fd_open(pid, scheme_id, write_handle, 0) {
        Some(fd) => fd,
        None => {
            crate::fd::fd_close(pid, read_fd);
            return u64::MAX;
        }
    };

    // Escribir los FDs en el espacio de usuario
    unsafe {
        let ptr = pipefd_ptr as *mut u32;
        *ptr           = read_fd  as u32;
        *ptr.add(1)    = write_fd as u32;
    }

    serial::serial_printf(format_args!(
        "[PIPE] new pipe: read_fd={} write_fd={}\n", read_fd, write_fd
    ));
    0
}

// ---------------------------------------------------------------------------
// sys_sigaction - Registrar / consultar un manejador de señal
// ---------------------------------------------------------------------------
/// sys_sigaction(signum, new_action_ptr, old_action_ptr) — Linux syscall 13.
///
/// Estructura simplificada sigaction (solo handler, sin sa_mask/sa_flags):
///   [0..8]  → handler (usize): 0 = SIG_DFL, 1 = SIG_IGN, else = fn ptr
///
/// Devuelve 0 en éxito, u64::MAX en error.
/// Linux `madvise(2)` — sin efecto en Eclipse; musl/cargo lo usan (p. ej. MADV_DONTNEED).
fn sys_madvise(_addr: u64, _len: u64, _advice: u64) -> u64 {
    0
}

/// Linux `sigaltstack(2)` — pila alternativa para manejadores; stub para musl.
fn sys_sigaltstack(ss: u64, oss: u64) -> u64 {
    // stack_t: ss_sp (8), ss_flags (4), padding (4), ss_size (8)
    const STACK_T_BYTES: u64 = 24;
    const SS_DISABLE: u32 = 2;

    if ss != 0 && !is_user_pointer(ss, STACK_T_BYTES) {
        return linux_abi_error(14);
    }
    if oss != 0 {
        if !is_user_pointer(oss, STACK_T_BYTES) {
            return linux_abi_error(14);
        }
        unsafe {
            core::ptr::write_bytes(oss as *mut u8, 0, STACK_T_BYTES as usize);
            ((oss + 8) as *mut u32).write_unaligned(SS_DISABLE);
        }
    }
    0
}

/// sys_sigprocmask - change signal mask
/// how: 0=SIG_BLOCK, 1=SIG_UNBLOCK, 2=SIG_SETMASK
fn sys_sigprocmask(how: u64, set_ptr: u64, oldset_ptr: u64) -> u64 {
    let Some(pid) = current_process_id() else { return -(crate::scheme::error::ESRCH as isize) as u64 };
    
    let mut old_mask = 0u64;
    let mut new_set = 0u64;

    if set_ptr != 0 {
        new_set = unsafe { *(set_ptr as *const u64) };
    }

    let mut table = process::PROCESS_TABLE.lock();
    for slot in table.iter_mut() {
        if let Some(p) = slot {
            if p.id == pid {
                old_mask = p.signal_mask;
                
                if set_ptr != 0 {
                    match how {
                        0 => p.signal_mask |= new_set, // SIG_BLOCK
                        1 => p.signal_mask &= !new_set, // SIG_UNBLOCK
                        2 => p.signal_mask = new_set,  // SIG_SETMASK
                        _ => return -(crate::scheme::error::EINVAL as isize) as u64,
                    }
                    // SIGKILL and SIGSTOP cannot be blocked
                    p.signal_mask &= !((1 << 9) | (1 << 19));
                }
                break;
            }
        }
    }

    if oldset_ptr != 0 {
        unsafe {
            *(oldset_ptr as *mut u64) = old_mask;
        }
    }

    0
}

fn sys_sigaction(signum: u64, new_action_ptr: u64, old_action_ptr: u64) -> u64 {
    if signum >= 64 {
        return u64::MAX;
    }

    let pid = match current_process_id() {
        Some(p) => p,
        None    => return u64::MAX,
    };

    if let Some(mut proc) = crate::process::get_process(pid) {
        // Devolver el manejador anterior si se pidió
        if old_action_ptr != 0 && is_user_pointer(old_action_ptr, 8) {
            unsafe {
                *(old_action_ptr as *mut u64) = proc.signal_handlers[signum as usize];
            }
        }
        // Instalar el nuevo manejador
        if new_action_ptr != 0 && is_user_pointer(new_action_ptr, 8) {
            let new_handler = unsafe { *(new_action_ptr as *const u64) };
            proc.signal_handlers[signum as usize] = new_handler;
        }
        crate::process::update_process(pid, proc);
        0
    } else {
        u64::MAX
    }
}

/// sys_dup - Duplicate a file descriptor
fn sys_dup(oldfd: u64) -> u64 {
    if let Some(pid) = current_process_id() {
        if let Some(fd_entry) = crate::fd::fd_get(pid, oldfd as usize) {
            // Notificar al esquema de que el recurso ahora tiene un nuevo FD apuntándole.
            let _ = crate::scheme::dup(fd_entry.scheme_id, fd_entry.resource_id);
            match crate::fd::fd_push(pid, fd_entry) {
                Some(newfd) => return newfd as u64,
                None => {
                    // Si falló el push (tabla llena), debemos cerrar el recurso recién duppeado.
                    let _ = crate::scheme::close(fd_entry.scheme_id, fd_entry.resource_id);
                    return u64::MAX;
                }
            }
        }
    }
    u64::MAX
}

/// sys_dup2 - Duplicate a file descriptor to a specific one
fn sys_dup2(oldfd: u64, new_dest_fd: u64) -> u64 {
    if oldfd == new_dest_fd { return new_dest_fd; }
    if let Some(pid) = current_process_id() {
        if let Some(fd_entry) = crate::fd::fd_get(pid, oldfd as usize) {
            // First close new_dest_fd if open
            crate::fd::fd_close(pid, new_dest_fd as usize);
            // Notificar al esquema: nueva referencia al recurso.
            let _ = crate::scheme::dup(fd_entry.scheme_id, fd_entry.resource_id);
            if crate::fd::fd_push_at(pid, new_dest_fd as usize, fd_entry) {
                return new_dest_fd;
            }
        }
    }
    u64::MAX
}

/// sys_dup3 - Like dup2 but with flags (O_CLOEXEC)
fn sys_dup3(oldfd: u64, newfd: u64, flags: u64) -> u64 {
    // We ignore O_CLOEXEC (0x80000) for now as we don't implement FD_CLOEXEC properly yet
    sys_dup2(oldfd, newfd)
}

/// sys_fcntl - File control
fn sys_fcntl(fd: u64, cmd: u64, arg: u64) -> u64 {
    // Linux fcntl cmds: F_GETFL=3, F_SETFL=4, F_DUPFD=0, F_DUPFD_CLOEXEC=1030
    match cmd {
        0 | 1030 => sys_dup(fd),
        3 => { // F_GETFL
            if let Some(pid) = current_process_id() {
                if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
                    return fd_entry.flags as u64;
                }
            }
            2 // Default: O_RDWR
        }
        4 => { // F_SETFL — update fd flags and propagate O_NONBLOCK to the underlying resource
            const O_NONBLOCK: u32 = 0x800;
            if let Some(pid) = current_process_id() {
                if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
                    let new_flags = arg as u32;
                    crate::fd::fd_set_flags(pid, fd as usize, new_flags);
                    // Propagate O_NONBLOCK to pipe handles so PipeScheme::read respects it
                    let nonblock = (new_flags & O_NONBLOCK) != 0;
                    if let Some(pipe_scheme_id) = crate::scheme::get_scheme_id("pipe") {
                        if fd_entry.scheme_id == pipe_scheme_id {
                            crate::pipe::PIPE_SCHEME.set_nonblock(fd_entry.resource_id, nonblock);
                        }
                    }
                }
            }
            0
        }
        1 | 2 => 0, // F_GETFD / F_SETFD
        _ => 0, // Stub other cmds
    }
}

/// sys_readlink - Read value of a symbolic link
fn sys_readlink(path_ptr: u64, buf_ptr: u64, size: u64) -> u64 {
    if path_ptr == 0 || buf_ptr == 0 || size == 0 {
        return linux_abi_error(22); // EINVAL
    }
    let path_len = strlen_user_unique(path_ptr, 4096);
    if path_len == 0 {
        return linux_abi_error(2); // ENOENT
    }
    let mut path_buf = [0u8; MAX_PATH_LENGTH];
    if path_len >= MAX_PATH_LENGTH as u64 {
        return linux_abi_error(36); // ENAMETOOLONG
    }
    unsafe {
        core::ptr::copy_nonoverlapping(path_ptr as *const u8, path_buf.as_mut_ptr(), path_len as usize);
    }
    let path_str = match core::str::from_utf8(&path_buf[0..path_len as usize]) {
        Ok(s) => s,
        Err(_) => return linux_abi_error(22),
    };

    // Strip scheme prefix and leading slashes to get the path under /sys.
    let stripped = path_str
        .trim_start_matches("sys:")
        .trim_start_matches('/')
        .trim_start_matches("sys/");

    // The SysScheme exposes these as symlinks.
    let target: &[u8] = match stripped {
        "dev/char/226:0" => b"../../class/drm/card0",
        "dev/char/226:128" => b"../../class/drm/renderD128",
        "dev/char/226:0/device/subsystem" | "dev/char/226:128/device/subsystem" => b"../../../../bus/pci",
        "class/drm/card0/device" | "class/drm/renderD128/device" => b"../../../devices/pci0000:00/0000:00:02.0",
        "class/graphics/fb0/device" => b"../../../devices/pci0000:00/0000:00:02.0",
        "dev/char/29:0"  => b"../../class/graphics/fb0",
        "devices/pci0000:00/0000:00:02.0/subsystem" => b"../../../../bus/pci",
        _ => return linux_abi_error(22), // EINVAL — not a symlink
    };

    if !is_user_pointer(buf_ptr, size) {
        return linux_abi_error(14); // EFAULT
    }
    let copy_len = core::cmp::min(target.len(), size as usize);
    unsafe {
        core::ptr::copy_nonoverlapping(target.as_ptr(), buf_ptr as *mut u8, copy_len);
    }
    copy_len as u64
}

/// sys_setpgid - Establishes a process group ID.
fn sys_setpgid(pid_arg: u64, pgid_arg: u64) -> u64 {
    let current_pid = current_process_id().unwrap_or(0);
    let pid = if pid_arg == 0 { current_pid } else { pid_arg as u32 };
    let pgid = if pgid_arg == 0 { pid } else { pgid_arg as u32 };
    
    if let Some(mut proc) = crate::process::get_process(pid) {
        // En una implementación real, deberíamos verificar que 'pid' es el proceso actual
        // o un hijo en la misma sesión. Para Eclipse single-user, permitimos el cambio.
        proc.pgid = pgid;
        crate::process::update_process(pid, proc);
        serial::serial_printf(format_args!("[SYSCALL] setpgid(pid={}, pgid={}) OK\n", pid, pgid));
        return 0;
    }
    u64::MAX
}

/// sys_setsid - Creates a new session.
fn sys_setsid() -> u64 {
    if let Some(pid) = current_process_id() {
        if let Some(mut proc) = crate::process::get_process(pid) {
            proc.sid = pid;
            proc.pgid = pid;
            crate::process::update_process(pid, proc);
            serial::serial_printf(format_args!("[SYSCALL] setsid() -> {}\n", pid));
            return pid as u64;
        }
    }
    u64::MAX
}

/// sys_getpgid - Get the process group ID.
fn sys_getpgid(pid_arg: u64) -> u64 {
    let pid = if pid_arg == 0 { current_process_id().unwrap_or(0) } else { pid_arg as u32 };
    if let Some(proc) = crate::process::get_process(pid) {
        return proc.pgid as u64;
    }
    u64::MAX
}

/// sys_getpgrp - Get process group ID of current process.
fn sys_getpgrp() -> u64 {
    sys_getpgid(0)
}

/// sys_sethostname - Set system hostname
fn sys_sethostname(name_ptr: u64, len: u64) -> u64 {
    0 // Stub
}

/// sys_prlimit64 - Get/set resource limits
fn sys_prlimit64(pid: u64, resource: u64, new_limit: u64, old_limit: u64) -> u64 {
    // If old_limit is provided, return some huge default limits
    if old_limit != 0 && is_user_pointer(old_limit, 16) {
        unsafe {
            let ptr = old_limit as *mut u64;
            ptr.write_unaligned(1024 * 1024); // soft
            ptr.add(1).write_unaligned(1024 * 1024); // hard
        }
    }
    0
}

/// sys_pselect6 - Synchronous I/O multiplexing
fn sys_pselect6(nfds: u64, readfds: u64, writefds: u64, exceptfds: u64, timeout: u64, sigmask: u64) -> u64 {
    // Very basic stub: if it's used for sleeping (no FDs), use sys_nanosleep or just yield
    if nfds == 0 && timeout != 0 {
        return sys_nanosleep(timeout);
    }
    0 // Return 0 (no FDs ready)
}
/// sys_faccessat - Check file permissions
fn sys_faccessat(dirfd: u64, path_ptr: u64, mode: u64, flags: u64) -> u64 {
    if path_ptr == 0 { return u64::MAX; }
    
    let path_len = strlen_user_unique(path_ptr, 4096);
    if path_len == 0 { return u64::MAX; }
    
    let mut path_buf = [0u8; MAX_PATH_LENGTH];
    if path_len >= MAX_PATH_LENGTH as u64 { return u64::MAX; }
    
    unsafe {
        core::ptr::copy_nonoverlapping(path_ptr as *const u8, path_buf.as_mut_ptr(), path_len as usize);
    }
    path_buf[path_len as usize] = 0;
    
    let path_str = match core::str::from_utf8(&path_buf[0..path_len as usize]) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };

    let scheme_path = user_path_to_scheme_path(path_str);
    if let Ok((scheme_id, resource_id)) = crate::scheme::open(&scheme_path, 0, 0) {
        crate::scheme::close(scheme_id, resource_id).ok();
        return 0; // File exists, permit access for now
    }
    
    u64::MAX // ENOENT or EACCES fallback
}

// ─────────────────────────────────────────────────────────────────────────────
// Bash / musl compatibility syscalls
// ─────────────────────────────────────────────────────────────────────────────

/// sys_rt_sigreturn — stub; proper implementation would restore signal context.
fn sys_rt_sigreturn() -> u64 {
    0
}

/// sys_wait4 — Linux wait4(pid, *wstatus, options, *rusage).
/// arg1 = pid  (-1 = any child, 0 = any in group, >0 = specific)
/// arg2 = *wstatus pointer (may be 0)
/// arg3 = options (WNOHANG=1, WUNTRACED=2, WCONTINUED=8)
fn sys_wait4_linux(pid: u64, status_ptr: u64, options: u64) -> u64 {
    // Map pid to wait_pid: negative/zero means "any child" (0 in our impl).
    let wait_pid: u64 = if (pid as i64) <= 0 {
        0 // wait for any child
    } else {
        pid
    };
    sys_wait_impl(status_ptr, wait_pid, options)
}

/// sys_execve — Linux execve(path, argv[], envp[]).
/// Reads the executable file, replaces the current process image, sets up
/// argc/argv/envp/auxv on the user stack and jumps to the new entry point.
fn sys_execve(path_ptr: u64, argv_ptr: u64, envp_ptr: u64) -> u64 {
    set_last_exec_error(b"execve: (no reason)");

    // 1. Read path from userspace.
    const MAX_PATH: usize = 1024;
    let path_len = strlen_user_unique(path_ptr, MAX_PATH);
    if path_ptr == 0 || path_len == 0 || path_len >= MAX_PATH as u64 {
        set_last_exec_error(b"execve: invalid path pointer");
        return linux_abi_error(14); // EFAULT
    }
    if !is_user_pointer(path_ptr, path_len + 1) {
        return linux_abi_error(14);
    }
    let path_str = unsafe {
        let s = core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize);
        match core::str::from_utf8(s) {
            Ok(v) => v,
            Err(_) => return linux_abi_error(22), // EINVAL
        }
    };

    // Resolve path against cwd if relative.
    let resolved_path: alloc::string::String;
    let path = if path_str.starts_with('/') {
        path_str
    } else {
        if let Some(pid) = current_process_id() {
            resolved_path = crate::process::resolve_path_cwd(pid, path_str);
            &resolved_path
        } else {
            path_str
        }
    };

    // 2. Read argv[] from userspace (null-terminated array of char* pointers).
    /// Tope acumulado de bytes copiados para argv+env en el heap del kernel (evita E2BIG-style OOM).
    const MAX_EXECVE_ARG_ENV_BYTES: usize = 4 * 1024 * 1024;
    let mut argv_env_byte_total: usize = 0;

    let mut argv_strings: alloc::vec::Vec<alloc::vec::Vec<u8>> = alloc::vec::Vec::new();
    if argv_ptr != 0 {
        let mut ptr_off: u64 = argv_ptr;
        for _ in 0..256usize {
            if !is_user_pointer(ptr_off, 8) { break; }
            let arg_ptr = unsafe { *(ptr_off as *const u64) };
            if arg_ptr == 0 { break; }
            let arg_len = strlen_user_unique(arg_ptr, 4096);
            let s = if arg_len == 0 || !is_user_pointer(arg_ptr, arg_len + 1) {
                b"\0".to_vec()
            } else {
                let mut s = unsafe {
                    core::slice::from_raw_parts(arg_ptr as *const u8, arg_len as usize).to_vec()
                };
                s.push(0); // null-terminate
                s
            };
            argv_env_byte_total = argv_env_byte_total.saturating_add(s.len());
            if argv_env_byte_total > MAX_EXECVE_ARG_ENV_BYTES {
                set_last_exec_error(b"execve: argv/env too large");
                return linux_abi_error(7); // E2BIG
            }
            argv_strings.push(s);
            ptr_off += 8;
        }
    }
    if argv_strings.is_empty() {
        // argv[0] = basename of path
        let base = path.rsplit('/').next().unwrap_or(path);
        let mut s = base.as_bytes().to_vec();
        s.push(0);
        argv_env_byte_total = argv_env_byte_total.saturating_add(s.len());
        if argv_env_byte_total > MAX_EXECVE_ARG_ENV_BYTES {
            set_last_exec_error(b"execve: argv/env too large");
            return linux_abi_error(7);
        }
        argv_strings.push(s);
    }

    // 3. Read envp[] from userspace.
    let mut envp_strings: alloc::vec::Vec<alloc::vec::Vec<u8>> = alloc::vec::Vec::new();
    if envp_ptr != 0 {
        let mut ptr_off: u64 = envp_ptr;
        for _ in 0..1024usize {
            if !is_user_pointer(ptr_off, 8) { break; }
            let env_ptr = unsafe { *(ptr_off as *const u64) };
            if env_ptr == 0 { break; }
            let env_len = strlen_user_unique(env_ptr, 65536);
            if env_len == 0 { ptr_off += 8; continue; }
            if !is_user_pointer(env_ptr, env_len + 1) { ptr_off += 8; continue; }
            let mut s = unsafe {
                core::slice::from_raw_parts(env_ptr as *const u8, env_len as usize).to_vec()
            };
            s.push(0);
            argv_env_byte_total = argv_env_byte_total.saturating_add(s.len());
            if argv_env_byte_total > MAX_EXECVE_ARG_ENV_BYTES {
                set_last_exec_error(b"execve: argv/env too large");
                return linux_abi_error(7);
            }
            envp_strings.push(s);
            ptr_off += 8;
        }
    }
    // Ensure minimal envp for bash if none provided.
    if envp_strings.is_empty() {
        let minimal_bytes: usize = crate::elf_loader::MINIMAL_ENVP.iter().map(|e| e.len()).sum();
        if argv_env_byte_total.saturating_add(minimal_bytes) > MAX_EXECVE_ARG_ENV_BYTES {
            set_last_exec_error(b"execve: argv/env too large");
            return linux_abi_error(7);
        }
        for e in crate::elf_loader::MINIMAL_ENVP {
            envp_strings.push(e.to_vec());
        }
        argv_env_byte_total = argv_env_byte_total.saturating_add(minimal_bytes);
    }

    // 4. Replace current process image.
    let current_pid = match current_process_id() {
        Some(p) => p,
        None => return linux_abi_error(3), // ESRCH
    };
    if let Err(msg) = crate::process::vfork_detach_mm_for_exec_if_needed(current_pid) {
        set_last_exec_error(msg.as_bytes());
        return linux_abi_error(12); // ENOMEM / recurso
    }
    let res = match crate::elf_loader::replace_process_image_path(current_pid, path) {
        Ok(r) => r,
        Err(msg) => {
            set_last_exec_error(msg.as_bytes());
            return linux_abi_error(8); // ENOEXEC
        }
    };

    // 6. Update process metadata.
    if let Some(mut proc) = crate::process::get_process(current_pid) {
        {
            let mut r = proc.resources.lock();
            r.vmas.clear();
            r.brk_current = res.max_vaddr;
        }
        proc.mem_frames = (0x100000 / 4096) + res.segment_frames;
        proc.fs_base = if res.dynamic_linker.is_some() { 0 } else { res.tls_base };
        proc.dynamic_linker_aux = res.dynamic_linker;
        // Update process name from argv[0].
        if let Some(first_arg) = argv_strings.first() {
            let base = first_arg.iter().rposition(|&b| b == b'/').map(|p| p + 1).unwrap_or(0);
            let name_bytes = &first_arg[base..];
            let name_len = name_bytes.iter().position(|&b| b == 0).unwrap_or(name_bytes.len()).min(16);
            proc.name[..name_len].copy_from_slice(&name_bytes[..name_len]);
            if name_len < 16 { proc.name[name_len] = 0; }
        }
        crate::process::update_process(current_pid, proc);
    }
    crate::process::clear_pending_process_args(current_pid);

    // 7. Allocate a fresh user stack.
    const USER_STACK_BASE: u64 = 0x2000_0000;
    const USER_STACK_SIZE: usize = 0x10_0000; // 1 MB
    let cr3 = crate::memory::get_cr3();
    if let Err(e) = crate::elf_loader::setup_user_stack(cr3, USER_STACK_BASE, USER_STACK_SIZE) {
        set_last_exec_error(b"execve: stack alloc failed");
        serial::serial_print("[SYSCALL] execve: failed to allocate stack: ");
        serial::serial_print(e);
        serial::serial_print("\n");
        return linux_abi_error(12); // ENOMEM
    }
    crate::process::register_post_exec_vm_as(
        current_pid,
        &res,
        USER_STACK_BASE,
        USER_STACK_SIZE as u64,
    );
    crate::fd::fd_ensure_stdio(current_pid);
    let stack_top = USER_STACK_BASE + USER_STACK_SIZE as u64;

    // Linux vfork: el padre puede salir de `clone` solo cuando el exec está listo
    // para saltar a userspace (no despertar si falla el stack tras `replace`).
    crate::process::vfork_wake_parent_waiting_for_child(current_pid);

    // 8. Set up user stack with argv/envp/auxv and jump.
    let tls_base = if res.dynamic_linker.is_some() { 0 } else { res.tls_base };
    crate::serial::serial_printf(format_args!(
        "[EXECVE] pid={} salto userspace entry={:#x} stack_top={:#x} phdr={:#x} phnum={} tls={:#x}\n",
        current_pid,
        res.entry_point,
        stack_top,
        res.phdr_va,
        res.phnum,
        tls_base
    ));
    if let Some((at_base, at_entry)) = res.dynamic_linker {
        crate::serial::serial_printf(format_args!(
            "[EXECVE] pid={} intérprete AT_BASE={:#x} AT_ENTRY(main)={:#x}\n",
            current_pid, at_base, at_entry
        ));
    }
    unsafe {
        crate::elf_loader::jump_to_userspace_with_argv_envp(
            &res, stack_top, &argv_strings, &envp_strings, tls_base,
        );
    }
}

/// sys_chdir — Linux chdir(path).
fn sys_chdir(path_ptr: u64) -> u64 {
    const MAX_PATH: usize = 1024;
    let path_len = strlen_user_unique(path_ptr, MAX_PATH);
    if path_ptr == 0 || path_len == 0 || path_len >= MAX_PATH as u64 {
        return linux_abi_error(14); // EFAULT
    }
    if !is_user_pointer(path_ptr, path_len + 1) {
        return linux_abi_error(14);
    }
    let path_str = unsafe {
        let s = core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize);
        match core::str::from_utf8(s) {
            Ok(v) => alloc::string::String::from(v),
            Err(_) => return linux_abi_error(22), // EINVAL
        }
    };

    // Resolve against current cwd.
    let pid = match current_process_id() {
        Some(p) => p,
        None => return linux_abi_error(3),
    };
    let new_path = if path_str.starts_with('/') {
        path_str
    } else {
        crate::process::resolve_path_cwd(pid, &path_str)
    };

    // Normalize: remove trailing slash (except root).
    let normalized = normalize_path(&new_path);

    // Verify the directory exists.
    match crate::filesystem::list_dir_children(&normalized) {
        Ok(_) => {
            if crate::process::set_process_cwd(pid, &normalized) {
                serial::serial_printf(format_args!("[SYSCALL] chdir -> {}\n", normalized));
                0
            } else {
                linux_abi_error(36) // ENAMETOOLONG
            }
        }
        Err(_) => linux_abi_error(2), // ENOENT
    }
}

/// Normalize a path: collapse double slashes, remove trailing slash (except root).
fn normalize_path(path: &str) -> alloc::string::String {
    if path.is_empty() { return alloc::string::String::from("/"); }
    let mut result = alloc::string::String::new();
    // Remove duplicate slashes.
    let mut prev_slash = false;
    for ch in path.chars() {
        if ch == '/' {
            if !prev_slash { result.push('/'); }
            prev_slash = true;
        } else {
            result.push(ch);
            prev_slash = false;
        }
    }
    // Remove trailing slash unless it's the root.
    if result.len() > 1 && result.ends_with('/') {
        result.pop();
    }
    if result.is_empty() { result.push('/'); }
    result
}

/// sys_fchdir — Linux fchdir(fd): change cwd to the directory referenced by fd.
fn sys_fchdir(_fd: u64) -> u64 {
    // Not yet implemented; return ENOSYS.
    linux_abi_error(38) // ENOSYS
}

/// sys_pipe2 — Linux pipe2(pipefd[2], flags).
/// Flags: O_CLOEXEC=0x80000, O_NONBLOCK=0x800. We support O_CLOEXEC only.
fn sys_pipe2(pipefd_ptr: u64, _flags: u64) -> u64 {
    // Create pipe (same as sys_pipe for now; cloexec tracking is a future enhancement).
    sys_pipe(pipefd_ptr)
}

/// sys_getdents64 — Linux getdents64(fd, buf, count).
/// Returns total bytes written into buf, or -errno on error.
///
/// struct linux_dirent64 {
///   ino64_t  d_ino;      // 8 bytes
///   off64_t  d_off;      // 8 bytes
///   u16      d_reclen;   // 2 bytes
///   u8       d_type;     // 1 byte
///   char     d_name[];   // null-terminated name
/// };
fn sys_getdents64(fd: u64, buf_ptr: u64, count: u64) -> u64 {
    if buf_ptr == 0 || count < 20 { return linux_abi_error(22); } // EINVAL
    if !is_user_pointer(buf_ptr, count) { return linux_abi_error(14); } // EFAULT

    let pid = match current_process_id() {
        Some(p) => p,
        None => return linux_abi_error(3),
    };

    let fd_entry = match crate::fd::fd_get(pid, fd as usize) {
        Some(e) => e,
        None => return linux_abi_error(9), // EBADF
    };

    // Get directory children via scheme system
    let children = match crate::scheme::getdents(fd_entry.scheme_id, fd_entry.resource_id) {
        Ok(names) => names,
        Err(e) => return linux_abi_error(e as i32),
    };

    // Use the FD's offset field as directory position.
    let offset = crate::fd::fd_get_offset(pid, fd as usize).unwrap_or(0);
    
    let mut written: usize = 0;
    let mut bounce = [0u8; 4096];
    let mut current_idx = offset;

    // Helper to write a dirent64 entry to the bounce buffer
    let mut write_entry = |ino: u64, next_off: u64, d_type: u8, name: &str, buf_off: &mut usize| -> bool {
        let name_len = name.len();
        let rec_size = (8 + 8 + 2 + 1 + name_len + 1 + 7) & !7usize;
        if *buf_off + rec_size > bounce.len() || *buf_off + rec_size > count as usize {
            return false;
        }
        let p = unsafe { bounce.as_mut_ptr().add(*buf_off) };
        unsafe {
            (p as *mut u64).write_unaligned(ino);                 // d_ino
            (p.add(8) as *mut u64).write_unaligned(next_off);     // d_off
            (p.add(16) as *mut u16).write_unaligned(rec_size as u16); // d_reclen
            *(p.add(18)) = d_type;                                // d_type
            core::ptr::copy_nonoverlapping(name.as_bytes().as_ptr(), p.add(19), name_len);
            *(p.add(19 + name_len)) = 0u8;                        // NUL
            for i in (19 + name_len + 1)..rec_size { *(p.add(i)) = 0u8; } // padding
        }
        *buf_off += rec_size;
        true
    };

    // 0: "."
    if current_idx == 0 {
        if write_entry(fd_entry.resource_id as u64, 1, 4, ".", &mut written) {
            current_idx = 1;
        } else {
            return 0;
        }
    }

    // 1: ".."
    if current_idx == 1 {
        // We don't easily have parent_inode here, so we use root (inode 1) or 
        // just reuse the current inode for now. Linux doesn't strictly check the .. inode.
        if write_entry(1, 2, 4, "..", &mut written) {
            current_idx = 2;
        } else {
            // If we can't even fit "..", just return what we have (which is ".")
            goto_finish(pid, fd as usize, current_idx, written, buf_ptr);
            return written as u64;
        }
    }

    // 2+: Real children
    let mut child_idx = (current_idx as usize).saturating_sub(2);
    while child_idx < children.len() {
        let name = &children[child_idx];
        if write_entry(child_idx as u64 + 100, current_idx + 1, 0, name, &mut written) {
            child_idx += 1;
            current_idx += 1;
        } else {
            break;
        }
    }

    fn goto_finish(pid: u32, fd_idx: usize, next_off: u64, written: usize, buf_ptr: u64) {
        if written > 0 {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    [0u8; 4096].as_ptr(), // Placeholder for real bounce buffer access if this were a real function
                    buf_ptr as *mut u8, 
                    written
                );
            }
        }
        crate::fd::fd_set_offset(pid, fd_idx, next_off);
    }

    // Actual finish logic (not using the helper above to avoid closure/lifetime issues)
    if written > 0 {
        unsafe {
            core::ptr::copy_nonoverlapping(bounce.as_ptr(), buf_ptr as *mut u8, written);
        }
    }
    crate::fd::fd_set_offset(pid, fd as usize, current_idx);
    written as u64
}

/// Get the filesystem path associated with an open file descriptor (unused for now).
fn get_path_from_fd(pid: u32, fd: usize) -> Option<alloc::string::String> {
    use crate::fd::fd_get;
    let fd_entry = fd_get(pid, fd)?;
    crate::scheme::get_resource_path(fd_entry.scheme_id, fd_entry.resource_id)
}

/// sys_epoll_create1 — Linux epoll_create1(flags).
fn sys_epoll_create1(_flags: u64) -> u64 {
    // We ignore flags for now (e.g. EPOLL_CLOEXEC).
    match crate::scheme::open("epoll:", 0, 0) {
        Ok((scheme_id, resource_id)) => {
            if let Some(pid) = current_process_id() {
                if let Some(fd) = crate::fd::fd_open(pid, scheme_id, resource_id, 0) {
                    return fd as u64;
                }
            }
        }
        _ => {}
    }
    linux_abi_error(12) // ENOMEM
}

/// sys_epoll_ctl — Linux epoll_ctl(epfd, op, fd, event).
fn sys_epoll_ctl(epfd: u64, op: u64, fd: u64, event_ptr: u64) -> u64 {
    let pid = match current_process_id() { Some(p) => p, None => return u64::MAX };
    let epfd_entry = match crate::fd::fd_get(pid, epfd as usize) {
        Some(e) => e,
        None => return linux_abi_error(9), // EBADF
    };
    
    // We should verify that epfd_entry.scheme_id corresponds to EpollScheme.
    
    let event = if event_ptr != 0 {
        if !is_user_pointer(event_ptr, core::mem::size_of::<crate::epoll::EpollEvent>() as u64) {
            return linux_abi_error(14); // EFAULT
        }
        Some(unsafe { *(event_ptr as *const crate::epoll::EpollEvent) })
    } else {
        None
    };
    
    match crate::epoll::get_epoll_scheme().ctl(epfd_entry.resource_id, op as usize, fd as usize, event) {
        Ok(_) => 0,
        Err(e) => linux_abi_error(e as i32),
    }
}

/// sys_epoll_wait — Linux epoll_wait(epfd, events, maxevents, timeout).
fn sys_epoll_wait(epfd: u64, event_ptr: u64, maxevents: u64, timeout: u64) -> u64 {
    let pid = match current_process_id() { Some(p) => p, None => return u64::MAX };
    let epfd_entry = match crate::fd::fd_get(pid, epfd as usize) {
        Some(e) => e,
        None => return linux_abi_error(9), // EBADF
    };
    
    if event_ptr == 0 || maxevents == 0 { return linux_abi_error(22); }
    if !is_user_pointer(event_ptr, maxevents * core::mem::size_of::<crate::epoll::EpollEvent>() as u64) {
        return linux_abi_error(14);
    }

    let start_ticks = crate::interrupts::ticks();
    
    loop {
        let watched_res = crate::epoll::get_epoll_scheme().get_instance_watched_fds(epfd_entry.resource_id);
        let watched = match watched_res {
            Some(w) => w,
            None => return linux_abi_error(9),
        };

        if watched.is_empty() {
            if timeout == 0 { return 0; }
            // If timeout > 0, sleep a bit and try again (real blocking would use a trigger)
            crate::scheduler::yield_cpu();
            if timeout < 0xFFFFFFFF && (crate::interrupts::ticks() - start_ticks) * 10 >= timeout {
                return 0;
            }
            continue;
        }

        let mut count = 0;
        for (fd, ev) in watched {
            if (count as u64) >= maxevents { break; }
            
            // Check readiness via scheme::poll wrapper
            if let Some(fd_info) = crate::fd::fd_get(pid, fd) {
                if let Ok(ready_bits) = crate::scheme::poll(fd_info.scheme_id, fd_info.resource_id, ev.events as usize) {
                    if ready_bits != 0 {
                        unsafe {
                            let ev_out = (event_ptr as *mut crate::epoll::EpollEvent).add(count);
                            (*ev_out).events = ready_bits as u32;
                            (*ev_out).data = ev.data;
                        }
                        count += 1;
                    }
                }
            }
        }

        if count > 0 || timeout == 0 {
            return count as u64;
        }

        // Blocking: sleep a bit
        crate::scheduler::yield_cpu();
        if timeout < 0xFFFFFFFF && (crate::interrupts::ticks() - start_ticks) * 10 >= timeout {
            return 0;
        }
    }
}

/// sys_eventfd2 — Linux eventfd2(initval, flags).
fn sys_eventfd2(initval: u64, flags: u64) -> u64 {
    let path = alloc::format!("eventfd:{}/{}", initval, flags);
    match crate::scheme::open(&path, 0, 0) {
        Ok((scheme_id, resource_id)) => {
            if let Some(pid) = current_process_id() {
                if let Some(fd) = crate::fd::fd_open(pid, scheme_id, resource_id, 0) {
                    return fd as u64;
                }
            }
        }
        _ => {}
    }
    linux_abi_error(12) // ENOMEM
}

/// sys_socketpair — Linux socketpair(domain, type, protocol, sv[2]).
fn sys_socketpair(domain: u64, type_: u64, protocol: u64, sv_ptr: u64) -> u64 {
    if !is_user_pointer(sv_ptr, 8) { return linux_abi_error(14); } // EFAULT
    
    let scheme_id = match crate::scheme::get_scheme_id("socket") {
        Some(id) => id,
        None => return linux_abi_error(38),
    };
    
    let scheme = match crate::servers::get_socket_scheme() {
        Some(s) => s,
        None => return linux_abi_error(38),
    };
    
    match scheme.socketpair(domain as u32, type_ as u32, protocol as u32) {
        Ok((r1, r2)) => {
            if let Some(pid) = current_process_id() {
                if let Some(fd1) = crate::fd::fd_open(pid, scheme_id, r1, 0) {
                    if let Some(fd2) = crate::fd::fd_open(pid, scheme_id, r2, 0) {
                        unsafe {
                            *(sv_ptr as *mut i32) = fd1 as i32;
                            *((sv_ptr as *mut i32).add(1)) = fd2 as i32;
                        }
                        return 0;
                    }
                }
            }
        }
        Err(e) => return linux_abi_error(e as i32),
    }
    linux_abi_error(12) // ENOMEM
}

/// sys_signalfd4 — Linux signalfd4(fd, mask, sizemask, flags).
fn sys_signalfd4(_fd: u64, _mask_ptr: u64, _mask_size: u64, _flags: u64) -> u64 {
    match crate::scheme::open("signalfd:", 0, 0) {
        Ok((scheme_id, resource_id)) => {
            if let Some(pid) = current_process_id() {
                if let Some(fd) = crate::fd::fd_open(pid, scheme_id, resource_id, 0) {
                    return fd as u64;
                }
            }
        }
        Err(e) => return linux_abi_error(e as i32),
    }
    linux_abi_error(12) // ENOMEM
}

/// sys_timerfd_create — Linux timerfd_create(clockid, flags).
fn sys_timerfd_create(clockid: u64, flags: u64) -> u64 {
    if clockid != 0 && !matches!(clockid, 1 | 4 | 7) {
        return linux_abi_error(22); // EINVAL
    }
    let path = alloc::format!("timerfd:{}/{}", clockid, flags as u32);
    match crate::scheme::open(&path, 0, 0) {
        Ok((scheme_id, resource_id)) => {
            if let Some(pid) = current_process_id() {
                if let Some(fd) = crate::fd::fd_open(pid, scheme_id, resource_id, 0) {
                    return fd as u64;
                }
            }
            let _ = crate::scheme::close(scheme_id, resource_id);
        }
        Err(e) => return linux_abi_error(e as i32),
    }
    linux_abi_error(12) // ENOMEM
}

/// sys_timerfd_settime — Linux timerfd_settime(fd, flags, new_value, old_value).
fn sys_timerfd_settime(fd: u64, flags: u64, new_value: u64, old_value: u64) -> u64 {
    let sz = core::mem::size_of::<crate::timerfd::Itimerspec>();
    if !is_user_pointer(new_value, sz as u64) {
        return linux_abi_error(14); // EFAULT
    }
    if old_value != 0 && !is_user_pointer(old_value, sz as u64) {
        return linux_abi_error(14);
    }
    let Some(scheme_id) = crate::scheme::get_scheme_id("timerfd") else {
        return linux_abi_error(38); // ENOSYS
    };
    let Some(pid) = current_process_id() else {
        return linux_abi_error(3);
    };
    let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) else {
        return linux_abi_error(9); // EBADF
    };
    if fd_entry.scheme_id != scheme_id {
        return linux_abi_error(22); // EINVAL — no es un timerfd
    }
    let new = unsafe { core::ptr::read_unaligned(new_value as *const crate::timerfd::Itimerspec) };
    match crate::timerfd::get_timerfd_scheme().settime(
        fd_entry.resource_id,
        flags as i32,
        &new,
        old_value,
    ) {
        Ok(()) => 0,
        Err(e) => linux_abi_error(e as i32),
    }
}

/// sys_timerfd_gettime — Linux timerfd_gettime(fd, curr_value).
fn sys_timerfd_gettime(fd: u64, curr_ptr: u64) -> u64 {
    let sz = core::mem::size_of::<crate::timerfd::Itimerspec>();
    if !is_user_pointer(curr_ptr, sz as u64) {
        return linux_abi_error(14);
    }
    let Some(scheme_id) = crate::scheme::get_scheme_id("timerfd") else {
        return linux_abi_error(38);
    };
    let Some(pid) = current_process_id() else {
        return linux_abi_error(3);
    };
    let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) else {
        return linux_abi_error(9);
    };
    if fd_entry.scheme_id != scheme_id {
        return linux_abi_error(22);
    }
    match crate::timerfd::get_timerfd_scheme().gettime(fd_entry.resource_id, curr_ptr) {
        Ok(()) => 0,
        Err(e) => linux_abi_error(e as i32),
    }
}

/// sys_inotify_init1 — Linux inotify_init1(flags).
fn sys_inotify_init1(_flags: u64) -> u64 {
    // Another stub using signalfd: scheme
    match crate::scheme::open("signalfd:", 0, 0) {
        Ok((scheme_id, resource_id)) => {
            if let Some(pid) = current_process_id() {
                if let Some(fd) = crate::fd::fd_open(pid, scheme_id, resource_id, 0) {
                    return fd as u64;
                }
            }
        }
        _ => {}
    }
    linux_abi_error(12)
}

/// sys_inotify_add_watch — Linux inotify_add_watch(fd, pathname, mask).
fn sys_inotify_add_watch(_fd: u64, _path: u64, _mask: u64) -> u64 {
    1 // Dummy watch descriptor
}

// ─────────────────────────────────────────────────────────────────────────────
// Linux-compatible stubs added for full x86-64 ABI coverage
// ─────────────────────────────────────────────────────────────────────────────

/// sys_sendto — send a message to a specific destination address.
/// Falls back to sys_write when dest_addr is NULL (connected socket).
fn sys_sendto(fd: u64, buf_ptr: u64, len: u64, _flags: u64, dest_addr: u64, _addrlen: u64) -> u64 {
    if dest_addr == 0 {
        // Connected socket: behave like write.
        sys_write(fd, buf_ptr, len)
    } else {
        // Unconnected socket with destination — not yet implemented; treat as write.
        sys_write(fd, buf_ptr, len)
    }
}

/// sys_recvfrom — receive a message and optionally record the sender address.
/// Falls back to sys_read; source address output is left zero-filled.
fn sys_recvfrom(fd: u64, buf_ptr: u64, len: u64, _flags: u64, src_addr: u64, addrlen_ptr: u64) -> u64 {
    let ret = sys_read(fd, buf_ptr, len);
    // Zero-fill the source address if provided.
    if src_addr != 0 && addrlen_ptr != 0 && is_user_pointer(addrlen_ptr, 4) {
        let addrlen = unsafe { *(addrlen_ptr as *const u32) } as u64;
        if addrlen > 0 && is_user_pointer(src_addr, addrlen) {
            unsafe { core::ptr::write_bytes(src_addr as *mut u8, 0, addrlen as usize) };
        }
    }
    ret
}

/// sys_shutdown — shut down part or all of a full-duplex connection.
fn sys_shutdown(_sockfd: u64, _how: u64) -> u64 {
    0 // Stub: pretend success
}

/// sys_getsockname — get the current address of a socket.
fn sys_getsockname(fd: u64, addr_ptr: u64, addrlen_ptr: u64) -> u64 {
    if addr_ptr == 0 || addrlen_ptr == 0 { return linux_abi_error(14); } // EFAULT
    if !is_user_pointer(addrlen_ptr, 4) { return linux_abi_error(14); }
    let addrlen = unsafe { *(addrlen_ptr as *const u32) } as u64;
    if addrlen > 0 && is_user_pointer(addr_ptr, addrlen) {
        unsafe { core::ptr::write_bytes(addr_ptr as *mut u8, 0, addrlen as usize) };
    }
    0
}

/// sys_getpeername — get the address of the peer connected to a socket.
fn sys_getpeername(fd: u64, addr_ptr: u64, addrlen_ptr: u64) -> u64 {
    sys_getsockname(fd, addr_ptr, addrlen_ptr)
}

/// sys_flock — apply or remove an advisory lock on an open file.
/// Stub: always succeeds (Eclipse uses advisory locking at VFS level).
fn sys_flock(_fd: u64, _operation: u64) -> u64 { 0 }

/// sys_fsync — flush in-core state of a file to storage.
fn sys_fsync(_fd: u64) -> u64 { 0 }

/// sys_fdatasync — flush file data (but not metadata) to storage.
fn sys_fdatasync(_fd: u64) -> u64 { 0 }

/// sys_truncate — truncate a file to a specified length by path.
fn sys_truncate(path_ptr: u64, length: u64) -> u64 {
    const O_WRONLY: u64 = 0x1;
    // Open for write, ftruncate, then close.
    let fd = sys_open(path_ptr, O_WRONLY, 0);
    if fd >= 0xFFFF_FFFF_FFFF_F000 { return fd; } // propagate error
    let ret = sys_ftruncate(fd, length);
    sys_close(fd);
    ret
}

/// sys_rmdir — delete a directory.
fn sys_rmdir(path_ptr: u64) -> u64 {
    // Reuse unlink — the VFS scheme handles empty-directory removal.
    sys_unlink(path_ptr)
}

/// sys_creat — open (or create + truncate) a file, return fd.
/// Equivalent to open(path, O_CREAT|O_WRONLY|O_TRUNC, mode).
fn sys_creat(path_ptr: u64, mode: u64) -> u64 {
    const O_CREAT:  u64 = 0x40;
    const O_WRONLY: u64 = 0x1;
    const O_TRUNC:  u64 = 0x200;
    sys_open(path_ptr, O_CREAT | O_WRONLY | O_TRUNC, mode)
}

/// sys_link — create a hard link (not fully supported; stub returns EPERM).
fn sys_link(_oldpath: u64, _newpath: u64) -> u64 {
    linux_abi_error(1) // EPERM — hard links not implemented
}

/// sys_symlink — create a symbolic link (stub; returns EPERM).
fn sys_symlink(_target: u64, _linkpath: u64) -> u64 {
    linux_abi_error(1) // EPERM — symlinks not implemented
}

/// sys_chmod — change permissions of a file by path (stub; always succeeds).
fn sys_chmod(_path_ptr: u64, _mode: u64) -> u64 { 0 }

/// sys_fchmod — change permissions of an open file (stub; always succeeds).
fn sys_fchmod(_fd: u64, _mode: u64) -> u64 { 0 }

/// sys_chown — change ownership of a file by path (stub; always succeeds).
fn sys_chown(_path_ptr: u64, _uid: u64, _gid: u64) -> u64 { 0 }

/// sys_fchown — change ownership of an open file (stub; always succeeds).
fn sys_fchown(_fd: u64, _uid: u64, _gid: u64) -> u64 { 0 }

/// sys_lchown — change ownership of a symlink (stub; always succeeds).
fn sys_lchown(_path_ptr: u64, _uid: u64, _gid: u64) -> u64 { 0 }

/// sys_umask — set file creation mask. Returns the previous mask (022).
fn sys_umask(_mask: u64) -> u64 { 0o022 }

#[repr(C)]
struct Timeval {
    tv_sec:  i64,
    tv_usec: i64,
}

/// sys_gettimeofday — get current time as struct timeval.
fn sys_gettimeofday(tv_ptr: u64, _tz_ptr: u64) -> u64 {
    if tv_ptr != 0 && is_user_pointer(tv_ptr, core::mem::size_of::<Timeval>() as u64) {
        let ticks = crate::interrupts::ticks();
        let wall_offset = WALL_TIME_OFFSET.load(Ordering::Relaxed);
        let sec  = wall_offset + ticks / 1000;
        let usec = (ticks % 1000) * 1000;
        unsafe {
            let tv = tv_ptr as *mut Timeval;
            (*tv).tv_sec  = sec as i64;
            (*tv).tv_usec = usec as i64;
        }
    }
    0
}

#[repr(C)]
struct RLimit {
    rlim_cur: u64,
    rlim_max: u64,
}

const RLIM_INFINITY: u64 = u64::MAX;
/// Default soft stack limit (8 MiB, matching Linux default).
const DEFAULT_STACK_LIMIT: u64 = 8 * 1024 * 1024;
/// Maximum number of open file descriptors per process.
const MAX_OPEN_FILES: u64 = 1024;

/// sys_getrlimit — get resource limits.
fn sys_getrlimit(resource: u64, rlim_ptr: u64) -> u64 {
    if rlim_ptr == 0 || !is_user_pointer(rlim_ptr, core::mem::size_of::<RLimit>() as u64) {
        return linux_abi_error(14); // EFAULT
    }
    let (soft, hard): (u64, u64) = match resource {
        0  => (RLIM_INFINITY, RLIM_INFINITY),          // RLIMIT_CPU
        1  => (RLIM_INFINITY, RLIM_INFINITY),          // RLIMIT_FSIZE
        2  => (RLIM_INFINITY, RLIM_INFINITY),          // RLIMIT_DATA
        3  => (DEFAULT_STACK_LIMIT, RLIM_INFINITY),    // RLIMIT_STACK
        4  => (RLIM_INFINITY, RLIM_INFINITY),          // RLIMIT_CORE
        5  => (RLIM_INFINITY, RLIM_INFINITY),          // RLIMIT_RSS
        6  => (RLIM_INFINITY, RLIM_INFINITY),          // RLIMIT_NPROC
        7  => (MAX_OPEN_FILES, MAX_OPEN_FILES),        // RLIMIT_NOFILE
        8  => (RLIM_INFINITY, RLIM_INFINITY),          // RLIMIT_MEMLOCK
        9  => (RLIM_INFINITY, RLIM_INFINITY),          // RLIMIT_AS
        10 => (RLIM_INFINITY, RLIM_INFINITY),          // RLIMIT_LOCKS
        11 => (RLIM_INFINITY, RLIM_INFINITY),          // RLIMIT_SIGPENDING
        12 => (RLIM_INFINITY, RLIM_INFINITY),          // RLIMIT_MSGQUEUE
        13 => (0, 0),                                  // RLIMIT_NICE
        14 => (0, 0),                                  // RLIMIT_RTPRIO
        15 => (RLIM_INFINITY, RLIM_INFINITY),          // RLIMIT_RTTIME
        _  => (RLIM_INFINITY, RLIM_INFINITY),
    };
    unsafe {
        let rl = rlim_ptr as *mut RLimit;
        (*rl).rlim_cur = soft;
        (*rl).rlim_max = hard;
    }
    0
}

#[repr(C)]
struct Rusage {
    ru_utime:    Timeval,
    ru_stime:    Timeval,
    ru_maxrss:   i64,
    ru_ixrss:    i64,
    ru_idrss:    i64,
    ru_isrss:    i64,
    ru_minflt:   i64,
    ru_majflt:   i64,
    ru_nswap:    i64,
    ru_inblock:  i64,
    ru_oublock:  i64,
    ru_msgsnd:   i64,
    ru_msgrcv:   i64,
    ru_nsignals: i64,
    ru_nvcsw:    i64,
    ru_nivcsw:   i64,
}

/// sys_getrusage — get resource usage. Returns zeroed struct (stub).
fn sys_getrusage(_who: u64, rusage_ptr: u64) -> u64 {
    if rusage_ptr == 0 || !is_user_pointer(rusage_ptr, core::mem::size_of::<Rusage>() as u64) {
        return linux_abi_error(14); // EFAULT
    }
    unsafe { core::ptr::write_bytes(rusage_ptr as *mut u8, 0, core::mem::size_of::<Rusage>()) };
    0
}

#[repr(C)]
struct SysinfoStruct {
    uptime:    i64,
    loads:     [u64; 3],
    totalram:  u64,
    freeram:   u64,
    sharedram: u64,
    bufferram: u64,
    totalswap: u64,
    freeswap:  u64,
    procs:     u16,
    _pad:      [u8; 6],
    totalhigh: u64,
    freehigh:  u64,
    mem_unit:  u32,
    _pad2:     [u8; 20],
}

/// sys_sysinfo — return system information.
fn sys_sysinfo(info_ptr: u64) -> u64 {
    if info_ptr == 0 || !is_user_pointer(info_ptr, core::mem::size_of::<SysinfoStruct>() as u64) {
        return linux_abi_error(14); // EFAULT
    }
    let ticks = crate::interrupts::ticks();
    let (pool_total, pool_used) = crate::memory::get_memory_stats();
    let boot_bi = crate::boot::get_boot_info();
    let total_ram = if boot_bi.conventional_mem_total_bytes > 0 {
        boot_bi.conventional_mem_total_bytes
    } else {
        pool_total * 4096
    };
    let free_ram = total_ram.saturating_sub(pool_used * 4096);

    unsafe {
        let si = info_ptr as *mut SysinfoStruct;
        core::ptr::write_bytes(si as *mut u8, 0, core::mem::size_of::<SysinfoStruct>());
        (*si).uptime   = (ticks / 1000) as i64;
        (*si).totalram = total_ram;
        (*si).freeram  = free_ram;
        (*si).mem_unit = 1;
    }
    0
}

/// sys_setuid — set user ID (stub; Eclipse runs as root, always succeeds).
fn sys_setuid(_uid: u64) -> u64 { 0 }

/// sys_setgid — set group ID (stub; Eclipse runs as root, always succeeds).
fn sys_setgid(_gid: u64) -> u64 { 0 }

/// sys_setreuid — set real and effective user IDs (stub).
fn sys_setreuid(_ruid: u64, _euid: u64) -> u64 { 0 }

/// sys_setregid — set real and effective group IDs (stub).
fn sys_setregid(_rgid: u64, _egid: u64) -> u64 { 0 }

/// sys_setresuid — set real, effective, and saved user IDs (stub).
fn sys_setresuid(_ruid: u64, _euid: u64, _suid: u64) -> u64 { 0 }

/// sys_setresgid — set real, effective, and saved group IDs (stub).
fn sys_setresgid(_rgid: u64, _egid: u64, _sgid: u64) -> u64 { 0 }
