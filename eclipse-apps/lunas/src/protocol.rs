use std::prelude::v1::*;
use std::rc::Rc;
use core::cell::RefCell;
use std::collections::BTreeMap;
use wayland_proto::wl::{ObjectId, NewId, Payload};
use wayland_proto::wl::wire::Handle;
use wayland_proto::wl::server::client::{Client, ClientId};
use wayland_proto::wl::server::objects::{Object, ObjectInner, ObjectLogic, ServerError};
use wayland_proto::wl::protocols::common::*;
use crate::compositor::{ShellWindow, WindowContent};

use libc::{open, mmap, munmap, close, PROT_READ, PROT_WRITE, MAP_SHARED, O_RDWR, O_NONBLOCK};

// ────────────────────────────────────────────────────────────────────────────
// Shared data types
// ────────────────────────────────────────────────────────────────────────────

/// Metadata for a wl_buffer backed by a shared-memory pool.
#[derive(Clone, Copy, Debug)]
pub struct BufferInfo {
    /// Virtual address in the compositor's address space.
    pub vaddr: usize,
    pub width: u32,
    pub height: u32,
    /// Row stride in bytes.
    pub stride: u32,
    /// Pixel format (e.g. WL_SHM_FORMAT_XRGB8888 = 1).
    pub format: u32,
}

