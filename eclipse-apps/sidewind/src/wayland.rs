
extern crate alloc;
use alloc::vec::Vec;
use alloc::collections::VecDeque;
use alloc::boxed::Box;

/// Error types for Wayland protocol handling
#[derive(Debug)]
pub enum WaylandError {
    InvalidMessage,
    UnknownObject(u32),
    UnknownOpcode(u16),
    DecodeError,
}

/// Identificadores globales para servicios base de Wayland.
pub const ID_COMPOSITOR: u32 = 1;
pub const ID_SHM: u32 = 2;
pub const ID_SEAT: u32 = 3;

/// Cabecera binaria del protocolo Wayland optimizado.
/// Total: 8 bytes.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct WaylandHeader {
    pub object_id: u32,
    pub opcode: u8,
    pub flags: u8,
    pub length: u16,
}

impl WaylandHeader {
    pub fn new(object_id: u32, opcode: u8, length: u16) -> Self {
        Self {
            object_id,
            opcode,
            flags: 0,
            length,
        }
    }
}

/// Mensaje para crear una superficie.
/// Opcode: 1 (Target: ID_COMPOSITOR)
/// Total: 16 bytes.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct WaylandMsgCreateSurface {
    pub header: WaylandHeader,
    pub new_id: u32,
    pub width: u16,
    pub height: u16,
}

/// Mensaje para crear un pool de memoria compartida.
/// Opcode: 1 (Target: ID_SHM)
/// Total: 20 bytes.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct WaylandMsgCreatePool {
    pub header: WaylandHeader,
    pub new_id: u32,
    pub size: u32,
    pub _reserved: u32,
}

/// Mensaje atómico para atachar un buffer y hacer commit.
/// Opcode: 1 (Target: surface_id)
/// Total: 32 bytes. Includes direct vaddr for flat-memory compositing.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct WaylandMsgCommitFrame {
    pub header: WaylandHeader,
    pub pool_id: u32,
    pub offset: u32,
    pub width: u16,
    pub height: u16,
    pub stride: u16,
    pub format: u16,
    /// Direct virtual address of the pixel buffer in the sender's address space.
    /// Non-zero value allows the compositor to blit without a shared-file pool.
    pub vaddr: u64,
}

/// Message to set the window title shown in the compositor's decoration bar.
/// Opcode: 2 (Target: surface_id)
/// Total: 40 bytes.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct WaylandMsgSetTitle {
    pub header: WaylandHeader,
    /// Null-terminated UTF-8 window title.  Remaining bytes are zero-padded.
    pub title: [u8; 32],
}

/// Evento de teclado enviado al cliente.
/// Opcode: 10 (Target: ID_SEAT)
/// Total: 12 bytes.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct WaylandEvtKeyboard {
    pub header: WaylandHeader,
    pub key: u16,
    pub state: u16,
}

pub struct WaylandConnection {
    pub registry: ObjectRegistry,
    pub pending_events: VecDeque<Vec<u8>>,
    pub internal_events: VecDeque<WaylandInternalEvent>,
}

#[derive(Debug, Clone)]
pub enum WaylandInternalEvent {
    SurfaceCommitted {
        surface_id: u32,
        buffer_id: Option<u32>,
        damage: Vec<(i32, i32, i32, i32)>,
    },
    ShellSurfaceCreated {
        surface_id: u32,
        shell_surface_id: u32,
    },
    XdgSurfaceCreated {
        surface_id: u32,
        xdg_surface_id: u32,
    },
    XdgToplevelCreated {
        xdg_surface_id: u32,
        toplevel_id: u32,
        surface_id: u32,
    },
}

impl WaylandConnection {
    pub fn new() -> Self {
        Self {
            registry: ObjectRegistry::new(),
            pending_events: VecDeque::new(),
            internal_events: VecDeque::new(),
        }
    }

    pub fn send_event(&mut self, object_id: u32, opcode: u16, args: &[u8]) {
        let size = 8 + args.len();
        let mut data = Vec::with_capacity(size);
        data.extend_from_slice(&object_id.to_le_bytes());
        let size_op = ((size as u32) << 16) | (opcode as u32);
        data.extend_from_slice(&size_op.to_le_bytes());
        data.extend_from_slice(args);
        while data.len() % 4 != 0 {
            data.push(0);
        }
        self.pending_events.push_back(data);
    }

