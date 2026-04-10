use std::prelude::v1::*;
use std::rc::Rc;
use core::cell::RefCell;
use std::collections::BTreeMap;
use ::wayland_proto::wl::{ObjectId, NewId, Payload};
use ::wayland_proto::wl::wire::Handle;
use ::wayland_proto::wl::server::client::{Client, ClientId};
use ::wayland_proto::wl::server::objects::{Object, ObjectInner, ObjectLogic, ServerError};
use ::wayland_proto::wl::protocols::common::*;
use crate::compositor::{WindowContent, ShellWindow};

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

/// Pending surface commit: posted by AppSurface on commit and drained by
/// SmithayState to create / update ShellWindows.
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
// AppCompositor  (wl_compositor)
// ────────────────────────────────────────────────────────────────────────────

pub struct AppCompositor {
    pub pending_commits: SharedCommits,
    pub buffer_registry: SharedBuffers,
}

impl ObjectLogic for AppCompositor {
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
                let surface = ObjectInner::Rc(Rc::new(RefCell::new(AppSurface {
                    id: id.as_id(),
                    pid: client.client_id().0,
                    attached_buffer: None,
                    pending_commits: self.pending_commits.clone(),
                    buffer_registry: self.buffer_registry.clone(),
                })));
                client.add_object(id, Object::new::<::wayland_proto::wl::protocols::common::wl_surface::WlSurface>(id, surface));
                Ok(())
            }
            1 => {
                // create_region(id: new_id)
                let id = match args.first() {
                    Some(Payload::NewId(id)) => *id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                // Regions are currently ignored as they don't affect our rendering loop yet.
                let region = ObjectInner::Rc(Rc::new(RefCell::new(AppRegion)));
                client.add_object(id, Object::new::<::wayland_proto::wl::protocols::common::wl_compositor::WlCompositor>(id, region)); // Using WlCompositor as a placeholder for region interface
                // Actually wl_compositor.create_region should bind a wl_region object.
                // Since we don't have wl_region defined in wayland-proto yet, we just stub it.
                Ok(())
            }
            _ => Err(ServerError::ObjectMismatch),
        }
    }
}

pub struct AppRegion;
impl ObjectLogic for AppRegion {
    fn handle_request(&mut self, _client: &mut Client, _opcode: u16, _args: &[Payload], _handles: &[Handle]) -> Result<(), ServerError> {
        Ok(())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// AppSurface  (wl_surface)
// ────────────────────────────────────────────────────────────────────────────

pub struct AppSurface {
    pub id: ObjectId,
    pub pid: u32,
    /// The buffer currently attached (set by opcode 1 = attach).
    pub attached_buffer: Option<BufferInfo>,
    pub pending_commits: SharedCommits,
    pub buffer_registry: SharedBuffers,
}

impl ObjectLogic for AppSurface {
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
                // attach(buffer: object_id, x: int, y: int)
                let buffer_id = match args.first() {
                    Some(Payload::ObjectId(id)) => *id,
                    _ => return Ok(()), // null buffer is valid (detach)
                };
                self.attached_buffer = (*self.buffer_registry).borrow().get(&buffer_id).copied();
                Ok(())
            }
            2 | 9 => Ok(()), // damage / damage_buffer
            3 => Ok(()), // frame — callback not implemented yet
            4 | 5 => Ok(()), // set_opaque_region / set_input_region
            6 => {
                // commit — publish a new frame
                if let Some(info) = self.attached_buffer {
                    if info.vaddr != 0 {
                        (*self.pending_commits).borrow_mut().push(SurfaceCommit {
                            pid: self.pid,
                            surface_id: self.id.0,
                            vaddr: info.vaddr,
                            width: info.width,
                            height: info.height,
                            stride: info.stride,
                        });
                    }
                }
                Ok(())
            }
            7 | 8 => Ok(()), // set_buffer_transform / set_buffer_scale
            _ => Ok(()),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// AppShm  (wl_shm)
// ────────────────────────────────────────────────────────────────────────────

pub struct AppShm {
    pub buffer_registry: SharedBuffers,
}

impl ObjectLogic for AppShm {
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
                let id = match args.first() {
                    Some(Payload::NewId(id)) => *id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let fd = match args.get(1) {
                    Some(Payload::Handle(h)) => h.0,
                    _ => -1,
                };
                let size = match args.get(2) {
                    Some(Payload::Int(s)) => *s as usize,
                    _ => return Err(ServerError::MessageDeserializeError),
                };

                let vaddr = if fd >= 0 {
                    map_shm_fd(fd, size)
                } else {
                    // Fallback for non-SCM_RIGHTS clients (Eclipse IPC)
                    let pid = client.client_id().0;
                    map_shm_file(pid, size)
                };

                let pool = ObjectInner::Rc(Rc::new(RefCell::new(AppShmPool {
                    vaddr,
                    size,
                    buffer_registry: self.buffer_registry.clone(),
                })));
                client.add_object(id, Object::new::<::wayland_proto::wl::protocols::common::wl_shm::WlShmPool>(id, pool));
                Ok(())
            }
            _ => Err(ServerError::ObjectMismatch),
        }
    }
}

