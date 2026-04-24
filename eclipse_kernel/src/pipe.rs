//! Esquema Pipe — par de descriptores anónimos (read-end / write-end)
//!
//! Crea canales unidireccionales tipo POSIX: el escritor empuja bytes al buffer
//! y el lector los consume.  Cuando todos los escritores cierran, el lector ve EOF.
//! Cuando todos los lectores cierran y el escritor intenta escribir, recibe EPIPE.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;
use crate::scheme::{Scheme, error, Stat};

/// Capacidad máxima del buffer interno de una pipe (64 KiB).
pub const PIPE_BUF_CAP: usize = 65536;

// ---------------------------------------------------------------------------
// Estructuras internas
// ---------------------------------------------------------------------------

struct PipeChannel {
    buffer:     VecDeque<u8>,
    write_ends: usize,   // cuántos extremos de escritura siguen abiertos
    read_ends:  usize,   // cuántos extremos de lectura siguen abiertos
    pub creator_uid: u32,
    pub creator_gid: u32,
}

impl PipeChannel {
    fn new() -> Self {
        Self {
            buffer:     VecDeque::with_capacity(4096),
            write_ends: 1,
            read_ends:  1,
            creator_uid: 0,
            creator_gid: 0,
        }
    }
}

pub struct PipeHandle {
    pub channel_id: usize,
    pub is_write:   bool,
    /// Number of open references to this handle slot (incremented by dup, decremented by close).
    /// The channel side counter is only decremented when this reaches 0.
    pub ref_count:  usize,
    /// When true, reads/writes return EAGAIN instead of blocking when the buffer is empty/full.
    pub nonblock:   bool,
}

// ---------------------------------------------------------------------------
// Esquema global
// ---------------------------------------------------------------------------

pub struct PipeScheme {
    channels: Mutex<Vec<Option<Arc<Mutex<PipeChannel>>>>>,
    handles:  Mutex<Vec<Option<PipeHandle>>>,
}

// SAFETY: PipeScheme es completamente thread-safe vía sus Mutex internos.
unsafe impl Send for PipeScheme {}
unsafe impl Sync for PipeScheme {}

impl PipeScheme {
    pub const fn new() -> Self {
        Self {
            channels: Mutex::new(Vec::new()),
            handles:  Mutex::new(Vec::new()),
        }
    }

    fn alloc_channel(&self) -> usize {
        let mut channels = self.channels.lock();
        // Reutilizar slot vacío si lo hay
        for (i, slot) in channels.iter_mut().enumerate() {
            if slot.is_none() {
                let mut ch = PipeChannel::new();
                if let Some(pid) = crate::process::current_process_id() {
                    if let Some(p) = crate::process::get_process(pid) {
                        ch.creator_uid = p.uid;
                        ch.creator_gid = p.gid;
                    }
                }
                *slot = Some(Arc::new(Mutex::new(ch)));
                return i;
            }
        }
        let id = channels.len();
        let mut ch = PipeChannel::new();
        if let Some(pid) = crate::process::current_process_id() {
            if let Some(p) = crate::process::get_process(pid) {
                ch.creator_uid = p.uid;
                ch.creator_gid = p.gid;
            }
        }
        channels.push(Some(Arc::new(Mutex::new(ch))));
        id
    }

