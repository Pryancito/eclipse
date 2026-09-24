//! Pseudo-terminals (`/dev/ptmx` + `/dev/pts/N`).
//!
//! A PTY is a bidirectional pipe with a terminal line discipline in the middle:
//!
//! ```text
//!   terminal (st)  ──write──►  master  ──[input discipline]──►  slave read   (shell stdin)
//!   terminal (st)  ◄──read───  master  ◄──[output discipline]──  slave write  (shell stdout)
//! ```
//!
//! Opening `/dev/ptmx` allocates a fresh pair and exposes the slave at
//! `/dev/pts/N`; the master is returned to the opener. This is what lets a
//! terminal emulator run a real shell under TinyX/Xfbdev.
//!
//! Only the common path is implemented (canonical + raw input, ECHO, ISIG,
//! ICRNL/ONLCR, winsize, the ptmx/pts ioctls). It is deliberately gated behind
//! `/dev/ptmx`, so nothing else in the system is affected.

use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};
use core::{
    any::Any,
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicU32, Ordering},
    task::{Context, Poll},
};

use kernel_hal::console::ConsoleWinSize;
use lock::Mutex;
use rcore_fs::vfs::*;
use rcore_fs_devfs::DevFS;

use super::super::ioctl::*;
use crate::fs::stdio::wake_tty_intr_waiters;

// c_iflag
const INLCR: u32 = 0x0040;
const IGNCR: u32 = 0x0080;
const ICRNL: u32 = 0x0100;
// c_lflag
const ICANON: u32 = 0x0002;
const ECHO: u32 = 0x0008;
// c_cc indices
const VERASE: usize = 2;
const VEOF: usize = 4;
// `ISIG`, the signal characters and `VDISABLE` come from `ioctl.rs`, which is
// where the rule the three line disciplines share now lives.

// The bound on each queue, and the rule for a queue that is full, come from
// `ioctl.rs` too: `N_TTY_BUF_SIZE` in, `TTY_OUTPUT_CAP` out, `input_room` for
// the answer. This end used to have one number of its own for both directions,
// 16 KiB, which is four times Linux's on the way in and a fortieth of it on
// the way out -- and `canon` was not bounded by it at all.

/// Shared state of one PTY pair.
struct PtyInner {
    /// slave → master: program output, read by the terminal.
    output: VecDeque<u8>,
    /// master → slave (after the input line discipline): read by the program.
    input: VecDeque<u8>,
    /// In-progress canonical line, not yet visible to the slave reader.
    canon: VecDeque<u8>,
    /// A `VEOF` has been committed and the slave's next read ends at it.
    ///
    /// Without this, Ctrl-D on an empty line commits an empty `canon`, the
    /// slave finds nothing queued and blocks — so end-of-input never arrives
    /// and the only way out is closing the master.
    eof_pending: bool,
    termios: Termios,
    /// Where the cursor sits on the master's line, counted the way the driver
    /// counts it. `ONOCR` is defined in terms of this and `ONLRET` exists to
    /// keep it honest; see [`Termios::output_char`].
    out_column: usize,
    winsize: ConsoleWinSize,
    /// Foreground process group of the slave (`TIOCSPGRP`), for job control
    /// and signal delivery (Ctrl-C).
    fg_pgrp: i32,
    /// `TIOCSPTLCK` lock flag (1 = locked). The terminal clears it via
    /// `unlockpt()` before opening the slave.
    locked: i32,
    num: u32,
    master_open: bool,
}

impl PtyInner {
    fn new(num: u32) -> Self {
        Self {
            output: VecDeque::new(),
            input: VecDeque::new(),
            canon: VecDeque::new(),
            eof_pending: false,
            termios: Termios::default_tty(),
            out_column: 0,
            winsize: ConsoleWinSize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
            fg_pgrp: 0,
            locked: 1,
            num,
            master_open: true,
        }
    }

    fn push_output(&mut self, b: u8) {
        if self.output.len() < TTY_OUTPUT_CAP {
            self.output.push_back(b);
        }
    }

    /// Room for one more input byte. `input` and `canon` share the bound.
    ///
    /// No `canonical` to pass: this end has no writer to hand a short count
    /// to, so the two ways of being full mean the same thing to it.
    fn has_room(&self) -> bool {
        has_input_room(self.input.len(), self.canon.len())
    }

