#![no_std]
//! SideWind — stack unificada de ventanas, IPC, OpenGL, Wayland y X11 para Eclipse OS.
//!
//! Incluye: core (IPC SideWind vía sidewind_core), opengl (software GL), wayland, xwayland, nvidia (GSP),
//! superficies, fuentes, UI y GPU API.

extern crate alloc;

pub mod opengl;
// El módulo wayland contiene el manejador del protocolo Wayland puro en Rust.
// Se exporta en todas las plataformas para que los tests de crates dependientes
// (p. ej. smithay_app) puedan usarlo sin necesitar el flag `test` en sidewind.
pub mod wayland;
#[cfg(not(target_os = "linux"))]
pub mod xwayland;
#[cfg(not(target_os = "linux"))]
pub mod nvidia;

pub mod font_terminus_12;
pub mod font_terminus_14;
pub mod font_terminus_16;
pub mod font_terminus_18;
pub mod font_terminus_20;
pub mod font_terminus_24;
pub mod ui;
pub mod gpu;

pub use gpu::{GpuBackend, GpuCommandEncoder, GpuDevice, SurfaceGpuExt};

use eclipse_ipc::prelude::IpcChannel;
#[cfg(not(target_os = "linux"))]
use libc::{
    close, eclipse_send as send, mmap, munmap, open, receive, yield_cpu, MAP_SHARED, O_RDWR,
    PROT_READ, PROT_WRITE,
};

#[cfg(target_os = "linux")]
use libc::{
    close, mmap, munmap, open, MAP_SHARED, O_RDWR,
    PROT_READ, PROT_WRITE,
};

#[cfg(target_os = "linux")]
pub unsafe fn send(_target: u32, _msg_type: u32, _data: *const core::ffi::c_void, _len: usize, _flags: i32) -> isize { -1 }
#[cfg(target_os = "linux")]
pub unsafe fn receive(_buffer: *mut u8, _len: usize, _sender_pid: *mut u32) -> usize { 0 }
#[cfg(target_os = "linux")]
pub unsafe fn yield_cpu() { core::hint::spin_loop(); }
/// Re-export del módulo core para compatibilidad.
pub use sidewind_core::{
    SideWindEvent, SideWindMessage, SIDEWIND_TAG, SIDEWIND_VERSION, MSG_TYPE_GRAPHICS,
    MSG_TYPE_INPUT, MSG_TYPE_WAYLAND, MSG_TYPE_X11, SWND_OP_COMMIT, SWND_OP_CREATE, SWND_OP_DESTROY,
    SWND_OP_UPDATE, SWND_EVENT_TYPE_KEY, SWND_EVENT_TYPE_MOUSE_BUTTON, SWND_EVENT_TYPE_MOUSE_MOVE,
    SWND_EVENT_TYPE_RESIZE,
};

/// Re-export de OpenGL para uso directo.
pub use opengl::{GlContext, Texture2D};

/// Descubre el PID del compositor preguntando a init (PID 1).
pub fn discover_composer() -> Option<u32> {
    const INIT_PID: u32 = 1;
    const MAX_RETRIES: u32 = 500;

    let _ = unsafe {
        send(
            INIT_PID,
            255,
            b"GET_DISPLAY_PID".as_ptr() as *const core::ffi::c_void,
            15,
            0,
        )
    };

    let mut buffer = [0u8; 32];
    for _ in 0..MAX_RETRIES {
        let mut sender: u32 = 0;
        let len = unsafe { receive(buffer.as_mut_ptr(), buffer.len(), &mut sender) };
        if len >= 8 && sender == INIT_PID && &buffer[0..4] == b"DSPL" {
            let mut pid_bytes = [0u8; 4];
            pid_bytes.copy_from_slice(&buffer[4..8]);
            return Some(u32::from_le_bytes(pid_bytes));
        }
        unsafe { yield_cpu() };
    }
    None
}

pub struct SideWindSurface {
    composer_pid: u32,
    vaddr: *mut u32,
    size_bytes: usize,
    width: u32,
    height: u32,
}

impl SideWindSurface {
    pub fn new(composer_pid: u32, x: i32, y: i32, w: u32, h: u32, name: &str) -> Option<Self> {
        let mut path = [0u8; 64];
        path[0..5].copy_from_slice(b"/tmp/");
        let name_bytes = name.as_bytes();
        let name_len = name_bytes.len().min(32);
        path[5..5 + name_len].copy_from_slice(&name_bytes[..name_len]);
        let path_str = unsafe { ::core::str::from_utf8_unchecked(&path[..5 + name_len]) };
        let mut path_c = [0u8; 65];
        path_c[..path_str.len()].copy_from_slice(path_str.as_bytes());
        path_c[path_str.len()] = 0;
        let fd = unsafe { open(path_c.as_ptr() as *const ::core::ffi::c_char, O_RDWR, 0) };
        if fd < 0 {
            return None;
        }
        let size_bytes = (w * h * 4) as usize;
        let vaddr = unsafe {
            mmap(
                ::core::ptr::null_mut(),
                size_bytes,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                fd,
                0,
            )
        };
        unsafe { close(fd) };
        if vaddr.is_null() || vaddr == (-1isize as *mut ::core::ffi::c_void) {
            return None;
        }
        let ptr = vaddr as *mut u32;
        let msg = SideWindMessage::new_create(x, y, w, h, name);
        if !IpcChannel::send_sidewind(composer_pid, &msg) {
            unsafe { munmap(vaddr, size_bytes) };
            return None;
        }
        Some(Self {
            composer_pid,
            vaddr: ptr,
            size_bytes,
            width: w,
            height: h,
        })
    }

    #[inline]
    pub fn buffer(&mut self) -> &mut [u32] {
        let len = (self.width as usize).saturating_mul(self.height as usize);
        unsafe { ::core::slice::from_raw_parts_mut(self.vaddr, len) }
    }

    pub fn commit(&self) {
        let msg = SideWindMessage::new_commit();
        let _ = IpcChannel::send_sidewind(self.composer_pid, &msg);
    }

    pub fn poll_event(&self) -> Option<SideWindEvent> {
        let mut buffer = [0u8; ::core::mem::size_of::<SideWindEvent>()];
        let mut sender: u32 = 0;
        let len = unsafe { receive(buffer.as_mut_ptr(), buffer.len(), &mut sender) };
        if len == ::core::mem::size_of::<SideWindEvent>() && sender == self.composer_pid {
            Some(unsafe { ::core::ptr::read_unaligned(buffer.as_ptr() as *const SideWindEvent) })
        } else {
            None
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn set_size(&mut self, w: u32, h: u32) {
        self.width = w;
        self.height = h;
    }

    pub fn gl_context(&mut self) -> opengl::GlContext {
        unsafe { opengl::GlContext::new(self.vaddr, self.width, self.height) }
    }
}

impl Drop for SideWindSurface {
    fn drop(&mut self) {
        let mut msg = SideWindMessage::new_commit();
        msg.op = SWND_OP_DESTROY;
        let _ = IpcChannel::send_sidewind(self.composer_pid, &msg);
        unsafe { munmap(self.vaddr as *mut ::core::ffi::c_void, self.size_bytes) };
    }
}
