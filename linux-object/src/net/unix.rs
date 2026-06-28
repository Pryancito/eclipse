use crate::fs::{FileLike, OpenFlags, PollEvents, PollStatus};
use crate::{
    error::{LxError, LxResult},
    net::{Endpoint, Socket, SysResult},
    sync::{Event, EventBus},
};
use alloc::{
    boxed::Box,
    collections::VecDeque,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use async_trait::async_trait;
use hashbrown::HashMap;
use lazy_static::lazy_static;
use lock::Mutex;
use zircon_object::object::*;

lazy_static! {
    static ref UNIX_SOCKETS: Mutex<HashMap<String, Weak<UnixSocketState>>> =
        Mutex::new(HashMap::new());
}

const MAX_UNIX_SOCKET_REGISTRY: usize = 1024;

fn purge_dead_registry(map: &mut HashMap<String, Weak<UnixSocketState>>) {
    map.retain(|_, weak| weak.strong_count() > 0);
}

/// Unix domain socket (AF_UNIX / AF_LOCAL) implementation.
///
/// Supports the full AF_UNIX workflow (as used by many DHCP clients and daemons):
/// - Server: socket → bind → listen → accept
/// - Client: socket → connect  (→ ECONNREFUSED if no listener)
pub struct UnixSocketState {
    base: KObjectBase,
    inner: Arc<Mutex<UnixInner>>,
}

#[derive(Debug)]
struct UnixInner {
    flags: OpenFlags,
    /// Local bound path (set by bind or inherited on accept)
    path: String,
    /// Weak ref to the connected peer socket's inner state
    peer: Option<Weak<Mutex<UnixInner>>>,
    /// Inbound data buffer
    buffer: VecDeque<u8>,
    /// Monotonic total bytes ever appended to `buffer` (for SCM_RIGHTS fd/byte
    /// stream synchronization).
    total_written: usize,
    /// Monotonic total bytes ever consumed from `buffer`.
    total_read: usize,
    eventbus: EventBus,
    /// True after listen() is called
    is_listening: bool,
    /// Pending connections waiting for accept()
    accept_queue: VecDeque<Arc<UnixSocketState>>,
    /// True once a successful connect() has completed both ends
    connected: bool,
    /// True when the peer has closed / disconnected
    peer_closed: bool,
    read_closed: bool,
    write_closed: bool,
    /// PID of the process that created this socket, reported to the *peer* via
    /// `SO_PEERCRED` (seatd reads it to authorize a Wayland client). `0` until
    /// set by `sys_socket`.
    owner_pid: i32,
    /// File descriptors handed to us by the peer via `SCM_RIGHTS`. Each batch is
    /// tagged with the `total_written` stream offset at which it was attached
    /// (the end of the carrying message's bytes), so a `recvmsg` only receives
    /// the fds once it has consumed the bytes they accompanied. Without this
    /// byte/fd synchronization a `recvmsg` reading an fd-less message (e.g.
    /// seatd's ENABLE_SEAT event) would steal the fd queued for a later
    /// OPEN_DEVICE reply, so the compositor's device fd arrives mismatched.
    pending_fds: VecDeque<(usize, Vec<Arc<dyn FileLike>>)>,
}

impl Default for UnixSocketState {
    fn default() -> Self {
        Self {
            base: KObjectBase::new(),
            inner: Arc::new(Mutex::new(UnixInner {
                flags: OpenFlags::RDWR,
                path: String::new(),
                peer: None,
                buffer: VecDeque::new(),
                total_written: 0,
                total_read: 0,
                eventbus: EventBus::default(),
                is_listening: false,
                accept_queue: VecDeque::new(),
                connected: false,
                peer_closed: false,
                read_closed: false,
                write_closed: false,
                owner_pid: 0,
                pending_fds: VecDeque::new(),
            })),
        }
    }
}

impl UnixSocketState {
    /// Create a new Unix socket wrapped in Arc (needed everywhere).
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Record the PID of the process that created this socket, so the peer can
    /// read it via `SO_PEERCRED` (used by seatd to authorize a client).
    pub fn set_owner_pid(&self, pid: i32) {
        self.inner.lock().owner_pid = pid;
    }

    /// Wire two sockets together bidirectionally.
    /// Must be called while neither inner lock is held.
    pub fn connect_pair(a: &Arc<Self>, b: &Arc<Self>) {
        {
            let mut ai = a.inner.lock();
            ai.peer = Some(Arc::downgrade(&b.inner));
            ai.connected = true;
            ai.eventbus.set(Event::WRITABLE);
        }
        {
            let mut bi = b.inner.lock();
            bi.peer = Some(Arc::downgrade(&a.inner));
            bi.connected = true;
            bi.eventbus.set(Event::WRITABLE);
        }
    }

    /// Return true if this socket has been marked as listening.
    pub fn is_listening(&self) -> bool {
        self.inner.lock().is_listening
    }

    /// Mark this socket as connected (used by sys_connect for the client side).
    pub fn mark_connected(&self) {
        self.inner.lock().connected = true;
    }

    /// Register this socket under `path` so that connect() can find it.
    pub fn register(path: String, socket: Arc<Self>) -> LxResult<()> {
        let mut map = UNIX_SOCKETS.lock();
        purge_dead_registry(&mut map);
        if let Some(w) = map.get(&path) {
            if w.upgrade().is_some() {
                return Err(LxError::EADDRINUSE);
            }
        }
        if map.len() >= MAX_UNIX_SOCKET_REGISTRY {
            return Err(LxError::ENOMEM);
        }
        map.insert(path, Arc::downgrade(&socket));
        Ok(())
    }

    /// Look up a registered socket by path.
    pub fn lookup(path: &String) -> Option<Arc<Self>> {
        let mut map = UNIX_SOCKETS.lock();
        purge_dead_registry(&mut map);
        if let Some(w) = map.get(path) {
            if let Some(arc) = w.upgrade() {
                return Some(arc);
            }
            map.remove(path);
        }
        None
    }

    /// Remove a registration (called on drop / close).
    pub fn unregister(path: &str) {
        UNIX_SOCKETS.lock().remove(path);
    }

    /// Push an already-wired server-side endpoint into this server's accept
    /// queue, to be handed out by the next `accept()`.
    pub fn push_accept(self: &Arc<Self>, peer: Arc<UnixSocketState>) {
        let mut inner = self.inner.lock();
        inner.accept_queue.push_back(peer);
        inner.eventbus.set(Event::READABLE);
    }

    /// Set the local bound path (used to label the server side of a pair).
    pub fn set_path(&self, path: String) {
        self.inner.lock().path = path;
    }

    /// The local bound path.
    pub fn bound_path(&self) -> String {
        self.inner.lock().path.clone()
    }
}

impl Drop for UnixSocketState {
    fn drop(&mut self) {
        let path = self.inner.lock().path.clone();
        if !path.is_empty() {
            Self::unregister(path.as_str());
        }
    }
}

#[async_trait]
impl Socket for UnixSocketState {
    // -----------------------------------------------------------------------
    // read — dequeue bytes from our inbound buffer
    // -----------------------------------------------------------------------
    async fn read(&self, data: &mut [u8]) -> (LxResult<usize>, Endpoint) {
        loop {
            let mut inner = self.inner.lock();
            let path = inner.path.clone();

            if inner.read_closed {
                return (Ok(0), Endpoint::Unix(path));
            }

            if !inner.buffer.is_empty() {
                let len = core::cmp::min(data.len(), inner.buffer.len());
                for d in data[..len].iter_mut() {
                    *d = inner.buffer.pop_front().unwrap();
                }
                inner.total_read += len;
                if inner.buffer.is_empty() {
                    inner.eventbus.clear(Event::READABLE);
                }
                return (Ok(len), Endpoint::Unix(path));
            }

            // EOF: peer gone
            let peer_gone =
                inner.peer_closed || inner.peer.as_ref().map_or(true, |w| w.strong_count() == 0);
            if peer_gone && inner.connected {
                return (Ok(0), Endpoint::Unix(path));
            }

            if inner.flags.contains(OpenFlags::NON_BLOCK) {
                return (Err(LxError::EAGAIN), Endpoint::Unix(path));
            }

            drop(inner);
            // Throttle the blocking retry to ~200 Hz instead of busy-spinning
            // with yield_now, which pegged a core. This loop doesn't subscribe
            // to the eventbus, so the 5 ms timer bounds worst-case latency; a
            // proper eventbus park (like stdin) could drop it further later.
            kernel_hal::thread::sleep_until(kernel_hal::timer::deadline_after(
                core::time::Duration::from_millis(5),
            ))
            .await;
        }
    }

    // -----------------------------------------------------------------------
    // write — append bytes into the peer's inbound buffer
    // -----------------------------------------------------------------------
    fn write(&self, data: &[u8], _sendto_endpoint: Option<Endpoint>) -> SysResult {
        // Resolve the peer and release our own lock BEFORE taking the peer's, so
        // two connected ends writing concurrently can't deadlock: holding
        // self→peer here while the peer's `write` holds peer→self is a classic
        // AB-BA lock cycle. Because `lock::Mutex` is an IRQ-disabling spinlock,
        // that cycle hangs the whole machine — which is exactly what happened
        // when a Wayland client (alacritty) and the compositor (labwc) flooded
        // their socket bidirectionally. Mirrors `peer_pid`/`send_fds`.
        let peer = {
            let inner = self.inner.lock();
            if inner.write_closed {
                return Err(LxError::EPIPE);
            }
            match &inner.peer {
                None => return Err(LxError::ENOTCONN),
                Some(peer_weak) => peer_weak.upgrade().ok_or(LxError::EPIPE)?,
            }
        };
        let mut pi = peer.lock();
        if pi.read_closed {
            return Err(LxError::EPIPE);
        }
        pi.buffer.extend(data.iter().copied());
        pi.total_written += data.len();
        pi.eventbus.set(Event::READABLE);
        Ok(data.len())
    }

    // -----------------------------------------------------------------------
    // connect — look up the server, enqueue ourselves, wire both ends
    // -----------------------------------------------------------------------
    async fn connect(&self, endpoint: Endpoint) -> SysResult {
        if let Endpoint::Unix(path) = endpoint {
            // Resolve server
            let server = match Self::lookup(&path) {
                Some(s) => s,
                None => return Err(LxError::ECONNREFUSED),
            };

            // Check it's listening
            if !server.inner.lock().is_listening {
                return Err(LxError::ECONNREFUSED);
            }

            // We need Arc<Self> to wire both ends.
            // Since connect() only has &self, we look ourselves up via the
            // UNIX_SOCKETS registry (if we're bound) or build a temporary
            // Arc by reconstructing from our KObjectBase id — but the simplest
            // approach is to create a fresh connected socket on the client side
            // and wire it. sys_connect already has the Arc and will call
            // push_accept; here we just confirm the server is listening.
            // The actual wiring is done in sys_connect via connect_pair().
            Ok(0)
        } else {
            Err(LxError::EINVAL)
        }
    }

    // -----------------------------------------------------------------------
    // bind — record the local path
    // -----------------------------------------------------------------------
    fn bind(&self, endpoint: Endpoint) -> SysResult {
        if let Endpoint::Unix(path) = endpoint {
            self.inner.lock().path = path;
            Ok(0)
        } else {
            Err(LxError::EINVAL)
        }
    }

    // -----------------------------------------------------------------------
    // listen — mark socket as passive
    // -----------------------------------------------------------------------
    fn listen(&self) -> SysResult {
        self.inner.lock().is_listening = true;
        Ok(0)
    }

    // -----------------------------------------------------------------------
    // accept — dequeue a pending connection and return connected pair
    // -----------------------------------------------------------------------
    async fn accept(&self) -> LxResult<(Arc<dyn FileLike>, Endpoint)> {
        loop {
            let mut inner = self.inner.lock();
            if let Some(server_side) = inner.accept_queue.pop_front() {
                if inner.accept_queue.is_empty() {
                    inner.eventbus.clear(Event::READABLE);
                }
                drop(inner);

                // `server_side` was already wired to the connecting client in
                // `sys_connect`, so any bytes the client sent before we accepted
                // (e.g. the X11 connection setup) are already buffered.
                // Clone the peer weak ref without nesting locks to label the
                // returned endpoint with the client's path.
                let peer_weak = server_side.inner.lock().peer.clone();
                let peer_path = peer_weak
                    .and_then(|w| w.upgrade())
                    .map(|p| p.lock().path.clone())
                    .unwrap_or_default();
                return Ok((server_side, Endpoint::Unix(peer_path)));
            }

            if inner.flags.contains(OpenFlags::NON_BLOCK) {
                return Err(LxError::EAGAIN);
            }
            drop(inner);
            // Throttle the blocking retry to ~200 Hz instead of busy-spinning
            // with yield_now, which pegged a core. This loop doesn't subscribe
            // to the eventbus, so the 5 ms timer bounds worst-case latency; a
            // proper eventbus park (like stdin) could drop it further later.
            kernel_hal::thread::sleep_until(kernel_hal::timer::deadline_after(
                core::time::Duration::from_millis(5),
            ))
            .await;
        }
    }

    fn shutdown(&self, howto: usize) -> SysResult {
        // Take the peer ref under our lock but drop our lock before locking the
        // peer, to avoid the self→peer / peer→self AB-BA deadlock (see `write`).
        let peer = {
            let mut inner = self.inner.lock();
            if howto == 0 || howto == 2 {
                inner.read_closed = true;
                inner.eventbus.set(Event::READABLE); // wake blocked reader
            }
            if howto == 1 || howto == 2 {
                inner.write_closed = true;
                inner.peer.as_ref().and_then(|w| w.upgrade())
            } else {
                None
            }
        };
        if let Some(peer) = peer {
            let mut pi = peer.lock();
            pi.peer_closed = true;
            pi.eventbus.set(Event::READABLE); // wake blocked reader
        }
        Ok(0)
    }

    fn endpoint(&self) -> Option<Endpoint> {
        let path = self.inner.lock().path.clone();
        if !path.is_empty() {
            Some(Endpoint::Unix(path))
        } else {
            None
        }
    }

    fn remote_endpoint(&self) -> Option<Endpoint> {
        // Drop our lock before taking the peer's (see `write`): holding self→peer
        // here races AB-BA against a concurrent `write`/`shutdown` on the peer.
        let peer = {
            let inner = self.inner.lock();
            inner.peer.as_ref()?.upgrade()?
        };
        Some(Endpoint::Unix(peer.lock().path.clone()))
    }

    fn setsockopt(&self, _level: usize, _opt: usize, _data: &[u8]) -> SysResult {
        Ok(0)
    }

    fn peer_pid(&self) -> Option<i32> {
        // The connected peer's `owner_pid` — i.e. the process on the other end.
        // Release our own lock before taking the peer's to avoid holding both.
        let peer = {
            let inner = self.inner.lock();
            inner.peer.as_ref()?.upgrade()?
        };
        let pid = peer.lock().owner_pid;
        Some(pid)
    }

    fn send_fds(&self, fds: Vec<Arc<dyn FileLike>>) -> SysResult {
        if fds.is_empty() {
            return Ok(0);
        }
        // Append to the *peer's* queue, mirroring how `write` appends bytes to
        // the peer's buffer.
        let peer = {
            let inner = self.inner.lock();
            inner.peer.as_ref().and_then(|w| w.upgrade())
        };
        match peer {
            Some(peer) => {
                let mut pi = peer.lock();
                // Tag the batch with the current end-of-stream offset. `write`
                // (which appended this message's bytes) ran first in sendmsg, so
                // `total_written` is the offset just past those bytes; the peer
                // receives the fds only once its reads have consumed up to here.
                let offset = pi.total_written;
                pi.pending_fds.push_back((offset, fds));
                Ok(0)
            }
            None => Err(LxError::ENOTCONN),
        }
    }

    fn recv_fds(&self, max: usize) -> Vec<Arc<dyn FileLike>> {
        if max == 0 {
            return Vec::new();
        }
        let mut inner = self.inner.lock();
        let mut out: Vec<Arc<dyn FileLike>> = Vec::new();
        // Deliver only fd batches whose accompanying bytes have already been
        // read, and only whole batches that fit in the caller's fd budget.
        loop {
            let take = match inner.pending_fds.front() {
                Some((offset, batch)) => *offset <= inner.total_read && out.len() + batch.len() <= max,
                None => false,
            };
            if !take {
                break;
            }
            let (_, batch) = inner.pending_fds.pop_front().unwrap();
            out.extend(batch);
        }
        out
    }

    fn ioctl(&self, request: usize, arg1: usize, arg2: usize, arg3: usize) -> SysResult {
        crate::net::handle_net_ioctl(request, arg1, arg2, arg3, false)
    }

    fn poll(&self, _events: PollEvents) -> (bool, bool, bool) {
        let inner = self.inner.lock();
        let readable = !inner.buffer.is_empty()
            || (inner.is_listening && !inner.accept_queue.is_empty())
            || inner.peer_closed;
        let writable = inner.peer.as_ref().map_or(false, |w| w.strong_count() > 0);
        (readable, writable, false)
    }
}

impl_kobject!(UnixSocketState);

#[async_trait]
impl FileLike for UnixSocketState {
    fn flags(&self) -> OpenFlags {
        self.inner.lock().flags
    }

    fn set_flags(&self, f: OpenFlags) -> LxResult {
        let mut inner = self.inner.lock();
        inner
            .flags
            .set(OpenFlags::APPEND, f.contains(OpenFlags::APPEND));
        inner
            .flags
            .set(OpenFlags::NON_BLOCK, f.contains(OpenFlags::NON_BLOCK));
        inner
            .flags
            .set(OpenFlags::CLOEXEC, f.contains(OpenFlags::CLOEXEC));
        Ok(())
    }

    fn dup(&self) -> Arc<dyn FileLike> {
        Arc::new(Self {
            base: KObjectBase::new(),
            inner: self.inner.clone(),
        })
    }

    async fn read(&self, buf: &mut [u8]) -> LxResult<usize> {
        Socket::read(self, buf).await.0
    }

    async fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> LxResult<usize> {
        Err(LxError::ESPIPE)
    }

    fn write(&self, buf: &[u8]) -> LxResult<usize> {
        Socket::write(self, buf, None)
    }

    fn poll(&self, events: PollEvents) -> LxResult<PollStatus> {
        let (read, write, error) = Socket::poll(self, events);
        Ok(PollStatus { read, write, error })
    }

    async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus> {
        // For blocking sockets: wait until something is readable or writable.
        let non_block = self.inner.lock().flags.contains(OpenFlags::NON_BLOCK);
        if !non_block {
            loop {
                {
                    let inner = self.inner.lock();
                    let peer_gone = inner.peer_closed
                        || inner.peer.as_ref().map_or(true, |w| w.strong_count() == 0);
                    let readable = !inner.buffer.is_empty()
                        || (inner.is_listening && !inner.accept_queue.is_empty())
                        || peer_gone;
                    let want_read = events.contains(PollEvents::IN);
                    let want_write = events.contains(PollEvents::OUT);
                    let writable = !peer_gone;
                    if (want_read && readable) || (want_write && (writable || peer_gone)) {
                        break;
                    }
                }
                // Throttle the blocking retry to ~200 Hz instead of busy-spinning with
                // yield_now (which pegs a core). A peer write also sets the eventbus,
                // so latency stays low; the timer just bounds the idle spin.
                kernel_hal::thread::sleep_until(kernel_hal::timer::deadline_after(
                    core::time::Duration::from_millis(5),
                ))
                .await;
            }
        }
        let (read, write, error) = Socket::poll(self, events);
        Ok(PollStatus { read, write, error })
    }

    fn ioctl(&self, request: usize, arg1: usize, arg2: usize, arg3: usize) -> LxResult<usize> {
        Socket::ioctl(self, request, arg1, arg2, arg3)
    }

    fn as_socket(&self) -> LxResult<&dyn Socket> {
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;

    /// Reproduces the X11 connection-setup race: an X client writes its first
    /// bytes (the connection setup) immediately after `connect()`, before the
    /// server calls `accept()`. With connect-time wiring those bytes must be
    /// buffered for the server, not rejected with `ENOTCONN`.
    #[test]
    fn client_write_before_accept_is_buffered() {
        let server = UnixSocketState::new();
        server.inner.lock().is_listening = true;

        // Simulate what sys_connect now does: create the server-side endpoint,
        // wire it to the client, and queue it for accept().
        let client = UnixSocketState::new();
        let server_side = UnixSocketState::new();
        server_side.set_path(String::from("\0/tmp/.X11-unix/X0"));
        UnixSocketState::connect_pair(&client, &server_side);
        server.push_accept(server_side.clone());

        // Client sends the handshake before the server has accepted.
        let n = Socket::write(&*client, b"x11-setup", None).expect("write before accept");
        assert_eq!(n, 9);

        // The bytes are waiting on the server side; the connection is queued.
        assert_eq!(server_side.inner.lock().buffer.len(), 9);
        assert_eq!(server.inner.lock().accept_queue.len(), 1);
    }

    /// A socket with no peer must report `ENOTCONN` on write.
    #[test]
    fn write_without_peer_is_enotconn() {
        let lone = UnixSocketState::new();
        assert!(matches!(
            Socket::write(&*lone, b"x", None),
            Err(LxError::ENOTCONN)
        ));
    }

    /// bind registry: register/lookup/unregister and duplicate-bind refusal.
    #[test]
    fn register_lookup_roundtrip() {
        let path = String::from("\0/tmp/.X11-unix/Xtest-reg");
        let s = UnixSocketState::new();
        UnixSocketState::register(path.clone(), s.clone()).unwrap();
        assert!(UnixSocketState::lookup(&path).is_some());

        let s2 = UnixSocketState::new();
        assert!(matches!(
            UnixSocketState::register(path.clone(), s2),
            Err(LxError::EADDRINUSE)
        ));

        UnixSocketState::unregister(&path);
        assert!(UnixSocketState::lookup(&path).is_none());
    }

    /// End-to-end: after the server accepts, it reads what the client wrote
    /// before the accept, and its reply reaches the client (full duplex).
    #[async_std::test]
    async fn accept_then_full_duplex() {
        let server = UnixSocketState::new();
        server.inner.lock().is_listening = true;

        let client = UnixSocketState::new();
        let server_side = UnixSocketState::new();
        UnixSocketState::connect_pair(&client, &server_side);
        server.push_accept(server_side);
        Socket::write(&*client, b"ping", None).unwrap();

        let (accepted, _ep) = Socket::accept(&*server).await.unwrap();

        // Server reads the bytes the client sent before accept.
        let mut buf = [0u8; 16];
        let n = accepted.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"ping");

        // Reply path: server -> client.
        accepted.write(b"pong").unwrap();
        let mut cbuf = [0u8; 16];
        let (r, _) = Socket::read(&*client, &mut cbuf).await;
        assert_eq!(&cbuf[..r.unwrap()], b"pong");
    }
}
