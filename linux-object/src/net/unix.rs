use crate::fs::{FileLike, OpenFlags, PollEvents, PollStatus};
use crate::{
    error::{LxError, LxResult},
    net::{Endpoint, Socket, SocketType, SysResult},
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
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;
use hashbrown::HashMap;
use lazy_static::lazy_static;
use lock::Mutex;
use zircon_object::object::*;

lazy_static! {
    static ref UNIX_SOCKETS: Mutex<HashMap<String, Weak<UnixSocketState>>> =
        Mutex::new(HashMap::new());
}

const MAX_UNIX_SOCKET_REGISTRY: usize = 1024;

/// Snapshot the bound-socket registry for `/proc/net/unix`:
/// `(path, strong reference count, is_listening)` per live entry, sorted by
/// path so the listing is stable across reads.
pub(crate) fn registry_snapshot() -> alloc::vec::Vec<(String, usize, bool)> {
    let map = UNIX_SOCKETS.lock();
    let mut rows: alloc::vec::Vec<(String, usize, bool)> = map
        .iter()
        .filter_map(|(path, weak)| {
            weak.upgrade()
                .map(|sock| (path.clone(), weak.strong_count(), sock.is_listening()))
        })
        .collect();
    rows.sort();
    rows
}

fn purge_dead_registry(map: &mut HashMap<String, Weak<UnixSocketState>>) {
    map.retain(|_, weak| weak.strong_count() > 0);
}

/// Unix domain socket (AF_UNIX / AF_LOCAL) implementation.
///
/// Supports the full AF_UNIX workflow (as used by many DHCP clients and daemons):
/// - Server: socket → bind → listen → accept
/// - Client: socket → connect  (→ ENOENT if nothing is bound at the path,
///   ECONNREFUSED if a socket is bound there but not listening — Linux)
pub struct UnixSocketState {
    base: KObjectBase,
    inner: Arc<Mutex<UnixInner>>,
}

#[derive(Debug)]
struct UnixInner {
    flags: OpenFlags,
    /// The `type` argument this socket was created with. `sys_socket` routes
    /// EVERY AF_UNIX type here -- `(Domain::AF_UNIX, _, _)` -- but this
    /// implementation is byte-stream only: `write` appends into the peer's
    /// `buffer` and `read` drains it, with no message boundaries anywhere. So
    /// the requested type is recorded rather than implemented, purely so
    /// callers can tell a datagram/seqpacket socket apart from a stream one
    /// (`sendmsg` refuses to truncate an oversized message on the former).
    sock_type: SocketType,
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
                sock_type: SocketType::SOCK_STREAM,
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

    /// Record the `type` this socket was requested with (see `UnixInner::sock_type`).
    pub fn set_socket_type(&self, sock_type: SocketType) {
        self.inner.lock().sock_type = sock_type;
    }

    /// Wire two sockets together bidirectionally.
    /// Must be called while neither inner lock is held.
    pub fn connect_pair(a: &Arc<Self>, b: &Arc<Self>) {
        {
            let mut ai = a.inner.lock();
            ai.peer = Some(Arc::downgrade(&b.inner));
            ai.connected = true;
            // Do NOT latch WRITABLE on the eventbus. Stream sockets report
            // POLLOUT from peer-liveness in poll()/UnixPollWait synchronously;
            // a permanent WRITABLE flag made every fresh POLLIN async_poll
            // (poll/epoll re-scan) either busy-spin (callback matched OUT) or
            // retain orphan wakers (masked OUT + no Drop) — both seen at
            // labwc start. WRITABLE wakes belong to transitions, not connect.
        }
        {
            let mut bi = b.inner.lock();
            bi.peer = Some(Arc::downgrade(&a.inner));
            bi.connected = true;
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

    /// The listener a `connect()` to `path` must reach, with Linux's errno
    /// split. Every AF_UNIX `connect()` -- the `sys_connect` fast path and
    /// the trait method alike -- decides through here, so this is the one
    /// place the rule lives.
    ///
    /// * A **pathname** socket nobody bound is `ENOENT`: on Linux that means
    ///   "no socket file at this path", and on this kernel `bind()` creates
    ///   no inode, so a registry miss is exactly that case (a leftover file
    ///   from a dead process cannot exist here).
    /// * An **abstract** name (leading NUL) nobody bound is `ECONNREFUSED`:
    ///   abstract names have no file, and Linux reports "no listener" for
    ///   them -- which is what libxcb expects when it tries `\0/tmp/.X11-
    ///   unix/X0` before falling back to the pathname socket.
    /// * A socket bound there but not listening is `ECONNREFUSED`.
    ///
    /// A pathname miss used to be `ECONNREFUSED` too. PulseAudio's
    /// `pa_unix_socket_is_stale()` treats ECONNREFUSED from connect() as
    /// "a dead socket file is in the way" and `unlink()`s it -- which then
    /// failed with ENOENT because there never was a file -- so
    /// `module-native-protocol-unix` refused to load and the daemon ran
    /// with no socket at all: every libpulse client, Firefox's cubeb
    /// included, got ENOENT on /run/pulse/native. With ENOENT here
    /// `pa_unix_socket_remove_stale()` returns 0 and the module binds.
    pub fn resolve_listener(path: &String) -> LxResult<Arc<Self>> {
        let Some(server) = Self::lookup(path) else {
            return Err(if path.starts_with('\0') {
                LxError::ECONNREFUSED
            } else {
                LxError::ENOENT
            });
        };
        if !server.inner.lock().is_listening {
            return Err(LxError::ECONNREFUSED);
        }
        Ok(server)
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

    /// Remove a registration, but ONLY if its socket is already dead.
    ///
    /// This is identity-checked on purpose. `sys_connect` stamps the
    /// LISTENER's path onto every accepted server-side socket (so
    /// getsockname works), and `dup()` creates additional droppable handles
    /// carrying the same path. An unconditional remove-by-key meant the
    /// first accepted connection the server closed evicted the LISTENER's
    /// own registry entry: from that moment every new connect() failed
    /// ECONNREFUSED while the server was alive and serving — exactly the
    /// "some Wayland clients connect, later ones are refused" flakiness
    /// seen at desktop-session start. A dropping socket can never upgrade
    /// its own Weak (strong count already 0), so "entry is dead" is
    /// equivalent to "entry is me (or stale)": the live listener's entry
    /// survives any same-path sibling's death.
    pub fn unregister(path: &str) {
        let mut map = UNIX_SOCKETS.lock();
        if let Some(w) = map.get(path) {
            if w.upgrade().is_none() {
                map.remove(path);
            }
        }
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
        // EOF notification: when the last handle sharing this end drops (dup()s
        // and SCM_RIGHTS-passed fds share `inner`), mark the peer closed and
        // wake anything parked on its eventbus. Blocking readers and pollers
        // are event-driven now, so without this they would never notice a peer
        // that vanished without calling shutdown().
        if Arc::strong_count(&self.inner) == 1 {
            let peer = {
                let inner = self.inner.lock();
                inner.peer.as_ref().and_then(|w| w.upgrade())
            };
            if let Some(peer) = peer {
                let mut pi = peer.lock();
                pi.peer_closed = true;
                pi.eventbus.set(Event::READABLE | Event::CLOSED);
            }
        }
    }
}

/// Future that resolves once the socket's eventbus carries any bit of `mask`.
///
/// The bits are checked under the same `UnixInner` lock that every writer
/// holds while `set()`ing them, so a transition between the check and the
/// waker subscription cannot be lost. Used by the blocking `read`/`accept`
/// paths, which re-validate their own condition after each wake.
struct UnixEventWait {
    inner: Arc<Mutex<UnixInner>>,
    mask: Event,
    sub_id: Option<u64>,
    /// Backstop timer slot (`timer_waker`): armed once while Pending; re-polls
    /// only refresh the waker. Drop/`take()` cancel so a late tick cannot wake
    /// a finished task — the lunarbar null fn-ptr `#PF` (`rip=0x3` in
    /// `timer_tick`) after Wayland sessions.
    timer: Option<kernel_hal::timer_waker::TimerWakerSlot>,
}

impl Drop for UnixEventWait {
    fn drop(&mut self) {
        if let Some(id) = self.sub_id.take() {
            self.inner.lock().eventbus.unsubscribe(id);
        }
        kernel_hal::timer_waker::kill_timer_waker(&mut self.timer);
    }
}

impl Future for UnixEventWait {
    /// `Err` when the wait was interrupted -- a signal, a kill, the process
    /// exiting. The event bus reports only what the SOCKET does, so without
    /// this the wait could not be ended by anything that happens to the
    /// waiter, and a client parked on an idle Wayland or D-Bus connection
    /// survived every attempt to kill it.
    type Output = LxResult<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        {
            let mut inner = this.inner.lock();
            if !(inner.eventbus.events() & this.mask).is_empty() {
                if let Some(id) = this.sub_id.take() {
                    inner.eventbus.unsubscribe(id);
                }
                kernel_hal::timer_waker::kill_timer_waker(&mut this.timer);
                return Poll::Ready(Ok(()));
            }
            if this.sub_id.is_none() {
                let waker = cx.waker().clone();
                let mask = this.mask;
                this.sub_id = inner.eventbus.subscribe(Box::new(move |ev| {
                    if (ev & mask).is_empty() {
                        return false;
                    }
                    waker.wake_by_ref();
                    true
                }));
            }
        }
        // Nothing on the socket. Before parking again, ask whether this
        // thread is still supposed to be waiting at all: the bus only reports
        // what the socket does, so this is the only thing here that can answer
        // a signal or a kill. Checked AFTER the readiness test, deliberately:
        // Linux serves a read it can serve now and synthesises EINTR only when
        // it would otherwise block.
        if let Err(err) = crate::process::check_signals() {
            if let Some(id) = this.sub_id.take() {
                this.inner.lock().eventbus.unsubscribe(id);
            }
            kernel_hal::timer_waker::kill_timer_waker(&mut this.timer);
            return Poll::Ready(Err(err));
        }
        // Backstop: eventbus is the fast path, but a parked callback can be
        // evicted from a full table -- and nothing on the bus ever fires for a
        // signal, so this tick is also what bounds how late a `kill` can be.
        // Arm once; refresh waker on re-poll — never push a fresh `timer_set`
        // every Pending (that was the churn behind the desktop null fn-ptr
        // #PF).
        let deadline = kernel_hal::timer::deadline_after(Duration::from_millis(20));
        kernel_hal::timer_waker::ensure_timer_waker(&mut this.timer, deadline, cx);
        Poll::Pending
    }
}

/// Future behind [`FileLike::async_poll`]: resolves with the poll status once
/// any *requested* readiness (or an EOF/error condition) holds, parking a
/// waker on the eventbus meanwhile. This is what lets `poll`/`select`/`epoll`
/// wake on the very write that makes a Wayland socket readable, instead of
/// noticing it on the multiplex fallback tick.
struct UnixPollWait<'a> {
    sock: &'a UnixSocketState,
    events: PollEvents,
    /// Subscription id from [`EventBus::subscribe`], if a waker is parked.
    /// Must be unsubscribed on Ready/`Drop`: poll/epoll re-scan creates a
    /// fresh future every pass, and without cleanup a POLLIN waiter that
    /// correctly ignores latched WRITABLE would leave an orphan callback per
    /// scan — filling the bus and corrupting the heap (observed as #DF in
    /// `__from_user` when labwc started parking for real).
    sub_id: Option<u64>,
}

impl Drop for UnixPollWait<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.sub_id.take() {
            self.sock.inner.lock().eventbus.unsubscribe(id);
        }
    }
}

