//! Sistema de syscalls del microkernel
//! 
//! Implementa la interfaz entre userspace y kernel

use alloc::format;

use crate::process::{ProcessId, exit_process, current_process_id};
use crate::scheduler::yield_cpu;
use crate::ipc::{MessageType, send_message, receive_message};
use crate::serial;
use spin::Mutex;

/// Números de syscalls
#[repr(u64)]
#[derive(Debug, Clone, Copy)]
pub enum SyscallNumber {
    Exit = 0,
    Write = 1,
    Read = 2,
    Send = 3,
    Receive = 4,
    Yield = 5,
    GetPid = 6,
    Fork = 7,
    Exec = 8,
    Wait = 9,
    GetServiceBinary = 10,
    Open = 11,
    Close = 12,
    Lseek = 14,
    GetFramebufferInfo = 15,
    MapFramebuffer = 16,
    PciEnumDevices = 17,
    PciReadConfig = 18,
    PciWriteConfig = 19,
    Mmap = 20,
    Munmap = 21,
    Clone = 22,
    GetTid = 23,
    Futex = 24,
    Nanosleep = 25,
    Brk = 26,
    RegisterDevice = 27,
    Fmap = 28,
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

/// Handler principal de syscalls
pub extern "C" fn syscall_handler(
    syscall_num: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    _arg4: u64,
    _arg5: u64,
    context: &mut crate::interrupts::SyscallContext,
) -> u64 {
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
    
    // DEBUG: Trace all syscalls
    // Limit verbosity - maybe only print specific ones or all for now
    // DEBUG: Trace all syscalls
    // crate::serial::serial_print("SYSCALL: ");
    // crate::serial::serial_print_dec(syscall_num);
    // crate::serial::serial_print("\n");

    let ret = match syscall_num {
        0 => sys_exit(arg1),
        1 => sys_write(arg1, arg2, arg3),
        2 => sys_read(arg1, arg2, arg3),
        3 => sys_send(arg1, arg2, arg3),
        4 => sys_receive(arg1, arg2, arg3),
        5 => sys_yield(),
        6 => sys_getpid(),
        7 => sys_fork(&process_context),
        8 => sys_exec(arg1, arg2),
        9 => sys_wait(arg1),
        10 => sys_get_service_binary(arg1, arg2, arg3),
        11 => sys_open(arg1, arg2, arg3),
        12 => sys_close(arg1),
        13 => sys_getppid(),
        14 => sys_lseek(arg1, arg2 as i64, arg3 as usize),
        15 => sys_get_framebuffer_info(arg1),
        16 => sys_map_framebuffer(),
        17 => sys_pci_enum_devices(arg1, arg2, arg3),
        18 => sys_pci_read_config(arg1, arg2, arg3),
        19 => sys_pci_write_config(arg1, arg2, arg3),
        20 => sys_mmap(arg1, arg2, arg3, _arg4, _arg5),
        21 => sys_munmap(arg1, arg2),
        22 => sys_clone(arg1, arg2, arg3),
        23 => sys_gettid(),
        24 => sys_futex(arg1, arg2, arg3, _arg4),
        25 => sys_nanosleep(arg1),
        26 => sys_brk(arg1),
        27 => sys_register_device(arg1, arg2, arg3),
        28 => sys_fmap(arg1, arg2, arg3),
        _ => {
            serial::serial_print("Unknown syscall: ");
            serial::serial_print_hex(syscall_num);
            serial::serial_print("\n");
            u64::MAX
        }
    };

    ret
}

/// Verify if a pointer range points to valid user memory
/// User memory range: 0x0000_0000_0000_0000 to 0x0000_7FFF_FFFF_FFFF
fn is_user_pointer(ptr: u64, len: u64) -> bool {
    // Check for null pointer
    if ptr == 0 {
        return false;
    }
    
    // Check for overflow
    let end = match ptr.checked_add(len) {
        Some(e) => e,
        None => return false,
    };
    
    // Check upper bound (Canonical lower half)
    if end > 0x0000_8000_0000_0000 {
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
    
    serial::serial_print("Process exiting with code: ");
    serial::serial_print_hex(exit_code);
    serial::serial_print("\n");
    
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
    
    // Handle stdout (1) and stderr (2) by writing to serial
    if fd == 1 || fd == 2 {
        unsafe {
            let slice = core::slice::from_raw_parts(buf_ptr as *const u8, len as usize);
            if let Ok(s) = core::str::from_utf8(slice) {
                serial::serial_print(s);
            }
        }
        return len;
    }

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
    
    if buf_ptr == 0 || len == 0 || len > 1024 * 1024 {
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
                    Ok(bytes_read) => return bytes_read as u64,
                    Err(_) => return u64::MAX,
                }
            }
        }
    }
    
    u64::MAX
}