    /// Process one byte written to the master (a keystroke from the terminal)
    /// through the slave's input line discipline.
    fn master_input_byte(&mut self, mut b: u8) {
        let iflag = self.termios.c_iflag;
        let lflag = self.termios.c_lflag;
        let cc = self.termios.c_cc;

        // CR/NL input translation.
        if b == b'\r' {
            if iflag & IGNCR != 0 {
                return;
            }
            if iflag & ICRNL != 0 {
                b = b'\n';
            }
        } else if b == b'\n' && iflag & INLCR != 0 {
            b = b'\r';
        }

        // Signals (Ctrl-C / Ctrl-\ / Ctrl-Z).
        if let Some(which) = self.termios.tty_signal(b) {
            let sig = match which {
                TtySignal::Intr => crate::signal::Signal::SIGINT,
                TtySignal::Quit => crate::signal::Signal::SIGQUIT,
                TtySignal::Susp => crate::signal::Signal::SIGTSTP,
            };
            if lflag & ECHO != 0 {
                self.echo_ctrl(b);
            }
            // `fg_pgrp` is a process *group*, and a job is a group precisely
            // so that one Ctrl-C reaches every process in a pipeline. Sending
            // to a *process* numbered `fg_pgrp` reaches the group leader
            // alone, and nothing at all once the leader has exited while the
            // rest of the pipeline is still running.
            if self.fg_pgrp > 0 {
                let _ = crate::process::send_signal_to_pgrp(self.fg_pgrp as usize, sig);
            }
            return;
        }

        if lflag & ICANON != 0 {
            // Erase (Backspace / DEL).
            if cc[VERASE] != VDISABLE && b == cc[VERASE] {
                // One rubout moves the cursor one column, and a character
                // several bytes long still occupies one. Taking a byte off
                // leaves half a character in the line, which is not a
                // character, and leaves the screen disagreeing with the line
                // about how much is written.
                let n = if self.termios.utf8_input() {
                    let tail: alloc::vec::Vec<u8> = self.canon.iter().copied().collect();
                    utf8_erase_len(&tail)
                } else {
                    usize::from(!self.canon.is_empty())
                };
                for _ in 0..n {
                    self.canon.pop_back();
                }
                // `ECHO` decides WHETHER to echo, `ECHOE` only decides HOW.
                // Testing `ECHO | ECHOE` makes a backspace visible on a
                // terminal that turned echo off, which is what a password
                // prompt does.
                if n > 0 && lflag & ECHO != 0 {
                    // Erase the echoed glyph: backspace, space, backspace.
                    self.push_output(0x08);
                    self.push_output(b' ');
                    self.push_output(0x08);
                }
                return;
            }
            // End of file on an empty line: deliver a zero-length read.
            if cc[VEOF] != VDISABLE && b == cc[VEOF] {
                if self.canon.is_empty() {
                    self.eof_pending = true;
                } else {
                    self.commit_canon();
                }
                return;
            }
            if lflag & ECHO != 0 {
                if b == b'\n' {
                    self.push_output(b'\n');
                } else {
                    self.echo_ctrl(b);
                }
            }
            // A full line that is all there is keeps being taken so VERASE
            // still reaches the discipline above -- refusing it wedges the
            // terminal at the one moment the user needs to shorten the line --
            // but the byte is dropped, and the newline with it.
            if self.has_room() {
                self.canon.push_back(b);
                if b == b'\n' {
                    self.commit_canon();
                }
            }
        } else {
            // Raw mode: deliver immediately.
            if lflag & ECHO != 0 {
                self.echo_ctrl(b);
            }
            if self.has_room() {
                self.input.push_back(b);
            }
        }
    }

    /// Echo a byte to the master, rendering control chars as `^X`.
    fn echo_ctrl(&mut self, b: u8) {
        if (b < 0x20 && b != b'\n' && b != b'\t') || b == 0x7f {
            self.push_output(b'^');
            self.push_output(if b == 0x7f { b'?' } else { b'@' + b });
        } else {
            self.push_output(b);
        }
    }

    /// Move the line being edited to where a reader can take it.
    ///
    /// No bound to check: the two queues share one, so what was in `canon` was
    /// already inside it. Dropping here instead, which is what the old
    /// per-queue cap did, took bytes out of the **middle** of a line the user
    /// had already finished and handed the reader the rest as if it were
    /// whole.
    fn commit_canon(&mut self) {
        while let Some(b) = self.canon.pop_front() {
            self.input.push_back(b);
        }
    }

    /// Whether a program reading the slave has something to take.
    ///
    /// Three places ask this — the blocking `read_at`, `poll(2)` and the
    /// future behind `async_poll` — and they must give the same answer or a
    /// reader parks on input the read would have handed over. So they ask
    /// here instead of each spelling it out.
    fn slave_readable(&self) -> bool {
        !self.input.is_empty() || self.eof_pending || !self.master_open
    }

