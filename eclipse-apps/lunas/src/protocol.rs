use std::prelude::v1::*;
use std::rc::Rc;
use core::cell::RefCell;
use std::collections::BTreeMap;
use wayland_proto::wl::{ObjectId, NewId, Payload};
use wayland_proto::wl::wire::Handle;
use wayland_proto::wl::server::client::{Client, ClientId};
use wayland_proto::wl::server::objects::{CallbackObject, Object, ObjectInner, ObjectLogic, ServerError};
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
    /// wl_buffer ObjectId that was attached — the compositor sends `wl_buffer.release`
    /// after it's done with this commit so the client can reuse the buffer.
    pub buffer_id: Option<wayland_proto::wl::ObjectId>,
    /// Pending `wl_surface.frame` callback ObjectId to fire `wl_callback.done` after
    /// this commit has been rendered.  None if the client did not request a callback.
    pub frame_callback: Option<wayland_proto::wl::ObjectId>,
}

/// Shared registry of live wl_buffer objects, keyed by ObjectId.
pub type SharedBuffers = Rc<RefCell<BTreeMap<ObjectId, BufferInfo>>>;

/// Shared list of pending surface commits.
pub type SharedCommits = Rc<RefCell<std::vec::Vec<SurfaceCommit>>>;

/// Shared mapping of Xwayland serials (64-bit) to wl_surface ObjectIds.
pub type SharedXwaylandSerials = Rc<RefCell<BTreeMap<u64, ObjectId>>>;

/// Shared registry mapping (ClientId, surface_id) → xdg_toplevel ObjectId.
/// Used so the compositor can dispatch `xdg_toplevel.close` to the correct object.
pub type SharedToplevels = Rc<RefCell<BTreeMap<(ClientId, u32), ObjectId>>>;

/// Shared registry mapping surface_id → window title string.
/// Populated by `xdg_toplevel.set_title`; read by the compositor to keep title bars fresh.
pub type SharedTitles = Rc<RefCell<BTreeMap<u32, std::string::String>>>;

