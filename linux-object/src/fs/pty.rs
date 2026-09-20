//! Pseudo-terminals (PTYs).
//!
//! A PTY is a bidirectional pipe with a TTY line discipline in the middle. The
//! *master* end is handed out by opening `/dev/ptmx`; the matching *slave*
//! appears as `/dev/pts/N` and behaves like a real terminal for the program
//! running on it (a shell). Terminal emulators such as xterm drive the master:
//! they write the user's keystrokes into it and read back the program's output
//! (plus the echoed input) to paint the window.
//!
//! Data flow:
//! - master write → input line discipline (cooking, signals, echo) → slave read
//! - slave write  → output processing (ONLCR) → master read
//! - echo produced while cooking input is written to the *master* read side, so
//!   the emulator shows what was typed.

use super::ioctl::*;
use crate::signal::Signal;
use crate::sync::{Event, EventBus};
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use core::any::Any;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use core::task::{Context, Poll};
use kernel_hal::console::ConsoleWinSize;
use lazy_static::lazy_static;
use lock::Mutex;
use rcore_fs::vfs::*;

// termios c_iflag bits
const IGNCR: u32 = 0x0080;
const ICRNL: u32 = 0x0100;
const INLCR: u32 = 0x0040;
const IXON: u32 = 0x0400;
const IXANY: u32 = 0x0800;
// termios c_oflag bits
const OPOST: u32 = 0x0001;
const ONLCR: u32 = 0x0004;
// termios c_lflag bits
const ISIG: u32 = 0x0001;
const ICANON: u32 = 0x0002;
const ECHO: u32 = 0x0008;
const ECHOE: u32 = 0x0010;
const NOFLSH: u32 = 0x0080;
const ECHOCTL: u32 = 0x0200;
const IEXTEN: u32 = 0x8000;
// c_cc indices
const VINTR: usize = 0;
const VQUIT: usize = 1;
const VERASE: usize = 2;
const VKILL: usize = 3;
const VEOF: usize = 4;
const VSTART: usize = 8;
const VSTOP: usize = 9;
const VSUSP: usize = 10;
const VEOL: usize = 11;
const VREPRINT: usize = 12;
const VDISCARD: usize = 13;
const VWERASE: usize = 14;
const VLNEXT: usize = 15;
const VEOL2: usize = 16;

/// Mutable, lock-protected state shared by a master/slave pair.
struct PtyInner {
    /// Bytes available to the slave's `read` (cooked input from the master).
    input: VecDeque<u8>,
    /// Canonical-mode line being assembled before it is committed to `input`.
    canon: VecDeque<u8>,
    /// `VLNEXT` latch: the next input byte is taken verbatim. Persisted here (not
    /// a loop-local) so Ctrl-V and its quoted char may arrive in separate writes.
    lnext: bool,
    /// Virtual modem control lines (`TIOCM_*`) reported by `TIOCMGET`. A PTY has
    /// no real lines; this just remembers what a program set via `TIOCMSET`.
    modem: i32,
    /// Software flow control (IXON): when set by a `VSTOP` (Ctrl-S) from the
    /// master, program output is held in `output` until a `VSTART` (Ctrl-Q).
    stopped: bool,
    /// Canonical VEOF on an empty line: next slave read returns EOF (0) once.
    eof_pending: bool,
    /// Bytes available to the master's `read` (program output + echoed input).
    output: VecDeque<u8>,
    termios: Termios,
    winsize: ConsoleWinSize,
}

/// One pseudo-terminal pair.
pub struct Pty {
    id: u32,
    inner: Mutex<PtyInner>,
    /// Readable-on-master signalling (output non-empty, or slave hung up).
    master_bus: Arc<Mutex<EventBus>>,
    /// Readable-on-slave signalling (input non-empty, or master closed).
    slave_bus: Arc<Mutex<EventBus>>,
    /// Foreground process group of the terminal (for signal delivery).
    fg_pgrp: AtomicI32,
    /// Number of currently-open slave fds.
    slave_open: AtomicI32,
    /// Set once the slave has been opened at least once; the master only reports
    /// EOF after the slave was opened and then fully closed.
    slave_ever_open: AtomicBool,
    /// The master fd has been closed: the slave then sees EOF.
    master_closed: AtomicBool,
    /// `TIOCSPTLCK` flag. Stored for `TIOCGPTLCK`-style queries but not enforced
    /// on slave open, so programs that skip `unlockpt(3)` still work.
    locked: AtomicBool,
}

impl Pty {
    fn wake_master(&self) {
        self.master_bus.lock().set(Event::READABLE);
    }
    fn wake_slave(&self) {
        self.slave_bus.lock().set(Event::READABLE);
    }

    /// Master read side is satisfiable now (data ready, or hangup → EOF).
    fn master_readable(&self) -> bool {
        let inner = self.inner.lock();
        // Output paused by flow control (Ctrl-S) is withheld from the master.
        if !inner.stopped && !inner.output.is_empty() {
            return true;
        }
        drop(inner);
        self.slave_ever_open.load(Ordering::Relaxed) && self.slave_open.load(Ordering::Relaxed) <= 0
    }

    /// Slave read side is satisfiable now (data ready, or master closed → EOF).
    fn slave_readable(&self) -> bool {
        let inner = self.inner.lock();
        !inner.input.is_empty() || inner.eof_pending || self.master_closed.load(Ordering::Relaxed)
    }

