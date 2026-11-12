//! Stream sockets whose stack is a process. What the kernel and the stack
//! say to each other is in the module above.
//!
//! The connection is the stack's. What is here is what a program can ask
//! without waiting for it: the bytes that arrived and were not read yet,
//! how many the stack still takes, the connections nobody accepted yet,
//! and the error a failed connection left behind.

use super::*;

/// The most credit a stack is believed to give: a stack that says more
/// would have the kernel queue messages without end.
const CREDIT_MAX: usize = 16 * 1024 * 1024;

/// The most connections a listener holds for a program that does not
/// accept them: past this, the stack is told to close what it makes.
const WAITING_MAX: usize = 256;

/// Where a stream socket is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// Not connected and not listening: new, or its connect failed.
    Fresh,
    /// `CONNECT` went and nothing came back yet.
    Connecting,
    /// Connected.
    Open,
    /// Taking connections.
    Listening,
    /// It was connected, and the connection is gone.
    Over,
}

/// What a stream socket shares with the stack's link.
pub(super) struct Stream {
    /// `READABLE`, `WRITABLE` and `ERROR` as [`readable`], [`writable`]
    /// and [`failed`] say. Changed under `inner`.
    bus: Arc<Mutex<EventBus>>,
    inner: Mutex<Inner>,
}

struct Inner {
    phase: Phase,
    /// What arrived, in the pieces it arrived in.
    chunks: VecDeque<Vec<u8>>,
    /// How much of the first piece was read already.
    read_at: usize,
    /// The bytes in `chunks` not read yet.
    queued: usize,
    /// The bytes read since the stack was last told (`WINDOW`).
    unsaid: usize,
    /// The bytes the stack still takes.
    credit: usize,
    /// The peer will send no more.
    eof: bool,
    /// What `SO_ERROR` answers, and the next read or write.
    error: Option<LxError>,
    port: Option<u16>,
    /// Whether `port` is this socket's in the table: a connection that
    /// arrived on a listener has the listener's.
    holds_port: bool,
    remote: Option<IpEndpoint>,
    flags: OpenFlags,
    read_closed: bool,
    write_closed: bool,
    /// The connections that arrived and nobody accepted yet.
    waiting: VecDeque<(u64, Arc<Stream>)>,
}

impl Inner {
    fn new(phase: Phase) -> Inner {
        Inner {
            phase,
            chunks: VecDeque::new(),
            read_at: 0,
            queued: 0,
            unsaid: 0,
            credit: 0,
            eof: false,
            error: None,
            port: None,
            holds_port: false,
            remote: None,
            flags: OpenFlags::RDWR,
            read_closed: false,
            write_closed: false,
            waiting: VecDeque::new(),
        }
    }

    /// `n` bytes left the queue. How many to tell the stack it may send,
    /// when it is time to: half a window at once, so a program that reads
    /// a little at a time does not send a message for each.
    fn consumed(&mut self, n: usize) -> Option<u32> {
        self.unsaid += n;
        if self.unsaid >= STREAM_WINDOW / 2 {
            Some(core::mem::take(&mut self.unsaid) as u32)
        } else {
            None
        }
    }

    /// Copy the first unread bytes into `data`, and take them off the
    /// queue when `take` says so. How many.
    fn copy_out(&mut self, data: &mut [u8], take: bool) -> usize {
        let mut done = 0;
        let mut at = self.read_at;
        for chunk in &self.chunks {
            if done == data.len() {
                break;
            }
            let n = (chunk.len() - at).min(data.len() - done);
            data[done..done + n].copy_from_slice(&chunk[at..at + n]);
            done += n;
            at = 0;
        }
        if take {
            let mut left = done;
            while left > 0 {
                let first = self.chunks.front().map_or(0, Vec::len) - self.read_at;
                if left < first {
                    self.read_at += left;
                    break;
                }
                left -= first;
                self.read_at = 0;
                self.chunks.pop_front();
            }
            self.queued -= done;
        }
        done
    }
}

/// Whether a read would not wait. `dead`: the stack is gone.
fn readable(inner: &Inner, dead: bool) -> bool {
    dead || match inner.phase {
        Phase::Listening => !inner.waiting.is_empty(),
        Phase::Connecting => false,
        _ => {
            inner.queued > 0
                || inner.eof
                || inner.read_closed
                || inner.error.is_some()
                || inner.phase == Phase::Over
        }
    }
}

/// Whether a write would not wait: it has room, or it has an answer that
/// is not "wait".
fn writable(inner: &Inner, dead: bool) -> bool {
    dead || match inner.phase {
        Phase::Open => inner.credit > 0 || inner.write_closed,
        Phase::Connecting | Phase::Listening => false,
        Phase::Fresh | Phase::Over => true,
    }
}

fn failed(inner: &Inner, dead: bool) -> bool {
    dead || inner.error.is_some()
}

impl Stream {
    fn new(inner: Inner) -> Arc<Stream> {
        let stream = Arc::new(Stream {
            bus: Arc::new(Mutex::new(EventBus::default())),
            inner: Mutex::new(inner),
        });
        stream.sync(&stream.inner.lock(), false, Event::empty());
        stream
    }

    /// Make the bus say what `inner` says. `news` are the events that
    /// just happened again: said once more even if they were said, for
    /// who waits for an edge.
    fn sync(&self, inner: &Inner, dead: bool, news: Event) {
        let mut bus = self.bus.lock();
        let was = bus.events();
        for (event, now) in [
            (Event::READABLE, readable(inner, dead)),
            (Event::WRITABLE, writable(inner, dead)),
            (Event::ERROR, failed(inner, dead)),
        ] {
            if now && (!was.contains(event) || news.contains(event)) {
                bus.set(event);
            } else if !now && was.contains(event) {
                bus.clear(event);
            }
        }
    }

    /// The stack is gone: whoever waits on this socket is woken to find
    /// out.
    pub(super) fn wake(&self) {
        let inner = self.inner.lock();
        self.sync(
            &inner,
            true,
            Event::READABLE | Event::WRITABLE | Event::ERROR,
        );
    }
}

/// The new socket, and where it connected from, of an `ACCEPTED`.
fn parse_accepted(bytes: &[u8]) -> Option<(u64, IpEndpoint)> {
    if bytes.len() != ACCEPTED_LEN {
        return None;
    }
    let sock = u64::from_le_bytes(bytes[..8].try_into().ok()?);
    let addr: [u8; 4] = bytes[8..12].try_into().ok()?;
    let port = u16::from_le_bytes(bytes[12..14].try_into().ok()?);
    Some((
        sock,
        IpEndpoint::new(IpAddress::Ipv4(Ipv4Address(addr)), port),
    ))
}