impl Future for UnixPollWait<'_> {
    type Output = LxResult<PollStatus>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let ready = {
            let mut inner = this.sock.inner.lock();
            let peer_gone =
                inner.peer_closed || inner.peer.as_ref().is_none_or(|w| w.strong_count() == 0);
            let readable = !inner.buffer.is_empty()
                || (inner.is_listening && !inner.accept_queue.is_empty())
                || inner.read_closed
                || peer_gone;
            let writable = !peer_gone;
            let want_read = this.events.contains(PollEvents::IN);
            let want_write = this.events.contains(PollEvents::OUT);
            let ready = (want_read && readable)
                || (want_write && (writable || peer_gone))
                || (!want_read && !want_write);
            if ready {
                if let Some(id) = this.sub_id.take() {
                    inner.eventbus.unsubscribe(id);
                }
            } else if this.sub_id.is_none() {
                // Wake only for events we actually asked for. connect_pair
                // latches WRITABLE and EventBus::subscribe fires immediately on
                // already-set flags: if the callback also matched WRITABLE while
                // the waiter only wanted POLLIN, every fresh async_poll (poll/
                // epoll re-scan creates a new future) woke the task with no
                // data → busy-spin shared across labwc/seatd/lunarbg/lunarbar.
                // Same mask contract as EventBusFuture (used by read()).
                let mut wake_mask = Event::CLOSED | Event::ERROR;
                if want_read {
                    wake_mask |= Event::READABLE;
                }
                if want_write {
                    wake_mask |= Event::WRITABLE;
                }
                let waker = cx.waker().clone();
                this.sub_id = inner.eventbus.subscribe(Box::new(move |ev| {
                    if (ev & wake_mask).is_empty() {
                        return false;
                    }
                    waker.wake_by_ref();
                    true
                }));
            }
            ready
        };
        if !ready {
            return Poll::Pending;
        }
        let (read, write, error) = Socket::poll(this.sock, this.events);
        Poll::Ready(Ok(PollStatus {
            read,
            write,
            error,
            hangup: false,
        }))
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

            // `unix_stream_read_generic`: `if (sk->sk_state != TCP_ESTABLISHED)
            // return -EINVAL;` -- a socket that was never connected (or is a
            // listener) has no stream to read from. Parking here instead, as
            // this used to, was a hang: nothing can ever wake a reader on a
            // socket with no peer to write to it. `connected` is set by every
            // wiring path (`connect_pair`, `mark_connected`) and stays set
            // after the peer goes away, so this only names the never-wired.
            if !inner.connected {
                return (Err(LxError::EINVAL), Endpoint::Unix(path));
            }

            // Queued bytes come out before a receive-side shutdown is honoured:
            // Linux only reports `RCV_SHUTDOWN` as EOF once the receive queue
            // is empty (`unix_stream_read_generic` peeks the queue first). The
            // old order answered `shutdown(SHUT_RD)` with 0 straight away and
            // silently dropped whatever the peer had already sent.
            if !inner.buffer.is_empty() {
                let len = core::cmp::min(data.len(), inner.buffer.len());
                // Bulk copy from the deque's two contiguous halves instead of a
                // per-byte pop_front loop: every Wayland/D-Bus message crosses
                // here, and the byte loop made each one O(n) branchy work under
                // an IRQ-disabling lock.
                let (front, back) = inner.buffer.as_slices();
                if len <= front.len() {
                    data[..len].copy_from_slice(&front[..len]);
                } else {
                    data[..front.len()].copy_from_slice(front);
                    data[front.len()..len].copy_from_slice(&back[..len - front.len()]);
                }
                inner.buffer.drain(..len);
                inner.total_read += len;
                if inner.buffer.is_empty() {
                    inner.eventbus.clear(Event::READABLE);
                }
                return (Ok(len), Endpoint::Unix(path));
            }

            if inner.read_closed {
                return (Ok(0), Endpoint::Unix(path));
            }

            // EOF: peer gone
            let peer_gone =
                inner.peer_closed || inner.peer.as_ref().is_none_or(|w| w.strong_count() == 0);
            if peer_gone && inner.connected {
                return (Ok(0), Endpoint::Unix(path));
            }

            if inner.flags.contains(OpenFlags::NON_BLOCK) {
                return (Err(LxError::EAGAIN), Endpoint::Unix(path));
            }

            drop(inner);
            // Park on the eventbus until a peer write / shutdown / close flips
            // READABLE (or CLOSED); the loop re-validates the condition after
            // each wake. Event-driven, so a blocked reader resumes on the very
            // write instead of on a retry timer.
            if let Err(err) = (UnixEventWait {
                inner: self.inner.clone(),
                mask: Event::READABLE | Event::CLOSED,
                sub_id: None,
                timer: None,
            })
            .await
            {
                return (Err(err), Endpoint::Unix(self.inner.lock().path.clone()));
            }
        }
    }

    // -----------------------------------------------------------------------
    // write — append bytes into the peer's inbound buffer
    // -----------------------------------------------------------------------
    /// The requested type, so callers can distinguish a message-oriented
    /// AF_UNIX socket from a stream one. Reported even though the transport is
    /// a byte stream either way: a caller that must not silently split a
    /// message needs to know what the user asked for, and the alternative
    /// (`None`) is indistinguishable from "no idea".
    fn socket_type(&self) -> Option<SocketType> {
        Some(self.inner.lock().sock_type)
    }

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
        // Bound the peer's inbound buffer so a peer that never reads cannot grow
        // it without limit and exhaust the fixed kernel heap (a local DoS). Write
        // only what fits and return that count (correct stream semantics — the
        // syscall layer already loops on short writes); return EAGAIN when the
        // buffer is completely full. This method is synchronous and never blocked
        // before, so no blocking behavior is lost.
        const UNIX_STREAM_BUF_MAX: usize = 4 * 1024 * 1024;
        let space = UNIX_STREAM_BUF_MAX.saturating_sub(pi.buffer.len());
        if space == 0 {
            return Err(LxError::EAGAIN);
        }
        let n = data.len().min(space);
        // `extend(&slice)` takes VecDeque's Copy-slice specialization (a pair
        // of memcpys) instead of the element-wise TrustedLen loop.
        pi.buffer.extend(&data[..n]);
        pi.total_written += n;
        pi.eventbus.set(Event::READABLE);
        Ok(n)
    }

    // -----------------------------------------------------------------------
    // connect — look up the server, enqueue ourselves, wire both ends
    // -----------------------------------------------------------------------
    async fn connect(&self, endpoint: Endpoint) -> SysResult {
        if let Endpoint::Unix(path) = endpoint {
            // Resolve the listener (ENOENT / ECONNREFUSED as Linux does). The
            // wiring below looks the server up again by path, so only the
            // errno decision is needed here.
            Self::resolve_listener(&path)?;

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
            // Park on the eventbus: `push_accept` sets READABLE when a client
            // connects, so a blocked accept resumes immediately.
            UnixEventWait {
                inner: self.inner.clone(),
                mask: Event::READABLE | Event::CLOSED,
                sub_id: None,
                timer: None,
            }
            .await?;
        }
    }

    fn shutdown(&self, howto: usize) -> SysResult {
        // `unix_shutdown`: `if (mode < SHUT_RD || mode > SHUT_RDWR) return
        // -EINVAL;`. Anything else used to fall through both `if`s below and
        // return Ok having shut nothing down -- the caller believed the half
        // it named was closed.
        if howto > 2 {
            return Err(LxError::EINVAL);
        }
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
        let path = peer.lock().path.clone();
        Some(Endpoint::Unix(path))
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
                // Tag the batch with the byte offset of the FIRST byte of the
                // accompanying message. `sendmsg` now queues the fds BEFORE it
                // appends this message's bytes, so `total_written` here is the
                // index of that first byte. The peer receives the fds on the
                // recvmsg that reads past this offset (see `recv_fds`), matching
                // Linux, where a passed fd arrives with the first data byte.
                let offset = pi.total_written;
                pi.pending_fds.push_back((offset, fds));
                Ok(0)
            }
            None => Err(LxError::ENOTCONN),
        }
    }

    fn recv_fds(&self, max: usize) -> Vec<Arc<dyn FileLike>> {
        let mut inner = self.inner.lock();
        let mut out: Vec<Arc<dyn FileLike>> = Vec::new();
        // Deliver an fd batch once the reader has consumed at least the first
        // byte of the message it was attached to (`offset` is that first byte's
        // index, so the gate is strict `<`). This hands the fd to the same
        // recvmsg that returns the message's leading bytes, as Linux does.
        //
        // A batch larger than the caller's fd budget used to be refused whole,
        // and it sits at the HEAD of the queue: the fds never came out, and
        // neither did any batch behind them — fd passing on that socket was
        // over for good. A caller sizes its control buffer for the fds it
        // expects, so one message carrying more than that wedged the
        // connection. Now the budget takes what fits and the rest stays at the
        // head, keeping its offset, for the next call.
        //
        // (Linux instead drops the leftovers and sets `MSG_CTRUNC`. Holding
        // them is the more conservative divergence: nothing is lost, and a
        // caller that asks again gets the rest.)
        loop {
            let room = max - out.len();
            if room == 0 {
                break;
            }
            let ready = match inner.pending_fds.front() {
                Some((offset, _)) => *offset < inner.total_read,
                None => false,
            };
            if !ready {
                break;
            }
            let (offset, mut batch) = inner.pending_fds.pop_front().unwrap();
            if batch.len() <= room {
                out.extend(batch);
            } else {
                let rest = batch.split_off(room);
                out.extend(batch);
                inner.pending_fds.push_front((offset, rest));
                break;
            }
        }
        out
    }

    fn ioctl(&self, request: usize, arg1: usize, arg2: usize, arg3: usize) -> SysResult {
        crate::net::handle_net_ioctl(request, arg1, arg2, arg3, false)
    }

    fn poll(&self, _events: PollEvents) -> (bool, bool, bool) {
        let inner = self.inner.lock();
        // `read_closed` counts as readable: a read would return immediately
        // (EOF), which is exactly what POLLIN promises.
        let readable = !inner.buffer.is_empty()
            || (inner.is_listening && !inner.accept_queue.is_empty())
            || inner.peer_closed
            || inner.read_closed;
        let writable = inner.peer.as_ref().is_some_and(|w| w.strong_count() > 0);
        (readable, writable, false)
    }
}

