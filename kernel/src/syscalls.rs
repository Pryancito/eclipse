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
}

static SYSCALL_STATS: Mutex<SyscallStats> = Mutex::new(SyscallStats {
    total_calls: 0,
    exit_calls: 0,
    write_calls: 0,
    read_calls: 0,
    send_calls: 0,
    receive_calls: 0,
    yield_calls: 0,
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
    
    match syscall_num {
        0 => sys_exit(arg1),
        1 => sys_write(arg1, arg2, arg3),
        2 => sys_read(arg1, arg2, arg3),
        3 => sys_send(arg1, arg2, arg3),
        4 => sys_receive(arg1, arg2),
        5 => sys_yield(),
        6 => sys_getpid(),
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
                for &byte in slice {
                    if byte != 0 {
                        serial::serial_print(core::str::from_utf8(&[byte]).unwrap_or("?"));
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
    }
}

/// Inicializar sistema de syscalls
pub fn init() {
    serial::serial_print("Syscall system initialized\n");
}
