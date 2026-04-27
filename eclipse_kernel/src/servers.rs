//! Servidores del sistema ejecutándose en el microkernel

use crate::ipc::{MessageType, register_server, ServerId, receive_message};
use crate::serial;
use crate::scheme::{Scheme, Stat, error as scheme_error};
use crate::net::*;

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

    // Registrar servidor de FileSystem
    if let Some(fs_id) = register_server(b"FileSystem", MessageType::FileSystem, 10) {
        unsafe {
            SYSTEM_SERVERS.filesystem = Some(fs_id);
        }
        
        /*
        if let Some(_pid) = create_process(filesystem_server as *const () as u64, 0x500000, 0x10000, 0, 0) {
            serial::serial_print("FileSystem server process created\n");
        }
        */
    }
    
    // Registrar servidor de Graphics
    if let Some(gfx_id) = register_server(b"Graphics", MessageType::Graphics, 10) {
        unsafe {
            SYSTEM_SERVERS.graphics = Some(gfx_id);
        }
        
        /*
        if let Some(_pid) = create_process(graphics_server as *const () as u64, 0x600000, 0x10000, 0, 0) {
            serial::serial_print("Graphics server process created\n");
        }
        */
    }
    
    // Registrar servidor de Network
    if let Some(net_id) = register_server(b"Network", MessageType::Network, 5) {
        unsafe {
            SYSTEM_SERVERS.network = Some(net_id);
        }
    }
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
    
    loop {
        // Procesar mensajes IPC
        if let Some(server_id) = get_filesystem_server() {
            if let Some(msg) = receive_message(server_id) {
                handle_filesystem_message(&msg);
            }
        }
        
        crate::cpu::idle(10);
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
    
    // Get framebuffer info from kernel (syscall 503)
    let fb_info_ptr = unsafe {
        let result: u64;
        core::arch::asm!(
            "mov rax, 503",  // SYS_GET_FRAMEBUFFER_INFO
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
        
        crate::cpu::idle(10);
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

    fn write(&self, _id: usize, buf: &[u8], _offset: u64) -> Result<usize, usize> {
        let (fb_phys, fb_size) = if crate::boot::get_boot_info().framebuffer.base_address != 0
            && crate::boot::get_boot_info().framebuffer.base_address != 0xDEADBEEF
        {
            let fi = &crate::boot::get_boot_info().framebuffer;
            (fi.base_address, (fi.pixels_per_scan_line * fi.height * 4) as usize)
        } else if let Some((phys, _w, _h, _pitch, size)) = crate::virtio::get_primary_virtio_display() {
            (phys, size)
        } else if let Some((phys, _bar_phys, _w, h, pitch)) = crate::nvidia::get_nvidia_fb_info() {
            (phys, (pitch * h) as usize)
        } else {
            return Err(scheme_error::EIO);
        };

        let fb_ptr = (crate::memory::PHYS_MEM_OFFSET + fb_phys) as *mut u8;
        let to_copy = buf.len().min(fb_size);
        unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), fb_ptr, to_copy); }
        
        // Ensure changes are visible on VirtIO-GPU
        crate::virtio::gpu_flush_primary();
        
        Ok(to_copy)
    }

    fn read(&self, _id: usize, _buffer: &mut [u8], _offset: u64) -> Result<usize, usize> {
        Err(scheme_error::EIO)
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize, _current_offset: u64) -> Result<usize, usize> {
        Ok(0)
    }

    fn close(&self, _id: usize) -> Result<usize, usize> {
        Ok(0)
    }

    fn fstat(&self, _id: usize, _stat: &mut Stat) -> Result<usize, usize> {
        if crate::boot::get_boot_info().framebuffer.base_address != 0
            && crate::boot::get_boot_info().framebuffer.base_address != 0xDEADBEEF
        {
            let fi = &crate::boot::get_boot_info().framebuffer;
            _stat.size = (fi.pixels_per_scan_line * fi.height * 4) as u64;
        } else if let Some((_phys, _w, _h, _pitch, size)) = crate::virtio::get_primary_virtio_display() {
            _stat.size = size as u64;
        } else if let Some((_phys, _bar_phys, _w, h, pitch)) = crate::nvidia::get_nvidia_fb_info() {
            _stat.size = (pitch * h) as u64;
        } else {
            return Err(scheme_error::EIO);
        }
        Ok(0)
    }

    fn fmap(&self, _id: usize, _offset: usize, _len: usize) -> Result<usize, usize> {
        let fb_info = &crate::boot::get_boot_info().framebuffer;
        if fb_info.base_address != 0 && fb_info.base_address != 0xDEADBEEF {
            return Ok(fb_info.base_address as usize);
        }
        if let Some((phys, _w, _h, _pitch, size)) = crate::virtio::get_primary_virtio_display() {
            if _len <= size {
                return Ok(phys as usize);
            }
        }
        if let Some((phys, _bar_phys, _w, _h, _pitch)) = crate::nvidia::get_nvidia_fb_info() {
            // BAR1 is a large VRAM aperture (typically ≥256 MB on Turing+), so any
            // reasonable framebuffer mapping fits.  Return the base address directly
            // without a size guard, consistent with how the EFI GOP path is handled.
            return Ok(phys as usize);
        }
        Err(scheme_error::EIO)
    }
}

