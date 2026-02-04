//! Sistema de syscalls del microkernel
//! 
//! Implementa la interfaz entre userspace y kernel

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
    frame_ptr: u64,
) -> u64 {
    // Read user context from the interrupt frame
    // Frame structure relative to frame_ptr (RBP):
    // [32] RSP, [24] RFLAGS, [16] CS, [8] RIP, [0] RBP
    // Pushed registers: [-8] RAX, [-16] RBX, [-24] RCX, [-32] RDX, [-40] RSI, [-48] RDI, 
    // [-56] R8, [-64] R9, [-72] R10, [-80] R11, [-88] R12, [-96] R13, [-104] R14, [-112] R15
    let mut context = crate::process::Context::new();
    unsafe {
        unsafe fn read_stack(rbp: u64, offset: isize) -> u64 {
            let ptr = (rbp as isize + offset) as *const u64;
            *ptr
        }
    
        context.rsp = read_stack(frame_ptr, 32);
        context.rip = read_stack(frame_ptr, 8);
        context.rflags = read_stack(frame_ptr, 24);
        context.rbp = read_stack(frame_ptr, 0);
        
        context.rax = read_stack(frame_ptr, -8);
        context.rbx = read_stack(frame_ptr, -16);
        context.rcx = read_stack(frame_ptr, -24);
        context.rdx = read_stack(frame_ptr, -32);
        context.rsi = read_stack(frame_ptr, -40);
        context.rdi = read_stack(frame_ptr, -48);
        context.r8  = read_stack(frame_ptr, -56);
        context.r9  = read_stack(frame_ptr, -64);
        context.r10 = read_stack(frame_ptr, -72);
        context.r11 = read_stack(frame_ptr, -80);
        context.r12 = read_stack(frame_ptr, -88);
        context.r13 = read_stack(frame_ptr, -96);
        context.r14 = read_stack(frame_ptr, -104);
        context.r15 = read_stack(frame_ptr, -112);
    }

    let mut stats = SYSCALL_STATS.lock();
    stats.total_calls += 1;
    drop(stats);
    
    // DEBUG: Trace all syscalls
    // serial::serial_print("SYSCALL: ");
    // serial::serial_print_dec(syscall_num);
    // serial::serial_print("\n");

    let ret = match syscall_num {
        0 => sys_exit(arg1),
        1 => sys_write(arg1, arg2, arg3),
        2 => sys_read(arg1, arg2, arg3),
        3 => sys_send(arg1, arg2, arg3),
        4 => sys_receive(arg1, arg2, arg3),
        5 => sys_yield(),
        6 => sys_getpid(),
        7 => sys_fork(&context),
        8 => sys_exec(arg1, arg2),
        9 => sys_wait(arg1),
        10 => sys_get_service_binary(arg1, arg2, arg3),
        11 => sys_open(arg1, arg2, arg3),
        12 => sys_close(arg1),
        13 => sys_getppid(),
        14 => sys_lseek(arg1, arg2 as i64, arg3),
        _ => {
            serial::serial_print("Unknown syscall: ");
            serial::serial_print_hex(syscall_num);
            serial::serial_print("\n");
            u64::MAX
        }
    };

    ret
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
    if buf_ptr == 0 || len == 0 || len > 4096 {
        serial::serial_print("[SYSCALL] write() - invalid parameters\n");
        return u64::MAX; // Error
    }
    
    // Handle stdin (0) - error, can't write to stdin
    if fd == 0 {
        serial::serial_print("[SYSCALL] write() - cannot write to stdin\n");
        return u64::MAX;
    }
    
    // Handle stdout/stderr (1, 2) - write to serial
    if fd == 1 || fd == 2 {
        unsafe {
            let slice = core::slice::from_raw_parts(buf_ptr as *const u8, len as usize);
            if let Ok(s) = core::str::from_utf8(slice) {
                serial::serial_print(s);
            } else {
                // Fallback for non-utf8 (print safe chars)
                for &byte in slice {
                    if byte >= 32 && byte <= 126 || byte == b'\n' || byte == b'\r' {
                         serial::serial_print(core::str::from_utf8(&[byte]).unwrap_or("."));
                    } else {
                        serial::serial_print(".");
                    }
                }
            }
        }
        return len;
    }
    
    // Handle regular files (fd 3+)
    if let Some(pid) = current_process_id() {
        // Look up file descriptor
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            serial::serial_print("[SYSCALL] write(FD=");
            serial::serial_print_dec(fd);
            serial::serial_print(", inode=");
            serial::serial_print_dec(fd_entry.inode as u64);
            serial::serial_print(", offset=");
            serial::serial_print_dec(fd_entry.offset);
            serial::serial_print(", len=");
            serial::serial_print_dec(len);
            serial::serial_print(")\n");
            
            // Copy data from user buffer
            unsafe {
                let slice = core::slice::from_raw_parts(buf_ptr as *const u8, len as usize);
                
                // Call filesystem write
                match crate::filesystem::Filesystem::write_file_by_inode(
                    fd_entry.inode, 
                    slice, 
                    fd_entry.offset
                ) {
                    Ok(bytes_written) => {
                        serial::serial_print("[SYSCALL] write() - ");
                        serial::serial_print_dec(bytes_written as u64);
                        serial::serial_print(" bytes written to disk\n");
                        
                        // Update file offset
                        let new_offset = fd_entry.offset + bytes_written as u64;
                        crate::fd::fd_update_offset(pid, fd as usize, new_offset);
                        
                        serial::serial_print("[SYSCALL] write() - offset updated to ");
                        serial::serial_print_dec(new_offset);
                        serial::serial_print("\n");
                        
                        return bytes_written as u64;
                    }
                    Err(e) => {
                        serial::serial_print("[SYSCALL] write() - error: ");
                        serial::serial_print(e);
                        serial::serial_print("\n");
                        return u64::MAX; // Error
                    }
                }
            }
        } else {
            serial::serial_print("[SYSCALL] write() - invalid FD\n");
            return u64::MAX; // Invalid FD
        }
    }
    
    serial::serial_print("[SYSCALL] write() - no current process\n");
    u64::MAX // Error
}