    /// Feed bytes written to the master through the input line discipline.
    fn master_write(&self, data: &[u8]) -> usize {
        let mut wake_slave = false;
        let mut wake_master = false;
        let mut clear_master_readable = false;
        let mut clear_slave_readable = false;
        let mut signals: alloc::vec::Vec<Signal> = alloc::vec::Vec::new();
        {
            let mut inner = self.inner.lock();
            let iflag = inner.termios.c_iflag;
            let lflag = inner.termios.c_lflag;
            let oflag = inner.termios.c_oflag;
            let cc = inner.termios.c_cc;
            for &b in data {
                let mut c = b;
                // Input CR/NL translation.
                if c == b'\r' {
                    if iflag & IGNCR != 0 {
                        continue;
                    }
                    if iflag & ICRNL != 0 {
                        c = b'\n';
                    }
                } else if c == b'\n' && iflag & INLCR != 0 {
                    c = b'\r';
                }

                // Literal-next (VLNEXT, Ctrl-V): the previous byte armed it, so
                // insert this one verbatim, skipping signal/edit interpretation.
                if inner.lnext {
                    inner.lnext = false;
                    if lflag & ICANON != 0 {
                        inner.canon.push_back(c);
                        if echo_byte(&mut inner.output, c, lflag, oflag) {
                            wake_master = true;
                        }
                    } else {
                        inner.input.push_back(c);
                        if echo_byte(&mut inner.output, c, lflag, oflag) {
                            wake_master = true;
                        }
                        wake_slave = true;
                    }
                    continue;
                }

                // Software flow control (IXON): VSTOP (Ctrl-S) holds program
                // output bound for the master; VSTART (Ctrl-Q) releases it. With
                // IXANY, any byte releases. These control bytes are consumed.
                // `cc[X] == 0` means VDISABLE — do not match.
                if iflag & IXON != 0 {
                    if cc[VSTOP] != 0 && c == cc[VSTOP] {
                        inner.stopped = true;
                        clear_master_readable = true;
                        continue;
                    }
                    if cc[VSTART] != 0 && c == cc[VSTART] {
                        if inner.stopped {
                            inner.stopped = false;
                            if !inner.output.is_empty() {
                                wake_master = true;
                            }
                        }
                        continue;
                    }
                    if iflag & IXANY != 0 && inner.stopped {
                        inner.stopped = false;
                        if !inner.output.is_empty() {
                            wake_master = true;
                        }
                    }
                }

                // Discard (VDISCARD, Ctrl-O): no separate output queue to flush,
                // so just consume the byte under IEXTEN.
                if lflag & IEXTEN != 0 && cc[VDISCARD] != 0 && c == cc[VDISCARD] {
                    continue;
                }

                // Signal-generating characters.
                if lflag & ISIG != 0 {
                    let sig = if cc[VINTR] != 0 && c == cc[VINTR] {
                        Some((Signal::SIGINT, "^C"))
                    } else if cc[VQUIT] != 0 && c == cc[VQUIT] {
                        Some((Signal::SIGQUIT, "^\\"))
                    } else if cc[VSUSP] != 0 && c == cc[VSUSP] {
                        Some((Signal::SIGTSTP, "^Z"))
                    } else {
                        None
                    };
                    if let Some((signal, label)) = sig {
                        if lflag & NOFLSH == 0 {
                            inner.input.clear();
                            inner.canon.clear();
                            inner.eof_pending = false;
                            clear_slave_readable = true;
                        }
                        // Resume any output frozen by Ctrl-S so the signalled
                        // program isn't left blocked behind a stopped terminal.
                        if inner.stopped {
                            inner.stopped = false;
                            wake_master = true;
                        }
                        if lflag & ECHO != 0 {
                            inner.output.extend(label.as_bytes());
                            inner.output.extend(b"\r\n");
                            wake_master = true;
                        }
                        signals.push(signal);
                        continue;
                    }
                }

                if lflag & ICANON != 0 {
                    let iexten = lflag & IEXTEN != 0;
                    if iexten && cc[VWERASE] != 0 && c == cc[VWERASE] {
                        // Word erase: drop trailing blanks, then the word.
                        // ECHO alone gates it: with `stty -echo` nothing at all
                        // may reach the terminal (see VERASE below).
                        let echo = lflag & ECHO != 0;
                        while matches!(inner.canon.back(), Some(&b' ') | Some(&b'\t')) {
                            inner.canon.pop_back();
                            if echo {
                                inner.output.extend(b"\x08 \x08");
                                wake_master = true;
                            }
                        }
                        while let Some(&b) = inner.canon.back() {
                            if b == b' ' || b == b'\t' {
                                break;
                            }
                            inner.canon.pop_back();
                            if echo {
                                inner.output.extend(b"\x08 \x08");
                                wake_master = true;
                            }
                        }
                    } else if iexten && cc[VREPRINT] != 0 && c == cc[VREPRINT] {
                        // Reprint the pending line on a fresh line.
                        if lflag & ECHO != 0 {
                            if lflag & ECHOCTL != 0 {
                                inner.output.extend(b"^R");
                            }
                            inner.output.extend(b"\r\n");
                            let pending: alloc::vec::Vec<u8> =
                                inner.canon.iter().copied().collect();
                            for b in pending {
                                echo_byte(&mut inner.output, b, lflag, oflag);
                            }
                            wake_master = true;
                        }
                    } else if iexten && cc[VLNEXT] != 0 && c == cc[VLNEXT] {
                        inner.lnext = true;
                        if lflag & ECHO != 0 && lflag & ECHOCTL != 0 {
                            inner.output.extend(b"^\x08");
                            wake_master = true;
                        }
                    } else if cc[VERASE] != 0 && c == cc[VERASE] {
                        // ECHO decides WHETHER to echo, ECHOE only decides HOW.
                        // Testing `ECHO | ECHOE` made a backspace visible under
                        // `stty -echo` -- ECHOE stays set in the default termios,
                        // so a password prompt echoed a rubout for every
                        // correction, painting over the prompt and telling an
                        // onlooker the secret was being edited. Linux's n_tty
                        // and this kernel's own console (`stdio.rs`) both gate
                        // on ECHO first.
                        if inner.canon.pop_back().is_some() && lflag & ECHO != 0 {
                            if lflag & ECHOE != 0 {
                                inner.output.extend(b"\x08 \x08");
                            } else {
                                inner.output.push_back(cc[VERASE]);
                            }
                            wake_master = true;
                        }
                    } else if cc[VKILL] != 0 && c == cc[VKILL] {
                        let n = inner.canon.len();
                        inner.canon.clear();
                        if lflag & ECHO != 0 {
                            for _ in 0..n {
                                inner.output.extend(b"\x08 \x08");
                            }
                            wake_master = true;
                        }
                    } else if cc[VEOF] != 0 && c == cc[VEOF] {
                        // Commit the pending line without a newline; an empty
                        // line signals end-of-file to the reader (eof_pending).
                        let had_data = !inner.canon.is_empty();
                        while let Some(ch) = inner.canon.pop_front() {
                            inner.input.push_back(ch);
                        }
                        if had_data {
                            wake_slave = true;
                        } else {
                            inner.eof_pending = true;
                            wake_slave = true;
                        }
                    } else {
                        inner.canon.push_back(c);
                        if echo_byte(&mut inner.output, c, lflag, oflag) {
                            wake_master = true;
                        }
                        // Commit the line on newline or a configured EOL delimiter.
                        let is_eol = c == b'\n'
                            || (cc[VEOL] != 0 && c == cc[VEOL])
                            || (cc[VEOL2] != 0 && c == cc[VEOL2]);
                        if is_eol {
                            while let Some(ch) = inner.canon.pop_front() {
                                inner.input.push_back(ch);
                            }
                            wake_slave = true;
                        }
                    }
                } else {
                    inner.input.push_back(c);
                    if echo_byte(&mut inner.output, c, lflag, oflag) {
                        wake_master = true;
                    }
                    wake_slave = true;
                }
            }
        }
        // Clear latched READABLE before waking: a later VSTART in the same
        // write may re-arm the master bus after a VSTOP cleared it.
        if clear_master_readable && !self.master_readable() {
            self.master_bus.lock().clear(Event::READABLE);
        }
        if clear_slave_readable && !self.slave_readable() {
            self.slave_bus.lock().clear(Event::READABLE);
        }
        if wake_master {
            self.wake_master();
        }
        if wake_slave {
            self.wake_slave();
        }
        let pgrp = self.fg_pgrp.load(Ordering::Relaxed);
        if pgrp > 0 {
            // Terminal-generated signals go to the whole foreground process
            // GROUP, not just its leader — otherwise Ctrl-C reaches only the
            // shell (or only the pipeline leader) and the running job survives.
            for signal in signals {
                let _ = crate::process::send_signal_to_pgrp(pgrp as usize, signal);
            }
        }
        data.len()
    }

