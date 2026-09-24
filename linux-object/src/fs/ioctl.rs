#![allow(unused)]

// for IOR and IOW:
// 32bits total, command in lower 16bits, size of the parameter structure in the lower 14 bits of the upper 16 bits
// higher 2 bits: 01 = write, 10 = read

#[cfg(not(target_arch = "mips"))]
pub const TCGETS: usize = 0x5401;
#[cfg(target_arch = "mips")]
pub const TCGETS: usize = 0x540D;

pub const TCSETS: usize = 0x5402;
pub const TCSETSW: usize = 0x5403;
pub const TCSETSF: usize = 0x5404;

/// The Linux **kernel ABI** `struct termios` exchanged by `TCGETS`/`TCSETS`
/// (asm-generic/termbits.h): `NCCS = 19`, no separate `c_ispeed`/`c_ospeed`
/// (the line speed lives in the `CBAUD` bits of `c_cflag`). This is exactly 36
/// bytes.
///
/// This MUST match the kernel ABI struct, not libc's larger userspace `struct
/// termios` (60 bytes, `NCCS = 32` + speed fields). glibc's `tcgetattr` passes
/// a kernel-sized stack buffer to `TCGETS` and expects ≤36 bytes back; writing
/// 60 bytes overran that buffer, clobbering the saved frame pointer and return
/// address and crashing the process on return (SIGSEGV, `pc=0xf`) — which made
/// every glibc `exec` flaky. musl passes its own 60-byte struct, so it happened
/// to survive, masking the bug.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Termios {
    pub c_iflag: u32,
    pub c_oflag: u32,
    pub c_cflag: u32,
    pub c_lflag: u32,
    pub c_line: u8,
    pub c_cc: [u8; 19],
}

impl Termios {
    /// Defaults aligned with Linux `n_tty` cooked TTY settings.
    pub const fn default_tty() -> Self {
        Self {
            // ICRNL | IXON | IMAXBEL | IUTF8.
            //
            // Linux's own `INIT_C_IFLAG` has no `IUTF8`: there it is `agetty`
            // or `login` that turns it on once it knows the locale, and the
            // default dates from before terminals were UTF-8 at all. This
            // system has neither, and its console, its keymaps and every shell
            // on it are UTF-8, so a terminal that starts without the flag
            // starts wrong: Backspace over `ñ` would take one of its two bytes
            // and leave the other.
            c_iflag: 0x6500,
            // OPOST | ONLCR
            c_oflag: 0x0005,
            // B38400 | CS8 | CREAD | HUPCL
            c_cflag: 0x08bf,
            // ISIG | ICANON | ECHO | ECHOE | ECHOK | IEXTEN
            c_lflag: 0x803b,
            c_line: 0,
            // Matches Linux `INIT_C_CC` (include/linux/tty.h), one entry per
            // NCCS=19 control char: VINTR=^C, VQUIT=^\, VERASE=DEL, VKILL=^U,
            // VEOF=^D, VTIME=0, VMIN=1, VSWTC=0, VSTART=^Q, VSTOP=^S, VSUSP=^Z,
            // VEOL=0, VREPRINT=^R, VDISCARD=^O, VWERASE=^W, VLNEXT=^V, VEOL2=0.
            c_cc: [
                3, 28, 127, 21, 4, 0, 1, 0, 17, 19, 26, 0, 18, 15, 23, 22, 0, 0, 0,
            ],
            // Line speed is carried in the CBAUD bits of c_cflag (B38400 above).
        }
    }
}

/// Index into [`Termios::c_cc`] of the non-canonical read timer, in
/// deciseconds (`include/uapi/asm-generic/termbits.h`).
pub const VTIME_CC: usize = 5;
/// Index into [`Termios::c_cc`] of the non-canonical read minimum, in bytes.
pub const VMIN_CC: usize = 6;
/// Index into [`Termios::c_cc`] of the line-kill character (Ctrl-U).
pub const VKILL_CC: usize = 3;
/// Index into [`Termios::c_cc`] of the interrupt character (Ctrl-C).
pub const VINTR_CC: usize = 0;
/// Index into [`Termios::c_cc`] of the quit character (Ctrl-\).
pub const VQUIT_CC: usize = 1;
/// Index into [`Termios::c_cc`] of the suspend character (Ctrl-Z).
pub const VSUSP_CC: usize = 10;

/// `c_lflag` bit: the signal-generating characters are active.
pub const L_ISIG: u32 = 0x0001;
/// `c_lflag` bit for canonical (line-at-a-time) input.
pub const L_ICANON: u32 = 0x0002;
/// `c_lflag` bit: echo input back to the terminal at all.
pub const L_ECHO: u32 = 0x0008;
/// `c_lflag` bit: the line-kill character echoes a newline.
pub const L_ECHOK: u32 = 0x0020;
/// `c_lflag` bit: echo a newline even when `ECHO` is off.
pub const L_ECHONL: u32 = 0x0040;
/// `c_lflag` bit: the line-kill character rubs the line out instead.
pub const L_ECHOKE: u32 = 0x0800;

/// `c_iflag` bit: input is UTF-8, so line editing works on characters.
pub const I_IUTF8: u32 = 0x4000;

/// `c_oflag` bit: output is post-processed at all. Every other bit in this
/// word is read only when this one is set.
pub const O_OPOST: u32 = 0x0001;
/// `c_oflag` bit: map lower case to upper on output.
pub const O_OLCUC: u32 = 0x0002;
/// `c_oflag` bit: a newline also returns the carriage, so it goes out as CR-NL.
pub const O_ONLCR: u32 = 0x0004;
/// `c_oflag` bit: a carriage return goes out as a newline instead.
pub const O_OCRNL: u32 = 0x0008;
/// `c_oflag` bit: a carriage return at column zero is not sent.
pub const O_ONOCR: u32 = 0x0010;
/// `c_oflag` bit: the terminal's own newline returns the carriage, so the
/// driver's idea of the column must be reset by one even without `ONLCR`.
pub const O_ONLRET: u32 = 0x0020;

/// The character in `c_cc` that means "this one is switched off".
///
/// `_POSIX_VDISABLE` is 0 on Linux, and `n_tty` checks it before every single
/// `c_cc` comparison. Skipping the check does not disable anything — it aims
/// the character at byte `0x00` instead, which is a byte a program can
/// perfectly well send.
pub const VDISABLE: u8 = 0;

/// Which signal a byte asks for, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtySignal {
    /// `VINTR`, Ctrl-C by default: SIGINT.
    Intr,
    /// `VQUIT`, Ctrl-\ by default: SIGQUIT.
    Quit,
    /// `VSUSP`, Ctrl-Z by default: SIGTSTP.
    Susp,
}

/// How much input a line discipline may keep waiting for its reader.
///
/// Linux reads into a fixed `read_buf[N_TTY_BUF_SIZE]` (`drivers/tty/n_tty.c`)
/// and works out the room left before every batch. The committed input and the
/// line still being edited are two indices into that one buffer, so they share
/// the number.
///
/// There are three line disciplines in this tree and they gave three answers:
/// the console (`fs/stdio.rs`) and the live PTY (`fs/pty.rs`) had no bound at
/// all, and `fs/devfs/pty.rs` had 16 KiB. It is one question, so it is answered
/// here.
pub const N_TTY_BUF_SIZE: usize = 4096;

/// How much output may wait for a terminal to read it.
///
/// The other direction, which Linux bounds somewhere else: a write to a PTY
/// slave lands in the master port's flip buffer, capped at
/// `TTYB_DEFAULT_MEM_LIMIT` (`drivers/tty/tty_buffer.c`). It is what makes
/// `yes > /dev/pts/N` with nobody reading block instead of eating the machine.
pub const TTY_OUTPUT_CAP: usize = 640 * 1024;

