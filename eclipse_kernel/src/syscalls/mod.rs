//! Sistema de syscalls del microkernel Eclipse
//!
//! Interfaz entre userspace y kernel: despacho central, compatibilidad Linux x86-64
//! y extensiones nativas (≥500).

pub mod fs;
pub mod process;
pub mod memory;
pub mod ipc;
pub mod misc;
pub mod graphics;
pub mod network;
pub mod multiplex;
pub mod signals;
pub mod futex;

use alloc::string::String;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use crate::process::current_process_id;
use crate::interrupts::SyscallContext;

/// Debug/Tracing: Track the last syscall to aid in kernel debugging.
pub(crate) static LAST_SYSCALL_PID: AtomicU32 = AtomicU32::new(0);
pub(crate) static LAST_SYSCALL_NUM: AtomicU64 = AtomicU64::new(0);
pub(crate) static RECV_OK: AtomicU64 = AtomicU64::new(0);
pub(crate) static RECV_EMPTY: AtomicU64 = AtomicU64::new(0);

/// Inicialización del sistema de syscalls
pub fn init() {
    // Por ahora nada que inicializar, pero se deja el hook
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u64)]
pub enum SyscallNumber {
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
    RtSigaction = 13,
    RtSigprocmask = 14,
    RtSigreturn = 15,
    Ioctl = 16,
    Pread64 = 17,
    Pwrite64 = 18,
    Readv = 19,
    Writev = 20,
    Access = 21,
    Pipe = 22,
    Select = 23,
    Yield = 24,
    Mremap = 25,
    Msync = 26,
    Mincore = 27,
    Madvise = 28,
    Shmget = 29,
    Shmat = 30,
    Shmctl = 31,
    Dup = 32,
    Dup2 = 33,
    Pause = 34,
    Nanosleep = 35,
    Getitimer = 36,
    Alarm = 37,
    Setitimer = 38,
    Getpid = 39,
    Sendfile = 40,
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
    Semget = 64,
    Semop = 65,
    Semctl = 66,
    Shmdt = 67,
    Msgget = 68,
    Msgsnd = 69,
    Msgrcv = 70,
    Msgctl = 71,
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
    Times = 100,
    Ptrace = 101,
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
    Getgroups = 115,
    Setgroups = 116,
    Setresuid = 117,
    Getresuid = 118,
    Setresgid = 119,
    Getresgid = 120,
    Getpgid = 121,
    Setfsuid = 122,
    Setfsgid = 123,
    Getsid = 124,
    Capget = 125,
    Capset = 126,
    RtSigpending = 127,
    RtSigtimedwait = 128,
    RtSigqueueinfo = 129,
    RtSigsuspend = 130,
    Sigaltstack = 131,
    Utime = 132,
    Mknod = 133,
    Uselib = 134,
    Personality = 135,
    Ustat = 136,
    Statfs = 137,
    Fstatfs = 138,
    Sysfs = 139,
    Getpriority = 140,
    Setpriority = 141,
    SchedSetparam = 142,
    SchedGetparam = 143,
    SchedSetscheduler = 144,
    SchedGetscheduler = 145,
    SchedGetPriorityMax = 146,
    SchedGetPriorityMin = 147,
    SchedRrGetInterval = 148,
    Mlock = 149,
    Munlock = 150,
    Mlockall = 151,
    Munlockall = 152,
    Vhangup = 153,
    ModifyLdt = 154,
    PivotRoot = 155,
    Sysctl = 156,
    Prctl = 157,
    ArchPrctl = 158,
    Adjtimex = 159,
    Setrlimit = 160,
    Chroot = 161,
    Sync = 162,
    Acct = 163,
    Settimeofday = 164,
    MountLinux = 165,
    Umount2 = 166,
    Swapon = 167,
    Swapoff = 168,
    Reboot = 169,
    Sethostname = 170,
    Setdomainname = 171,
    Iopl = 172,
    Ioperm = 173,
    Gettid = 186,
    Readahead = 187,
    Setxattr = 188,
    Lsetxattr = 189,
    Fsetxattr = 190,
    Getxattr = 191,
    Lgetxattr = 192,
    Fgetxattr = 193,
    Listxattr = 194,
    Llistxattr = 195,
    Flistxattr = 196,
    Removexattr = 197,
    Lremovexattr = 198,
    Fremovexattr = 199,
    Tkill = 200,
    Time = 201,
    Futex = 202,
    SchedSetaffinity = 203,
    SchedGetaffinity = 204,
    IoSetup = 206,
    IoDestroy = 207,
    IoGetevents = 208,
    IoSubmit = 209,
    IoCancel = 210,
    LookupDcookie = 212,
    EpollCreate = 213,
    RemapFilePages = 216,
    Getdents64 = 217,
    SetTidAddress = 218,
    RestartSyscall = 219,
    Semtimedop = 220,
    Fadvise64 = 221,
    TimerCreate = 222,
    TimerSettime = 223,
    TimerGettime = 224,
    TimerGetoverrun = 225,
    TimerDelete = 226,
    ClockSettime = 227,
    ClockGettime = 228,
    ClockGetres = 229,
    ClockNanosleep = 230,
    ExitGroup = 231,
    EpollWait = 232,
    EpollCtl = 233,
    Tgkill = 234,
    Utimes = 235,
    Mbind = 237,
    SetMempolicy = 238,
    GetMempolicy = 239,
    MqOpen = 240,
    MqUnlink = 241,
    MqTimedsend = 242,
    MqTimedreceive = 243,
    MqNotify = 244,
    MqGetsetattr = 245,
    KexecLoad = 246,
    Waitid = 247,
    AddKey = 248,
    RequestKey = 249,
    Keyctl = 250,
    IoprioSet = 251,
    IoprioGet = 252,
    InotifyInit = 253,
    InotifyAddWatch = 254,
    InotifyRmWatch = 255,
    MigratePages = 256,
    Openat = 257,
    Mkdirat = 258,
    Mknodat = 259,
    Fchownat = 260,
    Futimesat = 261,
    Newfstatat = 262,
    Unlinkat = 263,
    Renameat = 264,
    Linkat = 265,
    Symlinkat = 266,
    Readlinkat = 267,
    Fchmodat = 268,
    Faccessat = 269,
    Pselect6 = 270,
    Ppoll = 271,
    Unshare = 272,
    SetRobustList = 273,
    GetRobustList = 274,
    Splice = 275,
    Tee = 276,
    SyncFileRange = 277,
    Vmsplice = 278,
    MovePages = 279,
    Utimensat = 280,
    EpollPwait = 281,
    Signalfd4 = 282,
    TimerfdCreate = 283,
    Eventfd = 284,
    Fallocate = 285,
    TimerfdSettime = 286,
    TimerfdGettime = 287,
    Accept4 = 288,
    Eventfd2 = 290,
    EpollCreate1 = 291,
    Dup3 = 292,
    Pipe2 = 293,
    InotifyInit1 = 294,
    Preadv = 295,
    Pwritev = 296,
    RtTgsigqueueinfo = 297,
    PerfEventOpen = 298,
    Recvmmsg = 299,
    FanotifyInit = 300,
    FanotifyMark = 301,
    Prlimit64 = 302,
    NameToHandleAt = 303,
    OpenByHandleAt = 304,
    ClockAdjtime = 305,
    Syncfs = 306,
    Sendmmsg = 307,
    Setns = 308,
    Getcpu = 309,
    ProcessVmReadv = 310,
    ProcessVmWritev = 311,
    Kcmp = 312,
    FinitModule = 313,
    SchedSetattr = 314,
    SchedGetattr = 315,
    Renameat2 = 316,
    Seccomp = 317,
    Getrandom = 318,
    MemfdCreate = 319,
    KexecFileLoad = 320,
    Bpf = 321,
    Execveat = 322,
    Userfaultfd = 323,
    Membarrier = 324,
    Mlock2 = 325,
    CopyFileRange = 326,
    Preadv2 = 327,
    Pwritev2 = 328,
    PkeyMprotect = 329,
    PkeyAlloc = 330,
    PkeyFree = 331,
    Statx = 332,
    IoPgetevents = 333,
    Rseq = 334,
    PidfdSendSignal = 424,
    IoUringSetup = 425,
    IoUringEnter = 426,
    IoUringRegister = 427,
    OpenTree = 428,
    MoveMount = 429,
    Fsopen = 430,
    Fsconfig = 431,
    Fsmount = 432,
    Fspick = 433,
    PidfdOpen = 434,
    Clone3 = 435,
    CloseRange = 436,
    Openat2 = 437,
    PidfdGetfd = 438,
    Faccessat2 = 439,
    ProcessMadvise = 440,

