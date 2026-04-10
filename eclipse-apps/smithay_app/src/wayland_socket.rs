//! Standard Wayland Unix socket server for Smithay App.
//!
//! Accepts connections on `/tmp/wayland-0` (the standard Wayland socket path)
//! so that any program linked against `libwayland-client` can connect to the
//! Eclipse OS compositor without changes.

use std::rc::Rc;
use core::cell::RefCell;
use wayland_proto::UnixSocketServer;
use wayland_proto::wl::server::client::ClientId;
use wayland_proto::wl::server::server::WaylandServer;
use wayland_proto::wl::connection::{Connection, RecvError};
use wayland_proto::wl::wire::RawMessage;

/// Unix socket client IDs start here to avoid collisions with internal clients.
const UNIX_CLIENT_ID_BASE: u32 = 0x8000_0000;

/// Manages the standard Wayland Unix socket and routes messages to the shared
/// `WaylandServer` protocol state machine.
pub struct WaylandSocketServer {
    listener: UnixSocketServer,
    next_id: u32,
}

impl WaylandSocketServer {
    /// Bind the Wayland socket at `path` (typically `/tmp/wayland-0`).
    /// Returns `None` if the socket cannot be created or bound.
    pub fn new(path: &str) -> Option<Self> {
        let listener = UnixSocketServer::new(path)?;
        Some(Self {
            listener,
            next_id: UNIX_CLIENT_ID_BASE,
        })
    }

    /// Poll once per frame:
    ///  1. Accept any newly connected clients.
    ///  2. Try to receive and process pending messages from all Unix clients.
    pub fn poll(&mut self, protocol: &mut WaylandServer) -> bool {
        let mut any = false;

        // ── Accept new clients ──────────────────────────────────────────────
        while let Some(conn) = self.listener.accept_nonblocking() {
            let id = ClientId(self.next_id);
            self.next_id = if self.next_id == u32::MAX { UNIX_CLIENT_ID_BASE } else { self.next_id + 1 };

            let conn_rc: Rc<RefCell<dyn Connection>> = Rc::new(RefCell::new(conn));
            protocol.add_client(id, conn_rc);
            // println!("[WL-SOCK] new client 0x{:x}", id.0);
            any = true;
        }

        // ── Receive from existing Unix socket clients ───────────────────────
        let ids: Vec<ClientId> = protocol
            .clients
            .keys()
            .copied()
            .filter(|id| id.0 >= UNIX_CLIENT_ID_BASE)
            .collect();

        let mut disconnected: Vec<ClientId> = Vec::new();

        for id in ids {
            let conn_rc = match protocol.clients.get(&id) {
                Some(c) => c.connection().clone(),
                None    => continue,
            };

            let recv_res = (*conn_rc).borrow().recv();
            match recv_res {
                Ok((data, handles)) => {
                    let mut pos = 0usize;
                    let mut handle_off = 0usize;
                    while pos + 8 <= data.len() {
                        match RawMessage::deserialize_header(&data[pos..]) {
                            Ok((_, _, msg_len)) if pos + msg_len <= data.len() => {
                                let chunk = &data[pos..pos + msg_len];
                                let slots = protocol
                                    .clients
                                    .get(&id)
                                    .and_then(|c| c.handle_arg_count_for_message(chunk).ok())
                                    .unwrap_or(0);
                                let tail = &handles[handle_off..];
                                let _ = protocol.process_message(id, chunk, tail);
                                handle_off += slots.min(tail.len());
                                pos += msg_len;
                                any = true;
                            }
                            _ => break,
                        }
                    }
                }
                Err(RecvError::WouldBlock) => {}
                Err(_) => {
                    disconnected.push(id);
                }
            }
        }

        // Remove disconnected clients
        for id in &disconnected {
            protocol.clients.remove(id);
        }

        any
    }
}