/// What a line discipline should do with the next input byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputRoom {
    /// There is room: store it.
    Store,
    /// Full, but everything in the queue is one line still being edited.
    ///
    /// The byte is still processed -- VERASE and VKILL *have* to reach the
    /// discipline, or the terminal wedges at the one moment the user needs to
    /// shorten the line -- and a data character is dropped instead of stored.
    /// Linux calls this `overflow` and says why in its own comment: "let
    /// characters through without limit, so that erase characters will be
    /// handled".
    Process,
    /// Full, with committed input a reader has not taken yet.
    ///
    /// A discipline with a writer to answer says so, with a short write. One
    /// fed by a keyboard has nobody to tell and no way to ask it to wait, so it
    /// treats this like [`InputRoom::Process`]: everything is still
    /// interpreted -- a Ctrl-C is how a user rescues the program that stopped
    /// reading in the first place, so losing it here would be losing it exactly
    /// when it is needed -- and only the storing stops.
    Full,
}

/// Room for one more input byte, given what is already queued.
///
/// `committed` is what a reader could take right now and `editing` is the line
/// being assembled behind it, both in **bytes**: a discipline that keeps its
/// line as characters has to spell it out first, because the bound Linux uses
/// is a buffer size and not a keystroke count.
pub fn input_room(committed: usize, editing: usize, canonical: bool) -> InputRoom {
    if committed + editing < N_TTY_BUF_SIZE {
        InputRoom::Store
    } else if canonical && committed == 0 {
        InputRoom::Process
    } else {
        InputRoom::Full
    }
}

/// Whether a discipline with nobody to answer should store one more byte.
///
/// [`InputRoom::Process`] and [`InputRoom::Full`] are one thing to a console
/// or to any other end fed by something that cannot be asked to wait: keep
/// interpreting, stop storing. Only an end with a writer in front of it can
/// act on the difference, by handing back a short count. So the others ask
/// here, rather than passing a `canonical` that cannot change the answer.
pub fn has_input_room(committed: usize, editing: usize) -> bool {
    input_room(committed, editing, false) == InputRoom::Store
}

/// Whether `b` continues a UTF-8 character rather than starting one.
///
/// A rubout moves the cursor one column, and a character several bytes long
/// still occupies one, so this is what tells a byte that needs its own rubout
/// from one that rides along with the byte before it.
pub fn utf8_continuation(b: u8) -> bool {
    b & 0xc0 == 0x80
}

/// How many bytes at the end of `line` make up the one character that a single
/// erase should take away.
///
/// A terminal that stores its pending line as bytes has to be told that a
/// character can be more than one of them, or Backspace over `ñ` leaves the
/// `0xc3` behind: half a character, which is not a character at all, and which
/// the program then reads as a byte that cannot be decoded.
///
/// Malformed input still moves: a run of continuation bytes with nothing
/// starting it is capped at the four bytes a character can be, so an erase
/// always takes at least one byte and never walks off the line.
pub fn utf8_erase_len(line: &[u8]) -> usize {
    if line.is_empty() {
        return 0;
    }
    let mut n = 1;
    while n < line.len() && n < 4 && utf8_continuation(line[line.len() - n]) {
        n += 1;
    }
    n
}

/// What a `read` on a terminal in non-canonical mode may do right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtyRead {
    /// Hand back exactly this many bytes. Never zero.
    Take(usize),
    /// Come back with nothing: either the caller asked for nothing, or the
    /// settings say a read with an empty queue is over rather than waiting.
    Now,
    /// Not enough yet. Block, or `EAGAIN` on a non-blocking descriptor.
    Wait,
}

/// What the line-kill character (Ctrl-U) writes back to the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillEcho {
    /// `ECHOKE`: walk the cursor back over the line that was discarded.
    Rubout,
    /// `ECHOK`: leave the line on screen and move to the next one.
    Newline,
    /// Neither flag: the line goes away with nothing to show for it.
    Nothing,
}

impl Termios {
    /// True when input is delivered a line at a time (`ICANON`).
    pub fn canonical(&self) -> bool {
        self.c_lflag & L_ICANON != 0
    }

    /// `c_cc[VMIN]`: how many bytes a non-canonical read waits for.
    pub fn vmin(&self) -> u8 {
        self.c_cc[VMIN_CC]
    }

    /// `c_cc[VTIME]`: the non-canonical read timer, in deciseconds.
    pub fn vtime(&self) -> u8 {
        self.c_cc[VTIME_CC]
    }

    /// How many bytes must be queued before a non-canonical read of `buf_len`
    /// can come back on count alone.
    ///
    /// A buffer smaller than VMIN lowers the bar to the buffer: waiting for
    /// more than the caller has room for would never end. The floor of one is
    /// what makes this a count and not a way out with nothing — a read that
    /// returns empty is always the timer's doing, never the count's.
    ///
    /// This is the number a blocking reader parks on, so it has to be
    /// available apart from the decision itself: the thing that wakes the
    /// reader knows what is queued but not what was asked for.
    pub fn noncanon_need(&self, buf_len: usize) -> usize {
        (self.vmin() as usize).min(buf_len).max(1)
    }

    /// The POSIX non-canonical read rule (`termios(3)`, "Canonical and
    /// noncanonical mode"), with `queued` bytes buffered and room for
    /// `buf_len`.
    ///
    /// `timed_out` says whether the VTIME timer armed for this read has
    /// already expired. The clock is a parameter and not a call to it on
    /// purpose: what a terminal read is allowed to do is a rule about four
    /// numbers, and a rule that has to be asked through a clock can only be
    /// checked by a test that waits.
    ///
    /// The four cases are Linux's `n_tty_read`, where `minimum` is
    /// `min(nr, MIN_CHAR(tty))` and the timer is `TIME_CHAR(tty)`:
    ///
    /// | VMIN | VTIME | when the read comes back |
    /// | --- | --- | --- |
    /// | 0 | 0 | at once, with whatever is queued, even nothing |
    /// | 0 | >0 | on the first byte, or when the timer runs out |
    /// | >0 | 0 | on the VMIN'th byte, however long that takes |
    /// | >0 | >0 | on the VMIN'th byte, or when the gap between bytes runs the timer out (never with nothing) |
    pub fn noncanon_read(&self, queued: usize, buf_len: usize, timed_out: bool) -> TtyRead {
        // A read that asked for no bytes is over before any of this; it is the
        // one way out with nothing that does not depend on the settings.
        if buf_len == 0 {
            return TtyRead::Now;
        }
        let vmin = self.vmin() as usize;
        let vtime = self.vtime();
        let need = self.noncanon_need(buf_len);
        if queued >= need {
            return TtyRead::Take(queued.min(buf_len));
        }
        if vmin == 0 {
            // No minimum, so nothing queued is an answer: give it now unless a
            // timer was asked for and has not run out yet.
            if vtime == 0 || timed_out {
                return TtyRead::Now;
            }
            return TtyRead::Wait;
        }
        // VMIN > 0: the read always comes back with at least one byte, so an
        // empty queue waits however long it takes. VTIME here is the gap
        // *between* bytes, and its timer only starts once the first one is in.
        if queued > 0 && vtime > 0 && timed_out {
            return TtyRead::Take(queued.min(buf_len));
        }
        TtyRead::Wait
    }

