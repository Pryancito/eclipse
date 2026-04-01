//! Implementación del Esquema PTY (Pseudo-Terminal)
//! Permite a un emulador de terminal y un shell comunicarse vía Maestro/Esclavo

use alloc::vec::Vec;
use alloc::string::String;
use alloc::collections::VecDeque;
use spin::Mutex;
use crate::scheme::{Scheme, error, Stat};
use alloc::sync::Arc;

pub struct PtyChannel {
    pub master_in: VecDeque<u8>, // Master reads from here, Slave writes to here
    pub slave_in: VecDeque<u8>,  // Slave reads from here, Master writes to here
    pub closed: bool,
    /// Dimensiones de la ventana (TIOCSWINSZ / TIOCGWINSZ)
    pub ws_rows: u16,
    pub ws_cols: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
    /// PID del proceso que tiene abierto el extremo esclavo (0 = no registrado).
    /// Se establece automáticamente en la primera apertura de "pty:slave/N".
    pub slave_pid: crate::process::ProcessId,
}

impl PtyChannel {
    pub fn new() -> Self {
        Self {
            master_in: VecDeque::with_capacity(4096),
            slave_in: VecDeque::with_capacity(4096),
            closed: false,
            ws_rows: 24,
            ws_cols: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
            slave_pid: 0,
        }
    }
}

pub struct PtyHandle {
    pub pair_id: usize,
    pub is_master: bool,
}

pub struct PtyScheme {
    channels: Mutex<Vec<Option<Arc<Mutex<PtyChannel>>>>>,
    handles: Mutex<Vec<Option<PtyHandle>>>,
}

impl PtyScheme {
    pub fn new() -> Self {
        Self {
            channels: Mutex::new(Vec::new()),
            handles: Mutex::new(Vec::new()),
        }
    }

    fn new_handle(&self, pair_id: usize, is_master: bool) -> usize {
        let mut handles = self.handles.lock();
        for (i, slot) in handles.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(PtyHandle { pair_id, is_master });
                return i;
            }
        }
        let id = handles.len();
        handles.push(Some(PtyHandle { pair_id, is_master }));
        id
    }
}

impl Scheme for PtyScheme {
    fn open(&self, path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        let path = path.trim_start_matches('/');
        if path == "master" {
            let mut channels = self.channels.lock();
            let mut pair_id = None;
            for (i, c) in channels.iter_mut().enumerate() {
                if c.is_none() {
                    *c = Some(Arc::new(Mutex::new(PtyChannel::new())));
                    pair_id = Some(i);
                    break;
                }
            }
            let pair_id = pair_id.unwrap_or_else(|| {
                let id = channels.len();
                channels.push(Some(Arc::new(Mutex::new(PtyChannel::new()))));
                id
            });
            return Ok(self.new_handle(pair_id, true));
        } else if path.starts_with("slave/") {
            let id_str = &path[6..];
            let pair_id = id_str.parse::<usize>().map_err(|_| error::ENOENT)?;
            let channels = self.channels.lock();
            if pair_id < channels.len() {
                if let Some(arc) = channels[pair_id].as_ref() {
                    // Registrar el PID del proceso que abre el esclavo
                    let caller_pid = crate::process::current_process_id().unwrap_or(0);
                    arc.lock().slave_pid = caller_pid;
                    return Ok(self.new_handle(pair_id, false));
                }
            }
            return Err(error::ENOENT);
        }
        Err(error::ENOENT)
    }

    fn read(&self, id: usize, buffer: &mut [u8]) -> Result<usize, usize> {
        if buffer.is_empty() { return Ok(0); }
        let handle = {
            let handles = self.handles.lock();
            let h = handles.get(id).and_then(|x| x.as_ref()).ok_or(error::EBADF)?;
            PtyHandle { pair_id: h.pair_id, is_master: h.is_master }
        };

        let channel_arc = {
            let channels = self.channels.lock();
            channels.get(handle.pair_id).and_then(|c| c.as_ref()).cloned().ok_or(error::EIO)?
        };

        loop {
            let mut channel = channel_arc.lock();
            let queue = if handle.is_master { &mut channel.master_in } else { &mut channel.slave_in };
            
            if !queue.is_empty() {
                let mut read = 0;
                while read < buffer.len() && !queue.is_empty() {
                    if let Some(byte) = queue.pop_front() {
                        buffer[read] = byte;
                        read += 1;
                    }
                }
                return Ok(read);
            } else if channel.closed {
                return Ok(0); // EOF
            }
            drop(channel);

            // Comprobar señales pendientes antes de bloquearse: devolver EINTR
            if let Some(pid) = crate::process::current_process_id() {
                if crate::process::get_pending_signals(pid) != 0 {
                    return Err(4); // EINTR = 4
                }
            }

            crate::scheduler::yield_cpu(); // Blocking read
        }
    }