/// Pending surface commit: posted by LunasSurface on commit and drained by
/// LunasState to create / update ShellWindows.
pub struct SurfaceCommit {
    pub pid: u32,
    pub surface_id: u32,
    pub vaddr: usize,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

/// Shared registry of live wl_buffer objects, keyed by ObjectId.
pub type SharedBuffers = Rc<RefCell<BTreeMap<ObjectId, BufferInfo>>>;

/// Shared list of pending surface commits.
pub type SharedCommits = Rc<RefCell<std::vec::Vec<SurfaceCommit>>>;

/// Shared mapping of Xwayland serials (64-bit) to wl_surface ObjectIds.
pub type SharedXwaylandSerials = Rc<RefCell<BTreeMap<u64, ObjectId>>>;


// ────────────────────────────────────────────────────────────────────────────
// LunasCompositor  (wl_compositor)
// ────────────────────────────────────────────────────────────────────────────

pub struct LunasCompositor {
    pub pending_commits: SharedCommits,
    pub buffer_registry: SharedBuffers,
}

impl ObjectLogic for LunasCompositor {
    fn handle_request(
        &mut self,
        client: &mut Client,
        opcode: u16,
        args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => {
                // create_surface(id: new_id)
                let id = match args.first() {
                    Some(Payload::NewId(id)) => *id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let surface = ObjectInner::Rc(Rc::new(RefCell::new(LunasSurface {
                    id: id.as_id(),
                    pid: client.client_id().0,
                    attached_buffer: None,
                    pending_commits: self.pending_commits.clone(),
                    buffer_registry: self.buffer_registry.clone(),
                })));
                client.add_object(id, Object::new::<wl_surface::WlSurface>(id, surface));
                Ok(())
            }
            _ => Err(ServerError::ObjectMismatch),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LunasSurface  (wl_surface)
// ────────────────────────────────────────────────────────────────────────────

pub struct LunasSurface {
    pub id: ObjectId,
    pub pid: u32,
    /// The buffer currently attached (set by opcode 1 = attach).
    pub attached_buffer: Option<BufferInfo>,
    pub pending_commits: SharedCommits,
    pub buffer_registry: SharedBuffers,
}

impl ObjectLogic for LunasSurface {
    fn handle_request(
        &mut self,
        _client: &mut Client,
        opcode: u16,
        args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => Ok(()), // destroy — nothing to do yet
            1 => {
                // attach(buffer: object_id, x: int, y: int)
                let buffer_id = match args.first() {
                    Some(Payload::ObjectId(id)) => *id,
                    _ => return Ok(()), // null buffer is valid (detach)
                };
                self.attached_buffer = (*self.buffer_registry).borrow().get(&buffer_id).copied();
                Ok(())
            }
            2 | 9 => Ok(()), // damage / damage_buffer — ignore for now
            3 => Ok(()), // frame — callback not implemented yet
            4 | 5 => Ok(()), // set_opaque_region / set_input_region — ignore
            6 => {
                // commit — publish a new frame
                if let Some(info) = self.attached_buffer {
                    (*self.pending_commits).borrow_mut().push(SurfaceCommit {
                        pid: self.pid,
                        surface_id: self.id.0,
                        vaddr: info.vaddr,
                        width: info.width,
                        height: info.height,
                        stride: info.stride,
                    });
                }
                Ok(())
            }
            7 | 8 => Ok(()), // set_buffer_transform / set_buffer_scale — ignore
            _ => Ok(()),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LunasShm  (wl_shm)
// ────────────────────────────────────────────────────────────────────────────

pub struct LunasShm {
    pub buffer_registry: SharedBuffers,
}

impl ObjectLogic for LunasShm {
    fn handle_request(
        &mut self,
        client: &mut Client,
        opcode: u16,
        args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => {
                // create_pool(id: new_id, fd: handle, size: int)
                //
                // Wire layout per PAYLOAD_TYPES: [NewId, Handle, Int]
                // The Handle is inserted into args[1] by RawMessage::deserialize
                // (it pulls it out of the SCM_RIGHTS handles slice).
                let id = match args.first() {
                    Some(Payload::NewId(id)) => *id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let fd = match args.get(1) {
                    Some(Payload::Handle(h)) => h.0,
                    got => {
                        // Log what we actually got to diagnose the mismatch.
                        let msg = alloc::format!("[LUNAS-SHM] create_pool: args[1] is not Handle: {:?}\n", got);
                        unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len()); }
                        -1
                    },
                };
                let size = match args.get(2) {
                    Some(Payload::Int(s)) => *s as usize,
                    got => {
                        let msg = alloc::format!("[LUNAS-SHM] create_pool: args[2] is not Int: {:?} (args={:?})\n", got, args);
                        unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len()); }
                        return Err(ServerError::MessageDeserializeError);
                    },
                };

                let vaddr = if fd >= 0 {
                    // Standard Wayland: mmap the received shm fd directly.
                    let v = map_shm_fd(fd, size);
                    {
                        let msg = alloc::format!("[LUNAS-SHM] create_pool: fd={} size={} vaddr=0x{:x}\n", fd, size, v);
                        unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len()); }
                    }
                    v
                } else {
                    // No fd (Handle(-1)) — Eclipse OS doesn't support SCM_RIGHTS.
                    // Try the PID-based path first (Eclipse IPC clients), then scan
                    // /tmp/twb_* to find the terminal's shared buffer.
                    let pid = client.client_id().0;
                    let v = if pid < 0x8000_0000 {
                        // Eclipse IPC client: pid IS the process pid.
                        let v = map_shm_file(pid, size);
                        let msg = alloc::format!("[LUNAS-SHM] create_pool fallback IPC: pid={} vaddr=0x{:x}\n", pid, v);
                        unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len()); }
                        v
                    } else {
                        // Unix socket client with no fd: scan /tmp/twb_{1..64}.
                        scan_twb_files(size)
                    };
                    v
                };

                let pool = ObjectInner::Rc(Rc::new(RefCell::new(LunasShmPool {
                    vaddr,
                    size,
                    buffer_registry: self.buffer_registry.clone(),
                })));
                client.add_object(id, Object::new::<wl_shm::WlShmPool>(id, pool));
                Ok(())
            }
            _ => Err(ServerError::ObjectMismatch),
        }
    }
}