    /// What the line-kill character echoes, given `ECHO`, `ECHOKE` and
    /// `ECHOK`.
    ///
    /// `ECHO` gates all three: a program that turned echo off (a password
    /// prompt) may not have the line it is hiding painted back over the
    /// screen, even as rubouts. Past that, `ECHOKE` wins over `ECHOK` —
    /// that is `n_tty`'s order in `eraser()`, and it matters because the
    /// cooked default has `ECHOK` on and `ECHOKE` off, so the stock answer
    /// is a newline and not a rubout.
    pub fn kill_echo(&self) -> KillEcho {
        if self.c_lflag & L_ECHO == 0 {
            return KillEcho::Nothing;
        }
        if self.c_lflag & L_ECHOKE != 0 {
            KillEcho::Rubout
        } else if self.c_lflag & L_ECHOK != 0 {
            KillEcho::Newline
        } else {
            KillEcho::Nothing
        }
    }

    /// True when the terminal says its input is UTF-8 (`IUTF8`), which is what
    /// makes line editing work on characters instead of bytes.
    pub fn utf8_input(&self) -> bool {
        self.c_iflag & I_IUTF8 != 0
    }

    /// Which signal, if any, the byte `b` asks the line discipline to raise.
    ///
    /// Two rules live here rather than in each discipline, because there are
    /// three of them and they have not agreed before. First, `ISIG` off means
    /// none of these characters is special at all. Second, a `c_cc` slot set
    /// to [`VDISABLE`] is *switched off*, so it must not be compared against:
    /// a terminal that turns Ctrl-C off by writing a zero there would
    /// otherwise have every `0x00` byte in its input raise SIGINT, which is
    /// the opposite of what it asked for.
    pub fn tty_signal(&self, b: u8) -> Option<TtySignal> {
        if self.c_lflag & L_ISIG == 0 {
            return None;
        }
        for (idx, sig) in [
            (VINTR_CC, TtySignal::Intr),
            (VQUIT_CC, TtySignal::Quit),
            (VSUSP_CC, TtySignal::Susp),
        ] {
            if self.c_cc[idx] != VDISABLE && self.c_cc[idx] == b {
                return Some(sig);
            }
        }
        None
    }

    /// True when a newline typed in canonical mode is echoed.
    ///
    /// `ECHONL` is the one thing a terminal with echo *off* still shows, and
    /// it exists for exactly one caller: `getpass(3)` clears `ECHO` and sets
    /// `ECHONL`, so that the Enter that ends a password still moves the
    /// cursor off the prompt line.
    pub fn echoes_newline(&self) -> bool {
        self.c_lflag & L_ECHO != 0 || self.c_lflag & L_ECHONL != 0
    }

    /// What the byte `c` becomes on its way to the terminal, and where that
    /// leaves the cursor.
    ///
    /// `OPOST` is the switch for the whole `c_oflag` word: with it off the
    /// byte goes out untouched, and with it on every other bit in the word
    /// gets a say. All three line disciplines in this tree instead asked for
    /// `OPOST && ONLCR` together and did nothing otherwise, in six separate
    /// places -- so `OCRNL`, `ONOCR`, `ONLRET` and `OLCUC` were unreachable by
    /// construction, however a program set them. It is one question, asked
    /// once per output byte, so it is answered here.
    ///
    /// `column` is the driver's own count of how far along the line the cursor
    /// is. It is not bookkeeping for its own sake: `ONOCR` is *defined* in
    /// terms of it (a carriage return at column zero is the one that is not
    /// sent), and `ONLRET` exists only to keep it honest on a terminal whose
    /// newline returns the carriage by itself. A discipline that tracked no
    /// column could implement neither.
    ///
    /// Follows `do_output_char` (`drivers/tty/n_tty.c`) except for `TABDLY ==
    /// XTABS`, tab expansion to spaces, which is left out: it is the one rule
    /// here that turns one byte into up to eight, and nothing in this tree
    /// sets it. The column still advances to the next tab stop, which is what
    /// a terminal that expands its own tabs needs.
    pub fn output_char(&self, c: u8, column: &mut usize) -> Output {
        if self.c_oflag & O_OPOST == 0 {
            return Output::one(c);
        }
        match c {
            b'\n' => {
                if self.c_oflag & O_ONLRET != 0 {
                    *column = 0;
                }
                if self.c_oflag & O_ONLCR != 0 {
                    *column = 0;
                    return Output::two(b'\r', b'\n');
                }
                // Neither flag: the terminal moves down and stays where it
                // was across, so the column is deliberately left alone.
                Output::one(b'\n')
            }
            b'\r' => {
                if self.c_oflag & O_ONOCR != 0 && *column == 0 {
                    return Output::none();
                }
                if self.c_oflag & O_OCRNL != 0 {
                    if self.c_oflag & O_ONLRET != 0 {
                        *column = 0;
                    }
                    // Deliberately *not* re-entering the newline rule above:
                    // Linux breaks out of its switch here, so a carriage
                    // return turned into a newline goes out as the one byte
                    // even with `ONLCR` set.
                    return Output::one(b'\n');
                }
                *column = 0;
                Output::one(b'\r')
            }
            b'\t' => {
                *column = column.saturating_add(8 - (*column & 7));
                Output::one(b'\t')
            }
            0x08 => {
                *column = column.saturating_sub(1);
                Output::one(0x08)
            }
            _ => {
                let mut c = c;
                if !c.is_ascii_control() {
                    if self.c_oflag & O_OLCUC != 0 {
                        c = c.to_ascii_uppercase();
                    }
                    // A character several bytes long occupies one column, so
                    // only the byte that starts it counts -- and only when the
                    // terminal has been told its input is UTF-8 at all.
                    if !(self.utf8_input() && utf8_continuation(c)) {
                        *column = column.saturating_add(1);
                    }
                }
                Output::one(c)
            }
        }
    }
}

/// What one byte turns into on its way out of a terminal: nothing at all,
/// itself, or a short run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Output {
    buf: [u8; 2],
    len: u8,
}

impl Output {
    /// The byte is swallowed. Only `ONOCR` does this.
    const fn none() -> Self {
        Output {
            buf: [0; 2],
            len: 0,
        }
    }

    const fn one(b: u8) -> Self {
        Output {
            buf: [b, 0],
            len: 1,
        }
    }

    const fn two(a: u8, b: u8) -> Self {
        Output {
            buf: [a, b],
            len: 2,
        }
    }

    /// The bytes to send, which may be none at all.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }

