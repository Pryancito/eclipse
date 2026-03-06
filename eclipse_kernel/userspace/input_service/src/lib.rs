//! Librería del input_service: cola de eventos y tests.

extern crate std;
extern crate alloc;

use std::libc::InputEvent;

/// Cola de eventos de entrada (256 slots).
pub struct EventQueue {
    events: [InputEvent; 256],
    head: usize,
    tail: usize,
    count: usize,
}

impl EventQueue {
    pub fn new() -> Self {
        EventQueue {
            events: [InputEvent {
                device_id: 0,
                event_type: 0,
                code: 0,
                value: 0,
                timestamp: 0,
            }; 256],
            head: 0,
            tail: 0,
            count: 0,
        }
    }

    pub fn push(&mut self, event: InputEvent) -> bool {
        if self.count >= 256 {
            return false;
        }
        self.events[self.tail] = event;
        self.tail = (self.tail + 1) % 256;
        self.count += 1;
        true
    }

    pub fn pop(&mut self) -> Option<InputEvent> {
        if self.count == 0 {
            return None;
        }
        let event = self.events[self.head];
        self.head = (self.head + 1) % 256;
        self.count -= 1;
        Some(event)
    }

    /// Número de eventos actualmente en la cola (para debug/heartbeat).
    #[inline]
    pub fn count(&self) -> usize {
        self.count
    }
}

#[cfg(feature = "test")]
pub mod tests {
    use super::*;

    fn make_event(id: u32, ts: u64) -> InputEvent {
        InputEvent {
            device_id: id,
            event_type: 0,
            code: 1,
            value: 1i32,
            timestamp: ts,
        }
    }

    pub fn event_queue_empty_after_new() {
        let mut q = EventQueue::new();
        assert!(q.pop().is_none());
    }

    pub fn event_queue_push_pop_one() {
        let mut q = EventQueue::new();
        let ev = make_event(0, 1);
        assert!(q.push(ev));
        assert!(q.pop().as_ref() == Some(&ev));
        assert!(q.pop().is_none());
    }

    pub fn event_queue_fifo_order() {
        let mut q = EventQueue::new();
        for i in 0..10u64 {
            let ev = make_event(0, i);
            assert!(q.push(ev));
        }
        for i in 0..10u64 {
            let ev = q.pop().expect("event");
            assert_eq!(ev.timestamp, i);
        }
        assert!(q.pop().is_none());
    }

    pub fn event_queue_full_rejects_push() {
        let mut q = EventQueue::new();
        for i in 0..256u64 {
            assert!(q.push(make_event(0, i)), "push {} should succeed", i);
        }
        assert!(!q.push(make_event(0, 256)));
        assert!(q.pop().is_some());
        assert!(q.push(make_event(0, 256)));
    }

    pub fn event_queue_stress_10000_cycles() {
        let mut q = EventQueue::new();
        for cycle in 0..10_000u32 {
            for i in 0..32 {
                let ev = make_event(cycle as u32, i as u64);
                assert!(q.push(ev), "cycle {} push {}", cycle, i);
            }
            for i in 0..32 {
                let ev = q.pop().expect("pop");
                assert_eq!(ev.timestamp, i as u64);
                assert_eq!(ev.device_id, cycle);
            }
        }
        assert!(q.pop().is_none());
    }

    pub fn event_queue_bench_50k_ops() {
        let mut q = EventQueue::new();
        for i in 0..50_000u64 {
            let ev = make_event(0, i);
            while !q.push(ev) {
                let _ = q.pop();
            }
        }
        let mut count = 0u64;
        while q.pop().is_some() {
            count += 1;
        }
        assert!(count > 0 && count <= 50_000);
    }

    pub fn run_all() {
        event_queue_empty_after_new();
        event_queue_push_pop_one();
        event_queue_fifo_order();
        event_queue_full_rejects_push();
        event_queue_stress_10000_cycles();
        event_queue_bench_50k_ops();
        println!("[INPUT-SERVICE] All tests passed.");
    }
}