/// sys_send - Enviar mensaje IPC
fn sys_send(server_id: u64, msg_type: u64, data_ptr: u64) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.send_calls += 1;
    drop(stats);
    
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
        
        // Por ahora enviamos un mensaje vacío (TODO: copiar data_ptr)
        let data = [0u8; 32];
        
        if send_message(client_id, server_id as u32, message_type, &data) {
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
    
    // Validar parámetros
    if buffer_ptr == 0 || size == 0 || size > 4096 {
        return u64::MAX; // Error
    }
    
    // DEBUG: Print entry
    crate::serial::serial_print("R"); // R = syscall receive called
    
    if let Some(client_id) = current_process_id() {
        // Intentar recibir mensaje
        if let Some(msg) = receive_message(client_id) {
            crate::serial::serial_print("!"); // ! = Message found
            
            // Copiar mensaje a buffer de usuario
            unsafe {
                let user_buf = core::slice::from_raw_parts_mut(
                    buffer_ptr as *mut u8,
                    core::cmp::min(size as usize, 32)
                );
                user_buf.copy_from_slice(&msg.data[..core::cmp::min(size as usize, 32)]);
                
                // Si se proporcionó un puntero para el PID del remitente, escribirlo
                if sender_pid_ptr != 0 {
                    *(sender_pid_ptr as *mut u64) = msg.from as u64;
                }
            }
            return msg.data_size as u64;
        }
    }
    
    // crate::serial::serial_print("0"); // 0 = Only print if debugging spam needed
    0 // No hay mensajes
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
    
    let current_pid = process::current_process_id().unwrap_or(0);
    serial::serial_print("[SYSCALL] fork() called from PID ");
    serial::serial_print_dec(current_pid as u64);
    serial::serial_print("\n");
    
    // Create child process with modified context
    // The child needs to see RAX=0 (return value of fork)
    let mut child_context = *context;
    child_context.rax = 0;
    
    // Create child process
    match process::fork_process(&child_context) {
        Some(child_pid) => {
            serial::serial_print("[SYSCALL] fork() created child process with PID: ");
            serial::serial_print_dec(child_pid as u64);
            serial::serial_print(", returning to parent PID ");
            serial::serial_print_dec(current_pid as u64);
            serial::serial_print("\n");
            
            // Add child to scheduler
            crate::scheduler::enqueue_process(child_pid);
            
            // Return child PID to parent
            serial::serial_print("[SYSCALL] fork() returning ");
            serial::serial_print_dec(child_pid as u64);
            serial::serial_print(" to parent\n");
            child_pid as u64
        }
        None => {
            serial::serial_print("[SYSCALL] fork() failed - could not create child\n");
            u64::MAX // -1 indicates error
        }
    }
}