    pub fn process_message(&mut self, data: &[u8]) -> Result<(), WaylandError> {
        if data.len() < 8 {
            return Err(WaylandError::InvalidMessage);
        }
        
        let obj_id = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let size_op = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let size = (size_op >> 16) as usize;
        let opcode = (size_op & 0xFFFF) as u16;
        
        if data.len() < size {
            return Err(WaylandError::InvalidMessage);
        }

        // To call handle_request we need to temporarily take the object out of the registry
        // to avoid double mutable borrow of the connection and the object.
        if let Some(mut obj) = self.registry.take(obj_id) {
            let res = obj.handle_request(self, obj_id, opcode, &data[8..size]);
            self.registry.set(obj_id, obj);
            res
        } else {
            Err(WaylandError::UnknownObject(obj_id))
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ShmBufferInfo {
    pub offset: i32,
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub format: u32,
    pub shm_pool_fd: i32,
    pub pool_id: u32,
}

pub trait WaylandObject {
    fn interface_name(&self) -> &'static str;
    fn version(&self) -> u32;
    fn handle_request(&mut self, conn: &mut WaylandConnection, id: u32, opcode: u16, args: &[u8]) -> Result<(), WaylandError>;

    fn as_buffer(&self) -> Option<ShmBufferInfo> { None }
    fn as_dmabuf_buffer(&self) -> Option<DmabufBufferInfo> { None }
    fn as_surface_pending_buffer(&self) -> Option<u32> { None }
}

#[derive(Debug, Clone, Copy)]
pub struct DmabufBufferInfo {
    pub handle: u32,
    pub width: i32,
    pub height: i32,
    pub format: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WaylandArg {
    Int(i32),
    Uint(u32),
    Fixed(i32),
    String(Vec<u8>),
    Object(u32),
    NewId(u32),
    Array(Vec<u8>),
    Fd(i32),
}

pub fn decode_args(signature: &str, data: &[u8]) -> (Vec<WaylandArg>, usize) {
    let mut args = Vec::new();
    let mut offset = 0;
    for c in signature.chars() {
        if offset + 4 > data.len() && c != 's' && c != 'a' { break; }
        match c {
            'i' | 'h' => {
                let mut val = [0u8; 4];
                val.copy_from_slice(&data[offset..offset + 4]);
                args.push(if c == 'i' {
                    WaylandArg::Int(i32::from_le_bytes(val))
                } else {
                    WaylandArg::Fd(i32::from_le_bytes(val))
                });
                offset += 4;
            }
            'u' | 'o' | 'n' => {
                let mut val = [0u8; 4];
                val.copy_from_slice(&data[offset..offset + 4]);
                let u = u32::from_le_bytes(val);
                args.push(match c {
                    'u' => WaylandArg::Uint(u),
                    'o' => WaylandArg::Object(u),
                    'n' => WaylandArg::NewId(u),
                    _ => unreachable!(),
                });
                offset += 4;
            }
            's' => {
                if offset + 4 > data.len() { break; }
                let mut len_bytes = [0u8; 4];
                len_bytes.copy_from_slice(&data[offset..offset + 4]);
                let len = u32::from_le_bytes(len_bytes) as usize;
                offset += 4;
                if len > 0 {
                    if offset + len > data.len() { break; }
                    let s = data[offset..offset + len.saturating_sub(1)].to_vec();
                    args.push(WaylandArg::String(s));
                    offset += (len + 3) & !3;
                } else {
                    args.push(WaylandArg::String(Vec::new()));
                }
            }
            'f' => {
                let mut val = [0u8; 4];
                val.copy_from_slice(&data[offset..offset + 4]);
                args.push(WaylandArg::Fixed(i32::from_le_bytes(val)));
                offset += 4;
            }
            'a' => {
                if offset + 4 > data.len() { break; }
                let mut len_bytes = [0u8; 4];
                len_bytes.copy_from_slice(&data[offset..offset + 4]);
                let len = u32::from_le_bytes(len_bytes) as usize;
                offset += 4;
                if offset + len > data.len() { break; }
                let a = data[offset..offset + len].to_vec();
                args.push(WaylandArg::Array(a));
                offset += (len + 3) & !3;
            }
            _ => {}
        }
    }
    (args, offset)
}

pub struct ObjectRegistry {
    pub objects: Vec<Option<Box<dyn WaylandObject>>>,
}

impl ObjectRegistry {
    pub fn new() -> Self {
        let mut reg = Self {
            objects: Vec::new(),
        };
        reg.set(1, Box::new(objects::WlDisplay::new()));
        reg
    }
    pub fn get(&self, id: u32) -> Option<&(dyn WaylandObject + 'static)> {
        if id == 0 || id as usize > self.objects.len() {
            return None;
        }
        self.objects[id as usize - 1].as_deref()
    }
    pub fn get_mut(&mut self, id: u32) -> Option<&mut (dyn WaylandObject + 'static)> {
        if id == 0 || id as usize > self.objects.len() {
            return None;
        }
        self.objects[id as usize - 1].as_deref_mut()
    }
    pub fn set(&mut self, id: u32, obj: Box<dyn WaylandObject>) {
        let idx = id as usize - 1;
        if idx >= self.objects.len() {
            self.objects.resize_with(idx + 1, || None);
        }
        self.objects[idx] = Some(obj);
    }
    pub fn take(&mut self, id: u32) -> Option<Box<dyn WaylandObject>> {
        let idx = id as usize - 1;
        if idx < self.objects.len() {
            self.objects[idx].take()
        } else {
            None
        }
    }
}

pub mod objects {
    use super::*;

    pub struct WlDisplay {
        pub callback_id: u32,
    }
    impl WlDisplay {
        pub fn new() -> Self {
            Self { callback_id: 0 }
        }
    }
    impl WaylandObject for WlDisplay {
        fn interface_name(&self) -> &'static str { "wl_display" }
        fn version(&self) -> u32 { 1 }
        fn handle_request(&mut self, conn: &mut WaylandConnection, _id: u32, opcode: u16, args: &[u8]) -> Result<(), WaylandError> {
            match opcode {
                0 => { // sync(callback = new_id)
                    let (decoded, _) = decode_args("n", args);
                    if let Some(WaylandArg::NewId(callback_id)) = decoded.get(0) {
                        self.callback_id = *callback_id;
                        conn.registry.set(*callback_id, Box::new(WlCallback { done: false }));
                        // Emit done event immediately for now (simple sync)
                        let mut event_args = Vec::new();
                        event_args.extend_from_slice(&0u32.to_le_bytes()); // serial
                        conn.send_event(*callback_id, 0, &event_args);
                    }
                }
                1 => { // get_registry(registry = new_id)
                    let (decoded, _) = decode_args("n", args);
                    if let Some(WaylandArg::NewId(registry_id)) = decoded.get(0) {
                        let id = *registry_id;
                        conn.registry.set(id, Box::new(WlRegistry));
                        
                        // Send globals
                        // 1. wl_compositor
                        let mut ev = Vec::new();
                        ev.extend_from_slice(&1u32.to_le_bytes()); // name (arbitrary index)
                        let ifname = b"wl_compositor\0";
                        ev.extend_from_slice(&(ifname.len() as u32).to_le_bytes());
                        ev.extend_from_slice(ifname);
                        while ev.len() % 4 != 0 { ev.push(0); }
                        ev.extend_from_slice(&4u32.to_le_bytes()); // version
                        conn.send_event(id, 0, &ev);

                        // 2. wl_shm
                        let mut ev = Vec::new();
                        ev.extend_from_slice(&2u32.to_le_bytes()); // name
                        let ifname = b"wl_shm\0";
                        ev.extend_from_slice(&(ifname.len() as u32).to_le_bytes());
                        ev.extend_from_slice(ifname);
                        while ev.len() % 4 != 0 { ev.push(0); }
                        ev.extend_from_slice(&1u32.to_le_bytes()); // version
                        conn.send_event(id, 0, &ev);

                        // 3. wl_shell
                        let mut ev = Vec::new();
                        ev.extend_from_slice(&3u32.to_le_bytes()); // name
                        let ifname = b"wl_shell\0";
                        ev.extend_from_slice(&(ifname.len() as u32).to_le_bytes());
                        ev.extend_from_slice(ifname);
                        while ev.len() % 4 != 0 { ev.push(0); }
                        ev.extend_from_slice(&1u32.to_le_bytes()); // version
                        conn.send_event(id, 0, &ev);
                        
                        // 4. xdg_wm_base
                        let mut ev = Vec::new();
                        ev.extend_from_slice(&4u32.to_le_bytes()); // name
                        let ifname = b"xdg_wm_base\0";
                        ev.extend_from_slice(&(ifname.len() as u32).to_le_bytes());
                        ev.extend_from_slice(ifname);
                        while ev.len() % 4 != 0 { ev.push(0); }
                        ev.extend_from_slice(&1u32.to_le_bytes()); // version
                        conn.send_event(id, 0, &ev);
                    }
                }
                _ => return Err(WaylandError::UnknownOpcode(opcode)),
            }
            Ok(())
        }
    }

    pub struct WlCallback { pub done: bool }
    impl WaylandObject for WlCallback {
        fn interface_name(&self) -> &'static str { "wl_callback" }
        fn version(&self) -> u32 { 1 }
        fn handle_request(&mut self, _conn: &mut WaylandConnection, _id: u32, _opcode: u16, _args: &[u8]) -> Result<(), WaylandError> { Ok(()) }
    }

    pub struct WlRegistry;
    impl WaylandObject for WlRegistry {
        fn interface_name(&self) -> &'static str { "wl_registry" }
        fn version(&self) -> u32 { 1 }
        fn handle_request(&mut self, conn: &mut WaylandConnection, _id: u32, opcode: u16, args: &[u8]) -> Result<(), WaylandError> {
            match opcode {
                0 => { // bind(name, interface, version, id = new_id)
                    let (decoded, _) = decode_args("usun", args);
                    if let (Some(WaylandArg::Uint(_name)), Some(WaylandArg::String(iface)), Some(WaylandArg::NewId(new_id))) =
                        (decoded.get(0), decoded.get(1), decoded.get(3))
                    {
                        match iface.as_slice() {
                            b"wl_compositor" => {
                                conn.registry.set(*new_id, Box::new(WlCompositor));
                            }
                            b"wl_shm" => {
                                conn.registry.set(*new_id, Box::new(WlShm));
                                // Inform supported formats
                                let mut fmt = Vec::new();
                                fmt.extend_from_slice(&0u32.to_le_bytes()); // ARGB8888
                                conn.send_event(*new_id, 0, &fmt);
                                let mut fmt = Vec::new();
                                fmt.extend_from_slice(&1u32.to_le_bytes()); // XRGB8888
                                conn.send_event(*new_id, 0, &fmt);
                            }
                            b"wl_shell" => {
                                conn.registry.set(*new_id, Box::new(WlShell));
                            }
                            b"xdg_wm_base" => {
                                conn.registry.set(*new_id, Box::new(XdgWmBase));
                            }
                            b"zwp_linux_dmabuf_v1" => {
                                conn.registry.set(*new_id, Box::new(ZwpLinuxDmabufV1));
                                // Anunciar formatos soportados (modifier_hi=0, modifier_lo=0 para linear)
                                let format_xrgb: u32 = 0x34325258; // XRGB8888 (Little Endian 'XR24')
                                let format_argb: u32 = 0x34325241; // ARGB8888 ('AR24')
                                
                                let mut args = Vec::new();
                                args.extend_from_slice(&format_argb.to_le_bytes());
                                args.extend_from_slice(&0u32.to_le_bytes()); // mod_hi
                                args.extend_from_slice(&0u32.to_le_bytes()); // mod_lo
                                conn.send_event(*new_id, 0, &args); // format event
                                
                                let mut args = Vec::new();
                                args.extend_from_slice(&format_xrgb.to_le_bytes());
                                args.extend_from_slice(&0u32.to_le_bytes()); // mod_hi
                                args.extend_from_slice(&0u32.to_le_bytes()); // mod_lo
                                conn.send_event(*new_id, 0, &args);
                            }
                            _ => {}
                        }
                    }
                }
                _ => return Err(WaylandError::UnknownOpcode(opcode)),
            }
            Ok(())
        }
    }

    pub struct WlCompositor;
    impl WaylandObject for WlCompositor {
        fn interface_name(&self) -> &'static str { "wl_compositor" }
        fn version(&self) -> u32 { 4 }
        fn handle_request(&mut self, conn: &mut WaylandConnection, _id: u32, opcode: u16, args: &[u8]) -> Result<(), WaylandError> {
            match opcode {
                0 => { // create_surface(id = new_id)
                    let (decoded, _) = decode_args("n", args);
                    if let Some(WaylandArg::NewId(new_id)) = decoded.get(0) {
                        conn.registry.set(*new_id, Box::new(WlSurface { pending_buffer: None, damage: Vec::new() }));
                    }
                }
                _ => return Err(WaylandError::UnknownOpcode(opcode)),
            }
            Ok(())
        }
    }

    pub struct WlSurface {
        pub pending_buffer: Option<u32>,
        pub damage: Vec<(i32, i32, i32, i32)>,
    }
    impl WaylandObject for WlSurface {
        fn interface_name(&self) -> &'static str { "wl_surface" }
        fn version(&self) -> u32 { 4 }
        fn handle_request(&mut self, conn: &mut WaylandConnection, id: u32, opcode: u16, args: &[u8]) -> Result<(), WaylandError> {
            match opcode {
                1 => { // attach(buffer, x, y)
                    let (decoded, _) = decode_args("oii", args);
                    if let Some(WaylandArg::Object(buf_id)) = decoded.get(0) {
                        self.pending_buffer = Some(*buf_id);
                    }
                }
                2 | 9 => { // damage(x, y, w, h) | damage_buffer(x, y, w, h)
                    let (decoded, _) = decode_args("iiii", args);
                    if let (Some(WaylandArg::Int(x)), Some(WaylandArg::Int(y)), Some(WaylandArg::Int(w)), Some(WaylandArg::Int(h))) =
                        (decoded.get(0), decoded.get(1), decoded.get(2), decoded.get(3))
                    {
                        self.damage.push((*x, *y, *w, *h));
                    }
                }
                6 => { // commit()
                    conn.internal_events.push_back(WaylandInternalEvent::SurfaceCommitted {
                        surface_id: id,
                        buffer_id: self.pending_buffer,
                        damage: core::mem::take(&mut self.damage),
                    });
                }
                _ => {}
            }
            Ok(())
        }
        fn as_surface_pending_buffer(&self) -> Option<u32> { self.pending_buffer }
    }

    pub struct WlShm;
    impl WaylandObject for WlShm {
        fn interface_name(&self) -> &'static str { "wl_shm" }
        fn version(&self) -> u32 { 1 }
        fn handle_request(&mut self, conn: &mut WaylandConnection, _id: u32, opcode: u16, args: &[u8]) -> Result<(), WaylandError> {
            match opcode {
                0 => { // create_pool(id = new_id, fd, size)
                    let (decoded, _) = decode_args("nhi", args);
                    if let (Some(WaylandArg::NewId(new_id)), Some(WaylandArg::Fd(fd)), Some(WaylandArg::Int(size))) =
                        (decoded.get(0), decoded.get(1), decoded.get(2))
                    {
                        conn.registry.set(*new_id, Box::new(WlShmPool { fd: *fd, size: *size }));
                    }
                }
                _ => return Err(WaylandError::UnknownOpcode(opcode)),
            }
            Ok(())
        }
    }

    pub struct WlShmPool { pub fd: i32, pub size: i32 }
    impl WaylandObject for WlShmPool {
        fn interface_name(&self) -> &'static str { "wl_shm_pool" }
        fn version(&self) -> u32 { 1 }
        fn handle_request(&mut self, conn: &mut WaylandConnection, _id: u32, opcode: u16, args: &[u8]) -> Result<(), WaylandError> {
            match opcode {
                0 => { // create_buffer(id = new_id, offset, width, height, stride, format)
                    let (decoded, _) = decode_args("niiiii", args);
                    if let (Some(WaylandArg::NewId(new_id)), Some(WaylandArg::Int(offset)), Some(WaylandArg::Int(width)), Some(WaylandArg::Int(height)), Some(WaylandArg::Int(stride)), Some(WaylandArg::Uint(format))) =
                        (decoded.get(0), decoded.get(1), decoded.get(2), decoded.get(3), decoded.get(4), decoded.get(5))
                    {
                         conn.registry.set(*new_id, Box::new(WlBuffer {
                             offset: *offset,
                             width: *width,
                             height: *height,
                             stride: *stride,
                             format: *format,
                             shm_pool_fd: self.fd,
                             pool_id: _id, // _id is the pool_id
                         }));
                    }
                }
                _ => return Err(WaylandError::UnknownOpcode(opcode)),
            }
            Ok(())
        }
    }

    pub struct WlBuffer {
        pub offset: i32,
        pub width: i32,
        pub height: i32,
        pub stride: i32,
        pub format: u32,
        pub shm_pool_fd: i32,
        pub pool_id: u32,
    }
    impl WaylandObject for WlBuffer {
        fn interface_name(&self) -> &'static str { "wl_buffer" }
        fn version(&self) -> u32 { 1 }
        fn handle_request(&mut self, _conn: &mut WaylandConnection, _id: u32, _opcode: u16, _args: &[u8]) -> Result<(), WaylandError> { Ok(()) }
        fn as_buffer(&self) -> Option<ShmBufferInfo> {
            Some(ShmBufferInfo {
                offset: self.offset,
                width: self.width,
                height: self.height,
                stride: self.stride,
                format: self.format,
                shm_pool_fd: self.shm_pool_fd,
                pool_id: self.pool_id,
            })
        }
    }

    pub struct WlShell;
    impl WaylandObject for WlShell {
        fn interface_name(&self) -> &'static str { "wl_shell" }
        fn version(&self) -> u32 { 1 }
        fn handle_request(&mut self, conn: &mut WaylandConnection, _id: u32, opcode: u16, args: &[u8]) -> Result<(), WaylandError> {
            match opcode {
                0 => { // get_shell_surface(id = new_id, surface)
                    let (decoded, _) = decode_args("no", args);
                    if let (Some(WaylandArg::NewId(new_id)), Some(WaylandArg::Object(surface_id))) =
                        (decoded.get(0), decoded.get(1))
                    {
                        conn.registry.set(*new_id, Box::new(WlShellSurface { surface_id: *surface_id }));
                        conn.internal_events.push_back(WaylandInternalEvent::ShellSurfaceCreated {
                            surface_id: *surface_id,
                            shell_surface_id: *new_id,
                        });
                    }
                }
                _ => return Err(WaylandError::UnknownOpcode(opcode)),
            }
            Ok(())
        }
    }

    pub struct WlShellSurface { pub surface_id: u32 }
    impl WaylandObject for WlShellSurface {
        fn interface_name(&self) -> &'static str { "wl_shell_surface" }
        fn version(&self) -> u32 { 1 }
        fn handle_request(&mut self, _conn: &mut WaylandConnection, _id: u32, opcode: u16, _args: &[u8]) -> Result<(), WaylandError> {
            match opcode {
                1 => { // set_toplevel()
                    // nothing to do here, but we could trigger a "Map" event
                }
                _ => {}
            }
            Ok(())
        }
    }

    pub struct XdgWmBase;
    impl WaylandObject for XdgWmBase {
        fn interface_name(&self) -> &'static str { "xdg_wm_base" }
        fn version(&self) -> u32 { 1 }
        fn handle_request(&mut self, conn: &mut WaylandConnection, _id: u32, opcode: u16, args: &[u8]) -> Result<(), WaylandError> {
            match opcode {
                0 => { // destroy
                    // handled by registry removal if needed
                }
                1 => { // create_positioner(id)
                }
                2 => { // get_xdg_surface(id, wl_surface)
                    let (decoded, _) = decode_args("no", args);
                    if let (Some(WaylandArg::NewId(new_id)), Some(WaylandArg::Object(surface_id))) =
                        (decoded.get(0), decoded.get(1))
                    {
                        conn.registry.set(*new_id, Box::new(XdgSurface { surface_id: *surface_id }));
                        conn.internal_events.push_back(WaylandInternalEvent::XdgSurfaceCreated {
                            surface_id: *surface_id,
                            xdg_surface_id: *new_id,
                        });
                    }
                }
                3 => { // pong(serial)
                }
                _ => return Err(WaylandError::UnknownOpcode(opcode)),
            }
            Ok(())
        }
    }

    pub struct XdgSurface { pub surface_id: u32 }
    impl WaylandObject for XdgSurface {
        fn interface_name(&self) -> &'static str { "xdg_surface" }
        fn version(&self) -> u32 { 1 }
        fn handle_request(&mut self, conn: &mut WaylandConnection, id: u32, opcode: u16, args: &[u8]) -> Result<(), WaylandError> {
            match opcode {
                0 => { // destroy
                }
                1 => { // get_toplevel(id)
                    let (decoded, _) = decode_args("n", args);
                    if let Some(WaylandArg::NewId(new_id)) = decoded.get(0) {
                        conn.registry.set(*new_id, Box::new(XdgToplevel { xdg_surface_id: id }));
                        conn.internal_events.push_back(WaylandInternalEvent::XdgToplevelCreated {
                            xdg_surface_id: id,
                            toplevel_id: *new_id,
                            surface_id: self.surface_id,
                        });
                        
                        // Enviar configure inmediatamente para que la app sepa su tamaño inicial
                        let mut ev = Vec::new();
                        ev.extend_from_slice(&0i32.to_le_bytes()); // width (0 = compositor choice)
                        ev.extend_from_slice(&0i32.to_le_bytes()); // height
                        ev.extend_from_slice(&1u32.to_le_bytes()); // array length for states
                        ev.extend_from_slice(&0u32.to_le_bytes()); // dummy state
                        conn.send_event(*new_id, 0, &ev); // xdg_toplevel.configure
                        
                        // xdg_surface.configure(serial)
                        let mut ev = Vec::new();
                        ev.extend_from_slice(&0u32.to_le_bytes()); // serial
                        conn.send_event(id, 0, &ev);
                    }
                }
                2 => { // get_popup
                }
                3 => { // set_window_geometry
                }
                4 => { // ack_configure(serial)
                }
                _ => return Err(WaylandError::UnknownOpcode(opcode)),
            }
            Ok(())
        }
    }

    pub struct XdgToplevel { pub xdg_surface_id: u32 }
    impl WaylandObject for XdgToplevel {
        fn interface_name(&self) -> &'static str { "xdg_toplevel" }
        fn version(&self) -> u32 { 1 }
        fn handle_request(&mut self, _conn: &mut WaylandConnection, _id: u32, opcode: u16, _args: &[u8]) -> Result<(), WaylandError> {
            match opcode {
                0 => { // destroy
                }
                1 => { // set_parent
                }
                2 => { // set_title
                }
                3 => { // set_app_id
                }
                4 => { // show_window_menu
                }
                5 => { // move
                }
                6 => { // resize
                }
                7 => { // set_max_size
                }
                8 => { // set_min_size
                }
                9 => { // set_maximized
                }
                10 => { // unset_maximized
                }
                11 => { // set_fullscreen
                }
                12 => { // unset_fullscreen
                }
                13 => { // set_minimized
                }
                _ => {} // Ignore others for now
            }
            Ok(())
        }
    }

    pub struct ZwpLinuxDmabufV1;
    impl WaylandObject for ZwpLinuxDmabufV1 {
        fn interface_name(&self) -> &'static str { "zwp_linux_dmabuf_v1" }
        fn version(&self) -> u32 { 3 }
        fn handle_request(&mut self, conn: &mut WaylandConnection, _id: u32, opcode: u16, args: &[u8]) -> Result<(), WaylandError> {
            match opcode {
                0 => {} // destroy
                1 => { // create_params(id = new_id)
                    let (decoded, _) = decode_args("n", args);
                    if let Some(WaylandArg::NewId(new_id)) = decoded.get(0) {
                        conn.registry.set(*new_id, Box::new(ZwpLinuxBufferParamsV1::new()));
                    }
                }
                _ => return Err(WaylandError::UnknownOpcode(opcode)),
            }
            Ok(())
        }
    }

    pub struct ZwpLinuxBufferParamsV1 {
        pub planes: Vec<(u32, u32, u32, u32, u32, u32)>, // handle, idx, offset, stride, mod_hi, mod_lo
    }
    impl ZwpLinuxBufferParamsV1 {
        pub fn new() -> Self { Self { planes: Vec::new() } }
    }
    impl WaylandObject for ZwpLinuxBufferParamsV1 {
        fn interface_name(&self) -> &'static str { "zwp_linux_buffer_params_v1" }
        fn version(&self) -> u32 { 3 }
        fn handle_request(&mut self, conn: &mut WaylandConnection, _id: u32, opcode: u16, args: &[u8]) -> Result<(), WaylandError> {
            match opcode {
                0 => {} // destroy
                1 => { // add(fd, idx, offset, stride, mod_hi, mod_lo)
                    let (decoded, _) = decode_args("huuuu", args); // Note: Wayland 'h' is Fd, we use it as handle ID in Eclipse
                    if let (Some(WaylandArg::Fd(fd)), Some(WaylandArg::Uint(idx)), Some(WaylandArg::Uint(off)), Some(WaylandArg::Uint(stride)), Some(WaylandArg::Uint(mhi)), Some(WaylandArg::Uint(mlo))) =
                        (decoded.get(0), decoded.get(1), decoded.get(2), decoded.get(3), decoded.get(4), decoded.get(5))
                    {
                        self.planes.push((*fd as u32, *idx, *off, *stride, *mhi, *mlo));
                    }
                }
                2 | 3 => { // create / create_immed (id, width, height, format, flags)
                    let sig = if opcode == 2 { "iiuu" } else { "niiuu" };
                    let (decoded, _) = decode_args(sig, args);
                    let (new_id, w, h, fmt) = if opcode == 3 {
                        (decoded.get(0), decoded.get(1), decoded.get(2), decoded.get(3))
                    } else {
                        // create (opcode 2) expects event 'created' with new buffer, but for MVP we prefer immed
                        (None, decoded.get(0), decoded.get(1), decoded.get(2))
                    };
                    
                    if let (Some(WaylandArg::NewId(id)), Some(WaylandArg::Int(width)), Some(WaylandArg::Int(height)), Some(WaylandArg::Uint(format))) =
                        (new_id, w, h, fmt)
                    {
                        if let Some(plane) = self.planes.get(0) {
                            conn.registry.set(*id, Box::new(DmabufBuffer {
                                handle: plane.0,
                                width: *width,
                                height: *height,
                                format: *format,
                            }));
                        }
                    }
                }
                _ => {}
            }
            Ok(())
        }
    }

    pub struct DmabufBuffer {
        pub handle: u32,
        pub width: i32,
        pub height: i32,
        pub format: u32,
    }
    impl WaylandObject for DmabufBuffer {
        fn interface_name(&self) -> &'static str { "wl_buffer" }
        fn version(&self) -> u32 { 1 }
        fn handle_request(&mut self, _conn: &mut WaylandConnection, _id: u32, opcode: u16, _args: &[u8]) -> Result<(), WaylandError> {
            match opcode {
                0 => {} // destroy
                _ => {}
            }
            Ok(())
        }
        fn as_dmabuf_buffer(&self) -> Option<DmabufBufferInfo> {
            Some(DmabufBufferInfo {
                handle: self.handle,
                width: self.width,
                height: self.height,
                format: self.format,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wayland::objects::WlSurface;

    #[test]
    fn test_surface_damage_accumulation() {
        let mut conn = WaylandConnection::new();
        let mut surface = WlSurface { pending_buffer: None, damage: Vec::new() };
        
        // 1. Send damage: (10, 10, 50, 50)
        let damage1 = [10i32, 10, 50, 50];
        let mut args1 = Vec::new();
        for &v in &damage1 { args1.extend_from_slice(&v.to_le_bytes()); }
        surface.handle_request(&mut conn, 1, 2, &args1).expect("damage 1 failed");
        
        // 2. Send damage_buffer: (20, 20, 100, 100)
        let damage2 = [20i32, 20, 100, 100];
        let mut args2 = Vec::new();
        for &v in &damage2 { args2.extend_from_slice(&v.to_le_bytes()); }
        surface.handle_request(&mut conn, 1, 9, &args2).expect("damage 2 failed");

        assert_eq!(surface.damage.len(), 2);
        assert_eq!(surface.damage[0], (10, 10, 50, 50));
        assert_eq!(surface.damage[1], (20, 20, 100, 100));

        // 3. Commit
        surface.handle_request(&mut conn, 1, 6, &[]).expect("commit failed");
        
        // 4. Verify event contains damage
        let ev = conn.internal_events.pop_front().expect("no event found");
        if let WaylandInternalEvent::SurfaceCommitted { surface_id, damage, .. } = ev {
            assert_eq!(surface_id, 1);
            assert_eq!(damage.len(), 2);
            assert_eq!(damage[0], (10, 10, 50, 50));
            assert_eq!(damage[1], (20, 20, 100, 100));
        } else {
            panic!("wrong event type");
        }

        // 5. Verify surface damage is cleared
        assert!(surface.damage.is_empty());
    }
}