impl StackLink {
    /// One record of an `EVENTS`.
    pub(super) fn event(&self, sock: u64, what: u16, number: u32, bytes: &[u8]) {
        let stream = self.table.lock().streams.get(&sock).and_then(Weak::upgrade);
        let Some(stream) = stream else {
            // A connection for a listener that closed meanwhile is
            // nobody's, and the stack is told so. Anything else for a
            // socket that closed is late.
            if what == EV_ACCEPTED {
                if let Some((new, _)) = parse_accepted(bytes) {
                    let _ = self.tell(close_message(new), true);
                }
            }
            return;
        };
        let dead = self.is_dead();
        let mut say = None;
        let mut refuse = None;
        {
            let mut inner = stream.inner.lock();
            match (what, inner.phase) {
                (EV_CONNECTED, Phase::Connecting) => {
                    inner.phase = Phase::Open;
                    inner.credit = (number as usize).min(CREDIT_MAX);
                    inner.error = None;
                    stream.sync(&inner, dead, Event::WRITABLE);
                }
                (EV_FAILED, Phase::Connecting | Phase::Open) => {
                    if inner.phase == Phase::Connecting {
                        inner.phase = Phase::Fresh;
                        inner.remote = None;
                    } else {
                        inner.phase = Phase::Over;
                    }
                    inner.credit = 0;
                    inner.error = Some(match number {
                        WHY_REFUSED => LxError::ECONNREFUSED,
                        WHY_TIMED_OUT => LxError::ETIMEDOUT,
                        WHY_UNREACHABLE => LxError::ENETUNREACH,
                        WHY_NO_ROOM => LxError::ENOBUFS,
                        _ => LxError::ECONNRESET,
                    });
                    stream.sync(
                        &inner,
                        dead,
                        Event::READABLE | Event::WRITABLE | Event::ERROR,
                    );
                }
                (EV_ACCEPTED, Phase::Listening) => {
                    let Some((new, from)) = parse_accepted(bytes) else {
                        warn!("user stack: an ACCEPTED of {} bytes", bytes.len());
                        return;
                    };
                    let mut table = self.table.lock();
                    if new & ACCEPTED_BIT == 0 || table.streams.contains_key(&new) {
                        warn!("user stack: an ACCEPTED with a number that is taken");
                        return;
                    }
                    if inner.waiting.len() >= WAITING_MAX {
                        // Nobody accepts: the connection is not kept.
                        refuse = Some(close_message(new));
                    } else {
                        let mut child = Inner::new(Phase::Open);
                        child.credit = (number as usize).min(CREDIT_MAX);
                        child.port = inner.port;
                        child.remote = Some(from);
                        let child = Stream::new(child);
                        table.streams.insert(new, Arc::downgrade(&child));
                        drop(table);
                        inner.waiting.push_back((new, child));
                        stream.sync(&inner, dead, Event::READABLE);
                    }
                }
                (EV_BYTES, Phase::Open) if !bytes.is_empty() => {
                    if inner.queued + inner.unsaid + bytes.len() > STREAM_WINDOW {
                        // More than the kernel said it takes: a stack that
                        // does not count is not one to queue for.
                        warn!("user stack: bytes past the window of a socket");
                        inner.phase = Phase::Over;
                        inner.credit = 0;
                        inner.error = Some(LxError::ECONNRESET);
                        refuse = Some(close_message(sock));
                        stream.sync(
                            &inner,
                            dead,
                            Event::READABLE | Event::WRITABLE | Event::ERROR,
                        );
                    } else if inner.read_closed {
                        // `shutdown(SHUT_RD)`: taken and thrown away, so
                        // the peer is not left waiting for room.
                        say = inner.consumed(bytes.len());
                    } else {
                        inner.queued += bytes.len();
                        inner.chunks.push_back(bytes.to_vec());
                        stream.sync(&inner, dead, Event::READABLE);
                    }
                }
                (EV_EOF, Phase::Open) => {
                    inner.eof = true;
                    stream.sync(&inner, dead, Event::READABLE);
                }
                (EV_CREDIT, Phase::Open) => {
                    inner.credit = (inner.credit + number as usize).min(CREDIT_MAX);
                    stream.sync(&inner, dead, Event::WRITABLE);
                }
                // An event for a socket that is no longer where the event
                // finds it: a CREDIT after a reset, a second CONNECTED.
                _ => {}
            }
        }
        if let Some(bytes) = say {
            let _ = self.tell(window_message(sock, bytes), true);
        }
        if let Some(close) = refuse {
            let _ = self.tell(close, true);
        }
    }
}

/// A stream socket whose stack is a process.
pub struct ServedTcpSocket {
    base: KObjectBase,
    link: Arc<StackLink>,
    sock: u64,
    stream: Arc<Stream>,
}

impl ServedTcpSocket {
    /// A socket of the stack behind `link`. `ENETDOWN` when it is gone.
    pub fn new(link: Arc<StackLink>) -> LxResult<Self> {
        if link.is_dead() {
            return Err(LxError::ENETDOWN);
        }
        let stream = Stream::new(Inner::new(Phase::Fresh));
        let sock = {
            let mut table = link.table.lock();
            table.next_sock += 1;
            let sock = table.next_sock;
            table.streams.insert(sock, Arc::downgrade(&stream));
            sock
        };
        Ok(ServedTcpSocket {
            base: KObjectBase::new(),
            link,
            sock,
            stream,
        })
    }

    /// The socket's port, taking any when it has none: what a connect or
    /// a listen on an unbound socket does.
    fn autobind(&self, inner: &mut Inner) -> LxResult<u16> {
        if let Some(port) = inner.port {
            return Ok(port);
        }
        let port = self.link.table.lock().stream_ports.take(0)?;
        inner.port = Some(port);
        inner.holds_port = true;
        Ok(port)
    }

    /// The next bytes, taken off the queue or only looked at.
    async fn receive(&self, data: &mut [u8], take: bool) -> SysResult {
        loop {
            {
                let mut inner = self.stream.inner.lock();
                let dead = self.link.is_dead();
                if inner.queued > 0 {
                    if data.is_empty() {
                        return Ok(0);
                    }
                    let n = inner.copy_out(data, take);
                    let say = if take { inner.consumed(n) } else { None };
                    self.stream.sync(&inner, dead, Event::empty());
                    drop(inner);
                    if let Some(bytes) = say {
                        let _ = self.link.tell(window_message(self.sock, bytes), true);
                    }
                    return Ok(n);
                }
                // `tcp_recvmsg`: what was queued first, then the error,
                // once, then the end.
                if let Some(e) = inner.error.take() {
                    self.stream.sync(&inner, dead, Event::empty());
                    return Err(e);
                }
                if inner.eof || inner.read_closed || inner.phase == Phase::Over {
                    return Ok(0);
                }
                if matches!(inner.phase, Phase::Fresh | Phase::Listening) {
                    return Err(LxError::ENOTCONN);
                }
                if dead {
                    return Err(LxError::ENETDOWN);
                }
                if data.is_empty() {
                    return Ok(0);
                }
                if inner.flags.contains(OpenFlags::NON_BLOCK) {
                    return Err(LxError::EAGAIN);
                }
            }
            // Woken by bytes, by the end, or ended by a signal.
            crate::sync::wait_for_event(self.stream.bus.clone(), Event::READABLE).await?;
        }
    }