// --- Input Scheme ---

pub struct InputScheme {
    kbd: Mutex<VecDeque<u8>>,
    ptr: Mutex<VecDeque<u8>>,
}

impl InputScheme {
    // Cola acotada: evita que un lector atascado consuma memoria sin límite.
    const MAX_QUEUE_BYTES: usize = 256 * 1024;
    // Layout compatible con userspace `std::libc::InputEvent` (ver tests en `ipc.rs`).
    const INPUT_EVENT_SIZE: usize = 24;

    pub fn new() -> Self {
        Self {
            kbd: Mutex::new(VecDeque::new()),
            ptr: Mutex::new(VecDeque::new()),
        }
    }

    fn trim_overflow(queue: &mut VecDeque<u8>, incoming_len: usize) {
        if incoming_len >= Self::MAX_QUEUE_BYTES {
            queue.clear();
            return;
        }

        let needed = queue.len().saturating_add(incoming_len);
        if needed <= Self::MAX_QUEUE_BYTES {
            return;
        }

        // Tirar bytes antiguos para hacer espacio, manteniendo alineación por evento.
        let mut drop_bytes = needed - Self::MAX_QUEUE_BYTES;
        drop_bytes -= drop_bytes % Self::INPUT_EVENT_SIZE;
        for _ in 0..drop_bytes {
            let _ = queue.pop_front();
        }
    }
}

impl Scheme for InputScheme {
    fn open(&self, path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        // Map common Linux-like nodes to stable IDs:
        // - event0 / keyboard -> keyboard
        // - event1 / mouse -> pointer
        // - empty path (input:) is used by input_service as a write-only multiplexed endpoint
        match path {
            "" => Ok(2),
            "keyboard" | "event0" => Ok(0),
            "mouse" | "event1" => Ok(1),
            _ => Ok(2),
        }
    }

    fn read(&self, _id: usize, _buffer: &mut [u8], _offset: u64) -> Result<usize, usize> {
        if _buffer.len() < Self::INPUT_EVENT_SIZE {
            return Ok(0);
        }

        let mut q = match _id {
            0 => self.kbd.lock(),
            1 => self.ptr.lock(),
            _ => return Ok(0),
        };
        if q.is_empty() {
            return Ok(0);
        }

        let max = core::cmp::min(_buffer.len(), q.len());
        let to_copy = max - (max % Self::INPUT_EVENT_SIZE);
        if to_copy == 0 {
            return Ok(0);
        }

        for i in 0..to_copy {
            _buffer[i] = q.pop_front().unwrap_or(0);
        }

        Ok(to_copy)
    }

    fn write(&self, _id: usize, buf: &[u8], _offset: u64) -> Result<usize, usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        // Solo aceptamos registros completos; mantener alineación simplifica al lector.
        let aligned_len = buf.len() - (buf.len() % Self::INPUT_EVENT_SIZE);
        if aligned_len == 0 {
            return Ok(0);
        }

