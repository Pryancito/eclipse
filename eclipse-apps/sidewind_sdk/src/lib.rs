#![no_std]

pub mod font_terminus_12;
pub mod font_terminus_14;
pub mod font_terminus_16;
pub mod font_terminus_18;
pub mod font_terminus_20;
pub mod font_terminus_24;
pub mod ui;

use eclipse_libc::{send, receive, mmap, munmap, open, close, PROT_READ, PROT_WRITE, MAP_SHARED, O_RDWR, yield_cpu};
use sidewind_core::{SideWindMessage, MSG_TYPE_GRAPHICS, MSG_TYPE_INPUT, SWND_OP_CREATE, SWND_OP_DESTROY, SWND_OP_COMMIT, SideWindEvent};

/// Discover the compositor's PID by talking to init (PID 1)
pub fn discover_composer() -> Option<u32> {
    const INIT_PID: u32 = 1;
    let _ = send(INIT_PID, 255, b"GET_DISPLAY_PID");

    let mut buffer = [0u8; 32];
    for _ in 0..100 {
        let (len, sender) = receive(&mut buffer);
        if len >= 8 && sender == INIT_PID && &buffer[0..4] == b"DSPL" {
            let mut pid_bytes = [0u8; 4];
            pid_bytes.copy_from_slice(&buffer[4..8]);
            return Some(u32::from_le_bytes(pid_bytes));
        }
        yield_cpu();
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
        // 1. Create/Open the SHM file in /tmp/
        let mut path = [0u8; 64];
        path[0..5].copy_from_slice(b"/tmp/");
        let name_bytes = name.as_bytes();
        let name_len = name_bytes.len().min(32);
        path[5..5+name_len].copy_from_slice(&name_bytes[..name_len]);
        
        let path_str = unsafe { core::str::from_utf8_unchecked(&path[..5+name_len]) };
        
        // In Eclipse OS, to create a virtual file in /tmp we just open it.
        // We might need a flag to ensure it's created if not exists?
        // Current kernel implementation of DevFS/EclipseFS might need O_CREAT if we had it.
        // For now assume open creates or exists.
        let fd = open(path_str, O_RDWR, 0);
        if fd < 0 {
            return None;
        }

        let size_bytes = (w * h * 4) as usize;
        
        // 2. Map the file into memory
        let vaddr = mmap(0, size_bytes as u64, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        close(fd);

        if vaddr == 0 || vaddr == u64::MAX {
            return None;
        }

        let ptr = vaddr as *mut u32;

        // 3. Send CREATE message to compositor
        let msg = SideWindMessage::new_create(x, y, w, h, name);
        let msg_bytes = unsafe {
            core::slice::from_raw_parts(
                &msg as *const SideWindMessage as *const u8,
                core::mem::size_of::<SideWindMessage>(),
            )
        };
        if send(composer_pid, MSG_TYPE_GRAPHICS, msg_bytes) != 0 {
            unsafe { munmap(vaddr, size_bytes as u64); }
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

    pub fn buffer(&mut self) -> &mut [u32] {
        unsafe {
            core::slice::from_raw_parts_mut(self.vaddr, (self.width * self.height) as usize)
        }
    }

    pub fn commit(&self) {
        let msg = SideWindMessage::new_commit();
        let msg_bytes = unsafe {
            core::slice::from_raw_parts(
                &msg as *const SideWindMessage as *const u8,
                core::mem::size_of::<SideWindMessage>(),
            )
        };
        let _ = send(self.composer_pid, MSG_TYPE_GRAPHICS, msg_bytes);
    }

    pub fn poll_event(&self) -> Option<SideWindEvent> {
        let mut buffer = [0u8; core::mem::size_of::<SideWindEvent>()];
        let (len, sender) = receive(&mut buffer);
        if len == core::mem::size_of::<SideWindEvent>() && sender == self.composer_pid {
            let mut event = SideWindEvent {
                event_type: 0, data1: 0, data2: 0, data3: 0
            };
            unsafe {
                core::ptr::copy_nonoverlapping(
                    buffer.as_ptr(),
                    &mut event as *mut _ as *mut u8,
                    len,
                );
            }
            return Some(event);
        }
        None
    }

    pub fn width(&self) -> u32 { self.width }
    pub fn height(&self) -> u32 { self.height }
    pub fn set_size(&mut self, w: u32, h: u32) {
        self.width = w;
        self.height = h;
    }
}

impl Drop for SideWindSurface {
    fn drop(&mut self) {
        // Send DESTROY message
        let mut msg = SideWindMessage::new_commit(); // reuse helper for empty msg
        msg.op = SWND_OP_DESTROY;
        let msg_bytes = unsafe {
            core::slice::from_raw_parts(
                &msg as *const SideWindMessage as *const u8,
                core::mem::size_of::<SideWindMessage>(),
            )
        };
        let _ = send(self.composer_pid, MSG_TYPE_GRAPHICS, msg_bytes);

        // Unmap memory
        unsafe {
            munmap(self.vaddr as u64, self.size_bytes as u64);
        }
    }
}