    /// Stop taking connections: the ones nobody accepted are closed.
    fn unlisten(&self, waiting: VecDeque<(u64, Arc<Stream>)>) {
        {
            let mut table = self.link.table.lock();
            for (sock, _) in &waiting {
                table.streams.remove(sock);
            }
        }
        for (sock, _) in waiting {
            let _ = self.link.tell(close_message(sock), true);
        }
    }

    fn nobody() -> Endpoint {
        Endpoint::Ip(IpEndpoint::UNSPECIFIED)
    }
}

impl Drop for ServedTcpSocket {
    fn drop(&mut self) {
        let (port, waiting, known) = {
            let mut inner = self.stream.inner.lock();
            (
                inner.port.filter(|_| inner.holds_port),
                core::mem::take(&mut inner.waiting),
                // What the stack has a socket for. One whose connection
                // failed it has let go of already.
                matches!(
                    inner.phase,
                    Phase::Connecting | Phase::Open | Phase::Listening
                ),
            )
        };
        {
            let mut table = self.link.table.lock();
            table.streams.remove(&self.sock);
            if let Some(port) = port {
                table.stream_ports.taken.remove(&port);
            }
        }
        self.unlisten(waiting);
        if known {
            let _ = self.link.tell(close_message(self.sock), true);
        }
    }
}

#[async_trait]
impl Socket for ServedTcpSocket {
    async fn read(&self, data: &mut [u8]) -> (SysResult, Endpoint) {
        (self.receive(data, true).await, Self::nobody())
    }

    async fn peek(&self, data: &mut [u8]) -> (SysResult, Endpoint) {
        (self.receive(data, false).await, Self::nobody())
    }

    /// As many of `data` as the stack still takes. `EAGAIN` when it takes
    /// none, and the socket is not writable until it does: the system
    /// call waits for that, for a socket that blocks.
    fn write(&self, data: &[u8], _sendto_endpoint: Option<Endpoint>) -> SysResult {
        let n = {
            let mut inner = self.stream.inner.lock();
            let dead = self.link.is_dead();
            if dead {
                return Err(LxError::ENETDOWN);
            }
            match inner.phase {
                Phase::Open if !inner.write_closed => {}
                Phase::Connecting => return Err(LxError::EAGAIN),
                // `sk_stream_error`: the error the connection left, once,
                // and a broken pipe from then on.
                _ => {
                    let e = inner.error.take().unwrap_or(LxError::EPIPE);
                    self.stream.sync(&inner, dead, Event::empty());
                    return Err(e);
                }
            }
            if data.is_empty() {
                return Ok(0);
            }
            let n = data.len().min(inner.credit).min(WRITE_MAX);
            inner.credit -= n;
            self.stream.sync(&inner, dead, Event::empty());
            n
        };
        if n == 0 {
            return Err(LxError::EAGAIN);
        }
        self.link.tell(write_message(self.sock, &data[..n]), true)?;
        Ok(n)
    }

    fn poll(&self, _events: PollEvents) -> (bool, bool, bool) {
        let inner = self.stream.inner.lock();
        let dead = self.link.is_dead();
        (
            readable(&inner, dead),
            writable(&inner, dead),
            failed(&inner, dead),
        )
    }

    async fn connect(&self, endpoint: Endpoint) -> SysResult {
        let Endpoint::Ip(peer) = endpoint else {
            return Err(LxError::EAFNOSUPPORT);
        };
        let IpAddress::Ipv4(addr) = peer.addr else {
            return Err(LxError::EAFNOSUPPORT);
        };
        let started = {
            let mut inner = self.stream.inner.lock();
            if self.link.is_dead() {
                return Err(LxError::ENETDOWN);
            }
            match inner.phase {
                Phase::Fresh => {
                    // Port 0 is nobody's.
                    if peer.port == 0 {
                        return Err(LxError::ECONNREFUSED);
                    }
                    let local = self.autobind(&mut inner)?;
                    inner.phase = Phase::Connecting;
                    inner.error = None;
                    inner.remote = Some(peer);
                    self.stream.sync(&inner, false, Event::empty());
                    Some(local)
                }
                // `__inet_stream_connect`: a second connect on a socket
                // that does not wait is told the first is under way; one
                // that waits, waits for it.
                Phase::Connecting if inner.flags.contains(OpenFlags::NON_BLOCK) => {
                    return Err(LxError::EALREADY)
                }
                Phase::Connecting => None,
                Phase::Open | Phase::Listening | Phase::Over => return Err(LxError::EISCONN),
            }
        };
        if let Some(local) = started {
            let connect =
                connect_message(self.sock, addr.0, peer.port, local, STREAM_WINDOW as u32);
            if let Err(e) = self.link.tell(connect, true) {
                let mut inner = self.stream.inner.lock();
                inner.phase = Phase::Fresh;
                inner.remote = None;
                self.stream.sync(&inner, true, Event::empty());
                return Err(e);
            }
            if self
                .stream
                .inner
                .lock()
                .flags
                .contains(OpenFlags::NON_BLOCK)
            {
                return Err(LxError::EINPROGRESS);
            }
        }
        loop {
            {
                let mut inner = self.stream.inner.lock();
                let dead = self.link.is_dead();
                match inner.phase {
                    Phase::Open => return Ok(0),
                    Phase::Connecting if dead => return Err(LxError::ENETDOWN),
                    Phase::Connecting => {}
                    _ => {
                        let e = inner.error.take().unwrap_or(LxError::ECONNREFUSED);
                        self.stream.sync(&inner, dead, Event::empty());
                        return Err(e);
                    }
                }
            }
            // A signal ends the wait and leaves the connection being made,
            // as Linux does: the next connect waits for it again.
            crate::sync::wait_for_event(self.stream.bus.clone(), Event::WRITABLE | Event::ERROR)
                .await?;
        }
    }

    fn bind(&self, endpoint: Endpoint) -> SysResult {
        let Endpoint::Ip(local) = endpoint else {
            return Err(LxError::EINVAL);
        };
        match local.addr {
            IpAddress::Unspecified => {}
            IpAddress::Ipv4(addr) if addr.is_unspecified() || addr == self.link.address => {}
            // The stack has one address, and no loopback.
            IpAddress::Ipv4(_) => return Err(LxError::EADDRNOTAVAIL),
            _ => return Err(LxError::EAFNOSUPPORT),
        }
        let mut inner = self.stream.inner.lock();
        // `inet_bind`: a socket that has a port, or is past needing one,
        // is `EINVAL`.
        if inner.port.is_some() || inner.phase != Phase::Fresh {
            return Err(LxError::EINVAL);
        }
        let port = self.link.table.lock().stream_ports.take(local.port)?;
        inner.port = Some(port);
        inner.holds_port = true;
        Ok(0)
    }