/// sys_exec - Replace current process with new program
/// arg1: pointer to ELF buffer
/// arg2: size of ELF buffer
/// Returns: 0 on success (doesn't return on success), -1 on error
fn sys_exec(elf_ptr: u64, elf_size: u64) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.exec_calls += 1;
    drop(stats);
    
    serial::serial_print("[SYSCALL] exec() called with buffer at ");
    serial::serial_print_hex(elf_ptr);
    serial::serial_print(", size: ");
    serial::serial_print_dec(elf_size);
    serial::serial_print("\n");
    
    if elf_ptr == 0 || elf_size == 0 || elf_size > 10 * 1024 * 1024 {
        serial::serial_print("[SYSCALL] exec() invalid parameters\n");
        return u64::MAX;
    }
    
    // Create slice from buffer
    let elf_data = unsafe {
        core::slice::from_raw_parts(elf_ptr as *const u8, elf_size as usize)
    };
    
    // Replace current process with ELF binary
    if let Some(entry_point) = crate::elf_loader::replace_process_image(elf_data) {
        serial::serial_print("[SYSCALL] exec() replacing process image, entry: ");
        serial::serial_print_hex(entry_point);
        serial::serial_print("\n");
        
        // This doesn't return - we jump to the new process entry point
        unsafe {
            // Use standard userspace stack top (512MB + 256KB)
            let stack_top: u64 = 0x20040000;
            crate::elf_loader::jump_to_userspace(entry_point, stack_top);
        }
    } else {
        serial::serial_print("[SYSCALL] exec() failed to load ELF\n");
        return u64::MAX;
    }
}

/// sys_wait - Wait for child process to terminate
/// arg1: pointer to status variable (or 0 to ignore)
/// Returns: PID of terminated child, or -1 on error
fn sys_wait(_status_ptr: u64) -> u64 {
    use crate::process;
    
    let mut stats = SYSCALL_STATS.lock();
    stats.wait_calls += 1;
    drop(stats);
    
    // Get current process ID
    let current_pid = match process::current_process_id() {
        Some(pid) => pid,
        None => {
            serial::serial_print("[SYSCALL] wait() failed - no current process\n");
            return u64::MAX;
        }
    };
    
    // Look for terminated child processes
    let processes = process::list_processes();
    for (pid, state) in processes.iter() {
        if *pid == 0 {
            continue;
        }
        
        if state == &process::ProcessState::Terminated {
            if let Some(proc) = process::get_process(*pid) {
                if proc.parent_pid == Some(current_pid) {
                    // TODO: Clean up child process resources
                    // TODO: Write exit status to status_ptr if non-zero
                    
                    return *pid as u64;
                }
            }
        }
    }
    
    u64::MAX // -1 indicates no children or error
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
    serial::serial_print("[SYSCALL] get_service_binary(");
    serial::serial_print_dec(service_id);
    serial::serial_print(")\n");
    
    // Validate pointers
    if out_ptr == 0 || out_size == 0 {
        return u64::MAX;
    }
    
    // Check user pointer validity
    if !is_user_pointer(out_ptr, 8) || !is_user_pointer(out_size, 8) {
         serial::serial_print("[SYSCALL] get_service_binary - invalid user pointers\n");
         return u64::MAX;
    }
    
    // Get service binary based on ID (new init startup order)
    let (bin_ptr, bin_size) = match service_id {
        0 => (crate::binaries::LOG_SERVICE_BINARY.as_ptr() as u64, crate::binaries::LOG_SERVICE_BINARY.len() as u64),
        1 => (crate::binaries::DEVFS_SERVICE_BINARY.as_ptr() as u64, crate::binaries::DEVFS_SERVICE_BINARY.len() as u64),
        2 => (crate::binaries::FILESYSTEM_SERVICE_BINARY.as_ptr() as u64, crate::binaries::FILESYSTEM_SERVICE_BINARY.len() as u64),
        3 => (crate::binaries::INPUT_SERVICE_BINARY.as_ptr() as u64, crate::binaries::INPUT_SERVICE_BINARY.len() as u64),
        4 => (crate::binaries::DISPLAY_SERVICE_BINARY.as_ptr() as u64, crate::binaries::DISPLAY_SERVICE_BINARY.len() as u64),
        5 => (crate::binaries::AUDIO_SERVICE_BINARY.as_ptr() as u64, crate::binaries::AUDIO_SERVICE_BINARY.len() as u64),
        6 => (crate::binaries::NETWORK_SERVICE_BINARY.as_ptr() as u64, crate::binaries::NETWORK_SERVICE_BINARY.len() as u64),
        7 => (crate::binaries::GUI_SERVICE_BINARY.as_ptr() as u64, crate::binaries::GUI_SERVICE_BINARY.len() as u64),
        _ => {
            serial::serial_print("[SYSCALL] Invalid service ID\n");
            return u64::MAX;
        }
    };
    
    // Write pointer and size to user-provided addresses
    unsafe {
        *(out_ptr as *mut u64) = bin_ptr;
        *(out_size as *mut u64) = bin_size;
    }
    
    serial::serial_print("[SYSCALL] Service binary: ptr=");
    serial::serial_print_hex(bin_ptr);
    serial::serial_print(", size=");
    serial::serial_print_dec(bin_size);
    serial::serial_print("\n");
    
    0 // Success
}

