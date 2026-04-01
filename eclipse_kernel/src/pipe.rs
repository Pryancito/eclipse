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
const PIPE_BUF_CAP: usize = 65536;

// ---------------------------------------------------------------------------
// Estructuras internas
// ---------------------------------------------------------------------------

struct PipeChannel {
    buffer:     VecDeque<u8>,
    write_ends: usize,   // cuántos extremos de escritura siguen abiertos
    read_ends:  usize,   // cuántos extremos de lectura siguen abiertos
}

impl PipeChannel {
    fn new() -> Self {
        Self {
            buffer:     VecDeque::with_capacity(4096),
            write_ends: 1,
            read_ends:  1,
        }
    }
}

pub struct PipeHandle {
    pub channel_id: usize,
    pub is_write:   bool,
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
                *slot = Some(Arc::new(Mutex::new(PipeChannel::new())));
                return i;
            }
        }
        let id = channels.len();
        channels.push(Some(Arc::new(Mutex::new(PipeChannel::new()))));
        id
    }

    fn alloc_handle(&self, channel_id: usize, is_write: bool) -> usize {
        let mut handles = self.handles.lock();
        for (i, slot) in handles.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(PipeHandle { channel_id, is_write });
                return i;
            }
        }
        let id = handles.len();
        handles.push(Some(PipeHandle { channel_id, is_write }));
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

    fn read(&self, id: usize, buffer: &mut [u8]) -> Result<usize, usize> {
        if buffer.is_empty() {
            return Ok(0);
        }

        let handle = {
            let handles = self.handles.lock();
            let h = handles.get(id).and_then(|x| x.as_ref()).ok_or(error::EBADF)?;
            if h.is_write {
                return Err(error::EBADF);
            }
            (h.channel_id, h.is_write)
        };

        let channel_arc = self.get_channel(handle.0).ok_or(error::EIO)?;

        loop {
            {
                let mut ch = channel_arc.lock();
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

    fn write(&self, id: usize, buffer: &[u8]) -> Result<usize, usize> {
        if buffer.is_empty() {
            return Ok(0);
        }

        let handle = {
            let handles = self.handles.lock();
            let h = handles.get(id).and_then(|x| x.as_ref()).ok_or(error::EBADF)?;
            if !h.is_write {
                return Err(error::EBADF);
            }
            (h.channel_id, h.is_write)
        };

        let channel_arc = self.get_channel(handle.0).ok_or(error::EIO)?;
        let mut ch = channel_arc.lock();

        if ch.read_ends == 0 {
            return Err(error::EPIPE);
        }

        // Si el buffer está lleno, descartar bytes más antiguos para no bloquear
        // (comportamiento de desbordamiento; en un sistema real bloquearía)
        let available = PIPE_BUF_CAP.saturating_sub(ch.buffer.len());
        let to_write = buffer.len().min(available);
        for &b in &buffer[..to_write] {
            ch.buffer.push_back(b);
        }

        Ok(to_write)
    }

    fn close(&self, id: usize) -> Result<usize, usize> {
        let handle = {
            let mut handles = self.handles.lock();
            handles.get_mut(id).and_then(|h| h.take()).ok_or(error::EBADF)?
        };

        if let Some(arc) = self.get_channel(handle.channel_id) {
            let mut ch = arc.lock();
            if handle.is_write {
                ch.write_ends = ch.write_ends.saturating_sub(1);
            } else {
                ch.read_ends = ch.read_ends.saturating_sub(1);
            }
            // Si ningún extremo sigue abierto, podemos liberar el canal
            if ch.write_ends == 0 && ch.read_ends == 0 {
                drop(ch); // soltar el lock antes de mutar channels
                let mut channels = self.channels.lock();
                if handle.channel_id < channels.len() {
                    channels[handle.channel_id] = None;
                }
            }
        }

        Ok(0)
    }

    fn fstat(&self, _id: usize, stat: &mut Stat) -> Result<usize, usize> {
        stat.mode = 0o600 | 0x1000; // FIFO, solo owner rw
        stat.size = 0;
        Ok(0)
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize) -> Result<usize, usize> {
        Err(error::ESPIPE)
    }
}