    fn listen(&self) -> SysResult {
        let port = {
            let mut inner = self.stream.inner.lock();
            if self.link.is_dead() {
                return Err(LxError::ENETDOWN);
            }
            match inner.phase {
                Phase::Listening => return Ok(0),
                Phase::Fresh => {}
                // `inet_listen`: only a closed socket can start listening.
                _ => return Err(LxError::EINVAL),
            }
            let port = self.autobind(&mut inner)?;
            inner.phase = Phase::Listening;
            inner.error = None;
            self.stream.sync(&inner, false, Event::empty());
            port
        };
        let listen = listen_message(self.sock, port, LISTEN_BACKLOG, STREAM_WINDOW as u32);
        if let Err(e) = self.link.tell(listen, true) {
            let mut inner = self.stream.inner.lock();
            inner.phase = Phase::Fresh;
            self.stream.sync(&inner, true, Event::empty());
            return Err(e);
        }
        Ok(0)
    }

    fn is_listening(&self) -> bool {
        self.stream.inner.lock().phase == Phase::Listening
    }

    fn shutdown(&self, howto: usize) -> SysResult {
        let (rd, wr) = shutdown_sides(howto)?;
        let mut inner = self.stream.inner.lock();
        let dead = self.link.is_dead();
        match inner.phase {
            // `inet_shutdown` on a listener: the read side stops it,
            // `SHUT_WR` alone leaves it listening.
            Phase::Listening => {
                if rd {
                    inner.phase = Phase::Fresh;
                    let waiting = core::mem::take(&mut inner.waiting);
                    self.stream.sync(&inner, dead, Event::READABLE);
                    drop(inner);
                    self.unlisten(waiting);
                    let _ = self.link.tell(close_message(self.sock), true);
                }
                Ok(0)
            }
            Phase::Open => {
                let fin = wr && !inner.write_closed;
                inner.read_closed |= rd;
                inner.write_closed |= wr;
                self.stream
                    .sync(&inner, dead, Event::READABLE | Event::WRITABLE);
                drop(inner);
                if fin {
                    self.link.tell(shutdown_message(self.sock), true)?;
                }
                Ok(0)
            }
            // `inet_shutdown` on a socket that is not connected: the
            // sides are recorded, and the call fails.
            _ => {
                inner.read_closed |= rd;
                self.stream.sync(&inner, dead, Event::READABLE);
                Err(LxError::ENOTCONN)
            }
        }
    }

    async fn accept(&self) -> LxResult<(Arc<dyn FileLike>, Endpoint)> {
        loop {
            {
                let mut inner = self.stream.inner.lock();
                let dead = self.link.is_dead();
                if inner.phase != Phase::Listening {
                    return Err(LxError::EINVAL);
                }
                if let Some((sock, stream)) = inner.waiting.pop_front() {
                    self.stream.sync(&inner, dead, Event::empty());
                    drop(inner);
                    let from = stream.inner.lock().remote;
                    let socket = Arc::new(ServedTcpSocket {
                        base: KObjectBase::new(),
                        link: self.link.clone(),
                        sock,
                        stream,
                    });
                    let from = from.unwrap_or(IpEndpoint::UNSPECIFIED);
                    return Ok((socket as Arc<dyn FileLike>, Endpoint::Ip(from)));
                }
                if dead {
                    return Err(LxError::ENETDOWN);
                }
                if inner.flags.contains(OpenFlags::NON_BLOCK) {
                    return Err(LxError::EAGAIN);
                }
            }
            crate::sync::wait_for_event(self.stream.bus.clone(), Event::READABLE).await?;
        }
    }

    fn endpoint(&self) -> Option<Endpoint> {
        let inner = self.stream.inner.lock();
        // A connection has the stack's address; a socket that only has a
        // port has every address, which is the one there is.
        let addr = match inner.phase {
            Phase::Connecting | Phase::Open | Phase::Over => self.link.address,
            Phase::Fresh | Phase::Listening => Ipv4Address::UNSPECIFIED,
        };
        Some(Endpoint::Ip(IpEndpoint::new(
            IpAddress::Ipv4(addr),
            inner.port.unwrap_or(0),
        )))
    }

    fn remote_endpoint(&self) -> Option<Endpoint> {
        let inner = self.stream.inner.lock();
        inner
            .remote
            .filter(|_| inner.phase == Phase::Open)
            .map(Endpoint::Ip)
    }

    fn take_so_error(&self) -> i32 {
        let mut inner = self.stream.inner.lock();
        let error = inner.error.take();
        self.stream
            .sync(&inner, self.link.is_dead(), Event::empty());
        error.map_or(0, |e| e as isize as i32)
    }

    fn is_tcp(&self) -> bool {
        true
    }

    fn is_inet(&self) -> bool {
        true
    }

    fn so_domain(&self) -> Option<u32> {
        Some(Domain::AF_INET as u32)
    }

    fn so_protocol(&self) -> Option<u32> {
        Some(6) // IPPROTO_TCP
    }

    fn socket_type(&self) -> Option<SocketType> {
        Some(SocketType::SOCK_STREAM)
    }
}

impl_kobject!(ServedTcpSocket);

#[async_trait]
impl FileLike for ServedTcpSocket {
    fn metadata(&self) -> LxResult<rcore_fs::vfs::Metadata> {
        Ok(crate::fs::anon_metadata(
            rcore_fs::vfs::FileType::Socket,
            self.id() as usize,
        ))
    }

    fn flags(&self) -> OpenFlags {
        self.stream.inner.lock().flags
    }

    fn set_flags(&self, f: OpenFlags) -> LxResult {
        self.stream.inner.lock().flags.take_settable(f);
        Ok(())
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

    fn poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        let inner = self.stream.inner.lock();
        let dead = self.link.is_dead();
        Ok(PollStatus {
            read: readable(&inner, dead),
            write: writable(&inner, dead),
            error: failed(&inner, dead),
            // `tcp_poll`: both directions are over.
            hangup: inner.phase == Phase::Over
                || ((inner.eof || inner.read_closed) && inner.write_closed),
        })
    }

    async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus> {
        let (read, write) = (events.wants_read(), events.wants_write());
        let mut mask = Event::ERROR;
        if read {
            mask |= Event::READABLE;
        }
        if write {
            mask |= Event::WRITABLE;
        }
        loop {
            let status = FileLike::poll(self, events)?;
            if (read && status.read)
                || (write && status.write)
                || status.error
                || status.hangup
                || !(read || write)
            {
                return Ok(status);
            }
            crate::sync::wait_for_event(self.stream.bus.clone(), mask).await?;
        }
    }

    fn subscribe_readiness(
        &self,
        events: PollEvents,
        waker: &core::task::Waker,
    ) -> Option<crate::sync::ReadinessSub> {
        let mask = crate::fs::poll_events_to_bus_mask(events);
        Some(crate::sync::subscribe_readiness_on(
            &self.stream.bus,
            mask,
            waker,
        ))
    }

    fn readiness_seq(&self, events: PollEvents) -> Option<u64> {
        let mask = crate::fs::poll_events_to_bus_mask(events);
        Some(self.stream.bus.lock().seq_for(mask))
    }

