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
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use core::task::{Context, Poll};
use core::time::Duration;
use kernel_hal::console::ConsoleWinSize;
use lazy_static::lazy_static;
use lock::Mutex;
use rcore_fs::vfs::*;

// termios c_iflag bits
const IXON: u32 = 0x0400;
const IXANY: u32 = 0x0800;
// termios c_lflag bits
const ISIG: u32 = 0x0001;
const ICANON: u32 = 0x0002;
const ECHO: u32 = 0x0008;
const ECHOE: u32 = 0x0010;
const ECHONL: u32 = 0x0040;
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
    /// How many bytes have ever been taken off the front of `input`, so a
    /// position in it can be named independently of what is still queued: the
    /// back of the queue is at `input_consumed + input.len()`.
    input_consumed: usize,
    /// Where a VEOF cut a canonical line short, as positions in the sequence
    /// `input_consumed` counts. Linux keeps the same thing as an `EOF` bit in
    /// the line discipline's `read_flags` bitmap, and `n_tty_read` stops there
    /// exactly as it stops on a newline.
    ///
    /// Without it a line committed by VEOF -- which carries no terminator --
    /// ran straight into the next one: `printf 'abc'; ^D; printf 'def\n'` came
    /// back from a single `read` as `abcdef\n`, so a program reading a line at
    /// a time (every prompt, every `read` builtin) saw the two joined. The
    /// marks cost nothing when nobody presses Ctrl-D: the queue stays empty.
    eof_marks: VecDeque<usize>,
    /// Bytes available to the master's `read` (program output + echoed input).
    output: VecDeque<u8>,
    /// Where the cursor sits on the master's line. One counter for program
    /// output and echo alike, because they land on one screen: `ONOCR` is
    /// defined in terms of it and `ONLRET` exists to keep it honest. See
    /// [`Termios::output_char`].
    out_column: usize,
    termios: Termios,
    winsize: ConsoleWinSize,
}

impl PtyInner {
    /// Throw away everything on the way in, as `n_tty_flush_buffer` does: the
    /// committed bytes, the line still being assembled, a VEOF waiting to be
    /// reported, and the line marks that went with them. The four moved
    /// together at every one of the four call sites already; keeping the
    /// counter honest is what made it worth a name, because a flush leaves
    /// positions behind that no byte will ever occupy.
    fn flush_input(&mut self) {
        self.input_consumed += self.input.len();
        self.input.clear();
        self.canon.clear();
        self.eof_pending = false;
        self.eof_marks.clear();
    }
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
    /// Double-Ctrl-C arm: first VINTR for a pgrp stores it here; a second
    /// VINTR for the same pgrp escalates to SIGKILL (see
    /// [`crate::process::interrupt_or_force_pgrp`]).
    ctrl_c_armed_pgid: AtomicI32,
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
    /// Non-canonical `VTIME`: the monotonic deadline (ns) the slave read in
    /// progress is waiting on. Zero means no timer is running.
    vtime_deadline_ns: AtomicU64,
    /// Non-canonical `VMIN`: how many queued bytes would end the slave read
    /// that parked, already lowered to what its buffer can take. The thing
    /// that wakes the reader sees the queue but not the buffer, so the read
    /// leaves the number behind for it.
    slave_need: AtomicUsize,
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
    ///
    /// This is `poll(2)`'s answer, and deliberately not the blocking read's:
    /// `poll` reports a terminal readable as soon as one byte is queued,
    /// whatever `VMIN` says, while the read itself may still have to wait for
    /// more. See [`Pty::slave_read_ready`].
    fn slave_readable(&self) -> bool {
        let inner = self.inner.lock();
        !inner.input.is_empty() || inner.eof_pending || self.master_closed.load(Ordering::Relaxed)
    }

    /// Whether a write to the master would store anything.
    ///
    /// `poll(2)` promising POLLOUT on a queue that cannot take a byte is how a
    /// writer that polls before it writes becomes a spin, so this has to give
    /// the same answer as [`Pty::master_write`] -- including the case where
    /// the queue is full and the discipline keeps taking bytes anyway.
    fn master_writable(&self) -> bool {
        let inner = self.inner.lock();
        input_room(
            inner.input.len(),
            inner.canon.len(),
            inner.termios.c_lflag & ICANON != 0,
        ) != InputRoom::Full
    }

    /// Whether a write to the slave would store anything.
    fn slave_writable(&self) -> bool {
        self.inner.lock().output.len() < TTY_OUTPUT_CAP
    }

    /// Whether the *blocking* slave read parked in [`Pty::slave_read`] can now
    /// make progress.
    ///
    /// `File::read` is `loop { read_at; on EAGAIN await async_poll }`, so this
    /// and `slave_read` have to give the same answer to the same queue. Wiring
    /// the wait to `slave_readable` instead would spin the CPU between the
    /// first byte and the `VMIN`th: `read_at` says not yet, the wait says
    /// ready, round again with nothing changed.
    fn slave_read_ready(&self) -> bool {
        let inner = self.inner.lock();
        if inner.eof_pending || self.master_closed.load(Ordering::Relaxed) {
            return true;
        }
        if inner.termios.canonical() {
            return !inner.input.is_empty();
        }
        if inner.input.len() >= self.slave_need.load(Ordering::Relaxed).max(1) {
            return true;
        }
        drop(inner);
        // Short of the count, only the VTIME timer can end this wait. It is
        // only ever armed where returning on it is allowed, so reaching it is
        // enough on its own.
        self.vtime_expired()
    }

    /// True when the `VTIME` timer armed for the slave read in progress has
    /// run out.
    fn vtime_expired(&self) -> bool {
        let dl = self.vtime_deadline_ns.load(Ordering::Acquire);
        dl != 0 && kernel_hal::timer::timer_now().as_nanos() as u64 >= dl
    }

    /// Start the `VTIME` timer for a slave read that is about to wait, if it
    /// asked for one and has not started it already.
    ///
    /// With `VMIN > 0` the timer measures the gap *between* bytes, so it does
    /// not start until the first one is in. Starting it at the read would turn
    /// every `min 1 time N` program — the ordinary terminal setting — into one
    /// that comes back empty on an idle terminal, which a reader reads as end
    /// of file.
    fn arm_vtime(&self, termios: &Termios, queued: usize) {
        if termios.vtime() == 0 || (termios.vmin() > 0 && queued == 0) {
            return;
        }
        if self.vtime_deadline_ns.load(Ordering::Acquire) != 0 {
            return;
        }
        let now = kernel_hal::timer::timer_now().as_nanos() as u64;
        // VTIME is in deciseconds.
        let deadline = now.saturating_add((termios.vtime() as u64) * 100_000_000);
        self.vtime_deadline_ns.store(deadline, Ordering::Release);
    }

    /// Stop the `VTIME` timer: the read it belonged to is over.
    fn clear_vtime(&self) {
        self.vtime_deadline_ns.store(0, Ordering::Release);
    }