    // Eclipse-specific syscalls (Range 500+)
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
    SchedSetaffinityEclipse = 533,
    RegisterLogHud = 534,
    SetTime = 535,
    SpawnWithStdio = 536,
    ThreadCreate = 537,
    WaitPid = 538,
    Readdir = 539,
    SetChildArgs = 542,
    GetProcessArgs = 543,
    SpawnWithStdioPath = 544,
    Strace = 545,
    Exec = 546,

    VirglCtxCreate = 570,
    VirglCtxDestroy = 571,
    VirglCtxAttachResource = 572,
    VirglCtxDetachResource = 573,
    VirglAllocBacking = 574,
    VirglResourceAttachBacking = 575,
    VirglResourceSubmit3d = 576,

    ReceiveFast = 600,
}


// Re-export signal infrastructure
pub use signals::{
    push_rt_signal_frame, 
    deliver_pending_signals_for_current, 
    deliver_signal_from_exception
};

// Signal related types for ABI
#[repr(C)]
pub struct SigInfo {
    pub si_signo: i32,
    pub si_errno: i32,
    pub si_code:  i32,
    pub _rest:    [u8; 116],
}

#[repr(C)]
pub struct StackT {
    pub ss_sp:    u64,
    pub ss_flags: i32,
    pub _pad:     u32,
    pub ss_size:  u64,
}