    /// Program output written to the slave, post-processed for the master.
    fn slave_write(&self, data: &[u8]) -> usize {
        if data.is_empty() {
            return 0;
        }
        {
            let mut inner = self.inner.lock();
            let oflag = inner.termios.c_oflag;
            let post = oflag & OPOST != 0 && oflag & ONLCR != 0;
            for &b in data {
                if post && b == b'\n' {
                    inner.output.push_back(b'\r');
                }
                inner.output.push_back(b);
            }
        }
        self.wake_master();
        data.len()
    }

    fn master_read(&self, buf: &mut [u8]) -> Result<usize> {
        let mut inner = self.inner.lock();
        // While stopped by flow control (Ctrl-S), hold output back from the
        // master, but still surface EOF if the slave has hung up.
        if inner.output.is_empty() || inner.stopped {
            drop(inner);
            if self.slave_ever_open.load(Ordering::Relaxed)
                && self.slave_open.load(Ordering::Relaxed) <= 0
            {
                return Ok(0); // slave hung up → EOF
            }
            return Err(FsError::Again);
        }
        let mut n = 0;
        while n < buf.len() {
            match inner.output.pop_front() {
                Some(b) => {
                    buf[n] = b;
                    n += 1;
                }
                None => break,
            }
        }
        if inner.output.is_empty() {
            drop(inner);
            self.master_bus.lock().clear(Event::READABLE);
        }
        Ok(n)
    }

    fn slave_read(&self, buf: &mut [u8]) -> Result<usize> {
        let mut inner = self.inner.lock();
        let canon = inner.termios.c_lflag & ICANON != 0;
        if inner.input.is_empty() {
            if inner.eof_pending {
                inner.eof_pending = false;
                drop(inner);
                if !self.slave_readable() {
                    self.slave_bus.lock().clear(Event::READABLE);
                }
                return Ok(0); // VEOF on empty line → EOF
            }
            drop(inner);
            if self.master_closed.load(Ordering::Relaxed) {
                return Ok(0); // master closed → EOF
            }
            return Err(FsError::Again);
        }
        let mut n = 0;
        while n < buf.len() {
            match inner.input.pop_front() {
                Some(b) => {
                    buf[n] = b;
                    n += 1;
                    if canon && b == b'\n' {
                        break;
                    }
                }
                None => break,
            }
        }
        if inner.input.is_empty() && !inner.eof_pending {
            drop(inner);
            if !self.master_closed.load(Ordering::Relaxed) {
                self.slave_bus.lock().clear(Event::READABLE);
            }
        }
        Ok(n)
    }

    /// Shared ioctl handling for both ends. `is_master` selects the queue a
    /// count ioctl (`FIONREAD`/`TIOCOUTQ`) reports on.
    fn ioctl(&self, cmd: u32, data: usize, is_master: bool) -> Result<usize> {
        match cmd as usize {
            TIOCGPTN => {
                unsafe { *(data as *mut u32) = self.id };
                Ok(0)
            }
            TIOCSPTLCK => {
                let lock = unsafe { *(data as *const i32) };
                self.locked.store(lock != 0, Ordering::Relaxed);
                Ok(0)
            }
            TCGETS => {
                unsafe { *(data as *mut Termios) = self.inner.lock().termios };
                Ok(0)
            }
            TCSETS | TCSETSW => {
                let t = unsafe { *(data as *const Termios) };
                self.inner.lock().termios = t;
                Ok(0)
            }
            TCSETSF => {
                // Set attributes and flush the input queue (Linux TCSETSF).
                let t = unsafe { *(data as *const Termios) };
                let clear_readable;
                {
                    let mut inner = self.inner.lock();
                    inner.termios = t;
                    inner.input.clear();
                    inner.canon.clear();
                    inner.eof_pending = false;
                    clear_readable = inner.input.is_empty();
                }
                if clear_readable {
                    self.slave_bus.lock().clear(Event::READABLE);
                }
                Ok(0)
            }
            TIOCGWINSZ => {
                unsafe { *(data as *mut ConsoleWinSize) = self.inner.lock().winsize };
                Ok(0)
            }
            TIOCSWINSZ => {
                let ws = unsafe { *(data as *const ConsoleWinSize) };
                let changed = {
                    let mut inner = self.inner.lock();
                    let old = inner.winsize;
                    inner.winsize = ws;
                    old.ws_row != ws.ws_row
                        || old.ws_col != ws.ws_col
                        || old.ws_xpixel != ws.ws_xpixel
                        || old.ws_ypixel != ws.ws_ypixel
                };
                // Notify the foreground group that the window changed — but only
                // when it actually CHANGED, like Linux (`tty_do_resize`). foot
                // re-sends TIOCSWINSZ on every surface configure event, and an
                // unconditional signal per call lands a SIGWINCH storm on the
                // shell exactly while it prints its first prompt and arms line
                // editing: the redraws left the cursor a line below the prompt
                // and wedged the pending read.
                if changed {
                    let pgrp = self.fg_pgrp.load(Ordering::Relaxed);
                    if pgrp > 0 {
                        let _ =
                            crate::process::send_signal_to_pgrp(pgrp as usize, Signal::SIGWINCH);
                    }
                }
                Ok(0)
            }
            TIOCGPGRP => {
                let mut pgid = self.fg_pgrp.load(Ordering::Relaxed);
                if pgid == 0 {
                    // No foreground group recorded yet: report the CALLER's own
                    // pgrp, not a made-up constant. busybox ash's job-control
                    // init loops `killpg(0, SIGTTIN)` until tcgetpgrp() ==
                    // getpgrp(), so any other answer leaves the shell stopped
                    // before its first prompt (same fallback stdio's VT ioctl
                    // uses). This matters for xterm: it acquires the pty as
                    // ctty IMPLICITLY (setsid + first slave open, Linux
                    // `tty_open` semantics) without ever issuing TIOCSCTTY, so
                    // the explicit seeding in the syscall layer never fires —
                    // unlike foot/alacritty, whose login_tty() does.
                    use zircon_object::object::KernelObject;
                    if let Some(arc) = kernel_hal::thread::get_current_thread() {
                        if let Ok(thread) = arc.downcast::<zircon_object::task::Thread>() {
                            pgid = crate::process::get_process_pgid(thread.proc().id()).unwrap_or(0)
                                as i32;
                        }
                    }
                }
                if pgid == 0 {
                    pgid = 1;
                }
                unsafe { *(data as *mut i32) = pgid };
                Ok(0)
            }
            TIOCSPGRP => {
                let pgid = unsafe { *(data as *const i32) };
                self.fg_pgrp.store(pgid, Ordering::Relaxed);
                Ok(0)
            }
            TCFLSH | TIOCSCTTY | TIOCNOTTY => Ok(0),
            // Bytes readable at this end: the master reads program output, the
            // slave reads cooked input.
            FIONREAD => {
                let inner = self.inner.lock();
                let n = if is_master {
                    inner.output.len()
                } else {
                    inner.input.len()
                } as i32;
                unsafe { *(data as *mut i32) = n };
                Ok(0)
            }
            // Bytes still queued toward the other end.
            TIOCOUTQ => {
                let inner = self.inner.lock();
                let n = if is_master {
                    inner.input.len() + inner.canon.len()
                } else {
                    inner.output.len()
                } as i32;
                unsafe { *(data as *mut i32) = n };
                Ok(0)
            }
            TIOCGSID => {
                let mut sid = self.fg_pgrp.load(Ordering::Relaxed);
                if sid <= 0 {
                    sid = 1;
                }
                unsafe { *(data as *mut i32) = sid };
                Ok(0)
            }
            // Virtual modem control lines (no real hardware behind a PTY).
            TIOCMGET => {
                unsafe { *(data as *mut i32) = self.inner.lock().modem };
                Ok(0)
            }
            TIOCMSET => {
                let v = unsafe { *(data as *const i32) };
                self.inner.lock().modem = v;
                Ok(0)
            }
            TIOCMBIS => {
                let v = unsafe { *(data as *const i32) };
                self.inner.lock().modem |= v;
                Ok(0)
            }
            TIOCMBIC => {
                let v = unsafe { *(data as *const i32) };
                self.inner.lock().modem &= !v;
                Ok(0)
            }
            // No real UART behind a PTY, so all serial line counters are zero.
            TIOCGICOUNT => {
                unsafe { *(data as *mut SerialIcounter) = SerialIcounter::default() };
                Ok(0)
            }
            _ => Err(FsError::NotSupported),
        }
    }
}