    fn write(&self, id: usize, buffer: &[u8]) -> Result<usize, usize> {
        let handle = {
            let handles = self.handles.lock();
            let h = handles.get(id).and_then(|x| x.as_ref()).ok_or(error::EBADF)?;
            PtyHandle { pair_id: h.pair_id, is_master: h.is_master }
        };

        let channel_arc = {
            let channels = self.channels.lock();
            channels.get(handle.pair_id).and_then(|c| c.as_ref()).cloned().ok_or(error::EIO)?
        };

        let mut channel = channel_arc.lock();
        if channel.closed {
            return Err(error::EPIPE);
        }

        let queue = if handle.is_master { &mut channel.slave_in } else { &mut channel.master_in };
        for &byte in buffer {
            queue.push_back(byte);
        }
        
        Ok(buffer.len())
    }

    fn close(&self, id: usize) -> Result<usize, usize> {
        let handle = {
            let mut handles = self.handles.lock();
            if let Some(h) = handles.get_mut(id) {
                h.take().ok_or(error::EBADF)?
            } else {
                return Err(error::EBADF);
            }
        };

        let channel_arc = {
            let channels = self.channels.lock();
            channels.get(handle.pair_id).and_then(|c| c.as_ref()).cloned()
        };

        if let Some(arc) = channel_arc {
            // Signal EOF via closed flag
            arc.lock().closed = true;
        }

        Ok(0)
    }

    fn ioctl(&self, id: usize, request: usize, arg: usize) -> Result<usize, usize> {
        let handle = {
            let handles = self.handles.lock();
            let h = handles.get(id).and_then(|x| x.as_ref()).ok_or(error::EBADF)?;
            PtyHandle { pair_id: h.pair_id, is_master: h.is_master }
        };
        // Request 1: TIOCGPTN (Get PTY Number)
        if request == 1 {
            if arg != 0 {
                // Warning! The arg needs to be validated properly in a real kernel memory map check
                unsafe {
                    *(arg as *mut usize) = handle.pair_id;
                }
            }
            Ok(0)
        } else if request == 2 {
            // Request 2: FIONREAD (Get available bytes to read)
            let channel_arc = {
                let channels = self.channels.lock();
                channels.get(handle.pair_id).and_then(|c| c.as_ref()).cloned().ok_or(error::EIO)?
            };
            let channel = channel_arc.lock();
            let queue = if handle.is_master { &channel.master_in } else { &channel.slave_in };
            
            if arg != 0 {
                unsafe {
                    *(arg as *mut usize) = queue.len();
                }
            }
            Ok(queue.len())
        } else if request == 3 {
            // TIOCSWINSZ: establecer tamaño de ventana del terminal.
            // arg = puntero a [u16; 4] = [ws_rows, ws_cols, ws_xpixel, ws_ypixel]
            if arg == 0 {
                return Err(error::EINVAL);
            }
            let channel_arc = {
                let channels = self.channels.lock();
                channels.get(handle.pair_id).and_then(|c| c.as_ref()).cloned().ok_or(error::EIO)?
            };
            let mut channel = channel_arc.lock();
            let ptr = arg as *const u16;
            // SAFETY: el puntero viene de userspace; en un kernel real usaríamos
            // copy_from_user. Por ahora asumimos que el caller lo mapea correctamente.
            let slave_pid = unsafe {
                channel.ws_rows   = *ptr;
                channel.ws_cols   = *ptr.add(1);
                channel.ws_xpixel = *ptr.add(2);
                channel.ws_ypixel = *ptr.add(3);
                channel.slave_pid
            };
            // Enviar SIGWINCH (28) al proceso esclavo para que sepa que cambió el tamaño
            if slave_pid != 0 {
                crate::process::set_pending_signal(slave_pid, 28);
            }
            Ok(0)
        } else if request == 4 {
            // TIOCGWINSZ: leer tamaño de ventana del terminal.
            // arg = puntero a [u16; 4] = [ws_rows, ws_cols, ws_xpixel, ws_ypixel]
            if arg == 0 {
                return Err(error::EINVAL);
            }
            let channel_arc = {
                let channels = self.channels.lock();
                channels.get(handle.pair_id).and_then(|c| c.as_ref()).cloned().ok_or(error::EIO)?
            };
            let channel = channel_arc.lock();
            let ptr = arg as *mut u16;
            unsafe {
                *ptr          = channel.ws_rows;
                *ptr.add(1)   = channel.ws_cols;
                *ptr.add(2)   = channel.ws_xpixel;
                *ptr.add(3)   = channel.ws_ypixel;
            }
            Ok(0)
        } else {
            Err(error::ENOSYS)
        }
    }

    fn fstat(&self, _id: usize, stat: &mut Stat) -> Result<usize, usize> {
        stat.mode = 0o666 | 0x2000; // Character device
        stat.size = 0;
        Ok(0)
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize) -> Result<usize, usize> {
        Err(error::ESPIPE)
    }
}