#[repr(C)]
pub struct SigContext {
    pub r8: u64, pub r9: u64, pub r10: u64, pub r11: u64,
    pub r12: u64, pub r13: u64, pub r14: u64, pub r15: u64,
    pub rdi: u64, pub rsi: u64, pub rbp: u64, pub rbx: u64,
    pub rdx: u64, pub rax: u64, pub rcx: u64, pub rsp: u64,
    pub rip: u64, pub eflags: u64,
    pub cs: u16, pub gs: u16, pub fs: u16, pub ss: u16,
    pub err: u64, pub trapno: u64, pub oldmask: u64, pub cr2: u64,
    pub fpstate: u64,
    pub _reserved1: [u64; 8],
}

#[repr(C)]
pub struct UContext {
    pub uc_flags:    u64,
    pub uc_link:     u64,
    pub uc_stack:    StackT,
    pub uc_mcontext: SigContext,
    pub uc_sigmask:  u64,
}

#[repr(C)]
pub struct RtSigframe {
    pub pretcode: u64,
    pub uc:       UContext,
    pub info:     SigInfo,
    pub _pad:     u64,
    pub fpstate:  [u8; 512],
}

/// Statistics for sys_get_system_stats
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemStats {
    pub uptime_ms: u64,
    pub idle_ms: u64,
    pub total_memory_kb: u64,
    pub free_memory_kb: u64,
    pub cpu_load: [u32; 16],
    pub cpu_temp: [u32; 16],
    pub gpu_load: [u32; 4],
    pub gpu_temp: [u32; 4],
    pub gpu_vram_total_kb: u64,
    pub gpu_vram_used_kb: u64,
    pub anomaly_count: u32,
    pub heap_fragmentation: u32,
    pub wall_time_offset: u64,
}