/// Echo one input byte to the master read side. Returns whether anything was
/// written. Mirrors the console line discipline's `echo_char`.
fn echo_byte(out: &mut VecDeque<u8>, c: u8, lflag: u32, oflag: u32) -> bool {
    if lflag & ECHO == 0 {
        return false;
    }
    match c {
        b'\n' => {
            if oflag & OPOST != 0 && oflag & ONLCR != 0 {
                out.extend(b"\r\n");
            } else {
                out.push_back(b'\n');
            }
        }
        b'\r' => out.push_back(b'\r'),
        0x7f | 0x08 => out.extend(b"\x08 \x08"),
        b'\t' => out.push_back(b'\t'),
        c if c < 0x20 => {
            if lflag & ECHOCTL != 0 {
                out.push_back(b'^');
                out.push_back(c + 64);
            } else {
                out.push_back(c);
            }
        }
        c => out.push_back(c),
    }
    true
}

lazy_static! {
    /// All live PTYs, keyed by number. A pair stays alive as long as either end
    /// (or any inherited fd) references its `Arc<Pty>`.
    static ref PTYS: Mutex<BTreeMap<u32, Arc<Pty>>> = Mutex::new(BTreeMap::new());
}

static NEXT_PTY: AtomicU32 = AtomicU32::new(0);

/// Allocate a fresh PTY pair and return the master INode. Called from the open
/// path when a process opens `/dev/ptmx`.
pub fn alloc_ptmx() -> Arc<dyn INode> {
    let id = NEXT_PTY.fetch_add(1, Ordering::Relaxed);
    let pty = Arc::new(Pty {
        id,
        inner: Mutex::new(PtyInner {
            input: VecDeque::new(),
            canon: VecDeque::new(),
            lnext: false,
            modem: TIOCM_DTR | TIOCM_RTS | TIOCM_CAR | TIOCM_CTS | TIOCM_DSR,
            stopped: false,
            eof_pending: false,
            output: VecDeque::new(),
            termios: Termios::default_tty(),
            winsize: ConsoleWinSize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        }),
        master_bus: Arc::new(Mutex::new(EventBus::default())),
        slave_bus: Arc::new(Mutex::new(EventBus::default())),
        fg_pgrp: AtomicI32::new(0),
        slave_open: AtomicI32::new(0),
        slave_ever_open: AtomicBool::new(false),
        master_closed: AtomicBool::new(false),
        locked: AtomicBool::new(false),
    });
    PTYS.lock().insert(id, pty.clone());
    Arc::new(PtyMaster { pty })
}

/// Parse the slave number from a `/dev/pts/N` path.
pub fn pts_id_from_path(path: &str) -> Option<u32> {
    path.strip_prefix("/dev/pts/")?.parse::<u32>().ok()
}

/// Open the slave `/dev/pts/N`. Returns `None` if the pair no longer exists
/// (its master was closed).
pub fn open_pts(id: u32) -> Option<Arc<dyn INode>> {
    let pty = PTYS.lock().get(&id).cloned()?;
    if pty.master_closed.load(Ordering::Relaxed) {
        return None;
    }
    pty.slave_open.fetch_add(1, Ordering::Relaxed);
    pty.slave_ever_open.store(true, Ordering::Relaxed);
    Some(Arc::new(PtySlave { pty }))
}

/// Master endpoint INode (the `/dev/ptmx` open result).
pub struct PtyMaster {
    pty: Arc<Pty>,
}

/// Slave endpoint INode (`/dev/pts/N`).
pub struct PtySlave {
    pty: Arc<Pty>,
}

impl PtyMaster {
    /// Pty number of this master (the `N` of its `/dev/pts/N`).
    pub fn pty_id(&self) -> u32 {
        self.pty.id
    }