/// sys_read - Leer de un file descriptor (IMPLEMENTED)
fn sys_read(fd: u64, buf_ptr: u64, len: u64) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.read_calls += 1;
    drop(stats);
    
    // Validar parámetros
    if buf_ptr == 0 || len == 0 || len > 4096 {
        return u64::MAX; // Error
    }
    
    // Handle stdin (fd=0) specially
    if fd == 0 {
        // TODO: Implementar buffer de input real
        // Por ahora retornar 0 (EOF)
        return 0;
    }
    
    // Get current process ID
    if let Some(pid) = current_process_id() {
        // Look up file descriptor
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            serial::serial_print("[SYSCALL] read(FD=");
            serial::serial_print_dec(fd);
            serial::serial_print(", len=");
            serial::serial_print_dec(len);
            serial::serial_print(", inode=");
            serial::serial_print_dec(fd_entry.inode as u64);
            serial::serial_print(")\n");
            
            // Read from file using filesystem
            // We need to use read_file_by_inode or similar
            // For now, let's skip offset handling and read from beginning
            let mut temp_buffer = [0u8; 4096];
            let read_len = core::cmp::min(len as usize, 4096);
            
            // TODO: Implement read_file_by_inode_with_offset in filesystem
            // For now, this is a limitation - we can't read with offset
            match crate::filesystem::Filesystem::read_file_by_inode(fd_entry.inode, &mut temp_buffer[..read_len]) {
                Ok(bytes_read) => {
                    // Copy to user buffer
                    unsafe {
                        let user_buf = core::slice::from_raw_parts_mut(
                            buf_ptr as *mut u8,
                            bytes_read
                        );
                        user_buf.copy_from_slice(&temp_buffer[..bytes_read]);
                    }
                    
                    // Update file offset
                    let new_offset = fd_entry.offset + bytes_read as u64;
                    crate::fd::fd_update_offset(pid, fd as usize, new_offset);
                    
                    serial::serial_print("[SYSCALL] read() - success, ");
                    serial::serial_print_dec(bytes_read as u64);
                    serial::serial_print(" bytes\n");
                    
                    bytes_read as u64
                },
                Err(e) => {
                    serial::serial_print("[SYSCALL] read() - error: ");
                    serial::serial_print(e);
                    serial::serial_print("\n");
                    u64::MAX
                }
            }
        } else {
            serial::serial_print("[SYSCALL] read() - invalid FD\n");
            u64::MAX
        }
    } else {
        u64::MAX
    }
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
    
    // Create child process
    match process::fork_process(context) {
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
                    serial::serial_print("[SYSCALL] wait() found terminated child PID: ");
                    serial::serial_print_dec(*pid as u64);
                    serial::serial_print("\n");
                    
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
fn sys_get_service_binary(service_id: u64, out_ptr: u64, out_size: u64) -> u64 {
    serial::serial_print("[SYSCALL] get_service_binary(");
    serial::serial_print_dec(service_id);
    serial::serial_print(")\n");
    
    // Validate pointers
    if out_ptr == 0 || out_size == 0 {
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

/// sys_open - Open a file
/// Args: path_ptr (pointer to path string), path_len (length of path), flags (open flags)
/// Returns: file descriptor on success, -1 on error
fn sys_open(path_ptr: u64, path_len: u64, flags: u64) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.open_calls += 1;
    drop(stats);
    
    serial::serial_print("[SYSCALL] open() called\n");
    
    // Validate parameters
    if path_ptr == 0 || path_len == 0 || path_len > 4096 {
        serial::serial_print("[SYSCALL] open() - invalid parameters\n");
        return u64::MAX;
    }
    
    // Extract path string
    let path = unsafe {
        let slice = core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize);
        core::str::from_utf8(slice).unwrap_or("")
    };
    
    serial::serial_print("[SYSCALL] open(\"");
    serial::serial_print(path);
    serial::serial_print("\", flags=");
    serial::serial_print_hex(flags);
    serial::serial_print(")\n");
    
    // Check if filesystem is mounted
    if !crate::filesystem::is_mounted() {
        serial::serial_print("[SYSCALL] open() - filesystem not mounted\n");
        return u64::MAX;
    }
    
    // Try to look up the file in the filesystem
    // For now, we'll use a simplified approach - if the file exists,
    // we can open it for reading
    match crate::filesystem::Filesystem::lookup_path(path) {
        Ok(inode) => {
            // Get current process ID
            if let Some(pid) = current_process_id() {
                // Allocate file descriptor
                match crate::fd::fd_open(pid, inode, flags as u32) {
                    Some(fd) => {
                        serial::serial_print("[SYSCALL] open() - success, FD=");
                        serial::serial_print_dec(fd as u64);
                        serial::serial_print("\n");
                        fd as u64
                    },
                    None => {
                        serial::serial_print("[SYSCALL] open() - FD table full\n");
                        u64::MAX
                    }
                }
            } else {
                serial::serial_print("[SYSCALL] open() - no current process\n");
                u64::MAX
            }
        },
        Err(_) => {
            serial::serial_print("[SYSCALL] open() - file not found: ");
            serial::serial_print(path);
            serial::serial_print("\n");
            u64::MAX
        }
    }
}

/// sys_close - Close a file descriptor
/// Args: fd (file descriptor)
/// Returns: 0 on success, -1 on error
fn sys_close(fd: u64) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.close_calls += 1;
    drop(stats);
    
    serial::serial_print("[SYSCALL] close(");
    serial::serial_print_dec(fd);
    serial::serial_print(")\n");
    
    // Don't allow closing stdio descriptors
    if fd < 3 {
        serial::serial_print("[SYSCALL] close() - cannot close stdio\n");
        return u64::MAX;
    }
    
    // Get current process ID
    if let Some(pid) = current_process_id() {
        // Close the file descriptor
        if crate::fd::fd_close(pid, fd as usize) {
            serial::serial_print("[SYSCALL] close() - success\n");
            0
        } else {
            serial::serial_print("[SYSCALL] close() - invalid FD\n");
            u64::MAX
        }
    } else {
        u64::MAX
    }
}