/// Process info for sys_get_process_list
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub state: u32,
    pub cpu_usage: u32,
    pub mem_usage_kb: u64,
    pub name: [u8; 32],
    pub thread_count: u32,
    pub priority: u32,
}

/// Entrada principal de syscalls (desde el stub en `interrupts`).
pub extern "C" fn syscall_handler(
    num: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
    arg6: u64,
    context: &mut SyscallContext,
) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    LAST_SYSCALL_PID.store(pid, Ordering::Relaxed);
    LAST_SYSCALL_NUM.store(num, Ordering::Relaxed);

    if pid != 0 && !crate::ai_core::audit_syscall(pid, num) {
        return 0xFFFF_FFFF_FFFF_FFFF;
    }

    let (strace, p_name): (bool, String) = if let Some(p) = crate::process::get_process(pid) {
        let proc = p.proc.lock();
        let end = proc.name.iter().position(|&b| b == 0).unwrap_or(proc.name.len());
        let n = core::str::from_utf8(&proc.name[..end])
            .unwrap_or("?")
            .trim();
        let name = if n.is_empty() { String::from("unknown") } else { String::from(n) };
        (proc.syscall_trace, name)
    } else {
        (false, String::new())
    };
    if strace {
        crate::serial::serial_printf(format_args!(
            "[strace] pid={} ({}) call {}({:#x}, {:#x}, {:#x}, {:#x}, {:#x}, {:#x})\n",
            pid, p_name, num, arg1, arg2, arg3, arg4, arg5, arg6
        ));
    }

    let result = match num {
        // --- Filesystem (Linux) ---
        0   => fs::sys_read(arg1, arg2, arg3),
        1   => fs::sys_write(arg1, arg2, arg3),
        2   => fs::sys_open(arg1, arg2, arg3),
        3   => fs::sys_close(arg1),
        4   => fs::sys_stat(arg1, arg2),
        5   => fs::sys_fstat(arg1, arg2),
        6   => fs::sys_fstatat(0xFFFFFFFFFFFFFF9C, arg1, arg2, 0x100), // lstat
        7   => multiplex::sys_poll(arg1, arg2, arg3),
        8   => fs::sys_lseek(arg1, arg2 as i64, arg3),
        9   => memory::sys_mmap(arg1, arg2, arg3, arg4, arg5, arg6),
        10  => memory::sys_mprotect(arg1, arg2, arg3),
        11  => memory::sys_munmap(arg1, arg2),
        12  => memory::sys_brk(arg1),
        13  => signals::sys_rt_sigaction(arg1, arg2, arg3, arg4),
        14  => signals::sys_rt_sigprocmask(arg1, arg2, arg3, arg4),
        15  => signals::sys_rt_sigreturn(context),
        16  => fs::sys_ioctl(arg1, arg2, arg3),
        17  => fs::sys_pread64(arg1, arg2, arg3, arg4),
        18  => fs::sys_pwrite64(arg1, arg2, arg3, arg4),
        19  => fs::sys_readv(arg1, arg2, arg3),
        20  => fs::sys_writev(arg1, arg2, arg3),
        21  => fs::sys_faccessat(0xFFFFFFFFFFFFFF9C, arg1, arg2, 0), // access
        22  => fs::sys_pipe(arg1),
        23  => multiplex::sys_select(arg1, arg2, arg3, arg4, arg5),
        24  => misc::sys_yield(),
        25  => memory::sys_mremap(arg1, arg2, arg3, arg4, arg5),
        28  => memory::sys_madvise(arg1, arg2, arg3),
        32  => fs::sys_dup(arg1),
        33  => fs::sys_dup2(arg1, arg2),
        34  => multiplex::sys_pause(),
        35  => misc::sys_nanosleep(arg1, arg2),
        39  => process::sys_getpid(),
        41  => network::sys_socket(arg1, arg2, arg3),
        42  => network::sys_connect(arg1, arg2, arg3),
        43  => network::sys_accept(arg1, arg2, arg3),
        44  => network::sys_sendto(arg1, arg2, arg3, arg4, arg5, arg6),
        45  => network::sys_recvfrom(arg1, arg2, arg3, arg4, arg5, arg6),
        46  => network::sys_sendmsg(arg1, arg2, arg3),
        47  => network::sys_recvmsg(arg1, arg2, arg3),
        48  => network::sys_shutdown(arg1, arg2),
        49  => network::sys_bind(arg1, arg2, arg3),
        50  => network::sys_listen(arg1, arg2),
        51  => network::sys_getsockname(arg1, arg2, arg3),
        52  => network::sys_getpeername(arg1, arg2, arg3),
        53  => network::sys_socketpair(arg1, arg2, arg3, arg4),
        54  => network::sys_setsockopt(arg1, arg2, arg3, arg4, arg5),
        55  => network::sys_getsockopt(arg1, arg2, arg3, arg4, arg5),
        56  => process::sys_clone(arg1, arg2, arg3, arg4, arg5, context),
        57  => process::sys_fork(context),
        58  => process::sys_fork(context), // vfork
        59  => process::sys_execve(arg1, arg2, arg3),
        60  => process::sys_exit(arg1),
        61  => process::sys_wait4_linux(arg1, arg2, arg3, arg4),
        62  => signals::sys_kill(arg1, arg2),
        63  => misc::sys_uname(arg1),
        72  => fs::sys_fcntl(arg1, arg2, arg3),
        73  => fs::sys_flock(arg1, arg2),
        74  => fs::sys_fsync(arg1),
        75  => fs::sys_fdatasync(arg1),
        76  => fs::sys_truncate(arg1, arg2),
        77  => fs::sys_ftruncate(arg1, arg2),
        78  => fs::sys_getdents64(arg1, arg2, arg3),
        79  => fs::sys_getcwd(arg1, arg2),
        80  => fs::sys_chdir(arg1),
        81  => fs::sys_fchdir(arg1),
        82  => fs::sys_rename(arg1, arg2),
        83  => fs::sys_mkdir(arg1, arg2),
        84  => fs::sys_rmdir(arg1),
        85  => fs::sys_creat(arg1, arg2),
        86  => fs::sys_link(arg1, arg2),
        87  => fs::sys_unlink(arg1),
        88  => fs::sys_symlink(arg1, arg2),
        89  => fs::sys_readlink(arg1, arg2, arg3),
        90  => fs::sys_chmod(arg1, arg2),
        91  => fs::sys_fchmod(arg1, arg2),
        92  => fs::sys_chown(arg1, arg2, arg3),
        93  => fs::sys_fchown(arg1, arg2, arg3),
        94  => fs::sys_lchown(arg1, arg2, arg3),
        95  => fs::sys_umask(arg1),
        96  => misc::sys_gettimeofday(arg1, arg2),
        97  => misc::sys_getrlimit(arg1, arg2),
        98  => misc::sys_getrusage(arg1, arg2),
        99  => misc::sys_sysinfo(arg1),
        100 => linux_abi_error(38), // times — no implementado
        101 => process::sys_ptrace(arg1, arg2, arg3, arg4),
        102 => process::sys_getuid(),
        103 => linux_abi_error(38), // syslog — no implementado
        104 => process::sys_getgid(),
        105 => process::sys_setuid(arg1),
        106 => process::sys_setgid(arg1),
        107 => process::sys_geteuid(),
        108 => process::sys_getegid(),
        109 => process::sys_setpgid(arg1, arg2),
        110 => process::sys_getppid(),
        111 => process::sys_getpgrp(),
        112 => process::sys_setsid(),
        113 => process::sys_setreuid(arg1, arg2),
        114 => process::sys_setregid(arg1, arg2),
        117 => process::sys_setresuid(arg1, arg2, arg3),
        118 => process::sys_getresuid(arg1, arg2, arg3),
        119 => process::sys_setresgid(arg1, arg2, arg3),
        120 => process::sys_getresgid(arg1, arg2, arg3),
        121 => process::sys_getpgid(arg1),
        127 => signals::sys_rt_sigpending(arg1, arg2),
        131 => signals::sys_sigaltstack(arg1, arg2),
        157 => process::sys_prctl(arg1, arg2, arg3, arg4, arg5),
        158 => process::sys_arch_prctl(arg1, arg2),
        162 => fs::sys_sync(),
        170 => misc::sys_sethostname(arg1, arg2),
        186 => process::sys_gettid(),
        200 => signals::sys_tkill(arg1, arg2),
        202 => futex::sys_futex(arg1, arg2, arg3, arg4, arg5, arg6 as u32),
        204 => process::sys_sched_getaffinity(arg1, arg2, arg3),
        217 => fs::sys_getdents64(arg1, arg2, arg3),
        218 => process::sys_set_tid_address(arg1),
        228 => misc::sys_clock_gettime(arg1, arg2),
        230 => misc::sys_nanosleep(arg3, arg4),
        231 => process::sys_exit(arg1),
        232 => multiplex::sys_epoll_wait(arg1, arg2, arg3, arg4),
        233 => multiplex::sys_epoll_ctl(arg1, arg2, arg3, arg4),
        247 => process::sys_waitid(arg1, arg2, arg3, arg4, arg5),
        254 => multiplex::sys_inotify_add_watch(arg1, arg2, arg3),
        257 => fs::sys_openat(arg1, arg2, arg3, arg4),
        258 => fs::sys_mkdirat(arg1, arg2, arg3),
        262 => fs::sys_fstatat(arg1, arg2, arg3, arg4),
        269 => fs::sys_faccessat(arg1, arg2, arg3, arg4),
        270 => multiplex::sys_pselect6(arg1, arg2, arg3, arg4, arg5, arg6),
        271 => multiplex::sys_ppoll(arg1, arg2, arg3, arg4, arg5),
        282 => signals::sys_signalfd4(arg1, arg2, arg3, arg4),
        283 => multiplex::sys_timerfd_create(arg1, arg2),
        286 => multiplex::sys_timerfd_settime(arg1, arg2, arg3, arg4),
        287 => multiplex::sys_timerfd_gettime(arg1, arg2),
        290 => multiplex::sys_eventfd2(arg1, arg2),
        291 => multiplex::sys_epoll_create1(arg1),
        292 => fs::sys_dup3(arg1, arg2, arg3),
        293 => fs::sys_pipe2(arg1, arg2),
        294 => multiplex::sys_inotify_init1(arg1),
        295 => fs::sys_preadv(arg1, arg2, arg3, arg4),
        296 => fs::sys_pwritev(arg1, arg2, arg3, arg4),
        302 => misc::sys_prlimit64(arg1, arg2, arg3, arg4),
        314 => process::sys_sched_set_deadline(arg1 as u32, arg2, arg3, arg4),
        316 => fs::sys_renameat2(arg1, arg2, arg3, arg4, arg5),
        318 => misc::sys_getrandom(arg1, arg2, arg3),
        319 => fs::sys_memfd_create(arg1, arg2),
        324 => misc::sys_membarrier(arg1, arg2, arg3),
        439 => fs::sys_faccessat(arg1, arg2, arg3, arg4),

        // --- Eclipse Extensions (500+) ---
        500 => ipc::sys_send(arg1, arg2, arg3, arg4),
        501 => ipc::sys_receive(arg1, arg2, arg3),
        502 => misc::sys_get_service_binary(arg1, arg2, arg3),
        503 => graphics::sys_get_framebuffer_info(arg1),
        504 => memory::sys_map_framebuffer(arg1, arg2),
        505 => misc::sys_pci_enum_devices(arg1, arg2, arg3),
        506 => misc::sys_pci_read_config(arg1, arg2, arg3),
        507 => misc::sys_pci_write_config(arg1, arg2, arg3, arg4),
        508 => misc::sys_register_device(arg1, arg2, arg3),
        509 => fs::sys_fmap(arg1, arg2, arg3),
        510 => fs::sys_mount(arg1, arg2),
        511 => process::sys_spawn(arg1, arg2, arg3),
        512 => process::sys_get_last_exec_error(arg1, arg2),
        513 => misc::sys_read_key(),
        514 => misc::sys_read_mouse_packet(),
        515 => graphics::sys_get_gpu_display_info(arg1),
        516 => graphics::sys_set_cursor_position(arg1, arg2),
        517 => graphics::sys_gpu_alloc_display_buffer(arg1, arg2, arg3),
        518 => graphics::sys_gpu_present(arg1, arg2, arg3, arg4, arg5),
        519 => misc::sys_get_logs(arg1, arg2),
        520 => misc::sys_get_storage_device_count(),
        521 => misc::sys_get_system_stats(arg1),
        522 => process::sys_get_process_list(arg1, arg2),
        523 => process::sys_set_process_name(arg1, arg2),
        524 => process::sys_spawn_service(arg1, arg2, arg3),
        525 => graphics::sys_gpu_command(arg1, arg2, arg3),
        526 => misc::sys_stop_progress(),
        527 => graphics::sys_gpu_get_backend(),
        528 => graphics::sys_drm_page_flip(arg1, arg2, arg3, arg4, arg5),
        529 => graphics::sys_drm_get_caps(arg1, arg2),
        530 => graphics::sys_drm_alloc_buffer(arg1, arg2, arg3, arg4),
        531 => graphics::sys_drm_create_fb(arg1, arg2, arg3, arg4, arg5, arg6),
        532 => graphics::sys_drm_map_handle(arg1, arg2),
        533 => process::sys_sched_setaffinity(arg1, arg2),
        534 => misc::sys_register_log_hud(arg1),
        535 => misc::sys_set_time(arg1),
        536 => process::sys_spawn_with_stdio(arg1, arg2, arg3, arg4, arg5, arg6),
        537 => process::sys_thread_create(arg1, arg2, arg3, context),
        538 => process::sys_wait_pid(arg1, arg2, arg3),
        539 => fs::sys_readdir(arg1, arg2, arg3),
        542 => process::sys_spawn_with_stdio_args(arg1, arg2, arg3, arg4, arg5, arg6, context),
        543 => process::sys_get_process_args(arg1, arg2),
        544 => process::sys_spawn_with_stdio_path(arg1, arg2, arg3, arg4, arg5, arg6),
        545 => process::sys_strace(arg1, arg2),
        546 => process::sys_exec(arg1, arg2),

        570 => graphics::sys_virgl_ctx_create(arg1, arg2, arg3),
        571 => graphics::sys_virgl_ctx_destroy(arg1),
        572 => graphics::sys_virgl_ctx_attach_resource(arg1, arg2),
        573 => graphics::sys_virgl_ctx_detach_resource(arg1, arg2),
        574 => graphics::sys_virgl_alloc_backing(arg1, arg2),
        575 => graphics::sys_virgl_resource_attach_backing(arg1, arg2, arg3),
        576 => graphics::sys_virgl_submit_3d(arg1, arg2, arg3),

        600 => ipc::sys_receive_fast(context),

        _ => {
            let cpu = crate::process::get_cpu_id();
            if num < 500 {
                crate::serial::serial_printf(format_args!(
                    "[SYSCALL] Unknown syscall: {} (Linux range) from pid {} on CPU {}\n",
                    num, pid, cpu
                ));
                linux_abi_error(38)
            } else {
                crate::serial::serial_printf(format_args!(
                    "[SYSCALL] Unknown syscall: {} (Eclipse range) from pid {} on CPU {}\n",
                    num, pid, cpu
                ));
                u64::MAX
            }
        }
    };

    context.rax = result;

    if strace {
        crate::serial::serial_printf(format_args!("[strace] pid={} returns {:#x}\n", pid, result));
    }

    // No entregar señales en la vuelta de exit / exit_group (el proceso termina).
    if num != 60 && num != 231 {
        signals::deliver_pending_signals_for_current(context);
    }

    result
}