/// Shared registry mapping ClientId → xdg_wm_base ObjectId.
/// Used by the compositor to send `xdg_wm_base.ping` and verify client responsiveness.
pub type SharedXdgWmBases = Rc<RefCell<BTreeMap<ClientId, ObjectId>>>;


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
                    attached_buffer_id: None,
                    pending_frame_callback: None,
                    pending_commits: self.pending_commits.clone(),
                    buffer_registry: self.buffer_registry.clone(),
                })));
                client.add_object(id, Object::new::<wl_surface::WlSurface>(id, surface));
                Ok(())
            }
            1 => {
                // create_region(id: new_id) — region is a no-op; just register the object
                let id = match args.first() {
                    Some(Payload::NewId(id)) => *id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let region = ObjectInner::Rc(Rc::new(RefCell::new(LunasRegion)));
                client.add_object(id, Object::new::<wl_region::WlRegion>(id, region));
                Ok(())
            }
            _ => Err(ServerError::ObjectMismatch),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LunasRegion  (wl_region) — no-op; compositor ignores all region hints
// ────────────────────────────────────────────────────────────────────────────

pub struct LunasRegion;

impl ObjectLogic for LunasRegion {
    fn handle_request(
        &mut self,
        _client: &mut Client,
        _opcode: u16,
        _args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        Ok(()) // destroy / add / subtract — all ignored
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
    /// ObjectId of the currently attached wl_buffer (for release signalling).
    pub attached_buffer_id: Option<ObjectId>,
    /// Pending frame-callback ObjectId registered via wl_surface.frame(callback).
    /// Cleared after each commit by moving it into SurfaceCommit.
    pub pending_frame_callback: Option<NewId>,
    pub pending_commits: SharedCommits,
    pub buffer_registry: SharedBuffers,
}

impl ObjectLogic for LunasSurface {
    fn handle_request(
        &mut self,
        client: &mut Client,
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
                    _ => {
                        // null buffer (ObjectId(0)) is valid — detaches the buffer.
                        self.attached_buffer = None;
                        self.attached_buffer_id = None;
                        return Ok(());
                    }
                };
                self.attached_buffer = (*self.buffer_registry).borrow().get(&buffer_id).copied();
                self.attached_buffer_id = Some(buffer_id);
                Ok(())
            }
            2 | 9 => Ok(()), // damage / damage_buffer — ignore for now
            3 => {
                // frame(callback: new_id)
                // Register a frame callback.  We store it and fire wl_callback.done
                // in drain_pending_wayland_commits() after the frame is rendered.
                // If the client registered multiple callbacks before committing, the
                // last one wins (the spec says only one per commit is sensible).
                let callback_id = match args.first() {
                    Some(Payload::NewId(id)) => *id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                // Create the WlCallback object so the client can track it.
                let cb_inner = ObjectInner::Rc(Rc::new(RefCell::new(CallbackObject)));
                client.add_object(
                    callback_id,
                    Object::new::<wl_callback::WlCallback>(callback_id, cb_inner),
                );
                self.pending_frame_callback = Some(callback_id);
                Ok(())
            }
            4 | 5 => Ok(()), // set_opaque_region / set_input_region — ignore
            6 => {
                // commit — publish a new frame
                let frame_callback = self.pending_frame_callback.take()
                    .map(|id| id.as_id());
                let buffer_id = self.attached_buffer_id;
                if let Some(info) = self.attached_buffer {
                    if info.vaddr != 0 {
                        (*self.pending_commits).borrow_mut().push(SurfaceCommit {
                            pid: self.pid,
                            surface_id: self.id.0,
                            vaddr: info.vaddr,
                            width: info.width,
                            height: info.height,
                            stride: info.stride,
                            buffer_id,
                            frame_callback,
                        });
                    }
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
                    // No fd (Handle(-1)): Unix client without SCM_RIGHTS delivery, or IPC client.
                    // Try PID-based /tmp/twb_{pid} for native IPC clients, else scan /tmp.
                    let pid = client.client_id().0;
                    let v = if pid < 0x8000_0000 {
                        // Eclipse IPC client: pid IS the process pid.
                        let v = map_shm_file(pid, size);
                        let msg = alloc::format!("[LUNAS-SHM] create_pool fallback IPC: pid={} vaddr=0x{:x}\n", pid, v);
                        unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len()); }
                        v
                    } else {
                        // Unix socket client with no fd: scan for shared buffer files.
                        scan_shm_files(size)
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

/// Scan /tmp/ for shared-memory buffer files (twb_, glxg_, etc.) and mmap the
/// first matching one. Used when `create_pool` has no fd (e.g. Unix client
/// before SCM_RIGHTS is wired, or mis-ordered fds).
fn scan_shm_files(size: usize) -> usize {
    let prefixes: &[&[u8]] = &[b"/tmp/glxg_", b"/tmp/twb_", b"/tmp/sn_"];
    
    for &prefix in prefixes {
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
            
            // Check size of the file to verify it's the right one (loose check)
            // Lunas could do fstat here if needed.
            
            let vaddr = unsafe {
                mmap(core::ptr::null_mut(), size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0)
            };
            unsafe { close(fd) };
            if !vaddr.is_null() && vaddr != libc::MAP_FAILED {
                let msg = alloc::format!(
                    "[LUNAS-SHM] scan_shm: found {} size={} vaddr=0x{:x}\n",
                    core::str::from_utf8(&prefix[5..]).unwrap_or("?"), size, vaddr as usize
                );
                unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len()); }
                return vaddr as usize;
            }
        }
    }
    let msg = b"[LUNAS-SHM] scan_shm: no SHM file found!\n";
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
// LunasXdgPositioner  (xdg_positioner) — no-op; all positioning hints ignored
// ────────────────────────────────────────────────────────────────────────────

pub struct LunasXdgPositioner;

impl ObjectLogic for LunasXdgPositioner {
    fn handle_request(
        &mut self,
        _client: &mut Client,
        _opcode: u16,
        _args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        Ok(()) // set_size / set_anchor_rect / etc. — all ignored
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LunasXdgPopup  (xdg_popup) — minimal; sends configure(0,0,w,h) immediately
// ────────────────────────────────────────────────────────────────────────────

pub struct LunasXdgPopup {
    pub id: ObjectId,
    pub xdg_surface_id: ObjectId,
    pub surface_id: ObjectId,
    pub pending_commits: SharedCommits,
    pub buffer_registry: SharedBuffers,
}

impl ObjectLogic for LunasXdgPopup {
    fn handle_request(
        &mut self,
        _client: &mut Client,
        opcode: u16,
        _args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => Ok(()), // destroy
            1 => Ok(()), // grab
            2 => Ok(()), // reposition
            _ => Ok(()),
        }
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
    pub toplevel_registry: SharedToplevels,
    pub title_registry: SharedTitles,
    /// Tracks this client's own ObjectId so the compositor can send ping.
    pub xdg_wm_base_registry: SharedXdgWmBases,
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
                // create_positioner(id: new_id) — creates an xdg_positioner object
                let id = match args.first() {
                    Some(Payload::NewId(id)) => *id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let pos = ObjectInner::Rc(Rc::new(RefCell::new(LunasXdgPositioner)));
                client.add_object(id, Object::new::<xdg_popup::XdgPositioner>(id, pos));
                Ok(())
            }
            2 => {
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
                    toplevel_registry: self.toplevel_registry.clone(),
                    title_registry: self.title_registry.clone(),
                })));
                client.add_object(id, Object::new::<xdg_surface::XdgSurface>(id, xdg_surf));
                Ok(())
            }
            3 => Ok(()), // pong — ignored (we don't track pending pings)
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
    pub toplevel_registry: SharedToplevels,
    pub title_registry: SharedTitles,
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
                    app_id: std::string::String::new(),
                    pending_commits: self.pending_commits.clone(),
                    buffer_registry: self.buffer_registry.clone(),
                    toplevel_registry: self.toplevel_registry.clone(),
                    title_registry: self.title_registry.clone(),
                })));
                client.add_object(id, Object::new::<xdg_toplevel::XdgToplevel>(id, toplevel));

                // Register the toplevel so the compositor can send close events to it.
                (*self.toplevel_registry).borrow_mut().insert(
                    (client.client_id(), self.surface_id.0),
                    id.as_id(),
                );

                // Send initial configure sequence:
                // 1. xdg_toplevel.configure(0, 0, [ACTIVATED]) — client picks its own size
                let mut states_buf = std::vec::Vec::new();
                // XDG_TOPLEVEL_STATE_ACTIVATED = 4
                states_buf.extend_from_slice(&4u32.to_ne_bytes());
                let tl_cfg = xdg_toplevel::Event::Configure {
                    width: 0, height: 0,
                    states: wayland_proto::wl::wire::Array(states_buf),
                };
                client.send_event(id.as_id(), tl_cfg)?;

                // 2. xdg_surface.configure(serial) — use the surface id as a simple serial
                let serial = self.surface_id.0;
                let surf_cfg = xdg_surface::Event::Configure { serial };
                client.send_event(self.id, surf_cfg)?;

                Ok(())
            }
            2 => {
                // get_popup(id: new_id, parent: object_id, positioner: object_id)
                let id = match args.first() {
                    Some(Payload::NewId(id)) => *id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let popup = ObjectInner::Rc(Rc::new(RefCell::new(LunasXdgPopup {
                    id: id.as_id(),
                    xdg_surface_id: self.id,
                    surface_id: self.surface_id,
                    pending_commits: self.pending_commits.clone(),
                    buffer_registry: self.buffer_registry.clone(),
                })));
                client.add_object(id, Object::new::<xdg_popup::XdgPopup>(id, popup));
                // Send popup configure(x=0, y=0, width=0, height=0) + xdg_surface.configure
                let pop_cfg = xdg_popup::PopupEvent::Configure { x: 0, y: 0, width: 0, height: 0 };
                client.send_event(id.as_id(), pop_cfg)?;
                let surf_cfg = xdg_surface::Event::Configure { serial: self.surface_id.0 };
                client.send_event(self.id, surf_cfg)?;
                Ok(())
            }
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
    pub app_id: std::string::String,
    pub pending_commits: SharedCommits,
    pub buffer_registry: SharedBuffers,
    pub toplevel_registry: SharedToplevels,
    pub title_registry: SharedTitles,
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
                    // Publish to shared title registry for compositor to apply to title bar
                    (*self.title_registry).borrow_mut().insert(self.surface_id.0, t.clone());
                }
                Ok(())
            }
            3 => {
                // set_app_id(app_id: string)
                if let Some(Payload::String(a)) = args.first() {
                    self.app_id = a.clone();
                }
                Ok(())
            }
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

