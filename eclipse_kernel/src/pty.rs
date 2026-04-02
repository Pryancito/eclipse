//! Implementación del Esquema PTY (Pseudo-Terminal)
//! Permite a un emulador de terminal y un shell comunicarse vía Maestro/Esclavo

use alloc::vec::Vec;
use alloc::string::String;
use alloc::collections::VecDeque;
use spin::Mutex;
use crate::scheme::{Scheme, error, Stat};
use alloc::sync::Arc;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Termios {
    pub c_iflag: u32,
    pub c_oflag: u32,
    pub c_cflag: u32,
    pub c_lflag: u32,
    pub c_line: u8,
    pub c_cc: [u8; 32],
    pub c_ispeed: u32,
    pub c_ospeed: u32,
}

impl Default for Termios {
    fn default() -> Self {
        let mut cc = [0u8; 32];
        // Standard defaults: VINTR=^C, VQUIT=^\, VERASE=^H, VKILL=^U, VEOF=^D, VEOL=0, VEOL2=0, VSTART=^Q, VSTOP=^S, VSUSP=^Z
        cc[0] = 3;  // VINTR
        cc[1] = 28; // VQUIT
        cc[2] = 8;  // VERASE
        cc[3] = 21; // VKILL
        cc[4] = 4;  // VEOF
        cc[8] = 17; // VSTART
        cc[9] = 19; // VSTOP
        cc[10] = 26; // VSUSP

        Self {
            c_iflag: 0x0500, // ICRNL | IXON
            c_oflag: 0x0005, // OPOST | ONLCR
            c_cflag: 0x00BF, // B38400 | CS8 | CREAD | HUPCL
            c_lflag: 0x8A3B, // ISIG | ICANON | ECHO | ECHOE | ECHOK | IEXTEN | ECHOCTL | ECHOKE
            c_line: 0,
            c_cc: cc,
            c_ispeed: 15, // B38400
            c_ospeed: 15, // B38400
        }
    }
}

pub struct PtyChannel {
    pub master_in: VecDeque<u8>, // Master reads from here, Slave writes to here
    pub slave_in: VecDeque<u8>,  // Slave reads from here, Master writes to here
    /// True when all master-side handles have been closed (slave reads get EOF).
    pub master_closed: bool,
    /// True when all slave-side handles have been closed (master reads get EOF).
    pub slave_closed: bool,
    /// Dimensiones de la ventana (TIOCSWINSZ / TIOCGWINSZ)
    pub ws_rows: u16,
    pub ws_cols: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
    /// PID del proceso que tiene abierto el extremo esclavo (0 = no registrado).
    /// Se establece automáticamente en la primera apertura de "pty:slave/N".
    pub slave_pid: crate::process::ProcessId,
    /// Terminal state (flags, control characters)
    pub termios: Termios,
}

impl PtyChannel {
    pub fn new() -> Self {
        Self {
            master_in: VecDeque::with_capacity(4096),
            slave_in: VecDeque::with_capacity(4096),
            master_closed: false,
            slave_closed: false,
            ws_rows: 24,
            ws_cols: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
            slave_pid: 0,
            termios: Termios::default(),
        }
    }
}