        // Multiplex: route each 24-byte input_event by device_id (u32 at offset 0).
        // Contract with userspace `InputEvent` in input_service: device_id 0=keyboard, 1=mouse/pointer.
        for chunk in buf[..aligned_len].chunks_exact(Self::INPUT_EVENT_SIZE) {
            let dev_id = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let mut q = if dev_id == 0 { self.kbd.lock() } else { self.ptr.lock() };
            Self::trim_overflow(&mut q, Self::INPUT_EVENT_SIZE);
            q.extend(chunk);
        }
        Ok(aligned_len)
    }

    fn poll(&self, _id: usize, events: usize) -> Result<usize, usize> {
        let mut ready = 0;
        if (events & crate::scheme::event::POLLIN) != 0 {
            let has_data = match _id {
                0 => !self.kbd.lock().is_empty(),
                1 => !self.ptr.lock().is_empty(),
                _ => false,
            };
            if has_data {
                ready |= crate::scheme::event::POLLIN;
            }
        }
        if (events & crate::scheme::event::POLLOUT) != 0 {
            ready |= crate::scheme::event::POLLOUT;
        }
        Ok(ready)
    }

    fn ioctl(&self, _id: usize, request: usize, arg: usize) -> Result<usize, usize> {
        // evdev compatibility layer (x86_64 Linux ioctl encoding).
        // Decode: dir(2) | size(14) | type(8) | nr(8)
        let ty = ((request >> 8) & 0xFF) as u8;
        let nr = (request & 0xFF) as u8;
        let size = ((request >> 16) & 0x3FFF) as usize;

        // Only handle 'E' (evdev) ioctls here.
        if ty != 0x45 {
            return Err(scheme_error::ENOSYS);
        }

        let dev_kind = match _id {
            0 => 0, // keyboard
            1 => 1, // pointer
            _ => 1, // default to pointer-ish for writer handle
        };

        // Helpers
        fn write_user(arg: usize, size: usize, src: &[u8]) -> Result<(), usize> {
            if arg == 0 {
                return Err(scheme_error::EFAULT);
            }
            if !crate::syscalls::is_user_pointer(arg as u64, size as u64) {
                return Err(scheme_error::EFAULT);
            }
            unsafe {
                core::ptr::write_bytes(arg as *mut u8, 0, size);
                let n = core::cmp::min(size, src.len());
                core::ptr::copy_nonoverlapping(src.as_ptr(), arg as *mut u8, n);
            }
            Ok(())
        }

        match nr {
            // EVIOCGVERSION
            0x01 => {
                if !crate::syscalls::is_user_pointer(arg as u64, 4) {
                    return Err(scheme_error::EFAULT);
                }
                unsafe { *(arg as *mut u32) = 0x010001; } // EV_VERSION
                Ok(0)
            }
            // EVIOCGID (struct input_id: bustype,u16 vendor,u16 product,u16 version,u16)
            0x02 => {
                let mut id = [0u8; 8];
                // bustype = BUS_VIRTUAL(0x06) as a harmless default
                id[0..2].copy_from_slice(&(0x06u16).to_le_bytes());
                // vendor/product/version left as 0
                write_user(arg, 8, &id)?;
                Ok(0)
            }
            // EVIOCGNAME(len)
            0x06 => {
                let name: &[u8] = if dev_kind == 0 { b"Eclipse Keyboard\0" } else { b"Eclipse Pointer\0" };
                write_user(arg, size, name)?;
                Ok(0)
            }
            // EVIOCGPHYS(len)
            0x07 => {
                let phys: &[u8] = if dev_kind == 0 { b"eclipse/input/keyboard0\0" } else { b"eclipse/input/pointer0\0" };
                write_user(arg, size, phys)?;
                Ok(0)
            }
            // EVIOCGUNIQ(len)
            0x08 => {
                let uniq = if dev_kind == 0 { b"kbd0\0" } else { b"ptr0\0" };
                write_user(arg, size, uniq)?;
                Ok(0)
            }
            // EVIOCGBIT(ev, len): nr = 0x20 + ev
            n if (0x20..=0x3f).contains(&n) => {
                let ev = (n - 0x20) as usize;
                // Build bitmasks in Linux format: little-endian array of unsigned long bits.
                // We keep it simple: return a byte array where each bit represents a code.
                let mut out: Vec<u8> = Vec::new();
                out.resize(size.max(1), 0u8);

                // EV = 0: event types bitmask
                if ev == 0 {
                    // EV_SYN(0), EV_KEY(1)
                    out[0] |= 1 << 0;
                    out[0] |= 1 << 1;
                    if dev_kind == 1 {
                        // EV_REL(2)
                        out[0] |= 1 << 2;
                    }
                } else if ev == 1 {
                    // EV_KEY: key codes bitmask
                    if dev_kind == 0 {
                        // Mark a sane range of KEY_* as supported (0..=127).
                        for code in 0u16..=127u16 {
                            let idx = (code / 8) as usize;
                            let bit = (code % 8) as u8;
                            if idx < out.len() {
                                out[idx] |= 1 << bit;
                            }
                        }
                    } else {
                        // Mouse buttons: BTN_LEFT(272), BTN_RIGHT(273), BTN_MIDDLE(274)
                        for code in [272u16, 273u16, 274u16] {
                            let idx = (code / 8) as usize;
                            let bit = (code % 8) as u8;
                            if idx < out.len() {
                                out[idx] |= 1 << bit;
                            }
                        }
                    }
                } else if ev == 2 && dev_kind == 1 {
                    // EV_REL: REL_X(0), REL_Y(1), REL_WHEEL(8)
                    for code in [0u16, 1u16, 8u16] {
                        let idx = (code / 8) as usize;
                        let bit = (code % 8) as u8;
                        if idx < out.len() {
                            out[idx] |= 1 << bit;
                        }
                    }
                }

                write_user(arg, size, &out)?;
                Ok(0)
            }
            // EVIOCGABS(abs): nr = 0x40 + abs
            n if (0x40..=0x7f).contains(&n) => {
                // Only meaningful for absolute devices; return zeros.
                #[repr(C)]
                struct InputAbsInfo {
                    value: i32,
                    minimum: i32,
                    maximum: i32,
                    fuzz: i32,
                    flat: i32,
                    resolution: i32,
                }
                let abs = InputAbsInfo { value: 0, minimum: 0, maximum: 0, fuzz: 0, flat: 0, resolution: 0 };
                let bytes = unsafe {
                    core::slice::from_raw_parts((&abs as *const InputAbsInfo) as *const u8, core::mem::size_of::<InputAbsInfo>())
                };
                write_user(arg, size, bytes)?;
                Ok(0)
            }
            _ => Err(scheme_error::ENOSYS),
        }
    }

    fn fstat(&self, _id: usize, stat: &mut Stat) -> Result<usize, usize> {
        stat.mode = 0o444 | 0x2000; // Character device, read-only (for userspace readers)
        stat.size = match _id {
            0 => self.kbd.lock().len() as u64,
            1 => self.ptr.lock().len() as u64,
            _ => 0,
        };
        Ok(0)
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize, _current_offset: u64) -> Result<usize, usize> {
        Ok(0)
    }

    fn close(&self, _id: usize) -> Result<usize, usize> {
        Ok(0)
    }
}

// --- Audio Scheme ---

pub struct AudioScheme;

impl Scheme for AudioScheme {
    fn open(&self, _path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        Ok(0)
    }

    fn read(&self, _id: usize, _buffer: &mut [u8], _offset: u64) -> Result<usize, usize> {
        Ok(0)
    }