fn map_shm_file(pid: u32, size: usize) -> usize {
    let path = format!("/tmp/twb_{}", pid);
    let fd = unsafe { open(path.as_ptr() as *const i8, O_RDWR | O_NONBLOCK, 0) };
    if fd < 0 { return 0; }
    let vaddr = unsafe {
        mmap(core::ptr::null_mut(), size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0)
    };
    unsafe { close(fd) };
    if vaddr.is_null() || vaddr == libc::MAP_FAILED { 0 } else { vaddr as usize }
}

fn map_shm_fd(fd: i32, size: usize) -> usize {
    if fd < 0 || size == 0 { return 0; }
    let vaddr = unsafe {
        mmap(core::ptr::null_mut(), size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0)
    };
    let _ = unsafe { close(fd) };
    if vaddr.is_null() || vaddr == libc::MAP_FAILED { 0 } else { vaddr as usize }
}

// ────────────────────────────────────────────────────────────────────────────
// AppShmPool  (wl_shm_pool)
// ────────────────────────────────────────────────────────────────────────────

pub struct AppShmPool {
    pub vaddr: usize,
    pub size: usize,
    pub buffer_registry: SharedBuffers,
}

impl ObjectLogic for AppShmPool {
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
                let offset = match args.get(1) { Some(Payload::Int(v)) => *v as usize, _ => 0 };
                let width = match args.get(2) { Some(Payload::Int(v)) => *v as u32, _ => 0 };
                let height = match args.get(3) { Some(Payload::Int(v)) => *v as u32, _ => 0 };
                let stride = match args.get(4) { Some(Payload::Int(v)) => *v as u32, _ => 0 };
                let format = match args.get(5) { Some(Payload::UInt(v)) => *v, _ => 0 };

                let buf_vaddr = if self.vaddr != 0 { self.vaddr + offset } else { 0 };
                let info = BufferInfo { vaddr: buf_vaddr, width, height, stride, format };
                (*self.buffer_registry).borrow_mut().insert(id.as_id(), info);

                let buf_obj = ObjectInner::Rc(Rc::new(RefCell::new(AppBuffer { info })));
                client.add_object(id, Object::new::<::wayland_proto::wl::protocols::common::wl_buffer::WlBuffer>(id, buf_obj));
                Ok(())
            }
            1 => Ok(()), // destroy
            2 => Ok(()), // resize
            _ => Err(ServerError::ObjectMismatch),
        }
    }
}

