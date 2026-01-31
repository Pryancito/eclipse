//! Servidores del sistema ejecutándose en el microkernel

use crate::ipc::{MessageType, register_server, ServerId, receive_message};
use crate::process::{create_process, ProcessId};
use crate::serial;
use crate::scheduler::yield_cpu;

/// IDs de los servidores del sistema
pub struct SystemServers {
    pub filesystem: Option<ServerId>,
    pub graphics: Option<ServerId>,
    pub network: Option<ServerId>,
}

static mut SYSTEM_SERVERS: SystemServers = SystemServers {
    filesystem: None,
    graphics: None,
    network: None,
};

/// Inicializar todos los servidores del sistema
pub fn init_servers() {
    serial::serial_print("Initializing system servers...\n");
    
    // Registrar servidor de FileSystem
    if let Some(fs_id) = register_server(b"FileSystem", MessageType::FileSystem, 10) {
        unsafe {
            SYSTEM_SERVERS.filesystem = Some(fs_id);
        }
        serial::serial_print("FileSystem server registered with ID: ");
        serial::serial_print_dec(fs_id as u64);
        serial::serial_print("\n");
        
        if let Some(_pid) = create_process(filesystem_server as *const () as u64, 0x500000, 0x10000) {
            serial::serial_print("FileSystem server process created\n");
        }
    }
    
    // Registrar servidor de Graphics
    if let Some(gfx_id) = register_server(b"Graphics", MessageType::Graphics, 10) {
        unsafe {
            SYSTEM_SERVERS.graphics = Some(gfx_id);
        }
        serial::serial_print("Graphics server registered with ID: ");
        serial::serial_print_dec(gfx_id as u64);
        serial::serial_print("\n");
        
        if let Some(_pid) = create_process(graphics_server as *const () as u64, 0x600000, 0x10000) {
            serial::serial_print("Graphics server process created\n");
        }
    }
    
    // Registrar servidor de Network
    if let Some(net_id) = register_server(b"Network", MessageType::Network, 5) {
        unsafe {
            SYSTEM_SERVERS.network = Some(net_id);
        }
        serial::serial_print("Network server registered with ID: ");
        serial::serial_print_dec(net_id as u64);
        serial::serial_print("\n");
    }
    
    serial::serial_print("System servers initialized\n");
}

pub fn get_filesystem_server() -> Option<ServerId> {
    unsafe { SYSTEM_SERVERS.filesystem }
}

pub fn get_graphics_server() -> Option<ServerId> {
    unsafe { SYSTEM_SERVERS.graphics }
}

pub fn get_network_server() -> Option<ServerId> {
    unsafe { SYSTEM_SERVERS.network }
}

// ============================================================================
// FileSystem Server
// ============================================================================

extern "C" fn filesystem_server() -> ! {
    serial::serial_print("FileSystem server started\n");
    
    loop {
        // Procesar mensajes IPC
        if let Some(server_id) = get_filesystem_server() {
            if let Some(msg) = receive_message(server_id) {
                handle_filesystem_message(&msg);
            }
        }
        
        yield_cpu();
    }
}

/// Handler de mensajes del filesystem
fn handle_filesystem_message(msg: &crate::ipc::Message) {
    serial::serial_print("FS: Received message type ");
    serial::serial_print_hex(msg.msg_type as u64);
    serial::serial_print("\n");
    
    // TODO: Implementar operaciones de filesystem
    // - OPEN: abrir archivo
    // - READ: leer archivo
    // - WRITE: escribir archivo
    // - CLOSE: cerrar archivo
    // - STAT: información de archivo
}

// ============================================================================
// Graphics Server
// ============================================================================

extern "C" fn graphics_server() -> ! {
    serial::serial_print("Graphics server started\n");
    
    // TODO: Obtener framebuffer info del kernel
    
    loop {
        if let Some(server_id) = get_graphics_server() {
            if let Some(msg) = receive_message(server_id) {
                handle_graphics_message(&msg);
            }
        }
        
        yield_cpu();
    }
}

/// Handler de mensajes de graphics
fn handle_graphics_message(msg: &crate::ipc::Message) {
    serial::serial_print("GFX: Received message type ");
    serial::serial_print_hex(msg.msg_type as u64);
    serial::serial_print("\n");
    
    // TODO: Implementar operaciones de graphics
    // - DRAW_PIXEL: dibujar pixel
    // - DRAW_RECT: dibujar rectángulo
    // - DRAW_LINE: dibujar línea
    // - FILL: rellenar área
    // - BLIT: copiar buffer
}