    /// True when nothing at all goes out for this byte.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[cfg(not(target_arch = "mips"))]
pub const TIOCGPGRP: usize = 0x540F;
// _IOR('t', 119, int)
#[cfg(target_arch = "mips")]
pub const TIOCGPGRP: usize = 0x4_004_74_77;

#[cfg(not(target_arch = "mips"))]
pub const TIOCSPGRP: usize = 0x5410;
// _IOW('t', 118, int)
#[cfg(target_arch = "mips")]
pub const TIOCSPGRP: usize = 0x8_004_74_76;

#[cfg(not(target_arch = "mips"))]
pub const TIOCGWINSZ: usize = 0x5413;
// _IOR('t', 104, struct winsize)
#[cfg(target_arch = "mips")]
pub const TIOCGWINSZ: usize = 0x4_008_74_68;

#[cfg(not(target_arch = "mips"))]
pub const TIOCSWINSZ: usize = 0x5414;
// _IOW('t', 103, struct winsize)
#[cfg(target_arch = "mips")]
pub const TIOCSWINSZ: usize = 0x8_008_74_67;

/// Linux-specific console multiplexor ioctl; the subcommand is the first byte
/// of the argument (e.g. 6 = `TIOCL_GETSHIFTSTATE`).
pub const TIOCLINUX: usize = 0x541c;
/// `TIOCLINUX` subcommand: read the keyboard shift/modifier state.
pub const TIOCL_GETSHIFTSTATE: u8 = 6;

#[cfg(not(target_arch = "mips"))]
pub const FIONCLEX: usize = 0x5450;
#[cfg(target_arch = "mips")]
pub const FIONCLEX: usize = 0x6602;

#[cfg(not(target_arch = "mips"))]
pub const FIOCLEX: usize = 0x5451;
#[cfg(target_arch = "mips")]
pub const FIOCLEX: usize = 0x6601;

// rustc using pipe and ioctl pipe file with this request id
// for non-blocking/blocking IO control setting
pub const FIONBIO: usize = 0x5421;

// Queue / session ioctls (`<asm-generic/ioctls.h>`).
/// Bytes available to read (a.k.a. `TIOCINQ`); written as an `int`.
pub const FIONREAD: usize = 0x541B;
/// Alias of [`FIONREAD`] — bytes waiting in the TTY input queue.
pub const TIOCINQ: usize = FIONREAD;
/// Bytes still queued in the TTY output buffer; written as an `int`.
pub const TIOCOUTQ: usize = 0x5411;
/// Get the session ID of the terminal; written as a `pid_t` (`int`).
pub const TIOCGSID: usize = 0x5429;
/// Get serial line interrupt counters into a [`SerialIcounter`].
pub const TIOCGICOUNT: usize = 0x545D;

/// Linux `struct serial_icounter_struct` — cumulative serial line event
/// counters reported by `TIOCGICOUNT`. Virtual TTYs have no real UART, so all
/// fields are reported as zero.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SerialIcounter {
    pub cts: i32,
    pub dsr: i32,
    pub rng: i32,
    pub dcd: i32,
    pub rx: i32,
    pub tx: i32,
    pub frame: i32,
    pub overrun: i32,
    pub parity: i32,
    pub brk: i32,
    pub buf_overrun: i32,
    pub reserved: [i32; 9],
}

// Modem control line ioctls (`<asm-generic/termios.h>`). The argument is an
// `int` bitmask of the `TIOCM_*` flags below.
/// Read the state of the modem control lines into an `int`.
pub const TIOCMGET: usize = 0x5415;
/// Set the modem control lines to the given bitmask.
pub const TIOCMSET: usize = 0x5418;
/// Set (OR in) the given modem control line bits.
pub const TIOCMBIS: usize = 0x5416;
/// Clear (AND out) the given modem control line bits.
pub const TIOCMBIC: usize = 0x5417;

/// DTR (Data Terminal Ready) output line.
pub const TIOCM_DTR: i32 = 0x002;
/// RTS (Request To Send) output line.
pub const TIOCM_RTS: i32 = 0x004;
/// CTS (Clear To Send) input line.
pub const TIOCM_CTS: i32 = 0x020;
/// Carrier Detect input line (a.k.a. `TIOCM_CD`).
pub const TIOCM_CAR: i32 = 0x040;
/// DSR (Data Set Ready) input line.
pub const TIOCM_DSR: i32 = 0x100;

// VT / KD console ioctls (Linux `<linux/kd.h>`).
/// Get console mode (`KD_TEXT` / `KD_GRAPHICS`) into an `int`.
pub const KDGETMODE: usize = 0x4B3B;
/// Set console mode from an `int` (`KD_TEXT` / `KD_GRAPHICS`).
pub const KDSETMODE: usize = 0x4B3A;
/// Text mode: the kernel draws the framebuffer console.
pub const KD_TEXT: usize = 0x00;
/// Graphics mode: userspace owns the framebuffer; the console stops drawing.
pub const KD_GRAPHICS: usize = 0x01;

/// Get keyboard type (`<linux/kd.h>`), written as a single `char`. Used by X to
/// validate that a file descriptor is really a virtual console.
pub const KDGKBTYPE: usize = 0x4B33;
/// 101-key PC keyboard — the value reported by `KDGKBTYPE`.
pub const KB_101: u8 = 0x02;

/// Get keyboard translation mode (`K_RAW` / `K_XLATE` / ...) into an `int`.
pub const KDGKBMODE: usize = 0x4B44;
/// Set keyboard translation mode from an `int`.
pub const KDSKBMODE: usize = 0x4B45;
/// Raw scancodes; the kernel does no translation.
pub const K_RAW: i32 = 0x00;
/// Cooked mode: keycodes translated to characters (the default).
pub const K_XLATE: i32 = 0x01;
/// Medium-raw keycodes.
pub const K_MEDIUMRAW: i32 = 0x02;
/// Unicode translation.
pub const K_UNICODE: i32 = 0x03;
/// Keyboard input disabled — used by X/Wayland while they own input via evdev.
pub const K_OFF: i32 = 0x04;

// Virtual terminal ioctls (Linux `<linux/vt.h>`).
/// Find the first free VT number; writes a 1-based VT index into an `int`.
pub const VT_OPENQRY: usize = 0x5600;
/// Get the VT switching mode into a [`VtMode`].
pub const VT_GETMODE: usize = 0x5601;
/// Set the VT switching mode from a [`VtMode`].
pub const VT_SETMODE: usize = 0x5602;
/// Get global VT state into a [`VtStat`].
pub const VT_GETSTATE: usize = 0x5603;
/// Acknowledge a VT release/acquire (arg by value).
pub const VT_RELDISP: usize = 0x5605;
/// Make the given (1-based) VT active (arg by value).
pub const VT_ACTIVATE: usize = 0x5606;
/// Wait until the given (1-based) VT is active (arg by value).
pub const VT_WAITACTIVE: usize = 0x5607;
/// Deallocate the given VT (arg by value).
pub const VT_DISALLOCATE: usize = 0x5608;

/// `mode` value: kernel handles VT switches automatically (default).
pub const VT_AUTO: u8 = 0x00;
/// `mode` value: the process handles VT switches via signals.
pub const VT_PROCESS: u8 = 0x01;

/// `VT_RELDISP` argument: the process acknowledges it has acquired the VT (the
/// reply to `acqsig` on a switch-*to*). A non-zero, non-`VT_ACKACQ` argument on
/// a switch-*from* instead completes the release.
pub const VT_ACKACQ: usize = 0x02;

/// Linux `struct vt_mode` — VT switch signalling configuration.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VtMode {
    pub mode: u8,
    pub waitv: u8,
    pub relsig: i16,
    pub acqsig: i16,
    pub frsig: i16,
}

impl VtMode {
    /// Default mode: automatic, kernel-driven VT switching.
    pub const fn auto() -> Self {
        Self {
            mode: VT_AUTO,
            waitv: 0,
            relsig: 0,
            acqsig: 0,
            frsig: 0,
        }
    }
}

/// Linux `struct vt_stat` — global VT state returned by `VT_GETSTATE`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VtStat {
    pub v_active: u16,
    pub v_signal: u16,
    pub v_state: u16,
}

// Misc TTY control ioctls an X server may issue; accepted as no-ops.
/// Flush the terminal queues.
pub const TCFLSH: usize = 0x540B;
/// Make the terminal the controlling TTY of the calling process.
pub const TIOCSCTTY: usize = 0x540E;
/// Give up the controlling TTY.
pub const TIOCNOTTY: usize = 0x5422;