    /// Feed bytes written to the master through the input line discipline.
    ///
    /// Returns how many of `data` were taken, which may be fewer than were
    /// offered: the input queue is bounded, and a short write is how the
    /// writer is told to stop.
    fn master_write(&self, data: &[u8]) -> usize {
        let mut consumed = data.len();
        let mut wake_slave = false;
        let mut wake_master = false;
        let mut clear_master_readable = false;
        let mut clear_slave_readable = false;
        let mut signals: alloc::vec::Vec<Signal> = alloc::vec::Vec::new();
        {
            let mut inner = self.inner.lock();
            let iflag = inner.termios.c_iflag;
            let lflag = inner.termios.c_lflag;
            let cc = inner.termios.c_cc;
            let kill_echo = inner.termios.kill_echo();
            let utf8 = inner.termios.utf8_input();
            // Copies, not borrows: `inner` is a guard, so two field borrows of
            // it in one expression are two borrows of the guard.
            let termios = inner.termios;
            let mut out_col = inner.out_column;
            for (i, &b) in data.iter().enumerate() {
                // Room for one more? The rule is shared with the console and
                // with `fs/devfs/pty.rs`, so it lives in `ioctl.rs`. Both
                // queues here are bytes already, which is the unit it wants.
                let overflow =
                    match input_room(inner.input.len(), inner.canon.len(), lflag & ICANON != 0) {
                        InputRoom::Store => false,
                        InputRoom::Process => true,
                        // This end *has* a writer to answer, so it answers.
                        InputRoom::Full => {
                            consumed = i;
                            break;
                        }
                    };
                // What the byte becomes on the way in: `ISTRIP`, `IUCLC`,
                // then the CR/NL rules, in that order. The block used to be
                // here, again in the console and again in `fs/devfs/pty.rs`,
                // and none of the three had `ISTRIP` or `IUCLC`; it is one
                // question per input byte, so it is answered in `ioctl.rs`.
                let c = match termios.input_char(b) {
                    Some(c) => c,
                    None => continue,
                };

                // Literal-next (VLNEXT, Ctrl-V): the previous byte armed it, so
                // insert this one verbatim, skipping signal/edit interpretation.
                if inner.lnext {
                    inner.lnext = false;
                    if lflag & ICANON != 0 {
                        if !overflow {
                            inner.canon.push_back(c);
                        }
                        if echo_byte(&mut inner.output, c, &termios, &mut out_col) {
                            wake_master = true;
                        }
                    } else {
                        inner.input.push_back(c);
                        if echo_byte(&mut inner.output, c, &termios, &mut out_col) {
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
                    // Ctrl-C: first press → SIGINT; second for the same pgrp →
                    // SIGKILL so a hung job can be torn down without closing
                    // the terminal emulator (foot, etc.).
                    if cc[VINTR] != 0 && c == cc[VINTR] {
                        if lflag & NOFLSH == 0 {
                            inner.flush_input();
                            clear_slave_readable = true;
                        }
                        if inner.stopped {
                            inner.stopped = false;
                            wake_master = true;
                        }
                        let pgid = self.fg_pgrp.load(Ordering::Relaxed);
                        let sent =
                            crate::process::interrupt_or_force_pgrp(pgid, &self.ctrl_c_armed_pgid);
                        if lflag & ECHO != 0 {
                            let label: &[u8] = if sent == Signal::SIGKILL {
                                b"^C (killed)"
                            } else {
                                b"^C"
                            };
                            out_extend(&mut inner.output, label);
                            out_extend(&mut inner.output, b"\r\n");
                            wake_master = true;
                        }
                        continue;
                    }
                    let sig = if cc[VQUIT] != 0 && c == cc[VQUIT] {
                        Some((Signal::SIGQUIT, "^\\"))
                    } else if cc[VSUSP] != 0 && c == cc[VSUSP] {
                        Some((Signal::SIGTSTP, "^Z"))
                    } else {
                        None
                    };
                    if let Some((signal, label)) = sig {
                        if lflag & NOFLSH == 0 {
                            inner.flush_input();
                            clear_slave_readable = true;
                        }
                        // Resume any output frozen by Ctrl-S so the signalled
                        // program isn't left blocked behind a stopped terminal.
                        if inner.stopped {
                            inner.stopped = false;
                            wake_master = true;
                        }
                        if lflag & ECHO != 0 {
                            out_extend(&mut inner.output, label.as_bytes());
                            out_extend(&mut inner.output, b"\r\n");
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
                        //
                        // One rubout per column, so the bytes that only
                        // continue a character do not each ask for one.
                        let echo = lflag & ECHO != 0;
                        let rub = |inner: &mut PtyInner, wake: &mut bool| {
                            let b = match inner.canon.pop_back() {
                                Some(b) => b,
                                None => return,
                            };
                            if echo && !(utf8 && utf8_continuation(b)) {
                                out_extend(&mut inner.output, b"\x08 \x08");
                                *wake = true;
                            }
                        };
                        while matches!(inner.canon.back(), Some(&b' ') | Some(&b'\t')) {
                            rub(&mut inner, &mut wake_master);
                        }
                        while let Some(&b) = inner.canon.back() {
                            if b == b' ' || b == b'\t' {
                                break;
                            }
                            rub(&mut inner, &mut wake_master);
                        }
                    } else if iexten && cc[VREPRINT] != 0 && c == cc[VREPRINT] {
                        // Reprint the pending line on a fresh line.
                        if lflag & ECHO != 0 {
                            if lflag & ECHOCTL != 0 {
                                out_extend(&mut inner.output, b"^R");
                            }
                            out_extend(&mut inner.output, b"\r\n");
                            let pending: alloc::vec::Vec<u8> =
                                inner.canon.iter().copied().collect();
                            for b in pending {
                                echo_byte(&mut inner.output, b, &termios, &mut out_col);
                            }
                            wake_master = true;
                        }
                    } else if iexten && cc[VLNEXT] != 0 && c == cc[VLNEXT] {
                        inner.lnext = true;
                        if lflag & ECHO != 0 && lflag & ECHOCTL != 0 {
                            out_extend(&mut inner.output, b"^\x08");
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
                        //
                        // And it takes a *character*, not a byte. The line is
                        // kept as bytes, so under IUTF8 a `ñ` is two of them
                        // and taking one leaves half a character behind, which
                        // the program then reads as a byte that cannot be
                        // decoded. One rubout either way: however many bytes
                        // spell the character, it stands in one column.
                        let n = if utf8 {
                            let tail: alloc::vec::Vec<u8> = inner.canon.iter().copied().collect();
                            utf8_erase_len(&tail)
                        } else {
                            usize::from(!inner.canon.is_empty())
                        };
                        for _ in 0..n {
                            inner.canon.pop_back();
                        }
                        if n > 0 && lflag & ECHO != 0 {
                            if lflag & ECHOE != 0 {
                                out_extend(&mut inner.output, b"\x08 \x08");
                            } else {
                                out_push(&mut inner.output, cc[VERASE]);
                            }
                            wake_master = true;
                        }
                    } else if cc[VKILL] != 0 && c == cc[VKILL] {
                        let n = if utf8 {
                            inner
                                .canon
                                .iter()
                                .filter(|&&b| !utf8_continuation(b))
                                .count()
                        } else {
                            inner.canon.len()
                        };
                        inner.canon.clear();
                        // ECHOKE rubs the line out, ECHOK leaves it on screen
                        // and moves to the next one, neither shows anything.
                        // This end always rubbed out, which is the answer to a
                        // flag the cooked default does not even set: ECHOK is
                        // on there and ECHOKE is off, so the stock Ctrl-U is a
                        // newline. The console's discipline has had the three
                        // cases since it was written.
                        match kill_echo {
                            KillEcho::Rubout => {
                                // One per column, not per byte.
                                for _ in 0..n {
                                    out_post(
                                        &mut inner.output,
                                        &termios,
                                        &mut out_col,
                                        b"\x08 \x08",
                                    );
                                }
                                wake_master = true;
                            }
                            KillEcho::Newline => {
                                out_post(&mut inner.output, &termios, &mut out_col, b"\n");
                                wake_master = true;
                            }
                            KillEcho::Nothing => {}
                        }
                    } else if cc[VEOF] != 0 && c == cc[VEOF] {
                        // Commit the pending line without a newline; an empty
                        // line signals end-of-file to the reader (eof_pending).
                        let had_data = !inner.canon.is_empty();
                        while let Some(ch) = inner.canon.pop_front() {
                            inner.input.push_back(ch);
                        }
                        if had_data {
                            // The line ends here although no terminator does,
                            // so the mark is what tells the reader to stop.
                            let end = inner.input_consumed + inner.input.len();
                            inner.eof_marks.push_back(end);
                            wake_slave = true;
                        } else {
                            inner.eof_pending = true;
                            wake_slave = true;
                        }
                    } else {
                        if !overflow {
                            inner.canon.push_back(c);
                        }
                        if echo_byte(&mut inner.output, c, &termios, &mut out_col) {
                            wake_master = true;
                        }
                        // Commit the line on newline or a configured EOL delimiter.
                        // Dropped along with everything else while overflowing:
                        // a line that cannot hold its own terminator is not a
                        // line the reader should be handed.
                        let is_eol = c == b'\n'
                            || (cc[VEOL] != 0 && c == cc[VEOL])
                            || (cc[VEOL2] != 0 && c == cc[VEOL2]);
                        if is_eol && !overflow {
                            while let Some(ch) = inner.canon.pop_front() {
                                inner.input.push_back(ch);
                            }
                            wake_slave = true;
                        }
                    }
                } else {
                    inner.input.push_back(c);
                    if echo_byte(&mut inner.output, c, &termios, &mut out_col) {
                        wake_master = true;
                    }
                    wake_slave = true;
                }
            }
            inner.out_column = out_col;
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
        consumed
    }

    /// Program output written to the slave, post-processed for the master.
    ///
    /// Returns how much was taken. Short when the master is not keeping up:
    /// see [`TTY_OUTPUT_CAP`].
    fn slave_write(&self, data: &[u8]) -> usize {
        if data.is_empty() {
            return 0;
        }
        let mut n = 0;
        let mut sent = false;
        {
            let mut inner = self.inner.lock();
            let termios = inner.termios;
            let mut out_col = inner.out_column;
            for &b in data {
                // Under ONLCR a `\n` leaves as `\r\n`: two bytes of the
                // budget, and never split across the cap. A `\r` left alone at
                // the end of the queue is not the line ending the program on
                // the other side is waiting for.
                //
                // `Posted::Nothing` is not a failure: the rule swallowed the
                // byte (`ONOCR` on a carriage return at column zero), so it is
                // consumed. Counting it as a short write would have the writer
                // offer the same byte again for ever.
                match out_post(&mut inner.output, &termios, &mut out_col, &[b]) {
                    Posted::Full => break,
                    Posted::Sent => sent = true,
                    Posted::Nothing => {}
                }
                n += 1;
            }
            inner.out_column = out_col;
        }
        // Not `n > 0`: a write of nothing but swallowed carriage returns is
        // consumed in full and puts not one byte in front of the master.
        if sent {
            self.wake_master();
        }
        n
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
        let canon = inner.termios.canonical();
        if inner.input.is_empty() {
            if inner.eof_pending {
                inner.eof_pending = false;
                self.clear_vtime();
                drop(inner);
                if !self.slave_readable() {
                    self.slave_bus.lock().clear(Event::READABLE);
                }
                return Ok(0); // VEOF on empty line → EOF
            }
            if self.master_closed.load(Ordering::Relaxed) {
                self.clear_vtime();
                drop(inner);
                return Ok(0); // master closed → EOF
            }
        }
        // Non-canonical mode: VMIN and VTIME say when a read is over, not
        // whether the queue happens to be empty. Without them a `min 0` read —
        // which POSIX says comes back at once, with nothing if there is
        // nothing — waited for a byte that may never be typed.
        // A hang-up or a pending EOF ends the wait whatever VMIN says: no more
        // input is coming, so holding bytes back for a count that can never be
        // reached would strand them. `slave_read_ready` says the same, and the
        // two have to agree.
        let hungup = inner.eof_pending || self.master_closed.load(Ordering::Relaxed);
        let limit = if canon || hungup {
            buf.len()
        } else {
            let queued = inner.input.len();
            match inner
                .termios
                .noncanon_read(queued, buf.len(), self.vtime_expired())
            {
                TtyRead::Take(n) => n,
                TtyRead::Now => {
                    self.clear_vtime();
                    drop(inner);
                    return Ok(0);
                }
                TtyRead::Wait => {
                    self.slave_need
                        .store(inner.termios.noncanon_need(buf.len()), Ordering::Relaxed);
                    self.arm_vtime(&inner.termios, queued);
                    drop(inner);
                    return Err(FsError::Again);
                }
            }
        };
        if inner.input.is_empty() {
            // Canonical mode only: non-canonical never reaches here empty.
            drop(inner);
            return Err(FsError::Again);
        }
        self.clear_vtime();
        let mut n = 0;
        while n < limit {
            // A VEOF boundary ends this read as a newline would (`n_tty_read`
            // stops on either). Reaching it with nothing read yet means the
            // last read already stopped here, so the mark is spent and this
            // read carries on into the line behind it.
            if inner.eof_marks.front() == Some(&inner.input_consumed) {
                inner.eof_marks.pop_front();
                if n > 0 {
                    break;
                }
            }
            match inner.input.pop_front() {
                Some(b) => {
                    inner.input_consumed += 1;
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
                {
                    let mut inner = self.inner.lock();
                    inner.termios = t;
                    inner.flush_input();
                }
                // Not `input.is_empty()`, which is always true a line above:
                // the slave is ALSO readable when the master has closed, and
                // that readability is an EOF nobody else will re-announce.
                // Clearing it here left a program that had done its last
                // `tcsetattr(TCSAFLUSH)` -- which is what every line editor
                // does on the way out, and what `stty sane` is -- polling a
                // hung-up terminal that never reported ready again.
                if !self.slave_readable() {
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
                let old = self.fg_pgrp.swap(pgid, Ordering::Relaxed);
                if old != pgid {
                    crate::process::clear_interrupt_arm(&self.ctrl_c_armed_pgid);
                }
                Ok(0)
            }
            // `tcflush(3)`: throw away what is queued and not yet read or
            // written. Answering `Ok(0)` and discarding nothing is a failure
            // nobody attributes to the kernel: `getpass(3)`, sudo, ssh and
            // every other password prompt call `tcflush(fd, TCIFLUSH)` first
            // precisely so that what was typed ahead does NOT land in the
            // password read -- and with the no-op it did, on a terminal with
            // echo off, so the typist never saw where it went. `stty`,
            // readline and screen flush for the same reason.
            //
            // The queue selector arrives BY VALUE (the ioctl takes an int, not
            // a pointer), so `data` IS the selector, and a selector outside the
            // three is `EINVAL` (`tty_perform_flush`'s `default:`).
            //
            // The queues are this end's, as `tty_perform_flush` works on the
            // tty it was called on and as `FIONREAD` below already reads them:
            // the master reads program output and writes keystrokes, the slave
            // the other way round.
            TCFLSH => {
                const TCIFLUSH: usize = 0;
                const TCOFLUSH: usize = 1;
                const TCIOFLUSH: usize = 2;
                let (read_side, write_side) = match data {
                    TCIFLUSH => (true, false),
                    TCOFLUSH => (false, true),
                    TCIOFLUSH => (true, true),
                    _ => return Err(FsError::InvalidParam),
                };
                let flush_input = if is_master { write_side } else { read_side };
                let flush_output = if is_master { read_side } else { write_side };
                {
                    let mut inner = self.inner.lock();
                    if flush_input {
                        // The line being assembled goes with the bytes already
                        // committed (`n_tty_flush_buffer` resets both), and so
                        // does a VEOF waiting to be reported: it belongs to a
                        // line nobody will ever read now.
                        inner.flush_input();
                    }
                    if flush_output {
                        inner.output.clear();
                    }
                }
                if flush_input && !self.slave_readable() {
                    self.slave_bus.lock().clear(Event::READABLE);
                }
                if flush_output && !self.master_readable() {
                    self.master_bus.lock().clear(Event::READABLE);
                }
                Ok(0)
            }
            TIOCSCTTY | TIOCNOTTY => Ok(0),
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

/// Append one byte bound for the master, if [`TTY_OUTPUT_CAP`] has room.
/// Returns whether it went in.
///
/// Every path that puts something in front of the master comes through here or
/// through [`out_extend`]: program output, the echo of a keystroke, the `^C`
/// label, a rubout. Leaving one of them to push straight onto the queue would
/// be enough to lose the bound, because the ones that are not program output
/// are driven by input the discipline *consumes* -- a master sending nothing
/// but Ctrl-C stores not one byte of input and still asks for `^C\r\n` each
/// time.
fn out_push(out: &mut VecDeque<u8>, b: u8) -> bool {
    if out.len() >= TTY_OUTPUT_CAP {
        return false;
    }
    out.push_back(b);
    true
}

/// Append a run bound for the master: all of it, or none of it.
///
/// The runs that come through here are a label or a `\x08 \x08` rubout, and
/// half of either is worse on screen than neither.
fn out_extend(out: &mut VecDeque<u8>, bytes: &[u8]) -> bool {
    if out.len() + bytes.len() > TTY_OUTPUT_CAP {
        return false;
    }
    out.extend(bytes);
    true
}

/// What happened to a run handed to [`out_post`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Posted {
    /// The run went out whole.
    Sent,
    /// The rule swallowed every byte of it, so there was nothing to send and
    /// nothing is wrong. Only `ONOCR` does this.
    ///
    /// Worth its own answer rather than sharing one with [`Posted::Full`]:
    /// `slave_write` stops at a full queue and reports a short write, and a
    /// writer told its carriage return did not fit would offer the same byte
    /// again forever.
    Nothing,
    /// The queue had no room for the whole run, so none of it went.
    Full,
}

/// Post-process a short run and put it in front of the master, whole or not
/// at all.
///
/// Every byte that reaches the master goes through here, program output and
/// echo alike, and they share one column -- which is what Linux does:
/// `__process_echoes` hands each byte it is about to put out to
/// `do_output_char` whenever `OPOST` is set (`drivers/tty/n_tty.c`). The
/// column has to be shared for the same reason: it is one line on one screen,
/// whatever put the bytes on it.
fn out_post(out: &mut VecDeque<u8>, termios: &Termios, column: &mut usize, run: &[u8]) -> Posted {
    // The longest run is the three bytes of a rubout, and the rule turns at
    // most one byte into two.
    let mut buf = [0u8; 8];
    let mut n = 0;
    let mut col = *column;
    for &b in run {
        for &o in termios.output_char(b, &mut col).as_bytes() {
            buf[n] = o;
            n += 1;
        }
    }
    if n == 0 {
        // Nothing to send, but the cursor may still have moved.
        *column = col;
        return Posted::Nothing;
    }
    if out_extend(out, &buf[..n]) {
        *column = col;
        Posted::Sent
    } else {
        Posted::Full
    }
}

/// Echo one input byte to the master read side. Returns whether anything was
/// written. Mirrors the console line discipline's `echo_char`.
fn echo_byte(out: &mut VecDeque<u8>, c: u8, termios: &Termios, column: &mut usize) -> bool {
    let lflag = termios.c_lflag;
    if lflag & ECHO == 0 {
        // ECHONL is the one thing a terminal with echo off still shows, and it
        // has one caller: getpass(3) clears ECHO and sets ECHONL so the Enter
        // that ends a password still moves the cursor off the prompt line.
        // Without it the password is invisible *and* so is the newline, and
        // whatever prints next lands on top of the prompt.
        let echonl = lflag & ICANON != 0 && lflag & ECHONL != 0;
        if !(c == b'\n' && echonl) {
            return false;
        }
    }
    // The answer is whether the byte actually went out, not whether the flags
    // said it should: the queue is bounded, and a caller that takes `true` for
    // "the master has something to read" would wake it for nothing. A byte the
    // rule swallowed did not go out either.
    let posted = match c {
        // A rubout is one thing on screen, so it goes out whole or not.
        0x7f | 0x08 => out_post(out, termios, column, b"\x08 \x08"),
        // `^X`, likewise. `\n`, `\r` and `\t` are control characters that
        // the terminal acts on rather than shows, so they are not captioned.
        c if c < 0x20 && c != b'\n' && c != b'\r' && c != b'\t' && lflag & ECHOCTL != 0 => {
            out_post(out, termios, column, &[b'^', c + 64])
        }
        c => out_post(out, termios, column, &[c]),
    };
    posted == Posted::Sent
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
            input_consumed: 0,
            eof_marks: VecDeque::new(),
            output: VecDeque::new(),
            out_column: 0,
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
        ctrl_c_armed_pgid: AtomicI32::new(0),
        slave_open: AtomicI32::new(0),
        slave_ever_open: AtomicBool::new(false),
        master_closed: AtomicBool::new(false),
        locked: AtomicBool::new(false),
        vtime_deadline_ns: AtomicU64::new(0),
        slave_need: AtomicUsize::new(0),
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
        let old = self.pty.fg_pgrp.swap(pgid, Ordering::Relaxed);
        if old != pgid {
            crate::process::clear_interrupt_arm(&self.pty.ctrl_c_armed_pgid);
        }
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
    /// Slave reads only: a waker armed for the `VTIME` deadline, so a
    /// `min 0 time N` read comes back after N deciseconds instead of waiting
    /// for a byte. Nothing else would wake it — the only other source of
    /// wakeups on this end is the master writing, which is exactly what the
    /// timer is there to stop depending on.
    timer: Option<kernel_hal::timer_waker::TimerWakerSlot>,
    /// Whether this is the slave's read side, the only one with a line
    /// discipline and therefore the only one with a timer.
    slave: bool,
}

impl Drop for PtyReadFuture<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.sub_id.take() {
            self.bus.lock().unsubscribe(id);
        }
        kernel_hal::timer_waker::kill_timer_waker(&mut self.timer);
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
            kernel_hal::timer_waker::kill_timer_waker(&mut this.timer);
            return Poll::Ready(ready);
        }
        if this.sub_id.is_none() {
            let waker = cx.waker().clone();
            this.sub_id = this.bus.lock().subscribe(Box::new(move |_| {
                waker.wake_by_ref();
                true
            }));
        }
        // A VTIME read has a deadline nothing else will announce.
        if this.slave {
            let dl = this.pty.vtime_deadline_ns.load(Ordering::Acquire);
            if dl != 0 {
                kernel_hal::timer_waker::ensure_timer_waker(
                    &mut this.timer,
                    Duration::from_nanos(dl),
                    cx,
                );
            }
        }
        // Re-check after subscribing: data may have arrived in the window
        // between the first check and the subscription, which would otherwise
        // be a missed wakeup.
        if (this.check)(this.pty) {
            if let Some(id) = this.sub_id.take() {
                this.bus.lock().unsubscribe(id);
            }
            kernel_hal::timer_waker::kill_timer_waker(&mut this.timer);
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
    slave: bool,
) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
    Box::pin(PtyReadFuture {
        pty,
        bus,
        check,
        sub_id: None,
        timer: None,
        slave,
    })
}

impl INode for PtyMaster {
    fn read_at(&self, _offset: usize, buf: &mut [u8]) -> Result<usize> {
        self.pty.master_read(buf)
    }
    fn write_at(&self, _offset: usize, buf: &[u8]) -> Result<usize> {
        let n = self.pty.master_write(buf);
        // Nothing fitted. A `write(2)` that returns 0 tells a program it made
        // no progress on a request that asked for some, and the loop it is
        // sitting in calls straight back with the same buffer: that is a spin,
        // not a wait. The queue is full, so say that instead.
        if n == 0 && !buf.is_empty() {
            return Err(FsError::Again);
        }
        Ok(n)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: self.pty.master_readable(),
            write: self.pty.master_writable(),
            error: false,
            hangup: false,
        })
    }
    fn async_poll<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
        readable_future(
            &self.pty,
            self.pty.master_bus.clone(),
            Pty::master_readable,
            false,
        )
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
        let n = self.pty.slave_write(buf);
        // As on the master end: no room is EAGAIN, never a zero-byte write.
        if n == 0 && !buf.is_empty() {
            return Err(FsError::Again);
        }
        Ok(n)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: self.pty.slave_readable(),
            write: self.pty.slave_writable(),
            error: false,
            hangup: false,
        })
    }
    fn async_poll<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
        readable_future(
            &self.pty,
            self.pty.slave_bus.clone(),
            Pty::slave_read_ready,
            true,
        )
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
    use core::mem::ManuallyDrop;

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
                input_consumed: 0,
                eof_marks: VecDeque::new(),
                output: VecDeque::new(),
                out_column: 0,
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
            ctrl_c_armed_pgid: AtomicI32::new(0),
            slave_open: AtomicI32::new(1),
            slave_ever_open: AtomicBool::new(true),
            master_closed: AtomicBool::new(false),
            locked: AtomicBool::new(false),
            vtime_deadline_ns: AtomicU64::new(0),
            slave_need: AtomicUsize::new(0),
        };
        p
    }

