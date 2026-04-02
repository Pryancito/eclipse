#![no_std]
//! SideWind — stack unificada de ventanas, IPC, OpenGL, Wayland y X11 para Eclipse OS.

extern crate alloc;

pub mod opengl;
pub mod protocol;
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
pub mod xwayland;

pub use gpu::{GpuBackend, GpuCommandEncoder, GpuDevice, SurfaceGpuExt};
/// Canal IPC compartido con el compositor (`send_sidewind`, `send_raw`, etc.).
pub use eclipse_ipc::prelude::IpcChannel;

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

pub use sidewind_core::{
    SideWindEvent, SideWindMessage, SIDEWIND_TAG, SIDEWIND_VERSION, MSG_TYPE_GRAPHICS,
    MSG_TYPE_INPUT, MSG_TYPE_WAYLAND, MSG_TYPE_X11, SWND_OP_COMMIT, SWND_OP_CREATE, SWND_OP_DESTROY,
    SWND_OP_UPDATE, SWND_OP_SET_TITLE, SWND_EVENT_TYPE_KEY, SWND_EVENT_TYPE_MOUSE_BUTTON,
    SWND_EVENT_TYPE_MOUSE_MOVE, SWND_EVENT_TYPE_RESIZE, SWND_EVENT_TYPE_CLOSE,
};

pub use opengl::{GlContext, Texture2D};

pub fn discover_composer() -> Option<u32> {
    const INIT_PID: u32 = 1;
    const MAX_RETRIES: u32 = 100_000;

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
    surface_id: u32,
    vaddr: *mut u32,
    size_bytes: usize,
    width: u32,
    height: u32,
    ring_ptr: *mut protocol::SnpRingControl,
    cmds_ptr: *mut protocol::SnpCommand,
    current_fence: u64,
}

impl SideWindSurface {
    pub fn new(composer_pid: u32, _x: i32, _y: i32, w: u32, h: u32, name: &str) -> Option<Self> {
        let name_bytes = name.as_bytes();
        let name_len = name_bytes.len().min(32);

        // 1. Setup URB (Ring Buffer)
        let ring_fd = unsafe { open(b"/tmp/snp_ring_terminal\0".as_ptr() as *const _, O_RDWR | 0x0040, 0) };
        if ring_fd < 0 { return None; }
        
        let ring_size = (core::mem::size_of::<protocol::SnpRingControl>() + 1024 * 64) as usize;
        if eclipse_syscall::ftruncate(ring_fd as usize, ring_size).is_err() {
            unsafe { close(ring_fd) };
            return None;
        }
        let ring_vaddr = unsafe {
            mmap(core::ptr::null_mut(), ring_size, PROT_READ | PROT_WRITE, MAP_SHARED, ring_fd, 0)
        };
        unsafe { close(ring_fd) };
        
        if ring_vaddr.is_null() || ring_vaddr == (-1isize as *mut ::core::ffi::c_void) {
            return None;
        }

        let ring_ptr = ring_vaddr as *mut protocol::SnpRingControl;
        let cmds_ptr = unsafe { ring_vaddr.add(core::mem::size_of::<protocol::SnpRingControl>()) } as *mut protocol::SnpCommand;
        
        unsafe {
            (*ring_ptr).head = 0;
            (*ring_ptr).tail = 0;
            (*ring_ptr).size = 1024;
        }

        // 2. Setup Pixel Buffer
        let mut path = [0u8; 64];
        path[0..5].copy_from_slice(b"/tmp/");
        path[5..5+name_len].copy_from_slice(&name_bytes[..name_len]);
        let fd = unsafe { open(path.as_ptr() as *const _, O_RDWR | 0x0040, 0) };
        if fd < 0 { return None; }
        
        let size_bytes = (w * h * 4) as usize;
        if eclipse_syscall::ftruncate(fd as usize, size_bytes).is_err() {
            unsafe { close(fd) };
            return None;
        }
        let vaddr = unsafe {
            mmap(core::ptr::null_mut(), size_bytes, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0)
        };
        unsafe { close(fd) };

        if vaddr.is_null() || vaddr == (-1isize as *mut ::core::ffi::c_void) {
            return None;
        }

        // 3. Bootstrap (Unified Ring Buffer Handshake)
        // Note: The message MUST start with "WAYL" tag for the IPC parser to recognize it.
        let mut bootstrap_data = [0u8; 64];
        bootstrap_data[0..4].copy_from_slice(b"WAYL");
        bootstrap_data[4..8].copy_from_slice(b"URB\0");
        let ring_name = b"snp_ring_terminal\0";
        let copy_len = ring_name.len().min(32);
        bootstrap_data[8..8+copy_len].copy_from_slice(&ring_name[..copy_len]);
        IpcChannel::send_raw(composer_pid, MSG_TYPE_WAYLAND, &bootstrap_data);

        let mut surface = Self {
            composer_pid,
            surface_id: 1,
            vaddr: vaddr as *mut u32,
            size_bytes,
            width: w,
            height: h,
            ring_ptr,
            cmds_ptr,
            current_fence: 0,
        };

        // 4. Create Layer via Ring
        let mut cmd = protocol::SnpCommand::new(protocol::SnpOpcode::LayerCreate, 1);
        let mut payload = protocol::SnpPayloadLayerCreate {
            width: w as u16,
            height: h as u16,
            format: 0,
            name: [0; 24],
        };
        let copy_len = name.as_bytes().len().min(23);
        payload.name[..copy_len].copy_from_slice(&name.as_bytes()[..copy_len]);
        unsafe { cmd.set_payload(&payload); }
        surface.submit_command(cmd);
        surface.notify_activity();
        
        Some(surface)
    }