/// sys_lseek - Reposition file offset
///
/// Changes the file offset for the specified file descriptor.
/// Returns the new offset from the beginning of the file, or u64::MAX on error.
///
/// Parameters:
/// - fd: File descriptor
/// - offset: Offset value (interpretation depends on whence)
/// - whence: How to interpret offset:
///   - SEEK_SET (0): Set to offset bytes
///   - SEEK_CUR (1): Set to current + offset bytes  
///   - SEEK_END (2): Set to size + offset bytes
///
/// Implementation notes:
/// - For simplicity, we allow seeking beyond file size (needed for future writes)
/// - Negative offsets are converted from i64 and validated
/// - File size is estimated as u32::MAX for simplicity (actual size requires
///   parsing filesystem metadata which is complex)
fn sys_lseek(fd: u64, offset: i64, whence: u64) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.lseek_calls += 1;
    drop(stats);
    
    serial::serial_print("[SYSCALL] lseek(FD=");
    serial::serial_print_dec(fd);
    serial::serial_print(", offset=");
    serial::serial_print_dec(offset as u64);
    serial::serial_print(", whence=");
    serial::serial_print_dec(whence);
    serial::serial_print(")\n");
    
    // Don't allow seeking on stdio
    if fd < 3 {
        serial::serial_print("[SYSCALL] lseek() - cannot seek on stdio\n");
        return u64::MAX;
    }
    
    // Get current process ID
    let pid = match current_process_id() {
        Some(p) => p,
        None => {
            serial::serial_print("[SYSCALL] lseek() - no current process\n");
            return u64::MAX;
        }
    };
    
    // Get file descriptor entry (just to check it's current offset)
    let current_offset = match crate::fd::fd_get(pid, fd as usize) {
        Some(entry) => entry.offset,
        None => {
            serial::serial_print("[SYSCALL] lseek() - invalid FD\n");
            return u64::MAX;
        }
    };
    
    // Calculate new offset based on whence
    let new_offset = match whence {
        SEEK_SET => {
            // Absolute positioning
            if offset < 0 {
                serial::serial_print("[SYSCALL] lseek() - negative offset with SEEK_SET\n");
                return u64::MAX;
            }
            offset as u64
        }
        SEEK_CUR => {
            // Relative to current position
            let current = current_offset as i64;
            let result = current + offset;
            if result < 0 {
                serial::serial_print("[SYSCALL] lseek() - result offset is negative\n");
                return u64::MAX;
            }
            result as u64
        }
        SEEK_END => {
            // Relative to end of file
            // For simplicity, we don't have an easy way to get file size
            // without reading the full inode metadata. For now, we'll
            // treat SEEK_END as an error.
            // TODO: Implement file size retrieval from inode
            serial::serial_print("[SYSCALL] lseek() - SEEK_END not yet implemented\n");
            return u64::MAX;
        }
        _ => {
            serial::serial_print("[SYSCALL] lseek() - invalid whence value\n");
            return u64::MAX;
        }
    };
    
    // Update the file offset
    if crate::fd::fd_update_offset(pid, fd as usize, new_offset) {
        serial::serial_print("[SYSCALL] lseek() - new offset: ");
        serial::serial_print_dec(new_offset);
        serial::serial_print("\n");
        new_offset
    } else {
        serial::serial_print("[SYSCALL] lseek() - failed to update offset\n");
        u64::MAX
    }
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

/// Inicializar sistema de syscalls
pub fn init() {
    serial::serial_print("Syscall system initialized\n");
}