    /// `tcflush(fd, TCIFLUSH)` from the program throws away what was typed
    /// ahead. This used to answer `Ok(0)` and keep every byte, which is how
    /// type-ahead ends up inside a password prompt.
    #[test]
    fn tciflush_from_the_slave_discards_typed_ahead_input() {
        let p = pty();
        p.master_write(b"secret-typed-early\r");
        assert!(!p.inner.lock().input.is_empty());
        assert_eq!(p.ioctl(TCFLSH as u32, 0, false), Ok(0));
        assert!(p.inner.lock().input.is_empty());
        assert!(p.inner.lock().canon.is_empty());
        assert_eq!(slave_reads(&p), Vec::<String>::new());
    }

    /// Half-typed line included: the line being assembled is part of the input
    /// queue (`n_tty_flush_buffer` resets both).
    #[test]
    fn tciflush_discards_a_half_typed_line_too() {
        let p = pty();
        p.master_write(b"half-a-line");
        assert!(!p.inner.lock().canon.is_empty());
        assert_eq!(p.ioctl(TCFLSH as u32, 0, false), Ok(0));
        assert!(p.inner.lock().canon.is_empty());
        p.master_write(b"rest\r");
        assert_eq!(slave_reads(&p), vec![String::from("rest\n")]);
    }