/// Open and mmap `/tmp/twb_{pid}` into the compositor's address space.
/// Returns 0 on failure (pool will be inactive).
fn map_shm_file(pid: u32, size: usize) -> usize {
    // Build null-terminated path: "/tmp/twb_NNNNN\0"
    let mut path = [0u8; 32];
    let prefix = b"/tmp/twb_";
    path[..prefix.len()].copy_from_slice(prefix);
    let mut n = pid;
    let mut tmp = [0u8; 10];
    let mut i = 0usize;
    if n == 0 {
        tmp[0] = b'0';
        i = 1;
    } else {
        while n > 0 && i < tmp.len() {
            tmp[i] = b'0' + (n % 10) as u8;
            n /= 10;
            i += 1;
        }
    }
    // reverse digit string
    let mut lo = 0usize;
    let mut hi = i.saturating_sub(1);
    while lo < hi {
        tmp.swap(lo, hi);
        lo += 1;
        hi -= 1;
    }
    let end = prefix.len() + i;
    if end >= path.len() {
        return 0;
    }
    path[prefix.len()..end].copy_from_slice(&tmp[..i]);
    // path[end] is already 0 (null terminator)

    let fd = unsafe {
        open(
            path.as_ptr() as *const core::ffi::c_char,
            O_RDWR | O_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return 0;
    }
    let vaddr = unsafe {
        mmap(
            core::ptr::null_mut(),
            size,
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            fd,
            0,
        )
    };
    unsafe { close(fd) };
    if vaddr.is_null() || vaddr == libc::MAP_FAILED {
        0
    } else {
        vaddr as usize
    }
}

/// Scan /tmp/twb_{2..64} and mmap the first file found.
/// Used when Handle fd=-1 (Eclipse OS Unix sockets don't carry SCM_RIGHTS ancilla data).
fn scan_twb_files(size: usize) -> usize {
    let prefix = b"/tmp/twb_";
    for pid in 2u32..=64 {
        let mut path = [0u8; 32];
        path[..prefix.len()].copy_from_slice(prefix);
        let mut n = pid;
        let mut tmp_digits = [0u8; 10];
        let mut i = 0usize;
        if n == 0 { tmp_digits[0] = b'0'; i = 1; } else {
            while n > 0 && i < tmp_digits.len() {
                tmp_digits[i] = b'0' + (n % 10) as u8; n /= 10; i += 1;
            }
        }
        let mut lo = 0usize; let mut hi = i.saturating_sub(1);
        while lo < hi { tmp_digits.swap(lo, hi); lo += 1; hi -= 1; }
        let end = prefix.len() + i;
        if end >= path.len() { continue; }
        path[prefix.len()..end].copy_from_slice(&tmp_digits[..i]);

        let fd = unsafe { open(path.as_ptr() as *const core::ffi::c_char, O_RDWR | O_NONBLOCK, 0) };
        if fd < 0 { continue; }
        let vaddr = unsafe {
            mmap(core::ptr::null_mut(), size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0)
        };
        unsafe { close(fd) };
        if !vaddr.is_null() && vaddr != libc::MAP_FAILED {
            let msg = alloc::format!(
                "[LUNAS-SHM] scan_twb: found /tmp/twb_{} size={} vaddr=0x{:x}\n",
                pid, size, vaddr as usize
            );
            unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len()); }
            return vaddr as usize;
        }
    }
    let msg = b"[LUNAS-SHM] scan_twb: no twb file found!\n";
    unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len()); }
    0
}