    fn write(&self, _id: usize, buf: &[u8], _offset: u64) -> Result<usize, usize> {
        Ok(buf.len())
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize, _current_offset: u64) -> Result<usize, usize> {
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

    fn read(&self, _id: usize, _buffer: &mut [u8], _offset: u64) -> Result<usize, usize> {
        Ok(0)
    }

    fn write(&self, _id: usize, buf: &[u8], _offset: u64) -> Result<usize, usize> {
        Ok(buf.len())
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize, _current_offset: u64) -> Result<usize, usize> {
        Ok(0)
    }

    fn close(&self, _id: usize) -> Result<usize, usize> {
        Ok(0)
    }

    fn fstat(&self, _id: usize, _stat: &mut Stat) -> Result<usize, usize> {
        Ok(0)
    }
}

use alloc::sync::Arc;
use spin::Mutex;

use alloc::collections::BTreeMap;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketState {
    Unused,
    Created,
    Bound,
    Listening,
    Connected,
}

pub struct Socket {
    pub id: usize,
    pub domain: u32,
    pub type_: u32,
    pub protocol: u32,
    pub state: SocketState,
    pub path: Option<String>,
    /// When Connected, this socket is one end of a connection (client or server).
    pub connection_id: Option<usize>,
}

/// Maximum bytes buffered per direction per connection (avoid OOM).
pub const CONNECTION_BUFFER_CAP: usize = 256 * 1024;

/// One bidirectional connection between a listener (server) and a client.
struct Connection {
    id: usize,
    /// Data written by server, read by client.
    buffer_to_client: VecDeque<u8>,
    /// Data written by client, read by server.
    buffer_to_server: VecDeque<u8>,
    client_socket_id: usize,
    server_socket_id: Option<usize>,
    /// True when one side has closed; the other will see EOF on read.
    closed_by_client: bool,
    closed_by_server: bool,
    /// File-descriptor batches (scheme_id, resource_id) queued by client for server to receive.
    fd_queue_to_server: VecDeque<alloc::vec::Vec<(usize, usize)>>,
    /// File-descriptor batches queued by server for client to receive.
    fd_queue_to_client: VecDeque<alloc::vec::Vec<(usize, usize)>>,
}

/// Pending connections: listener socket id -> queue of connection ids waiting for accept().
type PendingQueue = BTreeMap<usize, VecDeque<usize>>;

struct SocketSchemeState {
    sockets: BTreeMap<usize, Socket>,
    connections: BTreeMap<usize, Connection>,
    pending: PendingQueue,
    next_socket_id: usize,
    next_connection_id: usize,
}

// --- Socket Scheme ---

pub struct SocketScheme {
    state: Mutex<SocketSchemeState>,
    network_pid: Mutex<Option<u32>>,
}

impl SocketScheme {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(SocketSchemeState {
                sockets: BTreeMap::new(),
                connections: BTreeMap::new(),
                pending: BTreeMap::new(),
                next_socket_id: 1,
                next_connection_id: 1,
            }),
            network_pid: Mutex::new(None),
        }
    }

    fn get_network_pid(&self) -> Option<u32> {
        let mut pid_opt = self.network_pid.lock();
        if let Some(pid) = *pid_opt {
             if let Some(p) = crate::process::get_process(pid) {
                 let proc = p.proc.lock();
                 let name_len = proc.name.iter().position(|&b| b == 0).unwrap_or(16);
                 if &proc.name[..name_len] == b"network" {
                     return Some(pid);
                 }
             }
        }
        if let Some(pid) = crate::process::get_process_by_name("network") {
            *pid_opt = Some(pid);
            return Some(pid);
        }
        None
    }

    fn send_request_and_wait(&self, net_pid: u32, op: NetOp, resource_id: u64, data: &[u8]) -> Result<(i64, Option<crate::ipc::Message>), usize> {
        static NEXT_REQUEST_ID: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);
        let request_id = NEXT_REQUEST_ID.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        let client_pid = crate::process::current_process_id().unwrap_or(0);
        
        let mut msg_data = [0u8; 512];
        let header = NetRequestHeader {
            magic: NET_MAGIC,
            op,
            request_id,
            client_pid,
            resource_id,
        };
        
        unsafe {
            let header_ptr = &header as *const NetRequestHeader as *const u8;
            core::ptr::copy_nonoverlapping(header_ptr, msg_data.as_mut_ptr(), core::mem::size_of::<NetRequestHeader>());
            
            let payload_offset = core::mem::size_of::<NetRequestHeader>();
            let payload_len = core::cmp::min(data.len(), 512 - payload_offset);
            if payload_len > 0 {
                core::ptr::copy_nonoverlapping(data.as_ptr(), msg_data.as_mut_ptr().add(payload_offset), payload_len);
            }
        }
        
        if !crate::ipc::send_message(0, net_pid, MessageType::Network, &msg_data) {
            return Err(scheme_error::EIO);
        }
        
        let start_ticks = crate::interrupts::ticks();
        loop {
            let msg_opt = crate::ipc::receive_message_filtered(client_pid, |msg| {
                if msg.msg_type == MessageType::Network && msg.data_size >= core::mem::size_of::<NetResponseHeader>() as u32 {
                    let resp = unsafe { &*(msg.data.as_ptr() as *const NetResponseHeader) };
                    return resp.magic == NET_MAGIC && resp.op == NetOp::Response && resp.request_id == request_id;
                }
                false
            });

            if let Some(msg) = msg_opt {
                let resp = unsafe { &*(msg.data.as_ptr() as *const NetResponseHeader) };
                return Ok((resp.status, Some(msg)));
            }

            if crate::interrupts::ticks() > start_ticks + 5000 {
                return Err(scheme_error::EAGAIN);
            }
            crate::scheduler::yield_cpu();
        }
    }

    pub fn bind(&self, id: usize, path: String) -> Result<(), usize> {
        let mut st = self.state.lock();
        let socket = st.sockets.get_mut(&id).ok_or(scheme_error::EBADF)?;
        let domain = socket.domain;
        
        if domain == 1 {
            socket.path = Some(path);
            socket.state = SocketState::Bound;
            return Ok(());
        } else if domain == 2 {
            drop(st);
            if let Some(net_pid) = self.get_network_pid() {
                let (res, _) = self.send_request_and_wait(net_pid, crate::net::NetOp::Bind, id as u64, path.as_bytes())?;
                if res < 0 {
                    return Err((-res) as usize);
                }
                return Ok(());
            }
        }
        Err(scheme_error::EAFNOSUPPORT)
    }

    pub fn listen(&self, id: usize) -> Result<(), usize> {
        let mut st = self.state.lock();
        let socket = st.sockets.get_mut(&id).ok_or(scheme_error::EBADF)?;
        let domain = socket.domain;

        if domain == 1 {
            socket.state = SocketState::Listening;
            st.pending.entry(id).or_default();
            return Ok(());
        } else if domain == 2 {
            drop(st);
            if let Some(net_pid) = self.get_network_pid() {
                let (res, _) = self.send_request_and_wait(net_pid, crate::net::NetOp::Listen, id as u64, &[])?;
                if res < 0 {
                    return Err((-res) as usize);
                }
                return Ok(());
            }
        }
        Err(scheme_error::EAFNOSUPPORT)
    }

    pub fn accept(&self, id: usize) -> Result<usize, usize> {
        let mut st = self.state.lock();
        let socket = st.sockets.get(&id).ok_or(scheme_error::EBADF)?;
        let domain = socket.domain;
        let type_ = socket.type_;
        let protocol = socket.protocol;

        if domain == 1 {
            if socket.state != SocketState::Listening {
                return Err(scheme_error::EINVAL);
            }
            let conn_id = match st.pending.get_mut(&id).and_then(|q| q.pop_front()) {
                Some(cid) => cid,
                None => return Err(scheme_error::EAGAIN),
            };
            let new_id = st.next_socket_id;
            st.next_socket_id += 1;
            if let Some(conn) = st.connections.get_mut(&conn_id) {
                conn.server_socket_id = Some(new_id);
            }
            st.sockets.insert(new_id, Socket {
                id: new_id,
                domain,
                type_,
                protocol,
                state: SocketState::Connected,
                path: None,
                connection_id: Some(conn_id),
            });
            return Ok(new_id);
        } else if domain == 2 {
            drop(st);
            if let Some(net_pid) = self.get_network_pid() {
                let (res, _) = self.send_request_and_wait(net_pid, crate::net::NetOp::Accept, id as u64, &[])?;
                if res < 0 {
                    return Err((-res) as usize);
                }
                let new_id = res as usize;
                let mut st = self.state.lock();
                st.sockets.insert(new_id, Socket {
                    id: new_id,
                    domain: 2,
                    type_,
                    protocol,
                    state: SocketState::Connected,
                    path: None,
                    connection_id: None,
                });
                return Ok(new_id);
            }
        }
        Err(scheme_error::EAFNOSUPPORT)
    }

    pub fn connect(&self, id: usize, path: &str) -> Result<(), usize> {
        let mut st = self.state.lock();
        let socket = st.sockets.get(&id).ok_or(scheme_error::EBADF)?;
        let domain = socket.domain;

        if domain == 1 {
            if socket.state != SocketState::Created && socket.state != SocketState::Bound {
                return Err(scheme_error::EISCONN);
            }
            let listener_id = match st.sockets.iter().find(|(_, s)| {
                s.path.as_deref() == Some(path) && s.state == SocketState::Listening
            }) {
                Some((&lid, _)) => lid,
                None => return Err(scheme_error::ENOENT),
            };
            let conn_id = st.next_connection_id;
            st.next_connection_id += 1;
            st.connections.insert(conn_id, Connection {
                id: conn_id,
                buffer_to_client: VecDeque::new(),
                buffer_to_server: VecDeque::new(),
                client_socket_id: id,
                server_socket_id: None,
                closed_by_client: false,
                closed_by_server: false,
                fd_queue_to_server: VecDeque::new(),
                fd_queue_to_client: VecDeque::new(),
            });
            st.pending.entry(listener_id).or_default().push_back(conn_id);
            if let Some(socket) = st.sockets.get_mut(&id) {
                socket.state = SocketState::Connected;
                socket.connection_id = Some(conn_id);
            }
            return Ok(());
        } else if domain == 2 {
            drop(st);
            if let Some(net_pid) = self.get_network_pid() {
                // Parsing target IP/Port from path "IP:Port"
                let (res, _) = self.send_request_and_wait(net_pid, crate::net::NetOp::Connect, id as u64, path.as_bytes())?;
                if res < 0 {
                    return Err((-res) as usize);
                }
                return Ok(());
            }
        }
        Err(scheme_error::EAFNOSUPPORT)
    }

    pub fn socketpair(&self, domain: u32, type_: u32, proto: u32) -> Result<(usize, usize), usize> {
        if domain != 1 { return Err(scheme_error::EAFNOSUPPORT); }
        let mut st = self.state.lock();
        let conn_id = st.next_connection_id;
        st.next_connection_id += 1;
        
        let s1_id = st.next_socket_id;
        st.next_socket_id += 1;
        let s2_id = st.next_socket_id;
        st.next_socket_id += 1;
        
        st.connections.insert(conn_id, Connection {
            id: conn_id,
            buffer_to_client: VecDeque::new(),
            buffer_to_server: VecDeque::new(),
            client_socket_id: s1_id,
            server_socket_id: Some(s2_id),
            closed_by_client: false,
            closed_by_server: false,
            fd_queue_to_server: VecDeque::new(),
            fd_queue_to_client: VecDeque::new(),
        });
        
        st.sockets.insert(s1_id, Socket {
            id: s1_id,
            domain: 1,
            type_,
            protocol: proto,
            state: SocketState::Connected,
            path: None,
            connection_id: Some(conn_id),
        });
        
        st.sockets.insert(s2_id, Socket {
            id: s2_id,
            domain: 1,
            type_,
            protocol: proto,
            state: SocketState::Connected,
            path: None,
            connection_id: Some(conn_id),
        });
        
        Ok((s1_id, s2_id))
    }

    pub fn shutdown(&self, id: usize, _how: i32) -> Result<(), usize> {
        let mut st = self.state.lock();
        let _socket = st.sockets.get(&id).ok_or(scheme_error::EBADF)?;
        // For now, we just mark as closed.
        Self::close_connection(&mut st, id);
        Ok(())
    }

    pub fn getsockname(&self, id: usize, buf: &mut [u8]) -> Result<usize, usize> {
        let st = self.state.lock();
        let socket = st.sockets.get(&id).ok_or(scheme_error::EBADF)?;
        if let Some(ref path) = socket.path {
            let bytes = path.as_bytes();
            let len = bytes.len().min(buf.len());
            buf[..len].copy_from_slice(&bytes[..len]);
            return Ok(len);
        }
        Ok(0)
    }

    pub fn getpeername(&self, id: usize, buf: &mut [u8]) -> Result<usize, usize> {
        let st = self.state.lock();
        let socket = st.sockets.get(&id).ok_or(scheme_error::EBADF)?;
        if let Some(conn_id) = socket.connection_id {
            if let Some(conn) = st.connections.get(&conn_id) {
                let peer_id = if id == conn.client_socket_id { conn.server_socket_id } else { Some(conn.client_socket_id) };
                if let Some(pid) = peer_id {
                    if let Some(peer) = st.sockets.get(&pid) {
                        if let Some(ref path) = peer.path {
                            let bytes = path.as_bytes();
                            let len = bytes.len().min(buf.len());
                            buf[..len].copy_from_slice(&bytes[..len]);
                            return Ok(len);
                        }
                    }
                }
            }
        }
        Ok(0)
    }

    pub fn setsockopt(&self, _id: usize, _level: i32, _opt: i32, _val: &[u8]) -> Result<(), usize> {
        Ok(()) // Placeholder
    }

    pub fn getsockopt(&self, _id: usize, _level: i32, _opt: i32, _buf: &mut [u8]) -> Result<usize, usize> {
        Ok(0) // Placeholder
    }
}