pub struct PtyHandle {
    pub pair_id: usize,
    pub is_master: bool,
    /// Reference count: starts at 1, incremented on dup(), decremented on close().
    /// The channel side is only marked closed when ref_count reaches 0.
    pub ref_count: usize,
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
                *slot = Some(PtyHandle { pair_id, is_master, ref_count: 1 });
                return i;
            }
        }
        let id = handles.len();
        handles.push(Some(PtyHandle { pair_id, is_master, ref_count: 1 }));
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
                    let caller_pid = crate::process::current_process_id().unwrap_or(0);
                    let mut ch = arc.lock();
                    ch.slave_pid = caller_pid;
                    // Reset slave_closed when a new slave handle is opened (e.g. shell respawn).
                    ch.slave_closed = false;
                    drop(ch);
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
            PtyHandle { pair_id: h.pair_id, is_master: h.is_master, ref_count: h.ref_count }
        };

        let channel_arc = {
            let channels = self.channels.lock();
            channels.get(handle.pair_id).and_then(|c| c.as_ref()).cloned().ok_or(error::EIO)?
        };

        loop {
            let mut channel = channel_arc.lock();
            let termios = channel.termios;
            // ICANON only applies to slave reads (line discipline on the app side).
            // The master (terminal emulator) must always read raw data immediately,
            // regardless of canonical mode, so that output without a trailing newline
            // (e.g. the shell prompt) is visible at once.
            let icanon = !handle.is_master && (termios.c_lflag & 0x0002) != 0; // ICANON=0x0002

            let queue = if handle.is_master { &mut channel.master_in } else { &mut channel.slave_in };

            if !queue.is_empty() {
                // Si ICANON está activo, solo leemos si hay un newline en la cola
                let mut has_newline = false;
                if icanon {
                    for &byte in queue.iter() {
                        if byte == b'\n' || byte == b'\r' {
                            has_newline = true;
                            break;
                        }
                    }
                }

                if !icanon || has_newline {
                    let mut read = 0;
                    while read < buffer.len() && !queue.is_empty() {
                        if let Some(byte) = queue.pop_front() {
                            buffer[read] = byte;
                            read += 1;
                            // En modo canónico, dejamos de leer tras el newline
                            if icanon && (byte == b'\n' || byte == b'\r') {
                                break;
                            }
                        }
                    }
                    return Ok(read);
                }
            }
            
            // EOF when the other side has fully closed
            let eof = if handle.is_master { channel.slave_closed } else { channel.master_closed };
            if eof { return Ok(0); }

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
            PtyHandle { pair_id: h.pair_id, is_master: h.is_master, ref_count: h.ref_count }
        };

        let channel_arc = {
            let channels = self.channels.lock();
            channels.get(handle.pair_id).and_then(|c| c.as_ref()).cloned().ok_or(error::EIO)?
        };

        let mut channel = channel_arc.lock();
        // EPIPE when the other side has fully closed
        let closed = if handle.is_master { channel.slave_closed } else { channel.master_closed };
        if closed {
            return Err(error::EPIPE);
        }

        let slave_pid = channel.slave_pid;
        let is_master = handle.is_master;
        let lflag = channel.termios.c_lflag;
        let oflag = channel.termios.c_oflag;
        let cc = channel.termios.c_cc;
        
        let mut written = 0;
        let mut i = 0;
        while i < buffer.len() {
            let byte = buffer[i];
            i += 1;

            if is_master {
                // Master writing to Slave (User typing)
                
                // 1. Check for signals (ISIG)
                if (lflag & 0x0001) != 0 { // ISIG
                    let signo = if byte == cc[0] { // VINTR
                        Some(2) // SIGINT
                    } else if byte == cc[1] { // VQUIT
                        Some(3) // SIGQUIT
                    } else if byte == cc[10] { // VSUSP
                        Some(20) // SIGTSTP
                    } else {
                        None
                    };

                    if let Some(s) = signo {
                        if channel.slave_pid != 0 {
                            crate::process::set_pending_signal(channel.slave_pid, s as u8);
                        }
                        written += 1;
                        continue;
                    }
                }

                // 2. Echo (ECHO)
                if (lflag & 0x0008) != 0 { // ECHO
                    channel.master_in.push_back(byte);
                }

                channel.slave_in.push_back(byte);
                written += 1;
            } else {
                // Slave writing to Master (App output)
                
                // 3. Output Processing (OPOST)
                if (oflag & 0x0001) != 0 { // OPOST
                    if byte == b'\n' && (oflag & 0x0004) != 0 { // ONLCR
                        channel.master_in.push_back(b'\r');
                        channel.master_in.push_back(b'\n');
                        written += 1;
                        continue;
                    }
                }

                channel.master_in.push_back(byte);
                written += 1;
            }
        }
        
        Ok(written)
    }

    fn close(&self, id: usize) -> Result<usize, usize> {
        // Decrement ref_count; only remove the handle and mark channel when it reaches 0.
        let (pair_id, is_master, last_ref) = {
            let mut handles = self.handles.lock();
            let h = handles.get_mut(id).and_then(|x| x.as_mut()).ok_or(error::EBADF)?;
            h.ref_count = h.ref_count.saturating_sub(1);
            let last = h.ref_count == 0;
            let pair_id = h.pair_id;
            let is_master = h.is_master;
            if last {
                handles[id] = None;
            }
            (pair_id, is_master, last)
        };

        if last_ref {
            let channel_arc = {
                let channels = self.channels.lock();
                channels.get(pair_id).and_then(|c| c.as_ref()).cloned()
            };
            if let Some(arc) = channel_arc {
                let mut channel = arc.lock();
                if is_master {
                    channel.master_closed = true;
                    // Send SIGHUP (1) to the foreground process group (slave_pid)
                    let slave_pid = channel.slave_pid;
                    if slave_pid != 0 {
                        crate::process::set_pending_signal(slave_pid, 1);
                    }
                } else {
                    channel.slave_closed = true;
                }
            }
        }

        Ok(0)
    }

    fn ioctl(&self, id: usize, request: usize, arg: usize) -> Result<usize, usize> {
        let handle = {
            let handles = self.handles.lock();
            let h = handles.get(id).and_then(|x| x.as_ref()).ok_or(error::EBADF)?;
            PtyHandle { pair_id: h.pair_id, is_master: h.is_master, ref_count: h.ref_count }
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
        } else if request == 5 {
            // TIOCSPGRP: establecer PID del proceso en primer plano.
            if arg == 0 { return Err(error::EINVAL); }
            let channel_arc = {
                let channels = self.channels.lock();
                channels.get(handle.pair_id).and_then(|c| c.as_ref()).cloned().ok_or(error::EIO)?
            };
            let mut channel = channel_arc.lock();
            let pid_ptr = arg as *const u32;
            unsafe {
                channel.slave_pid = *pid_ptr;
            }
            Ok(0)
        } else if request == 6 {
            // TIOCGPGRP: obtener PID del proceso en primer plano.
            if arg == 0 { return Err(error::EINVAL); }
            let channel_arc = {
                let channels = self.channels.lock();
                channels.get(handle.pair_id).and_then(|c| c.as_ref()).cloned().ok_or(error::EIO)?
            };
            let channel = channel_arc.lock();
            let pid_ptr = arg as *mut u32;
            unsafe {
                *pid_ptr = channel.slave_pid;
            }
            Ok(0)
        } else if request == 0x5401 {
            // TCGETS: Read termios structure.
            if arg == 0 { return Err(error::EINVAL); }
            let channel_arc = {
                let channels = self.channels.lock();
                channels.get(handle.pair_id).and_then(|c| c.as_ref()).cloned().ok_or(error::EIO)?
            };
            let channel = channel_arc.lock();
            let ptr = arg as *mut Termios;
            unsafe {
                *ptr = channel.termios;
            }
            Ok(0)
        } else if request == 0x5402 {
            // TCSETS: Write termios structure.
            if arg == 0 { return Err(error::EINVAL); }
            let channel_arc = {
                let channels = self.channels.lock();
                channels.get(handle.pair_id).and_then(|c| c.as_ref()).cloned().ok_or(error::EIO)?
            };
            let mut channel = channel_arc.lock();
            let ptr = arg as *const Termios;
            unsafe {
                channel.termios = *ptr;
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

    fn dup(&self, id: usize) -> Result<usize, usize> {
        let mut handles = self.handles.lock();
        if let Some(Some(h)) = handles.get_mut(id) {
            h.ref_count += 1;
            Ok(0)
        } else {
            Err(error::EBADF)
        }
    }
}