pub struct AppBuffer { pub info: BufferInfo }
impl ObjectLogic for AppBuffer {
    fn handle_request(&mut self, _client: &mut Client, _opcode: u16, _args: &[Payload], _handles: &[Handle]) -> Result<(), ServerError> {
        Ok(())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// AppSeat  (wl_seat)
// ────────────────────────────────────────────────────────────────────────────

pub type SharedKeyboards = Rc<RefCell<BTreeMap<ClientId, ObjectId>>>;
pub type SharedPointers = Rc<RefCell<BTreeMap<ClientId, ObjectId>>>;

pub struct AppSeat {
    pub keyboard_registry: SharedKeyboards,
    pub pointer_registry: SharedPointers,
}

impl ObjectLogic for AppSeat {
    fn handle_request(
        &mut self,
        client: &mut Client,
        opcode: u16,
        args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => {
                // get_pointer
                let id = match args.first() { Some(Payload::NewId(id)) => *id, _ => return Err(ServerError::MessageDeserializeError) };
                (*self.pointer_registry).borrow_mut().insert(client.client_id(), id.as_id());
                let ptr = ObjectInner::Rc(Rc::new(RefCell::new(AppPointer { id: id.as_id() })));
                client.add_object(id, Object::new::<::wayland_proto::wl::protocols::common::wl_pointer::WlPointer>(id, ptr));
                Ok(())
            }
            1 => {
                // get_keyboard
                let id = match args.first() { Some(Payload::NewId(id)) => *id, _ => return Err(ServerError::MessageDeserializeError) };
                (*self.keyboard_registry).borrow_mut().insert(client.client_id(), id.as_id());
                let kb = ObjectInner::Rc(Rc::new(RefCell::new(AppKeyboard { id: id.as_id() })));
                client.add_object(id, Object::new::<::wayland_proto::wl::protocols::common::wl_keyboard::WlKeyboard>(id, kb));
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

pub struct AppPointer { pub id: ObjectId }
impl ObjectLogic for AppPointer {
    fn handle_request(&mut self, _client: &mut Client, _opcode: u16, _args: &[Payload], _handles: &[Handle]) -> Result<(), ServerError> { Ok(()) }
}
pub struct AppKeyboard { pub id: ObjectId }
impl ObjectLogic for AppKeyboard {
    fn handle_request(&mut self, _client: &mut Client, _opcode: u16, _args: &[Payload], _handles: &[Handle]) -> Result<(), ServerError> { Ok(()) }
}

// ────────────────────────────────────────────────────────────────────────────
// AppXdgWmBase  (xdg_wm_base)
// ────────────────────────────────────────────────────────────────────────────

pub struct AppXdgWmBase {
    pub pending_commits: SharedCommits,
    pub buffer_registry: SharedBuffers,
}

impl ObjectLogic for AppXdgWmBase {
    fn handle_request(
        &mut self,
        client: &mut Client,
        opcode: u16,
        args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            1 => {
                // get_xdg_surface
                let id = match args.first() { Some(Payload::NewId(id)) => *id, _ => return Err(ServerError::MessageDeserializeError) };
                let surf_id = match args.get(1) { Some(Payload::ObjectId(v)) => *v, _ => return Err(ServerError::MessageDeserializeError) };
                let xdg_surf = ObjectInner::Rc(Rc::new(RefCell::new(AppXdgSurface {
                    id: id.as_id(),
                    surface_id: surf_id,
                    pending_commits: self.pending_commits.clone(),
                    buffer_registry: self.buffer_registry.clone(),
                })));
                client.add_object(id, Object::new::<::wayland_proto::wl::protocols::common::xdg_surface::XdgSurface>(id, xdg_surf));
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

pub struct AppXdgSurface {
    pub id: ObjectId,
    pub surface_id: ObjectId,
    pub pending_commits: SharedCommits,
    pub buffer_registry: SharedBuffers,
}

impl ObjectLogic for AppXdgSurface {
    fn handle_request(
        &mut self,
        client: &mut Client,
        opcode: u16,
        args: &[Payload],
        _handles: &[Handle],
    ) -> Result<(), ServerError> {
        match opcode {
            1 => {
                // get_toplevel
                let id = match args.first() { Some(Payload::NewId(id)) => *id, _ => return Err(ServerError::MessageDeserializeError) };
                let toplevel = ObjectInner::Rc(Rc::new(RefCell::new(AppXdgToplevel {
                    id: id.as_id(),
                    title: String::from("Wayland Window"),
                })));
                client.add_object(id, Object::new::<::wayland_proto::wl::protocols::common::xdg_toplevel::XdgToplevel>(id, toplevel));
                
                // Initial configure
                client.send_event(id.as_id(), ::wayland_proto::wl::protocols::common::xdg_toplevel::Event::Configure { width: 0, height: 0, states: ::wayland_proto::wl::wire::Array(std::vec::Vec::new()) })?;
                client.send_event(self.id, ::wayland_proto::wl::protocols::common::xdg_surface::Event::Configure { serial: 1 })?;
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

pub struct AppXdgToplevel {
    pub id: ObjectId,
    pub title: String,
}

impl ObjectLogic for AppXdgToplevel {
    fn handle_request(&mut self, _client: &mut Client, opcode: u16, args: &[Payload], _handles: &[Handle]) -> Result<(), ServerError> {
        match opcode {
            2 => {
                 if let Some(Payload::String(t)) = args.first() { self.title = t.clone(); }
                 Ok(())
            }
            _ => Ok(()),
        }
    }
}
