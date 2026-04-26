use core::sync::atomic::Ordering;
use crate::process::current_process_id;
use crate::ipc::{MessageType, receive_message};
use super::{copy_from_user, copy_to_user, is_user_pointer, RECV_OK, RECV_EMPTY};
use spin::Mutex;

/// Estadísticas de syscalls IPC
pub struct SyscallStats {
    pub send_calls: u64,
    pub receive_calls: u64,
}

pub static SYSCALL_STATS: Mutex<SyscallStats> = Mutex::new(SyscallStats {
    send_calls: 0,
    receive_calls: 0,
});

/// sys_send - Enviar mensaje IPC
/// arg4 = data_len (bytes to copy from data_ptr; max 512)
pub fn sys_send(server_id: u64, msg_type: u64, data_ptr: u64, data_len: u64) -> u64 {
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
            if !copy_from_user(data_ptr, &mut data[..len]) {
                return u64::MAX;
            }
        }
        
        if len > 512 {
            crate::serial::serial_printf(format_args!("[IPC-SEND] ERROR: len {} exceeds max\n", len));
            return u64::MAX;
        }

        crate::serial::serial_printf(format_args!(
            "[IPC-SEND] from={} to={} type={:?} data_len={}\n",
            client_id, server_id, message_type, len
        ));

        if crate::ipc::send_message(client_id, server_id as u32, message_type, &data[..len]) {
            return 0; // Success
        }
    }
    
    u64::MAX // Error
}

/// sys_receive - Recibir mensaje IPC
pub fn sys_receive(buffer_ptr: u64, size: u64, sender_pid_ptr: u64) -> u64 {
    let mut stats = SYSCALL_STATS.lock();
    stats.receive_calls += 1;
    drop(stats);
    
    // Rechazar punteros en página nula (evita crash 0x11 por punteros corruptos)
    if (buffer_ptr != 0 && buffer_ptr < 0x1000) || (sender_pid_ptr != 0 && sender_pid_ptr < 0x1000) {
        return u64::MAX;
    }
    if size > 4096 {
        return u64::MAX;
    }
    if buffer_ptr != 0 && !is_user_pointer(buffer_ptr, size) {
        return u64::MAX;
    }
    if sender_pid_ptr != 0 && !is_user_pointer(sender_pid_ptr, 8) {
        return u64::MAX;
    }
    
    if let Some(client_id) = current_process_id() {
        if let Some(msg) = receive_message(client_id) {
            RECV_OK.fetch_add(1, Ordering::Relaxed);
            crate::serial::serial_printf(format_args!(
                "[IPC-RECV] pid={} got msg data_size={} from={} type={:?}\n",
                client_id, msg.data_size, msg.from, msg.msg_type
            ));
            // Calcular cuántos bytes copiar al buffer del usuario
            let data_size = msg.data_size as usize;
            if data_size > 512 {
                crate::serial::serial_printf(format_args!("[IPC-RECV] ERROR: data_size {} corrupted\n", data_size));
                return u64::MAX;
            }
            let data_len = data_size.min(msg.data.len());
            let copy_len = core::cmp::min(size as usize, data_len);

            if copy_len > 0 && buffer_ptr != 0 {
                // Pre-check if copy_len is huge (e.g. -38)
                if copy_len > 1024 * 1024 { // 1MB threshold for insanity
                    crate::serial::serial_printf(format_args!("[IPC-RECV] ERROR: insane copy_len {}\n", copy_len));
                    return u64::MAX;
                }
                if !copy_to_user(buffer_ptr, &msg.data[..copy_len]) {
                    return u64::MAX;
                }
            }

            // Escribir el PID del remitente si se solicitó
            if sender_pid_ptr != 0 {
                let b = (msg.from as u64).to_le_bytes();
                if !copy_to_user(sender_pid_ptr, &b[..4]) {
                    return u64::MAX;
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

pub fn sys_receive_fast(ctx: &mut crate::interrupts::SyscallContext) -> u64 {
    let pid = match current_process_id() {
        Some(p) => p,
        None => return 0,
    };

    // Camino rápido al estilo del código antiguo: solo mensajes pequeños (≤24 B),
    // entregados directamente en registros.
    if let Some((data_size, from, data)) = crate::ipc::pop_small_message_24(pid) {
        RECV_OK.fetch_add(1, Ordering::Relaxed);
        let mut w = [0u64; 3];
        for i in 0..3 {
            let off = i * 8;
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&data[off..off + 8]);
            w[i] = u64::from_le_bytes(buf);
        }
        ctx.rdi = w[0];
        ctx.rsi = w[1];
        ctx.rdx = w[2];
        ctx.rcx = from as u64;
        return data_size as u64;
    }

    RECV_EMPTY.fetch_add(1, Ordering::Relaxed);
    0
}