impl_kobject!(UnixSocketState);

#[async_trait]
impl FileLike for UnixSocketState {
    /// A socket reports `S_IFSOCK` to `fstat(2)`, as on Linux.
    fn metadata(&self) -> LxResult<rcore_fs::vfs::Metadata> {
        Ok(crate::fs::anon_metadata(
            rcore_fs::vfs::FileType::Socket,
            self.id() as usize,
        ))
    }

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
        Ok(PollStatus {
            read,
            write,
            error,
            hangup: false,
        })
    }

    fn subscribe_readiness(
        &self,
        events: PollEvents,
        waker: &core::task::Waker,
    ) -> Option<crate::sync::ReadinessSub> {
        let mask = crate::fs::poll_events_to_bus_mask(events);
        let id = {
            let mut inner = self.inner.lock();
            crate::sync::subscribe_waker(&mut inner.eventbus, mask, waker)
        };
        Some(match id {
            Some(id) => {
                let inner = self.inner.clone();
                crate::sync::ReadinessSub::new(Box::new(move || {
                    inner.lock().eventbus.unsubscribe(id);
                }))
            }
            None => crate::sync::ReadinessSub::noop(),
        })
    }

    async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus> {
        // Event-driven readiness: stay Pending with a waker parked on the
        // socket's eventbus until a requested event (or EOF/close) holds, then
        // report the status. `sys_poll`/`select`/`epoll` poll this future with
        // their own Context, so a peer write wakes them immediately — the same
        // contract the pipe implementation follows.
        UnixPollWait {
            sock: self,
            events,
            sub_id: None,
        }
        .await
    }

    fn ioctl(&self, request: usize, arg1: usize, arg2: usize, arg3: usize) -> LxResult<usize> {
        Socket::ioctl(self, request, arg1, arg2, arg3)
    }

    /// What `FIONREAD` reports: bytes sitting in this end's receive queue.
    fn readable_bytes(&self) -> Option<usize> {
        Some(self.inner.lock().buffer.len())
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

    /// connect() errno split, as Linux: a PATHNAME nobody bound is ENOENT
    /// (no socket file exists); an ABSTRACT name nobody bound is
    /// ECONNREFUSED (no file to speak of, "no listener"); a bound-but-not-
    /// listening socket is ECONNREFUSED; a listener resolves. The pathname
    /// ENOENT is what PulseAudio's stale-socket check needs -- ECONNREFUSED
    /// made it unlink a phantom file and refuse to create /run/pulse/native.
    #[test]
    fn connect_resolution_is_enoent_then_econnrefused_then_ok() {
        let path = String::from("/run/pulse/native-test-resolve");
        assert!(matches!(
            UnixSocketState::resolve_listener(&path),
            Err(LxError::ENOENT)
        ));
        let abstract_name = String::from("\0/tmp/.X11-unix/Xtest-resolve");
        assert!(matches!(
            UnixSocketState::resolve_listener(&abstract_name),
            Err(LxError::ECONNREFUSED)
        ));
        let s = UnixSocketState::new();
        UnixSocketState::register(path.clone(), s.clone()).unwrap();
        assert!(matches!(
            UnixSocketState::resolve_listener(&path),
            Err(LxError::ECONNREFUSED)
        ));
        s.inner.lock().is_listening = true;
        assert!(UnixSocketState::resolve_listener(&path).is_ok());
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

    /// bind registry: register/lookup and duplicate-bind refusal; unregister
    /// is identity-checked, so a dying same-path sibling (an accepted socket
    /// carries the listener's path, dup() handles do too) can NEVER evict a
    /// live listener — that eviction was exactly the bug that made Wayland
    /// clients get ECONNREFUSED as soon as the compositor closed its first
    /// accepted connection.
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

        // A same-path sibling dying (its Drop calls unregister) must not
        // remove the live listener's entry.
        let sibling = UnixSocketState::new();
        sibling.set_path(path.clone());
        drop(sibling);
        assert!(UnixSocketState::lookup(&path).is_some());

        // Explicit unregister while the listener is alive is a no-op too.
        UnixSocketState::unregister(&path);
        assert!(UnixSocketState::lookup(&path).is_some());

        // Once the listener itself dies, the entry goes with it.
        drop(s);
        assert!(UnixSocketState::lookup(&path).is_none());
    }

    /// A reader blocked on an empty socket is woken by the peer's write (no
    /// retry timer involved — the eventbus subscription must fire).
    #[async_std::test]
    async fn blocked_read_wakes_on_peer_write() {
        let a = UnixSocketState::new();
        let b = UnixSocketState::new();
        UnixSocketState::connect_pair(&a, &b);

        let reader = {
            let b = b.clone();
            async_std::task::spawn(async move {
                let mut buf = [0u8; 8];
                let (r, _) = Socket::read(&*b, &mut buf).await;
                (r.unwrap(), buf)
            })
        };
        // Give the reader a chance to park on the eventbus first.
        async_std::task::sleep(core::time::Duration::from_millis(20)).await;
        Socket::write(&*a, b"hola", None).unwrap();
        let (n, buf) = reader.await;
        assert_eq!(&buf[..n], b"hola");
    }

    /// Dropping the last handle of one end must wake and EOF a blocked reader
    /// on the other end, even without an explicit shutdown().
    #[async_std::test]
    async fn blocked_read_eofs_when_peer_drops() {
        let a = UnixSocketState::new();
        let b = UnixSocketState::new();
        UnixSocketState::connect_pair(&a, &b);

        let reader = {
            let b = b.clone();
            async_std::task::spawn(async move {
                let mut buf = [0u8; 8];
                let (r, _) = Socket::read(&*b, &mut buf).await;
                r.unwrap()
            })
        };
        async_std::task::sleep(core::time::Duration::from_millis(20)).await;
        drop(a);
        assert_eq!(reader.await, 0, "peer drop must read as EOF");
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

    /// connect_pair must NOT latch WRITABLE. Even if something else sets it,
    /// async_poll(POLLIN) on an empty connected socket must stay Pending
    /// without a synchronous wake — otherwise every poll/epoll re-scan busy-
    /// spins (or leaked wakers before Drop unsubscribed).
    #[test]
    fn poll_in_empty_connected_does_not_spuriously_wake() {
        use core::sync::atomic::{AtomicBool, Ordering};
        use core::task::{RawWaker, RawWakerVTable, Waker};

        static WOKE: AtomicBool = AtomicBool::new(false);

        fn raw(ptr: *const ()) -> RawWaker {
            fn clone(ptr: *const ()) -> RawWaker {
                raw(ptr)
            }
            fn wake(_: *const ()) {
                WOKE.store(true, Ordering::SeqCst);
            }
            fn wake_by_ref(_: *const ()) {
                WOKE.store(true, Ordering::SeqCst);
            }
            fn drop(_: *const ()) {}
            RawWaker::new(ptr, &RawWakerVTable::new(clone, wake, wake_by_ref, drop))
        }

        WOKE.store(false, Ordering::SeqCst);
        let a = UnixSocketState::new();
        let b = UnixSocketState::new();
        UnixSocketState::connect_pair(&a, &b);
        assert!(
            a.inner.lock().buffer.is_empty(),
            "precondition: empty receive buffer"
        );
        // Simulate the old connect_pair latch — POLLIN must still ignore it.
        a.inner.lock().eventbus.set(Event::WRITABLE);

        let waker = unsafe { Waker::from_raw(raw(core::ptr::null())) };
        let mut cx = Context::from_waker(&waker);
        let mut fut = UnixPollWait {
            sock: &a,
            events: PollEvents::IN,
            sub_id: None,
        };
        match Pin::new(&mut fut).poll(&mut cx) {
            Poll::Pending => {}
            Poll::Ready(Ok(_status)) => {
                panic!("POLLIN on empty connected socket must be Pending, got Ready")
            }
            Poll::Ready(Err(e)) => panic!("unexpected err {:?}", e),
        }
        assert!(
            !WOKE.load(Ordering::SeqCst),
            "latched WRITABLE must not wake a POLLIN-only waiter"
        );
        assert!(
            fut.sub_id.is_some(),
            "POLLIN waiter must retain a bus subscription while Pending"
        );
        let n_after_park = a.inner.lock().eventbus.get_callback_len();
        assert!(n_after_park >= 1, "subscription must be on the bus");

        // Dropping the future (as poll/epoll do every re-scan) must unsubscribe
        // — otherwise each scan leaks a callback and fills the bus.
        drop(fut);
        assert_eq!(
            a.inner.lock().eventbus.get_callback_len(),
            n_after_park - 1,
            "Drop must unsubscribe the parked POLLIN waker"
        );

        // POLLOUT on a connected socket is ready immediately (writable).
        let mut fut_out = UnixPollWait {
            sock: &a,
            events: PollEvents::OUT,
            sub_id: None,
        };
        match Pin::new(&mut fut_out).poll(&mut cx) {
            Poll::Ready(Ok(status)) => {
                assert!(status.write, "connected peer ⇒ writable");
            }
            other => panic!("POLLOUT expected Ready(write), got {:?}", other),
        }
    }

    /// `unix_stream_read_generic`: `if (sk->sk_state != TCP_ESTABLISHED)
    /// return -EINVAL;`. A read on a socket that was never connected used to
    /// park on the eventbus forever (nothing can ever write to it), or answer
    /// EAGAIN on a non-blocking one -- "try again" for a condition that no
    /// retry can change. Blocking or not, the answer is EINVAL.
    #[async_std::test]
    async fn read_on_a_never_connected_socket_is_einval() {
        let lone = UnixSocketState::new();
        let mut buf = [0u8; 8];
        let (r, _) = Socket::read(&*lone, &mut buf).await;
        assert!(matches!(r, Err(LxError::EINVAL)), "blocking: {:?}", r);

        lone.inner.lock().flags |= OpenFlags::NON_BLOCK;
        let (r, _) = Socket::read(&*lone, &mut buf).await;
        assert!(matches!(r, Err(LxError::EINVAL)), "non-blocking: {:?}", r);

        // A listener is not a stream either.
        let listener = UnixSocketState::new();
        listener.inner.lock().is_listening = true;
        let (r, _) = Socket::read(&*listener, &mut buf).await;
        assert!(matches!(r, Err(LxError::EINVAL)), "listener: {:?}", r);
    }

    /// Linux reports `RCV_SHUTDOWN` as EOF only once the receive queue is
    /// empty: bytes the peer sent before `shutdown(SHUT_RD)` are still
    /// delivered. Answering 0 straight away dropped them on the floor.
    #[async_std::test]
    async fn shutdown_rd_delivers_what_was_already_queued_before_eof() {
        let a = UnixSocketState::new();
        let b = UnixSocketState::new();
        UnixSocketState::connect_pair(&a, &b);
        Socket::write(&*a, b"queued", None).unwrap();

        assert!(Socket::shutdown(&*b, 0).is_ok(), "SHUT_RD");
        let mut buf = [0u8; 16];
        let (r, _) = Socket::read(&*b, &mut buf).await;
        assert_eq!(r.unwrap(), 6, "the queued bytes come out first");
        assert_eq!(&buf[..6], b"queued");
        let (r, _) = Socket::read(&*b, &mut buf).await;
        assert_eq!(r.unwrap(), 0, "then EOF, without blocking");
    }

    /// `unix_shutdown`: `if (mode < SHUT_RD || mode > SHUT_RDWR) return
    /// -EINVAL;`. Any other value used to match neither half and return Ok
    /// with both halves still open.
    #[test]
    fn shutdown_refuses_a_how_that_is_not_a_how() {
        let a = UnixSocketState::new();
        let b = UnixSocketState::new();
        UnixSocketState::connect_pair(&a, &b);

        for bogus in [3usize, 0x2a, usize::MAX] {
            assert!(
                matches!(Socket::shutdown(&*a, bogus), Err(LxError::EINVAL)),
                "how={:#x}",
                bogus
            );
        }
        let ai = a.inner.lock();
        assert!(!ai.read_closed && !ai.write_closed, "nothing was shut down");
        drop(ai);
        // The three real values still act: SHUT_WR on one side, and
        // SHUT_RDWR (2, the upper bound of the check) on the other closes
        // both halves at once.
        assert!(Socket::shutdown(&*a, 1).is_ok());
        assert!(a.inner.lock().write_closed);
        assert!(matches!(
            Socket::write(&*a, b"x", None),
            Err(LxError::EPIPE)
        ));
        assert!(Socket::shutdown(&*b, 2).is_ok(), "SHUT_RDWR is a how");
        let bi = b.inner.lock();
        assert!(bi.read_closed && bi.write_closed, "SHUT_RDWR closes both");
    }
}

#[cfg(test)]
mod fd_passing_tests {
    use super::*;

    /// Two connected endpoints: bytes written to `.0` arrive at `.1`.
    fn pair() -> (Arc<UnixSocketState>, Arc<UnixSocketState>) {
        let a = UnixSocketState::new();
        let b = UnixSocketState::new();
        UnixSocketState::connect_pair(&a, &b);
        (a, b)
    }

    /// Some file to pass. A socket is one, and it needs no filesystem.
    fn a_file() -> Arc<dyn FileLike> {
        UnixSocketState::new()
    }

    /// Whether two handles are the same object — the only thing that matters
    /// about a passed descriptor.
    fn same(x: &Arc<dyn FileLike>, y: &Arc<dyn FileLike>) -> bool {
        core::ptr::eq(Arc::as_ptr(x) as *const u8, Arc::as_ptr(y) as *const u8)
    }

    /// Attach `fds` to the next message and send `bytes` with it.
    fn send_with(from: &Arc<UnixSocketState>, fds: Vec<Arc<dyn FileLike>>, bytes: &[u8]) {
        Socket::send_fds(&**from, fds).expect("send_fds");
        Socket::write(&**from, bytes, None).expect("write");
    }

    /// Read `n` bytes on `at`.
    async fn recv_bytes(at: &Arc<UnixSocketState>, n: usize) -> usize {
        let mut buf = vec![0u8; n];
        Socket::read(&**at, &mut buf).await.0.expect("read")
    }

    #[async_std::test]
    async fn a_descriptor_arrives_with_the_message_it_was_sent_with() {
        let (a, b) = pair();
        let f = a_file();
        send_with(&a, vec![f.clone()], b"hola");
        // Before the receiver has read a byte of that message, the descriptor
        // is not hers yet: Linux delivers it with the recvmsg that returns the
        // message's first data byte.
        assert!(Socket::recv_fds(&*b, 8).is_empty());
        assert_eq!(recv_bytes(&b, 4).await, 4);
        let got = Socket::recv_fds(&*b, 8);
        assert_eq!(got.len(), 1);
        assert!(same(&got[0], &f));
    }

    #[async_std::test]
    async fn a_batch_bigger_than_the_receivers_buffer_no_longer_wedges_the_socket() {
        // This is the bug. With a control buffer sized for one descriptor, the
        // three-descriptor batch used to come back empty every single time,
        // for ever, and block everything queued behind it.
        let (a, b) = pair();
        let f: Vec<Arc<dyn FileLike>> = (0..3).map(|_| a_file()).collect();
        send_with(&a, f.clone(), b"hola");
        assert_eq!(recv_bytes(&b, 4).await, 4);

        let mut got: Vec<Arc<dyn FileLike>> = Vec::new();
        for _ in 0..3 {
            let round = Socket::recv_fds(&*b, 1);
            assert_eq!(round.len(), 1, "one descriptor per call, and never zero");
            got.extend(round);
        }
        assert!(Socket::recv_fds(&*b, 1).is_empty());
        for (want, have) in f.iter().zip(got.iter()) {
            assert!(same(want, have), "and in the order they were sent");
        }
    }

    #[async_std::test]
    async fn what_does_not_fit_stays_at_the_head_and_does_not_block_the_rest() {
        let (a, b) = pair();
        let first: Vec<Arc<dyn FileLike>> = (0..3).map(|_| a_file()).collect();
        let second = a_file();
        send_with(&a, first.clone(), b"uno");
        send_with(&a, vec![second.clone()], b"dos");
        assert_eq!(recv_bytes(&b, 6).await, 6);

        // Room for two: two of the first batch come out, the third stays put,
        // and the later batch stays behind it rather than jumping the queue.
        let got = Socket::recv_fds(&*b, 2);
        assert_eq!(got.len(), 2);
        assert!(same(&got[0], &first[0]) && same(&got[1], &first[1]));

        let rest = Socket::recv_fds(&*b, 8);
        assert_eq!(rest.len(), 2);
        assert!(same(&rest[0], &first[2]));
        assert!(same(&rest[1], &second));
    }

    #[async_std::test]
    async fn a_receiver_with_no_room_takes_nothing_and_loses_nothing() {
        // A recvmsg with no control buffer at all asks for zero descriptors.
        // They must still be there for the call that does ask.
        let (a, b) = pair();
        let f = a_file();
        send_with(&a, vec![f.clone()], b"hola");
        assert_eq!(recv_bytes(&b, 4).await, 4);
        assert!(Socket::recv_fds(&*b, 0).is_empty());
        let got = Socket::recv_fds(&*b, 4);
        assert_eq!(got.len(), 1);
        assert!(same(&got[0], &f));
    }

    #[async_std::test]
    async fn each_message_hands_over_its_own_descriptors_and_not_the_next_ones() {
        // The reason the fds are tagged with a byte offset at all: a peer that
        // reads a header first and the body second must get the first
        // message's descriptor with the first message.
        let (a, b) = pair();
        let one = a_file();
        let two = a_file();
        send_with(&a, vec![one.clone()], b"aaaa");
        send_with(&a, vec![two.clone()], b"bbbb");

        assert_eq!(recv_bytes(&b, 4).await, 4);
        let got = Socket::recv_fds(&*b, 8);
        assert_eq!(got.len(), 1);
        assert!(same(&got[0], &one));

        assert_eq!(recv_bytes(&b, 4).await, 4);
        let got = Socket::recv_fds(&*b, 8);
        assert_eq!(got.len(), 1);
        assert!(same(&got[0], &two));
    }

    #[async_std::test]
    async fn a_descriptor_sent_with_a_message_nobody_has_read_yet_waits() {
        let (a, b) = pair();
        send_with(&a, vec![a_file()], b"aaaa");
        send_with(&a, vec![a_file()], b"bbbb");
        // Only the first message has been read, so only its descriptor is due.
        assert_eq!(recv_bytes(&b, 4).await, 4);
        assert_eq!(Socket::recv_fds(&*b, 8).len(), 1);
        assert!(Socket::recv_fds(&*b, 8).is_empty());
    }

    #[test]
    fn passing_a_descriptor_with_no_peer_is_enotconn() {
        // Not a panic and not a silent success: the caller has to learn that
        // the file did not go anywhere.
        let lone = UnixSocketState::new();
        assert!(matches!(
            Socket::send_fds(&*lone, vec![a_file()]),
            Err(LxError::ENOTCONN)
        ));
    }

    #[test]
    fn passing_no_descriptors_is_a_no_op_even_without_a_peer() {
        // `sendmsg` with a control buffer that holds no SCM_RIGHTS must not
        // start failing because of it.
        let lone = UnixSocketState::new();
        assert!(Socket::send_fds(&*lone, Vec::new()).is_ok());
    }

    #[test]
    fn a_socket_with_nothing_queued_hands_back_nothing() {
        let (_a, b) = pair();
        assert!(Socket::recv_fds(&*b, 8).is_empty());
    }
}