    /// `TCOFLUSH` from the program drops what it wrote and the terminal has
    /// not read yet, and leaves the input queue alone.
    #[test]
    fn tcoflush_from_the_slave_drops_pending_output_only() {
        let p = pty();
        p.master_write(b"typed\r");
        p.slave_write(b"printed");
        assert!(!p.inner.lock().output.is_empty());
        assert_eq!(p.ioctl(TCFLSH as u32, 1, false), Ok(0));
        assert!(p.inner.lock().output.is_empty());
        assert!(!p.inner.lock().input.is_empty());
    }

    /// `TCIOFLUSH` takes both, and the selector is read by value.
    #[test]
    fn tcioflush_takes_both_queues() {
        let p = pty();
        p.master_write(b"typed\r");
        p.slave_write(b"printed");
        assert_eq!(p.ioctl(TCFLSH as u32, 2, false), Ok(0));
        assert!(p.inner.lock().input.is_empty());
        assert!(p.inner.lock().output.is_empty());
    }

    /// A selector outside the three is `EINVAL` (`tty_perform_flush`'s
    /// `default:`), not a silent success over the wrong queue.
    #[test]
    fn an_unknown_flush_selector_is_einval() {
        let p = pty();
        p.master_write(b"typed\r");
        assert_eq!(p.ioctl(TCFLSH as u32, 3, false), Err(FsError::InvalidParam));
        assert_eq!(
            p.ioctl(TCFLSH as u32, usize::MAX, false),
            Err(FsError::InvalidParam)
        );
        // And nothing was thrown away on the way to the error.
        assert!(!p.inner.lock().input.is_empty());
    }

    /// From the master the two queues swap over: the master READS program
    /// output, so its `TCIFLUSH` is the one that drops it.
    #[test]
    fn the_masters_queues_are_the_other_way_round() {
        let p = pty();
        p.master_write(b"typed\r");
        p.slave_write(b"printed");
        assert_eq!(p.ioctl(TCFLSH as u32, 0, true), Ok(0));
        assert!(p.inner.lock().output.is_empty());
        assert!(!p.inner.lock().input.is_empty());
        assert_eq!(p.ioctl(TCFLSH as u32, 1, true), Ok(0));
        assert!(p.inner.lock().input.is_empty());
    }