impl SocketScheme {
    /// Read from the connection buffer for this socket (peer's written data).
    fn read_connection(st: &mut SocketSchemeState, id: usize, buffer: &mut [u8]) -> Result<usize, usize> {
        let conn_id = match st.sockets.get(&id).and_then(|s| s.connection_id) {
            Some(cid) => cid,
            None => return Ok(0),
        };
        let conn = match st.connections.get_mut(&conn_id) {
            Some(c) => c,
            None => return Err(scheme_error::EBADF),
        };
        let (buf, closed) = if Some(id) == conn.server_socket_id {
            (&mut conn.buffer_to_server, conn.closed_by_client)
        } else if id == conn.client_socket_id {
            (&mut conn.buffer_to_client, conn.closed_by_server)
        } else {
            return Err(scheme_error::EBADF);
        };
        if buf.is_empty() {
            return if closed { Ok(0) } else { Err(scheme_error::EAGAIN) };
        }
        let n = core::cmp::min(buffer.len(), buf.len());
        for i in 0..n {
            buffer[i] = buf.pop_front().unwrap();
        }
        Ok(n)
    }

    /// Write into the connection buffer for the peer to read.
    fn write_connection(st: &mut SocketSchemeState, id: usize, buf: &[u8]) -> Result<usize, usize> {
        let conn_id = match st.sockets.get(&id).and_then(|s| s.connection_id) {
            Some(cid) => cid,
            None => return Err(scheme_error::ENOTCONN),
        };
        let conn = match st.connections.get_mut(&conn_id) {
            Some(c) => c,
            None => return Err(scheme_error::EPIPE),
        };
        let (target, closed) = if Some(id) == conn.server_socket_id {
            (&mut conn.buffer_to_client, conn.closed_by_client)
        } else if id == conn.client_socket_id {
            (&mut conn.buffer_to_server, conn.closed_by_server)
        } else {
            return Err(scheme_error::EBADF);
        };
        if closed {
            return Err(scheme_error::EPIPE);
        }
        let space = CONNECTION_BUFFER_CAP.saturating_sub(target.len());
        let n = core::cmp::min(buf.len(), space);
        target.extend(buf[..n].iter().copied());
        Ok(n)
    }

