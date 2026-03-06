use crate::render::FramebufferState;
use crate::ipc::IpcHandler;
use crate::input::CompositorEvent;
use core::option::Option::{self, Some};
use std::libc::{eclipse_open, eclipse_read, InputEvent, O_RDONLY};
#[cfg(test)]
use alloc::collections::VecDeque;

/// Backend represents the hardware/OS interface.
/// It encapsulates the Framebuffer and IPC capabilities.
pub struct Backend {
    pub fb: FramebufferState,
    pub ipc: IpcHandler,
    input_fd: Option<i32>,
    #[cfg(test)]
    input_queue: VecDeque<InputEvent>,
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
            let fd = eclipse_open("input:", O_RDONLY, 0);
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
        })
    }

    pub fn poll_event(&mut self) -> Option<CompositorEvent> {
        if let Some(ev) = self.poll_input_scheme_event() {
            return Some(ev);
        }
        self.ipc.process_messages()
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
            let n = eclipse_read(fd as u32, &mut buf);
            if n < 0 {
                return None;
            }
            if n as usize != buf.len() {
                return None;
            }
            let ev = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const InputEvent) };
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_input_event(code: u16, value: i32) -> InputEvent {
        InputEvent {
            device_id: 0,
            event_type: 0,
            code,
            value,
            timestamp: 0,
        }
    }

    fn sample_input_event_with_device(device_id: u32, code: u16, value: i32, timestamp: u64) -> InputEvent {
        InputEvent {
            device_id,
            event_type: 0,
            code,
            value,
            timestamp,
        }
    }

    #[test]
    fn input_scheme_has_priority_over_ipc() {
        let mut backend = Backend::new().expect("backend");
        let scheme_ev = sample_input_event(0x1E, 1);
        let ipc_ev = sample_input_event(0x30, 1);

        // Queue an IPC input event (fallback path)...
        backend.ipc.mock_events.push(CompositorEvent::Input(ipc_ev));
        // ...and also a scheme input event.
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
    fn stress_input_scheme_priority_then_ipc_drains() {
        let mut backend = Backend::new().expect("backend");

        // Mantener el test rápido pero con suficiente carga para detectar
        // regresiones (p. ej. starvation o orden incorrecto).
        const SCHEME_N: usize = 10_000;
        const IPC_N: usize = 10_000;

        // Rellenar IPC primero para maximizar presión sobre el fallback.
        for i in 0..IPC_N {
            let ev = sample_input_event_with_device(2, (i as u16).wrapping_add(0x2000), 1, i as u64);
            backend.ipc.mock_events.push(CompositorEvent::Input(ev));
        }

        for i in 0..SCHEME_N {
            let ev = sample_input_event_with_device(1, (i as u16).wrapping_add(0x1000), 1, i as u64);
            backend.push_mock_input_event(ev);
        }

        // Primero deben salir TODOS los eventos de input: (device_id=1)…
        for _ in 0..SCHEME_N {
            match backend.poll_event() {
                Some(CompositorEvent::Input(ev)) => assert_eq!(ev.device_id, 1),
                _ => panic!("expected scheme input event during scheme drain"),
            }
        }

        // …y después se debe drenar IPC (device_id=2) por completo.
        for _ in 0..IPC_N {
            match backend.poll_event() {
                Some(CompositorEvent::Input(ev)) => assert_eq!(ev.device_id, 2),
                _ => panic!("expected ipc input event during ipc drain"),
            }
        }

        assert!(backend.poll_event().is_none(), "queues should be empty after draining");
    }

    #[test]
    fn stress_ipc_only_drains_all_events() {
        let mut backend = Backend::new().expect("backend");
        const N: usize = 20_000;

        for i in 0..N {
            let ev = sample_input_event_with_device(2, (i as u16).wrapping_add(0x3000), 1, i as u64);
            backend.ipc.mock_events.push(CompositorEvent::Input(ev));
        }

        let mut count = 0usize;
        while let Some(CompositorEvent::Input(_ev)) = backend.poll_event() {
            count += 1;
            if count > N {
                panic!("received more events than queued");
            }
        }
        assert_eq!(count, N);
    }

    #[test]
    fn stress_poll_event_empty_is_stable() {
        let mut backend = Backend::new().expect("backend");
        for _ in 0..50_000 {
            assert!(backend.poll_event().is_none());
        }
    }
}