/// Map a shared-memory fd (received via SCM_RIGHTS in wl_shm.create_pool) into
/// the compositor's address space.  Closes the fd after mapping (mmap retains
/// a reference to the underlying file object).  Returns 0 on failure.
fn map_shm_fd(fd: i32, size: usize) -> usize {
    if fd < 0 || size == 0 { return 0; }
    let vaddr = unsafe {
        mmap(
            core::ptr::null_mut(),
            size,
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            fd,
            0,
        )
    };
    let _ = unsafe { close(fd) };
    if vaddr.is_null() || vaddr == libc::MAP_FAILED {
        0
    } else {
        vaddr as usize
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LunasShmPool  (wl_shm_pool)
// ────────────────────────────────────────────────────────────────────────────

pub struct LunasShmPool {
    pub vaddr: usize,
    pub size: usize,
    pub buffer_registry: SharedBuffers,
}

impl ObjectLogic for LunasShmPool {
    fn handle_request(
        &mut self,
        client: &mut Client,
        opcode: u16,
        args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => {
                // create_buffer(id, offset, width, height, stride, format)
                let id = match args.first() {
                    Some(Payload::NewId(id)) => *id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let offset = match args.get(1) {
                    Some(Payload::Int(v)) => *v as usize,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let width = match args.get(2) {
                    Some(Payload::Int(v)) => *v as u32,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let height = match args.get(3) {
                    Some(Payload::Int(v)) => *v as u32,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let stride = match args.get(4) {
                    Some(Payload::Int(v)) => *v as u32,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let format = match args.get(5) {
                    Some(Payload::UInt(v)) => *v,
                    _ => return Err(ServerError::MessageDeserializeError),
                };

                let buf_vaddr = if self.vaddr != 0 { self.vaddr + offset } else { 0 };
                let info = BufferInfo { vaddr: buf_vaddr, width, height, stride, format };
                (*self.buffer_registry).borrow_mut().insert(id.as_id(), info);

                let buf_obj = ObjectInner::Rc(Rc::new(RefCell::new(LunasBuffer { info })));
                client.add_object(id, Object::new::<wl_buffer::WlBuffer>(id, buf_obj));
                Ok(())
            }
            1 => Ok(()), // destroy
            2 => Ok(()), // resize — not implemented
            _ => Err(ServerError::ObjectMismatch),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LunasBuffer  (wl_buffer)
// ────────────────────────────────────────────────────────────────────────────

pub struct LunasBuffer {
    pub info: BufferInfo,
}

impl ObjectLogic for LunasBuffer {
    fn handle_request(
        &mut self,
        _client: &mut Client,
        opcode: u16,
        _args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => Ok(()), // destroy — buffer registry cleanup could happen here
            _ => Err(ServerError::ObjectMismatch),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Helper to build a ShellWindow for a Wayland surface (used externally).
// ────────────────────────────────────────────────────────────────────────────

pub fn make_wayland_window(
    surface_id: ObjectId,
    workspace: u8,
    title: &str,
) -> ShellWindow {
    let x = 120;
    let y = 120;
    let w = 640;
    let h = 480;
    let mut title_buf = [0u8; 32];
    let copy = title.len().min(31);
    title_buf[..copy].copy_from_slice(&title.as_bytes()[..copy]);
    ShellWindow {
        x, y, w, h: h + ShellWindow::TITLE_H,
        curr_x: (x + w / 2) as f32,
        curr_y: (y + (h + ShellWindow::TITLE_H) / 2) as f32,
        curr_w: 0.0, curr_h: 0.0,
        content: WindowContent::Snp { surface_id: surface_id.0, pid: 0 },
        workspace,
        title: title_buf,
        ..Default::default()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Shared keyboard registry
// ────────────────────────────────────────────────────────────────────────────

/// Maps `ClientId → wl_keyboard ObjectId` so the compositor can send key events.
pub type SharedKeyboards = Rc<RefCell<BTreeMap<ClientId, ObjectId>>>;

/// Maps `ClientId → wl_pointer ObjectId` for mouse event dispatch.
pub type SharedPointers = Rc<RefCell<BTreeMap<ClientId, ObjectId>>>;

// ────────────────────────────────────────────────────────────────────────────
// LunasXdgWmBase  (xdg_wm_base)
// ────────────────────────────────────────────────────────────────────────────

pub struct LunasXdgWmBase {
    pub pending_commits: SharedCommits,
    pub buffer_registry: SharedBuffers,
}

impl ObjectLogic for LunasXdgWmBase {
    fn handle_request(
        &mut self,
        client: &mut Client,
        opcode: u16,
        args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => Ok(()), // destroy
            1 => {
                // get_xdg_surface(id: new_id, surface: object_id)
                let id = match args.first() {
                    Some(Payload::NewId(id)) => *id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let surface_id = match args.get(1) {
                    Some(Payload::ObjectId(v)) => *v,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let xdg_surf = ObjectInner::Rc(Rc::new(RefCell::new(LunasXdgSurface {
                    id: id.as_id(),
                    surface_id,
                    pending_commits: self.pending_commits.clone(),
                    buffer_registry: self.buffer_registry.clone(),
                })));
                client.add_object(id, Object::new::<xdg_surface::XdgSurface>(id, xdg_surf));
                Ok(())
            }
            2 => Ok(()), // pong — ignored
            _ => Err(ServerError::ObjectMismatch),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LunasXdgSurface  (xdg_surface)
// ────────────────────────────────────────────────────────────────────────────

pub struct LunasXdgSurface {
    pub id: ObjectId,
    /// The underlying wl_surface this xdg_surface wraps.
    pub surface_id: ObjectId,
    pub pending_commits: SharedCommits,
    pub buffer_registry: SharedBuffers,
}

impl ObjectLogic for LunasXdgSurface {
    fn handle_request(
        &mut self,
        client: &mut Client,
        opcode: u16,
        args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => Ok(()), // destroy
            1 => {
                // get_toplevel(id: new_id)
                let id = match args.first() {
                    Some(Payload::NewId(id)) => *id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let toplevel = ObjectInner::Rc(Rc::new(RefCell::new(LunasXdgToplevel {
                    id: id.as_id(),
                    xdg_surface_id: self.id,
                    surface_id: self.surface_id,
                    title: std::string::String::from("Wayland Window"),
                    pending_commits: self.pending_commits.clone(),
                    buffer_registry: self.buffer_registry.clone(),
                })));
                client.add_object(id, Object::new::<xdg_toplevel::XdgToplevel>(id, toplevel));

                // Send initial configure sequence:
                // 1. xdg_toplevel.configure(0, 0, []) — client picks its own size
                let tl_cfg = xdg_toplevel::Event::Configure {
                    width: 0, height: 0,
                    states: wayland_proto::wl::wire::Array(std::vec::Vec::new()),
                };
                client.send_event(id.as_id(), tl_cfg)?;

                // 2. xdg_surface.configure(serial=1)
                let surf_cfg = xdg_surface::Event::Configure { serial: 1 };
                client.send_event(self.id, surf_cfg)?;

                Ok(())
            }
            2 => Ok(()), // get_popup — not implemented
            3 => Ok(()), // set_window_geometry — ignored for now
            4 => Ok(()), // ack_configure — acknowledged
            _ => Ok(()),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LunasXdgToplevel  (xdg_toplevel)
// ────────────────────────────────────────────────────────────────────────────

pub struct LunasXdgToplevel {
    pub id: ObjectId,
    pub xdg_surface_id: ObjectId,
    pub surface_id: ObjectId,
    pub title: std::string::String,
    pub pending_commits: SharedCommits,
    pub buffer_registry: SharedBuffers,
}

impl ObjectLogic for LunasXdgToplevel {
    fn handle_request(
        &mut self,
        _client: &mut Client,
        opcode: u16,
        args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => Ok(()), // destroy
            1 => Ok(()), // set_parent
            2 => {
                // set_title(title: string)
                if let Some(Payload::String(t)) = args.first() {
                    self.title = t.clone();
                }
                Ok(())
            }
            3 => Ok(()), // set_app_id
            _ => Ok(()),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LunasSeat  (wl_seat)
// ────────────────────────────────────────────────────────────────────────────

pub struct LunasSeat {
    pub keyboard_registry: SharedKeyboards,
    pub pointer_registry: SharedPointers,
    pub screen_w: u32,
    pub screen_h: u32,
}

impl ObjectLogic for LunasSeat {
    fn handle_request(
        &mut self,
        client: &mut Client,
        opcode: u16,
        args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => {
                // get_pointer(id: new_id)
                let id = match args.first() {
                    Some(Payload::NewId(id)) => *id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                (*self.pointer_registry).borrow_mut().insert(client.client_id(), id.as_id());
                let ptr_obj = ObjectInner::Rc(Rc::new(RefCell::new(LunasPointer {
                    id: id.as_id(),
                    client_id: client.client_id(),
                })));
                client.add_object(id, Object::new::<wl_pointer::WlPointer>(id, ptr_obj));
                Ok(())
            }
            1 => {
                // get_keyboard(id: new_id)
                let id = match args.first() {
                    Some(Payload::NewId(id)) => *id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                (*self.keyboard_registry).borrow_mut().insert(client.client_id(), id.as_id());
                let kb = ObjectInner::Rc(Rc::new(RefCell::new(LunasKeyboard {
                    id: id.as_id(),
                    client_id: client.client_id(),
                })));
                client.add_object(id, Object::new::<wl_keyboard::WlKeyboard>(id, kb));

                // Send keymap immediately: format=0 (no keymap), fd=-1, size=0
                // Note: wl_keyboard.keymap carries an fd as ancillary data; for
                // format=NO_KEYMAP we send fd=-1 which libwayland-client accepts.
                let keymap = wl_keyboard::Event::Keymap {
                    format: wl_keyboard::KEYMAP_FORMAT_NO_KEYMAP,
                    fd: wayland_proto::wl::wire::Handle(-1),
                    size: 0,
                };
                client.send_event(id.as_id(), keymap)?;

                // Send repeat info: disabled (rate=0)
                let repeat = wl_keyboard::Event::RepeatInfo { rate: 0, delay: 0 };
                client.send_event(id.as_id(), repeat)?;

                Ok(())
            }
            2 => Ok(()), // get_touch
            3 => Ok(()), // release
            _ => Err(ServerError::ObjectMismatch),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LunasPointer  (wl_pointer)
// ────────────────────────────────────────────────────────────────────────────

pub struct LunasPointer {
    pub id: ObjectId,
    pub client_id: ClientId,
}

impl ObjectLogic for LunasPointer {
    fn handle_request(
        &mut self,
        _client: &mut Client,
        opcode: u16,
        _args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => Ok(()), // set_cursor — ignored for now
            1 => Ok(()), // release
            _ => Err(ServerError::ObjectMismatch),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LunasKeyboard  (wl_keyboard)
// ────────────────────────────────────────────────────────────────────────────

pub struct LunasKeyboard {
    pub id: ObjectId,
    pub client_id: ClientId,
}

impl ObjectLogic for LunasKeyboard {
    fn handle_request(
        &mut self,
        _client: &mut Client,
        opcode: u16,
        _args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => Ok(()), // release
            _ => Err(ServerError::ObjectMismatch),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LunasOutput  (wl_output)
// ────────────────────────────────────────────────────────────────────────────

pub struct LunasOutput {
    pub screen_w: u32,
    pub screen_h: u32,
    pub refresh_mhz: i32,
}

impl ObjectLogic for LunasOutput {
    fn handle_request(
        &mut self,
        _client: &mut Client,
        opcode: u16,
        _args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => Ok(()), // release
            _ => Err(ServerError::ObjectMismatch),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LunasXwaylandShell  (xwayland_shell_v1)
// ────────────────────────────────────────────────────────────────────────────

pub struct LunasXwaylandShell {
    pub xwayland_serials: SharedXwaylandSerials,
}

impl ObjectLogic for LunasXwaylandShell {
    fn handle_request(
        &mut self,
        client: &mut Client,
        opcode: u16,
        args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => Ok(()), // destroy
            1 => {
                // get_xwayland_surface(id: new_id, surface: object_id)
                let id = match args.first() {
                    Some(Payload::NewId(id)) => *id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let surface_id = match args.get(1) {
                    Some(Payload::ObjectId(id)) => *id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let xwayland_surface = ObjectInner::Rc(Rc::new(RefCell::new(LunasXwaylandSurface {
                    surface_id,
                    xwayland_serials: self.xwayland_serials.clone(),
                })));
                client.add_object(id, Object::new::<xwayland_shell::XwaylandSurfaceV1>(id, xwayland_surface));
                Ok(())
            }
            _ => Err(ServerError::ObjectMismatch),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LunasXwaylandSurface  (xwayland_surface_v1)
// ────────────────────────────────────────────────────────────────────────────

pub struct LunasXwaylandSurface {
    pub surface_id: ObjectId,
    pub xwayland_serials: SharedXwaylandSerials,
}

impl ObjectLogic for LunasXwaylandSurface {
    fn handle_request(
        &mut self,
        _client: &mut Client,
        opcode: u16,
        args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => Ok(()), // destroy
            1 => {
                // set_serial(serial_lo: uint, serial_hi: uint)
                let lo = match args.first() {
                    Some(Payload::UInt(v)) => *v,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let hi = match args.get(1) {
                    Some(Payload::UInt(v)) => *v,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let serial = ((hi as u64) << 32) | (lo as u64);
                (*self.xwayland_serials).borrow_mut().insert(serial, self.surface_id);
                
                // Log the association for debugging
                let msg = alloc::format!("[LUNAS-XWAYLAND] Associated serial 0x{:x} with surface id {}\n", serial, self.surface_id.0);
                unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len()); }
                
                Ok(())
            }
            _ => Err(ServerError::ObjectMismatch),
        }
    }
}

#[cfg(test)]
mod wayland_server_tests {
    use super::{LunasCompositor, LunasShm, SharedBuffers, SharedCommits};
    use std::rc::Rc;
    use std::vec::Vec;
    use core::cell::RefCell;
    use std::collections::BTreeMap;
    use wayland_proto::eclipse_transport::EclipseWaylandConnection;
    use wayland_proto::wl::protocols::common::wl_compositor::WlCompositor;
    use wayland_proto::wl::protocols::common::wl_display::Request;
    use wayland_proto::wl::protocols::common::wl_shm::WlShm;
    use wayland_proto::wl::server::client::ClientId;
    use wayland_proto::wl::server::objects::{Object, ObjectInner, ObjectLogic};
    use wayland_proto::wl::server::server::WaylandServer;
    use wayland_proto::wl::{Message, NewId, ObjectId};

    fn make_shared() -> (SharedCommits, SharedBuffers) {
        (
            Rc::new(RefCell::new(Vec::new())),
            Rc::new(RefCell::new(BTreeMap::new())),
        )
    }

    fn sample_server() -> WaylandServer {
        let (commits, buffers) = make_shared();
        let mut server = WaylandServer::new();
        {
            let c = commits.clone();
            let b = buffers.clone();
            server.register_global(
                "wl_compositor",
                4,
                move || ObjectInner::Rc(Rc::new(RefCell::new(LunasCompositor {
                    pending_commits: c.clone(),
                    buffer_registry: b.clone(),
                }))),
                |id, inner| Object::new::<WlCompositor>(id, inner),
            );
        }
        {
            let b = buffers.clone();
            server.register_global(
                "wl_shm",
                1,
                move || ObjectInner::Rc(Rc::new(RefCell::new(LunasShm {
                    buffer_registry: b.clone(),
                }))),
                |id, inner| Object::new::<WlShm>(id, inner),
            );
        }
        server
    }

    /// Regression: `wl_display::PAYLOAD_TYPES[1]` must be a single `NewId`
    /// (get_registry), not the error-event layout.
    #[test]
    fn get_registry_deserializes_and_creates_registry_object() {
        let mut server = sample_server();
        let con = Rc::new(RefCell::new(EclipseWaylandConnection::new(1, 2)));
        server.add_client(ClientId(42), con);
        let msg = Request::GetRegistry {
            registry: NewId(2),
        }
        .into_raw(ObjectId(1));
        let mut buf = [0u8; 256];
        let mut handles = Vec::new();
        let len = msg.serialize(&mut buf, &mut handles).expect("serialize get_registry");
        let r = server.process_message(ClientId(42), &buf[..len], &handles);
        assert!(r.is_ok(), "process_message: {:?}", r);
        let client = server
            .clients
            .get_mut(&ClientId(42))
            .expect("client");
        assert!(
            client.object_mut(ObjectId(2)).is_ok(),
            "wl_registry object id 2 should exist after get_registry"
        );
    }

    #[test]
    fn compositor_create_surface_adds_wl_surface_object() {
        let (commits, buffers) = make_shared();
        let mut client = wayland_proto::wl::server::client::Client::new(
            ClientId(1),
            Rc::new(RefCell::new(EclipseWaylandConnection::new(1, 2))),
        );
        let mut comp = LunasCompositor {
            pending_commits: commits,
            buffer_registry: buffers,
        };
        let args = std::vec![
            wayland_proto::wl::Payload::NewId(NewId(5)),
        ];
        let r = ObjectLogic::handle_request(&mut comp, &mut client, 0, &args, &[]);
        assert!(r.is_ok(), "create_surface: {:?}", r);
        assert!(client.object_mut(ObjectId(5)).is_ok());
    }
}