    /// `TIOCGPTPEER`: open this master's slave end directly, without going
    /// through `/dev/pts/N` (Linux 4.13+). Same bookkeeping as opening the
    /// path. `None` once the master is closed.
    ///
    /// rustix-openpty (alacritty and every other Rust terminal built on it)
    /// tries this ioctl FIRST and only falls back to `ptsname()` + `open()`
    /// on `ENOSYS`/`EPERM`; the generic `ENOTTY` the master answered for an
    /// unknown request was passed straight up as the program's fatal error
    /// (`Os { code: 25, message: "Not a tty" }`) on real hardware.
    pub fn open_peer(&self) -> Option<Arc<dyn INode>> {
        open_pts(self.pty.id)
    }

    /// Park `waker` on the master-side event bus (see
    /// `FileLike::subscribe_readiness`; reached through `File`'s inode
    /// downcast). Safe to gate a long poll backstop on: every
    /// `master_bus` set is paired with actual master readability, and the
    /// VSTOP path clears a stale latch (see `Pty::slave_write_common`).
    pub fn subscribe_readiness(
        &self,
        events: crate::fs::PollEvents,
        waker: &core::task::Waker,
    ) -> crate::sync::ReadinessSub {
        let mask = crate::fs::poll_events_to_bus_mask(events);
        crate::sync::subscribe_readiness_on(&self.pty.master_bus, mask, waker)
    }
}

impl PtySlave {
    /// Pty number of this slave (the `N` in `/dev/pts/N`).
    pub fn pty_id(&self) -> u32 {
        self.pty.id
    }

    /// Slave-side counterpart of [`PtyMaster::subscribe_readiness`].
    pub fn subscribe_readiness(
        &self,
        events: crate::fs::PollEvents,
        waker: &core::task::Waker,
    ) -> crate::sync::ReadinessSub {
        let mask = crate::fs::poll_events_to_bus_mask(events);
        crate::sync::subscribe_readiness_on(&self.pty.slave_bus, mask, waker)
    }

    /// Set the terminal's foreground process group. Used by the syscall layer's
    /// `TIOCSCTTY` handling: adopting a controlling terminal sets its foreground
    /// group to the caller's pgrp (Linux `tty_jobctrl.c` semantics), which the
    /// inode-level ioctl cannot do itself — it has no process context.
    pub fn set_fg_pgrp(&self, pgid: i32) {
        self.pty.fg_pgrp.store(pgid, Ordering::Relaxed);
    }
}

impl Drop for PtyMaster {
    fn drop(&mut self) {
        self.pty.master_closed.store(true, Ordering::Relaxed);
        // Hang up the session and wake any slave reader so it observes EOF.
        // SIGHUP goes to the foreground process GROUP: the shell's current
        // foreground job (a running `top`) must be hung up along with it, not
        // survive as an orphan writing into a dead pty.
        let pgrp = self.pty.fg_pgrp.load(Ordering::Relaxed);
        if pgrp > 0 {
            let _ = crate::process::send_signal_to_pgrp(pgrp as usize, Signal::SIGHUP);
        }
        self.pty.wake_slave();
        PTYS.lock().remove(&self.pty.id);
    }
}

impl Drop for PtySlave {
    fn drop(&mut self) {
        if self.pty.slave_open.fetch_sub(1, Ordering::Relaxed) <= 1 {
            // Last slave gone: wake the master so it reports EOF.
            self.pty.wake_master();
        }
    }
}

/// Future that resolves when one PTY end becomes readable (data or hangup).
/// Manual future (rather than an `async` block) to match the `Pipe` pattern and
/// keep the boxed future `Send + Sync`.
struct PtyReadFuture<'a> {
    pty: &'a Pty,
    bus: Arc<Mutex<EventBus>>,
    check: fn(&Pty) -> bool,
    sub_id: Option<u64>,
}

impl Drop for PtyReadFuture<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.sub_id.take() {
            self.bus.lock().unsubscribe(id);
        }
    }
}

impl Future for PtyReadFuture<'_> {
    type Output = Result<PollStatus>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.get_mut();
        let ready = Ok(PollStatus {
            read: true,
            write: true,
            error: false,
            hangup: false,
        });
        if (this.check)(this.pty) {
            if let Some(id) = this.sub_id.take() {
                this.bus.lock().unsubscribe(id);
            }
            return Poll::Ready(ready);
        }
        if this.sub_id.is_none() {
            let waker = cx.waker().clone();
            this.sub_id = this.bus.lock().subscribe(Box::new(move |_| {
                waker.wake_by_ref();
                true
            }));
        }
        // Re-check after subscribing: data may have arrived in the window
        // between the first check and the subscription, which would otherwise
        // be a missed wakeup.
        if (this.check)(this.pty) {
            if let Some(id) = this.sub_id.take() {
                this.bus.lock().unsubscribe(id);
            }
            Poll::Ready(ready)
        } else {
            Poll::Pending
        }
    }
}

fn readable_future<'a>(
    pty: &'a Pty,
    bus: Arc<Mutex<EventBus>>,
    check: fn(&Pty) -> bool,
) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
    Box::pin(PtyReadFuture {
        pty,
        bus,
        check,
        sub_id: None,
    })
}

impl INode for PtyMaster {
    fn read_at(&self, _offset: usize, buf: &mut [u8]) -> Result<usize> {
        self.pty.master_read(buf)
    }
    fn write_at(&self, _offset: usize, buf: &[u8]) -> Result<usize> {
        Ok(self.pty.master_write(buf))
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: self.pty.master_readable(),
            write: true,
            error: false,
            hangup: false,
        })
    }
    fn async_poll<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
        readable_future(&self.pty, self.pty.master_bus.clone(), Pty::master_readable)
    }
    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        self.pty.ioctl(cmd, data, true)
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(pty_metadata(make_rdev(5, 2)))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

impl INode for PtySlave {
    fn read_at(&self, _offset: usize, buf: &mut [u8]) -> Result<usize> {
        self.pty.slave_read(buf)
    }
    fn write_at(&self, _offset: usize, buf: &[u8]) -> Result<usize> {
        Ok(self.pty.slave_write(buf))
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: self.pty.slave_readable(),
            write: true,
            error: false,
            hangup: false,
        })
    }
    fn async_poll<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
        readable_future(&self.pty, self.pty.slave_bus.clone(), Pty::slave_readable)
    }
    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        self.pty.ioctl(cmd, data, false)
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(pty_metadata(make_rdev(136, self.pty.id as usize)))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

fn pty_metadata(rdev: usize) -> Metadata {
    Metadata {
        dev: 1,
        inode: 0,
        size: 0,
        blk_size: 0,
        blocks: 0,
        atime: Timespec { sec: 0, nsec: 0 },
        mtime: Timespec { sec: 0, nsec: 0 },
        ctime: Timespec { sec: 0, nsec: 0 },
        type_: FileType::CharDevice,
        mode: 0o620,
        nlinks: 1,
        uid: 0,
        gid: 0,
        rdev,
    }
}

