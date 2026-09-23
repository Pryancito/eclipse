use crate::error::{LxError, LxResult};
use alloc::{boxed::Box, vec::Vec};
use lock::Mutex;
use smoltcp::wire::IpEndpoint;

const PORT_NUM: usize = 65536;

pub struct ListenTableEntry {
    pub listen_endpoint: IpEndpoint,
}

pub struct ListenTable {
    tcp: Box<[Mutex<Option<Box<ListenTableEntry>>>]>,
}

impl Default for ListenTable {
    fn default() -> Self {
        Self::new()
    }
}

impl ListenTable {
    pub fn new() -> Self {
        let mut vec = Vec::with_capacity(PORT_NUM);
        for _ in 0..PORT_NUM {
            vec.push(Mutex::new(None));
        }
        Self {
            tcp: vec.into_boxed_slice(),
        }
    }

    pub fn can_listen(&self, port: u16) -> bool {
        self.tcp[port as usize].lock().is_none()
    }

    pub fn listen(&self, listen_endpoint: IpEndpoint) -> LxResult<()> {
        let port = listen_endpoint.port;
        if port == 0 {
            return Err(LxError::EINVAL);
        }
        let mut entry = self.tcp[port as usize].lock();
        if entry.is_none() {
            *entry = Some(Box::new(ListenTableEntry { listen_endpoint }));
            Ok(())
        } else {
            Err(LxError::EADDRINUSE)
        }
    }

    pub fn unlisten(&self, port: u16) {
        *self.tcp[port as usize].lock() = None;
    }
}

/// Whether two endpoints claim the same port on overlapping addresses:
/// the wildcard overlaps everything, a specific address only itself
/// (`inet_rcv_saddr_equal`).
pub fn endpoints_collide(a: IpEndpoint, b: IpEndpoint) -> bool {
    a.port == b.port && (a.addr.is_unspecified() || b.addr.is_unspecified() || a.addr == b.addr)
}

/// A port a TCP socket took with `bind(2)`.
#[derive(Clone, Copy, Debug)]
pub struct BoundPort {
    pub endpoint: IpEndpoint,
    /// `SO_REUSEADDR` was set when the socket bound.
    pub reuse_addr: bool,
}

/// Linux's bind hash bucket, for the sockets smoltcp cannot see: a socket
/// that has bound but neither listens nor connects yet exists only in
/// `TcpInner`, so without this table a second `bind` on the same port
/// succeeded and the two servers found out at `listen`, or never (two
/// clients that bound the same source port).
pub struct BindTable {
    tcp: Mutex<Vec<BoundPort>>,
}

impl Default for BindTable {
    fn default() -> Self {
        Self::new()
    }
}

impl BindTable {
    pub fn new() -> Self {
        Self {
            tcp: Mutex::new(Vec::new()),
        }
    }

    pub fn insert(&self, endpoint: IpEndpoint, reuse_addr: bool) {
        self.tcp.lock().push(BoundPort {
            endpoint,
            reuse_addr,
        });
    }

    /// Give back one registration of `endpoint` (there is one per socket).
    pub fn release(&self, endpoint: IpEndpoint) {
        let mut tcp = self.tcp.lock();
        if let Some(i) = tcp.iter().position(|b| b.endpoint == endpoint) {
            tcp.swap_remove(i);
        }
    }

    pub fn snapshot(&self) -> Vec<BoundPort> {
        self.tcp.lock().clone()
    }
}

lazy_static::lazy_static! {
    pub static ref LISTEN_TABLE: ListenTable = ListenTable::new();
    pub static ref BIND_TABLE: BindTable = BindTable::new();
}
