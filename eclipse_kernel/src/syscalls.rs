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
    // CAPTURE USER CONTEXT
    // Update the current process context with the User register state from the interrupt frame
    // Frame structure relative to frame_ptr (RBP):
    // [32] RSP
    // [8]  RIP
    // [-8]  RAX
    // [-16] RBX
    // [-24] RCX
    // [-32] RDX
    // [-40] RSI
    // [-48] RDI
    // [-56] R8
    // [-64] R9
    // [-72] R10
    // [-80] R11
    // [-88] R12
    // [-96] R13
    // [-104] R14
    // [-112] R15
    unsafe {
        unsafe fn read_stack(rbp: u64, offset: isize) -> u64 {
            let ptr = (rbp as isize + offset) as *const u64;
            *ptr
        }
    
        // Read registers
        let user_rsp = read_stack(frame_ptr, 32);
        let user_rip = read_stack(frame_ptr, 8);
        
        let user_rax = read_stack(frame_ptr, -8);
        
        /* 
        crate::serial::serial_print("SYSCALL: frame_ptr=");
        crate::serial::serial_print_hex(frame_ptr);
        crate::serial::serial_print(" user_rax=");
        crate::serial::serial_print_hex(user_rax);
        crate::serial::serial_print(" user_rip=");
        crate::serial::serial_print_hex(user_rip);
        crate::serial::serial_print("\n");
        */
        let user_rbx = read_stack(frame_ptr, -16);
        let user_rcx = read_stack(frame_ptr, -24);
        let user_rdx = read_stack(frame_ptr, -32);
        let user_rsi = read_stack(frame_ptr, -40);
        let user_rdi = read_stack(frame_ptr, -48);
        let user_r8  = read_stack(frame_ptr, -56);
        let user_r9  = read_stack(frame_ptr, -64);
        let user_r10 = read_stack(frame_ptr, -72);
        let user_r11 = read_stack(frame_ptr, -80);
        let user_r12 = read_stack(frame_ptr, -88);
        let user_r13 = read_stack(frame_ptr, -96);
        let user_r14 = read_stack(frame_ptr, -104);
        let user_r15 = read_stack(frame_ptr, -112);
        
        // Note: RBP is frame_ptr itself (value at [rbp] is saved RBP)
        let user_rbp = read_stack(frame_ptr, 0); 
        
        let stats = SYSCALL_STATS.lock();
        drop(stats);

        if let Some(pid) = current_process_id() {
             use crate::process::{update_process, get_process};
             if let Some(mut process) = get_process(pid) {
                 process.context.rsp = user_rsp;
                 process.context.rip = user_rip;
                 // DON'T update RAX here - it will be updated after the syscall completes
                 // process.context.rax = user_rax;
                 process.context.rbx = user_rbx;
                 process.context.rcx = user_rcx;
                 process.context.rdx = user_rdx;
                 process.context.rsi = user_rsi;
                 process.context.rdi = user_rdi;
                 process.context.rbp = user_rbp;
                 process.context.r8  = user_r8;
                 process.context.r9  = user_r9;
                 process.context.r10 = user_r10;
                 process.context.r11 = user_r11;
                 process.context.r12 = user_r12;
                 process.context.r13 = user_r13;
                 process.context.r14 = user_r14;
                 process.context.r15 = user_r15;
                 
                 update_process(pid, process);
             }
        }
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
        7 => sys_fork(),
        8 => sys_exec(arg1, arg2),
        9 => sys_wait(arg1),
        10 => sys_get_service_binary(arg1, arg2, arg3),
        11 => sys_open(arg1, arg2, arg3),
        12 => sys_close(arg1),
        13 => sys_getppid(),
        _ => {
            serial::serial_print("Unknown syscall: ");
            serial::serial_print_hex(syscall_num);
            serial::serial_print("\n");
            u64::MAX
        }
    };

    // DEBUG Trace return
    /* 
    if syscall_num == 5 { 
        if let Some(pid) = crate::process::current_process_id() {
            crate::serial::serial_print("YRet[PID=");
            crate::serial::serial_print_dec(pid as u64);
            crate::serial::serial_print(" RIP=");
            unsafe {
                 let rip_val = *( (frame_ptr + 8) as *const u64 );
                 let cs_val = *( (frame_ptr + 16) as *const u64 );
                 let rflags_val = *( (frame_ptr + 24) as *const u64 );
                 crate::serial::serial_print_hex(rip_val);
                 crate::serial::serial_print(" CS=");
                 crate::serial::serial_print_hex(cs_val);
                 crate::serial::serial_print(" FL=");
                 crate::serial::serial_print_hex(rflags_val);
            }
            crate::serial::serial_print("] ");
        }
    }
    */
    // if syscall_num == 4 { crate::serial::serial_print("RRet "); }
    
    // CRITICAL FIX: Update the calling process's RAX with the return value
    // This must happen AFTER the syscall executes, not before
    // Otherwise, if the process gets context-switched during the syscall,
    // it will be restored with the wrong RAX value
    if let Some(pid) = current_process_id() {
        use crate::process::{update_process, get_process};
        if let Some(mut process) = get_process(pid) {
            process.context.rax = ret;
            update_process(pid, process);
            
            // CRITICAL FIX FOR CONTEXT SWITCH BUG:
            // If a context switch occurred during this syscall (e.g., in yield_cpu or send/receive),
            // the GP registers were restored from the PCB by switch_context().
            // However, the values on the kernel stack (pushed by syscall_int80) are STALE.
            // We MUST write the updated PCB register values back to the stack frame
            // so that when syscall_int80 pops them, it restores the correct context.
            unsafe {
                unsafe fn write_stack(rbp: u64, offset: isize, value: u64) {
                    let ptr = (rbp as isize + offset) as *mut u64;
                    *ptr = value;
                }
                
                // Write updated register values back to the stack frame
                // This ensures syscall_int80's pop instructions restore the correct values
                write_stack(frame_ptr, -8, process.context.rax);   // RAX
                write_stack(frame_ptr, -16, process.context.rbx);  // RBX
                write_stack(frame_ptr, -24, process.context.rcx);  // RCX
                write_stack(frame_ptr, -32, process.context.rdx);  // RDX
                write_stack(frame_ptr, -40, process.context.rsi);  // RSI
                write_stack(frame_ptr, -48, process.context.rdi);  // RDI
                write_stack(frame_ptr, -56, process.context.r8);   // R8
                write_stack(frame_ptr, -64, process.context.r9);   // R9
                write_stack(frame_ptr, -72, process.context.r10);  // R10
                write_stack(frame_ptr, -80, process.context.r11);  // R11
                write_stack(frame_ptr, -88, process.context.r12);  // R12
                write_stack(frame_ptr, -96, process.context.r13);  // R13
                write_stack(frame_ptr, -104, process.context.r14); // R14
                write_stack(frame_ptr, -112, process.context.r15); // R15
                write_stack(frame_ptr, 0, process.context.rbp);    // RBP
                // Note: RSP and RIP are in the IRETQ frame and will be restored by IRETQ
                // They don't need to be written back to the GP register save area
            }
        }
    }
    
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

