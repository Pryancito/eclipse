use std::prelude::v1::*;
use alloc::rc::Rc;
use core::cell::RefCell;
use wayland_proto::wl::{ObjectId, NewId, Payload, Message};
use wayland_proto::wl::server::client::{Client, ClientId};
use wayland_proto::wl::server::objects::{Object, ObjectInner, ObjectLogic, ServerError};
use wayland_proto::wl::protocols::common::{wl_compositor, wl_surface, wl_shm, wl_display, wl_registry};
use crate::compositor::{ShellWindow, WindowContent};

/// Lunas implementation of wl_compositor.
pub struct LunasCompositor;

impl ObjectLogic for LunasCompositor {
    fn handle_request(
        &mut self,
        client: &mut Client,
        opcode: u16,
        args: &[Payload],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => { // create_surface
                let id = match args[0] {
                    Payload::NewId(id) => id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                println!("[LUNAS] wl_compositor::create_surface id={:?}", id);
                let surface = ObjectInner::Rc(Rc::new(RefCell::new(LunasSurface::new(id.as_id()))));
                client.add_object(id, Object::new::<wl_surface::WlSurface>(id, surface));
                Ok(())
            }
            _ => Err(ServerError::ObjectMismatch),
        }
    }
}

/// Lunas implementation of wl_surface.
pub struct LunasSurface {
    id: ObjectId,
    pub title: String,
}

impl LunasSurface {
    pub fn new(id: ObjectId) -> Self {
        Self { id, title: String::from("Wayland Window") }
    }
}

impl ObjectLogic for LunasSurface {
    fn handle_request(
        &mut self,
        _client: &mut Client,
        opcode: u16,
        args: &[Payload],
    ) -> Result<(), ServerError> {
        match opcode {
            6 => { // commit
                println!("[LUNAS] wl_surface::commit id={:?}", self.id);
                Ok(())
            }
            _ => {
                // Handle attach, damage, etc.
                Ok(())
            }
        }
    }
}

/// Lunas implementation of wl_shm.
pub struct LunasShm;

impl ObjectLogic for LunasShm {
    fn handle_request(
        &mut self,
        client: &mut Client,
        opcode: u16,
        args: &[Payload],
    ) -> Result<(), ServerError> {
        match opcode {
            0 => { // create_pool
                let id = match args[0] {
                    Payload::NewId(id) => id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let fd = match args[1] {
                     Payload::Int(fd) => fd,
                     _ => return Err(ServerError::MessageDeserializeError),
                };
                let size = match args[2] {
                     Payload::Int(size) => size,
                     _ => return Err(ServerError::MessageDeserializeError),
                };
                println!("[LUNAS] wl_shm::create_pool id={:?} fd={} size={}", id, fd, size);
                // Implementation for shm pool would go here
                Ok(())
            }
            _ => Err(ServerError::ObjectMismatch),
        }
    }
}

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
        content: WindowContent::Snp { surface_id: surface_id.0, pid: 0 }, // Reusing Snp content for simplicity for now
        workspace,
        title: title_buf,
        ..Default::default()
    }
}

#[cfg(test)]
mod wayland_server_tests {
    use super::{LunasCompositor, LunasShm};
    use alloc::rc::Rc;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use wayland_proto::eclipse_transport::EclipseWaylandConnection;
    use wayland_proto::wl::protocols::common::wl_compositor::WlCompositor;
    use wayland_proto::wl::protocols::common::wl_display::Request;
    use wayland_proto::wl::protocols::common::wl_shm::WlShm;
    use wayland_proto::wl::server::client::ClientId;
    use wayland_proto::wl::server::objects::{Object, ObjectInner, ObjectLogic};
    use wayland_proto::wl::server::server::WaylandServer;
    use wayland_proto::wl::{Message, NewId, ObjectId};

    fn sample_server() -> WaylandServer {
        let mut server = WaylandServer::new();
        server.register_global(
            "wl_compositor",
            4,
            || ObjectInner::Rc(Rc::new(RefCell::new(LunasCompositor))),
            |id, inner| Object::new::<WlCompositor>(id, inner),
        );
        server.register_global(
            "wl_shm",
            1,
            || ObjectInner::Rc(Rc::new(RefCell::new(LunasShm))),
            |id, inner| Object::new::<WlShm>(id, inner),
        );
        server
    }

    /// Regresión: `wl_display::PAYLOAD_TYPES[2]` debe ser un solo `NewId` (get_registry), no el layout del evento error.
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
        let r = server.process_message(ClientId(42), &buf[..len]);
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
        let mut client = wayland_proto::wl::server::client::Client::new(
            ClientId(1),
            Rc::new(RefCell::new(EclipseWaylandConnection::new(1, 2))),
        );
        let mut comp = LunasCompositor;
        let args = alloc::vec![
            wayland_proto::wl::Payload::NewId(NewId(5)),
        ];
        let r = ObjectLogic::handle_request(&mut comp, &mut client, 0, &args);
        assert!(r.is_ok(), "create_surface: {:?}", r);
        assert!(client.object_mut(ObjectId(5)).is_ok());
    }
}
