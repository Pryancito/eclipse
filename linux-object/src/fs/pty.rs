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
const VSUSP: usize = 10;
const VEOL: usize = 11;
const VREPRINT: usize = 12;
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
        if !self.inner.lock().output.is_empty() {
            return true;
        }
        self.slave_ever_open.load(Ordering::Relaxed) && self.slave_open.load(Ordering::Relaxed) <= 0
    }

    /// Slave read side is satisfiable now (data ready, or master closed → EOF).
    fn slave_readable(&self) -> bool {
        !self.inner.lock().input.is_empty() || self.master_closed.load(Ordering::Relaxed)
    }

    /// Feed bytes written to the master through the input line discipline.
    fn master_write(&self, data: &[u8]) -> usize {
        let mut wake_slave = false;
        let mut wake_master = false;
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

                // Signal-generating characters.
                if lflag & ISIG != 0 {
                    let sig = if c == cc[VINTR] {
                        Some((Signal::SIGINT, "^C"))
                    } else if c == cc[VQUIT] {
                        Some((Signal::SIGQUIT, "^\\"))
                    } else if c == cc[VSUSP] {
                        Some((Signal::SIGTSTP, "^Z"))
                    } else {
                        None
                    };
                    if let Some((signal, label)) = sig {
                        if lflag & NOFLSH == 0 {
                            inner.input.clear();
                            inner.canon.clear();
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
                        let echo = lflag & (ECHO | ECHOE) != 0;
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
                            let pending: alloc::vec::Vec<u8> = inner.canon.iter().copied().collect();
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
                    } else if c == cc[VERASE] {
                        if inner.canon.pop_back().is_some() && lflag & (ECHO | ECHOE) != 0 {
                            inner.output.extend(b"\x08 \x08");
                            wake_master = true;
                        }
                    } else if c == cc[VKILL] {
                        let n = inner.canon.len();
                        inner.canon.clear();
                        if lflag & ECHO != 0 {
                            for _ in 0..n {
                                inner.output.extend(b"\x08 \x08");
                            }
                            wake_master = true;
                        }
                    } else if c == cc[VEOF] {
                        // Commit the pending line without a newline; an empty
                        // line at the start signals end-of-file to the reader.
                        while let Some(ch) = inner.canon.pop_front() {
                            inner.input.push_back(ch);
                        }
                        wake_slave = true;
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
        if wake_master {
            self.wake_master();
        }
        if wake_slave {
            self.wake_slave();
        }
        let pgrp = self.fg_pgrp.load(Ordering::Relaxed);
        if pgrp > 0 {
            for signal in signals {
                let _ = crate::process::send_signal_to_process(pgrp as usize, signal);
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
        if inner.output.is_empty() {
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
        if inner.input.is_empty() {
            drop(inner);
            self.slave_bus.lock().clear(Event::READABLE);
        }
        Ok(n)
    }

    /// Shared ioctl handling for both ends.
    fn ioctl(&self, cmd: u32, data: usize) -> Result<usize> {
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
            TCSETS | TCSETSW | TCSETSF => {
                let t = unsafe { *(data as *const Termios) };
                self.inner.lock().termios = t;
                Ok(0)
            }
            TIOCGWINSZ => {
                unsafe { *(data as *mut ConsoleWinSize) = self.inner.lock().winsize };
                Ok(0)
            }
            TIOCSWINSZ => {
                let ws = unsafe { *(data as *const ConsoleWinSize) };
                self.inner.lock().winsize = ws;
                // Notify the foreground program that the window changed.
                let pgrp = self.fg_pgrp.load(Ordering::Relaxed);
                if pgrp > 0 {
                    let _ = crate::process::send_signal_to_process(pgrp as usize, Signal::SIGWINCH);
                }
                Ok(0)
            }
            TIOCGPGRP => {
                let mut pgid = self.fg_pgrp.load(Ordering::Relaxed);
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

impl Drop for PtyMaster {
    fn drop(&mut self) {
        self.pty.master_closed.store(true, Ordering::Relaxed);
        // Hang up the session and wake any slave reader so it observes EOF.
        let pgrp = self.pty.fg_pgrp.load(Ordering::Relaxed);
        if pgrp > 0 {
            let _ = crate::process::send_signal_to_process(pgrp as usize, Signal::SIGHUP);
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
}

impl Future for PtyReadFuture<'_> {
    type Output = Result<PollStatus>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let ready = Ok(PollStatus {
            read: true,
            write: true,
            error: false,
        });
        if (self.check)(self.pty) {
            return Poll::Ready(ready);
        }
        let waker = cx.waker().clone();
        self.bus.lock().subscribe(Box::new(move |_| {
            waker.wake_by_ref();
            true
        }));
        // Re-check after subscribing: data may have arrived in the window
        // between the first check and the subscription, which would otherwise
        // be a missed wakeup.
        if (self.check)(self.pty) {
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
    Box::pin(PtyReadFuture { pty, bus, check })
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
        })
    }
    fn async_poll<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
        readable_future(&self.pty, self.pty.master_bus.clone(), Pty::master_readable)
    }
    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        self.pty.ioctl(cmd, data)
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
        })
    }
    fn async_poll<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
        readable_future(&self.pty, self.pty.slave_bus.clone(), Pty::slave_readable)
    }
    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        self.pty.ioctl(cmd, data)
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
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(pty_metadata(make_rdev(5, 2)))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}