    /// Process bytes written by the slave (program output) toward the master,
    /// applying the whole of `c_oflag`, not just `ONLCR`.
    fn slave_output(&mut self, buf: &[u8]) {
        for &b in buf {
            let out = self.termios.output_char(b, &mut self.out_column);
            for &o in out.as_bytes() {
                self.push_output(o);
            }
        }
    }
}

// ----------------------------------------------------------------------------
// Registry: pts number → slave inode, plus the `/dev/pts` directory handle.
// ----------------------------------------------------------------------------

lazy_static::lazy_static! {
    static ref PTYS: Mutex<BTreeMap<u32, Arc<PtySlave>>> = Mutex::new(BTreeMap::new());
}
static NEXT_PTY: AtomicU32 = AtomicU32::new(0);

/// `/dev/pts`: a directory whose children resolve dynamically to live slaves.
/// `ptsname()` opens `/dev/pts/N`; lookup walks here and `find("N")` returns the
/// registered slave for pty number `N`.
pub struct PtsDir {
    inode_id: usize,
}

impl Default for PtsDir {
    fn default() -> Self {
        Self::new()
    }
}

impl PtsDir {
    pub fn new() -> Self {
        Self {
            inode_id: DevFS::new_inode_id(),
        }
    }
}

impl INode for PtsDir {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Err(FsError::IsDir)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::IsDir)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        let num: u32 = name.parse().map_err(|_| FsError::EntryNotFound)?;
        PTYS.lock()
            .get(&num)
            .cloned()
            .map(|s| s as Arc<dyn INode>)
            .ok_or(FsError::EntryNotFound)
    }
    fn metadata(&self) -> Result<Metadata> {
        let mut m = chardev_metadata(self.inode_id, 0o755);
        m.type_ = FileType::Dir;
        Ok(m)
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

// ----------------------------------------------------------------------------
// `/dev/ptmx`: opening it clones a new pair.
// ----------------------------------------------------------------------------

/// The `/dev/ptmx` node. Resolving it is normal; the open path downcasts to
/// this type and calls [`PtmxINode::open_master`] to get a fresh master.
pub struct PtmxINode {
    inode_id: usize,
}

impl Default for PtmxINode {
    fn default() -> Self {
        Self::new()
    }
}

impl PtmxINode {
    pub fn new() -> Self {
        Self {
            inode_id: DevFS::new_inode_id(),
        }
    }

    /// Allocate a new PTY pair: publish the slave at `/dev/pts/N` and return
    /// the master inode for the opener.
    pub fn open_master(&self) -> Result<Arc<dyn INode>> {
        let num = NEXT_PTY.fetch_add(1, Ordering::Relaxed);
        let inner = Arc::new(Mutex::new(PtyInner::new(num)));
        let slave = Arc::new(PtySlave {
            inner: inner.clone(),
            inode_id: DevFS::new_inode_id(),
        });
        let master = Arc::new(PtyMaster {
            inner,
            inode_id: DevFS::new_inode_id(),
        });
        PTYS.lock().insert(num, slave);
        Ok(master as Arc<dyn INode>)
    }
}

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
        Ok(chardev_metadata(self.inode_id, 0o666))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

// ----------------------------------------------------------------------------
// Master end.
// ----------------------------------------------------------------------------

pub struct PtyMaster {
    inner: Arc<Mutex<PtyInner>>,
    inode_id: usize,
}

impl Drop for PtyMaster {
    fn drop(&mut self) {
        let num = {
            let mut g = self.inner.lock();
            g.master_open = false;
            g.num
        };
        PTYS.lock().remove(&num);
        wake_tty_intr_waiters();
    }
}

impl INode for PtyMaster {
    fn read_at(&self, _offset: usize, buf: &mut [u8]) -> Result<usize> {
        let mut g = self.inner.lock();
        if g.output.is_empty() {
            // Block until the program produces output. The master never EOFs on
            // an idle slave (slaves may not have been opened yet); the terminal
            // detects the child exiting via waitpid and closes the master.
            return Err(FsError::Again);
        }
        let mut n = 0;
        while n < buf.len() {
            match g.output.pop_front() {
                Some(b) => {
                    buf[n] = b;
                    n += 1;
                }
                None => break,
            }
        }
        Ok(n)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> Result<usize> {
        {
            let mut g = self.inner.lock();
            for &b in buf {
                g.master_input_byte(b);
            }
        }
        wake_tty_intr_waiters();
        Ok(buf.len())
    }

    fn poll(&self) -> Result<PollStatus> {
        let g = self.inner.lock();
        Ok(PollStatus {
            read: !g.output.is_empty(),
            write: true,
            error: false,
            hangup: false,
        })
    }

    fn async_poll<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
        Box::pin(PtyReadFuture {
            inner: &self.inner,
            master: true,
            armed: false,
        })
    }

    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        pty_ioctl(&self.inner, true, cmd, data)
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(chardev_metadata(self.inode_id, 0o600))
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

// ----------------------------------------------------------------------------
// Slave end (`/dev/pts/N`).
// ----------------------------------------------------------------------------

pub struct PtySlave {
    inner: Arc<Mutex<PtyInner>>,
    inode_id: usize,
}

impl INode for PtySlave {
    fn read_at(&self, _offset: usize, buf: &mut [u8]) -> Result<usize> {
        let mut g = self.inner.lock();
        if g.input.is_empty() {
            if !g.slave_readable() {
                return Err(FsError::Again);
            }
            // A pending `VEOF` is consumed by the read it ends, the way
            // Ctrl-D does: the next read after it blocks again rather than
            // reporting end-of-input for ever. A closed master is not
            // consumed, because it does not come back.
            g.eof_pending = false;
            return Ok(0); // end of input
        }
        let mut n = 0;
        while n < buf.len() {
            match g.input.pop_front() {
                Some(b) => {
                    buf[n] = b;
                    n += 1;
                }
                None => break,
            }
        }
        Ok(n)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> Result<usize> {
        {
            let mut g = self.inner.lock();
            g.slave_output(buf);
        }
        wake_tty_intr_waiters();
        Ok(buf.len())
    }

    fn poll(&self) -> Result<PollStatus> {
        let g = self.inner.lock();
        Ok(PollStatus {
            read: g.slave_readable(),
            write: true,
            error: false,
            hangup: false,
        })
    }

    fn async_poll<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
        Box::pin(PtyReadFuture {
            inner: &self.inner,
            master: false,
            armed: false,
        })
    }

    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        pty_ioctl(&self.inner, false, cmd, data)
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(chardev_metadata(self.inode_id, 0o620))
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

// ----------------------------------------------------------------------------
// Shared ioctl handling.
// ----------------------------------------------------------------------------

const TIOCSPTLCK: u32 = 0x4004_5431; // set/clear pty lock
const TIOCGPTN: u32 = 0x8004_5430; // get pty number

fn pty_ioctl(inner: &Arc<Mutex<PtyInner>>, master: bool, cmd: u32, data: usize) -> Result<usize> {
    let cmd = cmd as usize;
    let mut g = inner.lock();
    match cmd {
        TCGETS => {
            if data == 0 {
                return Err(FsError::InvalidParam);
            }
            unsafe { *(data as *mut Termios) = g.termios };
            Ok(0)
        }
        TCSETS | TCSETSW | TCSETSF => {
            if data == 0 {
                return Err(FsError::InvalidParam);
            }
            g.termios = unsafe { *(data as *const Termios) };
            Ok(0)
        }
        TIOCGWINSZ => {
            if data == 0 {
                return Err(FsError::InvalidParam);
            }
            unsafe { *(data as *mut ConsoleWinSize) = g.winsize };
            Ok(0)
        }
        TIOCSWINSZ => {
            if data == 0 {
                return Err(FsError::InvalidParam);
            }
            g.winsize = unsafe { *(data as *const ConsoleWinSize) };
            Ok(0)
        }
        TIOCSPGRP => {
            if data != 0 {
                g.fg_pgrp = unsafe { *(data as *const i32) };
            }
            Ok(0)
        }
        TIOCGPGRP => {
            if data == 0 {
                return Err(FsError::InvalidParam);
            }
            let mut pgid = g.fg_pgrp;
            if pgid == 0 {
                // Same caller-pgrp fallback as `fs::pty` / stdio: reporting 0
                // (or any constant) to busybox ash's job-control init leaves it
                // looping `killpg(0, SIGTTIN)` forever with no prompt, because
                // clients that acquire the ctty implicitly (setsid + first
                // slave open, e.g. xterm) never issue TIOCSCTTY to seed us.
                use zircon_object::object::KernelObject;
                if let Some(arc) = kernel_hal::thread::get_current_thread() {
                    if let Ok(thread) = arc.downcast::<zircon_object::task::Thread>() {
                        pgid = crate::process::get_process_pgid(thread.proc().id()).unwrap_or(0)
                            as i32;
                    }
                }
                if pgid == 0 {
                    pgid = 1;
                }
            }
            unsafe { *(data as *mut i32) = pgid };
            Ok(0)
        }
        _ if cmd as u32 == TIOCGPTN => {
            if !master || data == 0 {
                return Err(FsError::InvalidParam);
            }
            unsafe { *(data as *mut u32) = g.num };
            Ok(0)
        }
        _ if cmd as u32 == TIOCSPTLCK => {
            if !master || data == 0 {
                return Err(FsError::InvalidParam);
            }
            g.locked = unsafe { *(data as *const i32) };
            Ok(0)
        }
        // TIOCSCTTY / TIOCNOTTY: accept (controlling-tty assignment is a no-op
        // here); TCFLSH: nothing buffered worth flushing distinctly — accept
        // rather than hand terminals a spurious ENOTTY.
        0x540E | TIOCNOTTY | TCFLSH => Ok(0),
        _ => Err(FsError::NotSupported),
    }
}

// ----------------------------------------------------------------------------
// Blocking-read future: wakes when the peer writes (via the TTY intr wakers).
// ----------------------------------------------------------------------------

struct PtyReadFuture<'a> {
    inner: &'a Arc<Mutex<PtyInner>>,
    master: bool,
    armed: bool,
}

impl<'a> Future for PtyReadFuture<'a> {
    type Output = Result<PollStatus>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.as_mut().get_mut();
        let ready = {
            let g = this.inner.lock();
            if this.master {
                !g.output.is_empty()
            } else {
                g.slave_readable()
            }
        };
        if ready {
            return Poll::Ready(Ok(PollStatus {
                read: true,
                write: true,
                error: false,
                hangup: false,
            }));
        }
        if this.armed {
            crate::net::retain_io_wait_wakers(cx.waker(), false, true);
        } else {
            crate::net::register_io_wait_wakers(cx.waker(), false, true);
            this.armed = true;
        }
        Poll::Pending
    }
}

// ----------------------------------------------------------------------------

fn chardev_metadata(inode_id: usize, mode: u16) -> Metadata {
    Metadata {
        dev: 0,
        inode: inode_id,
        size: 0,
        blk_size: 0,
        blocks: 0,
        atime: Timespec { sec: 0, nsec: 0 },
        mtime: Timespec { sec: 0, nsec: 0 },
        ctime: Timespec { sec: 0, nsec: 0 },
        type_: FileType::CharDevice,
        mode,
        nlinks: 1,
        uid: 0,
        gid: 0,
        rdev: 0,
    }
}

#[cfg(test)]
mod tests {
    //! Host tests for the third line discipline.
    //!
    //! This is the PTY behind `/dev/ptmx`, which is what a terminal emulator
    //! under TinyX/Xfbdev opens to run a shell. It is a **third** independent
    //! implementation of the input discipline — the console's is in
    //! `fs/stdio.rs` and the live PTY's in `fs/pty.rs` — and until these
    //! existed nothing exercised a single line of it.
    //!
    //! Being third is the whole problem: every rule here is a rule the other
    //! two also have, and this copy answered several of them differently.
    //!
    //! `PtyInner` owns all its state, so each test builds its own and nothing
    //! is shared.

    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    fn pty() -> PtyInner {
        PtyInner::new(0)
    }

    /// Type these bytes at the terminal.
    fn typed(p: &mut PtyInner, bytes: &[u8]) {
        for &b in bytes {
            p.master_input_byte(b);
        }
    }

    /// What the program reading the slave would get.
    fn read_by_the_program(p: &mut PtyInner) -> Vec<u8> {
        p.input.drain(..).collect()
    }

    /// What the terminal would display.
    fn shown_on_screen(p: &mut PtyInner) -> Vec<u8> {
        p.output.drain(..).collect()
    }

    fn slave_of(inner: PtyInner) -> PtySlave {
        PtySlave {
            inner: Arc::new(Mutex::new(inner)),
            inode_id: 0,
        }
    }

    // ---------------------------------------------------------------- lines

    #[test]
    fn a_line_reaches_the_program_only_when_enter_ends_it() {
        let mut p = pty();
        typed(&mut p, b"hola");
        assert!(
            read_by_the_program(&mut p).is_empty(),
            "una linea a medias no se entrega"
        );
        typed(&mut p, b"\n");
        assert_eq!(read_by_the_program(&mut p), b"hola\n");
    }

    #[test]
    fn what_was_typed_is_what_the_terminal_shows() {
        let mut p = pty();
        typed(&mut p, b"hola\n");
        assert_eq!(shown_on_screen(&mut p), b"hola\n");
    }

    #[test]
    fn a_return_key_becomes_a_newline() {
        // ICRNL is on in a cooked terminal, which is why Enter (which sends
        // CR) ends a line at all.
        let mut p = pty();
        typed(&mut p, b"hola\r");
        assert_eq!(read_by_the_program(&mut p), b"hola\n");
    }

    #[test]
    fn a_terminal_told_to_ignore_the_return_key_ignores_it() {
        let mut p = pty();
        p.termios.c_iflag |= IGNCR;
        typed(&mut p, b"ho\rla\n");
        assert_eq!(read_by_the_program(&mut p), b"hola\n");
    }

    #[test]
    fn inlcr_swaps_the_two_the_other_way_round() {
        let mut p = pty();
        p.termios.c_iflag &= !ICRNL;
        p.termios.c_iflag |= INLCR;
        typed(&mut p, b"a\n");
        // The newline became a CR, so no line was ever ended.
        assert!(read_by_the_program(&mut p).is_empty());
        assert_eq!(p.canon.iter().copied().collect::<Vec<u8>>(), b"a\r");
    }

    // --------------------------------------------------------------- erase

    #[test]
    fn backspace_takes_a_whole_character_and_not_a_byte() {
        // `ñ` is `c3 b1`. Taking one byte off leaves `c3`, which is half a
        // letter, and half a letter is not a letter.
        let mut p = pty();
        assert!(p.termios.utf8_input(), "el terminal arranca en UTF-8");
        typed(&mut p, "añ".as_bytes());
        typed(&mut p, &[127]); // DEL
        typed(&mut p, b"\n");
        assert_eq!(read_by_the_program(&mut p), b"a\n");
    }

    #[test]
    fn one_character_gets_one_rubout_however_many_bytes_it_is() {
        // `\x08 \x08` moves the cursor one column, and `ñ` occupies one.
        let mut p = pty();
        typed(&mut p, "ñ".as_bytes());
        let _ = shown_on_screen(&mut p);
        typed(&mut p, &[127]);
        assert_eq!(shown_on_screen(&mut p), b"\x08 \x08");
    }

    #[test]
    fn a_terminal_that_is_not_utf8_erases_a_byte() {
        // Without IUTF8 the line is a pipe of bytes, and that is what someone
        // sending raw data through a cooked terminal is asking for.
        let mut p = pty();
        p.termios.c_iflag &= !I_IUTF8;
        typed(&mut p, "añ".as_bytes());
        typed(&mut p, &[127]);
        typed(&mut p, b"\n");
        assert_eq!(read_by_the_program(&mut p), b"a\xc3\n");
    }

    #[test]
    fn a_terminal_with_echo_off_does_not_show_the_backspace() {
        // This is the password prompt: `ECHO` decides WHETHER to echo and
        // `ECHOE` only decides HOW. Testing `ECHO | ECHOE` makes every
        // backspace visible on a terminal that was asked to show nothing,
        // which says how long the password is.
        let mut p = pty();
        p.termios.c_lflag &= !ECHO;
        typed(&mut p, b"secreto");
        assert!(shown_on_screen(&mut p).is_empty());
        typed(&mut p, &[127]);
        assert!(
            shown_on_screen(&mut p).is_empty(),
            "el borrado no puede delatar la longitud"
        );
        typed(&mut p, b"\n");
        assert_eq!(read_by_the_program(&mut p), b"secret\n");
    }

    #[test]
    fn a_backspace_on_an_empty_line_shows_nothing() {
        // There is nothing to rub out, and a rubout would eat the prompt.
        let mut p = pty();
        typed(&mut p, &[127]);
        assert!(shown_on_screen(&mut p).is_empty());
    }

    #[test]
    fn an_erase_character_switched_off_is_just_a_byte() {
        let mut p = pty();
        p.termios.c_cc[VERASE] = VDISABLE;
        typed(&mut p, b"ab");
        typed(&mut p, &[0]);
        typed(&mut p, b"\n");
        assert_eq!(read_by_the_program(&mut p), b"ab\x00\n");
    }

    // ----------------------------------------------------------------- eof

    #[test]
    fn ctrl_d_on_an_empty_line_ends_the_read() {
        // The comment on this branch has always promised a zero-length read.
        // Committing an empty line delivers nothing, so the reader found an
        // empty queue and blocked: end-of-input never arrived and the only
        // way out was closing the master.
        let mut p = pty();
        typed(&mut p, &[4]); // Ctrl-D
        assert!(p.eof_pending);

        let s = slave_of(p);
        let mut buf = [0u8; 8];
        assert_eq!(s.read_at(0, &mut buf).unwrap(), 0, "fin de entrada");
    }

    #[test]
    fn ctrl_d_after_a_partial_line_hands_it_over_without_a_newline() {
        // What `read` returns for a line the user did not end with Enter.
        let mut p = pty();
        typed(&mut p, b"hola");
        typed(&mut p, &[4]);
        assert!(!p.eof_pending, "habia linea, asi que no es fin de entrada");
        assert_eq!(read_by_the_program(&mut p), b"hola");
    }

    #[test]
    fn the_end_of_input_is_consumed_by_the_read_it_ends() {
        // Ctrl-D ends one read. A shell that keeps reading after it blocks
        // again rather than spinning on an end-of-input that never clears.
        let mut p = pty();
        typed(&mut p, &[4]);
        let s = slave_of(p);
        let mut buf = [0u8; 8];
        assert_eq!(s.read_at(0, &mut buf).unwrap(), 0);
        assert_eq!(s.read_at(0, &mut buf), Err(FsError::Again));
    }

    #[test]
    fn poll_and_read_agree_about_a_pending_end_of_input() {
        // Three places answer "can the slave read yet?". A `poll` that says
        // no on a pending Ctrl-D parks a reader on input that `read_at` would
        // have handed over right away.
        let mut p = pty();
        typed(&mut p, &[4]);
        let s = slave_of(p);
        assert!(s.poll().unwrap().read, "poll tiene que decir que si");
        let mut buf = [0u8; 8];
        assert_eq!(s.read_at(0, &mut buf).unwrap(), 0);
        assert!(!s.poll().unwrap().read, "y que no en cuanto se consume");
    }

    // ------------------------------------------------------------- signals

    #[test]
    fn a_closed_master_ends_the_read_and_keeps_ending_it() {
        // The other end-of-input: the terminal went away. Unlike Ctrl-D this
        // one does not come back, so it is not consumed by a read.
        let mut p = pty();
        p.master_open = false;
        let s = slave_of(p);
        let mut buf = [0u8; 8];
        assert_eq!(s.read_at(0, &mut buf).unwrap(), 0);
        assert_eq!(s.read_at(0, &mut buf).unwrap(), 0, "sigue cerrado");
        assert!(s.poll().unwrap().read);
    }

    #[test]
    fn queued_input_outranks_a_pending_end_of_input() {
        // Ctrl-D after a full line: the line is delivered first and the
        // end-of-input waits for the read after it.
        let mut p = pty();
        typed(&mut p, b"hola\n");
        typed(&mut p, &[4]);
        let s = slave_of(p);
        let mut buf = [0u8; 16];
        let n = s.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"hola\n");
        assert_eq!(s.read_at(0, &mut buf).unwrap(), 0);
    }

    #[test]
    fn a_signal_character_does_not_enter_the_line() {
        let mut p = pty();
        typed(&mut p, b"ab");
        typed(&mut p, &[3]); // Ctrl-C
        typed(&mut p, b"\n");
        assert_eq!(read_by_the_program(&mut p), b"ab\n");
    }

    #[test]
    fn a_signal_character_is_shown_as_a_caret_pair() {
        let mut p = pty();
        typed(&mut p, &[3]);
        assert_eq!(shown_on_screen(&mut p), b"^C");
        typed(&mut p, &[26]); // Ctrl-Z, which this discipline did not know
        assert_eq!(shown_on_screen(&mut p), b"^Z");
    }

    #[test]
    fn an_interrupt_character_switched_off_is_just_a_byte() {
        // `stty intr undef` writes a zero into `c_cc`. Comparing against it
        // without checking first turns every NUL byte into a Ctrl-C.
        let mut p = pty();
        p.termios.c_cc[VINTR_CC] = VDISABLE;
        typed(&mut p, b"a");
        typed(&mut p, &[0]);
        typed(&mut p, b"\n");
        assert_eq!(read_by_the_program(&mut p), b"a\x00\n");
    }

    #[test]
    fn an_end_of_input_character_switched_off_is_just_a_byte() {
        // Same rule as the interrupt character: a zero in `c_cc` switches it
        // off, it does not aim it at the NUL byte.
        let mut p = pty();
        p.termios.c_cc[VEOF] = VDISABLE;
        typed(&mut p, &[0]);
        assert!(!p.eof_pending, "un NUL no es un Ctrl-D");
        typed(&mut p, b"\n");
        assert_eq!(read_by_the_program(&mut p), b"\x00\n");
    }

    #[test]
    fn a_line_longer_than_the_queue_does_not_grow_it() {
        // The cap has to hold on the canonical path too, and that is a second
        // place answering the same question: a terminal nobody reads from can
        // otherwise take the kernel's memory with it one full line at a time.
        let mut p = pty();
        p.termios.c_lflag &= !ECHO; // keep the output queue out of it
        typed(&mut p, &vec![b'x'; N_TTY_BUF_SIZE + 10]);
        // The newline does not fit either, so the line is not handed over: the
        // user still has VERASE to shorten it with, which is the whole reason
        // the bytes kept being taken.
        typed(&mut p, b"\n");
        assert_eq!(p.canon.len(), N_TTY_BUF_SIZE);
        assert!(p.input.is_empty());
        // One column back and the line ends.
        typed(&mut p, &[0x7f]);
        typed(&mut p, b"\n");
        assert_eq!(p.input.len(), N_TTY_BUF_SIZE);
        assert!(p.canon.is_empty());
    }

    #[test]
    fn a_raw_terminal_gets_ctrl_c_as_a_byte() {
        let mut p = pty();
        p.termios.c_lflag &= !(L_ISIG | ICANON);
        typed(&mut p, &[3]);
        assert_eq!(read_by_the_program(&mut p), &[3]);
    }

    // ------------------------------------------------------------ raw mode

    #[test]
    fn raw_mode_hands_every_byte_over_at_once() {
        let mut p = pty();
        p.termios.c_lflag &= !ICANON;
        typed(&mut p, b"ab");
        assert_eq!(read_by_the_program(&mut p), b"ab");
    }

    // -------------------------------------------------------------- output

    #[test]
    fn the_programs_newlines_get_a_carriage_return_in_front() {
        // A terminal needs CR-LF to return the cursor to column zero;
        // without ONLCR the output walks off to the right.
        let mut p = pty();
        p.slave_output(b"a\nb\n");
        assert_eq!(shown_on_screen(&mut p), b"a\r\nb\r\n");
    }

    #[test]
    fn opost_off_leaves_the_output_exactly_as_written() {
        let mut p = pty();
        p.termios.c_oflag &= !O_OPOST;
        p.slave_output(b"a\nb");
        assert_eq!(shown_on_screen(&mut p), b"a\nb");
    }

    #[test]
    fn opost_on_its_own_is_not_onlcr() {
        // This site asked for `OPOST && ONLCR` together, so `stty opost
        // -onlcr` behaved like raw output instead of post-processed output
        // with nothing to translate.
        let mut p = pty();
        p.termios.c_oflag = O_OPOST;
        p.slave_output(b"a\nb");
        assert_eq!(shown_on_screen(&mut p), b"a\nb");
    }

    #[test]
    fn ocrnl_and_onocr_reach_the_screen() {
        let mut p = pty();
        p.termios.c_oflag = O_OPOST | O_OCRNL;
        p.slave_output(b"a\rb");
        assert_eq!(shown_on_screen(&mut p), b"a\nb");

        let mut p = pty();
        p.termios.c_oflag = O_OPOST | O_ONOCR;
        p.slave_output(b"\rab\r");
        assert_eq!(shown_on_screen(&mut p), b"ab\r");
    }

    #[test]
    fn the_column_carries_across_separate_writes() {
        // Where the last write left the cursor is where the next one starts.
        let mut p = pty();
        p.termios.c_oflag = O_OPOST | O_ONOCR;
        p.slave_output(b"ab");
        p.slave_output(b"\r");
        assert_eq!(shown_on_screen(&mut p), b"ab\r");
    }

    // --------------------------------------------------------------- limits

    #[test]
    fn neither_queue_grows_without_end() {
        // A terminal nobody is reading must not take the kernel's memory
        // with it.
        let mut p = pty();
        p.slave_output(&vec![b'x'; TTY_OUTPUT_CAP + 64]);
        assert_eq!(p.output.len(), TTY_OUTPUT_CAP);

        let mut p = pty();
        p.termios.c_lflag &= !ICANON;
        typed(&mut p, &vec![b'x'; N_TTY_BUF_SIZE + 10]);
        assert_eq!(p.input.len(), N_TTY_BUF_SIZE);
    }
}
