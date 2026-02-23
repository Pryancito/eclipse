use eclipse_libc::{receive, send, yield_cpu, InputEvent};
const IPC_BUFFER_SIZE: usize = 256;
use sidewind_core::{SideWindMessage, SIDEWIND_TAG, SWND_OP_CREATE, SWND_OP_DESTROY, SWND_OP_UPDATE, SWND_OP_COMMIT};
use crate::input::{CompositorEvent, InputState};
use crate::compositor::{ExternalSurface, ShellWindow, WindowContent, MAX_EXTERNAL_SURFACES, MAX_WINDOWS_COUNT, MAX_SURFACE_DIM, MAX_SURFACE_BYTES};
const MSG_TYPE_INPUT: u32 = 0x00000040;

pub struct IpcHandler {
    pub message_count: u64,
}

impl IpcHandler {
    pub fn new() -> Self {
        Self { message_count: 0 }
    }

    pub fn process_messages(&mut self) -> Option<CompositorEvent> {
        let mut buffer = [0u8; IPC_BUFFER_SIZE];
        let (len, sender_pid) = receive(&mut buffer);
        let len = len.min(buffer.len());
        if len > 0 {
            self.message_count += 1;
            if len >= core::mem::size_of::<SideWindMessage>() && &buffer[0..4] == b"SWND" {
                let sw = unsafe { core::ptr::read_unaligned(buffer.as_ptr() as *const SideWindMessage) };
                if sw.tag == SIDEWIND_TAG { return Some(CompositorEvent::SideWind(sw, sender_pid)); }
            }
            if len == core::mem::size_of::<InputEvent>() {
                let ev = unsafe { core::ptr::read_unaligned(buffer.as_ptr() as *const InputEvent) };
                return Some(CompositorEvent::Input(ev));
            }
        }
        None
    }
}

pub fn query_input_service_pid() -> Option<u32> {
    let _ok = send(1, 0x00000040, b"GET_INPUT_PID") == 0;
    let mut buffer = [0u8; IPC_BUFFER_SIZE];
    for _ in 0..1000 {
        let (len, sender_pid) = receive(&mut buffer);
        if len >= 8 && sender_pid == 1 && &buffer[0..4] == b"INPT" {
            let mut id = [0u8; 4]; id.copy_from_slice(&buffer[4..8]);
            return Some(u32::from_le_bytes(id));
        }
        yield_cpu();
    }
    None
}

pub fn subscribe_to_input_service(input_pid: u32, self_pid: u32) {
    let mut msg = [0u8; 8]; msg[0..4].copy_from_slice(b"SUBS"); msg[4..8].copy_from_slice(&self_pid.to_le_bytes());
    let _ = send(input_pid, 0x00000040, &msg);
}

pub fn handle_sidewind_message(msg: SideWindMessage, sender_pid: u32, surfaces: &mut [ExternalSurface], windows: &mut [ShellWindow], window_count: &mut usize, input_state: &mut InputState) {
    match msg.op {
        SWND_OP_CREATE => {
            if msg.w == 0 || msg.h == 0 || msg.w > MAX_SURFACE_DIM || msg.h > MAX_SURFACE_DIM { return; }
            let surface_idx = surfaces.iter().position(|s| !s.active);
            if let Some(s_idx) = surface_idx {
                if *window_count < windows.len() {
                    surfaces[s_idx] = ExternalSurface { id: sender_pid, pid: sender_pid, vaddr: 0x1000, buffer_size: (msg.w * msg.h * 4) as usize, active: true };
                    windows[*window_count] = ShellWindow { x: msg.x, y: msg.y, w: msg.w as i32, h: msg.h as i32 + 26, curr_x: msg.x as f32, curr_y: msg.y as f32, curr_w: msg.w as f32, curr_h: (msg.h as i32 + 26) as f32, minimized: false, maximized: false, closing: false, stored_rect: (msg.x, msg.y, msg.w as i32, msg.h as i32 + 26), workspace: input_state.current_workspace, content: WindowContent::External(s_idx as u32) };
                    *window_count += 1;
                }
            }
        }
        SWND_OP_DESTROY => {
            if let Some(s_idx) = surfaces.iter().position(|s| s.active && s.pid == sender_pid) {
                surfaces[s_idx].active = false;
                if let Some(w_idx) = windows.iter().position(|w| w.content == WindowContent::External(s_idx as u32)) {
                    for i in w_idx..(*window_count - 1) { windows[i] = windows[i+1]; }
                    *window_count -= 1;
                }
            }
        }
        _ => {}
    }
}