// Pseudo-terminal (PTY) ioctls issued on the `/dev/ptmx` master.
/// Get the PTY number of the master (`unsigned int` out) — `ptsname(3)` uses it
/// to build `/dev/pts/N`.
pub const TIOCGPTN: usize = 0x8004_5430;
/// Lock/unlock the PTY slave (`int` in); `unlockpt(3)` writes 0 here.
pub const TIOCSPTLCK: usize = 0x4004_5431;
/// Open the slave side of the master without a path (returns an fd). Accepted
/// best-effort; most libcs fall back to `open("/dev/pts/N")`.
pub const TIOCGPTPEER: usize = 0x5441;

/// Get keyboard LED state (Scroll/Num/Caps) into an `int`.
pub const KDGETLED: usize = 0x4B11;
/// Set keyboard LED state from an `int` (by value, not a pointer).
pub const KDSETLED: usize = 0x4B32;
/// Read one keymap entry into a [`KbEntry`].
pub const KDGKBENT: usize = 0x4B46;
/// Console bell tone (accepted as a no-op).
pub const KDMKTONE: usize = 0x4B30;

/// Key-type field in a `kb_value` (`KTYP(x) == (x >> 8)`).
pub const KT_LATIN: u16 = 0;
pub const KT_FN: u16 = 1;
pub const KT_SPEC: u16 = 2;

/// Linux `struct kbentry` — one keymap cell for `KDGKBENT`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct KbEntry {
    pub kb_table: u8,
    pub kb_index: u8,
    pub kb_value: u16,
}

#[cfg(test)]
mod termios_tests {
    //! The rules a terminal read and a terminal echo follow, asked directly.
    //!
    //! There are two line disciplines in this kernel — the console's in
    //! `stdio.rs` and the pseudo-terminal's in `pty.rs` — and they are separate
    //! implementations of the same POSIX text. Anything both must agree on
    //! belongs here, once, where a test can reach it without a terminal.
    //!
    //! These functions touch no clock and no queue, so nothing here is timing
    //! dependent: `timed_out` is the clock's answer handed in as a fact.

    use super::*;

    fn raw(vmin: u8, vtime: u8) -> Termios {
        let mut t = Termios::default_tty();
        t.c_lflag &= !L_ICANON;
        t.c_cc[VMIN_CC] = vmin;
        t.c_cc[VTIME_CC] = vtime;
        t
    }

    #[test]
    fn the_signal_characters_are_the_ones_termbits_h_names() {
        // Indices into `c_cc` and the bytes a cooked terminal starts with.
        // Three line disciplines read these, so they are pinned here once.
        assert_eq!(VINTR_CC, 0);
        assert_eq!(VQUIT_CC, 1);
        assert_eq!(VSUSP_CC, 10);
        assert_eq!(L_ISIG, 0o000001);
        assert_eq!(VDISABLE, 0);

        let t = Termios::default_tty();
        assert_eq!(t.c_cc[VINTR_CC], 3, "Ctrl-C");
        assert_eq!(t.c_cc[VQUIT_CC], 28, "Ctrl-backslash");
        assert_eq!(t.c_cc[VSUSP_CC], 26, "Ctrl-Z");
    }

    #[test]
    fn a_cooked_terminal_answers_the_three_signal_characters() {
        let t = Termios::default_tty();
        assert_eq!(t.tty_signal(3), Some(TtySignal::Intr));
        assert_eq!(t.tty_signal(28), Some(TtySignal::Quit));
        assert_eq!(t.tty_signal(26), Some(TtySignal::Susp));
        assert_eq!(t.tty_signal(b'a'), None);
    }

    #[test]
    fn a_signal_character_moved_to_another_byte_follows_it() {
        // `stty intr ^X` is a real thing people do.
        let mut t = Termios::default_tty();
        t.c_cc[VINTR_CC] = 24; // Ctrl-X
        assert_eq!(t.tty_signal(24), Some(TtySignal::Intr));
        assert_eq!(t.tty_signal(3), None, "el viejo Ctrl-C ya no es nada");
    }

    #[test]
    fn a_signal_character_switched_off_does_not_answer_to_a_zero_byte() {
        // `stty intr undef` writes VDISABLE, which is the byte 0. Comparing
        // against it without checking first does not switch the character
        // off: it points it at 0x00, a byte any program can send.
        let mut t = Termios::default_tty();
        t.c_cc[VINTR_CC] = VDISABLE;
        assert_eq!(t.tty_signal(0), None, "un NUL no es un Ctrl-C");
        assert_eq!(t.tty_signal(3), None, "y Ctrl-C tampoco lo es ya");
        // The other two still answer, because only one was switched off.
        assert_eq!(t.tty_signal(28), Some(TtySignal::Quit));
        assert_eq!(t.tty_signal(26), Some(TtySignal::Susp));
    }

    #[test]
    fn every_c_cc_switched_off_leaves_a_zero_byte_ordinary() {
        let mut t = Termios::default_tty();
        for idx in [VINTR_CC, VQUIT_CC, VSUSP_CC] {
            t.c_cc[idx] = VDISABLE;
        }
        assert_eq!(t.tty_signal(0), None);
    }

    #[test]
    fn isig_off_means_no_byte_is_special() {
        // Raw mode: a program driving the terminal itself wants Ctrl-C as a
        // byte, not as a signal.
        let mut t = Termios::default_tty();
        t.c_lflag &= !L_ISIG;
        for b in [3u8, 28, 26] {
            assert_eq!(t.tty_signal(b), None, "{}", b);
        }
    }

    #[test]
    fn room_is_measured_across_both_queues() {
        // Two queues here, one `read_buf` in `n_tty`: `canon_head` and
        // `read_tail` are indices into it. A budget each would double the
        // bound, and a discipline that reports `TIOCOUTQ` already adds them.
        assert_eq!(input_room(0, 0, true), InputRoom::Store);
        assert_eq!(input_room(N_TTY_BUF_SIZE - 1, 0, false), InputRoom::Store);
        assert_eq!(input_room(0, N_TTY_BUF_SIZE - 1, true), InputRoom::Store);
        let half = N_TTY_BUF_SIZE / 2;
        assert_eq!(input_room(half, half - 1, true), InputRoom::Store);
        assert_eq!(input_room(half, half, true), InputRoom::Full);
    }

    #[test]
    fn a_full_line_with_nothing_behind_it_keeps_being_taken() {
        // The one case the bound may not refuse. Refusing here refuses VERASE
        // and VKILL with it, and the terminal wedges at exactly the moment the
        // user needs to shorten the line.
        assert_eq!(input_room(0, N_TTY_BUF_SIZE, true), InputRoom::Process);
        assert_eq!(input_room(0, N_TTY_BUF_SIZE * 2, true), InputRoom::Process);
    }

    #[test]
    fn committed_input_nobody_has_read_makes_the_queue_full() {
        // One byte a reader has not taken and the exception is gone: the line
        // is no longer all there is, so there is somewhere for the pressure to
        // go and a writer can be told to wait.
        assert_eq!(input_room(1, N_TTY_BUF_SIZE - 1, true), InputRoom::Full);
        assert_eq!(input_room(N_TTY_BUF_SIZE, 0, true), InputRoom::Full);
    }

    #[test]
    fn the_shortcut_and_the_rule_answer_alike() {
        // `has_input_room` is the same question asked by an end that cannot
        // act on the difference between the two ways of being full. It has to
        // agree with the rule about the one case it does report.
        for (committed, editing) in [
            (0, 0),
            (1, N_TTY_BUF_SIZE - 2),
            (0, N_TTY_BUF_SIZE - 1),
            (0, N_TTY_BUF_SIZE),
            (N_TTY_BUF_SIZE, 0),
            (1, N_TTY_BUF_SIZE),
        ] {
            for canonical in [false, true] {
                assert_eq!(
                    has_input_room(committed, editing),
                    input_room(committed, editing, canonical) == InputRoom::Store,
                    "{committed} + {editing}, canonical {canonical}"
                );
            }
        }
    }