/// Convierte `errno` Linux (1..4095) a valor de retorno en RAX (`-errno` como unsigned).
#[inline]
pub fn linux_abi_error(errno: i32) -> u64 {
    if errno <= 0 || errno >= 4096 {
        u64::MAX
    } else {
        (errno.wrapping_neg()) as u64
    }
}

/// Duerme al proceso actual ~`ms` milisegundos (delegado en el scheduler).
pub fn process_sleep_ms(ms: u64) {
    crate::scheduler::sleep(ms);
}

/// Offset de tiempo de pared: `Unix_sec ≈ WALL_TIME_OFFSET + interrupts::ticks()/1000`.
pub static WALL_TIME_OFFSET: AtomicU64 = AtomicU64::new(0);

pub fn linux_makedev(major: u32, minor: u32) -> u64 {
    ((minor as u64 & 0xff) << 0) | ((major as u64 & 0xfff) << 8) | ((minor as u64 & !0xff) << 12) | ((major as u64 & !0xfff) << 32)
}

pub use futex::futex_wake_all_atomic;

pub fn is_user_pointer(ptr: u64, len: u64) -> bool {
    if len == 0 { return true; }
    // Reject NULL and the first page to catch null pointer dereferences
    if ptr < 4096 { return false; }
    if ptr >= 0x0000_8000_0000_0000 { return false; }
    if ptr.checked_add(len).map_or(true, |end| end >= 0x0000_8000_0000_0000) { return false; }
    true
}