// ────────────────────────────────────────────────────────────────────────────
// LunasDecorationManager  (zxdg_decoration_manager_v1)
// ────────────────────────────────────────────────────────────────────────────
//
// labwc is a server-side-decoration (SSD) compositor.  The decoration manager
// protocol lets clients negotiate with the compositor to agree on who draws
// the window border.  Lunas always responds with MODE_SERVER_SIDE.

pub struct LunasDecorationManager;

impl ObjectLogic for LunasDecorationManager {
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
                // get_toplevel_decoration(id: new_id, toplevel: object)
                let id = match args.first() {
                    Some(Payload::NewId(id)) => *id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let decoration_obj = ObjectInner::Rc(Rc::new(RefCell::new(LunasTopLevelDecoration {
                    id: id.as_id(),
                })));
                client.add_object(
                    id,
                    Object::new::<xdg_decoration::ZxdgToplevelDecorationV1>(id, decoration_obj),
                );
                // Immediately tell the client to use server-side decorations.
                client.send_event(
                    id.as_id(),
                    xdg_decoration::DecorationEvent::Configure {
                        mode: xdg_decoration::MODE_SERVER_SIDE,
                    },
                )?;
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LunasTopLevelDecoration  (zxdg_toplevel_decoration_v1)
// ────────────────────────────────────────────────────────────────────────────

pub struct LunasTopLevelDecoration {
    pub id: ObjectId,
}

impl ObjectLogic for LunasTopLevelDecoration {
    fn handle_request(
        &mut self,
        client: &mut Client,
        opcode: u16,
        args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => Ok(()), // destroy
            1 | 2 => {
                // set_mode(mode) or unset_mode — client preference; we always
                // enforce server-side decorations as labwc does.
                client.send_event(
                    self.id,
                    xdg_decoration::DecorationEvent::Configure {
                        mode: xdg_decoration::MODE_SERVER_SIDE,
                    },
                )?;
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LunasWlShell  (wl_shell)
// ────────────────────────────────────────────────────────────────────────────
//
// Legacy shell used by old GTK2/Qt4 clients.  Creates a wl_shell_surface and
// sends an initial configure event so the client can present its window.

pub struct LunasWlShell {
    pub pending_commits: SharedCommits,
    pub buffer_registry: SharedBuffers,
}

impl ObjectLogic for LunasWlShell {
    fn handle_request(
        &mut self,
        client: &mut Client,
        opcode: u16,
        args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => {
                // get_shell_surface(id: new_id, surface: object)
                let id = match args.first() {
                    Some(Payload::NewId(id)) => *id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let surface_id = match args.get(1) {
                    Some(Payload::ObjectId(s)) => *s,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let shell_surface = ObjectInner::Rc(Rc::new(RefCell::new(LunasWlShellSurface {
                    id: id.as_id(),
                    surface_id,
                    title: std::string::String::new(),
                    class: std::string::String::new(),
                    pending_commits: self.pending_commits.clone(),
                    buffer_registry: self.buffer_registry.clone(),
                })));
                client.add_object(id, Object::new::<wl_shell::WlShellSurface>(id, shell_surface));
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LunasWlShellSurface  (wl_shell_surface)
// ────────────────────────────────────────────────────────────────────────────

pub struct LunasWlShellSurface {
    pub id: ObjectId,
    pub surface_id: ObjectId,
    pub title: std::string::String,
    pub class: std::string::String,
    pub pending_commits: SharedCommits,
    pub buffer_registry: SharedBuffers,
}

impl ObjectLogic for LunasWlShellSurface {
    fn handle_request(
        &mut self,
        client: &mut Client,
        opcode: u16,
        args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => {
                // pong(serial) — reply to a ping
                Ok(())
            }
            1 | 2 => {
                // move / resize — forward to input system later; no-op for now
                Ok(())
            }
            3 => {
                // set_toplevel — treat as a normal managed window; no-op here,
                // windows become visible on the next wl_surface.commit.
                Ok(())
            }
            4 | 5 | 6 | 7 => {
                // set_transient / set_fullscreen / set_popup / set_maximized
                Ok(())
            }
            8 => {
                // set_title(title: string)
                if let Some(Payload::String(t)) = args.first() {
                    self.title = t.clone();
                }
                Ok(())
            }
            9 => {
                // set_class(class: string)
                if let Some(Payload::String(c)) = args.first() {
                    self.class = c.clone();
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LunasXdgOutputManager  (zxdg_output_manager_v1)
// ────────────────────────────────────────────────────────────────────────────
//
// Provides logical coordinates for the output.  Many modern Wayland clients
// (waybar, foot, swaybg) query this before rendering.

pub struct LunasXdgOutputManager {
    pub screen_w: u32,
    pub screen_h: u32,
}

impl ObjectLogic for LunasXdgOutputManager {
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
                // get_xdg_output(id: new_id, output: object) — `output` arg is
                // the wl_output object; we ignore it (single-output compositor).
                let id = match args.first() {
                    Some(Payload::NewId(id)) => *id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let out = ObjectInner::Rc(Rc::new(RefCell::new(LunasXdgOutput {
                    id: id.as_id(),
                    screen_w: self.screen_w,
                    screen_h: self.screen_h,
                })));
                client.add_object(id, Object::new::<xdg_output::ZxdgOutputV1>(id, out));
                // Immediately send all the logical output events then Done.
                client.send_event(id.as_id(), xdg_output::OutputEvent::LogicalPosition { x: 0, y: 0 })?;
                client.send_event(id.as_id(), xdg_output::OutputEvent::LogicalSize {
                    width: self.screen_w as i32,
                    height: self.screen_h as i32,
                })?;
                client.send_event(id.as_id(), xdg_output::OutputEvent::Name {
                    name: std::string::String::from("Virtual-1"),
                })?;
                client.send_event(id.as_id(), xdg_output::OutputEvent::Description {
                    description: std::string::String::from("Eclipse OS Virtual Display"),
                })?;
                client.send_event(id.as_id(), xdg_output::OutputEvent::Done)?;
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LunasXdgOutput  (zxdg_output_v1)
// ────────────────────────────────────────────────────────────────────────────

pub struct LunasXdgOutput {
    pub id: ObjectId,
    pub screen_w: u32,
    pub screen_h: u32,
}

impl ObjectLogic for LunasXdgOutput {
    fn handle_request(
        &mut self,
        _client: &mut Client,
        opcode: u16,
        _args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => Ok(()), // destroy
            _ => Ok(()),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LunasLayerShell  (zwlr_layer_shell_v1)
// ────────────────────────────────────────────────────────────────────────────
//
// Lets panel / overlay clients (waybar, swaylock, swaybg, mako …) request a
// dedicated layer surface on a particular compositor layer.

pub struct LunasLayerShell {
    pub pending_commits: SharedCommits,
    pub buffer_registry: SharedBuffers,
    pub screen_w: u32,
    pub screen_h: u32,
}

impl ObjectLogic for LunasLayerShell {
    fn handle_request(
        &mut self,
        client: &mut Client,
        opcode: u16,
        args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => {
                // get_layer_surface(id, surface, output, layer, namespace)
                let id = match args.first() {
                    Some(Payload::NewId(id)) => *id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let surface_id = match args.get(1) {
                    Some(Payload::ObjectId(s)) => *s,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let layer = match args.get(3) {
                    Some(Payload::UInt(l)) => *l,
                    _ => zwlr_layer_shell::LAYER_TOP,
                };
                let namespace = match args.get(4) {
                    Some(Payload::String(s)) => s.clone(),
                    _ => std::string::String::new(),
                };
                let layer_surf = ObjectInner::Rc(Rc::new(RefCell::new(LunasLayerSurface {
                    id: id.as_id(),
                    surface_id,
                    layer,
                    namespace,
                    anchor: 0,
                    exclusive_zone: 0,
                    margin_top: 0, margin_right: 0, margin_bottom: 0, margin_left: 0,
                    width: 0,
                    height: 0,
                    keyboard_interactivity: zwlr_layer_shell::KEYBOARD_INTERACTIVITY_NONE,
                    pending_commits: self.pending_commits.clone(),
                    buffer_registry: self.buffer_registry.clone(),
                })));
                client.add_object(id, Object::new::<zwlr_layer_shell::ZwlrLayerSurfaceV1>(id, layer_surf));
                // Send initial configure: compositor chooses the size based on
                // anchor/exclusive zone, but since the client hasn't set those
                // yet we send (0,0) and let it issue set_size first.
                client.send_event(id.as_id(), zwlr_layer_shell::SurfaceEvent::Configure {
                    serial: 1,
                    width: self.screen_w,
                    height: self.screen_h,
                })?;
                Ok(())
            }
            1 => Ok(()), // destroy
            _ => Ok(()),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LunasLayerSurface  (zwlr_layer_surface_v1)
// ────────────────────────────────────────────────────────────────────────────

pub struct LunasLayerSurface {
    pub id: ObjectId,
    pub surface_id: ObjectId,
    pub layer: u32,
    pub namespace: std::string::String,
    pub anchor: u32,
    pub exclusive_zone: i32,
    pub margin_top: i32,
    pub margin_right: i32,
    pub margin_bottom: i32,
    pub margin_left: i32,
    pub width: u32,
    pub height: u32,
    pub keyboard_interactivity: u32,
    pub pending_commits: SharedCommits,
    pub buffer_registry: SharedBuffers,
}

impl ObjectLogic for LunasLayerSurface {
    fn handle_request(
        &mut self,
        _client: &mut Client,
        opcode: u16,
        args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => {
                // set_size(width, height)
                if let (Some(Payload::UInt(w)), Some(Payload::UInt(h))) = (args.get(0), args.get(1)) {
                    self.width = *w;
                    self.height = *h;
                }
                Ok(())
            }
            1 => {
                // set_anchor(anchor)
                if let Some(Payload::UInt(a)) = args.first() {
                    self.anchor = *a;
                }
                Ok(())
            }
            2 => {
                // set_exclusive_zone(zone)
                if let Some(Payload::Int(z)) = args.first() {
                    self.exclusive_zone = *z;
                }
                Ok(())
            }
            3 => {
                // set_margin(top, right, bottom, left)
                if let (Some(Payload::Int(t)), Some(Payload::Int(r)),
                        Some(Payload::Int(b)), Some(Payload::Int(l)))
                    = (args.get(0), args.get(1), args.get(2), args.get(3))
                {
                    self.margin_top = *t;
                    self.margin_right = *r;
                    self.margin_bottom = *b;
                    self.margin_left = *l;
                }
                Ok(())
            }
            4 => {
                // set_keyboard_interactivity
                if let Some(Payload::UInt(k)) = args.first() {
                    self.keyboard_interactivity = *k;
                }
                Ok(())
            }
            5 => Ok(()), // get_popup — not implemented
            6 => Ok(()), // ack_configure
            7 => Ok(()), // destroy
            8 => {
                // set_layer(layer)
                if let Some(Payload::UInt(l)) = args.first() {
                    self.layer = *l;
                }
                Ok(())
            }
            9 => Ok(()), // set_exclusive_edge — ignore
            _ => Ok(()),
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

    // ── New protocol handler tests ───────────────────────────────────────────

    #[test]
    fn decoration_manager_get_toplevel_returns_server_side_mode() {
        use super::{LunasDecorationManager};
        let mut client = wayland_proto::wl::server::client::Client::new(
            ClientId(1),
            Rc::new(RefCell::new(EclipseWaylandConnection::new(1, 2))),
        );
        let mut mgr = LunasDecorationManager;
        // opcode 1 = get_toplevel_decoration(id: new_id, toplevel: object)
        let args = std::vec![
            wayland_proto::wl::Payload::NewId(NewId(10)),
            wayland_proto::wl::Payload::ObjectId(ObjectId(5)),
        ];
        let r = ObjectLogic::handle_request(&mut mgr, &mut client, 1, &args, &[]);
        assert!(r.is_ok(), "get_toplevel_decoration: {:?}", r);
        // A zxdg_toplevel_decoration_v1 object should have been registered.
        assert!(client.object_mut(ObjectId(10)).is_ok(), "decoration object must exist");
    }

    #[test]
    fn decoration_manager_destroy_is_noop() {
        use super::LunasDecorationManager;
        let mut client = wayland_proto::wl::server::client::Client::new(
            ClientId(2),
            Rc::new(RefCell::new(EclipseWaylandConnection::new(1, 2))),
        );
        let mut mgr = LunasDecorationManager;
        let r = ObjectLogic::handle_request(&mut mgr, &mut client, 0, &[], &[]);
        assert!(r.is_ok(), "destroy: {:?}", r);
    }

    #[test]
    fn wl_shell_get_shell_surface_creates_object() {
        use super::LunasWlShell;
        let (commits, buffers) = make_shared();
        let mut client = wayland_proto::wl::server::client::Client::new(
            ClientId(3),
            Rc::new(RefCell::new(EclipseWaylandConnection::new(1, 2))),
        );
        let mut shell = LunasWlShell {
            pending_commits: commits,
            buffer_registry: buffers,
        };
        // opcode 0 = get_shell_surface(id: new_id, surface: object)
        let args = std::vec![
            wayland_proto::wl::Payload::NewId(NewId(20)),
            wayland_proto::wl::Payload::ObjectId(ObjectId(7)),
        ];
        let r = ObjectLogic::handle_request(&mut shell, &mut client, 0, &args, &[]);
        assert!(r.is_ok(), "get_shell_surface: {:?}", r);
        assert!(client.object_mut(ObjectId(20)).is_ok(), "wl_shell_surface must be registered");
    }

    #[test]
    fn xdg_output_manager_get_xdg_output_creates_object_and_sends_events() {
        use super::LunasXdgOutputManager;
        let mut client = wayland_proto::wl::server::client::Client::new(
            ClientId(4),
            Rc::new(RefCell::new(EclipseWaylandConnection::new(1, 2))),
        );
        let mut mgr = LunasXdgOutputManager { screen_w: 1920, screen_h: 1080 };
        // opcode 1 = get_xdg_output(id: new_id, output: object)
        let args = std::vec![
            wayland_proto::wl::Payload::NewId(NewId(30)),
            wayland_proto::wl::Payload::ObjectId(ObjectId(9)),
        ];
        let r = ObjectLogic::handle_request(&mut mgr, &mut client, 1, &args, &[]);
        assert!(r.is_ok(), "get_xdg_output: {:?}", r);
        assert!(client.object_mut(ObjectId(30)).is_ok(), "zxdg_output_v1 must be registered");
    }

    #[test]
    fn layer_shell_get_layer_surface_creates_object_and_sends_configure() {
        use super::LunasLayerShell;
        let (commits, buffers) = make_shared();
        let mut client = wayland_proto::wl::server::client::Client::new(
            ClientId(5),
            Rc::new(RefCell::new(EclipseWaylandConnection::new(1, 2))),
        );
        let mut shell = LunasLayerShell {
            pending_commits: commits,
            buffer_registry: buffers,
            screen_w: 1920,
            screen_h: 1080,
        };
        // opcode 0 = get_layer_surface(id, surface, output, layer, namespace)
        let args = std::vec![
            wayland_proto::wl::Payload::NewId(NewId(40)),
            wayland_proto::wl::Payload::ObjectId(ObjectId(11)),
            wayland_proto::wl::Payload::ObjectId(ObjectId(12)), // output (may be null)
            wayland_proto::wl::Payload::UInt(
                wayland_proto::wl::protocols::common::zwlr_layer_shell::LAYER_TOP
            ),
            wayland_proto::wl::Payload::String(String::from("waybar")),
        ];
        let r = ObjectLogic::handle_request(&mut shell, &mut client, 0, &args, &[]);
        assert!(r.is_ok(), "get_layer_surface: {:?}", r);
        assert!(client.object_mut(ObjectId(40)).is_ok(), "zwlr_layer_surface_v1 must be registered");
    }

    #[test]
    fn xdg_toplevel_stores_app_id() {
        use super::{LunasXdgToplevel, SharedCommits, SharedBuffers};
        let (commits, buffers) = make_shared();
        let toplevel_registry = Rc::new(RefCell::new(std::collections::BTreeMap::new()));
        let title_registry = Rc::new(RefCell::new(std::collections::BTreeMap::new()));
        let mut client = wayland_proto::wl::server::client::Client::new(
            ClientId(6),
            Rc::new(RefCell::new(EclipseWaylandConnection::new(1, 2))),
        );
        let mut toplevel = LunasXdgToplevel {
            id: ObjectId(50),
            xdg_surface_id: ObjectId(49),
            surface_id: ObjectId(48),
            title: String::from("My App"),
            app_id: String::new(),
            pending_commits: commits,
            buffer_registry: buffers,
            toplevel_registry,
            title_registry,
        };
        // opcode 3 = set_app_id(app_id: string)
        let args = std::vec![
            wayland_proto::wl::Payload::String(String::from("com.example.myapp")),
        ];
        let r = ObjectLogic::handle_request(&mut toplevel, &mut client, 3, &args, &[]);
        assert!(r.is_ok(), "set_app_id: {:?}", r);
        assert_eq!(toplevel.app_id, "com.example.myapp");
    }

    /// wl_display.sync must immediately fire wl_callback.done and wl_display.delete_id.
    /// Without this, wl_display_roundtrip() in libwayland-client stalls forever.
    #[test]
    fn wl_display_sync_fires_done_and_delete_id() {
        use wayland_proto::wl::protocols::common::wl_display;
        let mut server = sample_server();
        let con = Rc::new(RefCell::new(EclipseWaylandConnection::new(1, 2)));
        server.add_client(ClientId(99), con);

        // Build a wl_display.sync(callback=NewId(5)) request
        let msg = wl_display::Request::Sync { callback: NewId(5) }
            .into_raw(ObjectId(1));
        let mut buf = [0u8; 256];
        let mut handles = Vec::new();
        let len = msg.serialize(&mut buf, &mut handles).expect("serialize sync");
        let r = server.process_message(ClientId(99), &buf[..len], &[]);
        assert!(r.is_ok(), "wl_display.sync: {:?}", r);
        // The callback object must have been created
        let client = server.clients.get_mut(&ClientId(99)).expect("client");
        assert!(
            client.object_mut(ObjectId(5)).is_ok(),
            "wl_callback object id 5 must exist after sync"
        );
    }

    /// wl_surface.frame must register a WlCallback object and include it in the
    /// SurfaceCommit so the compositor can fire done after rendering.
    #[test]
    fn wl_surface_frame_creates_callback_in_commit() {
        use super::LunasSurface;
        use wayland_proto::wl::server::objects::CallbackObject;
        let (commits, buffers) = make_shared();
        let mut client = wayland_proto::wl::server::client::Client::new(
            ClientId(7),
            Rc::new(RefCell::new(EclipseWaylandConnection::new(1, 2))),
        );
        let mut surface = LunasSurface {
            id: ObjectId(3),
            pid: 7,
            attached_buffer: None,
            attached_buffer_id: None,
            pending_frame_callback: None,
            pending_commits: commits.clone(),
            buffer_registry: buffers.clone(),
        };

        // opcode 3 = frame(callback: new_id(6))
        let r = ObjectLogic::handle_request(
            &mut surface,
            &mut client,
            3,
            &[wayland_proto::wl::Payload::NewId(NewId(6))],
            &[],
        );
        assert!(r.is_ok(), "frame: {:?}", r);
        assert_eq!(surface.pending_frame_callback, Some(NewId(6)));
        // WlCallback object must have been registered in client
        assert!(client.object_mut(ObjectId(6)).is_ok(), "wl_callback must exist");

        // Inject a buffer so commit does something
        let info = super::BufferInfo { vaddr: 0xDEAD, width: 64, height: 64, stride: 256, format: 1 };
        (*buffers).borrow_mut().insert(ObjectId(8), info);
        surface.attached_buffer = Some(info);
        surface.attached_buffer_id = Some(ObjectId(8));

        // opcode 6 = commit
        let r = ObjectLogic::handle_request(&mut surface, &mut client, 6, &[], &[]);
        assert!(r.is_ok(), "commit: {:?}", r);

        // Commit must carry the frame callback
        let pending = (*commits).borrow();
        assert_eq!(pending.len(), 1, "one commit expected");
        assert_eq!(pending[0].frame_callback, Some(ObjectId(6)), "commit must carry callback id");
        assert_eq!(pending[0].buffer_id, Some(ObjectId(8)), "commit must carry buffer id");
        // Pending callback cleared after commit
        assert_eq!(surface.pending_frame_callback, None, "pending_frame_callback must be cleared after commit");
    }

    /// wl_compositor.create_region (opcode 1) must register a LunasRegion object.
    /// Previously this returned ObjectMismatch, causing crashes in GTK4/Qt clients.
    #[test]
    fn compositor_create_region_registers_object() {
        let (commits, buffers) = make_shared();
        let mut client = wayland_proto::wl::server::client::Client::new(
            ClientId(8),
            Rc::new(RefCell::new(EclipseWaylandConnection::new(1, 2))),
        );
        let mut comp = super::LunasCompositor { pending_commits: commits, buffer_registry: buffers };
        let args = std::vec![wayland_proto::wl::Payload::NewId(NewId(55))];
        let r = ObjectLogic::handle_request(&mut comp, &mut client, 1, &args, &[]);
        assert!(r.is_ok(), "create_region: {:?}", r);
        assert!(client.object_mut(ObjectId(55)).is_ok(), "wl_region object must exist");
    }

    /// xdg_toplevel.set_title must publish to the shared title registry.
    #[test]
    fn xdg_toplevel_set_title_publishes_to_registry() {
        let (commits, buffers) = make_shared();
        let toplevel_registry = Rc::new(RefCell::new(std::collections::BTreeMap::new()));
        let title_registry: super::SharedTitles = Rc::new(RefCell::new(std::collections::BTreeMap::new()));
        let mut client = wayland_proto::wl::server::client::Client::new(
            ClientId(9),
            Rc::new(RefCell::new(EclipseWaylandConnection::new(1, 2))),
        );
        let mut toplevel = super::LunasXdgToplevel {
            id: ObjectId(60),
            xdg_surface_id: ObjectId(59),
            surface_id: ObjectId(58),
            title: String::new(),
            app_id: String::new(),
            pending_commits: commits,
            buffer_registry: buffers,
            toplevel_registry,
            title_registry: title_registry.clone(),
        };
        let r = ObjectLogic::handle_request(
            &mut toplevel,
            &mut client,
            2, // set_title
            &[wayland_proto::wl::Payload::String(String::from("My Wayland App"))],
            &[],
        );
        assert!(r.is_ok(), "set_title: {:?}", r);
        let reg = (*title_registry).borrow();
        assert_eq!(reg.get(&58u32).map(|s| s.as_str()), Some("My Wayland App"));
    }

    /// LunasXdgSurface.get_toplevel must register the toplevel in toplevel_registry.
    #[test]
    fn xdg_surface_get_toplevel_registers_toplevel_id() {
        let (commits, buffers) = make_shared();
        let toplevel_registry: super::SharedToplevels = Rc::new(RefCell::new(std::collections::BTreeMap::new()));
        let title_registry: super::SharedTitles = Rc::new(RefCell::new(std::collections::BTreeMap::new()));
        let mut client = wayland_proto::wl::server::client::Client::new(
            ClientId(10),
            Rc::new(RefCell::new(EclipseWaylandConnection::new(1, 2))),
        );
        let mut xdg_surf = super::LunasXdgSurface {
            id: ObjectId(70),
            surface_id: ObjectId(71),
            pending_commits: commits,
            buffer_registry: buffers,
            toplevel_registry: toplevel_registry.clone(),
            title_registry,
        };
        let r = ObjectLogic::handle_request(
            &mut xdg_surf,
            &mut client,
            1, // get_toplevel
            &[wayland_proto::wl::Payload::NewId(NewId(72))],
            &[],
        );
        assert!(r.is_ok(), "get_toplevel: {:?}", r);
        // The registry must now contain (ClientId(10), 71) → ObjectId(72)
        use wayland_proto::wl::server::client::ClientId;
        let reg = (*toplevel_registry).borrow();
        assert_eq!(reg.get(&(ClientId(10), 71u32)), Some(&ObjectId(72)));
    }

    /// xdg_wm_base opcode 1 (create_positioner) must register an XdgPositioner object.
    #[test]
    fn xdg_wm_base_create_positioner_registers_object() {
        let (commits, buffers) = make_shared();
        let toplevel_registry = Rc::new(RefCell::new(std::collections::BTreeMap::new()));
        let title_registry = Rc::new(RefCell::new(std::collections::BTreeMap::new()));
        let xdg_wm_base_registry = Rc::new(RefCell::new(std::collections::BTreeMap::new()));
        let mut client = wayland_proto::wl::server::client::Client::new(
            ClientId(11),
            Rc::new(RefCell::new(EclipseWaylandConnection::new(1, 2))),
        );
        let mut wm_base = super::LunasXdgWmBase {
            pending_commits: commits,
            buffer_registry: buffers,
            toplevel_registry,
            title_registry,
            xdg_wm_base_registry,
        };
        // opcode 1 = create_positioner(id: new_id(80))
        let r = ObjectLogic::handle_request(
            &mut wm_base, &mut client, 1,
            &[wayland_proto::wl::Payload::NewId(NewId(80))],
            &[],
        );
        assert!(r.is_ok(), "create_positioner: {:?}", r);
        assert!(client.object_mut(ObjectId(80)).is_ok(), "xdg_positioner must exist");
    }

    /// xdg_wm_base opcode 2 (get_xdg_surface) must register an XdgSurface object.
    #[test]
    fn xdg_wm_base_get_xdg_surface_uses_opcode_2() {
        let (commits, buffers) = make_shared();
        let toplevel_registry = Rc::new(RefCell::new(std::collections::BTreeMap::new()));
        let title_registry = Rc::new(RefCell::new(std::collections::BTreeMap::new()));
        let xdg_wm_base_registry = Rc::new(RefCell::new(std::collections::BTreeMap::new()));
        let mut client = wayland_proto::wl::server::client::Client::new(
            ClientId(12),
            Rc::new(RefCell::new(EclipseWaylandConnection::new(1, 2))),
        );
        let mut wm_base = super::LunasXdgWmBase {
            pending_commits: commits,
            buffer_registry: buffers,
            toplevel_registry,
            title_registry,
            xdg_wm_base_registry,
        };
        // opcode 2 = get_xdg_surface(id: new_id(81), surface: ObjectId(82))
        let r = ObjectLogic::handle_request(
            &mut wm_base, &mut client, 2,
            &[
                wayland_proto::wl::Payload::NewId(NewId(81)),
                wayland_proto::wl::Payload::ObjectId(ObjectId(82)),
            ],
            &[],
        );
        assert!(r.is_ok(), "get_xdg_surface: {:?}", r);
        assert!(client.object_mut(ObjectId(81)).is_ok(), "xdg_surface must exist");
    }

    /// xdg_surface.get_popup (opcode 2) must create an XdgPopup and send configure.
    #[test]
    fn xdg_surface_get_popup_creates_popup_object() {
        let (commits, buffers) = make_shared();
        let toplevel_registry = Rc::new(RefCell::new(std::collections::BTreeMap::new()));
        let title_registry = Rc::new(RefCell::new(std::collections::BTreeMap::new()));
        let mut client = wayland_proto::wl::server::client::Client::new(
            ClientId(13),
            Rc::new(RefCell::new(EclipseWaylandConnection::new(1, 2))),
        );
        let mut xdg_surf = super::LunasXdgSurface {
            id: ObjectId(90),
            surface_id: ObjectId(91),
            pending_commits: commits,
            buffer_registry: buffers,
            toplevel_registry,
            title_registry,
        };
        // opcode 2 = get_popup(id: new_id(92), parent: ObjectId(93), positioner: ObjectId(94))
        let r = ObjectLogic::handle_request(
            &mut xdg_surf, &mut client, 2,
            &[
                wayland_proto::wl::Payload::NewId(NewId(92)),
                wayland_proto::wl::Payload::ObjectId(ObjectId(93)),
                wayland_proto::wl::Payload::ObjectId(ObjectId(94)),
            ],
            &[],
        );
        assert!(r.is_ok(), "get_popup: {:?}", r);
        assert!(client.object_mut(ObjectId(92)).is_ok(), "xdg_popup must exist");
    }
}

