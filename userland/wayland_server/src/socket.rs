//! Socket and IPC handling for Wayland server

use heapless::Vec;

/// Maximum message size
pub const MAX_MESSAGE_SIZE: usize = 4096;

/// Socket state
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SocketState {
    Closed,
    Listening,
    Connected,
    Error,
}

/// Unix domain socket wrapper
pub struct UnixSocket {
    pub fd: i32,
    pub state: SocketState,
}

impl UnixSocket {
    pub fn new() -> Self {
        Self {
            fd: -1,
            state: SocketState::Closed,
        }
    }

    /// Create and bind to Wayland socket
    pub fn bind(path: &str) -> Result<Self, &'static str> {
        // In a real implementation, this would call socket() syscall
        // For now, return a simulated socket
        Ok(Self {
            fd: 3, // Simulated file descriptor
            state: SocketState::Listening,
        })
    }

    /// Listen for connections
    pub fn listen(&mut self, backlog: i32) -> Result<(), &'static str> {
        if self.state != SocketState::Listening {
            return Err("Socket not in listening state");
        }
        // Call listen() syscall
        self.state = SocketState::Listening;
        Ok(())
    }

    /// Accept a client connection
    pub fn accept(&self) -> Result<i32, &'static str> {
        if self.state != SocketState::Listening {
            return Err("Socket not listening");
        }
        // Call accept() syscall
        // Return simulated client fd
        Ok(4)
    }

    /// Read data from socket
    pub fn read(&self, buffer: &mut [u8]) -> Result<usize, &'static str> {
        // Call read() syscall
        // For now, return 0 (no data)
        Ok(0)
    }

    /// Write data to socket
    pub fn write(&self, data: &[u8]) -> Result<usize, &'static str> {
        // Call write() syscall
        Ok(data.len())
    }

    /// Close socket
    pub fn close(&mut self) -> Result<(), &'static str> {
        if self.fd >= 0 {
            // Call close() syscall
            self.fd = -1;
            self.state = SocketState::Closed;
        }
        Ok(())
    }
}

/// Message buffer for reading/writing
pub struct MessageBuffer {
    pub data: Vec<u8, MAX_MESSAGE_SIZE>,
}

impl MessageBuffer {
    pub fn new() -> Self {
        Self {
            data: Vec::new(),
        }
    }

    pub fn clear(&mut self) {
        self.data.clear();
    }

    pub fn push(&mut self, byte: u8) -> Result<(), &'static str> {
        self.data.push(byte).map_err(|_| "Buffer full")
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.data
    }
}