pub fn copy_to_user(user_ptr: u64, src: &[u8]) -> bool {
    if !is_user_pointer(user_ptr, src.len() as u64) { return false; }
    // Recuperación ante #PF: si userspace apunta a una página no mapeada,
    // devolvemos `false` en vez de matar al kernel.
    if unsafe { !crate::interrupts::set_recovery_point() } {
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), user_ptr as *mut u8, src.len());
        }
        unsafe { crate::interrupts::clear_recovery_point() };
        true
    } else {
        // set_recovery_point devolvió "estoy recuperando de un fault"
        unsafe { crate::interrupts::clear_recovery_point() };
        false
    }
}

pub fn copy_from_user(user_ptr: u64, dest: &mut [u8]) -> bool {
    if !is_user_pointer(user_ptr, dest.len() as u64) { return false; }
    // Recuperación ante #PF: evita panics al leer memoria de userspace inválida.
    if unsafe { !crate::interrupts::set_recovery_point() } {
        unsafe {
            core::ptr::copy_nonoverlapping(user_ptr as *const u8, dest.as_mut_ptr(), dest.len());
        }
        unsafe { crate::interrupts::clear_recovery_point() };
        true
    } else {
        unsafe { crate::interrupts::clear_recovery_point() };
        false
    }
}

pub fn strlen_user_unique(user_ptr: u64, max_len: usize) -> usize {
    for i in 0..max_len {
        if !is_user_pointer(user_ptr + i as u64, 1) { return i; }
        let c = unsafe { *( (user_ptr + i as u64) as *const u8 ) };
        if c == 0 { return i; }
    }
    max_len
}

pub fn set_fs_base(addr: u64) {
    unsafe {
        crate::cpu::wrmsr(0xC0000100, addr); // FS_BASE
    }
}
