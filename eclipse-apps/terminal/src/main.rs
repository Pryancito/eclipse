#![no_std]
#![no_main]

extern crate alloc;
extern crate eclipse_std as std;

use std::prelude::v1::*;
use alloc::rc::Rc;
use core::cell::RefCell;
use wayland_proto::wl::{ObjectId, NewId, Message, RawMessage, Interface, connection::Connection};
use wayland_proto::EclipseWaylandConnection;
use wayland_proto::wl::protocols::common::{wl_registry, wl_compositor, wl_shm, wl_surface, wl_buffer};
use wayland_proto::wl::protocols::common::wl_display::WlDisplay;

#[no_mangle]
pub fn main() {
    std::init_runtime();

    std::println!("--- Terminal (Kazari-based Wayland) ---");

    // In Eclipse OS, Lunas is typically started by init.
    // For this prototype, we'll assume Lunas is PID 2.
    let lunas_pid = 2; 
    let self_pid = 100; // Placeholder, in real app we'd get this from kernel

    std::println!("Connecting to Lunas (PID {})...", lunas_pid);

    let connection = Rc::new(RefCell::new(EclipseWaylandConnection::new(lunas_pid, self_pid)));
    
    // Object 1 is always the wl_display
    let mut display = WlDisplay::new(connection.clone(), ObjectId(1));

    // Request the registry (new object ID 2)
    let registry_id = NewId(2);
    std::println!("Requesting wl_registry (ID 2)...");
    if let Err(e) = display.get_registry(registry_id) {
        std::println!("Failed to send get_registry: {:?}", e);
        return;
    }

    std::println!("Handshake sent. Waiting for events...");

    // Main loop to receive events
    let mut registry = wl_registry::WlRegistry::new(connection.clone(), ObjectId(2));
    let mut compositor_id = None;
    let mut shm_id = None;

    loop {
        let recv_res = (*connection).borrow().recv();
        match recv_res {
            Ok((data_vec, _handles)) => {
                let data: &[u8] = &data_vec[..];
                if let Ok((id, op, _len)) = RawMessage::deserialize_header(data) {
                    if id == ObjectId(2) {
                        let raw = RawMessage::deserialize(data, wayland_proto::wl::protocols::common::wl_registry::WlRegistry::PAYLOAD_TYPES[op.0 as usize], &[])
                            .unwrap();
                        if let Ok(event) = wl_registry::Event::from_raw(connection.clone(), &raw) {
                            match event {
                                wl_registry::Event::Global { name, interface, version } => {
                                    std::println!("Registry: Global {} {} v{}", name, interface, version);
                                    if interface == "wl_compositor" {
                                        let id = NewId(3);
                                        std::println!("Binding to wl_compositor (ID 3)...");
                                        registry.bind(name, id).unwrap();
                                        compositor_id = Some(id.as_id());
                                    } else if interface == "wl_shm" {
                                        let id = NewId(4);
                                        std::println!("Binding to wl_shm (ID 4)...");
                                        registry.bind(name, id).unwrap();
                                        shm_id = Some(id.as_id());
                                    }
                                }
                                _ => {}
                            }
                        }
                    } else {
                        std::println!("Received message for object {:?}: Opcode={:?}", id, op);
                    }
                }
            }
            Err(e) => {
                std::println!("Recv error or timeout: {:?}", e);
            }
        }

        if compositor_id.is_some() && shm_id.is_some() {
            std::println!("Handshake complete! Both compositor and shm bound.");
            break;
        }
    }

    std::println!("Terminal initialized successfully.");
    loop {
        // Keep running...
        std::thread::yield_now();
    }
}