    fn alloc_handle(&self, channel_id: usize, is_write: bool) -> usize {
        let mut handles = self.handles.lock();
        for (i, slot) in handles.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(PipeHandle { channel_id, is_write, ref_count: 1, nonblock: false });
                return i;
            }
        }
        let id = handles.len();
        handles.push(Some(PipeHandle { channel_id, is_write, ref_count: 1, nonblock: false }));
        id
    }

    /// Crea una nueva pipe. Devuelve (handle_lectura, handle_escritura).
    /// Estos handles son resource_id dentro del scheme "pipe".
    pub fn new_pipe(&self) -> (usize, usize) {
        let cid = self.alloc_channel();
        let rh  = self.alloc_handle(cid, false);
        let wh  = self.alloc_handle(cid, true);
        (rh, wh)
    }

    fn get_channel(&self, channel_id: usize) -> Option<Arc<Mutex<PipeChannel>>> {
        let channels = self.channels.lock();
        channels.get(channel_id).and_then(|c| c.as_ref()).cloned()
    }

    /// Set or clear the O_NONBLOCK flag for a pipe handle.
    pub fn set_nonblock(&self, id: usize, nonblock: bool) {
        let mut handles = self.handles.lock();
        if let Some(Some(h)) = handles.get_mut(id) {
            h.nonblock = nonblock;
        }
    }

    /// Query readiness of a pipe handle (for poll/select).
    /// Returns a bitmask of `scheme::event::POLLIN` / `scheme::event::POLLOUT`.
    pub fn poll_pipe(&self, id: usize, events: usize) -> Result<usize, usize> {
        let (channel_id, is_write) = {
            let handles = self.handles.lock();
            let h = handles.get(id).and_then(|x| x.as_ref()).ok_or(error::EBADF)?;
            (h.channel_id, h.is_write)
        };
        let channel_arc = self.get_channel(channel_id).ok_or(error::EIO)?;
        let ch = channel_arc.lock();
        let mut ready = 0usize;
        if !is_write {
            // Read end: POLLIN when buffer has data or all writers have closed (EOF).
            if (events & crate::scheme::event::POLLIN) != 0 {
                if !ch.buffer.is_empty() || ch.write_ends == 0 {
                    ready |= crate::scheme::event::POLLIN;
                }
            }
        } else {
            // Write end: POLLOUT when buffer has space and at least one reader is open.
            if (events & crate::scheme::event::POLLOUT) != 0 {
                if ch.buffer.len() < PIPE_BUF_CAP && ch.read_ends > 0 {
                    ready |= crate::scheme::event::POLLOUT;
                }
            }
        }
        Ok(ready)
    }

    pub fn get_credentials(&self, id: usize) -> Result<(u32, u32), usize> {
        let channel_id = {
            let handles = self.handles.lock();
            let h = handles.get(id).and_then(|x| x.as_ref()).ok_or(error::EBADF)?;
            h.channel_id
        };
        let channel_arc = self.get_channel(channel_id).ok_or(error::EIO)?;
        let ch = channel_arc.lock();
        Ok((ch.creator_uid, ch.creator_gid))
    }
}

/// Singleton global del scheme de pipes (usado directamente por sys_pipe).
pub static PIPE_SCHEME: PipeScheme = PipeScheme::new();

// ---------------------------------------------------------------------------
// Implementación del trait Scheme
// ---------------------------------------------------------------------------

impl Scheme for PipeScheme {
    fn open(&self, _path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        // Las pipes se crean exclusivamente mediante sys_pipe, no con open().
        Err(error::ENOSYS)
    }

    fn read(&self, id: usize, buffer: &mut [u8], _offset: u64) -> Result<usize, usize> {
        if buffer.is_empty() {
            return Ok(0);
        }

        let (channel_id, nonblock) = {
            let handles = self.handles.lock();
            let h = handles.get(id).and_then(|x| x.as_ref()).ok_or(error::EBADF)?;
            if h.is_write {
                return Err(error::EBADF);
            }
            (h.channel_id, h.nonblock)
        };

        let channel_arc = self.get_channel(channel_id).ok_or(error::EIO)?;

        loop {
            {
                let mut ch = channel_arc.lock();

                if ch.read_ends == 0 {
                    if let Some(pid) = crate::process::current_process_id() {
                        crate::process::set_pending_signal(pid, 13); // SIGPIPE
                    }
                    return Err(error::EPIPE);
                }
                if !ch.buffer.is_empty() {
                    let mut n = 0;
                    while n < buffer.len() {
                        match ch.buffer.pop_front() {
                            Some(b) => { buffer[n] = b; n += 1; }
                            None    => break,
                        }
                    }
                    return Ok(n);
                }
                if ch.write_ends == 0 {
                    return Ok(0); // EOF: todos los escritores cerraron
                }
                // If non-blocking, return EAGAIN immediately instead of blocking
                if nonblock {
                    return Err(error::EAGAIN);
                }
            }

            // Comprobar señales pendientes antes de bloquearse
            if let Some(pid) = crate::process::current_process_id() {
                if crate::process::get_pending_signals(pid) != 0 {
                    return Err(4); // EINTR
                }
            }

            crate::scheduler::yield_cpu();
        }
    }

