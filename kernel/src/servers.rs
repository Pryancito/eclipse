//! Servidores del sistema ejecutándose en el microkernel
//! 
//! Define e inicializa servidores básicos para FileSystem, Graphics, etc.

use crate::ipc::{MessageType, register_server, ServerId};
use crate::process::{create_process, ProcessId};
use crate::serial;

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
        
        // Crear proceso para el servidor de filesystem
        // TODO: Cargar desde binario, por ahora usamos función stub
        if let Some(_pid) = create_process(filesystem_server as u64, 0x500000, 0x10000) {
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
        
        // Crear proceso para el servidor de graphics
        if let Some(_pid) = create_process(graphics_server as u64, 0x600000, 0x10000) {
            serial::serial_print("Graphics server process created\n");
        }
    }
    
    // Registrar servidor de Network (futuro)
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

/// Obtener ID del servidor de FileSystem
pub fn get_filesystem_server() -> Option<ServerId> {
    unsafe { SYSTEM_SERVERS.filesystem }
}

/// Obtener ID del servidor de Graphics
pub fn get_graphics_server() -> Option<ServerId> {
    unsafe { SYSTEM_SERVERS.graphics }
}

/// Obtener ID del servidor de Network
pub fn get_network_server() -> Option<ServerId> {
    unsafe { SYSTEM_SERVERS.network }
}

// ============================================================================
// Implementaciones de servidores (stubs)
// ============================================================================

/// Servidor de FileSystem (stub)
extern "C" fn filesystem_server() -> ! {
    serial::serial_print("FileSystem server started\n");
    
    loop {
        // TODO: Procesar mensajes IPC de filesystem
        // Por ahora solo yield
        crate::scheduler::yield_cpu();
    }
}

/// Servidor de Graphics (stub)
extern "C" fn graphics_server() -> ! {
    serial::serial_print("Graphics server started\n");
    
    loop {
        // TODO: Procesar mensajes IPC de graphics
        // Manejar framebuffer, dibujo, etc.
        crate::scheduler::yield_cpu();
    }
}