/// sys_write - Escribir a un file descriptor
fn sys_write(fd: u64, buf_ptr: u64, len: u64) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.write_calls += 1;
    drop(stats);
    
    // Handle stdout/stderr (1, 2) - write to serial
    if fd == 1 || fd == 2 {
        if buf_ptr != 0 && len > 0 && len < 4096 {
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
    }
    // Handle file descriptor 3 (/var/log/system.log) - write to in-kernel buffer
    else if fd == 3 {
        if buf_ptr != 0 && len > 0 && len < 4096 {
            unsafe {
                let slice = core::slice::from_raw_parts(buf_ptr as *const u8, len as usize);
                // For now, also write to serial to show it's working
                serial::serial_print("[LOGFILE] ");
                if let Ok(s) = core::str::from_utf8(slice) {
                    serial::serial_print(s);
                } else {
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
    }
    0
}

/// sys_read - Leer de un file descriptor (IMPLEMENTADO)
fn sys_read(fd: u64, buf_ptr: u64, len: u64) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.read_calls += 1;
    drop(stats);
    
    // Validar parámetros
    if buf_ptr == 0 || len == 0 || len > 4096 {
        return u64::MAX; // Error
    }
    
    // Por ahora, solo soportamos lectura desde stdin (fd=0)
    if fd == 0 {
        // TODO: Implementar buffer de input real
        // Por ahora retornar 0 (EOF)
        return 0;
    }
    
    u64::MAX // Error - fd no soportado
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
fn sys_fork() -> u64 {
    use crate::process;
    
    let mut stats = SYSCALL_STATS.lock();
    stats.fork_calls += 1;
    drop(stats);
    
    let current_pid = process::current_process_id().unwrap_or(0);
    serial::serial_print("[SYSCALL] fork() called from PID ");
    serial::serial_print_dec(current_pid as u64);
    serial::serial_print("\n");
    
    // Create child process
    match process::fork_process() {
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
    
    serial::serial_print("[SYSCALL] wait() called\n");
    
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
    
    // No terminated children found
    serial::serial_print("[SYSCALL] wait() - no terminated children\n");
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
    
    // For now, we support a very simple file system simulation
    // Return a fake file descriptor for /var/log/system.log
    if path == "/var/log/system.log" {
        serial::serial_print("[SYSCALL] open() - returning FD 3 for log file\n");
        3 // Return FD 3 for log file
    } else {
        serial::serial_print("[SYSCALL] open() - file not found\n");
        u64::MAX // File not found
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
    
    // For now, just validate the FD
    if fd >= 3 && fd < 1024 {
        serial::serial_print("[SYSCALL] close() - success\n");
        0 // Success
    } else {
        serial::serial_print("[SYSCALL] close() - invalid FD\n");
        u64::MAX // Error
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
    }
}

/// Inicializar sistema de syscalls
pub fn init() {
    serial::serial_print("Syscall system initialized\n");
}