    pub fn notify_activity(&self) {
        let mut notify_data = [0u8; 64];
        notify_data[0..4].copy_from_slice(b"WAYL");
        notify_data[4..8].copy_from_slice(b"SNP\0");
        let _ = IpcChannel::send_raw(self.composer_pid, MSG_TYPE_WAYLAND, &notify_data);
    }

    pub fn submit_command(&mut self, mut cmd: protocol::SnpCommand) {
        unsafe {
            let ring = &mut *self.ring_ptr;
            let next_tail = (ring.tail + 1) % ring.size;
            while next_tail == ring.head {
                yield_cpu();
            }
            
            self.current_fence += 1;
            cmd.fence = self.current_fence;
            
            core::ptr::write_volatile(self.cmds_ptr.add(ring.tail as usize), cmd);
            core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
            ring.tail = next_tail;
        }
    }

    #[inline]
    pub fn buffer(&mut self) -> &mut [u32] {
        let len = (self.width as usize).saturating_mul(self.height as usize);
        unsafe { ::core::slice::from_raw_parts_mut(self.vaddr, len) }
    }

    pub fn commit(&mut self) {
        let cmd = protocol::SnpCommand::new(protocol::SnpOpcode::Commit, self.surface_id);
        self.submit_command(cmd);
        self.notify_activity();
    }

    pub fn poll_event(&self) -> Option<SideWindEvent> {
        let mut buffer = [0u8; 64];
        let mut sender: u32 = 0;
        let len = unsafe { receive(buffer.as_mut_ptr(), buffer.len(), &mut sender) };
        if len >= 64 && sender == self.composer_pid {
            let cmd = unsafe { *(buffer.as_ptr() as *const protocol::SnpCommand) };
            let opcode = unsafe { core::mem::transmute::<u32, protocol::SnpOpcode>(cmd.opcode) };
            match opcode {
                protocol::SnpOpcode::EventKey => {
                    let msg = unsafe { cmd.get_payload::<protocol::SnpPayloadEventKey>() };
                    return Some(SideWindEvent {
                        event_type: SWND_EVENT_TYPE_KEY,
                        data1: msg.key as i32,
                        data2: msg.state as i32,
                        data3: 0,
                    });
                }
                _ => {}
            }
        }
        None
    }

    pub fn width(&self) -> u32 { self.width }
    pub fn height(&self) -> u32 { self.height }
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
        let cmd = protocol::SnpCommand::new(protocol::SnpOpcode::Destroy, self.surface_id);
        self.submit_command(cmd);
        unsafe { 
            munmap(self.vaddr as *mut ::core::ffi::c_void, self.size_bytes);
            munmap(self.ring_ptr as *mut ::core::ffi::c_void, 1024 * 64 + 64);
        };
    }
}