/// sys_register_device - Register a new device node (Syscall 27)
fn sys_register_device(name_ptr: u64, name_len: u64, type_id: u64) -> u64 {
    serial::serial_print("[SYSCALL] register_device called\n");
    
    if name_ptr == 0 || name_len == 0 || name_len > 256 {
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
fn sys_open(path_ptr: u64, path_len: u64, flags: u64) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.open_calls += 1;
    drop(stats);
    
    // Validate parameters
    if path_ptr == 0 || path_len == 0 || path_len > 4096 {
        return u64::MAX;
    }
    
    // Extract path string
    let path = unsafe {
        let slice = core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize);
        core::str::from_utf8(slice).unwrap_or("")
    };
    
    serial::serial_print("[SYSCALL] open(\"");
    serial::serial_print(path);
    serial::serial_print("\")\n");

    // Route through scheme system
    // Paths starting with '/' are routed to the 'file:' scheme for compatibility
    let (scheme_id, resource_id) = if path.starts_with('/') {
        match crate::scheme::open(&format!("file:{}", path), flags as usize, 0) {
            Ok(res) => res,
            Err(e) => {
                serial::serial_print("[SYSCALL] open() failed: error ");
                serial::serial_print_dec(e as u64);
                serial::serial_print("\n");
                return u64::MAX;
            }
        }
    } else {
        match crate::scheme::open(path, flags as usize, 0) {
            Ok(res) => res,
            Err(e) => {
                serial::serial_print("[SYSCALL] open() failed: error ");
                serial::serial_print_dec(e as u64);
                serial::serial_print("\n");
                return u64::MAX;
            }
        }
    };

    if let Some(pid) = current_process_id() {
        match crate::fd::fd_open(pid, scheme_id, resource_id, flags as u32) {
            Some(fd) => {
                serial::serial_print("[SYSCALL] open() -> FD ");
                serial::serial_print_dec(fd as u64);
                serial::serial_print("\n");
                fd as u64
            }
            None => u64::MAX,
        }
    } else {
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
    
    u64::MAX
}

/// sys_fmap - Map a resource into memory via its scheme
fn sys_fmap(fd: u64, offset: u64, len: u64) -> u64 {
    if let Some(pid) = current_process_id() {
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            serial::serial_print("SYS_FMAP: PID ");
            serial::serial_print_dec(pid as u64);
            serial::serial_print(" FD ");
            serial::serial_print_dec(fd);
            serial::serial_print(" Scheme ");
            serial::serial_print_dec(fd_entry.scheme_id as u64);
            serial::serial_print("\n");

            match crate::scheme::fmap(fd_entry.scheme_id, fd_entry.resource_id, offset as usize, len as usize) {
                Ok(phys_addr) => {
                    // For now, we perform the mapping here.
                    // Ideally, it would be a separate syscall or fmap would return something
                    // that the kernel then maps into the process address space.
                    
                    // HACK: For display, we perform the actual mapping
                    if fd_entry.scheme_id == 1 { // display: scheme
                         return crate::memory::map_framebuffer_for_process(
                             crate::process::get_process_page_table(crate::process::current_process_id()),
                             phys_addr as u64,
                             len as u64
                         );
                    }

                    // Return physical address for others (not really a mapping yet)
                    return phys_addr as u64;
                }
                Err(_) => return u64::MAX,
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

/// sys_get_framebuffer_info - Get framebuffer information from bootloader
/// Accepts a pointer to userspace buffer and copies framebuffer info into it
/// Returns 0 on success, -1 on failure
/// sys_get_framebuffer_info - Get framebuffer information from bootloader
/// Accepts a pointer to userspace buffer and copies framebuffer info into it
/// Returns 0 on success, -1 on failure
fn sys_get_framebuffer_info(user_buffer: u64) -> u64 {
    use crate::servers::FramebufferInfo;
    
    if user_buffer == 0 {
        return u64::MAX; // -1 as u64
    }
    
    let fb_info_ptr = crate::boot::get_framebuffer_info();
    serial::serial_print("[SYSCALL] get_framebuffer_info ptr: ");
    serial::serial_print_hex(fb_info_ptr);
    serial::serial_print("\n");

    if fb_info_ptr == 0 {
        serial::serial_print("[SYSCALL] ERROR: Framebuffer info pointer is NULL\n");
        return u64::MAX; // -1 as u64
    }
    
    // The boot/kernel FramebufferInfo is what we want.
    // We just need to make sure we copy it to the userspace buffer format.
    
    // NOTE: boot::get_framebuffer_info() returns a pointer to the FramebufferInfo struct inside BootInfo
    let kernel_fb_ptr = crate::boot::get_framebuffer_info() as *const crate::boot::FramebufferInfo;

    unsafe {
        if kernel_fb_ptr.is_null() {
             serial::serial_print("[SYSCALL] ERROR: Kernel framebuffer pointer is null\n");
             return u64::MAX;
        }

        let kernel_fb = &*kernel_fb_ptr;
        
        // Calculate BPP from pixel format
        // Pixel format 1 = RGB, typically 32bpp
        // For now, assume 32bpp for RGB formats
        let bpp: u16 = 32;
        let bytes_per_pixel = bpp / 8;
        
        // Calculate pitch (bytes per scanline)
        let pitch = kernel_fb.pixels_per_scan_line * bytes_per_pixel as u32;
        
        // Create syscall structure
        let syscall_fb = FramebufferInfo {
            address: kernel_fb.base_address,
            width: kernel_fb.width,
            height: kernel_fb.height,
            pitch,
            bpp,
            red_mask_size: 8,
            red_mask_shift: 16,
            green_mask_size: 8,
            green_mask_shift: 8,
            blue_mask_size: 8,
            blue_mask_shift: 0,
        };
        
        // Copy to userspace buffer
        let user_fb_info = user_buffer as *mut FramebufferInfo;
        core::ptr::write(user_fb_info, syscall_fb);
    }
    
    0 // Success
}

/// sys_map_framebuffer - Map framebuffer physical memory into process virtual space
/// Returns the virtual address where framebuffer is mapped, or 0 on failure
fn sys_map_framebuffer() -> u64 {
    // Get framebuffer info
    let fb_info_ptr = crate::boot::get_framebuffer_info();
    if fb_info_ptr == 0 {
        serial::serial_print("MAP_FB: No framebuffer info available\n");
        return 0;
    }
    
    let kernel_fb = unsafe { &*(fb_info_ptr as *const crate::boot::FramebufferInfo) };
    
    // Calculate framebuffer size correctly
    // pixels_per_scan_line * height * 4 bytes per pixel (32bpp)
    // Map 2x size to support double buffering in display_service
    let single_frame_size = (kernel_fb.pixels_per_scan_line * kernel_fb.height * 4) as u64;
    let fb_size = single_frame_size * 2;
    
    // Align to 4KB (page size)
    let fb_size = (fb_size + 0xFFF) & !0xFFF;
    
    serial::serial_print("MAP_FB: Framebuffer mapping request (Double Buffered)\n");
    serial::serial_print("  Phys addr: ");
    serial::serial_print_hex(kernel_fb.base_address);
    serial::serial_print("\n  Size: ");
    serial::serial_print_hex(fb_size);
    serial::serial_print("\n");
    
    // Get current process page table
    let current_pid = crate::process::current_process_id();
    let page_table_phys = crate::process::get_process_page_table(current_pid);
    
    if page_table_phys == 0 {
        serial::serial_print("MAP_FB: ERROR - Could not get process page table\n");
        return 0;
    }
    
    // Map framebuffer into process address space
    let vaddr = crate::memory::map_framebuffer_for_process(page_table_phys, kernel_fb.base_address, fb_size);
    serial::serial_print("MAP_FB: Done. Returning v=");
    serial::serial_print_hex(vaddr);
    serial::serial_print("\n");
    vaddr
}

/// sys_pci_enum_devices - Enumerate PCI devices by class
/// Args:
///   arg1: class_code (0xFF = all devices, 0x04 = multimedia/audio)
///   arg2: buffer pointer (array of PciDeviceInfo structs)
///   arg3: buffer size (max number of devices)
/// Returns: number of devices found, or u64::MAX on error
fn sys_pci_enum_devices(class_code: u64, buffer_ptr: u64, max_devices: u64) -> u64 {
    serial::serial_print("[SYSCALL] pci_enum_devices(class=");
    serial::serial_print_hex(class_code);
    serial::serial_print(")\n");
    
    // Validate parameters
    if buffer_ptr == 0 || max_devices == 0 || max_devices > 256 {
        serial::serial_print("[SYSCALL] pci_enum_devices - invalid parameters\n");
        return u64::MAX;
    }
    
    // Get audio devices from PCI subsystem
    let devices = if class_code == 0x04 {
        // Multimedia/Audio devices
        crate::pci::find_audio_devices()
    } else if class_code == 0xFF {
        // All devices - not implemented for now
        serial::serial_print("[SYSCALL] pci_enum_devices - all devices not supported yet\n");
        return 0;
    } else {
        serial::serial_print("[SYSCALL] pci_enum_devices - unsupported class\n");
        return 0;
    };
    
    let count = core::cmp::min(devices.len(), max_devices as usize);
    
    serial::serial_print("[SYSCALL] pci_enum_devices - found ");
    serial::serial_print_dec(count as u64);
    serial::serial_print(" device(s)\n");
    
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
    let offset = offset as u8;
    
    // Validate parameters
    if device > 31 || function > 7 || offset > 252 {
        serial::serial_print("[SYSCALL] pci_read_config - invalid parameters\n");
        return u64::MAX;
    }
    
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
    let offset = offset as u8;
    
    // Validate parameters
    if device > 31 || function > 7 || offset > 252 {
        serial::serial_print("[SYSCALL] pci_write_config - invalid parameters\n");
        return u64::MAX;
    }
    
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
fn sys_mmap(addr: u64, length: u64, _prot: u64, flags: u64, _fd: u64) -> u64 {
    // For now, simple implementation using heap allocator
    // Proper implementation would use page tables
    
    if length == 0 {
        return u64::MAX; // Invalid size
    }
    
    // MAP_ANONYMOUS (0x20) - not backed by a file
    let is_anonymous = (flags & 0x20) != 0;
    
    if !is_anonymous {
        // File-backed mappings not yet supported
        return u64::MAX;
    }
    
    // Allocate memory (simplified - should use page allocator)
    // For now, return a fake address in user space
    // Real implementation would allocate pages and map them
    
    // Use a simple bump allocator concept
    static MMAP_NEXT_ADDR: Mutex<u64> = Mutex::new(0x40000000);
    
    let mut next_addr = MMAP_NEXT_ADDR.lock();
    let result_addr = if addr == 0 {
        *next_addr
    } else {
        addr
    };
    
    // Align to page boundary (4KB)
    let aligned_addr = (result_addr + 0xFFF) & !0xFFF;
    let aligned_length = (length + 0xFFF) & !0xFFF;
    
    *next_addr = aligned_addr + aligned_length;
    drop(next_addr);
    
    // TODO: Actually allocate and map pages
    // For now, just return the address
    aligned_addr
}

/// sys_munmap - Unmap memory from process address space
/// 
/// Arguments:
///   addr: Start address to unmap
///   length: Number of bytes to unmap
/// 
/// Returns: 0 on success, u64::MAX on error
fn sys_munmap(_addr: u64, _length: u64) -> u64 {
    // TODO: Actually unmap pages
    // For now, just return success
    0
}

/// sys_clone - Create a new thread or process
/// 
/// Arguments:
///   flags: CLONE_* flags determining what is shared
///   stack: Stack pointer for new thread (0 = kernel allocates)
///   parent_tid: Where to store TID in parent (can be 0)
/// 
/// Returns: TID of new thread/process, or u64::MAX on error
fn sys_clone(_flags: u64, _stack: u64, _parent_tid: u64) -> u64 {
    // Thread creation not yet fully implemented
    // Would need:
    // - Thread local storage
    // - Separate stack allocation
    // - Thread scheduler support
    // - Proper synchronization
    
    serial::serial_print("sys_clone: Thread creation not yet implemented\n");
    u64::MAX // Not implemented yet
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
fn sys_futex(_uaddr: u64, op: u64, _val: u64, _timeout: u64) -> u64 {
    // Futex operations:
    // 0 = FUTEX_WAIT - wait if *uaddr == val
    // 1 = FUTEX_WAKE - wake up to val waiters
    // 2 = FUTEX_FD - deprecated
    // 3 = FUTEX_REQUEUE - requeue waiters to another futex
    
    match op & 0x7F {
        0 => {
            // FUTEX_WAIT - for now, just yield
            sys_yield();
            0
        }
        1 => {
            // FUTEX_WAKE - return number woken (0 for now)
            0
        }
        _ => {
            serial::serial_print("sys_futex: Unknown operation: ");
            serial::serial_print_hex(op);
            serial::serial_print("\n");
            u64::MAX
        }
    }
}

/// sys_nanosleep - Sleep for specified time
/// 
/// Arguments:
///   req: Pointer to timespec structure (seconds + nanoseconds)
/// 
/// Returns: 0 on success, u64::MAX on error
fn sys_nanosleep(_req: u64) -> u64 {
    // For now, just yield CPU a few times to simulate sleep
    // Real implementation would use timer interrupts
    for _ in 0..100 {
        sys_yield();
    }
    0
}

/// sys_brk - Change program break (heap end)
/// 
/// Arguments:
///   addr: New program break address (0 = query current)
/// 
/// Returns: New program break address
fn sys_brk(addr: u64) -> u64 {
    // Simple heap management
    static PROGRAM_BREAK: Mutex<u64> = Mutex::new(0x50000000);
    
    let mut brk = PROGRAM_BREAK.lock();
    
    if addr == 0 {
        // Query current break
        return *brk;
    }
    
    // Set new break (no validation for now)
    *brk = addr;
    addr
}

/// Inicializar sistema de syscalls
pub fn init() {
    serial::serial_print("Syscall system initialized\n");
}