    /// Mark this end of the connection as closed; remove connection when both sides closed.
    fn close_connection(st: &mut SocketSchemeState, id: usize) {
        let conn_id = match st.sockets.get(&id).and_then(|s| s.connection_id) {
            Some(cid) => cid,
            None => {
                st.sockets.remove(&id);
                return;
            }
        };
        let both_closed = if let Some(conn) = st.connections.get_mut(&conn_id) {
            if Some(id) == conn.server_socket_id {
                conn.closed_by_server = true;
            } else if id == conn.client_socket_id {
                conn.closed_by_client = true;
            }
            conn.closed_by_server && conn.closed_by_client
        } else {
            false
        };
        st.sockets.remove(&id);
        if both_closed {
            st.connections.remove(&conn_id);
        }
    }

    /// Write raw bytes to a socket identified by resource id (for sendmsg).
    pub fn socket_write_raw(&self, id: usize, buf: &[u8]) -> Result<usize, usize> {
        let mut st = self.state.lock();
        let socket = st.sockets.get(&id).ok_or(scheme_error::EBADF)?;
        if socket.domain != 1 {
            return Err(scheme_error::EAFNOSUPPORT);
        }
        Self::write_connection(&mut st, id, buf)
    }

    /// Read raw bytes from a socket identified by resource id (for recvmsg).
    pub fn socket_read_raw(&self, id: usize, buf: &mut [u8]) -> Result<usize, usize> {
        let mut st = self.state.lock();
        let socket = st.sockets.get(&id).ok_or(scheme_error::EBADF)?;
        if socket.domain != 1 {
            return Err(scheme_error::EAFNOSUPPORT);
        }
        Self::read_connection(&mut st, id, buf)
    }