    fn write(&self, id: usize, buffer: &[u8], _offset: u64) -> Result<usize, usize> {
        if buffer.is_empty() {
            return Ok(0);
        }

        let (channel_id, nonblock) = {
            let handles = self.handles.lock();
            let h = handles.get(id).and_then(|x| x.as_ref()).ok_or(error::EBADF)?;
            if !h.is_write {
                return Err(error::EBADF);
            }
            (h.channel_id, h.nonblock)
        };

        let channel_arc = self.get_channel(channel_id).ok_or(error::EIO)?;

        loop {
            {
                let mut ch = channel_arc.lock();

                if ch.read_ends == 0 {
                    if let Some(pid) = crate::process::current_process_id() {
                        crate::process::set_pending_signal(pid, 13); // SIGPIPE
                    }
                    return Err(error::EPIPE);
                }


                let available = PIPE_BUF_CAP.saturating_sub(ch.buffer.len());
                if available > 0 {
                    let to_write = buffer.len().min(available);
                    for &b in &buffer[..to_write] {
                        ch.buffer.push_back(b);
                    }
                    return Ok(to_write);
                }

                // Buffer full: non-blocking returns EAGAIN immediately.
                if nonblock {
                    return Err(error::EAGAIN);
                }
                // Blocking: release the lock and yield below.
            }

            // Check for pending signals before blocking.
            if let Some(pid) = crate::process::current_process_id() {
                if crate::process::get_pending_signals(pid) != 0 {
                    return Err(4); // EINTR
                }
            }

            crate::scheduler::yield_cpu();
        }
    }

    fn close(&self, id: usize) -> Result<usize, usize> {
        // Decrement ref_count; only release the channel end when it reaches 0.
        let (channel_id, is_write, last_ref) = {
            let mut handles = self.handles.lock();
            let h = handles.get_mut(id).and_then(|x| x.as_mut()).ok_or(error::EBADF)?;
            h.ref_count = h.ref_count.saturating_sub(1);
            let last = h.ref_count == 0;
            let channel_id = h.channel_id;
            let is_write = h.is_write;
            if last {
                handles[id] = None;
            }
            (channel_id, is_write, last)
        };

        if last_ref {
            if let Some(arc) = self.get_channel(channel_id) {
                let mut ch = arc.lock();
                if is_write {
                    ch.write_ends = ch.write_ends.saturating_sub(1);
                } else {
                    ch.read_ends = ch.read_ends.saturating_sub(1);
                }
                // Si ningún extremo sigue abierto, podemos liberar el canal
                if ch.write_ends == 0 && ch.read_ends == 0 {
                    drop(ch); // soltar el lock antes de mutar channels
                    let mut channels = self.channels.lock();
                    if channel_id < channels.len() {
                        channels[channel_id] = None;
                    }
                }
            }
        }

        Ok(0)
    }

    fn dup(&self, id: usize) -> Result<usize, usize> {
        let mut handles = self.handles.lock();
        let h = handles.get_mut(id).and_then(|x| x.as_mut()).ok_or(error::EBADF)?;
        // Only increment the per-handle ref_count.  write_ends / read_ends on
        // the channel count distinct open handle *slots*, not individual
        // references.  They are decremented only when the last reference to a
        // slot disappears (ref_count → 0 in close).  Incrementing them here
        // would cause the counter to permanently exceed the true number of
        // open handles, preventing readers from ever seeing EOF.
        h.ref_count += 1;
        Ok(0)
    }

    fn fstat(&self, _id: usize, stat: &mut Stat) -> Result<usize, usize> {
        stat.mode = 0o600 | 0x1000; // FIFO, solo owner rw
        stat.size = 0;
        Ok(0)
    }

    fn poll(&self, id: usize, events: usize) -> Result<usize, usize> {
        self.poll_pipe(id, events)
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize, _current_offset: u64) -> Result<usize, usize> {
        Err(error::ESPIPE)
    }
}
