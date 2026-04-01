//! Sistema de syscalls del microkernel
//! 
//! Implementa la interfaz entre userspace y kernel

use alloc::format;
use alloc::string::String;

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use crate::process::{ProcessId, exit_process, current_process_id};
use crate::scheduler::yield_cpu;
use crate::ipc::{MessageType, send_message, receive_message, pop_small_message_24};
use crate::serial;
use spin::Mutex;
use alloc::sync::Arc;

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
    Read = 0,
    Write = 1,
    Open = 2,
    Close = 3,
    Stat = 4,
    Fstat = 5,
    Lseek = 8,
    Mmap = 9,
    Munmap = 11,
    Brk = 12,
    SigAction = 13,
    Ioctl = 16,
    Yield = 24,
    Nanosleep = 35,
    GetPid = 39,
    Socket = 41,
    Connect = 42,
    Accept = 43,
    Bind = 49,
    Listen = 50,
    Setsockopt = 54,
    Getsockopt = 55,
    Clone = 56,
    Fork = 57,
    Exec = 59,
    Exit = 60,
    Wait = 61,
    Kill = 62,
    Ftruncate = 77,
    Rename = 82,
    Mkdir = 83,
    Unlink = 87,
    Getppid = 110,
    ArchPrctl = 158,
    Gettid = 186,
    Futex = 202,
    Fstatat = 262,
    GetRandom = 318,

    // Eclipse-specific (500+)
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
    ReceiveFast = 600,
}



/// lseek whence values (POSIX standard)
pub const SEEK_SET: u64 = 0; // Absolute position
pub const SEEK_CUR: u64 = 1; // Relative to current position  
pub const SEEK_END: u64 = 2; // Relative to end of file

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

    // AI-Core: Audit syscall for anomalies (DoS detection)
    if pid != 0 {
        if !crate::ai_core::audit_syscall(pid as u32, syscall_num) {
            // Syscall blocked by AI policy
            return 0xFFFF_FFFF_FFFF_FFFF; // -1 Error
        }
    }


    // Read user context directly from the struct passed by assembly
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
    
    let mut stats = SYSCALL_STATS.lock();
    stats.total_calls += 1;
    drop(stats);
 
    // Linux ABI: translate only if process is marked as Linux (e.g. Xfbdev)
    let mut is_linux = crate::process::current_process_id()
        .and_then(|pid| crate::process::get_process(pid))
        .map(|p| p.is_linux)
        .unwrap_or(false);

    // Auto-detection: if it calls a high Linux-specific syscall, mark it as Linux permanently
    if !is_linux && (syscall_num == 158 || syscall_num == 231 || syscall_num == 41 || syscall_num == 202) {
         if let Some(pid) = crate::process::current_process_id() {
             if let Some(mut proc) = crate::process::get_process(pid) {
                 proc.is_linux = true;
                 crate::process::update_process(pid, proc);
                 is_linux = true;
             }
         }
    }

    let (syscall_num, arg1, arg2, arg3, arg4, arg5, arg6) = (syscall_num, arg1, arg2, arg3, arg4, arg5, arg6);

    let ret = match syscall_num {
        0 => sys_read(arg1, arg2, arg3),
        1 => sys_write(arg1, arg2, arg3),
        2 => sys_open(arg1, arg2, arg3),
        3 => sys_close(arg1),
        5 => sys_fstat(arg1, arg2),
        8 => sys_lseek(arg1, arg2 as i64, arg3 as usize),
        9 => sys_mmap(arg1, arg2, arg3, arg4, arg5, arg6),
        11 => sys_munmap(arg1, arg2),
        12 => sys_brk(arg1),
        13 => u64::MAX, // sys_sigaction not yet implemented
        16 => sys_ioctl(arg1, arg2, arg3),
        24 => sys_yield(),
        35 => sys_nanosleep(arg1),
        39 => sys_getpid(),
        41 => sys_socket(arg1, arg2, arg3),
        42 => sys_connect(arg1, arg2, arg3),
        43 => sys_accept(arg1, arg2, arg3),
        49 => sys_bind(arg1, arg2, arg3),
        50 => sys_listen(arg1, arg2),
        54 => sys_setsockopt(arg1, arg2, arg3, arg4, arg5),
        55 => sys_getsockopt(arg1, arg2, arg3, arg4, arg5),
        56 => sys_clone(arg1, arg2, arg3),
        57 => sys_fork(&process_context),
        59 => sys_exec(arg1, arg2),
        60 => sys_exit(arg1),
        231 => sys_exit(arg1),  // Linux exit_group
        61 => sys_wait(arg1),
        62 => sys_kill(arg1, arg2),
        77 => sys_ftruncate(arg1, arg2),
        82 => sys_rename(arg1, arg2),
        83 => sys_mkdir(arg1, arg2),
        87 => sys_unlink(arg1),
        110 => sys_getppid(),
        158 => sys_arch_prctl(arg1, arg2),
        186 => sys_gettid(),
        202 => sys_futex(arg1, arg2, arg3, arg4),
        262 => sys_fstatat(arg1, arg2, arg3, arg4),
        318 => sys_getrandom(arg1, arg2, arg3),

        // Eclipse-specific (500+)
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
        250 => sys_gpu_present(arg1, arg2, arg3, arg4, arg5), // Legacy alias
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
        537 => sys_thread_create(arg1, arg2, arg3),
        538 => sys_wait_pid(arg1, arg2),
        600 => sys_receive_fast(context),
        _ => {
            serial::serial_printf(format_args!(
                "[SYSCALL] Unknown syscall: {}{}{} from process {} on CPU {}\n",
                syscall_num,
                if is_linux { " (Linux translated)" } else { "" },
                "", // Padding if needed
                crate::process::current_process_id().unwrap_or(0),
                crate::process::get_cpu_id()
            ));
            u64::MAX
        }
    };
    
    // Linux Compatibility: Map results to signed if it's a Linux process
    // glibc expects -1 for failure and sets errno based on the positive error code.
    // Our kernel currently returns u64::MAX for error.
    let final_ret = if is_linux && ret == u64::MAX {
        // Return -1 (0xFFFFFFFFFFFFFFFF)
        u64::MAX
    } else {
        ret
    };

    context.rax = final_ret;
    final_ret
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