    /// Enqueue a batch of file descriptors (as kernel (scheme_id, resource_id) pairs) to be
    /// delivered to the peer on the next recvmsg call.
    pub fn socket_enqueue_fds(&self, id: usize, fds: alloc::vec::Vec<(usize, usize)>) {
        let mut st = self.state.lock();
        let conn_id = match st.sockets.get(&id).and_then(|s| s.connection_id) {
            Some(cid) => cid,
            None => return,
        };
        let conn = match st.connections.get_mut(&conn_id) {
            Some(c) => c,
            None => return,
        };
        if Some(id) == conn.server_socket_id {
            conn.fd_queue_to_client.push_back(fds);
        } else if id == conn.client_socket_id {
            conn.fd_queue_to_server.push_back(fds);
        }
    }

    /// Pop the oldest queued FD batch meant for this socket's reader.
    pub fn socket_dequeue_fds(&self, id: usize) -> Option<alloc::vec::Vec<(usize, usize)>> {
        let mut st = self.state.lock();
        let conn_id = st.sockets.get(&id)?.connection_id?;
        let conn = st.connections.get_mut(&conn_id)?;
        if Some(id) == conn.server_socket_id {
            conn.fd_queue_to_server.pop_front()
        } else if id == conn.client_socket_id {
            conn.fd_queue_to_client.pop_front()
        } else {
            None
        }
    }
}

impl Scheme for SocketScheme {
    fn open(&self, path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        let mut parts = path.split('/');
        let domain_str = parts.next().unwrap_or("0");
        let type_str = parts.next().unwrap_or("0");
        let proto_str = parts.next().unwrap_or("0");

        let domain = domain_str.parse::<u32>().unwrap_or(0);
        let type_ = type_str.parse::<u32>().unwrap_or(0);
        let proto = proto_str.parse::<u32>().unwrap_or(0);

        if domain == 1 || domain_str == "unix" {
            let mut st = self.state.lock();
            let id = st.next_socket_id;
            st.next_socket_id += 1;
            st.sockets.insert(id, Socket {
                id,
                domain: 1,
                type_,
                protocol: proto,
                state: SocketState::Created,
                path: None,
                connection_id: None,
            });
            return Ok(id);
        } else if domain == 2 {
            // AF_INET: delegate to network service
            if let Some(net_pid) = self.get_network_pid() {
                let (res, _) = self.send_request_and_wait(net_pid, crate::net::NetOp::Socket, 0, path.as_bytes())?;
                if res < 0 {
                    return Err((-res) as usize);
                }
                let id = res as usize;
                let mut st = self.state.lock();
                // Store a local entry to track it
                st.sockets.insert(id, Socket {
                    id,
                    domain: 2,
                    type_,
                    protocol: proto,
                    state: SocketState::Created,
                    path: None,
                    connection_id: None, // We don't use this for domain 2
                });
                return Ok(id);
            }
        } else if domain == 16 {
            // AF_NETLINK: stub implementation for udev/netlink
            let mut st = self.state.lock();
            let id = st.next_socket_id;
            st.next_socket_id += 1;
            st.sockets.insert(id, Socket {
                id,
                domain: 16,
                type_,
                protocol: proto,
                state: SocketState::Created,
                path: None,
                connection_id: None,
            });
            return Ok(id);
        }

        Err(scheme_error::EAFNOSUPPORT)
    }