    #[test]
    fn raw_mode_never_gets_the_editing_exception() {
        // There is no line being edited without ICANON, so there is nothing
        // an extra byte could be needed for.
        assert_eq!(input_room(0, N_TTY_BUF_SIZE, false), InputRoom::Full);
        assert_eq!(input_room(N_TTY_BUF_SIZE, 0, false), InputRoom::Full);
    }

    #[test]
    fn the_two_bounds_are_the_numbers_linux_uses() {
        // Not arbitrary and not ours to round off: a program sized against a
        // Linux terminal -- a shell reading a line, a pager filling a screen --
        // is sized against these. Three disciplines in this tree used to give
        // three answers: none, none, and 16 KiB for both directions at once.
        assert_eq!(N_TTY_BUF_SIZE, 4096); // drivers/tty/n_tty.c
        assert_eq!(TTY_OUTPUT_CAP, 640 * 1024); // TTYB_DEFAULT_MEM_LIMIT
    }

    #[test]
    fn every_number_here_is_the_one_in_termbits_h() {
        // UABI: these are positions in a word and offsets into an array that
        // userspace fills in, so a wrong one compiles, runs, and answers a
        // question nobody asked. Written in octal, which is how
        // `include/uapi/asm-generic/termbits.h` writes them, so the two can be
        // read side by side.
        assert_eq!(L_ICANON, 0o000002);
        assert_eq!(L_ECHO, 0o000010);
        assert_eq!(L_ECHOK, 0o000040);
        assert_eq!(L_ECHONL, 0o000100);
        assert_eq!(L_ECHOKE, 0o004000);
        assert_eq!(VKILL_CC, 3);
        assert_eq!(VTIME_CC, 5);
        assert_eq!(VMIN_CC, 6);
        // And the five flags are five different bits.
        let bits = [L_ICANON, L_ECHO, L_ECHOK, L_ECHONL, L_ECHOKE];
        for (i, a) in bits.iter().enumerate() {
            for b in &bits[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn the_cooked_default_is_one_byte_and_no_timer() {
        let t = Termios::default_tty();
        assert!(t.canonical(), "the stock terminal is line at a time");
        // Linux INIT_C_CC: VMIN=1, VTIME=0. A raw-mode read on a terminal
        // nobody reconfigured therefore blocks for one byte, which is what
        // every `stty raw` without `min`/`time` relies on.
        assert_eq!(t.vmin(), 1);
        assert_eq!(t.vtime(), 0);
    }

    #[test]
    fn min_zero_time_zero_comes_back_with_nothing_rather_than_waiting() {
        // The polling read: `stty -icanon min 0 time 0`. This is the one that
        // must never block, and the one a queue-only implementation gets
        // wrong, because "no bytes" looks exactly like "not ready yet".
        let t = raw(0, 0);
        assert_eq!(t.noncanon_read(0, 64, false), TtyRead::Now);
        assert_eq!(t.noncanon_read(3, 64, false), TtyRead::Take(3));
    }

    #[test]
    fn min_zero_with_a_timer_waits_until_the_timer_runs_out() {
        let t = raw(0, 2);
        assert_eq!(t.noncanon_read(0, 64, false), TtyRead::Wait);
        assert_eq!(
            t.noncanon_read(0, 64, true),
            TtyRead::Now,
            "the timer running out ends the read with nothing"
        );
        assert_eq!(
            t.noncanon_read(1, 64, false),
            TtyRead::Take(1),
            "a byte ends it early, timer or no timer"
        );
    }

    #[test]
    fn a_minimum_of_one_waits_for_its_byte_however_long_that_takes() {
        let t = raw(1, 0);
        assert_eq!(t.noncanon_read(0, 64, false), TtyRead::Wait);
        assert_eq!(t.noncanon_read(0, 64, true), TtyRead::Wait);
        assert_eq!(t.noncanon_read(1, 64, false), TtyRead::Take(1));
    }

    #[test]
    fn a_minimum_above_one_holds_the_bytes_back_until_it_is_met() {
        let t = raw(4, 0);
        assert_eq!(t.noncanon_read(1, 64, false), TtyRead::Wait);
        assert_eq!(t.noncanon_read(3, 64, false), TtyRead::Wait);
        assert_eq!(t.noncanon_read(4, 64, false), TtyRead::Take(4));
    }

    #[test]
    fn everything_queued_goes_back_at_once_and_not_just_the_minimum() {
        // `n_tty_read` copies up to `nr`; VMIN decides *when* the read returns,
        // not how much it is allowed to carry. Capping at VMIN would leave the
        // rest queued and make a reader go round again for bytes it had room
        // for.
        let t = raw(4, 0);
        assert_eq!(t.noncanon_read(9, 64, false), TtyRead::Take(9));
    }

    #[test]
    fn a_buffer_smaller_than_the_minimum_lowers_the_bar_to_the_buffer() {
        // Otherwise a `read(fd, buf, 2)` with `min 4` could never return: the
        // caller cannot take four bytes, so waiting for four is waiting for
        // ever.
        let t = raw(4, 0);
        assert_eq!(t.noncanon_read(2, 2, false), TtyRead::Take(2));
        assert_eq!(t.noncanon_read(1, 2, false), TtyRead::Wait);
    }

    #[test]
    fn asking_for_no_bytes_is_over_before_the_settings_are_consulted() {
        for (vmin, vtime) in [(0u8, 0u8), (1, 0), (4, 2), (0, 5)] {
            assert_eq!(
                raw(vmin, vtime).noncanon_read(7, 0, false),
                TtyRead::Now,
                "min {} time {}",
                vmin,
                vtime
            );
        }
    }

    #[test]
    fn with_both_set_the_timer_is_the_gap_between_bytes_and_not_the_wait_for_the_first() {
        // POSIX: with VMIN > 0 and VTIME > 0 the timer starts after the first
        // byte arrives, so a read that has nothing yet is not allowed to give
        // up. Starting it at the read instead would turn every `min 1 time N`
        // program — the common terminal setting — into one that returns zero
        // bytes on an idle terminal, which reads as end of file.
        let t = raw(4, 1);
        assert_eq!(t.noncanon_read(0, 64, true), TtyRead::Wait);
        assert_eq!(t.noncanon_read(2, 64, true), TtyRead::Take(2));
        assert_eq!(t.noncanon_read(2, 64, false), TtyRead::Wait);
    }

    #[test]
    fn an_erase_takes_the_whole_character_and_not_one_of_its_bytes() {
        // `ñ` is 0xc3 0xb1 and `€` is 0xe2 0x82 0xac. Taking one byte leaves
        // half a character, which is not a character at all: the program then
        // reads a byte that cannot be decoded, and the screen and the line no
        // longer agree on how much is there.
        assert_eq!(utf8_erase_len(b"hola"), 1);
        assert_eq!(utf8_erase_len("añ".as_bytes()), 2);
        assert_eq!(utf8_erase_len("a€".as_bytes()), 3);
        assert_eq!(utf8_erase_len("a😀".as_bytes()), 4);
    }

    #[test]
    fn an_erase_on_an_empty_line_takes_nothing() {
        assert_eq!(utf8_erase_len(b""), 0);
    }

    #[test]
    fn malformed_input_still_erases_and_never_walks_off_the_line() {
        // A run of continuation bytes with nothing starting them cannot be a
        // character. An erase that kept walking would empty the whole line on
        // one Backspace; one that refused to move would wedge it.
        assert_eq!(utf8_erase_len(&[0x80]), 1);
        assert_eq!(utf8_erase_len(&[0x80, 0x80, 0x80, 0x80, 0x80]), 4);
        assert_eq!(utf8_erase_len(&[b'a', 0x80]), 2);
    }

    #[test]
    fn only_the_bytes_that_ride_along_are_continuations() {
        // One rubout per column: the lead byte of a character asks for one,
        // the bytes that only continue it do not.
        for c in ['a', 'ñ', '€', '😀'] {
            let mut buf = [0u8; 4];
            let bytes = c.encode_utf8(&mut buf).as_bytes();
            assert!(!utf8_continuation(bytes[0]), "{} starts a character", c);
            for &b in &bytes[1..] {
                assert!(utf8_continuation(b), "{} continues with {:#x}", c, b);
            }
            assert_eq!(
                bytes.iter().filter(|&&b| !utf8_continuation(b)).count(),
                1,
                "{} is one column",
                c
            );
        }
    }

    #[test]
    fn the_stock_terminal_says_its_input_is_utf8() {
        // Linux's INIT_C_IFLAG has no IUTF8 — there `agetty` turns it on once
        // it knows the locale. This system has no `agetty`, and its console,
        // its keymaps and every shell on it are UTF-8.
        assert_eq!(I_IUTF8, 0o040000);
        assert!(Termios::default_tty().utf8_input());
    }

    #[test]
    fn the_stock_kill_character_echoes_a_newline_and_not_a_rubout() {
        // ISIG | ICANON | ECHO | ECHOE | ECHOK | IEXTEN: ECHOK is on and
        // ECHOKE is off, so Ctrl-U on a terminal nobody reconfigured leaves
        // the killed line on screen and starts a fresh one.
        assert_eq!(Termios::default_tty().kill_echo(), KillEcho::Newline);
    }

    #[test]
    fn echoke_wins_over_echok_when_a_program_asks_for_both() {
        let mut t = Termios::default_tty();
        t.c_lflag |= L_ECHOKE;
        assert_eq!(t.kill_echo(), KillEcho::Rubout);
    }

    #[test]
    fn a_program_that_turned_both_kill_flags_off_gets_no_echo_at_all() {
        let mut t = Termios::default_tty();
        t.c_lflag &= !(L_ECHOK | L_ECHOKE);
        assert_eq!(t.kill_echo(), KillEcho::Nothing);
    }

    #[test]
    fn echo_off_hides_the_kill_echo_whatever_the_other_two_say() {
        // A password prompt clears ECHO and leaves ECHOK where it was. Painting
        // rubouts back over the screen would say how long the secret is.
        let mut t = Termios::default_tty();
        t.c_lflag &= !L_ECHO;
        t.c_lflag |= L_ECHOKE;
        assert_eq!(t.kill_echo(), KillEcho::Nothing);
    }

    #[test]
    fn echonl_is_what_lets_a_password_prompt_end_its_line() {
        // getpass(3): ECHO off, ECHONL on. Without it the Enter that ends the
        // password is invisible and whatever prints next lands on the prompt.
        let mut t = Termios::default_tty();
        t.c_lflag &= !L_ECHO;
        assert!(!t.echoes_newline());
        t.c_lflag |= L_ECHONL;
        assert!(t.echoes_newline());
    }

    #[test]
    fn a_terminal_with_echo_on_shows_the_newline_without_being_asked() {
        let t = Termios::default_tty();
        assert_eq!(t.c_lflag & L_ECHONL, 0, "ECHONL is off in the default");
        assert!(t.echoes_newline());
    }

    /// Post-process a whole run the way a discipline does, carrying the column
    /// across bytes, and hand back what the terminal would receive.
    fn post(t: &Termios, input: &[u8]) -> alloc::vec::Vec<u8> {
        let mut col = 0usize;
        let mut out = alloc::vec::Vec::new();
        for &b in input {
            out.extend_from_slice(t.output_char(b, &mut col).as_bytes());
        }
        out
    }

    fn oflag(bits: u32) -> Termios {
        let mut t = Termios::default_tty();
        t.c_oflag = bits;
        t
    }

    #[test]
    fn opost_off_hands_every_byte_through_untouched() {
        // Raw mode. Not one bit of c_oflag may be consulted, including the
        // ones a program left set from before it went raw.
        let t = oflag(O_ONLCR | O_OCRNL | O_ONOCR | O_OLCUC);
        assert_eq!(post(&t, b"a\r\nb\tc"), b"a\r\nb\tc".to_vec());
    }

    #[test]
    fn opost_alone_is_not_onlcr() {
        // The bug this whole rule exists for: all six sites asked for
        // `OPOST && ONLCR` and did nothing otherwise, so OPOST on its own --
        // which is what `stty opost -onlcr` leaves -- behaved like raw output.
        // It must still be post-processing, just with nothing to translate.
        let t = oflag(O_OPOST);
        assert_eq!(post(&t, b"a\nb"), b"a\nb".to_vec());
    }

    #[test]
    fn onlcr_turns_a_newline_into_carriage_return_newline() {
        let t = oflag(O_OPOST | O_ONLCR);
        assert_eq!(post(&t, b"a\nb"), b"a\r\nb".to_vec());
    }

    #[test]
    fn onlcr_without_opost_does_nothing() {
        // OPOST is the switch for the whole word. A program that sets ONLCR
        // and clears OPOST has asked for raw output and must get it.
        let t = oflag(O_ONLCR);
        assert_eq!(post(&t, b"a\nb"), b"a\nb".to_vec());
    }

    #[test]
    fn ocrnl_turns_a_carriage_return_into_a_newline() {
        // `stty ocrnl`. Unreachable before this: OCRNL was declared in
        // stdio.rs and never read.
        let t = oflag(O_OPOST | O_OCRNL);
        assert_eq!(post(&t, b"a\rb"), b"a\nb".to_vec());
    }

    #[test]
    fn a_carriage_return_that_ocrnl_made_a_newline_is_not_then_expanded() {
        // Linux breaks out of its switch after OCRNL rather than falling into
        // the newline case, so the result is one byte even with ONLCR set.
        // Getting this wrong doubles every line ending on a terminal that has
        // both, which is `stty onlcr ocrnl`.
        let t = oflag(O_OPOST | O_OCRNL | O_ONLCR);
        assert_eq!(post(&t, b"a\rb"), b"a\nb".to_vec());
        // ...while a real newline still gets its carriage return.
        assert_eq!(post(&t, b"a\nb"), b"a\r\nb".to_vec());
    }

    #[test]
    fn onocr_drops_a_carriage_return_only_at_the_start_of_the_line() {
        // The point of ONOCR: on a printing terminal a CR at column zero costs
        // a head movement and shows nothing. Mid-line it is a real return and
        // must go.
        let t = oflag(O_OPOST | O_ONOCR);
        assert_eq!(post(&t, b"\r"), b"".to_vec());
        assert_eq!(post(&t, b"ab\r"), b"ab\r".to_vec());
    }

    #[test]
    fn a_carriage_return_puts_the_cursor_back_at_column_zero() {
        // So the *second* of two returns is the one ONOCR swallows.
        let t = oflag(O_OPOST | O_ONOCR);
        assert_eq!(post(&t, b"ab\r\r"), b"ab\r".to_vec());
    }

    #[test]
    fn onlcr_leaves_the_cursor_at_column_zero_so_onocr_sees_it() {
        // The two flags are usually set together and only agree if the newline
        // rule updates the column.
        let t = oflag(O_OPOST | O_ONLCR | O_ONOCR);
        assert_eq!(post(&t, b"ab\n\r"), b"ab\r\n".to_vec());
    }

    #[test]
    fn a_bare_newline_does_not_move_the_cursor_across() {
        // With neither ONLCR nor ONLRET the terminal drops a line and stays in
        // the same column, so a following CR is a real one.
        let t = oflag(O_OPOST | O_ONOCR);
        assert_eq!(post(&t, b"ab\n\r"), b"ab\n\r".to_vec());
    }

    #[test]
    fn onlret_says_the_terminal_returns_its_own_carriage() {
        // That is all ONLRET does: it emits nothing. It tells the driver the
        // cursor is back at column zero, which is the only reason ONOCR can
        // then be right about the CR that follows.
        let t = oflag(O_OPOST | O_ONLRET | O_ONOCR);
        assert_eq!(post(&t, b"ab\n\r"), b"ab\n".to_vec());
    }

    #[test]
    fn onlret_adds_no_byte_of_its_own() {
        let t = oflag(O_OPOST | O_ONLRET);
        assert_eq!(post(&t, b"a\nb"), b"a\nb".to_vec());
    }

    #[test]
    fn ocrnl_with_onlret_also_leaves_the_cursor_at_column_zero() {
        // The return became a newline, and on this terminal a newline returns
        // the carriage, so the column has to follow it.
        let t = oflag(O_OPOST | O_OCRNL | O_ONLRET | O_ONOCR);
        assert_eq!(post(&t, b"ab\r\r"), b"ab\n".to_vec());
    }

    #[test]
    fn ocrnl_without_onlret_leaves_the_column_where_it_was() {
        // No ONLRET: the newline the CR became does not return the carriage,
        // so the column stands and the next CR is a real one.
        let t = oflag(O_OPOST | O_OCRNL | O_ONOCR);
        assert_eq!(post(&t, b"ab\r\r"), b"ab\n\n".to_vec());
    }

    #[test]
    fn olcuc_upper_cases_the_letters_and_leaves_the_rest() {
        let t = oflag(O_OPOST | O_OLCUC);
        assert_eq!(post(&t, b"ab1-z\n"), b"AB1-Z\n".to_vec());
    }

    #[test]
    fn olcuc_does_not_touch_a_control_byte() {
        // `iscntrl` guards the whole default branch in Linux. 0x01 upper-cased
        // would still be 0x01, but 0x7f is DEL and must stay DEL.
        let t = oflag(O_OPOST | O_OLCUC);
        assert_eq!(post(&t, &[0x01, 0x7f]), alloc::vec![0x01, 0x7f]);
    }

    #[test]
    fn a_tab_moves_the_cursor_to_the_next_stop_of_eight() {
        // Not for the tab's own sake -- it goes out as a tab -- but because
        // ONOCR asks where the cursor is afterwards.
        let t = oflag(O_OPOST | O_ONOCR);
        assert_eq!(post(&t, b"\tx"), b"\tx".to_vec());
        let mut col = 0usize;
        t.output_char(b'\t', &mut col);
        assert_eq!(col, 8);
        t.output_char(b'\t', &mut col);
        assert_eq!(col, 16);
        col = 3;
        t.output_char(b'\t', &mut col);
        assert_eq!(col, 8, "a tab from column 3 lands on the stop, not 3 + 8");
    }

    #[test]
    fn a_backspace_takes_the_column_back_one_and_stops_at_zero() {
        let t = oflag(O_OPOST | O_ONOCR);
        let mut col = 2usize;
        t.output_char(0x08, &mut col);
        assert_eq!(col, 1);
        t.output_char(0x08, &mut col);
        assert_eq!(col, 0);
        t.output_char(0x08, &mut col);
        assert_eq!(col, 0, "the column must not go below zero");
        // And at column zero a backspace has put us where ONOCR bites.
        assert!(t.output_char(b'\r', &mut col).is_empty());
    }

    #[test]
    fn a_character_several_bytes_long_occupies_one_column() {
        // Otherwise a line of accented text reports a column three times too
        // far along, and ONOCR then sends a carriage return that was not
        // wanted -- the same mistake the erase path made before it learnt
        // about continuation bytes.
        let mut t = oflag(O_OPOST | O_ONOCR);
        t.c_iflag |= I_IUTF8;
        let mut col = 0usize;
        for &b in "ñ".as_bytes() {
            t.output_char(b, &mut col);
        }
        assert_eq!(col, 1, "two bytes, one column");
    }

    #[test]
    fn a_terminal_that_was_not_told_its_input_is_utf8_counts_bytes() {
        // IUTF8 off is a terminal that has been told its bytes are characters,
        // and the column has to agree with it rather than with the truth.
        let mut t = oflag(O_OPOST | O_ONOCR);
        t.c_iflag &= !I_IUTF8;
        let mut col = 0usize;
        for &b in "ñ".as_bytes() {
            t.output_char(b, &mut col);
        }
        assert_eq!(col, 2);
    }

    #[test]
    fn a_control_byte_does_not_move_the_cursor_across() {
        let t = oflag(O_OPOST | O_ONOCR);
        let mut col = 0usize;
        for b in [0x07u8, 0x1b, 0x00] {
            t.output_char(b, &mut col);
        }
        assert_eq!(col, 0, "a bell, an escape and a NUL take no column");
        assert!(t.output_char(b'\r', &mut col).is_empty());
    }

    #[test]
    fn a_byte_above_ascii_is_not_a_control_byte() {
        // `iscntrl` is false from 0x80 up, so a Latin-1 byte on a terminal
        // with IUTF8 off takes its column like any other.
        let mut t = oflag(O_OPOST | O_ONOCR);
        t.c_iflag &= !I_IUTF8;
        let mut col = 0usize;
        t.output_char(0xe9, &mut col);
        assert_eq!(col, 1);
    }

    #[test]
    fn the_column_saturates_instead_of_wrapping() {
        // Nothing reaches this, but a wrap would put the cursor back at column
        // zero and make ONOCR swallow a carriage return that was wanted.
        let t = oflag(O_OPOST);
        let mut col = usize::MAX;
        t.output_char(b'x', &mut col);
        assert_eq!(col, usize::MAX);
        t.output_char(b'\t', &mut col);
        assert_eq!(col, usize::MAX);
    }

    #[test]
    fn the_cooked_default_is_post_processed_with_onlcr_and_nothing_else() {
        let t = Termios::default_tty();
        assert_ne!(t.c_oflag & O_OPOST, 0);
        assert_ne!(t.c_oflag & O_ONLCR, 0);
        assert_eq!(t.c_oflag & (O_OCRNL | O_ONOCR | O_ONLRET | O_OLCUC), 0);
        assert_eq!(post(&t, b"hola\n"), b"hola\r\n".to_vec());
    }

    #[test]
    fn nothing_but_onocr_ever_swallows_a_byte() {
        // A discipline may hand the result straight to its queue, so the empty
        // case has exactly one cause and it is worth pinning.
        for bits in [
            O_OPOST,
            O_OPOST | O_ONLCR,
            O_OPOST | O_OCRNL,
            O_OPOST | O_ONLRET,
            O_OPOST | O_OLCUC,
            O_OPOST | O_ONLCR | O_OCRNL | O_ONLRET | O_OLCUC,
        ] {
            let t = oflag(bits);
            for b in 0u8..=255 {
                let mut col = 0usize;
                assert!(
                    !t.output_char(b, &mut col).is_empty(),
                    "oflag {:#x} swallowed {:#04x}",
                    bits,
                    b
                );
            }
        }
    }
}