    fn subscribe_edge(
        &self,
        events: PollEvents,
        waker: &core::task::Waker,
        seen: u64,
    ) -> Option<crate::sync::ReadinessSub> {
        let mask = crate::fs::poll_events_to_bus_mask(events);
        Some(crate::sync::subscribe_edge_on(
            &self.stream.bus,
            mask,
            waker,
            seen,
        ))
    }

    /// `FIONREAD`: the bytes that arrived and were not read.
    fn readable_bytes(&self) -> Option<usize> {
        Some(self.stream.inner.lock().queued)
    }

    fn as_socket(&self) -> LxResult<&dyn Socket> {
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    //! The stream socket against a stack that is the test, as the datagram
    //! socket's tests are.

    extern crate std;

    use super::super::tests::{any, at, ip, stack, Stack, HOST, STACK_ADDR};
    use super::*;
    use crate::net::NET_TEST_LOCK as LOCK;

    /// A stream socket that does not wait.
    fn stream(stack: &Stack) -> ServedTcpSocket {
        let s = ServedTcpSocket::new(stack.link.clone()).unwrap();
        FileLike::set_flags(&s, OpenFlags::NON_BLOCK).unwrap();
        s
    }

    /// The stack says `records`, in one message.
    fn events(stack: &Stack, records: &[EventRecord<'_>]) {
        let mut m = events_message();
        for &(sock, what, number, bytes) in records {
            push_event(&mut m, sock, what, number, bytes);
        }
        stack.say(m);
    }

    fn connect(s: &ServedTcpSocket, to: IpEndpoint) -> SysResult {
        async_std::task::block_on(Socket::connect(s, Endpoint::Ip(to)))
    }

    fn read(s: &ServedTcpSocket, buf: &mut [u8]) -> SysResult {
        async_std::task::block_on(Socket::read(s, buf)).0
    }

    fn peek(s: &ServedTcpSocket, buf: &mut [u8]) -> SysResult {
        async_std::task::block_on(Socket::peek(s, buf)).0
    }

    fn accept(s: &ServedTcpSocket) -> LxResult<(Arc<dyn FileLike>, IpEndpoint)> {
        async_std::task::block_on(Socket::accept(s)).map(|(socket, from)| (socket, ip(from)))
    }

    fn status(s: &ServedTcpSocket) -> PollStatus {
        FileLike::poll(s, PollEvents::IN | PollEvents::OUT).unwrap()
    }

    /// A socket connected to `HOST:80`, with `credit` bytes of credit, and
    /// the stack's messages read up to there. Its number.
    fn connected(stack: &Stack, credit: u32) -> (ServedTcpSocket, u64) {
        let s = stream(stack);
        assert_eq!(connect(&s, at(HOST, 80)), Err(LxError::EINPROGRESS));
        let (op, sock) = parse_header(&stack.told().unwrap()).unwrap();
        assert_eq!(op, OP_CONNECT);
        events(stack, &[(sock, EV_CONNECTED, credit, &[])]);
        (s, sock)
    }

    #[test]
    fn the_stream_messages_are_these_bytes() {
        // The same bytes `tools/eclipse-netstack/src/socks.rs` reads and
        // writes: a field moved on one side fails on the other.
        assert_eq!(
            connect_message(7, [10, 0, 9, 1], 80, 49152, 0x0004_0000),
            [
                0, 0, 0, 0, 5, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, // header
                10, 0, 9, 1, 80, 0, 0, 0xc0, 0, 0, 4, 0,
            ]
        );
        assert_eq!(
            listen_message(7, 8080, 8, 0x0004_0000),
            [0, 0, 0, 0, 6, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0x90, 0x1f, 8, 0, 0, 0, 4, 0]
        );
        assert_eq!(
            write_message(7, b"hola"),
            [0, 0, 0, 0, 7, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, b'h', b'o', b'l', b'a']
        );
        assert_eq!(
            shutdown_message(7),
            [0, 0, 0, 0, 8, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            window_message(7, 0x0002_0000),
            [0, 0, 0, 0, 9, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0]
        );

        let mut m = events_message();
        assert_eq!(parse_header(&m), Some((OP_EVENTS, 0)));
        push_event(&mut m, 7, EV_BYTES, 0, b"hola");
        let new = ACCEPTED_BIT | 1;
        push_event(
            &mut m,
            9,
            EV_ACCEPTED,
            0x0004_0000,
            &accepted_bytes(new, [10, 0, 9, 1], 40000),
        );
        assert_eq!(
            m[HEADER_LEN..],
            [
                7, 0, 0, 0, 0, 0, 0, 0, 4, 0, 4, 0, 0, 0, 0, 0, b'h', b'o', b'l',
                b'a', // BYTES
                9, 0, 0, 0, 0, 0, 0, 0, 3, 0, 16, 0, 0, 0, 4, 0, // ACCEPTED
                1, 0, 0, 0, 0, 0, 0, 0x80, 10, 0, 9, 1, 0x40, 0x9c, 0, 0,
            ]
        );
        let (first, rest) = parse_event(&m[HEADER_LEN..]).unwrap();
        assert_eq!(first, (7, EV_BYTES, 0, &b"hola"[..]));
        let ((sock, what, number, bytes), rest) = parse_event(rest).unwrap();
        assert_eq!((sock, what, number), (9, EV_ACCEPTED, 0x0004_0000));
        assert_eq!(parse_accepted(bytes), Some((new, at([10, 0, 9, 1], 40000))));
        assert!(rest.is_empty());
        // A record cut short, in its head or in its bytes, is none.
        assert_eq!(parse_event(&m[HEADER_LEN..HEADER_LEN + 15]), None);
        assert_eq!(parse_event(&m[HEADER_LEN..HEADER_LEN + 18]), None);
        // The longest WRITE is a whole channel message.
        assert_eq!(write_message(1, &[0; WRITE_MAX]).len(), 65536);
    }

    #[test]
    fn a_connect_is_told_and_is_done_when_the_stack_says_so() {
        let _lock = LOCK.lock();
        let stack = stack();
        let s = stream(&stack);
        assert!(
            status(&s).write,
            "a write on a new socket does not wait: it fails"
        );
        assert_eq!(Socket::write(&s, b"x", None), Err(LxError::EPIPE));
        let mut buf = [0u8; 8];
        assert_eq!(read(&s, &mut buf), Err(LxError::ENOTCONN));
        assert!(Socket::remote_endpoint(&s).is_none());

        assert_eq!(connect(&s, at(HOST, 0)), Err(LxError::ECONNREFUSED));
        assert_eq!(stack.told(), None);
        assert_eq!(connect(&s, at(HOST, 80)), Err(LxError::EINPROGRESS));
        let told = stack.told().unwrap();
        let (op, sock) = parse_header(&told).unwrap();
        assert_eq!(op, OP_CONNECT);
        assert_eq!(sock & ACCEPTED_BIT, 0);
        let port = ip(Socket::endpoint(&s).unwrap()).port;
        assert!(EPHEMERAL.contains(&port));
        assert_eq!(
            told,
            connect_message(sock, HOST, 80, port, STREAM_WINDOW as u32)
        );

        // Under way: neither readable nor writable, and a second connect
        // is told so.
        let now = status(&s);
        assert!(!now.read && !now.write && !now.error);
        assert_eq!(connect(&s, at(HOST, 80)), Err(LxError::EALREADY));
        assert_eq!(Socket::write(&s, b"x", None), Err(LxError::EAGAIN));
        assert_eq!(read(&s, &mut buf), Err(LxError::EAGAIN));
        assert_eq!(stack.told(), None);

        events(&stack, &[(sock, EV_CONNECTED, 1000, &[])]);
        let now = status(&s);
        assert!(!now.read && now.write && !now.error && !now.hangup);
        assert_eq!(Socket::take_so_error(&s), 0);
        assert_eq!(connect(&s, at(HOST, 80)), Err(LxError::EISCONN));
        assert_eq!(ip(Socket::endpoint(&s).unwrap()), at(STACK_ADDR, port));
        assert_eq!(ip(Socket::remote_endpoint(&s).unwrap()), at(HOST, 80));
        assert_eq!(Socket::socket_type(&s), Some(SocketType::SOCK_STREAM));
        assert!(Socket::is_tcp(&s));

        // Closed: the stack is told, and the port is free again.
        drop(s);
        assert_eq!(stack.told(), Some(close_message(sock)));
        let other = stream(&stack);
        assert_eq!(Socket::bind(&other, any(port)), Ok(0));
    }

    #[test]
    fn a_connect_that_fails_leaves_its_error_once_and_a_socket_to_try_again() {
        let _lock = LOCK.lock();
        let stack = stack();
        for (why, error) in [
            (WHY_REFUSED, LxError::ECONNREFUSED),
            (WHY_TIMED_OUT, LxError::ETIMEDOUT),
            (WHY_UNREACHABLE, LxError::ENETUNREACH),
            (WHY_NO_ROOM, LxError::ENOBUFS),
        ] {
            let s = stream(&stack);
            assert_eq!(connect(&s, at(HOST, 80)), Err(LxError::EINPROGRESS));
            let (_, sock) = parse_header(&stack.told().unwrap()).unwrap();
            events(&stack, &[(sock, EV_FAILED, why, &[])]);
            // What a program that waited in `poll` for the connect sees.
            let now = status(&s);
            assert!(now.read && now.write && now.error);
            assert_eq!(Socket::take_so_error(&s), error as isize as i32);
            assert_eq!(Socket::take_so_error(&s), 0);
            assert!(!status(&s).error);
            assert!(Socket::remote_endpoint(&s).is_none());
            // The stack let go of it: a close says nothing, and the same
            // socket connects again, from the same port.
            assert_eq!(connect(&s, at(HOST, 81)), Err(LxError::EINPROGRESS));
            let port = ip(Socket::endpoint(&s).unwrap()).port;
            assert_eq!(
                stack.told(),
                Some(connect_message(sock, HOST, 81, port, STREAM_WINDOW as u32))
            );
            events(&stack, &[(sock, EV_FAILED, why, &[])]);
            drop(s);
            assert_eq!(stack.told(), None);
        }
    }

    #[test]
    fn a_write_carries_what_the_stack_takes_and_no_more() {
        let _lock = LOCK.lock();
        let stack = stack();
        let (s, sock) = connected(&stack, 10);
        assert_eq!(Socket::write(&s, b"", None), Ok(0));
        assert_eq!(stack.told(), None);
        assert_eq!(Socket::write(&s, b"0123456789abcdef", None), Ok(10));
        assert_eq!(stack.told(), Some(write_message(sock, b"0123456789")));
        // Full: not writable, and the write says to wait.
        assert!(!status(&s).write);
        assert_eq!(Socket::write(&s, b"abcdef", None), Err(LxError::EAGAIN));
        assert_eq!(stack.told(), None);

        events(&stack, &[(sock, EV_CREDIT, 4, &[])]);
        assert!(status(&s).write);
        assert_eq!(Socket::write(&s, b"abcdef", None), Ok(4));
        assert_eq!(stack.told(), Some(write_message(sock, b"abcd")));

        // However much the stack takes, a WRITE is one channel message.
        events(&stack, &[(sock, EV_CREDIT, 200_000, &[])]);
        let big = vec![7u8; 100_000];
        assert_eq!(Socket::write(&s, &big, None), Ok(WRITE_MAX));
        assert_eq!(stack.told().unwrap().len(), 65536);
        assert_eq!(
            Socket::write(&s, &big[WRITE_MAX..], None),
            Ok(100_000 - WRITE_MAX)
        );
    }

    #[test]
    fn what_arrives_is_read_in_order_across_the_pieces_it_came_in() {
        let _lock = LOCK.lock();
        let stack = stack();
        let (s, sock) = connected(&stack, 100);
        let mut buf = [0u8; 6];
        assert_eq!(read(&s, &mut buf), Err(LxError::EAGAIN));
        assert!(!status(&s).read);

        events(
            &stack,
            &[
                (sock, EV_BYTES, 0, b"hola"),
                (sock, EV_BYTES, 0, b", mundo"),
            ],
        );
        assert!(status(&s).read);
        assert_eq!(FileLike::readable_bytes(&s), Some(11));
        assert_eq!(peek(&s, &mut buf), Ok(6));
        assert_eq!(&buf, b"hola, ");
        assert_eq!(FileLike::readable_bytes(&s), Some(11));
        assert_eq!(read(&s, &mut buf[..2]), Ok(2));
        assert_eq!(&buf[..2], b"ho");
        assert_eq!(read(&s, &mut buf), Ok(6));
        assert_eq!(&buf, b"la, mu");
        assert_eq!(read(&s, &mut buf), Ok(3));
        assert_eq!(&buf[..3], b"ndo");
        assert_eq!(read(&s, &mut buf), Err(LxError::EAGAIN));
        assert!(!status(&s).read);

        // The end: readable for ever, and every read is 0.
        events(
            &stack,
            &[(sock, EV_BYTES, 0, b"fin"), (sock, EV_EOF, 0, &[])],
        );
        assert_eq!(read(&s, &mut buf), Ok(3));
        assert!(status(&s).read);
        assert_eq!(read(&s, &mut buf), Ok(0));
        assert_eq!(read(&s, &mut buf), Ok(0));
        // Half closed: the other half still writes.
        assert!(!status(&s).hangup);
        assert_eq!(Socket::write(&s, b"x", None), Ok(1));
        // Reading told the stack nothing: far from half a window.
        assert_eq!(stack.told(), Some(write_message(sock, b"x")));
        assert_eq!(stack.told(), None);
    }

    #[test]
    fn the_stack_is_told_of_room_half_a_window_at_a_time() {
        let _lock = LOCK.lock();
        let stack = stack();
        let (s, sock) = connected(&stack, 100);
        let piece = vec![1u8; 32 * 1024];
        let pieces = STREAM_WINDOW / piece.len();
        for _ in 0..pieces {
            events(&stack, &[(sock, EV_BYTES, 0, &piece)]);
        }
        assert_eq!(FileLike::readable_bytes(&s), Some(STREAM_WINDOW));
        let mut buf = vec![0u8; piece.len()];
        let mut said = 0usize;
        for done in 1..=pieces {
            assert_eq!(read(&s, &mut buf), Ok(piece.len()));
            match stack.told() {
                Some(m) => {
                    assert_eq!(done * piece.len() - said, STREAM_WINDOW / 2);
                    assert_eq!(m, window_message(sock, (STREAM_WINDOW / 2) as u32));
                    said = done * piece.len();
                }
                None => assert!(done * piece.len() - said < STREAM_WINDOW / 2),
            }
        }
        assert_eq!(said, STREAM_WINDOW);

        // A stack that sends past what it was told there is room for is
        // not counting, and its connection is over.
        for _ in 0..pieces {
            events(&stack, &[(sock, EV_BYTES, 0, &piece)]);
        }
        assert_eq!(stack.told(), None);
        events(&stack, &[(sock, EV_BYTES, 0, b"x")]);
        assert_eq!(stack.told(), Some(close_message(sock)));
        let now = status(&s);
        assert!(now.error && now.hangup);
        // What was queued in time is still read, then the error, then the
        // end.
        for _ in 0..pieces {
            assert_eq!(read(&s, &mut buf), Ok(piece.len()));
        }
        assert_eq!(read(&s, &mut buf), Err(LxError::ECONNRESET));
        assert_eq!(read(&s, &mut buf), Ok(0));
        while stack.told().is_some() {}
        drop(s);
        assert_eq!(stack.told(), None, "the stack was told already");
    }

    #[test]
    fn a_reset_is_an_error_once_and_a_broken_pipe_after() {
        let _lock = LOCK.lock();
        let stack = stack();
        let (s, sock) = connected(&stack, 100);
        events(
            &stack,
            &[
                (sock, EV_BYTES, 0, b"antes"),
                (sock, EV_FAILED, WHY_RESET, &[]),
            ],
        );
        let now = status(&s);
        assert!(now.read && now.write && now.error && now.hangup);
        let mut buf = [0u8; 16];
        assert_eq!(read(&s, &mut buf), Ok(5));
        assert_eq!(read(&s, &mut buf), Err(LxError::ECONNRESET));
        assert_eq!(read(&s, &mut buf), Ok(0));
        assert_eq!(Socket::write(&s, b"x", None), Err(LxError::EPIPE));
        assert_eq!(Socket::take_so_error(&s), 0);
        assert!(status(&s).hangup && !status(&s).error);
        assert!(Socket::remote_endpoint(&s).is_none());
        // Late news for it changes nothing.
        events(
            &stack,
            &[(sock, EV_CREDIT, 50, &[]), (sock, EV_BYTES, 0, b"x")],
        );
        assert_eq!(read(&s, &mut buf), Ok(0));

        // The error a write finds first is the connection's.
        let (s, sock) = connected(&stack, 100);
        events(&stack, &[(sock, EV_FAILED, WHY_RESET, &[])]);
        assert_eq!(Socket::write(&s, b"x", None), Err(LxError::ECONNRESET));
        assert_eq!(Socket::write(&s, b"x", None), Err(LxError::EPIPE));
        assert_eq!(stack.told(), None);
    }

    #[test]
    fn a_listener_hands_out_the_connections_the_stack_numbers() {
        let _lock = LOCK.lock();
        let stack = stack();
        let l = stream(&stack);
        assert_eq!(Socket::bind(&l, any(8080)), Ok(0));
        assert_eq!(Socket::bind(&l, any(8081)), Err(LxError::EINVAL));
        let other = stream(&stack);
        assert_eq!(Socket::bind(&other, any(8080)), Err(LxError::EADDRINUSE));
        assert_eq!(accept(&l).err(), Some(LxError::EINVAL));
        assert_eq!(Socket::listen(&l), Ok(0));
        assert!(Socket::is_listening(&l));
        let told = stack.told().unwrap();
        let (op, sock) = parse_header(&told).unwrap();
        assert_eq!(op, OP_LISTEN);
        assert_eq!(
            told,
            listen_message(sock, 8080, LISTEN_BACKLOG, STREAM_WINDOW as u32)
        );
        assert_eq!(Socket::listen(&l), Ok(0));
        assert_eq!(stack.told(), None, "it was listening");
        assert_eq!(connect(&l, at(HOST, 80)), Err(LxError::EISCONN));
        assert_eq!(accept(&l).err(), Some(LxError::EAGAIN));
        assert!(!status(&l).read);

        // Two connections, the first with bytes behind it in the same
        // message.
        let (one, two) = (ACCEPTED_BIT | 1, ACCEPTED_BIT | 2);
        events(
            &stack,
            &[
                (sock, EV_ACCEPTED, 500, &accepted_bytes(one, HOST, 40001)),
                (one, EV_BYTES, 0, b"GET /"),
                (sock, EV_ACCEPTED, 500, &accepted_bytes(two, HOST, 40002)),
            ],
        );
        assert!(status(&l).read);
        let (first, from) = accept(&l).unwrap();
        assert_eq!(from, at(HOST, 40001));
        let first = first.downcast_arc::<ServedTcpSocket>().ok().unwrap();
        assert!(!FileLike::flags(&*first).contains(OpenFlags::NON_BLOCK));
        FileLike::set_flags(&*first, OpenFlags::NON_BLOCK).unwrap();
        assert_eq!(ip(Socket::endpoint(&*first).unwrap()), at(STACK_ADDR, 8080));
        assert_eq!(
            ip(Socket::remote_endpoint(&*first).unwrap()),
            at(HOST, 40001)
        );
        let mut buf = [0u8; 16];
        assert_eq!(read(&first, &mut buf), Ok(5));
        assert_eq!(Socket::write(&*first, b"200", None), Ok(3));
        assert_eq!(stack.told(), Some(write_message(one, b"200")));

        // A number the stack may not give, or gave already, is no
        // connection.
        events(
            &stack,
            &[
                (sock, EV_ACCEPTED, 500, &accepted_bytes(77, HOST, 40003)),
                (sock, EV_ACCEPTED, 500, &accepted_bytes(one, HOST, 40004)),
            ],
        );

        // A listener nobody accepts from holds so many, and the rest are
        // closed as they come.
        {
            let idle = stream(&stack);
            assert_eq!(Socket::bind(&idle, any(8081)), Ok(0));
            assert_eq!(Socket::listen(&idle), Ok(0));
            while stack.told().is_some() {}
            let number = |i: usize| ACCEPTED_BIT | (1000 + i as u64);
            for i in 0..WAITING_MAX + 2 {
                let bytes = accepted_bytes(number(i), HOST, 41000 + i as u16);
                events(&stack, &[(idle.sock, EV_ACCEPTED, 500, &bytes)]);
            }
            assert_eq!(stack.told(), Some(close_message(number(WAITING_MAX))));
            assert_eq!(stack.told(), Some(close_message(number(WAITING_MAX + 1))));
            assert_eq!(stack.told(), None);
            drop(idle);
            while stack.told().is_some() {}
        }

        // The connection goes on after the listener: its port is the
        // listener's to give back, and the one nobody accepted is closed.
        drop(l);
        assert_eq!(stack.told(), Some(close_message(two)));
        assert_eq!(stack.told(), Some(close_message(sock)));
        assert_eq!(stack.told(), None);
        assert_eq!(Socket::bind(&other, any(8080)), Ok(0));
        events(&stack, &[(one, EV_BYTES, 0, b"mas")]);
        assert_eq!(read(&first, &mut buf), Ok(3));
        drop(first);
        assert_eq!(stack.told(), Some(close_message(one)));

        // A connection that arrives for a listener that is gone is closed
        // at once.
        let three = ACCEPTED_BIT | 3;
        events(
            &stack,
            &[(sock, EV_ACCEPTED, 500, &accepted_bytes(three, HOST, 40005))],
        );
        assert_eq!(stack.told(), Some(close_message(three)));
    }

    #[test]
    fn shutdown_closes_a_side_and_stops_a_listener() {
        let _lock = LOCK.lock();
        let stack = stack();
        let s = stream(&stack);
        assert_eq!(Socket::shutdown(&s, 3), Err(LxError::EINVAL));
        assert_eq!(Socket::shutdown(&s, 1), Err(LxError::ENOTCONN));
        drop(s);

        let (s, sock) = connected(&stack, 100);
        assert_eq!(Socket::shutdown(&s, 1), Ok(0));
        assert_eq!(stack.told(), Some(shutdown_message(sock)));
        assert_eq!(Socket::shutdown(&s, 1), Ok(0));
        assert_eq!(stack.told(), None, "said once");
        assert!(status(&s).write, "a write does not wait: it fails");
        assert_eq!(Socket::write(&s, b"x", None), Err(LxError::EPIPE));
        // The other side still arrives.
        events(&stack, &[(sock, EV_BYTES, 0, b"aun")]);
        let mut buf = [0u8; 8];
        assert_eq!(read(&s, &mut buf), Ok(3));
        assert!(!status(&s).hangup);
        events(&stack, &[(sock, EV_EOF, 0, &[])]);
        assert!(status(&s).hangup);

        // SHUT_RD: what was queued is read, then the end; what arrives
        // after is counted as read, so the peer is not held.
        let (s, sock) = connected(&stack, 100);
        events(&stack, &[(sock, EV_BYTES, 0, b"antes")]);
        assert_eq!(Socket::shutdown(&s, 0), Ok(0));
        assert_eq!(stack.told(), None);
        assert_eq!(read(&s, &mut buf), Ok(5));
        assert_eq!(read(&s, &mut buf), Ok(0));
        let piece = vec![0u8; 32 * 1024];
        for _ in 0..4 {
            events(&stack, &[(sock, EV_BYTES, 0, &piece)]);
        }
        // The five that were read, and the four pieces that were not.
        let counted = 5 + 4 * piece.len();
        assert_eq!(stack.told(), Some(window_message(sock, counted as u32)));
        assert_eq!(FileLike::readable_bytes(&s), Some(0));

        // A listener: SHUT_WR leaves it, SHUT_RD stops it.
        let l = stream(&stack);
        assert_eq!(Socket::listen(&l), Ok(0));
        let (_, sock) = parse_header(&stack.told().unwrap()).unwrap();
        let waiting = ACCEPTED_BIT | 9;
        events(
            &stack,
            &[(sock, EV_ACCEPTED, 1, &accepted_bytes(waiting, HOST, 40001))],
        );
        assert_eq!(Socket::shutdown(&l, 1), Ok(0));
        assert!(Socket::is_listening(&l));
        assert_eq!(stack.told(), None);
        assert_eq!(Socket::shutdown(&l, 0), Ok(0));
        assert!(!Socket::is_listening(&l));
        assert_eq!(stack.told(), Some(close_message(waiting)));
        assert_eq!(stack.told(), Some(close_message(sock)));
        assert_eq!(accept(&l).err(), Some(LxError::EINVAL));
        drop(l);
        assert_eq!(stack.told(), None, "the stack has nothing of it");
    }

    #[test]
    fn a_stack_that_dies_takes_its_connections_with_it() {
        let _lock = LOCK.lock();
        let stack = stack();
        let (open, sock) = connected(&stack, 100);
        events(&stack, &[(sock, EV_BYTES, 0, b"queda")]);
        let connecting = stream(&stack);
        assert_eq!(
            connect(&connecting, at(HOST, 80)),
            Err(LxError::EINPROGRESS)
        );
        let listening = stream(&stack);
        assert_eq!(Socket::listen(&listening), Ok(0));
        let Stack { link, end } = stack;
        drop(end);
        link.pump();
        assert!(link.is_dead());

        for s in [&open, &connecting, &listening] {
            let now = status(s);
            assert!(now.read && now.write && now.error);
        }
        let mut buf = [0u8; 8];
        assert_eq!(read(&open, &mut buf), Ok(5), "what had arrived is read");
        assert_eq!(read(&open, &mut buf), Err(LxError::ENETDOWN));
        assert_eq!(Socket::write(&open, b"x", None), Err(LxError::ENETDOWN));
        assert_eq!(connect(&connecting, at(HOST, 80)), Err(LxError::ENETDOWN));
        assert_eq!(accept(&listening).err(), Some(LxError::ENETDOWN));
        assert_eq!(
            ServedTcpSocket::new(link.clone()).err(),
            Some(LxError::ENETDOWN)
        );
    }

    #[test]
    fn a_connect_that_waits_is_woken_by_the_answer() {
        let _lock = LOCK.lock();
        let stack = stack();
        for answer in [EV_CONNECTED, EV_FAILED] {
            let s = Arc::new(ServedTcpSocket::new(stack.link.clone()).unwrap());
            let waiter = {
                let s = s.clone();
                std::thread::spawn(move || connect(&s, at(HOST, 80)))
            };
            // The CONNECT is the sign that the other thread is in. (What
            // comes before it is the close of the socket before.)
            let sock = loop {
                match stack.told().as_deref().and_then(parse_header) {
                    Some((OP_CONNECT, sock)) => break sock,
                    _ => std::thread::yield_now(),
                }
            };
            events(&stack, &[(sock, answer, WHY_REFUSED, &[])]);
            let expected = if answer == EV_CONNECTED {
                Ok(0)
            } else {
                Err(LxError::ECONNREFUSED)
            };
            assert_eq!(waiter.join().unwrap(), expected);
            if answer == EV_FAILED {
                assert_eq!(Socket::take_so_error(&*s), 0, "the connect took it");
            }
        }
    }
}
