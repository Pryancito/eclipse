//! Standard Wayland Unix socket server for Lunas.
//!
//! Accepts connections on `/tmp/wayland-0` (the standard Wayland socket path)
//! so that any program linked against `libwayland-client` can connect to the
//! Eclipse OS compositor without changes.
//!
//! This module is polled every frame from the Lunas main loop.

use std::prelude::v1::*;
use alloc::rc::Rc;
use core::cell::RefCell;
use wayland_proto::UnixSocketServer;
use wayland_proto::unix_transport::UnixSocketConnection;
use wayland_proto::wl::server::client::ClientId;
use wayland_proto::wl::server::server::WaylandServer;
use wayland_proto::wl::connection::{Connection, RecvError};
use wayland_proto::wl::wire::{RawMessage, Handle};

/// Unix socket client IDs start here to avoid collisions with Eclipse IPC
/// clients (which use the process PID, always < 2^31).
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
    ///
    /// Returns `true` if any activity occurred (new client or messages processed).
    pub fn poll(&mut self, protocol: &mut WaylandServer) -> bool {
        let mut any = false;

        // ── Accept new clients ──────────────────────────────────────────────
        while let Some(conn) = self.listener.accept_nonblocking() {
            let id = ClientId(self.next_id);
            self.next_id = if self.next_id == u32::MAX { UNIX_CLIENT_ID_BASE } else { self.next_id + 1 };

            // Coerce Rc<RefCell<UnixSocketConnection>> → Rc<RefCell<dyn Connection>>
            let conn_rc: Rc<RefCell<dyn Connection>> = Rc::new(RefCell::new(conn));
            protocol.add_client(id, conn_rc);
            any = true;
        }

        // ── Receive from existing Unix socket clients ───────────────────────
        // Collect Unix-socket client IDs first to avoid holding an active
        // borrow while we call process_message (which needs &mut protocol).
        let ids: Vec<ClientId> = protocol
            .clients
            .keys()
            .copied()
            .filter(|id| id.0 >= UNIX_CLIENT_ID_BASE)
            .collect();

        let mut disconnected: Vec<ClientId> = Vec::new();

        for id in ids {
            // Clone the connection Rc so we can release the immutable borrow
            // before calling the mutable process_message.
            let conn_rc = match protocol.clients.get(&id) {
                Some(c) => c.connection().clone(),
                None    => continue,
            };

            // Try to receive data from this client's socket.
            let recv_res = (*conn_rc).borrow().recv();
            match recv_res {
                Ok((data, handles)) => {
                    // There may be multiple Wayland messages concatenated.
                    // Handles (fds via SCM_RIGHTS) arrive with the batch; associate them
                    // with the first message that expects them (wl_shm.create_pool uses fd).
                    let mut pos = 0usize;
                    let mut handles_remaining = handles;
                    while pos + 8 <= data.len() {
                        match RawMessage::deserialize_header(&data[pos..]) {
                            Ok((_, _, msg_len)) if pos + msg_len <= data.len() => {
                                let chunk = &data[pos..pos + msg_len];
                                let _ = protocol.process_message(id, chunk, &handles_remaining);
                                // Consume handles after first message that could use them.
                                handles_remaining = (&[]).to_vec();
                                pos += msg_len;
                                any = true;
                            }
                            _ => break,
                        }
                    }
                }
                Err(RecvError::WouldBlock) => {
                    // Non-blocking socket: no data ready yet.  The client is still
                    // alive — do nothing and check again next frame.
                }
                Err(_) => {
                    // Fatal I/O error or EOF — client disconnected.
                    disconnected.push(id);
                }
            }
        }

        // Remove disconnected clients
        for id in disconnected {
            protocol.clients.remove(&id);
        }

        any
    }
}