/// sys_kill - Terminar un proceso por su PID
/// sys_kill(pid, sig) — terminar o señalizar un proceso.
/// Por ahora solo implementa SIGKILL (9) y SIGTERM (15) como terminación forzosa.
/// Cualquier otra señal es ignorada (devuelve 0).
fn sys_kill(pid: u64, sig: u64) -> u64 {
    if pid == 0 || pid == 1 {
        return u64::MAX; // No se puede matar al kernel ni al init
    }

    // Señales que no matan: ignorarlas.
    if sig != 0 && sig != 9 && sig != 15 {
        return 0;
    }

    let target_pid = pid as crate::process::ProcessId;

    serial::serial_printf(format_args!("[KILL] pid={} sig={}\n", target_pid, sig));

    let parent_pid = {
        let killed = x86_64::instructions::interrupts::without_interrupts(|| {
            let mut table = crate::process::PROCESS_TABLE.lock();
            for (slot_idx, slot) in table.iter_mut().enumerate() {
                if let Some(p) = slot {
                    if p.id == target_pid {
                        if p.state == crate::process::ProcessState::Terminated {
                            // Ya es zombie (exit() y kill() simultáneos): no actuar.
                            return Some(None);
                        }
                        p.exit_code = 128 + sig; // exit code estándar POSIX para señal
                        p.state = crate::process::ProcessState::Terminated;
                        // NO desregistramos el slot: el proceso queda como zombie hasta
                        // que el padre llame wait() y lo coseche.  El slot se libera en
                        // sys_wait_impl.  Igual que exit_process().
                        crate::ipc::clear_mailbox_slot(slot_idx);
                        return Some(p.parent_pid);
                    }
                }
            }
            None
        });
        match killed {
            Some(pp) => pp,
            None => return u64::MAX, // Proceso no encontrado
        }
    };

    // Notificar al padre (como hace exit_process) para desbloquear su wait().
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

static SERVICE_BIN_LOCK: spin::Mutex<()> = spin::Mutex::new(());

fn get_service_slice(service_id: u64) -> Option<&'static [u8]> {
    // Acquire a global lock during the load check/filesystem read to prevent
    // race conditions on SMP systems where multiple CPUs try to load the
    // same service binary simultaneously.
    let _guard = SERVICE_BIN_LOCK.lock();

    unsafe {
        let (slot, path) = match service_id {
            0 => (&mut SERVICE_LOG_BIN, "/sbin/log_service"),
            1 => (&mut SERVICE_DEVFS_BIN, "/sbin/devfs_service"),
            2 => (&mut SERVICE_FS_BIN, "/sbin/filesystem_service"),
            3 => (&mut SERVICE_INPUT_BIN, "/sbin/input_service"),
            4 => (&mut SERVICE_DISPLAY_BIN, "/sbin/display_service"),
            5 => (&mut SERVICE_AUDIO_BIN, "/sbin/audio_service"),
            6 => (&mut SERVICE_NET_BIN, "/sbin/network_service"),
            7 => (&mut SERVICE_GUI_BIN, "/sbin/gui_service"),
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
    let elf_slice: &[u8] = match get_service_slice(service_id) {
        Some(s) => s,
        None => {
            serial::serial_print("[SYSCALL] spawn_service: invalid service_id or failed to load from disk\n");
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
        match service_id {
            0 => "log",
            1 => "devfs",
            2 => "filesystem",
            3 => "input",
            4 => "display",
            5 => "audio",
            6 => "network",
            7 => "gui",
            _ => "service",
        }
    } else {
        name_str.trim_matches('\0')
    };

    match crate::process::spawn_process(elf_slice, name_trimmed) {
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
    }
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
        
        // Find a free VMA spot (0x40000000 to 0x70000000)
        let mut target_addr: u64 = 0;
        let mut candidate = 0x40000000u64;
        let mut found = false;
        while !found && candidate < 0x70000000 {
            let mut overlap = false;
            for vma in r.vmas.iter() {
                if candidate < vma.end && (candidate + aligned_length as u64) > vma.start {
                    overlap = true;
                    break;
                }
            }
            if !overlap {
                found = true;
                target_addr = candidate;
            } else {
                candidate += 0x1000;
            }
        }
        
        if !found {
            return u64::MAX;
        }
        
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

/// Rutas absolutas de usuario (`/…`) → `file:…`; `/dev/…` → `dev:…`.
fn user_path_to_scheme_path(path_str: &str) -> String {
    if path_str.starts_with("/dev/") {
        format!("dev:{}", path_str.trim_start_matches("/dev/"))
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
fn is_user_pointer(ptr: u64, len: u64) -> bool {
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
    if buf_ptr == 0 || len == 0 || len > 1024 * 1024 {
        return u64::MAX;
    }

    if !is_user_pointer(buf_ptr, len) {
        return u64::MAX;
    }
    
    // File descriptor routing
    // La salida estándar (fd 1 y 2) ya está inicializada hacia "log:" vía fd_init_stdio,
    // así que no necesitamos hardcodear llamadas serial_print que romperían los pipes/pty.

    if let Some(pid) = current_process_id() {
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            unsafe {
                let slice = core::slice::from_raw_parts(buf_ptr as *const u8, len as usize);
                match crate::scheme::write(fd_entry.scheme_id, fd_entry.resource_id, slice) {
                    Ok(written) => return written as u64,
                    Err(_) => return u64::MAX,
                }
            }
        }
    }
    
    u64::MAX
}

/// sys_read - Leer de un file descriptor (IMPLEMENTED)
fn sys_read(fd: u64, buf_ptr: u64, len: u64) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.read_calls += 1;
    drop(stats);
    
    if buf_ptr == 0 || len == 0 || len > 32 * 1024 * 1024 {
        return u64::MAX;
    }
    
    if !is_user_pointer(buf_ptr, len) {
        return u64::MAX;
    }
    
    if let Some(pid) = current_process_id() {
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            unsafe {
                let slice = core::slice::from_raw_parts_mut(buf_ptr as *mut u8, len as usize);
                match crate::scheme::read(fd_entry.scheme_id, fd_entry.resource_id, slice) {
                    Ok(bytes_read) => {
                        return bytes_read as u64
                    },
                    Err(e) => {
                        if e != crate::scheme::error::EAGAIN {
                            serial::serial_printf(format_args!("[SYSCALL] read() scheme error: {}\n", e));
                        }
                        return u64::MAX;
                    }
                }
            }
        } else {
            serial::serial_print("[SYSCALL] read() failed: FD not found\n");
        }
    }
    
    u64::MAX
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
    // Only check pointer if arg is likely a pointer (non-zero and in user range)
    // Some ioctls take integer args, so strictly enforcing is_user_pointer might be wrong.
    // However, if it IS a pointer, verify it.
    // Let's rely on Scheme to validate, or just do range check if it LOOKS like a pointer?
    // Safer: if arg looks like a kernel address, reject it.
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
                     if e != crate::scheme::error::EAGAIN {
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
                    *(sender_pid_ptr as *mut u32) = msg.from;
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
            // Diagnóstico: loguear mensajes recibidos por PID 11 (glxgears).
            if client_id == 11 {
                crate::serial::serial_printf(format_args!(
                    "[RECV-FAST] glxgears pid=11 got msg data_size={} from={} data0={:#x}\n",
                    data_size, from, u32::from_le_bytes([data[0],data[1],data[2],data[3]])
                ));
            }
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

/// sys_getpid - Obtener PID del proceso actual
fn sys_getpid() -> u64 {
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

    // Create child process with modified context
    // The child needs to see RAX=0 (return value of fork)
    let mut child_context = *context;
    child_context.rax = 0;
    
    // Create child process
    match process::fork_process(&child_context) {
        Some(child_pid) => {
            // Add child to scheduler
            crate::scheduler::enqueue_process(child_pid);
            
            child_pid as u64
        }
        None => {
            serial::serial_print("[SYSCALL] fork() failed - could not create child\n");
            u64::MAX // -1 indicates error
        }
    }
}

/// Kernel half: get_service_binary returns pointers in this range.
/// The kernel image itself is linked at KERNEL_OFFSET (0xFFFF_8000_0000_0000),
/// so service binaries embedded in .rodata live at addresses starting there.
const KERNEL_HALF: u64 = 0xFFFF_8000_0000_0000;

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

    if elf_ptr == 0 || elf_size == 0 || elf_size > 32 * 1024 * 1024 {
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
    match crate::elf_loader::replace_process_image(current_pid, elf_data.as_slice()) {
        Ok((entry_point, max_vaddr, phdr_va, phnum, phentsize, segment_frames)) => {
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
                        r.brk_current = max_vaddr;
                    }
                    proc.mem_frames = (0x100000 / 4096) + segment_frames; // stack + segments
                    crate::process::update_process(pid, proc);
                }
            }
            
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
            unsafe {
                let stack_top: u64 = USER_STACK_BASE + USER_STACK_SIZE as u64;
                crate::elf_loader::jump_to_userspace(entry_point, stack_top, phdr_va, phnum, phentsize);
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
    if elf_ptr == 0 || elf_size == 0 || elf_size > 32 * 1024 * 1024 {
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
            // Set parent_pid so the spawning process can wait() for the child.
            let caller_pid = crate::process::current_process_id();
            if let Some(cpid) = caller_pid {
                if let Some(mut child) = crate::process::get_process(pid) {
                    child.parent_pid = Some(cpid);
                    crate::process::update_process(pid, child);
                }
            }
            
            // Add to scheduler
            crate::scheduler::enqueue_process(pid);
            
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
fn sys_spawn_with_stdio(elf_ptr: u64, elf_size: u64, name_ptr: u64, fd_in: u64, fd_out: u64, fd_err: u64) -> u64 {
    // Re-implement the base logic to avoid the race condition!
    // We cannot call sys_spawn directly because it enqueues the process, 
    // making it runnable before we replace its FDs.
    
    use alloc::vec::Vec;
    let elf_slice = unsafe { core::slice::from_raw_parts(elf_ptr as *const u8, elf_size as usize) };
    let mut elf_data = Vec::with_capacity(elf_size as usize);
    elf_data.extend_from_slice(elf_slice);
    
    let name_trimmed = if name_ptr != 0 {
        let name_slice = unsafe { core::slice::from_raw_parts(name_ptr as *const u8, 16) };
        let len = name_slice.iter().position(|&b| b == 0).unwrap_or(16);
        core::str::from_utf8(&name_slice[..len]).unwrap_or("unknown")
    } else {
        "unknown"
    };

    match crate::process::spawn_process(&elf_data, name_trimmed) {
        Ok(pid) => {
            crate::process::modify_process(pid, |p| {
                p.parent_pid = crate::process::current_process_id();
            }).unwrap();

            // Override file descriptors BEFORE enqueuing
            if let Some(parent_pid) = crate::process::current_process_id() {
                let p_fd_in = crate::fd::fd_get(parent_pid, fd_in as usize);
                let p_fd_out = crate::fd::fd_get(parent_pid, fd_out as usize);
                let p_fd_err = crate::fd::fd_get(parent_pid, fd_err as usize);

                if let Some(mut tables) = crate::fd::get_fd_table(pid as u32) {
                    if let Some(child_fd_idx) = crate::fd::pid_to_fd_idx(pid as u32) {
                        if let Some(fd) = p_fd_in { tables[child_fd_idx].fds[0] = fd; }
                        if let Some(fd) = p_fd_out { tables[child_fd_idx].fds[1] = fd; }
                        if let Some(fd) = p_fd_err { tables[child_fd_idx].fds[2] = fd; }
                    }
                }
            }

            // Now it's safely configured, we can let the scheduler run it
            crate::scheduler::enqueue_process(pid);
            pid as u64
        }
        Err(_) => u64::MAX,
    }
}


/// sys_wait - Wait for child process to terminate
/// arg1: pointer to status variable (or 0 for non-blocking poll / WNOHANG semantics)
/// Returns: PID of terminated child, or -1 on error
fn sys_wait(status_ptr: u64) -> u64 {
    sys_wait_impl(status_ptr, 0)
}

/// Esperar hijo concreto (`wait_pid == 0` → cualquier hijo).
fn sys_wait_pid(status_ptr: u64, wait_pid: u64) -> u64 {
    sys_wait_impl(status_ptr, wait_pid)
}

fn sys_wait_impl(status_ptr: u64, wait_pid: u64) -> u64 {
    use crate::process;

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
                                core::ptr::write_unaligned(status_ptr as *mut u32, wait_status);
                            }
                        }
                        process::unregister_child_waiter(current_pid);
                        proc.parent_pid = None;
                        process::update_process(wp, proc);
                        // Cosechar el zombie: liberar el slot ahora que el padre leyó
                        // el exit_code.  Se llama DESPUÉS de update_process porque
                        // update_process usa pid_to_slot_fast internamente.
                        crate::ipc::unregister_pid_slot(wp);
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
                                    core::ptr::write_unaligned(status_ptr as *mut u32, wait_status);
                                }
                            }

                            process::unregister_child_waiter(current_pid);
                            proc.parent_pid = None;
                            process::update_process(*pid, proc);
                            // Cosechar el zombie: liberar slot después de update_process.
                            crate::ipc::unregister_pid_slot(*pid);
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
/// Args: service_id (0-4), out_ptr (pointer to store binary pointer), out_size (pointer to store size)
/// Returns: 0 on success, -1 on error
/// 
/// Service IDs (matching init startup order):
/// 0 = log_service (Log Server / Console)
/// 1 = devfs_service (Device Manager)
/// 2 = filesystem_service (Filesystem Server)
/// 3 = input_service (Input Server)
/// 4 = display_service (Graphics Server)
/// 5 = audio_service (Audio Server)
/// 6 = network_service (Network Server)
/// 7 = gui_service (GUI Launcher)
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
        *(out_ptr as *mut u64) = bin_ptr;
        *(out_size as *mut u64) = bin_size;
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
        return u64::MAX;
    }

    if !is_user_pointer(path_ptr, path_len) {
        return u64::MAX;
    }
    
    // Extract path string
    let path = unsafe {
        let slice = core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize);
        core::str::from_utf8(slice).unwrap_or("")
    };
    

    // Route through scheme system
    // /dev/xxx -> dev:xxx (framebuffer, etc.); other /paths -> file:path
    let (scheme_id, resource_id) = if path.starts_with("/dev/") {
        let dev_path = alloc::format!("dev:{}", path.trim_start_matches("/dev/"));
        match crate::scheme::open(&dev_path, flags as usize, 0) {
            Ok(res) => res,
            Err(e) => {
                if e != crate::scheme::error::EAGAIN {
                    serial::serial_printf(format_args!("[SYSCALL] open() dev failed: error {}\n", e));
                }
                return u64::MAX;
            }
        }
    } else if path.starts_with('/') {
        match crate::scheme::open(&format!("file:{}", path), flags as usize, 0) {
            Ok(res) => res,
            Err(e) => {
                if e != crate::scheme::error::EAGAIN {
                    serial::serial_printf(format_args!("[SYSCALL] open('{}') failed: error {}\n", path, e));
                }
                return u64::MAX;
            }
        }
    } else {
        match crate::scheme::open(path, flags as usize, 0) {
            Ok(res) => res,
            Err(e) => {
                if e != crate::scheme::error::EAGAIN {
                    serial::serial_printf(format_args!("[SYSCALL] open('{}') failed: error {}\n", path, e));
                }
                return u64::MAX;
            }
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
                u64::MAX
            }
        }
    } else {
        // No current process — release the scheme resource to avoid a leak.
        let _ = crate::scheme::close(scheme_id, resource_id);
        u64::MAX
    }
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

            match crate::scheme::lseek(fd_entry.scheme_id, fd_entry.resource_id, offset as isize, whence) {
                Ok(new_offset) => return new_offset as u64,
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
        Err(e) => {
            serial::serial_print("[SYSCALL] mount() failed: ");
            serial::serial_print(e);
            serial::serial_print("\n");
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
        return u64::MAX;
    }
    let aligned_length = (length + 0xFFF) & !0xFFF;
    let num_pages = aligned_length / 4096;

    let current_pid = match process::current_process_id() {
        Some(pid) => pid,
        None => return u64::MAX,
    };

    // Resolve file descriptor before acquiring process resources.
    // A file-backed mapping populates each page with content from the open file.
    // MAP_ANONYMOUS (0x20) means the mapping is not backed by a file.
    // The fd limit matches the per-process FD table size.
    const MMAP_MAP_ANONYMOUS: u64 = 0x20;
    const MMAP_MAX_FD: u64 = crate::fd::MAX_FDS_PER_PROCESS as u64;
    let fd_entry = if (flags & MMAP_MAP_ANONYMOUS) == 0 && fd < MMAP_MAX_FD {
        crate::fd::fd_get(current_pid, fd as usize)
    } else {
        None
    };

    if let Some(mut proc) = process::get_process(current_pid) {
        let mut r = proc.resources.lock();
        let page_table_phys = r.page_table_phys;

        let mut target_addr = addr;
        if target_addr == 0 {
            // Find a free spot
            let mut candidate = 0x40000000;
            let mut found = false;
            while !found && candidate < 0x70000000 {
                let mut overlap = false;
                for vma in r.vmas.iter() {
                    if candidate < vma.end && (candidate + aligned_length) > vma.start {
                        overlap = true;
                        break;
                    }
                }
                if !overlap {
                    found = true;
                } else {
                    candidate += 0x1000;
                }
            }
            target_addr = candidate;
        }

        // Map pages with real physical frames. For shared file-backed mappings, we attempt
        // to use fmap to get the direct physical address. For private mappings (anonymous 
        // or file-backed), we allocate new frames and copy data if needed.
        let mut current = target_addr;
        let end = target_addr + aligned_length;
        let file_len = length as usize;
        let mut file_offset: usize = 0;

        const MAP_SHARED: u64 = 0x01;
        let is_shared = (flags & MAP_SHARED) != 0;

        // Try to use fmap for shared mappings
        let mut fmap_phys_base = None;
        if is_shared {
            if let Some(ref fde) = fd_entry {
                if let Ok(phys) = crate::scheme::fmap(
                    fde.scheme_id,
                    fde.resource_id,
                    offset as usize,
                    aligned_length as usize,
                ) {
                    fmap_phys_base = Some(phys as u64);
                }
            }
        }

        while current < end {
            let frame_phys = if let Some(base) = fmap_phys_base {
                // Use physical frame directly from fmap
                base + (current - target_addr)
            } else if let Some(phys) = memory::alloc_phys_frame_for_anon_mmap() {
                // Zero the frame via the higher-half direct mapping.
                let frame_virt = memory::PHYS_MEM_OFFSET + phys;
                unsafe { core::ptr::write_bytes(frame_virt as *mut u8, 0, 4096); }

                // For private file-backed mappings, read the next 4 KB of file data into the private frame.
                if let Some(ref fde) = fd_entry {
                    let remaining = file_len.saturating_sub(file_offset);
                    if remaining > 0 {
                        let to_read = remaining.min(4096);
                        let frame_slice = unsafe {
                            core::slice::from_raw_parts_mut(frame_virt as *mut u8, to_read)
                        };
                        match crate::scheme::read(fde.scheme_id, fde.resource_id, frame_slice) {
                            Ok(n) => { file_offset += n; }
                            Err(e) => {
                                serial::serial_printf(format_args!(
                                    "[SYSCALL] mmap: file read error at offset {}: {}\n",
                                    file_offset, e
                                ));
                                file_offset += to_read;
                            }
                        }
                    }
                }
                phys
            } else {
                serial::serial_print("[SYSCALL] mmap: physical frame pool exhausted\n");
                0 // map to 0 as fallback
            };

            memory::map_user_page_4kb(page_table_phys, current, frame_phys, prot);
            current += 4096;
        }

        r.vmas.push(VMARegion {
            start: target_addr,
            end: target_addr + aligned_length,
            flags: prot,
            file_backed: fd_entry.is_some(),
        });
        
        proc.mem_frames += num_pages;
        drop(r);
        process::update_process(current_pid, proc);
        return target_addr;
    }
    u64::MAX
}

fn sys_munmap(addr: u64, length: u64) -> u64 {
    use crate::process;
    use crate::memory;
    if length == 0 { return u64::MAX; }
    if let Some(pid) = process::current_process_id() {
        if let Some(mut proc) = process::get_process(pid) {
            let mut r = proc.resources.lock();
            let page_table_phys = r.page_table_phys;
            memory::unmap_user_range(page_table_phys, addr, length);
            r.vmas.retain(|vma| {
                let overlap = core::cmp::max(addr, vma.start) < core::cmp::min(addr + length, vma.end);
                !overlap
            });
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
                            memory::map_user_page_4kb(r.page_table_phys, curr, frame_phys, 0x7);
                            proc.mem_frames += 1;
                        }
                        None => {
                            serial::serial_print("[SYSCALL] brk: physical frame pool exhausted\n");
                            return u64::MAX;
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
    u64::MAX
}

/// sys_clone - Create a new thread or process
/// 
/// Arguments:
///   flags: CLONE_* flags determining what is shared
///   stack: Stack pointer for new thread (0 = kernel allocates)
///   parent_tid: Where to store TID in parent (can be 0)
/// 
/// Returns: TID of new thread/process, or u64::MAX on error
fn sys_clone(flags: u64, stack: u64, _parent_tid: u64) -> u64 {
    use crate::process;
    
    // CLONE_VM (0x100) and CLONE_THREAD (0x10000)
    const CLONE_VM: u64 = 0x00000100;
    const CLONE_THREAD: u64 = 0x00010000;

    if flags & (CLONE_VM | CLONE_THREAD) != (CLONE_VM | CLONE_THREAD) {
        serial::serial_print("sys_clone: Only CLONE_VM | CLONE_THREAD supported for now (threading)\n");
        return u64::MAX;
    }

    if let Some(parent_pid) = process::current_process_id() {
        if let Some(parent) = process::get_process(parent_pid) {
            // Share the resources Arc
            let resources = Arc::clone(&parent.resources);
            
            // Create a new process entry for this thread
            let mut thread = process::Process::new(resources);
            
            // New PID
            let tid = process::next_pid();
            thread.id = tid;
            thread.state = process::ProcessState::Ready;
            thread.priority = parent.priority;
            thread.time_slice = parent.time_slice;
            thread.parent_pid = Some(parent_pid);
            thread.fs_base = parent.fs_base; 
            thread.is_linux = parent.is_linux;
            
            // Copy registers from parent current context
            thread.context = parent.context;
            
            // Set the child return value (RAX=0)
            thread.context.rax = 0;
            
            if stack != 0 {
                thread.context.rsp = stack;
            }

            // Allocate a kernel stack for this thread
            let kstack_size = 32768; // 32KB
            let kstack = alloc::vec![0u8; kstack_size];
            let kstack_top = kstack.as_ptr() as u64 + kstack_size as u64;
            core::mem::forget(kstack);
            thread.kernel_stack_top = kstack_top & !0xF;

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
                crate::scheduler::enqueue_process(tid);
                return tid as u64;
            }
        }
    }

    u64::MAX
}

/// sys_thread_create — nuevo hilo (comparte VM del padre), primera ejecución en `entry` con **rdi** = `arg`.
fn sys_thread_create(stack_top: u64, entry: u64, arg: u64) -> u64 {
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

    if let Some(parent_pid) = process::current_process_id() {
        if let Some(parent) = process::get_process(parent_pid) {
            let resources = Arc::clone(&parent.resources);
            let mut thread = process::Process::new(resources);
            let tid = process::next_pid();
            thread.id = tid;
            thread.state = process::ProcessState::Ready;
            thread.priority = parent.priority;
            thread.time_slice = parent.time_slice;
            thread.parent_pid = Some(parent_pid);
            thread.fs_base = parent.fs_base;
            thread.is_linux = parent.is_linux;

            thread.context = parent.context;
            thread.context.rip = entry;
            thread.context.rdi = arg;
            thread.context.rsp = stack_top;
            thread.context.rax = 0;

            let kstack_size = 32768u32;
            let kstack = alloc::vec![0u8; kstack_size as usize];
            let kstack_top = kstack.as_ptr() as u64 + u64::from(kstack_size);
            core::mem::forget(kstack);
            thread.kernel_stack_top = kstack_top & !0xF;

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

/// sys_gettid - Get thread ID
/// 
/// Returns: Current thread ID (for now, same as PID)
fn sys_gettid() -> u64 {
    // For now, threads not implemented, return PID
    current_process_id().unwrap_or(0) as u64
}

/// sys_futex - Fast userspace mutex
/// 
/// Arguments:
///   uaddr: Address of futex word in user space
///   op: Operation (FUTEX_WAIT, FUTEX_WAKE, etc.)
///   val: Value for operation
///   timeout: Timeout for FUTEX_WAIT (can be 0)
/// 
/// Returns: Depends on operation, u64::MAX on error
struct FutexWaiter {
    addr: u64,
    pid: crate::process::ProcessId,
}

static FUTEX_WAITERS: Mutex<alloc::vec::Vec<FutexWaiter>> = Mutex::new(alloc::vec::Vec::new());

fn sys_futex(uaddr: u64, op: u64, val: u64, _timeout: u64) -> u64 {
    use crate::process;
    
    match op & 0x7F {
        0 => { // FUTEX_WAIT
            // 1. Verify that *uaddr == val
            if !is_user_pointer(uaddr, 4) { return u64::MAX; }
            let current_val = unsafe { *(uaddr as *const u32) };
            if current_val != val as u32 {
                return 11; // -EAGAIN (Linux)
            }
            
            if let Some(pid) = process::current_process_id() {
                // 2. Add to waiters list
                {
                    let mut waiters = FUTEX_WAITERS.lock();
                    waiters.push(FutexWaiter { addr: uaddr, pid });
                }
                
                // 3. Block current process
                if let Some(mut p) = process::get_process(pid) {
                    p.state = process::ProcessState::Blocked;
                    process::update_process(pid, p);
                }
                crate::scheduler::yield_cpu();
                return 0;
            }
            u64::MAX
        }
        1 => { // FUTEX_WAKE
            let mut woken = 0;
            let mut waiters = FUTEX_WAITERS.lock();
            let mut i = 0;
            while i < waiters.len() && woken < val {
                if waiters[i].addr == uaddr {
                    let waiter_pid = waiters[i].pid;
                    if let Some(mut p) = process::get_process(waiter_pid) {
                        p.state = process::ProcessState::Ready;
                        process::update_process(waiter_pid, p);
                        crate::scheduler::enqueue_process(waiter_pid);
                        woken += 1;
                    }
                    waiters.remove(i);
                } else {
                    i += 1;
                }
            }
            woken
        }
        _ => {
            u64::MAX
        }
    }
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

    if ms == 0 {
        // Zero-duration sleep: just yield once to be cooperative
        yield_cpu();
        return 0;
    }

    let current_tick = crate::interrupts::ticks();
    let wake_tick = current_tick.saturating_add(ms);

    // Mark process as Blocked and register in the sleep queue.
    // The timer interrupt will re-enqueue it when wake_tick is reached.
    // NOTE: We set the state directly in PROCESS_TABLE because update_process()
    // intentionally preserves the original state (to protect against races).
    if let Some(pid) = current_process_id() {
        // PROCESS_TABLE is indexed by slot, not by PID value.  After slot reuse,
        // a process can have PID ≥ 64, so table[pid as usize] would be out-of-bounds.
        // Use pid_to_slot_fast() to obtain the correct slot index.
        x86_64::instructions::interrupts::without_interrupts(|| {
            let slot = crate::ipc::pid_to_slot_fast(pid);
            {
                let mut table = crate::process::PROCESS_TABLE.lock();
                if let Some(slot_idx) = slot {
                    if let Some(p) = table[slot_idx].as_mut() {
                        // Safety check: ensure we are still targeting the correct PID
                        if p.id == pid {
                             p.state = crate::process::ProcessState::Blocked;
                        }
                    }
                }
            } // Release PROCESS_TABLE lock before add_sleep to avoid deadlock
            
            // Add to sleep queue while still in the interrupt-disabled section
            // to prevent preemption between marking as Blocked and enqueuing.
            crate::scheduler::add_sleep(pid, wake_tick);
        });
    }

    // Yield CPU; we will be rescheduled by the timer once the sleep expires.
    yield_cpu();
    0
}


fn sys_fstat(fd: u64, stat_ptr: u64) -> u64 {
    if stat_ptr == 0 { return u64::MAX; }
    if !is_user_pointer(stat_ptr, core::mem::size_of::<crate::scheme::Stat>() as u64) {
        return u64::MAX;
    }
    
    if let Some(pid) = current_process_id() {
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            let mut stat = crate::scheme::Stat::default();
            
            // Call scheme fstat
            match crate::scheme::fstat(fd_entry.scheme_id, fd_entry.resource_id, &mut stat) {
                Ok(_) => {
                    // Copy to user memory
                    unsafe {
                        *(stat_ptr as *mut crate::scheme::Stat) = stat;
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
        Err(_) => {
            serial::serial_printf(format_args!(
                "[SYSCALL] socket(domain={}, type={}) -> failed\n",
                domain, type_
            ));
        }
    }
    u64::MAX
}

/// sys_bind - Bind a name to a socket
/// fd: socket file descriptor
/// addr: pointer to sockaddr structure
/// addrlen: size of sockaddr structure
fn sys_bind(fd: u64, addr: u64, addrlen: u64) -> u64 {
    // Validate arguments
    if addr == 0 || addrlen < 2 {
        return u64::MAX; // EINVAL
    }
    
    if !is_user_pointer(addr, addrlen) {
        return u64::MAX; // EFAULT
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
            if let Ok((_scheme_id, _resource_id)) = crate::scheme::open(&final_path_str, 0x40 | 0x80, 0o777) {
                 // Successfully created file node. 
            } else {
                serial::serial_print("[SYSCALL] bind failed to create file node for path\n");
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
            if scheme.listen(fd_info.resource_id).is_ok() {
                return 0;
            }
        }
    }
    u64::MAX
}

/// sys_accept - Accept a connection on a socket
fn sys_accept(fd: u64, _addr: u64, _addrlen: u64) -> u64 {
    if let (Some(pid), Some(scheme)) = (current_process_id(), crate::servers::get_socket_scheme()) {
        if let Some(fd_info) = crate::fd::fd_get(pid, fd as usize) {
            match scheme.accept(fd_info.resource_id) {
                Ok(new_res_id) => {
                    // Create a new FD for the accepted connection
                    if let Some(new_fd) = crate::fd::fd_open(pid, fd_info.scheme_id, new_res_id, fd_info.flags) {
                        return new_fd as u64;
                    }
                },
                Err(e) if e == crate::scheme::error::EAGAIN => {
                    return u64::MAX; // Should return -EAGAIN if we have proper errno handling
                },
                Err(_) => return u64::MAX,
            }
        }
    }
    u64::MAX
}

fn sys_connect(fd: u64, addr: u64, addrlen: u64) -> u64 {
    if addr == 0 || addrlen < 2 { return u64::MAX; }
    if !is_user_pointer(addr, addrlen) { return u64::MAX; }
    let family = unsafe { *(addr as *const u16) };
    
    if family == 1 { // AF_UNIX
        let path_start = addr + 2;
        let path_len = strlen_user_unique(path_start, (addrlen - 2) as usize);
        let mut path_buf = [0u8; 108];
        if path_len > 107 { return u64::MAX; }
        unsafe {
            core::ptr::copy_nonoverlapping(path_start as *const u8, path_buf.as_mut_ptr(), path_len as usize);
        }
        path_buf[path_len as usize] = 0;
        let path_str = match core::str::from_utf8(&path_buf[0..path_len as usize]) {
            Ok(s) => s,
            Err(_) => return u64::MAX,
        };

        if let (Some(pid), Some(scheme)) = (current_process_id(), crate::servers::get_socket_scheme()) {
            if let Some(fd_info) = crate::fd::fd_get(pid, fd as usize) {
                if scheme.connect(fd_info.resource_id, path_str).is_ok() {
                    return 0;
                }
            }
        }
    } else if family == 2 { // AF_INET
        let port_ptr = addr + 2;
        let ip_ptr = addr + 4;
        let port = unsafe { u16::from_be(*(port_ptr as *const u16)) };
        let ip = unsafe { *(ip_ptr as *const [u8; 4]) };
        
        let path = alloc::format!("{}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], port);

        if let (Some(pid), Some(scheme)) = (current_process_id(), crate::servers::get_socket_scheme()) {
            if let Some(fd_info) = crate::fd::fd_get(pid, fd as usize) {
                if scheme.connect(fd_info.resource_id, &path).is_ok() {
                    return 0;
                }
            }
        }
    }

    u64::MAX
}

/// sys_setsockopt - Set options on a socket
fn sys_setsockopt(_fd: u64, _level: u64, _optname: u64, _optval: u64, _optlen: u64) -> u64 {
    // Stub: Always return success
    0
}

/// sys_getsockopt - Get options on a socket
fn sys_getsockopt(_fd: u64, _level: u64, _optname: u64, _optval: u64, _optlen: u64) -> u64 {
    // Stub: Always return success
    0
}

fn sys_fstatat(dirfd: u64, path_ptr: u64, stat_ptr: u64, flags: u64) -> u64 {
    if path_ptr == 0 || stat_ptr == 0 { return u64::MAX; }
    if !is_user_pointer(stat_ptr, core::mem::size_of::<crate::scheme::Stat>() as u64) {
        return u64::MAX;
    }
    
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
        let mut stat = crate::scheme::Stat::default();
        let res = if crate::scheme::fstat(scheme_id, resource_id, &mut stat).is_ok() {
            unsafe {
                *(stat_ptr as *mut crate::scheme::Stat) = stat;
            }
            0
        } else {
            u64::MAX
        };
        crate::scheme::close(scheme_id, resource_id).ok();
        return res;
    }
    
    u64::MAX
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
        Err(_) => u64::MAX,
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
        Err(_) => u64::MAX,
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