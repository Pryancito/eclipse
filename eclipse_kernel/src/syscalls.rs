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
});

/// Handler principal de syscalls
pub extern "C" fn syscall_handler(
    syscall_num: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    _arg4: u64,
    _arg5: u64,
) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.total_calls += 1;
    drop(stats);
    
    // DEBUG: Trace all syscalls
    // serial::serial_print("SYSCALL: ");
    // serial::serial_print_dec(syscall_num);
    // serial::serial_print("\n");

    match syscall_num {
        0 => sys_exit(arg1),
        1 => sys_write(arg1, arg2, arg3),
        2 => sys_read(arg1, arg2, arg3),
        3 => sys_send(arg1, arg2, arg3),
        4 => sys_receive(arg1, arg2),
        5 => sys_yield(),
        6 => sys_getpid(),
        7 => sys_fork(),
        8 => sys_exec(arg1, arg2),
        9 => sys_wait(arg1),
        10 => sys_get_service_binary(arg1, arg2, arg3),
        _ => {
            serial::serial_print("Unknown syscall: ");
            serial::serial_print_hex(syscall_num);
            serial::serial_print("\n");
            u64::MAX
        }
    }
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

/// sys_receive - Recibir mensaje IPC (IMPLEMENTADO)
fn sys_receive(buffer_ptr: u64, size: u64) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.receive_calls += 1;
    drop(stats);
    
    // Validar parámetros
    if buffer_ptr == 0 || size == 0 || size > 4096 {
        return u64::MAX; // Error
    }
    
    if let Some(client_id) = current_process_id() {
        // Intentar recibir mensaje
        if let Some(msg) = receive_message(client_id) {
            // Copiar mensaje a buffer de usuario
            unsafe {
                let user_buf = core::slice::from_raw_parts_mut(
                    buffer_ptr as *mut u8,
                    core::cmp::min(size as usize, 32)
                );
                user_buf.copy_from_slice(&msg.data[..core::cmp::min(size as usize, 32)]);
            }
            return msg.data.len() as u64;
        }
    }
    
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

/// sys_fork - Create a new process (child)
/// Returns: Child PID in parent, 0 in child, -1 on error
fn sys_fork() -> u64 {
    use crate::process;
    
    let mut stats = SYSCALL_STATS.lock();
    stats.fork_calls += 1;
    drop(stats);
    
    serial::serial_print("[SYSCALL] fork() called\n");
    
    // Create child process
    match process::fork_process() {
        Some(child_pid) => {
            serial::serial_print("[SYSCALL] fork() created child process with PID: ");
            serial::serial_print_dec(child_pid as u64);
            serial::serial_print("\n");
            
            // Add child to scheduler
            crate::scheduler::enqueue_process(child_pid);
            
            // Return child PID to parent
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
            // Use standard userspace stack top (96MB + 64KB)
            let stack_top: u64 = 0x6010000;
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
fn sys_get_service_binary(service_id: u64, out_ptr: u64, out_size: u64) -> u64 {
    serial::serial_print("[SYSCALL] get_service_binary(");
    serial::serial_print_dec(service_id);
    serial::serial_print(")\n");
    
    // Validate pointers
    if out_ptr == 0 || out_size == 0 {
        return u64::MAX;
    }
    
    // Get service binary based on ID
    let (bin_ptr, bin_size) = match service_id {
        0 => (crate::binaries::FILESYSTEM_SERVICE_BINARY.as_ptr() as u64, crate::binaries::FILESYSTEM_SERVICE_BINARY.len() as u64),
        1 => (crate::binaries::NETWORK_SERVICE_BINARY.as_ptr() as u64, crate::binaries::NETWORK_SERVICE_BINARY.len() as u64),
        2 => (crate::binaries::DISPLAY_SERVICE_BINARY.as_ptr() as u64, crate::binaries::DISPLAY_SERVICE_BINARY.len() as u64),
        3 => (crate::binaries::AUDIO_SERVICE_BINARY.as_ptr() as u64, crate::binaries::AUDIO_SERVICE_BINARY.len() as u64),
        4 => (crate::binaries::INPUT_SERVICE_BINARY.as_ptr() as u64, crate::binaries::INPUT_SERVICE_BINARY.len() as u64),
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
    }
}

/// Inicializar sistema de syscalls
pub fn init() {
    serial::serial_print("Syscall system initialized\n");
}
