//! Números de syscall para Eclipse OS (ABI **x86-64**).
//!
//! - **Compatibilidad Linux**: los syscalls “POSIX”/comunes usan los mismos números que
//!   **Linux x86-64** (`read`=0, `write`=1, `open`=2, …, `getrandom`=318). El kernel
//!   los despacha en `eclipse_kernel::syscalls::syscall_handler`.
//! - **Extensiones Eclipse**: reservado el rango **≥ 500** (IPC, framebuffer, spawn de
//!   servicios, DRM, etc.). No coinciden con Linux: ahí siempre usar estas constantes.
//!
//! Única fuente de verdad: este módulo + tabla `match` en `eclipse_kernel/src/syscalls.rs`.
pub const SYS_READ: usize = 0;
pub const SYS_WRITE: usize = 1;
pub const SYS_OPEN: usize = 2;
pub const SYS_CLOSE: usize = 3;
pub const SYS_STAT: usize = 4;
pub const SYS_FSTAT: usize = 5;
pub const SYS_LSEEK: usize = 8;
pub const SYS_MMAP: usize = 9;
pub const SYS_MUNMAP: usize = 11;
pub const SYS_BRK: usize = 12;
pub const SYS_SIGACTION: usize = 13;
pub const SYS_IOCTL: usize = 16;
pub const SYS_YIELD: usize = 24;
pub const SYS_NANOSLEEP: usize = 35;
pub const SYS_GETPID: usize = 39;
pub const SYS_SOCKET: usize = 41;
pub const SYS_CONNECT: usize = 42;
pub const SYS_ACCEPT: usize = 43;
pub const SYS_BIND: usize = 49;
pub const SYS_LISTEN: usize = 50;
pub const SYS_SETSOCKOPT: usize = 54;
pub const SYS_GETSOCKOPT: usize = 55;
pub const SYS_CLONE: usize = 56;
pub const SYS_FORK: usize = 57;
pub const SYS_EXEC: usize = 59;
pub const SYS_EXIT: usize = 60;
pub const SYS_WAIT: usize = 61;
pub const SYS_KILL: usize = 62;
pub const SYS_FTRUNCATE: usize = 77;
/// rename(oldpath, newpath) — mismos argumentos que Linux x86-64 (punteros a C-strings).
pub const SYS_RENAME: usize = 82;
pub const SYS_MKDIR: usize = 83;
pub const SYS_UNLINK: usize = 87;
pub const SYS_GETPPID: usize = 110;
pub const SYS_ARCH_PRCTL: usize = 158;
pub const SYS_GETTID: usize = 186;
pub const SYS_FUTEX: usize = 202;
pub const SYS_FSTATAT: usize = 262;
pub const SYS_GETRANDOM: usize = 318;

// Eclipse-specific syscalls (Range 500+)
pub const SYS_SEND: usize = 500;
pub const SYS_RECEIVE: usize = 501;
pub const SYS_GET_SERVICE_BINARY: usize = 502;
pub const SYS_GET_FRAMEBUFFER_INFO: usize = 503;
pub const SYS_MAP_FRAMEBUFFER: usize = 504;
pub const SYS_PCI_ENUM_DEVICES: usize = 505;
pub const SYS_PCI_READ_CONFIG: usize = 506;
pub const SYS_PCI_WRITE_CONFIG: usize = 507;
pub const SYS_REGISTER_DEVICE: usize = 508;
pub const SYS_FMAP: usize = 509;
pub const SYS_MOUNT: usize = 510;
pub const SYS_SPAWN: usize = 511;
pub const SYS_GET_LAST_EXEC_ERROR: usize = 512;
pub const SYS_READ_KEY: usize = 513;
pub const SYS_READ_MOUSE_PACKET: usize = 514;
pub const SYS_GET_GPU_DISPLAY_INFO: usize = 515;
pub const SYS_SET_CURSOR_POSITION: usize = 516;
pub const SYS_GPU_ALLOC_DISPLAY_BUFFER: usize = 517;
pub const SYS_GPU_PRESENT: usize = 518;
pub const SYS_GET_LOGS: usize = 519;
pub const SYS_GET_STORAGE_DEVICE_COUNT: usize = 520;
pub const SYS_GET_SYSTEM_STATS: usize = 521;
pub const SYS_GET_PROCESS_LIST: usize = 522;
pub const SYS_SET_PROCESS_NAME: usize = 523;
pub const SYS_SPAWN_SERVICE: usize = 524;
pub const SYS_GPU_COMMAND: usize = 525;
pub const SYS_STOP_PROGRESS: usize = 526;
pub const SYS_GET_GPU_BACKEND: usize = 527;
pub const SYS_DRM_PAGE_FLIP: usize = 528;
pub const SYS_DRM_GET_CAPS: usize = 529;
pub const SYS_DRM_ALLOC_BUFFER: usize = 530;
pub const SYS_DRM_CREATE_FB: usize = 531;
pub const SYS_DRM_MAP_HANDLE: usize = 532;
pub const SYS_SCHED_SETAFFINITY: usize = 533;
pub const SYS_REGISTER_LOG_HUD: usize = 534;
pub const SYS_SET_TIME: usize = 535;
pub const SYS_SPAWN_WITH_STDIO: usize = 536;
/// Crear hilo de usuario: (stack_top alineado, entry rip, arg en rdi). Eclipse específico.
pub const SYS_THREAD_CREATE: usize = 537;
/// Esperar hijo: (status_ptr, wait_pid) — wait_pid == 0 equivale a cualquier hijo.
pub const SYS_WAIT_PID: usize = 538;

pub const SYS_RECEIVE_FAST: usize = 600;
