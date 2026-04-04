//! Backend — hardware/OS interface for Lunas desktop.
//! Encapsulates framebuffer, IPC, and input scheme access.

use crate::render::FramebufferState;
use crate::ipc::IpcHandler;
use crate::input::CompositorEvent;
use core::option::Option::{self, Some};
#[cfg(target_os = "eclipse")]
use libc::{open, read, InputEvent, O_RDONLY, O_NONBLOCK};
#[cfg(not(target_os = "eclipse"))]
use eclipse_syscall::InputEvent;
#[cfg(not(target_os = "eclipse"))]
use libc::{open, read, O_RDONLY, O_NONBLOCK};
#[cfg(test)]
use std::collections::VecDeque;
#[cfg(not(test))]
use heapless::Deque;

/// Backend represents the hardware/OS interface.
/// It encapsulates the Framebuffer and IPC capabilities.
pub struct Backend {
    pub fb: FramebufferState,
    pub ipc: IpcHandler,
    input_fd: Option<i32>,
    #[cfg(test)]
    input_queue: VecDeque<InputEvent>,
    #[cfg(not(test))]
    ipc_pending: Deque<CompositorEvent, 256>,
}

impl Backend {
    pub fn new() -> Option<Self> {
        #[cfg(not(test))]
        let fb = crate::render::FramebufferState::init()?;
        #[cfg(test)]
        let fb = crate::render::FramebufferState::mock();

        let ipc = IpcHandler::new();

        #[cfg(not(test))]
        let input_fd = {
            #[cfg(target_os = "eclipse")]
            let fd = unsafe { open(b"input:\0".as_ptr() as *const core::ffi::c_char, O_RDONLY | O_NONBLOCK, 0) };
            #[cfg(not(target_os = "eclipse"))]
            let fd = unsafe { open(b"input:\0".as_ptr() as *const i8, O_RDONLY | O_NONBLOCK, 0) };
            if fd >= 0 { Some(fd) } else { None }
        };
        #[cfg(test)]
        let input_fd = None;

        Some(Self {
            fb,
            ipc,
            input_fd,
            #[cfg(test)]
            input_queue: VecDeque::new(),
            #[cfg(not(test))]
            ipc_pending: Deque::new(),
        })
    }

    pub fn poll_event(&mut self) -> Option<CompositorEvent> {
        if let Some(ev) = self.poll_input_scheme_event() {
            return Some(ev);
        }
        #[cfg(not(test))]
        if let Some(ev) = self.ipc_pending.pop_front() {
            return Some(ev);
        }
        self.ipc.process_messages()
    }

    /// Drain IPC mailbox into pending queue at frame start to prevent overflow.
    #[cfg(not(test))]
    pub fn drain_ipc_into_pending(&mut self, max: usize) {
        for _ in 0..max {
            if let Some(ev) = self.ipc.process_messages() {
                if self.ipc_pending.push_back(ev).is_err() {
                    break;
                }
            } else {
                break;
            }
        }
    }

    fn poll_input_scheme_event(&mut self) -> Option<CompositorEvent> {
        #[cfg(test)]
        {
            self.input_queue.pop_front().map(CompositorEvent::Input)
        }

        #[cfg(not(test))]
        {
            let fd = self.input_fd?;
            let mut buf = [0u8; core::mem::size_of::<InputEvent>()];
            #[cfg(target_os = "eclipse")]
            let n = unsafe { read(fd as i32, buf.as_mut_ptr() as *mut core::ffi::c_void, buf.len()) };
            #[cfg(not(target_os = "eclipse"))]
            let n = unsafe { read(fd as i32, buf.as_mut_ptr() as *mut core::ffi::c_void, buf.len()) };
            if n < 0 {
                return None;
            }
            if n as usize != buf.len() {
                return None;
            }
            let ev = unsafe { core::ptr::read_unaligned::<InputEvent>(buf.as_ptr() as *const InputEvent) };
            Some(CompositorEvent::Input(ev))
        }
    }

    #[cfg(test)]
    pub fn push_mock_input_event(&mut self, ev: InputEvent) {
        self.input_queue.push_back(ev);
    }

    pub fn swap_buffers(&mut self) {
        let _ = self.fb.present();
    }

    pub fn input_scheme_available(&self) -> bool {
        self.input_fd.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_input_event(code: u16, value: i32) -> InputEvent {
        InputEvent { device_id: 0, event_type: 0, code, value, timestamp: 0 }
    }

    #[test]
    fn input_scheme_has_priority_over_ipc() {
        let mut backend = Backend::new().expect("backend");
        let scheme_ev = sample_input_event(0x1E, 1);
        let ipc_ev = sample_input_event(0x30, 1);

        backend.ipc.mock_events.push(CompositorEvent::Input(ipc_ev));
        backend.push_mock_input_event(scheme_ev);

        match backend.poll_event() {
            Some(CompositorEvent::Input(ev)) => assert_eq!(ev.code, scheme_ev.code),
            _ => panic!("expected Input from scheme"),
        }
    }

    #[test]
    fn falls_back_to_ipc_when_input_scheme_empty() {
        let mut backend = Backend::new().expect("backend");
        let ipc_ev = sample_input_event(0x20, 1);
        backend.ipc.mock_events.push(CompositorEvent::Input(ipc_ev));

        match backend.poll_event() {
            Some(CompositorEvent::Input(ev)) => assert_eq!(ev.code, ipc_ev.code),
            _ => panic!("expected Input from ipc fallback"),
        }
    }

    #[test]
    fn stress_poll_event_empty_is_stable() {
        let mut backend = Backend::new().expect("backend");
        for _ in 0..50_000 {
            assert!(backend.poll_event().is_none());
        }
    }
}