    /// Flushing the input queue must not un-announce an EOF: the slave is
    /// readable while the master is gone, and nobody re-announces that.
    #[test]
    fn flushing_input_keeps_the_hangup_readable() {
        let p = pty();
        p.master_write(b"typed\r");
        p.master_closed.store(true, Ordering::Relaxed);
        p.slave_bus.lock().set(Event::READABLE);
        assert_eq!(p.ioctl(TCFLSH as u32, 0, false), Ok(0));
        assert!(p.slave_readable());
        assert!(p.slave_bus.lock().events().contains(Event::READABLE));
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

    /// The two INode ends of one pair, for the tests that go in through
    /// `write_at` and `poll` rather than straight at the discipline.
    ///
    /// Never dropped: `Drop for PtyMaster` reaches into the global `PTYS` map,
    /// and these pairs were never registered there. Everything else in this
    /// module keeps to its own `Pty`, and this keeps that true.
    fn ends(p: Arc<Pty>) -> (ManuallyDrop<PtyMaster>, ManuallyDrop<PtySlave>) {
        (
            ManuallyDrop::new(PtyMaster { pty: p.clone() }),
            ManuallyDrop::new(PtySlave { pty: p }),
        )
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

    /// A `Termios` with these `c_lflag` bits and no output post-processing.
    fn lflag(bits: u32) -> Termios {
        let mut t = Termios::default_tty();
        t.c_lflag = bits;
        t.c_oflag = 0;
        t
    }

    /// Echo one byte with a throwaway column, and hand back what went out.
    fn echoed(t: &Termios, c: u8) -> Vec<u8> {
        let mut out = VecDeque::new();
        let mut col = 0usize;
        echo_byte(&mut out, c, t, &mut col);
        Vec::from(out)
    }

    #[test]
    fn echo_byte_renders_each_class_of_character() {
        let mut out = VecDeque::new();
        let mut col = 0usize;
        // ECHO off: nothing written, and the caller is told so.
        assert!(!echo_byte(&mut out, b'a', &lflag(0), &mut col));
        assert!(out.is_empty());

        // Newline becomes CRLF only when OPOST|ONLCR are both on.
        let mut t = lflag(ECHO);
        t.c_oflag = O_OPOST | O_ONLCR;
        let mut out = VecDeque::new();
        let mut col = 0usize;
        assert!(echo_byte(&mut out, b'\n', &t, &mut col));
        assert_eq!(Vec::from(out), b"\r\n");
        assert_eq!(echoed(&lflag(ECHO), b'\n'), b"\n".to_vec());

        // Backspace and DEL both erase visually.
        for c in [0x08u8, DEL] {
            assert_eq!(echoed(&lflag(ECHO), c), b"\x08 \x08".to_vec());
        }

        // Tab and CR go out as themselves, never as ^I / ^M.
        assert_eq!(echoed(&lflag(ECHO | ECHOCTL), b'\t'), b"\t".to_vec());
        assert_eq!(echoed(&lflag(ECHO | ECHOCTL), b'\r'), b"\r".to_vec());

        // Other control characters: caret notation under ECHOCTL, raw without.
        assert_eq!(echoed(&lflag(ECHO | ECHOCTL), CTRL_C), b"^C".to_vec());
        assert_eq!(echoed(&lflag(ECHO), CTRL_C), alloc::vec![CTRL_C]);
    }

    #[test]
    fn an_echoed_byte_goes_through_the_same_output_rule_as_program_output() {
        // `__process_echoes` hands every byte it is about to put out to
        // `do_output_char` when OPOST is set (`drivers/tty/n_tty.c`), so a
        // terminal with OCRNL echoes a typed carriage return as a newline.
        // Before this the echo path only knew OPOST|ONLCR.
        let mut t = lflag(ECHO);
        t.c_oflag = O_OPOST | O_OCRNL;
        assert_eq!(echoed(&t, b'\r'), b"\n".to_vec());
    }

    #[test]
    fn an_echo_the_rule_swallowed_does_not_claim_to_have_written_anything() {
        // ONOCR at column zero. A `true` here wakes the master for a queue
        // that has nothing new in it.
        let mut t = lflag(ECHO);
        t.c_oflag = O_OPOST | O_ONOCR;
        let mut out = VecDeque::new();
        let mut col = 0usize;
        assert!(!echo_byte(&mut out, b'\r', &t, &mut col));
        assert!(out.is_empty());
    }

    #[test]
    fn echoed_bytes_move_the_same_cursor_the_program_output_moves() {
        // One line on one screen. If the echo of a typed character did not
        // count, ONOCR would think the cursor was still at column zero and
        // swallow the carriage return that ends the line.
        let mut t = lflag(ECHO);
        t.c_oflag = O_OPOST | O_ONOCR;
        let mut out = VecDeque::new();
        let mut col = 0usize;
        echo_byte(&mut out, b'a', &t, &mut col);
        assert_eq!(col, 1);
        assert!(echo_byte(&mut out, b'\r', &t, &mut col));
        assert_eq!(Vec::from(out), b"a\r".to_vec());
    }

    #[test]
    fn a_run_that_does_not_fit_leaves_the_cursor_where_it_was() {
        // out_post is whole-or-nothing, and a column that moved for bytes
        // that never went out would make every later ONOCR decision wrong.
        let mut t = Termios::default_tty();
        t.c_oflag = O_OPOST;
        let mut out: VecDeque<u8> = (0..TTY_OUTPUT_CAP as u32).map(|_| b'x').collect();
        let mut col = 7usize;
        assert_eq!(out_post(&mut out, &t, &mut col, b"ab"), Posted::Full);
        assert_eq!(col, 7);
        assert_eq!(out.len(), TTY_OUTPUT_CAP);
    }

    // ---- the whole of c_oflag, end to end -------------------------------

    /// Set this PTY's output flags, leaving everything else stock.
    fn with_oflag(p: &Pty, bits: u32) {
        p.inner.lock().termios.c_oflag = bits;
    }

    #[test]
    fn program_output_gets_the_whole_output_word_not_just_onlcr() {
        // The bug: every site asked `OPOST && ONLCR` together, so `stty opost
        // -onlcr ocrnl` -- a perfectly ordinary thing to ask for -- fell into
        // the raw branch and the carriage return went out untranslated.
        let p = pty();
        with_oflag(&p, O_OPOST | O_OCRNL);
        assert_eq!(p.slave_write(b"a\rb"), 3);
        assert_eq!(master_drain(&p), "a\nb");
    }

    #[test]
    fn onocr_swallows_the_carriage_return_without_shortening_the_write() {
        // The writer must be told all three bytes were taken. A short count
        // here has it offer the same carriage return again, for ever.
        let p = pty();
        with_oflag(&p, O_OPOST | O_ONOCR);
        assert_eq!(p.slave_write(b"\rab"), 3);
        assert_eq!(master_drain(&p), "ab");
    }

    #[test]
    fn the_column_carries_across_separate_writes() {
        // It is one line on one screen, so where the last write left the
        // cursor is where the next one starts. Keeping the column in a local
        // would make the first byte of every write look like column zero.
        let p = pty();
        with_oflag(&p, O_OPOST | O_ONOCR);
        p.slave_write(b"ab");
        p.slave_write(b"\r");
        assert_eq!(master_drain(&p), "ab\r", "mid-line, so the return stands");
    }

    #[test]
    fn a_write_of_nothing_but_swallowed_returns_is_consumed_whole() {
        let p = pty();
        with_oflag(&p, O_OPOST | O_ONOCR);
        assert_eq!(p.slave_write(b"\r\r\r"), 3);
        assert_eq!(master_drain(&p), "");
    }

    #[test]
    fn olcuc_reaches_the_terminal() {
        let p = pty();
        with_oflag(&p, O_OPOST | O_OLCUC);
        p.slave_write(b"hola\n");
        assert_eq!(master_drain(&p), "HOLA\n");
    }

    #[test]
    fn the_program_leaves_the_cursor_where_the_echo_picks_it_up() {
        // The other direction of the shared cursor, and the one a local would
        // hide: the input path has to start from where the program's last
        // write left the line, or the first thing the user types after a
        // prompt looks like column zero and ONOCR swallows the return that
        // ends it.
        let p = pty();
        with_oflag(&p, O_OPOST | O_ONOCR);
        {
            let mut inner = p.inner.lock();
            inner.termios.c_lflag = ECHO;
            // ICRNL is on in the cooked default and would turn the typed
            // return into a newline before the echo ever saw it.
            inner.termios.c_iflag = 0;
        }
        p.slave_write(b"$ ");
        assert_eq!(master_drain(&p), "$ ");
        // Mid-line now, so the typed return is a real one and is echoed.
        p.master_write(b"\r");
        assert_eq!(master_drain(&p), "\r");
    }

    #[test]
    fn the_echo_and_the_program_share_one_cursor() {
        // A typed character is on the same line as the program's output, so
        // ONOCR has to count it. Two columns would make the terminal drop a
        // carriage return that was wanted, or send one that was not.
        let p = pty();
        with_oflag(&p, O_OPOST | O_ONOCR);
        // Raw mode so the typed byte is echoed and delivered at once.
        p.inner.lock().termios.c_lflag = ECHO;
        p.master_write(b"x");
        assert_eq!(master_drain(&p), "x");
        // The cursor is at column 1 now, so a program's return is a real one.
        p.slave_write(b"\r");
        assert_eq!(master_drain(&p), "\r");
    }

    #[test]
    fn the_stock_terminal_still_turns_a_newline_into_carriage_return_newline() {
        // The one translation that was already working, and the one every
        // shell on the machine depends on.
        let p = pty();
        p.slave_write(b"hola\n");
        assert_eq!(master_drain(&p), "hola\r\n");
    }

    #[test]
    fn a_raw_terminal_translates_nothing_at_all() {
        let p = pty();
        with_oflag(&p, 0);
        p.slave_write(b"a\r\nb");
        assert_eq!(master_drain(&p), "a\r\nb");
    }

    #[test]
    fn a_swallowed_run_is_not_a_full_queue() {
        // The two are one answer only if you never have to tell them apart,
        // and `slave_write` does: a writer told its byte did not fit offers
        // the same byte again for ever.
        let mut t = Termios::default_tty();
        t.c_oflag = O_OPOST | O_ONOCR;
        let mut out = VecDeque::new();
        let mut col = 0usize;
        assert_eq!(out_post(&mut out, &t, &mut col, b"\r"), Posted::Nothing);
        assert!(out.is_empty());
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

    /// A raw terminal with `min`/`time` set, as `stty -icanon min N time M`
    /// leaves it.
    fn raw(p: &Pty, vmin: u8, vtime: u8) {
        set_flags(p, |t| {
            t.c_lflag &= !ICANON;
            t.c_cc[VMIN_CC] = vmin;
            t.c_cc[VTIME_CC] = vtime;
        });
    }

    #[test]
    fn a_raw_read_with_min_zero_comes_back_empty_instead_of_waiting() {
        // `stty -icanon min 0 time 0` is the polling read, and POSIX says it
        // returns at once with whatever is there, including nothing. This end
        // answered EAGAIN, so a program with a blocking descriptor waited for
        // a byte that was never going to be typed. It is the same shape as any
        // "no data" mistaken for "not ready yet": the queue cannot tell the
        // two apart, only VMIN can.
        let p = pty();
        raw(&p, 0, 0);
        let mut buf = [0u8; 16];
        assert_eq!(p.slave_read(&mut buf), Ok(0));
    }

    #[test]
    fn nothing_queued_and_nothing_asked_for_is_not_a_reason_to_wake_a_reader() {
        // The floor of one byte on the count a reader parks on. Without it a
        // terminal with an empty queue reads as ready before any read has
        // parked, and the blocking loop goes round on nothing.
        let p = pty();
        raw(&p, 1, 0);
        assert!(!p.slave_read_ready());
        p.master_write(b"a");
        assert!(p.slave_read_ready());
    }

    #[test]
    fn a_raw_read_with_min_zero_still_takes_what_is_queued() {
        let p = pty();
        raw(&p, 0, 0);
        p.master_write(b"abc");
        let mut buf = [0u8; 16];
        assert_eq!(p.slave_read(&mut buf), Ok(3));
        assert_eq!(&buf[..3], b"abc");
        // And is empty again straight after, without waiting.
        assert_eq!(p.slave_read(&mut buf), Ok(0));
    }

    #[test]
    fn a_raw_read_with_the_default_minimum_waits_for_its_byte() {
        // VMIN=1 is the cooked default and the one `stty raw` leaves alone, so
        // this is the behaviour that must not change: an empty queue waits.
        let p = pty();
        raw(&p, 1, 0);
        let mut buf = [0u8; 16];
        assert_eq!(p.slave_read(&mut buf), Err(FsError::Again));
        assert!(!p.slave_read_ready());
    }

    #[test]
    fn a_raw_read_holds_the_bytes_back_until_the_minimum_is_met() {
        let p = pty();
        raw(&p, 4, 0);
        let mut buf = [0u8; 16];
        p.master_write(b"ab");
        assert_eq!(p.slave_read(&mut buf), Err(FsError::Again));
        p.master_write(b"cd");
        assert_eq!(p.slave_read(&mut buf), Ok(4));
        assert_eq!(&buf[..4], b"abcd");
    }

    #[test]
    fn the_wait_and_the_read_agree_on_when_a_raw_read_is_over() {
        // `File::read` is `loop { read_at; on EAGAIN await the wait }`. If the
        // wait says ready while the read says not yet, the loop spins at full
        // tilt on a terminal that is merely half way to its minimum.
        let p = pty();
        raw(&p, 4, 0);
        let mut buf = [0u8; 16];
        for n in 0..4 {
            assert_eq!(
                p.slave_read(&mut buf),
                Err(FsError::Again),
                "{} byte(s) queued",
                n
            );
            assert!(!p.slave_read_ready(), "{} byte(s) queued", n);
            p.master_write(b"x");
        }
        assert!(p.slave_read_ready());
        assert_eq!(p.slave_read(&mut buf), Ok(4));
    }

    #[test]
    fn a_buffer_smaller_than_the_minimum_does_not_wait_for_ever() {
        // `read(fd, buf, 2)` under `min 4`: the caller cannot take four bytes,
        // so a read that insisted on four would never come back. The wait has
        // to know this too, or the reader sleeps through the wakeup that would
        // have let it through.
        let p = pty();
        raw(&p, 4, 0);
        let mut buf = [0u8; 2];
        assert_eq!(p.slave_read(&mut buf), Err(FsError::Again));
        p.master_write(b"ab");
        assert!(p.slave_read_ready());
        assert_eq!(p.slave_read(&mut buf), Ok(2));
    }

    #[test]
    fn a_hangup_hands_over_what_is_queued_however_short_of_the_minimum() {
        // Nothing more is coming, so a count that can never be reached would
        // strand the bytes that did arrive.
        let p = pty();
        raw(&p, 8, 0);
        p.master_write(b"ab");
        let mut buf = [0u8; 16];
        assert_eq!(p.slave_read(&mut buf), Err(FsError::Again));
        p.master_closed.store(true, Ordering::Relaxed);
        assert!(p.slave_read_ready());
        assert_eq!(p.slave_read(&mut buf), Ok(2));
        assert_eq!(p.slave_read(&mut buf), Ok(0), "and then end of file");
    }

    #[test]
    fn min_zero_does_not_swallow_the_end_of_file_of_a_closed_master() {
        // Both answers are `Ok(0)`, so the only way to tell them apart is what
        // happens next; an EOF that is really a poll would have the reader
        // loop for ever on a terminal that is gone.
        let p = pty();
        raw(&p, 0, 0);
        p.master_closed.store(true, Ordering::Relaxed);
        let mut buf = [0u8; 16];
        assert_eq!(p.slave_read(&mut buf), Ok(0));
        assert!(p.slave_read_ready(), "a closed master stays ready");
    }

    #[test]
    fn canonical_mode_is_untouched_by_min_and_time() {
        // VMIN and VTIME have no meaning with ICANON set, and a line
        // discipline that consulted them anyway would hold back a whole line
        // that is already complete.
        let p = pty();
        set_flags(&p, |t| {
            t.c_cc[VMIN_CC] = 8;
            t.c_cc[VTIME_CC] = 3;
        });
        p.master_write(b"hi\n");
        assert_eq!(slave_reads(&p), vec!["hi\n"]);
    }

    #[test]
    fn a_raw_read_never_waits_on_a_timer_it_was_not_asked_for() {
        // The timer is the only thing that can end a wait short of the count,
        // so arming one where VTIME is zero would make a `min 4 time 0` read
        // come back with two bytes.
        let p = pty();
        raw(&p, 4, 0);
        let mut buf = [0u8; 16];
        p.master_write(b"ab");
        assert_eq!(p.slave_read(&mut buf), Err(FsError::Again));
        assert_eq!(
            p.vtime_deadline_ns.load(Ordering::Relaxed),
            0,
            "no deadline without VTIME"
        );
    }

    #[test]
    fn the_between_bytes_timer_does_not_start_on_an_empty_queue() {
        // With VMIN > 0 the timer measures the gap between bytes. Starting it
        // at the read would let a `min 1 time N` program — the ordinary
        // terminal setting — come back empty on an idle terminal, which every
        // reader takes for end of file.
        let p = pty();
        raw(&p, 2, 5);
        let mut buf = [0u8; 16];
        assert_eq!(p.slave_read(&mut buf), Err(FsError::Again));
        assert_eq!(p.vtime_deadline_ns.load(Ordering::Relaxed), 0);
        p.master_write(b"a");
        assert_eq!(p.slave_read(&mut buf), Err(FsError::Again));
        assert_ne!(
            p.vtime_deadline_ns.load(Ordering::Relaxed),
            0,
            "the first byte starts it"
        );
    }

    #[test]
    fn a_read_that_came_back_leaves_no_timer_running_behind_it() {
        // A stale deadline would end the *next* read early, and that read may
        // be one whose settings forbid coming back empty at all.
        let p = pty();
        raw(&p, 0, 5);
        let mut buf = [0u8; 16];
        assert_eq!(p.slave_read(&mut buf), Err(FsError::Again));
        assert_ne!(p.vtime_deadline_ns.load(Ordering::Relaxed), 0);
        p.master_write(b"a");
        assert_eq!(p.slave_read(&mut buf), Ok(1));
        assert_eq!(p.vtime_deadline_ns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn icrnl_completes_a_line_but_inlcr_and_igncr_do_not() {
        let p = pty();
        p.master_write(b"ab\r");
        assert_eq!(slave_reads(&p), vec!["ab\n"]);

        let p = pty();
        set_flags(&p, |t| t.c_iflag = I_IGNCR);
        p.master_write(b"ab\r");
        assert!(!p.slave_readable());

        let p = pty();
        set_flags(&p, |t| t.c_iflag = I_INLCR);
        p.master_write(b"ab\n");
        // '\n' became '\r', which is not an end of line.
        assert!(!p.slave_readable());
    }

    #[test]
    fn a_seven_bit_terminal_ends_its_line_with_the_return_key() {
        // On a 7-bit line Enter arrives as 0x8d, and ISTRIP is what makes it
        // a carriage return for ICRNL to end the line with. The copy of the
        // CR/NL block this file used to carry had no ISTRIP in it, so the key
        // did nothing.
        let p = pty();
        set_flags(&p, |t| t.c_iflag |= I_ISTRIP);
        p.master_write(&[b'a', b'b', 0x8d]);
        assert_eq!(slave_reads(&p), vec!["ab\n"]);

        // Without it the byte is just a byte and the line stays open.
        let p = pty();
        p.master_write(&[b'a', b'b', 0x8d]);
        assert!(!p.slave_readable());
    }

    #[test]
    fn iuclc_folds_the_keystrokes_and_iexten_turns_it_off() {
        let p = pty();
        set_flags(&p, |t| {
            assert_ne!(t.c_lflag & L_IEXTEN, 0);
            t.c_iflag |= I_IUCLC;
        });
        p.master_write(b"HOLA\r");
        assert_eq!(slave_reads(&p), vec!["hola\n"]);

        let p = pty();
        set_flags(&p, |t| {
            t.c_iflag |= I_IUCLC;
            t.c_lflag &= !L_IEXTEN;
        });
        p.master_write(b"HOLA\r");
        assert_eq!(slave_reads(&p), vec!["HOLA\n"]);
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
    fn vkill_rubs_the_line_out_on_the_stock_terminal() {
        // `tty_std_termios` has ECHOK and ECHOKE both, and ECHOKE wins, so
        // Ctrl-U on a terminal nobody reconfigured walks back over the line it
        // threw away. The default here was missing ECHOKE, so this used to
        // echo a bare newline and leave the killed line on screen.
        let p = pty();
        p.master_write(b"abcd");
        let _ = master_drain(&p);
        p.master_write(&[CTRL_U]);
        assert_eq!(master_drain(&p), "\x08 \x08".repeat(4));
        p.master_write(b"\n");
        assert_eq!(slave_reads(&p), vec!["\n"]);
    }

    #[test]
    fn vkill_echoes_a_newline_when_the_program_asks_for_echok_alone() {
        let p = pty();
        set_flags(&p, |t| t.c_lflag &= !L_ECHOKE);
        p.master_write(b"abcd");
        let _ = master_drain(&p);
        p.master_write(&[CTRL_U]);
        assert_eq!(master_drain(&p), "\r\n");
    }

    #[test]
    fn vkill_shows_nothing_when_the_program_asks_for_neither_flag() {
        let p = pty();
        set_flags(&p, |t| t.c_lflag &= !(L_ECHOK | L_ECHOKE));
        p.master_write(b"abcd");
        let _ = master_drain(&p);
        p.master_write(&[CTRL_U]);
        assert_eq!(master_drain(&p), "");
        // The line is still gone, echo or no echo.
        p.master_write(b"\n");
        assert_eq!(slave_reads(&p), vec!["\n"]);
    }

    #[test]
    fn vkill_paints_nothing_back_over_a_password_prompt() {
        // ECHO off with ECHOK left where it was: rubouts or a newline would
        // both say something about a secret nobody may see the shape of.
        let p = pty();
        set_flags(&p, |t| t.c_lflag &= !ECHO);
        p.master_write(b"hunter2");
        let _ = master_drain(&p);
        p.master_write(&[CTRL_U]);
        assert_eq!(master_drain(&p), "");
    }

    #[test]
    fn echonl_ends_the_line_of_a_prompt_that_turned_echo_off() {
        // getpass(3): ECHO off, ECHONL on. The password stays invisible and
        // the Enter that ends it does not.
        let p = pty();
        set_flags(&p, |t| {
            t.c_lflag &= !ECHO;
            t.c_lflag |= ECHONL;
        });
        p.master_write(b"hunter2\n");
        assert_eq!(master_drain(&p), "\r\n");
        assert_eq!(slave_reads(&p), vec!["hunter2\n"]);
    }

    #[test]
    fn echonl_shows_the_newline_and_nothing_else() {
        let p = pty();
        set_flags(&p, |t| {
            t.c_lflag &= !ECHO;
            t.c_lflag |= ECHONL;
        });
        p.master_write(b"ab");
        assert_eq!(master_drain(&p), "", "the characters stay hidden");
    }

    #[test]
    fn echo_off_without_echonl_leaves_the_newline_invisible_too() {
        let p = pty();
        set_flags(&p, |t| t.c_lflag &= !ECHO);
        p.master_write(b"hunter2\n");
        assert_eq!(master_drain(&p), "");
    }

    #[test]
    fn echonl_is_a_canonical_mode_flag_and_does_nothing_raw() {
        // POSIX: "If ICANON is also set". In raw mode the newline is data
        // like any other byte, and ECHO is what decides whether data is
        // shown.
        let p = pty();
        set_flags(&p, |t| {
            t.c_lflag &= !(ICANON | ECHO);
            t.c_lflag |= ECHONL;
        });
        p.master_write(b"a\nb");
        assert_eq!(master_drain(&p), "");
    }

    #[test]
    fn backspace_over_an_accented_letter_takes_the_whole_letter() {
        // `ñ` travels as 0xc3 0xb1. Taking one byte would leave the 0xc3 in
        // the line, and the shell would then read a byte that is not a
        // character. On a Spanish keyboard this is every `ñ` and every accent.
        let p = pty();
        p.master_write("añ".as_bytes());
        let _ = master_drain(&p);
        p.master_write(&[DEL]);
        assert_eq!(master_drain(&p), "\x08 \x08", "one column, one rubout");
        p.master_write(b"\n");
        assert_eq!(slave_reads(&p), vec!["a\n"]);
    }

    #[test]
    fn backspace_over_a_three_and_a_four_byte_character() {
        for text in ["a€", "a😀"] {
            let p = pty();
            p.master_write(text.as_bytes());
            let _ = master_drain(&p);
            p.master_write(&[DEL]);
            assert_eq!(master_drain(&p), "\x08 \x08", "{}", text);
            p.master_write(b"\n");
            assert_eq!(slave_reads(&p), vec!["a\n"], "{}", text);
        }
    }

    #[test]
    fn a_terminal_that_says_it_is_not_utf8_erases_one_byte() {
        // IUTF8 is what says the line is characters. Without it the terminal
        // is a byte pipe and Backspace takes a byte, which is what a program
        // sending raw bytes through a cooked terminal is asking for.
        let p = pty();
        set_flags(&p, |t| t.c_iflag &= !I_IUTF8);
        p.master_write("añ".as_bytes());
        let _ = master_drain(&p);
        p.master_write(&[DEL]);
        p.master_write(b"\n");
        // The 0xc3 survives, so what is read is not valid UTF-8.
        let mut buf = [0u8; 16];
        let n = p.slave_read(&mut buf).unwrap();
        assert_eq!(&buf[..n], &[b'a', 0xc3, b'\n']);
    }

    #[test]
    fn backspace_on_an_empty_line_still_shows_nothing() {
        let p = pty();
        p.master_write(&[DEL]);
        assert_eq!(master_drain(&p), "");
    }

    #[test]
    fn word_erase_counts_columns_and_not_bytes() {
        // `añb` is four bytes and three columns. One rubout per byte would
        // walk the cursor one place too far and paint over what is left of the
        // prompt.
        let p = pty();
        p.master_write("hola añb".as_bytes());
        let _ = master_drain(&p);
        p.master_write(&[CTRL_W]);
        assert_eq!(master_drain(&p), "\x08 \x08".repeat(3));
        p.master_write(b"\n");
        assert_eq!(slave_reads(&p), vec!["hola \n"]);
    }

    #[test]
    fn the_kill_rubout_counts_columns_and_not_bytes() {
        let p = pty();
        set_flags(&p, |t| t.c_lflag |= L_ECHOKE);
        p.master_write("añ€".as_bytes());
        let _ = master_drain(&p);
        p.master_write(&[CTRL_U]);
        assert_eq!(master_drain(&p), "\x08 \x08".repeat(3));
    }

    #[test]
    fn a_multibyte_character_arrives_whole_however_it_was_written() {
        // A terminal emulator may hand over the two bytes of `ñ` in separate
        // writes; the line discipline may not treat the halves as characters
        // of their own.
        let p = pty();
        for b in "ñ".as_bytes() {
            p.master_write(&[*b]);
        }
        p.master_write(b"\n");
        assert_eq!(slave_reads(&p), vec!["ñ\n"]);
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
        // ECHOCTL is in the cooked default, so the "^R" marker is drawn first.
        assert_eq!(master_drain(&p), "^R\r\nintacta");
        p.master_write(b"\n");
        assert_eq!(slave_reads(&p), vec!["intacta\n"]);

        // Without ECHOCTL only the fresh line and the pending text.
        let p = pty();
        set_flags(&p, |t| t.c_lflag &= !ECHOCTL);
        p.master_write(b"intacta");
        let _ = master_drain(&p);
        p.master_write(&[CTRL_R]);
        assert_eq!(master_drain(&p), "\r\nintacta");
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
    fn a_line_cut_short_by_veof_does_not_run_into_the_next_one() {
        let p = pty();
        // What `printf 'abc'; ^D; printf 'def\n'` puts down the line. The
        // first line has no terminator of its own, so before the EOF marks
        // one `read` handed back `abcdef\n` and the program saw one line.
        p.master_write(b"abc");
        p.master_write(&[CTRL_D]);
        p.master_write(b"def\n");
        assert_eq!(slave_reads(&p), vec!["abc", "def\n"]);
    }

    #[test]
    fn two_lines_cut_short_by_veof_each_come_back_on_their_own() {
        let p = pty();
        for line in [&b"one"[..], &b"two"[..], &b"three"[..]] {
            p.master_write(line);
            p.master_write(&[CTRL_D]);
        }
        assert_eq!(slave_reads(&p), vec!["one", "two", "three"]);
    }

    #[test]
    fn a_read_that_fills_its_buffer_exactly_at_the_boundary_does_not_come_back_empty() {
        let p = pty();
        p.master_write(b"abc");
        p.master_write(&[CTRL_D]);
        p.master_write(b"de\n");
        // A three-byte buffer takes the whole first line and stops at the
        // boundary without having looked at it, so the next read must not
        // spend its turn on the leftover mark.
        let mut buf = [0u8; 3];
        assert_eq!(p.slave_read(&mut buf).unwrap(), 3);
        assert_eq!(&buf[..], b"abc");
        assert_eq!(p.slave_read(&mut buf).unwrap(), 3);
        assert_eq!(&buf[..], b"de\n");
    }

    #[test]
    fn a_flush_takes_the_boundaries_with_it() {
        let p = pty();
        p.master_write(b"abc");
        p.master_write(&[CTRL_D]);
        assert_eq!(p.ioctl(TCFLSH as u32, 0, false), Ok(0));
        assert!(p.inner.lock().eof_marks.is_empty());
        // And the position the flush skipped past may not be mistaken for a
        // boundary by whatever is typed next.
        p.master_write(b"xyz\n");
        assert_eq!(slave_reads(&p), vec!["xyz\n"]);
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
    fn a_second_ctrl_c_escalates_to_sigkill_and_echoes_killed() {
        let p = pty();
        p.fg_pgrp.store(4242, Ordering::Relaxed);
        p.master_write(&[CTRL_C]);
        assert_eq!(
            p.ctrl_c_armed_pgid.load(Ordering::Relaxed),
            4242,
            "first Ctrl-C arms the pgrp"
        );
        assert_eq!(master_drain(&p), "^C\r\n");
        p.master_write(&[CTRL_C]);
        assert_eq!(
            p.ctrl_c_armed_pgid.load(Ordering::Relaxed),
            0,
            "second Ctrl-C clears the arm after force-kill"
        );
        assert_eq!(master_drain(&p), "^C (killed)\r\n");
    }

    #[test]
    fn changing_fg_pgrp_clears_the_ctrl_c_arm() {
        let p = pty();
        p.fg_pgrp.store(100, Ordering::Relaxed);
        p.master_write(&[CTRL_C]);
        assert_eq!(p.ctrl_c_armed_pgid.load(Ordering::Relaxed), 100);
        let mut pgid: i32 = 200;
        assert!(p
            .ioctl(TIOCSPGRP as u32, &mut pgid as *mut i32 as usize, true)
            .is_ok());
        assert_eq!(p.fg_pgrp.load(Ordering::Relaxed), 200);
        assert_eq!(
            p.ctrl_c_armed_pgid.load(Ordering::Relaxed),
            0,
            "a new foreground job must not inherit a force-kill arm"
        );
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

    // ---- flow control: both queues are bounded --------------------------
    //
    // A terminal is the one file where the reader is a person, so the writer
    // can outrun it by any factor and for any length of time. Linux answers
    // that with a bound at each end -- `N_TTY_BUF_SIZE` on the way in,
    // `TTYB_DEFAULT_MEM_LIMIT` on the way out -- and a short write to say so.
    // Without one, `yes > /dev/pts/N` with nobody reading the master is an
    // unprivileged program asking the kernel for every page it has.

    #[test]
    fn a_raw_write_longer_than_the_input_queue_comes_back_short() {
        let p = pty();
        raw(&p, 1, 0);
        let big = vec![b'x'; N_TTY_BUF_SIZE + 512];
        assert_eq!(p.master_write(&big), N_TTY_BUF_SIZE);
        assert_eq!(p.inner.lock().input.len(), N_TTY_BUF_SIZE);
        // And it stays there: the second write is refused outright, which is
        // what stops the caller's loop from being a way to keep allocating.
        assert_eq!(p.master_write(b"x"), 0);
        assert_eq!(p.inner.lock().input.len(), N_TTY_BUF_SIZE);
    }

    #[test]
    fn what_did_not_fit_goes_in_once_the_program_has_read() {
        let p = pty();
        raw(&p, 1, 0);
        assert_eq!(p.master_write(&vec![b'x'; N_TTY_BUF_SIZE]), N_TTY_BUF_SIZE);
        let mut buf = [0u8; 100];
        assert_eq!(p.slave_read(&mut buf), Ok(100));
        // Exactly the room that was freed, and not a byte more.
        assert_eq!(p.master_write(&vec![b'y'; 500]), 100);
        assert_eq!(p.inner.lock().input.len(), N_TTY_BUF_SIZE);
    }

    #[test]
    fn the_committed_input_and_the_line_being_edited_share_one_budget() {
        // Two queues in this file, one buffer in Linux: `canon_head` and
        // `read_tail` are indices into the same `read_buf`. Budgeting them
        // separately would double the bound, and `TIOCOUTQ` at this end
        // already reports the two added together.
        let p = pty();
        p.master_write(b"hecho\n"); // 6 bytes committed to `input`
        assert_eq!(p.inner.lock().input.len(), 6);
        let rest = N_TTY_BUF_SIZE - 6;
        assert_eq!(p.master_write(&vec![b'x'; rest + 100]), rest);
        let inner = p.inner.lock();
        assert_eq!(inner.input.len() + inner.canon.len(), N_TTY_BUF_SIZE);
    }

    #[test]
    fn a_line_that_fills_the_queue_can_still_be_erased() {
        // The case the bound may not refuse. A canonical line long enough to
        // fill the queue on its own, with nothing committed behind it: if the
        // write is turned away here it turns away VERASE too, and the terminal
        // is wedged at the one moment the user needs to shorten the line, with
        // no way out but closing it.
        let p = pty();
        set_flags(&p, |t| t.c_lflag &= !ECHO);
        assert_eq!(p.master_write(&vec![b'x'; N_TTY_BUF_SIZE]), N_TTY_BUF_SIZE);
        assert_eq!(p.inner.lock().canon.len(), N_TTY_BUF_SIZE);
        assert_eq!(p.master_write(&[DEL]), 1);
        assert_eq!(p.inner.lock().canon.len(), N_TTY_BUF_SIZE - 1);
        // One column free, and the line takes one more character.
        p.master_write(b"z");
        assert_eq!(p.inner.lock().canon.len(), N_TTY_BUF_SIZE);
        p.master_write(&[CTRL_U]);
        assert!(p.inner.lock().canon.is_empty());
    }

    #[test]
    fn a_line_that_fills_the_queue_stops_growing_and_will_not_commit() {
        // The other half of the same rule: the bytes are taken so the editing
        // characters keep working, but a data character is dropped rather than
        // stored, and the newline with it. A line that cannot hold its own
        // terminator is not one to hand the reader.
        let p = pty();
        set_flags(&p, |t| t.c_lflag &= !ECHO);
        p.master_write(&vec![b'x'; N_TTY_BUF_SIZE]);
        assert_eq!(p.master_write(b"yyy"), 3); // taken
        assert_eq!(p.inner.lock().canon.len(), N_TTY_BUF_SIZE); // not stored
        assert_eq!(p.master_write(b"\n"), 1);
        assert!(p.inner.lock().input.is_empty()); // nothing committed
        assert_eq!(p.inner.lock().canon.len(), N_TTY_BUF_SIZE);
        // Erase one column and the newline lands, ending the line.
        p.master_write(&[DEL]);
        p.master_write(b"\n");
        assert_eq!(p.inner.lock().canon.len(), 0);
        assert_eq!(p.inner.lock().input.len(), N_TTY_BUF_SIZE);
    }

    #[test]
    fn a_line_that_fills_the_queue_still_answers_ctrl_c() {
        let p = pty();
        set_flags(&p, |t| t.c_lflag &= !ECHO);
        p.master_write(&vec![b'x'; N_TTY_BUF_SIZE]);
        p.master_write(&[CTRL_C]);
        assert!(p.inner.lock().canon.is_empty());
    }

    #[test]
    fn program_output_stops_at_the_cap_and_says_how_much_it_took() {
        let p = pty();
        set_flags(&p, |t| t.c_oflag = 0); // no ONLCR: one byte is one byte
        let big = vec![b'x'; TTY_OUTPUT_CAP + 1024];
        assert_eq!(p.slave_write(&big), TTY_OUTPUT_CAP);
        assert_eq!(p.inner.lock().output.len(), TTY_OUTPUT_CAP);
        assert_eq!(p.slave_write(b"x"), 0);
        assert_eq!(p.inner.lock().output.len(), TTY_OUTPUT_CAP);
    }

    #[test]
    fn the_cap_never_splits_a_crlf_pair() {
        // Under ONLCR a newline leaves as two bytes. Letting the `\r` in and
        // stopping before the `\n` would leave the reader a carriage return
        // that is not the line ending it is waiting for -- and the writer,
        // told one byte went in, would send the `\n` again.
        let p = pty();
        p.slave_write(&vec![b'x'; TTY_OUTPUT_CAP - 1]);
        assert_eq!(p.inner.lock().output.len(), TTY_OUTPUT_CAP - 1);
        assert_eq!(p.slave_write(b"\n"), 0);
        assert_eq!(p.inner.lock().output.len(), TTY_OUTPUT_CAP - 1);
        // One more byte of room and the pair goes in whole.
        let mut buf = [0u8; 1];
        assert_eq!(p.master_read(&mut buf), Ok(1));
        assert_eq!(p.slave_write(b"\n"), 1);
        assert_eq!(p.inner.lock().output.len(), TTY_OUTPUT_CAP);
    }

    #[test]
    fn output_room_comes_back_when_the_terminal_reads() {
        let p = pty();
        set_flags(&p, |t| t.c_oflag = 0);
        p.slave_write(&vec![b'x'; TTY_OUTPUT_CAP]);
        let mut buf = [0u8; 300];
        assert_eq!(p.master_read(&mut buf), Ok(300));
        assert_eq!(p.slave_write(&vec![b'y'; 1000]), 300);
    }

    #[test]
    fn an_echo_cannot_push_the_output_queue_past_its_cap() {
        // The echo of a keystroke goes into the same queue the program writes
        // to, so it is bounded by the same number or it is not bounded at all.
        let p = pty();
        set_flags(&p, |t| t.c_oflag = 0);
        p.slave_write(&vec![b'x'; TTY_OUTPUT_CAP]);
        p.master_write(b"abc\n");
        assert_eq!(p.inner.lock().output.len(), TTY_OUTPUT_CAP);
        // The input itself still went in: the two directions are two budgets.
        assert_eq!(p.inner.lock().input.len(), 4);
    }

    #[test]
    fn a_master_sending_nothing_but_ctrl_c_cannot_grow_the_output_queue() {
        // The bound on the input queue does not cover this on its own. A
        // signal character is *consumed* -- it is never stored -- so no number
        // of them fills the input queue, and each one asks for a `^C` on
        // screen. Every append toward the master has to go through the cap,
        // not just the ones carrying program output.
        let p = pty();
        set_flags(&p, |t| t.c_oflag = 0);
        for _ in 0..(TTY_OUTPUT_CAP / 2) {
            p.master_write(&[CTRL_C]);
        }
        assert!(p.inner.lock().input.is_empty());
        assert!(p.inner.lock().output.len() <= TTY_OUTPUT_CAP);
    }

    #[test]
    fn a_full_queue_is_eagain_and_never_a_zero_byte_write() {
        // `write(2)` returning 0 tells a program it made no progress on a
        // request that asked for some, and the loop it sits in calls straight
        // back with the same buffer. That is a spin, not a wait.
        let p = Arc::new(pty());
        set_flags(&p, |t| {
            t.c_lflag &= !ICANON;
            t.c_oflag = 0;
        });
        let (master, slave) = ends(p.clone());
        assert_eq!(
            slave.write_at(0, &vec![b'x'; TTY_OUTPUT_CAP]),
            Ok(TTY_OUTPUT_CAP)
        );
        assert_eq!(slave.write_at(0, b"x"), Err(FsError::Again));
        assert_eq!(
            master.write_at(0, &vec![b'y'; N_TTY_BUF_SIZE]),
            Ok(N_TTY_BUF_SIZE)
        );
        assert_eq!(master.write_at(0, b"y"), Err(FsError::Again));
        // An empty write is still a no-op and not an error.
        assert_eq!(slave.write_at(0, b""), Ok(0));
        assert_eq!(master.write_at(0, b""), Ok(0));
    }

    #[test]
    fn poll_stops_promising_writable_once_the_queue_is_full() {
        let p = Arc::new(pty());
        set_flags(&p, |t| {
            t.c_lflag &= !ICANON;
            t.c_oflag = 0;
        });
        let (master, slave) = ends(p.clone());
        assert!(slave.poll().unwrap().write);
        assert!(master.poll().unwrap().write);

        // One end at a time: the two queues are two budgets, so filling the
        // one the program writes to may not report the terminal's end full.
        p.slave_write(&vec![b'x'; TTY_OUTPUT_CAP]);
        assert!(!slave.poll().unwrap().write);
        assert!(master.poll().unwrap().write);

        p.master_write(&vec![b'y'; N_TTY_BUF_SIZE]);
        assert!(!master.poll().unwrap().write);

        let mut buf = [0u8; 8];
        let _ = p.master_read(&mut buf);
        let _ = p.slave_read(&mut buf);
        assert!(slave.poll().unwrap().write);
        assert!(master.poll().unwrap().write);
    }

    #[test]
    fn writable_measures_the_same_queue_the_write_does() {
        // The budget is shared, so the answer has to be worked out from both
        // halves of it. Measuring only the committed input says "room" while a
        // line long enough to fill the queue sits in front of it.
        let p = Arc::new(pty());
        set_flags(&p, |t| t.c_lflag &= !ECHO);
        let (master, _slave) = ends(p.clone());
        p.master_write(b"hecho\n");
        p.master_write(&vec![b'x'; N_TTY_BUF_SIZE - 6]);
        {
            let inner = p.inner.lock();
            assert_eq!(inner.input.len(), 6);
            assert_eq!(inner.canon.len(), N_TTY_BUF_SIZE - 6);
        }
        assert!(!master.poll().unwrap().write);
        assert_eq!(p.master_write(b"z"), 0);

        // The program takes the committed line and the room comes back.
        let mut buf = [0u8; 16];
        assert_eq!(p.slave_read(&mut buf), Ok(6));
        assert!(master.poll().unwrap().write);
    }

    #[test]
    fn the_two_bounds_are_the_numbers_linux_uses() {
        // Not arbitrary, and not ours to round off: a program that has been
        // sized against a Linux terminal -- a shell reading a line, a pager
        // filling a screen -- is sized against these.
        assert_eq!(N_TTY_BUF_SIZE, 4096); // N_TTY_BUF_SIZE, drivers/tty/n_tty.c
        assert_eq!(TTY_OUTPUT_CAP, 640 * 1024); // TTYB_DEFAULT_MEM_LIMIT
    }

    #[test]
    fn a_line_being_edited_polls_writable_even_with_the_queue_full() {
        // `poll` has to answer the same question the write does, and the write
        // still takes bytes in this one case. Reporting it unwritable would
        // park a terminal emulator that polls before it writes, holding back
        // the very Backspace that would free the queue.
        let p = Arc::new(pty());
        set_flags(&p, |t| t.c_lflag &= !ECHO);
        let (master, _slave) = ends(p.clone());
        p.master_write(&vec![b'x'; N_TTY_BUF_SIZE]);
        assert!(master.poll().unwrap().write);
        // Commit the line and the same full queue is no longer writable.
        p.master_write(&[DEL]);
        p.master_write(b"\n");
        assert!(!master.poll().unwrap().write);
    }
}