    fn read(&self, id: usize, buffer: &mut [u8], _offset: u64) -> Result<usize, usize> {
        let mut st = self.state.lock();
        let socket = st.sockets.get(&id).ok_or(scheme_error::EBADF)?;
        let domain = socket.domain;
        
        if domain == 1 {
            if socket.connection_id.is_none() {
                return Err(scheme_error::ENOTCONN);
            }
            return Self::read_connection(&mut st, id, buffer);
        } else if domain == 2 {
            drop(st);
            if let Some(net_pid) = self.get_network_pid() {
                let (res, msg_opt) = self.send_request_and_wait(net_pid, crate::net::NetOp::Recv, id as u64, &[])?;
                if res < 0 {
                    return Err((-res) as usize);
                }
                if let Some(msg) = msg_opt {
                    let header_size = core::mem::size_of::<crate::net::NetResponseHeader>();
                    let data_start = &msg.data[header_size..];
                    let to_copy = core::cmp::min(buffer.len(), (msg.data_size as usize).saturating_sub(header_size));
                    buffer[..to_copy].copy_from_slice(&data_start[..to_copy]);
                    return Ok(to_copy);
                }
            }
        }
        
        Err(scheme_error::ENOSYS)
    }

    fn write(&self, id: usize, buf: &[u8], _offset: u64) -> Result<usize, usize> {
        let mut st = self.state.lock();
        let socket = st.sockets.get(&id).ok_or(scheme_error::EBADF)?;
        let domain = socket.domain;

        if domain == 1 {
            if socket.connection_id.is_some() {
                return Self::write_connection(&mut st, id, buf);
            }
            return Err(scheme_error::ENOTCONN);
        } else if domain == 2 {
            drop(st);
            if let Some(net_pid) = self.get_network_pid() {
                let (res, _) = self.send_request_and_wait(net_pid, crate::net::NetOp::Send, id as u64, buf)?;
                if res < 0 {
                    return Err((-res) as usize);
                }
                return Ok(res as usize);
            }
        }
        
        Err(scheme_error::ENOTCONN)
    }

    fn close(&self, id: usize) -> Result<usize, usize> {
        let mut st = self.state.lock();
        if let Some(socket) = st.sockets.get(&id) {
            let domain = socket.domain;
            if domain == 2 {
                drop(st);
                if let Some(net_pid) = self.get_network_pid() {
                    let _ = self.send_request_and_wait(net_pid, crate::net::NetOp::Close, id as u64, &[]);
                }
                let mut st = self.state.lock();
                st.sockets.remove(&id);
            } else {
                Self::close_connection(&mut st, id);
            }
        }
        Ok(0)
    }

    fn fstat(&self, _id: usize, _stat: &mut Stat) -> Result<usize, usize> {
        _stat.mode = 0o140000;
        Ok(0)
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize, _current_offset: u64) -> Result<usize, usize> {
        Err(scheme_error::ESPIPE)
    }

    fn poll(&self, id: usize, events: usize) -> Result<usize, usize> {
        let st = self.state.lock();
        let socket = st.sockets.get(&id).ok_or(scheme_error::EBADF)?;
        let domain = socket.domain;
        let state = socket.state;

        let mut ready = 0;

        if domain == 1 {
            if state == SocketState::Listening {
                if let Some(q) = st.pending.get(&id) {
                    if !q.is_empty() {
                        ready |= crate::scheme::event::POLLIN;
                    }
                }
            } else if let Some(conn_id) = socket.connection_id {
                if let Some(conn) = st.connections.get(&conn_id) {
                    let is_server = conn.server_socket_id == Some(id);
                    let rx_buf: &alloc::collections::VecDeque<u8> = if is_server {
                        &conn.buffer_to_server
                    } else {
                        &conn.buffer_to_client
                    };
                    let peer_closed = if is_server {
                        conn.closed_by_client
                    } else {
                        conn.closed_by_server
                    };

                    if (events & crate::scheme::event::POLLIN) != 0 {
                        // Ready for POLLIN when data is available OR peer has closed (EOF)
                        if !rx_buf.is_empty() || peer_closed {
                            ready |= crate::scheme::event::POLLIN;
                        }
                    }
                    if (events & crate::scheme::event::POLLOUT) != 0 {
                        // For now, UNIX sockets are always ready for write if connected
                        ready |= crate::scheme::event::POLLOUT;
                    }
                }
            }
        } else if domain == 16 {
            // AF_NETLINK: stub - never ready for read for now
        }

        Ok(ready)
    }
}

static mut SOCKET_SCHEME: Option<Arc<SocketScheme>> = None;

pub fn get_socket_scheme() -> Option<Arc<SocketScheme>> {
    unsafe { SOCKET_SCHEME.clone() }
}

pub fn init() {
    init_servers();
    
    let socket_scheme = Arc::new(SocketScheme::new());
    unsafe {
        SOCKET_SCHEME = Some(socket_scheme.clone());
    }
    crate::scheme::register_scheme("socket", socket_scheme);
}