/// Marker INode registered at `/dev/ptmx`. Opening it is special-cased in the
/// `openat` path to mint a fresh master; direct reads/writes are not meaningful.
pub struct PtmxINode;

impl INode for PtmxINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: false,
            write: true,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(pty_metadata(make_rdev(5, 2)))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    //! Host tests for the pseudo-terminal pair.
    //!
    //! Every terminal emulator, `ssh` session and `tmux` pane runs through
    //! this. It is a second, independent implementation of the line discipline
    //! (the console's lives in `stdio.rs`), so the two can drift apart, and a
    //! regression here is invisible until someone is typing into a shell.
    //!
    //! Unlike the console's, this one's echo **is** observable: it goes into
    //! the same queue the master reads, so these tests check what the terminal
    //! would actually display as well as what the program would read.
    //!
    //! A `Pty` owns all its state, so each test builds its own pair and
    //! nothing is shared — no serialisation needed.

    use super::*;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    /// A fresh pair with one slave open, as `alloc_ptmx` + `open_pts` would
    /// leave it, but without registering in the global `PTYS` map.
    fn pty() -> Pty {
        let p = Pty {
            id: 0,
            inner: Mutex::new(PtyInner {
                input: VecDeque::new(),
                canon: VecDeque::new(),
                lnext: false,
                modem: 0,
                stopped: false,
                eof_pending: false,
                output: VecDeque::new(),
                termios: Termios::default_tty(),
                winsize: ConsoleWinSize {
                    ws_row: 24,
                    ws_col: 80,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                },
            }),
            master_bus: Arc::new(Mutex::new(EventBus::default())),
            slave_bus: Arc::new(Mutex::new(EventBus::default())),
            fg_pgrp: AtomicI32::new(0),
            slave_open: AtomicI32::new(1),
            slave_ever_open: AtomicBool::new(true),
            master_closed: AtomicBool::new(false),
            locked: AtomicBool::new(false),
        };
        p
    }

    fn set_flags(p: &Pty, f: impl FnOnce(&mut Termios)) {
        f(&mut p.inner.lock().termios);
    }

    /// Everything the program would read right now, one `read` at a time (so
    /// canonical mode's one-line-per-read rule shows up).
    fn slave_reads(p: &Pty) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            let mut buf = [0u8; 64];
            match p.slave_read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.push(String::from_utf8_lossy(&buf[..n]).into_owned()),
                Err(_) => break,
            }
        }
        out
    }

    /// Everything the terminal would display right now.
    fn master_drain(p: &Pty) -> String {
        let mut out = String::new();
        loop {
            let mut buf = [0u8; 128];
            match p.master_read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.push_str(&String::from_utf8_lossy(&buf[..n])),
                Err(_) => break,
            }
        }
        out
    }

    const DEL: u8 = 0x7f;
    const CTRL_C: u8 = 3;
    const CTRL_D: u8 = 4;
    const CTRL_O: u8 = 15;
    const CTRL_Q: u8 = 17;
    const CTRL_R: u8 = 18;
    const CTRL_S: u8 = 19;
    const CTRL_U: u8 = 21;
    const CTRL_V: u8 = 22;
    const CTRL_W: u8 = 23;

    // ---- echo_byte, on its own -----------------------------------------

    #[test]
    fn echo_byte_renders_each_class_of_character() {
        let d = Termios::default_tty();
        let mut out = VecDeque::new();
        // ECHO off: nothing written, and the caller is told so.
        assert!(!echo_byte(&mut out, b'a', 0, d.c_oflag));
        assert!(out.is_empty());

        // Newline becomes CRLF only when OPOST|ONLCR are both on.
        let mut out = VecDeque::new();
        assert!(echo_byte(&mut out, b'\n', ECHO, OPOST | ONLCR));
        assert_eq!(Vec::from(out), b"\r\n");
        let mut out = VecDeque::new();
        echo_byte(&mut out, b'\n', ECHO, 0);
        assert_eq!(Vec::from(out), b"\n");

        // Backspace and DEL both erase visually.
        for c in [0x08u8, DEL] {
            let mut out = VecDeque::new();
            echo_byte(&mut out, c, ECHO, 0);
            assert_eq!(Vec::from(out), b"\x08 \x08");
        }

        // Tab and CR go out as themselves, never as ^I / ^M.
        let mut out = VecDeque::new();
        echo_byte(&mut out, b'\t', ECHO | ECHOCTL, 0);
        echo_byte(&mut out, b'\r', ECHO | ECHOCTL, 0);
        assert_eq!(Vec::from(out), b"\t\r");

        // Other control characters: caret notation under ECHOCTL, raw without.
        let mut out = VecDeque::new();
        echo_byte(&mut out, CTRL_C, ECHO | ECHOCTL, 0);
        assert_eq!(Vec::from(out), b"^C");
        let mut out = VecDeque::new();
        echo_byte(&mut out, CTRL_C, ECHO, 0);
        assert_eq!(Vec::from(out), [CTRL_C]);
    }

    // ---- canonical input ------------------------------------------------

    #[test]
    fn a_canonical_line_reaches_the_program_only_on_its_newline() {
        let p = pty();
        p.master_write(b"hola");
        // Still being edited: nothing for the program to read yet.
        assert!(!p.slave_readable());
        // But it is already echoed to the terminal.
        assert_eq!(master_drain(&p), "hola");
        p.master_write(b"\n");
        assert_eq!(slave_reads(&p), vec!["hola\n"]);
        assert_eq!(master_drain(&p), "\r\n");
    }

    #[test]
    fn a_canonical_read_stops_at_the_end_of_one_line() {
        let p = pty();
        p.master_write(b"una\ndos\n");
        // Two lines queued, but each read returns exactly one -- what a shell
        // depends on to not swallow the next command.
        assert_eq!(slave_reads(&p), vec!["una\n", "dos\n"]);
    }

    #[test]
    fn a_raw_read_takes_everything_queued() {
        let p = pty();
        set_flags(&p, |t| t.c_lflag &= !ICANON);
        p.master_write(b"una\ndos\n");
        assert_eq!(slave_reads(&p), vec!["una\ndos\n"]);
    }

    #[test]
    fn icrnl_completes_a_line_but_inlcr_and_igncr_do_not() {
        let p = pty();
        p.master_write(b"ab\r");
        assert_eq!(slave_reads(&p), vec!["ab\n"]);

        let p = pty();
        set_flags(&p, |t| t.c_iflag = IGNCR);
        p.master_write(b"ab\r");
        assert!(!p.slave_readable());

        let p = pty();
        set_flags(&p, |t| t.c_iflag = INLCR);
        p.master_write(b"ab\n");
        // '\n' became '\r', which is not an end of line.
        assert!(!p.slave_readable());
    }

    #[test]
    fn veol_terminates_a_line_and_a_zero_control_char_is_disabled() {
        let p = pty();
        set_flags(&p, |t| t.c_cc[VEOL] = b';');
        p.master_write(b"uno;");
        assert_eq!(slave_reads(&p), vec!["uno;"]);

        // VEOL back at its default of 0 must not make NUL an end of line.
        let p = pty();
        p.master_write(b"dos\0");
        assert!(!p.slave_readable());
    }

    // ---- line editing ---------------------------------------------------

    #[test]
    fn verase_removes_one_character_and_echoes_the_rubout() {
        let p = pty();
        p.master_write(b"abc");
        assert_eq!(master_drain(&p), "abc");
        p.master_write(&[DEL]);
        assert_eq!(master_drain(&p), "\x08 \x08");
        p.master_write(b"\n");
        assert_eq!(slave_reads(&p), vec!["ab\n"]);
    }

    #[test]
    fn verase_on_an_empty_line_erases_nothing_and_echoes_nothing() {
        let p = pty();
        p.master_write(&[DEL, DEL]);
        // No rubout for a character that was never there, or the terminal
        // would eat the prompt.
        assert_eq!(master_drain(&p), "");
        assert!(!p.slave_readable());
    }

    #[test]
    fn with_echo_off_nothing_at_all_reaches_the_terminal() {
        // `stty -echo`, which is what a password prompt does. ECHOE stays set,
        // because it is in the default termios and nothing clears it.
        let p = pty();
        set_flags(&p, |t| t.c_lflag &= !ECHO);
        assert!(p.inner.lock().termios.c_lflag & ECHOE != 0);
        p.master_write(b"secreto");
        assert_eq!(master_drain(&p), "");
        // A correction must be silent too: a rubout here paints over the
        // prompt and tells an onlooker the secret is being edited.
        p.master_write(&[DEL]);
        assert_eq!(master_drain(&p), "");
        p.master_write(&[CTRL_W]);
        assert_eq!(master_drain(&p), "");
        // The program still gets what was typed.
        p.master_write(b"\n");
        assert_eq!(slave_reads(&p), vec!["\n"]);
    }

    #[test]
    fn without_echoe_an_erase_echoes_the_erase_character_itself() {
        // ECHO decides whether to echo; ECHOE only decides how.
        let p = pty();
        set_flags(&p, |t| t.c_lflag &= !ECHOE);
        p.master_write(b"ab");
        let _ = master_drain(&p);
        p.master_write(&[DEL]);
        assert_eq!(master_drain(&p), "\x7f");
    }

    #[test]
    fn vkill_clears_the_line_and_rubs_out_every_character() {
        let p = pty();
        p.master_write(b"abcd");
        let _ = master_drain(&p);
        p.master_write(&[CTRL_U]);
        assert_eq!(master_drain(&p), "\x08 \x08".repeat(4));
        p.master_write(b"\n");
        assert_eq!(slave_reads(&p), vec!["\n"]);
    }

    #[test]
    fn vwerase_drops_trailing_blanks_then_one_word() {
        let p = pty();
        p.master_write(b"foo bar  ");
        let _ = master_drain(&p);
        p.master_write(&[CTRL_W]);
        // Two blanks plus the three letters of "bar".
        assert_eq!(master_drain(&p), "\x08 \x08".repeat(5));
        p.master_write(b"\n");
        assert_eq!(slave_reads(&p), vec!["foo \n"]);
    }

    #[test]
    fn vreprint_redraws_the_pending_line_without_changing_it() {
        let p = pty();
        p.master_write(b"intacta");
        let _ = master_drain(&p);
        p.master_write(&[CTRL_R]);
        // ECHOCTL is off in the default termios, so the "^R" marker is not
        // drawn -- only the fresh line and the pending text.
        assert_eq!(master_drain(&p), "\r\nintacta");
        p.master_write(b"\n");
        assert_eq!(slave_reads(&p), vec!["intacta\n"]);

        // With ECHOCTL the marker is drawn first.
        let p = pty();
        set_flags(&p, |t| t.c_lflag |= ECHOCTL);
        p.master_write(b"intacta");
        let _ = master_drain(&p);
        p.master_write(&[CTRL_R]);
        assert_eq!(master_drain(&p), "^R\r\nintacta");
    }

    #[test]
    fn vlnext_quotes_the_next_byte_even_an_interrupt() {
        let p = pty();
        p.master_write(&[CTRL_V]);
        p.master_write(&[CTRL_C]);
        p.master_write(b"\n");
        // The Ctrl-C is data, not a signal, and it is echoed as ^C rather than
        // acting as one.
        assert_eq!(slave_reads(&p), vec!["\x03\n"]);

        // The latch is one-shot: the SECOND Ctrl-C is a signal again, so it
        // flushes the line the first one was quoted into.
        let p = pty();
        p.master_write(&[CTRL_V, CTRL_C]);
        assert_eq!(p.inner.lock().canon.len(), 1);
        let _ = master_drain(&p);
        p.master_write(&[CTRL_C]);
        assert!(p.inner.lock().canon.is_empty());
        assert_eq!(master_drain(&p), "^C\r\n");
    }

    #[test]
    fn vlnext_survives_a_write_boundary() {
        // Ctrl-V and the byte it quotes routinely arrive in separate writes
        // from a terminal, which is why the latch lives in `PtyInner`.
        let p = pty();
        p.master_write(&[CTRL_V]);
        assert!(p.inner.lock().lnext);
        p.master_write(&[CTRL_C]);
        p.master_write(b"\n");
        assert_eq!(slave_reads(&p), vec!["\x03\n"]);
    }

    #[test]
    fn the_extended_editing_bytes_need_iexten() {
        let p = pty();
        set_flags(&p, |t| t.c_lflag &= !IEXTEN);
        p.master_write(b"ab cd");
        p.master_write(&[CTRL_W]);
        p.master_write(b"\n");
        // Ctrl-W is ordinary input without IEXTEN.
        assert_eq!(slave_reads(&p), vec!["ab cd\x17\n"]);
    }

    // ---- end of file ----------------------------------------------------

    #[test]
    fn veof_delivers_a_partial_line_and_only_ends_the_file_on_an_empty_one() {
        let p = pty();
        p.master_write(b"abc");
        p.master_write(&[CTRL_D]);
        // The partial line arrives with no newline...
        let mut buf = [0u8; 64];
        assert_eq!(p.slave_read(&mut buf).unwrap(), 3);
        assert_eq!(&buf[..3], b"abc");
        // ...and this is NOT end of file: the program must keep reading, so
        // the next read blocks rather than returning 0.
        assert!(!p.inner.lock().eof_pending);
        assert!(p.slave_read(&mut buf).is_err());

        // Ctrl-D at the start of a line IS end of file, exactly once.
        p.master_write(&[CTRL_D]);
        assert!(p.inner.lock().eof_pending);
        assert_eq!(p.slave_read(&mut buf).unwrap(), 0);
        assert!(p.slave_read(&mut buf).is_err());
    }

    #[test]
    fn an_interrupt_clears_a_pending_eof() {
        let p = pty();
        p.master_write(&[CTRL_D]);
        assert!(p.inner.lock().eof_pending);
        p.master_write(&[CTRL_C]);
        // Otherwise the next read returns 0 and the shell exits on a Ctrl-C.
        assert!(!p.inner.lock().eof_pending);
    }

    // ---- signals --------------------------------------------------------

    #[test]
    fn an_interrupt_flushes_the_pending_input_and_echoes_its_label() {
        let p = pty();
        p.master_write(b"a medio escribir");
        let _ = master_drain(&p);
        p.master_write(&[CTRL_C]);
        assert!(!p.slave_readable());
        assert_eq!(master_drain(&p), "^C\r\n");
    }

    #[test]
    fn noflsh_keeps_the_pending_input_across_an_interrupt() {
        let p = pty();
        set_flags(&p, |t| t.c_lflag |= NOFLSH);
        p.master_write(b"abc");
        p.master_write(&[CTRL_C]);
        p.master_write(b"\n");
        assert_eq!(slave_reads(&p), vec!["abc\n"]);
    }

    #[test]
    fn without_isig_an_interrupt_byte_is_ordinary_input() {
        let p = pty();
        set_flags(&p, |t| t.c_lflag &= !ISIG);
        p.master_write(&[b'a', CTRL_C, b'\n']);
        assert_eq!(slave_reads(&p), vec!["a\x03\n"]);
    }

    // ---- flow control ---------------------------------------------------

    #[test]
    fn ctrl_s_holds_program_output_back_from_the_terminal() {
        let p = pty();
        p.slave_write(b"antes\n");
        p.master_write(&[CTRL_S]);
        // The program's output is queued but must not reach the terminal.
        assert_eq!(master_drain(&p), "");
        assert!(!p.master_readable());
        p.master_write(&[CTRL_Q]);
        assert_eq!(master_drain(&p), "antes\r\n");
        // Neither control byte may reach the program.
        assert!(!p.slave_readable());
    }

    #[test]
    fn ixany_lets_any_byte_resume_and_still_delivers_it() {
        let p = pty();
        set_flags(&p, |t| t.c_iflag |= IXANY);
        p.slave_write(b"x");
        p.master_write(&[CTRL_S]);
        assert_eq!(master_drain(&p), "");
        p.master_write(b"z\n");
        // Output flows again, and 'z' was input, not just a resume trigger.
        assert!(master_drain(&p).starts_with('x'));
        assert_eq!(slave_reads(&p), vec!["z\n"]);
    }

    #[test]
    fn without_ixany_an_ordinary_byte_does_not_resume_output() {
        let p = pty();
        p.slave_write(b"x");
        p.master_write(&[CTRL_S]);
        p.master_write(b"z\n");
        assert_eq!(master_drain(&p), "");
        assert!(p.inner.lock().stopped);
    }

    #[test]
    fn an_interrupt_lifts_an_output_freeze() {
        let p = pty();
        p.slave_write(b"x");
        p.master_write(&[CTRL_S]);
        assert!(p.inner.lock().stopped);
        // Otherwise the signalled program stays blocked behind the Ctrl-S.
        p.master_write(&[CTRL_C]);
        assert!(!p.inner.lock().stopped);
        assert_eq!(master_drain(&p), "x^C\r\n");
    }

    #[test]
    fn vdiscard_is_swallowed_under_iexten() {
        let p = pty();
        p.master_write(&[b'a', CTRL_O, b'\n']);
        assert_eq!(slave_reads(&p), vec!["a\n"]);
        let p = pty();
        set_flags(&p, |t| t.c_lflag &= !IEXTEN);
        p.master_write(&[b'a', CTRL_O, b'\n']);
        assert_eq!(slave_reads(&p), vec!["a\x0f\n"]);
    }

    // ---- output post-processing ----------------------------------------

    #[test]
    fn slave_write_expands_newlines_only_under_opost_and_onlcr() {
        let p = pty();
        p.slave_write(b"uno\ndos\n");
        assert_eq!(master_drain(&p), "uno\r\ndos\r\n");

        let p = pty();
        set_flags(&p, |t| t.c_oflag = 0);
        p.slave_write(b"uno\ndos\n");
        assert_eq!(master_drain(&p), "uno\ndos\n");

        // An empty write is a no-op, not a wake.
        let p = pty();
        assert_eq!(p.slave_write(b""), 0);
        assert!(!p.master_readable());
    }

    // ---- hangup ---------------------------------------------------------

    #[test]
    fn an_empty_queue_is_eagain_until_the_far_end_hangs_up() {
        let p = pty();
        let mut buf = [0u8; 8];
        // Both ends open, nothing queued: try again, not end of file.
        assert!(p.master_read(&mut buf).is_err());
        assert!(p.slave_read(&mut buf).is_err());

        // Slave closes: the master now reads EOF.
        p.slave_open.store(0, Ordering::Relaxed);
        assert_eq!(p.master_read(&mut buf).unwrap(), 0);
        assert!(p.master_readable());

        // Master closes: the slave reads EOF.
        let p = pty();
        p.master_closed.store(true, Ordering::Relaxed);
        assert_eq!(p.slave_read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn output_already_queued_survives_the_slave_hanging_up() {
        let p = pty();
        p.slave_write(b"ultimas palabras\n");
        p.slave_open.store(0, Ordering::Relaxed);
        // The terminal must still get what the program printed before exiting,
        // and only then see EOF.
        let mut buf = [0u8; 128];
        let n = p.master_read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"ultimas palabras\r\n");
        assert_eq!(p.master_read(&mut buf).unwrap(), 0);
    }

    // ---- path parsing ---------------------------------------------------

    #[test]
    fn pts_paths_parse_only_when_they_name_a_number() {
        assert_eq!(pts_id_from_path("/dev/pts/0"), Some(0));
        assert_eq!(pts_id_from_path("/dev/pts/42"), Some(42));
        assert_eq!(pts_id_from_path("/dev/pts/"), None);
        assert_eq!(pts_id_from_path("/dev/pts/ptmx"), None);
        assert_eq!(pts_id_from_path("/dev/pts/-1"), None);
        assert_eq!(pts_id_from_path("/dev/pts/1/2"), None);
        assert_eq!(pts_id_from_path("/dev/tty0"), None);
        assert_eq!(pts_id_from_path("pts/0"), None);
    }
}
