//! Servidores del sistema ejecutándose en el microkernel

use crate::ipc::{MessageType, register_server, ServerId, receive_message};
use crate::process::{create_process, ProcessId};
use crate::serial;
use crate::scheduler::yield_cpu;
use crate::scheme::{Scheme, Stat, error as scheme_error};
use alloc::boxed::Box;

/// Framebuffer information from bootloader
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    pub address: u64,      // Physical address of framebuffer
    pub width: u32,        // Width in pixels
    pub height: u32,       // Height in pixels
    pub pitch: u32,        // Bytes per scanline
    pub bpp: u16,          // Bits per pixel
    pub red_mask_size: u8,
    pub red_mask_shift: u8,
    pub green_mask_size: u8,
    pub green_mask_shift: u8,
    pub blue_mask_size: u8,
    pub blue_mask_shift: u8,
}

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
        
        /*
        if let Some(_pid) = create_process(filesystem_server as *const () as u64, 0x500000, 0x10000) {
            serial::serial_print("FileSystem server process created\n");
        }
        */
    }
    
    // Registrar servidor de Graphics
    if let Some(gfx_id) = register_server(b"Graphics", MessageType::Graphics, 10) {
        unsafe {
            SYSTEM_SERVERS.graphics = Some(gfx_id);
        }
        serial::serial_print("Graphics server registered with ID: ");
        serial::serial_print_dec(gfx_id as u64);
        serial::serial_print("\n");
        
        /*
        if let Some(_pid) = create_process(graphics_server as *const () as u64, 0x600000, 0x10000) {
            serial::serial_print("Graphics server process created\n");
        }
        */
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
    
    // Get framebuffer info from kernel (syscall 15)
    let fb_info_ptr = unsafe {
        let result: u64;
        core::arch::asm!(
            "mov rax, 15",  // SYS_GET_FRAMEBUFFER_INFO
            "syscall",
            out("rax") result,
            out("rcx") _,
            out("r11") _,
        );
        result
    };
    
    if fb_info_ptr != 0 {
        // Parse framebuffer info structure
        let fb_info = unsafe { &*(fb_info_ptr as *const FramebufferInfo) };
        
        serial::serial_print("Graphics: Framebuffer initialized\n");
        serial::serial_print("  Address: ");
        serial::serial_print_hex(fb_info.address);
        serial::serial_print("\n  Resolution: ");
        serial::serial_print_dec(fb_info.width as u64);
        serial::serial_print("x");
        serial::serial_print_dec(fb_info.height as u64);
        serial::serial_print("\n  Pitch: ");
        serial::serial_print_dec(fb_info.pitch as u64);
        serial::serial_print("\n  BPP: ");
        serial::serial_print_dec(fb_info.bpp as u64);
        serial::serial_print("\n");
        
        // TODO: Map framebuffer into process address space
        // TODO: Initialize graphics operations
    } else {
        serial::serial_print("Graphics: No framebuffer available\n");
    }
    
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

// --- Display Scheme ---

pub struct DisplayScheme;

impl Scheme for DisplayScheme {
    fn open(&self, _path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        Ok(0) // Single display resource
    }

    fn write(&self, _id: usize, buf: &[u8]) -> Result<usize, usize> {
        let fb_info = &crate::boot::get_boot_info().framebuffer;
        if fb_info.base_address == 0 {
            return Err(scheme_error::EIO);
        }

        // Direct write to framebuffer (not optimized, but consistent)
        let fb_ptr = (crate::memory::PHYS_MEM_OFFSET + fb_info.base_address) as *mut u8;
        let fb_size = (fb_info.pixels_per_scan_line * fb_info.height * 4) as usize; // Assuming 32bpp
        
        let to_copy = buf.len().min(fb_size);
        unsafe {
            core::ptr::copy_nonoverlapping(buf.as_ptr(), fb_ptr, to_copy);
        }
        
        Ok(to_copy)
    }

    fn read(&self, _id: usize, _buffer: &mut [u8]) -> Result<usize, usize> {
        Err(scheme_error::EIO)
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize) -> Result<usize, usize> {
        Ok(0) // Simplified seeker
    }

    fn close(&self, _id: usize) -> Result<usize, usize> {
        Ok(0)
    }

    fn fstat(&self, _id: usize, _stat: &mut Stat) -> Result<usize, usize> {
        let fb_info = &crate::boot::get_boot_info().framebuffer;
        _stat.size = (fb_info.pixels_per_scan_line * fb_info.height * 4) as u64;
        Ok(0)
    }

    fn fmap(&self, _id: usize, _offset: usize, _len: usize) -> Result<usize, usize> {
        let fb_info = &crate::boot::get_boot_info().framebuffer;
        if fb_info.base_address == 0 {
            return Err(scheme_error::EIO);
        }
        // Return physical address
        Ok(fb_info.base_address as usize)
    }
}

// --- Input Scheme ---

pub struct InputScheme;

impl Scheme for InputScheme {
    fn open(&self, _path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        Ok(0)
    }

    fn read(&self, _id: usize, _buffer: &mut [u8]) -> Result<usize, usize> {
        Ok(0) // Will be populated with input events
    }

    fn write(&self, _id: usize, buf: &[u8]) -> Result<usize, usize> {
        Ok(buf.len())
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize) -> Result<usize, usize> {
        Ok(0)
    }

    fn close(&self, _id: usize) -> Result<usize, usize> {
        Ok(0)
    }

    fn fstat(&self, _id: usize, _stat: &mut Stat) -> Result<usize, usize> {
        Ok(0)
    }
}

// --- Audio Scheme ---

pub struct AudioScheme;

impl Scheme for AudioScheme {
    fn open(&self, _path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        Ok(0)
    }

    fn read(&self, _id: usize, _buffer: &mut [u8]) -> Result<usize, usize> {
        Ok(0)
    }

    fn write(&self, _id: usize, buf: &[u8]) -> Result<usize, usize> {
        Ok(buf.len())
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize) -> Result<usize, usize> {
        Ok(0)
    }

    fn close(&self, _id: usize) -> Result<usize, usize> {
        Ok(0)
    }

    fn fstat(&self, _id: usize, _stat: &mut Stat) -> Result<usize, usize> {
        Ok(0)
    }
}

// --- Network Scheme ---

pub struct NetworkScheme;

impl Scheme for NetworkScheme {
    fn open(&self, _path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        Ok(0)
    }

    fn read(&self, _id: usize, _buffer: &mut [u8]) -> Result<usize, usize> {
        Ok(0)
    }

    fn write(&self, _id: usize, buf: &[u8]) -> Result<usize, usize> {
        Ok(buf.len())
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize) -> Result<usize, usize> {
        Ok(0)
    }

    fn close(&self, _id: usize) -> Result<usize, usize> {
        Ok(0)
    }

    fn fstat(&self, _id: usize, _stat: &mut Stat) -> Result<usize, usize> {
        Ok(0)
    }
}

pub fn init() {
    init_servers();
    crate::scheme::register_scheme("display", Box::new(DisplayScheme));
    crate::scheme::register_scheme("input", Box::new(InputScheme));
    crate::scheme::register_scheme("snd", Box::new(AudioScheme));
    crate::scheme::register_scheme("net", Box::new(NetworkScheme));
}
