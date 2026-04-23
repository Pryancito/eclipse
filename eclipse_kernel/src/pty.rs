//! Implementación del Esquema PTY (Pseudo-Terminal)
//! Permite a un emulador de terminal y un shell comunicarse vía Maestro/Esclavo

use alloc::vec::Vec;
use alloc::string::String;
use alloc::collections::VecDeque;
use spin::Mutex;
use crate::scheme::{Scheme, error, Stat};
use crate::serial;
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
        // Standard defaults: VINTR=^C, VQUIT=^\, VERASE=DEL, VKILL=^U, VEOF=^D, VEOL=0, VEOL2=0, VSTART=^Q, VSTOP=^S, VSUSP=^Z
        cc[0] = 3;   // VINTR
        cc[1] = 28;  // VQUIT
        cc[2] = 127; // VERASE = DEL (0x7f) — matches what terminal emulators send for Backspace
        cc[3] = 21;  // VKILL
        cc[4] = 4;   // VEOF
        cc[8] = 17;  // VSTART
        cc[9] = 19;  // VSTOP
        cc[10] = 26; // VSUSP

        Self {
            c_iflag: 0x0500, // ICRNL | IXON
            c_oflag: 0x0005, // OPOST | ONLCR
            c_cflag: 0x00BF, // B38400 | CS8 | CREAD | HUPCL
            // Raw mode: no canonical line buffering (ICANON), no automatic echo (ECHO).
            // Terminal emulators like xterm open PTYs in raw mode so that the shell
            // (or any other app) can handle each byte as it arrives and manage its own
            // echoing and line editing.  With ICANON=ON the slave read() would block
            // until a newline, meaning the shell's readline() couldn't process
            // individual keystrokes (backspace, arrows…) until Enter was pressed.
            // With ECHO=ON the PTY would echo every typed byte immediately AND the
            // shell would echo it again after processing → double echo after Enter.
            c_lflag: 0x0000, // raw: no ISIG, no ICANON, no ECHO
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
    pub slave_pid: u32,
    pub foreground_pgrp: u32,
    /// Terminal state (flags, control characters)
    pub termios: Termios,
    /// Reference counts for master and slave sides
    pub master_open_count: usize,
    pub slave_open_count: usize,
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
            foreground_pgrp: 0,
            termios: Termios::default(),
            master_open_count: 0,
            slave_open_count: 0,
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
            // Update channel counters
            if let Some(arc) = &channels[pair_id] {
                arc.lock().master_open_count += 1;
            }
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
                    ch.slave_open_count += 1;
                    drop(ch);
                    return Ok(self.new_handle(pair_id, false));
                }
            }
            return Err(error::ENOENT);
        }
        Err(error::ENOENT)
    }

    fn read(&self, id: usize, buffer: &mut [u8], _offset: u64) -> Result<usize, usize> {
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
            if eof { 
                serial::serial_printf(format_args!("[PTY] read() returned EOF for pair {} (is_master={}, master_closed={}, slave_closed={})\n", 
                    handle.pair_id, handle.is_master, channel.master_closed, channel.slave_closed));
                return Ok(0); 
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

    fn write(&self, id: usize, buffer: &[u8], _offset: u64) -> Result<usize, usize> {
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
                    channel.master_open_count = channel.master_open_count.saturating_sub(1);
                    if channel.master_open_count == 0 {
                        channel.master_closed = true;
                        // Send SIGHUP (1) to the foreground process group (slave_pid)
                        let slave_pid = channel.slave_pid;
                        if slave_pid != 0 {
                            crate::process::set_pending_signal(slave_pid, 1);
                        }
                    }
                } else {
                    channel.slave_open_count = channel.slave_open_count.saturating_sub(1);
                    if channel.slave_open_count == 0 {
                        channel.slave_closed = true;
                    }
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
        // Map Linux-standard ioctl numbers to internal ones
        let request = match request {
            0x5413 => 4, // TIOCGWINSZ
            0x5414 => 3, // TIOCSWINSZ
            0x540F => 6, // TIOCGPGRP
            0x5410 => 5, // TIOCSPGRP
            0x541B => 2, // FIONREAD
            0x5403 | 0x5404 => 0x5402, // TCSETSW/F -> TCSETS
            0x540E => 7, // TIOCSCTTY
            _ => request,
        };

        // Request 1: TIOCGPTN (Get PTY Number)
        if request == 1 {
            if arg == 0 || !crate::syscalls::is_user_pointer(arg as u64, core::mem::size_of::<usize>() as u64) {
                return Err(error::EINVAL);
            }
            unsafe {
                *(arg as *mut usize) = handle.pair_id;
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
            let len = queue.len();

            if arg != 0 {
                if !crate::syscalls::is_user_pointer(arg as u64, core::mem::size_of::<usize>() as u64) {
                    return Err(error::EFAULT);
                }
                unsafe {
                    *(arg as *mut usize) = len;
                }
            }
            Ok(len)
        } else if request == 3 {
            // TIOCSWINSZ: establecer tamaño de ventana del terminal.
            // arg = puntero a [u16; 4] = [ws_rows, ws_cols, ws_xpixel, ws_ypixel]
            if arg == 0 || !crate::syscalls::is_user_pointer(arg as u64, 8) {
                return Err(error::EINVAL);
            }
            let channel_arc = {
                let channels = self.channels.lock();
                channels.get(handle.pair_id).and_then(|c| c.as_ref()).cloned().ok_or(error::EIO)?
            };
            let mut channel = channel_arc.lock();
            let ptr = arg as *const u16;
            let slave_pid = unsafe {
                channel.ws_rows   = ptr.read_unaligned();
                channel.ws_cols   = ptr.add(1).read_unaligned();
                channel.ws_xpixel = ptr.add(2).read_unaligned();
                channel.ws_ypixel = ptr.add(3).read_unaligned();
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
            if arg == 0 || !crate::syscalls::is_user_pointer(arg as u64, 8) {
                return Err(error::EINVAL);
            }
            let channel_arc = {
                let channels = self.channels.lock();
                channels.get(handle.pair_id).and_then(|c| c.as_ref()).cloned().ok_or(error::EIO)?
            };
            let channel = channel_arc.lock();
            let ptr = arg as *mut u16;
            unsafe {
                ptr.write_unaligned(channel.ws_rows);
                ptr.add(1).write_unaligned(channel.ws_cols);
                ptr.add(2).write_unaligned(channel.ws_xpixel);
                ptr.add(3).write_unaligned(channel.ws_ypixel);
            }
            Ok(0)
        } else if request == 5 || request == 0x5410 { // TIOCSPGRP
            // TIOCSPGRP: establecer PID del proceso en primer plano.
            if arg == 0 || !crate::syscalls::is_user_pointer(arg as u64, 4) { return Err(error::EINVAL); }
            let channel_arc = {
                let channels = self.channels.lock();
                channels.get(handle.pair_id).and_then(|c| c.as_ref()).cloned().ok_or(error::EIO)?
            };
            let mut channel = channel_arc.lock();
            let pid_ptr = arg as *const u32;
            unsafe {
                channel.foreground_pgrp = pid_ptr.read_unaligned();
                serial::serial_printf(format_args!("[PTY] TIOCSPGRP: foreground_pgrp set to {}\n", channel.foreground_pgrp));
            }
            Ok(0)
        } else if request == 6 || request == 0x540F { // TIOCGPGRP
            // TIOCGPGRP: obtener PID del proceso en primer plano.
            if arg == 0 || !crate::syscalls::is_user_pointer(arg as u64, 4) { return Err(error::EINVAL); }
            let channel_arc = {
                let channels = self.channels.lock();
                channels.get(handle.pair_id).and_then(|c| c.as_ref()).cloned().ok_or(error::EIO)?
            };
            let channel = channel_arc.lock();
            let pid_ptr = arg as *mut u32;
            unsafe {
                pid_ptr.write_unaligned(channel.foreground_pgrp);
            }
            Ok(0)
        } else if request == 0x5401 {
            // TCGETS: Read termios structure.
            if arg == 0 || !crate::syscalls::is_user_pointer(arg as u64, core::mem::size_of::<Termios>() as u64) {
                return Err(error::EINVAL);
            }
            let channel_arc = {
                let channels = self.channels.lock();
                channels.get(handle.pair_id).and_then(|c| c.as_ref()).cloned().ok_or(error::EIO)?
            };
            let channel = channel_arc.lock();
            let ptr = arg as *mut Termios;
            unsafe {
                ptr.write_unaligned(channel.termios);
            }
            Ok(0)
        } else if request == 0x5402 {
            // TCSETS: Write termios structure.
            if arg == 0 || !crate::syscalls::is_user_pointer(arg as u64, core::mem::size_of::<Termios>() as u64) {
                return Err(error::EINVAL);
            }
            let channel_arc = {
                let channels = self.channels.lock();
                channels.get(handle.pair_id).and_then(|c| c.as_ref()).cloned().ok_or(error::EIO)?
            };
            let mut channel = channel_arc.lock();
            let ptr = arg as *const Termios;
            unsafe {
                channel.termios = ptr.read_unaligned();
            }
            Ok(0)
        } else if request == 7 { // TIOCSCTTY
            // Set controlling terminal: associate the PTY slave with the calling process's session.
            // arg == 0: steal ctty if needed; arg == 1: fail if already has ctty.
            // For now: record the calling process as the PTY's controlling process.
            let current_pid = crate::process::current_process_id().unwrap_or(0);
            if !handle.is_master && current_pid != 0 {
                let channel_arc = {
                    let channels = self.channels.lock();
                    channels.get(handle.pair_id).and_then(|c| c.as_ref()).cloned().ok_or(error::EIO)?
                };
                let mut channel = channel_arc.lock();
                channel.slave_pid = current_pid;
                channel.foreground_pgrp = current_pid;
                serial::serial_printf(format_args!("[PTY] TIOCSCTTY: pid={} is now ctty owner\n", current_pid));
            }
            Ok(0)
        } else {
            Err(error::ENOSYS)
        }
    }

    fn fstat(&self, _id: usize, stat: &mut Stat) -> Result<usize, usize> {
        stat.mode = 0o620 | 0x2000; // Character device, rw for user, w for group
        stat.size = 0;
        Ok(0)
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize, _current_offset: u64) -> Result<usize, usize> {
        Err(error::ESPIPE)
    }

    fn dup(&self, id: usize) -> Result<usize, usize> {
        let mut handles = self.handles.lock();
        if let Some(Some(h)) = handles.get_mut(id) {
            h.ref_count += 1;
            // Also increment the channel's side counter
            let is_master = h.is_master;
            let pair_id = h.pair_id;
            drop(handles);
            
            let channels = self.channels.lock();
            if let Some(Some(arc)) = channels.get(pair_id) {
                let mut channel = arc.lock();
                if is_master {
                    channel.master_open_count += 1;
                } else {
                    channel.slave_open_count += 1;
                }
            }
            Ok(id)
        } else {
            Err(error::EBADF)
        }
    }
}
